//! Tokio-based attach client.
//!
//! Setup is synchronous (connect, exchange Hello/Welcome, enter raw mode);
//! the bidirectional event loop runs on a current_thread runtime.
//!
//! Three tokio tasks (counting the main task itself):
//!  - **Server reader task** owns the read half of the UnixStream, parses
//!    packets, and forwards them to the main task via a `ServerEvent`
//!    channel.
//!  - **Server writer task** owns the write half and drains a `Vec<u8>`
//!    mpsc — used by the main task to send Content/Resize/Detach packets
//!    without serializing on a Mutex.
//!  - **Main task** drives stdin (`AsyncFd<RawFd>`), SIGWINCH
//!    (`tokio::signal::unix`), and reacts to ServerEvents (writing
//!    Replay/Content payloads to stdout via another `AsyncFd<RawFd>`).
//!
//! O_NONBLOCK on inherited stdio is restored in `NonblockGuard::drop` so
//! the parent shell isn't left in nonblocking mode.

use crate::protocol::{
    self, ClientFlags, HEADER_SIZE, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
};

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Attach result
// ---------------------------------------------------------------------------

pub enum AttachResult {
    Detached,
    Exited(u32),
    IoError(DisconnectReason),
}

pub enum DisconnectReason {
    ServerHungUp,
    ServerRead(io::Error),
    ServerWrite(io::Error),
    ServerError(String),
    #[allow(dead_code)] // referenced by tests
    InvalidExitPacket,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerHungUp => write!(f, "server hung up"),
            Self::ServerRead(e) => write!(f, "read from server failed: {e}"),
            Self::ServerWrite(e) => write!(f, "write to server failed: {e}"),
            Self::ServerError(msg) => write!(f, "server sent error: {msg}"),
            Self::InvalidExitPacket => write!(f, "invalid exit packet from server"),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

pub fn attach(
    socket_path: &Path,
    flags: ClientFlags,
    rows: u16,
    cols: u16,
    detach_key: u8,
    _quiet: bool,
) -> Result<AttachResult, crate::Error> {
    // Sync handshake -- blocking is fine and gives us crisp early errors.
    let stream = StdUnixStream::connect(socket_path)?;
    stream.set_nonblocking(false)?;

    let hello = protocol::Hello {
        version: PROTOCOL_VERSION,
        intent: Intent::Attach,
        flags,
        rows,
        cols,
    };
    protocol::send_packet(stream.as_fd(), &Packet::hello(&hello))?;

    let pkt = protocol::recv_packet(stream.as_fd())?;
    match pkt.msg_type {
        MsgType::Welcome => {
            let welcome = pkt.parse_welcome().ok_or("malformed welcome packet")?;
            if welcome.version != PROTOCOL_VERSION {
                return Err(format!(
                    "protocol version mismatch: client={}, server={}",
                    PROTOCOL_VERSION, welcome.version
                )
                .into());
            }
        }
        MsgType::Error => {
            let msg = pkt
                .parse_error()
                .unwrap_or_else(|| "unknown server error".into());
            return Err(msg.into());
        }
        _ => return Err("unexpected response from server".into()),
    }

    // Raw-mode termios guard before we go async.
    let _raw_guard = match RawTerminal::enter() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("mn: warning: could not set raw mode: {e}");
            None
        }
    };

    // Switch socket to nonblocking for tokio.
    stream.set_nonblocking(true)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let stream = UnixStream::from_std(stream)?;
        client_mainloop(stream, detach_key, flags).await
    })
}

// ---------------------------------------------------------------------------
// Raw terminal mode guard (RAII)
// ---------------------------------------------------------------------------

struct RawTerminal {
    orig: rustix::termios::Termios,
}

impl RawTerminal {
    fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let orig = rustix::termios::tcgetattr(&stdin)?;
        let mut raw = orig.clone();

