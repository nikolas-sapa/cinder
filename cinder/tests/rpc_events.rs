mod common;

use common::rpc::dispatch::{EngineConfig, Transport};
use common::rpc::events::OutMsg;
use common::*;
use serde_json::json;

fn emit_msgs(h: &Harness, n: u64) {
    for i in 1..=n {
        h.core.emit(
            "message.received",
            Some(CHAT),
            json!({"chat_id": CHAT, "message": msg(&format!("msg:e{i:02}"), "in", &format!("inbound-{i}"), "2026-11-18T16:00:00Z")}),
        );
    }
}

fn ev_seq(v: &serde_json::Value) -> u64 {
    v["params"]["seq"].as_u64().unwrap()
}

// P5 — subscribe with since= replays [since+1..latest] in order, then backlog.
#[tokio::test]
async fn p5_subscribe_replays_since() {
    let mut h = harness();
    hello(&mut h).await;
    emit_msgs(&h, 5);

    let r = call(&mut h, 10, "events.subscribe", json!({"since": 2})).await;
    assert_eq!(r["latest_seq"], 5);
    assert_eq!(r["replaying"], true);
    let sub = r["subscription_id"].as_str().unwrap().to_string();

    let mut seqs = Vec::new();
    for _ in 0..4 {
        let f = frame(&mut h.rx).await;
        assert_eq!(f["params"]["subscription_id"], sub);
        if f["method"] == "session.backlog" {
            seqs.push(-1i64);
            break;
        }
        seqs.push(ev_seq(&f) as i64);
    }
    assert_eq!(
        seqs,
        vec![3, 4, 5, -1],
        "replayed 3,4,5 then backlog: {seqs:?}"
    );

    // exactly one backlog marker, with summary + counts
    let frames = drain(&mut h.rx).await;
    for f in &frames {
        assert_ne!(
            f["method"], "session.backlog",
            "backlog emitted exactly once"
        );
    }
}

