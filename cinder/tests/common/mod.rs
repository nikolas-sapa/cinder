#![allow(dead_code)]

#[path = "../../src/rpc/mod.rs"]
pub mod rpc;

use async_trait::async_trait;
use self::rpc::core::*;
use self::rpc::dispatch::{Engine, EngineConfig, Session, Transport};
use self::rpc::events::OutMsg;
use self::rpc::protocol::{ErrorKind, RpcError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

pub const CHAT: &str = "chat:77aa01";
pub const CONTACT: &str = "contact:9f3b12";
pub const PEER: &str = "peer:oyv4kq";

pub fn msg(msg_id: &str, dir: &str, text: &str, ts: &str) -> Value {
    json!({
        "msg_id": msg_id,
        "chat_id": CHAT,
        "dir": dir,
        "from": if dir == "in" { "peer:x9w2" } else { PEER },
        "kind": "text",
        "body": {"text": text, "trusted": dir != "in"},
        "state": "delivered",
        "ts": ts,
        "revision": 0,
        "reactions": [],
    })
}

pub struct MockCore {
    pub sends: Mutex<Vec<(String, String, String)>>,
    pub edits: Mutex<Vec<Value>>,
    pub deletes: Mutex<Vec<Value>>,
    pub reactions_calls: Mutex<Vec<Value>>,
    pub msg_counter: Mutex<u64>,
    pub messages: Mutex<Vec<Value>>,
    pub shutdowns: Mutex<Vec<u64>>,
    pub summary: Mutex<Value>,
    pub errors: Mutex<HashMap<String, RpcError>>,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
}

impl MockCore {
    pub fn basic() -> Self {
        let msgs = (1..=7)
            .rev()
            .map(|i| msg(&format!("msg:m{i:02}"), if i % 2 == 0 { "in" } else { "out" }, &format!("text-{i}"), "2026-11-18T16:00:00Z"))
            .collect();
        Self {
            sends: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
            deletes: Mutex::new(Vec::new()),
            reactions_calls: Mutex::new(Vec::new()),
            msg_counter: Mutex::new(0),
            messages: Mutex::new(msgs),
            shutdowns: Mutex::new(Vec::new()),
            summary: Mutex::new(json!({"unread_chats": 0, "pending_contact_requests": 0, "queued_outbound": 0})),
            errors: Mutex::new(HashMap::new()),
            sink: Mutex::new(None),
        }
    }

    pub fn attach(&self, sink: Arc<dyn EventSink>) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    pub fn emit(&self, kind: &str, chat_id: Option<&str>, payload: Value) {
        self.sink.lock().unwrap().as_ref().expect("attach an EventSink first").emit(kind, chat_id, payload);
    }

    pub fn fail(&self, method: &str, err: RpcError) {
        self.errors.lock().unwrap().insert(method.to_string(), err);
    }

    fn check(&self, method: &str) -> Result<(), RpcError> {
        match self.errors.lock().unwrap().remove(method) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn next_msg_id(&self) -> String {
        let mut n = self.msg_counter.lock().unwrap();
        *n += 1;
        format!("msg:svc{n:04}")
    }

    fn resolve_chat(to: &str) -> String {
        if let Some(rest) = to.strip_prefix("chat:") {
            format!("chat:{rest}")
        } else {
            CHAT.to_string()
        }
    }
}

#[async_trait]
impl CinderCore for MockCore {
    async fn session_identity(&self) -> Result<Value, RpcError> {
        Ok(json!({"peer_id": PEER, "display_name": "mock-account"}))
    }

    async fn msg_send(&self, p: MsgSendParams) -> Result<Value, RpcError> {
        self.check("msg.send")?;
        self.sends.lock().unwrap().push((p.to.clone(), p.body.clone(), p.client_msg_id.clone()));
        Ok(json!({"msg_id": self.next_msg_id(), "chat_id": Self::resolve_chat(&p.to)}))
    }

    async fn msg_edit(&self, p: MsgEditParams) -> Result<Value, RpcError> {
        self.check("msg.edit")?;
        self.edits.lock().unwrap().push(json!({"msg_id": p.msg_id, "body": p.body}));
        Ok(json!({"msg_id": p.msg_id, "chat_id": CHAT, "revision": 1, "state": "queued"}))
    }

    async fn msg_delete(&self, p: MsgDeleteParams) -> Result<Value, RpcError> {
        self.check("msg.delete")?;
        let scope = p.scope.clone().unwrap_or_else(|| "local".into());
        self.deletes.lock().unwrap().push(json!({"msg_id": p.msg_id, "scope": scope}));
        Ok(json!({
            "msg_id": p.msg_id,
            "deleted": true,
            "scope": scope,
            "state": if scope == "everyone" { "queued" } else { "done" },
        }))
    }

    async fn msg_react(&self, p: MsgReactParams) -> Result<Value, RpcError> {
        self.check("msg.react")?;
        self.reactions_calls.lock().unwrap().push(json!({"msg_id": p.msg_id, "emoji": p.emoji}));
        let reactions = match &p.emoji {
            Some(e) => json!([{"emoji": e, "from": PEER, "mine": true}]),
            None => json!([]),
        };
        Ok(json!({"msg_id": p.msg_id, "reactions": reactions, "state": "queued"}))
    }

    async fn chat_list(&self, p: ChatListParams) -> Result<Value, RpcError> {
        self.check("chat.list")?;
        let chat = json!({
            "chat_id": CHAT,
            "kind": "direct",
            "contact_id": CONTACT,
            "peer_id": PEER,
            "display_name": "eidolon",
            "trust": "verified",
            "unread": 2,
            "last_activity": "2026-11-18T16:04:22Z",
            "last_message": {"msg_id": "msg:m01", "dir": "in", "kind": "text", "body": {"text": "copy that", "trusted": false}, "ts": "2026-11-18T16:04:21Z"},
        });
        Ok(json!({"chats": [chat], "next_cursor": null}))
    }

    async fn chat_history(&self, p: ChatHistoryParams) -> Result<Value, RpcError> {
        self.check("chat.history")?;
        let all = self.messages.lock().unwrap();
        let start = match &p.before {
            None => 0usize,
            Some(cur) => {
                let parts: Vec<&str> = cur.splitn(3, ':').collect();
                if parts.len() != 3 || parts[0] != "cur" || parts[1] != p.chat_id.trim_start_matches("chat:") {
                    return Err(RpcError::invalid_params("cursor is bound to a different chat or malformed"));
                }
                match parts[2].parse::<usize>() {
                    Ok(i) if i <= all.len() => i,
                    _ => {
                        return Err(RpcError::new(
                            ErrorKind::NotFound,
                            "cursor expired; restart pagination",
                        ))
                    }
                }
            }
        };
        let limit = p.limit.unwrap_or(100) as usize;
        let page: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
        let has_more = start + page.len() < all.len();
        let next_before = if has_more {
            json!(format!("cur:{}:{}", p.chat_id.trim_start_matches("chat:"), start + page.len()))
        } else {
            Value::Null
        };
        Ok(json!({"messages": page, "has_more": has_more, "next_before": next_before}))
    }

    async fn contact_list(&self, _p: ContactListParams) -> Result<Value, RpcError> {
        self.check("contact.list")?;
        Ok(json!({"contacts": [{
            "contact_id": CONTACT,
            "peer_id": PEER,
            "display_name": "eidolon",
            "state": "active",
            "substate": null,
            "trust": "verified",
            "chat_id": CHAT,
            "created_at": "2026-10-30T09:11:00Z",
        }]}))
    }

    async fn contact_add(&self, _p: ContactAddParams) -> Result<Value, RpcError> {
        self.check("contact.add")?;
        Ok(json!({"contact_id": "contact:new01", "state": "pending_out", "substate": "connecting"}))
    }

    async fn contact_accept(&self, p: ContactIdParams) -> Result<Value, RpcError> {
        self.check("contact.accept")?;
        Ok(json!({"contact_id": p.contact_id, "state": "active", "chat_id": "chat:90cd44"}))
    }

    async fn contact_decline(&self, p: ContactIdParams) -> Result<Value, RpcError> {
        self.check("contact.decline")?;
        Ok(json!({"contact_id": p.contact_id, "state": "declined"}))
    }

    async fn invite_create(&self, _p: InviteCreateParams) -> Result<Value, RpcError> {
        self.check("invite.create")?;
        Ok(json!({
            "invite_id": "inv:2c44ab",
            "link": "cinder://oyv4kqz3n5m2w7j1h8g0f4d6s9a2b5c8e1x4v7u0@relay.example.com:7700?rendezvous=tok_m41x9",
            "expires_at": "2026-11-19T16:05:00Z",
        }))
    }

    async fn invite_accept(&self, _p: InviteAcceptParams) -> Result<Value, RpcError> {
        self.check("invite.accept")?;
        Ok(json!({
            "contact_id": "contact:5t77ee",
            "state": "pending_out",
            "substate": "connecting",
            "peer_id": "peer:x9w2kv",
        }))
    }

    async fn ident_verify(&self, p: IdentVerifyParams) -> Result<Value, RpcError> {
        self.check("ident.verify")?;
        Ok(json!({"contact_id": p.contact_id, "trust": "verified", "verified_at": "2026-11-18T16:06:11Z"}))
    }

    async fn backlog_summary(&self) -> Result<Value, RpcError> {
        Ok(self.summary.lock().unwrap().clone())
    }

    async fn shutdown(&self, grace_ms: u64) -> Result<(), RpcError> {
        self.shutdowns.lock().unwrap().push(grace_ms);
        Ok(())
    }
}

pub struct Harness {
    pub engine: Arc<Engine>,
    pub core: Arc<MockCore>,
    pub sess: Session,
    pub rx: mpsc::Receiver<OutMsg>,
}

pub fn harness_with(cfg: EngineConfig) -> Harness {
    let core = Arc::new(MockCore::basic());
    let engine = Engine::new(core.clone(), cfg);
    core.attach(engine.events.clone());
    let (sess, rx) = engine.connect(Transport::Stdio);
    Harness { engine, core, sess, rx }
}

pub fn harness() -> Harness {
    harness_with(EngineConfig::default())
}

/// Feed one line through the session; returns the verdict.
pub async fn line(h: &mut Harness, l: &str) {
    h.sess.handle_line(l.as_bytes()).await;
}

/// Next outbound frame parsed as JSON; Hangup/close/timeout panic.
pub async fn frame(rx: &mut mpsc::Receiver<OutMsg>) -> Value {
    match timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(OutMsg::Frame(s))) => serde_json::from_str(&s).expect("frame is valid JSON"),
        Ok(Some(OutMsg::Hangup)) => panic!("unexpected hangup"),
        Ok(None) => panic!("channel closed"),
        Err(_) => panic!("timeout waiting for frame"),
    }
}