        raw.input_modes &= !(rustix::termios::InputModes::IGNBRK
            | rustix::termios::InputModes::BRKINT
            | rustix::termios::InputModes::PARMRK
            | rustix::termios::InputModes::ISTRIP
            | rustix::termios::InputModes::INLCR
            | rustix::termios::InputModes::IGNCR
            | rustix::termios::InputModes::ICRNL
            | rustix::termios::InputModes::IXON);
        raw.output_modes &= !rustix::termios::OutputModes::OPOST;
        raw.local_modes &= !(rustix::termios::LocalModes::ECHO
            | rustix::termios::LocalModes::ECHONL
            | rustix::termios::LocalModes::ICANON
            | rustix::termios::LocalModes::ISIG
            | rustix::termios::LocalModes::IEXTEN);
        raw.control_modes &= !rustix::termios::ControlModes::CSIZE;
        raw.control_modes |= rustix::termios::ControlModes::CS8;
        raw.control_modes &= !rustix::termios::ControlModes::PARENB;

        raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;

        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;
        Ok(Self { orig })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let stdout = io::stdout();
        let _ = protocol::write_all_fd(
            stdout.as_fd(),
            b"\x1b[?1049l\x1b[<u\x1b[0m\x1b[?25h\x1b[?7h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[r\r\n",
        );
        let stdin = io::stdin();
        let _ =
            rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Flush, &self.orig);
    }
}

// ---------------------------------------------------------------------------
// O_NONBLOCK guard for inherited stdio.
//
// Setting NONBLOCK on a dup'd inherited descriptor affects the underlying
// open file description, so the parent shell can be left in nonblocking
// mode if we don't restore. We save the original flags here and restore
// them in Drop (alongside RawTerminal).
// ---------------------------------------------------------------------------

struct NonblockGuard {
    fd: RawFd,
    orig: rustix::fs::OFlags,
}

impl NonblockGuard {
    fn set(fd: RawFd) -> io::Result<Self> {
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let orig = rustix::fs::fcntl_getfl(bfd)?;
        rustix::fs::fcntl_setfl(bfd, orig | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self { fd, orig })
    }
}

impl Drop for NonblockGuard {
    fn drop(&mut self) {
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = rustix::fs::fcntl_setfl(bfd, self.orig);
    }
}

// ---------------------------------------------------------------------------
// AsyncFd over a raw stdio fd (we don't own it — never close on drop).
// ---------------------------------------------------------------------------

struct StdioFd(RawFd);

