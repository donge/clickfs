//! Streaming bridge between async ClickHouse stream and sync FUSE read().
//!
//! Strict-sequential semantics: read(offset) where offset != stream_pos → EIO.

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Whether we've already warned about a reverse seek on this handle.
    /// Used to keep the log from filling up if a tool retries repeatedly.
    reverse_warned: AtomicBool,
    /// Caller-side query_id used for KILL QUERY in Drop. We keep the
    /// driver too so we can fire the KILL from a detached spawn that
    /// outlives `self` only by milliseconds.
    query_id: String,
    driver: HttpDriver,
    /// Tokio handle used by Drop to spawn the detached KILL. We can't
    /// call `Handle::current()` from Drop because the FUSE worker thread
    /// is not inside a runtime context.
    rt: tokio::runtime::Handle,
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
        // Tag every streaming query so we can KILL it on early close.
        // Prefix is ours so an operator inspecting system.processes can
        // tell at a glance "this came from clickfs".
        let query_id = format!("clickfs-{}", uuid::Uuid::new_v4());
        let driver_for_task = driver.clone();
        let qid_for_task = query_id.clone();

        let task = rt.spawn(async move {
            let stream = match driver_for_task
                .query_stream_with_id(&sql, Some(&qid_for_task))
                .await
            {
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
                        tracing::debug!(target: "clickfs::stream", query_id = %qid_for_task, "cancelled");
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
            reverse_warned: AtomicBool::new(false),
            query_id,
            driver,
            rt: rt.clone(),
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
                // A backwards (or skipping) seek breaks streaming semantics.
                // Warn once per handle so the log isn't flooded by tools that
                // retry. Common offenders: `tail -n N` (seeks to EOF first),
                // editors with mmap, anything calling lseek(SEEK_END).
                if offset < g.pos && !self.reverse_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "clickfs::stream",
                        cursor = g.pos,
                        requested_offset = offset,
                        "reverse seek not supported on streaming files; \
                         use 'cat ... | tail -n N' or 'head -c N | tail' instead"
                    );
                }
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

    /// The server-side query_id assigned to this stream. Exposed mainly
    /// for tests and for operators wanting to correlate with
    /// system.query_log.
    #[allow(dead_code)]
    pub fn query_id(&self) -> &str {
        &self.query_id
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.cancel();
        // Best-effort server-side cancellation. Connection drop alone is
        // usually enough on a healthy server (ClickHouse notices the
        // socket close and aborts), but the abort can lag by seconds on
        // long-running queries — KILL QUERY ASYNC makes it deterministic.
        // Detached: we don't await; the task lives in the runtime and
        // will outlive `self` long enough to fire.
        let driver = self.driver.clone();
        let qid = self.query_id.clone();
        self.rt.spawn(async move {
            driver.kill_query(&qid).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::HttpDriver;
    use url::Url;

    /// Build a StreamHandle whose upstream returns immediately with a single
    /// chunk, so we can drive `read_blocking` against it without a real
    /// ClickHouse server.
    fn fake_handle(rt: &tokio::runtime::Runtime, body: &'static [u8]) -> StreamHandle {
        // Spawn a real StreamHandle, but we won't actually let it talk to the
        // network — we manually craft Inner state below.
        let driver = HttpDriver::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            "x".into(),
            String::new(),
            60,
            0,
            crate::driver::TlsConfig::default(),
            crate::driver::CompressionConfig::default(),
        )
        .unwrap();
        let h = StreamHandle::spawn(rt.handle(), driver, "SELECT 1".into());
        // Replace the channel state synchronously: stuff `body` into pending
        // and mark upstream done so reads don't block on rx.
        rt.block_on(async {
            let mut g = h.inner.lock().await;
            g.pending.extend_from_slice(body);
            g.upstream_done = true;
            g.last_err = None;
        });
        h.cancel(); // make sure the spawned task quits
        h
    }

    #[test]
    fn reverse_seek_warns_once_and_errors() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = fake_handle(&rt, b"abcdefghij");

        // Forward read advances cursor.
        let buf = h.read_blocking(rt.handle(), 0, 5).unwrap();
        assert_eq!(buf, b"abcde");

        // Reverse seek -> error, warn fires once.
        let err1 = h.read_blocking(rt.handle(), 0, 5).unwrap_err();
        assert!(matches!(err1, ClickFsError::ReadOrder { .. }));
        assert!(h.reverse_warned.load(Ordering::Relaxed));

        // Second reverse seek -> still error, but warn flag already set.
        let err2 = h.read_blocking(rt.handle(), 0, 5).unwrap_err();
        assert!(matches!(err2, ClickFsError::ReadOrder { .. }));
        assert!(h.reverse_warned.load(Ordering::Relaxed));
    }

    #[test]
    fn sequential_reads_do_not_warn() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = fake_handle(&rt, b"abcdefghij");

        let a = h.read_blocking(rt.handle(), 0, 4).unwrap();
        assert_eq!(a, b"abcd");
        let b = h.read_blocking(rt.handle(), 4, 4).unwrap();
        assert_eq!(b, b"efgh");
        let c = h.read_blocking(rt.handle(), 8, 10).unwrap();
        assert_eq!(c, b"ij");
        assert!(!h.reverse_warned.load(Ordering::Relaxed));
    }

    #[test]
    fn query_id_is_assigned_and_clickfs_prefixed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = fake_handle(&rt, b"x");
        let id = h.query_id();
        assert!(
            id.starts_with("clickfs-"),
            "query_id should be clickfs-prefixed, got: {id}"
        );
        // UUID v4 is 36 chars; "clickfs-" prefix is 8 → 44 total.
        assert_eq!(id.len(), 44, "expected clickfs- + uuidv4, got: {id}");
    }

    #[test]
    fn distinct_handles_get_distinct_query_ids() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let h1 = fake_handle(&rt, b"x");
        let h2 = fake_handle(&rt, b"y");
        assert_ne!(h1.query_id(), h2.query_id());
    }
}
