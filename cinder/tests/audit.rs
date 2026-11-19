#![allow(dead_code)] // #[path] includes expose items other tests use

//! Regressions for the code-audit findings: mailbox-drain deadlock, peer
//! mutation scoping, contact-add wedge, inbound gating, offline queue
//! flush, react gating, and unread marking.

#[path = "../src/call.rs"]
mod call;
#[path = "../src/core.rs"]
mod core;
#[path = "../src/rpc/mod.rs"]
mod rpc;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/tui/mod.rs"]
mod tui;
#[path = "../src/xfer.rs"]
mod xfer;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cinder_core::frame::{wire_deserialize, wire_serialize};
use cinder_core::identity::Identity;
use cinder_core::proto::{PeerId, SessionId};
use cinder_core::session::Session;
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::net::TcpListener;

use crate::core::{chat_str, contact_str, decode_app, AppMsg, Core, MetaState};
use crate::rpc::core::{
    ChatHistoryParams, ChatListParams, CinderCore, ContactIdParams, FileSendParams,
    MsgDeleteParams, MsgReactParams, MsgSendParams,
};
use crate::rpc::protocol::ErrorKind;
use crate::store::{Dir, MsgKind, MsgState, NewMessage, Store};
use crate::tui::backend as tb;

const CONNECT_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(25);
const DELIVERY_WAIT: Duration = Duration::from_secs(20);

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "audit=debug".into()),
        )
        .try_init();
}

async fn spawn_relay() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
            .await;
    });
    addr
}

async fn open_core(dir: &std::path::Path, relay: SocketAddr) -> Arc<Core> {
    Core::open(
        dir,
        Some((relay, relay.to_string())),
        Identity::generate().unwrap(),
    )
    .await
    .expect("core open")
}

/// Restart path: reopen `dir` with the SAME identity blob (same static key →
/// same store key → the existing encrypted DB opens).
async fn reopen_core(dir: &std::path::Path, relay: SocketAddr, blob: &[u8]) -> Arc<Core> {
    Core::open(
        dir,
        Some((relay, relay.to_string())),
        Identity::from_stored_blob(blob).expect("identity clone"),
    )
    .await
    .expect("core open")
}

async fn paired(
    relay: SocketAddr,
    dir_a: &std::path::Path,
    dir_b: &std::path::Path,
) -> (Arc<Core>, Arc<Core>) {
    let a = open_core(dir_a, relay).await;
    let b = open_core(dir_b, relay).await;
    assert!(
        a.wait_connected(CONNECT_WAIT).await,
        "A never reached the relay"
    );
    assert!(
        b.wait_connected(CONNECT_WAIT).await,
        "B never reached the relay"
    );

    let (link, _) = a.invite_link(3600).await.expect("invite.create");
    let peer_a = b
        .clone()
        .start_add(&link, None)
        .await
        .expect("invite.accept");
    assert!(
        b.wait_meta(&peer_a, MetaState::Active, HANDSHAKE_WAIT)
            .await,
        "B never went active with A"
    );
    let peer_b = b.identity().peer_id();
    assert!(
        a.wait_meta(&peer_b, MetaState::PendingIn, HANDSHAKE_WAIT)
            .await,
        "A never saw B's inbound request"
    );
    a.contact_accept(ContactIdParams {
        contact_id: contact_str(&peer_b),
    })
    .await
    .expect("contact.accept");
    assert!(
        a.wait_meta(&peer_b, MetaState::Active, HANDSHAKE_WAIT)
            .await,
        "A never went active with B"
    );
    (a, b)
}

