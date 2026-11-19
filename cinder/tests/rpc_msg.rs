mod common;

use common::rpc::dispatch::{EngineConfig, Transport};
use common::*;
use serde_json::json;
use std::time::Duration;

// P4 — client_msg_id idempotency: retry returns the same msg_id, deduped:true,
// and the core sees exactly one send.
#[tokio::test]
async fn p4_client_msg_id_idempotent() {
    let mut h = harness();
    hello(&mut h).await;

    let p = json!({"to": "contact:9f3b12", "body": "rendezvous at the old port, 0400", "client_msg_id": "agent-0001"});
    let r1 = call(&mut h, 10, "msg.send", p.clone()).await;
    assert_eq!(r1["deduped"], false);
    assert_eq!(r1["state"], "queued");

    // same key, same payload: identical msg_id, deduped:true, no second enqueue
    let r2 = call(&mut h, 11, "msg.send", p.clone()).await;
    assert_eq!(r2["msg_id"], r1["msg_id"]);
    assert_eq!(r2["chat_id"], r1["chat_id"]);
    assert_eq!(r2["deduped"], true);
    assert_eq!(
        h.core.sends.lock().unwrap().len(),
        1,
        "deduped retry must not re-enqueue"
    );

    // a different key is a new send
    let r3 = call(&mut h, 12, "msg.send", json!({"to": "contact:9f3b12", "body": "rendezvous at the old port, 0400", "client_msg_id": "agent-0002"})).await;
    assert_eq!(r3["deduped"], false);
    assert_ne!(r3["msg_id"], r1["msg_id"]);
    assert_eq!(h.core.sends.lock().unwrap().len(), 2);
}

// Idempotency survives a reconnect — the dedup table lives on the engine, not
// the connection.
#[tokio::test]
async fn dedup_survives_reconnect() {
    let mut h = harness();
    hello(&mut h).await;
    let p = json!({"to": "contact:9f3b12", "body": "survive me", "client_msg_id": "reconnect-1"});
    let r1 = call(&mut h, 1, "msg.send", p.clone()).await;

    // drop the session entirely (simulates a crashed client) and reconnect
    let engine = h.engine.clone();
    let core = h.core.clone();
    drop(h.sess);
    let (sess2, rx2) = engine.connect(Transport::Stdio);
    let mut h2 = Harness {
        engine,
        core,
        sess: sess2,
        rx: rx2,
    };
    hello(&mut h2).await;
    let r2 = call(&mut h2, 1, "msg.send", p).await;
    assert_eq!(r2["msg_id"], r1["msg_id"]);
    assert_eq!(r2["deduped"], true);
    assert_eq!(h2.core.sends.lock().unwrap().len(), 1);
}

// Same key + different payload = -32018 conflict, never a second message.
#[tokio::test]
async fn dedup_conflict_on_different_payload() {
    let mut h = harness();
    hello(&mut h).await;
    call(
        &mut h,
        1,
        "msg.send",
        json!({"to": "contact:9f3b12", "body": "first", "client_msg_id": "dup"}),
    )
    .await;

    let e = call_err(
        &mut h,
        2,
        "msg.send",
        json!({"to": "contact:9f3b12", "body": "DIFFERENT", "client_msg_id": "dup"}),
    )
    .await;
    assert_eq!(e["code"], -32018);
    assert_eq!(e["data"]["kind"], "conflict");
    assert_eq!(e["data"]["retryable"], false);
    assert_eq!(h.core.sends.lock().unwrap().len(), 1);

    let e = call_err(
        &mut h,
        3,
        "msg.send",
        json!({"to": "chat:other99", "body": "first", "client_msg_id": "dup"}),
    )
    .await;
    assert_eq!(e["code"], -32018);
    assert_eq!(h.core.sends.lock().unwrap().len(), 1);
}

