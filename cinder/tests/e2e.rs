#![allow(dead_code)] // #[path] includes expose items other tests use

//! End-to-end: real relay, real Core, real Noise — plus a wire tap proving
//! the relay never sees plaintext, and a full `cinder rpc` subprocess
//! round-trip.

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

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cinder_core::identity::Identity;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::core::{contact_str, Core, MetaState};
use crate::rpc::core::{
    ChatHistoryParams, ChatSetDisappearingParams, CinderCore, ContactIdParams, FileIdParams,
    FileSaveParams, FileSendParams, MsgSendParams,
};

const CONNECT_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const DELIVERY_WAIT: Duration = Duration::from_secs(20);

fn relay_hostport(addr: SocketAddr) -> (SocketAddr, String) {
    (addr, addr.to_string())
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "e2e=debug".into()),
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
        Some(relay_hostport(relay)),
        Identity::generate().unwrap(),
    )
    .await
    .expect("core open")
}

/// Connect two fresh identities through `relay` (invite out, accept in) and
/// return both cores once both sides report `active`.
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
    // Inbound side lands pending_in + contact.request per spec; accept it.
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

/// Poll `chat_history` on `core` for `chat_id` until a body matching `text`
/// appears; returns the message JSON.
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// 1. in-process round-trip: two Cores, one relay, encrypted send both ways.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_cores_roundtrip_via_relay() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;

    let marker = format!("e2e-marker-{}", std::process::id());
    let peer_a = a.identity().peer_id();
    let r = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: marker.clone(),
            client_msg_id: "e2e-cmid-1".into(),
            ttl_ms: None,
        })
        .await
        .expect("msg.send");
    assert!(r["msg_id"].is_string() && r["chat_id"].is_string());

    let peer_b = b.identity().peer_id();
    let m = wait_for_body(&a, &crate::core::chat_str(&peer_b), &marker, DELIVERY_WAIT).await;
    assert_eq!(m["dir"], "in");
    assert_eq!(m["body"]["trusted"], false, "inbound must be untrusted");

    // And back the other way.
    let reply = "e2e-reply-pong";
    a.msg_send(MsgSendParams {
        to: contact_str(&peer_b),
        body: reply.into(),
        client_msg_id: "e2e-cmid-2".into(),
        ttl_ms: None,
    })
    .await
    .expect("msg.send reply");
    wait_for_body(&b, &crate::core::chat_str(&peer_a), reply, DELIVERY_WAIT).await;
}

