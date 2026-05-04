//! `fuser::Filesystem` implementation.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, Request,
};
use tokio::runtime::Handle;

use crate::cache::TtlCache;
use crate::driver::{self, HttpDriver, TailConfig};
use crate::error::{ClickFsError, Result};
use crate::inode::{InodeTable, INO_ROOT};
use crate::resolver::{self, PlanKind, QueryPlan};
use crate::stream::{StreamHandle, TailContext};

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;

/// Per-open-file handle state.
enum FileHandle {
    Stream(Arc<StreamHandle>),
    Special { bytes: Arc<Vec<u8>> },
}

pub struct ClickFs {
    pub driver: HttpDriver,
    pub rt: Handle,
    pub inodes: Arc<InodeTable>,
    fh_counter: AtomicU64,
    handles: Mutex<HashMap<u64, FileHandle>>,
    uid: u32,
    gid: u32,
    start_time: SystemTime,
    /// Configuration for tail-mode reverse-pread synthesis. Cloned into
    /// each StreamAll handle's `TailContext`.
    tail_cfg: TailConfig,

    // --- TTL caches (2s) ---
    // Empty key () for "list databases"; db name for table lists;
    // (db, tbl) for partition lists. Coalesces the storm of follow-up
    // lookups inside a single `ls` (one query, then in-memory contains
    // checks for each child).
    db_cache: Arc<TtlCache<(), Vec<String>>>,
    table_cache: Arc<TtlCache<String, Vec<String>>>,
    part_cache: Arc<TtlCache<(String, String), Vec<String>>>,
}

impl ClickFs {
    pub fn new(driver: HttpDriver, rt: Handle, tail_cfg: TailConfig) -> Self {
        let ttl = Duration::from_millis(2000);
        if !tail_cfg.enabled {
            tracing::info!(target: "clickfs::tail", "tail-mode disabled (--no-tail)");
        } else {
            tracing::info!(
                target: "clickfs::tail",
                rows = tail_cfg.rows,
                "tail-mode enabled (large reverse pread → ORDER BY <pk> DESC LIMIT N)"
            );
        }
        Self {
            driver,
            rt,
            inodes: Arc::new(InodeTable::new()),
            fh_counter: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            start_time: SystemTime::now(),
            tail_cfg,
            db_cache: Arc::new(TtlCache::new(ttl)),
            table_cache: Arc::new(TtlCache::new(ttl)),
            part_cache: Arc::new(TtlCache::new(ttl)),
        }
    }