// Failed sends leave no dedup record — the retry really retries.
#[tokio::test]
async fn dedup_only_on_acceptance() {
    let mut h = harness();
    hello(&mut h).await;
    h.core.fail(
        "msg.send",
        common::rpc::protocol::RpcError::new(
            common::rpc::protocol::ErrorKind::PeerOffline,
            "no mailbox path",
        ),
    );
    let e = call_err(
        &mut h,
        1,
        "msg.send",
        json!({"to": "contact:x", "body": "b", "client_msg_id": "k9"}),
    )
    .await;
    assert_eq!(e["code"], -32011);

    let r = call(
        &mut h,
        2,
        "msg.send",
        json!({"to": "contact:x", "body": "b", "client_msg_id": "k9"}),
    )
    .await;
    assert_eq!(r["deduped"], false);
    assert_eq!(h.core.sends.lock().unwrap().len(), 1);
}

// Dedup table is bounded: beyond capacity the oldest record evicts and a
// replayed key re-sends (spec-permitted after expiry).
#[tokio::test]
async fn dedup_table_bounded() {
    let cfg = EngineConfig {
        dedup_max_entries: 2,
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;

    let r1 = call(
        &mut h,
        1,
        "msg.send",
        json!({"to": "chat:x", "body": "1", "client_msg_id": "k1"}),
    )
    .await;
    call(
        &mut h,
        2,
        "msg.send",
        json!({"to": "chat:x", "body": "2", "client_msg_id": "k2"}),
    )
    .await;
    call(
        &mut h,
        3,
        "msg.send",
        json!({"to": "chat:x", "body": "3", "client_msg_id": "k3"}),
    )
    .await;

    // k1 evicted: a retry is treated as a fresh send
    let r = call(
        &mut h,
        4,
        "msg.send",
        json!({"to": "chat:x", "body": "1", "client_msg_id": "k1"}),
    )
    .await;
    assert_eq!(r["deduped"], false);
    assert_ne!(r["msg_id"], r1["msg_id"]);
    assert_eq!(h.core.sends.lock().unwrap().len(), 4);
}

// TTL expiry likewise frees the key.
#[tokio::test]
async fn dedup_ttl_expiry() {
    let cfg = EngineConfig {
        dedup_ttl: Duration::from_millis(20),
        ..Default::default()
    };
    let mut h = harness_with(cfg);
    hello(&mut h).await;
    call(
        &mut h,
        1,
        "msg.send",
        json!({"to": "chat:x", "body": "b", "client_msg_id": "ttl"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let r = call(
        &mut h,
        2,
        "msg.send",
        json!({"to": "chat:x", "body": "b", "client_msg_id": "ttl"}),
    )
    .await;
    assert_eq!(r["deduped"], false);
    assert_eq!(h.core.sends.lock().unwrap().len(), 2);
}

// msg.send ttl_ms is forwarded to the core; the wire carries the absolute
// expires_at inside the encrypted payload, not the TTL itself.
#[tokio::test]
async fn msg_send_forwards_ttl_ms() {
    let mut h = harness();
    hello(&mut h).await;
    let r = call(
        &mut h,
        1,
        "msg.send",
        json!({"to": "contact:9f3b12", "body": "vanish", "client_msg_id": "ttl-1", "ttl_ms": 5000}),
    )
    .await;
    assert_eq!(r["state"], "queued");
    assert_eq!(h.core.sends.lock().unwrap().len(), 1);
    assert_eq!(
        h.core.sends.lock().unwrap()[0].3,
        Some(5000),
        "ttl_ms must reach the core"
    );

    // absent ttl_ms → None → the chat default decides
    call(
        &mut h,
        2,
        "msg.send",
        json!({"to": "contact:9f3b12", "body": "stay", "client_msg_id": "ttl-2"}),
    )
    .await;
    assert_eq!(h.core.sends.lock().unwrap()[1].3, None);
}

// chat.set_disappearing validates the chat: prefix and round-trips {chat_id,
// ttl_ms}; a missing chat_id is -32602.
#[tokio::test]
async fn chat_set_disappearing_params() {
    let mut h = harness();
    hello(&mut h).await;
    let r = call(
        &mut h,
        1,
        "chat.set_disappearing",
        json!({"chat_id": CHAT, "ttl_ms": 30_000}),
    )
    .await;
    assert_eq!(r["chat_id"], CHAT);
    assert_eq!(r["ttl_ms"], 30_000);
    assert_eq!(
        h.core.disappearing.lock().unwrap().as_slice(),
        &[(CHAT.to_string(), 30_000)]
    );

    // disable
    let r = call(
        &mut h,
        2,
        "chat.set_disappearing",
        json!({"chat_id": CHAT, "ttl_ms": 0}),
    )
    .await;
    assert_eq!(r["ttl_ms"], 0);

    let e = call_err(
        &mut h,
        3,
        "chat.set_disappearing",
        json!({"chat_id": "contact:x", "ttl_ms": 5}),
    )
    .await;
    assert_eq!(e["code"], -32602);

    let e = call_err(&mut h, 4, "chat.set_disappearing", json!({"ttl_ms": 5})).await;
    assert_eq!(e["code"], -32602);
}

// msg.expired flows through the event pipeline and honors the chats filter.
#[tokio::test]
async fn msg_expired_event_flows() {
    let mut h = harness();
    hello(&mut h).await;
    let r = call(
        &mut h,
        1,
        "events.subscribe",
        json!({"kinds": ["msg.expired"], "chats": [CHAT]}),
    )
    .await;
    let sub = r["subscription_id"].clone();
    let _backlog = frame(&mut h.rx).await;

    // a different chat's expiry must NOT reach this subscription
    h.core.emit(
        "msg.expired",
        Some("chat:other"),
        json!({"chat_id": "chat:other", "msg_id": "msg:x"}),
    );
    assert!(maybe_frame(&mut h.rx, 100).await.is_none());

    h.core.emit(
        "msg.expired",
        Some(CHAT),
        json!({"chat_id": CHAT, "msg_id": "msg:gone1"}),
    );
    let ev = frame(&mut h.rx).await;
    assert_eq!(ev["method"], "msg.expired");
    assert_eq!(ev["params"]["subscription_id"], sub);
    assert_eq!(ev["params"]["chat_id"], CHAT);
    assert_eq!(ev["params"]["msg_id"], "msg:gone1");
    assert!(ev["params"]["seq"].as_u64().unwrap() >= 1);
}

// client_op_id dedup on the mutating ops.
#[tokio::test]
async fn client_op_id_dedup() {
    let mut h = harness();
    hello(&mut h).await;

    let r1 = call(
        &mut h,
        1,
        "msg.edit",
        json!({"msg_id": "msg:e1", "body": "v2", "client_op_id": "op-1"}),
    )
    .await;
    let r2 = call(
        &mut h,
        2,
        "msg.edit",
        json!({"msg_id": "msg:e1", "body": "v2", "client_op_id": "op-1"}),
    )
    .await;
    assert_eq!(r1, r2, "op replay returns the stored result");
    assert_eq!(h.core.edits.lock().unwrap().len(), 1);

    let e = call_err(
        &mut h,
        3,
        "msg.edit",
        json!({"msg_id": "msg:e1", "body": "v3", "client_op_id": "op-1"}),
    )
    .await;
    assert_eq!(e["code"], -32018);
    assert_eq!(h.core.edits.lock().unwrap().len(), 1);
}

// send returns at acceptance; msg_state is core-driven via the sink.
#[tokio::test]
async fn send_accepts_and_core_events_flow() {
    let mut h = harness();
    hello(&mut h).await;
    let _r = call(
        &mut h,
        1,
        "events.subscribe",
        json!({"kinds": ["msg_state"]}),
    )
    .await;
    let _backlog = frame(&mut h.rx).await; // session.backlog marker first

    let r = call(
        &mut h,
        2,
        "msg.send",
        json!({"to": "contact:9f3b12", "body": "hi", "client_msg_id": "cm-7"}),
    )
    .await;
    let msg_id = r["msg_id"].clone();

    h.core.emit("msg_state", Some(CHAT), json!({"msg_id": msg_id, "client_msg_id": "cm-7", "chat_id": CHAT, "state": "delivered", "reason": null}));
    let ev = frame(&mut h.rx).await;
    assert_eq!(ev["method"], "msg_state");
    assert_eq!(ev["params"]["client_msg_id"], "cm-7");
    assert_eq!(ev["params"]["state"], "delivered");
    assert!(ev["params"]["seq"].as_u64().unwrap() >= 1);
}
