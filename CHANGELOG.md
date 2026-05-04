# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

- Default e2e log level bumped to `clickfs=debug` so the SQL trace
  needed by the new cache-effectiveness test is captured.

### Fixed

- *(see git log for the v0.1.0 → v0.1.1 release fixes around mount
  options and pseudo-file sizes that landed earlier.)*

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

[Unreleased]: https://github.com/donge/clickfs/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/donge/clickfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/donge/clickfs/releases/tag/v0.1.0