// ---------------------------------------------------------------------------
// 2. relay blindness: a TCP tap between clients and relay records every byte
//    on the wire; the plaintext marker must never appear, while a raw canary
//    written through the same tap proves capture actually works.
// ---------------------------------------------------------------------------
async fn spawn_tap(target: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cap = Arc::new(Mutex::new(Vec::new()));
    let cap_loop = cap.clone();
    tokio::spawn(async move {
        loop {
            let Ok((inbound, _)) = listener.accept().await else {
                break;
            };
            let Ok(outbound) = TcpStream::connect(target).await else {
                continue;
            };
            let cap = cap_loop.clone();
            tokio::spawn(async move {
                let (mut ri, mut wi) = inbound.into_split();
                let (mut ro, mut wo) = outbound.into_split();
                let cap_up = cap.clone();
                let cap_down = cap.clone();
                let up = tokio::spawn(async move {
                    let mut buf = [0u8; 16384];
                    loop {
                        match ri.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                cap_up.lock().unwrap().extend_from_slice(&buf[..n]);
                                if wo.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                let down = tokio::spawn(async move {
                    let mut buf = [0u8; 16384];
                    loop {
                        match ro.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                cap_down.lock().unwrap().extend_from_slice(&buf[..n]);
                                if wi.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                let _ = up.await;
                let _ = down.await;
            });
        }
    });
    (addr, cap)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn relay_never_sees_plaintext() {
    init_logs();
    let relay = spawn_relay().await;
    let (tap, cap) = spawn_tap(relay).await;

    // Canary: raw bytes through the tap must show up in the capture —
    // proves the tap isn't blind itself.
    let canary = b"CANARY-plaintext-checkbytes-0123456789";
    {
        let mut s = TcpStream::connect(tap).await.unwrap();
        s.write_all(canary).await.unwrap();
        s.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    {
        let seen = cap.lock().unwrap();
        assert!(
            seen.windows(canary.len()).any(|w| w == canary.as_slice()),
            "tap failed to capture a raw canary; blindness proof is void"
        );
    }

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    // Both clients point at the tap: all of their wire bytes are captured.
    let (a, b) = paired(tap, dir_a.path(), dir_b.path()).await;

    let marker = "THE_SECRET_PLAINTEXT_must-not-appear-on-wire";
    let peer_a = a.identity().peer_id();
    b.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: marker.into(),
        client_msg_id: "e2e-tap-1".into(),
        ttl_ms: None,
    })
    .await
    .expect("msg.send");

    let peer_b = b.identity().peer_id();
    wait_for_body(&a, &crate::core::chat_str(&peer_b), marker, DELIVERY_WAIT).await;

    // Both directions exercised; give the tap a beat to flush captures.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = cap.lock().unwrap();
    assert!(!seen.is_empty(), "tap captured nothing — no traffic flowed");
    assert!(
        !seen.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "plaintext leaked onto the wire ({} captured bytes)",
        seen.len()
    );
    // And the identity public keys / peer ids are base32-on-the-wire only in
    // handshake context; the marker assertion above is the core guarantee.
}

// ---------------------------------------------------------------------------
// 3. subprocess `cinder rpc` round-trip: two OS processes, newline JSON-RPC
//    on stdio, invite → accept → send → message.received on stdout.
// ---------------------------------------------------------------------------
struct RpcChild {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    /// Frames consumed by a `wait_for` that did not match its predicate —
    /// events interleave with responses on the wire, so a later wait must be
    /// able to re-examine them.
    backlog: std::collections::VecDeque<Value>,
}

impl RpcChild {
    fn spawn(dir: &std::path::Path, relay: SocketAddr) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cinder"))
            .args([
                "--data-dir",
                dir.to_str().unwrap(),
                "--relay",
                &relay.to_string(),
                "rpc",
            ])
            .env("CINDER_IDENTITY", "ephemeral") // no keychain in tests
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn cinder rpc");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
            backlog: std::collections::VecDeque::new(),
        }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{}", v).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Wait for a frame matching `pred` (id response or event method).
    /// Non-matching frames are parked in `backlog` so event frames consumed
    /// while awaiting a response stay visible to later waits.
    fn wait_for(&mut self, pred: impl Fn(&Value) -> bool, dur: Duration) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i).unwrap();
        }
        let deadline = std::time::Instant::now() + dur;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "timed out waiting for rpc frame");
            match self.lines.recv_timeout(left.min(Duration::from_secs(1))) {
                Ok(v) if pred(&v) => return v,
                Ok(v) => {
                    self.backlog.push_back(v);
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("rpc stdout closed: {e}"),
            }
        }
    }
}

impl Drop for RpcChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn rpc_subprocess_roundtrip() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    let relay = rt.block_on(spawn_relay());

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = RpcChild::spawn(dir_a.path(), relay);
    let mut b = RpcChild::spawn(dir_b.path(), relay);

    let hello = json!({"jsonrpc":"2.0","id":1,"method":"session.hello",
        "params":{"protocol_version":1,"capabilities_requested":["*"]}});
    a.send(hello.clone());
    b.send(hello);
    a.wait_for(|v| v["id"] == 1 && v["result"].is_object(), CONNECT_WAIT);
    b.wait_for(|v| v["id"] == 1 && v["result"].is_object(), CONNECT_WAIT);

    // B subscribes for the contact.connected event; A for the inbound
    // request + message.received.
    b.send(json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe",
        "params":{"kinds":["contact.connected"]}}));
    a.send(json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe",
        "params":{"kinds":["message.received","contact.connected","contact.request"]}}));
    b.wait_for(|v| v["id"] == 2 && v["result"].is_object(), CONNECT_WAIT);
    a.wait_for(|v| v["id"] == 2 && v["result"].is_object(), CONNECT_WAIT);

    // A mints an invite; B accepts it.
    a.send(json!({"jsonrpc":"2.0","id":3,"method":"invite.create",
        "params":{"ttl_sec":3600}}));
    let inv = a.wait_for(|v| v["id"] == 3, CONNECT_WAIT);
    let link = inv["result"]["link"]
        .as_str()
        .expect("invite.create must return link")
        .to_string();

    b.send(json!({"jsonrpc":"2.0","id":4,"method":"invite.accept",
        "params":{"link": link}}));
    let acc = b.wait_for(|v| v["id"] == 4, HANDSHAKE_WAIT);
    let contact_id = acc["result"]["contact_id"]
        .as_str()
        .expect("invite.accept must return contact_id")
        .to_string();

    // B (initiator) goes straight to contact.connected; A lands pending_in
    // and emits contact.request — accept it to complete the pair.
    b.wait_for(|v| v["method"] == "contact.connected", HANDSHAKE_WAIT);
    let req = a.wait_for(|v| v["method"] == "contact.request", HANDSHAKE_WAIT);
    let req_id = req["params"]["contact_id"].as_str().unwrap().to_string();
    a.send(json!({"jsonrpc":"2.0","id":6,"method":"contact.accept",
        "params":{"contact_id": req_id}}));
    a.wait_for(|v| v["id"] == 6 && v["result"].is_object(), HANDSHAKE_WAIT);
    a.wait_for(|v| v["method"] == "contact.connected", HANDSHAKE_WAIT);

    // B → A send; A must see message.received with trusted:false.
    let marker = format!("rpc-e2e-{}", std::process::id());
    b.send(json!({"jsonrpc":"2.0","id":5,"method":"msg.send",
        "params":{"to": contact_id, "body": marker, "client_msg_id": "rpc-e2e-cmid"}}));
    let sent = b.wait_for(|v| v["id"] == 5, DELIVERY_WAIT);
    assert!(sent["result"].is_object(), "msg.send failed: {sent}");

    let evt = a.wait_for(
        |v| v["method"] == "message.received" && v["params"]["message"]["body"]["text"] == marker,
        DELIVERY_WAIT,
    );
    assert_eq!(
        evt["params"]["message"]["body"]["trusted"], false,
        "inbound body must be untrusted"
    );
}

