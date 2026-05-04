# ClickFS 测试报告

- 测试日期: 2026-05-04
- 版本: **v0.2.0** (`clickfs 0.2.0`, commit `e6450e1`)
- 测试机: macOS (darwin), macFUSE 已安装 (libfuse 2.9.9)
- 二进制: `target/release/clickfs` (release build)
- ClickHouse: 25.3.2.39 @ http://127.0.0.1:8123, 用户 `default`
- 挂载点: `~/mnt/ch-e2e`
- 后台日志: `/tmp/clickfs-e2e/mount.log`

---

## 一句话结论

**全绿。** 单元 47/47 通过,e2e 45/45 通过,clippy `-D warnings` 干净,fmt 干净,
`--no-default-features` (无 fuse 特性) 也能 `cargo check` 通过 (CI 在 macOS 无 macFUSE 主机上验证)。
v0.1.0 报告里发现的 4 个 bug **已全部修复并由 e2e 守护**,详见下文"v0.1 → v0.2 修复"。

---

## 总览

| 套件 | 通过 | 失败 | 跳过 | 备注 |
|---|---:|---:|---:|---|
| `cargo test --release` (47) | **47** | 0 | 0 | unit + integration |
| `tests/e2e.sh` (45 cases / 8 phases) | **45** | 0 | 0 | 真实 ClickHouse + macFUSE 挂载 |
| `cargo clippy --release --all-targets -- -D warnings` | ✅ | — | — | 无任何警告 |
| `cargo fmt --check` | ✅ | — | — | |
| `cargo check --no-default-features` | ✅ | — | — | macOS CI 守护;**v0.2.0 临时回归已修复** (commit `e6450e1`) |
| GitHub Actions CI (`main`) | ✅ | — | — | run id `25297678071`,3/3 jobs |
| GitHub Actions release (tag `v0.2.0`) | ✅ | — | — | run id `25297715883`,4/4 jobs;6 个发布物上传完毕 |

---

## 单元/集成测试 (47/47)

来源:`cargo test --release --locked`,运行 0.21s。

| 模块 | 用例数 | 内容要点 |
|---|---:|---|
| `cache` | 8 | TTL 命中/过期、refresh 重置计时、并发安全 |
| `driver` | 7 | URL 构造、密码 URL 编码、`SETTINGS readonly=2 max_execution_time=60`、`enable_http_compression=1`、TLS option 互斥、`KILL QUERY` SQL 形态 |
| `error` | 4 | ClickHouse exception 解析(已知 code → 名字;未知 code → `Unknown`) |
| `fs` | 2 | `--cache-ttl-ms 0` 不 panic 且跳过预取;非零 TTL 时后台预取失败也不 panic |
| `inode` | 3 | inode 分配稳定性、不同路径返回不同 inode、根 inode 固定 |
| `readme` | 5 | `human_bytes` 单位换算、`json_to_u64` 接受 int/string、`render_stats` 在有/无 parts 两路径输出正确、`render_columns` 正确转义 `\|` |
| `resolver` | 10 | 路径解析:root、db、table、`.schema`、`README.md`、`head.ndjson`、`all.tsv`、partition `*.tsv`、拒绝 `..`、拒绝非法 ID |
| `stream` | 4 | reverse-seek 仅 warn 一次后返回 EIO、顺序读取不 warn、不同 handle 各自分配 query_id、query_id 以 `clickfs-` 为前缀 |

---

## 端到端测试 (45/45)

来源:`CH_PASSWORD=*** CLICKFS_PASSWORD=*** bash tests/e2e.sh`。

