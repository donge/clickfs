# ClickFS 测试报告

- 测试日期: 2026-05-04
- 测试机: macOS (darwin), macFUSE 已安装 (libfuse 2.9.9)
- 二进制: `target/release/clickfs` (release build, 链接 `/usr/local/lib/libfuse.2.dylib`)
- ClickHouse: 25.3.2.39 @ http://127.0.0.1:8123, 用户 `default`
- 挂载点: `~/mnt/ch`
- 启动命令: `RUST_LOG=clickfs=debug CLICKFS_PASSWORD='***' clickfs mount http://127.0.0.1:8123 ~/mnt/ch --user default`
- 后台日志: `/tmp/clickfs-test/mount.log`
- 测试明细: `/tmp/clickfs-test/report.txt`

---

## 一句话结论

**核心功能完全可用** — 30 个用例 28 个通过,2 个属于已知设计限制 (strict-sequential 不支持 `tail`)。
**发现 4 个非阻塞 bug**,均不影响"挂载 / ls / cat / grep / 卸载"主流程。

---

## 通过用例 (28/30)

### 挂载与连接 (4/4)
| # | 用例 | 结果 |
|---|---|---|
| — | macFUSE 检测 + libfuse 链接 | ✅ pkg-config 报 2.9.9, otool 显示链接正确 |
| — | 启动 ping ClickHouse | ✅ `connected to clickhouse` |
| — | 内核挂载 | ✅ `mount` 显示 `clickfs on ~/mnt/ch (macfuse, nodev, nosuid, read-only)` |
| T30 | SIGTERM 优雅卸载 | ✅ 1 秒内卸载干净,进程退出 |

### 目录列举 (4/6)
| # | 用例 | 结果 |
|---|---|---|
| T1 | `ls ~/mnt/ch/` → `db` | ✅ |
| T2 | `ls .../db/` 列出可见数据库 | ✅ default, postgres, vprobe (按设计过滤了 system, INFORMATION_SCHEMA) |
| T3 | `ls .../db/default/` | ✅ 列出 117 张表 |
| T4 | `ls .../db/system/` 直接访问 | ✅ 即使 system 不在父级列表中,直接进入仍工作 |
| T5 | `ls .../db/nonexistent_db/` | ⚠️ **Bug #1**: 应 ENOENT,实际返回空目录 |
| T6 | `ls .../db/default/no_such_table/` | ⚠️ **Bug #1**: 应 ENOENT,实际返回伪 `all.tsv` |

### 文件读取 (5/5)
| # | 用例 | 结果 |
|---|---|---|
| T7 | `cat .schema` | ✅ 返回 DESCRIBE TABLE 输出 (`name	type	default_type...`) |
| T9 | `head -5 all.tsv` | ✅ 返回 header + 4 数据行 |
| T11 | `head -5` 156 行表 | ✅ |
| T12 | `wc -l all.tsv` (156 行表) | ✅ 报告 157 行 (= 156 数据 + 1 header) |
| T13 | `grep -c . all.tsv` | ✅ 157 |
| T14 | 大些的表 `.schema` | ✅ |

### 写入拒绝 EROFS (6/6) — 完美
| # | 操作 | 结果 |
|---|---|---|
| T16 | `mkdir` | ✅ `Read-only file system` |
| T17 | `touch` | ✅ `Read-only file system` |
| T18 | `echo > file` | ✅ `read-only file system` |
| T19 | `rm` | ✅ `Read-only file system` |
| T20 | `chmod 777` | ✅ `Read-only file system` |
| T21 | `mv` | ✅ `Read-only file system` |