// ---------------------------------------------------------------------------
// 4. disappearing messages: the wire TTL rides inside the encrypted payload —
//    the receiver persists the deadline, the ~1s sweep tombstones it, and
//    msg.expired lands in the event log while history drops the body.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn disappearing_roundtrip_and_sweep() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    // Production wires SqliteReplayLog → EventManager → attach_sink in
    // build_engine; the in-process open does none of that, so attach it here
    // to observe msg.expired in the persisted event log.
    let log = crate::core::SqliteReplayLog::open(a.store().clone(), 16_384)
        .await
        .unwrap();
    a.attach_sink(Arc::new(crate::rpc::events::EventManager::new(
        Box::new(log),
        1024,
    )));

    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let chat_on_a = crate::core::chat_str(&peer_b);
    let marker = format!("e2e-vanish-{}", std::process::id());
    let sent_at = now_ms();

    let r = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: marker.clone(),
            client_msg_id: "e2e-ttl-1".into(),
            ttl_ms: Some(1_500),
        })
        .await
        .expect("msg.send");
    let msg_id = r["msg_id"].as_str().unwrap().to_string();
    assert!(
        r["expires_at"].is_string(),
        "send result must report its deadline"
    );

    // Live on receipt: the row lands with the peer-supplied deadline.
    let m = wait_for_body(&a, &chat_on_a, &marker, DELIVERY_WAIT).await;
    let exp = m["expires_at_ms"]
        .as_i64()
        .expect("receiver must persist expires_at_ms");
    assert!(exp > sent_at, "deadline must be in the sender's future");

    // Past the deadline the sweep tombstones it: msg.expired in the event
    // log, body cleared, deadline retained for audit, history drops the row.
    wait_for(
        || {
            a.store().events_since(0, 500).ok().and_then(|p| {
                p.events
                    .iter()
                    .any(|e| e.kind == "msg.expired" && e.payload["msg_id"] == msg_id)
                    .then_some(())
            })
        },
        Duration::from_secs(15),
        "msg.expired for the vanishing message",
    )
    .await;

    let tomb = a.store().get_message(&msg_id).unwrap().unwrap();
    assert!(
        tomb.deleted && tomb.body.is_empty(),
        "swept row is a tombstone"
    );
    assert_eq!(tomb.expires_at, Some(exp), "audit keeps the deadline");
    let hist = a
        .chat_history(ChatHistoryParams {
            chat_id: chat_on_a.clone(),
            before: None,
            limit: Some(50),
        })
        .await
        .unwrap();
    assert!(
        hist["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["msg_id"] != msg_id),
        "expired message must not appear in history"
    );
}

