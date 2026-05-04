# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-05-04

### Added

- **Tail-mode for `all.tsv`.** Reading near the end of the synthetic
  `all.tsv` file (the way `tail -n N`, `less +G`, and most log viewers
  do) no longer fails with `EIO`. The file now advertises a virtual
  EOF at `2^63`; when a reader pread()s deep inside that pseudo-EOF
  window, clickfs transparently issues a one-shot
  `SELECT * FROM <db>.<tbl> ORDER BY <pk> DESC LIMIT N
   FORMAT TabSeparatedWithNames` and serves the (row-reversed) result
  from an in-memory buffer pinned to the end of the file. The header
  line is preserved.
  - Default: enabled, `N = 10000`. Tune with `--tail-rows N` or
    `CLICKFS_TAIL_ROWS=N`; disable entirely with `--no-tail`.
  - The `ORDER BY` column list comes from `system.tables.primary_key`,
    falling back to `sorting_key`, then `tuple()` for engines without
    a key (Memory, Log, StripeLog).
  - Multi-column primary keys correctly emit `DESC` per column
    (`ORDER BY a DESC, b DESC, c DESC`) — using a single trailing
    `DESC` would have only sorted the last column descending.
- 5 new unit tests on `stream::` (8 total) and 4 on `driver::` covering
  tail buffer materialization, pseudo-EOF window math, header
  preservation across row reversal, the `--no-tail` disabled path, and
  the per-column `DESC` SQL fix.
- 5 new e2e cases (T38..T38d) exercising tail-mode end-to-end via
  `dd bs=1 skip=<PSEUDO_EOF-N>`, plus `--help` smoke tests for the new
  CLI flags.

### Changed

- `e2e.sh` no longer depends on `python3`. The reverse-pread driver
  switched to `dd bs=1 skip=N count=M` (works identically on Linux
  and macOS), and the `head.ndjson` JSON validator switched to a
  structural shell check (`{...}` per line). The previous T31/T31a
  e2e cases that relied on `python3 -c "os.pread(...)"` were dropped;
  the underlying contract is exhaustively pinned by
  `stream::tests::reverse_seek_warns_once_and_errors` and
  `stream::tests::tail_disabled_keeps_reverse_seek_eio`.
- The "reverse seek not supported" log line was demoted from `warn`
  to `debug`. Tail-mode now handles the common case (`tail`, `less`),
  so the message had become noise rather than a useful hint.

## [0.2.0] - 2026-05-04

### Added

- **AI-friendly per-table pseudo-files.** Every
  `/db/<db>/<tbl>/` directory now exposes two new files designed for
  agents and humans doing reconnaissance without writing SQL:
  - `README.md` — markdown summary regenerated on each `open()`,
    synthesized from 5 concurrent sub-queries (DESCRIBE, aggregate
    stats from `system.parts`, `system.columns`, `COUNT()`, and a
    5-row sample). Sections: **Stats** (rows / size on disk / parts /
    partitions / avg bytes-per-row), **Schema** (`SHOW CREATE TABLE`
    fenced as ```sql```), **Columns** (markdown table), **Sample**
    (5 rows as `JSONEachRow`), **Example queries**, **Files**.
    Failed sub-queries degrade to `_(unavailable)_` so a missing
    `SELECT` privilege on `system.parts` doesn't black-hole the
    file.
  - `head.ndjson` — first 100 rows in `JSONEachRow`, streamed.
    Bounded by `LIMIT 100` so it terminates even on
    billion-row tables and is safe to pipe to `jq`.
- **HTTP gzip compression** (default on; `--no-compression` to
  disable). Sends `Accept-Encoding: gzip` and asks ClickHouse to
  compress with `enable_http_compression=1`. Typically 30-50%
  fewer wire bytes on large `all.tsv` streams over WAN/TLS.
- **Mount-time metadata prefetch.** `ClickFs::new` spawns a
  background warm-up that populates `db_cache` (one
  `SHOW DATABASES`) and the first level of `table_cache`
  (`SHOW TABLES` per database, bounded to 8 in flight). The very
  first `ls /mnt/db` after mount becomes a cache hit. Skipped
  when `--cache-ttl-ms=0`.
- **Server-side query cancellation.** Every streaming query is
  tagged with a `clickfs-<uuid>` `query_id`; closing the file
  before EOF now triggers a detached
  `KILL QUERY WHERE query_id = ... ASYNC`. Connection-drop alone
  worked but could lag seconds on long-running queries — KILL
  makes Ctrl-C deterministic. Best-effort: any KILL error is
  logged at debug and swallowed.
