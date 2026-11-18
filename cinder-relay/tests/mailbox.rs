mod common;

use cinder_core::frame::wire_serialized_size;
use cinder_core::proto::{
    caps, ClientMsg, DepositEnvelope, ErrorCode, HandshakeMsg, HsId, InviteToken, MailboxId,
    PeerId, ServerMsg, SessionId, StoredEnvelope, Token, TokenHash,
};
use cinder_relay::server::{RateLimit, RelayConfig};
use common::*;
use sha2::{Digest, Sha256};
use std::time::Duration;

fn h(token: &Token) -> TokenHash {
    TokenHash::from(Into::<[u8; 32]>::into(Sha256::digest(token.as_bytes())))
}

/// Relaxed per-conn frame + per-mailbox deposit rates so cap-focused tests
/// aren't fighting the 120/s and 60/min buckets; per-test knobs override.
fn fast_cfg() -> RelayConfig {
    RelayConfig {
        transport_rate: RateLimit {
            count: 100_000,
            per: Duration::from_secs(1),
            burst: 100_000,
        },
        deposit_rate: RateLimit {
            count: 100_000,
            per: Duration::from_secs(1),
            burst: 100_000,
        },
        ..RelayConfig::default()
    }
}

fn env(body: Vec<u8>, sid: SessionId) -> DepositEnvelope {
    DepositEnvelope {
        session_id: sid,
        body,
    }
}

async fn open_mb(c: &mut TestClient, mid: MailboxId, hd: TokenHash, hf: TokenHash) {
    c.send(ClientMsg::MailboxOpen {
        mailbox_id: mid,
        h_deposit: hd,
        h_fetch: hf,
        ttl_secs: 60,
    })
    .await;
    match c.recv().await {
        ServerMsg::MailboxOpened { mailbox_id, .. } => assert_eq!(mailbox_id, mid),
        other => panic!("expected MailboxOpened, got {other:?}"),
    }
}

async fn deposit(
    c: &mut TestClient,
    mid: MailboxId,
    tok: Token,
    sid: SessionId,
    body: Vec<u8>,
) -> u64 {
    c.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: tok,
        env: env(body, sid),
    })
    .await;
    match c.recv().await {
        ServerMsg::MailboxDeposited { msg_id } => msg_id,
        other => panic!("expected MailboxDeposited, got {other:?}"),
    }
}

/// Collect a full MailboxMessages sequence; returns (envelopes, per-frame `more`).
async fn fetch_all(
    c: &mut TestClient,
    mid: MailboxId,
    tok: Token,
    max: u16,
) -> (Vec<StoredEnvelope>, Vec<bool>) {
    c.send(ClientMsg::MailboxFetch {
        mailbox_id: mid,
        token: tok,
        max_msgs: max,
    })
    .await;
    let mut all = Vec::new();
    let mut flags = Vec::new();
    loop {
        match c.recv().await {
            ServerMsg::MailboxMessages { msgs, more } => {
                flags.push(more);
                all.extend(msgs);
                if !more {
                    return (all, flags);
                }
            }
            other => panic!("expected MailboxMessages, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn open_deposit_fetch_ack_end_to_end() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    let sid = SessionId::random();
    let bodies: Vec<Vec<u8>> = (0..3).map(|i| vec![0xA0 + i; 100]).collect();
    for (i, b) in bodies.iter().enumerate() {
        let msg_id = deposit(&mut peer, mid, dep_tok, sid, b.clone()).await;
        assert_eq!(msg_id, i as u64 + 1);
    }

    let (envs, flags) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(flags, vec![false]);
    assert_eq!(envs.len(), 3);
    for (i, e) in envs.iter().enumerate() {
        assert_eq!(e.msg_id, i as u64 + 1);
        assert_eq!(e.from, peer.peer_id);
        assert_eq!(e.session_id, sid);
        assert_eq!(&e.body, &bodies[i]);
        assert!(e.received_at > 0);
    }

    owner
        .send(ClientMsg::MailboxAck {
            mailbox_id: mid,
            token: fetch_tok,
            msg_ids: vec![1, 2],
        })
        .await;
    assert!(matches!(owner.recv().await, ServerMsg::Ok));

    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].msg_id, 3);
}

#[tokio::test]
async fn r8_wrong_and_cross_scope_tokens_rejected() {
    let cfg = RelayConfig {
        bad_token_strikes: 1000,
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    // 20 wrong deposit tokens → BadToken every time (R8).
    for _ in 0..20 {
        peer.send(ClientMsg::MailboxDeposit {
            mailbox_id: mid,
            token: Token::random(),
            env: env(vec![1], SessionId::random()),
        })
        .await;
        let (code, _) = peer.recv_error().await;
        assert_eq!(code, ErrorCode::BadToken);
    }

    // Fetch token cannot deposit; deposit token cannot fetch (cross-scope).
    peer.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: fetch_tok,
        env: env(vec![1], SessionId::random()),
    })
    .await;
    assert_eq!(peer.recv_error().await.0, ErrorCode::BadToken);

    peer.send(ClientMsg::MailboxFetch {
        mailbox_id: mid,
        token: dep_tok,
        max_msgs: 10,
    })
    .await;
    assert_eq!(peer.recv_error().await.0, ErrorCode::BadToken);

    // Real tokens still work after the strike series (conn stayed open).
    deposit(&mut peer, mid, dep_tok, SessionId::random(), vec![9]).await;
    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 10).await;
    assert_eq!(envs.len(), 1);
}