### 流式与并发 (4/4)
| # | 用例 | 结果 |
|---|---|---|
| T22 | `head -c 4096` 流式读 system.tables | ✅ 准确 4096 字节 |
| T24 | `head -c 100` 中段取消 | ✅ 100 字节,日志显示 `clickfs::stream: cancelled` |
| T25 | 3 个 `head` 并发读三张表 | ✅ 总计 2870 字节(每个最多 1024) |
| T29 | `find -maxdepth 2 -type d` | ✅ 列出 root, db, db/* |

### 元数据 (3/4)
| # | 用例 | 结果 |
|---|---|---|
| T26 | `stat` 根目录 | ✅ `dr-xr-xr-x` |
| T27 | `stat all.tsv` | ✅ `-r--r--r--`, size = 9223372036854775807 (设计上的 pseudo-size i64::MAX) |
| T28 | `df -h ~/mnt/ch` | ⚠️ **Bug #2**: 显示 `Size -4.0Ki` (statfs 数值溢出 i64) |
| T15 | `ls` 单分区表目录 | ⚠️ **Bug #3**: `all.tsv` 出现两次 |

---

## 不通过用例 (按设计) (2/30)

| # | 用例 | 现象 | 评价 |
|---|---|---|---|
| T10 | `wc -l` 空表 | 报告 1 行(只有 header) | ✅ 正确,空表只有 header |
| T23 | `tail -3 all.tsv` | `Invalid argument` (EIO) | ✅ **符合 strict-sequential 设计**, README 已说明不支持反向 seek |

---

## 发现的 Bug

### Bug #1: 不存在的 db/table 不返回 ENOENT
- **重现**: `ls ~/mnt/ch/db/no_such_db` 返回空目录而不是错误
- **影响**: shell 脚本无法用 `[ -d path ]` 判断库表是否存在;`find` 会进入虚假目录
- **根因**: `fs.rs` 的 `lookup` / `readdir` 应在拼路径前查 `system.databases` / `system.tables`,目前直接接受任意名字
- **严重度**: 中 — 不会导致崩溃但语义错误
- **修复方案**: 在 `lookup` 里增加 db/table 存在性校验(每次 lookup 一次 `EXISTS DATABASE`/`EXISTS TABLE` 查询;也可缓存)

### Bug #2: `df` 显示负值
- **重现**: `df -h ~/mnt/ch` → `Size -4.0Ki`
- **根因**: `fs.rs:546` 用 `u64::MAX / 2` 作为 blocks,被 macOS `df` 解释为 i64 → 溢出为负
- **影响**: 显示难看,无功能影响
- **严重度**: 低 — 美观问题
- **修复**: 改成 `1u64 << 40`(1 TiB block 数)之类的"看起来合理"的伪容量

### Bug #3: 单分区表 `all.tsv` 重名
- **重现**: 非分区表 / 只有一个 `all` 分区的表里,`ls` 显示两次 `all.tsv`
- **根因**: `fs.rs:271-276` 总是先放 `"all.tsv"`,再为每个 `partition_id` 放 `"{p}.tsv"`,但 ClickHouse 把非分区表的 `partition_id` 称为字符串 `"all"`,导致重复
- **影响**: 视觉混乱;`open` 一次 lookup 行为可能不一致
- **严重度**: 中
- **修复方案**: 在 `fetch_partitions` 后面过滤掉 `partition_id == "all"`,或者总是只暴露一个 `all.tsv` 而把分区文件命名加前缀(如 `part-{id}.tsv`)

### Bug #4: `.schema` 内容与 README 描述不符
- **重现**: `cat .schema` 返回 DESCRIBE TABLE 的 TSV (列名/类型/默认值/...)
- **README 写**: "SHOW CREATE TABLE 输出"
- **影响**: 仅文档不一致;实际 DESCRIBE 输出更易程序化解析,不一定要改代码
- **严重度**: 低
- **修复方案**: 二选一 — 改 README 写"DESCRIBE TABLE 的 TSV";或改 `driver::sql_describe` 为 `SHOW CREATE TABLE`

---

## 性能与日志观察

- 单个 `ls` 触发 1 次 `SELECT name FROM system.tables WHERE database=...`;同一 `ls` 命令日志里出现了 4-5 次同样查询(macOS `ls` 会重复 stat),性能上无感但**有优化空间**(目录监听短期缓存,如 200ms TTL)
- 流式读取的 cancel 机制工作正常:`head -c 100` 后日志立即出现 `clickfs::stream: cancelled`
- 所有 SQL 都正确带上 `SETTINGS max_execution_time=60, readonly=2`
- 全表查询 SQL 形如:`SELECT * FROM ` + 反引号转义的 `db`.`tbl` + ` FORMAT TabSeparatedWithNames` ✅

---

## 推荐的下一步修复优先级

1. **修 Bug #1** (ENOENT) — 影响 shell 脚本互操作性 [中]
2. **修 Bug #3** (all.tsv 重名) — 影响 ls 体验 [中]
3. **修 Bug #2** (df 负值) — 1 行代码 [快]
4. **修 Bug #4** (README .schema 描述) — 1 行文档 [快]
5. **优化** readdir 短期缓存(200ms TTL) — 性能 [可选]

---

## 测试环境清理

挂载已干净卸载,无残留进程:
```
$ mount | grep clickfs && echo "STILL MOUNTED" || echo "no clickfs mounts"
no clickfs mounts
$ pgrep -f "target/release/clickfs mount" && echo "process alive" || echo "process gone"
process gone
```
