//! Minimal loopback probe for the exterior bridge's exact shape (#385):
//! handshake on a remote-opened bi stream, then framed traffic on the same
//! stream, host half running as detached pump-shaped tasks.

#![allow(missing_docs)]

use iroh::RelayMode;
use std::sync::Arc;
use std::time::Duration;

fn key(tag: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = tag;
    bytes[31] = 0xEE;
    iroh::SecretKey::from_bytes(&bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_then_frames_on_one_stream() {
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

    // Loopback form of the host's bound socket: same port, explicit address.
    let bound = host_ep.bound_sockets()[0];
    let addr = iroh::EndpointAddr::from_parts(
        host_ep.id(),
        [iroh::TransportAddr::Ip(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            bound.port(),
        ))],
    );

    eprintln!("B1: endpoints bound");
    let remote_for_dial = remote_ep.clone();
    let dialer = tokio::spawn(async move { remote_for_dial.connect(addr, b"x/1").await.unwrap() });
    let incoming = host_ep.accept().await.unwrap();
    let host_conn = incoming.accept().unwrap().await.unwrap();
    let remote_conn = dialer.await.unwrap();

    // Host half, detached and pump-shaped: accept the one stream, read the
    // join, reply, then count 7-byte frames forever.
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        let conn = host_conn.clone();
        tokio::spawn(async move {
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(halves) => halves,
                Err(_) => return,
            };
            let mut join = [0u8; 5];
            if recv.read_exact(&mut join).await.is_err() {
                return;
            }
            if send.write_all(b"accept").await.is_err() {
                return;
            }
            loop {
                let mut header = [0u8; 7];
                if recv.read_exact(&mut header).await.is_err() {
                    break;
                }
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // Remote half: open the stream, handshake, hammer frames down it.
    let (mut rsend, mut rrecv) = remote_conn.open_bi().await.unwrap();
    {
        rsend.write_all(b"join!").await.unwrap();
        let mut ack = [0u8; 6];
        rrecv.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"accept");
    }
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
    println!("frames received: {got} of 20");
    assert_eq!(got, 20, "post-handshake frames lost");
}
