# Protocol sequence diagrams

## Session creation (`mneme -c work sh`)

```
    CLI                          Server (--server)              Child (sh)
     │                                │                            │
     │── socketpair() ──┐             │                            │
     │                  │             │                            │
     │── Command::new(mneme --server work                          │
     │     --rows 24 --cols 80 -- sh)                              │
     │   .spawn() ─────────────────▶ │                             │
     │                               │                             │
     │                               │── setsid()                  │
     │                               │── bind(socket)              │
     │                               │── listen()                  │
     │                               │                             │
     │                               │── openpty() → leader,follower
     │                               │── ioctl(leader, TIOCSWINSZ, │
     │                               │         24, 80)             │
     │                               │                             │
     │                               │── Command::new(sh)          │
     │                               │   .stdin/out/err(follower)  │
     │                               │   .pre_exec(setsid+ctty)   │
     │                               │   .spawn() ──────────────▶ │
     │                               │                             │
     │                               │── close(follower_fd)        │
     │                               │── close(ready_fd)           │
     │                               │                             │
     │◀─ EOF on ready_fd ────────────│                             │
     │   (server is ready)           │                             │
     │                               │                             │
     │── connect(socket) ──────────▶ │                             │
     │          ... attach flow ...  │                             │
```

## Query (`mneme` — list sessions)

```
    CLI                          Server
     │                              │
     │── connect() ───────────────▶ │
     │                              │
     │── Hello ───────────────────▶ │
     │   { version: 1,              │
     │     intent: Query,           │
     │     flags: 0,                │
     │     rows: 0, cols: 0 }      │
     │                              │
     │◀──────────────── Welcome ──  │
     │   { version: 1,              │
     │     server_pid: 1234,        │
     │     child_pid: 1235,         │
     │     child_running: 1,        │
     │     exit_status: 0,          │
     │     client_count: 0,         │
     │     ring_size: 1048576,      │
     │     ring_used: 4200 }        │
     │                              │
     │── close() ─────────────────▶ │
     │                              │
```

Repeated for each socket in the session directory. CLI then prints:

```
Active sessions
  2026-04-15 09:12    1234    work
* 2026-04-15 08:30    1200    build
+ 2026-04-15 07:00    1180    logs
```

## Attach with replay (`mneme -a work`)

```
    CLI (client)                 Server                        Child
     │                              │                            │
     │── connect() ───────────────▶ │                            │
     │                              │── getpeereid() check       │
     │                              │                            │
     │── Hello ───────────────────▶ │                            │
     │   { version: 1,              │                            │
     │     intent: Attach,          │                            │
     │     flags: 0,                │                            │
     │     rows: 24, cols: 80 }     │                            │
     │                              │── copy ring buffer         │
     │                              │   (snapshot for replay)    │
     │                              │                            │
     │◀──────────────── Welcome ──  │                            │
     │   { ..., ring_used: 4200 }   │                            │
     │                              │                            │
     │◀──────────────── Replay ───  │                            │
     │   [first 4093 bytes]         │                            │
     │                              │                            │
     │◀──────────────── Replay ───  │                            │
     │   [remaining 107 bytes]      │                            │
     │                              │     (PTY output during     │
     │                              │◀──── replay goes into ───  │
     │                              │     client outbound buf)   │
     │                              │                            │
     │◀──────────────── ReplayEnd   │                            │
     │                              │                            │
     │◀──────────────── Content ──  │  (buffered data drained)   │
     │   [bytes that arrived        │                            │
     │    during replay]            │                            │
     │                              │                            │
     ╔══════════════════════════════╗                             │
     ║     LIVE — bidirectional     ║                             │
     ╚══════════════════════════════╝                             │
     │                              │                            │
     │◀──────────────── Content ──  │◀──── PTY output ─────────  │
     │   [child output bytes]       │                            │
     │                              │                            │
     │── Content ────────────────▶  │──── PTY input ──────────▶  │
     │   [keyboard input]           │                            │
     │                              │                            │
     │── Resize ─────────────────▶  │                            │
     │   { rows: 50, cols: 120 }    │── ioctl(TIOCSWINSZ) ────▶  │
     │                              │── kill(SIGWINCH) ────────▶  │
     │                              │                            │
```

## Detach (`Ctrl-\`)

```
    CLI (client)                 Server
     │                              │
     │   user presses Ctrl-\        │
     │                              │
     │── Detach ─────────────────▶  │
     │                              │── remove client from list
     │── close() ─────────────────▶ │
     │                              │
     │   restore terminal           │
     │   print "detached"           │
     │                              │
```

## Unexpected disconnect (SSH drop, terminal close)

```
    CLI (client)                 Server
     │                              │
     │   connection drops           │
     ╳                              │
                                    │── poll: POLLHUP/POLLERR
                                    │── treat as implicit Detach
                                    │── remove client from list
                                    │── if was controller:
                                    │     promote next eligible
                                    │     send ResizeReq to new
                                    │     controller
```

## Child exits while attached

```
    CLI (client)                 Server                        Child
     │                              │                            │
     │                              │                  exit(0) ──│
     │                              │                            ╳
     │                              │◀─ SIGCHLD
     │                              │── waitpid() → status=0
     │                              │
     │◀──────────────── Exit ─────  │
     │   { status: 0 }              │
     │                              │
     │── close() ─────────────────▶ │
     │                              │
     │   restore terminal           │── (no clients remain AND
     │   print "session terminated  │    at least one was notified)
     │     with exit status 0"      │── unlink(socket)
     │                              │── exit
     │                              ╳
```

