use crate::rpc::core::{EventSink, CORE_EVENT_KINDS};
use crate::rpc::protocol::event_notification;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Notify};

pub fn now_ts() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub chat_id: Option<String>,
    pub payload: Value,
}

/// Bounded, ordered replay store. `seq` is global and monotonic per account;
/// implementations evict oldest first. (sqlite impl lands elsewhere.)
pub trait ReplayLog: Send {
    fn append(&mut self, record: EventRecord);
    /// Oldest retained seq; 0 when empty.
    fn oldest_seq(&self) -> u64;
    /// Newest seq; 0 when empty.
    fn latest_seq(&self) -> u64;
    /// Retained events with seq > `since` in seq order.
    fn since(&self, since: u64) -> Vec<EventRecord>;
}

/// Engine::new's default log — used by tests via `#[path]` include;
/// production wires SqliteReplayLog (core.rs) through Engine::with_events.
#[allow(dead_code)]
pub struct InMemoryReplayLog {
    cap: usize,
    events: VecDeque<EventRecord>,
    latest: u64,
}

#[allow(dead_code)]
impl InMemoryReplayLog {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            events: VecDeque::new(),
            latest: 0,
        }
    }
}

impl ReplayLog for InMemoryReplayLog {
    fn append(&mut self, record: EventRecord) {
        self.latest = self.latest.max(record.seq);
        self.events.push_back(record);
        while self.events.len() > self.cap {
            self.events.pop_front();
        }
    }

    fn oldest_seq(&self) -> u64 {
        self.events.front().map(|e| e.seq).unwrap_or(0)
    }

    fn latest_seq(&self) -> u64 {
        self.latest
    }

    fn since(&self, since: u64) -> Vec<EventRecord> {
        self.events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub kinds: Option<HashSet<String>>,
    pub chats: Option<HashSet<String>>,
}

impl Filters {
    fn matches(&self, kind: &str, chat_id: Option<&str>) -> bool {
        if let Some(kinds) = &self.kinds {
            if !kinds.contains(kind) {
                return false;
            }
        }
        if kind == "message.received" || kind == "msg_state" {
            if let Some(chats) = &self.chats {
                return chat_id.map(|c| chats.contains(c)).unwrap_or(false);
            }
        }
        true
    }
}

pub enum OutMsg {
    Frame(String),
    Hangup,
}

pub struct ConnHandle {
    pub tx: mpsc::Sender<OutMsg>,
    pub dead: Arc<AtomicBool>,
    pub wake: Arc<Notify>,
}

impl ConnHandle {
    fn kill(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let _ = self.tx.try_send(OutMsg::Hangup);
    }
}

struct Subscription {
    id: String,
    filters: Filters,
    /// While true, live events buffer in `pending` so replay + backlog stay
    /// ahead of them on the wire (strictly increasing seq per subscription).
    replaying: bool,
    pending: VecDeque<String>,
}

struct ConnState {
    handle: ConnHandle,
    subs: HashMap<String, Subscription>,
}

pub struct SubscribeOutcome {
    pub subscription_id: String,
    pub oldest_seq: u64,
    pub latest_seq: u64,
    pub replaying: bool,
    pub gap: bool,
    /// Already-serialized frames: optional event.gap marker followed by the
    /// filtered replay slice. Sent before the session.backlog marker.
    pub replay_frames: Vec<String>,
    pub replayed: usize,
}

struct ManagerState {
    log: Box<dyn ReplayLog>,
    conns: HashMap<u64, ConnState>,
    queue_cap: usize,
}

pub struct EventManager {
    state: Mutex<ManagerState>,
    next_sub: AtomicU64,
    next_conn: AtomicU64,
}

impl EventManager {
    pub fn new(log: Box<dyn ReplayLog>, queue_cap: usize) -> Self {
        Self {
            state: Mutex::new(ManagerState {
                log,
                conns: HashMap::new(),
                queue_cap,
            }),
            next_sub: AtomicU64::new(1),
            next_conn: AtomicU64::new(1),
        }
    }

    pub fn register_conn(
        &self,
        tx: mpsc::Sender<OutMsg>,
        dead: Arc<AtomicBool>,
        wake: Arc<Notify>,
    ) -> u64 {
        let id = self.next_conn.fetch_add(1, Ordering::SeqCst);
        let handle = ConnHandle { tx, dead, wake };
        self.state.lock().unwrap().conns.insert(
            id,
            ConnState {
                handle,
                subs: HashMap::new(),
            },
        );
        id
    }

    pub fn remove_conn(&self, conn_id: u64) {
        self.state.lock().unwrap().conns.remove(&conn_id);
    }

    pub fn latest_seq(&self) -> u64 {
        self.state.lock().unwrap().log.latest_seq()
    }

