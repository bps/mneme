#!/bin/bash
# A/B benchmark: main (sync) vs tokio-port.
# Both binaries are pre-built into /tmp/mn-main and /tmp/mn-tokio.
set -euo pipefail

DIR=$(mktemp -d)
export MNEME_SOCKET_DIR="$DIR"
trap 'pkill -f "mn-(main|tokio) --server" 2>/dev/null; rm -rf "$DIR"' EXIT

BINS=(/tmp/mn-main /tmp/mn-tokio)
LABELS=("main" "tokio")

cleanup() {
    pkill -f "mn-(main|tokio) --server" 2>/dev/null || true
    sleep 0.2
    rm -f "$DIR"/*
}

# Hyperfine "compare" mode: runs both commands head-to-head with a single
# warmup loop. We label arguments via -n.

echo "=========================================================="
echo " (1) create session  (mn new <name> /bin/sh -c 'sleep 60')"
echo "=========================================================="
PREP=$(mktemp); cat > "$PREP" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
pkill -f "mn-(main|tokio) --server bench-create " 2>/dev/null || true
sleep 0.15; rm -f "$DIR/bench-create" "$DIR/bench-create.lock"
EOF
chmod +x "$PREP"
hyperfine --shell=bash --warmup 3 --runs 30 --prepare "$PREP" \
    -n main  "MNEME_SOCKET_DIR='$DIR' /tmp/mn-main  new bench-create /bin/sh -c 'sleep 60'" \
    -n tokio "MNEME_SOCKET_DIR='$DIR' /tmp/mn-tokio new bench-create /bin/sh -c 'sleep 60'"
rm -f "$PREP"
cleanup

echo
echo "=========================================================="
echo " (2) list (1 session)"
echo "=========================================================="
"${BINS[0]}" new bench-one /bin/sh -c 'sleep 600' 2>/dev/null
sleep 0.2
hyperfine --shell=bash --warmup 5 --runs 50 \
    -n main  "MNEME_SOCKET_DIR='$DIR' /tmp/mn-main  list" \
    -n tokio "MNEME_SOCKET_DIR='$DIR' /tmp/mn-tokio list"

echo
echo "=========================================================="
echo " (3) list (5 sessions)"
echo "=========================================================="
for i in 2 3 4 5; do
    "${BINS[0]}" new "bench-s$i" /bin/sh -c 'sleep 600' 2>/dev/null
done
sleep 0.3
hyperfine --shell=bash --warmup 5 --runs 50 \
    -n main  "MNEME_SOCKET_DIR='$DIR' /tmp/mn-main  list" \
    -n tokio "MNEME_SOCKET_DIR='$DIR' /tmp/mn-tokio list"
cleanup

echo
echo "=========================================================="
echo " (4) attach + immediate detach"
echo "=========================================================="
# Use the main binary's server for both attach binaries so we measure
# *attach client* overhead independently.
"${BINS[0]}" new bench-one /bin/sh -c 'sleep 600' 2>/dev/null
sleep 0.2
hyperfine --shell=bash --warmup 5 --runs 30 \
    -n "main->main"   "echo -ne '\x1c' | MNEME_SOCKET_DIR='$DIR' /tmp/mn-main  attach bench-one" \
    -n "tokio->main"  "echo -ne '\x1c' | MNEME_SOCKET_DIR='$DIR' /tmp/mn-tokio attach bench-one"
cleanup

# Now a tokio-server attach test: tokio server + both clients.
"${BINS[1]}" new bench-one /bin/sh -c 'sleep 600' 2>/dev/null
sleep 0.2
hyperfine --shell=bash --warmup 5 --runs 30 \
    -n "main->tokio"   "echo -ne '\x1c' | MNEME_SOCKET_DIR='$DIR' /tmp/mn-main  attach bench-one" \
    -n "tokio->tokio"  "echo -ne '\x1c' | MNEME_SOCKET_DIR='$DIR' /tmp/mn-tokio attach bench-one"
cleanup

echo
echo "=========================================================="
echo " (5) attach + receive output flood + detach"
echo "=========================================================="
echo "--- main server, both clients ---"
"${BINS[0]}" new bench-flood /bin/sh -c \
    'while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done' 2>/dev/null
sleep 0.3
FLOOD_M=$(mktemp); cat > "$FLOOD_M" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
echo -ne '\x1c' | timeout 2 /tmp/mn-main attach bench-flood > /dev/null
EOF
FLOOD_T=$(mktemp); cat > "$FLOOD_T" << EOF
#!/bin/bash
export MNEME_SOCKET_DIR="$DIR"
echo -ne '\x1c' | timeout 2 /tmp/mn-tokio attach bench-flood > /dev/null
EOF
chmod +x "$FLOOD_M" "$FLOOD_T"
hyperfine --shell=bash --warmup 2 --runs 10 \
    -n "main->main"  "$FLOOD_M" \
    -n "tokio->main" "$FLOOD_T"
cleanup

echo "--- tokio server, both clients ---"
"${BINS[1]}" new bench-flood /bin/sh -c \
    'while true; do dd if=/dev/zero bs=65536 count=16 2>/dev/null; done' 2>/dev/null
sleep 0.3
hyperfine --shell=bash --warmup 2 --runs 10 \
    -n "main->tokio"  "$FLOOD_M" \
    -n "tokio->tokio" "$FLOOD_T"
rm -f "$FLOOD_M" "$FLOOD_T"
cleanup
