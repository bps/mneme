# Testing and end-to-end coverage

## Reproducible checks

Use a current stable Rust toolchain on macOS or Linux. The PTY resize harness
also needs `python3` on `PATH`. Run these commands from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
timeout 180 cargo test --all-targets
git diff --check
```

`timeout` is GNU coreutils (`gtimeout` on some macOS installations). It bounds
the test command, not every detached process that a failing test might leave.
A timeout is a failed/incomplete run, never a pass. Inspect the failed fixture
and its PID before cleanup. Do not kill unrelated servers or delete shared
runtime directories. Tests normally use private temporary directories and
fixture-specific cleanup. Do not run overlapping suites against shared fixtures.

For lifecycle, queue, replay, and security changes, also run:

```sh
timeout 120 cargo test --test startup --test locking_security --test managed_shutdown --test server_controls --test client_backpressure --test output_tail --test replay --test backpressure
timeout 60 cargo test --test cli_parse --test cli_list --test cli_kill --test protocol_edge --test packet_codecs --test session_metadata --test harness --test e2e_resize
```

Record the commit, OS, exact command, exit code, test counts, ignored tests,
and logs. Report failures before retrying. A serial diagnostic retry does not
erase a parallel failure. Do not weaken assertions to make a run pass.

## Regression coverage

| Tests | Contract |
|---|---|
| `startup.rs` and startup unit tests | Explicit readiness, early EOF failure, timeout, invalid capacity, missing command, bind failure |
| `locking_security.rs`, `socket_unit.rs` | Stable hidden lock identity, shared probes, legacy guards, safe runtime paths, force waits, inode-aware cleanup |
| `managed_shutdown.rs` | Child HUP disposition, detach survival, bounded TERM/KILL and direct-child reap, active foreground group |
| `server_controls.rs` | Queries, attach, kill, detach, and controller handoff while output queues are full |
| `backpressure.rs` | Producer throttling without live-client eviction, exact delivery after resumption |
| Server replay unit tests and `replay.rs` | Snapshot ordering, post-snapshot output queued during replay, bounded fan-out, replay-to-live transition |
| `client_backpressure.rs` | Bounded stdout queues, partial-write ordering through Exit, stalled-output detach and signals, replay typeahead, terminal restoration |
| `output_tail.rs`, `child_exit.rs`, server exit unit tests | Final bytes before terminal result and EOF, late attach, stalled replay deadline, retained-slave Error, accumulated PTY drain budget |
| `cli_parse.rs`, `cli_list.rs`, `cli_kill.rs` | Public subcommands, aliases, flags, list markers and guarded cleanup, kill behavior |
| `packet_codecs.rs` | Shared sync/async v1 framing, exact bytes, fragmentation, malformed frames, partial-read timeouts, oversized sends |
| `session_metadata.rs` | Authoritative child PID and exit state, current ring usage, replaying/read-only attachment counts, disconnects, query/controller isolation |
| `harness.rs` | Shared helper safety: short private paths, signal targets, unwind cleanup, collection timeout, partial-packet timeout, probe cleanup |
| `multi_client.rs`, `protocol_edge.rs`, `integration.rs` | Client roles, controller election, wire errors and boundaries, session operations |
| `e2e_resize.rs` | Real attach client inside a controlling PTY, kernel-generated resize and detach paths |

Queue bounds do not prove a global memory bound. Each replay snapshot can be
as large as the configured ring. Connections and the internal control channel
are not globally bounded. Regression tests exercise specific contracts, not
whole-process-tree containment or immunity to same-UID attacks.

### Shared harness and cleanup limits

`tests/common/mod.rs` supplies short private `/tmp` runtime directories,
command-local environments, Hello/query/live-attach helpers, and client-count
waits. Raw packet reads use the production codec. Partial-frame errors require
discarding the connection. Socket timeouts remain per read, not whole-frame
deadlines against a peer that trickles bytes.

The subprocess collector drains stdout and stderr together under one absolute
deadline. It does not wait for inherited pipe EOF after direct-child exit.
Child guards request termination and attempt direct-child reaping during
unwind. Cleanup uses finite reaping polls and reports kernel-stuck children
rather than waiting indefinitely. Runtime cleanup queries private sessions,
validates signal targets, and requests SIGTERM before directory removal.
The explicit crash fixture also attempts managed-group cleanup without a
broad process search.

These helpers cover representative suites, not every fixture. Specialized
fixtures and some observation-window sleeps remain. Cleanup needs a queryable
server and ordinary kernel process termination. It cannot guarantee cleanup
after runner SIGKILL/abort or successful reaping of a kernel-stuck process.
Detached/new-session descendants remain outside managed-group coverage.
The outer timeout and fixture-specific failure inspection are still necessary.

### Final-output limits to preserve

After *observed* child exit, clients have five-second drain deadlines. Terminal
finalization permits a five-second final writer flush. Expiry disconnects a
client with a server diagnostic instead of sending success ahead of missing
bytes. A later attachment recovers only the retained ring.

A separate two-second PTY EOF budget charges unblocked ingestion time.
Backpressure pauses but never replenishes it. Retained slave descriptors
produce an incomplete-output `Error`, not a successful `Exit`. Deterministic
unit coverage holds a slave descriptor independently to exercise this path.

On macOS, terminal flushing can block a dying child before `WEXITED` is
observable. Post-observation timers do not cover that state. It behaves like
a blocked live producer. Readers must resume or the operator must use
`mn kill`. Tests must not claim universal autonomous exit from that state.
Managed process-group cleanup also does not promise to terminate detached
descendants or every job group. See [DESIGN.md](DESIGN.md).

## Performance measurements are separate

`tests/latency.rs` contains profiling tests that print latency and throughput.
They run as part of the full suite, but their numbers are host/load-dependent.
For a dedicated measurement:

```sh
timeout 180 cargo test --release --test latency -- --nocapture --test-threads=1
```

Record build mode and system load. Passing profiling tests does not establish
replay correctness, bounded memory, lock safety, or shutdown correctness.
[THROUGHPUT.md](THROUGHPUT.md) and [TOKIO_PORT.md](TOKIO_PORT.md) are historical
records, not current validation. The legacy `bench.sh` and `bench-ab.sh`
scripts contain broad process-kill cleanup. Do not run them against a host
with unrelated sessions. They are not required checks.

## Resize coverage overview

### Protocol-level (integration) tests — `tests/multi_client.rs`, `tests/protocol_edge.rs`

The integration test suite covers resize semantics using raw Unix socket clients:

- Controller resize changes PTY dimensions (verified via SIGWINCH probe)
- Non-controller resize is silently ignored
- Readonly resize is silently ignored
- Replaying clients cannot control size; non-controller resize is ignored
- ResizeReq on controller handoff (various scenarios)
- Protocol boundary values (0×0, 1×1, max×max, asymmetric, extra payload)

### End-to-end tests — `tests/e2e_resize.rs`

A PTY harness exercises the *full client-side path*: a real `mn attach`
process running inside a PTY that the test controls.  Resize events are
triggered by `TIOCSWINSZ` on the PTY leader fd and propagate through
the kernel-generated `SIGWINCH` — no manual signalling — which verifies
that the client properly owns a controlling terminal and foreground
process group.

| Test | Coverage |
|---|---|
| `e2e_resize_basic` | Create session, attach via PTY, resize, verify child sees new dimensions |
| `e2e_resize_multiple` | Multiple sequential resizes all propagate correctly |
| `e2e_detach_reattach_resize` | Detach with Ctrl-\, reattach via a fresh PTY, verify resize still works |
| `e2e_resize_controller_handoff` | Client A on PTY + client B on raw socket; proves A's resize is ignored when B is controller, and that A re-announces its PTY size (via ResizeReq) after B detaches |
| `e2e_attach_exits_on_server_hangup` | Attach client terminates after its fixture server disappears |

What these tests additionally cover beyond the protocol-level tests:

- Client-side `SIGWINCH` handling and coalesced resize enqueue when outbound capacity is available
- Client-side `ResizeReq` response using `get_terminal_size()`
- Full round-trip: terminal emulator → mn client → mn server → child process
- Detach key handling via raw PTY input
- Multiple attach/detach cycles with resize between them

## Historical gx-ghostty investigation

The original harness investigation considered
[gx-ghostty](https://github.com/ashsidhu/gx-ghostty) and selected direct PTY
control instead. It recorded these reasons at that time:

- macOS-only integration through AppleScript and the Accessibility API
- No programmatic pane/window resize command among the examined `send`,
  `peek`, `split`, and `close` operations
- Coupling to Ghostty 1.3+ internals
- Concern about fragility in CI

These are historical findings, not verified claims about current gx-ghostty
or Ghostty capabilities. The direct PTY harness does not depend on them.

## PTY harness design notes

Key decisions made when implementing `tests/e2e_resize.rs`:

### Kernel-generated SIGWINCH only

We rely on `TIOCSWINSZ` on the leader fd to trigger a kernel-delivered
`SIGWINCH` to the follower's foreground process group — we do *not* call
`kill(child_pid, SIGWINCH)` manually.  This way, if `setsid()` /
`TIOCSCTTY` / `tcsetpgrp()` were broken in the harness, the tests would
fail rather than silently passing via manual signalling.

### Raw byte matching, not string conversion

The `mn` client emits raw escape sequences mixed with the child's
output (ring-buffer replay, plus a soft-reset sequence on detach).
We keep a `Vec<u8>` buffer and search
for ASCII byte patterns (`b"READY"`, `b"WINCH 40 100"`) so fragmented
reads and ANSI sequences can't break marker detection.

### Unique dimensions per step

Every resize uses a distinct `(rows, cols)` pair so replayed output
from an earlier phase can't match a later assertion.  The reattach
test uses `35×110` for the first attachment and `60×160` for the
second.

### PTY EOF via EIO

On Linux and macOS, reading the PTY leader after the follower closes
often returns `EIO` rather than clean EOF.  The reader treats `EIO` as
hang-up.

### Drain during wait

On detach, the mn client writes a few bytes (soft-reset sequence,
restore termios) to the PTY follower.  If the test blocks in
`child.wait()` without draining the leader, the PTY buffer can fill
and the client blocks writing — deadlock.  The test drains the leader
concurrently with `try_wait()` polling.

## Session / controlling-terminal setup

In `pre_exec` on the `mn attach` child we do:

```rust
libc::setsid();                                 // new session
libc::ioctl(0, TIOCSCTTY as c_ulong, 0);        // make PTY follower our ctty
libc::tcsetpgrp(0, libc::getpid());             // be the foreground pgrp
```

All three steps are required for `TIOCSWINSZ` on the leader to
generate `SIGWINCH` for the client.
