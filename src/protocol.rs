use std::io;
use std::os::fd::BorrowedFd;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 3; // type(1) + len(2)
pub const MAX_PAYLOAD: usize = 4093;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    Hello = 0,
    Welcome = 1,
    Error = 2,
    Content = 3,
    Resize = 4,
    ResizeReq = 5,
    Detach = 6,
    Exit = 7,
    Replay = 8,
    ReplayEnd = 9,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Hello),
            1 => Some(Self::Welcome),
            2 => Some(Self::Error),
            3 => Some(Self::Content),
            4 => Some(Self::Resize),
            5 => Some(Self::ResizeReq),
            6 => Some(Self::Detach),
            7 => Some(Self::Exit),
            8 => Some(Self::Replay),
            9 => Some(Self::ReplayEnd),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client intent & flags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Intent {
    Query = 0,
    Attach = 1,
}

impl Intent {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Query),
            1 => Some(Self::Attach),
            _ => None,
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClientFlags: u16 {
        const READONLY     = 0x01;
        const LOW_PRIORITY = 0x02;
    }
}

// ---------------------------------------------------------------------------
// Structured messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Hello {
    pub version: u8,
    pub intent: Intent,
    pub flags: ClientFlags,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone)]
pub struct Welcome {
    pub version: u8,
    pub server_pid: u32,
    pub child_pid: u32,
    pub child_running: bool,
    pub exit_status: u8,
    pub client_count: u16,
    pub ring_size: u32,
    pub ring_used: u32,
}

// ---------------------------------------------------------------------------
// Packet — the wire unit
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Packet {
    pub msg_type: MsgType,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Packet")
            .field("msg_type", &self.msg_type)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Packet {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        debug_assert!(payload.len() <= MAX_PAYLOAD);
        Self { msg_type, payload }
    }

    pub fn empty(msg_type: MsgType) -> Self {
        Self::new(msg_type, Vec::new())
    }

    // -- Constructors for specific message types ----------------------------

    pub fn hello(hello: &Hello) -> Self {
        let mut buf = Vec::with_capacity(8);
        buf.push(hello.version);
        buf.push(hello.intent as u8);
        buf.extend_from_slice(&hello.flags.bits().to_le_bytes());
        buf.extend_from_slice(&hello.rows.to_le_bytes());
        buf.extend_from_slice(&hello.cols.to_le_bytes());
        Self::new(MsgType::Hello, buf)
    }

    pub fn welcome(w: &Welcome) -> Self {
        let mut buf = Vec::with_capacity(20);
        buf.push(w.version);
        buf.extend_from_slice(&w.server_pid.to_le_bytes());
        buf.extend_from_slice(&w.child_pid.to_le_bytes());
        buf.push(u8::from(w.child_running));
        buf.push(w.exit_status);
        buf.extend_from_slice(&w.client_count.to_le_bytes());
        buf.extend_from_slice(&w.ring_size.to_le_bytes());
        buf.extend_from_slice(&w.ring_used.to_le_bytes());
        Self::new(MsgType::Welcome, buf)
    }

    pub fn error(msg: &str) -> Self {
        Self::new(MsgType::Error, msg.as_bytes().to_vec())
    }

    pub fn content(data: &[u8]) -> Self {
        Self::new(MsgType::Content, data.to_vec())
    }

    pub fn replay(data: &[u8]) -> Self {
        Self::new(MsgType::Replay, data.to_vec())
    }

    pub fn resize(rows: u16, cols: u16) -> Self {
        let mut buf = Vec::with_capacity(4);
        buf.extend_from_slice(&rows.to_le_bytes());
        buf.extend_from_slice(&cols.to_le_bytes());
        Self::new(MsgType::Resize, buf)
    }

    pub fn exit(status: u32) -> Self {
        Self::new(MsgType::Exit, status.to_le_bytes().to_vec())
    }

    // -- Parsers for payload ------------------------------------------------

    pub fn parse_hello(&self) -> Option<Hello> {
        if self.msg_type != MsgType::Hello || self.payload.len() < 8 {
            return None;
        }
        let p = &self.payload;
        Some(Hello {
            version: p[0],
            intent: Intent::from_u8(p[1])?,
            flags: ClientFlags::from_bits_truncate(u16::from_le_bytes([p[2], p[3]])),
            rows: u16::from_le_bytes([p[4], p[5]]),
            cols: u16::from_le_bytes([p[6], p[7]]),
        })
    }

    pub fn parse_welcome(&self) -> Option<Welcome> {
        if self.msg_type != MsgType::Welcome || self.payload.len() < 21 {
            return None;
        }
        let p = &self.payload;
        Some(Welcome {
            version: p[0],
            server_pid: u32::from_le_bytes([p[1], p[2], p[3], p[4]]),
            child_pid: u32::from_le_bytes([p[5], p[6], p[7], p[8]]),
            child_running: p[9] != 0,
            exit_status: p[10],
            client_count: u16::from_le_bytes([p[11], p[12]]),
            ring_size: u32::from_le_bytes([p[13], p[14], p[15], p[16]]),
            ring_used: u32::from_le_bytes([p[17], p[18], p[19], p[20]]),
        })
    }

    pub fn parse_resize(&self) -> Option<(u16, u16)> {
        if self.msg_type != MsgType::Resize || self.payload.len() < 4 {
            return None;
        }
        let rows = u16::from_le_bytes([self.payload[0], self.payload[1]]);
        let cols = u16::from_le_bytes([self.payload[2], self.payload[3]]);
        Some((rows, cols))
    }

    pub fn parse_exit_status(&self) -> Option<u32> {
        if self.msg_type != MsgType::Exit || self.payload.len() < 4 {
            return None;
        }
        Some(u32::from_le_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]))
    }

    pub fn parse_error(&self) -> Option<String> {
        if self.msg_type != MsgType::Error {
            return None;
        }
        String::from_utf8(self.payload.clone()).ok()
    }

    // -- Wire encoding/decoding ---------------------------------------------

    /// Encode to wire format: [type:u8][len:u16 LE][payload]
    pub fn encode(&self) -> Vec<u8> {
        let len = self.payload.len() as u16;
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        buf.push(self.msg_type as u8);
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }
}

