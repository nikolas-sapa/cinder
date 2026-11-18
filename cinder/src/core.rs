//! The live messenger core: owns the identity, contacts pin store, sqlite
//! store, the relay link, and all Noise `Session`s. The RPC engine and the
//! TUI both drive this one object; the pump task is the only owner of the
//! `RelayTransport`, and `Session`s are never serialized.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

use cinder_core::contacts::{ContactStore, MailboxGrant, TrustState};
use cinder_core::error::Error as NetErr;
use cinder_core::frame::{wire_deserialize, wire_serialize};
use cinder_core::handshake_flow;
use cinder_core::identity::Identity;
use cinder_core::proto::{
    ClientMsg, DepositEnvelope, ErrorCode, HandshakeMsg, HsId, InviteToken, MailboxId, PeerId,
    SessionId, Token, TokenHash,
};
use cinder_core::session::Session;
use cinder_core::transport::{ClientEvent, Link, RelayTransport};

use crate::rpc::core::{
    ChatHistoryParams, ChatListParams, CinderCore, ContactAddParams, ContactIdParams,
    ContactListParams, EventSink, IdentVerifyParams, InviteAcceptParams, InviteCreateParams,
    MsgDeleteParams, MsgEditParams, MsgReactParams, MsgSendParams,
};
use crate::rpc::events::{EventRecord, ReplayLog};
use crate::rpc::protocol::{ErrorKind, RpcError};
use crate::store::{Dir, MsgKind, MsgState, NewMessage, Store, StoreError};
use crate::tui::backend as tb;

const PENDING_OUT_CAP: usize = 64;
const CMD_CHANNEL_CAP: usize = 256;
const HS_CHANNEL_CAP: usize = 8;
const INBOUND_CAP: usize = 64;
/// Concurrent inbound responder tasks (Msg1 flood bound); per-peer dedup is
/// enforced separately — excess Msg1s expire relay-side at the §12 deadline.
const RESPONDER_CAP: usize = 64;
/// Total frames parked in `early` across all peers (per-peer cap is 8).
const EARLY_GLOBAL_CAP: usize = 4096;
/// Wire + store cap for reaction emoji (§2 plaintext budget is far larger;
/// 32 B covers any real emoji cluster).
const EMOJI_MAX_BYTES: usize = 32;
/// §11 re-handshake backoff steps; exhausted → the add stays `failed`.
const RETRY_STEPS_MS: [i64; 5] = [1_000, 2_000, 5_000, 15_000, 60_000];
const PING_EVERY: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const META_KEY: &str = "contact_meta.v1";
const MBOX_KEY: &str = "mailbox.v1";
const RELAY_KEY: &str = "relay_addr";
const INVITE_TTL_SECS: u32 = 24 * 3600;
const MAILBOX_TTL_SECS: u32 = 0;
const MAILBOX_FETCH_PAGE: u16 = 500;
const REQ_TIMEOUT: Duration = Duration::from_secs(15);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rfc3339(ts_ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

fn b32(data: &[u8]) -> String {
    data_encoding::BASE32_NOPAD.encode(data).to_lowercase()
}

fn b32_decode_32(s: &str) -> Option<[u8; 32]> {
    data_encoding::BASE32_NOPAD
        .decode(s.to_uppercase().as_bytes())
        .ok()?
        .try_into()
        .ok()
}

pub fn peer_str(p: &PeerId) -> String {
    format!("peer:{}", b32(p.as_bytes()))
}

pub fn contact_str(p: &PeerId) -> String {
    format!("contact:{}", b32(p.as_bytes()))
}

pub fn chat_str(p: &PeerId) -> String {
    format!("chat:{}", b32(p.as_bytes()))
}

fn new_msg_id() -> String {
    format!("msg:{}", b32(HsId::random().as_bytes()))
}

fn new_invite_id() -> String {
    format!("inv:{}", b32(&HsId::random().as_bytes()[..8]))
}

/// Typed relay-op error so callers can match on the code (e.g.
/// `MailboxExists` on re-open) instead of parsing strings.
#[derive(Debug, thiserror::Error)]
#[error("relay {0:?}: {1}")]
struct RelayError(ErrorCode, String);

/// Short `failed:<reason>` substate tag for a handshake/transport error.
fn net_err_reason(e: &NetErr) -> &'static str {
    match e {
        NetErr::HandshakeTimeout => "timeout",
        NetErr::PeerOffline => "peer_offline",
        NetErr::Protocol(m) if m.contains("InviteInvalid") => "invite_invalid",
        _ => "peer_offline",
    }
}

fn link_down() -> NetErr {
    NetErr::Transport("relay link is down".into())
}

fn token_hash(t: &Token) -> TokenHash {
    TokenHash::from(<[u8; 32]>::from(Sha256::digest(t.as_bytes())))
}

/// Application payload carried INSIDE the Noise channel (§11): bincode
/// serialized, then `Session::encrypt` pads it. The relay only ever sees the
/// outer `Transport` frame.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum AppMsg {
    Text {
        msg_id: String,
        body: String,
    },
    Edit {
        msg_id: String,
        body: String,
    },
    Delete {
        msg_id: String,
    },
    React {
        msg_id: String,
        emoji: Option<String>,
    },
    /// Mailbox capability grant (§8): our mailbox + raw deposit token.
    Grant {
        mailbox_id: [u8; 32],
        deposit_token: [u8; 32],
    },
    /// In-band delivery acknowledgment (§11): flips sent->delivered.
    Ack {
        msg_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaState {
    PendingOut,
    PendingIn,
    Active,
    Declined,
    Blocked,
}

impl MetaState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingOut => "pending_out",
            Self::PendingIn => "pending_in",
            Self::Active => "active",
            Self::Declined => "declined",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerGrant {
    mailbox_id: [u8; 32],
    deposit_token: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContactMeta {
    state: MetaState,
    #[serde(default)]
    substate: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    created_at: i64,
    #[serde(default)]
    sas_emoji: Option<String>,
    #[serde(default)]
    sas_digits: Option<String>,
    #[serde(default)]
    nagged_sas: Option<String>,
    #[serde(default)]
    grant: Option<PeerGrant>,
    #[serde(default)]
    grant_sent: bool,
    /// Failed add attempts so far (drives RETRY_STEPS_MS backoff).
    #[serde(default)]
    retry_count: u32,
    /// Wall-clock ms when the next auto retry is allowed; None = none armed.
    #[serde(default)]
    retry_after_ms: Option<i64>,
}

impl ContactMeta {
    fn new(state: MetaState, display_name: Option<String>) -> Self {
        Self {
            state,
            substate: None,
            display_name,
            created_at: now_ms(),
            sas_emoji: None,
            sas_digits: None,
            nagged_sas: None,
            grant: None,
            grant_sent: false,
            retry_count: 0,
            retry_after_ms: None,
        }
    }
}

#[derive(Clone)]
struct OwnMailbox {
    id: MailboxId,
    fetch_token: Token,
    deposit_token: Token,
}

#[derive(Serialize, Deserialize)]
struct MailboxPersist {
    mailbox_id: [u8; 32],
    fetch_token: [u8; 32],
    deposit_token: [u8; 32],
}

struct PendingMsg {
    app: AppMsg,
    msg_id: Option<String>,
    client_msg_id: Option<String>,
    chat_id: String,
}

struct PeerSession {
    session: Mutex<Session>,
    /// OUR wire id — stamped on every outbound Transport/deposit envelope.
    session_id: SessionId,
    /// THEIR wire id — learned from the first inbound frame for this
    /// session (relay-stamps `from`, so the binding is authentic). One-shot:
    /// a frame under any other sid while bound is dropped without decrypt.
    remote_sid: Mutex<Option<SessionId>>,
}

struct Shared {
    sessions: HashMap<PeerId, Arc<PeerSession>>,
    sid_index: HashMap<SessionId, PeerId>,
    /// Sids that can never be legitimately bound again: remote ids of
    /// replaced/dropped sessions, decrypt-fatal streams, and frames that
    /// arrived while bound to a different sid. Noise transport nonces are
    /// sequential, so a partially-consumed stream can never be decrypted
    /// later — a tombstoned frame is dropped BEFORE it can poison a fresh
    /// session with a fatal decrypt.
    dead_sids: HashMap<SessionId, Instant>,
    /// Frames that arrived for a peer whose session install is still in
    /// flight (responder finish vs. relay delivery race). Drained in order
    /// when `install_session` runs — never bound blindly: each frame still
    /// goes through the normal dead-sid/bind/decrypt path.
    early: HashMap<PeerId, VecDeque<(SessionId, Vec<u8>)>>,
    /// Frames parked in `early` across ALL peers — bounds the fake-peer flood
    /// (each identity is free to mint, the per-peer cap alone is not a bound).
    early_total: usize,
    /// Inbound responder drivers in flight, keyed by the frame's claimed
    /// peer-id: one responder per peer + RESPONDER_CAP globally.
    responders: HashSet<PeerId>,
    /// Peers currently inside on_session_ready (post-handshake install
    /// window) — part of the `early` parking predicate.
    installing: HashSet<PeerId>,
    /// Frames/messages dropped because the contact is declined or blocked —
    /// counted so the drop is observable, not silent.
    dropped_inbound: u64,
    pending_out: HashMap<PeerId, VecDeque<PendingMsg>>,
    hs_inflight: HashSet<PeerId>,
    meta: HashMap<PeerId, ContactMeta>,
    presence: HashMap<PeerId, bool>,
    own_mailbox: Option<OwnMailbox>,
    /// Outstanding rendezvous invites we created; an inbound Msg1 while one is
    /// open is attributed `via: invite` and consumes it.
    invites_open: Vec<(InviteToken, String)>,
    conn: tb::ConnState,
    shutting_down: bool,
}

/// Response waiters completed by the pump. Request ops are serialized by
/// `op_lock`, so each slot holds at most one pending request.
#[derive(Default)]
struct ReqWaiters {
    invite: Option<oneshot::Sender<ClientEvent>>,
    mbox_open: Option<oneshot::Sender<ClientEvent>>,
    deposit: Option<oneshot::Sender<ClientEvent>>,
    fetch: Option<mpsc::Sender<ClientEvent>>,
    ok: Option<oneshot::Sender<ClientEvent>>,
}

enum ReqSlot {
    Invite,
    MailboxOpen,
    Deposit,
    Ok,
}

struct LinkHub {
    cmd_tx: mpsc::Sender<ClientMsg>,
    cmd_rx: Mutex<Option<mpsc::Receiver<ClientMsg>>>,
    hs_waiters: Arc<Mutex<HashMap<HsId, mpsc::Sender<ClientEvent>>>>,
    inbound_tx: mpsc::Sender<ClientEvent>,
    reqs: Mutex<ReqWaiters>,
}

impl LinkHub {
    /// Fail every in-flight handshake waiter and request slot with `ev`.
    /// Used when the link dies or the relay reports a connection-fatal code.
    fn fail_all(&self, ev: &ClientEvent) {
        for tx in self
            .hs_waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            let _ = tx.try_send(ev.clone());
        }
        let mut reqs = self.reqs.lock().unwrap_or_else(|e| e.into_inner());
        for slot in [
            reqs.invite.take(),
            reqs.mbox_open.take(),
            reqs.deposit.take(),
            reqs.ok.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = slot.send(ev.clone());
        }
        if let Some(tx) = reqs.fetch.take() {
            let _ = tx.try_send(ev.clone());
        }
    }
}

/// Per-handshake `Link`: outbound frames go through the shared cmd channel,
/// inbound handshake frames are demuxed by hs_id into this link's channel.
struct DemuxLink {
    cmd: mpsc::Sender<ClientMsg>,
    me: mpsc::Sender<ClientEvent>,
    rx: mpsc::Receiver<ClientEvent>,
    waiters: Arc<Mutex<HashMap<HsId, mpsc::Sender<ClientEvent>>>>,
    registered: Vec<HsId>,
}

impl DemuxLink {
    fn new(
        cmd: mpsc::Sender<ClientMsg>,
        waiters: Arc<Mutex<HashMap<HsId, mpsc::Sender<ClientEvent>>>>,
    ) -> Self {
        let (me, rx) = mpsc::channel(HS_CHANNEL_CAP);
        Self {
            cmd,
            me,
            rx,
            waiters,
            registered: Vec::new(),
        }
    }

    fn seed(&mut self, ev: ClientEvent) {
        let _ = self.me.try_send(ev);
    }

    fn register(&mut self, hs_id: HsId) {
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hs_id, self.me.clone());
        self.registered.push(hs_id);
    }
}

impl Drop for DemuxLink {
    fn drop(&mut self) {
        let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        for id in self.registered.drain(..) {
            w.remove(&id);
        }
    }
}

impl Link for DemuxLink {
    async fn send_handshake(
        &mut self,
        to: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    ) -> std::result::Result<(), NetErr> {
        self.register(hs_id);
        self.cmd
            .send(ClientMsg::Handshake { to, hs_id, body })
            .await
            .map_err(|_| link_down())
    }

    async fn send_invite_accept(
        &mut self,
        token: InviteToken,
        hs_id: HsId,
        msg1: Vec<u8>,
    ) -> std::result::Result<(), NetErr> {
        self.register(hs_id);
        self.cmd
            .send(ClientMsg::InviteAccept { token, hs_id, msg1 })
            .await
            .map_err(|_| link_down())
    }

    async fn send_transport(
        &mut self,
        to: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) -> std::result::Result<(), NetErr> {
        self.cmd
            .send(ClientMsg::Transport {
                to,
                session_id,
                body,
            })
            .await
            .map_err(|_| link_down())
    }

    async fn recv(&mut self) -> std::result::Result<ClientEvent, NetErr> {
        match self.rx.recv().await {
            Some(ClientEvent::Error { code, detail }) => {
                Err(NetErr::Protocol(format!("relay error {code:?}: {detail}")))
            }
            Some(ev) => Ok(ev),
            None => Err(link_down()),
        }
    }
}

