#!/bin/bash
set -euo pipefail

# End-to-end benchmarks using hyperfine.
#
# Usage: ./bench.sh [--quick]

cd "$(dirname "$0")"
cargo build --release 2>/dev/null
BIN=$(pwd)/target/release/mn

QUICK=false
[ "${1:-}" = "--quick" ] && QUICK=true

DIR=$(mktemp -d)
export MNEME_SOCKET_DIR="$DIR"
trap 'pkill -f "mn --server" 2>/dev/null; rm -rf "$DIR"' EXIT

if $QUICK; then
    FAST=10; MED=5; SLOW=3
else
    FAST=50; MED=30; SLOW=10
fi

cleanup_session() {
    pkill -f "mn --server $1 " 2>/dev/null || true
    sleep 0.15
    rm -f "$DIR/$1"
}

ensure_session() {
    local name="$1"; shift
    cleanup_session "$name"
    $BIN new "$name" "$@" 2>/dev/null
    sleep 0.2
}

echo "╭──────────────────────────────────────────────╮"
echo "│  mn end-to-end benchmarks                    │"
echo "╰──────────────────────────────────────────────╯"
echo ""

# --- create ---
echo "▸ create session"
PREP=$(mktemp); cat > "$PREP" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
pkill -f "mn --server bench-create " 2>/dev/null || true
sleep 0.15; rm -f "$DIR/bench-create"
EOF
chmod +x "$PREP"
hyperfine --shell=bash --warmup 2 --runs $MED --prepare "$PREP" \
    "MNEME_SOCKET_DIR='$DIR' '$BIN' new bench-create /bin/sh -c 'sleep 60'"
rm -f "$PREP"

# --- list (1 session) ---
echo ""
echo "▸ list (1 session)"
pkill -f "mn --server" 2>/dev/null || true; sleep 0.2; rm -f "$DIR"/*
ensure_session bench-one /bin/sh -c 'sleep 600'
hyperfine --shell=bash --warmup 3 --runs $FAST \
    "MNEME_SOCKET_DIR='$DIR' '$BIN' list"

# --- list (5 sessions) ---
echo ""
echo "▸ list (5 sessions)"
for i in 2 3 4 5; do
    ensure_session "bench-s$i" /bin/sh -c 'sleep 600'
done
hyperfine --shell=bash --warmup 3 --runs $FAST \
    "MNEME_SOCKET_DIR='$DIR' '$BIN' list"

# --- attach + immediate detach ---
echo ""
echo "▸ attach + immediate detach"
hyperfine --shell=bash --warmup 3 --runs $MED \
    "echo -ne '\x1c' | MNEME_SOCKET_DIR='$DIR' '$BIN' attach bench-one"

# --- attach + receive output + detach ---
echo ""
echo "▸ attach + receive output + detach"
ensure_session bench-flood /bin/sh -c \
    'while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done'
FLOOD_CMD=$(mktemp); cat > "$FLOOD_CMD" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
echo -ne '\x1c' | timeout 2 "$BIN" attach bench-flood > /dev/null
EOF
chmod +x "$FLOOD_CMD"
hyperfine --shell=bash --warmup 2 --runs $SLOW "$FLOOD_CMD"
rm -f "$FLOOD_CMD"

# --- kill ---
echo ""
echo "▸ kill session"
KPREP=$(mktemp); cat > "$KPREP" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
"$BIN" new bench-kill /bin/sh -c 'sleep 60' 2>/dev/null; sleep 0.2
EOF
chmod +x "$KPREP"
hyperfine --shell=bash --warmup 1 --runs $SLOW --prepare "$KPREP" \
    "MNEME_SOCKET_DIR='$DIR' '$BIN' kill bench-kill"
rm -f "$KPREP"
