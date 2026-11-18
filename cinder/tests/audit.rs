//! Regressions for the code-audit findings: mailbox-drain deadlock, peer
//! mutation scoping, contact-add wedge, inbound gating, offline queue
//! flush, react gating, and unread marking.

#[path = "../src/rpc/mod.rs"]
mod rpc;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/tui/mod.rs"]
mod tui;
#[path = "../src/core.rs"]
mod core;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cinder_core::identity::Identity;
use cinder_core::proto::PeerId;
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::net::TcpListener;

use crate::core::{chat_str, contact_str, AppMsg, Core, MetaState};
use crate::rpc::core::{
    ChatHistoryParams, ChatListParams, CinderCore, ContactIdParams, MsgReactParams, MsgSendParams,
};
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
        let _ =
            cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
                .await;
    });
    addr
}

async fn open_core(dir: &std::path::Path, relay: SocketAddr) -> Arc<Core> {
    Core::open(dir, Some((relay, relay.to_string())), Identity::generate().unwrap())
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
    assert!(a.wait_connected(CONNECT_WAIT).await, "A never reached the relay");
    assert!(b.wait_connected(CONNECT_WAIT).await, "B never reached the relay");

    let (link, _) = a.invite_link(3600).await.expect("invite.create");
    let peer_a = b.clone().start_add(&link, None).await.expect("invite.accept");
    assert!(
        b.wait_meta(&peer_a, MetaState::Active, HANDSHAKE_WAIT).await,
        "B never went active with A"
    );
    let peer_b = b.identity().peer_id();
    assert!(
        a.wait_meta(&peer_b, MetaState::PendingIn, HANDSHAKE_WAIT).await,
        "A never saw B's inbound request"
    );
    a.contact_accept(ContactIdParams {
        contact_id: contact_str(&peer_b),
    })
    .await
    .expect("contact.accept");
    assert!(
        a.wait_meta(&peer_b, MetaState::Active, HANDSHAKE_WAIT).await,
        "A never went active with B"
    );
    (a, b)
}

/// Kill a core's link/tasks. The pump holds an Arc so `drop(core)` is not
/// enough — begin_shutdown is what actually closes the TCP connection.
async fn stop(core: &Arc<Core>) {
    core.begin_shutdown();
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < Duration::from_secs(10) {
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
    })
    .unwrap();

    // Legit: peer edits its own inbound message in its own chat.
    s.apply_peer_edit("msg:in", "chat:a", "edited by peer").unwrap();
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
    assert!(unread > 0, "precondition: chat must be unread before history");

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
    let h = b2
        .chat_history(ChatHistoryParams {
            chat_id: chat_str(&peer_a),
            before: None,
            limit: Some(50),
        })
        .await
        .unwrap();
    assert!(
        h["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["body"]["text"] != deposited),
        "undecryptable old-session deposit must not land in history"
    );

    // The drain completed (no op_lock deadlock): a fresh B2→A2 handshake
    // works end-to-end, proving the pump survived the drain+ack.
    let after = format!("post-drain-{}", std::process::id());
    b2.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: after.clone(),
        client_msg_id: "post-1".into(),
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
    // and the message lands.
    let b2 = reopen_core(dir_b.path(), relay, &blob_b).await;
    assert!(b2.wait_connected(CONNECT_WAIT).await);
    wait_for_body(
        &b2,
        &chat_str(&peer_a),
        &queued_body,
        HANDSHAKE_WAIT + DELIVERY_WAIT,
    )
    .await;
    wait_for(
        || (a2.test_pending_len(&peer_b) == 0).then_some(()),
        Duration::from_secs(15),
        "pending_out to drain",
    )
    .await;
}
