# 路径映射模型 (path-mapping)

> 隶属：[ARCHITECTURE.md §3](./ARCHITECTURE.md)
> 范围：路径语法、路径 → QueryPlan 映射、Inode 分配、缓存失效信号

---

## 1. 路径语法 (BNF)

```ebnf
mount_path    = "/" namespace [ "/" rest ] ;
namespace     = "db" | "sys" ;

(* /db 命名空间 *)
db_path       = "db"
              | "db/" database
              | "db/" database "/" table
              | "db/" database "/" table "/" entry ;

entry         = data_file | meta_file ;
data_file     = partition_file | "all.tsv" ;
partition_file= partition_id ".tsv" ;
meta_file     = ".schema" | ".stats" | ".hint" | ".explain" ;

partition_id  = ? 1..=128 字符，匹配 [A-Za-z0-9._-]+ ?
database      = ? CH 合法 identifier，1..=192 字符 ? ;
table         = ? 同上 ? ;

(* /sys 命名空间 *)
sys_path      = "sys/clickfs/" sys_entry ;
sys_entry     = "stats" | "queries" | "queries/" query_id
              | "cache" | "errors" | "config" | "version" ;
query_id      = ? 32 hex 字符（UUIDv4 去掉横线） ? ;
```

**说明**：

- 顶层只暴露 `/db` 和 `/sys` 两个命名空间，其余路径返回 `ENOENT`
- `database` / `table` 名先做 ClickHouse identifier 合法性校验（字符集 `[A-Za-z0-9_]`，首字符非数字；超出走 `\`...\`` 反引号转义路径）；不合法名直接 `ENOENT`，不下推到 CH
- `partition_id` 直接来自 `system.parts.partition_id`，不做语义解析，按字符串透传

## 2. 标准路径表

| 路径 | 类型 | 内容 | 来源查询 |
|------|------|------|----------|
| `/db` | dir | 数据库列表 | `SELECT name FROM system.databases WHERE name NOT IN ('system','INFORMATION_SCHEMA','information_schema')` |
| `/db/<db>` | dir | 表列表 | `SELECT name FROM system.tables WHERE database = ?` |
| `/db/<db>/<tbl>` | dir | 分区文件 + 元数据文件 | `SELECT DISTINCT partition_id FROM system.parts WHERE database=? AND table=? AND active` |
| `/db/<db>/<tbl>/.schema` | file (special) | DESCRIBE 输出（TSV） | `DESCRIBE TABLE <db>.<tbl>` |
| `/db/<db>/<tbl>/.stats` | file (special) | 行数/字节/分区数 | `SELECT sum(rows), sum(bytes_on_disk), uniqExact(partition_id), max(modification_time) FROM system.parts WHERE database=? AND table=? AND active` |
| `/db/<db>/<tbl>/.hint` | file (special) | 静态生成的 AI 友好提示 | 无（基于 schema 文本拼接） |
| `/db/<db>/<tbl>/.explain` | file (special) | 最近一次该表查询的执行计划 | 进程内缓存 |
| `/db/<db>/<tbl>/<part>.tsv` | file (stream) | 分区数据 | `SELECT * FROM <db>.<tbl> WHERE _partition_id = '<part>' FORMAT TSVWithNames` |
| `/db/<db>/<tbl>/all.tsv` | file (stream) | 全表（**仅当表无 PARTITION BY**） | `SELECT * FROM <db>.<tbl> FORMAT TSVWithNames` |
| `/sys/clickfs/stats` | file (special) | 概览 | 进程内 |
| `/sys/clickfs/queries` | dir | 当前在跑的查询 | 进程内 |
| `/sys/clickfs/queries/<id>` | file (special) | 单查询详情 | 进程内 |
| `/sys/clickfs/cache` | file (special) | 缓存命中率 | 进程内 |
| `/sys/clickfs/errors` | file (special) | 最近 100 条错误 | 进程内 |
| `/sys/clickfs/config` | file (special) | 当前生效配置 | 进程内 |
| `/sys/clickfs/version` | file (special) | 构建版本 | 进程内 |

### 2.1 `all.tsv` 与 `<part>.tsv` 互斥规则

