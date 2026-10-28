# mneme design

mneme separates a terminal process from its attaching terminal. The binary is
`mn`. Each named session has one background server, one PTY, and an in-memory
ring buffer. macOS and Linux are the supported targets.

## Scope and CLI

mneme provides detach, reattach, output replay, and multiple clients. It does
not emulate a terminal, multiplex panes, group sessions, or provide networking.
Use SSH for remote access, for example `ssh -t host mn attach work`.

```sh
mn create work                 # create and attach
mn new build -- sh -c 'cargo build; exec sh'
mn attach work
mn auto work                   # attach, otherwise create
mn list                        # also mn with no arguments
mn kill work
```

These are subcommands, not the former `mneme -c` / `mneme -a` interface.
See the [README](../README.md) for options, environment, installation, runtime
directory selection, and cleanup precautions.

## Runtime architecture

Each server uses a Tokio `current_thread` runtime. It is not the former
`rustix::event::poll` server with self-pipe signal handling.

```text
CLI <--readiness status-- background mn --server
                              |
clients <--Unix streams--> main task <--AsyncFd--> PTY <---> child
                              |
                         ring buffer
```

Readiness status travels through an internal pipe from server to CLI.
`--server` is an internal entry point.
The CLI starts it in a separate process group with null stdin/stdout. Server
stderr goes to `/dev/null` unless `MNEME_SERVER_LOG` selects a log opened in
append mode. The server attempts `setsid()` and ignores SIGHUP. `setsid()` can fail when
it is already a group leader, so this is not a guaranteed double-fork daemon.

The child gets a new session and controlling PTY. Its pre-exec setup restores
the default SIGHUP disposition with async-signal-safe libc calls. Original
PTY descriptors and the readiness reporter are close-on-exec for the command.

### Server tasks

- The main task owns the ring, listener, child lifecycle, client registration,
  controller election, and PTY output fan-out.
- An intake task validates Hello with a five-second timeout, then forwards
  the stream and Hello to the main task for authoritative metadata.
- The main task builds Welcome from current child state, PID, ring usage,
  and attachment count. Queries schedule a bounded Welcome write and close.
  Attach registers and snapshots before its writer sends Welcome, then replay.
  The Attach count includes itself. Read-only and replaying clients count too.
  Counts saturate at `u16::MAX`. Welcome writes have a five-second timeout.
  No handshake socket write blocks the main task.
- Each registered client gets a writer and a reader. The writer sends its
  snapshot and `ReplayEnd`, then consumes a bounded outbound channel.
- The reader writes writable-client `Content` directly to the PTY under a
  shared mutex. Resize, detach, and disconnect events go to the main task.
- A control channel carries events. It is unbounded. Output channels are
  bounded, but that is not a total-memory bound against arbitrarily many
  same-user connections or control messages.

The signal path independently selects SIGTERM against the main loop. Managed
cleanup also runs on setup failure after spawn and on normal child exit.

### Attach client

The client synchronously connects, checks the peer UID, and exchanges
Hello/Welcome. It then uses a current-thread runtime with socket reader and
writer tasks. Its main task selects stdin, SIGWINCH, ordered server events,
partial stdout writes, and outbound capacity.

The output-event and outbound-packet channels each hold at most 64 items.
Output events carry at most 4093 bytes each. One active output packet and
in-flight tasks add bounded storage. Typeahead has a separate limit of
`16 * MAX_PAYLOAD` bytes. Resize requests coalesce rather than accumulating.

A stalled stdout stops further event consumption and eventually socket reads.
Partial-write offsets survive selection cancellation. Detach and resize stay
serviceable without awaiting an entire stdout packet. Detach responsiveness
still depends on available typeahead capacity. Once that finite buffer fills,
stdin pauses too. Ordinary input waits until replay ends. During a stdout
stall the client can recognize detach during replay and retain typeahead.

Terminal guards restore inherited nonblocking flags and termios. A best-effort
soft reset exits alternate screen, resets attributes and scrolling, shows the
cursor, and disables supported keyboard, paste, and mouse modes. Reset output
has a 200 ms deadline. Abnormal termination such as SIGKILL bypasses guards.

## Startup and readiness