impl AsRawFd for StdioFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Server events
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ServerEvent {
    /// Bytes to write to local stdout (Replay or Content payload).
    Output(Vec<u8>),
    ReplayEnd,
    ResizeReq,
    Exit(u32),
    Error(String),
    HungUp,
    ReadFailed(io::Error),
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn client_mainloop(
    stream: UnixStream,
    detach_key: u8,
    flags: ClientFlags,
) -> Result<AttachResult, crate::Error> {
    let stdin_fd = io::stdin().as_raw_fd();
    let stdout_fd = io::stdout().as_raw_fd();

    let _stdin_nb = NonblockGuard::set(stdin_fd).ok();
    let _stdout_nb = NonblockGuard::set(stdout_fd).ok();

    // /dev/null and regular files can't be registered with kqueue; if
    // AsyncFd refuses, treat stdin as immediately at EOF so we just
    // wait for the server (e.g. to drive child exit through to the end).
    let stdin_async = AsyncFd::with_interest(StdioFd(stdin_fd), Interest::READABLE).ok();
    let stdout_async = AsyncFd::with_interest(StdioFd(stdout_fd), Interest::WRITABLE).ok();

    let stdin_is_tty = rustix::termios::isatty(unsafe { BorrowedFd::borrow_raw(stdin_fd) });
    let mut stdin_eof = stdin_async.is_none();

    let debug_file = std::env::var("MNEME_DEBUG").ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    let stdout_log = std::env::var("MNEME_STDOUT_LOG").ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    let csi_u_detach = csi_u_press_sequence(detach_key);

    // Split socket and spawn reader/writer tasks.
    let (read_half, write_half) = stream.into_split();
    let (server_evt_tx, mut server_evt_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let (out_pkt_tx, out_pkt_rx) = mpsc::channel::<Packet>(64);

    tokio::spawn(server_reader_task(read_half, server_evt_tx.clone()));
    tokio::spawn(server_writer_task(write_half, out_pkt_rx));

    let mut sigwinch = signal(SignalKind::window_change()).map_err(|e| format!("sigwinch: {e}"))?;

    let mut in_replay = true;
    let mut stdin_buf = vec![0u8; 4096];

    let mut stdout_log = stdout_log;
    let debug_file = debug_file;

    loop {
        // Stdin handling depends on whether replay is over and whether we've EOF'd.
        let stdin_active = !in_replay && !stdin_eof;

        tokio::select! {
            biased;

            // Server events ----------------------------------------------------
            evt = server_evt_rx.recv() => {
                let evt = match evt {
                    Some(e) => e,
                    None => return Ok(AttachResult::IoError(DisconnectReason::ServerHungUp)),
                };
                match evt {
                    ServerEvent::Output(bytes) => {
                        if let Some(ref mut f) = stdout_log {
                            use std::io::Write;
                            let _ = f.write_all(&bytes);
                        }
                        let res = match stdout_async.as_ref() {
                            Some(a) => write_async(a, &bytes).await,
                            None => {
                                let bfd = unsafe { BorrowedFd::borrow_raw(stdout_fd) };
                                protocol::write_all_fd(bfd, &bytes)
                            }
                        };
                        if let Err(e) = res {
                            return Ok(AttachResult::IoError(DisconnectReason::ServerWrite(e)));
                        }
                    }
                    ServerEvent::ReplayEnd => {
                        in_replay = false;
                        if let Err(e) = send_resize(&out_pkt_tx).await {
                            return Ok(AttachResult::IoError(DisconnectReason::ServerWrite(e)));
                        }
                    }
                    ServerEvent::ResizeReq => {
                        if let Err(e) = send_resize(&out_pkt_tx).await {
                            return Ok(AttachResult::IoError(DisconnectReason::ServerWrite(e)));
                        }
                    }
                    ServerEvent::Exit(status) => return Ok(AttachResult::Exited(status)),
                    ServerEvent::Error(msg) => {
                        return Ok(AttachResult::IoError(DisconnectReason::ServerError(msg)))
                    }
                    ServerEvent::HungUp => {
                        return Ok(AttachResult::IoError(DisconnectReason::ServerHungUp))
                    }
                    ServerEvent::ReadFailed(e) => {
                        return Ok(AttachResult::IoError(DisconnectReason::ServerRead(e)))
                    }
                }
            }

            // SIGWINCH ---------------------------------------------------------
            _ = sigwinch.recv(), if !in_replay => {
                if let Err(e) = send_resize(&out_pkt_tx).await {
                    return Ok(AttachResult::IoError(DisconnectReason::ServerWrite(e)));
                }
            }

            // Stdin readable ---------------------------------------------------
            res = async {
                match stdin_async.as_ref() {
                    Some(a) => a.readable().await.map(Some),
                    None => std::future::pending().await,
                }
            }, if stdin_active => {
                let mut guard = match res {
                    Ok(Some(g)) => g,
                    _ => continue,
                };
                let n = match guard.try_io(|inner| {
                    let fd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
                    match rustix::io::read(fd, &mut stdin_buf) {
                        Ok(0) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                        Ok(n) => Ok(n),
                        Err(rustix::io::Errno::AGAIN) => Err(io::ErrorKind::WouldBlock.into()),
                        Err(e) => Err(io::Error::from(e)),
                    }
                }) {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Ok(Err(_)) => {
                        if stdin_is_tty {
                            return Ok(AttachResult::Detached);
                        }
                        stdin_eof = true;
                        continue;
                    }
                    Err(_) => continue,
                };

                if let Some(f) = debug_file.as_ref() {
                    use std::io::Write;
                    let _ = writeln!(&*f, "stdin[{}]: {:02x?}", n, &stdin_buf[..n]);
                }

                let detach_pos = find_detach_key(&stdin_buf[..n], detach_key, &csi_u_detach);
                if let Some((pos, _len)) = detach_pos {
                    if pos > 0 && !flags.contains(ClientFlags::READONLY) {
                        for chunk in stdin_buf[..pos].chunks(MAX_PAYLOAD) {
                            let _ = out_pkt_tx.send(Packet::content(chunk)).await;
                        }
                    }
                    let _ = out_pkt_tx.send(Packet::empty(MsgType::Detach)).await;
                    return Ok(AttachResult::Detached);
                }

                if !flags.contains(ClientFlags::READONLY) {
                    for chunk in stdin_buf[..n].chunks(MAX_PAYLOAD) {
                        if out_pkt_tx.send(Packet::content(chunk)).await.is_err() {
                            return Ok(AttachResult::IoError(DisconnectReason::ServerHungUp));
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

async fn server_reader_task(mut r: OwnedReadHalf, tx: mpsc::UnboundedSender<ServerEvent>) {
    loop {
        let pkt = match recv_packet_half(&mut r).await {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                let _ = tx.send(ServerEvent::HungUp);
                return;
            }
            Err(e) => {
                let _ = tx.send(ServerEvent::ReadFailed(e));
                return;
            }
        };
        let evt = match pkt.msg_type {
            MsgType::Replay | MsgType::Content => ServerEvent::Output(pkt.payload),
            MsgType::ReplayEnd => ServerEvent::ReplayEnd,
            MsgType::ResizeReq => ServerEvent::ResizeReq,
            MsgType::Exit => match pkt.parse_exit_status() {
                Some(s) => ServerEvent::Exit(s),
                None => ServerEvent::Error("invalid exit packet".into()),
            },
            MsgType::Error => {
                ServerEvent::Error(pkt.parse_error().unwrap_or_else(|| "unknown".into()))
            }
            _ => continue,
        };
        if tx.send(evt).is_err() {
            return;
        }
    }
}

async fn server_writer_task(mut w: OwnedWriteHalf, mut rx: mpsc::Receiver<Packet>) {
    while let Some(pkt) = rx.recv().await {
        let buf = pkt.encode();
        if w.write_all(&buf).await.is_err() {
            return;
        }
    }
    let _ = w.shutdown().await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_resize(tx: &mpsc::Sender<Packet>) -> io::Result<()> {
    let (rows, cols) = crate::get_terminal_size();
    tx.send(Packet::resize(rows, cols))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
}

async fn write_async(out: &AsyncFd<StdioFd>, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut guard = out.writable().await?;
        match guard.try_io(|inner| {
            let fd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
            match rustix::io::write(fd, buf) {
                Ok(0) => Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => Ok(n),
                Err(rustix::io::Errno::AGAIN) => Err(io::ErrorKind::WouldBlock.into()),
                Err(e) => Err(io::Error::from(e)),
            }
        }) {
            Ok(Ok(n)) => buf = &buf[n..],
            Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Ok(Err(e)) => return Err(e),
            Err(_) => continue,
        }
    }
    Ok(())
}

async fn recv_packet_half(r: &mut OwnedReadHalf) -> io::Result<Packet> {
    let mut header = [0u8; HEADER_SIZE];
    r.read_exact(&mut header).await?;
    let msg_type = MsgType::from_u8(header[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown message type"))?;
    let len = u16::from_le_bytes([header[1], header[2]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Packet::new(msg_type, payload))
}

/// Build the CSI u (kitty keyboard protocol) encoding for a Ctrl-key press.
fn csi_u_press_sequence(detach_key: u8) -> Vec<u8> {
    if detach_key >= 0x20 {
        return Vec::new();
    }
    let base_codepoint = (detach_key as u32) | 0x40;
    format!("\x1b[{base_codepoint};5u").into_bytes()
}

fn find_detach_key(buf: &[u8], raw_key: u8, csi_u: &[u8]) -> Option<(usize, usize)> {
    if !csi_u.is_empty()
        && let Some(pos) = buf.windows(csi_u.len()).position(|w| w == csi_u)
    {
        return Some((pos, csi_u.len()));
    }
    if let Some(pos) = buf.iter().position(|&b| b == raw_key) {
        return Some((pos, 1));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::DisconnectReason;

    #[test]
    fn disconnect_reason_messages_are_actionable() {
        assert_eq!(DisconnectReason::ServerHungUp.to_string(), "server hung up");
        assert_eq!(
            DisconnectReason::ServerError("boom".into()).to_string(),
            "server sent error: boom"
        );
        assert_eq!(
            DisconnectReason::InvalidExitPacket.to_string(),
            "invalid exit packet from server"
        );
    }
}
