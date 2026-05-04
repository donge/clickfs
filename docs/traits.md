# 核心 Trait 定义（附录 B）

> 隶属：[ARCHITECTURE.md 附录 B](./ARCHITECTURE.md)
> 范围：跨 crate 的契约接口；仅给出签名 + 语义说明，不含实现

---

## 1. clickfs-vfs

### 1.1 `InodeKind`

```rust
pub enum InodeKind {
    Dir,
    StreamFile,   // <part>.tsv / all.tsv
    SpecialFile,  // .schema / .stats / .hint / .explain / sysfs
}

pub struct InodeMeta {
    pub ino:        u64,
    pub kind:       InodeKind,
    pub size_hint:  u64,        // getattr 返回值，估算
    pub mtime:      SystemTime,
    pub mode:       u16,        // 0o555 / 0o444
    pub uid:        u32,
    pub gid:        u32,
}
```

### 1.2 `SpecialFile`

供非数据文件（meta / sysfs）注册到 VFS：

```rust
#[async_trait::async_trait]
pub trait SpecialFile: Send + Sync + 'static {
    /// 路径，必须以 '/' 开头，相对挂载根
    fn path(&self) -> &str;

    /// `getattr` 用，可不精确（≤ 4 KiB 通常给真实长度）
    fn size_hint(&self) -> u64;

    /// `cat` 时调用。返回完整内容；调用方负责对 offset/size 做切片。
    /// 不在这里返回流是因为 special file 内容总是小且整体生成。
    async fn render(&self, ctx: &RenderCtx) -> Result<Bytes, ClickFsError>;
}

pub struct RenderCtx<'a> {
    pub driver: &'a dyn Driver,
    pub cache:  &'a dyn MetaCache,
    pub stats:  &'a RuntimeStats,
}
```

### 1.3 `SpecialFileRegistry`

```rust
pub trait SpecialFileRegistry: Send + Sync {
    fn register(&self, file: Arc<dyn SpecialFile>);
    fn lookup(&self, path: &str) -> Option<Arc<dyn SpecialFile>>;
    fn list_under(&self, dir: &str) -> Vec<Arc<dyn SpecialFile>>;
}
```

obs / hint / schema / stats 的实现都在自身 crate，启动时 `register` 进 VFS。

### 1.4 `InodeTable`

```rust
pub trait InodeTable: Send + Sync {
    fn allocate(&self, path: &Path) -> u64;
    fn lookup(&self, ino: u64) -> Option<Arc<PathBuf>>;
    fn forget(&self, ino: u64, lookups: u64);
}
```

## 2. clickfs-resolver

```rust
pub enum PlanKind {
    ListDatabases,
    ListTables,
    ListPartitions,
    DescribeTable,
    TableStats,
    StaticHint,
    LastExplain,
    StreamPartition(String),
    StreamAll,
    Sys(SysKind),
}

pub struct QueryPlan {
    pub kind:  PlanKind,
    pub db:    Option<String>,
    pub table: Option<String>,
}

pub trait Resolver: Send + Sync {
    fn resolve(&self, path: &Path) -> Result<QueryPlan, ResolveError>;
}

pub enum ResolveError {
    NotFound,
    NotADir,
    InvalidIdentifier,
    Reserved,
}
```

`Resolver` 只做语法解析，**不**访问 ClickHouse、不查缓存。

## 3. clickfs-driver

```rust
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// 一次性查询：返回全部行的二进制 buffer（小结果集）
    async fn query_buffered(&self, sql: &Sql) -> Result<Bytes, QueryError>;

    /// 流式查询：返回字节流（数据文件）
    /// drop 返回的 stream 等同于 cancel。
    async fn query_stream(&self, sql: &Sql, query_id: Uuid) -> Result<RowStream, QueryError>;

    /// 显式 cancel（HTTP 模式或外部调用）
    async fn kill(&self, query_id: Uuid) -> Result<(), QueryError>;

    fn protocol(&self) -> Protocol;  // Native / Http
}

pub struct Sql {
    pub text:     String,
    pub settings: SettingsBlob,
    pub query_id: Uuid,
}

pub type RowStream = Pin<Box<dyn Stream<Item = Result<Bytes, QueryError>> + Send>>;

pub enum QueryError {
    Connect(io::Error),
    Auth,
    Server { code: i32, message: String },
    Cancelled,
    Timeout,
    Protocol(String),
}
```

## 4. clickfs-cache

```rust
#[async_trait::async_trait]
pub trait MetaCache: Send + Sync {
    async fn get_or_query<F, Fut, V>(
        &self,
        key: MetaKey,
        ttl: Duration,
        loader: F,
    ) -> Result<Arc<V>, ClickFsError>
    where
        V: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<V, ClickFsError>> + Send;

    fn invalidate(&self, key: &MetaKey);
    fn invalidate_prefix(&self, prefix: &MetaKey);

    fn stats(&self) -> CacheStats;
}

pub enum MetaKey {
    DbList,
    TableList { db: String },
    PartitionList { db: String, table: String },
    Schema { db: String, table: String },
    Stats { db: String, table: String },
    HasPartitionBy { db: String, table: String },
}

pub struct CacheStats {
    pub entries:   u64,
    pub capacity:  u64,
    pub hit:       u64,
    pub miss:      u64,
}

/// v1 不实现，仅占位
pub trait DataCache: Send + Sync {
    fn get(&self, key: &DataKey) -> Option<Bytes>;
    fn put(&self, key: DataKey, val: Bytes);
}
```

