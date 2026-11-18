use crate::rpc::core::*;
use crate::rpc::describe;
use crate::rpc::dispatch::{Engine, Session};
use crate::rpc::events::{Filters, OutMsg};
use crate::rpc::protocol::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_GRACE_MS: u64 = 5000;
pub const MAX_GRACE_MS: u64 = 30000;
pub const INVITE_MAX_TTL_SEC: u64 = 259_200;
pub const INVITE_MIN_TTL_SEC: u64 = 60;
const KNOWN_SCOPES: &[&str] = &["read", "send", "contacts", "verify", "admin"];

/// `(account, client_msg_id)` dedup records: bounded count + bounded age
/// (spec floor is 24h; configurable down for tests).
pub struct DedupTable {
    map: HashMap<String, DedupRec>,
    order: VecDeque<(String, Instant)>,
    ttl: Duration,
    cap: usize,
}

struct DedupRec {
    fingerprint: String,
    result: Value,
    at: Instant,
}

pub enum DedupHit {
    Miss,
    Replay(Value),
    Conflict,
}

impl DedupTable {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), ttl, cap: cap.max(1) }
    }

    fn evict(&mut self) {
        let ttl = self.ttl;
        while let Some((key, at)) = self.order.front() {
            if at.elapsed() <= ttl {
                break;
            }
            let key = key.clone();
            self.order.pop_front();
            self.map.remove(&key);
        }
    }

    pub fn lookup(&mut self, key: &str, fingerprint: &str) -> DedupHit {
        self.evict();
        match self.map.get(key) {
            None => DedupHit::Miss,
            Some(rec) if rec.at.elapsed() > self.ttl => {
                self.map.remove(key);
                DedupHit::Miss
            }
            Some(rec) if rec.fingerprint != fingerprint => DedupHit::Conflict,
            Some(rec) => DedupHit::Replay(rec.result.clone()),
        }
    }

    pub fn insert(&mut self, key: String, fingerprint: String, result: Value) {
        self.evict();
        if self.map.contains_key(&key) {
            return;
        }
        while self.map.len() >= self.cap {
            let Some((old, _)) = self.order.pop_front() else { break };
            self.map.remove(&old);
        }
        self.map.insert(key.clone(), DedupRec { fingerprint, result, at: Instant::now() });
        self.order.push_back((key, Instant::now()));
    }
}

fn parse_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    let raw = match params {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v,
        Some(_) => return Err(RpcError::invalid_params("params must be an object (named params only)")),
    };
    serde_json::from_value(raw)
        .map_err(|e| RpcError::invalid_params(format!("bad params: {}", short_serde_err(&e))))
}

fn short_serde_err(e: &serde_json::Error) -> String {
    e.to_string().split(" at line ").next().unwrap_or("invalid").to_string()
}

fn req_str<'a>(m: &'a Map<String, Value>, field: &str) -> Result<&'a str, RpcError> {
    m.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params(format!("missing or invalid required param '{field}'")))
}

fn check_prefix(value: &str, prefixes: &[&str], field: &str) -> Result<(), RpcError> {
    if prefixes.iter().any(|p| value.starts_with(p)) {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!("'{field}' must start with one of {prefixes:?}")))
    }
}

fn check_limit(eng: &Engine, limit: u32, field: &str) -> Result<(), RpcError> {
    if (1..=eng.cfg.history_limit_max).contains(&limit) {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!("'{field}' out of range 1..={}", eng.cfg.history_limit_max)))
    }
}