/// Kill a core's link/tasks. The pump holds an Arc so `drop(core)` is not
/// enough — begin_shutdown is what actually closes the TCP connection.
/// 30s: the workspace run executes dozens of tokio runtimes in parallel
/// and the link teardown can be scheduled late under load.
async fn stop(core: &Arc<Core>) {
    core.begin_shutdown();
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < Duration::from_secs(30) {
        if core.conn_state() == tb::ConnState::Offline {
            return;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    panic!("core never went offline");
}

async fn wait_for_body(core: &Arc<Core>, chat_id: &str, text: &str, dur: Duration) -> Value {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < dur {
        if let Ok(v) = core
            .chat_history(ChatHistoryParams {
                chat_id: chat_id.to_string(),
                before: None,
                limit: Some(50),
            })
            .await
        {
            if let Some(m) = v["messages"]
                .as_array()
                .and_then(|ms| ms.iter().find(|m| m["body"]["text"] == text))
            {
                return m.clone();
            }
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    panic!("message {text:?} never arrived in {chat_id}");
}

async fn wait_for<T>(mut f: impl FnMut() -> Option<T>, dur: Duration, what: &str) -> T {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < dur {
        if let Some(t) = f() {
            return t;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// fix #2: peer-supplied mutations may only touch dir='in' rows in the peer's
// own chat — outbound rows and other chats are unreachable by msg_id.
// ---------------------------------------------------------------------------
#[test]
fn audit_peer_mutations_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), &[0x5au8; 32]).unwrap();
    let peer_a = [1u8; 32];
    let peer_b = [2u8; 32];
    s.upsert_chat("chat:a", &peer_a, None, false).unwrap();
    s.upsert_chat("chat:b", &peer_b, None, false).unwrap();

    // An inbound message in chat:a (the peer's own message), an outbound one
    // (ours), and an inbound message in a DIFFERENT chat.
    s.insert_message(&NewMessage {
        msg_id: "msg:in".into(),
        chat_id: "chat:a".into(),
        dir: Dir::In,
        kind: MsgKind::Text,
        body: "peer wrote this".into(),
        state: Some(MsgState::Delivered),
        client_msg_id: None,
        ts: 1000,
        expires_at: None,
    })
    .unwrap();
    s.insert_message(&NewMessage {
        msg_id: "msg:out".into(),
        chat_id: "chat:a".into(),
        dir: Dir::Out,
        kind: MsgKind::Text,
        body: "we wrote this".into(),
        state: Some(MsgState::Sent),
        client_msg_id: None,
        ts: 1001,
        expires_at: None,
    })
    .unwrap();
    s.insert_message(&NewMessage {
        msg_id: "msg:other".into(),
        chat_id: "chat:b".into(),
        dir: Dir::In,
        kind: MsgKind::Text,
        body: "other chat inbound".into(),
        state: Some(MsgState::Delivered),
        client_msg_id: None,
        ts: 1002,
        expires_at: None,
    })
    .unwrap();

    // Legit: peer edits its own inbound message in its own chat.
    s.apply_peer_edit("msg:in", "chat:a", "edited by peer")
        .unwrap();
    assert_eq!(
        s.get_message("msg:in").unwrap().unwrap().body,
        "edited by peer"
    );

    // Forgery: peer tries to rewrite OUR outbound message — rejected.
    assert!(s.apply_peer_edit("msg:out", "chat:a", "forged").is_err());
    assert_eq!(
        s.get_message("msg:out").unwrap().unwrap().body,
        "we wrote this"
    );

    // Forgery: peer names a msg_id living in ANOTHER chat — rejected.
    assert!(s.apply_peer_edit("msg:other", "chat:a", "x-chat").is_err());
    assert!(s.apply_peer_delete("msg:other", "chat:a").is_err());
    assert_eq!(
        s.get_message("msg:other").unwrap().unwrap().body,
        "other chat inbound"
    );

    // Peer delete + react scoped the same way.
    assert!(s.apply_peer_delete("msg:out", "chat:a").is_err());
    assert!(s.apply_peer_delete("msg:in", "chat:a").is_ok());
    assert!(s.get_message("msg:in").unwrap().unwrap().deleted);

    let reactor = peer_a;
    assert!(s
        .apply_peer_reaction("msg:out", "chat:a", "x", &reactor)
        .is_ok());
    assert!(s
        .apply_peer_reaction("msg:other", "chat:a", "x", &reactor)
        .is_err());
    assert!(s
        .remove_peer_reaction("msg:out", "chat:a", &reactor)
        .is_ok());
    assert!(s
        .remove_peer_reaction("msg:nope", "chat:a", &reactor)
        .is_err());
}

// ---------------------------------------------------------------------------
// fix #5 (+ #2 belt): a contact in Declined state keeps no inbound channel —
// app frames drop silently and are counted.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_declined_inbound_dropped() {
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let relay = spawn_relay().await;
    let a = open_core(dir.path(), relay).await;
    assert!(a.wait_connected(CONNECT_WAIT).await);

    let peer = PeerId::from_static_key(&[9u8; 32]);
    a.test_set_state(&peer, MetaState::Declined);
    let before = a.test_dropped_inbound();
    a.test_handle_app(
        peer,
        AppMsg::Text {
            msg_id: "msg:declined".into(),
            body: "should never land".into(),
            expires_at: None,
        },
    )
    .await;
    assert_eq!(a.test_dropped_inbound(), before + 1);
    let found = a
        .chat_history(ChatHistoryParams {
            chat_id: chat_str(&peer),
            before: None,
            limit: Some(10),
        })
        .await
        .unwrap();
    assert!(
        found["messages"].as_array().unwrap().is_empty(),
        "declined contact's message must not be stored"
    );
}

// ---------------------------------------------------------------------------
// fix #7 + #12: msg.react goes through the send gate (declined → blocked
// error) and rejects oversized emoji with InvalidParams.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_react_gate_and_emoji_cap() {
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let relay = spawn_relay().await;
    let a = open_core(dir.path(), relay).await;
    assert!(a.wait_connected(CONNECT_WAIT).await);

    let peer = PeerId::from_static_key(&[7u8; 32]);
    let chat_id = chat_str(&peer);
    a.store()
        .upsert_chat(&chat_id, peer.as_bytes(), None, false)
        .unwrap();
    a.store()
        .insert_message(&NewMessage {
            msg_id: "msg:t".into(),
            chat_id: chat_id.clone(),
            dir: Dir::Out,
            kind: MsgKind::Text,
            body: "react to me".into(),
            state: Some(MsgState::Sent),
            client_msg_id: None,
            ts: 5000,
            expires_at: None,
        })
        .unwrap();

    // Oversized emoji → InvalidParams, before any queueing.
    let big = "🦀".repeat(20); // 80 bytes > 32
    let err = a
        .msg_react(MsgReactParams {
            msg_id: "msg:t".into(),
            emoji: Some(big),
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind.as_str(), "invalid_params");

    // Declined contact → the send gate refuses.
    a.test_set_state(&peer, MetaState::Declined);
    let err = a
        .msg_react(MsgReactParams {
            msg_id: "msg:t".into(),
            emoji: Some("ok".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind.as_str(), "contact_blocked");
    a.test_set_state(&peer, MetaState::Active);
    assert!(
        a.msg_react(MsgReactParams {
            msg_id: "msg:t".into(),
            emoji: Some("ok".into()),
        })
        .await
        .is_ok(),
        "react on an active contact must succeed"
    );
}

// ---------------------------------------------------------------------------
// fix #11: fetching chat.history marks the chat read (no extra event).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_history_marks_read() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;

    let peer_a = a.identity().peer_id();
    let marker = format!("unread-marker-{}", std::process::id());
    let r = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: marker.clone(),
            client_msg_id: "unread-1".into(),
            ttl_ms: None,
        })
        .await
        .expect("msg.send");
    let msg_id = r["msg_id"].as_str().unwrap().to_string();

    let peer_b = b.identity().peer_id();
    let chat_id = chat_str(&peer_b);
    // Wait via the store (does NOT mark read) so unread is really > 0 first.
    wait_for(
        || a.store().get_message(&msg_id).ok().flatten(),
        DELIVERY_WAIT,
        "inbound message row",
    )
    .await;
    let unread = a
        .chat_list(ChatListParams {
            limit: Some(50),
            cursor: None,
        })
        .await
        .unwrap()["chats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["chat_id"] == chat_id)
        .and_then(|c| c["unread"].as_u64())
        .unwrap_or(0);
    assert!(
        unread > 0,
        "precondition: chat must be unread before history"
    );

    a.chat_history(ChatHistoryParams {
        chat_id: chat_id.clone(),
        before: None,
        limit: Some(50),
    })
    .await
    .unwrap();
    let unread_after = a
        .chat_list(ChatListParams {
            limit: Some(50),
            cursor: None,
        })
        .await
        .unwrap()["chats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["chat_id"] == chat_id)
        .and_then(|c| c["unread"].as_u64())
        .unwrap_or(0);
    assert_eq!(unread_after, 0, "chat.history must mark the chat read");
}

// ---------------------------------------------------------------------------
// fix #4: a handshake failure flips pending_out → failed:<reason> and the
// contact can be re-added instead of wedging as Conflict.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_failed_add_retryable() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let a = open_core(dir_a.path(), relay).await;
    assert!(a.wait_connected(CONNECT_WAIT).await);

    // A syntactically valid link whose owner never comes online: the
    // handshake times out, the contact must land in failed:* and accept a
    // retry instead of wedging as Conflict.
    let ghost = Identity::generate().unwrap();
    let link = ghost.uri(&relay.to_string(), None);
    let peer = a.start_add(&link, None).await.expect("first contact.add");
    wait_for(
        || a.test_substate(&peer).filter(|s| s.starts_with("failed")),
        Duration::from_secs(30),
        "pending_out to reach a failed substate",
    )
    .await;

    let again = a.start_add(&link, None).await;
    assert!(
        again.is_ok(),
        "re-adding a failed contact must not be a Conflict: {again:?}"
    );
    assert_eq!(
        a.test_substate(&peer).as_deref(),
        Some("connecting"),
        "retry must re-arm the pending add"
    );
}

// ---------------------------------------------------------------------------
// fixes #1 + #10 + drain safety: a deposit left in the peer's mailbox is
// fetched+acked on the peer's next link-up without deadlocking op_lock, and
// a dead-generation deposited frame is skipped + counted, never parked.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_mailbox_deposit_drain_no_deadlock() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();
    let peer_a = a.identity().peer_id();
    let blob_a = a.identity().stored_blob().expose_secret().to_vec();
    let blob_b = b.identity().stored_blob().expose_secret().to_vec();

    // Wait for B's mailbox grant to land on A before the stop (the grant is
    // in-band app traffic and shutdown kills the pump without draining).
    wait_for(
        || a.test_has_grant(&peer_b).then_some(()),
        DELIVERY_WAIT,
        "B's mailbox grant to persist on A",
    )
    .await;

    // Restart A so its link-up presence_subscribe covers B (subscribe only
    // catches contacts that were Active when the link came up).
    stop(&a).await;
    let a2 = reopen_core(dir_a.path(), relay, &blob_a).await;
    assert!(a2.wait_connected(CONNECT_WAIT).await);
    wait_for(
        || a2.test_presence(&peer_b).filter(|o| *o),
        CONNECT_WAIT,
        "A2 to learn B online via presence",
    )
    .await;

    // Live round-trip once so A2's session for B exists (a deposit is
    // encrypted under a live session).
    let warm = format!("warm-{}", std::process::id());
    a2.msg_send(MsgSendParams {
        to: contact_str(&peer_b),
        body: warm.clone(),
        client_msg_id: "warm-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("warm send");
    wait_for_body(&b, &chat_str(&peer_a), &warm, DELIVERY_WAIT).await;

    // B goes offline; A2 sees presence=false, then deposits into B's box.
    stop(&b).await;
    wait_for(
        || a2.test_presence(&peer_b).filter(|o| !*o),
        CONNECT_WAIT,
        "A2 to learn B offline",
    )
    .await;
    let deposited = format!("deposited-{}", std::process::id());
    let r = a2
        .msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: deposited.clone(),
            client_msg_id: "dep-1".into(),
            ttl_ms: None,
        })
        .await
        .expect("deposit send");
    assert!(r["msg_id"].is_string());

    // B restarts: ensure_mailbox must tolerate MailboxExists (its box is
    // still live), mailbox_drain must NOT deadlock on the ack op, and the
    // dead-generation deposited frame must be skipped + counted, never
    // parked into the next session's poison.
    let b2 = reopen_core(dir_b.path(), relay, &blob_b).await;
    assert!(b2.wait_connected(CONNECT_WAIT).await);
    wait_for(
        || (b2.test_dropped_inbound() > 0).then_some(()),
        CONNECT_WAIT,
        "dead-generation deposit to be dropped+counted",
    )
    .await;
    // The dead-generation frame provably never stored itself: it was
    // dropped+counted above. If `deposited` appears in history now or
    // later it arrived via the intended live re-drive over the fresh
    // session (deposit healing — the SEC-M2 fix), which is correct
    // behavior, not the dead frame decrypting.

    // The drain completed (no op_lock deadlock): a fresh B2→A2 handshake
    // works end-to-end, proving the pump survived the drain+ack.
    let after = format!("post-drain-{}", std::process::id());
    b2.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: after.clone(),
        client_msg_id: "post-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("post-drain send");
    wait_for_body(&a2, &chat_str(&peer_b), &after, DELIVERY_WAIT).await;
}

// ---------------------------------------------------------------------------
// fix #6: a message queued while the peer is offline flushes when presence
// reports them back (queue → handshake → deliver), and pending_out drains.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_offline_queue_flush_on_return() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();
    let peer_a = a.identity().peer_id();
    let blob_a = a.identity().stored_blob().expose_secret().to_vec();
    let blob_b = b.identity().stored_blob().expose_secret().to_vec();

    // Grant must be persisted before the stop (in-band traffic dies with
    // the pump).
    wait_for(
        || a.test_has_grant(&peer_b).then_some(()),
        DELIVERY_WAIT,
        "B's mailbox grant on A",
    )
    .await;
    // Restart A so presence subscription covers B, then stop B.
    stop(&a).await;
    let a2 = reopen_core(dir_a.path(), relay, &blob_a).await;
    assert!(a2.wait_connected(CONNECT_WAIT).await);
    wait_for(
        || a2.test_presence(&peer_b),
        CONNECT_WAIT,
        "A2 presence for B",
    )
    .await;
    stop(&b).await;
    wait_for(
        || a2.test_presence(&peer_b).filter(|o| !*o),
        CONNECT_WAIT,
        "A2 to learn B offline",
    )
    .await;

    // A2 restarted → no live session for B → this send MUST queue (the
    // deposit path needs a live session, the handshake can't complete while
    // B is offline).
    let queued_body = format!("queued-{}", std::process::id());
    a2.msg_send(MsgSendParams {
        to: contact_str(&peer_b),
        body: queued_body.clone(),
        client_msg_id: "q-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("queued send");
    wait_for(
        || (a2.test_pending_len(&peer_b) > 0).then_some(()),
        Duration::from_secs(10),
        "message to sit in pending_out",
    )
    .await;

    // B returns: PeerStatus{online} kicks a handshake, the queue flushes,
    // and the message lands. Extra slack: presence→handshake→flush→decrypt
    // is a 5-hop async chain — starved CI runners (macos, 3 cores, dozens
    // of concurrent test runtimes) can stretch it well past 45s.
    let b2 = reopen_core(dir_b.path(), relay, &blob_b).await;
    assert!(b2.wait_connected(CONNECT_WAIT).await);
    wait_for_body(
        &b2,
        &chat_str(&peer_a),
        &queued_body,
        HANDSHAKE_WAIT + DELIVERY_WAIT + Duration::from_secs(45),
    )
    .await;
    wait_for(
        || (a2.test_pending_len(&peer_b) == 0).then_some(()),
        Duration::from_secs(15),
        "pending_out to drain",
    )
    .await;
}

// ---------------------------------------------------------------------------
// HIGH-1: a transport frame stamped `from` a peer that does NOT own the
// bound sid is a hijack probe — it must drop pre-decrypt, without tombstoning
// or even touching the owner's stream. Same-peer frames under a foreign sid
// tombstone the bogus stream id, never the live one.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_bound_sid_foreign_sender_dropped() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();
    let peer_a = a.identity().peer_id();
    let b_sid = a
        .test_remote_sid(&peer_b)
        .expect("B's stream id bound after pairing");
    let foreign = PeerId::from_static_key(&[0xEEu8; 32]);

    // `from` = a peer that does not own b_sid. Pre-fix this decrypted
    // under B's session and destroyed it on the inevitable decrypt-fatal.
    let dropped = a.test_dropped_inbound();
    a.test_handle_transport(foreign, b_sid, vec![0xAAu8; 64])
        .await;
    assert_eq!(a.test_dropped_inbound(), dropped + 1);
    assert_eq!(a.test_remote_sid(&peer_b), Some(b_sid));
    assert!(a.test_has_session(&peer_b));

    // `from` = B itself but under a sid that is NOT its bound stream — a
    // stale or fabricated stream id: tombstone + drop, live sid untouched.
    let stale = SessionId::random();
    a.test_handle_transport(peer_b, stale, vec![0xBBu8; 64])
        .await;
    assert_eq!(a.test_remote_sid(&peer_b), Some(b_sid));
    assert!(a.test_has_session(&peer_b));

    // A replay of the now-tombstoned sid hits the dead-sid path — still
    // no session damage, still no decrypt.
    a.test_handle_transport(peer_b, stale, vec![0xCCu8; 64])
        .await;
    assert!(a.test_has_session(&peer_b));
    assert_eq!(a.test_remote_sid(&peer_b), Some(b_sid));

    // The real stream still decrypts end-to-end: a genuine message from B
    // lands on A — the probe never poisoned the channel.
    let proof = format!("post-inject-{}", std::process::id());
    b.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: proof.clone(),
        client_msg_id: "proof-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("post-inject send");
    wait_for_body(&a, &chat_str(&peer_b), &proof, DELIVERY_WAIT).await;
}

// ---------------------------------------------------------------------------
// HIGH-2: a handshake-claimed remote sid already bound to a DIFFERENT live
// peer is a sid the claimant cannot own — indexing it would hijack the
// victim's inbound routing. install_session must refuse and leave the
// victim's binding intact. An honest claim installs normally.
// ---------------------------------------------------------------------------
/// Drive a full XX for `init` against a fresh responder holding `resp_id`;
/// `resp_peer` is the peer-id the initiator pins (the responder's own).
fn drive_xx(resp_id: &Identity, resp_peer: &PeerId, init: &mut Session) -> Session {
    let mut resp = Session::responder(resp_id).unwrap();
    let m1 = init.produce_handshake().unwrap();
    resp.accept_handshake(m1).unwrap();
    let m2 = resp.produce_handshake().unwrap();
    init.accept_handshake(m2).unwrap();
    init.verify_peer_id(resp_peer).unwrap();
    let m3 = init.produce_handshake().unwrap();
    resp.accept_handshake(m3).unwrap();
    resp.accept_learned_peer().unwrap();
    resp.finish_handshake().unwrap();
    init.finish_handshake().unwrap();
    resp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_claimed_remote_sid_install_refused() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();
    let (a, c) = paired(relay, dir_a.path(), dir_c.path()).await;
    let peer_c = c.identity().peer_id();
    let peer_a = a.identity().peer_id();
    let victim_sid = a
        .test_remote_sid(&peer_c)
        .expect("C's stream id bound on A");

    // Forge: a real XX where the initiator stamps C's stream id as its own.
    // A's responder learns remote_sid = victim_sid from Msg1 — exactly the
    // field install_session must not trust blindly.
    let attacker_id = Identity::generate().unwrap();
    let attacker_peer = attacker_id.peer_id();
    let mut forged = Session::initiator(&attacker_id).unwrap();
    forged.test_set_local_sid(victim_sid);
    let resp = drive_xx(a.identity(), &peer_a, &mut forged);
    assert_eq!(
        resp.remote_session_id(),
        Some(victim_sid),
        "responder must have learned the forged sid from Msg1"
    );
    assert!(
        !a.test_install_session(attacker_peer, resp),
        "a sid claimed over a live peer's binding must refuse install"
    );
    assert_eq!(a.test_remote_sid(&peer_c), Some(victim_sid));

    // Honest control: a second peer claiming a fresh random sid installs.
    let honest_id = Identity::generate().unwrap();
    let honest_peer = honest_id.peer_id();
    let mut init = Session::initiator(&honest_id).unwrap();
    let resp2 = drive_xx(a.identity(), &peer_a, &mut init);
    let learned = resp2.remote_session_id().expect("sid learned in XX");
    assert_ne!(learned, victim_sid, "random sid collision is not the test");
    assert!(a.test_install_session(honest_peer, resp2));
    assert_eq!(a.test_remote_sid(&honest_peer), Some(learned));

    // C's stream is untouched by the refused install: a live round-trip
    // still decrypts over it.
    let proof = format!("victim-alive-{}", std::process::id());
    c.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: proof.clone(),
        client_msg_id: "victim-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("victim send");
    wait_for_body(&a, &chat_str(&peer_c), &proof, DELIVERY_WAIT).await;
}

// ---------------------------------------------------------------------------
// HIGH-3: a rekey control unit is the LAST frame under the old egress key —
// it must reach the wire after the data frame it follows. With the policy
// shrunk to msgs=3, sends 3 and 6 each carry a control unit; any ordering
// swap desyncs the Noise stream and the next decrypt kills the session.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_rekey_boundary_wire_order() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();
    let peer_a = a.identity().peer_id();

    a.test_set_rekey_policy(&peer_b, 3);
    for i in 0..7 {
        let body = format!("rk-{i}-{}", std::process::id());
        a.msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: body.clone(),
            client_msg_id: format!("rk-{i}"),
            ttl_ms: None,
        })
        .await
        .expect("send");
        wait_for_body(&b, &chat_str(&peer_a), &body, DELIVERY_WAIT).await;
    }
    assert!(
        a.test_has_session(&peer_b),
        "session must survive two rekey boundaries"
    );
}

