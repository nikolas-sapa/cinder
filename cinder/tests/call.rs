#![allow(dead_code)] // #[path] includes expose items other tests use

//! Phase A voice calls (docs/plans/voice-video-calls.md §12): unit tests for
//! the pure machinery (codec, jitter buffer, outbound queue, state model)
//! plus in-process engine tests over a real relay with injected `CallCaps`
//! so every timeout runs in milliseconds instead of seconds.
//!
//! CA1/CA9/CA10/CA11 live in `e2e_call.rs` (they need the wire tap /
//! subprocess harness); CA17 lives in `rpc_methods.rs`.

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
use tokio::net::TcpListener;

use crate::call::{CallCaps, Playout, FRAME_SAMPLES};
use crate::core::{contact_str, Core, MetaState};
use crate::rpc::core::{
    CallIdParams, CallMuteParams, CallRingParams, CinderCore, ContactIdParams, EventSink,
};
use crate::rpc::protocol::ErrorKind;

const CONNECT_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const WAIT: Duration = Duration::from_secs(20);

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "call=debug".into()),
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
    pair_with(&a, &b).await;
    (a, b)
}

/// Drive the invite dance between an already-open `a` and a fresh `b`.
async fn pair_with(a: &Arc<Core>, b: &Arc<Core>) {
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
}

async fn stop(core: &Arc<Core>) {
    core.begin_shutdown();
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < Duration::from_secs(10) {
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

    /// `(kind,state,reason)` for call.state events of one call_id, plus
    /// whether call.incoming was seen, in arrival order.
    fn call_trail(&self, call_id: &str) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p["call_id"].as_str() == Some(call_id))
            .cloned()
            .collect()
    }

    fn ended_reason(&self, call_id: &str) -> Option<String> {
        self.call_trail(call_id)
            .iter()
            .find(|(k, p)| k == "call.state" && p["state"] == "ended")
            .and_then(|(_, p)| p["reason"].as_str().map(str::to_string))
    }
}

/// Wait for the terminal `call.state` event of `call_id` — polling the row
/// races `end_call`'s wire-notify-then-emit ordering, so wait on the event.
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

/// Shrink every call deadline so janitor sweeps run in test time.
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

// ---------------------------------------------------------------------------
// Pure-machinery unit tests: codec, jitter buffer, outbound queue.
// ---------------------------------------------------------------------------

// CA6 — encode → jitter → decode, 0-loss: monotonic seq, PLC == 0.
// Then 10% drops: PLC invoked exactly once per gap, no panic.
#[test]
fn ca6_media_loopback_codec() {
    let enc = call::OpusEnc::new(48_000).expect("encoder");
    let mut dec = call::OpusDec::new().expect("decoder");
    let mut pcm = [0.0f32; FRAME_SAMPLES];
    // Distinct non-silent input so the packets are real payloads.
    for (i, s) in pcm.iter_mut().enumerate() {
        *s = ((i % 96) as f32 - 48.0) / 48.0;
    }

    // 1 000 lossless frames, one insert per 20 ms playout tick (isochronous
    // steady state: buffer sits at the 6-frame target).
    let mut j = call::JitterBuf::new(6, 12);
    let mut out = [0.0f32; FRAME_SAMPLES];
    let mut played = 0u64;
    for seq in 0u16..1000 {
        let pkt = enc.encode(&pcm).expect("encode");
        assert!(!pkt.is_empty() && pkt.len() <= call::MAX_OPUS_PACKET);
        assert!(j.insert(seq, pkt), "seq {seq} dropped");
        match j.tick() {
            Playout::Frame(p) => {
                dec.decode(Some(&p), &mut out).expect("decode");
                played += 1;
            }
            Playout::Plc => {
                dec.decode(None, &mut out).expect("plc decode");
            }
            Playout::Buffering => {}
        }
    }
    // Drain the remaining buffered frames — once playing, an empty buffer
    // yields Plc forever, so stop when the map drains (Playout ticks after
    // that are tail-silence, not per-gap PLC).
    while j.buffered() > 0 {
        match j.tick() {
            Playout::Frame(p) => {
                dec.decode(Some(&p), &mut out).expect("decode");
                played += 1;
            }
            Playout::Plc => {
                dec.decode(None, &mut out).expect("plc decode");
            }
            Playout::Buffering => break,
        }
    }
    assert_eq!(played, 1000, "every buffered frame must play exactly once");
    assert_eq!(j.plc, 0, "lossless stream must not PLC");
    assert_eq!(j.drops_late, 0);

    // 10% deterministic drops: every seq ≡ 5 (mod 10) never inserted — all
    // interior gaps (a gap before the first frame isn't a perceived drop,
    // the buffer simply anchors at the first present seq). Each missing seq
    // produces exactly one PLC tick once playout reaches its slot.
    let mut j = call::JitterBuf::new(6, 12);
    let mut missing = 0u64;
    for seq in 0u16..1000 {
        if seq % 10 != 5 {
            j.insert(seq, enc.encode(&pcm).expect("encode"));
        } else {
            missing += 1;
        }
        match j.tick() {
            Playout::Frame(p) => {
                dec.decode(Some(&p), &mut out).expect("decode");
            }
            Playout::Plc => {
                dec.decode(None, &mut out).expect("plc decode");
            }
            Playout::Buffering => {}
        }
    }
    // Drain (stop at empty map — tail ticks would be silence-PLC, not gaps).
    while j.buffered() > 0 {
        match j.tick() {
            Playout::Frame(p) => {
                dec.decode(Some(&p), &mut out).expect("decode");
            }
            Playout::Plc => {
                dec.decode(None, &mut out).expect("plc decode");
            }
            Playout::Buffering => break,
        }
    }
    assert_eq!(j.plc, missing, "one PLC per missing seq");
}