- **Metadata cache** (`--cache-ttl-ms`, env `CLICKFS_CACHE_TTL_MS`,
  default `2000` ms). Database / table / partition listings and the
  per-lookup existence probe are now cached in-process for the
  configured TTL, drastically reducing query traffic from `ls`,
  `find`, and shell tab-completion. Set to `0` to disable.
  `.schema` text and data streams are intentionally **not** cached
  so `ALTER TABLE` and freshly inserted rows show up immediately.
- **TLS options** for HTTPS endpoints:
  - `--insecure` disables certificate **and** hostname verification.
    Logs a prominent WARN at startup; intended for self-signed dev
    clusters only.
  - `--ca-bundle <PATH>` adds an extra PEM bundle (one or many certs)
    on top of the system trust store, for endpoints fronted by a
    private CA.
  - The two flags are mutually exclusive (enforced at clap-parse
    time and again inside the driver constructor).
- **Structured ClickHouse exception parsing.** Non-2xx HTTP responses
  are now decoded into a `ClickHouseException { code, name, message }`
  using either the `X-ClickHouse-Exception-Code` header or the
  canonical `Code: NNN. DB::Exception: ...` body prefix. A small map
  translates known codes (UNKNOWN_TABLE, UNKNOWN_DATABASE,
  AUTHENTICATION_FAILED, READONLY, TIMEOUT_EXCEEDED,
  TOO_MANY_SIMULTANEOUS_QUERIES, MEMORY_LIMIT_EXCEEDED,
  QUERY_WAS_CANCELLED, …) to descriptive POSIX errnos (ENOENT,
  EACCES, EROFS, ETIMEDOUT, EAGAIN, ENOMEM, ECANCELED). Unknown
  codes still degrade to EIO. Each exception is logged once at
  WARN with code + name + message, making mount-log diagnostics
  immediately actionable.
- **Diagnostic warning for reverse seeks.** When a tool issues
  `read(offset)` with `offset < cursor` on a streaming file the
  call still returns EIO, but we now emit a one-shot WARN
  explaining the strict-sequential constraint and suggesting the
  canonical workarounds (`cat | tail -n N` or `head -c N | tail`).

### Changed

- The `Files` table inside `README.md` is the single discovery
  surface for agents — it documents `.schema`, `README.md`,
  `head.ndjson`, `all.tsv`, and `<partition>.tsv` together.
- Default e2e log level bumped to `clickfs=debug` so the SQL trace
  needed by the cache-effectiveness and KILL-query tests is captured.

## [0.1.1] - 2026-05-04

### Added

- One-liner installer script (`docs/install.sh`) and matching GitHub
  release workflow that publishes prebuilt binaries for
  linux-{x86_64,aarch64} (musl) and macos-{x86_64,arm64} on every
  `v*` tag, with SHA-256 manifests.
- `cargo-binstall` metadata so `cargo binstall clickfs` pulls the
  same prebuilt tarballs.
- Website install one-liner alongside `cargo install clickfs`.

### Changed

- `--auto-unmount` (always-on, couldn't be disabled) replaced with
  the more conventional opt-out `--no-auto-unmount`.

## [0.1.0] - 2026-05-03

Initial public release.

### Added

- `clickfs mount <URL> <MOUNTPOINT>` and `clickfs umount <MOUNTPOINT>`.
- Read-only POSIX layout: `/db/<database>/<table>/{.schema, all.tsv,
  <partition>.tsv}`.
- HTTP driver with rustls-backed TLS, basic auth, server-side
  `max_execution_time`, and best-effort cancel on stream drop.
- Strict-sequential streaming reader (`StreamHandle`) with
  `FOPEN_DIRECT_IO` so the kernel doesn't buffer ahead.
- Existence-checked `lookup` (`verify_plan_exists`) so bogus paths
  return ENOENT instead of materializing fake directories.
- Linux libfuse3 + macOS macFUSE support behind a single
  `fuse` cargo feature.
- 26-case end-to-end test harness against a real ClickHouse server.
- GitHub Actions CI: rustfmt, Linux build/test/clippy `-D warnings`
  with binary artifact, macOS build/test (`--no-default-features`).

[Unreleased]: https://github.com/donge/clickfs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/donge/clickfs/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/donge/clickfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/donge/clickfs/releases/tag/v0.1.0
