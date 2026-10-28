//! Shared v1 framing, including bounded regressions for partial sync reads.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use mneme::protocol::{
    ClientFlags, HEADER_SIZE, Hello, Intent, MAX_PAYLOAD, MsgType, PROTOCOL_VERSION, Packet,
    Welcome, recv_packet, recv_packet_async, send_packet, send_packet_async,
};
use tokio::io::AsyncWriteExt;

#[test]
fn v1_layouts_are_unchanged() {
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(HEADER_SIZE, 3);
    assert_eq!(MAX_PAYLOAD, 4093);
    let hello = Hello {
        version: 1,
        intent: Intent::Attach,
        flags: ClientFlags::READONLY | ClientFlags::LOW_PRIORITY,
        rows: 0x1234,
        cols: 0x5678,
    };
    assert_eq!(
        Packet::hello(&hello).encode(),
        [0, 8, 0, 1, 1, 3, 0, 0x34, 0x12, 0x78, 0x56]
    );
    let welcome = Welcome {
        version: 1,
        server_pid: 0x04030201,
        child_pid: 0x08070605,
        child_running: true,
        exit_status: 42,
        client_count: 0x0a09,
        ring_size: 0x0e0d0c0b,
        ring_used: 0x1211100f,
    };
    assert_eq!(
        Packet::welcome(&welcome).encode(),
        [
            1, 21, 0, 1, 1, 2, 3, 4, 5, 6, 7, 8, 1, 42, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18
        ]
    );
}

#[tokio::test]
async fn fragmented_frames_and_back_to_back_packets() {
    // One byte of capacity forces partial header, payload, and write progress.
    let (mut writer, mut reader) = tokio::io::duplex(1);
    let packets = [
        Packet::empty(MsgType::ReplayEnd),
        Packet::content(b"fragmented"),
        Packet::replay(&vec![0xab; MAX_PAYLOAD]),
        Packet::exit(42),
    ];
    let send = async {
        for pkt in &packets {
            send_packet_async(&mut writer, pkt).await.unwrap();
        }
        writer.shutdown().await.unwrap();
    };
    let receive = async {
        for expected in &packets {
            let got = recv_packet_async(&mut reader).await.unwrap();
            assert_eq!(got.msg_type, expected.msg_type);
            assert_eq!(got.payload, expected.payload);
        }
        assert_eq!(
            recv_packet_async(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(send, receive);
    })
    .await
    .expect("fragmented frame transfer stalled");
}

#[tokio::test]
async fn sync_and_async_codecs_use_identical_frames_for_every_type() {
    for value in 0..=9 {
        let pkt = Packet::new(MsgType::from_u8(value).unwrap(), vec![1, 2, 3, 4]);
        let (reader, peer) = UnixStream::pair().unwrap();
        send_packet(peer.as_fd(), &pkt).unwrap();
        let mut wire = vec![0; HEADER_SIZE + pkt.payload.len()];
        (&reader).read_exact(&mut wire).unwrap();
        let got = recv_packet_async(&mut wire.as_slice()).await.unwrap();
        assert_eq!(got.msg_type, pkt.msg_type);
        assert_eq!(got.payload, pkt.payload);

        // A full UnixStream and its owned halves use the same generic helpers.
        reader.set_nonblocking(true).unwrap();
        let mut stream = tokio::net::UnixStream::from_std(reader).unwrap();
        send_packet_async(&mut stream, &pkt).await.unwrap();
        let got = recv_packet(peer.as_fd()).unwrap();
        assert_eq!(got.msg_type, pkt.msg_type);
        assert_eq!(got.payload, pkt.payload);
        let (mut read_half, mut write_half) = stream.into_split();
        send_packet_async(&mut write_half, &pkt).await.unwrap();
        let got = recv_packet(peer.as_fd()).unwrap();
        assert_eq!(got.msg_type, pkt.msg_type);
        assert_eq!(got.payload, pkt.payload);
        send_packet(peer.as_fd(), &pkt).unwrap();
        let got = recv_packet_async(&mut read_half).await.unwrap();
        assert_eq!(got.msg_type, pkt.msg_type);
        assert_eq!(got.payload, pkt.payload);
    }
}

#[tokio::test]
async fn malformed_headers_are_rejected_without_waiting_for_payload() {
    for header in [[255, 0, 0], [3, 0xfe, 0x0f], [3, 0xff, 0xff]] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&header).unwrap();
        assert_eq!(
            recv_packet(reader.as_fd()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let (mut reader, mut writer) = tokio::io::duplex(3);
        writer.write_all(&header).await.unwrap();
        // Keep the writer open: decoding must reject the header immediately.
        let error = tokio::time::timeout(Duration::from_secs(1), recv_packet_async(&mut reader))
            .await
            .expect("invalid header waited for payload")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

#[tokio::test]
async fn truncated_headers_and_payloads_report_eof() {
    for bytes in [
        vec![],
        vec![3],
        vec![3, 2],
        vec![3, 2, 0],
        vec![3, 2, 0, 42],
    ] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&bytes).unwrap();
        drop(writer);
        assert_eq!(
            recv_packet(reader.as_fd()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            recv_packet_async(&mut bytes.as_slice())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[tokio::test]
async fn mutated_oversized_packets_are_rejected_before_writing() {
    let mut pkt = Packet::empty(MsgType::Content);
    pkt.payload.resize(MAX_PAYLOAD + 1, 0);
    let (mut reader, writer) = UnixStream::pair().unwrap();
    assert_eq!(
        send_packet(writer.as_fd(), &pkt).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    reader.set_nonblocking(true).unwrap();
    assert_eq!(
        reader.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    let (mut writer, mut reader) = tokio::io::duplex(1);
    assert_eq!(
        send_packet_async(&mut writer, &pkt)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    drop(writer);
    assert_eq!(
        tokio::io::AsyncReadExt::read(&mut reader, &mut [0])
            .await
            .unwrap(),
        0
    );
}

#[test]
#[should_panic(expected = "payload too large")]
fn encode_rejects_mutated_oversized_payload() {
    let pkt = Packet {
        msg_type: MsgType::Content,
        payload: vec![0; MAX_PAYLOAD + 1],
    };
    pkt.encode();
}

fn assert_partial_read_returns_would_block(prefix: &[u8], nonblocking: bool) {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(nonblocking).unwrap();
    reader
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    writer.write_all(prefix).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(recv_packet(reader.as_fd()).unwrap_err().kind())
            .unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(1));
    // Always release and join the reader, even with the old infinite EAGAIN
    // loop. Closing the writer makes that loop return EOF, then this fails.
    drop(writer);
    worker.join().unwrap();
    assert_eq!(
        result.expect("partial frame ignored socket timeout or spun on EAGAIN"),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn sync_partial_header_honors_receive_timeout() {
    assert_partial_read_returns_would_block(&[3], false);
}

#[test]
fn sync_partial_payload_honors_receive_timeout() {
    assert_partial_read_returns_would_block(&[3, 2, 0, 42], false);
}

#[test]
fn sync_nonblocking_partial_frames_do_not_spin() {
    for prefix in [&[3][..], &[3, 2, 0, 42][..]] {
        assert_partial_read_returns_would_block(prefix, true);
    }
}