// CA7 — jitter bounds: 240 ms horizon caps buffered frames; frames behind
// the play position count as drops_late and are never played.
#[test]
fn ca7_jitter_bounds() {
    let mut j = call::JitterBuf::new(6, 12);
    // Burst 20 frames at once: the 12-frame horizon swallows 12, drops 8.
    for seq in 0u16..20 {
        j.insert(seq, vec![seq as u8]);
    }
    assert!(j.buffered() <= 12, "buffer past horizon: {}", j.buffered());
    assert_eq!(j.drops_late, 8);

    // Drain a few playout slots, then try to re-insert an old seq.
    let mut played = 0;
    for _ in 0..4 {
        if let Playout::Frame(_) = j.tick() {
            played += 1;
        }
    }
    assert!(played > 0);
    let before = j.drops_late;
    assert!(!j.insert(0, vec![0]), "seq 0 is behind the play position");
    assert_eq!(j.drops_late, before + 1, "late frame must count drops_late");
}

// CA8 — CALL_FRAME_QUEUE backpressure: full queue drops the OLDEST frame,
// counts via the push() flag, and len never exceeds the cap.
#[test]
fn ca8_backpressure_queue() {
    let q = call::FrameQueue::new(64);
    let mut dropped = 0u64;
    for i in 0u16..100 {
        if q.push(vec![i as u8]) {
            dropped += 1;
        }
        assert!(q.len() <= 64, "queue exceeded cap: {}", q.len());
    }
    assert_eq!(q.len(), 64);
    assert_eq!(dropped, 36, "100 pushes into a 64-cap queue evict 36");
    // The oldest 36 are gone: first pop is seq 36, not seq 0.
    assert_eq!(q.pop().unwrap(), vec![36u8]);
}

// CA12b — the spec's transition graph: every state/event pair enumerated,
// `ended` absorbing. 500 pseudo-random sequences (xorshift — no proptest
// dep) never leave the legal graph.
#[test]
fn ca12_state_machine_is_absorbing() {
    use crate::call::CallState::*;
    #[derive(Clone, Copy)]
    enum Ev {
        Ring,
        Ringing,
        Accept,
        End,
    }
    // Legal edges only: Outgoing --Ringing--> Ringing --Accept--> Active --End--> Ended;
    // Outgoing --Accept--> Active (early answer); every non-ended state --End--> Ended.
    fn advance(s: call::CallState, e: Ev) -> Option<call::CallState> {
        Some(match (s, e) {
            (Outgoing, Ev::Ringing) => Ringing,
            (Outgoing | Ringing, Ev::Accept) => Active,
            (Outgoing | Ringing | Active, Ev::End) => Ended,
            _ => return None,
        })
    }
    // Exhaustive: every pair either matches a legal edge or stays put.
    for (s, e, legal) in [
        (Outgoing, Ev::Ring, false),
        (Outgoing, Ev::Ringing, true),
        (Outgoing, Ev::Accept, true),
        (Outgoing, Ev::End, true),
        (Ringing, Ev::Ringing, false),
        (Ringing, Ev::Accept, true),
        (Ringing, Ev::End, true),
        (Active, Ev::Ringing, false),
        (Active, Ev::Accept, false),
        (Active, Ev::End, true),
        (Ended, Ev::Ring, false),
        (Ended, Ev::Ringing, false),
        (Ended, Ev::Accept, false),
        (Ended, Ev::End, false),
    ] {
        assert_eq!(advance(s, e).is_some(), legal, "{s:?} + event");
    }
    // 500 pseudo-random sequences from Outgoing: apply only legal edges,
    // then verify ended is never escaped.
    let mut x = 0x9e3779b97f4a7c15u64;
    let mut rng = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..500 {
        let mut s = Outgoing;
        for _ in 0..20 {
            let e = match rng() % 4 {
                0 => Ev::Ring,
                1 => Ev::Ringing,
                2 => Ev::Accept,
                _ => Ev::End,
            };
            if s == Ended {
                assert_eq!(s, Ended, "ended must be absorbing");
                break;
            }
            s = advance(s, e).unwrap_or(s);
        }
    }
}