    /// Register a subscription and capture the replay slice atomically, so a
    /// concurrent `emit` can never slip between the snapshot and registration.
    /// `since.is_none()` means live-only. Gap check happens in the caller for
    /// error reporting; here `gap` is precomputed.
    pub fn subscribe(
        &self,
        conn_id: u64,
        filters: Filters,
        since: Option<u64>,
    ) -> Option<SubscribeOutcome> {
        let mut st = self.state.lock().unwrap();
        if !st.conns.contains_key(&conn_id) {
            return None;
        }
        let latest = st.log.latest_seq();
        let oldest = st.log.oldest_seq();
        let gap = match since {
            Some(s) => oldest > 0 && s < oldest.saturating_sub(1),
            None => false,
        };
        let id = format!("sub:{}", self.next_sub.fetch_add(1, Ordering::SeqCst));
        let mut replay_frames = Vec::new();
        if let Some(s) = since {
            if gap {
                let params = json!({
                    "subscription_id": id,
                    "seq": oldest,
                    "ts": now_ts(),
                    "requested_since": s,
                    "oldest_available": oldest,
                    "lost": "unknown",
                });
                replay_frames.push(event_notification("event.gap", params).to_string());
            }
            for rec in st.log.since(s) {
                if !filters.matches(&rec.kind, rec.chat_id.as_deref()) {
                    continue;
                }
                let mut params = Map::new();
                params.insert("subscription_id".into(), json!(id));
                params.insert("seq".into(), json!(rec.seq));
                params.insert("ts".into(), json!(rec.ts));
                if let Value::Object(extra) = rec.payload {
                    params.extend(extra);
                }
                replay_frames
                    .push(event_notification(&rec.kind, Value::Object(params)).to_string());
            }
        }
        let replayed = replay_frames.len() - usize::from(gap);
        let sub = Subscription {
            id: id.clone(),
            filters,
            replaying: true,
            pending: VecDeque::new(),
        };
        st.conns
            .get_mut(&conn_id)
            .unwrap()
            .subs
            .insert(id.clone(), sub);
        Some(SubscribeOutcome {
            subscription_id: id,
            oldest_seq: oldest,
            latest_seq: latest,
            replaying: since.is_some(),
            gap,
            replay_frames,
            replayed,
        })
    }

    /// After replay frames + the backlog marker have been sent: move the sub
    /// live. Returns buffered pending frames (which the caller must still
    /// send, in order, after the backlog marker). Subsequent calls return
    /// empty once the sub is live and the buffer is drained.
    pub fn end_replay(&self, conn_id: u64, sub_id: &str) -> Vec<String> {
        let mut st = self.state.lock().unwrap();
        let Some(conn) = st.conns.get_mut(&conn_id) else {
            return Vec::new();
        };
        let Some(sub) = conn.subs.get_mut(sub_id) else {
            return Vec::new();
        };
        let drained: Vec<String> = sub.pending.drain(..).collect();
        if drained.is_empty() {
            sub.replaying = false;
        }
        drained
    }

    pub fn unsubscribe(&self, conn_id: u64, sub_id: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        st.conns
            .get_mut(&conn_id)
            .map(|c| c.subs.remove(sub_id).is_some())
            .unwrap_or(false)
    }

    /// Emit `session.backlog`-style control frames built by the caller into a
    /// specific connection's queue. Full queue kills that connection only.
    pub fn push_conn_frame(&self, conn_id: u64, frame: String) {
        let st = self.state.lock().unwrap();
        if let Some(conn) = st.conns.get(&conn_id) {
            if conn.handle.tx.try_send(OutMsg::Frame(frame)).is_err() {
                conn.handle.kill();
            }
        }
    }

    fn fanout(&self, st: &mut ManagerState, record: &EventRecord) {
        let queue_cap = st.queue_cap;
        let mut kill_list = Vec::new();
        for (conn_id, conn) in st.conns.iter_mut() {
            for sub in conn.subs.values_mut() {
                if !sub.filters.matches(&record.kind, record.chat_id.as_deref()) {
                    continue;
                }
                let mut params = Map::new();
                params.insert("subscription_id".into(), json!(sub.id));
                params.insert("seq".into(), json!(record.seq));
                params.insert("ts".into(), json!(record.ts));
                if let Value::Object(extra) = &record.payload {
                    params.extend(extra.clone());
                }
                let frame = event_notification(&record.kind, Value::Object(params)).to_string();
                if sub.replaying {
                    if sub.pending.len() >= queue_cap {
                        kill_list.push(*conn_id);
                        break;
                    }
                    sub.pending.push_back(frame);
                } else if conn.handle.tx.try_send(OutMsg::Frame(frame)).is_err() {
                    kill_list.push(*conn_id);
                    break;
                }
            }
        }
        for cid in kill_list {
            if let Some(conn) = st.conns.get_mut(&cid) {
                conn.handle.kill();
                conn.subs.clear();
            }
        }
    }
}

impl EventSink for EventManager {
    /// Never blocks: seq assignment, log append, and fanout are bounded work.
    /// A saturated subscriber queue kills that connection only.
    fn emit(&self, kind: &str, chat_id: Option<&str>, payload: Value) {
        if !CORE_EVENT_KINDS.contains(&kind) {
            tracing::warn!(kind, "dropping emit of non-catalog event kind");
            return;
        }
        let mut st = self.state.lock().unwrap();
        let seq = st.log.latest_seq() + 1;
        let record = EventRecord {
            seq,
            ts: now_ts(),
            kind: kind.to_string(),
            chat_id: chat_id.map(str::to_string),
            payload,
        };
        st.log.append(record.clone());
        self.fanout(&mut st, &record);
    }
}