/// Relay-side InviteAccept fix: the relay forwards the (serialized)
/// `HandshakeMsg::Msg1` verbatim inside `HandshakeMsg::Msg1`, so an
/// invite-originated Msg1 arrives double-wrapped. Unwrap one serialization
/// layer when the payload parses as a HandshakeMsg; raw-noise payloads never
/// parse, so both wire layouts interop correctly.
fn normalize_handshake(msg: HandshakeMsg) -> HandshakeMsg {
    if let HandshakeMsg::Msg1(payload) = &msg {
        if let Ok(inner @ HandshakeMsg::Msg1(_)) = wire_deserialize::<HandshakeMsg>(payload) {
            return inner;
        }
    }
    msg
}

pub struct Core {
    me: Weak<Core>,
    identity: Arc<Identity>,
    store: Arc<Store>,
    contacts: Arc<Mutex<ContactStore>>,
    shared: Mutex<Shared>,
    link: Arc<LinkHub>,
    event_sink: RwLock<Option<Arc<dyn EventSink>>>,
    ui_tx: broadcast::Sender<tb::Event>,
    /// Serializes relay request ops. NOT reentrant: `request()` takes it, so
    /// any caller holding it must drop the guard before calling `request()`.
    op_lock: tokio::sync::Mutex<()>,
    send_locks: Mutex<HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>>,
    shutdown: Notify,
    relay_host: Mutex<Option<String>>,
    peer_id_str: String,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Core {
    /// Opens the core: sqlite store + contacts + persisted meta, then (when
    /// `relay` is set) the reconnect/pump task and the inbound task.
    pub async fn open(
        data_dir: &Path,
        relay: Option<(SocketAddr, String)>,
        identity: Identity,
    ) -> Result<Arc<Core>> {
        let secret = identity.private();
        let dir = data_dir.to_path_buf();
        let (store, contacts) = tokio::task::spawn_blocking(move || {
            let sec = secret.expose_secret();
            let store = Store::open(&dir, sec).map_err(|e| anyhow!("store: {e}"))?;
            let contacts = ContactStore::open(&dir, sec).map_err(|e| anyhow!("contacts: {e}"))?;
            Ok::<_, anyhow::Error>((store, contacts))
        })
        .await??;

        let store = Arc::new(store);
        let meta: HashMap<PeerId, ContactMeta> = {
            let s = store.clone();
            tokio::task::spawn_blocking(move || {
                s.meta(META_KEY)
                    .ok()
                    .flatten()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, ContactMeta>>(&raw).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(k, v)| b32_decode_32(&k).map(|b| (PeerId::from_bytes(b), v)))
                    .collect()
            })
            .await?
        };
        let own_mailbox = {
            let s = store.clone();
            tokio::task::spawn_blocking(move || {
                s.meta(MBOX_KEY)
                    .ok()
                    .flatten()
                    .and_then(|raw| serde_json::from_str::<MailboxPersist>(&raw).ok())
                    .map(|m| OwnMailbox {
                        id: MailboxId::from_bytes(m.mailbox_id),
                        fetch_token: Token::from_bytes(m.fetch_token),
                        deposit_token: Token::from_bytes(m.deposit_token),
                    })
            })
            .await?
        };
        // `--relay` / `CINDER_RELAY` win; else the last address that
        // successfully connected (persisted by the link loop).
        let relay = match relay {
            Some(r) => Some(r),
            None => {
                let s = store.clone();
                let persisted: Option<SocketAddr> = tokio::task::spawn_blocking(move || {
                    s.meta(RELAY_KEY)
                        .ok()
                        .flatten()
                        .and_then(|a| a.parse().ok())
                })
                .await?;
                persisted.map(|a| (a, a.to_string()))
            }
        };

        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAP);
        let (ui_tx, _) = broadcast::channel(256);
        let identity = Arc::new(identity);
        let peer_id_str = peer_str(&identity.peer_id());

        let core = Arc::new_cyclic(|me: &Weak<Core>| Core {
            me: me.clone(),
            identity,
            store,
            contacts: Arc::new(Mutex::new(contacts)),
            shared: Mutex::new(Shared {
                sessions: HashMap::new(),
                sid_index: HashMap::new(),
                dead_sids: HashMap::new(),
                early: HashMap::new(),
                early_total: 0,
                responders: HashSet::new(),
                installing: HashSet::new(),
                dropped_inbound: 0,
                pending_out: HashMap::new(),
                hs_inflight: HashSet::new(),
                meta,
                presence: HashMap::new(),
                own_mailbox,
                invites_open: Vec::new(),
                conn: tb::ConnState::Offline,
                shutting_down: false,
            }),
            link: Arc::new(LinkHub {
                cmd_tx,
                cmd_rx: Mutex::new(Some(cmd_rx)),
                hs_waiters: Arc::new(Mutex::new(HashMap::new())),
                inbound_tx,
                reqs: Mutex::new(ReqWaiters::default()),
            }),
            event_sink: RwLock::new(None),
            ui_tx,
            op_lock: tokio::sync::Mutex::new(()),
            send_locks: Mutex::new(HashMap::new()),
            shutdown: Notify::new(),
            relay_host: Mutex::new(None),
            peer_id_str,
            tasks: Mutex::new(Vec::new()),
        });