// ---------------------------------------------------------------------------
// Engine tests over a real relay (injected fast caps).
// ---------------------------------------------------------------------------

// CA2 — unanswered ring ends `no_answer` at the (injected) ring TTL.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca2_ring_timeout() {
    init_logs();
    assert_eq!(CallCaps::default().ring_timeout, Duration::from_secs(60));
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec = Rec::attach(&a);

    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
    let call_id = r["call_id"].as_str().unwrap().to_string();
    // B auto-acknowledges → caller sees ringing, then nobody answers.
    wait_for(
        || a.test_call(&call_id).filter(|(s, _)| s == "ringing"),
        WAIT,
        "ringing on caller",
    )
    .await;
    let reason = wait_ended(&rec, &call_id, Duration::from_secs(10)).await;
    assert_eq!(reason, "no_answer");
    assert_eq!(
        rec.call_trail(&call_id)
            .iter()
            .filter(|(k, p)| k == "call.state" && p["state"] == "ended")
            .count(),
        1,
        "exactly one ended event per call"
    );
}

// CA3 — callee never emits CallRinging (its re-ring throttle eats the ring)
// → caller ends `timeout` at the injected CALL_DELIVER_TIMEOUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca3_deliver_timeout() {
    init_logs();
    assert_eq!(CallCaps::default().deliver_timeout, Duration::from_secs(10));
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec = Rec::attach(&a);

    // B silently swallows A's ring → A sees outgoing → deliver deadline.
    // (fast_caps zeroes the throttle; restore a real gap so touch_ring bites.)
    b.test_call_caps_mut().ring_min_gap = Duration::from_secs(60);
    let peer_a = a.identity().peer_id();
    b.test_touch_ring(&peer_a);
    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
    let call_id = r["call_id"].as_str().unwrap().to_string();
    let reason = wait_ended(&rec, &call_id, Duration::from_secs(10)).await;
    assert_eq!(reason, "timeout");
    assert_eq!(b.test_live_calls(), 0, "B never created call state");
}

// CA4 — inbound ring while a call is live → auto CallEnd{busy}; the caller
// sees ended(busy) and the existing call is untouched. Needs a third peer:
// C rings A (live inbound), then B rings A → busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca4_busy_while_live() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    let c = open_core(dir_c.path(), relay).await;
    assert!(c.wait_connected(CONNECT_WAIT).await);
    pair_with(&a, &c).await;
    fast_caps(&a);
    fast_caps(&b);
    fast_caps(&c);
    let rec_b = Rec::attach(&b);

    // C rings A → A has a live inbound (ringing) call.
    let peer_a = a.identity().peer_id();
    let r_c = c.call_ring(ring_params(&peer_a)).await.expect("C ring A");
    let call_c = r_c["call_id"].as_str().unwrap().to_string();
    wait_for(
        || {
            a.test_call(&call_c)
                .filter(|(s, d)| s == "ringing" && d == "in")
        },
        WAIT,
        "A ringing with C",
    )
    .await;

    // B rings A → CALL_MAX_ACTIVE → auto CallEnd{busy}.
    let r_b = b.call_ring(ring_params(&peer_a)).await.expect("B ring A");
    let call_b = r_b["call_id"].as_str().unwrap().to_string();
    let reason = wait_ended(&rec_b, &call_b, WAIT).await;
    assert_eq!(reason, "busy");
    // A's live call with C survives untouched — and A never took B's call.
    assert_eq!(a.test_call(&call_c).map(|t| t.0), Some("ringing".into()));
    assert!(
        a.test_call(&call_b).is_none(),
        "busy ring must not create call state on A"
    );
}

