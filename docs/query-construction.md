# SQL 构造规则

> 隶属：[ARCHITECTURE.md §7](./ARCHITECTURE.md)
> 范围：QueryPlan → SQL 文本的所有规则、查询保护、注入防御

---

## 1. SQL 模板总览

| PlanKind | SQL 模板（占位符 `%I` = identifier，`%S` = string literal） |
|----------|---------------------------------------------|
| `ListDatabases` | `SELECT name FROM system.databases WHERE name NOT IN ('system','INFORMATION_SCHEMA','information_schema') ORDER BY name FORMAT TabSeparated` |
| `ListTables` | `SELECT name FROM system.tables WHERE database = %S ORDER BY name FORMAT TabSeparated` |
| `ListPartitions` | `SELECT DISTINCT partition_id FROM system.parts WHERE database = %S AND table = %S AND active ORDER BY partition_id FORMAT TabSeparated` |
| `DescribeTable` | `DESCRIBE TABLE %I.%I FORMAT TabSeparatedWithNames` |
| `TableStats` | 见 §2.1 |
| `StreamPartition(p)` | `SELECT * FROM %I.%I WHERE _partition_id = %S FORMAT TabSeparatedWithNames` |
| `StreamAll` | `SELECT * FROM %I.%I FORMAT TabSeparatedWithNames` |
| `HasPartitionBy`（决定 all.tsv 是否存在） | `SELECT partition_key FROM system.tables WHERE database = %S AND name = %S` |

## 2. 复杂查询展开

### 2.1 `.stats`

```sql
SELECT
    sum(rows)                         AS rows,
    sum(bytes_on_disk)                AS bytes_on_disk,
    sum(data_compressed_bytes)        AS bytes_compressed,
    sum(data_uncompressed_bytes)      AS bytes_uncompressed,
    uniqExact(partition_id)           AS partitions,
    min(min_time)                     AS min_time,
    max(max_time)                     AS max_time,
    max(modification_time)            AS last_modified
FROM system.parts
WHERE database = %S AND table = %S AND active
FORMAT TabSeparatedWithNames
```

输出文件内容为单行 TSV（带表头），便于 `awk` 直接拆。

### 2.2 `.hint` 静态生成

不下发到 CH，由 `.schema` 结果在本地拼接生成。模板示例：

```text
# Query hints for `<db>.<tbl>`

Schema columns:
  ts (DateTime64) -- usually order key
  level, service  -- LowCardinality, cheap to filter
  message         -- text, expensive to scan

Suggested SQL when grep is too slow:
  SELECT * FROM <db>.<tbl>
   WHERE _partition_id = '<partition>'
     AND level = 'ERROR'
     AND message LIKE '%<keyword>%'
   LIMIT 100
   FORMAT TabSeparatedWithNames

Tip: read `<partition>.tsv` instead of `all.tsv` to avoid full scan.
```

模板规则：
- 如发现 `LowCardinality(*)` 列 → 提示「适合用作过滤」
- 如发现 `String` 且名为 `message`/`msg`/`body` → 提示「全文搜索代价高」
- 如有 `DateTime*` 列 → 推断为时间序，给出按时间过滤建议

### 2.3 `.explain`

仅返回**最近一次**该表的实际下发 SQL 的 EXPLAIN PIPELINE 输出（进程内缓存，无该表查询时返回提示文字）：

```sql
EXPLAIN PIPELINE
<最近一次本表下发的 SELECT 语句>
FORMAT TabSeparatedRaw
```

## 3. 查询保护 (Settings)

所有下发到 CH 的查询必须追加以下 SETTINGS 子句（在 `FORMAT` 之前）：

```sql
SETTINGS
    max_execution_time = {query_timeout_secs},          -- 默认 60
    max_result_bytes   = {max_result_bytes},            -- 默认 1 GiB
    max_memory_usage   = {max_memory_usage},            -- 默认 4 GiB
    timeout_overflow_mode = 'throw',
    result_overflow_mode  = 'break',                    -- 超过 max_result_bytes 平稳 EOF（不报错）
    readonly = 2,                                       -- 强制只读，多一道防线
    output_format_tsv_crlf_end_of_line = 0
```

**特殊规则**：
- `ListDatabases` / `ListTables` / `ListPartitions` 不加 `max_result_bytes`（结果体积小但必须完整）
- `Stream*` 必加全部 settings；`result_overflow_mode='break'` 让大表 cat 到上限自然结束（用户看到的是部分数据 + EOF，比报错友好）
- 查询超时使用 `'throw'`，让 ring buffer 路径明确感知错误

## 4. 注入防御

### 4.1 quote_identifier