        if let Some((addr, host)) = relay {
            *core.relay_host.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
            core.tasks.lock().unwrap_or_else(|e| e.into_inner()).push({
                let c = core.clone();
                tokio::spawn(async move { c.link_loop(addr).await })
            });
        }
        core.tasks.lock().unwrap_or_else(|e| e.into_inner()).push({
            let c = core.clone();
            tokio::spawn(async move { c.inbound_loop(inbound_rx).await })
        });
        Ok(core)
    }

    fn arc(&self) -> Arc<Core> {
        self.me.upgrade().expect("core arc dead")
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id_str
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn relay_host(&self) -> Option<String> {
        self.relay_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn conn_state(&self) -> tb::ConnState {
        self.sh().conn
    }

    pub fn has_live_session(&self, peer: &PeerId) -> bool {
        self.session(peer).is_some()
    }

    pub fn meta_state(&self, peer: &PeerId) -> Option<MetaState> {
        self.sh().meta.get(peer).map(|m| m.state)
    }

    /// Attach the engine's event manager (or a test sink). Called once after
    /// the engine exists; before that, events only reach the UI broadcast.
    pub fn attach_sink(&self, sink: Arc<dyn EventSink>) {
        *self.event_sink.write().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    pub fn ui_subscribe(&self) -> broadcast::Receiver<tb::Event> {
        self.ui_tx.subscribe()
    }

    /// Wait until the relay link reports Connected (or `dur` elapses).
    pub async fn wait_connected(&self, dur: Duration) -> bool {
        let step = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        while waited < dur {
            if self.conn_state() == tb::ConnState::Connected {
                return true;
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
        self.conn_state() == tb::ConnState::Connected
    }

    /// Wait until the contact meta for `peer` reaches `state` (or `dur`).
    pub async fn wait_meta(&self, peer: &PeerId, state: MetaState, dur: Duration) -> bool {
        let step = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        while waited < dur {
            if self.meta_state(peer) == Some(state) {
                return true;
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
        self.meta_state(peer) == Some(state)
    }

    // -- test-only seams (compiled into the crate only under cfg(test)) -----

    #[cfg(test)]
    pub fn test_substate(&self, peer: &PeerId) -> Option<String> {
        self.sh().meta.get(peer).and_then(|m| m.substate.clone())
    }

    #[cfg(test)]
    pub fn test_set_state(&self, peer: &PeerId, state: MetaState) {
        {
            let mut sh = self.sh();
            let m = sh
                .meta
                .entry(*peer)
                .or_insert_with(|| ContactMeta::new(state, None));
            m.state = state;
        }
        self.persist_meta();
    }

    #[cfg(test)]
    pub fn test_dropped_inbound(&self) -> u64 {
        self.sh().dropped_inbound
    }

    #[cfg(test)]
    pub fn test_presence(&self, peer: &PeerId) -> Option<bool> {
        self.sh().presence.get(peer).copied()
    }

    #[cfg(test)]
    pub fn test_has_grant(&self, peer: &PeerId) -> bool {
        self.sh()
            .meta
            .get(peer)
            .and_then(|m| m.grant.as_ref())
            .is_some()
    }

    #[cfg(test)]
    pub fn test_pending_len(&self, peer: &PeerId) -> usize {
        self.sh()
            .pending_out
            .get(peer)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub async fn test_handle_app(self: &Arc<Self>, peer: PeerId, app: AppMsg) {
        self.handle_app(peer, app).await;
    }

    fn emit(&self, kind: &str, chat_id: Option<&str>, payload: Value) {
        if let Some(ev) = ui_event(kind, &payload) {
            let _ = self.ui_tx.send(ev);
        }
        if let Some(s) = self
            .event_sink
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            s.emit(kind, chat_id, payload);
        }
    }

    fn set_conn(&self, to: tb::ConnState) {
        let mut sh = self.sh();
        if sh.conn == to {
            return;
        }
        sh.conn = to;
        drop(sh);
        let _ = self.ui_tx.send(tb::Event::ConnChanged(to));
    }

    /// Store ops run on the blocking pool; a panicking task becomes an
    /// `Internal` error instead of killing the calling async task.
    async fn sblk<T, F>(&self, f: F) -> std::result::Result<T, StoreError>
    where
        F: FnOnce(&Store) -> std::result::Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .unwrap_or_else(|_| Err(StoreError::Internal("store task panicked")))
    }

    /// Contacts ops on the blocking pool; `None` means the task panicked —
    /// callers pick a fail-safe default. Mutex poisoning is recovered rather
    /// than propagated (the store's own writes are atomic).
    async fn cblk<T, F>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&mut ContactStore) -> T + Send + 'static,
        T: Send + 'static,
    {
        let contacts = self.contacts.clone();
        tokio::task::spawn_blocking(move || {
            let mut g = contacts.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut g)
        })
        .await
        .ok()
    }

    /// `shared` lock that survives a poisoned mutex: a panic in one task
    /// must not wedge every later caller.
    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist_meta(&self) {
        let raw = {
            let sh = self.sh();
            let map: HashMap<String, &ContactMeta> = sh
                .meta
                .iter()
                .map(|(p, m)| (b32(p.as_bytes()), m))
                .collect();
            serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
        };
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _ = store.set_meta(META_KEY, &raw);
        });
    }

    fn set_meta(&self, peer: &PeerId, f: impl FnOnce(&mut ContactMeta)) -> bool {
        let mut sh = self.sh();
        match sh.meta.get_mut(peer) {
            Some(m) => {
                f(m);
                drop(sh);
                self.persist_meta();
                true
            }
            None => false,
        }
    }

    // ----- link pump ---------------------------------------------------------

    async fn link_loop(self: &Arc<Self>, addr: SocketAddr) {
        let mut wait = RECONNECT_MIN;
        loop {
            if self.shutdown_requested() {
                break;
            }
            match RelayTransport::connect(addr, self.identity.peer_id()).await {
                Ok(link) => {
                    self.set_conn(tb::ConnState::Connected);
                    {
                        let s = self.store.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = s.set_meta(RELAY_KEY, &addr.to_string());
                        });
                    }
                    let setup = {
                        let c = self.clone();
                        tokio::spawn(async move { c.on_link_up().await })
                    };
                    self.pump(link).await;
                    setup.abort();
                    self.on_link_down();
                    wait = RECONNECT_MIN;
                }
                Err(_) => {
                    self.set_conn(tb::ConnState::Connecting);
                }
            }
            if self.shutdown_requested() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.shutdown.notified() => break,
            }
            wait = (wait * 2).min(RECONNECT_MAX);
        }
    }

    async fn pump(self: &Arc<Self>, mut link: RelayTransport) {
        let mut cmd_rx = match self
            .link
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            Some(rx) => rx,
            None => return,
        };
        let mut ping = tokio::time::interval(PING_EVERY);
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => break,
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(m) => if link.send(m).await.is_err() { break; },
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    if link.send(ClientMsg::Ping { nonce: now_unix() }).await.is_err() {
                        break;
                    }
                }
                ev = link.recv() => {
                    match ev {
                        Ok(ev) => self.route(ev).await,
                        Err(_) => break,
                    }
                }
            }
        }
        self.link
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(cmd_rx);
    }

    async fn route(self: &Arc<Self>, ev: ClientEvent) {
        match ev {
            ClientEvent::Handshake { from, hs_id, body } => {
                let body = normalize_handshake(body);
                let waiter = self
                    .link
                    .hs_waiters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&hs_id)
                    .cloned();
                if let Some(tx) = waiter {
                    let _ = tx.send(ClientEvent::Handshake { from, hs_id, body }).await;
                } else if matches!(body, HandshakeMsg::Msg1(_)) {
                    let _ =
                        self.link
                            .inbound_tx
                            .try_send(ClientEvent::Handshake { from, hs_id, body });
                }
            }
            ClientEvent::Transport {
                from,
                session_id,
                body,
            } => self.handle_transport(from, session_id, body).await,
            ClientEvent::InviteCreated { .. } => {
                if let Some(tx) = self
                    .link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .invite
                    .take()
                {
                    let _ = tx.send(ev);
                }
            }
            ClientEvent::MailboxOpened { .. } => {
                if let Some(tx) = self
                    .link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mbox_open
                    .take()
                {
                    let _ = tx.send(ev);
                }
            }
            ClientEvent::MailboxDeposited { .. } => {
                if let Some(tx) = self
                    .link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .deposit
                    .take()
                {
                    let _ = tx.send(ev);
                }
            }
            ClientEvent::MailboxMessages { more, .. } => {
                let tx = self
                    .link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .fetch
                    .clone();
                if let Some(tx) = tx {
                    let done = !more;
                    let _ = tx.send(ev).await;
                    if done {
                        self.link
                            .reqs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .fetch = None;
                    }
                }
            }
            ClientEvent::Ok => {
                if let Some(tx) = self
                    .link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ok
                    .take()
                {
                    let _ = tx.send(ev);
                }
            }
            ClientEvent::PeerStatus { peer, online } => {
                let kick = {
                    let mut sh = self.sh();
                    sh.presence.insert(peer, online);
                    online
                        && sh
                            .pending_out
                            .get(&peer)
                            .map(|q| !q.is_empty())
                            .unwrap_or(false)
                };
                if kick {
                    self.ensure_handshake(peer);
                }
            }
            ClientEvent::Error { code, detail } => self.route_error(code, detail),
            ClientEvent::ServerHello { .. } | ClientEvent::Pong { .. } => {}
        }
    }

    /// Relay Error frames carry no op correlator, so they are routed by code
    /// domain (§10 close table): connection-fatal codes kill every waiter
    /// (the link is about to close), handshake-domain codes abort pending
    /// handshakes only, and op-domain codes answer whichever request op is
    /// currently in flight (ops serialize on op_lock, so at most one is).
    /// An Error matching no pending op is dropped — never leaked into an
    /// unrelated handshake or request.
    fn route_error(&self, code: ErrorCode, detail: String) {
        let ev = ClientEvent::Error { code, detail };
        match code {
            ErrorCode::VersionMismatch
            | ErrorCode::OversizeFrame
            | ErrorCode::ProtocolViolation
            | ErrorCode::PeerIdInUse => {
                self.link.fail_all(&ev);
            }
            ErrorCode::HandshakeTimeout | ErrorCode::InviteInvalid => {
                for tx in self
                    .link
                    .hs_waiters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                {
                    let _ = tx.try_send(ev.clone());
                }
            }
            _ => {
                let mut reqs = self.link.reqs.lock().unwrap_or_else(|e| e.into_inner());
                let slot = [
                    reqs.invite.take(),
                    reqs.mbox_open.take(),
                    reqs.deposit.take(),
                    reqs.ok.take(),
                ]
                .into_iter()
                .flatten()
                .next();
                if let Some(tx) = slot {
                    let _ = tx.send(ev);
                } else if let Some(tx) = reqs.fetch.take() {
                    let _ = tx.try_send(ev);
                } else if code == ErrorCode::RateLimited {
                    // A rate-limit burst most plausibly answers a handshake
                    // initiation (Msg1s are the highest-volume send).
                    drop(reqs);
                    for tx in self
                        .link
                        .hs_waiters
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .values()
                    {
                        let _ = tx.try_send(ev.clone());
                    }
                } else {
                    tracing::debug!(?code, "relay error with no pending op; dropped");
                }
            }
        }
    }

    async fn on_link_up(self: &Arc<Self>) {
        if self.ensure_mailbox().await.is_ok() {
            let _ = self.mailbox_drain().await;
        }
        let _ = self.presence_subscribe().await;

        // §11: on reconnect, re-arm outbound work — pending adds retry at
        // their backoff deadlines (or immediately if still `connecting`),
        // and active contacts with a non-empty pending queue get a fresh
        // handshake so the backlog drains (sessions died with the old link).
        let kicks: Vec<PeerId> = {
            let sh = self.sh();
            let now = now_ms();
            sh.meta
                .iter()
                .filter(|(p, m)| {
                    let queued = sh
                        .pending_out
                        .get(*p)
                        .map(|q| !q.is_empty())
                        .unwrap_or(false);
                    (m.state == MetaState::PendingOut
                        && match m.retry_after_ms {
                            Some(t) => t <= now,
                            None => m
                                .substate
                                .as_deref()
                                .map(|s| s == "connecting")
                                .unwrap_or(true),
                        })
                        || (m.state == MetaState::Active && queued)
                })
                .map(|(p, _)| *p)
                .collect()
        };
        if kicks.is_empty() {
            return;
        }
        if let Ok(summary) = self.backlog_summary().await {
            self.emit("session.backlog", None, summary);
        }
        for peer in kicks {
            // A still-live session needs no re-handshake — flush directly.
            if self.session(&peer).is_some() {
                self.flush_pending(peer).await;
            } else {
                self.ensure_handshake(peer);
            }
        }
    }

    fn on_link_down(self: &Arc<Self>) {
        self.set_conn(tb::ConnState::Offline);
        // §11: transport death is session death — no Noise session resumes
        // over a fresh TCP connection.
        {
            let mut sh = self.sh();
            // Every remote sid dies with the link: tombstone them so stale
            // mailbox/live frames can't bind to post-reconnect sessions.
            let dead: Vec<SessionId> = sh
                .sessions
                .values()
                .filter_map(|ps| *ps.remote_sid.lock().unwrap_or_else(|e| e.into_inner()))
                .collect();
            for rs in dead {
                Self::tombstone(&mut sh, rs);
            }
            sh.sessions.clear();
            sh.sid_index.clear();
            sh.early.clear();
            sh.early_total = 0;
            sh.presence.clear();
        }
        self.link.fail_all(&ClientEvent::Error {
            code: ErrorCode::ProtocolViolation,
            detail: "relay link lost".into(),
        });
    }

    fn shutdown_requested(&self) -> bool {
        self.sh().shutting_down
    }

    /// Public shutdown trigger for main/daemon paths.
    pub fn begin_shutdown(&self) {
        self.sh().shutting_down = true;
        self.shutdown.notify_waiters();
    }

    // ----- request ops (serialized by op_lock) --------------------------------

    async fn request(&self, msg: ClientMsg, slot: ReqSlot) -> Result<ClientEvent> {
        let _g = self.op_lock.lock().await;
        let (tx, rx) = oneshot::channel();
        {
            let mut w = self.link.reqs.lock().unwrap_or_else(|e| e.into_inner());
            match slot {
                ReqSlot::Invite => w.invite = Some(tx),
                ReqSlot::MailboxOpen => w.mbox_open = Some(tx),
                ReqSlot::Deposit => w.deposit = Some(tx),
                ReqSlot::Ok => w.ok = Some(tx),
            }
        }
        self.link
            .cmd_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("relay link down"))?;
        match tokio::time::timeout(REQ_TIMEOUT, rx).await {
            Ok(Ok(ClientEvent::Error { code, detail })) => Err(RelayError(code, detail).into()),
            Ok(Ok(ev)) => Ok(ev),
            _ => Err(anyhow!("relay request failed")),
        }
    }

    async fn ensure_mailbox(self: &Arc<Self>) -> Result<()> {
        if self.conn_state() != tb::ConnState::Connected {
            return Err(anyhow!("not connected"));
        }
        let mbox = match self.sh().own_mailbox.clone() {
            Some(m) => m,
            None => OwnMailbox {
                id: MailboxId::random(),
                fetch_token: Token::random(),
                deposit_token: Token::random(),
            },
        };
        let ev = match self
            .request(
                ClientMsg::MailboxOpen {
                    mailbox_id: mbox.id,
                    h_deposit: token_hash(&mbox.deposit_token),
                    h_fetch: token_hash(&mbox.fetch_token),
                    ttl_secs: MAILBOX_TTL_SECS,
                },
                ReqSlot::MailboxOpen,
            )
            .await
        {
            Ok(ev) => ev,
            // §9: a reconnect within the TTL can find our box still live —
            // the persisted tokens remain valid, proceed to drain.
            Err(e)
                if e.downcast_ref::<RelayError>()
                    .map(|r| r.0 == ErrorCode::MailboxExists)
                    .unwrap_or(false)
                    && self.sh().own_mailbox.is_some() =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if !matches!(ev, ClientEvent::MailboxOpened { .. }) {
            return Err(anyhow!("unexpected mailbox reply"));
        }
        let persist = MailboxPersist {
            mailbox_id: *mbox.id.as_bytes(),
            fetch_token: *mbox.fetch_token.as_bytes(),
            deposit_token: *mbox.deposit_token.as_bytes(),
        };
        let raw = serde_json::to_string(&persist)?;
        let s = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _ = s.set_meta(MBOX_KEY, &raw);
        });
        self.sh().own_mailbox = Some(mbox);
        Ok(())
    }

    /// Fetch + ack queued envelopes for our mailbox. Each envelope carries a
    /// Noise session_id: unknown sessions are dropped (still acked, so they
    /// do not redeliver forever).
    async fn mailbox_drain(self: &Arc<Self>) -> Result<()> {
        let mbox = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .own_mailbox
            .as_ref()
            .map(|m| (m.id, m.fetch_token));
        let Some((id, token)) = mbox else {
            return Ok(());
        };
        // op_lock is held only for the fetch window; the ack below is a
        // separate serialized op (request() takes the lock — holding it here
        // would self-deadlock).
        let mut ack_ids = Vec::new();
        {
            let _g = self.op_lock.lock().await;
            let (tx, mut rx) = mpsc::channel(8);
            self.link
                .reqs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fetch = Some(tx);
            if self
                .link
                .cmd_tx
                .send(ClientMsg::MailboxFetch {
                    mailbox_id: id,
                    token,
                    max_msgs: MAILBOX_FETCH_PAGE,
                })
                .await
                .is_err()
            {
                self.link
                    .reqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .fetch = None;
                return Err(anyhow!("link down"));
            }
            loop {
                let ev = match tokio::time::timeout(REQ_TIMEOUT, rx.recv()).await {
                    Ok(Some(ev)) => ev,
                    _ => break,
                };
                match ev {
                    ClientEvent::MailboxMessages { msgs, more } => {
                        for env in msgs {
                            // Mailbox frames only decrypt under a LIVE
                            // session: after a restart there is none for
                            // `env.from` (sessions never resume), and a
                            // parked frame would poison the next handshake
                            // via the `early` drain. Undeliverable deposits
                            // drop + count, but are still acked so they do
                            // not redeliver forever.
                            if self.session(&env.from).is_some() {
                                self.handle_transport(env.from, env.session_id, env.body)
                                    .await;
                            } else {
                                self.sh().dropped_inbound += 1;
                            }
                            ack_ids.push(env.msg_id);
                        }
                        if !more {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            // Release the fetch slot on every exit path so a later relay
            // Error can't be routed to a dead channel.
            self.link
                .reqs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fetch = None;
        }
        if !ack_ids.is_empty() {
            let _ = self
                .request(
                    ClientMsg::MailboxAck {
                        mailbox_id: id,
                        token,
                        msg_ids: ack_ids,
                    },
                    ReqSlot::Ok,
                )
                .await;
        }
        Ok(())
    }

    async fn presence_subscribe(&self) -> Result<()> {
        let peers: Vec<PeerId> = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .iter()
            .filter(|(_, m)| m.state == MetaState::Active)
            .map(|(p, _)| *p)
            .collect();
        if peers.is_empty() || self.conn_state() != tb::ConnState::Connected {
            return Ok(());
        }
        let _ = self
            .request(ClientMsg::PresenceSubscribe { peers }, ReqSlot::Ok)
            .await;
        Ok(())
    }

    // ----- sessions -----------------------------------------------------------

    fn install_session(&self, peer: PeerId, session: Session) -> Option<Arc<PeerSession>> {
        let sid = session.session_id()?;
        let ps = Arc::new(PeerSession {
            session: Mutex::new(session),
            session_id: sid,
            remote_sid: Mutex::new(None),
        });
        let mut sh = self.sh();
        if let Some(old) = sh.sessions.insert(peer, ps.clone()) {
            // The replaced session's inbound id is dead: tombstone it so
            // late-arriving old-generation frames drop instead of binding
            // (and fatally failing) against the new session.
            if let Some(rs) = old
                .remote_sid
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                sh.sid_index.remove(&rs);
                Self::tombstone(&mut sh, rs);
            }
        }
        Some(ps)
    }

    /// Bounded tombstone (~8192 entries, 10 min): enough to cover any in-
    /// flight reorder window without growing unbounded under junk frames.
    fn tombstone(sh: &mut Shared, sid: SessionId) {
        if sh.dead_sids.len() >= 8192 {
            let cutoff = Instant::now() - Duration::from_secs(600);
            sh.dead_sids.retain(|_, t| *t > cutoff);
            if sh.dead_sids.len() >= 8192 {
                return;
            }
        }
        sh.dead_sids.insert(sid, Instant::now());
    }

    fn drop_session(&self, peer: &PeerId) {
        let mut sh = self.sh();
        if let Some(ps) = sh.sessions.remove(peer) {
            if let Some(rs) = ps
                .remote_sid
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                sh.sid_index.remove(&rs);
                Self::tombstone(&mut sh, rs);
            }
        }
    }

    fn session(&self, peer: &PeerId) -> Option<Arc<PeerSession>> {
        let mut sh = self.sh();
        // Reap closed entries so a fatally-failed session can't linger and
        // blind the is-some checkers into thinking we're still live.
        let closed = sh
            .sessions
            .get(peer)
            .map(|ps| {
                ps.session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_closed()
            })
            .unwrap_or(false);
        if !closed {
            return sh.sessions.get(peer).cloned();
        }
        if let Some(ps) = sh.sessions.remove(peer) {
            if let Some(rs) = ps
                .remote_sid
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                sh.sid_index.remove(&rs);
                Self::tombstone(&mut sh, rs);
            }
        }
        None
    }

    fn send_lock(&self, peer: &PeerId) -> Arc<tokio::sync::Mutex<()>> {
        self.send_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(*peer)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Post-handshake convergence: pin store, meta, session install, mailbox
    /// grant send, chat row, pending flush, events. Shared by initiator and
    /// responder paths.
    async fn on_session_ready(self: &Arc<Self>, peer: PeerId, session: Session, outbound: bool) {
        // Declined/blocked contacts keep no inbound channel: a completed
        // handshake from one is dropped before install — no session, no
        // events, no pinned-key updates.
        {
            let mut sh = self.sh();
            if matches!(
                sh.meta.get(&peer).map(|m| m.state),
                Some(MetaState::Declined) | Some(MetaState::Blocked)
            ) {
                sh.dropped_inbound += 1;
                tracing::debug!(%peer, "session from declined/blocked contact; dropped");
                return;
            }
        }

        let sas_emoji = session.sas().map(|s| s.emoji());
        let sas_digits = session.sas().map(|s| s.digits());
        let remote_static = session.remote_static();
        self.sh().installing.insert(peer);

        // Pin-store convergence + §11 key-change detection on EVERY XX
        // (new contact → FirstSeen record; same peer-id, new static → loud
        // TrustState::KeyChanged that gates sends until dismissed).
        let (trust, key_changed, old_fp, session) = match remote_static {
            Some(static_key) => {
                match self
                    .cblk(move |c| {
                        if c.get(&peer).is_none() {
                            let _ = c.add_unverified(static_key, "");
                        }
                        let _ = c.record_seen(&peer);
                        let changed = session.detect_key_change(c, &peer).unwrap_or(false);
                        let old = c.get(&peer).and_then(|ct| match &ct.state {
                            TrustState::KeyChanged { old, .. } => Some(*old),
                            _ => None,
                        });
                        (
                            c.get(&peer).map(|ct| ct.state.clone()),
                            changed,
                            old,
                            session,
                        )
                    })
                    .await
                {
                    Some((t, kc, o, s)) => (t, kc, o, s),
                    None => {
                        self.sh().installing.remove(&peer);
                        return;
                    }
                }
            }
            None => (None, false, None, session),
        };

        if key_changed {
            self.emit(
                "security.identity_changed",
                None,
                json!({
                    "contact_id": contact_str(&peer),
                    "peer_id": peer_str(&peer),
                    "old_fingerprint": old_fp.map(|k| b32(PeerId::from_static_key(&k).as_bytes())),
                    "new_fingerprint": remote_static
                        .map(|k| b32(PeerId::from_static_key(&k).as_bytes())),
                    "trust": "key_changed",
                }),
            );
        }

        if self.install_session(peer, session).is_none() {
            self.sh().installing.remove(&peer);
            return;
        }

        // Frames that beat the install (grant/ack races) replay through the
        // normal bind path now that the session exists.
        let parked: Vec<(SessionId, Vec<u8>)> = {
            let mut sh = self.sh();
            match sh.early.remove(&peer) {
                Some(q) => {
                    sh.early_total = sh.early_total.saturating_sub(q.len());
                    q.into_iter().collect()
                }
                None => Vec::new(),
            }
        };
        for (sid, body) in parked {
            self.handle_transport(peer, sid, body).await;
        }

        let mut became_active = false;
        {
            let mut sh = self.sh();
            let meta = sh.meta.entry(peer).or_insert_with(|| {
                ContactMeta::new(
                    if outbound {
                        MetaState::PendingOut
                    } else {
                        MetaState::PendingIn
                    },
                    None,
                )
            });
            meta.sas_emoji = sas_emoji;
            meta.sas_digits = sas_digits;
            // A successful handshake clears the failure backoff whatever the
            // prior state — Active contacts accrue it for queue retries too.
            meta.retry_count = 0;
            meta.retry_after_ms = None;
            if meta.state == MetaState::PendingOut {
                meta.state = MetaState::Active;
                meta.substate = None;
                became_active = true;
            }
        }
        self.persist_meta();
        let verified = matches!(trust, Some(TrustState::Verified));
        let _ = self
            .sblk(move |s| s.upsert_chat(&chat_str(&peer), peer.as_bytes(), None, verified))
            .await;

        // Mailbox grant exchange (§8): we always send ours; theirs arrives
        // in-band as AppMsg::Grant.
        let our_grant = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .own_mailbox
            .as_ref()
            .map(|m| (*m.id.as_bytes(), *m.deposit_token.as_bytes()));
        if let Some((mid, dtok)) = our_grant {
            self.deliver_now(
                peer,
                &PendingMsg {
                    app: AppMsg::Grant {
                        mailbox_id: mid,
                        deposit_token: dtok,
                    },
                    msg_id: None,
                    client_msg_id: None,
                    chat_id: chat_str(&peer),
                },
            )
            .await;
            self.set_meta(&peer, |m| m.grant_sent = true);
        }

        self.flush_pending(peer).await;

        if outbound || became_active {
            self.emit(
                "contact.connected",
                None,
                json!({
                    "contact_id": contact_str(&peer),
                    "peer_id": peer_str(&peer),
                    "chat_id": chat_str(&peer),
                    "direction": if outbound { "out" } else { "in" },
                    "trust": trust_str(trust.as_ref()),
                }),
            );
            self.maybe_emit_verification(&peer, &trust);
        } else {
            let (via, invite_id) = {
                let mut sh = self.sh();
                if sh.invites_open.is_empty() {
                    ("static", Value::Null)
                } else {
                    let (_, id) = sh.invites_open.remove(0);
                    ("invite", Value::String(id))
                }
            };
            self.emit(
                "contact.request",
                None,
                json!({
                    "contact_id": contact_str(&peer),
                    "peer_id": peer_str(&peer),
                    "via": via,
                    "invite_id": invite_id,
                    "display_name": Value::Null,
                }),
            );
        }
        // A recorded key change always demands re-verification, whatever the
        // session direction (dedup inside maybe_emit_verification).
        if key_changed {
            self.maybe_emit_verification(&peer, &trust);
        }
        self.sh().installing.remove(&peer);
    }

    fn maybe_emit_verification(&self, peer: &PeerId, trust: &Option<TrustState>) {
        if matches!(trust, Some(TrustState::Verified)) {
            return;
        }
        let (emoji, digits) = {
            let mut sh = self.sh();
            match sh.meta.get_mut(peer) {
                Some(m) => {
                    let cur = m.sas_emoji.clone();
                    if cur.is_some() && m.nagged_sas == cur {
                        return;
                    }
                    m.nagged_sas = cur.clone();
                    (m.sas_emoji.clone(), m.sas_digits.clone())
                }
                None => return,
            }
        };
        self.persist_meta();
        self.emit(
            "security.verification_needed",
            None,
            json!({
                "contact_id": contact_str(peer),
                "peer_id": peer_str(peer),
                "sas_hint": {"emoji": emoji, "digits": digits},
            }),
        );
    }

    // ----- outbound delivery --------------------------------------------------

    /// Encrypt + send on a live session. For a known-offline peer with a
    /// mailbox grant, deposits the frame into their mailbox instead. Returns
    /// false when no usable path exists (caller queues/retries).
    async fn deliver_now(&self, peer: PeerId, item: &PendingMsg) -> bool {
        let Some(ps) = self.session(&peer) else {
            tracing::debug!(%peer, "deliver_now: no live session");
            return false;
        };
        let payload = match wire_serialize(&item.app) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(%e, "deliver_now: AppMsg encode failed");
                return false;
            }
        };
        let offline_deposit = {
            let sh = self.sh();
            sh.presence.get(&peer).map(|o| !*o).unwrap_or(false)
                && sh.meta.get(&peer).and_then(|m| m.grant.as_ref()).is_some()
        };
        let (ct, ctrl) = {
            let mut s = ps.session.lock().unwrap_or_else(|e| e.into_inner());
            match s.encrypt(&payload) {
                // §11: a rekey control frame must reach the peer ON the same
                // channel as the data — a mailbox deposit can't carry it in
                // order, so on the deposit path rotation is DEFERRED to the
                // next live send and the batch stays self-consistent.
                Ok(ct) => (
                    ct,
                    if offline_deposit {
                        None
                    } else {
                        s.maybe_rekey().ok().flatten()
                    },
                ),
                Err(e) => {
                    tracing::debug!(%e, "deliver_now: encrypt failed; dropping session");
                    drop(s);
                    drop(ps);
                    self.drop_session(&peer);
                    return false;
                }
            }
        };
        if offline_deposit {
            return self.deposit(peer, ps.session_id, ct, item).await;
        }
        if self
            .link
            .cmd_tx
            .send(ClientMsg::Transport {
                to: peer,
                session_id: ps.session_id,
                body: ct,
            })
            .await
            .is_err()
        {
            return false;
        }
        if let Some(ctrl) = ctrl {
            let _ = self
                .link
                .cmd_tx
                .send(ClientMsg::Transport {
                    to: peer,
                    session_id: ps.session_id,
                    body: ctrl,
                })
                .await;
        }
        if let Some(id) = &item.msg_id {
            self.transition(id, MsgState::Sent, None).await;
        }
        true
    }

    /// Offline send: the peer's mailbox grant lets us deposit the
    /// already-encrypted frame; `MailboxDeposited` means relay-accepted =
    /// "sent".
    async fn deposit(
        &self,
        peer: PeerId,
        session_id: SessionId,
        ct: Vec<u8>,
        item: &PendingMsg,
    ) -> bool {
        let grant = {
            self.shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .meta
                .get(&peer)
                .and_then(|m| m.grant.clone())
        };
        let Some(g) = grant else { return false };
        let ev = self
            .request(
                ClientMsg::MailboxDeposit {
                    mailbox_id: MailboxId::from_bytes(g.mailbox_id),
                    token: Token::from_bytes(g.deposit_token),
                    env: DepositEnvelope {
                        session_id,
                        body: ct,
                    },
                },
                ReqSlot::Deposit,
            )
            .await;
        match ev {
            Ok(ClientEvent::MailboxDeposited { .. }) => {
                if let Some(id) = &item.msg_id {
                    self.transition(id, MsgState::Sent, None).await;
                }
                true
            }
            _ => false,
        }
    }

    /// Queue-or-send: when no live path exists the item lands in pending_out
    /// and a handshake attempt is kicked off.
    async fn deliver_or_queue(self: &Arc<Self>, peer: PeerId, item: PendingMsg) {
        let send = self.send_lock(&peer);
        let _send = send.lock().await;
        if self.deliver_now(peer, &item).await {
            return;
        }
        let item_id = item.msg_id.clone();
        let full = {
            let mut sh = self.sh();
            let q = sh.pending_out.entry(peer).or_default();
            if q.len() >= PENDING_OUT_CAP {
                true
            } else {
                q.push_back(item);
                false
            }
        };
        if full {
            if let Some(id) = item_id {
                self.transition(&id, MsgState::Failed, Some("internal".into()))
                    .await;
            }
            return;
        }
        self.ensure_handshake(peer);
    }

    async fn flush_pending(&self, peer: PeerId) {
        let send = self.send_lock(&peer);
        let _send = send.lock().await;
        loop {
            let item = {
                let mut sh = self.sh();
                sh.pending_out.get_mut(&peer).and_then(|q| q.pop_front())
            };
            let Some(item) = item else { break };
            if !self.deliver_now(peer, &item).await {
                let mut sh = self.sh();
                sh.pending_out.entry(peer).or_default().push_front(item);
                break;
            }
        }
    }

    async fn fail_pending(&self, peer: PeerId, reason: &str) {
        let items: Vec<PendingMsg> = {
            let mut sh = self.sh();
            sh.pending_out
                .remove(&peer)
                .map(|q| q.into_iter().collect())
                .unwrap_or_default()
        };
        for item in items {
            if let Some(id) = item.msg_id {
                self.transition(&id, MsgState::Failed, Some(reason.into()))
                    .await;
            }
        }
    }

    async fn transition(&self, msg_id: &str, to: MsgState, reason: Option<String>) {
        let store_result = self
            .sblk({
                let id = msg_id.to_string();
                move |s| s.update_state(&id, to)
            })
            .await;
        if store_result.is_err() {
            // Already-terminal or unknown: never emit a state event that did
            // not happen.
            return;
        }
        let m = self
            .sblk({
                let id = msg_id.to_string();
                move |s| s.get_message(&id)
            })
            .await;
        if let Ok(Some(m)) = m {
            let mut p = json!({
                "chat_id": m.chat_id,
                "msg_id": m.msg_id,
                "client_msg_id": m.client_msg_id,
                "state": state_str(to),
            });
            if let Some(r) = reason {
                p["reason"] = Value::String(r);
            }
            self.emit("msg_state", Some(&m.chat_id), p);
        }
    }

    // ----- handshake drivers ---------------------------------------------------

    /// Kick an initiator handshake for `peer` unless one is in flight.
    fn ensure_handshake(self: &Arc<Self>, peer: PeerId) {
        if self.conn_state() != tb::ConnState::Connected {
            return;
        }
        {
            let mut sh = self.sh();
            if !sh.hs_inflight.insert(peer) {
                return;
            }
        }
        let c = self.clone();
        tokio::spawn(async move {
            c.outbound_handshake(peer, None).await;
        });
    }

    async fn outbound_handshake(self: &Arc<Self>, peer: PeerId, invite: Option<InviteToken>) {
        let mut dl = DemuxLink::new(self.link.cmd_tx.clone(), self.link.hs_waiters.clone());
        let result = handshake_flow::initiator(&self.identity, &mut dl, peer, &peer, invite).await;
        self.sh().hs_inflight.remove(&peer);
        match result {
            Ok(session) => {
                self.on_session_ready(peer, session, true).await;
            }
            Err(NetErr::PeerIdMismatch {
                actual,
                actual_static,
                ..
            }) => {
                // The remote static changed under a pinned peer-id: record
                // it (loud — is_send_blocked gates sends until dismissed),
                // never a silent rebind.
                let (old_key, trust) = self
                    .cblk(move |c| {
                        let old = c.get(&peer).map(|ct| ct.static_pubkey);
                        let _ = c.record_key_change(&peer, actual_static);
                        (old, c.get(&peer).map(|ct| ct.state.clone()))
                    })
                    .await
                    .unwrap_or((None, None));
                self.emit(
                    "security.identity_changed",
                    None,
                    json!({
                        "contact_id": contact_str(&peer),
                        "peer_id": peer_str(&peer),
                        "old_fingerprint": old_key.map(|k| b32(&k)),
                        "new_fingerprint": b32(actual.as_bytes()),
                        "trust": "key_changed",
                    }),
                );
                self.fail_pending(peer, "peer_offline").await;
                self.mark_add_failed(&peer, "peer_id_mismatch");
                self.maybe_emit_verification(&peer, &trust);
            }
            Err(e) => {
                tracing::debug!(%e, "outbound handshake failed");
                // Transient failures (offline, timeout, dead link) keep the
                // queue: a later PeerStatus{online} or link-up re-arms the
                // handshake and flush_pending delivers. Only terminal errors
                // (PeerIdMismatch above — the queue was destined for a
                // different key holder) destroy it.
                self.mark_add_failed(&peer, net_err_reason(&e));
            }
        }
    }

    /// §11 `pending_out → failed` bookkeeping: records the failure substate
    /// and arms the backoff retry (timer task while connected; the link-up
    /// sweep also re-arms it after a reconnect). Active contacts share the
    /// backoff when a queued backlog exists — otherwise a transient failure
    /// that raced the peer's `PeerStatus{online}` wedge the queue until the
    /// next link event. Once RETRY_STEPS_MS is exhausted the add stays
    /// `failed` until an explicit re-add; for Active contacts presence and
    /// link-up sweeps remain the re-arm path.
    fn mark_add_failed(self: &Arc<Self>, peer: &PeerId, reason: &str) {
        let mut delay_ms = None;
        self.set_meta(peer, |m| {
            if m.state == MetaState::PendingOut {
                m.substate = Some(format!("failed:{reason}"));
            } else if m.state != MetaState::Active {
                return;
            }
            m.retry_count = m.retry_count.saturating_add(1);
            m.retry_after_ms = RETRY_STEPS_MS
                .get((m.retry_count - 1) as usize)
                .map(|d| now_ms() + *d);
            delay_ms = m.retry_after_ms.map(|t| (t - now_ms()).max(0));
        });
        let Some(ms) = delay_ms else { return };
        let c = self.clone();
        let p = *peer;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms as u64)).await;
            let due = {
                let sh = c.sh();
                sh.meta
                    .get(&p)
                    .map(|m| {
                        let retry_due = m.retry_after_ms.map(|t| t <= now_ms()).unwrap_or(false);
                        let queued = sh
                            .pending_out
                            .get(&p)
                            .map(|q| !q.is_empty())
                            .unwrap_or(false);
                        let live = sh.sessions.contains_key(&p);
                        (m.state == MetaState::PendingOut && retry_due)
                            || (m.state == MetaState::Active && retry_due && queued && !live)
                    })
                    .unwrap_or(false)
            };
            if due {
                c.ensure_handshake(p);
            }
        });
    }

    async fn inbound_loop(self: Arc<Self>, mut rx: mpsc::Receiver<ClientEvent>) {
        while let Some(ev) = rx.recv().await {
            if self.shutdown_requested() {
                break;
            }
            let ClientEvent::Handshake { from, .. } = &ev else {
                continue;
            };
            let from = *from;
            {
                let mut sh = self.sh();
                // Declined/blocked contacts: no responder, no handshake —
                // the Msg1 dies here (attacker burns their 10 s slot).
                if matches!(
                    sh.meta.get(&from).map(|m| m.state),
                    Some(MetaState::Declined) | Some(MetaState::Blocked)
                ) {
                    sh.dropped_inbound += 1;
                    tracing::debug!(%from, "Msg1 from declined/blocked contact; dropped");
                    continue;
                }
                // One responder per claimed peer; RESPONDER_CAP bounds the
                // Msg1 flood — excess Msg1s expire relay-side at the §12
                // handshake deadline.
                if sh.responders.contains(&from) || sh.responders.len() >= RESPONDER_CAP {
                    continue;
                }
                sh.responders.insert(from);
            }
            let c = self.clone();
            tokio::spawn(async move {
                c.respond(ev).await;
                c.sh().responders.remove(&from);
            });
        }
    }

    /// Responder driver for one inbound Msg1. The seeded event already sits
    /// in the link's channel; the driver's `wait_first_handshake` picks it up.
    async fn respond(self: &Arc<Self>, first: ClientEvent) {
        let mut dl = DemuxLink::new(self.link.cmd_tx.clone(), self.link.hs_waiters.clone());
        dl.seed(first);
        let contacts = self.contacts.clone();
        let result = handshake_flow::responder(&self.identity, &mut dl, move |learned: &PeerId| {
            contacts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(learned)
                .map(|_| *learned)
        })
        .await;
        match result {
            Ok(session) => {
                let Some(peer) = session.remote_peer_id() else {
                    return;
                };
                self.on_session_ready(peer, session, false).await;
            }
            Err(e) => {
                tracing::debug!(%e, "inbound handshake failed");
            }
        }
    }

    // ----- inbound transport -------------------------------------------------

    /// `from` is relay-stamped (the relay stamps the connection's hello'd
    /// peer-id onto every forward/deposit), so it is authentic routing
    /// metadata — an attacker can only burn their own session by lying.
    async fn handle_transport(
        self: &Arc<Self>,
        from: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) {
        // Routing: bound sid → its session. Unbound sid → the peer's session
        // binds lazily (each side stamps a random local sid, learned here).
        // Tombstoned sids and bound-mismatches drop pre-decrypt; a decrypt
        // failure is fatal to the session AND tombstones the stream.
        let (ps, peer, fresh) = {
            let mut sh = self.sh();
            if sh.dead_sids.contains_key(&session_id) {
                // Provably-dead stream. If our session for `from` is live but
                // unbound, its inbound direction is dead too — resync.
                let unbound_live = sh
                    .sessions
                    .get(&from)
                    .map(|ps| {
                        !ps.session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_closed()
                            && ps
                                .remote_sid
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .is_none()
                    })
                    .unwrap_or(false);
                if unbound_live && sh.meta.contains_key(&from) {
                    drop(sh);
                    self.ensure_handshake(from);
                }
                return;
            }
            let bound = sh
                .sid_index
                .get(&session_id)
                .and_then(|p| sh.sessions.get(p).map(|ps| (*p, ps.clone())));
            match bound {
                Some((peer, ps)) => (ps, peer, false),
                None => match sh.sessions.get(&from).cloned() {
                    None => {
                        // Park ONLY while a handshake or install for `from`
                        // is genuinely in flight — that race is what `early`
                        // exists for. With nothing in flight no session is
                        // coming (Noise sessions never resume), so the frame
                        // is a dead-generation leftover: drop it. Feeding it
                        // to a LATER session's drain would bind a foreign sid
                        // and kill that session on decrypt-fail.
                        let in_flight = sh.hs_inflight.contains(&from)
                            || sh.responders.contains(&from)
                            || sh.installing.contains(&from);
                        if !in_flight {
                            sh.dropped_inbound += 1;
                            tracing::debug!(%from, ?session_id, "no session or handshake in flight; frame dropped");
                            return;
                        }
                        // Caps: 8 per peer, EARLY_GLOBAL_CAP across peers
                        // (each fake peer-id is free to mint — the per-peer
                        // cap alone is not a bound).
                        if sh.early_total >= EARLY_GLOBAL_CAP {
                            tracing::debug!(%from, ?session_id, "early global cap; frame dropped");
                            return;
                        }
                        let q = sh.early.entry(from).or_default();
                        if q.len() < 8 {
                            q.push_back((session_id, body));
                            sh.early_total += 1;
                        } else {
                            tracing::debug!(%from, ?session_id, "early queue full; frame dropped");
                        }
                        return;
                    }
                    Some(ps) => {
                        if ps
                            .remote_sid
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some()
                        {
                            // Bound to a different sid. Two cases: a stale
                            // superseded stream (tombstone + drop), or a frame
                            // from a handshake that COMPLETED peer-side while
                            // our responder is still installing — the old entry
                            // still occupies sessions[from] and tombstoning
                            // the new sid here would poison that generation
                            // permanently. Park it like `early` does and let
                            // the post-install drain bind it to the fresh
                            // session.
                            let in_flight = sh.hs_inflight.contains(&from)
                                || sh.responders.contains(&from)
                                || sh.installing.contains(&from);
                            if in_flight && sh.early_total < EARLY_GLOBAL_CAP {
                                let q = sh.early.entry(from).or_default();
                                if q.len() < 8 {
                                    q.push_back((session_id, body));
                                    sh.early_total += 1;
                                }
                                return;
                            }
                            Self::tombstone(&mut sh, session_id);
                            tracing::debug!(%from, ?session_id, "transport under foreign sid; dropped");
                            return;
                        }
                        *ps.remote_sid.lock().unwrap_or_else(|e| e.into_inner()) = Some(session_id);
                        sh.sid_index.insert(session_id, from);
                        tracing::debug!(%from, ?session_id, "bound peer's inbound session id");
                        (ps, from, true)
                    }
                },
            }
        };
        let pt = {
            let mut s = ps.session.lock().unwrap_or_else(|e| e.into_inner());
            match s.decrypt(&body) {
                Ok(pt) => pt,
                Err(e) => {
                    // decrypt() fatal-closes the Noise state on ANY failure:
                    // tombstone the sid so in-flight frames drop cleanly,
                    // drop the session, and re-handshake so the peer's next
                    // outbound lands on a fresh XX.
                    tracing::debug!(%e, %peer, ?session_id, fresh, "transport decrypt failed; destroying session");
                    drop(s);
                    drop(ps);
                    self.drop_session(&peer);
                    if self.sh().meta.contains_key(&peer) {
                        self.ensure_handshake(peer);
                    }
                    return;
                }
            }
        };
        if pt.is_empty() {
            // Rekey control unit consumed inside decrypt.
            return;
        }
        let app: AppMsg = match wire_deserialize(&pt) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(%e, "AppMsg decode failed; dropped");
                return;
            }
        };
        self.handle_app(peer, app).await;
    }

    async fn handle_app(self: &Arc<Self>, peer: PeerId, app: AppMsg) {
        let chat_id = chat_str(&peer);
        // Belt to the inbound_loop/on_session_ready suspenders: a session
        // that predates a decline keeps delivering frames — drop them here.
        {
            let mut sh = self.sh();
            if matches!(
                sh.meta.get(&peer).map(|m| m.state),
                Some(MetaState::Declined) | Some(MetaState::Blocked)
            ) {
                sh.dropped_inbound += 1;
                tracing::debug!(%peer, "app frame from declined/blocked contact; dropped");
                return;
            }
        }
        match app {
            AppMsg::Text { msg_id, body } => {
                let verified = self.contact_verified(&peer).await;
                let _ = self
                    .sblk({
                        let chat_id = chat_id.clone();
                        move |s| s.upsert_chat(&chat_id, peer.as_bytes(), None, verified)
                    })
                    .await;
                let inserted = self
                    .sblk({
                        let chat_id = chat_id.clone();
                        let body = body.clone();
                        let msg_id = msg_id.clone();
                        move |s| {
                            s.insert_message(&NewMessage {
                                msg_id,
                                chat_id,
                                dir: Dir::In,
                                kind: MsgKind::Text,
                                body,
                                state: Some(MsgState::Delivered),
                                client_msg_id: None,
                                ts: now_ms(),
                            })
                        }
                    })
                    .await;
                let Ok((id, deduped)) = inserted else { return };
                if !deduped {
                    if let Ok(Some(m)) = self
                        .sblk({
                            let id = id.clone();
                            move |s| s.get_message(&id)
                        })
                        .await
                    {
                        self.emit(
                            "message.received",
                            Some(&m.chat_id),
                            json!({
                                "chat_id": m.chat_id,
                                "message": message_json(&m, &peer, &self.peer_id_str),
                            }),
                        );
                    }
                }
                self.deliver_now(
                    peer,
                    &PendingMsg {
                        app: AppMsg::Ack { msg_ids: vec![id] },
                        msg_id: None,
                        client_msg_id: None,
                        chat_id,
                    },
                )
                .await;
            }
            AppMsg::Ack { msg_ids } => {
                for id in msg_ids {
                    let cur = self
                        .sblk({
                            let id = id.clone();
                            move |s| s.get_message(&id)
                        })
                        .await;
                    if let Ok(Some(m)) = cur {
                        // Acks only advance OUR outbound rows in THIS chat —
                        // a peer can't ack another chat's ids or their own
                        // inbound copies.
                        if m.chat_id == chat_id
                            && m.dir == Dir::Out
                            && matches!(m.state, Some(MsgState::Queued) | Some(MsgState::Sent))
                        {
                            self.transition(&m.msg_id, MsgState::Delivered, None).await;
                        }
                    }
                }
            }
            AppMsg::Grant {
                mailbox_id,
                deposit_token,
            } => {
                // Grants may beat the meta insert (early-drain runs before
                // on_session_ready's meta entry): upsert, never no-op.
                {
                    let mut sh = self.sh();
                    let m = sh
                        .meta
                        .entry(peer)
                        .or_insert_with(|| ContactMeta::new(MetaState::PendingIn, None));
                    m.grant = Some(PeerGrant {
                        mailbox_id,
                        deposit_token,
                    });
                }
                self.persist_meta();
                let _ = self
                    .cblk(move |c| {
                        c.set_mailbox_grant(
                            &peer,
                            MailboxGrant {
                                mailbox_id,
                                deposit_token_hash: Sha256::digest(deposit_token).into(),
                                // The peer's fetch token is never on the
                                // wire — slot reserved, not the deposit hash.
                                fetch_token_hash: [0u8; 32],
                            },
                        )
                    })
                    .await;
            }
            AppMsg::Edit { msg_id, body } => {
                let r = self
                    .sblk({
                        let msg_id = msg_id.clone();
                        let chat_id = chat_id.clone();
                        let body = body.clone();
                        move |s| s.apply_peer_edit(&msg_id, &chat_id, &body)
                    })
                    .await;
                if r.is_ok() {
                    if let Ok(Some(m)) = self.sblk(move |s| s.get_message(&msg_id)).await {
                        self.emit(
                            "message.received",
                            Some(&m.chat_id),
                            json!({
                                "chat_id": m.chat_id,
                                "message": message_json(&m, &peer, &self.peer_id_str),
                            }),
                        );
                    }
                }
            }
            AppMsg::Delete { msg_id } => {
                let r = self
                    .sblk({
                        let msg_id = msg_id.clone();
                        let chat_id = chat_id.clone();
                        move |s| s.apply_peer_delete(&msg_id, &chat_id)
                    })
                    .await;
                if r.is_ok() {
                    if let Ok(Some(m)) = self.sblk(move |s| s.get_message(&msg_id)).await {
                        self.emit(
                            "message.received",
                            Some(&m.chat_id),
                            json!({
                                "chat_id": m.chat_id,
                                "message": message_json(&m, &peer, &self.peer_id_str),
                            }),
                        );
                    }
                }
            }
            AppMsg::React { msg_id, emoji } => {
                // Wire cap (§2): reject oversized emoji before store + emit.
                if emoji.as_ref().map(|e| e.len()).unwrap_or(0) > EMOJI_MAX_BYTES {
                    tracing::debug!(%peer, "oversize emoji reaction; dropped");
                    return;
                }
                let reactor = *peer.as_bytes();
                match emoji {
                    Some(e) => {
                        let id = msg_id.clone();
                        let cid = chat_id.clone();
                        let _ = self
                            .sblk(move |s| s.apply_peer_reaction(&id, &cid, &e, &reactor))
                            .await;
                    }
                    None => {
                        let id = msg_id.clone();
                        let cid = chat_id.clone();
                        let _ = self
                            .sblk(move |s| s.remove_peer_reaction(&id, &cid, &reactor))
                            .await;
                    }
                }
                if let Ok(Some(m)) = self.sblk(move |s| s.get_message(&msg_id)).await {
                    self.emit(
                        "message.received",
                        Some(&m.chat_id),
                        json!({
                            "chat_id": m.chat_id,
                            "message": message_json(&m, &peer, &self.peer_id_str),
                        }),
                    );
                }
            }
        }
    }

    async fn contact_verified(&self, peer: &PeerId) -> bool {
        let p = *peer;
        self.cblk(move |c| {
            c.get(&p)
                .map(|ct| matches!(ct.state, TrustState::Verified))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    // ----- domain ops shared by RPC + TUI + CLI ---------------------------------

    fn resolve_to(&self, to: &str) -> Result<PeerId, RpcError> {
        for pfx in ["peer:", "contact:", "chat:"] {
            if let Some(rest) = to.strip_prefix(pfx) {
                let bytes = b32_decode_32(rest).ok_or_else(|| {
                    RpcError::new(ErrorKind::InvalidParams, "malformed id in `to`")
                })?;
                let peer = PeerId::from_bytes(bytes);
                if !self.sh().meta.contains_key(&peer) {
                    return Err(RpcError::new(ErrorKind::NotFound, "unknown contact"));
                }
                return Ok(peer);
            }
        }
        Err(RpcError::new(
            ErrorKind::InvalidParams,
            "`to` must be a peer:/contact:/chat: id",
        ))
    }

    fn peer_of_contact_id(&self, contact_id: &str) -> Result<PeerId, RpcError> {
        let rest = contact_id
            .strip_prefix("contact:")
            .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "expected a contact: id"))?;
        let bytes = b32_decode_32(rest)
            .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "malformed contact_id"))?;
        Ok(PeerId::from_bytes(bytes))
    }

    async fn check_send_allowed(&self, peer: &PeerId) -> Result<(), RpcError> {
        let state = self.meta_state(peer);
        match state {
            Some(MetaState::Blocked) | Some(MetaState::Declined) => {
                return Err(RpcError::new(
                    ErrorKind::ContactBlocked,
                    "contact is blocked or declined",
                ));
            }
            _ => {}
        }
        let p = *peer;
        // Fail closed: a poisoned/panicked contacts read treats sends as
        // blocked rather than bypassing the key-change gate.
        let blocked = self
            .cblk(move |c| c.is_send_blocked(&p))
            .await
            .unwrap_or(true);
        if blocked {
            return Err(RpcError::new(
                ErrorKind::KeyChangedBlocked,
                "contact key changed; re-run ident.verify with the new SAS",
            ));
        }
        Ok(())
    }

    /// Shared send path for `msg.send` and the TUI composer.
    async fn send_text(
        self: &Arc<Self>,
        peer: PeerId,
        body: &str,
        client_msg_id: &str,
    ) -> Result<(String, String), RpcError> {
        self.check_send_allowed(&peer).await?;
        let (live, offline_gate) = {
            let sh = self.sh();
            let known_offline = sh.presence.get(&peer).map(|o| !*o).unwrap_or(false);
            let has_grant = sh.meta.get(&peer).and_then(|m| m.grant.as_ref()).is_some();
            // Inlined from `session()`: `shared` is already locked here —
            // re-locking a std::sync::Mutex on the same thread deadlocks.
            let live = sh
                .sessions
                .get(&peer)
                .map(|ps| {
                    !ps.session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_closed()
                })
                .unwrap_or(false);
            (live, known_offline && !has_grant)
        };
        if !live && offline_gate {
            return Err(RpcError::new(
                ErrorKind::PeerOffline,
                "peer offline and no mailbox path",
            ));
        }
        let chat_id = chat_str(&peer);
        let mid = new_msg_id();
        let inserted = self
            .sblk({
                let chat_id = chat_id.clone();
                let body = body.to_string();
                let mid = mid.clone();
                let cmid = client_msg_id.to_string();
                move |s| {
                    s.insert_message(&NewMessage {
                        msg_id: mid,
                        chat_id,
                        dir: Dir::Out,
                        kind: MsgKind::Text,
                        body,
                        state: Some(MsgState::Queued),
                        client_msg_id: Some(cmid),
                        ts: now_ms(),
                    })
                }
            })
            .await
            .map_err(store_rpc)?;
        let (mid, deduped) = inserted;
        if deduped {
            return Ok((mid, chat_str(&peer)));
        }
        self.emit(
            "msg_state",
            Some(&chat_id),
            json!({
                "chat_id": chat_id,
                "msg_id": mid,
                "client_msg_id": client_msg_id,
                "state": "queued",
            }),
        );
        let me = self.clone();
        let item = PendingMsg {
            app: AppMsg::Text {
                msg_id: mid.clone(),
                body: body.to_string(),
            },
            msg_id: Some(mid.clone()),
            client_msg_id: Some(client_msg_id.to_string()),
            chat_id: chat_id.clone(),
        };
        let chat_id2 = chat_id.clone();
        tokio::spawn(async move {
            me.deliver_or_queue(peer, item).await;
        });
        Ok((mid, chat_id2))
    }

    /// `contact.add` / `invite.accept` / `cinder add`: parse URI, create the
    /// pending contact, spawn the initiator driver. `contact.connected` (or
    /// a substate flip on failure) is delivered as events.
    pub async fn start_add(
        self: &Arc<Self>,
        link: &str,
        display_name: Option<String>,
    ) -> Result<PeerId, RpcError> {
        let (static_key, host, invite) = Identity::parse_uri(link)
            .map_err(|e| RpcError::new(ErrorKind::InvalidParams, format!("bad link: {e}")))?;
        let peer = PeerId::from_static_key(&static_key);
        {
            let mut sh = self.sh();
            if let Some(m) = sh.meta.get(&peer) {
                // Retryable: declined, or a pending_out whose substate is a
                // terminal failure — a failed add must never wedge the peer.
                let retryable = m.state == MetaState::Declined
                    || (m.state == MetaState::PendingOut
                        && m.substate
                            .as_deref()
                            .map(|s| s.starts_with("failed"))
                            .unwrap_or(false));
                if !retryable {
                    return Err(RpcError::new(
                        ErrorKind::Conflict,
                        format!("contact already exists ({})", m.state.as_str()),
                    ));
                }
            }
            let mut m = ContactMeta::new(MetaState::PendingOut, display_name.clone());
            m.substate = Some("connecting".into());
            sh.meta.insert(peer, m);
        }
        self.persist_meta();
        if let Some(ours) = self.relay_host() {
            if host != ours && self.conn_state() == tb::ConnState::Connected {
                self.set_meta(&peer, |m| m.substate = Some("failed:wrong_relay".into()));
                return Err(RpcError::new(
                    ErrorKind::NotConnected,
                    "invite lives on a different relay than this process is connected to",
                ));
            }
        }
        if self.conn_state() != tb::ConnState::Connected {
            // Stays pending_out; retried when the link comes up.
            return Ok(peer);
        }
        {
            let mut sh = self.sh();
            if !sh.hs_inflight.insert(peer) {
                return Ok(peer);
            }
        }
        let c = self.clone();
        tokio::spawn(async move {
            c.outbound_handshake(peer, invite).await;
        });
        Ok(peer)
    }

    /// `cinder id` + `invite.create`: register a rendezvous slot on the relay
    /// and return the shareable link (plus the expires_at we were granted).
    pub async fn invite_link(&self, ttl_secs: u32) -> Result<(String, u64), RpcError> {
        if self.conn_state() != tb::ConnState::Connected {
            return Err(RpcError::new(
                ErrorKind::NotConnected,
                "relay link down; invite slots live on the relay",
            ));
        }
        let token = InviteToken::random();
        let ev = self
            .request(ClientMsg::InviteCreate { token, ttl_secs }, ReqSlot::Invite)
            .await
            .map_err(|e| RpcError::new(ErrorKind::NotConnected, e.to_string()))?;
        let ClientEvent::InviteCreated { expires_at, .. } = ev else {
            return Err(RpcError::internal("unexpected invite reply"));
        };
        let id = new_invite_id();
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invites_open
            .push((token, id.clone()));
        let host = self.relay_host().unwrap_or_default();
        Ok((self.identity.uri(&host, Some(&token)), expires_at))
    }
}