// ---------------------------------------------------------------------------
// SEC-M2 + M12: a mailbox deposit is not proof of delivery — the row stays
// `queued` and the item stays in pending_out; a restart repopulates the
// queue from the DB and the first live path re-drives it. The deposited
// copy (sealed under a dead generation) can never land on B.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_deposit_stays_queued_then_redrives() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();
    let peer_a = a.identity().peer_id();
    let blob_a = a.identity().stored_blob().expose_secret().to_vec();
    let blob_b = b.identity().stored_blob().expose_secret().to_vec();

    wait_for(
        || a.test_has_grant(&peer_b).then_some(()),
        DELIVERY_WAIT,
        "B's mailbox grant on A",
    )
    .await;
    // Take B REALLY offline: merely faking presence would be overturned by
    // the next truthful broadcast (B is online) and the live flush would
    // beat the deposit assertion. B's link drop is final — no broadcast
    // can resurrect it — so the offline view holds for the whole deposit.
    stop(&b).await;
    a.test_set_presence(&peer_b, false);

    let body = format!("deposited-{}", std::process::id());
    let r = a
        .msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: body.clone(),
            client_msg_id: "dep-1".into(),
            ttl_ms: None,
        })
        .await
        .expect("deposit send");
    let msg_id = r["msg_id"].as_str().unwrap().to_string();

    // The relay-side deposit settles in milliseconds on localhost — give
    // the spawned task room, then the row must still be `queued` (a deposit
    // is NOT delivery; only a live send or the peer's Ack may advance it).
    tokio::time::sleep(Duration::from_millis(800)).await;
    let m = a.store().get_message(&msg_id).unwrap().unwrap();
    assert_eq!(
        m.state,
        Some(MsgState::Queued),
        "mailbox deposit must not mark the message sent"
    );
    wait_for(
        || (a.test_pending_len(&peer_b) > 0).then_some(()),
        Duration::from_secs(10),
        "deposited item retained in pending_out",
    )
    .await;

    // Kill A before any live flush: the queued DB row is the only copy of
    // the intent — M12 repopulates pending_out from it on reopen. Assert
    // BEFORE the link is up: open() fills pending_out synchronously, and
    // the check must precede any link-up flush.
    stop(&a).await;
    let a2 = reopen_core(dir_a.path(), relay, &blob_a).await;
    assert!(
        a2.test_pending_len(&peer_b) > 0,
        "queued row must re-enter pending_out on restart"
    );
    assert!(a2.wait_connected(CONNECT_WAIT).await);

    // B restarts: when B2's link announces presence, A2 handshakes, flushes
    // the queue, and the LIVE copy lands — the deposited one is sealed
    // under a dead session generation and can never decrypt, so delivery
    // here is proof both that the deposit happened and that the re-drive
    // carried it.
    let b2 = reopen_core(dir_b.path(), relay, &blob_b).await;
    assert!(b2.wait_connected(CONNECT_WAIT).await);
    wait_for_body(
        &b2,
        &chat_str(&peer_a),
        &body,
        HANDSHAKE_WAIT + DELIVERY_WAIT,
    )
    .await;
    wait_for(
        || (a2.test_pending_len(&peer_b) == 0).then_some(()),
        Duration::from_secs(15),
        "pending_out drained by the live re-drive",
    )
    .await;
}