// P6 — since older than the retained window: event.gap first, then what the
// log still holds. Silent omission is a protocol violation.
#[tokio::test]
async fn p6_gap_marker_on_expiry() {
    let cfg = EngineConfig {
        replay_log_max_events: 4,
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;
    emit_msgs(&h, 10); // retained: seqs 7,8,9,10

    let r = call(&mut h, 10, "events.subscribe", json!({"since": 2})).await;
    assert_eq!(r["oldest_seq"], 7);
    assert_eq!(r["latest_seq"], 10);

    let first = frame(&mut h.rx).await;
    assert_eq!(
        first["method"], "event.gap",
        "gap marker must precede replayed events"
    );
    assert_eq!(first["params"]["requested_since"], 2);
    assert_eq!(first["params"]["oldest_available"], 7);
    assert_eq!(first["params"]["lost"], "unknown");
    assert_eq!(ev_seq(&first), 7, "gap seq is the oldest retained position");

    for expect in [7u64, 8, 9, 10] {
        let f = frame(&mut h.rx).await;
        assert_eq!(f["method"], "message.received");
        assert_eq!(ev_seq(&f), expect);
    }
    let back = frame(&mut h.rx).await;
    assert_eq!(back["method"], "session.backlog");
    assert_eq!(back["params"]["gap"], true);
    assert_eq!(back["params"]["replayed"], 4);
    assert!(back["params"]["summary"]["unread_chats"].is_u64());
}

// P11 — inbound bodies are always marked untrusted on the wire.
#[tokio::test]
async fn p11_trusted_false_on_inbound() {
    let mut h = harness();
    hello(&mut h).await;
    call(&mut h, 1, "events.subscribe", json!({})).await;
    frame(&mut h.rx).await; // backlog

    h.core.emit(
        "message.received",
        Some(CHAT),
        json!({"chat_id": CHAT, "message": msg("msg:f20b11", "in", "meet me at {evil.com}", "2026-11-18T16:06:58Z")}),
    );
    let ev = frame(&mut h.rx).await;
    assert_eq!(ev["method"], "message.received");
    assert_eq!(
        ev["params"]["message"]["body"]["trusted"], false,
        "inbound body.trusted must be false"
    );
    // and the schema's trusted field carries the injection-hygiene description
    let d = call(&mut h, 2, "rpc.describe", json!({})).await;
    let s = d["events"]["message.received"]["params"].to_string();
    assert!(
        s.contains("UNTRUSTED"),
        "schema must mark inbound text untrusted"
    );
}

// kind + chat filters gate which events land on this subscription.
#[tokio::test]
async fn subscription_filters() {
    let mut h = harness();
    hello(&mut h).await;
    let _r = call(
        &mut h,
        1,
        "events.subscribe",
        json!({"kinds": ["msg_state"], "chats": ["chat:onlyme"]}),
    )
    .await;
    frame(&mut h.rx).await; // backlog

    // wrong kind -> dropped for this sub
    h.core.emit(
        "message.received",
        Some(CHAT),
        json!({"chat_id": CHAT, "message": {}}),
    );
    // right kind, wrong chat -> dropped
    h.core.emit(
        "msg_state",
        Some(CHAT),
        json!({"chat_id": CHAT, "msg_id": "m", "client_msg_id": "c", "state": "sent"}),
    );
    // right kind, right chat -> delivered
    h.core.emit(
        "msg_state",
        Some("chat:onlyme"),
        json!({"chat_id": "chat:onlyme", "msg_id": "m", "client_msg_id": "c", "state": "sent"}),
    );

    let ev = frame(&mut h.rx).await;
    assert_eq!(ev["method"], "msg_state");
    assert_eq!(ev["params"]["chat_id"], "chat:onlyme");
    assert!(
        maybe_frame(&mut h.rx, 150).await.is_none(),
        "filtered events must not arrive"
    );
}

// No since -> live-only, still gets exactly one backlog marker first.
#[tokio::test]
async fn live_subscribe_flow() {
    let mut h = harness();
    hello(&mut h).await;
    emit_msgs(&h, 2);
    let r = call(&mut h, 1, "events.subscribe", json!({})).await;
    assert_eq!(r["replaying"], false);
    let back = frame(&mut h.rx).await;
    assert_eq!(back["method"], "session.backlog");
    assert_eq!(back["params"]["replayed"], 0);
    assert_eq!(back["params"]["gap"], false);

    h.core.emit("contact.request", None, json!({"contact_id": "contact:b21e08", "peer_id": "peer:z4mk", "via": "static", "invite_id": null, "display_name": null}));
    let ev = frame(&mut h.rx).await;
    assert_eq!(ev["method"], "contact.request");
    assert_eq!(ev_seq(&ev), 3);
}

// unsubscribe stops delivery; the cursor stays resumable.
#[tokio::test]
async fn unsubscribe_stops_and_resume_works() {
    let mut h = harness();
    hello(&mut h).await;
    let r = call(&mut h, 1, "events.subscribe", json!({})).await;
    let sub = r["subscription_id"].as_str().unwrap().to_string();
    frame(&mut h.rx).await; // backlog

    let r = call(
        &mut h,
        2,
        "events.unsubscribe",
        json!({"subscription_id": sub}),
    )
    .await;
    assert_eq!(r["closed"], true);
    h.core.emit(
        "message.received",
        Some(CHAT),
        json!({"chat_id": CHAT, "message": {}}),
    );
    assert!(maybe_frame(&mut h.rx, 150).await.is_none());

    // resume from seq 0 -> replays the event emitted while unsubscribed
    let _r = call(&mut h, 3, "events.subscribe", json!({"since": 0})).await;
    let mut got = Vec::new();
    for _ in 0..2 {
        got.push(frame(&mut h.rx).await);
    }
    assert_eq!(got[0]["method"], "message.received");
    assert_eq!(got[1]["method"], "session.backlog");
}

// §8.1 — a subscriber whose bounded queue overflows gets its connection
// killed; other clients are untouched. Never unbounded, never silent.
#[tokio::test]
async fn queue_overflow_kills_only_that_conn() {
    let cfg = EngineConfig {
        send_queue_frames: 4,
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;
    call(&mut h, 1, "events.subscribe", json!({})).await;
    frame(&mut h.rx).await; // backlog fills slot 1

    // second client stays healthy
    let (sess2, mut rx2) = h.engine.connect(Transport::Stdio);
    let mut h2sess = sess2;
    let _ = &mut h2sess;
    h2sess.handle_line(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\",\"params\":{\"protocol_version\":1}}").await;
    assert!(matches!(rx2.try_recv(), Ok(OutMsg::Frame(_))));

    // do not drain h.rx: fill 4-frame queue, then overflow
    emit_msgs(&h, 10);
    tokio::task::yield_now().await;
    assert!(h.sess.is_dead(), "overflowed connection must be killed");
}

// A slow reader can still complete a big replay: replay frames stream through
// the queue as it drains (send().await), and live events buffer meanwhile.
#[tokio::test]
async fn large_replay_streams_through_bounded_queue() {
    let cfg = EngineConfig {
        send_queue_frames: 4,
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;
    emit_msgs(&h, 12);

    let r = call(&mut h, 1, "events.subscribe", json!({"since": 0})).await;
    assert_eq!(r["latest_seq"], 12);
    let mut seqs = Vec::new();
    let mut saw_backlog = false;
    for _ in 0..14 {
        let f = frame(&mut h.rx).await;
        if f["method"] == "session.backlog" {
            saw_backlog = true;
            assert_eq!(f["params"]["replayed"], 12);
            break;
        }
        seqs.push(ev_seq(&f));
    }
    assert!(saw_backlog);
    assert_eq!(seqs, (1..=12).collect::<Vec<u64>>(), "replay order");
    assert!(!h.sess.is_dead());
}

// Events emitted DURING a replay arrive after the backlog, in seq order.
#[tokio::test]
async fn live_events_during_replay_buffer_behind_backlog() {
    let cfg = EngineConfig {
        send_queue_frames: 8,
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;
    emit_msgs(&h, 5);

    // subscribe (spawns replay task); emit a live event while replay streams
    line(&mut h, &req(1, "events.subscribe", json!({"since": 0}))).await;
    let resp = frame(&mut h.rx).await;
    assert_eq!(resp["id"], 1);
    h.core.emit(
        "msg_state",
        Some(CHAT),
        json!({"chat_id": CHAT, "msg_id": "msg:live", "client_msg_id": "c", "state": "sent"}),
    );

    // wire order: replay 1..5, backlog marker, then the buffered live event
    for expect in 1..=5u64 {
        let f = frame(&mut h.rx).await;
        assert_eq!(ev_seq(&f), expect);
        assert_eq!(f["method"], "message.received");
    }
    let back = frame(&mut h.rx).await;
    assert_eq!(back["method"], "session.backlog");
    let live = frame(&mut h.rx).await;
    assert_eq!(live["method"], "msg_state");
    assert_eq!(
        ev_seq(&live),
        6,
        "buffered live event keeps seq order behind backlog"
    );
}