// ---------- JSON shape helpers -------------------------------------------------

fn state_str(s: MsgState) -> &'static str {
    match s {
        MsgState::Queued => "queued",
        MsgState::Sent => "sent",
        MsgState::Delivered => "delivered",
        MsgState::Read => "read",
        MsgState::Failed => "failed",
        MsgState::Expired => "expired",
    }
}

fn dir_str(d: Dir) -> &'static str {
    match d {
        Dir::In => "in",
        Dir::Out => "out",
    }
}

fn kind_str(k: MsgKind) -> &'static str {
    match k {
        MsgKind::Text => "text",
        MsgKind::Edit => "edit",
        MsgKind::Delete => "delete",
        MsgKind::Reaction => "reaction",
    }
}

fn trust_str(t: Option<&TrustState>) -> &'static str {
    match t {
        Some(TrustState::Verified) => "verified",
        Some(TrustState::KeyChanged { .. }) => "key_changed",
        _ => "unverified",
    }
}

fn message_json(m: &crate::store::Message, sender: &PeerId, own_peer_str: &str) -> Value {
    let from = match m.dir {
        Dir::In => peer_str(sender),
        Dir::Out => own_peer_str.to_string(),
    };
    json!({
        "msg_id": m.msg_id,
        "chat_id": m.chat_id,
        "dir": dir_str(m.dir),
        "from": from,
        "kind": kind_str(m.kind),
        "body": {"text": m.body, "trusted": m.dir == Dir::Out},
        "state": m.state.map(state_str),
        "ts": rfc3339(m.ts),
        "ts_ms": m.ts,
        "revision": m.revision,
        "reactions": m.reactions.iter().map(|r| {
            json!({
                "emoji": r.emoji,
                "from": peer_str(&PeerId::from_bytes(r.reactor)),
                "mine": b32(&r.reactor) == own_peer_str.trim_start_matches("peer:"),
            })
        }).collect::<Vec<_>>(),
    })
}

