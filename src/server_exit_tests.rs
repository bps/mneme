//! Deliberately retain a slave independently of the direct child so that EOF
//! cannot occur. This avoids platform-dependent terminal revocation on exit.
use super::*;

#[test]
fn backpressure_pauses_but_does_not_reset_pty_drain_budget() {
    let now = tokio::time::Instant::now();
    let mut budget = Duration::from_secs(2);
    let mut clock = None;
    assert_eq!(
        drain_deadline(&mut budget, &mut clock, now, true),
        Some(now + budget)
    );
    let paused = now + Duration::from_secs(1);
    assert_eq!(drain_deadline(&mut budget, &mut clock, paused, false), None);
    assert_eq!(budget, Duration::from_secs(1));
    let resumed = paused + Duration::from_secs(100);
    assert_eq!(
        drain_deadline(&mut budget, &mut clock, resumed, true),
        Some(resumed + Duration::from_secs(1))
    );
    let expired = resumed + Duration::from_secs(1);
    // Expiration still wins if the last read just filled a client queue.
    assert_eq!(
        drain_deadline(&mut budget, &mut clock, expired, false),
        Some(expired)
    );
    assert!(budget.is_zero());
}

#[tokio::test]
async fn retained_slave_produces_explicit_error_not_successful_exit() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let (master, slave) = open_pty().unwrap();
        set_nonblocking(&master).unwrap();
        // SAFETY: AsyncFd owns master, keeping its valid PTY descriptor open
        // and unchanged until the registration is dropped.
        let leader = std::sync::Arc::new(unsafe { AsyncFd::register(master) }.unwrap());
        // This child does not own the slave or make it a controlling terminal.
        // Our retained slave therefore survives its exit on Linux and Darwin.
        let mut signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).unwrap();
        let mut child = tokio::process::Command::new("/usr/bin/true")
            .spawn()
            .unwrap();
        let (_ready, notify) = rustix::pipe::pipe().unwrap();
        let mut reporter = crate::startup::Reporter::new(notify).unwrap();

        // Preload accepted output. Error must follow it even without PTY EOF.
        rustix::io::write(&slave, b"accepted-tail").unwrap();
        let server = mainloop(
            listener,
            leader,
            &mut child,
            &mut signal,
            4096,
            &mut reporter,
        );
        let peer = async {
            let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
            send_packet(
                &mut stream,
                &Packet::hello(&Hello {
                    version: PROTOCOL_VERSION,
                    intent: Intent::Attach,
                    flags: ClientFlags::READONLY,
                    rows: 24,
                    cols: 80,
                }),
            )
            .await
            .unwrap();
            let mut output = Vec::new();
            let mut replay_end = false;
            loop {
                let packet = recv_packet(&mut stream).await.unwrap();
                match packet.msg_type {
                    MsgType::Welcome => {}
                    MsgType::Replay => {
                        assert!(!replay_end);
                        output.extend(packet.payload);
                    }
                    MsgType::ReplayEnd => {
                        assert!(!replay_end);
                        replay_end = true;
                    }
                    MsgType::Content => {
                        assert!(replay_end);
                        output.extend(packet.payload);
                    }
                    MsgType::Error => {
                        assert!(replay_end);
                        assert_eq!(output, b"accepted-tail");
                        assert!(packet.parse_error().unwrap().contains("PTY EOF"));
                        assert_eq!(
                            recv_packet(&mut stream).await.unwrap_err().kind(),
                            io::ErrorKind::UnexpectedEof
                        );
                        break;
                    }
                    other => panic!("unexpected packet: {other:?}"),
                }
            }
        };
        let (result, ()) = tokio::join!(server, peer);
        assert!(result.unwrap_err().to_string().contains("PTY EOF"));
        assert!(child.id().is_none(), "direct child must be reaped");
        drop(slave);
    })
    .await
    .expect("retained-slave drain was not bounded");
}