    fn dir_attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: self.start_time,
            mtime: self.start_time,
            ctime: self.start_time,
            crtime: self.start_time,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE,
        }
    }

    fn file_attr(&self, ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(BLOCK_SIZE as u64),
            atime: self.start_time,
            mtime: self.start_time,
            ctime: self.start_time,
            crtime: self.start_time,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE,
        }
    }

    fn attr_for_plan(&self, ino: u64, plan: &QueryPlan) -> FileAttr {
        if plan.is_dir() {
            self.dir_attr(ino)
        } else if plan.is_special_file() {
            // Use a small placeholder size; cat/head ignore inaccurate size.
            self.file_attr(ino, 4096)
        } else {
            // Stream files: report a large pseudo-size so tools don't EOF early.
            self.file_attr(ino, u64::MAX / 2)
        }
    }

    /// Resolve an inode to its path; returns ENOENT if unknown.
    fn path_of(&self, ino: u64) -> Result<PathBuf> {
        self.inodes.lookup(ino).ok_or(ClickFsError::NotFound)
    }

    /// Run a small async block on the runtime, mapping errors.
    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, ClickFsError>>,
    {
        self.rt.block_on(fut)
    }

    fn fetch_databases(&self) -> Result<Vec<String>> {
        if let Some(v) = self.db_cache.get(&()) {
            return Ok(v);
        }
        let driver = self.driver.clone();
        let v: Vec<String> = self.block_on(async move {
            let body = driver
                .query_text(&driver::sql_list_databases())
                .await
                .map_err(ClickFsError::Query)?;
            Ok(body
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect())
        })?;
        self.db_cache.insert((), v.clone());
        Ok(v)
    }

    fn fetch_tables(&self, db: &str) -> Result<Vec<String>> {
        if let Some(v) = self.table_cache.get(&db.to_string()) {
            return Ok(v);
        }
        let driver = self.driver.clone();
        let sql = driver::sql_list_tables(db);
        let v: Vec<String> = self.block_on(async move {
            let body = driver.query_text(&sql).await.map_err(ClickFsError::Query)?;
            Ok(body
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect())
        })?;
        self.table_cache.insert(db.to_string(), v.clone());
        Ok(v)
    }

    fn fetch_partitions(&self, db: &str, tbl: &str) -> Result<Vec<String>> {
        let key = (db.to_string(), tbl.to_string());
        if let Some(v) = self.part_cache.get(&key) {
            return Ok(v);
        }
        let driver = self.driver.clone();
        let sql = driver::sql_list_partitions(db, tbl);
        let v: Vec<String> = self.block_on(async move {
            let body = driver.query_text(&sql).await.map_err(ClickFsError::Query)?;
            Ok(body
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                // Non-partitioned tables report a single pseudo-partition
                // named "all"; exposing it as "all.tsv" would collide with
                // the always-present whole-table file. Drop it.
                .filter(|s| s != "all")
                .collect())
        })?;
        self.part_cache.insert(key, v.clone());
        Ok(v)
    }

    fn fetch_describe(&self, db: &str, tbl: &str) -> Result<String> {
        let driver = self.driver.clone();
        let sql = driver::sql_describe(db, tbl);
        self.block_on(async move { driver.query_text(&sql).await.map_err(ClickFsError::Query) })
    }

    /// Resolve the ORDER BY expression to use for tail-mode synthesis.
    /// Prefers `primary_key`, falls back to `sorting_key`, then to
    /// `tuple()` (effectively unordered) for engines without a key.
    /// Errors are non-fatal: we log and return `tuple()` so opens still
    /// succeed even if `system.tables` is unreachable.
    fn fetch_tail_order_expr(&self, db: &str, tbl: &str) -> String {
        let driver = self.driver.clone();
        let sql = driver::sql_table_pk(db, tbl);
        let body = self
            .rt
            .block_on(async move { driver.query_text(&sql).await });
        let body = match body {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(target: "clickfs::tail", db, tbl, error = %e, "pk lookup failed; falling back to tuple()");
                return "tuple()".to_string();
            }
        };
        // JSONEachRow → at most one line.
        let line = body.lines().next().unwrap_or("");
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return "tuple()".to_string(),
        };
        let pk = v.get("primary_key").and_then(|x| x.as_str()).unwrap_or("");
        if !pk.trim().is_empty() {
            return pk.to_string();
        }
        let sk = v.get("sorting_key").and_then(|x| x.as_str()).unwrap_or("");
        if !sk.trim().is_empty() {
            return sk.to_string();
        }
        tracing::debug!(target: "clickfs::tail", db, tbl, "no primary_key/sorting_key; using tuple()");
        "tuple()".to_string()
    }

    /// Verify the plan corresponds to a real ClickHouse object by
    /// checking the parent's listing (which is itself cached). Returns
    /// NotFound for bogus db/table/partition names. Cheap: at most one
    /// list query per (parent, TTL window), reused across all children.
    fn verify_plan_exists(&self, plan: &resolver::QueryPlan) -> Result<()> {
        use resolver::PlanKind;
        match (&plan.kind, plan.db.as_deref(), plan.table.as_deref()) {
            (PlanKind::Root, _, _) | (PlanKind::DbNamespace, _, _) => Ok(()),
            (PlanKind::ListTables, Some(db), _) => {
                if self.fetch_databases()?.iter().any(|d| d == db) {
                    Ok(())
                } else {
                    Err(ClickFsError::NotFound)
                }
            }
            (PlanKind::ListPartitions, Some(db), Some(t))
            | (PlanKind::DescribeTable, Some(db), Some(t))
            | (PlanKind::Readme, Some(db), Some(t))
            | (PlanKind::StreamAll, Some(db), Some(t)) => {
                if self.fetch_tables(db)?.iter().any(|x| x == t) {
                    Ok(())
                } else {
                    Err(ClickFsError::NotFound)
                }
            }
            (PlanKind::StreamPartition(part), Some(db), Some(t)) => {
                if self.fetch_partitions(db, t)?.iter().any(|p| p == part) {
                    Ok(())
                } else {
                    Err(ClickFsError::NotFound)
                }
            }
            _ => Err(ClickFsError::NotFound),
        }
    }
}