| 表是否有 PARTITION BY | `<part>.tsv` 出现 | `all.tsv` 出现 |
|---|---|---|
| 有 | ✅ 每个 active partition 一个 | ❌ |
| 无 | ❌ | ✅ |

理由：避免 AI 在分区表上 `cat all.tsv` 不小心扫全表；分区表想要全表就显式 `cat *.tsv`。

### 2.2 元数据文件命名规则

所有元数据文件以 `.` 开头，符合 Unix dotfile 习惯：
- 默认 `ls` 不显示，避免污染 AI 看到的列表
- `ls -a` 显示，AI 主动探索时可见
- 防止与真实分区名冲突（`partition_id` 不允许以 `.` 开头）

## 3. 路径 → QueryPlan 映射

### 3.1 QueryPlan 数据结构

```text
QueryPlan {
    kind: PlanKind,
    db:   Option<String>,
    tbl:  Option<String>,
    extra: PlanExtra,
}

PlanKind {
    ListDatabases,
    ListTables,
    ListPartitions,         // 用于 readdir 分区列表
    DescribeTable,          // .schema
    TableStats,             // .stats
    StaticHint,             // .hint
    LastExplain,            // .explain
    StreamPartition(String),// <part>.tsv
    StreamAll,              // all.tsv
    Sys(SysKind),           // /sys/clickfs/...
}
```

### 3.2 解析算法

```text
fn resolve(path: &Path) -> Result<QueryPlan, ResolveError>:
    components = path.split('/').filter(|s| !s.is_empty())
    match components.as_slice():
        []                                  => Plan(ListDatabases)              // 实际是 root，readdir 时返回 ["db","sys"]
        ["db"]                              => Plan(ListDatabases)
        ["db", db]                          => Plan(ListTables, db)
        ["db", db, tbl]                     => Plan(ListPartitions, db, tbl)
        ["db", db, tbl, ".schema"]          => Plan(DescribeTable, db, tbl)
        ["db", db, tbl, ".stats"]           => Plan(TableStats, db, tbl)
        ["db", db, tbl, ".hint"]            => Plan(StaticHint, db, tbl)
        ["db", db, tbl, ".explain"]         => Plan(LastExplain, db, tbl)
        ["db", db, tbl, "all.tsv"]          => Plan(StreamAll, db, tbl)
        ["db", db, tbl, name] if name.ends_with(".tsv")
                                            => Plan(StreamPartition(strip_suffix), db, tbl)
        ["sys", "clickfs", rest @ ..]       => parse_sys(rest)
        _                                   => Err(NotFound)
```

### 3.3 校验时机

- **lookup 阶段**：仅做语法校验（字符集、长度），不连 ClickHouse
- **getattr 阶段**：通过元数据缓存判断 db/tbl/partition 是否存在
- **open 阶段**：如缓存未命中或已过期，发起 metadata 查询；表不存在 → `ENOENT`
- **read 阶段**：不再校验存在性（信任 open）

## 4. Inode 分配

### 4.1 设计目标

- 同一路径在同一进程生命周期内 inode 必须稳定（kernel 缓存依赖）
- inode 必须唯一（不同路径不能冲突）
- 重启后**不要求**保持稳定（kernel 会重新 lookup）

### 4.2 算法

```text
ino = 1 .. (reserved for root)
ino = 2 .. (reserved for /db)
ino = 3 .. (reserved for /sys)
ino = 4 .. (reserved for /sys/clickfs)
ino >= 100:
    取 ahash64(canonical_path) 高 63 位 + 1（避免 0 和保留 ID）
    维护 ino → path 的反查表（DashMap），冲突时线性探测 +1（极低概率）
```

**冲突处理**：64 位空间下 hash 冲突概率可忽略；维护反查表用于 `getattr(ino)` 重建 path。

### 4.3 反查表 (Inode Table)

```text
struct InodeTable {
    forward: DashMap<u64, Arc<PathBuf>>,   // ino → path
    reverse: DashMap<Arc<PathBuf>, u64>,   // path → ino
}
```