// ---------------------------------------------------------------------------
// 5. the per-chat default: chat.set_disappearing on the sender makes a bare
//    msg.send inherit the TTL, while an explicit ttl_ms=0 still opts out.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn disappearing_chat_default_applies() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;

    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let chat_on_b = crate::core::chat_str(&peer_a);
    let chat_on_a = crate::core::chat_str(&peer_b);

    b.chat_set_disappearing(ChatSetDisappearingParams {
        chat_id: chat_on_b.clone(),
        ttl_ms: 300_000,
    })
    .await
    .expect("chat.set_disappearing");

    // No ttl_ms on the call → the chat default (5 min) applies end-to-end.
    let marker = format!("e2e-ttl-default-{}", std::process::id());
    let sent_at = now_ms();
    let r = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: marker.clone(),
            client_msg_id: "e2e-ttl-2".into(),
            ttl_ms: None,
        })
        .await
        .expect("msg.send with chat default");
    assert!(
        r["expires_at"].is_string(),
        "default TTL must produce a deadline"
    );
    let m = wait_for_body(&a, &chat_on_a, &marker, DELIVERY_WAIT).await;
    let exp = m["expires_at_ms"].as_i64().unwrap();
    assert!(
        exp >= sent_at + 299_000 && exp <= sent_at + 301_000,
        "receiver deadline must match the 5-min default, got {exp} (sent {sent_at})"
    );

    // Explicit ttl_ms=0 opts this one message out of the default.
    let keep = format!("e2e-ttl-keep-{}", std::process::id());
    let r = b
        .msg_send(MsgSendParams {
            to: contact_str(&peer_a),
            body: keep.clone(),
            client_msg_id: "e2e-ttl-3".into(),
            ttl_ms: Some(0),
        })
        .await
        .expect("msg.send ttl 0");
    assert!(
        r["expires_at"].is_null(),
        "ttl 0 sends a non-expiring message"
    );
    let m = wait_for_body(&a, &chat_on_a, &keep, DELIVERY_WAIT).await;
    assert!(m["expires_at_ms"].is_null());
}

