#!/bin/bash
set -euo pipefail

# End-to-end benchmarks using hyperfine.
# Measures what the user actually experiences.
#
# Usage: ./bench.sh

cd "$(dirname "$0")"
cargo build --release 2>/dev/null
BIN=./target/release/mn

export MNEME_SOCKET_DIR=$(mktemp -d)
trap 'pkill -f "mn --server" 2>/dev/null; rm -rf "$MNEME_SOCKET_DIR"' EXIT

echo "=== Session create (mn new) ==="
hyperfine \
    --warmup 1 \
    --runs 20 \
    --prepare 'pkill -f "mn --server bench-create" 2>/dev/null; sleep 0.1; rm -f "$MNEME_SOCKET_DIR/bench-create"' \
    "$BIN new bench-create /bin/sh -c 'sleep 60'"

echo ""
echo "=== Session list (mn list, 1 session) ==="
# Ensure one session exists
pkill -f "mn --server" 2>/dev/null; sleep 0.1; rm -f "$MNEME_SOCKET_DIR"/*
$BIN new bench-list /bin/sh -c 'sleep 600' 2>/dev/null
sleep 0.3
hyperfine \
    --warmup 3 \
    --runs 50 \
    "$BIN list"

echo ""
echo "=== Session list (mn list, 5 sessions) ==="
for i in 2 3 4 5; do
    $BIN new "bench-list-$i" /bin/sh -c 'sleep 600' 2>/dev/null
    sleep 0.2
done
hyperfine \
    --warmup 3 \
    --runs 50 \
    "$BIN list"

echo ""
echo "=== Attach + immediate detach ==="
hyperfine \
    --warmup 3 \
    --runs 30 \
    "echo -ne '\x1c' | $BIN attach bench-list"

echo ""
echo "=== Attach + receive 1 MiB + detach ==="
pkill -f "mn --server" 2>/dev/null; sleep 0.1; rm -f "$MNEME_SOCKET_DIR"/*
$BIN new bench-data /bin/sh -c 'while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done' 2>/dev/null
sleep 0.5
hyperfine \
    --warmup 2 \
    --runs 10 \
    "dd if=/dev/zero bs=1 count=0 2>/dev/null; echo -ne '\x1c' | timeout 2 $BIN attach bench-data > /dev/null"

echo ""
echo "=== Kill session ==="
hyperfine \
    --warmup 1 \
    --runs 20 \
    --prepare "$BIN new bench-kill /bin/sh -c 'sleep 60' 2>/dev/null; sleep 0.2" \
    "$BIN kill bench-kill"
