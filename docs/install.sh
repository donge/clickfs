#!/bin/sh
# clickfs installer — POSIX sh
#
# Usage:
#   curl -fsSL https://donge.github.io/clickfs/install.sh | sh
#   curl -fsSL https://donge.github.io/clickfs/install.sh | sh -s -- --version v0.1.1
#   curl -fsSL https://donge.github.io/clickfs/install.sh | sh -s -- --bin-dir ~/bin
#   curl -fsSL https://donge.github.io/clickfs/install.sh | sudo sh -s -- --prefix /usr/local
#   curl -fsSL https://donge.github.io/clickfs/install.sh | sh -s -- --dry-run
#
# Requires: curl, tar, uname, sh.  Optional: shasum or sha256sum (for verification).

set -eu

REPO="donge/clickfs"
DEFAULT_BIN_DIR="${HOME}/.local/bin"

# colors (no-op if stderr is not a tty)
if [ -t 2 ]; then
  GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; DIM='\033[0;90m'; RESET='\033[0m'
else
  GREEN=''; YELLOW=''; RED=''; DIM=''; RESET=''
fi

say()  { printf '%bclickfs%b %s\n' "$GREEN"  "$RESET" "$*" >&2; }
warn() { printf '%bclickfs%b %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '%bclickfs error:%b %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

usage() {
  cat <<'EOF' >&2
clickfs installer

Usage: install.sh [OPTIONS]

Options:
  --version <vX.Y.Z>   Install a specific version (default: latest release)
  --bin-dir <DIR>      Where to put the binary (default: ~/.local/bin)
  --prefix <DIR>       Equivalent to --bin-dir <DIR>/bin
  --dry-run            Print what would be done, don't install
  --skip-fuse-check    Skip /dev/fuse / macFUSE detection
  -h, --help           Show this help

Environment:
  CLICKFS_VERSION      Same as --version
  CLICKFS_BIN_DIR      Same as --bin-dir
EOF
}

main() {
  VERSION="${CLICKFS_VERSION:-}"
  BIN_DIR="${CLICKFS_BIN_DIR:-$DEFAULT_BIN_DIR}"
  DRY_RUN=0
  SKIP_FUSE=0

  while [ $# -gt 0 ]; do
    case "$1" in
      --version)         [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2;;
      --version=*)       VERSION="${1#*=}"; shift;;
      --bin-dir)         [ $# -ge 2 ] || die "--bin-dir needs a value"; BIN_DIR="$2"; shift 2;;
      --bin-dir=*)       BIN_DIR="${1#*=}"; shift;;
      --prefix)          [ $# -ge 2 ] || die "--prefix needs a value"; BIN_DIR="$2/bin"; shift 2;;
      --prefix=*)        BIN_DIR="${1#*=}/bin"; shift;;
      --dry-run)         DRY_RUN=1; shift;;
      --skip-fuse-check) SKIP_FUSE=1; shift;;
      -h|--help)         usage; exit 0;;
      *) die "unknown flag: $1 (try --help)";;
    esac
  done

  need_cmd curl
  need_cmd tar
  need_cmd uname
  need_cmd mktemp
  need_cmd install

  detect_platform
  resolve_version
  if [ "$SKIP_FUSE" = "0" ]; then
    check_fuse
  fi

  ASSET="clickfs-${VERSION}-${PLATFORM}.tar.gz"
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
  SHA_URL="${URL}.sha256"

  say "platform:    $PLATFORM"
  say "version:     $VERSION"
  say "download:    $URL"
  say "install to:  $BIN_DIR/clickfs"

  if [ "$DRY_RUN" = "1" ]; then
    say "dry-run, exiting before download"
    exit 0
  fi

  TMPDIR="$(mktemp -d 2>/dev/null || mktemp -d -t clickfs)"
  trap 'rm -rf "$TMPDIR"' EXIT
  trap 'rm -rf "$TMPDIR"; exit 130' INT TERM HUP

  download_and_verify
  install_binary
  verify_install
  print_quickstart
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

detect_platform() {
  os_raw="$(uname -s 2>/dev/null || echo unknown)"
  arch_raw="$(uname -m 2>/dev/null || echo unknown)"

  case "$os_raw" in
    Linux)   os_id=linux;;
    Darwin)  os_id=macos;;
    *) die "unsupported OS: $os_raw (only Linux and macOS supported)";;
  esac

  case "$arch_raw" in
    x86_64|amd64)
      if [ "$os_id" = "macos" ]; then
        # No native Intel macOS binary is published; use arm64 via Rosetta 2.
        warn "Intel macOS detected; using arm64 binary via Rosetta 2."
        warn "Make sure Rosetta is installed: softwareupdate --install-rosetta --agree-to-license"
        arch_id=arm64
      else
        arch_id=x86_64
      fi
      ;;
    aarch64|arm64)
      if [ "$os_id" = "macos" ]; then
        arch_id=arm64
      else
        arch_id=aarch64
      fi
      ;;
    *) die "unsupported arch: $arch_raw (only x86_64 and aarch64/arm64 supported)";;
  esac

  PLATFORM="${os_id}-${arch_id}"
  OS_ID="$os_id"
}

