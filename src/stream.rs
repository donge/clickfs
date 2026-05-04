//! Streaming bridge between async ClickHouse stream and sync FUSE read().
//!
//! Strict-sequential semantics: read(offset) where offset != stream_pos → EIO,
//! UNLESS tail-buffer materialization kicks in (large reverse pread, e.g.
//! `tail -n N`). In that case we issue a one-shot
//! `SELECT * ORDER BY <pk> DESC LIMIT N` and pin the (row-reversed)
//! result to the file's pseudo-EOF so subsequent reads land in-buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::stream::StreamExt;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::driver::{HttpDriver, TailConfig};
use crate::error::{ClickFsError, QueryError};

/// The pseudo-size we report for stream files. `tail` will lseek to this
/// then start pread-ing backwards from here.
pub const PSEUDO_EOF: u64 = u64::MAX / 2;

/// Reverse-pread distance that flips us into "this is tail" mode.
const TAIL_TRIGGER_GAP: u64 = 32 * 1024 * 1024;

/// Per-table info needed to materialize a tail buffer on demand.
#[derive(Clone)]
pub struct TailContext {
    pub db: String,
    pub tbl: String,
    /// Comma-separated column list, or the literal `tuple()` for tables
    /// without a primary/sorting key.
    pub order_expr: String,
    /// When `Some`, the materialization SELECT is restricted to a single
    /// partition via `WHERE _partition_id = '<id>'`. Used so `tail
    /// 20260503.tsv` works on partition pseudo-files (without this
    /// the partition file's reverse pread always returned EIO).
    pub partition: Option<String>,
    pub cfg: TailConfig,
}

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
    /// Tail-buffer materialization context. None = strict-sequential
    /// only (no reverse-pread fallback).
    tail: Option<TailContext>,
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
    /// Once a reverse pread triggers materialization we buffer the
    /// (row-reversed) tail here and serve subsequent reads from it.
    /// The buffer is conceptually pinned to `[PSEUDO_EOF - len, PSEUDO_EOF)`.
    tail_buf: Option<Bytes>,
    /// Set true once we've attempted (success or failure) to materialize
    /// the tail buffer. Prevents re-issuing the SQL on every reverse
    /// pread when the table is e.g. unreachable.
    tail_attempted: bool,
}

