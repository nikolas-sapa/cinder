#![allow(dead_code)] // #[path] includes expose items other tests use

//! Phase A voice calls (docs/plans/voice-video-calls.md §12): wire-level
//! e2e — CA1 happy path over a real relay, CA9 session death mid-call,
//! CA10 call/chat interleave, CA11 relay blindness through a TCP tap.

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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cinder_core::identity::Identity;
use cinder_core::PeerId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::core::{contact_str, Core, MetaState};
use crate::rpc::core::{
    CallIdParams, CallRingParams, ChatHistoryParams, CinderCore, ContactIdParams, EventSink,
    MsgSendParams,
};

const CONNECT_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const WAIT: Duration = Duration::from_secs(20);
const DELIVERY_WAIT: Duration = Duration::from_secs(20);

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "e2e_call=debug".into()),
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

/// Pair two fresh identities over `relay` (invite → accept → both Active).
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
    // 30s: parallel workspace runs can schedule the teardown late.
    while waited < Duration::from_secs(30) {
        if core.conn_state() == crate::tui::backend::ConnState::Offline {
            return;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
}

async fn wait_for<T>(mut f: impl FnMut() -> Option<T>, dur: Duration, what: &str) -> T {
    let step = Duration::from_millis(50);
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

/// Capturing EventSink — the in-process equivalent of the RPC event stream.
struct Rec(Mutex<Vec<(String, Value)>>);

impl EventSink for Rec {
    fn emit(&self, kind: &str, _chat_id: Option<&str>, payload: Value) {
        self.0.lock().unwrap().push((kind.to_string(), payload));
    }
}

impl Rec {
    fn attach(core: &Arc<Core>) -> Arc<Rec> {
        let r = Arc::new(Rec(Mutex::new(Vec::new())));
        core.attach_sink(r.clone());
        r
    }

    fn ended_reason(&self, call_id: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(k, p)| {
                k == "call.state" && p["call_id"].as_str() == Some(call_id) && p["state"] == "ended"
            })
            .and_then(|(_, p)| p["reason"].as_str().map(str::to_string))
    }
}

async fn wait_ended(rec: &Rec, call_id: &str, dur: Duration) -> String {
    wait_for(|| rec.ended_reason(call_id), dur, "call.state ended event").await
}

fn ring_params(peer_b: &PeerId) -> CallRingParams {
    CallRingParams {
        to: contact_str(peer_b),
        media: None,
        client_call_id: None,
    }
}

/// RFC4648 base32 (no pad) — same alphabet as the core's `b32`/`call:` ids.
fn call_id_raw(s: &str) -> [u8; 16] {
    let mut bits = 0u64;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for c in s.trim_start_matches("call:").chars() {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u8 - b'A',
            c @ '2'..='7' => c as u8 - b'2' + 26,
            _ => continue,
        };
        bits = (bits << 5) | v as u64;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    out.try_into().expect("call: id is 16 bytes")
}

/// Shrink every call deadline so sweeps run in test time.
fn fast_caps(core: &Arc<Core>) {
    let mut c = core.test_call_caps_mut();
    c.janitor_tick = Duration::from_millis(50);
    c.ring_timeout = Duration::from_millis(1_200);
    c.deliver_timeout = Duration::from_millis(800);
    c.media_dead = Duration::from_millis(1_500);
    c.media_keepalive = Duration::from_millis(400);
    c.max_duration = Duration::from_secs(3600);
    c.ring_min_gap = Duration::ZERO;
    c.frame_pace = Duration::from_millis(20);
}

/// Bring a call all the way to active with media counters moving.
/// Returns the shared `call:` id.
async fn dial_active(a: &Arc<Core>, b: &Arc<Core>, peer_b: &PeerId) -> String {
    let r = a.call_ring(ring_params(peer_b)).await.expect("call.ring");
    let call_id = r["call_id"].as_str().unwrap().to_string();
    wait_for(
        || b.test_call(&call_id).filter(|(s, _)| s == "ringing"),
        WAIT,
        "callee ringing",
    )
    .await;
    b.call_accept(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("accept");
    wait_for(
        || {
            a.test_call_stats(&call_id)
                .and_then(|(tx, _, _, _, _)| (tx > 0).then_some(()))
        },
        WAIT,
        "caller media flowing",
    )
    .await;
    call_id
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
    panic!("timed out waiting for body {text:?} in {chat_id}");
}

/// Per-connection tap capture: `(client→relay bytes, relay→client bytes)`.
/// Kept separate so each direction can be frame-parsed in order.
type TapCaps = Arc<Mutex<Vec<(Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<u8>>>)>>>;

/// TCP tap: forwards bytes between clients and the real relay while
/// recording everything, per connection and per direction.
async fn spawn_tap(target: SocketAddr) -> (SocketAddr, TapCaps) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns: TapCaps = Arc::new(Mutex::new(Vec::new()));
    let conns_loop = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((inbound, _)) = listener.accept().await else {
                break;
            };
            let Ok(outbound) = TcpStream::connect(target).await else {
                continue;
            };
            let up = Arc::new(Mutex::new(Vec::new()));
            let down = Arc::new(Mutex::new(Vec::new()));
            conns_loop.lock().unwrap().push((up.clone(), down.clone()));
            tokio::spawn(async move {
                let (mut ri, mut wi) = inbound.into_split();
                let (mut ro, mut wo) = outbound.into_split();
                let up_task = tokio::spawn(async move {
                    let mut buf = [0u8; 16384];
                    loop {
                        match ri.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                up.lock().unwrap().extend_from_slice(&buf[..n]);
                                if wo.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                let down_task = tokio::spawn(async move {
                    let mut buf = [0u8; 16384];
                    loop {
                        match ro.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                down.lock().unwrap().extend_from_slice(&buf[..n]);
                                if wi.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                let _ = up_task.await;
                let _ = down_task.await;
            });
        }
    });
    (addr, conns)
}

/// Frame-parse `bytes` as `[u32 BE len ‖ msg]` and deserialize each frame.
/// Returns (frames parsed, bytes consumed); a parse failure stops the walk
/// at that offset.
fn parse_frames<T: serde::de::DeserializeOwned>(bytes: &[u8], cap: usize) -> usize {
    let mut off = 0usize;
    let mut n = 0usize;
    while off + 4 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        if len == 0 || len as usize > cap {
            break;
        }
        let len = len as usize;
        if off + 4 + len > bytes.len() {
            break;
        }
        if cinder_core::frame::wire_deserialize::<T>(&bytes[off + 4..off + 4 + len]).is_err() {
            break;
        }
        off += 4 + len;
        n += 1;
    }
    n
}

// CA1 — full e2e: ring → ringing → accept → active (media both ways) →
// hangup → ended on both sides, all over a real relay with real Noise.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca1_ring_accept_hangup_e2e() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));
    let rec_a = Rec::attach(&a);
    let rec_b = Rec::attach(&b);

    let peer_b = b.identity().peer_id();
    let call_id = dial_active(&a, &b, &peer_b).await;

    // Media actually flowing in both directions (rx moves on the far side).
    wait_for(
        || {
            b.test_call_stats(&call_id)
                .and_then(|(tx, rx, _, _, _)| (tx > 0 && rx > 0).then_some(()))
        },
        WAIT,
        "bidirectional media",
    )
    .await;

    a.call_hangup(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("hangup");
    assert_eq!(wait_ended(&rec_a, &call_id, WAIT).await, "hangup");
    assert_eq!(wait_ended(&rec_b, &call_id, WAIT).await, "hangup");
}

// CA9 — session death mid-call: the caller's pinned session dies → its
// janitor ends the call `failed`; the far side, starved of media, dies
// `failed` via media_dead. Neither side re-rings a stale generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca9_session_death_mid_call() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));
    let rec_a = Rec::attach(&a);
    let rec_b = Rec::attach(&b);

    let peer_b = b.identity().peer_id();
    let call_id = dial_active(&a, &b, &peer_b).await;

    // Kill the session generation the call is pinned to — as if the link
    // had dropped and tombstoned it. Call must end failed, not linger.
    a.test_drop_session(&peer_b);
    assert!(!a.test_has_session(&peer_b));

    assert_eq!(wait_ended(&rec_a, &call_id, WAIT).await, "failed");
    // B's pinned session is still alive, but its frames die at A's dead-sid
    // filter; media_dead starvation ends it failed too.
    assert_eq!(wait_ended(&rec_b, &call_id, WAIT).await, "failed");
}

// CA10 — call and chat share the session: during an active call, chat
// messages land both ways while CallFrames keep flowing; neither stream
// starves the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca10_call_chat_interleave() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));

    let peer_a = a.identity().peer_id();
    let peer_b = b.identity().peer_id();
    let call_id = dial_active(&a, &b, &peer_b).await;

    // Chat both ways while media is mid-flight.
    a.msg_send(MsgSendParams {
        to: contact_str(&peer_b),
        body: "hello during call A→B".into(),
        client_msg_id: "ca10-a2b".into(),
        ttl_ms: None,
    })
    .await
    .expect("msg.send a→b");
    b.msg_send(MsgSendParams {
        to: contact_str(&peer_a),
        body: "hello during call B→A".into(),
        client_msg_id: "ca10-b2a".into(),
        ttl_ms: None,
    })
    .await
    .expect("msg.send b→a");

    wait_for_body(
        &b,
        &crate::core::chat_str(&peer_a),
        "hello during call A→B",
        DELIVERY_WAIT,
    )
    .await;
    wait_for_body(
        &a,
        &crate::core::chat_str(&peer_b),
        "hello during call B→A",
        DELIVERY_WAIT,
    )
    .await;

    // Media kept moving across the chat burst.
    wait_for(
        || {
            a.test_call_stats(&call_id)
                .and_then(|(tx, rx, _, _, _)| (tx > 10 && rx > 0).then_some(()))
        },
        WAIT,
        "media survived interleave",
    )
    .await;

    a.call_hangup(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("hangup");
}

// CA11 — relay blindness for calls: every call byte rides inside the Noise
// transport. A tap capturing both directions must never see the call's
// plaintext identity (its 16-byte call_id appears in every CallFrame, so
// one leaked plaintext frame would trip thousands of hits), and the tap
// itself is proven live by a raw canary first.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca11_relay_blind_to_calls() {
    init_logs();
    let relay = spawn_relay().await;
    let (tap, conns) = spawn_tap(relay).await;

    // Canary: raw bytes through the tap must show up in the capture —
    // proves the tap isn't blind itself.
    let canary = b"CANARY-call-tap-0123456789abcdef";
    {
        let mut s = TcpStream::connect(tap).await.unwrap();
        s.write_all(canary).await.unwrap();
        s.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    {
        let conns = conns.lock().unwrap();
        assert!(
            conns.iter().any(|(up, _)| {
                let u = up.lock().unwrap();
                u.windows(canary.len()).any(|w| w == canary.as_slice())
            }),
            "tap failed to capture a raw canary; blindness proof is void"
        );
    }

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(tap, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));
    let rec_a = Rec::attach(&a);

    let peer_b = b.identity().peer_id();
    let call_id = dial_active(&a, &b, &peer_b).await;
    let needle = call_id_raw(&call_id);

    // Let real media flow — dozens of CallFrames each direction.
    wait_for(
        || {
            a.test_call_stats(&call_id)
                .and_then(|(tx, rx, _, _, _)| (tx > 20 && rx > 20).then_some(()))
        },
        WAIT,
        "media through tap",
    )
    .await;
    a.call_hangup(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("hangup");
    wait_ended(&rec_a, &call_id, WAIT).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut total_bytes = 0usize;
    let mut client_frames = 0usize;
    let mut server_frames = 0usize;
    for (up, down) in conns.lock().unwrap().iter() {
        let up = up.lock().unwrap();
        let down = down.lock().unwrap();
        total_bytes += up.len() + down.len();
        // The call_id's 16 raw bytes appear in every CallFrame/AppMsg — a
        // single plaintext leak would repeat thousands of times.
        assert!(
            !up.windows(needle.len()).any(|w| w == needle.as_slice())
                && !down.windows(needle.len()).any(|w| w == needle.as_slice()),
            "call_id plaintext leaked onto the wire"
        );
        // Up-stream (client→relay) parses only ClientMsg; down-stream only
        // ServerMsg. The canary conn's raw ASCII parses zero frames — it is
        // skipped via the empty-parse allowance below.
        if up.windows(4).any(|w| w == b"CANA") {
            continue;
        }
        client_frames += parse_frames::<cinder_core::proto::ClientMsg>(
            &up,
            cinder_core::proto::caps::MAX_FRAME_TRANSPORT,
        );
        server_frames += parse_frames::<cinder_core::proto::ServerMsg>(
            &down,
            cinder_core::proto::caps::MAX_FRAME_TRANSPORT,
        );
    }
    assert!(total_bytes > 0, "tap captured nothing — no traffic flowed");
    assert!(
        client_frames > 0,
        "no parseable ClientMsg frames on the wire — capture path broken?"
    );
    assert!(
        server_frames > 0,
        "no parseable ServerMsg frames on the wire — capture path broken?"
    );
    // The ASCII "call:"-prefixed string form must not leak either.
    let ascii = call_id.as_bytes();
    for (up, down) in conns.lock().unwrap().iter() {
        let up = up.lock().unwrap();
        let down = down.lock().unwrap();
        assert!(
            !up.windows(ascii.len()).any(|w| w == ascii)
                && !down.windows(ascii.len()).any(|w| w == ascii),
            "call_id ascii form leaked onto the wire"
        );
    }
}
