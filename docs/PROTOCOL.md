# Protocol v1

This document describes the local Unix-stream protocol and its ordering rules.
Public commands use `mn create`, `mn new`, `mn attach`, `mn auto`, `mn list`,
and `mn kill`. The internal `--server` entry point is not a public subcommand.

## Transport and framing

Transport is `AF_UNIX`, `SOCK_STREAM`. Client and server verify peer UID on
macOS and Linux before exchanging protocol data. Sockets use mode `0600`
inside a private runtime directory. There is no network transport.

Every packet has this layout:

```text
[type: u8][payload_length: u16 little-endian][payload: length bytes]
```

The header is three bytes. Payloads contain at most 4093 bytes, for a maximum
packet size of 4096 bytes. All multibyte integers are little-endian. Stream
reads can fragment headers and payloads. Unknown message types and excessive
lengths fail decoding. Protocol version remains 1.

| Type | Value | Direction | Payload |
|---|---|---|---|
| `Hello` | 0 | client → server | 8-byte prefix, below |
| `Welcome` | 1 | server → client | 21-byte prefix, below |
| `Error` | 2 | server → client | UTF-8 diagnostic |
| `Content` | 3 | either | Raw terminal bytes |
| `Resize` | 4 | client → server | `rows: u16`, `cols: u16` |
| `ResizeReq` | 5 | server → client | Empty |
| `Detach` | 6 | client → server | Empty |
| `Exit` | 7 | server → client | `status: u32` |
| `Replay` | 8 | server → client | Raw historical bytes |
| `ReplayEnd` | 9 | server → client | Empty |

Structured parsers require their minimum payload length, not an exact length.
They accept extra trailing bytes. Unknown Hello flag bits are ignored. This
does not negotiate a new protocol version or change v1 packet layouts.

### Packet I/O helpers

The library provides shared synchronous and asynchronous packet helpers.
They validate framing, not message-specific roles or payload semantics.
Asynchronous framing is not resumable after cancellation. Packet I/O can
consume or transmit only part of a frame before returning an error. Discard
the connection after an I/O error or cancellation rather than retrying the
whole packet on the same stream.

Synchronous helpers return `WouldBlock` when a nonblocking read or socket
receive timeout interrupts a partial frame. Socket receive timeouts apply per
read, not to an entire frame. A peer that continually makes progress can
extend the total duration. Hello intake has a separate five-second timeout
for its entire operation. Welcome writes have their own five-second timeout.
Send helpers reject payloads larger than 4093 bytes before writing.

## Handshake and query

The first client packet must be `Hello`:

| Offset | Field | Type |
|---|---|---|
| 0 | version (`1`) | u8 |
| 1 | intent (`0` Query, `1` Attach) | u8 |
| 2 | flags (`0x01` READONLY, `0x02` LOW_PRIORITY) | u16 |
| 4 | terminal rows | u16 |
| 6 | terminal columns | u16 |

Query clients normally send zero flags and dimensions. Unknown intents or
short Hello payloads fail the handshake. Version mismatch returns `Error`
and closes. The intake handshake has a five-second timeout.

A successful handshake returns `Welcome`:

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | version | u8 | Server protocol version |
| 1 | server_pid | u32 | Session server PID |
| 5 | child_pid | u32 | Managed direct-child PID |
| 9 | child_running | u8 | Nonzero until child exit is observed |
| 10 | exit_status | u8 | Observed child status when no longer running |
| 11 | client_count | u16 | Currently registered attachments |
| 13 | ring_size | u32 | Configured history capacity |
| 17 | ring_used | u32 | Retained history length |

Metadata is a point-in-time view. Welcome is not an output-completion
notification. Child status can be available while final output still drains.
Welcome/query exit status reports the observed process outcome. A subsequent
terminal `Error` can report incomplete capture even if the child exited zero.
The main task builds metadata when it handles Hello, not at socket accept.
An Attach Welcome includes its own new registration. Counts include replaying
and read-only clients and saturate at `u16::MAX`. Query connections do not
register, contribute to the count, or become controllers. They receive Welcome
and close without replay or exit notification.

`mn list` queries sockets in the selected runtime directory and sorts names.
Its status markers are blank for running, `*` for running with attachments,
and `+` for observed-exited. Locked but unresponsive sessions can be omitted.
Listing can remove stale owned sockets under a lock guard. A query failure
alone never permits removal of a locked socket.

## Attach and ordered replay

```text
Client                          Server
  | ---- Hello(Attach) ----------> |
  |                                | register and snapshot ring
  | <--- Welcome ----------------- |
  | <--- Replay(snapshot)* ------- |
  | <--- ReplayEnd --------------- |
  | <--- Content(after snapshot)*  |
  | <--- ResizeReq, if elected --- |
  | ---- Resize / Content -------> |
```

Snapshot and queue registration have no intervening await. The writer sends
all snapshot chunks before `ReplayEnd`, then consumes its ordered outbound
queue. Every subsequent PTY byte queues for that client, even while replay
is in progress. The snapshot can occupy the configured ring capacity.

The normal CLI displays replay before forwarding ordinary input. It can
retain bounded typeahead while stdout stalls and still recognize detach until
that input buffer fills. This is a client policy, not server-side input gating:
a writable raw-protocol client can send `Content` during replay.

The newest live client without READONLY or LOW_PRIORITY controls PTY size.
Replaying clients are not eligible. On election or handoff, the server sends
`ResizeReq`. The client responds with current dimensions through `Resize`.
Hello dimensions are not applied directly. Non-controller resizes are ignored.
Read-only clients cannot write input or control size. Low-priority clients can
write input but never become controllers, even if no regular client remains.
Writable clients' input interleaves at packet boundaries under a PTY mutex.

