//! A host listening on a real TCP port with Noise inside WebSocket: a
//! device pairs with a published code, reconnects with the pinned key,
//! and is refused once revoked.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeHost, client_config, hello, start_host};
use futures_util::{SinkExt, StreamExt};
use maple_agent::host::HostBackend;
use maple_remote::client::RemoteHostBackend;
use maple_remote::dial::{ConnectTarget, connect_direct};
use maple_remote::keys::StaticKey;
use maple_remote::pairing::PairingCode;
use maple_remote::wire::ClientHello;
use tokio_tungstenite::tungstenite::Message;

fn device_hello(device: &StaticKey) -> ClientHello {
    let mut hello = hello();
    hello.device.public_key = device.public_id();
    hello.device.name = "bens-laptop".to_string();
    hello.device.user_id = Some("user-1".to_string());
    hello
}

#[tokio::test]
async fn a_device_pairs_reconnects_and_is_refused_once_revoked() {
    let fake = FakeHost::new(3);
    let host = start_host(Arc::clone(&fake)).await;
    let device = StaticKey::generate().unwrap();

    // No code published: pairing is refused and counts as a failure.
    let refused = connect_direct(
        &host.address,
        &device,
        ConnectTarget::Pair(PairingCode::generate()),
    )
    .await;
    assert!(refused.is_err());

    // A wrong code against a published one is refused too.
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    let wrong = connect_direct(
        &host.address,
        &device,
        ConnectTarget::Pair(PairingCode::generate()),
    )
    .await;
    assert!(wrong.is_err());
    assert!(
        host.pending.current().is_some(),
        "a failed attempt leaves the code pending"
    );

    // The right code pairs, pins the host key, and is consumed.
    let dialed = connect_direct(&host.address, &device, ConnectTarget::Pair(code))
        .await
        .expect("pairing");
    assert_eq!(dialed.host_key, host.key.public_id());
    assert!(host.pending.current().is_none(), "the code is spent");
    assert!(host.devices.is_paired(&device.public_id()));
    let client = RemoteHostBackend::connect(dialed.carrier, device_hello(&device), client_config())
        .await
        .expect("hello after pairing");
    assert_eq!(client.host_hello().host.id, host.key.public_id());
    let sessions = client.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 2);
    // The hello named the device.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let listed = host.devices.list().unwrap();
    assert_eq!(listed[0].name, "bens-laptop");
    assert_eq!(listed[0].user_id.as_deref(), Some("user-1"));
    client.close().await;

    // Reconnect with the pinned key; a wrong pin is refused.
    let mut wrong_pin = StaticKey::generate().unwrap().public_id();
    wrong_pin.truncate(43);
    assert!(
        connect_direct(
            &host.address,
            &device,
            ConnectTarget::Host {
                host_key: wrong_pin
            }
        )
        .await
        .is_err()
    );
    let dialed = connect_direct(
        &host.address,
        &device,
        ConnectTarget::Host {
            host_key: host.key.public_id(),
        },
    )
    .await
    .expect("reconnect with the pinned key");
    let client = RemoteHostBackend::connect(dialed.carrier, device_hello(&device), client_config())
        .await
        .unwrap();
    let detail = client.load_session("s1".to_string()).await.unwrap();
    assert_eq!(detail.timeline.len(), 3);

    // A hello that claims another device's key is refused.
    let stranger = StaticKey::generate().unwrap();
    let dialed = connect_direct(
        &host.address,
        &device,
        ConnectTarget::Host {
            host_key: host.key.public_id(),
        },
    )
    .await
    .unwrap();
    let error =
        RemoteHostBackend::connect(dialed.carrier, device_hello(&stranger), client_config())
            .await
            .err()
            .expect("mismatched device identity is refused");
    assert!(error.contains("different device"), "{error}");

    // An unpaired device cannot open a session handshake at all.
    assert!(
        connect_direct(
            &host.address,
            &stranger,
            ConnectTarget::Host {
                host_key: host.key.public_id(),
            },
        )
        .await
        .is_err()
    );

    // Revocation refuses the next connection.
    host.devices.revoke(&device.public_id()).unwrap();
    assert!(
        connect_direct(
            &host.address,
            &device,
            ConnectTarget::Host {
                host_key: host.key.public_id(),
            },
        )
        .await
        .is_err()
    );
    client.close().await;
    host.shutdown.cancel();
}

