#!/bin/bash
# Run latency benchmark and emit METRIC lines for autoresearch.
set -euo pipefail
cd "$(dirname "$0")"

OUT=$(cargo test --test latency --release -- --nocapture profile_roundtrip_latency profile_throughput 2>&1)
echo "$OUT" | tail -30

# Parse "Round-trip latency" block: p50, p95, mean
RTT=$(echo "$OUT" | awk '
    /Round-trip latency/ { in_block=1; next }
    in_block && /^test / { in_block=0 }
    in_block && /mean:/   { gsub(/[µs]/,"",$2); printf "mean=%s ", $2 }
    in_block && /p50:/    { gsub(/[µs]/,"",$2); printf "p50=%s ", $2 }
    in_block && /p95:/    { gsub(/[µs]/,"",$2); printf "p95=%s ", $2 }
    in_block && /p99:/    { gsub(/[µs]/,"",$2); printf "p99=%s ", $2 }
')
THR=$(echo "$OUT" | awk '
    /Throughput \(1 MiB/ { in_block=1; next }
    in_block && /^test / { in_block=0 }
    in_block && /p50:/   { gsub(/MiB\/s/,"",$2); printf "thr_p50=%s ", $2 }
    in_block && /mean:/  { gsub(/MiB\/s/,"",$2); printf "thr_mean=%s ", $2 }
')

# Emit METRIC lines
for kv in $RTT $THR; do
    k="${kv%%=*}"
    v="${kv#*=}"
    case "$k" in
        p50)  echo "METRIC rtt_p50_us=$v" ;;
        p95)  echo "METRIC rtt_p95_us=$v" ;;
        p99)  echo "METRIC rtt_p99_us=$v" ;;
        mean) echo "METRIC rtt_mean_us=$v" ;;
        thr_p50)  echo "METRIC thr_p50_mibs=$v" ;;
        thr_mean) echo "METRIC thr_mean_mibs=$v" ;;
    esac
done