### Phase 1 — 目录列举与不存在路径 (8/8)
| # | 用例 | 结果 |
|---|---|---|
| T01 | 根目录列出 `db` | ✅ |
| T02 | `ls /db/` 列出可见数据库 | ✅ |
| T03 | `ls /db/default/` 列出表 | ✅ |
| T04 | `ls /db/system/` 直接访问 | ✅ |
| T05 | `ls` 不存在的 db → ENOENT | ✅ **(原 Bug #1 修复)** |
| T06 | `stat` 不存在的 db → ENOENT | ✅ |
| T07 | `ls` 不存在的表 → ENOENT | ✅ |
| T08 | `cat` 不存在分区 → ENOENT | ✅ |

### Phase 2 — 元数据与基础读取 (4/4)
| # | 用例 | 结果 |
|---|---|---|
| T09 | `.schema` 含 `CREATE TABLE` | ✅ **(原 Bug #4 修复:已改为 SHOW CREATE TABLE)** |
| T10 | `head -n 3 all.tsv` | ✅ |
| T11 | `wc -l all.tsv` | ✅ |
| T12 | `all.tsv` 在目录里恰好出现一次 | ✅ **(原 Bug #3 修复)** |

### Phase 3 — 流式读取与并发 (3/3)
| # | 用例 | 结果 |
|---|---|---|
| T13 | `head -c 4096` 流式取 system.tables 准确 4096 字节 | ✅ |
| T14 | `head -c 100` 中段取消 100 字节并触发 `cancelled` | ✅ |
| T15 | 3 个 head 并发读不同表 | ✅ |

### Phase 4 — 反向 seek、TLS、新功能 (15/15)
| # | 用例 | 结果 |
|---|---|---|
| T31 | reverse pread → EIO | ✅(strict-sequential 设计) |
| T31a | mount.log 含 `reverse seek` 警告 | ✅ |
| T32 | `--insecure` 与 `--ca-bundle` 互斥(clap) | ✅ |
| T33 | `--no-compression` 标志被识别 | ✅ |
| T33a | 默认挂载下 SQL 带 `enable_http_compression=1` | ✅ |
| T34 | mount-time 预取热身 db cache | ✅ |
| T35 | 流读取的 query_id 以 `clickfs-` 为前缀 | ✅ |
| T35a | 提早关闭触发 `KILL QUERY`(best-effort) | ✅ |
| T36 | `README.md` 在表目录中 | ✅ |
| T36a–e | README 含 Stats / Schema / Columns / Sample / Files 5 段 | ✅×5 |
| T37 | `head.ndjson` 在表目录中 | ✅ |
| T37a | `head.ndjson` 每行合法 JSON 且行数 ≤ 100 | ✅ |
| T37b | `head.ndjson` LIMIT 100 强制截断 | ✅ |

### Phase 5 — Cache (2/2)
| # | 用例 | 结果 |
|---|---|---|
| T27 | `--cache-ttl-ms` 标志被识别 | ✅ |
| T28 | 启用缓存后重复 readdir 查询次数显著减少 | ✅ |

### Phase 6 — EROFS (5/5)
| # | 用例 | 结果 |
|---|---|---|
| T16 | `mkdir` → EROFS | ✅ |
| T17 | `touch` → EROFS | ✅ |
| T18 | `echo > file` → EROFS | ✅ |
| T19 | `rm` → EROFS | ✅ |
| T20 | `chmod` → EROFS | ✅ |

### Phase 7 — 元数据 (4/4)
| # | 用例 | 结果 |
|---|---|---|
| T21 | `df -h` 工作 | ✅ 显示 `Size 1.0Pi` **(原 Bug #2 修复)** |
| T22 | `df` 输出无负数 | ✅ |
| T23 | `stat` 根目录 | ✅ `dr-xr-xr-x` |
| T24 | `find -maxdepth 2 -type d` | ✅ |

### Phase 8 — 卸载 (2/2)
| # | 用例 | 结果 |
|---|---|---|
| T25 | SIGTERM 优雅卸载 | ✅ 1 秒内卸载 |
| T26 | clickfs 进程退出 | ✅ |

---

## v0.1 → v0.2 修复(回归历史)

v0.1.0 测试报告里发现的 4 个 bug,在 v0.2.0 全部修复并各自配套 e2e 守护:

| 旧 Bug | 现状 | 守护用例 |
|---|---|---|
| #1 不存在的 db/table 不返回 ENOENT | ✅ 修复(`fs.rs::lookup` 走存在性校验 + cache) | T05–T08 |
| #2 `df` 显示负值 | ✅ 修复(`statfs` 改用合理伪容量) | T21, T22 |
| #3 单分区表 `all.tsv` 重名 | ✅ 修复(`fetch_partitions` 过滤 `partition_id == "all"`) | T12 |
| #4 `.schema` 与 README 描述不符 | ✅ 修复(`.schema` 现为 `SHOW CREATE TABLE` DDL) | T09 |

---

## CI / Release 状态

- **CI** (`.github/workflows/ci.yml`,run `25297678071`,commit `e6450e1`)
  - rustfmt: ✅ 14s
  - build + test (macOS, no FUSE,`cargo check --no-default-features`): ✅ 58s
  - build + test (Linux,full features + clippy): ✅ 1m9s
- **Release** (`.github/workflows/release.yml`,tag `v0.2.0`,run `25297715883`)
  - build linux-x86_64: ✅ 1m29s
  - build linux-aarch64: ✅ 2m10s
  - build macos-arm64: ✅ 1m12s
  - publish release: ✅ 6s
- **发布物** (https://github.com/donge/clickfs/releases/tag/v0.2.0)
  - `clickfs-v0.2.0-linux-x86_64.tar.gz` + `.sha256`
  - `clickfs-v0.2.0-linux-aarch64.tar.gz` + `.sha256`
  - `clickfs-v0.2.0-macos-arm64.tar.gz` + `.sha256`

---

## 已知设计限制(非 bug)

- **strict-sequential 读取**:不支持反向 seek;`tail`、`less` 反向跳页会得 EIO,这是 v1 设计取舍(README 已说明);T31/T31a 守护此行为。
- **system / INFORMATION_SCHEMA 不出现在 `ls /db/`**:出于"用户库优先"过滤,但允许直接路径访问(`ls /db/system/` 工作);T04 守护。
- **`all.tsv` / `*.tsv` 在 `stat` 中报告极大伪 size** (`u64::MAX/2`):因为流式读取的真实长度需 SQL 才能得到,提前 `stat` 不便。`df` 已用单独的、合理的伪容量避免显示问题。

---

## 性能与日志观察

- 所有 SQL 都正确带 `SETTINGS max_execution_time=60, readonly=2`;启用压缩时还带 `enable_http_compression=1`(T33a 守护)。
- 流读取 cancel 链路:`head -c N` 关闭 fd 后,日志里几乎立刻出现 `clickfs::stream: cancelled`,且 `KILL QUERY clickfs-<uuid>` 在后台发出(T35a 守护)。
- mount-time 预取并发上限 8;TTL=0 时跳过(`fs::tests::new_with_ttl_zero_does_not_panic_and_skips_prefetch` 守护)。
- TTL 缓存让连续 5 次 `ls` 同一目录的 SQL 调用从 5 降到 ≤2(T28 守护)。

---

## 复现步骤

```bash
# 单元 + 集成
cargo test --release --locked

# 端到端(需要本地 ClickHouse + macFUSE)
CH_PASSWORD='***' CLICKFS_PASSWORD='***' bash tests/e2e.sh

# 静态检查
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo check --no-default-features
```

---

## 测试环境清理

挂载已干净卸载,无残留进程:

```
$ mount | grep clickfs && echo "STILL MOUNTED" || echo "no clickfs mounts"
no clickfs mounts
$ pgrep -f "target/release/clickfs mount" && echo "process alive" || echo "process gone"
process gone
```