// ---------------------------------------------------------------------------
// wire layout: AppMsg::Text carries the absolute deadline inside the Noise
// channel; pre-TTL frames still decode through the AppMsgV1 fallback.
// ---------------------------------------------------------------------------
#[test]
fn wire_text_ttl_roundtrip_and_v1_compat() {
    let m = AppMsg::Text {
        msg_id: "msg:x".into(),
        body: "vanish".into(),
        expires_at: Some(1_900_000_000_000),
    };
    let bytes = wire_serialize(&m).unwrap();
    match wire_deserialize::<AppMsg>(&bytes).unwrap() {
        AppMsg::Text {
            msg_id,
            body,
            expires_at,
        } => {
            assert_eq!(msg_id, "msg:x");
            assert_eq!(body, "vanish");
            assert_eq!(expires_at, Some(1_900_000_000_000));
        }
        _ => panic!("variant changed"),
    }

    let m = AppMsg::Text {
        msg_id: "msg:y".into(),
        body: "stay".into(),
        expires_at: None,
    };
    match wire_deserialize::<AppMsg>(&wire_serialize(&m).unwrap()).unwrap() {
        AppMsg::Text { expires_at, .. } => assert_eq!(expires_at, None),
        _ => panic!("variant changed"),
    }

    // A frame written by a pre-TTL build (no expires_at field on the wire):
    // the strict decoder errors on the missing tail, decode_app falls back.
    #[derive(serde::Serialize)]
    enum V1 {
        Text { msg_id: String, body: String },
    }
    let old = wire_serialize(&V1::Text {
        msg_id: "msg:old".into(),
        body: "hi".into(),
    })
    .unwrap();
    assert!(
        wire_deserialize::<AppMsg>(&old).is_err(),
        "bincode fixint layout can't read a short Text — that is exactly why decode_app exists"
    );
    match decode_app(&old).expect("v1 fallback must decode") {
        AppMsg::Text {
            msg_id,
            body,
            expires_at,
        } => {
            assert_eq!(msg_id, "msg:old");
            assert_eq!(body, "hi");
            assert_eq!(expires_at, None, "v1 frames carry no deadline");
        }
        _ => panic!("variant changed"),
    }
}