async fn session_hello(eng: &Engine, sess: &mut Session, params: Option<Value>) -> Result<Value, RpcError> {
    let obj = match params {
        None | Some(Value::Null) => Map::new(),
        Some(v) if v.is_object() => v.as_object().unwrap().clone(),
        Some(_) => return Err(RpcError::invalid_params("params must be an object")),
    };
    let offered: Vec<u64> = match obj.get("protocol_version") {
        Some(Value::Number(n)) => n.as_u64().into_iter().collect(),
        Some(Value::Array(a)) => {
            let mut v = Vec::new();
            for item in a {
                match item.as_u64() {
                    Some(x) => v.push(x),
                    None => return Err(RpcError::invalid_params("protocol_version must be int or int[]")),
                }
            }
            v
        }
        Some(_) => return Err(RpcError::invalid_params("protocol_version must be int or int[]")),
        None => return Err(RpcError::invalid_params("missing required param 'protocol_version'")),
    };
    if offered.is_empty() {
        return Err(RpcError::invalid_params("protocol_version must be int or int[]"));
    }
    let negotiated = eng
        .cfg
        .supported_versions
        .iter()
        .filter(|v| offered.contains(&(**v as u64)))
        .max()
        .copied()
        .ok_or_else(|| {
            RpcError::new(ErrorKind::Conflict, format!(
                "no common protocol_version; supported: {:?}",
                eng.cfg.supported_versions
            ))
            .with_code(INVALID_PARAMS)
        })?;

    let mut scopes: HashSet<String> = HashSet::new();
    if let Some(req) = obj.get("capabilities_requested") {
        let arr = req
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("capabilities_requested must be a string array"))?;
        for s in arr {
            let s = s
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("capabilities_requested must be a string array"))?;
            if s == "*" {
                scopes.insert("*".into());
            } else if KNOWN_SCOPES.contains(&s) {
                scopes.insert(s.to_string());
            }
        }
    }
    if scopes.is_empty() {
        scopes.insert("*".into());
    }

    let account = eng.core().session_identity().await?;
    if !account.is_object() {
        return Err(RpcError::internal("core returned malformed account"));
    }

    let granted: Vec<String> = if scopes.contains("*") {
        vec!["*".into()]
    } else {
        KNOWN_SCOPES.iter().filter(|s| scopes.contains(**s)).map(|s| s.to_string()).collect()
    };
    sess.helloed = true;
    sess.scopes = granted.iter().cloned().collect();

    Ok(json!({
        "protocol_version": negotiated,
        "account": account,
        "capabilities": {
            "auth_method": sess.transport.auth_method(),
            "scopes": granted,
            "features": {"events_ack": false, "multi_account": false},
            "limits": eng.cfg.limits_json(),
        },
        "server": {"name": "cinder", "version": eng.cfg.server_version, "transport": sess.transport.as_str()},
    }))
}

fn rpc_describe(eng: &Engine) -> Result<Value, RpcError> {
    Ok(describe::describe(eng.cfg.limits_json()))
}

async fn msg_send(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: MsgSendParams = parse_params(params)?;
    check_prefix(&p.to, &["contact:", "chat:", "peer:"], "to")?;
    if p.body.len() > eng.cfg.max_body_bytes {
        return Err(RpcError::invalid_params(format!(
            "body exceeds limits.max_body_bytes ({})",
            eng.cfg.max_body_bytes
        )));
    }
    if p.client_msg_id.is_empty() {
        return Err(RpcError::invalid_params("client_msg_id must be non-empty"));
    }
    let fingerprint = json!({"m": "msg.send", "to": p.to, "body": p.body}).to_string();
    match eng.dedup_lookup(&p.client_msg_id, &fingerprint) {
        DedupHit::Replay(mut result) => {
            result.as_object_mut().map(|o| o.insert("deduped".into(), json!(true)));
            return Ok(result);
        }
        DedupHit::Conflict => {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "client_msg_id reused with a different payload; use a new key per logical send",
            ));
        }
        DedupHit::Miss => {}
    }
    let core_res = eng.core().msg_send(MsgSendParams { to: p.to.clone(), body: p.body.clone(), client_msg_id: p.client_msg_id.clone() }).await?;
    let msg_id = core_res
        .get("msg_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::internal("core returned no msg_id"))?
        .to_string();
    let chat_id = core_res
        .get("chat_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::internal("core returned no chat_id"))?
        .to_string();
    let result = json!({"msg_id": msg_id, "chat_id": chat_id, "state": "queued", "deduped": false});
    eng.dedup_insert(p.client_msg_id.clone(), fingerprint, result.clone());
    Ok(result)
}

/// `client_op_id` dedup for the mutating ops: replays return the stored
/// result verbatim; a reused key with different args is a conflict.
async fn dedup_wrap<F, Fut>(
    eng: &Engine,
    op_key: Option<String>,
    fingerprint: Value,
    call: F,
) -> Result<Value, RpcError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    let Some(key) = op_key else { return call().await };
    let fp = fingerprint.to_string();
    match eng.dedup_lookup(&key, &fp) {
        DedupHit::Replay(result) => return Ok(result),
        DedupHit::Conflict => {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "client_op_id reused with a different payload; use a new key per logical op",
            ));
        }
        DedupHit::Miss => {}
    }
    let result = call().await?;
    eng.dedup_insert(key, fp, result.clone());
    Ok(result)
}

async fn msg_edit(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: MsgEditParams = parse_params(params)?;
    check_prefix(&p.msg_id, &["msg:"], "msg_id")?;
    if p.body.len() > eng.cfg.max_body_bytes {
        return Err(RpcError::invalid_params(format!(
            "body exceeds limits.max_body_bytes ({})",
            eng.cfg.max_body_bytes
        )));
    }
    dedup_wrap(
        eng,
        p.client_op_id.clone(),
        json!({"m": "msg.edit", "msg_id": p.msg_id, "body": p.body}),
        || eng.core().msg_edit(p),
    )
    .await
}