## 5. clickfs-query

```rust
pub trait SqlBuilder: Send + Sync {
    fn build(&self, plan: &QueryPlan, settings: &QuerySettings) -> Sql;
}

pub struct QuerySettings {
    pub max_execution_time:  Duration,
    pub max_result_bytes:    u64,
    pub max_memory_usage:    u64,
    pub readonly:            u8,
}

#[async_trait::async_trait]
pub trait StreamingExecutor: Send + Sync {
    async fn open(&self, plan: QueryPlan, fh: u64) -> Result<StreamHandle, ClickFsError>;
}

pub struct StreamHandle {
    pub query_id:  Uuid,
    pub cancel:    CancellationToken,
    pub join:      JoinHandle<Result<(), QueryError>>,
    pub ring:      Arc<RingBuffer>,
}

pub trait RingBuffer: Send + Sync {
    /// 写入端：调用方为 streaming task
    fn try_push(&self, chunk: Bytes) -> PushOutcome;

    /// 读取端：调用方为 fuse worker（同步或 block_on）
    fn pull(&self, n: usize, deadline: Instant) -> PullOutcome;

    fn mark_eof(&self);
    fn mark_error(&self, err: QueryError);
}

pub enum PushOutcome { Ok, Cancelled }
pub enum PullOutcome { Bytes(Bytes), Eof, Err(QueryError), Timeout }
```

## 6. clickfs-fuse

```rust
pub struct ClickFs {
    pub vfs:        Arc<dyn VfsRoot>,
    pub resolver:   Arc<dyn Resolver>,
    pub executor:   Arc<dyn StreamingExecutor>,
    pub specials:   Arc<dyn SpecialFileRegistry>,
    pub stats:      Arc<RuntimeStats>,
    pub runtime:    Arc<tokio::runtime::Runtime>,
}

// 实现 fuser::Filesystem
impl fuser::Filesystem for ClickFs { /* lookup/getattr/readdir/open/read/release/... */ }
```

`VfsRoot` 是 vfs crate 暴露的统一入口，封装 inode_table + special registry：

```rust
pub trait VfsRoot: Send + Sync {
    fn meta(&self, ino: u64) -> Option<InodeMeta>;
    fn meta_by_path(&self, path: &Path) -> Option<InodeMeta>;
    fn allocate_inode(&self, path: &Path, meta: InodeMeta) -> u64;
    fn list_dir(&self, ino: u64) -> Result<Vec<DirEntry>, ClickFsError>;
}
```

## 7. clickfs-obs

```rust
pub trait ObsSink: Send + Sync {
    fn record_op(&self, op: FuseOp, path: &Path, ino: u64);
    fn record_query_started(&self, query_id: Uuid, plan: &QueryPlan, sql: &str);
    fn record_query_progress(&self, query_id: Uuid, bytes: u64);
    fn record_query_finished(&self, query_id: Uuid, status: QueryStatus, elapsed: Duration);
    fn record_error(&self, err: &ClickFsError, ctx: ErrorCtx);
}

pub struct RuntimeStats {
    /* 见 observability.md §3.2 字段 */
}
```

`ObsSink` 实现同时：
- 写 tracing event
- 推送 errors ring buffer
- 更新 sysfs 输出快照

## 8. clickfs-cli

无 trait，仅 main 与 config 解析：

```rust
pub fn run(args: CliArgs) -> ExitCode;

pub struct CliArgs {
    pub mountpoint:    PathBuf,
    pub clickhouse_url:String,
    pub config:        Option<PathBuf>,
    pub foreground:    bool,
    pub allow_other:   bool,
    pub allow_restart: bool,
    pub max_concurrent:Option<usize>,
    pub query_timeout: Option<u64>,
    pub log:           Option<String>,
}
```

## 9. 错误层级（统一）

```rust
#[derive(thiserror::Error, Debug)]
pub enum ClickFsError {
    #[error("path not found")]
    NotFound,

    #[error("not a directory")]
    NotADir,

    #[error("read-only filesystem")]
    ReadOnly,

    #[error("read sequence violation: expected offset {expected}, got {got}")]
    ReadOrder { expected: u64, got: u64 },

    #[error("budget exhausted: {0:?}")]
    Budget(BudgetKind),

    #[error(transparent)]
    Resolve(#[from] ResolveError),

    #[error(transparent)]
    Query(#[from] QueryError),

    #[error("operation cancelled")]
    Cancelled,

    #[error("internal: {0}")]
    Internal(String),
}

pub enum BudgetKind { Concurrent, MemoryRing, ResultBytes }
```

`ClickFsError` 在 fuse 层有 `to_errno() -> i32`，保证 errno 映射唯一来源。

## 10. 模块依赖回顾

```text
clickfs-cli   →  clickfs-fuse, clickfs-obs
clickfs-fuse  →  clickfs-vfs, clickfs-resolver, clickfs-query, clickfs-obs
clickfs-vfs   →  (no inner deps; defines traits)
clickfs-resolver → clickfs-vfs (types)
clickfs-query →  clickfs-driver, clickfs-cache, clickfs-vfs(types)
clickfs-cache →  clickfs-driver, clickfs-vfs(types)
clickfs-driver→  (no inner deps; defines Driver trait + impls)
clickfs-obs   →  clickfs-vfs (实现 SpecialFile)
```

无环。所有「下层 crate 提供 trait，上层注入实现」。