// CA5 — call.decline → caller ended(decline), zero media on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca5_decline_no_media() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec_a = Rec::attach(&a);

    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
    let call_id = r["call_id"].as_str().unwrap().to_string();
    wait_for(
        || {
            b.test_call(&call_id)
                .filter(|(s, d)| s == "ringing" && d == "in")
        },
        WAIT,
        "inbound ringing on B",
    )
    .await;
    b.call_decline(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("call.decline");
    let reason = wait_ended(&rec_a, &call_id, WAIT).await;
    assert_eq!(reason, "decline");
    // No media runtime was ever created → zero CallFrames could have left.
    assert!(
        a.test_call_stats(&call_id).is_none(),
        "declined call must not carry a media runtime"
    );
}

// CA12 — second outbound call.ring while one is live → -32018 conflict;
// ended is terminal-absorbing (wire replays of CallAccept/CallFrame are
// dropped, the ended event fires exactly once).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca12_one_call_conflict_and_absorbing() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec_a = Rec::attach(&a);

    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
    let call_id = r["call_id"].as_str().unwrap().to_string();

    // A second ring while the first is live → conflict (-32018).
    let e = a.call_ring(ring_params(&peer_b)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Conflict, "second ring must conflict");
    assert_eq!(e.code, -32018);

    // Hang it up, then replay terminal-incompatible wire messages: they
    // must not revive or re-end the call.
    a.call_hangup(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("hangup");
    assert_eq!(a.test_call(&call_id).map(|t| t.0), Some("ended".into()));
    let raw = call_id_raw(&call_id);
    a.test_handle_app(
        peer_b,
        crate::core::AppMsg::CallAccept {
            call_id: raw,
            cap: call::CAP_AUDIO_OPUS48K,
        },
    )
    .await;
    a.test_handle_app(peer_b, crate::core::AppMsg::CallRinging { call_id: raw })
        .await;
    a.test_handle_app(
        peer_b,
        crate::core::AppMsg::CallFrame {
            call_id: raw,
            seq: 0,
            ts_ms: 0,
            payload: vec![0u8; 8],
        },
    )
    .await;
    a.test_handle_app(
        peer_b,
        crate::core::AppMsg::CallEnd {
            call_id: raw,
            reason: 0,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.test_call(&call_id).map(|t| t.0), Some("ended".into()));
    let ended = rec_a
        .call_trail(&call_id)
        .into_iter()
        .filter(|(k, p)| k == "call.state" && p["state"] == "ended")
        .count();
    assert_eq!(ended, 1, "ended must be emitted exactly once");
}

// CA13 — key change mid-call: security.identity_changed lands before the
// terminal call.state, the call ends failed-adjacent (key_changed), and
// post-change frames drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca13_key_changed_mid_call() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec_a = Rec::attach(&a);

    // Active call first.
    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
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
        || a.test_call(&call_id).filter(|(s, _)| s == "active"),
        WAIT,
        "caller active",
    )
    .await;

    // Drive the real key-change consequence path.
    a.test_key_change(&peer_b, [0xABu8; 32]).await;
    assert_eq!(a.test_call(&call_id).map(|t| t.0), Some("ended".into()));

    // Ordering: identity_changed strictly before this call's ended event.
    let events = rec_a.0.lock().unwrap().clone();
    let pos_id = events
        .iter()
        .position(|(k, _)| k == "security.identity_changed")
        .expect("identity_changed emitted");
    let pos_end = events
        .iter()
        .position(|(k, p)| {
            k == "call.state" && p["call_id"].as_str() == Some(&call_id) && p["state"] == "ended"
        })
        .expect("ended emitted");
    assert!(
        pos_id < pos_end,
        "identity_changed must precede the call's terminal event"
    );
    let ended = &events[pos_end].1;
    assert_eq!(ended["reason"], "key_changed");

    // A CallFrame for the dead call (any session) is a silent drop.
    let raw = call_id_raw(&call_id);
    a.test_handle_app(
        peer_b,
        crate::core::AppMsg::CallFrame {
            call_id: raw,
            seq: 1,
            ts_ms: 0,
            payload: vec![1u8; 8],
        },
    )
    .await;
    assert!(a.test_has_session(&peer_b));
}

// CA15 — per-call event ordering on the callee: call.incoming <
// call.state:ringing < active < ended, one ended per call. Repeated so a
// re-ordering regression can't hide behind timing luck.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca15_event_ordering() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    let rec_b = Rec::attach(&b);

    let peer_b = b.identity().peer_id();
    for round in 0..3 {
        let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
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
            || b.test_call(&call_id).filter(|(s, _)| s == "active"),
            WAIT,
            "callee active",
        )
        .await;
        a.call_hangup(CallIdParams {
            call_id: Some(call_id.clone()),
        })
        .await
        .expect("hangup");
        wait_ended(&rec_b, &call_id, WAIT).await;

        let trail = rec_b.call_trail(&call_id);
        let kinds: Vec<&str> = trail
            .iter()
            .map(|(k, p)| {
                if k == "call.incoming" {
                    "incoming"
                } else {
                    match p["state"].as_str() {
                        Some(s) => match s {
                            "ringing" => "ringing",
                            "active" => "active",
                            "ended" => "ended",
                            other => other,
                        },
                        None => "?",
                    }
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["incoming", "ringing", "active", "ended"],
            "round {round}: bad ordering {kinds:?}"
        );
    }
}

