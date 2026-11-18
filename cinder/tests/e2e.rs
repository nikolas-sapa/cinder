//! End-to-end: real relay, real Core, real Noise — plus a wire tap proving
//! the relay never sees plaintext, and a full `cinder rpc` subprocess
//! round-trip.

#[path = "../src/core.rs"]
mod core;
#[path = "../src/rpc/mod.rs"]
mod rpc;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/tui/mod.rs"]
mod tui;

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
use crate::rpc::core::{ChatHistoryParams, CinderCore, ContactIdParams, MsgSendParams};

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
        if let Some(i) = self.backlog.iter().position(|v| pred(v)) {
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
