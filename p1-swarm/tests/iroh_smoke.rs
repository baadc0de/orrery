//! Minimal loopback probe: which primitives cross between two endpoints
//! under this build (#385). Continuous senders, so a slow receiver start
//! cannot masquerade as a dead lane.

#![allow(missing_docs)]

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use iroh::RelayMode;

fn key(tag: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = tag;
    bytes[31] = 0xEE;
    iroh::SecretKey::from_bytes(&bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn datagrams_and_streams_cross_loopback_continuously() {
    let host_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(1))
        .bind()
        .await
        .unwrap();
    let remote_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(2))
        .bind()
        .await
        .unwrap();

    // Readers first, on both sides, before anything is sent.
    let socket = host_ep.bound_sockets()[0];
    let addr =
        iroh::EndpointAddr::from_parts(host_ep.id(), [iroh::TransportAddr::Ip(socket)]);

    let remote_for_dial = remote_ep.clone();
    let dialer = tokio::spawn(async move {
        remote_for_dial.connect(addr, b"x/1").await.unwrap()
    });
    let incoming = host_ep.accept().await.unwrap();
    let host_conn = incoming.accept().unwrap().await.unwrap();
    let remote_conn = dialer.await.unwrap();

    // Host reader task: count everything that shows up.
    let host_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let seen = host_seen.clone();
        let conn = host_conn.clone();
        tokio::spawn(async move {
            loop {
                match conn.read_datagram().await {
                    Ok(packet) => {
                        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        assert!(&packet[..] == b"up" || &packet[..] == b"stream-up");
                    }
                    Err(_) => break,
                }
            }
        });
    }
    // Host ordered-stream reader too.
    {
        let conn = host_conn.clone();
        tokio::spawn(async move {
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let _ = send;
                if recv.read_to_end(64 * 1_024).await.is_ok() {}
            }
        });
    }

    // Remote hammers both primitives. A clone of the connection stays held
    // for the rest of the test: dropping every handle closes the connection,
    // which is exactly how a run can look "joined" and still be dead.
    let keep_alive = remote_conn.clone();
    let hammer = {
        let remote_conn = remote_conn.clone();
        tokio::spawn(async move {
            let (mut shared_send, _shared_recv_unused) =
                remote_conn.open_bi().await.unwrap();
            for index in 0..200u32 {
                let _ = remote_conn.send_datagram(Bytes::from_static(b"up"));
                let frame = [b"s"[0], (index % 256) as u8];
                let _ = shared_send.write_all(&frame).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    tokio::time::sleep(Duration::from_secs(3)).await;
    let seen = host_seen.load(std::sync::atomic::Ordering::Relaxed);
    println!("continuous datagrams received at host: {seen}");

    // ── The swarm's own pattern: a bi stream opened by the remote carries
    // framed traffic back to the host indefinitely. ──
    let writer = {
        let remote_conn = remote_conn.clone();
        tokio::spawn(async move {
            let (mut send, _recv_unused) = remote_conn.open_bi().await.unwrap();
            for _ in 0..20u32 {
                send.write_all(b"frame12").await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let mut seen_frames = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    // The host's ordered reader accepts the stream and counts frames.
    loop {
        if seen_frames >= 20 || tokio::time::Instant::now() > deadline {
            break;
        }
        let accept = tokio::time::timeout(Duration::from_secs(2), host_conn.accept_bi()).await;
        let Ok(Ok((mut _send, mut recv))) = accept else { continue };
        use tokio::io::AsyncReadExt;
        let mut header = [0u8; 7];
        while recv.read_exact(&mut header).await.is_ok() {
            seen_frames += 1;
        }
    }
    writer.abort();
    println!("swarm-pattern stream frames received: {seen_frames}");
    drop(keep_alive);
    assert!(seen_frames > 0, "the reused handshake stream went deaf");

    // And the reverse direction once, host -> remote stream write.
    let (mut hs, _hr) = host_conn.open_bi().await.unwrap();
    use tokio::io::AsyncWriteExt;
    hs.write_all(b"down").await.unwrap();
    let _ = hs.finish();

    assert!(seen > 0, "not a single datagram crossed remote->host");
}

/// The swarm's exact orientation: the REMOTE opens the bi stream, writes the
/// handshake and then framed traffic; the host accepts and reads.
#[tokio::test(flavor = "multi_thread")]
async fn remote_opened_stream_reaches_the_host() {
    let host_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(1))
        .bind()
        .await
        .unwrap();
    let remote_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(3))
        .bind()
        .await
        .unwrap();

    // Host reader armed before the dial: counts datagrams and stream frames.
    let seen_dg = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_fr = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let socket = host_ep.bound_sockets()[0];
    let addr =
        iroh::EndpointAddr::from_parts(host_ep.id(), [iroh::TransportAddr::Ip(socket)]);
    let remote_for_dial = remote_ep.clone();
    let dialer = tokio::spawn(async move {
        remote_for_dial.connect(addr, b"x/1").await.unwrap()
    });
    let incoming = host_ep.accept().await.unwrap();
    let host_conn = incoming.accept().unwrap().await.unwrap();
    let _remote_conn = dialer.await.unwrap();

    {
        let seen_dg = Arc::clone(&seen_dg);
        let conn = host_conn.clone();
        tokio::spawn(async move {
            while conn.read_datagram().await.is_ok() {
                seen_dg.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    {
        let seen_fr = Arc::clone(&seen_fr);
        let conn = host_conn.clone();
        tokio::spawn(async move {
            while let Ok((mut _s, mut r)) = conn.accept_bi().await {
                use tokio::io::AsyncReadExt;
                let mut header = [0u8; 7];
                while r.read_exact(&mut header).await.is_ok() {
                    seen_fr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }

    let keep = _remote_conn;
    let writer = tokio::spawn(async move {
        let (mut send, _r) = keep.open_bi().await.unwrap();
        use tokio::io::AsyncWriteExt;
        for _ in 0..20u32 {
            send.write_all(b"frame12").await.unwrap();
            let _ = keep.send_datagram(Bytes::from_static(b"dg"));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    writer.await.unwrap();

    std::thread::sleep(Duration::from_secs(1));
    let dg = seen_dg.load(std::sync::atomic::Ordering::Relaxed);
    let fr = seen_fr.load(std::sync::atomic::Ordering::Relaxed);
    println!("host saw {dg} datagrams, {fr} stream frames");
    assert!(fr >= 20, "stream frames lost");
    assert!(dg >= 20, "datagrams lost");
}

/// Test C: handshake exchange (length-prefixed, both directions) on the
/// remote-opened stream BEFORE the frame traffic, exactly like the bridge
/// does. If frames die here but pass without the handshake, the handshake
/// interaction is the defect.
#[tokio::test(flavor = "multi_thread")]
async fn handshake_then_frames_on_one_stream() {
    let host_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(5))
        .bind()
        .await
        .unwrap();
    let remote_ep = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![b"x/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key(6))
        .bind()
        .await
        .unwrap();

    let socket = host_ep.bound_sockets()[0];
    eprintln!("C1: host bound {:?}", socket);
    let addr =
        iroh::EndpointAddr::from_parts(host_ep.id(), [iroh::TransportAddr::Ip(socket)]);
    let remote_for_dial = remote_ep.clone();
    let dialer = tokio::spawn(async move {
        eprintln!("C2: dialing");
        let c = remote_for_dial.connect(addr, b"x/1").await.unwrap();
        eprintln!("C3: connected");
        c
    });
    let incoming = host_ep.accept().await.unwrap();
    eprintln!("C4: incoming accepted (transport)");
    let host_conn = incoming.accept().unwrap().await.unwrap();
    eprintln!("C5: connection up");
    let remote_conn = dialer.await.unwrap();

    // Handshake on the remote-opened stream.
    eprintln!("C6: remote opening bi");
    let (mut rsend, mut rrecv) = remote_conn.open_bi().await.unwrap();
    eprintln!("C7: remote opened; host accepting bi");
    let (mut hsend, mut hrecv) = host_conn.accept_bi().await.unwrap();
    eprintln!("C8: host accepted bi");
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        eprintln!("C: remote writing join");
        rsend.write_all(b"join!").await.unwrap();
        eprintln!("C: remote wrote join; host reading");
        let mut join = [0u8; 5];
        hrecv.read_exact(&mut join).await.unwrap();
        eprintln!("C: host got join, replying");
        hsend.write_all(b"accept").await.unwrap();
        eprintln!("C: host replied; remote reading ack");
        let mut ack = [0u8; 6];
        rrecv.read_exact(&mut ack).await.unwrap();
        eprintln!("C: remote got ack");
        assert_eq!(&ack, b"accept");
    }

    // Host reader armed, counting 7-byte frames.
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let mut header = [0u8; 7];
                if hrecv.read_exact(&mut header).await.is_err() {
                    break;
                }
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // Remote writes 20 frames on the SAME send half.
    use tokio::io::AsyncWriteExt;
    for _ in 0..20u32 {
        rsend.write_all(b"frame12").await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while seen.load(std::sync::atomic::Ordering::Relaxed) < 20
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let got = seen.load(std::sync::atomic::Ordering::Relaxed);
    println!("handshake-then-frames: host saw {got} of 20");
    assert_eq!(got, 20, "post-handshake frames lost");
}
