#!/usr/bin/env bash
#
# clickfs end-to-end test
# -----------------------
# Mounts clickfs against a real ClickHouse server, runs a battery of
# functional checks (lookups, reads, EROFS, streaming, df, .schema),
# then cleanly unmounts. Exits non-zero on any failure.
#
# See tests/README.md for prerequisites, env vars, and usage examples.

set -u
set -o pipefail

# --------------------------------------------------------------------------
# Config (override via environment).
# --------------------------------------------------------------------------
: "${CH_URL:=http://127.0.0.1:8123}"
: "${CH_USER:=default}"
: "${CLICKFS_PASSWORD:=}"
: "${MOUNTPOINT:=$HOME/mnt/ch-e2e}"
: "${CLICKFS_BIN:=}"
: "${LOG_DIR:=/tmp/clickfs-e2e}"
: "${KEEP_LOGS:=0}"
: "${BUILD:=0}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --------------------------------------------------------------------------
# Pretty output.
# --------------------------------------------------------------------------
if [[ -t 1 ]]; then
  C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YEL=$'\033[33m'
  C_CYA=$'\033[36m'; C_DIM=$'\033[2m'; C_RST=$'\033[0m'
else
  C_RED=""; C_GRN=""; C_YEL=""; C_CYA=""; C_DIM=""; C_RST=""
fi

PASS=0
FAIL=0
SKIP=0
FAILED_TESTS=()

say()    { printf '%s\n' "$*"; }
info()   { printf '%s[i]%s %s\n' "$C_CYA" "$C_RST" "$*"; }
warn()   { printf '%s[!]%s %s\n' "$C_YEL" "$C_RST" "$*"; }
err()    { printf '%s[x]%s %s\n' "$C_RED" "$C_RST" "$*" >&2; }