// ---------------------------------------------------------------------------
// I/O helpers — blocking, retry on EINTR
// ---------------------------------------------------------------------------

/// Write all bytes to fd, retrying on EINTR. Returns Ok(()) or the first
/// real error.
pub fn write_all_fd(fd: BorrowedFd<'_>, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match rustix::io::write(fd, buf) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => buf = &buf[n..],
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read exactly `buf.len()` bytes from fd. Returns Ok(()) or error.
/// Returns UnexpectedEof if the fd closes before all bytes are read.
/// Returns WouldBlock if no data is available (non-blocking fd).
pub fn read_exact_fd(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::read(fd, &mut buf[pos..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => pos += n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => {
                if pos == 0 {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                // Partial read: keep trying (data is in-flight)
                std::thread::yield_now();
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Send a packet on a blocking fd.
pub fn send_packet(fd: BorrowedFd<'_>, pkt: &Packet) -> io::Result<()> {
    let encoded = pkt.encode();
    write_all_fd(fd, &encoded)
}

/// Receive a packet from a blocking fd.
pub fn recv_packet(fd: BorrowedFd<'_>) -> io::Result<Packet> {
    let mut header = [0u8; HEADER_SIZE];
    read_exact_fd(fd, &mut header)?;

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
        read_exact_fd(fd, &mut payload)?;
    }

    Ok(Packet::new(msg_type, payload))
}

// ---------------------------------------------------------------------------
// Non-blocking I/O helpers for the server event loop
// ---------------------------------------------------------------------------

/// Result of a non-blocking write attempt.
pub enum WriteResult {
    /// All bytes written.
    Complete,
    /// Partial write — `written` bytes consumed, rest must be buffered.
    Partial(usize),
    /// Would block — nothing written, buffer everything.
    WouldBlock,
    /// Peer gone or fatal error.
    Error,
}

/// Attempt a non-blocking write. Does NOT retry on EAGAIN.
pub fn try_write(fd: BorrowedFd<'_>, buf: &[u8]) -> WriteResult {
    let mut written = 0;
    while written < buf.len() {
        match rustix::io::write(fd, &buf[written..]) {
            Ok(0) => {
                return WriteResult::Error;
            }
            Ok(n) => written += n,
            Err(rustix::io::Errno::AGAIN) => {
                if written > 0 {
                    return WriteResult::Partial(written);
                }
                return WriteResult::WouldBlock;
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return WriteResult::Error,
        }
    }
    WriteResult::Complete
}

/// Read up to buf.len() bytes non-blocking. Returns number of bytes read,
/// Ok(0) for would-block, Err for real errors/EOF.
pub fn try_read(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    match rustix::io::read(fd, buf) {
        Ok(0) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        Ok(n) => Ok(n),
        Err(rustix::io::Errno::AGAIN) => Ok(0),
        Err(rustix::io::Errno::INTR) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn pipe_pair() -> (std::fs::File, std::fs::File) {
        let (r, w) = rustix::pipe::pipe().unwrap();
        let r = std::fs::File::from(r);
        let w = std::fs::File::from(w);
        (r, w)
    }

    #[test]
    fn roundtrip_hello() {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            intent: Intent::Attach,
            flags: ClientFlags::READONLY,
            rows: 24,
            cols: 80,
        };
        let pkt = Packet::hello(&hello);
        let (r, w) = pipe_pair();
        send_packet(w.as_fd(), &pkt).unwrap();
        drop(w);
        let got = recv_packet(r.as_fd()).unwrap();
        let parsed = got.parse_hello().unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(parsed.intent, Intent::Attach);
        assert_eq!(parsed.flags, ClientFlags::READONLY);
        assert_eq!(parsed.rows, 24);
        assert_eq!(parsed.cols, 80);
    }

    #[test]
    fn roundtrip_welcome() {
        let w = Welcome {
            version: PROTOCOL_VERSION,
            server_pid: 1234,
            child_pid: 1235,
            child_running: true,
            exit_status: 0,
            client_count: 2,
            ring_size: 1048576,
            ring_used: 4200,
        };
        let pkt = Packet::welcome(&w);
        let (r, wr) = pipe_pair();
        send_packet(wr.as_fd(), &pkt).unwrap();
        drop(wr);
        let got = recv_packet(r.as_fd()).unwrap();
        let parsed = got.parse_welcome().unwrap();
        assert_eq!(parsed.server_pid, 1234);
        assert_eq!(parsed.child_pid, 1235);
        assert!(parsed.child_running);
        assert_eq!(parsed.ring_size, 1048576);
        assert_eq!(parsed.ring_used, 4200);
    }

    #[test]
    fn roundtrip_content() {
        let data = b"hello world";
        let pkt = Packet::content(data);
        let (r, w) = pipe_pair();
        send_packet(w.as_fd(), &pkt).unwrap();
        drop(w);
        let got = recv_packet(r.as_fd()).unwrap();
        assert_eq!(got.msg_type, MsgType::Content);
        assert_eq!(got.payload, data);
    }

    #[test]
    fn roundtrip_empty_messages() {
        for msg_type in [MsgType::ResizeReq, MsgType::Detach, MsgType::ReplayEnd] {
            let pkt = Packet::empty(msg_type);
            let (r, w) = pipe_pair();
            send_packet(w.as_fd(), &pkt).unwrap();
            drop(w);
            let got = recv_packet(r.as_fd()).unwrap();
            assert_eq!(got.msg_type, msg_type);
            assert!(got.payload.is_empty());
        }
    }

    #[test]
    fn roundtrip_resize() {
        let pkt = Packet::resize(50, 120);
        let (r, w) = pipe_pair();
        send_packet(w.as_fd(), &pkt).unwrap();
        drop(w);
        let got = recv_packet(r.as_fd()).unwrap();
        let (rows, cols) = got.parse_resize().unwrap();
        assert_eq!(rows, 50);
        assert_eq!(cols, 120);
    }

    #[test]
    fn roundtrip_exit() {
        let pkt = Packet::exit(42);
        let (r, w) = pipe_pair();
        send_packet(w.as_fd(), &pkt).unwrap();
        drop(w);
        let got = recv_packet(r.as_fd()).unwrap();
        assert_eq!(got.parse_exit_status().unwrap(), 42);
    }

    #[test]
    fn max_payload() {
        let data = vec![0xAB; MAX_PAYLOAD];
        let pkt = Packet::content(&data);
        let (r, w) = pipe_pair();
        send_packet(w.as_fd(), &pkt).unwrap();
        drop(w);
        let got = recv_packet(r.as_fd()).unwrap();
        assert_eq!(got.payload.len(), MAX_PAYLOAD);
    }
}
