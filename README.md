# clickfs

[![crates.io](https://img.shields.io/crates/v/clickfs.svg)](https://crates.io/crates/clickfs)
[![docs](https://img.shields.io/badge/docs-donge.github.io%2Fclickfs-FFCC01)](https://donge.github.io/clickfs)
[![CI](https://github.com/donge/clickfs/actions/workflows/ci.yml/badge.svg)](https://github.com/donge/clickfs/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/clickfs.svg)](#license)

Mount a ClickHouse server as a read-only POSIX filesystem. Browse databases
with `ls`, stream tables with `cat`, slice with `head`/`tail`/`grep` — no
client library, no SQL boilerplate.

**Website:** https://donge.github.io/clickfs

<img width="1536" height="1024" alt="0a3556a64c0ead49ef732950ebfd0776" src="https://github.com/user-attachments/assets/e6757055-bcd7-445d-b829-3794ed6c89f4" />

> Status: **MVP / experimental**. Read-only. Single-node HTTP only.
> Tested on Linux (libfuse3) and macOS (macFUSE).

---

## Why

Sometimes you just want to grep a table. `clickhouse-client -q "SELECT * FROM
db.t FORMAT TSV" | grep ...` works, but breaks shell muscle memory: no tab
completion, no `find`, no piping into tools that expect file arguments.

`clickfs` mounts the server so the shell *is* the client:

```sh
clickfs mount http://localhost:8123 /mnt/ch &
ls /mnt/ch/db/default/
cat /mnt/ch/db/default/events/all.tsv | head -n 100
wc -l /mnt/ch/db/default/events/2024-01-15.tsv
cat /mnt/ch/db/default/events/.schema
```

Everything is streamed — no temp files, no buffering of full result sets.

---

## Layout

```
/mnt/ch/
└── db/
    └── <database>/
        └── <table>/
            ├── .schema       # SHOW CREATE TABLE output
            ├── README.md     # AI/agent-friendly summary (regenerated on open)
            ├── all.tsv       # full table, TSVWithNames
            └── <part_id>.tsv # one file per partition_id
```

Partitions are listed dynamically from `system.parts` (active only).
`all.tsv` and per-partition files are streamed lazily on `read()`.

### AI-friendly pseudo-files

`README.md` is synthesized on each `open()` from 5 concurrent
sub-queries (`DESCRIBE`, aggregate stats from `system.parts`,
`system.columns`, `COUNT()`, and a 5-row sample). It contains
**Stats**, **Schema**, **Columns**, **Sample**, **Example queries**,
and **Files** — everything an agent needs to understand a table
without writing SQL. Failed sub-queries degrade to `_(unavailable)_`
so a missing privilege never black-holes the file.

---

## Install

### One-liner (Linux & macOS)

```sh
curl -fsSL https://donge.github.io/clickfs/install.sh | sh
```

Downloads the latest prebuilt binary to `~/.local/bin/clickfs`. For a
system-wide install:

```sh
curl -fsSL https://donge.github.io/clickfs/install.sh | sudo sh -s -- --prefix /usr/local
```

Linux needs a `fuse3` package (`sudo apt install fuse3` or
`sudo yum install fuse3`); macOS needs
[macFUSE](https://osxfuse.github.io/) (`brew install --cask macfuse`).

### From crates.io

```sh
cargo install clickfs
```

Or with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) for
prebuilt binaries (no compile):

```sh
cargo binstall clickfs
```

### From source

```sh
cargo install --path .
# or
cargo build --release && ./target/release/clickfs --help
```

---

## Usage

### Mount

```sh
clickfs mount <URL> <MOUNTPOINT> [options]
```

Common options:

| Flag                   | Default          | Description                          |
| ---------------------- | ---------------- | ------------------------------------ |
| `--user`               | `default`        | ClickHouse user (or `CLICKFS_USER` env)     |
| `--password`           | *(empty)*        | Password (or `CLICKFS_PASSWORD` env, recommended) |
| `--allow-other`        | off              | Let other UIDs see the mount         |
| `--no-auto-unmount`    | off              | Keep the mount alive after the process exits (Linux) |
| `--query-timeout`      | `60`             | Server-side query timeout (seconds)  |
| `--max-result-bytes`   | `1073741824`     | Per-query byte cap (1 GiB)           |
| `--cache-ttl-ms`       | `2000`           | Metadata cache TTL (or `CLICKFS_CACHE_TTL_MS`); `0` disables |
| `--no-compression`     | off              | Disable HTTP gzip (default sends `Accept-Encoding: gzip` and `enable_http_compression=1`) |
| `--insecure`           | off              | Skip TLS cert + hostname verification (dev only) |
| `--ca-bundle <PATH>`   | *(unset)*        | Extra PEM CA bundle to trust on top of system roots |

Example:

```sh
CLICKFS_PASSWORD=secret clickfs mount \
    https://clickhouse.example.com:8443 \
    /mnt/ch \
    --user analyst
```

The process runs in the foreground; Ctrl-C unmounts and exits.

### Unmount

```sh
clickfs umount /mnt/ch
```

Or use the platform tool: `fusermount -u /mnt/ch` / `umount /mnt/ch`.

---

## Reads are strict-sequential (with a tail-mode escape hatch)

Each open file handle is one streaming `SELECT`. The kernel must read
contiguously from offset 0 forward — random seeks return `EIO`. This works
fine for `cat`, `head`, `tail -c +N` (after stream reposition), `wc`, `grep`,
and any pipeline. It will **not** work for `mmap` or random-access editors.

For the common "show me the latest N rows" use case, `all.tsv` advertises
a virtual EOF at `2^63` and supports **tail-mode**: when a reader pread()s
deep inside that pseudo-EOF window (as `tail -n N`, `less +G`, and many
log viewers do), clickfs transparently issues a one-shot
`SELECT * FROM <tbl> ORDER BY <pk> DESC LIMIT N FORMAT TabSeparatedWithNames`
and serves the (row-reversed) result from an in-memory buffer pinned to
the end of the file. The header line is preserved.

* Default: enabled, `N = 10000`. Tune with `--tail-rows N` /
  `CLICKFS_TAIL_ROWS`, or disable with `--no-tail`.
* The ORDER BY column list comes from `system.tables.primary_key`,
  falling back to `sorting_key`, then `tuple()` for engines without a
  key (Memory/Log/StripeLog).
* Multi-column keys correctly apply `DESC` per column.
* The buffer is per-fd; reads strictly *outside* the materialized
  window still return `EIO` with a debug-level reverse-seek hint.

`FOPEN_DIRECT_IO` is set on data files so the kernel page cache does not
attempt readahead beyond the current position.

---

## Read-only

Every mutating operation (`write`, `mkdir`, `unlink`, `rename`, `truncate`,
`chmod`, `chown`, `create`, ...) returns `EROFS`. v1 is intentionally
read-only; mutation requires careful design around ClickHouse's
non-transactional model.

---

## Logging

Uses `tracing`; configure via `RUST_LOG`:

```sh
RUST_LOG=clickfs=debug clickfs mount http://localhost:8123 /mnt/ch
RUST_LOG=clickfs::stream=trace,clickfs::driver=debug clickfs mount ...
```

All logs go to stderr.

---

## Architecture

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design, plus:

- [`docs/path-mapping.md`](docs/path-mapping.md) — path → query plan
- [`docs/streaming-read.md`](docs/streaming-read.md) — async → sync bridge
- [`docs/query-construction.md`](docs/query-construction.md) — SQL builders
- [`docs/observability.md`](docs/observability.md) — logging conventions
- [`docs/traits.md`](docs/traits.md) — internal interfaces

---

## Limitations (v1)

- HTTP/HTTPS protocol only (no native TCP)
- Read-only
- TSV output only (TSVWithNames for `all.tsv`)
- Strict-sequential reads per fd; backwards seeks return EIO unless
  they land inside the **tail-mode** materialization window (see
  "Reads are strict-sequential" above)
- Metadata listings (db/table/partition/existence) are cached for
  `--cache-ttl-ms` (default 2000 ms); `.schema` and data streams are
  always fresh
- Stream cancellation issues `KILL QUERY` to the server in addition to
  dropping the HTTP connection; queries also bound by
  `max_execution_time=60`
- Single ClickHouse server; no cluster / replica routing

---

## Tests

- Unit tests: `cargo test --bin clickfs` (56 cases, no FUSE needed)
- End-to-end: [`tests/e2e.sh`](tests/e2e.sh) — 48 cases against a real
  ClickHouse + mounted FUSE. See [`tests/README.md`](tests/README.md).

```sh
CLICKFS_PASSWORD='secret' tests/e2e.sh --build
```

---

## License

MIT OR Apache-2.0
