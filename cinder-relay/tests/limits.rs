mod common;

use cinder_core::proto::{ClientMsg, ErrorCode, HandshakeMsg, HsId, PeerId, ServerMsg};
use cinder_relay::server::{RateLimit, RelayConfig};
use common::*;
use futures::{SinkExt, StreamExt};
use std::time::Duration;

fn msg1() -> HandshakeMsg {
    HandshakeMsg::Msg1(vec![0xEE; 32])
}

#[tokio::test]
async fn handshake_rate_limit_trips() {
    // Injected limit: burst 5, slow refill (PLAN R2 pattern — trips fast).
    let cfg = RelayConfig {
        handshake_rate: RateLimit {
            count: 5,
            per: Duration::from_secs(600),
            burst: 5,
        },
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for _ in 0..5 {
        a.send(ClientMsg::Handshake {
            to: PeerId::random(),
            hs_id: HsId::random(),
            body: msg1(),
        })
        .await;
    }
    a.send(ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id: HsId::random(),
        body: msg1(),
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);

    // RateLimited stays open per §10.
    a.send(ClientMsg::Ping { nonce: 1 }).await;
    assert!(matches!(a.recv().await, ServerMsg::Pong { nonce: 1 }));
}

#[tokio::test]
async fn invite_accept_counts_as_initiation() {
    let cfg = RelayConfig {
        handshake_rate: RateLimit {
            count: 1,
            per: Duration::from_secs(600),
            burst: 1,
        },
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    a.send(ClientMsg::InviteAccept {
        token: cinder_core::proto::InviteToken::random(),
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::InviteInvalid);

    // Second initiation in the window is rate-limited before the invite check.
    a.send(ClientMsg::InviteAccept {
        token: cinder_core::proto::InviteToken::random(),
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn pending_handshake_cap() {
    let cfg = RelayConfig {
        pending_hs_per_ip: 4,
        handshake_rate: RateLimit {
            count: 1000,
            per: Duration::from_secs(60),
            burst: 1000,
        },
        handshake_timeout: Duration::from_secs(30),
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for _ in 0..4 {
        a.send(ClientMsg::Handshake {
            to: PeerId::random(),
            hs_id: HsId::random(),
            body: msg1(),
        })
        .await;
    }
    a.send(ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id: HsId::random(),
        body: msg1(),
    })
    .await;
    let (code, _) = tokio::time::timeout(Duration::from_millis(500), a.recv_error())
        .await
        .expect("refusal not immediate");
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn handshake_timeout_aborts_attempt() {
    let cfg = RelayConfig {
        handshake_timeout: Duration::from_millis(50),
        sweep_interval: Duration::from_millis(10),
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;
    let mut b = TestClient::hello(addr, PeerId::random()).await;

    // B receives the forward but never answers Msg2 — attempt must expire.
    a.send(ClientMsg::Handshake {
        to: b.peer_id,
        hs_id: HsId::random(),
        body: msg1(),
    })
    .await;
    assert!(matches!(b.recv().await, ServerMsg::Handshake { .. }));

    let start = std::time::Instant::now();
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::HandshakeTimeout);
    assert!(start.elapsed() < Duration::from_secs(2));

    // HandshakeTimeout stays open.
    a.send(ClientMsg::Ping { nonce: 2 }).await;
    assert!(matches!(a.recv().await, ServerMsg::Pong { nonce: 2 }));
}

#[tokio::test]
async fn idle_timeout_closes_connection() {
    let cfg = RelayConfig {
        idle_timeout: Duration::from_millis(50),
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    let msgs = a.drain(Duration::from_secs(2)).await;
    assert!(msgs.is_empty(), "unexpected messages: {msgs:?}");
}

#[tokio::test]
async fn transport_frame_rate_limit() {
    let cfg = RelayConfig {
        transport_rate: RateLimit {
            count: 5,
            per: Duration::from_secs(600),
            burst: 5,
        },
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for nonce in 0..5 {
        a.send(ClientMsg::Ping { nonce }).await;
    }
    a.send(ClientMsg::Ping { nonce: 99 }).await;

    let mut rate_limited = false;
    for _ in 0..6 {
        match a.recv().await {
            ServerMsg::Pong { .. } => {}
            ServerMsg::Error {
                code: ErrorCode::RateLimited,
                ..
            } => {
                rate_limited = true;
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(rate_limited);
}

#[tokio::test]
async fn mailboxes_per_ip_cap() {
    let cfg = RelayConfig {
        mailboxes_per_ip: 2,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;
    let h = || cinder_core::proto::Token::random();
    let th = |t: &cinder_core::proto::Token| {
        use sha2::{Digest, Sha256};
        cinder_core::proto::TokenHash::from(Into::<[u8; 32]>::into(Sha256::digest(t.as_bytes())))
    };

    for _ in 0..2 {
        a.send(ClientMsg::MailboxOpen {
            mailbox_id: cinder_core::proto::MailboxId::random(),
            h_deposit: th(&h()),
            h_fetch: th(&h()),
            ttl_secs: 60,
        })
        .await;
        assert!(matches!(a.recv().await, ServerMsg::MailboxOpened { .. }));
    }
    a.send(ClientMsg::MailboxOpen {
        mailbox_id: cinder_core::proto::MailboxId::random(),
        h_deposit: th(&h()),
        h_fetch: th(&h()),
        ttl_secs: 60,
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);

    a.send(ClientMsg::Ping { nonce: 1 }).await;
    assert!(matches!(a.recv().await, ServerMsg::Pong { nonce: 1 }));
}

#[tokio::test]
async fn invite_slots_per_conn_cap() {
    let cfg = RelayConfig {
        invites_per_conn: 2,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for _ in 0..2 {
        a.send(ClientMsg::InviteCreate {
            token: cinder_core::proto::InviteToken::random(),
            ttl_secs: 3600,
        })
        .await;
        assert!(matches!(a.recv().await, ServerMsg::InviteCreated { .. }));
    }
    a.send(ClientMsg::InviteCreate {
        token: cinder_core::proto::InviteToken::random(),
        ttl_secs: 3600,
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn deposit_rate_limit_per_mailbox() {
    let cfg = RelayConfig {
        deposit_rate: RateLimit {
            count: 2,
            per: Duration::from_secs(600),
            burst: 2,
        },
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = cinder_core::proto::MailboxId::random();
    let dep_tok = cinder_core::proto::Token::random();
    let th = {
        use sha2::{Digest, Sha256};
        cinder_core::proto::TokenHash::from(Into::<[u8; 32]>::into(Sha256::digest(
            dep_tok.as_bytes(),
        )))
    };
    owner
        .send(ClientMsg::MailboxOpen {
            mailbox_id: mid,
            h_deposit: th,
            h_fetch: th,
            ttl_secs: 60,
        })
        .await;
    assert!(matches!(
        owner.recv().await,
        ServerMsg::MailboxOpened { .. }
    ));

    let env = || cinder_core::proto::DepositEnvelope {
        session_id: cinder_core::proto::SessionId::random(),
        body: vec![1],
    };
    for _ in 0..2 {
        peer.send(ClientMsg::MailboxDeposit {
            mailbox_id: mid,
            token: dep_tok,
            env: env(),
        })
        .await;
        assert!(matches!(
            peer.recv().await,
            ServerMsg::MailboxDeposited { .. }
        ));
    }
    peer.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: dep_tok,
        env: env(),
    })
    .await;
    let (code, _) = peer.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn rate_strikes_close_connection() {
    let cfg = RelayConfig {
        transport_rate: RateLimit {
            count: 1,
            per: Duration::from_secs(600),
            burst: 1,
        },
        rate_strikes: 5,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for nonce in 0..10 {
        let _ = a.wr.send(ClientMsg::Ping { nonce }).await;
    }
    let msgs = a.drain(Duration::from_secs(2)).await;
    let errors = msgs
        .iter()
        .filter(|m| {
            matches!(
                m,
                ServerMsg::Error {
                    code: ErrorCode::RateLimited,
                    ..
                }
            )
        })
        .count();
    assert!(
        errors >= 5,
        "expected >=5 RateLimited errors, got {errors}: {msgs:?}"
    );
}

#[tokio::test]
async fn mailboxes_global_cap() {
    let cfg = RelayConfig {
        mailboxes_global: 2,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;
    let h = || cinder_core::proto::Token::random();
    let th = |t: &cinder_core::proto::Token| {
        use sha2::{Digest, Sha256};
        cinder_core::proto::TokenHash::from(Into::<[u8; 32]>::into(Sha256::digest(t.as_bytes())))
    };

    for _ in 0..2 {
        a.send(ClientMsg::MailboxOpen {
            mailbox_id: cinder_core::proto::MailboxId::random(),
            h_deposit: th(&h()),
            h_fetch: th(&h()),
            ttl_secs: 60,
        })
        .await;
        assert!(matches!(a.recv().await, ServerMsg::MailboxOpened { .. }));
    }
    a.send(ClientMsg::MailboxOpen {
        mailbox_id: cinder_core::proto::MailboxId::random(),
        h_deposit: th(&h()),
        h_fetch: th(&h()),
        ttl_secs: 60,
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn invites_global_cap() {
    let cfg = RelayConfig {
        invites_global: 2,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    for _ in 0..2 {
        a.send(ClientMsg::InviteCreate {
            token: cinder_core::proto::InviteToken::random(),
            ttl_secs: 3600,
        })
        .await;
        assert!(matches!(a.recv().await, ServerMsg::InviteCreated { .. }));
    }
    a.send(ClientMsg::InviteCreate {
        token: cinder_core::proto::InviteToken::random(),
        ttl_secs: 3600,
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn conns_global_cap_rejects_excess() {
    let cfg = RelayConfig {
        conns_global: 1,
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let _held = TestClient::hello(addr, PeerId::random()).await;

    // Second concurrent conn is over the global cap → RateLimited + close,
    // even though its per-IP connect bucket still has budget.
    let (mut rd, _wr) = connect(addr).await;
    match rd.next().await {
        Some(Ok(ServerMsg::Error {
            code: ErrorCode::RateLimited,
            ..
        })) => {}
        other => panic!("expected RateLimited, got {other:?}"),
    }
    match tokio::time::timeout(Duration::from_secs(2), rd.next()).await {
        Ok(None) | Ok(Some(Err(_))) => {}
        other => panic!("expected closed connection, got {other:?}"),
    }
}

#[tokio::test]
async fn pending_hs_id_collision_refused() {
    let cfg = RelayConfig {
        handshake_timeout: Duration::from_millis(200),
        sweep_interval: Duration::from_millis(20),
        handshake_rate: RateLimit {
            count: 1000,
            per: Duration::from_secs(60),
            burst: 1000,
        },
        ..RelayConfig::default()
    };
    let addr = spawn_relay(cfg).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    let hs_id = HsId::random();
    a.send(ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id,
        body: msg1(),
    })
    .await;
    // Re-using the live hs_id on the same conn is refused, not overwritten.
    a.send(ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id,
        body: msg1(),
    })
    .await;
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::RateLimited);

    // The original attempt survived the collision: it still dies by
    // HandshakeTimeout, and its pending_hs slot is freed.
    let (code, _) = a.recv_error().await;
    assert_eq!(code, ErrorCode::HandshakeTimeout);

    a.send(ClientMsg::Ping { nonce: 7 }).await;
    assert!(matches!(a.recv().await, ServerMsg::Pong { nonce: 7 }));
}

#[tokio::test]
async fn oversized_transport_body_closes() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;

    // Frame fits under the 16 KiB codec cap but the transport ciphertext
    // exceeds the spec's 8210 B → ProtocolViolation close.
    a.send(ClientMsg::Transport {
        to: PeerId::random(),
        session_id: cinder_core::proto::SessionId::random(),
        body: vec![0xAB; 8211],
    })
    .await;
    let msgs = a.drain(Duration::from_secs(2)).await;
    assert_eq!(msgs.len(), 1);
    assert!(matches!(
        msgs[0],
        ServerMsg::Error {
            code: ErrorCode::ProtocolViolation,
            ..
        }
    ));
}