async fn msg_delete(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: MsgDeleteParams = parse_params(params)?;
    check_prefix(&p.msg_id, &["msg:"], "msg_id")?;
    let scope = p.scope.clone().unwrap_or_else(|| "local".into());
    if scope != "local" && scope != "everyone" {
        return Err(RpcError::invalid_params("scope must be \"local\" or \"everyone\""));
    }
    dedup_wrap(
        eng,
        p.client_op_id.clone(),
        json!({"m": "msg.delete", "msg_id": p.msg_id, "scope": scope}),
        || eng.core().msg_delete(p),
    )
    .await
}

async fn msg_react(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let raw = match &params {
        Some(v) if v.is_object() => v.as_object().unwrap().clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => return Err(RpcError::invalid_params("params must be an object")),
    };
    // emoji is required AND may be null — serde Option can't tell missing from null
    if !raw.contains_key("emoji") {
        return Err(RpcError::invalid_params("missing required param 'emoji' (string or null)"));
    }
    let p: MsgReactParams = parse_params(params)?;
    check_prefix(&p.msg_id, &["msg:"], "msg_id")?;
    if let Some(e) = &p.emoji {
        if e.is_empty() {
            return Err(RpcError::invalid_params("emoji must be a non-empty string or null"));
        }
        if e.len() > 32 {
            return Err(RpcError::invalid_params("emoji exceeds 32 bytes"));
        }
    }
    eng.core().msg_react(p).await
}

async fn chat_list(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ChatListParams = parse_params(params)?;
    if let Some(l) = p.limit {
        check_limit(eng, l, "limit")?;
    }
    if let Some(c) = &p.cursor {
        check_prefix(c, &["cur:"], "cursor")?;
    }
    eng.core().chat_list(p).await
}

async fn chat_history(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ChatHistoryParams = parse_params(params)?;
    check_prefix(&p.chat_id, &["chat:"], "chat_id")?;
    if let Some(l) = p.limit {
        check_limit(eng, l, "limit")?;
    }
    if let Some(b) = &p.before {
        check_prefix(b, &["cur:"], "before")?;
    }
    eng.core().chat_history(p).await
}

async fn contact_list(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ContactListParams = parse_params(params)?;
    if let Some(s) = &p.state {
        if !["active", "pending_in", "pending_out", "declined", "blocked"].contains(&s.as_str()) {
            return Err(RpcError::invalid_params("state must be active|pending_in|pending_out|declined|blocked"));
        }
    }
    eng.core().contact_list(p).await
}

fn check_link(link: &str) -> Result<(), RpcError> {
    if !link.starts_with("cinder://") {
        return Err(RpcError::invalid_params("link must be a cinder:// URI (cinder://<base32-pubkey>@<relay-host>[:port][?rendezvous=<token>])"));
    }
    if !link["cinder://".len()..].contains('@') {
        return Err(RpcError::invalid_params("malformed cinder:// link: missing <pubkey>@<relay-host>"));
    }
    Ok(())
}

async fn contact_add(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ContactAddParams = parse_params(params)?;
    check_link(&p.link)?;
    eng.core().contact_add(p).await
}

async fn contact_accept(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ContactIdParams = parse_params(params)?;
    check_prefix(&p.contact_id, &["contact:"], "contact_id")?;
    eng.core().contact_accept(p).await
}

async fn contact_decline(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: ContactIdParams = parse_params(params)?;
    check_prefix(&p.contact_id, &["contact:"], "contact_id")?;
    eng.core().contact_decline(p).await
}

async fn invite_create(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let mut p: InviteCreateParams = parse_params(params)?;
    if let Some(ttl) = p.ttl_sec {
        if ttl < INVITE_MIN_TTL_SEC {
            return Err(RpcError::invalid_params(format!("ttl_sec must be >= {INVITE_MIN_TTL_SEC}")));
        }
        if ttl > INVITE_MAX_TTL_SEC {
            p.ttl_sec = Some(INVITE_MAX_TTL_SEC);
        }
    }
    eng.core().invite_create(p).await
}

async fn invite_accept(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: InviteAcceptParams = parse_params(params)?;
    check_link(&p.link)?;
    if !p.link.contains("rendezvous=") {
        return Err(RpcError::invalid_params("link has no rendezvous param; use contact.add for static addresses"));
    }
    eng.core().invite_accept(p).await
}