#[tokio::test]
async fn repeated_wrong_codes_lock_the_address_out() {
    let fake = FakeHost::new(0);
    let host = start_host(fake).await;
    let device = StaticKey::generate().unwrap();
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    for _ in 0..3 {
        let _ = connect_direct(
            &host.address,
            &device,
            ConnectTarget::Pair(PairingCode::generate()),
        )
        .await;
    }
    // The right code no longer works from this address within the window.
    assert!(
        connect_direct(&host.address, &device, ConnectTarget::Pair(code))
            .await
            .is_err()
    );
    host.shutdown.cancel();
}

#[tokio::test]
async fn one_code_pairs_exactly_one_of_two_racing_devices() {
    let host = start_host(FakeHost::new(0)).await;
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    let first = StaticKey::generate().unwrap();
    let second = StaticKey::generate().unwrap();
    let (a, b) = tokio::join!(
        connect_direct(&host.address, &first, ConnectTarget::Pair(code.clone())),
        connect_direct(&host.address, &second, ConnectTarget::Pair(code.clone())),
    );
    assert_eq!(
        a.is_ok() as u8 + b.is_ok() as u8,
        1,
        "exactly one pairing succeeds: {:?} / {:?}",
        a.as_ref().err(),
        b.as_ref().err()
    );
    assert!(host.pending.current().is_none(), "the code is spent");
    let paired = host.devices.list().unwrap();
    assert_eq!(paired.len(), 1);
    let winner = if a.is_ok() { &first } else { &second };
    assert_eq!(paired[0].public_key, winner.public_id());
    host.shutdown.cancel();
}

#[tokio::test]
async fn a_revoked_device_reconnecting_does_not_lock_out_pairing_again() {
    let host = start_host(FakeHost::new(0)).await;
    let device = StaticKey::generate().unwrap();
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    connect_direct(&host.address, &device, ConnectTarget::Pair(code))
        .await
        .expect("pairing");
    host.devices.revoke(&device.public_id()).unwrap();
    // More session refusals than the limiter allows pairing failures.
    for _ in 0..5 {
        assert!(
            connect_direct(
                &host.address,
                &device,
                ConnectTarget::Host {
                    host_key: host.key.public_id(),
                },
            )
            .await
            .is_err()
        );
    }
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    connect_direct(&host.address, &device, ConnectTarget::Pair(code))
        .await
        .expect("session refusals did not count against pairing");
    assert!(host.devices.is_paired(&device.public_id()));
    host.shutdown.cancel();
}

#[tokio::test]
async fn large_frames_cross_the_noise_carrier_in_pieces() {
    // Attachments are larger than one Noise message; the carrier must cut
    // and reassemble them.
    let fake = FakeHost::new(0);
    let host = start_host(Arc::clone(&fake)).await;
    let device = StaticKey::generate().unwrap();
    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    let dialed = connect_direct(&host.address, &device, ConnectTarget::Pair(code))
        .await
        .unwrap();
    let client = RemoteHostBackend::connect(dialed.carrier, device_hello(&device), client_config())
        .await
        .unwrap();
    let bytes = client
        .read_image_attachment("s1".to_string(), "a1".to_string())
        .await
        .unwrap();
    assert_eq!(bytes, fake.attachment);
    let boot = client.bootstrap().await.unwrap();
    assert_eq!(boot.sessions.len(), 2);
    client.close().await;
    host.shutdown.cancel();
}

#[tokio::test]
async fn an_oversized_websocket_message_is_refused_by_both_roles() {
    let oversized = || Message::Binary(vec![1u8; 70_000].into());

    // Host role: a raw peer without the cap sends more than one Noise
    // message can hold. The host ends the connection without answering.
    let host = start_host(FakeHost::new(0)).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/", host.address))
        .await
        .unwrap();
    socket.send(oversized()).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the host drops the connection");
    assert!(
        !matches!(next, Some(Ok(Message::Binary(_)))),
        "the host never answers an oversized message: {next:?}"
    );
    host.shutdown.cancel();

    // Client role: a raw host answers the client's first handshake message
    // with an oversized one. The dial fails instead of buffering it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = socket.next().await;
        let _ = socket.send(oversized()).await;
        while let Some(Ok(_)) = socket.next().await {}
    });
    let device = StaticKey::generate().unwrap();
    let error = connect_direct(
        &address,
        &device,
        ConnectTarget::Pair(PairingCode::generate()),
    )
    .await
    .err()
    .expect("an oversized message fails the dial");
    assert!(
        error.to_lowercase().contains("too long") || error.contains("limit"),
        "{error}"
    );
}
