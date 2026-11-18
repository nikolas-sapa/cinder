mod common;

use cinder_core::proto::{ClientMsg, ErrorCode, HandshakeMsg, HsId, PeerId, ServerMsg, SessionId};
use cinder_relay::server::RelayConfig;
use common::*;
use futures::{SinkExt, StreamExt};
use std::time::Duration;

// nc smoke — Hello frame on the wire (peer_id = 32 zero bytes):
//   payload = variant-index u32 LE (Hello = 0) ‖ version u16 LE ‖ peer_id[32]
//         = 00 00 00 00 | 01 00 | 00…00  → 38 B
//   frame   = len u32 BE ‖ payload → 00 00 00 26 ‖ payload
//   (printf '\x00\x00\x00\x26\x00\x00\x00\x00\x01\x00'; head -c 32 /dev/zero) | nc 127.0.0.1 7000 | xxd
// Expect back a ServerHello frame (14 B payload): 00 00 00 0e ‖ 00 00 00 00 ‖ 01 00 ‖ now_unix u64 LE.

#[tokio::test]
async fn r1_echo_routing() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let peer_a = PeerId::random();
    let peer_b = PeerId::random();
    let mut a = TestClient::hello(addr, peer_a).await;
    let mut b = TestClient::hello(addr, peer_b).await;

    let hs_id = HsId::random();
    let msg1 = vec![0xEE; 32];
    a.send(ClientMsg::Handshake {
        to: peer_b,
        hs_id,
        body: HandshakeMsg::Msg1(msg1.clone()),
    })
    .await;
    match b.recv().await {
        ServerMsg::Handshake {
            from,
            hs_id: got,
            body: HandshakeMsg::Msg1(body),
        } => {
            assert_eq!(from, peer_a);
            assert_eq!(got, hs_id);
            assert_eq!(body, msg1);
        }
        other => panic!("expected Handshake, got {other:?}"),
    }

    let session_id = SessionId::random();
    let ciphertext = vec![0xAB; 1000];
    a.send(ClientMsg::Transport {
        to: peer_b,
        session_id,
        body: ciphertext.clone(),
    })
    .await;
    let got = tokio::time::timeout(Duration::from_millis(100), b.rd.next())
        .await
        .expect("transport not delivered in 100ms")
        .expect("stream ended")
        .expect("decode error");
    match got {
        ServerMsg::Transport {
            from,
            session_id: got_sid,
            body,
        } => {
            assert_eq!(from, peer_a);
            assert_eq!(got_sid, session_id);
            assert_eq!(body, ciphertext);
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn peer_id_in_use() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let peer_b = PeerId::random();
    let mut b = TestClient::hello(addr, peer_b).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    let (mut rd, mut wr) = connect(addr).await;
    wr.send(ClientMsg::Hello {
        version: 1,
        peer_id: peer_b,
    })
    .await
    .unwrap();
    match rd.next().await {
        Some(Ok(ServerMsg::Error {
            code: ErrorCode::PeerIdInUse,
            ..
        })) => {}
        other => panic!("expected PeerIdInUse, got {other:?}"),
    }
    match tokio::time::timeout(Duration::from_secs(2), rd.next()).await {
        Ok(None) | Ok(Some(Err(_))) => {}
        other => panic!("expected closed connection, got {other:?}"),
    }

    // The squatted peer's real session is untouched.
    let session_id = SessionId::random();
    b.send(ClientMsg::Transport {
        to: a.peer_id,
        session_id,
        body: vec![1],
    })
    .await;
    match a.recv().await {
        ServerMsg::Transport { from, .. } => assert_eq!(from, peer_b),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_first_frame_closed() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let (mut rd, mut wr) = connect(addr).await;
    wr.send(ClientMsg::Ping { nonce: 1 }).await.unwrap();
    match rd.next().await {
        Some(Ok(ServerMsg::Error {
            code: ErrorCode::ProtocolViolation,
            ..
        })) => {}
        other => panic!("expected ProtocolViolation, got {other:?}"),
    }
}

#[tokio::test]
async fn version_mismatch_closed() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let (mut rd, mut wr) = connect(addr).await;
    wr.send(ClientMsg::Hello {
        version: 0xBEEF,
        peer_id: PeerId::random(),
    })
    .await
    .unwrap();
    match rd.next().await {
        Some(Ok(ServerMsg::Error {
            code: ErrorCode::VersionMismatch,
            ..
        })) => {}
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn ping_pong_and_offline_drop() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    a.send(ClientMsg::Ping { nonce: 0xDEAD }).await;
    match a.recv().await {
        ServerMsg::Pong { nonce } => assert_eq!(nonce, 0xDEAD),
        other => panic!("expected Pong, got {other:?}"),
    }

    // Transport to an offline peer: dropped (no error code exists for it; the
    // E2E layer owns retransmission). Connection must stay usable.
    a.send(ClientMsg::Transport {
        to: PeerId::random(),
        session_id: SessionId::random(),
        body: vec![7; 8],
    })
    .await;
    a.send(ClientMsg::Ping { nonce: 0xBEEF }).await;
    match a.recv().await {
        ServerMsg::Pong { nonce } => assert_eq!(nonce, 0xBEEF),
        other => panic!("expected Pong, got {other:?}"),
    }
}

#[tokio::test]
async fn mailbox_and_invite_variants_error_but_stay_open() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    // InviteCreate registers a real slot now.
    let token = cinder_core::proto::InviteToken::random();
    a.send(ClientMsg::InviteCreate {
        token,
        ttl_secs: 60,
    })
    .await;
    match a.recv().await {
        ServerMsg::InviteCreated { token: got, .. } => assert_eq!(got, token),
        other => panic!("expected InviteCreated, got {other:?}"),
    }

    a.send(ClientMsg::InviteAccept {
        token: cinder_core::proto::InviteToken::random(),
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::InviteInvalid);

    a.send(ClientMsg::MailboxFetch {
        mailbox_id: cinder_core::proto::MailboxId::random(),
        token: cinder_core::proto::Token::random(),
        max_msgs: 10,
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::UnknownMailbox);

    // Still open.
    a.send(ClientMsg::Ping { nonce: 9 }).await;
    match a.recv().await {
        ServerMsg::Pong { nonce } => assert_eq!(nonce, 9),
        other => panic!("expected Pong, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_payload_semantic_cap() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    // Frame fits under 16 KiB but the handshake payload exceeds 4 KiB → §2.6.
    a.send(ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id: HsId::random(),
        body: HandshakeMsg::Msg1(vec![0u8; 5000]),
    })
    .await;
    let msgs = a.drain(Duration::from_secs(2)).await;
    assert!(msgs.iter().any(|m| matches!(
        m,
        ServerMsg::Error {
            code: ErrorCode::ProtocolViolation,
            ..
        }
    )));
}