usage() {
  cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --build            Run \`cargo build --release\` before testing.
  --bin PATH         Path to clickfs binary (default: target/release/clickfs).
  --url URL          ClickHouse HTTP endpoint (default: $CH_URL).
  --user NAME        ClickHouse user (default: $CH_USER).
  --mountpoint DIR   Local mountpoint (default: $MOUNTPOINT).
  --keep-logs        Do not delete the log directory at exit.
  -h, --help         Show this help.

Environment overrides:
  CH_URL, CH_USER, CLICKFS_PASSWORD, MOUNTPOINT, CLICKFS_BIN, LOG_DIR,
  KEEP_LOGS=1, BUILD=1
EOF
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --build)        BUILD=1; shift ;;
    --bin)          CLICKFS_BIN="$2"; shift 2 ;;
    --url)          CH_URL="$2"; shift 2 ;;
    --user)         CH_USER="$2"; shift 2 ;;
    --mountpoint)   MOUNTPOINT="$2"; shift 2 ;;
    --keep-logs)    KEEP_LOGS=1; shift ;;
    -h|--help)      usage; exit 0 ;;
    *) err "unknown option: $1"; usage; exit 2 ;;
  esac
done

[[ -z "$CLICKFS_BIN" ]] && CLICKFS_BIN="$REPO_ROOT/target/release/clickfs"

# --------------------------------------------------------------------------
# Helpers.
# --------------------------------------------------------------------------
ch_query() {
  # ch_query <SQL>   -> stdout=body, exit=curl rc
  # POST the query in the body with the right content-type so ClickHouse
  # parses it as the query (not as URL-encoded form data).
  local q="$1"
  curl -fsS -u "${CH_USER}:${CLICKFS_PASSWORD}" \
       -H 'Content-Type: text/plain; charset=utf-8' \
       --data-binary "$q" "${CH_URL}/"
}

is_mounted() {
  mount | grep -q " on $MOUNTPOINT "
}

unmount_if_mounted() {
  if is_mounted; then
    diskutil unmount force "$MOUNTPOINT" >/dev/null 2>&1 \
      || umount -f "$MOUNTPOINT" >/dev/null 2>&1 \
      || fusermount -u "$MOUNTPOINT" >/dev/null 2>&1 \
      || true
    sleep 1
  fi
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  info "cleanup: stopping clickfs and unmounting"
  if [[ -n "${CLICKFS_PID:-}" ]] && kill -0 "$CLICKFS_PID" 2>/dev/null; then
    kill -TERM "$CLICKFS_PID" 2>/dev/null || true
    for _ in 1 2 3 4 5; do
      sleep 1
      kill -0 "$CLICKFS_PID" 2>/dev/null || break
    done
    kill -KILL "$CLICKFS_PID" 2>/dev/null || true
  fi
  unmount_if_mounted
  if [[ "$KEEP_LOGS" != "1" && -d "$LOG_DIR" ]]; then
    rm -rf "$LOG_DIR"
  fi
  exit $rc
}
trap cleanup EXIT INT TERM

# --------------------------------------------------------------------------
# Test runner: each `expect_*` records a pass or fail.
# --------------------------------------------------------------------------
TC=0
run_case() {
  # run_case "description" command...
  TC=$((TC+1))
  local name="$1"; shift
  printf '%sT%02d%s %s\n' "$C_DIM" "$TC" "$C_RST" "$name"
  printf '    %s$%s %s\n' "$C_DIM" "$C_RST" "$*"
  local out rc
  out=$("$@" 2>&1); rc=$?
  if [[ -n "$out" ]]; then
    printf '%s' "$out" | sed 's/^/    /'
    printf '\n'
  fi
  printf '    %s[exit=%d]%s\n' "$C_DIM" "$rc" "$C_RST"
  LAST_OUT="$out"; LAST_RC=$rc
}

pass() { PASS=$((PASS+1)); printf '    %s✓ PASS%s — %s\n\n' "$C_GRN" "$C_RST" "$1"; }
fail() { FAIL=$((FAIL+1)); FAILED_TESTS+=("T${TC}: ${CURRENT_NAME:-?} — $1");
         printf '    %s✗ FAIL%s — %s\n\n' "$C_RED" "$C_RST" "$1"; }

# Convenience wrappers — each starts a "current" test.
expect_zero() {
  CURRENT_NAME="$1"; shift
  run_case "$CURRENT_NAME" "$@"
  [[ $LAST_RC -eq 0 ]] && pass "exit=0" || fail "expected exit=0, got $LAST_RC"
}
expect_nonzero() {
  CURRENT_NAME="$1"; shift
  run_case "$CURRENT_NAME" "$@"
  [[ $LAST_RC -ne 0 ]] && pass "non-zero exit" || fail "expected non-zero, got 0"
}
expect_match() {
  # expect_match "desc" "regex" command...
  CURRENT_NAME="$1"; local pat="$2"; shift 2
  run_case "$CURRENT_NAME" "$@"
  if printf '%s' "$LAST_OUT" | grep -Eq "$pat"; then
    pass "matched /$pat/"
  else
    fail "output did not match /$pat/"
  fi
}
expect_eq() {
  # expect_eq "desc" "expected" command...
  CURRENT_NAME="$1"; local exp="$2"; shift 2
  run_case "$CURRENT_NAME" "$@"
  if [[ "$LAST_OUT" == "$exp" ]]; then
    pass "output == \"$exp\""
  else
    fail "expected \"$exp\", got \"$LAST_OUT\""
  fi
}

# --------------------------------------------------------------------------
# Phase 0: prerequisites.
# --------------------------------------------------------------------------
say ""
info "==== Phase 0: prerequisites ===="

# 0.1 ClickHouse reachable
if ! ver=$(ch_query 'SELECT version()' 2>&1); then
  err "cannot reach ClickHouse at $CH_URL as $CH_USER: $ver"
  exit 2
fi
info "ClickHouse version: $ver"

# 0.2 macFUSE / libfuse present
if [[ "$(uname -s)" == "Darwin" ]]; then
  if ! pkg-config --exists fuse 2>/dev/null && ! [[ -d /Library/Frameworks/macFUSE.framework ]]; then
    err "macFUSE not detected; install via: brew install --cask macfuse"
    exit 2
  fi
fi

# 0.3 Build if requested
if [[ "$BUILD" == "1" ]]; then
  info "building release binary"
  ( cd "$REPO_ROOT" && cargo build --release ) || { err "cargo build failed"; exit 2; }
fi

# 0.4 Binary exists
if [[ ! -x "$CLICKFS_BIN" ]]; then
  err "clickfs binary not found or not executable: $CLICKFS_BIN"
  err "hint: rerun with --build, or set CLICKFS_BIN=/path/to/clickfs"
  exit 2
fi
info "clickfs binary: $CLICKFS_BIN"

# 0.5 Discover a non-empty MergeTree table in `default`
info "discovering a non-empty test table in 'default'"
DISCOVER_SQL="SELECT name FROM system.tables \
WHERE database = 'default' AND engine LIKE '%MergeTree%' AND total_rows > 10 \
ORDER BY total_rows ASC LIMIT 1 FORMAT TabSeparated"
TEST_TABLE=$(ch_query "$DISCOVER_SQL" | tr -d '\r\n' || true)
if [[ -z "$TEST_TABLE" ]]; then
  warn "no non-empty MergeTree table in 'default'; falling back to system.databases"
  TEST_DB="system"
  TEST_TABLE="databases"
  EXPECT_NONEMPTY=1
else
  TEST_DB="default"
  EXPECT_NONEMPTY=1
fi
info "test target: ${TEST_DB}.${TEST_TABLE}"

# Pick a known-empty / nonexistent name with extremely low collision odds
GHOST_DB="clickfs_e2e_no_such_db_$$"
GHOST_TBL="clickfs_e2e_no_such_table_$$"
GHOST_PART="clickfs_e2e_no_such_partition_$$"

# 0.6 Prepare mountpoint
mkdir -p "$MOUNTPOINT"
if ! [[ -d "$MOUNTPOINT" && -w "$MOUNTPOINT" ]]; then
  err "mountpoint not a writable directory: $MOUNTPOINT"
  err "hint: on macOS use a path under \$HOME, not /mnt (SIP read-only)"
  exit 2
fi

unmount_if_mounted

# Stop any leftover clickfs from prior runs
if pgrep -f "$CLICKFS_BIN mount" >/dev/null; then
  warn "killing leftover clickfs processes"
  pkill -TERM -f "$CLICKFS_BIN mount" 2>/dev/null || true
  sleep 2
fi

mkdir -p "$LOG_DIR"

# --------------------------------------------------------------------------
# Phase 1: mount.
# --------------------------------------------------------------------------
say ""
info "==== Phase 1: mount ===="

CLICKFS_LOG="$LOG_DIR/mount.log"
RUST_LOG="${RUST_LOG:-clickfs=debug}" \
CLICKFS_PASSWORD="$CLICKFS_PASSWORD" \
nohup "$CLICKFS_BIN" mount "$CH_URL" "$MOUNTPOINT" --user "$CH_USER" \
  > "$CLICKFS_LOG" 2>&1 &
CLICKFS_PID=$!
info "clickfs pid=$CLICKFS_PID, log=$CLICKFS_LOG"

# Wait up to 8s for kernel mount
for i in 1 2 3 4 5 6 7 8; do
  sleep 1
  if is_mounted; then
    info "mounted after ${i}s"
    break
  fi
done
if ! is_mounted; then
  err "mount did not appear within 8s"
  err "----- mount log -----"
  cat "$CLICKFS_LOG" >&2
  exit 2
fi

# --------------------------------------------------------------------------
# Phase 2: directory listing.
# --------------------------------------------------------------------------
say ""
info "==== Phase 2: directory listing ===="

expect_match  "T01 root contains 'db'"          '^db$'   ls "$MOUNTPOINT"
expect_zero   "T02 list databases"              ls "$MOUNTPOINT/db/"
expect_zero   "T03 list tables in default"      bash -c "ls $MOUNTPOINT/db/default/ >/dev/null"
expect_match  "T04 system db accessible"        '.'      bash -c "ls $MOUNTPOINT/db/system/ | head -3"

# --------------------------------------------------------------------------
# Phase 3: ENOENT for bogus paths (Bug #1 regression).
# --------------------------------------------------------------------------
say ""
info "==== Phase 3: ENOENT ===="

expect_nonzero "T05 ls nonexistent db"          ls "$MOUNTPOINT/db/$GHOST_DB/"
expect_nonzero "T06 stat nonexistent db"        stat "$MOUNTPOINT/db/$GHOST_DB"
expect_nonzero "T07 ls nonexistent table"       ls "$MOUNTPOINT/db/default/$GHOST_TBL/"
expect_nonzero "T08 cat nonexistent partition"  cat "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/$GHOST_PART.tsv"

# --------------------------------------------------------------------------
# Phase 4: file reads.
# --------------------------------------------------------------------------
say ""
info "==== Phase 4: reads ===="

expect_match  "T09 .schema is DDL"              'CREATE TABLE' \
              cat "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/.schema"
expect_zero   "T10 head all.tsv"                head -n 3 "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv"
expect_zero   "T11 wc -l all.tsv"               wc -l "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv"

# Verify all.tsv occurs exactly once (Bug #3 regression)
expect_eq     "T12 all.tsv listed exactly once" "1" \
              bash -c "ls $MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/ | grep -c '^all.tsv$'"

# --------------------------------------------------------------------------
# Phase 5: streaming + cancel.
# --------------------------------------------------------------------------
say ""
info "==== Phase 5: streaming ===="

# Pick a table likely to have many rows for streaming tests
STREAM_FILE="$MOUNTPOINT/db/system/tables/all.tsv"
expect_eq     "T13 head -c 4096 returns 4096"   "4096" \
              bash -c "head -c 4096 '$STREAM_FILE' | wc -c | awk '{print \$1}'"
expect_eq     "T14 head -c 100 (cancel mid-stream)" "100" \
              bash -c "head -c 100 '$STREAM_FILE' | wc -c | awk '{print \$1}'"

# Concurrent reads (3 parallel) should all return data
expect_zero   "T15 concurrent reads (3 parallel)" \
              bash -c "
                ( head -c 1024 '$STREAM_FILE' >/dev/null & \
                  head -c 1024 $MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv >/dev/null & \
                  head -c 1024 $MOUNTPOINT/db/system/databases/all.tsv >/dev/null & \
                  wait )"

# T31 reverse-pread → EIO + warn log: covered exhaustively by unit
# tests `stream::tests::reverse_seek_warns_once_and_errors` and
# `stream::tests::tail_disabled_keeps_reverse_seek_eio`. We dropped the
# e2e equivalent because constructing forward-then-reverse pread on a
# single fd from pure shell is not portable (dd reopens the fd every
# invocation), and the unit tests already pin the contract.

# T32: TLS option mutual exclusion is enforced by clap.
# We invoke the binary directly (no mount) and check exit + message.
expect_zero   "T32 --insecure conflicts with --ca-bundle" \
              bash -c "'$CLICKFS_BIN' mount http://x /tmp --insecure --ca-bundle /etc/hosts 2>&1 | grep -q 'cannot be used with'"

# T33: HTTP gzip compression flag exists and SQL contains
# enable_http_compression=1 by default. We can't easily inspect the
# wire bytes here, but we CAN verify the SETTINGS clause is appended
# to the query the driver issues, which is what triggers ClickHouse
# to compress. The companion --no-compression flag must hide it.
expect_zero   "T33 --no-compression flag is recognized" \
              bash -c "'$CLICKFS_BIN' mount --help | grep -q -- '--no-compression'"
expect_zero   "T33a default mount sends enable_http_compression=1" \
              bash -c "
                # Trigger at least one query, then grep the SQL trace log.
                cat '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/.schema' >/dev/null
                grep -q 'enable_http_compression=1' '$CLICKFS_LOG'
              "

# T34: mount-time metadata prefetch warms db_cache so the very first
# `ls /mnt/db` is a cache hit. Best-effort: we just check the prefetch
# task ran and logged a "warmed db cache" line. The task is spawned
# synchronously by ClickFs::new, but completes asynchronously, so we
# poll the log up to a few seconds.
expect_zero   "T34 mount-time prefetch warms db cache" \
              bash -c "
                for i in 1 2 3 4 5 6 7 8 9 10; do
                  grep -q 'warmed db cache' '$CLICKFS_LOG' && exit 0
                  sleep 0.5
                done
                exit 1
              "

# T35: streaming reads get a clickfs- prefixed query_id, and closing
# the file early triggers a server-side KILL QUERY. We can't observe
# the KILL on the server cheaply from a shell, but we CAN verify the
# client-side log line that the kill_query call was issued. Read 1
# byte from a stream file (which forces the stream to start) then
# release the fd by letting head exit; Drop fires KILL.
expect_zero   "T35 stream gets clickfs-prefixed query_id" \
              bash -c "
                head -c 1 '$STREAM_FILE' >/dev/null
                # Wait for the spawned kill task to log.
                for i in 1 2 3 4 5 6 7 8; do
                  grep -q 'clickfs-' '$CLICKFS_LOG' && exit 0
                  sleep 0.25
                done
                exit 1
              "
expect_zero   "T35a early close triggers kill query (best-effort)" \
              bash -c "
                head -c 1 '$STREAM_FILE' >/dev/null
                for i in 1 2 3 4 5 6 7 8; do
                  grep -q 'kill query sent' '$CLICKFS_LOG' && exit 0
                  sleep 0.25
                done
                exit 1
              "

# T36: README.md pseudo-file is present in the table dir, opens
# successfully, and has the expected sections.
expect_zero   "T36 README.md is listed in table dir" \
              bash -c "ls '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/' | grep -qx 'README.md'"
expect_zero   "T36a README.md has Stats section" \
              bash -c "grep -q '^## Stats' '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/README.md'"
expect_zero   "T36b README.md has Schema section" \
              bash -c "grep -q '^## Schema' '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/README.md'"
expect_zero   "T36c README.md has Columns section" \
              bash -c "grep -q '^## Columns' '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/README.md'"
expect_zero   "T36d README.md has Sample section" \
              bash -c "grep -q '^## Sample' '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/README.md'"
expect_zero   "T36e README.md has Files section" \
              bash -c "grep -q '^## Files' '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/README.md'"

# T37: head.ndjson pseudo-file streams JSONEachRow, one object per
# line, bounded by LIMIT (so it terminates even on huge tables).
expect_zero   "T37 head.ndjson is listed in table dir" \
              bash -c "ls '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/' | grep -qx 'head.ndjson'"
expect_zero   "T37a head.ndjson is valid JSON-per-line and bounded" \
              bash -c "
                # Read the file (LIMIT 100 inside) and check every
                # non-empty line looks like a JSON object: starts with
                # '{', ends with '}'. ClickHouse JSONEachRow output is
                # well-formed by construction, so a structural check is
                # sufficient and avoids depending on python3.
                cat '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/head.ndjson' | \
                  awk 'NF{print}' | \
                  while IFS= read -r line; do
                    case \"\$line\" in
                      '{'*'}') ;;
                      *) exit 1 ;;
                    esac
                  done
              "
expect_zero   "T37b head.ndjson terminates (LIMIT 100 enforced)" \
              bash -c "
                COUNT=\$(cat '$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/head.ndjson' | awk 'NF' | wc -l | tr -d ' ')
                test \"\$COUNT\" -le 100 -a \"\$COUNT\" -gt 0
              "

# T38: tail-buffer materialization via reverse pread.
#
# A read whose offset lands deep inside the pseudo-EOF window
# (PSEUDO_EOF = u64::MAX/2 = 1<<63) triggers a one-shot
# `SELECT * ORDER BY <pk> DESC LIMIT N` and serves the (row-reversed)
# result from a buffer pinned to `[PSEUDO_EOF - len, PSEUDO_EOF)`.
#
# We drive it with `dd skip=<bytes>` because:
#   * BSD `tail(1)` on macOS reads forward and never reverse-seeks,
#   * `dd bs=1 skip=N count=M` translates 1:1 to `pread(fd,M,N)` via
#     FUSE, and works identically on Linux + macOS.
#
# Note: PSEUDO_EOF (1<<63 = 9223372036854775808) overflows shell
# signed int64, so we hard-code `PSEUDO_EOF - 8192` directly.
PSEUDO_EOF_MINUS_8K=9223372036854767616
expect_zero   "T38 reverse pread near pseudo-EOF returns data via tail buffer" \
              bash -c "
                BYTES=\$(dd if='$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv' \
                            bs=1 skip=$PSEUDO_EOF_MINUS_8K count=8192 \
                            2>/dev/null | wc -c | tr -d ' ')
                test \"\$BYTES\" -gt 0
              "
expect_zero   "T38a tail buffer payload contains row data (newline present)" \
              bash -c "
                dd if='$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv' \
                   bs=1 skip=$PSEUDO_EOF_MINUS_8K count=8192 2>/dev/null \
                  | grep -q .
              "
expect_zero   "T38b tail buffer materialization is logged" \
              bash -c "
                dd if='$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv' \
                   bs=1 skip=$PSEUDO_EOF_MINUS_8K count=4096 \
                   >/dev/null 2>&1
                for i in 1 2 3 4 5 6 7 8; do
                  grep -q 'tail buffer materialized' '$CLICKFS_LOG' && exit 0
                  sleep 0.25
                done
                exit 1
              "
expect_zero   "T38c --no-tail flag is recognized" \
              bash -c "'$CLICKFS_BIN' mount --help | grep -q -- '--no-tail'"
expect_zero   "T38d --tail-rows flag is recognized" \
              bash -c "'$CLICKFS_BIN' mount --help | grep -q -- '--tail-rows'"

# T27: --cache-ttl-ms flag exists and accepts 0.
expect_zero   "T27 --cache-ttl-ms is recognized" \
              bash -c "'$CLICKFS_BIN' mount --help | grep -q -- '--cache-ttl-ms'"

# T28: with cache enabled (default), repeated readdir should hit cache.
# Heuristic: list databases 5 times, then count "SHOW DATABASES" / "system.databases"
# entries in the SQL log. With ttl=2000ms, only the first call should hit CH.
expect_zero   "T28 cache reduces query count under load" \
              bash -c "
                : > '$CLICKFS_LOG.t28'
                # Capture line count before
                BEFORE=\$(grep -c 'system.databases' '$CLICKFS_LOG' || true)
                for i in 1 2 3 4 5; do ls '$MOUNTPOINT/db' >/dev/null; done
                AFTER=\$(grep -c 'system.databases' '$CLICKFS_LOG' || true)
                DELTA=\$((AFTER - BEFORE))
                # Without caching: 5 calls. With caching: at most 1 (first miss).
                # Allow up to 2 to absorb any concurrent kernel behavior.
                test \"\$DELTA\" -le 2
              "

# --------------------------------------------------------------------------
# Phase 6: read-only enforcement (EROFS).
# --------------------------------------------------------------------------
say ""
info "==== Phase 6: EROFS ===="

expect_nonzero "T16 mkdir denied"               mkdir "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/x"
expect_nonzero "T17 touch denied"               touch "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/x.tsv"
expect_nonzero "T18 echo > file denied"         bash -c "echo hi > $MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/x.tsv"
expect_nonzero "T19 rm denied"                  rm "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv"
expect_nonzero "T20 chmod denied"               chmod 777 "$MOUNTPOINT/db/$TEST_DB/$TEST_TABLE/all.tsv"

# --------------------------------------------------------------------------
# Phase 7: metadata.
# --------------------------------------------------------------------------
say ""
info "==== Phase 7: metadata ===="

# df should not show negative numbers (Bug #2 regression).
expect_zero   "T21 df works"                    df -h "$MOUNTPOINT"
expect_zero   "T22 df has no negative size"     bash -c "! df -h '$MOUNTPOINT' | tail -1 | grep -E -- '-[0-9]'"
expect_zero   "T23 stat root"                   stat "$MOUNTPOINT"
expect_zero   "T24 find depth=2 works"          bash -c "find '$MOUNTPOINT' -maxdepth 2 -type d >/dev/null"

# --------------------------------------------------------------------------
# Phase 8: graceful unmount.
# --------------------------------------------------------------------------
say ""
info "==== Phase 8: unmount ===="

if kill -0 "$CLICKFS_PID" 2>/dev/null; then
  kill -TERM "$CLICKFS_PID"
  for i in 1 2 3 4 5; do
    sleep 1
    if ! is_mounted; then
      info "unmounted after ${i}s"
      break
    fi
  done
fi

if is_mounted; then
  fail "T25 SIGTERM unmount: still mounted after 5s"
else
  CURRENT_NAME="T25 SIGTERM unmount"; TC=$((TC+1))
  pass "filesystem cleanly unmounted"
fi

if kill -0 "$CLICKFS_PID" 2>/dev/null; then
  fail "T26 process exit: clickfs still alive"
else
  CURRENT_NAME="T26 process exit"; TC=$((TC+1))
  pass "clickfs process exited"
fi
unset CLICKFS_PID

# --------------------------------------------------------------------------
# Summary.
# --------------------------------------------------------------------------
say ""
say "==========================================="
say "Total : $TC"
say "Passed: ${C_GRN}${PASS}${C_RST}"
say "Failed: ${C_RED}${FAIL}${C_RST}"
say "Skipped: ${SKIP}"
say "==========================================="

if (( FAIL > 0 )); then
  err "FAILURES:"
  for t in "${FAILED_TESTS[@]}"; do
    err "  - $t"
  done
  exit 1
fi

info "all tests passed"
exit 0