## Backpressure and disconnect

Each registered client has a bounded 64-packet output channel. The snapshot,
a packet being written, and kernel buffers occupy additional space. The main
task reserves enough capacity for one PTY read across *all* client channels.
If any channel lacks capacity, PTY ingestion pauses. Kernel buffers then
throttle the child. New accepts, query responses, detach, controller events,
and SIGTERM remain independently serviceable.

A full output queue alone does not evict a running client's connection. A
nonreading client can therefore stall output for all clients indefinitely
while the child remains live. This policy preserves output rather than
silently dropping it. It is not a total-memory or unlimited-client guarantee.
The internal control channel is unbounded.

The CLI also bounds both server events and outbound packets to 64 items.
A stalled stdout propagates backpressure to socket reads. It writes one
output packet incrementally and preserves offsets across selection. Terminal
results remain behind preceding output events. Resize requests coalesce.

`Detach` or socket EOF removes the attachment. Detach abandons unread output
rather than waiting for its delivery. The CLI allows 100 ms for pending input
and the Detach packet, then closes locally. If a reader is blocked writing
input into the PTY, server-side disconnect processing can wait for that write.
SIGTERM remains independent of client readers.

## Exit, final output, and retention

```text
Child                           Server                         Client
  | -- exit --------------------> |                               |
  |                               | observe waitid(WNOWAIT)        |
  |                               | start managed group cleanup   |
  |                               | drain PTY alongside controls  |
  |                               | -- remaining Content -------> |
  |                               | -- Exit(status) ------------> |
  |                               | -- write-half close --------> |
```

The server observes exit without reaping first, preserving child identity
for group cleanup. Normal exit codes become terminal status. Signal termination
maps to 1. `Exit` carries a `u32`, while Welcome retains its v1 `u8` status.

PTY EOF or EIO, not child exit alone, permits successful finalization. Every
accepted byte precedes the terminal packet. For replaying clients, ordering is
snapshot, `ReplayEnd`, queued `Content`, terminal result, then EOF. No new
controller request follows observed child exit. A late attachment receives
the retained ring snapshot and then the finalized result. Retention is bounded
by the ring capacity, not an unlimited recovery log.

Drain policy has two separate limits:

- After observed exit, a client has five seconds to drain, including replay.
  Late attachments to observed-exited sessions also receive a deadline.
  Finalization grants a five-second final writer flush. Expiry aborts that
  client's tasks and logs an undelivered-output diagnostic. A slow but
  progressing client can still exceed this fixed deadline.
- The PTY has two seconds of *unblocked ingestion time* to reach EOF after
  observed exit. Client backpressure pauses this budget without resetting it.
  Inherited slave descriptors can prevent EOF. Budget exhaustion produces
  `Error` describing incomplete output, never a successful `Exit`. Accepted
  output precedes that Error. An undeliverable Error need not reach a full
  socket before disconnection.

On macOS, terminal flushing can block a dying child before `WEXITED` becomes
observable. These post-observation timers do not cover that state. It remains
equivalent to a blocked live producer. Resume reading or use `mn kill`.
Explicit SIGTERM bypasses stalled drains through bounded managed cleanup.

Queueing a terminal result does not mark the session notified. A writer must
send it and successfully close its write half. This is not an application
acknowledgment. If no writer confirms, the server retains the session for
another attachment. Queries do not count as notification. A huge or slow
replay can exceed the five-second deadline on every retry, retaining the
exited server. Use `mn kill NAME` to remove it explicitly. Once a result is
confirmed, no clients remain, and managed cleanup finishes, the server removes
its socket and exits. A failed drain returns a server error after notification.

Thus `mn create done -- true` can attach after the child exits. Likewise,
`mn new done -- true` can wait for a later attachment. An exited session is
not a permanent archive after successful notification and final cleanup.

## Startup status is not protocol v1

Creation uses a separate internal pipe, not a session packet or socketpair:

```text
CLI                             Server
  | -- spawn mn --server -------> |
  |                               | lock, bind, open PTY
  |                               | create runtime, spawn child
  |                               | register fds, allocate ring/state
  | <--- explicit status byte ---- |
  | -- connect and Hello --------> |  (create/auto attach path)
```

Status `0` is success. Status `1` precedes up to 4095 bytes of failure text.
An empty EOF is failure. Success does not require pipe EOF. The CLI waits
at most 10 seconds, including incomplete status frames. Timeout reports the
server PID and socket but does not guarantee that the server stopped.
The status fd survives server exec and becomes close-on-exec for the child.
These status bytes do not change protocol v1.

## Kill, locks, and operational limits

`mn kill NAME` queries Welcome for the server PID, sends SIGTERM, and waits
up to two seconds for lock release. `-f` uses this path before replacement.
There is no v1 Kill message. Server cleanup signals managed process groups
with a 300 ms grace and a 500 ms direct-child reap deadline. It does not
promise to kill all descendants or prevent all group-ID reuse races. Managed
cleanup also runs after normal direct-child exit. Same-group background jobs,
including `nohup` jobs, can be terminated when that child exits.

Stable `.locks/NAME` inodes intentionally persist. Never sweep these locks
while commands or servers can run. Existing safe legacy `NAME.lock` files
are also locked, but current mn never creates or removes them. Old binaries
can replace those files and ignore hidden locks, so concurrent old/new use
in one directory is not guaranteed safe. See [DESIGN.md](DESIGN.md) and the
[README cleanup policy](../README.md#persistent-locks-and-upgrades).