impl StreamHandle {
    /// Spawn a streaming task. When `tail` is `Some`, a large reverse
    /// pread can synthesize a tail buffer on demand
    /// (`SELECT ... ORDER BY <pk> DESC LIMIT N`). When `None`, reverse
    /// preads beyond the current cursor are rejected with EIO.
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        driver: HttpDriver,
        sql: String,
        tail: Option<TailContext>,
    ) -> Self {
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
                tail_buf: None,
                tail_attempted: false,
            })),
            cancel,
            reverse_warned: AtomicBool::new(false),
            query_id,
            driver,
            rt: rt.clone(),
            tail,
            _task: task,
        }
    }

    /// Synchronous read used from the FUSE worker thread.
    ///
    /// Strict-sequential by default: `offset` MUST equal the current
    /// stream position. A large reverse pread (offset > pos +
    /// `TAIL_TRIGGER_GAP`) when a `TailContext` is set triggers a
    /// one-shot materialization of `SELECT * ORDER BY <pk> DESC LIMIT N`
    /// and serves subsequent reads from that buffer.
    pub fn read_blocking(
        &self,
        rt: &tokio::runtime::Handle,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, ClickFsError> {
        // Fast path: serve from an already-materialized tail buffer
        // whenever the offset falls inside it. Held under the lock to
        // keep tail_buf access consistent.
        let inner = self.inner.clone();

        // Step 1: maybe materialize the tail buffer. Done outside the
        // lock so the (potentially slow) HTTP call doesn't block other
        // reads. The flag `tail_attempted` (set under the lock) prevents
        // re-entry.
        let need_materialize = {
            let g = rt.block_on(async { inner.lock().await });
            offset != g.pos
                && !g.tail_attempted
                && g.tail_buf.is_none()
                && self.tail.is_some()
                && offset > g.pos.saturating_add(TAIL_TRIGGER_GAP)
        };
        if need_materialize {
            let tail = self.tail.as_ref().unwrap().clone();
            let driver = self.driver.clone();
            let mat = rt.block_on(async move { materialize_tail(&driver, &tail).await });
            let mut g = rt.block_on(async { inner.lock().await });
            g.tail_attempted = true;
            match mat {
                Ok(buf) => {
                    tracing::info!(
                        target: "clickfs::stream",
                        query_id = %self.query_id,
                        bytes = buf.len(),
                        "tail buffer materialized"
                    );
                    g.tail_buf = Some(buf);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "clickfs::stream",
                        query_id = %self.query_id,
                        error = %e,
                        "tail buffer materialization failed; falling back to EIO"
                    );
                }
            }
        }

        rt.block_on(async move {
            let mut g = inner.lock().await;

            // Check for prior fatal error on the forward stream (only
            // surfaces while we're still serving forward).
            if offset == g.pos {
                if let Some(e) = g.last_err.take() {
                    return Err(ClickFsError::Query(e));
                }
            }

            // Tail buffer hit: serve any read whose offset lands inside
            // the buffer's pinned range.
            if let Some(buf) = &g.tail_buf {
                let buf_origin = PSEUDO_EOF.saturating_sub(buf.len() as u64);
                if offset >= buf_origin && offset < PSEUDO_EOF {
                    let local = (offset - buf_origin) as usize;
                    if local >= buf.len() {
                        return Ok(Vec::new());
                    }
                    let take = std::cmp::min(size, buf.len() - local);
                    return Ok(buf[local..local + take].to_vec());
                }
                // Inside virtual EOF but past buffer end → EOF.
                if offset >= PSEUDO_EOF {
                    return Ok(Vec::new());
                }
                // Falls through: a forward read that came BEFORE the
                // tail trigger. We still serve it from the live stream.
            }

            if offset != g.pos {
                // A backwards (or skipping) seek breaks streaming
                // semantics. Demoted to debug now that tail-mode is the
                // documented escape hatch — common offenders (`tail`,
                // `less G`) are handled above.
                if offset < g.pos && !self.reverse_warned.swap(true, Ordering::Relaxed) {
                    tracing::debug!(
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

/// One-shot fetch of `SELECT * ORDER BY <pk> DESC LIMIT N` followed by
/// an in-memory row-reversal so the buffer reads "oldest first". The
/// header line is preserved at offset 0.
async fn materialize_tail(driver: &HttpDriver, tail: &TailContext) -> Result<Bytes, QueryError> {
    let sql = crate::driver::sql_select_tail(
        &tail.db,
        &tail.tbl,
        &tail.order_expr,
        tail.partition.as_deref(),
        tail.cfg.rows,
    );
    let body = driver.query_text(&sql).await?;
    Ok(reverse_tsv_rows(&body))
}

/// Reverse the data lines of a TabSeparatedWithNames body. Header line
/// (first \n-delimited line) stays in place; trailing empty line, if
/// present, is preserved as-is. Result has exactly one trailing
/// newline if the input did.
fn reverse_tsv_rows(body: &str) -> Bytes {
    if body.is_empty() {
        return Bytes::new();
    }
    let mut lines: Vec<&str> = body.split('\n').collect();
    // split('\n') on "a\nb\n" yields ["a","b",""]; preserve that empty
    // tail as the terminating newline.
    let trailing_empty = matches!(lines.last(), Some(&""));
    if trailing_empty {
        lines.pop();
    }
    if lines.len() <= 1 {
        // Just a header (or nothing): nothing to reverse.
        let mut out = lines.join("\n");
        if trailing_empty {
            out.push('\n');
        }
        return Bytes::from(out);
    }
    let header = lines.remove(0);
    lines.reverse();
    let mut out = String::with_capacity(body.len());
    out.push_str(header);
    out.push('\n');
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    if trailing_empty {
        out.push('\n');
    }
    Bytes::from(out)
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
        let h = StreamHandle::spawn(rt.handle(), driver, "SELECT 1".into(), None);
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

    #[test]
    fn reverse_tsv_rows_keeps_header_reverses_data() {
        let body = "name\tage\nalice\t30\nbob\t40\ncarol\t50\n";
        let out = reverse_tsv_rows(body);
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "name\tage\ncarol\t50\nbob\t40\nalice\t30\n");
    }

    #[test]
    fn reverse_tsv_rows_handles_empty_and_header_only() {
        assert_eq!(&reverse_tsv_rows("")[..], b"");
        // header only, with trailing newline
        assert_eq!(&reverse_tsv_rows("a\tb\n")[..], b"a\tb\n");
        // header only, no trailing newline
        assert_eq!(&reverse_tsv_rows("a\tb")[..], b"a\tb");
    }

    #[test]
    fn reverse_tsv_rows_no_trailing_newline() {
        let body = "h\nx\ny\nz";
        let out = reverse_tsv_rows(body);
        assert_eq!(std::str::from_utf8(&out).unwrap(), "h\nz\ny\nx");
    }

    #[test]
    fn tail_buffer_serves_reverse_pread_inside_buffer() {
        // Hand-craft a StreamHandle, then directly install a tail_buf
        // (simulating a successful materialize_tail) and verify reads
        // pinned to PSEUDO_EOF land in-buffer.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = fake_handle(&rt, b"");
        let tail_bytes = Bytes::from_static(b"name\tage\nalice\t30\nbob\t40\n");
        let tail_len = tail_bytes.len() as u64;
        rt.block_on(async {
            let mut g = h.inner.lock().await;
            g.tail_buf = Some(tail_bytes);
            g.tail_attempted = true;
        });
        let buf_origin = PSEUDO_EOF - tail_len;
        // Read whole buffer.
        let buf = h
            .read_blocking(rt.handle(), buf_origin, tail_len as usize)
            .unwrap();
        assert_eq!(buf, b"name\tage\nalice\t30\nbob\t40\n");
        // Read past EOF -> empty.
        let buf2 = h.read_blocking(rt.handle(), PSEUDO_EOF, 10).unwrap();
        assert!(buf2.is_empty());
        // Read partial from middle of buffer.
        let buf3 = h.read_blocking(rt.handle(), buf_origin + 9, 10).unwrap();
        assert_eq!(buf3, b"alice\t30\nb");
    }

    #[test]
    fn tail_disabled_keeps_reverse_seek_eio() {
        // No TailContext => reverse pread still EIO even if huge.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = fake_handle(&rt, b"abc");
        let _ = h.read_blocking(rt.handle(), 0, 3).unwrap();
        // Now pos=3; reverse pread to a huge offset.
        let err = h
            .read_blocking(rt.handle(), PSEUDO_EOF - 100, 4096)
            .unwrap_err();
        assert!(matches!(err, ClickFsError::ReadOrder { .. }));
    }
}