#[tokio::test]
async fn r9_mailbox_store_holds_hashes_only() {
    // Gate on the source: the store struct must declare TokenHash fields and
    // no raw Token field (§8 / §14.6).
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mailbox.rs"))
        .expect("mailbox.rs source");
    let struct_body = src
        .split("struct Mailbox {")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .expect("Mailbox struct body");
    assert!(struct_body.contains("h_deposit: TokenHash"));
    assert!(struct_body.contains("h_fetch: TokenHash"));
    assert!(!struct_body.contains(": Token,"));
    assert!(!struct_body.contains("token"));

    // Behavioral half: a mailbox opened with H(T) accepts T, proving the
    // stored value is compared as a digest, not a raw copy.
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mid = MailboxId::random();
    let dep_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&Token::random())).await;
    deposit(&mut owner, mid, dep_tok, SessionId::random(), vec![1]).await;
}

#[tokio::test]
async fn r5_mailbox_msg_cap_full() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    for _ in 0..caps::MAILBOX_MAX_MSGS {
        deposit(&mut peer, mid, dep_tok, SessionId::random(), vec![7]).await;
    }
    peer.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: dep_tok,
        env: env(vec![7], SessionId::random()),
    })
    .await;
    assert_eq!(peer.recv_error().await.0, ErrorCode::MailboxFull);

    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(envs.len(), caps::MAILBOX_MAX_MSGS);
}

#[tokio::test]
async fn r6_mailbox_byte_cap_full() {
    let cfg = RelayConfig {
        mailbox_max_bytes: 32 * 1024,
        mailbox_max_msgs: 10_000,
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    // 4 × 8192 B fills exactly 32 KiB; the next byte must be refused.
    for _ in 0..4 {
        deposit(&mut peer, mid, dep_tok, SessionId::random(), vec![0; 8192]).await;
    }
    peer.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: dep_tok,
        env: env(vec![0; 1], SessionId::random()),
    })
    .await;
    assert_eq!(peer.recv_error().await.0, ErrorCode::MailboxFull);

    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    let total: usize = envs.iter().map(|e| e.body.len()).sum();
    assert!(total <= 32 * 1024);
    assert_eq!(envs.len(), 4);
}

#[tokio::test]
async fn fetch_is_non_destructive_until_ack() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    let sid = SessionId::random();
    for i in 0..2 {
        deposit(&mut peer, mid, dep_tok, sid, vec![i; 50]).await;
    }

    let (first, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    let (second, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);

    owner
        .send(ClientMsg::MailboxAck {
            mailbox_id: mid,
            token: fetch_tok,
            msg_ids: first.iter().map(|e| e.msg_id).collect(),
        })
        .await;
    assert!(matches!(owner.recv().await, ServerMsg::Ok));

    let (after, flags) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert!(after.is_empty());
    assert_eq!(flags, vec![false]);
}

#[tokio::test]
async fn r7_envelope_ttl_expiry_fetch_and_sweep() {
    let cfg = RelayConfig {
        envelope_ttl: Duration::from_millis(150),
        sweep_interval: Duration::from_millis(15),
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    // Distinct session_ids so per-envelope expiry can't take the fresh one down.
    let sid_old = SessionId::random();
    let sid_new = SessionId::random();
    let old_id = deposit(&mut peer, mid, dep_tok, sid_old, vec![1]).await;
    tokio::time::sleep(Duration::from_millis(90)).await;
    let new_id = deposit(&mut peer, mid, dep_tok, sid_new, vec![2]).await;
    // At fetch time sid_old's envelope is ~aged past TTL, sid_new's is not.
    tokio::time::sleep(Duration::from_millis(90)).await;

    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].msg_id, new_id);
    assert_ne!(envs[0].msg_id, old_id);

    // Sweep already ran (15 ms cadence): the expired entry is purged, not
    // just filtered — the next fetch sees the same single live envelope.
    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(envs.len(), 1);
}