- 容量上限：100k entry，LRU 驱逐
- 驱逐策略：被驱逐的 ino 不会立即重用，留 grace 60s（应对 kernel 旧引用）
- 未在表中找到的 ino → `ESTALE`

## 5. 缓存与失效

### 5.1 路径相关的元数据缓存层级

| Key 形态 | TTL | 失效信号 |
|----------|-----|----------|
| `meta:db:list` | 30s | TTL only |
| `meta:tables:<db>` | 30s | TTL only |
| `meta:parts:<db>.<tbl>` | 5s | TTL + 后台 `system.parts` 版本轮询 |
| `meta:schema:<db>.<tbl>` | 60s | TTL only（schema 变更罕见） |
| `meta:stats:<db>.<tbl>` | 5s | TTL only |
| `inode:*` | 与上述 entry 同生命周期 | 上层失效连带失效 |

### 5.2 后台 partition 失效轮询

- 全局每 30s 拉取 `SELECT database, table, max(modification_time) FROM system.parts WHERE active GROUP BY database, table`
- 与上次结果 diff，发现变更则 invalidate 对应 `meta:parts:*`
- 若 `system.parts` 查询本身失败（CH 临时不可用），不清缓存（保留 last-known-good）

## 6. 路径示例与对应行为

```bash
# 示例 1：探索性 ls
$ ls /mnt/clickfs/db/
default  logs  metrics

$ ls /mnt/clickfs/db/logs/
app_prod  nginx  audit

$ ls /mnt/clickfs/db/logs/app_prod/
2026-04-30.tsv  2026-05-01.tsv  2026-05-02.tsv  2026-05-03.tsv

$ ls -a /mnt/clickfs/db/logs/app_prod/
.  ..  .schema  .stats  .hint  .explain
2026-04-30.tsv  2026-05-01.tsv  2026-05-02.tsv  2026-05-03.tsv
```

```bash
# 示例 2：理解 schema
$ cat /mnt/clickfs/db/logs/app_prod/.schema
name        type             default_kind  default_expression  comment
ts          DateTime64(3)
level       LowCardinality(String)
service     LowCardinality(String)
trace_id    String
message     String
host        LowCardinality(String)
```

```bash
# 示例 3：查询根因
$ grep "Database connection failed" /mnt/clickfs/db/logs/app_prod/2026-05-03.tsv | head -n 5
2026-05-03 10:14:22.331  ERROR  api  abc..  Database connection failed: timeout  pod-7
2026-05-03 10:14:22.452  ERROR  api  abc..  Database connection failed: refused  pod-7
...
```

```bash
# 示例 4：查询全过程审计
$ cat /mnt/clickfs/sys/clickfs/queries
id        path                                       state     elapsed_ms  bytes_out
9f3a..    /db/logs/app_prod/2026-05-03.tsv           streaming 1421        128974233
```

## 7. 与 ClickHouse 实际语义的对齐说明

| 现象 | 解释 |
|------|------|
| `ls` 显示的分区不一定按时间排序 | `partition_id` 是字符串，按字典序；如表用 `toYYYYMM(ts)` 分区，则恰好按时间序 |
| 同一分区在 `ls` 多次后大小变化 | 后台 merge / mutation 持续改动；`.stats` 数值刷新即可 |
| 删除分区后 `ls` 仍可见 | 受 5s `meta:parts` TTL 影响，最迟 5s 内消失 |
| 跨表 `cat a/x.tsv b/x.tsv` 列结构不同导致 awk 出错 | 由用户/AI 自行负责；ClickFS 不做合并 |
| 同名分区在 ReplicatedMergeTree 副本间存在 | `system.parts` 已按当前节点视图返回，不重复 |

## 8. 安全约束

- **路径遍历防御**：`..` 在 `Path` 解析阶段即 `ENOENT`（不允许跳出 `/db` 或 `/sys`）
- **identifier 注入防御**：所有 db/table/partition 进入 SQL 前必须经过 `quote_identifier()`（反引号转义）和 `quote_string()`（单引号转义）；详见 [query-construction.md](./query-construction.md) §4
- **隐藏系统库**：`system` / `information_schema` 默认从 `/db` 隐藏，可通过 `--show-system` 显示
