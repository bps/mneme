use crate::protocol::{self, ClientFlags, Intent, MsgType, PROTOCOL_VERSION, Packet};

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
) -> Result<AttachResult, crate::Error> {
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

    // Switch to non-blocking for the event loop
    stream.set_nonblocking(true)?;

    // Set terminal to raw mode (if stdin is a tty)
    let _raw_guard = match RawTerminal::enter() {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("mn: warning: could not set raw mode: {e}");
            None
        }
    };

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

        // Intentionally do NOT enter the alternate screen. The ring-buffer
        // replay drives the outer terminal into whatever state the child
        // is in (including altscreen, if a TUI is running), and we want
        // pre-attach terminal content to remain in scrollback.

        Ok(Self { orig })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Targeted soft reset: guarantee the outer terminal is left in a
        // usable state regardless of where the child's output stream
        // stopped (mid-SGR, mid-altscreen, mouse-reporting enabled, etc.).
        // We deliberately avoid a full RIS (\ec), which would clear
        // scrollback on some terminals.
        //
        //   \e[?1049l   exit altscreen if the child was in it (no-op otherwise)
        //   \e[<u       pop kitty keyboard protocol
        //   \e[0m       reset SGR
        //   \e[?25h     show cursor
        //   \e[?7h      autowrap on
        //   \e[?2004l   bracketed paste off
        //   \e[?1000l..1006l  mouse reporting off (normal, button, any, SGR)
        //   \e[r        reset scroll region
        //   \r\n        start the shell prompt on a fresh line
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
) -> Result<AttachResult, crate::Error> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    // Debug: log stdin bytes to a file if MNEME_DEBUG is set
    let debug_file = std::env::var("MNEME_DEBUG").ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    // Set up SIGWINCH self-pipe
    let (sig_read, sig_write) = {
        let (r, w) = rustix::pipe::pipe()?;
        set_fd_nonblocking(&r)?;
        set_fd_nonblocking(&w)?;
        (r, w)
    };
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, sig_write)?;

    // Build the CSI u (kitty keyboard protocol) encoding of the detach key.
    // When a TUI app enables this protocol, Ctrl-\ arrives as ESC[92;5u
    // instead of byte 0x1C.
    let csi_u_detach = csi_u_press_sequence(detach_key);

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

        // Keyboard input — check FIRST so detach key is never starved
        // by server output
        if pollfds[0].revents().contains(PollFlags::IN) && !in_replay {
            let n = match protocol::try_read(stdin.as_fd(), &mut read_buf) {
                Ok(0) => 0, // would block
                Ok(n) => n,
                Err(_) => return Ok(AttachResult::Detached), // stdin closed
            };

            if n > 0 {
                // Debug log
                if let Some(ref mut f) = debug_file.as_ref() {
                    use std::io::Write;
                    let _ = writeln!(f, "stdin[{}]: {:02x?}", n, &read_buf[..n]);
                }

                // Check for detach key: raw byte or CSI u encoded
                let detach_pos = find_detach_key(&read_buf[..n], detach_key, &csi_u_detach);
                if let Some((pos, _len)) = detach_pos {
                    // Send any bytes before the detach key
                    if pos > 0 && !flags.contains(ClientFlags::READONLY) {
                        let pkt = Packet::content(&read_buf[..pos]);
                        let _ = protocol::send_packet(stream.as_fd(), &pkt);
                    }
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
        }

        // Data from server
        if pollfds[1]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP)
        {
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

        // Server disconnected
        if pollfds[1].revents().contains(PollFlags::HUP)
            && !pollfds[1].revents().contains(PollFlags::IN)
        {
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
    protocol::send_packet(stream.as_fd(), &pkt).map_err(io::Error::other)
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

/// Build the CSI u (kitty keyboard protocol) encoding for a Ctrl-key press.
/// E.g., Ctrl-\ (0x1C) → ESC[92;5u (backslash codepoint=92, Ctrl modifier=5).
fn csi_u_press_sequence(detach_key: u8) -> Vec<u8> {
    if detach_key >= 0x20 {
        return Vec::new(); // not a control character
    }
    // Control character 0x01..0x1F corresponds to Ctrl + (key | 0x40)
    // e.g., 0x1C (Ctrl-\) → base char = 0x5C = 92 decimal
    let base_codepoint = (detach_key as u32) | 0x40;
    // CSI u format: ESC [ <codepoint> ; <modifiers> u
    // Ctrl modifier = 5 (modifier bits + 1, where Ctrl = bit 2 = 4, +1 = 5)
    format!("\x1b[{base_codepoint};5u").into_bytes()
}

/// Find the detach key in a byte buffer, checking both raw byte and
/// CSI u (kitty keyboard protocol) encoding.
/// Returns Some((position, length)) or None.
fn find_detach_key(buf: &[u8], raw_key: u8, csi_u: &[u8]) -> Option<(usize, usize)> {
    // Check CSI u first (longer match takes priority to avoid partial matches)
    if !csi_u.is_empty()
        && let Some(pos) = buf.windows(csi_u.len()).position(|w| w == csi_u)
    {
        return Some((pos, csi_u.len()));
    }
    // Check raw byte
    if let Some(pos) = buf.iter().position(|&b| b == raw_key) {
        return Some((pos, 1));
    }
    None
}