#[tokio::test]
async fn r10_session_batch_atomic_ttl_eviction() {
    let cfg = RelayConfig {
        envelope_ttl: Duration::from_millis(150),
        sweep_interval: Duration::from_millis(15),
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    let s1 = SessionId::random();
    let s2 = SessionId::random();
    for _ in 0..2 {
        deposit(&mut peer, mid, dep_tok, s1, vec![1]).await;
    }
    tokio::time::sleep(Duration::from_millis(90)).await;
    for _ in 0..2 {
        deposit(&mut peer, mid, dep_tok, s2, vec![2]).await;
    }
    // s1 ages past TTL → whole s1 batch evicted; s2 (deposited later) intact.
    tokio::time::sleep(Duration::from_millis(90)).await;

    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 500).await;
    assert_eq!(envs.len(), 2, "whole s1 group gone, s2 intact: {envs:?}");
    assert!(envs.iter().all(|e| e.session_id == s2));
    assert!(envs.iter().all(|e| e.body == vec![2]));
}

#[tokio::test]
async fn r13_mailbox_messages_multiframe_packing() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    // 50 × 7 KiB bodies: ~2 envelopes per 16 KiB frame → multi-frame response.
    let mut sent = Vec::new();
    for i in 0..50u8 {
        let body = vec![i; 7000];
        sent.push(body.clone());
        deposit(&mut peer, mid, dep_tok, SessionId::random(), body).await;
    }

    owner
        .send(ClientMsg::MailboxFetch {
            mailbox_id: mid,
            token: fetch_tok,
            max_msgs: 500,
        })
        .await;
    let mut frames = Vec::new();
    loop {
        match owner.recv().await {
            ServerMsg::MailboxMessages { msgs, more } => {
                let done = !more;
                frames.push((msgs, more));
                if done {
                    break;
                }
            }
            other => panic!("expected MailboxMessages, got {other:?}"),
        }
    }

    assert!(
        frames.len() > 10,
        "expected many frames, got {}",
        frames.len()
    );
    let mut reassembled = Vec::new();
    for (i, (msgs, more)) in frames.iter().enumerate() {
        let wire = wire_serialized_size(&ServerMsg::MailboxMessages {
            msgs: msgs.clone(),
            more: *more,
        })
        .unwrap();
        assert!(
            wire <= caps::MAX_FRAME_TRANSPORT as u64,
            "frame {i} is {wire} B"
        );
        assert!(!msgs.is_empty(), "frame {i} empty mid-sequence");
        assert_eq!(*more, i + 1 < frames.len(), "more flag at frame {i}");
        reassembled.extend(msgs.iter().cloned());
    }
    assert_eq!(reassembled.len(), 50);
    for (i, e) in reassembled.iter().enumerate() {
        assert_eq!(e.msg_id, i as u64 + 1);
        assert_eq!(e.body, sent[i]);
    }
}

#[tokio::test]
async fn mailbox_exists_on_duplicate_open() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mid = MailboxId::random();
    open_mb(&mut owner, mid, h(&Token::random()), h(&Token::random())).await;
    owner
        .send(ClientMsg::MailboxOpen {
            mailbox_id: mid,
            h_deposit: h(&Token::random()),
            h_fetch: h(&Token::random()),
            ttl_secs: 60,
        })
        .await;
    assert_eq!(owner.recv_error().await.0, ErrorCode::MailboxExists);
}

#[tokio::test]
async fn ops_on_unknown_mailbox_error_stay_open() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut a = TestClient::hello(addr, PeerId::random()).await;
    let mid = MailboxId::random();

    a.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: Token::random(),
        env: env(vec![1], SessionId::random()),
    })
    .await;
    assert_eq!(a.recv_error().await.0, ErrorCode::UnknownMailbox);
    a.send(ClientMsg::MailboxFetch {
        mailbox_id: mid,
        token: Token::random(),
        max_msgs: 10,
    })
    .await;
    assert_eq!(a.recv_error().await.0, ErrorCode::UnknownMailbox);
    a.send(ClientMsg::MailboxAck {
        mailbox_id: mid,
        token: Token::random(),
        msg_ids: vec![1],
    })
    .await;
    assert_eq!(a.recv_error().await.0, ErrorCode::UnknownMailbox);
    a.send(ClientMsg::MailboxRotate {
        mailbox_id: mid,
        token: Token::random(),
        new_h_deposit: h(&Token::random()),
        new_h_fetch: h(&Token::random()),
    })
    .await;
    assert_eq!(a.recv_error().await.0, ErrorCode::UnknownMailbox);
    a.send(ClientMsg::MailboxClose {
        mailbox_id: mid,
        token: Token::random(),
    })
    .await;
    assert_eq!(a.recv_error().await.0, ErrorCode::UnknownMailbox);

    a.send(ClientMsg::Ping { nonce: 3 }).await;
    assert!(matches!(a.recv().await, ServerMsg::Pong { nonce: 3 }));
}