fn contact_json(
    peer: &PeerId,
    meta: &ContactMeta,
    name: Option<&str>,
    trust: Option<&TrustState>,
) -> Value {
    let active = meta.state == MetaState::Active;
    json!({
        "contact_id": contact_str(peer),
        "peer_id": peer_str(peer),
        "display_name": meta.display_name.clone().or_else(|| name.map(String::from)),
        "state": meta.state.as_str(),
        "substate": meta.substate.clone(),
        "trust": trust_str(trust),
        "chat_id": active.then(|| chat_str(peer)),
        "created_at": rfc3339(meta.created_at),
    })
}

fn chat_json(
    chat: &crate::store::Chat,
    meta: Option<&ContactMeta>,
    name: Option<&str>,
    trust: Option<&TrustState>,
) -> Value {
    let peer = PeerId::from_bytes(chat.peer_id);
    json!({
        "chat_id": chat.chat_id,
        "kind": "direct",
        "contact_id": contact_str(&peer),
        "peer_id": peer_str(&peer),
        "display_name": meta.and_then(|m| m.display_name.clone()).or_else(|| name.map(String::from)),
        "trust": trust_str(trust),
        "unread": chat.unread.max(0) as u32,
        "last_activity": rfc3339(chat.last_activity),
        "last_message": chat.last_message.as_ref().map(|m| message_json(m, &peer, "")),
    })
}