// ---------------------------------------------------------------------------
// FT10: end-to-end encrypted file round-trip — path send, auto-accept,
// chunked delivery, hash verification, save, byte equality.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn file_roundtrip_via_relay() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;

    let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    let src = dir_b.path().join("photo bin.dat");
    std::fs::write(&src, &payload).unwrap();

    let peer_a = a.identity().peer_id();
    let sent = b
        .file_send(FileSendParams {
            to: contact_str(&peer_a),
            path: Some(src.to_string_lossy().to_string()),
            data: None,
            name: Some("photo bin.dat".into()),
            mime: None,
            kind: None,
            audio: None,
            caption: Some("roundtrip".into()),
            client_xfer_id: "ft10-x1".into(),
        })
        .await
        .expect("file.send");
    let fid = sent["file_id"].as_str().unwrap().to_string();
    assert!(sent["msg_id"].as_str().unwrap().starts_with("msg:"));
    assert_eq!(sent["state"], "offered");
    assert!(sent["chunks"].as_u64().unwrap() >= 1);
    assert_eq!(sent["bytes"], payload.len() as u64);

    let peer_b = b.identity().peer_id();
    let chat_on_a = crate::core::chat_str(&peer_b);

    let done = wait_for(
        || {
            a.store()
                .get_file(&fid)
                .ok()
                .flatten()
                .filter(|r| r.state == "complete")
        },
        Duration::from_secs(60),
        "inbound file completion",
    )
    .await;
    assert_eq!(done.sha256, {
        use sha2::{Digest, Sha256};
        Sha256::digest(&payload).to_vec()
    });

    // Saved bytes identical; name sanitized.
    let outdir = tempfile::tempdir().unwrap();
    let saved = a
        .file_save(FileSaveParams {
            file_id: fid.clone(),
            dir: Some(outdir.path().to_string_lossy().to_string()),
            name: None,
        })
        .await
        .expect("file.save");
    let saved_path = std::path::PathBuf::from(saved["path"].as_str().unwrap());
    assert_eq!(saved["bytes"], payload.len() as u64);
    assert_eq!(std::fs::read(&saved_path).unwrap(), payload);
    let canon_out = std::fs::canonicalize(outdir.path()).unwrap();
    assert_eq!(saved_path.parent(), Some(canon_out.as_path()));

    // file.info on the sender reports complete + correct sha256.
    let info = b
        .file_info(FileIdParams {
            file_id: fid.clone(),
        })
        .await
        .expect("file.info");
    assert_eq!(info["state"], "complete");
    assert_eq!(
        data_encoding::BASE64
            .decode(info["sha256"].as_str().unwrap().as_bytes())
            .unwrap(),
        {
            use sha2::{Digest, Sha256};
            Sha256::digest(&payload).to_vec()
        }
    );
    // chat.history shows the file body, not raw text.
    let hist = a
        .chat_history(ChatHistoryParams {
            chat_id: chat_on_a.clone(),
            before: None,
            limit: Some(20),
        })
        .await
        .expect("chat.history");
    let file_msg = hist["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["body"]["file"]["file_id"] == fid);
    assert!(file_msg.is_some(), "anchor message must carry body.file");
}

// ---------------------------------------------------------------------------
// FT15: relay blindness for file traffic — every byte of the transfer goes
// through the tap; neither filename nor chunk plaintext may appear.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn relay_never_sees_file_plaintext() {
    init_logs();
    let relay = spawn_relay().await;
    let (tap, cap) = spawn_tap(relay).await;

    // Prove the tap sees raw bytes (canary) before claiming blindness.
    let canary = b"TAP-CANARY-ft15-0123456789";
    {
        let mut s = TcpStream::connect(tap).await.unwrap();
        s.write_all(canary).await.unwrap();
        s.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            cap.lock()
                .unwrap()
                .windows(canary.len())
                .any(|w| w == canary.as_slice()),
            "tap failed to capture a canary"
        );
    }

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(tap, dir_a.path(), dir_b.path()).await;

    let content_marker = b"FT15-SECRET-CHUNK-PLAINTEXT-never-on-wire";
    let name_marker = "FT15-SECRET-FILENAME.bin";
    let mut payload = vec![0u8; 40_000];
    payload[12345..12345 + content_marker.len()].copy_from_slice(content_marker);
    let src = dir_b.path().join(name_marker);
    std::fs::write(&src, &payload).unwrap();

    let peer_a = a.identity().peer_id();
    let sent = b
        .file_send(FileSendParams {
            to: contact_str(&peer_a),
            path: Some(src.to_string_lossy().to_string()),
            data: None,
            name: Some(name_marker.into()),
            mime: None,
            kind: None,
            audio: None,
            caption: None,
            client_xfer_id: "ft15-x1".into(),
        })
        .await
        .expect("file.send");
    let fid = sent["file_id"].as_str().unwrap().to_string();

    // Drive the whole transfer, then inspect the capture.
    wait_for(
        || {
            a.store()
                .get_file(&fid)
                .ok()
                .flatten()
                .filter(|r| r.state == "complete")
        },
        Duration::from_secs(60),
        "ft15 completion",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let seen = cap.lock().unwrap();
    assert!(!seen.is_empty(), "tap captured nothing");
    for marker in [name_marker.as_bytes(), &content_marker[..]] {
        assert!(
            !seen.windows(marker.len()).any(|w| w == marker),
            "file plaintext leaked onto the wire"
        );
    }
}