1. Validate ring capacity in the CLI and server. The default is 1 MiB.
2. Acquire the stable exclusive session lock before stale socket replacement.
3. Bind the owned Unix socket and set mode `0600`.
4. Open the PTY and create the runtime. Install signal handling before spawn.
5. Spawn the child. Register PTY and listener with Tokio.
6. Allocate initial state, including the ring, then report explicit success.

The internal pipe uses a success byte or a failure byte with bounded error
text. EOF without success is a startup failure, not readiness. The CLI waits
at most 10 seconds. Timeout reports the PID and path but does not kill a
possibly-starting server. Readiness is independent of the child's longevity.
See [protocol and readiness](PROTOCOL.md) for framing details.

## Replay and controller policy

Registration copies the ring and installs the client's output queue without
an intervening await. The writer sends snapshot `Replay` packets followed by
`ReplayEnd`. All subsequent PTY bytes queue behind that snapshot, including
while the writer is still replaying. No registration-to-live output gap is
intentional. The snapshot can approach the configured ring size, not just 1 MiB.

The newest live client without `READONLY` or `LOW_PRIORITY` becomes controller.
Election occurs after replay completion and when clients leave. The server
requests the elected client's dimensions through `ResizeReq`. Hello dimensions
are not applied directly. Replaying clients are not eligible. No controller
exists if every client is read-only or low-priority.

Only controller resizes affect the PTY. All writable clients can send input,
serialized at packet boundaries. The normal CLI defers input during replay,
but the server reader does not reject raw-protocol `Content` during replay.
Read-only input is ignored. The wire protocol is defined in [PROTOCOL.md](PROTOCOL.md).

## Backpressure

Each registered client has a 64-packet outbound channel, roughly 256 KiB for
maximum-sized packets, plus a replay snapshot and in-flight writes. Before a
4096-byte PTY read, the main task checks every client's capacity for the
possible two packets. It pauses PTY ingestion when any client lacks capacity
or has pending ordered controls. It does not await a full channel in the
main control loop.

This policy throttles the producer through kernel PTY buffers. It does not
drop output or disconnect a running client merely because its queue fills.
One stalled client can stop output to all clients. Listener accepts, queries,
controller events, detach, and termination continue to be polled.

Resize controls coalesce. Terminal notifications and shutdown have separate
pending slots and enter the channel behind accepted output. A control slot
does not let `Exit` overtake content. Detach explicitly abandons delivery and
removes the client, releasing its backpressure.

## Child exit and final output

The server observes direct-child exit with `waitid(WNOWAIT)` before reaping.
It preserves that identity while bounded group cleanup runs alongside PTY
reads and controls. Signal termination maps to status 1.
Welcome/query status describes that observed process outcome, not capture
completion. A final incomplete-output `Error` can supersede successful child
status as the attachment result.

Child exit alone does not finalize output. The server keeps reading until
PTY EOF or EIO. Every accepted byte enters the ring and each registered
client queue before the terminal result. Writers send snapshot, `ReplayEnd`,
queued content, and finally `Exit`, then close their write half.

An inherited PTY slave can prevent EOF. A two-second drain budget counts only
time when PTY ingestion is not blocked by client backpressure. Blocked
intervals pause the budget but do not reset it. Exhaustion produces an
incomplete-output `Error`, not a successful `Exit`. PTY read errors also
report incomplete output.

At child exit, clients receive five-second drain deadlines. Attachments to an
exited session also get deadlines. Finalization starts a five-second terminal
drain. Expired clients disconnect with an undelivered-output server diagnostic.
Thus post-observation drain waits are bounded, but stalled clients can lose
output. On macOS, a dying child can block in terminal flush before `WEXITED`
becomes observable. Drain timers do not cover that pre-observation state.
It remains equivalent to a blocked live producer. Resume reading or use
`mn kill`. A retry recovers only the currently retained ring history.

A terminal packet merely entering a queue does not count as notification.
A writer must send the result and successfully shut down its write half.
The server then exits once no clients remain and group cleanup finishes.
This is a transport-level confirmation, not an application acknowledgment.
If no writer confirms delivery, the exited session remains queryable and
attachable. A new attachment receives the retained snapshot and result.
Queries do not consume the session. A huge or slow replay can exceed the
five-second deadline repeatedly, leaving the exited server retained. Use
`mn kill NAME` to terminate that retained session rather than retrying forever.