fn store_rpc(e: StoreError) -> RpcError {
    match e {
        StoreError::Conflict => RpcError::new(
            ErrorKind::Conflict,
            "client_msg_id replayed with a different payload",
        ),
        StoreError::NotFound(id) => RpcError::new(ErrorKind::NotFound, format!("{id}")),
        StoreError::InvalidCursor => RpcError::new(ErrorKind::InvalidParams, "invalid cursor"),
        StoreError::InvalidTransition { .. } => {
            RpcError::new(ErrorKind::Conflict, "invalid message state transition")
        }
        other => RpcError::internal(format!("store: {other}")),
    }
}

fn ui_event(kind: &str, p: &Value) -> Option<tb::Event> {
    match kind {
        "message.received" => {
            let m = &p["message"];
            Some(tb::Event::MessageReceived(tb::Message {
                msg_id: m["msg_id"].as_str()?.to_string(),
                chat_id: m["chat_id"].as_str()?.to_string(),
                dir: tb::Dir::In,
                from: m["from"].as_str().unwrap_or("").to_string(),
                text: m["body"]["text"].as_str().unwrap_or("").to_string(),
                state: tb::MsgState::Delivered,
                ts: m["ts_ms"].as_i64().unwrap_or(0) / 1000,
            }))
        }
        "msg_state" => Some(tb::Event::MsgState {
            msg_id: p["msg_id"].as_str()?.to_string(),
            client_msg_id: p["client_msg_id"].as_str().unwrap_or("").to_string(),
            chat_id: p["chat_id"].as_str()?.to_string(),
            state: match p["state"].as_str() {
                Some("sent") => tb::MsgState::Sent,
                Some("delivered") => tb::MsgState::Delivered,
                Some("queued") => tb::MsgState::Queued,
                _ => tb::MsgState::Failed,
            },
        }),
        "contact.connected" => Some(tb::Event::ContactConnected {
            contact_id: p["contact_id"].as_str()?.to_string(),
            peer_id: p["peer_id"].as_str()?.to_string(),
            chat_id: p["chat_id"].as_str().unwrap_or("").to_string(),
            display_name: None,
            trust: match p["trust"].as_str() {
                Some("verified") => tb::Trust::Verified,
                Some("key_changed") => tb::Trust::KeyChanged,
                _ => tb::Trust::Unverified,
            },
        }),
        "security.identity_changed" => Some(tb::Event::IdentityChanged {
            contact_id: p["contact_id"].as_str()?.to_string(),
            peer_id: p["peer_id"].as_str()?.to_string(),
            chat_id: String::new(),
        }),
        "security.verification_needed" => Some(tb::Event::VerificationNeeded {
            contact_id: p["contact_id"].as_str()?.to_string(),
            peer_id: p["peer_id"].as_str()?.to_string(),
            sas: p["sas_hint"]["emoji"].as_str().unwrap_or("").to_string(),
        }),
        _ => None,
    }
}

