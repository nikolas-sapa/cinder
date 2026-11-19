#![allow(dead_code)] // #[path] includes expose items other tests use

//! `contact.deny` + `chat.wipe` — compromised-peer containment.
//! Store-level denylist persistence, the full deny lane-closure (sessions,
//! handshake, transport, mailbox, re-add, send), restart survival, and the
//! two-sided chat destruction incl. forgery cases.

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

use cinder_core::identity::Identity;
use cinder_core::proto::PeerId;
use cinder_core::session::Session;
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::core::{chat_str, contact_str, AppMsg, Core, MetaState};
use crate::rpc::core::{
    ChatHistoryParams, ChatWipeParams, CinderCore, ContactAddParams, ContactDenyParams,
    ContactIdParams, ContactListParams, MsgSendParams,
};
use crate::rpc::protocol::ErrorKind;
use crate::store::{Dir, MsgKind, MsgState, NewFile, NewMessage, Store};
use crate::tui::backend as tb;

const CONNECT_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(25);
const DELIVERY_WAIT: Duration = Duration::from_secs(20);

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "revoke=debug".into()),
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

async fn send_text(core: &Arc<Core>, to: String, body: &str, key: &str) -> String {
    let r = core
        .msg_send(MsgSendParams {
            to,
            body: body.to_string(),
            client_msg_id: key.to_string(),
            ttl_ms: None,
        })
        .await
        .expect("msg.send");
    r["msg_id"].as_str().unwrap().to_string()
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

/// Attach the production replay sink so emitted events land in event_log
/// (mirrors the e2e wiring; in-process open does not attach one itself).
async fn attach_replay(core: &Arc<Core>) {
    let log = crate::core::SqliteReplayLog::open(core.store().clone(), 16_384)
        .await
        .unwrap();
    core.attach_sink(Arc::new(crate::rpc::events::EventManager::new(
        Box::new(log),
        1024,
    )));
}

fn events_of(core: &Arc<Core>, kind: &str) -> Vec<Value> {
    core.store()
        .events_since(0, 4096)
        .map(|p| {
            p.events
                .iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.payload.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Store: the denylist is schema-v4, idempotent, and survives a reopen.
// ---------------------------------------------------------------------------
#[test]
fn deny_list_store_persists() {
    let dir = tempfile::tempdir().unwrap();
    let key = [0x77u8; 32];
    {
        let s = Store::open(dir.path(), &key).unwrap();
        assert!(s.denied_peers().unwrap().is_empty());
        let peer = [9u8; 32];
        s.deny_peer(&peer).unwrap();
        // Idempotent — a panic button must never conflict on repeat.
        s.deny_peer(&peer).unwrap();
        s.deny_peer(&[3u8; 32]).unwrap();
        let mut ids = s.denied_peers().unwrap();
        ids.sort();
        assert_eq!(ids, vec![[3u8; 32], [9u8; 32]]);
    }
    {
        let s = Store::open(dir.path(), &key).unwrap();
        assert_eq!(
            s.denied_peers().unwrap().len(),
            2,
            "denylist must survive reopen"
        );
    }
}

// ---------------------------------------------------------------------------
// Store: wipe_chat hard-deletes every per-chat row and leaves others alone.
// ---------------------------------------------------------------------------
#[test]
fn wipe_chat_store_full_delete() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), &[0x55u8; 32]).unwrap();
    let peer = [1u8; 32];
    let other = [2u8; 32];
    s.upsert_chat("chat:dead", &peer, None, false).unwrap();
    s.upsert_chat("chat:lives", &other, None, false).unwrap();
    for (id, dir_flag, body) in [
        ("msg:in1", Dir::In, "peer text"),
        ("msg:out1", Dir::Out, "our text"),
    ] {
        s.insert_message(&NewMessage {
            msg_id: id.into(),
            chat_id: "chat:dead".into(),
            dir: dir_flag,
            kind: MsgKind::Text,
            body: body.into(),
            state: Some(MsgState::Delivered),
            client_msg_id: Some(format!("cm-{id}")),
            ts: 1000,
            expires_at: None,
        })
        .unwrap();
    }
    s.add_reaction("msg:in1", "x", &peer).unwrap();
    s.insert_file(&NewFile {
        file_id: "file:dead".into(),
        msg_id: "msg:in1".into(),
        chat_id: "chat:dead".into(),
        dir: Dir::In,
        name: "loot.bin".into(),
        mime: "application/octet-stream".into(),
        size: 4,
        sha256: vec![0u8; 32],
        chunk_size: 65536,
        chunks_total: 1,
        state: "transferring".into(),
        kind: "file".into(),
        thumb: None,
        audio_meta: None,
        fetch_token: None,
        caption: None,
        staged_path: Some("xfer/deadbeef.part".into()),
        client_xfer_id: None,
    })
    .unwrap();
    s.append_event(
        "message.received",
        Some("chat:dead"),
        &json!({"chat_id": "chat:dead", "message": {"body": {"text": "secret"}}}),
    )
    .unwrap();
    s.append_event("contact.connected", None, &json!({"peer_id": "peer:x"}))
        .unwrap();
    s.insert_message(&NewMessage {
        msg_id: "msg:keep".into(),
        chat_id: "chat:lives".into(),
        dir: Dir::In,
        kind: MsgKind::Text,
        body: "keep me".into(),
        state: Some(MsgState::Delivered),
        client_msg_id: None,
        ts: 1001,
        expires_at: None,
    })
    .unwrap();

    let (existed, staged) = s.wipe_chat("chat:dead").unwrap();
    assert!(existed);
    assert_eq!(staged, vec!["xfer/deadbeef.part".to_string()]);
    assert!(!s.chat_exists("chat:dead").unwrap());
    assert!(s.get_message("msg:in1").unwrap().is_none());
    assert!(s.get_message("msg:out1").unwrap().is_none());
    assert!(s.get_file("file:dead").unwrap().is_none());
    // Replay-log rows for the chat die with it; unrelated events survive.
    let remaining = s.events_since(0, 100).unwrap().events;
    assert!(
        remaining
            .iter()
            .all(|e| e.chat_id.as_deref() != Some("chat:dead")),
        "event_log rows for the wiped chat must be gone"
    );
    assert!(remaining.iter().any(|e| e.kind == "contact.connected"));
    // Neighbour chat + its rows are untouched.
    assert!(s.chat_exists("chat:lives").unwrap());
    assert_eq!(s.get_message("msg:keep").unwrap().unwrap().body, "keep me");
    // Wipe of an unknown chat is a no-op, not an error.
    let (existed, staged) = s.wipe_chat("chat:nope").unwrap();
    assert!(!existed && staged.is_empty());
}

// ---------------------------------------------------------------------------
// Deny closes every lane: session, send, re-add, accept, transport frames.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn deny_closes_every_lane() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    attach_replay(&b).await;
    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let blob_a = a.identity().stored_blob().expose_secret().to_vec();
    assert!(b.test_has_session(&peer_a));

    // Queue a message behind an offline presence so the deny has backlog
    // to kill. Take A REALLY offline first: merely injecting presence would
    // be overturned by the next truthful broadcast (A is online) and the
    // live flush would beat the deny. With A's link dead, offline + grant
    // = mailbox deposit, and pending_out retains it until the deny lands.
    wait_for(
        || b.test_has_grant(&peer_a).then_some(()),
        DELIVERY_WAIT,
        "mailbox grant for A",
    )
    .await;
    stop(&a).await;
    b.test_set_presence(&peer_a, false);
    let queued_id = send_text(&b, contact_str(&peer_a), "queued-1", "q-1").await;
    wait_for(
        || (b.test_pending_len(&peer_a) > 0).then_some(()),
        Duration::from_secs(10),
        "outbound item parked in pending_out",
    )
    .await;

    let r = b
        .contact_deny(ContactDenyParams {
            contact_id: contact_str(&peer_a),
        })
        .await
        .expect("contact.deny");
    assert_eq!(r["state"], "denied");
    assert!(r["peer_id"].as_str().unwrap().len() > 5);
    assert!(b.test_denied(&peer_a));
    assert!(!b.test_has_session(&peer_a), "deny must kill the session");

    // Backlog transitioned to failed; the queue itself is gone.
    assert_eq!(b.test_pending_len(&peer_a), 0, "deny drains pending_out");
    let row = b.store().get_message(&queued_id).unwrap().unwrap();
    assert_eq!(row.state, Some(MsgState::Failed));
    wait_for(
        || {
            events_of(&b, "msg_state")
                .iter()
                .any(|p| p["msg_id"] == queued_id && p["reason"] == "denied")
                .then_some(())
        },
        Duration::from_secs(10),
        "msg_state{failed, reason:denied}",
    )
    .await;

    // contact.denied emitted.
    assert!(
        events_of(&b, "contact.denied")
            .iter()
            .any(|p| p["contact_id"] == contact_str(&peer_a)),
        "contact.denied must emit"
    );

    // Every write path refuses with peer_denied.
    let e = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: "nope".into(),
            client_msg_id: "post-deny-1".into(),
            ttl_ms: None,
        })
        .await
        .expect_err("send to denied peer must fail");
    assert_eq!(e.kind, ErrorKind::PeerDenied);

    let link = a.identity().uri(&relay.to_string(), None);
    let e = b
        .contact_add(ContactAddParams {
            link: link.clone(),
            display_name: None,
        })
        .await
        .expect_err("re-add of denied peer must fail");
    assert_eq!(e.kind, ErrorKind::PeerDenied);
    let e = b
        .start_add(&link, None)
        .await
        .expect_err("invite.accept path (start_add) must refuse denied peer");
    assert_eq!(e.kind, ErrorKind::PeerDenied);
    let e = b
        .contact_accept(ContactIdParams {
            contact_id: contact_str(&peer_a),
        })
        .await
        .expect_err("accept of denied peer must fail");
    assert_eq!(e.kind, ErrorKind::PeerDenied);

    // A reopens unaware of the deny: its send must handshake first (B
    // dropped the session), so the Msg1 itself is the denied inbound
    // frame — the responder gate drops it before any app handling.
    let a = reopen_core(dir_a.path(), relay, &blob_a).await;
    assert!(a.wait_connected(CONNECT_WAIT).await);
    let drops_before = b.test_denied_drops();
    send_text(&a, contact_str(&peer_b), "post-deny wire", "pd-1").await;
    wait_for(
        || (b.test_denied_drops() > drops_before).then_some(()),
        DELIVERY_WAIT,
        "denied peer's transport frame dropped",
    )
    .await;
    let hist = b
        .chat_history(ChatHistoryParams {
            chat_id: chat_str(&peer_a),
            before: None,
            limit: Some(50),
        })
        .await
        .unwrap();
    assert!(
        !hist["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"]["text"] == "post-deny wire"),
        "denied peer's frame must never reach history"
    );

    // Deny is idempotent — the panic button does not conflict on repeat.
    let r = b
        .contact_deny(ContactDenyParams {
            contact_id: contact_str(&peer_a),
        })
        .await
        .expect("second deny");
    assert_eq!(r["state"], "denied");

    // And the contact reports denied.
    let cl = b
        .contact_list(ContactListParams {
            state: Some("denied".into()),
        })
        .await
        .unwrap();
    assert!(
        cl["contacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["contact_id"] == contact_str(&peer_a)),
        "contact.list must surface the denied record"
    );

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// install_session refuses denied peers before any sid binding; the inbound
// responder gate also refuses a fresh Msg1.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn denied_peer_session_never_binds() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_a = a.identity().peer_id();
    b.contact_deny(ContactDenyParams {
        contact_id: contact_str(&peer_a),
    })
    .await
    .expect("deny");

    // A driven XX from A's side still produces a finished responder
    // session for B — install_session must refuse it pre-sid_index.
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
    let peer_b = b.identity().peer_id();
    let mut init = Session::initiator(a.identity()).unwrap();
    let resp = drive_xx(b.identity(), &peer_b, &mut init);
    assert!(
        !b.test_install_session(peer_a, resp),
        "denied peer must never bind a session"
    );
    assert!(!b.test_has_session(&peer_a));

    // A real re-handshake over the wire also dies: A's Msg1 hits the
    // responder gate and is dropped — no session ever materializes.
    a.test_drop_session(&peer_b);
    let ghost = format!("ghost-{}", std::process::id());
    let _ = a
        .msg_send(MsgSendParams {
            to: contact_str(&peer_b),
            body: ghost.clone(),
            client_msg_id: "ghost-1".into(),
            ttl_ms: None,
        })
        .await;
    // Give A's handshake attempts a real window — B must never install.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !b.test_has_session(&peer_a),
        "denied peer's Msg1s must not resurrect a session"
    );

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// The denylist survives a restart: loaded at open, enforced identically.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn deny_survives_restart() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_a = a.identity().peer_id();
    let blob = b.identity().stored_blob().expose_secret().to_vec();

    b.contact_deny(ContactDenyParams {
        contact_id: contact_str(&peer_a),
    })
    .await
    .expect("deny");
    stop(&b).await;
    let b = reopen_core(dir_b.path(), relay, &blob).await;
    assert!(b.wait_connected(CONNECT_WAIT).await);
    assert!(b.test_denied(&peer_a), "denylist must reload at open");

    let link = a.identity().uri(&relay.to_string(), None);
    let e = b
        .contact_add(ContactAddParams {
            link,
            display_name: None,
        })
        .await
        .expect_err("re-add after restart must fail");
    assert_eq!(e.kind, ErrorKind::PeerDenied);
    assert!(!b.test_has_session(&peer_a));

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// chat.wipe: both sides lose the chat when a live session carries the notice.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chat_wipe_e2e_both_sides() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    attach_replay(&a).await;
    attach_replay(&b).await;
    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let chat_on_a = chat_str(&peer_b);
    let chat_on_b = chat_str(&peer_a);

    send_text(&b, contact_str(&peer_a), "wipe-me-1", "wm-1").await;
    wait_for_body(&a, &chat_on_a, "wipe-me-1", DELIVERY_WAIT).await;
    send_text(&a, contact_str(&peer_b), "wipe-me-2", "wm-2").await;
    wait_for_body(&b, &chat_on_b, "wipe-me-2", DELIVERY_WAIT).await;

    let r = b
        .chat_wipe(ChatWipeParams {
            chat_id: chat_on_b.clone(),
        })
        .await
        .expect("chat.wipe");
    assert_eq!(r["wiped"], true);
    assert_eq!(r["peer_notified"], true, "live session must carry the wipe");

    // Local: every row gone on B.
    assert!(!b.store().chat_exists(&chat_on_b).unwrap());
    let hist = b
        .chat_history(ChatHistoryParams {
            chat_id: chat_on_b.clone(),
            before: None,
            limit: Some(50),
        })
        .await
        .unwrap();
    assert!(hist["messages"].as_array().unwrap().is_empty());
    assert!(
        events_of(&b, "chat.wiped")
            .iter()
            .any(|p| p["chat_id"] == chat_on_b && p["by"] == "local"),
        "chat.wiped by:local must emit on B"
    );

    // Remote: the ChatWipe frame lands on A and erases its copy identically.
    wait_for(
        || (!a.store().chat_exists(&chat_on_a).unwrap()).then_some(()),
        DELIVERY_WAIT,
        "A's copy of the chat wiped",
    )
    .await;
    wait_for(
        || {
            events_of(&a, "chat.wiped")
                .iter()
                .any(|p| p["chat_id"] == chat_on_a && p["by"] == "peer")
                .then_some(())
        },
        Duration::from_secs(10),
        "chat.wiped{by:peer} on A",
    )
    .await;
    let hist_a = a
        .chat_history(ChatHistoryParams {
            chat_id: chat_on_a.clone(),
            before: None,
            limit: Some(50),
        })
        .await
        .unwrap();
    assert!(
        hist_a["messages"].as_array().unwrap().is_empty(),
        "A's history must be gone post-wipe"
    );

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// No live session → no notice, but the local wipe is unconditional. The
// peer's copy survives (best-effort is honest, not a guarantee).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chat_wipe_offline_peer_local_only() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let chat_on_b = chat_str(&peer_a);
    send_text(&b, contact_str(&peer_a), "hello", "h-1").await;
    wait_for_body(&a, &chat_str(&peer_b), "hello", DELIVERY_WAIT).await;

    b.test_drop_session(&peer_a);
    let r = b
        .chat_wipe(ChatWipeParams {
            chat_id: chat_on_b.clone(),
        })
        .await
        .expect("chat.wipe offline");
    assert_eq!(r["wiped"], true);
    assert_eq!(
        r["peer_notified"], false,
        "no session → nothing sent; result must admit it"
    );
    assert!(!b.store().chat_exists(&chat_on_b).unwrap());
    // A kept its copy — remote destruction is never claimed.
    assert!(a.store().chat_exists(&chat_str(&peer_b)).unwrap());

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// A wipe naming a chat that isn't the sender's is a forgery — dropped, and
// the named chat survives.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn wipe_foreign_chat_is_forgery() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    // Second peer C onto B.
    let c = open_core(dir_c.path(), relay).await;
    assert!(c.wait_connected(CONNECT_WAIT).await);
    let (link_c, _) = c.invite_link(3600).await.expect("invite");
    let peer_c = b.clone().start_add(&link_c, None).await.expect("add C");
    assert!(
        b.wait_meta(&peer_c, MetaState::Active, HANDSHAKE_WAIT)
            .await
    );
    let peer_b = b.identity().peer_id();
    assert!(
        c.wait_meta(&peer_b, MetaState::PendingIn, HANDSHAKE_WAIT)
            .await
    );
    c.contact_accept(ContactIdParams {
        contact_id: contact_str(&peer_b),
    })
    .await
    .expect("accept");
    assert!(
        c.wait_meta(&peer_b, MetaState::Active, HANDSHAKE_WAIT)
            .await
    );

    send_text(&b, contact_str(&peer_c), "c-chat", "cc-1").await;
    wait_for_body(&c, &chat_str(&peer_b), "c-chat", DELIVERY_WAIT).await;
    let chat_bc = chat_str(&peer_c);
    assert!(b.store().chat_exists(&chat_bc).unwrap());

    // A wipes "the A<->B chat"… except the frame names B<->C's chat.
    let peer_a = a.identity().peer_id();
    let drops = b.test_dropped_inbound();
    b.test_handle_app(
        peer_a,
        AppMsg::ChatWipe {
            chat_id: chat_bc.clone(),
        },
    )
    .await;
    assert!(b.test_dropped_inbound() > drops, "foreign wipe must drop");
    assert!(
        b.store().chat_exists(&chat_bc).unwrap(),
        "C's chat must survive A's forged wipe"
    );
    // A's own chat is also untouched — the forgery was a no-op.
    assert!(b.store().chat_exists(&chat_str(&peer_a)).unwrap());

    stop(&a).await;
    stop(&b).await;
    stop(&c).await;
}

