# mneme

mneme keeps terminal processes running when you detach or lose a connection.
The executable is `mn`. Each session has a background server, a PTY, and an
in-memory ring of recent output. Reattach to replay that history and continue.

mneme is not a terminal emulator or multiplexer. Use SSH for remote access.
The supported targets are macOS and Linux.

## Install

Install a current stable Rust toolchain with Rust 2024 edition support. From
this checkout, run:

```sh
cargo install --path . --locked
mn --help
```

Cargo installs `mn` in its binary directory, normally `~/.cargo/bin`. Ensure
that directory is on `PATH`. To build without installing, use
`cargo build --release --locked` and run `target/release/mn`.

## Examples

```sh
mn create work                 # create and attach, using the default shell
# Press Ctrl-\ to detach without terminating the session.
mn attach work                 # replay recent output, then continue
mn new build -- sh -c 'cargo build; exec sh'  # create without attaching
mn auto work                   # attach if it exists, otherwise create
mn attach -r work              # read-only attachment
mn attach -l work              # input allowed, but never controls PTY size
mn list                        # also the default when you run mn alone
mn kill build                  # terminate this session
```

Place options before the command. Use `--` before command arguments when
needed to separate them from `mn` options. Commands inherit the creator's
working directory and environment.

| Command | Alias | Purpose |
|---|---|---|
| `create NAME [CMD...]` | `c` | Create and attach |
| `new NAME [CMD...]` | `n` | Create without attaching |
| `attach NAME` | `a` | Attach to an existing session |
| `auto NAME [CMD...]` | `A` | Attach, or create and attach |
| `list` | `ls` | List responsive sessions |
| `kill NAME` | `rm` | Terminate one session |
| `kill all` | | Attempt to terminate every session in the selected runtime directory |

`kill all` is destructive. It reports individual failures but currently returns
success even if some sessions fail to terminate. `all` is a reserved target
for `kill`, even though it passes session-name validation.

Use `mn COMMAND --help` for available options:

- `-e KEY`: detach key for `create`, `attach`, and `auto`. The default is
  Ctrl-\ (`'^\'`). For example, `mn attach -e '^q' work` uses Ctrl-Q.
  A key is one byte or a caret followed by one byte.
- `-s SIZE`: ring capacity for `create`, `new`, and `auto`. The default is
  `1M`. `K` and `M` mean 1024 and 1048576 bytes. Zero and overflow are invalid.
  Capacity must fit protocol v1's `u32` and the platform's allocation limit.
- `-f`: replace an existing session for `create`, `new`, and the creation
  branch of `auto`. This terminates the old server before replacement.
  It is not a way to force an unresponsive, locked session to disappear.
- `-q`: suppress informational messages on commands other than `list`.

When `CMD` is omitted, mn selects the command in this order:

1. If `MNEME_CMD` is set, run its value through `/bin/sh -c`.
2. Otherwise, if `SHELL` is set, run its value as the executable.
3. Otherwise, run `/bin/sh`.

There is no `MNEME_SCROLLBACK` setting. Set capacity with `-s`.
See [shell completion](docs/COMPLETION.md) for optional completion.

## Detach, replay, and multiple clients

Ctrl-\ detaches locally. It does not kill the child. Detach attempts to flush
pending input and its protocol notification for up to 100 ms before closing.
Terminal cleanup restores termios and attempts a soft reset for up to 200 ms.
A blocked terminal can prevent the complete reset sequence from appearing.

Replay is raw byte history, not saved screen state. The oldest boundary can
split an escape sequence. Full-screen applications may need a redraw.
The ring overwrites the oldest bytes when full. It is not durable storage.

All writable clients can send input. The newest eligible live client controls
PTY size. Read-only and low-priority clients never control size. A slow reader
can pause output for *every* client and eventually block the child through
PTY backpressure. A full queue alone does not disconnect a running session's
client. Queries, new connections, and termination remain independently polled.

An attachment returns the child's exit status after ordered output delivery.
Signal termination maps to status 1. Detach returns 0. I/O or final-output
errors return 1. Explicit detach can abandon unread output.

## Runtime directory and security

mn selects its socket directory in this order:

1. Nonempty `$MNEME_SOCKET_DIR`, used directly. An unsafe override fails
   rather than falling back. Its parent must already exist.
2. `$XDG_RUNTIME_DIR/mneme`, if usable.
3. `$TMPDIR/mneme`, if usable.
4. `/tmp/mneme/<effective-uid>`, under a trusted sticky `/tmp/mneme` directory.

