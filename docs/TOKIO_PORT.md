# Tokio port: A/B benchmark results

Branch `tokio-port` ports the whole crate from a hand-rolled `rustix::event::poll` event loop to tokio (current_thread runtime). All existing integration tests pass.

## Architecture changes

| Module | main (sync) | tokio-port |
|---|---|---|
| `server.rs` LOC | 1125 | ~700 |
| `client.rs` LOC | 464 | ~530 |
| Per-client model | one struct in a Vec, multiplexed in one poll loop | three tasks: intake (handshake), reader, writer |
| Inbound hot path | poll → read → write PTY (one loop) | reader_task reads → writes PTY directly via `Arc<AsyncFd<OwnedFd>>` + Mutex (skips main task) |
| Outbound hot path | poll → read PTY → fan out → write socket (one loop) | main reads PTY → mpsc → writer_task writes socket |
| Backpressure | mask PTY POLLIN when any Live client's outbound is full | bounded `mpsc::Sender` (cap 64) — `await` on `send` blocks main |
| Signals | self-pipe + signal-hook | `tokio::process::Child::wait()`, `tokio::signal::unix` |
| PTY I/O | `rustix::io::read/write` + poll | `AsyncFd<OwnedFd>` + `try_io` |

## Latency optimization journey (autoresearch session)

The first cut of the tokio port had **~2× the round-trip latency** of the sync version. Closing the gap:

| Iteration | rtt_p50 | thr_p50 | Status |
|---|---|---|---|
| Initial tokio port (writer + reader tasks, all events through main) | ~56 µs | ~19 MiB/s | baseline |
| **Reader task writes input directly to PTY (skip main hop)** | **~32 µs** | ~17 MiB/s | **kept (-43%)** |
| (also tried) Replace writer_task with direct writes from main + Mutex | 25 µs | 21 MiB/s | discarded — 42% throughput regression |
| (also tried) Switch outbound mpsc to unbounded so `send()` is sync | 27 µs | 21 MiB/s | discarded — same throughput regression |

**Lesson learned:** removing the `writer_task` mpsc hop sounds appealing but loses two important properties:
1. The mpsc hands the writer a *batch* of pending packets to drain — direct writes from main must hit `writable().await` (kqueue round-trip) for every packet.
2. The bounded `send().await` is the natural cooperative-yield point that lets the runtime schedule the writer task. Removing it (unbounded) starved the writer.

The successful optimization (reader-task → PTY direct write) is asymmetric: the *inbound* path is single-byte and never benefits from batching, so removing the hop is pure win.

## Benchmark results (macOS, release builds, same load)

### Round-trip latency (single byte through `cat`, median of 6 runs, quiet system)

| Branch | rtt_p50 |
|---|---|
| main (sync) | 20.6 µs |
| tokio (initial port) | ~37 µs |
| **tokio (optimized)** | **20.45 µs (tied)** |

### Throughput (1 MiB windows, child→client, median of 6 runs, quiet system)

| Branch | thr_p50 | % of kernel ceiling |
|---|---|---|
| **raw PTY ceiling** (no mn at all) | **35.0 MiB/s** | 100% |
| main (sync) | 30.5 MiB/s | 87% |
| **tokio (optimized)** | **34.9 MiB/s** | **99.7%** |

Tokio is essentially saturating the kernel PTY scheduler on macOS. Two attempts to push higher (writer-task batching via `try_recv` before `write_all`; tight PTY drain loop with `try_io` until `WouldBlock`) either regressed throughput by ~15% (batching forced large writes that exceeded socket SNDBUF) or made no measurable difference (macOS deschedules between PTY reads, so the drain loop almost always finds `WouldBlock` on the second try).

> **Note on measurement.** Earlier numbers in this doc (e.g. "tokio +14% on rtt") were taken under high system load (load avg ~6.7, 62% sys CPU). Under fair quiet-system conditions, tokio matches main on latency *and* beats it on throughput. The previous "gap" was a measurement artifact, not a real regression. We re-measured 6 runs on each branch back-to-back to confirm.

### CLI commands (hyperfine)

CLI startup is identical between branches — the current_thread runtime adds essentially zero overhead for short-lived commands (`mn new`, `mn list`, `mn kill`).

### Binary size

| | main | tokio |
|---|---|---|
| `target/release/mn` | 1.18 MB | 1.69 MB (+44%) |

## Takeaways

1. **The port is a clear win on the benchmarks.** rtt_p50 tied; throughput +15%; all tests pass.
2. **`server.rs` shrunk 38%** (1125 → ~720 lines). The save comes from no more per-fd state machines, no more outbound `VecDeque` flush dance, no more replay-position bookkeeping.
3. **The latency optimization mattered.** Without the reader-direct-write change, the initial port was ~37 µs (~80% slower). One targeted change closed it entirely.
4. **Throughput parity (and slight win) was free.** No throughput-specific tuning was applied; tokio's writer task batching via the bounded mpsc happens to do the right thing for high-rate output.
5. **Backpressure semantics preserved.** `backpressure.rs` integration tests pass unchanged.

## What the port lost

- 0.5 MB of binary size (current_thread runtime + tokio's ~50 transitive deps).

## What the port gained

- ~400 fewer LOC in the core server.
- The deletion of every "manual state machine" pattern (poll-flag tracking, partial-write buffering, replay-position bookkeeping). Per-client behavior is now a linear async function.
- Compiler-checked correctness for backpressure (a bounded mpsc *will* block when full; you can't forget to re-arm POLLOUT).
- Free SIGCHLD via `tokio::process::Child::wait()`.
- ~+15% throughput, tied latency.