async fn ident_verify(eng: &Engine, params: Option<Value>) -> Result<Value, RpcError> {
    let p: IdentVerifyParams = parse_params(params)?;
    check_prefix(&p.contact_id, &["contact:"], "contact_id")?;
    if p.sas.is_empty() {
        return Err(RpcError::invalid_params("sas must be non-empty"));
    }
    eng.core().ident_verify(p).await
}

async fn events_subscribe(eng: &Arc<Engine>, sess: &mut Session, params: Option<Value>) -> Result<Value, RpcError> {
    let obj = match params {
        None | Some(Value::Null) => Map::new(),
        Some(v) if v.is_object() => v.as_object().unwrap().clone(),
        Some(_) => return Err(RpcError::invalid_params("params must be an object")),
    };

    let mut kinds: Option<HashSet<String>> = None;
    if let Some(v) = obj.get("kinds") {
        let arr = v.as_array().ok_or_else(|| RpcError::invalid_params("kinds must be a string array"))?;
        let mut set = HashSet::new();
        for k in arr {
            let k = k.as_str().ok_or_else(|| RpcError::invalid_params("kinds must be a string array"))?;
            if !describe::event_known(k) {
                return Err(RpcError::invalid_params(format!("unknown event kind '{k}'; call rpc.describe for the catalog")));
            }
            set.insert(k.to_string());
        }
        kinds = Some(set);
    }

    let mut chats: Option<HashSet<String>> = None;
    if let Some(v) = obj.get("chats") {
        let arr = v.as_array().ok_or_else(|| RpcError::invalid_params("chats must be an array of chat: ids"))?;
        let mut set = HashSet::new();
        for c in arr {
            let c = c.as_str().ok_or_else(|| RpcError::invalid_params("chats must be an array of chat: ids"))?;
            check_prefix(c, &["chat:"], "chats[]")?;
            set.insert(c.to_string());
        }
        chats = Some(set);
    }

    let since = match obj.get("since") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            if n.is_i64() && n.as_i64().unwrap() < 0 {
                return Err(RpcError::new(ErrorKind::NotFound, "since negative; nothing to resume from"));
            }
            Some(n.as_u64().ok_or_else(|| RpcError::invalid_params("since must be a non-negative integer"))?)
        }
        Some(_) => return Err(RpcError::invalid_params("since must be a non-negative integer")),
    };

    let latest = eng.events.latest_seq();
    if let Some(s) = since {
        if s > latest && latest > 0 {
            return Err(RpcError::new(ErrorKind::NotFound, "since exceeds latest_seq; nothing to resume from"));
        }
        if s > 0 && latest == 0 {
            return Err(RpcError::new(ErrorKind::NotFound, "replay log empty; nothing to resume from"));
        }
    }

    let outcome = eng
        .events
        .subscribe(sess.conn_id, Filters { kinds, chats }, since)
        .ok_or_else(|| RpcError::internal("connection gone"))?;
    sess.subscriptions.insert(outcome.subscription_id.clone());

    let result = json!({
        "subscription_id": outcome.subscription_id,
        "oldest_seq": outcome.oldest_seq,
        "latest_seq": outcome.latest_seq,
        "replaying": outcome.replaying,
    });

    let events = eng.events.clone();
    let core = eng.core().clone();
    let conn_id = sess.conn_id;
    let sub_id = outcome.subscription_id.clone();
    let latest_seq = outcome.latest_seq;
    let gap = outcome.gap;
    let replayed = outcome.replayed;
    let replay_frames = outcome.replay_frames;
    let tx = sess.out.clone();
    sess.after_response = Some(Box::new(move || {
        tokio::spawn(async move {
            for frame in replay_frames {
                if tx.send(OutMsg::Frame(frame)).await.is_err() {
                    return;
                }
            }
            let summary = core.backlog_summary().await.unwrap_or_else(|e| {
                tracing::warn!(err = %e, "backlog_summary failed; reporting zeros");
                json!({"unread_chats": 0, "pending_contact_requests": 0, "queued_outbound": 0})
            });
            let backlog = crate::rpc::protocol::event_notification(
                "session.backlog",
                json!({
                    "subscription_id": sub_id,
                    "seq": latest_seq,
                    "ts": crate::rpc::events::now_ts(),
                    "replayed": replayed,
                    "gap": gap,
                    "latest_seq": latest_seq,
                    "summary": summary,
                }),
            );
            if tx.send(OutMsg::Frame(backlog.to_string())).await.is_err() {
                return;
            }
            loop {
                let pending = events.end_replay(conn_id, &sub_id);
                if pending.is_empty() {
                    return;
                }
                for frame in pending {
                    if tx.send(OutMsg::Frame(frame)).await.is_err() {
                        return;
                    }
                }
            }
        });
    }));

    Ok(result)
}