// ---------------------------------------------------------------------------
// msg.delete scope=everyone on an inbound row must fail clean BEFORE the
// tombstone — the local copy is not ours to retract, and the pre-fix code
// path deleted it on its way to the error.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_msg_delete_everyone_rejects_inbound() {
    init_logs();
    let data = tempfile::tempdir().unwrap();
    let core = Core::open(data.path(), None, Identity::generate().unwrap())
        .await
        .expect("core open");

    // An inbound text anchored on a well-formed peer chat.
    let peer = PeerId::from_static_key(&[0x5Au8; 32]);
    let chat = chat_str(&peer);
    core.store()
        .upsert_chat(&chat, peer.as_bytes(), None, false)
        .unwrap();
    core.store()
        .insert_message(&NewMessage {
            msg_id: "msg:inbound".into(),
            chat_id: chat.clone(),
            dir: Dir::In,
            kind: MsgKind::Text,
            body: "theirs".into(),
            state: Some(MsgState::Delivered),
            client_msg_id: None,
            ts: 1000,
            expires_at: None,
        })
        .unwrap();

    let e = core
        .msg_delete(MsgDeleteParams {
            msg_id: "msg:inbound".into(),
            scope: Some("everyone".into()),
            client_op_id: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Conflict);
    assert!(
        !core
            .store()
            .get_message("msg:inbound")
            .unwrap()
            .unwrap()
            .deleted,
        "failed delete must not tombstone the row"
    );

    // scope=local stays allowed on inbound rows.
    core.msg_delete(MsgDeleteParams {
        msg_id: "msg:inbound".into(),
        scope: Some("local".into()),
        client_op_id: None,
    })
    .await
    .expect("local delete");
    assert!(
        core.store()
            .get_message("msg:inbound")
            .unwrap()
            .unwrap()
            .deleted
    );
    stop(&core).await;
}