/// Next frame or None if the channel closed (connection hangup).
pub async fn maybe_frame(rx: &mut mpsc::Receiver<OutMsg>, ms: u64) -> Option<Value> {
    match timeout(Duration::from_millis(ms), rx.recv()).await {
        Ok(Some(OutMsg::Frame(s))) => Some(serde_json::from_str(&s).expect("frame is valid JSON")),
        _ => None,
    }
}

pub fn req(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A request expecting a result; asserts the very next frame is that response.
pub async fn call(h: &mut Harness, id: u64, method: &str, params: Value) -> Value {
    line(h, &req(id, method, params)).await;
    let f = frame(&mut h.rx).await;
    assert_eq!(f["id"], id, "expected response to id {id}, got {f}");
    assert!(f.get("error").is_none(), "unexpected error for {method}: {f}");
    f["result"].clone()
}

/// A request expecting an error; returns the error object.
pub async fn call_err(h: &mut Harness, id: u64, method: &str, params: Value) -> Value {
    line(h, &req(id, method, params)).await;
    let f = frame(&mut h.rx).await;
    assert_eq!(f["id"], id, "expected response to id {id}, got {f}");
    assert!(f.get("error").is_some(), "expected error for {method}, got {f}");
    f["error"].clone()
}

pub async fn hello(h: &mut Harness) -> Value {
    call(h, 1, "session.hello", json!({"protocol_version": 1})).await
}

/// Drain all pending notification frames (non-blocking).
pub async fn drain(rx: &mut mpsc::Receiver<OutMsg>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        if let OutMsg::Frame(s) = m {
            out.push(serde_json::from_str(&s).unwrap());
        }
    }
    out
}
