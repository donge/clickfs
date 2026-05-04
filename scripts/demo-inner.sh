#!/bin/bash
# Inner demo script — runs inside the recorded shell.
# Simulates a human typing and executing commands.

set -e

GREEN='\033[0;32m'
YELLOW='\033[0;33m'
DIM='\033[0;90m'
RESET='\033[0m'
DOLLAR='$'
PROMPT="${GREEN}${DOLLAR}${RESET} "

# Type a command character-by-character, then press enter and run it.
type_run() {
    local cmd="$1"
    local delay="${2:-0.04}"
    printf "%b" "$PROMPT"
    for ((i=0; i<${#cmd}; i++)); do
        printf "%s" "${cmd:$i:1}"
        sleep "$delay"
    done
    printf "\n"
    eval "$cmd"
}

# Just print a comment line in dim color (no execution).
say() {
    printf "%b\n" "${DIM}# $1${RESET}"
    sleep 0.6
}

clear

say "clickfs — mount ClickHouse as a POSIX filesystem"
sleep 0.8

type_run "clickfs --help"
sleep 1.5

say "Mount the local ClickHouse server"
type_run "mkdir -p /mnt/ch"
type_run "CLICKFS_PASSWORD='Sw@123456' clickfs mount http://localhost:8123 /mnt/ch &"
sleep 2.5

say "Browse databases — every database is a directory"
type_run "ls /mnt/ch/db/"
sleep 1.2

say "Each table is a directory with .schema and per-partition .tsv files"
type_run "ls /mnt/ch/db/default/access_local/"
sleep 1.5

say "Read the schema"
type_run "head -10 /mnt/ch/db/default/access_local/.schema"
sleep 2

say "Stream the whole table"
type_run "head -2 /mnt/ch/db/default/access_local/all.tsv | cut -c1-80"
sleep 2

say "Or just one partition"
type_run "wc -l /mnt/ch/db/default/access_local/20260207.tsv"
sleep 1.5

say "Pipe into grep, awk, anything that reads stdin"
type_run "cut -f3,8 /mnt/ch/db/default/access_local/20260207.tsv | head -3"
sleep 2

say "Read-only — writes return EROFS"
type_run "echo hack > /mnt/ch/db/default/access_local/all.tsv 2>&1 || true"
sleep 1.5

say "Unmount"
type_run "fusermount -u /mnt/ch"
sleep 0.5

printf "\n%b\n" "${YELLOW}https://github.com/donge/clickfs${RESET}"
sleep 1.2