#[tokio::test]
async fn rotate_swaps_tokens_close_deletes() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    let new_dep = Token::random();
    let new_fetch = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    // Rotate authed by current fetch token.
    owner
        .send(ClientMsg::MailboxRotate {
            mailbox_id: mid,
            token: fetch_tok,
            new_h_deposit: h(&new_dep),
            new_h_fetch: h(&new_fetch),
        })
        .await;
    assert!(matches!(owner.recv().await, ServerMsg::Ok));

    // Old tokens die immediately.
    peer.send(ClientMsg::MailboxDeposit {
        mailbox_id: mid,
        token: dep_tok,
        env: env(vec![1], SessionId::random()),
    })
    .await;
    assert_eq!(peer.recv_error().await.0, ErrorCode::BadToken);
    owner
        .send(ClientMsg::MailboxFetch {
            mailbox_id: mid,
            token: fetch_tok,
            max_msgs: 10,
        })
        .await;
    assert_eq!(owner.recv_error().await.0, ErrorCode::BadToken);

    // New tokens work.
    deposit(&mut peer, mid, new_dep, SessionId::random(), vec![1]).await;
    let (envs, _) = fetch_all(&mut owner, mid, new_fetch, 10).await;
    assert_eq!(envs.len(), 1);

    // Close with new fetch token → everything gone.
    owner
        .send(ClientMsg::MailboxClose {
            mailbox_id: mid,
            token: new_fetch,
        })
        .await;
    assert!(matches!(owner.recv().await, ServerMsg::Ok));
    owner
        .send(ClientMsg::MailboxFetch {
            mailbox_id: mid,
            token: new_fetch,
            max_msgs: 10,
        })
        .await;
    assert_eq!(owner.recv_error().await.0, ErrorCode::UnknownMailbox);

    // Closed id can be re-opened (not MailboxExists).
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;
}

#[tokio::test]
async fn fetch_max_msgs_bounds_result() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut peer = TestClient::hello(addr, PeerId::random()).await;

    let mid = MailboxId::random();
    let dep_tok = Token::random();
    let fetch_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&fetch_tok)).await;

    for _ in 0..5 {
        deposit(&mut peer, mid, dep_tok, SessionId::random(), vec![1]).await;
    }
    let (envs, _) = fetch_all(&mut owner, mid, fetch_tok, 2).await;
    assert_eq!(envs.len(), 2);
    assert_eq!(envs[0].msg_id, 1);
    assert_eq!(envs[1].msg_id, 2);
}

#[tokio::test]
async fn oversize_envelope_body_is_protocol_violation() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mid = MailboxId::random();
    let dep_tok = Token::random();
    open_mb(&mut owner, mid, h(&dep_tok), h(&Token::random())).await;

    owner
        .send(ClientMsg::MailboxDeposit {
            mailbox_id: mid,
            token: dep_tok,
            env: env(vec![0; caps::MAX_PLAINTEXT + 19], SessionId::random()),
        })
        .await;
    let msgs = owner.drain(Duration::from_secs(2)).await;
    assert!(msgs.iter().any(|m| matches!(
        m,
        ServerMsg::Error {
            code: ErrorCode::ProtocolViolation,
            ..
        }
    )));
}