The private directory and its `.locks` subdirectory require owner-only mode
`0700`. Sockets and locks use `0600`. mn rejects unsafe ownership, permissions,
leaf symlinks, and unsafe ancestor directories. Parent symlinks such as macOS
`/var` are allowed. Client and server check peer UIDs on macOS and Linux.
This does not isolate a session from other processes belonging to the same user.

Session names contain 1–64 ASCII letters, digits, dots, underscores, or hyphens.
They cannot start with a dot or hyphen. Keep runtime paths short enough for
Unix socket path limits. Use the same environment when creating, listing,
attaching, and killing sessions.

Optional diagnostic variables append to user-selected files:
`MNEME_SERVER_LOG` (server stderr), `MNEME_PTY_LOG` (PTY bytes),
`MNEME_STDOUT_LOG` (client output), and `MNEME_DEBUG` (client diagnostics).
These files can expose commands, input, and output. Protect them and manage
their size. Normal scrollback stays in memory.

## Lifecycle and cleanup

Creation waits up to 10 seconds for explicit readiness, not pipe EOF. Success
means the server prepared its socket, child, runtime, PTY registration, and
initial buffers. It does not mean the command is still running. Setup failure
returns an error. A readiness timeout can leave a server still starting.
The error names its PID and socket path for inspection.

After child exit, the server drains final PTY bytes before sending `Exit`.
An inherited slave descriptor can prevent EOF. The server allows two seconds
of unblocked drain time, then reports incomplete output instead of success.
Clients get bounded five-second exit drains. A stalled client can lose
undelivered bytes when that deadline expires. A retry recovers only the
currently retained ring, not unlimited history. The server retains the session
if no writer confirms the terminal result. A very large or slow replay can
exceed the deadline on every retry. Use `mn kill NAME` to remove that retained
exited session. Query metadata reports the observed child's outcome. A final
`Error` can still report incomplete output capture instead of that child's status.

On macOS, a dying child can block in terminal flush before `WEXITED` becomes
observable. Post-observation drain timers do not cover this state. Resume
reading or use `mn kill` to end the session explicitly.

An exited session stays available until a writer sends its terminal result
and closes successfully, no clients remain, and managed cleanup finishes.
Thus `mn new result -- true` can wait for a later attachment. This is not an
archive: after notification and cleanup, the socket and in-memory history vanish.

`mn kill NAME` queries the server and sends SIGTERM. The server attempts
managed process-group cleanup, then removes its socket before releasing its
lock. The CLI waits up to two seconds for lock release. `-f` uses this same
termination path before it creates a replacement.

Managed cleanup also runs after normal direct-child exit, not only `mn kill`.
Same-group background jobs, including `nohup` jobs, can therefore be terminated
when the direct child exits. `nohup` alone does not escape group cleanup.

Managed shutdown targets the child's original group and, when observable and
revalidatable, the terminal's foreground group. It sends HUP, TERM, and CONT,
waits 300 ms, escalates to KILL, then attempts to reap the direct child within
500 ms. This is *not* whole-process-tree containment. Descendants that detach,
create new sessions, or occupy other groups can survive. Normal leader exit
can clear the foreground identity. Leaderless groups cannot be revalidated.
Portable group identity checks cannot eliminate every group-ID reuse race.
A process stuck in the kernel can exceed normal cleanup guarantees.

`list`, attach existence checks, and kill remove stale owned socket files only
while holding a lock guard. A failed query never authorizes removal of a locked
socket. Normal and error unwinds remove only the server's own socket inode.
SIGKILL or a crash can leave a stale socket for a later guarded cleanup.

### Persistent locks and upgrades

`.locks/NAME` files are *intentional persistent lock inodes*. Servers hold an
exclusive `flock`. Probes and cleaners hold shared guards. Do not remove lock
files or sweep `.locks` while mn processes or concurrent commands can run.
Unlinking a locked inode lets another process lock a different inode for the
same session. `mn kill all` leaves these files in place by design.

Older binaries used `NAME.lock` beside sockets. Current mn also locks an
existing safe legacy file but never creates or removes one. This reduces
upgrade collisions but does not guarantee mixed-version safety. Old binaries
can unlink or recreate legacy locks and do not honor `.locks/NAME`. Stop old
servers and avoid concurrent old commands before using a new binary in the
same runtime directory. Do not use old cleanup commands against new sessions.
Inspect unsafe legacy paths manually rather than deleting them automatically.

## Development and documentation

- [Design and operational policies](docs/DESIGN.md)
- [Protocol v1 and ordering](docs/PROTOCOL.md)
- [Tests and bounded verification commands](docs/E2E_TESTING.md)
- [Historical throughput experiment](docs/THROUGHPUT.md)
- [Historical Tokio-port measurements](docs/TOKIO_PORT.md)

Benchmark results describe past measurements. They do not prove current
correctness or replace the regression suite.