## Child exits during replay

```
    CLI (client)                 Server                        Child
     │                              │                            │
     │◀──────────────── Replay ───  │                  exit(1) ──│
     │                              │                            ╳
     │                              │◀─ SIGCHLD
     │                              │── waitpid() → status=1
     │                              │
     │◀──────────────── Replay ───  │  (replay continues —
     │   [remaining snapshot]       │   snapshot is a copy)
     │                              │
     │◀──────────────── ReplayEnd   │
     │                              │
     │◀──────────────── Exit ─────  │  (exit delivered after
     │   { status: 1 }              │   replay completes)
     │                              │
     │── close() ─────────────────▶ │
     ╳                              │── unlink + exit
                                    ╳
```

## Fast-exiting child (`mneme -c true`)

```
    CLI                          Server                        Child (true)
     │                              │                            │
     │── spawn ───────────────────▶ │                            │
     │                              │── setsid, bind, listen     │
     │                              │── openpty, spawn ────────▶ │
     │                              │                  exit(0) ──│
     │                              │                            ╳
     │                              │◀─ SIGCHLD, status=0
     │                              │── close(ready_fd)
     │                              │
     │◀─ EOF (ready) ───────────────│
     │                              │   Server stays alive:
     │── connect() ───────────────▶ │   child exited but no
     │── Hello(Attach) ──────────▶  │   client notified yet
     │                              │
     │◀──────────────── Welcome ──  │
     │   { child_running: 0,        │
     │     exit_status: 0 }         │
     │                              │
     │◀──────────────── Replay ···  │  (ring buffer contents)
     │◀──────────────── ReplayEnd   │
     │                              │
     │◀──────────────── Exit ─────  │
     │   { status: 0 }              │
     │                              │
     │── close() ─────────────────▶ │── client notified ✓
     │                              │── no clients remain
     │   print "session terminated  │── unlink + exit
     │     with exit status 0"      ╳
     │
```

## Attach to already-exited session (`mneme -a old`)

```
    CLI (client)                 Server
     │                              │   (child exited earlier,
     │                              │    another client was
     │                              │    notified and detached)
     │                              │
     │── connect() ───────────────▶ │
     │── Hello(Attach) ──────────▶  │
     │                              │
     │◀──────────────── Welcome ──  │
     │   { child_running: 0,        │
     │     exit_status: 0,          │
     │     ring_used: 50000 }       │
     │                              │
     │◀──────────────── Replay ···  │
     │◀──────────────── ReplayEnd   │
     │                              │
     │◀──────────────── Exit ─────  │
     │   { status: 0 }              │
     │                              │
     │── close() ─────────────────▶ │── unlink + exit
     ╳                              ╳
```

## Multiple clients — controlling client handoff

```
    Client A (controlling)       Server                     Client B (readonly)
     │                              │                            │
     │◀──── Content ──────────────  │──────── Content ─────────▶ │
     │   (both receive output)      │   (both receive output)    │
     │                              │                            │
     │── Resize ─────────────────▶  │                            │
     │   { rows: 50, cols: 120 }    │   (A is controlling,       │
     │                              │    resize applied to PTY)  │
     │                              │                            │
     │                              │◀──────── Content ────────  │
     │                              │   (B is readonly,          │
     │                              │    silently ignored)       │
     │                              │                            │
     │── Detach ─────────────────▶  │                            │
     │── close() ────────────────▶  │                            │
     │                              │── A gone; B has READONLY   │
     ╳                              │   flag, no new controller  │
                                    │                            │
```

```
    Client A (controlling)       Server                     Client B (attached)
     │                              │                            │
     │── Detach ─────────────────▶  │                            │
     │── close() ────────────────▶  │                            │
     │                              │── B becomes controlling    │
     ╳                              │                            │
                                    │──── ResizeReq ───────────▶ │
                                    │   (asks B to announce      │
                                    │    its terminal size)      │
                                    │                            │
                                    │◀──────── Resize ─────────  │
                                    │   { rows: 30, cols: 100 }  │
                                    │── ioctl(TIOCSWINSZ)        │
                                    │                            │
```

## Version mismatch

```
    CLI (client)                 Server (v2)
     │                              │
     │── Hello ───────────────────▶ │
     │   { version: 1, ... }        │
     │                              │── version != 2
     │◀──────────────── Error ────  │
     │   "protocol version          │
     │    mismatch: client=1        │
     │    server=2"                 │
     │                              │
     │── close() ─────────────────▶ │
     │                              │
```

## Slow client (any state)

```
    CLI (slow client)            Server                        Child
     │                              │                            │
     │◀──────────────── Content ──  │◀──── PTY output ─────────  │
     │   (client reads slowly)      │                            │
     │                              │── send() → EAGAIN          │
     │                              │── queue in outbound buf    │
     │                              │   (32 KB / 256 KB)         │
     │                              │                            │
     │                              │◀──── PTY output ─────────  │
     │                              │── queue in outbound buf    │
     │                              │   (200 KB / 256 KB)        │
     │                              │                            │
     │                              │◀──── PTY output ─────────  │
     │                              │── outbound buf full!        │
     │                              │── close(client socket)      │
     │                              │                            │
     │◀─ connection reset ────────  │                            │
     ╳                              │                            │
```

This applies identically in replay, live, and shutdown states —
one policy, one buffer, one limit.