resolve_version() {
  if [ -n "$VERSION" ]; then
    case "$VERSION" in
      v*) ;;
      *)  VERSION="v$VERSION";;
    esac
    return
  fi
  say "fetching latest release tag from GitHub..."
  api="https://api.github.com/repos/${REPO}/releases/latest"
  VERSION="$(curl --proto '=https' --tlsv1.2 -fsSL "$api" \
    | grep '"tag_name":' \
    | head -n 1 \
    | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  [ -n "$VERSION" ] || die "could not resolve latest release from $api"
}

check_fuse() {
  if [ "$OS_ID" = "linux" ]; then
    if [ ! -c /dev/fuse ]; then
      warn "/dev/fuse not present; you'll need a fuse3 package to mount."
      warn "  Debian/Ubuntu:  sudo apt install fuse3"
      warn "  RHEL/Amazon:    sudo yum install fuse3"
      warn "(Continuing with installation anyway.)"
    fi
  else
    if [ ! -e /Library/Filesystems/macfuse.fs ] \
       && ! kextstat 2>/dev/null | grep -qi macfuse \
       && [ ! -d /Library/Frameworks/macFUSE.framework ]; then
      die "macFUSE is required on macOS but was not detected.

Install it first:

    brew install --cask macfuse

After installing, you may need to approve the system extension in
System Settings -> Privacy & Security, then re-run this installer."
    fi
  fi
}

download_and_verify() {
  say "downloading $ASSET..."
  if ! curl --proto '=https' --tlsv1.2 -fSL --progress-bar \
         --connect-timeout 10 --retry 2 --retry-delay 2 \
         "$URL" -o "$TMPDIR/$ASSET"; then
    die "failed to download $URL
  - check your internet connection
  - confirm the release exists: https://github.com/${REPO}/releases"
  fi

  if curl --proto '=https' --tlsv1.2 -fsSL \
        --connect-timeout 5 --max-time 15 --retry 2 --retry-delay 1 \
        "$SHA_URL" -o "$TMPDIR/$ASSET.sha256" 2>/dev/null; then
    if command -v shasum >/dev/null 2>&1; then
      sumtool="shasum -a 256"
    elif command -v sha256sum >/dev/null 2>&1; then
      sumtool="sha256sum"
    else
      warn "no shasum/sha256sum found; skipping checksum verification"
      return
    fi
    expected="$(awk '{print $1}' "$TMPDIR/$ASSET.sha256")"
    actual="$(cd "$TMPDIR" && $sumtool "$ASSET" | awk '{print $1}')"
    if [ "$expected" != "$actual" ]; then
      die "checksum mismatch for $ASSET
  expected: $expected
  actual:   $actual"
    fi
    say "checksum ok"
  else
    warn "no .sha256 file at $SHA_URL; skipping checksum verification"
  fi
}

install_binary() {
  tar -C "$TMPDIR" -xzf "$TMPDIR/$ASSET"
  if [ ! -f "$TMPDIR/clickfs" ]; then
    die "tarball did not contain a 'clickfs' binary"
  fi
  if ! mkdir -p "$BIN_DIR" 2>/dev/null; then
    die "cannot create $BIN_DIR (try --bin-dir <writable-dir> or run with sudo)"
  fi
  if ! install -m 755 "$TMPDIR/clickfs" "$BIN_DIR/clickfs" 2>/dev/null; then
    die "cannot write to $BIN_DIR/clickfs (try --bin-dir <writable-dir> or run with sudo)"
  fi
  say "installed: $BIN_DIR/clickfs"
}

verify_install() {
  if [ ! -x "$BIN_DIR/clickfs" ]; then
    die "binary did not land at $BIN_DIR/clickfs"
  fi
  if ver_out="$("$BIN_DIR/clickfs" --version 2>&1)"; then
    say "verified: $ver_out"
  else
    warn "binary installed but '--version' failed; you may be missing libfuse3 (Linux) or macFUSE (macOS)"
  fi
  case ":$PATH:" in
    *":$BIN_DIR:"*)
      ;;
    *)
      warn "$BIN_DIR is not in your PATH. Add it with:"
      printf '\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR" >&2
      ;;
  esac
}

print_quickstart() {
  cat >&2 <<EOF

${GREEN}clickfs is ready.${RESET}

  ${DIM}# 1) Mount${RESET}
  CLICKFS_PASSWORD=secret clickfs mount http://your-clickhouse:8123 /mnt/ch &

  ${DIM}# 2) Use${RESET}
  ls /mnt/ch/                     ${DIM}# list databases${RESET}
  ls /mnt/ch/default/             ${DIM}# list tables${RESET}
  head /mnt/ch/default/users/all.tsv

  ${DIM}# 3) Unmount${RESET}
  fusermount3 -u /mnt/ch          ${DIM}# linux${RESET}
  umount /mnt/ch                  ${DIM}# macos${RESET}

Docs: https://donge.github.io/clickfs
EOF
}

main "$@"