// ---------- CinderCore (RPC seam) ----------------------------------------------

#[async_trait]
impl CinderCore for Core {
    async fn session_identity(&self) -> Result<Value, RpcError> {
        Ok(json!({
            "peer_id": self.peer_id_str,
            "display_name": Value::Null,
        }))
    }

    async fn msg_send(&self, p: MsgSendParams) -> Result<Value, RpcError> {
        let peer = self.resolve_to(&p.to)?;
        let (msg_id, chat_id) = self
            .arc()
            .send_text(peer, &p.body, &p.client_msg_id)
            .await?;
        Ok(json!({"msg_id": msg_id, "chat_id": chat_id}))
    }

    async fn msg_edit(&self, p: MsgEditParams) -> Result<Value, RpcError> {
        let m = self
            .sblk({
                let id = p.msg_id.clone();
                move |s| s.get_message(&id)
            })
            .await
            .map_err(store_rpc)?
            .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "msg_id not found"))?;
        if m.dir != Dir::Out {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "cannot edit a message you did not send",
            ));
        }
        if m.deleted {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "message already deleted",
            ));
        }
        let peer = peer_of_chat(&m.chat_id)?;
        self.check_send_allowed(&peer).await?;
        self.sblk({
            let id = p.msg_id.clone();
            let body = p.body.clone();
            move |s| s.edit_message(&id, &body)
        })
        .await
        .map_err(store_rpc)?;
        let revision = self
            .sblk({
                let id = p.msg_id.clone();
                move |s| s.get_message(&id).map(|o| o.map(|m| m.revision))
            })
            .await
            .map_err(store_rpc)?
            .unwrap_or(0);
        let me = self.arc();
        let chat_id = m.chat_id.clone();
        let msg_id = m.msg_id.clone();
        tokio::spawn(async move {
            me.deliver_or_queue(
                peer,
                PendingMsg {
                    app: AppMsg::Edit {
                        msg_id: msg_id.clone(),
                        body: p.body.clone(),
                    },
                    msg_id: Some(msg_id),
                    client_msg_id: p.client_op_id.clone(),
                    chat_id,
                },
            )
            .await;
        });
        Ok(json!({
            "msg_id": m.msg_id,
            "chat_id": m.chat_id,
            "revision": revision,
            "state": "queued",
        }))
    }

    async fn msg_delete(&self, p: MsgDeleteParams) -> Result<Value, RpcError> {
        let m = self
            .sblk({
                let id = p.msg_id.clone();
                move |s| s.get_message(&id)
            })
            .await
            .map_err(store_rpc)?
            .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "msg_id not found"))?;
        if m.deleted {
            return Ok(json!({
                "msg_id": m.msg_id,
                "chat_id": m.chat_id,
                "deleted": true,
                "scope": "local",
                "state": "done",
            }));
        }
        let scope = p.scope.as_deref().unwrap_or("everyone");
        self.sblk({
            let id = p.msg_id.clone();
            move |s| s.soft_delete(&id)
        })
        .await
        .map_err(store_rpc)?;
        if scope == "everyone" {
            if m.dir != Dir::Out {
                return Err(RpcError::new(
                    ErrorKind::Conflict,
                    "delete for everyone applies to own messages only",
                ));
            }
            let peer = peer_of_chat(&m.chat_id)?;
            let me = self.arc();
            let chat_id = m.chat_id.clone();
            let msg_id = m.msg_id.clone();
            tokio::spawn(async move {
                me.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::Delete {
                            msg_id: msg_id.clone(),
                        },
                        msg_id: Some(msg_id),
                        client_msg_id: p.client_op_id.clone(),
                        chat_id,
                    },
                )
                .await;
            });
            return Ok(json!({
                "msg_id": m.msg_id,
                "chat_id": m.chat_id,
                "deleted": true,
                "scope": "everyone",
                "state": "queued",
            }));
        }
        Ok(json!({
            "msg_id": m.msg_id,
            "chat_id": m.chat_id,
            "deleted": true,
            "scope": "local",
            "state": "done",
        }))
    }

    async fn msg_react(&self, p: MsgReactParams) -> Result<Value, RpcError> {
        let m = self
            .sblk({
                let id = p.msg_id.clone();
                move |s| s.get_message(&id)
            })
            .await
            .map_err(store_rpc)?
            .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "msg_id not found"))?;
        let peer = peer_of_chat(&m.chat_id)?;
        // Reactions are outbound traffic: same gates as send/edit/delete —
        // declined/blocked and undismissed key-changes refuse.
        self.check_send_allowed(&peer).await?;
        if let Some(e) = &p.emoji {
            if e.len() > EMOJI_MAX_BYTES {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "emoji exceeds 32 bytes",
                ));
            }
        }
        let me_pk = self.identity.public();
        match p.emoji.clone() {
            Some(e) => {
                self.sblk({
                    let id = p.msg_id.clone();
                    move |s| s.add_reaction(&id, &e, &me_pk)
                })
                .await
                .map_err(store_rpc)?;
            }
            None => {
                self.sblk({
                    let id = p.msg_id.clone();
                    move |s| s.remove_reaction(&id, None, &me_pk)
                })
                .await
                .map_err(store_rpc)?;
            }
        }
        let me = self.arc();
        let chat_id = m.chat_id.clone();
        let msg_id = m.msg_id.clone();
        tokio::spawn(async move {
            me.deliver_or_queue(
                peer,
                PendingMsg {
                    app: AppMsg::React {
                        msg_id: msg_id.clone(),
                        emoji: p.emoji.clone(),
                    },
                    msg_id: Some(msg_id),
                    client_msg_id: None,
                    chat_id,
                },
            )
            .await;
        });
        let m = self
            .sblk(move |s| s.get_message(&p.msg_id))
            .await
            .map_err(store_rpc)?
            .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "msg_id not found"))?;
        let reactions: Vec<Value> = m
            .reactions
            .iter()
            .map(|r| {
                json!({
                    "emoji": r.emoji,
                    "from": peer_str(&PeerId::from_bytes(r.reactor)),
                    "mine": r.reactor == me_pk,
                })
            })
            .collect();
        Ok(json!({
            "msg_id": m.msg_id,
            "chat_id": m.chat_id,
            "reactions": reactions,
            "state": "queued",
        }))
    }

    async fn chat_list(&self, p: ChatListParams) -> Result<Value, RpcError> {
        let limit = p.limit.unwrap_or(50).min(200) as usize;
        let offset = match &p.cursor {
            None => 0usize,
            Some(c) => c
                .strip_prefix("cur:off:")
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "invalid cursor"))?,
        };
        let chats = self
            .sblk(move |s| s.list_chats_sorted((offset + limit + 1) as u32))
            .await
            .map_err(store_rpc)?;
        let (metas, trusts, names) = self.snapshot_contacts(&chats).await;
        let mut out = Vec::new();
        for (i, chat) in chats.iter().enumerate() {
            if i < offset {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            let peer = PeerId::from_bytes(chat.peer_id);
            out.push(chat_json(
                chat,
                metas.get(&peer),
                names.get(&peer).map(String::as_str),
                trusts.get(&peer),
            ));
        }
        let next_cursor =
            (offset + out.len() < chats.len()).then(|| format!("cur:off:{}", offset + out.len()));
        Ok(json!({"chats": out, "next_cursor": next_cursor}))
    }

    async fn chat_history(&self, p: ChatHistoryParams) -> Result<Value, RpcError> {
        let peer = peer_of_chat(&p.chat_id)
            .map_err(|_| RpcError::new(ErrorKind::NotFound, "chat not found"))?;
        let limit = p.limit.unwrap_or(50);
        let before = p.before.clone();
        let chat_id = p.chat_id.clone();
        let (msgs, next) = self
            .sblk(move |s| s.window(&chat_id, before.as_deref(), limit))
            .await
            .map_err(store_rpc)?;
        let has_more = next.is_some();
        // Reading history is the read receipt: mark the chat read up to the
        // newest returned row. No extra event is emitted for this.
        if let Some(ts) = msgs.iter().map(|m| m.ts).max() {
            let cid = p.chat_id.clone();
            let _ = self.sblk(move |s| s.mark_read_upto(&cid, ts)).await;
        }
        let own = self.peer_id_str.clone();
        let items: Vec<Value> = msgs.iter().map(|m| message_json(m, &peer, &own)).collect();
        Ok(json!({
            "chat_id": p.chat_id,
            "messages": items,
            "has_more": has_more,
            "next_before": next,
        }))
    }

    async fn contact_list(&self, p: ContactListParams) -> Result<Value, RpcError> {
        let filter = p.state.clone();
        let (peers, metas) = {
            let sh = self.sh();
            (
                sh.meta.keys().cloned().collect::<Vec<PeerId>>(),
                sh.meta.clone(),
            )
        };
        let (names, trusts) = self
            .cblk(move |c| {
                let mut names = HashMap::new();
                let mut trusts = HashMap::new();
                for peer in &peers {
                    if let Some(ct) = c.get(peer) {
                        if !ct.name.is_empty() {
                            names.insert(*peer, ct.name.clone());
                        }
                        trusts.insert(*peer, ct.state.clone());
                    }
                }
                (names, trusts)
            })
            .await
            .unwrap_or_default();
        let mut items: Vec<Value> = metas
            .iter()
            .filter(|(_, m)| {
                filter
                    .as_ref()
                    .map(|f| m.state.as_str() == f.as_str())
                    .unwrap_or(true)
            })
            .map(|(peer, m)| {
                contact_json(
                    peer,
                    m,
                    names.get(peer).map(String::as_str),
                    trusts.get(peer),
                )
            })
            .collect();
        items.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
        Ok(json!({"contacts": items}))
    }

    async fn contact_add(&self, p: ContactAddParams) -> Result<Value, RpcError> {
        let peer = self.arc().start_add(&p.link, p.display_name).await?;
        let substate = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .get(&peer)
            .and_then(|m| m.substate.clone());
        Ok(json!({
            "contact_id": contact_str(&peer),
            "state": "pending_out",
            "substate": substate,
        }))
    }

    async fn contact_accept(&self, p: ContactIdParams) -> Result<Value, RpcError> {
        let peer = self.peer_of_contact_id(&p.contact_id)?;
        let state = self.meta_state(&peer);
        match state {
            Some(MetaState::PendingIn) => {}
            Some(MetaState::Active) => {
                return Ok(json!({
                    "contact_id": p.contact_id,
                    "state": "active",
                    "chat_id": chat_str(&peer),
                }));
            }
            _ => {
                return Err(RpcError::new(
                    ErrorKind::Conflict,
                    format!("cannot accept contact in state {:?}", state),
                ));
            }
        }
        self.set_meta(&peer, |m| m.state = MetaState::Active);
        let verified = self.contact_verified(&peer).await;
        let _ = self
            .sblk(move |s| s.upsert_chat(&chat_str(&peer), peer.as_bytes(), None, verified))
            .await;
        let trust = self
            .cblk(move |c| c.get(&peer).map(|ct| ct.state.clone()))
            .await
            .flatten();
        self.emit(
            "contact.connected",
            None,
            json!({
                "contact_id": contact_str(&peer),
                "peer_id": peer_str(&peer),
                "chat_id": chat_str(&peer),
                "direction": "in",
                "trust": trust_str(trust.as_ref()),
            }),
        );
        self.maybe_emit_verification(&peer, &trust);
        Ok(json!({
            "contact_id": p.contact_id,
            "state": "active",
            "chat_id": chat_str(&peer),
        }))
    }

    async fn contact_decline(&self, p: ContactIdParams) -> Result<Value, RpcError> {
        let peer = self.peer_of_contact_id(&p.contact_id)?;
        let state = self.meta_state(&peer);
        match state {
            Some(MetaState::PendingIn) => {}
            Some(MetaState::Declined) => {
                return Ok(json!({"contact_id": p.contact_id, "state": "declined"}));
            }
            _ => {
                return Err(RpcError::new(
                    ErrorKind::Conflict,
                    format!("cannot decline contact in state {:?}", state),
                ));
            }
        }
        self.set_meta(&peer, |m| m.state = MetaState::Declined);
        // The declined peer's live session goes too: their next frame hits
        // an unknown sid and dies, and further Msg1s are dropped by the
        // inbound gate.
        self.drop_session(&peer);
        Ok(json!({"contact_id": p.contact_id, "state": "declined"}))
    }

    async fn invite_create(&self, p: InviteCreateParams) -> Result<Value, RpcError> {
        let ttl = p.ttl_sec.unwrap_or(INVITE_TTL_SECS as u64) as u32;
        let (link, expires_at) = self.invite_link(ttl).await?;
        let id = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invites_open
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_default();
        Ok(json!({
            "invite_id": id,
            "link": link,
            "expires_at": rfc3339(expires_at as i64 * 1000),
        }))
    }

    async fn invite_accept(&self, p: InviteAcceptParams) -> Result<Value, RpcError> {
        let peer = self.arc().start_add(&p.link, p.display_name).await?;
        Ok(json!({
            "contact_id": contact_str(&peer),
            "state": "pending_out",
            "substate": "connecting",
            "peer_id": peer_str(&peer),
        }))
    }

    async fn ident_verify(&self, p: IdentVerifyParams) -> Result<Value, RpcError> {
        let peer = self.peer_of_contact_id(&p.contact_id)?;
        if self.meta_state(&peer).is_none() {
            return Err(RpcError::new(ErrorKind::NotFound, "unknown contact"));
        }
        let given = p.sas.split_whitespace().collect::<Vec<_>>().join(" ");
        let (emoji, digits) = {
            let sh = self.sh();
            let m = sh.meta.get(&peer);
            (
                m.and_then(|m| m.sas_emoji.clone()),
                m.and_then(|m| m.sas_digits.clone()),
            )
        };
        let matches_sas = [emoji.as_deref(), digits.as_deref()]
            .into_iter()
            .flatten()
            .any(|s| s.split_whitespace().collect::<Vec<_>>().join(" ") == given);
        if !matches_sas {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "sas mismatch; re-read out-of-band before verifying",
            ));
        }
        self.cblk(move |c| c.mark_verified(&peer))
            .await
            .ok_or_else(|| RpcError::internal("contacts store unavailable"))?
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let _ = self
            .sblk(move |s| s.upsert_chat(&chat_str(&peer), peer.as_bytes(), None, true))
            .await;
        Ok(json!({
            "contact_id": p.contact_id,
            "trust": "verified",
            "verified_at": rfc3339(now_ms()),
        }))
    }

    async fn backlog_summary(&self) -> Result<Value, RpcError> {
        let unread = self
            .sblk(|s| {
                s.list_chats_sorted(500)
                    .map(|cs| cs.iter().filter(|c| c.unread > 0).count() as u64)
            })
            .await
            .unwrap_or(0);
        let sh = self.sh();
        let pending_in = sh
            .meta
            .values()
            .filter(|m| m.state == MetaState::PendingIn)
            .count() as u64;
        let queued: u64 = sh.pending_out.values().map(|q| q.len() as u64).sum();
        Ok(json!({
            "unread_chats": unread,
            "pending_contact_requests": pending_in,
            "queued_outbound": queued,
            "dropped_inbound": sh.dropped_inbound,
        }))
    }

    async fn shutdown(&self, _grace_ms: u64) -> Result<(), RpcError> {
        self.begin_shutdown();
        Ok(())
    }
}