fn events_unsubscribe(eng: &Engine, sess: &mut Session, params: Option<Value>) -> Result<Value, RpcError> {
    let p: Value = params.unwrap_or(Value::Null);
    let obj = p.as_object().ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    let sub_id = req_str(obj, "subscription_id")?;
    check_prefix(sub_id, &["sub:"], "subscription_id")?;
    if eng.events.unsubscribe(sess.conn_id, sub_id) {
        sess.subscriptions.remove(sub_id);
        Ok(json!({"subscription_id": sub_id, "closed": true}))
    } else {
        Err(RpcError::new(ErrorKind::NotFound, "no such subscription_id on this connection"))
    }
}

async fn shutdown_request(eng: &Arc<Engine>, sess: &mut Session, params: Option<Value>) -> Result<Value, RpcError> {
    let obj = match params {
        None | Some(Value::Null) => Map::new(),
        Some(v) if v.is_object() => v.as_object().unwrap().clone(),
        Some(_) => return Err(RpcError::invalid_params("params must be an object")),
    };
    let grace_ms = match obj.get("grace_ms") {
        None | Some(Value::Null) => DEFAULT_GRACE_MS,
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| v.min(MAX_GRACE_MS))
            .ok_or_else(|| RpcError::invalid_params("grace_ms must be a non-negative integer"))?,
        Some(_) => return Err(RpcError::invalid_params("grace_ms must be a non-negative integer")),
    };
    eng.core().shutdown(grace_ms).await?;
    eng.begin_shutdown(grace_ms);
    let latest = eng.events.latest_seq();
    let tx = sess.out.clone();
    sess.after_response = Some(Box::new(move || {
        let notice = crate::rpc::protocol::event_notification(
            "session.backlog",
            json!({
                "subscription_id": Value::Null,
                "seq": latest,
                "ts": crate::rpc::events::now_ts(),
                "replayed": 0,
                "gap": false,
                "latest_seq": latest,
                "summary": {"unread_chats": 0, "pending_contact_requests": 0, "queued_outbound": 0},
                "shutting_down": true,
            }),
        );
        let _ = tx.try_send(OutMsg::Frame(notice.to_string()));
    }));
    Ok(json!({"shutting_down": true, "grace_ms": grace_ms}))
}

/// Emit the hello-time backlog notice when the core reports queued outbound
/// work (AGENT-API §5.7).
pub async fn post_hello_backlog(eng: &Arc<Engine>, conn_id: u64) {
    let Ok(summary) = eng.core().backlog_summary().await else { return };
    let queued = summary.get("queued_outbound").and_then(Value::as_u64).unwrap_or(0);
    if queued == 0 {
        return;
    }
    let latest = eng.events.latest_seq();
    let notice = crate::rpc::protocol::event_notification(
        "session.backlog",
        json!({
            "subscription_id": Value::Null,
            "seq": latest,
            "ts": crate::rpc::events::now_ts(),
            "replayed": 0,
            "gap": false,
            "latest_seq": latest,
            "summary": summary,
        }),
    );
    eng.events.push_conn_frame(conn_id, notice.to_string());
}

pub async fn dispatch_method(
    eng: &Arc<Engine>,
    sess: &mut Session,
    method: &str,
    params: Option<Value>,
) -> Result<Value, RpcError> {
    match method {
        "session.hello" => session_hello(eng, sess, params).await,
        "rpc.describe" => rpc_describe(eng),
        "msg.send" => msg_send(eng, params).await,
        "msg.edit" => msg_edit(eng, params).await,
        "msg.delete" => msg_delete(eng, params).await,
        "msg.react" => msg_react(eng, params).await,
        "chat.list" => chat_list(eng, params).await,
        "chat.history" => chat_history(eng, params).await,
        "contact.list" => contact_list(eng, params).await,
        "contact.add" => contact_add(eng, params).await,
        "contact.accept" => contact_accept(eng, params).await,
        "contact.decline" => contact_decline(eng, params).await,
        "invite.create" => invite_create(eng, params).await,
        "invite.accept" => invite_accept(eng, params).await,
        "ident.verify" => ident_verify(eng, params).await,
        "events.subscribe" => events_subscribe(eng, sess, params).await,
        "events.unsubscribe" => events_unsubscribe(eng, sess, params),
        "shutdown.request" => shutdown_request(eng, sess, params).await,
        _ => Err(RpcError::unknown_method()),
    }
}