/// Generates a `fuser::Filesystem` method that takes the named arguments
/// (in addition to `&mut self` + `_req: &Request<'_>`) plus a `reply` of
/// the given type, and immediately calls `reply.error(libc::EROFS)`.
/// Used to declare the read-only stubs concisely.
macro_rules! erofs_stub {
    (fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> $reply:ty) => {
        fn $name(
            &mut self,
            _req: &Request<'_>,
            $($arg: $ty,)*
            reply: $reply,
        ) {
            let _ = ($($arg,)*);
            reply.error(libc::EROFS);
        }
    };
}

impl Filesystem for ClickFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.path_of(parent) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let child = parent_path.join(name_str);

        let plan = match resolver::resolve(&child) {
            Ok(p) => p,
            Err(e) => {
                tracing::trace!(target: "clickfs::fs", path = %child.display(), error = %e, "lookup resolve failed");
                reply.error(e.to_errno());
                return;
            }
        };

        // Verify existence in ClickHouse so we return ENOENT for bogus
        // db/table/partition names instead of silently materializing fake
        // directories. The check is one tiny COUNT() per lookup; the kernel
        // caches entries via TTL so repeated stat()s won't re-query.
        if let Err(e) = self.verify_plan_exists(&plan) {
            tracing::trace!(target: "clickfs::fs", path = %child.display(), error = %e, "lookup existence check failed");
            reply.error(e.to_errno());
            return;
        }

        let ino = self.inodes.allocate(&child);
        let attr = self.attr_for_plan(ino, &plan);
        reply.entry(&TTL, &attr, 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        if ino == INO_ROOT {
            reply.attr(&TTL, &self.dir_attr(INO_ROOT));
            return;
        }
        let path = match self.path_of(ino) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };
        match resolver::resolve(&path) {
            Ok(plan) => reply.attr(&TTL, &self.attr_for_plan(ino, &plan)),
            Err(e) => reply.error(e.to_errno()),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.path_of(ino) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };
        let plan = match resolver::resolve(&path) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };

        // Build entries vector based on plan kind.
        let mut entries: Vec<(FileType, String)> = vec![
            (FileType::Directory, ".".to_string()),
            (FileType::Directory, "..".to_string()),
        ];

        let res: Result<()> = (|| {
            match &plan.kind {
                PlanKind::Root => {
                    entries.push((FileType::Directory, "db".to_string()));
                }
                PlanKind::DbNamespace => {
                    for db in self.fetch_databases()? {
                        entries.push((FileType::Directory, db));
                    }
                }
                PlanKind::ListTables => {
                    let db = plan.db.as_deref().unwrap();
                    for tbl in self.fetch_tables(db)? {
                        entries.push((FileType::Directory, tbl));
                    }
                }
                PlanKind::ListPartitions => {
                    let db = plan.db.as_deref().unwrap();
                    let tbl = plan.table.as_deref().unwrap();
                    // Always-present meta + AI files + all.tsv. Order
                    // matters only for human aesthetics — kernel sorts.
                    entries.push((FileType::RegularFile, ".schema".to_string()));
                    entries.push((FileType::RegularFile, "README.md".to_string()));
                    entries.push((FileType::RegularFile, "all.tsv".to_string()));
                    // Partitions (may be empty for non-partitioned tables).
                    match self.fetch_partitions(db, tbl) {
                        Ok(parts) => {
                            for p in parts {
                                entries.push((FileType::RegularFile, format!("{}.tsv", p)));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "clickfs::fs", %db, %tbl, error = %e, "partition list failed");
                            // Non-fatal: still show .schema and all.tsv.
                        }
                    }
                }
                _ => return Err(ClickFsError::NotADir),
            }
            Ok(())
        })();

        if let Err(e) = res {
            reply.error(e.to_errno());
            return;
        }

        // Allocate inodes lazily so that subsequent lookup() returns same ino.
        for (i, (kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let child_ino = if name == "." || name == ".." {
                ino
            } else {
                self.inodes.allocate(&path.join(name))
            };
            let next_offset = (i + 1) as i64;
            if reply.add(child_ino, next_offset, *kind, name) {
                break; // buffer full
            }
        }

        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // Reject any write intent.
        let access_mode = flags & libc::O_ACCMODE;
        if access_mode != libc::O_RDONLY {
            reply.error(libc::EROFS);
            return;
        }

        let path = match self.path_of(ino) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };
        let plan = match resolver::resolve(&path) {
            Ok(p) => p,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };

        let fh_id = self.fh_counter.fetch_add(1, Ordering::Relaxed);

        let handle = match &plan.kind {
            PlanKind::DescribeTable => {
                let db = plan.db.as_deref().unwrap();
                let tbl = plan.table.as_deref().unwrap();
                match self.fetch_describe(db, tbl) {
                    Ok(text) => FileHandle::Special {
                        bytes: Arc::new(text.into_bytes()),
                    },
                    Err(e) => {
                        reply.error(e.to_errno());
                        return;
                    }
                }
            }
            PlanKind::Readme => {
                let db = plan.db.as_deref().unwrap().to_string();
                let tbl = plan.table.as_deref().unwrap().to_string();
                let driver = self.driver.clone();
                // Render synchronously on the runtime. README is small
                // (a few KB) and we want a deterministic file size at
                // open() time so cat / cursor / glob behave normally.
                // The 5 sub-queries inside render() run concurrently.
                let text = self
                    .rt
                    .block_on(async move { crate::readme::render(&driver, &db, &tbl).await });
                FileHandle::Special {
                    bytes: Arc::new(text.into_bytes()),
                }
            }
            PlanKind::StreamAll => {
                let db = plan.db.as_deref().unwrap();
                let tbl = plan.table.as_deref().unwrap();
                let sql = driver::sql_stream_all(db, tbl);
                if self.tail_cfg.enabled {
                    // Resolve a usable ORDER BY expression for tail-mode
                    // synthesis. One extra round-trip per open(); cheap.
                    let order_expr = self.fetch_tail_order_expr(db, tbl);
                    let tail = TailContext {
                        db: db.to_string(),
                        tbl: tbl.to_string(),
                        order_expr,
                        partition: None,
                        cfg: self.tail_cfg.clone(),
                    };
                    let s = StreamHandle::spawn(&self.rt, self.driver.clone(), sql, Some(tail));
                    FileHandle::Stream(Arc::new(s))
                } else {
                    let s = StreamHandle::spawn(&self.rt, self.driver.clone(), sql, None);
                    FileHandle::Stream(Arc::new(s))
                }
            }
            PlanKind::StreamPartition(part) => {
                let db = plan.db.as_deref().unwrap();
                let tbl = plan.table.as_deref().unwrap();
                let sql = driver::sql_stream_partition(db, tbl, part);
                if self.tail_cfg.enabled {
                    // Same path as StreamAll, but with a partition filter
                    // baked into the tail SELECT so `tail 20260503.tsv`
                    // works on per-partition pseudo-files.
                    let order_expr = self.fetch_tail_order_expr(db, tbl);
                    let tail = TailContext {
                        db: db.to_string(),
                        tbl: tbl.to_string(),
                        order_expr,
                        partition: Some(part.clone()),
                        cfg: self.tail_cfg.clone(),
                    };
                    let s = StreamHandle::spawn(&self.rt, self.driver.clone(), sql, Some(tail));
                    FileHandle::Stream(Arc::new(s))
                } else {
                    let s = StreamHandle::spawn(&self.rt, self.driver.clone(), sql, None);
                    FileHandle::Stream(Arc::new(s))
                }
            }
            _ => {
                reply.error(libc::EISDIR);
                return;
            }
        };

        self.handles.lock().unwrap().insert(fh_id, handle);
        // FOPEN_DIRECT_IO disables kernel page cache for stream files,
        // which is what we want (sequential, dynamic content).
        reply.opened(fh_id, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Snapshot the handle (cheap Arc clone) and release the outer lock
        // immediately so other FUSE workers can proceed in parallel.
        let snapshot: Option<FileHandle> = {
            let map = self.handles.lock().unwrap();
            map.get(&fh).map(|h| match h {
                FileHandle::Stream(s) => FileHandle::Stream(Arc::clone(s)),
                FileHandle::Special { bytes } => FileHandle::Special {
                    bytes: Arc::clone(bytes),
                },
            })
        };

        let Some(handle) = snapshot else {
            reply.error(libc::EBADF);
            return;
        };

        match handle {
            FileHandle::Special { bytes } => {
                let off = offset.max(0) as usize;
                if off >= bytes.len() {
                    reply.data(&[]);
                    return;
                }
                let end = std::cmp::min(off + size as usize, bytes.len());
                reply.data(&bytes[off..end]);
            }
            FileHandle::Stream(s) => {
                match s.read_blocking(&self.rt, offset.max(0) as u64, size as usize) {
                    Ok(buf) => reply.data(&buf),
                    Err(e) => {
                        tracing::debug!(target: "clickfs::fs", fh, error = %e, "read failed");
                        reply.error(e.to_errno());
                    }
                }
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut map = self.handles.lock().unwrap();
        // Removing drops the handle (and any Arc<StreamHandle> inside);
        // for streams we also notify the producer task to stop early.
        if let Some(FileHandle::Stream(s)) = map.remove(&fh) {
            s.cancel();
        }
        reply.ok();
    }

    // --- Read-only enforcement: reject all mutating operations. ---

    // --- Read-only stubs ---
    //
    // The filesystem is mounted read-only; every mutating fuser op short-
    // circuits to EROFS. The macro keeps the surface honest without 90
    // lines of identical bodies.
    erofs_stub!(
        fn setattr(
            _ino: u64,
            _mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            _size: Option<u64>,
            _atime: Option<fuser::TimeOrNow>,
            _mtime: Option<fuser::TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<u64>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<u32>,
        ) -> ReplyAttr
    );
    erofs_stub!(
        fn mknod(_parent: u64, _name: &OsStr, _mode: u32, _umask: u32, _rdev: u32) -> ReplyEntry
    );
    erofs_stub!(fn mkdir(_parent: u64, _name: &OsStr, _mode: u32, _umask: u32) -> ReplyEntry);
    erofs_stub!(fn unlink(_parent: u64, _name: &OsStr) -> ReplyEmpty);
    erofs_stub!(fn rmdir(_parent: u64, _name: &OsStr) -> ReplyEmpty);
    erofs_stub!(
        fn rename(
            _parent: u64,
            _name: &OsStr,
            _newparent: u64,
            _newname: &OsStr,
            _flags: u32,
        ) -> ReplyEmpty
    );
    erofs_stub!(
        fn create(
            _parent: u64,
            _name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
        ) -> fuser::ReplyCreate
    );
    erofs_stub!(
        fn write(
            _ino: u64,
            _fh: u64,
            _offset: i64,
            _data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
        ) -> fuser::ReplyWrite
    );

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        // Pseudo capacity so `df` doesn't choke. Stay well under i64::MAX so
        // that tools like macOS `df` (which prints these as signed) don't
        // wrap to negative. 1 PiB-ish total / 1 PiB-ish free is plenty.
        const PSEUDO_BLOCKS: u64 = 1u64 << 38; // ~256G blocks * 4096 = 1 PiB
        const PSEUDO_FILES: u64 = 1u64 << 32; // ~4 billion; safe for i64 display
        let used_files = self.handles.lock().unwrap().len() as u64 + 1024;
        reply.statfs(
            PSEUDO_BLOCKS,                           // blocks
            PSEUDO_BLOCKS,                           // bfree
            PSEUDO_BLOCKS,                           // bavail
            PSEUDO_FILES,                            // files (total)
            PSEUDO_FILES.saturating_sub(used_files), // ffree
            BLOCK_SIZE,                              // bsize
            255,                                     // namelen
            BLOCK_SIZE,                              // frsize
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CompressionConfig, HttpDriver, TailConfig, TlsConfig};
    use url::Url;

    fn unreachable_driver() -> HttpDriver {
        // Port 1 is reserved & nothing listens — any query will fail
        // fast with connection refused.
        HttpDriver::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            "u".into(),
            "p".into(),
            5,
            1024,
            TlsConfig::default(),
            CompressionConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn new_does_not_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fs = ClickFs::new(
            unreachable_driver(),
            rt.handle().clone(),
            TailConfig::default(),
        );
        assert!(fs.db_cache.get(&()).is_none());
    }
}
