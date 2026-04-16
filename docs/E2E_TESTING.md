# End-to-end testing

## Coverage overview

### Protocol-level (integration) tests — `tests/multi_client.rs`, `tests/protocol_edge.rs`

The integration test suite covers resize semantics using raw Unix socket clients:

- Controller resize changes PTY dimensions (verified via SIGWINCH probe)
- Non-controller resize is silently ignored
- Readonly resize is silently ignored
- Resize during replay is ignored; connection stays synchronized afterward
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

What these tests additionally cover beyond the protocol-level tests:

- Client-side `SIGWINCH` handler and `send_resize()` flow
- Client-side `ResizeReq` response using `get_terminal_size()`
- Full round-trip: terminal emulator → mn client → mn server → child process
- Detach key handling via raw PTY input
- Multiple attach/detach cycles with resize between them

## Why not gx-ghostty?

[gx-ghostty](https://github.com/ashsidhu/gx-ghostty) was considered but
rejected:

- **macOS-only** (AppleScript + Accessibility API)
- **Cannot resize panes/windows programmatically** — it can `send`, `peek`,
  `split`, and `close`, but has no resize command
- Tightly coupled to Ghostty 1.3+ internals
- Too fragile for CI

## PTY harness design notes

Key decisions made when implementing `tests/e2e_resize.rs`:

### Kernel-generated SIGWINCH only

We rely on `TIOCSWINSZ` on the leader fd to trigger a kernel-delivered
`SIGWINCH` to the follower's foreground process group — we do *not* call
`kill(child_pid, SIGWINCH)` manually.  This way, if `setsid()` /
`TIOCSCTTY` / `tcsetpgrp()` were broken in the harness, the tests would
fail rather than silently passing via manual signalling.

### Raw byte matching, not string conversion

The `mn` client enters alternate screen mode and emits escape sequences
mixed with the child's output.  We keep a `Vec<u8>` buffer and search
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

On detach, the mn client writes a few bytes (restore termios, exit
alternate screen) to the PTY follower.  If the test blocks in
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