#[tokio::test]
async fn bad_token_strikes_close_connection() {
    let cfg = RelayConfig {
        bad_token_strikes: 3,
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mid = MailboxId::random();
    open_mb(&mut owner, mid, h(&Token::random()), h(&Token::random())).await;

    for _ in 0..3 {
        owner
            .send(ClientMsg::MailboxFetch {
                mailbox_id: mid,
                token: Token::random(),
                max_msgs: 10,
            })
            .await;
    }
    let msgs = owner.drain(Duration::from_secs(2)).await;
    let bad = msgs
        .iter()
        .filter(|m| {
            matches!(
                m,
                ServerMsg::Error {
                    code: ErrorCode::BadToken,
                    ..
                }
            )
        })
        .count();
    assert_eq!(bad, 3, "expected 3 BadToken errors: {msgs:?}");
}

#[tokio::test]
async fn invite_create_accept_delivers_msg1() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut acc = TestClient::hello(addr, PeerId::random()).await;

    let token = InviteToken::random();
    owner
        .send(ClientMsg::InviteCreate {
            token,
            ttl_secs: 3600,
        })
        .await;
    match owner.recv().await {
        ServerMsg::InviteCreated {
            token: got,
            expires_at,
        } => {
            assert_eq!(got, token);
            assert!(expires_at > 0);
        }
        other => panic!("expected InviteCreated, got {other:?}"),
    }

    let hs_id = HsId::random();
    let msg1 = vec![0xEE; 32];
    acc.send(ClientMsg::InviteAccept {
        token,
        hs_id,
        msg1: msg1.clone(),
    })
    .await;
    match owner.recv().await {
        ServerMsg::Handshake {
            from,
            hs_id: got,
            body: HandshakeMsg::Msg1(body),
        } => {
            assert_eq!(from, acc.peer_id);
            assert_eq!(got, hs_id);
            assert_eq!(body, msg1);
        }
        other => panic!("expected Handshake Msg1, got {other:?}"),
    }
}

#[tokio::test]
async fn invite_reuse_and_unknown_token_invalid() {
    let addr = spawn_relay(fast_cfg()).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut acc = TestClient::hello(addr, PeerId::random()).await;

    let token = InviteToken::random();
    owner
        .send(ClientMsg::InviteCreate {
            token,
            ttl_secs: 3600,
        })
        .await;
    assert!(matches!(
        owner.recv().await,
        ServerMsg::InviteCreated { .. }
    ));

    acc.send(ClientMsg::InviteAccept {
        token,
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    assert!(matches!(owner.recv().await, ServerMsg::Handshake { .. }));

    // Single-use: second accept of the consumed token → InviteInvalid.
    acc.send(ClientMsg::InviteAccept {
        token,
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    assert_eq!(acc.recv_error().await.0, ErrorCode::InviteInvalid);

    // Never-issued token → InviteInvalid.
    acc.send(ClientMsg::InviteAccept {
        token: InviteToken::random(),
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    assert_eq!(acc.recv_error().await.0, ErrorCode::InviteInvalid);
}

#[tokio::test]
async fn invite_offline_owner_drops_msg1_times_out() {
    let cfg = RelayConfig {
        handshake_timeout: Duration::from_millis(80),
        sweep_interval: Duration::from_millis(10),
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut acc = TestClient::hello(addr, PeerId::random()).await;

    let token = InviteToken::random();
    {
        let mut owner = TestClient::hello(addr, PeerId::random()).await;
        owner
            .send(ClientMsg::InviteCreate {
                token,
                ttl_secs: 3600,
            })
            .await;
        assert!(matches!(
            owner.recv().await,
            ServerMsg::InviteCreated { .. }
        ));
    } // owner disconnects

    // §7: slot consumed, Msg1 dropped (no handshake mailbox), NO error to the
    // acceptor — the attempt dies via HandshakeTimeout.
    acc.send(ClientMsg::InviteAccept {
        token,
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    let (code, _) = acc.recv_error().await;
    assert_eq!(code, ErrorCode::HandshakeTimeout);
}

#[tokio::test]
async fn invite_expired_token_invalid() {
    let cfg = RelayConfig {
        invite_max_ttl: Duration::from_millis(50),
        sweep_interval: Duration::from_millis(10),
        ..fast_cfg()
    };
    let addr = spawn_relay(cfg).await;
    let mut owner = TestClient::hello(addr, PeerId::random()).await;
    let mut acc = TestClient::hello(addr, PeerId::random()).await;

    let token = InviteToken::random();
    owner
        .send(ClientMsg::InviteCreate {
            token,
            ttl_secs: 3600, // clamped to the 50 ms injected max
        })
        .await;
    assert!(matches!(
        owner.recv().await,
        ServerMsg::InviteCreated { .. }
    ));

    tokio::time::sleep(Duration::from_millis(120)).await;
    acc.send(ClientMsg::InviteAccept {
        token,
        hs_id: HsId::random(),
        msg1: vec![0xEE; 32],
    })
    .await;
    assert_eq!(acc.recv_error().await.0, ErrorCode::InviteInvalid);
}