// ---------------------------------------------------------------------------
// A denied peer's wipe is a hostile destructive command — the deny gate
// drops it before it can erase our copy.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn denied_peer_cannot_wipe_us() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let peer_a = a.identity().peer_id();
    let chat = chat_str(&peer_a);
    send_text(&b, contact_str(&peer_a), "evidence", "ev-1").await;
    wait_for_body(
        &a,
        &chat_str(&b.identity().peer_id()),
        "evidence",
        DELIVERY_WAIT,
    )
    .await;

    b.contact_deny(ContactDenyParams {
        contact_id: contact_str(&peer_a),
    })
    .await
    .expect("deny");
    let drops = b.test_denied_drops();
    // The hostile endpoint fires a wipe at the shared chat — dropped unread.
    b.test_handle_app(
        peer_a,
        AppMsg::ChatWipe {
            chat_id: chat.clone(),
        },
    )
    .await;
    assert!(b.test_denied_drops() > drops, "denied wipe must drop");
    assert!(
        b.store().chat_exists(&chat).unwrap(),
        "a denied peer can never delete our copy"
    );

    stop(&a).await;
    stop(&b).await;
}

// ---------------------------------------------------------------------------
// Edge cases: unknown ids are not_found, malformed ids invalid_params.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wipe_and_deny_unknowns() {
    init_logs();
    let relay = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let c = open_core(dir.path(), relay).await;
    assert!(c.wait_connected(CONNECT_WAIT).await);

    // Well-formed ids that name nothing are not_found; gibberish that fails
    // the id decoder is invalid_params — the two error lanes stay distinct.
    let ghost = PeerId::from_static_key(&[0xabu8; 32]);
    let e = c
        .chat_wipe(ChatWipeParams {
            chat_id: chat_str(&ghost),
        })
        .await
        .expect_err("unknown chat");
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = c
        .contact_deny(ContactDenyParams {
            contact_id: contact_str(&ghost),
        })
        .await
        .expect_err("unknown contact");
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = c
        .contact_deny(ContactDenyParams {
            contact_id: "contact:not!b32".into(),
        })
        .await
        .expect_err("malformed contact_id");
    assert_eq!(e.kind, ErrorKind::InvalidParams);

    stop(&c).await;
}
