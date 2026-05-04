# clickfs Tests

This directory holds the end-to-end test harness for clickfs. Unit tests
(`cargo test`) live next to the source under `src/`.

| File | Purpose |
| --- | --- |
| `e2e.sh` | One-shot end-to-end test: mount, lookup, read, EROFS, statfs, unmount |
| `README.md` | This document |

---

## Quick start

```sh
# 1. Build the release binary (or pass --build to e2e.sh below)
cargo build --release

# 2. Run the e2e suite against a local ClickHouse on :8123
CLICKFS_PASSWORD='your-password' tests/e2e.sh

# All-in-one:
CLICKFS_PASSWORD='your-password' tests/e2e.sh --build
```

Expected output ends with:

```
===========================================
Total : 26
Passed: 26
Failed: 0
===========================================
[i] all tests passed
```

The script exits with status `0` on full pass, `1` on any test failure,
`2` on a setup/precondition failure (no ClickHouse, no macFUSE, etc.).

---

## What it tests

26 cases across 8 phases, each verifying one observable property:

| Phase | Cases | Coverage |
| --- | --- | --- |
| 0 — prerequisites | (setup, no test #) | ClickHouse reachable, libfuse / macFUSE present, binary built, test table discovered |
| 1 — mount | (setup) | `clickfs mount` brings up the kernel mount within 8 s |
| 2 — directory listing | T01–T04 | `ls` of `/`, `/db/`, `/db/<db>/`, `/db/system/` |
| 3 — ENOENT | T05–T08 | bogus db / table / partition paths return `No such file or directory` (Bug #1 regression) |
| 4 — reads | T09–T12 | `.schema` returns `CREATE TABLE`; `head`, `wc -l`; `all.tsv` listed exactly once (Bug #3, #4 regressions) |
| 5 — streaming | T13–T15 | `head -c` returns exact bytes; mid-stream cancel; 3-way concurrent reads |
| 6 — EROFS | T16–T20 | `mkdir`, `touch`, `echo>`, `rm`, `chmod` all denied |
| 7 — metadata | T21–T24 | `df` healthy with no negative numbers (Bug #2 regression); `stat`; `find` |
| 8 — unmount | T25–T26 | `SIGTERM` cleanly unmounts within 5 s and the process exits |

The test table is **auto-discovered** as the smallest non-empty
`MergeTree` table in the `default` database (so the suite is portable
across environments). If none exists, it falls back to
`system.databases`.

---

## Prerequisites

### macOS

```sh
brew install --cask macfuse        # one-time, requires reboot + KEXT approval
```

### Linux

```sh
sudo apt-get install -y libfuse-dev fuse3   # or your distro's equivalent
```

### Both

- A running ClickHouse you can reach over HTTP (default `http://127.0.0.1:8123`)
- Valid credentials (the script reads the password from the env, never
  from the command line, to keep it out of `ps` and shell history)
- `curl`, `bash` 4+, `mount`, `pkg-config` (macOS only) on `$PATH`

---

## Configuration

Every knob is settable either via CLI flag or environment variable. Flags
take precedence.

| Flag | Env | Default | What it controls |
| --- | --- | --- | --- |
| `--url` | `CH_URL` | `http://127.0.0.1:8123` | ClickHouse HTTP endpoint |
| `--user` | `CH_USER` | `default` | ClickHouse user |
| *(none — env only)* | `CLICKFS_PASSWORD` | *(empty)* | Password (env-only on purpose) |
| `--mountpoint` | `MOUNTPOINT` | `~/mnt/ch-e2e` | Local mountpoint (must be writable) |
| `--bin` | `CLICKFS_BIN` | `target/release/clickfs` | Path to the binary under test |
| `--build` | `BUILD=1` | off | Run `cargo build --release` first |
| `--keep-logs` | `KEEP_LOGS=1` | off | Don't delete `$LOG_DIR` on exit |
| *(none)* | `LOG_DIR` | `/tmp/clickfs-e2e` | Where the clickfs mount log goes |
| *(none)* | `RUST_LOG` | `clickfs=info` | Log filter for the clickfs process |

### Examples

```sh
# Default: local ClickHouse, local binary
CLICKFS_PASSWORD='secret' tests/e2e.sh

# Build first
CLICKFS_PASSWORD='secret' tests/e2e.sh --build

# Remote ClickHouse, custom user, verbose clickfs logs
CLICKFS_PASSWORD='secret' RUST_LOG=clickfs=debug \
    tests/e2e.sh --url https://ch.example.com:8443 --user analyst

# Test a specific binary at a custom mountpoint
CLICKFS_PASSWORD='secret' tests/e2e.sh \
    --bin ./pre-release/clickfs \
    --mountpoint /tmp/ch-test

# Keep logs for post-mortem
CLICKFS_PASSWORD='secret' tests/e2e.sh --keep-logs
ls /tmp/clickfs-e2e/   # mount.log
```

---

## Output format

Each test prints:

```
T07 T07 ls nonexistent table
    $ ls /Users/you/mnt/ch-e2e/db/default/clickfs_e2e_no_such_table_4047/
    ls: ...: No such file or directory
    [exit=1]
    ✓ PASS — non-zero exit
```

- `T07` — sequential test number
- the second line — the exact command being run
- indented body — captured stdout + stderr
- `[exit=N]` — the command's exit code
- `✓ PASS` / `✗ FAIL` — assertion result with a short reason

Failures are also collected and re-printed at the end with their test
numbers, so you don't need to scroll up in CI logs.

---

## Cleanup guarantees

The harness installs an `EXIT`/`INT`/`TERM` trap that always:

1. Sends `SIGTERM` to the spawned `clickfs` process (waits up to 5 s,
   then `SIGKILL`).
2. Force-unmounts the mountpoint via `diskutil unmount force` (macOS)
   or `umount -f` / `fusermount -u` (Linux).
3. Removes `$LOG_DIR` unless `--keep-logs` is set.

So even if you `Ctrl-C` mid-run, you should not be left with a hung
mount or a zombie process. If you somehow are:

```sh
# Find and kill leftovers
pgrep -fl 'clickfs mount'
pkill -KILL -f 'clickfs mount'

# Force-unmount
diskutil unmount force ~/mnt/ch-e2e   # macOS
fusermount -u ~/mnt/ch-e2e            # Linux
```

---

## CI considerations

- The script writes only to `$LOG_DIR` and `$MOUNTPOINT`; both are
  configurable per-job to allow parallel runs.
- All output goes to stdout; assertions print `✗ FAIL` lines that are
  easy to match. Final exit code is the source of truth.
- It does **not** mutate the ClickHouse server (read-only queries only).
  Safe to run against shared test instances.
- Total wall time on a healthy local ClickHouse: ~5–10 s.

---

## Adding a test

Inside `e2e.sh`, append a case using one of the four assertion helpers:

```bash
# Exit must be 0
expect_zero "T99 my test"  some-command --arg

# Exit must be non-zero (used for ENOENT / EROFS)
expect_nonzero "T99 must fail"  some-command --arg

# Output must match a regex
expect_match "T99 contains foo"  '^foo'  some-command

# Output must equal a literal string
expect_eq "T99 returns 42"  "42"  some-command
```

Each helper increments the test counter, prints the captured output,
and records pass/fail in the summary. Tests are independent — order is
only enforced for the bracketing mount / unmount phases.

---

## Known limitations

- macOS-only `diskutil unmount`/`stat` flags are used in cleanup; the
  script also tries Linux equivalents but has been exercised mostly on
  macOS so far.
- The streaming-cancel test (T14) verifies `head -c 100` returns
  exactly 100 bytes; the actual server-side `KILL QUERY` happens via
  HTTP-disconnect + `max_execution_time`. There is no assertion that
  the server-side query was terminated within a deadline.
- No assertion about *content* of returned rows (only that the byte
  counts and exit codes are right). Adding TSV-shape checks is welcome.


## Running on Linux from a macOS host (Docker)

`tests/e2e-docker.sh` builds a Linux test image (`rust:slim` + `libfuse-dev`
+ `fuse`) and runs `tests/e2e.sh` inside it. Use this when you want to
validate Linux behavior from a Mac without setting up a Linux box, or
when macFUSE is unavailable.

### Prerequisites

- A Docker engine that supports `--device /dev/fuse` and
  `--add-host=...:host-gateway`. **OrbStack** (recommended on macOS) and
  Docker Desktop both work.
- A reachable ClickHouse on the host (e.g. an OrbStack container on the
  `host` network like `sw_asdb`). The wrapper reaches it via
  `host.docker.internal:8123` (mapped to the host gateway).

### Quick start

```sh
CLICKFS_PASSWORD='secret' tests/e2e-docker.sh
```

First run takes ~90s (image build + `cargo build --release`). Subsequent
runs reuse:
- the docker image (`clickfs-e2e:latest`),
- the cargo registry/git caches under `./.cache/`,
- the Linux build dir under `./target-linux/`.

### Options

```
--rebuild         Rebuild the test docker image even if it exists
--image TAG       Use a different image tag
--url URL         Override ClickHouse URL inside the container
                  (default: http://host.docker.internal:8123)
--user NAME       ClickHouse user (default: default)
```

Anything after `--` is forwarded to `tests/e2e.sh`, e.g.:

```sh
CLICKFS_PASSWORD='secret' tests/e2e-docker.sh -- --keep
```

### How it works

The wrapper runs:

```
docker run --rm \
  --add-host=host.docker.internal:host-gateway \
  --device /dev/fuse --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined \
  -v $REPO:/work \
  -e CARGO_TARGET_DIR=/work/target-linux \
  -e CLICKFS_BIN=/work/target-linux/release/clickfs \
  -e BUILD=1  ...  clickfs-e2e:latest
```

Build artifacts are kept under `./target-linux/` so the host's macOS
build tree at `./target/` is never touched (Mach-O vs ELF).

### Why bridge network instead of `--network host`?

OrbStack's host-network mode can interfere with outbound TLS to
`crates.io`, breaking the in-container `cargo build`. Bridge network
avoids that, and the host gateway alias (`host.docker.internal`) gives
access to the host's ClickHouse anyway.

### Mac vs Linux portability notes for `e2e.sh`

- BSD `wc -c` (macOS) right-pads its output; GNU `wc -c` (Linux) does
  not. The two `wc -c` based assertions strip whitespace via
  `awk '{print $1}'` so they work on both.
- macFUSE detection is gated on `uname -s == Darwin`; on Linux the
  check is skipped.
- Cleanup tries `diskutil` (macOS) → `umount -f` → `fusermount -u`,
  so it works on both.
