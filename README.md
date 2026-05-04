# clickfs

[![crates.io](https://img.shields.io/crates/v/clickfs.svg)](https://crates.io/crates/clickfs)
[![docs](https://img.shields.io/badge/docs-donge.github.io%2Fclickfs-FFCC01)](https://donge.github.io/clickfs)
[![CI](https://github.com/donge/clickfs/actions/workflows/ci.yml/badge.svg)](https://github.com/donge/clickfs/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/clickfs.svg)](#license)

Mount a ClickHouse server as a read-only POSIX filesystem. Browse databases
with `ls`, stream tables with `cat`, slice with `head`/`tail`/`grep` — no
client library, no SQL boilerplate.

**Website:** https://donge.github.io/clickfs

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
            ├── .schema        # SHOW CREATE TABLE output
            ├── all.tsv        # full table, TSVWithNames
            └── <part_id>.tsv  # one file per partition_id
```

Partitions are listed dynamically from `system.parts` (active only).
Both `all.tsv` and per-partition files are streamed lazily on `read()`.

---

## Install

Requires Rust 1.85+, and either `libfuse3-dev` (Linux) or
[macFUSE](https://osxfuse.github.io/) (macOS).

```sh
cargo install clickfs
```

Or from source:

```sh
cargo install --path .
```

Or build locally:

```sh
cargo build --release
./target/release/clickfs --help
```

---

## Usage

### Mount

```sh
clickfs mount <URL> <MOUNTPOINT> [options]
```

Common options:

| Flag                  | Default          | Description                          |
| --------------------- | ---------------- | ------------------------------------ |
| `--user`              | `default`        | ClickHouse user (or `CLICKFS_USER` env)     |
| `--password`          | *(empty)*        | Password (or `CLICKFS_PASSWORD` env, recommended) |
| `--allow-other`       | off              | Let other UIDs see the mount         |
| `--auto-unmount`      | on               | Unmount automatically on process exit |
| `--query-timeout`     | `60`             | Server-side query timeout (seconds)  |
| `--max-result-bytes`  | `1073741824`     | Per-query byte cap (1 GiB)           |

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

## Reads are strict-sequential

Each open file handle is one streaming `SELECT`. The kernel must read
contiguously from offset 0 forward — random seeks return `EIO`. This works
fine for `cat`, `head`, `tail -c +N` (after stream reposition), `wc`, `grep`,
and any pipeline. It will **not** work for `mmap`, random-access editors, or
tools that seek backwards.

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

- HTTP protocol only (no native TCP)
- Read-only
- TSV output only (TSVWithNames for `all.tsv`)
- Strict-sequential reads per fd
- No caching of directory listings (one query per `ls`)
- No KILL QUERY on cancel — relies on dropped HTTP connection +
  `max_execution_time`
- Single ClickHouse server; no cluster / replica routing

---

## Tests

- Unit tests: `cargo test --no-default-features` (11 cases, no FUSE needed)
- End-to-end: [`tests/e2e.sh`](tests/e2e.sh) — 26 cases against a real
  ClickHouse + mounted FUSE. See [`tests/README.md`](tests/README.md).

```sh
CLICKFS_PASSWORD='secret' tests/e2e.sh --build
```

---

## License

MIT OR Apache-2.0
