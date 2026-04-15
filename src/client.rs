use crate::protocol::{self, ClientFlags, Intent, MsgType, Packet, PROTOCOL_VERSION};

use rustix::event::{PollFd, PollFlags};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

// ---------------------------------------------------------------------------
// Attach result
// ---------------------------------------------------------------------------

pub enum AttachResult {
    Detached,
    Exited(u32),
    IoError,
}

// ---------------------------------------------------------------------------
// Attach to a session
// ---------------------------------------------------------------------------

pub fn attach(
    socket_path: &Path,
    flags: ClientFlags,
    rows: u16,
    cols: u16,
    detach_key: u8,
    _quiet: bool,
) -> Result<AttachResult, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_nonblocking(false)?; // blocking for handshake

    // Send Hello
    let hello = protocol::Hello {
        version: PROTOCOL_VERSION,
        intent: Intent::Attach,
        flags,
        rows,
        cols,
    };
    protocol::send_packet(stream.as_fd(), &Packet::hello(&hello))?;

    // Receive Welcome
    let pkt = protocol::recv_packet(stream.as_fd())?;
    match pkt.msg_type {
        MsgType::Welcome => {
            let welcome = pkt
                .parse_welcome()
                .ok_or("malformed welcome packet")?;
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

    // Switch to non-blocking for the event loop
    stream.set_nonblocking(true)?;

    // Set terminal to raw mode (if stdin is a tty)
    let _raw_guard = RawTerminal::enter().ok();

    // Run the client event loop
    client_mainloop(stream, detach_key, flags)
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

        // cfmakeraw equivalent
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

        // VMIN=1, VTIME=0
        raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;

        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;

        Ok(Self { orig })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let stdin = io::stdin();
        let _ = rustix::termios::tcsetattr(
            &stdin,
            rustix::termios::OptionalActions::Flush,
            &self.orig,
        );
    }
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

fn set_fd_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

fn client_mainloop(
    stream: UnixStream,
    detach_key: u8,
    flags: ClientFlags,
) -> Result<AttachResult, Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    // Set up SIGWINCH self-pipe
    let (sig_read, sig_write) = {
        let (r, w) = rustix::pipe::pipe()?;
        set_fd_nonblocking(&r)?;
        set_fd_nonblocking(&w)?;
        (r, w)
    };
    signal_hook::low_level::pipe::register(libc::SIGWINCH, sig_write)?;

    // Send initial resize
    send_resize(&stream)?;

    let mut read_buf = [0u8; 4096];
    let mut in_replay = true;

    // Accumulation buffer for partial packets from server
    let mut server_buf: Vec<u8> = Vec::new();

    loop {
        let mut pollfds: Vec<PollFd<'_>> = vec![
            PollFd::new(&stdin, PollFlags::IN),
            PollFd::new(&stream, PollFlags::IN),
            PollFd::new(&sig_read, PollFlags::IN),
        ];

        match rustix::event::poll(&mut pollfds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }

        // SIGWINCH
        if pollfds[2].revents().contains(PollFlags::IN) {
            // Drain signal pipe
            let mut sig_buf = [0u8; 64];
            let _ = rustix::io::read(&sig_read, &mut sig_buf);

            if !in_replay {
                send_resize(&stream)?;
            }
        }

        // Data from server
        if pollfds[1].revents().intersects(PollFlags::IN | PollFlags::HUP) {
            let n = match protocol::try_read(stream.as_fd(), &mut read_buf) {
                Ok(0) => continue, // would block
                Ok(n) => n,
                Err(_) => return Ok(AttachResult::IoError),
            };

            server_buf.extend_from_slice(&read_buf[..n]);

            // Process complete packets from the buffer
            while let Some((pkt, consumed)) = try_parse_packet(&server_buf) {
                server_buf.drain(..consumed);

                match pkt.msg_type {
                    MsgType::Replay | MsgType::Content => {
                        // Write to stdout
                        let _ = protocol::write_all_fd(stdout.as_fd(), &pkt.payload);
                    }
                    MsgType::ReplayEnd => {
                        in_replay = false;
                        // Send resize now that we're live
                        send_resize(&stream)?;
                    }
                    MsgType::ResizeReq => {
                        send_resize(&stream)?;
                    }
                    MsgType::Exit => {
                        if let Some(status) = pkt.parse_exit_status() {
                            return Ok(AttachResult::Exited(status));
                        }
                        return Ok(AttachResult::IoError);
                    }
                    MsgType::Error => {
                        return Ok(AttachResult::IoError);
                    }
                    _ => {} // ignore unexpected
                }
            }
        }

        // Keyboard input
        if pollfds[0].revents().contains(PollFlags::IN) && !in_replay {
            let n = match protocol::try_read(stdin.as_fd(), &mut read_buf) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => return Ok(AttachResult::Detached), // stdin closed
            };

            // Check for detach key
            if n == 1 && read_buf[0] == detach_key {
                let pkt = Packet::empty(MsgType::Detach);
                let _ = protocol::send_packet(stream.as_fd(), &pkt);
                return Ok(AttachResult::Detached);
            }

            // Send to server (if not readonly)
            if !flags.contains(ClientFlags::READONLY) {
                let pkt = Packet::content(&read_buf[..n]);
                if protocol::send_packet(stream.as_fd(), &pkt).is_err() {
                    return Ok(AttachResult::IoError);
                }
            }
        }

        // Server disconnected
        if pollfds[1].revents().contains(PollFlags::HUP) && !pollfds[1].revents().contains(PollFlags::IN) {
            return Ok(AttachResult::IoError);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_resize(stream: &UnixStream) -> io::Result<()> {
    let (rows, cols) = crate::get_terminal_size();
    let pkt = Packet::resize(rows, cols);
    protocol::send_packet(stream.as_fd(), &pkt)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

/// Try to parse a complete packet from the front of a byte buffer.
/// Returns (packet, bytes_consumed) or None if not enough data.
fn try_parse_packet(buf: &[u8]) -> Option<(Packet, usize)> {
    if buf.len() < protocol::HEADER_SIZE {
        return None;
    }
    let msg_type = protocol::MsgType::from_u8(buf[0])?;
    let len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
    if len > protocol::MAX_PAYLOAD {
        return None;
    }
    let total = protocol::HEADER_SIZE + len;
    if buf.len() < total {
        return None;
    }
    let payload = buf[protocol::HEADER_SIZE..total].to_vec();
    Some((Packet::new(msg_type, payload), total))
}
