# mneme design

Session persistence for terminal processes. Decouples long-running processes
from their controlling terminal so they can be detached and reattached —
independent of any terminal multiplexer.

## Goals

- **Session persistence** — detach/reattach to running processes
- **Output capture** — ring buffer of recent output, replayed on reattach
- **Single-purpose** — no multiplexing, no terminal emulation
- **macOS-first** — primary target is desktop macOS, portable to Linux
- **Fast** — zero-copy where possible, minimal allocations in the hot path
- **Secure** — strict socket permissions, no network listener (SSH for remote)

## Non-goals

- Terminal multiplexing (use cmux, dvtm, zellij, etc.)
- Built-in networking / remote protocol (SSH handles auth + encryption)
- Terminal emulation or escape sequence parsing
- Session grouping (individual sessions only, v1)

## Intended usage

```
# Local: create a session running a shell
mneme -c work

# Detach with Ctrl-\ (configurable)

# Reattach — replays recent output for context
mneme -a work

# List sessions
mneme

# Remote: SSH to host, then attach normally
ssh host -- mneme -a work

# With a multiplexer: each pane attaches to a mneme session
cmux new-pane -- mneme -a build
```

## Architecture

```
 ┌─────────┐   Unix     ┌────────────────────────────────┐
 │ Client 1 │──socket──▶│         Server (daemon)         │
 └─────────┘            │                                  │
 ┌─────────┐            │  ┌───────────┐  ┌────────────┐  │
 │ Client 2 │──socket──▶│  │ Ring buf  │  │    PTY     │  │
 └─────────┘            │  │ (replay)  │  │  leader fd │  │
                        │  └───────────┘  └─────┬──────┘  │
                        │                       │          │
                        │                 ┌─────▼──────┐  │
                        │                 │   Child     │  │
                        │                 │  process    │  │
                        │                 └────────────┘  │
                        └─────────────────────────────────┘
```

**Server** — spawned as a background process via `std::process::Command`
(same binary, `--server` flag). Calls `setsid()` to fully detach from the
calling session. Owns the PTY. Accepts client connections over a Unix
domain socket. Reads child output, writes it to all attached clients and
appends to ring buffer. Reads client input, writes to PTY.

**Client** — connects to server socket. Puts local terminal in raw mode.
On attach, receives ring buffer replay then live data. Sends stdin to
server. Handles `SIGWINCH` for resize propagation.

## Protocol

### Transport

`AF_UNIX` + `SOCK_STREAM`. (`SOCK_SEQPACKET` would eliminate length
framing but is unsupported on macOS.) All client sockets are non-blocking.

### Framing

Every message uses a 3-byte header:

```
┌──────────┬───────────┬──────────────────────┐
│ type: u8 │ len: u16  │ payload: [u8; len]   │
│ (1 byte) │ (2 bytes) │ (0..4093 bytes)      │
└──────────┴───────────┴──────────────────────┘
```

Max packet size: 4096 bytes (3 header + 4093 payload). `len` is
little-endian (both macOS and Linux are LE on all supported arches).

### Connection state machine

```
                 ┌─────────┐
    connect() ──▶│Connected│
                 └────┬────┘
                      │ Client sends Hello
                      ▼
                 ┌─────────┐
                 │Handshake│
                 └────┬────┘
                      │ Server sends Welcome or Error
                      ▼
              ┌───────┴───────┐
              │               │
         intent=Query    intent=Attach
              │               │
              ▼               ▼
         disconnect      ┌─────────┐
                         │Replaying│
                         └────┬────┘
                              │ Replay* → ReplayEnd
                              ▼
                         ┌────────┐
                         │  Live  │◀── Content, Resize (bidi)
                         └────┬────┘
                              │ Detach, Exit, or disconnect
                              ▼
                         ┌────────┐
                         │ Closed │
                         └────────┘
```

### Handshake

Version and intent are negotiated *once* at connection time:

```
Client → Server:
  Hello {
    version: u8,        // protocol version (1 for v1)
    intent:  u8,        // 0 = Query, 1 = Attach
    flags:   u16,       // READONLY (0x01), LOW_PRIORITY (0x02)
    rows:    u16,       // terminal rows  (0 if intent=Query)
    cols:    u16,       // terminal cols  (0 if intent=Query)
  }

Server → Client:
  Welcome {
    version:       u8,  // server protocol version
    server_pid:    u32, // server daemon PID (for liveness checks)
    child_pid:     u32, // child process PID
    child_running: u8,  // 1 = running, 0 = exited
    exit_status:   u8,  // valid only if child_running=0
    client_count:  u16, // number of currently attached clients
    ring_size:     u32, // ring buffer size in bytes
    ring_used:     u32, // bytes currently in ring buffer
  }
  or
  Error {
    message: [u8]       // UTF-8 error description
  }
```

If `version` doesn't match, the server sends `Error` and disconnects.
Exact version match only — no compatibility negotiation in v1.

For **Query** intent: the client reads `Welcome`, extracts session info,
and disconnects. No replay, no live data. This is how `mneme` (list)
gets session metadata without side effects.

For **Attach** intent: after `Welcome`, the server begins replay.

### Message types

| Type        | Value | Direction       | Payload              | Description                         |
|-------------|-------|-----------------|----------------------|-------------------------------------|
| Hello       | 0     | client→server   | (see above)          | Handshake initiation                |
| Welcome     | 1     | server→client   | (see above)          | Handshake response                  |
| Error       | 2     | server→client   | UTF-8 message        | Error (handshake or runtime)        |
| Content     | 3     | bidi            | raw bytes            | Terminal I/O data                   |
| Resize      | 4     | client→server   | rows: u16, cols: u16 | Window size change                  |
| ResizeReq   | 5     | server→client   | (empty)              | Server asks client to send Resize   |
| Detach      | 6     | client→server   | (empty)              | Client requesting detach            |
| Exit        | 7     | server→client   | status: u32          | Child process exited                |
| Replay      | 8     | server→client   | raw bytes            | Ring buffer replay chunk            |
| ReplayEnd   | 9     | server→client   | (empty)              | Replay complete, live data follows  |

`Resize` is client→server only: the client tells the server its terminal
dimensions. `ResizeReq` is server→client only: the server asks a newly
promoted controlling client to re-announce its dimensions.

### Replay protocol

On attach (after handshake):

1. Server *copies* the ring buffer contents into a per-client replay
   buffer (up to 1 MiB — cheap, avoids concurrent-read hazards)
2. Server sends the copy as one or more `Replay` packets
3. Server sends `ReplayEnd`
4. Client transitions to `Live` state

During replay, new PTY output is queued in the client's outbound buffer
(same buffer used for all states — see backpressure below). If the buffer
fills, the client is disconnected.

Client input is *ignored* until `ReplayEnd` has been sent — the server
does not accept `Content` or `Resize` from a client in `Replaying` state.

### Backpressure and slow clients

Every client has a single **bounded outbound buffer** (default 256 KiB)
used in *all* connection states — replay, live, and shutdown. Policy:

- Server writes to the client socket (non-blocking). If `EAGAIN`, the
  data goes into the outbound buffer.
- If the outbound buffer exceeds the limit, the client is disconnected
  immediately (no warning, no drain — the client is too slow).
- This applies uniformly to `Replay`, `Content`, `Resize`, and `Exit`
  messages. No special-casing by state.

### Multi-client semantics

Multiple clients may attach simultaneously. Exactly one client is the
*controlling client* — the most recently attached non-readonly,
non-low-priority client. Rules:

- Only the controlling client's `Resize` is applied to the PTY
- `Resize` from non-controlling clients is silently ignored (not an error)
- `Content` from readonly clients is silently ignored
- All non-readonly clients may send input (interleaved at packet
  boundaries)
- When the controlling client detaches (or disconnects unexpectedly),
  the next eligible client (most recent first) becomes controlling
  and receives a `ResizeReq`. Controller transfer happens immediately,
  regardless of the new controller's connection state.

### Exited session semantics

When the child process exits:

1. Server records the exit status
2. Server sends `Exit { status }` to all connected clients
3. Server **remains alive and listening** — the session is still
   attachable and queryable
