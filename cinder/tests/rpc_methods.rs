mod common;

use common::rpc::describe::{EVENTS, METHODS};
use common::*;
use serde_json::{json, Value};

const ALL_METHODS: &[&str] = &[
    "session.hello",
    "rpc.describe",
    "msg.send",
    "msg.edit",
    "msg.delete",
    "msg.react",
    "chat.list",
    "chat.history",
    "contact.list",
    "contact.add",
    "contact.accept",
    "contact.decline",
    "invite.create",
    "invite.accept",
    "ident.verify",
    "events.subscribe",
    "events.unsubscribe",
    "shutdown.request",
];

fn required_fields(result: &Value, fields: &[&str], method: &str) {
    for f in fields {
        assert!(result.get(*f).is_some(), "{method}: missing required field '{f}' in {result}");
    }
}

// P1 — happy path: every catalog method returns a result with schema fields.
#[tokio::test]
async fn p1_all_methods_happy_path() {
    let mut h = harness();
    let r = hello(&mut h).await;
    assert_eq!(r["protocol_version"], 1);
    required_fields(&r, &["account", "capabilities", "server"], "session.hello");
    assert_eq!(r["server"]["transport"], "stdio");
    assert_eq!(r["capabilities"]["auth_method"], "stdio");
    assert_eq!(r["capabilities"]["scopes"], json!(["*"]));
    for l in ["max_frame_bytes", "max_body_bytes", "history_limit_max", "send_queue_frames", "replay_log_max_events"] {
        assert!(r["capabilities"]["limits"].get(l).is_some(), "limits.{l}");
    }

    let d = call(&mut h, 2, "rpc.describe", json!({})).await;
    required_fields(&d, &["protocol_version", "methods", "events", "errors", "limits"], "rpc.describe");

    let r = call(&mut h, 3, "msg.send", json!({"to": "contact:9f3b12", "body": "hello", "client_msg_id": "p1-1"})).await;
    required_fields(&r, &["msg_id", "chat_id", "state", "deduped"], "msg.send");
    assert_eq!(r["state"], "queued");
    assert_eq!(r["deduped"], false);

    let r = call(&mut h, 4, "msg.edit", json!({"msg_id": "msg:e01d7c", "body": "new body"})).await;
    required_fields(&r, &["msg_id", "chat_id", "revision", "state"], "msg.edit");

    let r = call(&mut h, 5, "msg.delete", json!({"msg_id": "msg:e01d7c", "scope": "everyone"})).await;
    required_fields(&r, &["msg_id", "deleted", "scope", "state"], "msg.delete");
    assert_eq!(r["deleted"], true);

    let r = call(&mut h, 6, "msg.react", json!({"msg_id": "msg:e01d7c", "emoji": "🫡"})).await;
    required_fields(&r, &["msg_id", "reactions", "state"], "msg.react");

    let r = call(&mut h, 7, "chat.list", json!({"limit": 50})).await;
    required_fields(&r, &["chats", "next_cursor"], "chat.list");
    for f in ["chat_id", "kind", "contact_id", "peer_id", "display_name", "trust", "unread", "last_activity", "last_message"] {
        assert!(r["chats"][0].get(f).is_some(), "chat.{f}");
    }

    let r = call(&mut h, 8, "chat.history", json!({"chat_id": CHAT, "limit": 3})).await;
    required_fields(&r, &["messages", "has_more", "next_before"], "chat.history");
    for f in ["msg_id", "chat_id", "dir", "from", "kind", "body", "ts", "revision", "reactions"] {
        assert!(r["messages"][0].get(f).is_some(), "message.{f}");
    }

    let r = call(&mut h, 9, "contact.list", json!({})).await;
    assert!(r["contacts"].is_array());
    for f in ["contact_id", "peer_id", "display_name", "state", "substate", "trust", "chat_id", "created_at"] {
        assert!(r["contacts"][0].get(f).is_some(), "contact.{f}");
    }

    let r = call(&mut h, 10, "contact.add", json!({"link": "cinder://abc123@relay.example.com?rendezvous=tok_x", "display_name": "x"})).await;
    required_fields(&r, &["contact_id", "state", "substate"], "contact.add");

    let r = call(&mut h, 11, "contact.accept", json!({"contact_id": "contact:b21e08"})).await;
    required_fields(&r, &["contact_id", "state", "chat_id"], "contact.accept");
    assert_eq!(r["state"], "active");

    let r = call(&mut h, 12, "contact.decline", json!({"contact_id": "contact:b21e08"})).await;
    required_fields(&r, &["contact_id", "state"], "contact.decline");
    assert_eq!(r["state"], "declined");

    let r = call(&mut h, 13, "invite.create", json!({"label": "ops", "ttl_sec": 86400})).await;
    required_fields(&r, &["invite_id", "link", "expires_at"], "invite.create");

    let r = call(&mut h, 14, "invite.accept", json!({"link": "cinder://xyz@relay.example.com:7700?rendezvous=tok_77zq"})).await;
    required_fields(&r, &["contact_id", "state", "substate", "peer_id"], "invite.accept");

    let r = call(&mut h, 15, "ident.verify", json!({"contact_id": "contact:9f3b12", "sas": "🦊🌊🔥🌵⚓🌙🍄"})).await;
    required_fields(&r, &["contact_id", "trust", "verified_at"], "ident.verify");
    assert_eq!(r["trust"], "verified");

    let r = call(&mut h, 16, "events.subscribe", json!({})).await;
    required_fields(&r, &["subscription_id", "oldest_seq", "latest_seq", "replaying"], "events.subscribe");
    let sub = r["subscription_id"].as_str().unwrap().to_string();
    assert!(sub.starts_with("sub:"));

    let r = call(&mut h, 17, "events.unsubscribe", json!({"subscription_id": sub})).await;
    assert_eq!(r["closed"], true);

    let r = call(&mut h, 18, "shutdown.request", json!({"grace_ms": 100})).await;
    assert_eq!(r["shutting_down"], true);
    assert_eq!(r["grace_ms"], 100);
}