// CA16 — call.mute is local truth: no AppMsg is produced for it. With the
// mic gate closed the outbound pump stops emitting CallFrames entirely —
// tx_frames freezes, and unmuting resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca16_mute_emits_nothing() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));

    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
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
        "media flowing",
    )
    .await;

    let (tx0, ..) = a.test_call_stats(&call_id).unwrap();
    let r = a
        .call_mute(CallMuteParams {
            call_id: Some(call_id.clone()),
            muted: Some(true),
        })
        .await
        .expect("call.mute");
    assert_eq!(r["muted"], true);
    // At most one already-captured frame may flush after the flag lands;
    // allow a short drain, then require the wire to stay silent.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let (tx_base, ..) = a.test_call_stats(&call_id).unwrap();
    assert!(
        tx_base - tx0 <= 2,
        "mute leaked {extra} frames beyond one in-flight",
        extra = tx_base - tx0
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (tx1, ..) = a.test_call_stats(&call_id).unwrap();
    assert_eq!(
        tx_base, tx1,
        "mute must stop CallFrames — nothing on the wire"
    );

    a.call_mute(CallMuteParams {
        call_id: Some(call_id.clone()),
        muted: Some(false),
    })
    .await
    .expect("unmute");
    wait_for(
        || {
            a.test_call_stats(&call_id)
                .and_then(|(tx, _, _, _, _)| (tx > tx1).then_some(()))
        },
        WAIT,
        "media resumed",
    )
    .await;
}

// CA18 — call.ring with no live path (peer offline; deposit would be the
// only route) → -32011 peer_offline, and zero call state is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca18_offline_ring_never_deposits() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);

    let peer_b = b.identity().peer_id();
    // The CA18 scenario is "mailbox grant exists but no live session":
    // drop the session + mark the peer offline so the only route would be
    // a mailbox deposit — which calls must never take.
    a.test_drop_session(&peer_b);
    a.test_set_presence(&peer_b, false);

    let e = a.call_ring(ring_params(&peer_b)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PeerOffline);
    assert_eq!(e.code, -32011);
    assert_eq!(
        a.test_live_calls(),
        0,
        "offline ring must not create a call"
    );

    // And the graceful variant: B actually disconnects — same -32011 path
    // through live presence (flaky-tolerant: just observe, don't require).
    stop(&b).await;
    let mut observed_flip = false;
    for _ in 0..50 {
        if a.test_peer_online(&peer_b) == Some(false) {
            observed_flip = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::debug!(observed_flip, "presence offline broadcast observed");
    let e = a.call_ring(ring_params(&peer_b)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PeerOffline);
}

// CA14 — sustained call: transport frames to the relay stay under the
// 120 frames/s ceiling. Measured on the wire via the tap in e2e_call;
// here we bound the engine's own production rate: ~50 fps media plus a
// few control frames, sampled over the call window.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ca14_media_frame_rate() {
    init_logs();
    let relay = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = paired(relay, dir_a.path(), dir_b.path()).await;
    fast_caps(&a);
    fast_caps(&b);
    a.test_media_factory(std::sync::Arc::new(call::null_devices));
    b.test_media_factory(std::sync::Arc::new(call::null_devices));

    let peer_b = b.identity().peer_id();
    let r = a.call_ring(ring_params(&peer_b)).await.expect("call.ring");
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
        || a.test_call(&call_id).filter(|(s, _)| s == "active"),
        WAIT,
        "caller active",
    )
    .await;

    // Sample A→wire frames over a 2 s window: 20 ms pacing ⇒ ~50 fps.
    let (tx0, ..) = wait_for(|| a.test_call_stats(&call_id), WAIT, "media stats").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (tx1, ..) = a.test_call_stats(&call_id).unwrap();
    let fps = (tx1 - tx0) as f64 / 2.0;
    assert!(
        (30.0..=120.0).contains(&fps),
        "media rate {fps} fps outside ~50 fps / relay cap window"
    );

    a.call_hangup(CallIdParams {
        call_id: Some(call_id.clone()),
    })
    .await
    .expect("hangup");
}