4. New clients can attach: they receive replay + `Exit` immediately
   (the ring buffer still contains the child's output history)
5. Server exits when *all* of the following are true:
   - The child has exited
   - No clients are connected
   - At least one client has been notified of the exit (prevents the
     race where `mneme -c true` loses the session before attaching)

This means `mneme -c true` works: the server waits for the initial
attach, delivers the exit status, and then cleans up after the client
disconnects.

## Ring buffer

- Simple circular byte buffer, *not* a terminal state tracker
- Default size: **1 MiB** (configurable via `-s` flag or `MNEME_SCROLLBACK`)
- Stored as a single contiguous allocation with head/tail indices
- All PTY output is appended; when full, oldest bytes are overwritten
- On reattach, the buffer contents are **copied** into a per-client
  replay buffer — this avoids concurrent-read hazards (the ring
  continues accepting PTY writes during replay without corrupting
  the snapshot)

### Replay is output history, not screen restoration

The ring buffer replays *raw byte history*. It does not parse escape
sequences or track terminal state. Consequences:

- Partial escape sequences at the oldest buffer boundary may cause brief
  visual artifacts (garbled colors, cursor misposition)
- Progress bars and CR-redraws replay their full byte history, not just
  the final state

Because the client does not wrap the session in the alternate screen,
replay drives the outer terminal into whatever state the child is in —
including altscreen, mouse reporting, or keyboard protocols a TUI app
has enabled. Pre-attach terminal content is preserved in scrollback; the
full ring streams into the main screen on attach.

On detach/exit the client issues a targeted soft reset (exit altscreen,
pop kitty keyboard, reset SGR, show cursor, reset scroll region, disable
bracketed paste and mouse reporting) so the shell prompt returns usable
regardless of where in its state machine the child was when we let go.

This is the right trade-off for v1: the primary use case is seeing
recent shell output after reattaching. For TUI apps, the app itself
redraws on `SIGWINCH` (sent on attach), making replay less relevant.

## Socket management

### Directory resolution

First success wins:

1. `$MNEME_SOCKET_DIR/`
2. `$XDG_RUNTIME_DIR/mneme/`
3. `$HOME/.mneme/`
4. `$TMPDIR/mneme/<uid>/`
5. `/tmp/mneme/<uid>/`

Sessions are not expected to outlive a reboot. `XDG_RUNTIME_DIR` is the
preferred default — it's the standard location for per-user runtime
state on modern systems. On macOS where `XDG_RUNTIME_DIR` is typically
unset, falls back to `$HOME/.mneme/`.

User identity is derived from `getuid()` / `getpwuid()`, *not* from
the `$USER` environment variable.

### Socket path

`<socket_dir>/<session_name>`

Session names are validated: `[a-zA-Z0-9._-]` only, max 64 bytes.
No hostname suffix — sessions are inherently local (remote access is
via SSH, which connects to the local socket). This keeps paths short
and avoids brittleness if the hostname changes.

### Directory creation and security

Socket directories are created and verified using safe traversal:

- `openat()` / `mkdirat()` with `O_NOFOLLOW | O_DIRECTORY` at each
  path component — prevents symlink attacks
- After creation, verify ownership with `fstat()` on the opened fd
  (not `lstat()` on the path, which is racy)
- Personal directories: mode `0700`
- Shared base dirs (e.g., `/tmp/mneme/`): mode `1777` (sticky),
  per-user subdirectories `0700`
- Sockets: mode `0600`
- Before unlinking a stale socket, verify it is owned by the current
  user and is actually a socket (not a regular file or symlink)

### Peer credential verification

On every client connection, the server calls `getpeereid()` (macOS) or
reads `SO_PEERCRED` (Linux) to verify the connecting process's UID
matches the server's UID. Connections from other UIDs are rejected.

### Session discovery (listing)

`mneme` (no args) lists sessions by:

1. Scanning the socket directory for socket files
2. Connecting to each socket with `intent=Query`
3. Reading the `Welcome` response for session metadata (PIDs, client
   count, child state, ring buffer usage)
4. Disconnecting

Stale sockets (connect refused, no response) are unlinked automatically
after verifying ownership (see above).

This is slightly slower than reading sidecar metadata files, but always
correct — no stale state, no second source of truth to keep in sync.

## Process lifecycle

No manual `fork()`. The CLI spawns the server as a background process
using `std::process::Command` (same binary, `--server` flag).

### Session creation

```
CLI process
  │
  ├─ socketpair() → (cli_fd, server_fd)
  ├─ Command::new(current_exe)
  │    .args(["--server", name,
  │           "--ready-fd", server_fd,
  │           "--rows", rows, "--cols", cols,
  │           "--", cmd...])
  │    .stdin(null).stdout(null).stderr(null)
  │    .process_group(0)
  │    .spawn()
  │
  ├─ blocks on cli_fd waiting for readiness
  │
  └─ on EOF: server is ready → connect as client
     on data: startup error → print and exit
```

The CLI passes its current terminal dimensions (`--rows`, `--cols`) so
the server can set the PTY size *before* spawning the child. This
ensures the child starts with the correct dimensions.

For `mneme -n` (create, don't attach), the CLI still reads its terminal
size if available, otherwise defaults to 80×24.

### Server startup (`--server` mode)

```
Server process (same binary)
  │
  ├─ setsid()                                       // fully detach from caller's session
  ├─ bind + listen on Unix socket                    // before child spawn — fail early
  │
  ├─ openpty() → (leader_fd, follower_fd)            // safe rustix call
  ├─ ioctl(leader_fd, TIOCSWINSZ, rows, cols)        // set initial PTY size
  ├─ Command::new(cmd)
  │    .stdin(Stdio::from(follower.try_clone()))      // safe — Command does dup2
  │    .stdout(Stdio::from(follower.try_clone()))     // safe
  │    .stderr(Stdio::from(follower))                 // safe
  │    .pre_exec(|| {                                 // unsafe (~2 lines)
  │        setsid();                                  //   new session for child
  │        ioctl(stdin, TIOCSCTTY);                   //   set controlling terminal
  │    })
  │    .spawn()
  │
  ├─ close(follower_fd)
  ├─ close(ready_fd) → EOF signals readiness to CLI
  │
  └─ enter server event loop
```

Startup order is deliberate: **bind/listen before child spawn**. If the
socket bind fails, we fail before creating a child process that would
need cleanup.

### File descriptor hygiene

All internal fds (ready_fd, listen socket, PTY leader) are set to
`CLOEXEC` before spawning the child. Only the PTY follower (as
stdin/stdout/stderr) is inherited. This prevents:
- The ready_fd from staying open in the child (which would block the
  CLI's readiness read indefinitely)
- The listen socket from leaking to the child process
- The PTY leader from being accessible to the child

### Readiness signaling

A `socketpair` connects the CLI process to the server:

- CLI creates the pair before spawning, passes one fd to the server
- Server inherits the fd and closes it after successful setup
  (socket bound, PTY open, child spawned)
- CLI blocks on `read()` — EOF means ready, any data means error
- If the child command fails to exec, the server writes the error
  to the ready fd before exiting

### Server `setsid()`

The server calls `setsid()` at the very start of `--server` mode. This
is a safe call (not inside `pre_exec`) that:
- Creates a new session — server is no longer in the caller's session
- Detaches from the caller's controlling terminal
- Ensures `SIGHUP` from the caller's terminal doesn't kill the server

The server also registers a `SIGHUP` handler (ignore) as defense in
depth. Combined with stdin/stdout/stderr redirected to `/dev/null` and
a new process group (`process_group(0)` in the spawn), the server is
fully daemonized.

### Cleanup on interruption

If the CLI is interrupted (`Ctrl-C`) while waiting on the readiness fd:
- If the server hasn't started yet: the spawned process receives
  `SIGINT` (same process group via `process_group(0)` — wait, no:
  `process_group(0)` puts it in a new group). The server will be
  orphaned but will fail to complete setup and exit.
- If the server has started: it continues running as a daemon. The
  session is accessible via `mneme -a`. This is the expected behavior —
  `Ctrl-C` during create is equivalent to a very fast detach.

## Unsafe audit

The entire unsafe surface is ~5 lines, all single syscalls:

| Site | Lines | Why unsafe | Avoidable? |
|------|-------|------------|------------|
| `pre_exec`: `setsid()` | 1 | Post-fork child; std has no safe session API | No |
| `pre_exec`: `TIOCSCTTY` | 1 | Set controlling terminal; may lack safe rustix wrapper | No |
| `getpeereid` (macOS) | 1 | Peer credential check; may need raw libc call | No (platform gap) |

Note: the server's own `setsid()` at the start of `--server` mode is
*not* unsafe — it's a normal function call, not inside `pre_exec`.

**What is *not* unsafe**:
- `openpty()` — safe in rustix, returns `OwnedFd`
- PTY follower fd setup — `Stdio::from(OwnedFd)` lets `Command` handle
  `dup2` internally, no unsafe needed
- `tcgetattr` / `tcsetattr` — safe rustix wrappers
- `TIOCGWINSZ` / `TIOCSWINSZ` — safe rustix ioctl helpers
- `poll` — safe
- `openat` / `mkdirat` — safe
- Peer credentials on Linux — `rustix::net::sockopt::get_socket_peercred()` is safe

## Event loop

Synchronous `poll(2)` — no async runtime.

**Server watches**:
- Listening socket fd (new client connections)
- PTY leader fd (child output)
- Each client socket fd (client input + writability when buffer non-empty)
- Self-pipe fd (for signals: `SIGCHLD`, `SIGTERM`, `SIGUSR1`)

**Client watches**:
- stdin fd (keyboard input)
- Server socket fd (server output)
- Self-pipe fd (for `SIGWINCH`)

All client sockets are non-blocking. The server never blocks on a
client write — if the socket buffer is full, data is queued up to
the per-client limit, then the client is disconnected.

## Crate dependencies

| Crate         | Purpose                                              |
|---------------|------------------------------------------------------|
| `rustix`      | openpty, termios, ioctl, poll, dup2, setsid, openat  |
| `signal-hook` | Safe signal-to-fd bridge (self-pipe trick)            |

`rustix` over `nix`: returns `OwnedFd` instead of raw ints, modular
feature flags (only pull in what we use), raw syscalls on Linux (no
libc indirection).

No async runtime, no serialization framework, no CLI framework.

## Module structure

```
src/
  main.rs         CLI parsing, session list, create/attach dispatch
  protocol.rs     Packet type, encode/decode, read_all/write_all
  server.rs       Server mode (--server), PTY, poll loop, client list, ring buffer relay
  client.rs       Terminal raw mode, attach loop, replay handling
  socket.rs       Socket dir creation, permission checks, path resolution
  ring.rs         Circular byte buffer
```

## CLI interface

```
mneme                          # list sessions
mneme -c <name> [cmd...]       # create session, attach
mneme -n <name> [cmd...]       # create session, don't attach
mneme -a <name>                # attach to existing session
mneme -A <name> [cmd...]       # attach if exists, else create
mneme -e <key>                 # set detach key (e.g. -e ^q)
mneme -r                       # attach read-only
mneme -s <size>                # ring buffer size (e.g. -s 2M)
mneme -q                       # quiet (no info messages)
mneme -f                       # force (reuse terminated session name)
```

Default command if none given: `$MNEME_CMD`, then `$SHELL`, then `/bin/sh`.

## Security considerations

- No network listener — remote access exclusively via SSH
- Socket directory created with `openat` / `O_NOFOLLOW` safe traversal
- Peer UID verified on every connection (`getpeereid` / `SO_PEERCRED`)
- Socket permissions `0600` — only owner can connect
- Session names validated (no path separators, no control characters)
- User identity from `getuid()`, not `$USER`
- Stale socket unlink only after verifying ownership + socket type
- Server redirects stdin/stdout/stderr to `/dev/null` after daemonizing
- Server calls `setsid()` — fully detached from caller's session
- All internal fds set `CLOEXEC` — only PTY follower inherited by child
- No `setuid`, no privilege escalation
- Ring buffer is in-memory only — no sensitive data written to disk
- Protocol version checked once at handshake — fail fast on mismatch
- Server enforces client roles — readonly `Content`/`Resize` silently
  ignored, non-controller `Resize` silently ignored

## Future considerations (not v1)

- **Heuristic replay start**: scan backward from ring buffer tail to find
  a clean boundary (last `\n`, last `ESC[0m`) to reduce artifacts
- **Session hooks**: `MNEME_HOOK` script on create/attach/detach/exit
- **Advisory sidecar metadata**: `<session>.meta` file for faster listing
  (avoids connecting to each socket), clearly marked as cache
- **Optional TLS listener**: for environments where SSH is unavailable