## Locking, paths, and security

Runtime directory selection and operator precautions appear in the
[README](../README.md#runtime-directory-and-security). Private directories
require current-user ownership and mode `0700`. Safe ancestor validation
permits canonicalized parent symlinks but rejects leaf symlinks. The fallback
shared base requires a trusted owner and sticky bit. Explicit unsafe overrides
fail closed instead of silently selecting another directory.

Sockets use `0600`. Both sides check peer credentials on Linux and macOS.
There is no network listener or privilege escalation. These checks do not
protect against malicious processes with the same UID. Diagnostic logs can
persist sensitive bytes even though normal ring history is memory-only.

A session's lock is `.locks/NAME`, outside the valid session-name namespace.
Locks are owned regular files, mode `0600`, with one hard link. Opens reject
symlinks. Servers use exclusive nonblocking `flock`. Probes and cleaners use
shared guards retained for the whole operation. Socket cleanup checks ownership
and Unix-socket type. Server cleanup additionally checks device/inode identity
and runs before lock release.

Lock inodes intentionally persist after shutdown, failed creation, queries,
and `kill all`. Never unlink them while servers or commands can still run.
Different inodes for one name defeat mutual exclusion. A failed query while
a lock remains held is not evidence that a socket can be removed.

Existing safe `NAME.lock` legacy files are also locked. Current mn never
creates or removes them, since that path can be another valid session name.
A socket there is treated as a session, not a legacy lock. Unsafe legacy
entries cause an error requiring manual inspection. Old binaries do not honor
the hidden locks and can replace legacy inodes. Mixed-version locking is
therefore not guaranteed. Stop old servers and avoid old commands in the
same runtime directory before upgrading.

`kill` queries the server PID and sends SIGTERM. It waits up to two seconds
for guarded lock release. `-f` follows this path before starting replacement.
An unresponsive locked server produces an error rather than an unsafe unlink.

## Managed shutdown limits

Cleanup also runs after normal direct-child exit. Same-group background and
`nohup` jobs can therefore receive termination signals after the leader exits.
`nohup` only changes hangup handling. It does not escape TERM/KILL group cleanup.

Cleanup sends HUP, TERM, and CONT to the original child group and an observed
foreground group when its leader still belongs to the child's session. After
300 ms it escalates to KILL and attempts direct-child reaping within 500 ms.
It never signals the old child PID after the child handle is reaped.

This is not a process-tree sweep or containment boundary. Detached descendants,
new sessions, and other job groups can survive. Normal leader exit can clear
the terminal's foreground group before observation. Leaderless groups cannot
be revalidated. Portable checks reduce but cannot eliminate group-ID reuse
races. Kernel-stuck children can defeat normal termination. SIGKILL of the
server bypasses managed cleanup and can leave stale sockets.

## Source map and verification

| Module | Responsibility |
|---|---|
| `main.rs` | Clap subcommands, create/list/kill, shell completion |
| `startup.rs` | Internal readiness pipe and ring capacity validation |
| `server.rs` | PTY setup, Tokio tasks, relay, replay, lifecycle |
| `client.rs` | Attach, raw terminal, bounded queues, stdio and detach |
| `protocol.rs` | Protocol v1 packets and shared synchronous/asynchronous codecs |
| `socket.rs` | Safe paths, persistent locks, UID checks, guarded cleanup |
| `ring.rs` | Circular byte history and snapshot |

Dependencies include Tokio, Clap, clap_complete, rustix, libc, and bitflags.
There is no terminal state parser or serialization framework. The binary
imports protocol, ring, and socket modules from the library rather than
compiling a second copy. Packet helpers share framing validation. A partially
consumed or sent frame is not resumable after error or cancellation. Discard
that connection. Socket read timeouts are per read, not whole-frame deadlines.

See [E2E_TESTING.md](E2E_TESTING.md) for regression coverage and check commands.
[THROUGHPUT.md](THROUGHPUT.md) and [TOKIO_PORT.md](TOKIO_PORT.md) preserve
historical measurements. Neither establishes current correctness, complete
backpressure safety, or current performance.