```text
fn quote_identifier(s: &str) -> String:
    assert!(s.len() <= 192)
    assert!(s.chars().all(|c| matches!(c, 'A'..='Z'|'a'..='z'|'0'..='9'|'_')))
    // 即便字符集已限制，仍主动加反引号 + escape backtick
    format!("`{}`", s.replace('`', "``"))
```

- 字符集限制在 path resolver 层就已强制（[path-mapping.md](./path-mapping.md) §1）
- SQL builder 层再做一次校验，防御深度

### 4.2 quote_string

```text
fn quote_string(s: &str) -> String:
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'")
    format!("'{}'", escaped)
```

用于 `WHERE database = %S` 等场景。

### 4.3 模板渲染

不使用字符串拼接，统一走类型化的 builder：

```rust
SqlBuilder::new()
    .select("*")
    .from_qualified(db, table)
    .where_eq_str("_partition_id", partition_id)
    .format("TabSeparatedWithNames")
    .with_default_settings(&cfg)
    .build()
```

`from_qualified` 内部调 `quote_identifier`，`where_eq_str` 内部调 `quote_string`，绝不暴露原始字符串拼接接口。

## 5. ClickHouse 协议选择

| 协议 | 端口 | ClickFS 使用 |
|------|------|--------------|
| Native TCP | 9000 / 9440 | **首选**：流式吞吐最高，cancel 语义最干净 |
| HTTP | 8123 / 8443 | 备选：`--protocol http` 显式启用，部分托管服务只暴露 HTTP |

切换通过配置 `[clickhouse].url` 的 scheme 自动推断（`tcp://` / `tcps://` / `http://` / `https://`）。

## 6. 取消查询的实现

### 6.1 Native 协议

`clickhouse` crate 的 streaming response drop 时会自动发送 `Cancel` packet。要点：
- 必须在 streaming task 的 `cancel_token` 触发时**立即** drop response（不要先 await 数据）
- 用 `tokio::select!{ _ = cancel.cancelled() => break, chunk = stream.next() => ... }`

### 6.2 HTTP 协议

HTTP 模式下 drop 连接 + 发送 `KILL QUERY WHERE query_id = '<id>'`：
- 每个 SELECT 都注入 `query_id = '<uuid>'`（来自 ClickFS 内部 fh id）
- cancel 时另起一个连接执行 `KILL QUERY ... SYNC`，超时 500ms

## 7. SQL 调试与审计

每条下发 SQL 都记录：
- `tracing::info!(target: "clickfs::sql", query_id, fh, path, sql, settings)`
- 写入 `/sys/clickfs/queries/<query_id>` 供 `cat` 读取（运行中）
- 进程内保留最近 100 条到 `/sys/clickfs/errors`（失败的）和 `~/.clickfs/history.sqlite`（如配置启用）

输出示例 (`cat /sys/clickfs/queries/9f3a...`)：

```text
query_id:    9f3a1b2c-...-...
path:        /db/logs/app_prod/2026-05-03.tsv
state:       streaming
started_at:  2026-05-03T10:14:11.221Z
elapsed_ms:  1421
bytes_out:   128974233
sql:         SELECT * FROM `logs`.`app_prod`
             WHERE _partition_id = '2026-05-03'
             FORMAT TabSeparatedWithNames
             SETTINGS max_execution_time=60, max_result_bytes=1073741824,
                      max_memory_usage=4294967296, timeout_overflow_mode='throw',
                      result_overflow_mode='break', readonly=2,
                      output_format_tsv_crlf_end_of_line=0
```

## 8. 已知限制 / 反模式

| 限制 | 说明 | 缓解 |
|------|------|------|
| Distributed 表的 `_partition_id` 可能不一致 | shard 间分区不同 | v1：建议针对底表查询；v0.3 探索 cluster-aware |
| ReplicatedMergeTree 的副本切换 | 同一 query_id 在副本切换时失效 | KILL QUERY 仅作用于当前副本，drop 连接更可靠 |
| MaterializedView 的 `_partition_id` | view 自身无分区 | resolver 检测到 view → 仅暴露 `all.tsv` |
| `Memory`、`Set`、`Join` 引擎 | 无 `system.parts` 行 | 仅暴露 `all.tsv` |
| `View`（普通 view） | 同上 | 同上 |
| 列名含大小写敏感字符 / 引号 | TSV 输出无歧义，但 `.schema` 列名展示需保真 | quote_identifier 已处理 |

## 9. 兼容性矩阵

| ClickHouse 版本 | 支持度 | 备注 |
|----------------|--------|------|
| 22.3+ (LTS) | ✅ 完整 | `system.parts.partition_id` 稳定 |
| 21.x | ⚠️ 部分 | `system.parts` 字段差异；建议 22+ |
| < 21 | ❌ 不支持 | |
| ClickHouse Cloud | ✅ | 通过 HTTPS + readonly user |
| chDB / clickhouse-local | ❌ | 无远程协议 |