// P1b — the dispatch table cannot drift from the describe table.
#[tokio::test]
async fn dispatch_covers_describe_table() {
    let mut h = harness();
    hello(&mut h).await;
    let d = call(&mut h, 2, "rpc.describe", json!({})).await;
    let described: Vec<String> = d["methods"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(described.len(), 18, "rpc.describe must describe 18 methods");
    assert_eq!(METHODS.len(), 18);
    for m in METHODS.iter() {
        assert!(described.contains(&m.name.to_string()), "describe missing {}", m.name);
    }
    for name in ALL_METHODS {
        assert!(METHODS.iter().any(|m| m.name == *name), "METHODS table missing {name}");
    }
    assert_eq!(EVENTS.len(), 8);
}

// P2 — rpc.describe answers pre-auth and covers the whole surface.
#[tokio::test]
async fn p2_describe_preauth() {
    let mut h = harness();
    // No session.hello — this must still answer.
    let d = call(&mut h, 42, "rpc.describe", json!({})).await;
    let methods = d["methods"].as_object().unwrap();
    for name in ALL_METHODS {
        assert!(methods.contains_key(*name), "describe missing method {name}");
        assert!(methods[*name].get("params").is_some(), "{name}.params");
        assert!(methods[*name].get("result").is_some(), "{name}.result");
        assert!(methods[*name].get("scope").is_some(), "{name}.scope");
    }
    let events = d["events"].as_object().unwrap();
    for name in [
        "message.received",
        "msg_state",
        "contact.request",
        "contact.connected",
        "security.identity_changed",
        "security.verification_needed",
        "session.backlog",
        "event.gap",
    ] {
        assert!(events.contains_key(name), "describe missing event {name}");
        assert!(events[name].get("params").is_some(), "{name}.params");
    }
    let kinds = d["errors"]["kinds"].as_object().unwrap();
    for (kind, code) in [
        ("not_connected", -32010),
        ("peer_offline", -32011),
        ("contact_blocked", -32012),
        ("key_changed_blocked", -32013),
        ("mailbox_full", -32014),
        ("rate_limited", -32015),
        ("unauthorized_scope", -32016),
        ("not_found", -32017),
        ("conflict", -32018),
    ] {
        assert_eq!(kinds[kind]["code"], code, "kind {kind}");
        assert!(kinds[kind].get("retryable").is_some(), "kind {kind}");
    }
    assert_eq!(d["limits"]["max_frame_bytes"], 4194304);
}

// §2.4 — everything but hello/describe fails pre-hello with -32600 +
// kind:unauthorized_scope.
#[tokio::test]
async fn pre_hello_gate() {
    let mut h = harness();
    let e = call_err(&mut h, 1, "msg.send", json!({"to": "chat:x", "body": "hi", "client_msg_id": "k"})).await;
    assert_eq!(e["code"], -32600);
    assert_eq!(e["data"]["kind"], "unauthorized_scope");
    assert_eq!(e["data"]["hint"], "call session.hello first");

    let e = call_err(&mut h, 2, "chat.list", json!({})).await;
    assert_eq!(e["code"], -32600);
    assert_eq!(e["data"]["kind"], "unauthorized_scope");

    // hello + describe still work pre-auth
    assert!(call(&mut h, 3, "rpc.describe", json!({})).await["methods"].is_object());
    hello(&mut h).await;
    let r = call(&mut h, 5, "chat.list", json!({})).await;
    assert!(r["chats"].is_array());
}

// §3.2 — a narrowed capability grant blocks out-of-scope methods.
#[tokio::test]
async fn scope_narrowing() {
    let mut h = harness();
    let r = call(&mut h, 1, "session.hello", json!({"protocol_version": 1, "capabilities_requested": ["read"]})).await;
    assert_eq!(r["capabilities"]["scopes"], json!(["read"]));

    let e = call_err(&mut h, 2, "msg.send", json!({"to": "chat:x", "body": "hi", "client_msg_id": "k"})).await;
    assert_eq!(e["code"], -32016);
    assert_eq!(e["data"]["kind"], "unauthorized_scope");
    assert_eq!(e["data"]["retryable"], false);

    // "verify" scope is withheld from a read-only grant
    let e = call_err(&mut h, 3, "ident.verify", json!({"contact_id": "contact:9f3b12", "sas": "x"})).await;
    assert_eq!(e["code"], -32016);
    assert_eq!(e["data"]["kind"], "unauthorized_scope");

    let r = call(&mut h, 4, "contact.list", json!({})).await;
    assert!(r["contacts"].is_array());
}

// §4.1 — version negotiation.
#[tokio::test]
async fn hello_version_negotiation() {
    let mut h = harness();
    let r = call(&mut h, 1, "session.hello", json!({"protocol_version": [1, 2, 3]})).await;
    assert_eq!(r["protocol_version"], 1);

    let mut h2 = harness();
    let e = call_err(&mut h2, 1, "session.hello", json!({"protocol_version": [2, 99]})).await;
    assert_eq!(e["code"], -32602);
    assert_eq!(e["data"]["kind"], "conflict");
    assert!(e["data"]["hint"].as_str().unwrap().contains("1"));

    let mut h3 = harness();
    let e = call_err(&mut h3, 1, "session.hello", json!({})).await;
    assert_eq!(e["code"], -32602);
    assert_eq!(e["data"]["kind"], "invalid_params");
}

// P9 — shutdown drains: result returned, notice emitted, later requests
// rejected with -32603 + "server shutting down".
#[tokio::test]
async fn p9_shutdown() {
    let mut h = harness();
    hello(&mut h).await;
    let r = call(&mut h, 2, "shutdown.request", json!({"grace_ms": 50})).await;
    assert_eq!(r["shutting_down"], true);
    assert!(h.engine.is_shutting_down());
    assert_eq!(h.core.shutdowns.lock().unwrap().as_slice(), &[50]);

    // shutdown notice lands after the response
    let note = frame(&mut h.rx).await;
    assert_eq!(note["method"], "session.backlog");
    assert_eq!(note["params"]["shutting_down"], true);

    let e = call_err(&mut h, 3, "chat.list", json!({})).await;
    assert_eq!(e["code"], -32603);
    assert_eq!(e["data"]["kind"], "internal");
    assert!(e["data"]["hint"].as_str().unwrap().contains("shutting down"));
}

// §5.7 — hello emits a backlog notice when outbound work is queued.
#[tokio::test]
async fn hello_backlog_notice() {
    let mut h = harness();
    *h.core.summary.lock().unwrap() = json!({"unread_chats": 1, "pending_contact_requests": 0, "queued_outbound": 2});
    hello(&mut h).await;
    let note = frame(&mut h.rx).await;
    assert_eq!(note["method"], "session.backlog");
    assert_eq!(note["params"]["summary"]["queued_outbound"], 2);
}