impl Core {
    async fn snapshot_contacts(
        &self,
        chats: &[crate::store::Chat],
    ) -> (
        HashMap<PeerId, ContactMeta>,
        HashMap<PeerId, TrustState>,
        HashMap<PeerId, String>,
    ) {
        let metas: HashMap<PeerId, ContactMeta> = { self.sh().meta.clone() };
        let peers: Vec<PeerId> = chats
            .iter()
            .map(|c| PeerId::from_bytes(c.peer_id))
            .collect();
        let (names, trusts) = self
            .cblk(move |c| {
                let mut names = HashMap::new();
                let mut trusts = HashMap::new();
                for peer in &peers {
                    if let Some(ct) = c.get(peer) {
                        if !ct.name.is_empty() {
                            names.insert(*peer, ct.name.clone());
                        }
                        trusts.insert(*peer, ct.state.clone());
                    }
                }
                (names, trusts)
            })
            .await
            .unwrap_or_default();
        (metas, trusts, names)
    }
}

fn peer_of_chat(chat_id: &str) -> Result<PeerId, RpcError> {
    let rest = chat_id
        .strip_prefix("chat:")
        .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "expected a chat: id"))?;
    let bytes = b32_decode_32(rest)
        .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "malformed chat_id"))?;
    Ok(PeerId::from_bytes(bytes))
}

// ---------- TUI backend ----------------------------------------------------------

pub struct TuiBackend {
    core: Arc<Core>,
    peer_id: String,
}

impl TuiBackend {
    pub fn new(core: Arc<Core>) -> Self {
        let peer_id = core.peer_id_str.clone();
        Self { core, peer_id }
    }
}

#[allow(async_fn_in_trait)]
impl tb::Backend for TuiBackend {
    fn peer_id(&self) -> &str {
        &self.peer_id
    }

    fn conn_state(&self) -> tb::ConnState {
        self.core.conn_state()
    }

    async fn list_chats(&mut self) -> Result<Vec<tb::ChatSummary>> {
        let core = self.core.clone();
        let chats = core.sblk(|s| s.list_chats_sorted(500)).await?;
        let metas: HashMap<PeerId, ContactMeta> = core
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .clone();
        let peers: Vec<PeerId> = chats
            .iter()
            .map(|c| PeerId::from_bytes(c.peer_id))
            .collect();
        let (names, trusts) = core
            .cblk(move |c| {
                let mut names = HashMap::new();
                let mut trusts = HashMap::new();
                for peer in &peers {
                    if let Some(ct) = c.get(peer) {
                        if !ct.name.is_empty() {
                            names.insert(*peer, ct.name.clone());
                        }
                        trusts.insert(*peer, ct.state.clone());
                    }
                }
                (names, trusts)
            })
            .await
            .unwrap_or_default();
        Ok(chats
            .into_iter()
            .map(|chat| {
                let peer = PeerId::from_bytes(chat.peer_id);
                let meta = metas.get(&peer);
                tb::ChatSummary {
                    chat_id: chat.chat_id.clone(),
                    contact_id: contact_str(&peer),
                    peer_id: peer_str(&peer),
                    display_name: meta
                        .and_then(|m| m.display_name.clone())
                        .or_else(|| names.get(&peer).cloned())
                        .or(chat.title.clone()),
                    trust: match trusts.get(&peer) {
                        Some(TrustState::Verified) => tb::Trust::Verified,
                        Some(TrustState::KeyChanged { .. }) => tb::Trust::KeyChanged,
                        _ => tb::Trust::Unverified,
                    },
                    unread: chat.unread.max(0) as u32,
                    last_activity: chat.last_activity / 1000,
                    last_message: chat.last_message.map(|m| m.body),
                }
            })
            .collect())
    }

    async fn history(
        &mut self,
        chat_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<tb::HistoryPage> {
        let core = self.core.clone();
        let peer = peer_of_chat(chat_id).map_err(|e| anyhow!("{e:?}"))?;
        let chat_id_owned = chat_id.to_string();
        let before = before.map(String::from);
        let (msgs, next) = core
            .sblk(move |s| s.window(&chat_id_owned, before.as_deref(), limit as u32))
            .await?;
        // Reading a chat's history marks it read; no extra event is emitted.
        if let Some(ts) = msgs.iter().map(|m| m.ts).max() {
            let cid = chat_id.to_string();
            let _ = core.sblk(move |s| s.mark_read_upto(&cid, ts)).await;
        }
        let own = self.peer_id.clone();
        Ok(tb::HistoryPage {
            messages: msgs
                .into_iter()
                .map(|m| tb::Message {
                    msg_id: m.msg_id,
                    chat_id: m.chat_id,
                    dir: match m.dir {
                        Dir::In => tb::Dir::In,
                        Dir::Out => tb::Dir::Out,
                    },
                    from: match m.dir {
                        Dir::In => peer_str(&peer),
                        Dir::Out => own.clone(),
                    },
                    text: m.body,
                    state: match m.state {
                        Some(MsgState::Delivered) => tb::MsgState::Delivered,
                        Some(MsgState::Failed) | Some(MsgState::Expired) => tb::MsgState::Failed,
                        Some(MsgState::Sent) => tb::MsgState::Sent,
                        _ => tb::MsgState::Queued,
                    },
                    ts: m.ts / 1000,
                })
                .collect(),
            has_more: next.is_some(),
            next_before: next,
        })
    }

    async fn send(&mut self, chat_id: &str, body: &str) -> Result<tb::SendAck> {
        let peer = peer_of_chat(chat_id).map_err(|e| anyhow!("{e:?}"))?;
        let cmid = format!("tui:{}", b32(HsId::random().as_bytes()));
        let (msg_id, _) = self
            .core
            .clone()
            .send_text(peer, body, &cmid)
            .await
            .map_err(|e| {
                anyhow!(
                    "{}: {}",
                    e.kind.as_str(),
                    e.hint.as_deref().unwrap_or(&e.message)
                )
            })?;
        Ok(tb::SendAck {
            msg_id,
            client_msg_id: cmid,
        })
    }

    async fn contacts(&mut self) -> Result<Vec<tb::Contact>> {
        let metas: HashMap<PeerId, ContactMeta> = self
            .core
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .clone();
        let peers: Vec<PeerId> = metas.keys().cloned().collect();
        let (names, trusts) = self
            .core
            .cblk(move |c| {
                let mut names = HashMap::new();
                let mut trusts = HashMap::new();
                for peer in &peers {
                    if let Some(ct) = c.get(peer) {
                        if !ct.name.is_empty() {
                            names.insert(*peer, ct.name.clone());
                        }
                        trusts.insert(*peer, ct.state.clone());
                    }
                }
                (names, trusts)
            })
            .await
            .unwrap_or_default();
        let mut out: Vec<tb::Contact> = metas
            .iter()
            .map(|(peer, m)| tb::Contact {
                contact_id: contact_str(peer),
                peer_id: peer_str(peer),
                display_name: m.display_name.clone().or_else(|| names.get(peer).cloned()),
                state: match m.state {
                    MetaState::PendingOut => tb::ContactState::PendingOut,
                    MetaState::PendingIn => tb::ContactState::PendingIn,
                    MetaState::Active => tb::ContactState::Active,
                    MetaState::Declined => tb::ContactState::Declined,
                    MetaState::Blocked => tb::ContactState::Blocked,
                },
                trust: match trusts.get(peer) {
                    Some(TrustState::Verified) => tb::Trust::Verified,
                    Some(TrustState::KeyChanged { .. }) => tb::Trust::KeyChanged,
                    _ => tb::Trust::Unverified,
                },
                chat_id: (m.state == MetaState::Active).then(|| chat_str(peer)),
            })
            .collect();
        out.sort_by(|a, b| a.contact_id.cmp(&b.contact_id));
        Ok(out)
    }

    async fn add_contact(&mut self, uri: &str) -> Result<String> {
        let peer = self.core.clone().start_add(uri, None).await.map_err(|e| {
            anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )
        })?;
        Ok(contact_str(&peer))
    }

    async fn verify_contact(&mut self, contact_id: &str, sas: &str) -> Result<()> {
        let params = IdentVerifyParams {
            contact_id: contact_id.to_string(),
            sas: sas.to_string(),
        };
        CinderCore::ident_verify(self.core.as_ref(), params)
            .await
            .map_err(|e| {
                anyhow!(
                    "{}: {}",
                    e.kind.as_str(),
                    e.hint.as_deref().unwrap_or(&e.message)
                )
            })?;
        Ok(())
    }

    async fn sas_hint(&mut self, contact_id: &str) -> Result<String> {
        let peer = self.core.peer_of_contact_id(contact_id).map_err(|e| {
            anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )
        })?;
        let sh = self.core.shared.lock().unwrap_or_else(|e| e.into_inner());
        Ok(sh
            .meta
            .get(&peer)
            .and_then(|m| m.sas_emoji.clone().or(m.sas_digits.clone()))
            .unwrap_or_default())
    }

    fn subscribe(&mut self) -> mpsc::Receiver<tb::Event> {
        let mut rx = self.core.ui_subscribe();
        let (tx, out) = mpsc::channel(256);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        out
    }
}

// ---------- sqlite-backed replay log --------------------------------------------

/// Bounded, ordered replay log persisted to `store.event_log`. Reads come
/// from a memory mirror warmed at open; appends write through to sqlite on a
/// writer task so the engine's `emit` never blocks on disk.
pub struct SqliteReplayLog {
    cap: usize,
    events: VecDeque<EventRecord>,
    latest: u64,
    tx: mpsc::UnboundedSender<EventRecord>,
}

impl SqliteReplayLog {
    pub async fn open(store: Arc<Store>, cap: usize) -> Result<Self> {
        let (events, latest) = {
            let s = store.clone();
            let cap_u32 = cap.clamp(1, u32::MAX as usize) as u32;
            tokio::task::spawn_blocking(move || -> Result<_, StoreError> {
                let page = s.events_since(0, cap_u32)?;
                let recs: VecDeque<EventRecord> = page
                    .events
                    .into_iter()
                    .map(|e| EventRecord {
                        seq: e.seq,
                        ts: rfc3339(e.ts),
                        kind: e.kind,
                        chat_id: e.chat_id,
                        payload: e.payload,
                    })
                    .collect();
                Ok((recs, page.latest_seq))
            })
            .await
            .expect("store task panicked")?
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<EventRecord>();
        tokio::spawn(async move {
            while let Some(rec) = rx.recv().await {
                let s = store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    s.append_event(&rec.kind, rec.chat_id.as_deref(), &rec.payload)
                })
                .await;
            }
        });
        Ok(Self {
            cap: cap.max(1),
            events,
            latest,
            tx,
        })
    }
}

impl ReplayLog for SqliteReplayLog {
    fn append(&mut self, record: EventRecord) {
        self.latest = self.latest.max(record.seq);
        let _ = self.tx.send(record.clone());
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