// ---------------------------------------------------------------------------
// Dedup replay must return the STORED row's deadline, not a re-computed
// one: the same client_msg_id resent with a different (or no) TTL keeps the
// original expires_at — otherwise the deduped row would silently outlive or
// die early relative to what the first send persisted.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_send_text_dedup_preserves_deadline() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();

    let first = a
        .msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: "ttl me".into(),
            client_msg_id: "dedup-ttl".into(),
            ttl_ms: Some(60_000),
        })
        .await
        .expect("first send");
    let stored = first["expires_at"].clone();
    assert!(!stored.is_null(), "ttl send must persist a deadline");

    // Replay: same client_msg_id, no TTL this time. The stored deadline
    // must come back — a `null` here means the dedup lost the expiry.
    let replay = a
        .msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: "ttl me".into(),
            client_msg_id: "dedup-ttl".into(),
            ttl_ms: None,
        })
        .await
        .expect("dedup replay");
    assert_eq!(replay["msg_id"], first["msg_id"], "dedup must reuse msg_id");
    assert_eq!(
        replay["expires_at"], stored,
        "deduped row must keep the stored deadline"
    );
}

// ---------------------------------------------------------------------------
// file.send rejects a payload whose chunk count exceeds the negotiated cap
// BEFORE the file row lands — the receiver would refuse the offer anyway.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn audit_file_send_chunk_cap() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_b = b.identity().peer_id();

    // Two chunks max; the payload needs three.
    a.test_caps_mut().max_chunks = 2;
    let chunk = a.test_caps_mut().chunk_bytes as usize;
    let payload = vec![0xABu8; chunk * 3];
    let e = a
        .file_send(FileSendParams {
            to: contact_str(&peer_b),
            path: None,
            data: Some(data_encoding::BASE64.encode(&payload)),
            name: Some("too-many-chunks.bin".into()),
            mime: None,
            kind: None,
            audio: None,
            caption: None,
            client_xfer_id: "xfer-cap".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        e.kind,
        ErrorKind::InvalidParams,
        "over-cap chunk count must fail the send"
    );
    let files = a.store().list_files(None, None, 50, 0).unwrap_or_default();
    assert!(
        files.iter().all(|f| f.name != "too-many-chunks.bin"),
        "rejected send must not leave a file row"
    );
}
