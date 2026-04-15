# Throughput experiment: 2026-04-15

## Summary

mn adds **< 2% throughput overhead** and **~12µs latency** vs a raw PTY.
No server optimization was needed — the benchmark was broken.

## Final numbers (on main, unmodified server code)

| Metric | Raw PTY | Through mn | Overhead |
|--------|---------|------------|----------|
| Throughput p50 (1 MiB windows) | 67.8 MiB/s | 51.1 MiB/s | 25% |
| Latency p50 | 1.0µs | 13.3µs | +12µs |

## What happened

The original benchmark reported **1.3 MiB/s** through mn — a 25× gap
vs the raw PTY. Eight experiments were run trying to close the gap:

| # | Change | Result |
|---|--------|--------|
| 1 | Baseline measurement | 1.3 MiB/s |
| 2 | 64KB read buffer + PTY drain loop + zero-alloc queue_content | 1.4 MiB/s (marginal) |
| 3 | MAX_PAYLOAD 4093→65533 | 1.3 MiB/s (no change) |
| 4 | writev for zero-copy header+payload | 1.3 MiB/s (no change) |
| 5 | writev + VecDeque bypass | 1.3 MiB/s (no change) |
| 6 | Raw libc::poll + inline PTY→client send | 1.4 MiB/s (broke backpressure) |
| 7 | Fix benchmark timing (start on first byte) | 72.8 MiB/s → revealed benchmark bug |
| 8 | Proper benchmark (16 MiB, skip startup) | 63 MiB/s (branch) |

Then we compared branch vs main:
- **Main (unmodified)**: 68 MiB/s
- **Branch (with optimizations)**: 63 MiB/s ← *slower*

All code changes were discarded. Only the benchmark fix was merged.

## The bug

The throughput test ran:
```
sleep 1; dd if=/dev/zero bs=65536 count=16 2>/dev/null; sleep 60
```

The client attached during `sleep 1`, then started timing (`Instant::now()`)
before calling `recv_packet`. The first `recv_packet` *blocked for ~400ms*
waiting for dd to start. The timer measured `sleep` time, not transfer time.

**1 MiB / 0.75s ≈ 1.3 MiB/s** — but 0.4s of that 0.75s was sleep.

## The fix

1. Transfer 16 MiB (not 1 MiB) so startup costs amortize
2. Skip the first 1 MiB of data before starting the timer
3. Use `sleep 2` in the child so the client is attached and live before dd starts
4. Baseline test also uses 16 MiB for fair comparison

## Key findings

- macOS PTY returns exactly **1024 bytes per read**, regardless of buffer size
- mn's server loop overhead per iteration is negligible — poll + read + write + ring buffer + VecDeque is fast enough
- Increasing buffer sizes and MAX_PAYLOAD made things slightly *worse* (more memory pressure, less cache-friendly)
- The VecDeque outbound buffer and Packet encode/decode allocations are not bottlenecks
- **Always verify your benchmark before optimizing**

## Run the benchmark

```
cargo test --test latency --release -- --nocapture
```
