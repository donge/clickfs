//! Streaming bridge between async ClickHouse stream and sync FUSE read().
//!
//! Strict-sequential semantics: read(offset) where offset != stream_pos → EIO.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::stream::StreamExt;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::driver::HttpDriver;
use crate::error::{ClickFsError, QueryError};

/// State of a single streaming file handle.
pub struct StreamHandle {
    inner: Arc<Mutex<Inner>>,
    cancel: Arc<Notify>,
    _task: JoinHandle<()>,
}

struct Inner {
    rx: mpsc::Receiver<std::result::Result<Bytes, QueryError>>,
    /// Bytes already pulled from rx but not yet consumed by read().
    pending: BytesMut,
    /// Bytes already returned to the kernel.
    pos: u64,
    upstream_done: bool,
    last_err: Option<QueryError>,
}

impl StreamHandle {
    /// Spawn a streaming task. Caller passes the runtime handle so we can spawn.
    pub fn spawn(rt: &tokio::runtime::Handle, driver: HttpDriver, sql: String) -> Self {
        let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, QueryError>>(8);
        let cancel = Arc::new(Notify::new());
        let cancel_task = cancel.clone();

        let task = rt.spawn(async move {
            let stream = match driver.query_stream(&sql).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            tokio::pin!(stream);
            loop {
                tokio::select! {
                    _ = cancel_task.notified() => {
                        tracing::debug!(target: "clickfs::stream", "cancelled");
                        break;
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break; // reader gone
                                }
                            }
                            Some(Err(e)) => {
                                let _ = tx.send(Err(e)).await;
                                break;
                            }
                            None => break, // EOF
                        }
                    }
                }
            }
            // Dropping `tx` signals EOF to the reader side.
        });

        StreamHandle {
            inner: Arc::new(Mutex::new(Inner {
                rx,
                pending: BytesMut::new(),
                pos: 0,
                upstream_done: false,
                last_err: None,
            })),
            cancel,
            _task: task,
        }
    }

    /// Synchronous read used from the FUSE worker thread.
    ///
    /// Strict-sequential: `offset` MUST equal the current stream position.
    pub fn read_blocking(
        &self,
        rt: &tokio::runtime::Handle,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, ClickFsError> {
        let inner = self.inner.clone();
        rt.block_on(async move {
            let mut g = inner.lock().await;

            // Check for prior fatal error.
            if let Some(e) = g.last_err.take() {
                return Err(ClickFsError::Query(e));
            }

            if offset != g.pos {
                return Err(ClickFsError::ReadOrder {
                    expected: g.pos,
                    got: offset,
                });
            }

            // Pull from rx until we have `size` bytes or EOF.
            while g.pending.len() < size && !g.upstream_done {
                match g.rx.recv().await {
                    Some(Ok(bytes)) => g.pending.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        g.upstream_done = true;
                        g.last_err = Some(e);
                        break;
                    }
                    None => {
                        g.upstream_done = true;
                        break;
                    }
                }
            }

            // If an error occurred and we have no pending data, surface it.
            if g.pending.is_empty() {
                if let Some(e) = g.last_err.take() {
                    return Err(ClickFsError::Query(e));
                }
            }

            let take = std::cmp::min(size, g.pending.len());
            let out = g.pending.split_to(take).to_vec();
            g.pos += take as u64;
            Ok(out)
        })
    }

    /// Signal the streaming task to stop and drop the HTTP connection.
    pub fn cancel(&self) {
        self.cancel.notify_waiters();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.cancel();
        // Task is detached; it will observe cancellation on next await.
    }
}
