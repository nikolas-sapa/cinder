//! The live messenger core: owns the identity, contacts pin store, sqlite
//! store, the relay link, and all Noise `Session`s. The RPC engine and the
//! TUI both drive this one object; the pump task is the only owner of the
//! `RelayTransport`, and `Session`s are never serialized.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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

use crate::call::{self, CallCaps, CallDir, CallRt, CallState, EndReason, Quality};
use crate::rpc::core::{
    CallIdParams, CallMuteParams, CallRingParams, ChatHistoryParams, ChatListParams,
    ChatSetDisappearingParams, CinderCore, ContactAddParams, ContactIdParams, ContactListParams,
    EventSink, FileIdParams, FileListParams, FileSaveParams, FileSendParams, IdentVerifyParams,
    InviteAcceptParams, InviteCreateParams, MsgDeleteParams, MsgEditParams, MsgReactParams,
    MsgSendParams,
};
use crate::rpc::events::{EventRecord, ReplayLog};
use crate::rpc::protocol::{ErrorKind, RpcError};
use crate::store::{Dir, FileRow, MsgKind, MsgState, NewFile, NewMessage, Store, StoreError};
use crate::tui::backend as tb;
use crate::xfer::{self, AudioMeta, FileCancelReason, FileKind, FileState, XferCaps};

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
/// Expiry sweep cadence for disappearing messages.
const EXPIRY_SWEEP: Duration = Duration::from_secs(1);
/// Orphan `.part` grace: blobs younger than this are never reaped — a row
/// insert in flight or a mid-append chunk writer must not lose its staging.
const PART_ORPHAN_GRACE: Duration = Duration::from_secs(60);

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

fn new_file_id() -> ([u8; 32], String) {
    let raw = *Token::random().as_bytes();
    (raw, format!("file:{}", b32(&raw)))
}

fn file_id_str(raw: &[u8; 32]) -> String {
    format!("file:{}", b32(raw))
}

fn file_id_raw(s: &str) -> Option<[u8; 32]> {
    b32_decode_32(s.strip_prefix("file:")?)
}

/// Positioned read of one outbound chunk from the staged blob (pump lane).
fn read_chunk_at(abs: &Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(abs)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
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
#[allow(clippy::large_enum_variant)] // wire enum; FileOffer is metadata-heavy by design
pub(crate) enum AppMsg {
    Text {
        msg_id: String,
        body: String,
        /// Absolute expiry deadline (unix ms) for disappearing messages.
        /// Travels inside the Noise channel — the relay never sees it.
        /// Appended last so the bincode layout only grows at the tail.
        expires_at: Option<u64>,
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
    /// File/image/voice offer (docs/plans/files-images.md §3). The whole
    /// serialized variant must fit the 8192 B plaintext bucket — senders
    /// trim `thumb` when the budget would blow (DEVIATION: the spec's
    /// 16 KiB thumb cap cannot coexist with the pad bucket, so the
    /// effective thumb ceiling is the serialized-offer budget).
    FileOffer {
        file_id: [u8; 32],
        fetch_token: [u8; 32],
        name: String,
        mime: String,
        size: u64,
        sha256: [u8; 32],
        chunk_size: u32,
        chunks_total: u32,
        kind: FileKind,
        thumb: Option<Vec<u8>>,
        audio: Option<AudioMeta>,
        caption: Option<String>,
        /// Anchor chat message id (`msg:`) the transfer binds to.
        reply_msg: String,
    },
    /// One chunk of file data — self-indexed, dup/reorder-tolerant.
    FileChunk {
        file_id: [u8; 32],
        index: u32,
        data: Vec<u8>,
    },
    /// Receiver progress: coalesced inclusive ranges of verified chunks.
    FileHave {
        file_id: [u8; 32],
        ranges: Vec<(u32, u32)>,
    },
    /// Acceptance + resume ranges in one frame.
    FileAccept {
        file_id: [u8; 32],
        ranges: Vec<(u32, u32)>,
    },
    /// Receiver→sender: whole-file verify passed.
    FileDone {
        file_id: [u8; 32],
        sha256: [u8; 32],
    },
    FileCancel {
        file_id: [u8; 32],
        reason: FileCancelReason,
    },
    FileDecline {
        file_id: [u8; 32],
    },
    // ----- Phase A voice calls (docs/plans/voice-video-calls.md §3) -----
    // All five ride the same Noise session → the relay stays blind; peers
    // on pre-call builds drop the unknown variant index on decode.
    /// Ring request (§3). The call pins to the session generation that
    /// carried it — the demux path already rejects stale/foreign sids
    /// before this variant can be reached.
    CallRing {
        call_id: [u8; 16],
        /// Media kind; `call::MEDIA_AUDIO` is the only Phase A kind.
        media: u8,
        /// Capability bitmask (§3): bit0 audio, bit1 video, bit2
        /// udp-capable (reserved for Phase B fallback), bits 3-7 = 0.
        cap: u8,
        /// Sender-side unix-ms ring deadline; the callee enforces its own
        /// 60 s cap (§3: `expires_unix` is informational).
        expires_unix: u64,
    },
    /// Delivery proof from the callee (§3): auto-emitted on a valid ring.
    CallRinging {
        call_id: [u8; 16],
    },
    /// Callee accepted; `cap` carries the negotiated intersection.
    CallAccept {
        call_id: [u8; 16],
        cap: u8,
    },
    /// Terminating signal for every terminal path (§4 table). Also the
    /// busy answer to an inbound ring under `CALL_MAX_ACTIVE`.
    CallEnd {
        call_id: [u8; 16],
        reason: u8,
    },
    /// One encrypted Opus frame (48 kHz mono 20 ms). `seq` wraps per
    /// direction (modular compare in `call::JitterBuf`); `ts_ms` is
    /// sender call-relative and informational.
    CallFrame {
        call_id: [u8; 16],
        seq: u16,
        ts_ms: u32,
        payload: Vec<u8>,
    },
}

/// Wire shape before `expires_at` was appended to `Text`: decode-only, kept
/// so frames from pre-TTL builds still land (`None` deadline). Variants must
/// stay index-aligned with `AppMsg` — bincode encodes the variant index.
#[derive(Debug, Deserialize)]
pub(crate) enum AppMsgV1 {
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
    Grant {
        mailbox_id: [u8; 32],
        deposit_token: [u8; 32],
    },
    Ack {
        msg_ids: Vec<String>,
    },
}

impl From<AppMsgV1> for AppMsg {
    fn from(v: AppMsgV1) -> Self {
        match v {
            AppMsgV1::Text { msg_id, body } => AppMsg::Text {
                msg_id,
                body,
                expires_at: None,
            },
            AppMsgV1::Edit { msg_id, body } => AppMsg::Edit { msg_id, body },
            AppMsgV1::Delete { msg_id } => AppMsg::Delete { msg_id },
            AppMsgV1::React { msg_id, emoji } => AppMsg::React { msg_id, emoji },
            AppMsgV1::Grant {
                mailbox_id,
                deposit_token,
            } => AppMsg::Grant {
                mailbox_id,
                deposit_token,
            },
            AppMsgV1::Ack { msg_ids } => AppMsg::Ack { msg_ids },
        }
    }
}

/// Decode an app payload: current shape first, then the pre-TTL `AppMsgV1`
/// layout so peers on older builds stay readable. One direction only — old
/// builds reject our `Text` frames (bincode grew the variant), which is why
/// the field rides strictly inside the Noise channel.
pub(crate) fn decode_app(pt: &[u8]) -> Option<AppMsg> {
    wire_deserialize::<AppMsg>(pt)
        .ok()
        .or_else(|| wire_deserialize::<AppMsgV1>(pt).map(AppMsg::from).ok())
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
}

/// Per-transfer runtime state (the durable half lives in `files` rows).
/// Keyed by the `file:` string id.
struct Xfer {
    peer: PeerId,
    /// Inbound: verified chunks. Outbound: peer-acked chunks. The persisted
    /// `have` bitmap is a monotone floor merged in on hydration.
    have: Vec<u8>,
    /// Outbound resend suppression: chunks transmitted since the last
    /// FileAccept (`sent &= have` on every accept re-queues the unacked).
    sent: Vec<u8>,
    /// Some while the outbound pump task is live.
    cancel: Option<Arc<Notify>>,
    /// Wakes a stalled pump (FileHave, presence, resume retry).
    nudge: Arc<Notify>,
    /// Geometry violations; the second strike cancels with `Policy`.
    violations: u8,
    /// Verified-but-unflushed chunk count → FileHave cadence.
    since_have: u32,
    /// Pump parked on MailboxFull.
    mbox_full: bool,
    /// file_state emit throttle: last state + stamp.
    last_state: Option<FileState>,
    last_emit: Option<Instant>,
}

impl Xfer {
    fn new(peer: PeerId, chunks_total: u32) -> Self {
        Self {
            peer,
            have: xfer::bitmap_new(chunks_total),
            sent: xfer::bitmap_new(chunks_total),
            cancel: None,
            nudge: Arc::new(Notify::new()),
            violations: 0,
            since_have: 0,
            mbox_full: false,
            last_state: None,
            last_emit: None,
        }
    }
}

/// Outcome of `deliver_xfer` — the pump's per-chunk verdict.
enum XferSend {
    Sent,
    /// Receiver mailbox is full: park the pump, retry on nudge/timeout.
    MailboxFull,
    /// No session/link: handshake kicked, pump waits.
    NoPath,
}

enum DepositResult {
    Ok,
    MailboxFull,
    Failed,
}

/// `deliver_now` verdict. `Deposited` is deliberately distinct from
/// `Sent`: a mailbox acceptance proves nothing about delivery (the sealed
/// generation may be dead by drain time), so queued copies are retained
/// and only a live-wire send or the peer's Ack advances the message.
enum Delivered {
    /// On the wire over a live session.
    Sent,
    /// Accepted into the peer's mailbox — unproven; retained for re-drive.
    Deposited,
    /// No usable path (no session, encrypt failure, closed channel).
    Failed,
}

enum SealOutcome {
    Sealed {
        ps: Arc<PeerSession>,
        ct: Vec<u8>,
        ctrl: Option<Vec<u8>>,
        deposit: bool,
    },
    NoSession,
    Failed,
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
    /// Live transfer runtime state, keyed by `file:` id.
    xfers: HashMap<String, Xfer>,
    /// Call rows by raw `call_id` — includes recently-ended rows so
    /// `call.info`/terminal dedup stay consistent (bounded: one live call,
    /// ended rows trimmed past CALL_ENDED_KEEP).
    calls: HashMap<[u8; 16], Call>,
    /// Last inbound ring per peer (CALL_RING_MIN_GAP throttle). Kept
    /// separate from the outbound stamp: glare (both sides ring at once)
    /// must never let our own ring suppress the peer's.
    call_last_ring_in: HashMap<PeerId, Instant>,
    /// Last outbound ring attempt per peer (CALL_RING_MIN_GAP throttle).
    call_last_ring_out: HashMap<PeerId, Instant>,
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
    /// Data dir root — the `xfer/` staging tree hangs off it.
    data_dir: PathBuf,
    /// Transfer caps; test-injectable via `test_caps_mut`.
    caps: Mutex<XferCaps>,
    /// Call caps; test-injectable via `test_call_caps_mut`.
    call_caps: Mutex<CallCaps>,
    /// Audio device factory for the media pumps — cpal under the `calls`
    /// feature, NullMedia otherwise; tests inject Loopback/Null.
    media_factory: Mutex<MediaFactory>,
}

/// One live (or recently-ended) Phase A call. Cloning shares the `CallRt`
/// arc so counters stay consistent after the row leaves the map.
#[derive(Clone)]
struct Call {
    id: [u8; 16],
    peer: PeerId,
    dir: CallDir,
    state: CallState,
    /// Local session id the call is pinned to — mismatch = dead call (§4).
    local_sid: SessionId,
    cap: u8,
    since_ms: i64,
    /// 60 s ring TTL (caller & callee both enforce).
    ring_deadline: Instant,
    /// Caller-side: no `CallRinging` by this point → `ended(timeout)`.
    deliver_deadline: Option<Instant>,
    /// CALL_MAX_DURATION hard cap.
    max_deadline: Instant,
    rt: Option<Arc<CallRt>>,
}

type MediaFactory = Arc<dyn Fn() -> call::Devices + Send + Sync>;

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
                xfers: HashMap::new(),
                calls: HashMap::new(),
                call_last_ring_in: HashMap::new(),
                call_last_ring_out: HashMap::new(),
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
            data_dir: data_dir.to_path_buf(),
            caps: Mutex::new(XferCaps::default()),
            call_caps: Mutex::new(CallCaps::default()),
            media_factory: Mutex::new(std::sync::Arc::new(call::default_devices) as MediaFactory),
        });

        // Restart recovery (§11): rows still `queued` when the last process
        // died re-enter pending_out — the first link-up sees the backlog,
        // kicks a handshake per peer, and flush_pending re-drives them.
        // File anchors are covered by resume_xfers' offer re-emit instead.
        let queued = {
            let s = core.store.clone();
            tokio::task::spawn_blocking(move || s.queued_out_texts()).await??
        };
        {
            let mut sh = core.sh();
            for (chat_id, msg_id, body, expires_at) in queued {
                let Ok(peer) = peer_of_chat(&chat_id) else {
                    continue;
                };
                sh.pending_out
                    .entry(peer)
                    .or_default()
                    .push_back(PendingMsg {
                        app: AppMsg::Text {
                            msg_id: msg_id.clone(),
                            body,
                            expires_at: expires_at.map(|e| e.max(0) as u64),
                        },
                        msg_id: Some(msg_id),
                    });
            }
        }

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
        core.tasks.lock().unwrap_or_else(|e| e.into_inner()).push({
            let c = core.clone();
            tokio::spawn(async move { c.expiry_loop().await })
        });
        core.tasks.lock().unwrap_or_else(|e| e.into_inner()).push({
            let c = core.clone();
            tokio::spawn(async move { c.file_reap_loop().await })
        });
        core.tasks.lock().unwrap_or_else(|e| e.into_inner()).push({
            let c = core.clone();
            tokio::spawn(async move { c.call_janitor_loop().await })
        });
        Ok(core)
    }

    fn arc(&self) -> Arc<Core> {
        self.me.upgrade().expect("core arc dead")
    }

    /// Own `peer:` id string (the session.hello `account.peer_id`).
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

    // -- test-only seams: dead in the bin's own test target, called by the
    // #[path]-include integration tests -----

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_substate(&self, peer: &PeerId) -> Option<String> {
        self.sh().meta.get(peer).and_then(|m| m.substate.clone())
    }

    #[cfg(test)]
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn test_dropped_inbound(&self) -> u64 {
        self.sh().dropped_inbound
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_presence(&self, peer: &PeerId) -> Option<bool> {
        self.sh().presence.get(peer).copied()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_has_grant(&self, peer: &PeerId) -> bool {
        self.sh()
            .meta
            .get(peer)
            .and_then(|m| m.grant.as_ref())
            .is_some()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_pending_len(&self, peer: &PeerId) -> usize {
        self.sh()
            .pending_out
            .get(peer)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn test_handle_app(self: &Arc<Self>, peer: PeerId, app: AppMsg) {
        self.handle_app(peer, app, None).await;
    }

    /// Test seam: mutate transfer caps (pace, staging cap, TTL, intervals).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_caps_mut(&self) -> std::sync::MutexGuard<'_, XferCaps> {
        self.caps.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Test seam: mutate call caps (timeouts, queue caps, jitter, pace).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_call_caps_mut(&self) -> std::sync::MutexGuard<'_, CallCaps> {
        self.call_caps.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Test seam: override the media device factory (Loopback/Null).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_media_factory(&self, f: MediaFactory) {
        *self.media_factory.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// Test seam: `{state, dir, since_ts}` for a call_id, or None.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_call(&self, call_id: &str) -> Option<(String, String)> {
        let id = Self::call_id_raw(call_id)?;
        self.sh()
            .calls
            .get(&id)
            .map(|c| (c.state.as_str().to_string(), c.dir.as_str().to_string()))
    }

    /// Test seam: rt counters for a call_id — `(tx, rx, plc, drops_out, drops_late)`.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_call_stats(&self, call_id: &str) -> Option<(u64, u64, u64, u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let id = Self::call_id_raw(call_id)?;
        let rt = self.sh().calls.get(&id)?.rt.clone()?;
        rt.sync_drops();
        Some((
            rt.tx_frames.load(Relaxed),
            rt.rx_frames.load(Relaxed),
            rt.plc.load(Relaxed),
            rt.drops_out.load(Relaxed),
            rt.drops_late.load(Relaxed),
        ))
    }

    /// Test seam: is there a live (non-ended) call at all?
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_live_calls(&self) -> usize {
        self.live_calls().len()
    }

    /// Test seam: does a live session exist for peer (used by CA9's
    /// no-decrypt-fatal assertion on stale-sid frames).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_has_session(&self, peer: &PeerId) -> bool {
        self.session(peer).is_some()
    }

    /// Test seam: last known presence for `peer` (CA18 waits for the
    /// offline flip before ringing).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_peer_online(&self, peer: &PeerId) -> Option<bool> {
        self.sh().presence.get(peer).copied()
    }

    /// Test seam: inject a presence value for `peer` (CA18 exercises the
    /// deposit-vs-live guard without a relay round-trip).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_set_presence(&self, peer: &PeerId, online: bool) {
        self.sh().presence.insert(*peer, online);
    }

    /// Test seam: stamp the per-peer inbound re-ring throttle so the next
    /// CallRing from `peer` is dropped silently (CA3 deliver-timeout).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_touch_ring(&self, peer: &PeerId) {
        self.sh().call_last_ring_in.insert(*peer, Instant::now());
    }

    /// Test seam: drop the live session for `peer` so the next send
    /// re-handshakes into a fresh generation (CA9/CA13 pin checks).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_drop_session(&self, peer: &PeerId) {
        self.drop_session(peer);
    }

    /// Test seam: the sid `peer` stamps on its inbound frames to us (the
    /// sid_index binding for the live session).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_remote_sid(&self, peer: &PeerId) -> Option<SessionId> {
        self.sh()
            .sessions
            .get(peer)
            .and_then(|ps| *ps.remote_sid.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Test seam: inject an inbound transport frame (the HIGH-1 foreign-
    /// sender regression drives a frame stamped with another peer's sid).
    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn test_handle_transport(
        self: &Arc<Self>,
        from: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) {
        self.handle_transport(from, session_id, body).await;
    }

    /// Test seam: run a real (in-process handshaken) Session through
    /// install_session's guard path — the HIGH-2 regression needs a peer
    /// claiming another live peer's stream id.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_install_session(&self, peer: PeerId, session: Session) -> bool {
        self.install_session(peer, session).is_some()
    }

    /// Test seam: shrink a live session's rekey policy so the media-lane
    /// rekey boundary is reachable inside a test window (HIGH-3).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_set_rekey_policy(&self, peer: &PeerId, msgs: u64) {
        let Some(ps) = self.sh().sessions.get(peer).cloned() else {
            return;
        };
        ps.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_rekey_policy(cinder_core::session::RekeyPolicy { msgs, secs: 600 });
    }

    /// Test seam: drive the real key-change consequence path as if a
    /// re-handshake presented `new_static` under `peer` (CA13).
    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn test_key_change(&self, peer: &PeerId, new_static: [u8; 32]) {
        let p = *peer;
        let old = self
            .cblk(move |c| {
                let old = c.get(&p).map(|ct| ct.static_pubkey);
                let _ = c.record_key_change(&p, new_static);
                old
            })
            .await
            .flatten();
        self.key_changed_path(*peer, old, Some(new_static)).await;
    }

    /// Test seam: mark a call's inbound-clock back so media_dead sweeps fire.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_age_call_rx(&self, call_id: &str, secs: u64) {
        let Some(id) = Self::call_id_raw(call_id) else {
            return;
        };
        let Some(rt) = self.sh().calls.get(&id).and_then(|c| c.rt.clone()) else {
            return;
        };
        *rt.last_rx.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() - Duration::from_secs(secs));
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_xfer_progress(&self, file_id: &str) -> Option<(u32, bool)> {
        self.sh()
            .xfers
            .get(file_id)
            .map(|x| (xfer::popcount(&x.have), x.cancel.is_some()))
    }

    fn caps(&self) -> XferCaps {
        self.caps.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
        // Pinned outside the loop so the waiter survives across iterations —
        // a notify_waiters landing mid-connect-attempt must not be lost.
        let notified = self.shutdown.notified();
        tokio::pin!(notified);
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
                _ = &mut notified => break,
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
        // One pinned Notified keeps the waiter registered across select
        // iterations: a notify_waiters() landing while another arm is being
        // serviced would otherwise fall between two dropped-and-recreated
        // futures and be lost — leaving the link up past a 60s ping tick,
        // or forever on an idle connection. The flag check covers a
        // shutdown that fired before this task ever registered.
        let notified = self.shutdown.notified();
        tokio::pin!(notified);
        loop {
            if self.shutdown_requested() {
                break;
            }
            tokio::select! {
                _ = &mut notified => break,
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
                let (kick, nudges) = {
                    let mut sh = self.sh();
                    sh.presence.insert(peer, online);
                    let kick = online
                        && sh
                            .pending_out
                            .get(&peer)
                            .map(|q| !q.is_empty())
                            .unwrap_or(false);
                    // A peer (re)appearing un-parks every pump stalled on
                    // MailboxFull/NoPath for that peer.
                    let nudges: Vec<Arc<Notify>> = if online {
                        sh.xfers
                            .values()
                            .filter(|x| x.peer == peer)
                            .map(|x| x.nudge.clone())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    (kick, nudges)
                };
                if kick {
                    self.ensure_handshake(peer);
                }
                for n in nudges {
                    n.notify_one();
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

        // File-transfer resume (§4.2): on every link-up, inbound rows
        // re-emit FileAccept{ranges} (peer re-sends only the holes) and
        // outbound rows re-emit their identical FileOffer. Must run even
        // when `kicks` is empty — a clean restart has no pending queue yet
        // still owes the peer a resume frame.
        self.resume_xfers().await;

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
        // §4: link death is session death — every pinned call ends failed.
        {
            let c = self.arc();
            tokio::spawn(async move {
                c.sweep_dead_session_calls().await;
            });
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
        let remote = session.remote_session_id();
        let mut sh = self.sh();
        // A claimed remote sid already bound to a DIFFERENT live session is
        // a sid the peer cannot own — honest peers draw theirs at random.
        // Indexing it would hijack the victim's inbound routing (and the
        // first decrypt-fail would tombstone the victim's live stream), so
        // refuse the install outright. Never tombstone `rs` here either:
        // it still belongs to its live owner.
        if let Some(rs) = remote {
            let conflict = match sh.sid_index.get(&rs) {
                Some(owner) if *owner != peer => sh
                    .sessions
                    .get(owner)
                    .map(|ps| {
                        !ps.session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_closed()
                    })
                    .unwrap_or(false),
                _ => false,
            };
            if conflict {
                tracing::debug!(%peer, ?rs, "claimed remote sid bound to a live peer; refusing install");
                return None;
            }
        }
        let ps = Arc::new(PeerSession {
            session: Mutex::new(session),
            session_id: sid,
            remote_sid: Mutex::new(remote),
        });
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
        // The peer's stream id was learned inside the handshake: pre-bind it
        // so only this generation's frames decrypt. Any older sid (dead-
        // generation leftovers) falls through to the foreign-sid drop, never
        // the fatal lazy-bind path. A sid already tombstoned (just killed
        // above, or earlier) must stay dead — never resurrect it.
        if let Some(rs) = remote {
            if !sh.dead_sids.contains_key(&rs) {
                sh.sid_index.insert(rs, peer);
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

    /// §4 key-change consequence path: emit `security.identity_changed`,
    /// then end every live call with that peer (`KeyChanged`, no wire
    /// notice — sends are blocked). Shared by `on_session_ready` and the
    /// test seam so the emit→end ordering is exercised, not duplicated.
    async fn key_changed_path(
        &self,
        peer: PeerId,
        old_fp: Option<[u8; 32]>,
        remote_static: Option<[u8; 32]>,
    ) {
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
        self.end_calls_peer(&peer, EndReason::KeyChanged, false)
            .await;
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
            // §4: a key change mid-call aborts like sends are blocked —
            // ordering keeps identity_changed ahead of the terminal event.
            self.key_changed_path(peer, old_fp, remote_static).await;
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
                },
            )
            .await;
            self.set_meta(&peer, |m| m.grant_sent = true);
        }

        self.flush_pending(peer).await;
        // Every fresh session owes the peer resume frames for this peer's
        // resumable rows — a link-up FileAccept can land in a dead session
        // generation (foreign-sid drop), so re-emit on install too.
        self.resume_xfers().await;

        // Presence refresh: the link-up subscribe only covered contacts
        // active at that moment — a peer that came online after must be
        // (re)subscribed so offline flips (mailbox-vs-live routing, call
        // availability) are observed.
        let _ = self.presence_subscribe().await;

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

    /// Serialize + encrypt `app` on the live session. Shared by the chat
    /// lane (`deliver_now`) and the chunk lane (`deliver_xfer`), which needs
    /// the MailboxFull verdict the bool API hides.
    fn seal_app(&self, peer: &PeerId, app: &AppMsg) -> SealOutcome {
        let Some(ps) = self.session(peer) else {
            return SealOutcome::NoSession;
        };
        self.seal_with(peer, ps, app)
    }

    /// `seal_app` on a caller-resolved session. Call-media callers pin to a
    /// generation BEFORE sealing: encrypting under a session that is about
    /// to be rejected burns a nonce and desyncs the stream for its real
    /// owner.
    fn seal_with(&self, peer: &PeerId, ps: Arc<PeerSession>, app: &AppMsg) -> SealOutcome {
        let payload = match wire_serialize(app) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(%e, "seal_app: AppMsg encode failed");
                return SealOutcome::Failed;
            }
        };
        let deposit = {
            let sh = self.sh();
            sh.presence.get(peer).map(|o| !*o).unwrap_or(false)
                && sh.meta.get(peer).and_then(|m| m.grant.as_ref()).is_some()
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
                    if deposit {
                        None
                    } else {
                        s.maybe_rekey().ok().flatten()
                    },
                ),
                Err(e) => {
                    tracing::debug!(%e, "seal_app: encrypt failed; dropping session");
                    drop(s);
                    drop(ps);
                    self.drop_session(peer);
                    return SealOutcome::Failed;
                }
            }
        };
        SealOutcome::Sealed {
            ps,
            ct,
            ctrl,
            deposit,
        }
    }

    /// Encrypt + send on a live session. For a known-offline peer with a
    /// mailbox grant, deposits the frame into their mailbox instead — a
    /// `Deposited` verdict, never `Sent` (see `Delivered`). `Failed` when
    /// no usable path exists (caller queues/retries).
    async fn deliver_now(&self, peer: PeerId, item: &PendingMsg) -> Delivered {
        let SealOutcome::Sealed {
            ps,
            ct,
            ctrl,
            deposit,
        } = self.seal_app(&peer, &item.app)
        else {
            return Delivered::Failed;
        };
        if deposit {
            // A mailbox deposit is NOT a delivery: the frame was sealed
            // under this session generation, and if the peer's session
            // moves on before it drains, the deposit is dead-on-arrival.
            // Never mark `sent` — the caller keeps the item queued so the
            // next live path re-drives it. The peer's dedupe makes a later
            // live copy harmless; only its Ack may advance the row.
            // MailboxFull is self-limiting.
            return match self.deposit_env(peer, ps.session_id, ct).await {
                DepositResult::Ok => Delivered::Deposited,
                // Full/failed deposits are Failed — flush_pending re-queues
                // at the head and stops hammering the mailbox.
                DepositResult::MailboxFull | DepositResult::Failed => Delivered::Failed,
            };
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
            return Delivered::Failed;
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
        Delivered::Sent
    }

    /// Chunk-lane delivery: same seal/send as `deliver_now` but reports
    /// MailboxFull so the pump parks instead of failing the transfer.
    async fn deliver_xfer(&self, peer: PeerId, app: &AppMsg) -> XferSend {
        let SealOutcome::Sealed {
            ps,
            ct,
            ctrl,
            deposit,
        } = self.seal_app(&peer, app)
        else {
            return XferSend::NoPath;
        };
        if deposit {
            return match self.deposit_env(peer, ps.session_id, ct).await {
                DepositResult::Ok => XferSend::Sent,
                DepositResult::MailboxFull => XferSend::MailboxFull,
                DepositResult::Failed => XferSend::NoPath,
            };
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
            return XferSend::NoPath;
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
        XferSend::Sent
    }

    /// Mailbox deposit primitive; the grant lookup + op + reply-matching
    /// that `deposit()`/`deliver_xfer()` share.
    async fn deposit_env(&self, peer: PeerId, session_id: SessionId, ct: Vec<u8>) -> DepositResult {
        let grant = {
            self.shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .meta
                .get(&peer)
                .and_then(|m| m.grant.clone())
        };
        let Some(g) = grant else {
            return DepositResult::Failed;
        };
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
            Ok(ClientEvent::MailboxDeposited { .. }) => DepositResult::Ok,
            Err(e)
                if e.downcast_ref::<RelayError>()
                    .map(|r| r.0 == ErrorCode::MailboxFull)
                    .unwrap_or(false) =>
            {
                DepositResult::MailboxFull
            }
            _ => DepositResult::Failed,
        }
    }

    /// Queue-or-send: on a live-wire send the item is done; a deposit or a
    /// failure keeps it in pending_out (a deposit proves nothing — it must
    /// be re-driven until a live path or the peer's Ack lands it) and a
    /// handshake attempt is kicked off.
    async fn deliver_or_queue(self: &Arc<Self>, peer: PeerId, item: PendingMsg) {
        let send = self.send_lock(&peer);
        let _send = send.lock().await;
        if matches!(self.deliver_now(peer, &item).await, Delivered::Sent) {
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
        // Items the mailbox just accepted are re-queued (once each) after
        // the pass — a deposit is unproven delivery, never `Sent`.
        let mut deposited: Vec<PendingMsg> = Vec::new();
        loop {
            let item = {
                let mut sh = self.sh();
                sh.pending_out.get_mut(&peer).and_then(|q| q.pop_front())
            };
            let Some(item) = item else { break };
            match self.deliver_now(peer, &item).await {
                Delivered::Sent => {}
                Delivered::Deposited => deposited.push(item),
                Delivered::Failed => {
                    let mut sh = self.sh();
                    sh.pending_out.entry(peer).or_default().push_front(item);
                    break;
                }
            }
        }
        if !deposited.is_empty() {
            let mut sh = self.sh();
            let q = sh.pending_out.entry(peer).or_default();
            for item in deposited {
                if q.len() < PENDING_OUT_CAP {
                    q.push_back(item);
                }
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

    // ----- Phase A call engine (docs/plans/voice-video-calls.md) -------------

    /// Trimmed ended rows keep `call.info`/dedup consistent without growing
    /// `Shared.calls` unboundedly.
    fn call_id_str(id: &[u8; 16]) -> String {
        format!("call:{}", b32(id))
    }

    fn call_id_raw(s: &str) -> Option<[u8; 16]> {
        data_encoding::BASE32_NOPAD
            .decode(s.strip_prefix("call:")?.to_uppercase().as_bytes())
            .ok()?
            .try_into()
            .ok()
    }

    fn call_caps(&self) -> CallCaps {
        self.call_caps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Non-terminal calls (Outgoing/Ringing/Active) — `CALL_MAX_ACTIVE` math.
    fn live_calls(&self) -> Vec<Call> {
        self.sh()
            .calls
            .values()
            .filter(|c| c.state != CallState::Ended)
            .cloned()
            .collect()
    }

    /// Live session pinned check: the session must exist AND be the exact
    /// generation the call was created on (a re-handshake = new call).
    fn call_session_live(&self, c: &Call) -> Option<Arc<PeerSession>> {
        match self.sh().sessions.get(&c.peer) {
            Some(ps) if ps.session_id == c.local_sid => Some(ps.clone()),
            _ => None,
        }
    }

    /// Ring-path liveness: session alive AND not a "known-offline + grant"
    /// deposit route — calls never ride mailboxes (§8, CA18).
    fn call_live_path(&self, peer: &PeerId) -> Option<Arc<PeerSession>> {
        let ps = self.session(peer)?;
        let deposit_would = {
            let sh = self.sh();
            sh.presence.get(peer).map(|o| !*o).unwrap_or(false)
                && sh.meta.get(peer).and_then(|m| m.grant.as_ref()).is_some()
        };
        if deposit_would {
            return None;
        }
        Some(ps)
    }

    /// Call signalling: seal on the pinned session, NEVER deposit, send
    /// reliably on cmd_tx (control messages are low-rate). The pin is
    /// checked BEFORE sealing — encrypting under a generation that is
    /// about to be rejected burns a nonce and desyncs its real owner.
    async fn send_call_ctrl(&self, peer: PeerId, app: &AppMsg, pinned_sid: SessionId) -> bool {
        let Some(ps) = self.session(&peer) else {
            return false;
        };
        if ps.session_id != pinned_sid {
            return false;
        }
        let SealOutcome::Sealed {
            ps,
            ct,
            ctrl,
            deposit,
        } = self.seal_with(&peer, ps, app)
        else {
            return false;
        };
        if deposit {
            return false;
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
        true
    }

    /// Media lane: seal on the pinned session, send via try_send so a
    /// saturated channel drops the frame (isochronous) instead of
    /// queueing behind chat. The pin is checked BEFORE sealing (a stale
    /// generation must not consume a nonce), and a rekey control unit —
    /// the last frame under the old egress key — goes on the wire AFTER
    /// the data frame it follows and via a blocking send so the rotation
    /// boundary can never be reordered or dropped mid-stream.
    async fn send_call_frame(&self, peer: PeerId, app: &AppMsg, pinned_sid: SessionId) -> bool {
        let Some(ps) = self.session(&peer) else {
            return false;
        };
        if ps.session_id != pinned_sid {
            return false;
        }
        let SealOutcome::Sealed {
            ps,
            ct,
            ctrl,
            deposit,
        } = self.seal_with(&peer, ps, app)
        else {
            return false;
        };
        if deposit {
            return false;
        }
        // Media backpressure must resolve at out_q (pre-seal drop-oldest),
        // never post-seal: a dropped sealed frame desyncs the receiver's
        // sequential nonces and DecryptFatal-destroys the session. Block on
        // cmd_tx like every other lane — a full channel stalls only this
        // drain task while out_q sheds the excess upstream of sealing.
        let sent = self
            .link
            .cmd_tx
            .send(ClientMsg::Transport {
                to: peer,
                session_id: ps.session_id,
                body: ct,
            })
            .await
            .is_ok();
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
        sent
    }

    fn emit_call_state(&self, c: &Call, reason: Option<EndReason>) {
        let rt = c.rt.clone();
        if let Some(r) = &rt {
            r.sync_drops();
        }
        let mut p = json!({
            "call_id": Self::call_id_str(&c.id),
            "state": c.state.as_str(),
            "dir": c.dir.as_str(),
            "peer_id": peer_str(&c.peer),
            "contact_id": contact_str(&c.peer),
            "chat_id": chat_str(&c.peer),
            "media": "audio",
            "since_ts": rfc3339(c.since_ms),
        });
        if let Some(rt) = &rt {
            use std::sync::atomic::Ordering::Relaxed;
            p["stats"] = json!({
                "tx_frames": rt.tx_frames.load(Relaxed),
                "rx_frames": rt.rx_frames.load(Relaxed),
                "plc": rt.plc.load(Relaxed),
                "drops_out": rt.drops_out.load(Relaxed),
                "drops_late": rt.drops_late.load(Relaxed),
            });
            p["muted"] = Value::Bool(rt.muted.load(Relaxed));
            p["quality"] = Value::String(
                rt.quality
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_str()
                    .into(),
            );
        }
        if let Some(r) = reason {
            p["reason"] = Value::String(r.as_str().into());
        }
        self.emit("call.state", Some(&chat_str(&c.peer)), p);
    }

    fn emit_call_incoming(&self, c: &Call) {
        self.emit(
            "call.incoming",
            Some(&chat_str(&c.peer)),
            json!({
                "call_id": Self::call_id_str(&c.id),
                "contact_id": contact_str(&c.peer),
                "peer_id": peer_str(&c.peer),
                "chat_id": chat_str(&c.peer),
                "media": "audio",
                "cap": c.cap,
            }),
        );
    }

    /// Terminal transition: idempotent (state CAS), stops pumps, optionally
    /// emits the wire `CallEnd`, and emits exactly one `call.state:ended`.
    async fn end_call(&self, call_id: &[u8; 16], reason: EndReason, notify_peer: bool) {
        let call = {
            let mut sh = self.sh();
            match sh.calls.get_mut(call_id) {
                Some(c) if c.state != CallState::Ended => {
                    c.state = CallState::Ended;
                    Some(c.clone())
                }
                _ => None,
            }
        };
        let Some(call) = call else { return };
        if let Some(rt) = &call.rt {
            rt.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if notify_peer {
            let _ = self
                .send_call_ctrl(
                    call.peer,
                    &AppMsg::CallEnd {
                        call_id: *call_id,
                        reason: reason.to_u8(),
                    },
                    call.local_sid,
                )
                .await;
        }
        self.emit_call_state(&call, Some(reason));
    }

    /// Sweep every live call whose pinned session no longer exists
    /// (link down, session drop, re-handshake) — `ended(failed)`.
    async fn sweep_dead_session_calls(&self) {
        let ids: Vec<[u8; 16]> = self
            .live_calls()
            .into_iter()
            .filter(|c| self.call_session_live(c).is_none())
            .map(|c| c.id)
            .collect();
        for id in ids {
            self.end_call(&id, EndReason::Failed, true).await;
        }
    }

    /// End every non-terminal call pinned to a peer (key change, link
    /// death, contact loss) — sweeps from `sh()` each call.
    async fn end_calls_peer(&self, peer: &PeerId, reason: EndReason, notify: bool) {
        let ids: Vec<[u8; 16]> = self
            .live_calls()
            .into_iter()
            .filter(|c| c.peer == *peer)
            .map(|c| c.id)
            .collect();
        for id in ids {
            self.end_call(&id, reason, notify).await;
        }
    }

    /// Spawn the three pumps for a call that just went Active: mic→encode→
    /// out_q, out_q→wire (try_send), and jitter→decode→speaker.
    fn start_media(self: &Arc<Self>, call_id: [u8; 16], peer: PeerId, local_sid: SessionId) {
        let caps = self.call_caps();
        let rt = {
            let mut sh = self.sh();
            let Some(c) = sh.calls.get_mut(&call_id) else {
                return;
            };
            let rt = Arc::new(CallRt::new(&caps));
            c.rt = Some(rt.clone());
            rt
        };
        let devices = {
            let f = self
                .media_factory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            f()
        };

        // Mic → encode → out_q. spawn_blocking: device reads can block.
        {
            let rt = rt.clone();
            let mut mic = devices.mic;
            let pace = caps.frame_pace;
            std::thread::spawn(move || {
                let Ok(enc) = call::OpusEnc::new(caps.bitrate) else {
                    tracing::warn!("call mic pump: opus encoder init failed");
                    rt.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                };
                let mut pcm = [0.0f32; call::FRAME_SAMPLES];
                // Absolute-deadline pacing: schedule each frame from the
                // PREVIOUS target so work-time jitter never accumulates
                // into rate drift (a relative sleep slips ~the scheduler
                // overshoot per frame). A stall longer than a few frames
                // resets the anchor instead of bursting catch-up.
                let ka_gap = (caps.media_keepalive / 2).max(pace);
                let mut next = Instant::now();
                let mut next_ka = Instant::now() + ka_gap;
                while !rt.stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                    if rt.stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if mic.read(&mut pcm).is_err() {
                        pcm.fill(0.0);
                    }
                    if rt.muted.load(std::sync::atomic::Ordering::Relaxed) {
                        // §7 mute is local truth: the mic gate closes at the
                        // source — but a TOTAL media gap reads as dead media
                        // to the peer (media_dead ends the call), so sparse
                        // real silence frames ride at keepalive cadence.
                        if Instant::now() >= next_ka {
                            next_ka = Instant::now() + ka_gap;
                            pcm.fill(0.0);
                            if let Ok(pkt) = enc.encode(&pcm) {
                                if pkt.len() <= call::MAX_OPUS_PACKET && rt.out_q.push(pkt) {
                                    rt.drops_out
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                    } else {
                        match enc.encode(&pcm) {
                            Ok(pkt) if pkt.len() <= call::MAX_OPUS_PACKET => {
                                if rt.out_q.push(pkt) {
                                    rt.drops_out
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            _ => {
                                rt.drops_out
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    next += pace;
                    if next <= Instant::now() {
                        next = Instant::now() + pace;
                    }
                }
            });
        }

        // out_q → sealed CallFrame → cmd_tx. Wire backpressure stalls this
        // drain task; drop-oldest loss happens upstream in out_q.push.
        {
            let me = self.arc();
            let rt = rt.clone();
            tokio::spawn(async move {
                loop {
                    if rt.stop.load(std::sync::atomic::Ordering::Relaxed) && rt.out_q.len() == 0 {
                        break;
                    }
                    let Some(pkt) = rt.out_q.pop() else {
                        rt.out_q.wait(Duration::from_millis(200)).await;
                        if rt.stop.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        continue;
                    };
                    // Live-call check: call may have ended while queued.
                    let still_active = {
                        let sh = me.sh();
                        sh.calls
                            .get(&call_id)
                            .map(|c| c.state == CallState::Active)
                            .unwrap_or(false)
                    };
                    if !still_active {
                        continue;
                    }
                    let seq = rt.next_seq();
                    let ts_ms = rt.ts_ms();
                    let sent = me
                        .send_call_frame(
                            peer,
                            &AppMsg::CallFrame {
                                call_id,
                                seq,
                                ts_ms,
                                payload: pkt,
                            },
                            local_sid,
                        )
                        .await;
                    if sent {
                        rt.tx_frames
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        rt.drops_out
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Pinned session gone → the janitor's sweep ends it;
                        // stop pumping instead of burning codec cycles.
                        let pinned_alive = {
                            let sh = me.sh();
                            sh.sessions
                                .get(&peer)
                                .map(|ps| ps.session_id == local_sid)
                                .unwrap_or(false)
                        };
                        if !pinned_alive {
                            rt.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            });
        }

        // Jitter → decode → speaker, 20 ms isochronous playout.
        {
            let rt = rt.clone();
            let mut speaker = devices.speaker;
            tokio::spawn(async move {
                let Ok(mut dec) = call::OpusDec::new() else {
                    tracing::warn!("call play pump: opus decoder init failed");
                    return;
                };
                let mut pcm = [0.0f32; call::FRAME_SAMPLES];
                let mut iv = tokio::time::interval(Duration::from_millis(call::FRAME_MS));
                loop {
                    if rt.stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    iv.tick().await;
                    let play = { rt.jitter.lock().unwrap_or_else(|e| e.into_inner()).tick() };
                    let pkt = match play {
                        call::Playout::Frame(p) => Some(p),
                        call::Playout::Plc => None,
                        call::Playout::Buffering => continue,
                    };
                    if dec.decode(pkt.as_deref(), &mut pcm).is_ok() {
                        let _ = speaker.write(&pcm);
                    }
                }
            });
        }
    }

    /// Deadline/session sweeps: deliver timeout, ring TTL, media dead,
    /// max duration, session-pin death, and quality transitions.
    async fn call_janitor_loop(self: Arc<Self>) {
        loop {
            if self.shutdown_requested() {
                return;
            }
            let tick = self.call_caps().janitor_tick;
            tokio::time::sleep(tick).await;
            let caps = self.call_caps();
            let now = Instant::now();
            let today = now_ms() / 86_400_000;
            // Re-ring throttle cleanup alongside the call sweeps.
            {
                let mut sh = self.sh();
                sh.call_last_ring_in
                    .retain(|_, t| t.elapsed() < caps.ring_min_gap);
                sh.call_last_ring_out
                    .retain(|_, t| t.elapsed() < caps.ring_min_gap);
                let ended: Vec<[u8; 16]> = sh
                    .calls
                    .iter()
                    .filter(|(_, c)| c.state == CallState::Ended)
                    .map(|(k, _)| *k)
                    .collect();
                // Keep at most 128 ended rows for info/dedup.
                if ended.len() > 128 {
                    let mut dated: Vec<([u8; 16], i64)> = ended
                        .into_iter()
                        .map(|id| (id, sh.calls.get(&id).map(|c| c.since_ms).unwrap_or(0)))
                        .collect();
                    dated.sort_by_key(|(_, t)| *t);
                    let excess = dated.len() - 128;
                    for (id, _) in dated.into_iter().take(excess) {
                        sh.calls.remove(&id);
                    }
                }
            }
            let _ = today;
            for c in self.live_calls() {
                // Session-pin: any mismatch with the generation the call was
                // created on = dead call (§4 "a dead session ends the call").
                if self.call_session_live(&c).is_none() {
                    self.end_call(&c.id, EndReason::Failed, true).await;
                    continue;
                }
                match c.state {
                    CallState::Outgoing => {
                        if let Some(dl) = c.deliver_deadline {
                            if now >= dl {
                                self.end_call(&c.id, EndReason::Timeout, true).await;
                                continue;
                            }
                        }
                        if now >= c.ring_deadline {
                            self.end_call(&c.id, EndReason::NoAnswer, true).await;
                        }
                    }
                    CallState::Ringing => {
                        if now >= c.ring_deadline {
                            self.end_call(&c.id, EndReason::NoAnswer, true).await;
                        }
                    }
                    CallState::Active => {
                        if now >= c.max_deadline {
                            self.end_call(&c.id, EndReason::Timeout, true).await;
                            continue;
                        }
                        let Some(rt) = &c.rt else { continue };
                        let last_rx = *rt.last_rx.lock().unwrap_or_else(|e| e.into_inner());
                        let silence = now.saturating_duration_since(last_rx.unwrap_or(rt.started));
                        if silence >= caps.media_dead {
                            self.end_call(&c.id, EndReason::Failed, true).await;
                            continue;
                        }
                        // Quality transitions, throttled to ≤1 emit / 5 s.
                        let want = if silence >= caps.media_keepalive {
                            Quality::Degraded
                        } else {
                            Quality::Good
                        };
                        let (cur, last_emit) = {
                            let q = rt.quality.lock().unwrap_or_else(|e| e.into_inner());
                            let le = rt
                                .last_quality_emit
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            (*q, *le)
                        };
                        if want != cur
                            && now.saturating_duration_since(last_emit) >= Duration::from_secs(5)
                        {
                            *rt.quality.lock().unwrap_or_else(|e| e.into_inner()) = want;
                            *rt.last_quality_emit
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = now;
                            self.emit_call_state(&c, None);
                        }
                    }
                    CallState::Ended => {}
                }
            }
        }
    }

    /// Resolve `call_id` (or the single live call when omitted) under the
    /// shared lock, gated by a predicate — shared by accept/decline/hangup.
    fn pick_call_locked<'a>(
        &self,
        sh: &'a mut Shared,
        call_id: &Option<String>,
        pred: impl Fn(&Call) -> bool,
    ) -> Result<&'a mut Call, RpcError> {
        match call_id {
            Some(s) => {
                let id = Self::call_id_raw(s)
                    .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "malformed call_id"))?;
                let c = sh
                    .calls
                    .get_mut(&id)
                    .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "call_id not found"))?;
                if !pred(c) {
                    return Err(RpcError::new(
                        ErrorKind::Conflict,
                        "call is not in a state that accepts this action",
                    ));
                }
                Ok(c)
            }
            None => sh
                .calls
                .values_mut()
                .find(|c| pred(c))
                .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "no matching call")),
        }
    }

    // ---- inbound call signalling ----

    /// `sid` = the local session id that carried the inbound frame. Every
    /// call row pins to its own generation (`local_sid`), so a frame that
    /// arrived under any other generation is stale and must not mutate the
    /// call (M7). `None` = no transport context (test seam) → unpinned.
    async fn on_call_ring(
        self: &Arc<Self>,
        peer: PeerId,
        call_id: [u8; 16],
        media: u8,
        cap: u8,
        expires_unix: u64,
        sid: Option<SessionId>,
    ) {
        let caps = self.call_caps();
        // The frame already decrypted under a live session for `peer`
        // (stale/foreign sids never reach here); that generation is the pin.
        let Some(ps) = self.session(&peer) else {
            return;
        };
        let pin = sid.unwrap_or(ps.session_id);
        // Re-ring of a call we already know → re-ack, no new state/event.
        let existing = { self.sh().calls.get(&call_id).cloned() };
        if let Some(existing) = existing {
            if existing.state == CallState::Ringing
                && sid.map(|s| s == existing.local_sid).unwrap_or(true)
            {
                let _ = self
                    .send_call_ctrl(peer, &AppMsg::CallRinging { call_id }, existing.local_sid)
                    .await;
            }
            return;
        }
        // Unsupported media → explicit decline path.
        if media & call::CAP_AUDIO_OPUS48K == 0 {
            let _ = self
                .send_call_ctrl(
                    peer,
                    &AppMsg::CallEnd {
                        call_id,
                        reason: EndReason::Failed.to_u8(),
                    },
                    pin,
                )
                .await;
            return;
        }
        // Expired ring → dead on arrival.
        if expires_unix != 0 && expires_unix < now_ms() as u64 {
            return;
        }
        // Re-ring throttle: same peer re-ringing inside CALL_RING_MIN_GAP.
        {
            let mut sh = self.sh();
            if let Some(t) = sh.call_last_ring_in.get(&peer) {
                if t.elapsed() < caps.ring_min_gap {
                    return;
                }
            }
            sh.call_last_ring_in.insert(peer, Instant::now());
        }
        // Busy: one live call — busy answer must not disturb it (CA4).
        // Recount + insert under the SAME lock acquisition: two inbound
        // rings landing together must not both pass the cap (mirrors the
        // outbound M11 fix). The guard must be gone before the busy-answer
        // await, so the decision is computed inside the lock scope.
        let c = {
            let mut sh = self.sh();
            let over_cap = sh
                .calls
                .values()
                .filter(|c| c.state != CallState::Ended)
                .count()
                >= caps.max_active;
            if over_cap {
                None
            } else {
                let c = Call {
                    id: call_id,
                    peer,
                    dir: CallDir::In,
                    state: CallState::Ringing,
                    local_sid: pin,
                    cap,
                    since_ms: now_ms(),
                    ring_deadline: Instant::now() + caps.ring_timeout,
                    deliver_deadline: None,
                    max_deadline: Instant::now() + caps.max_duration,
                    rt: None,
                };
                sh.calls.insert(call_id, c.clone());
                Some(c)
            }
        };
        let Some(c) = c else {
            let _ = self
                .send_call_ctrl(
                    peer,
                    &AppMsg::CallEnd {
                        call_id,
                        reason: EndReason::Busy.to_u8(),
                    },
                    pin,
                )
                .await;
            return;
        };
        self.emit_call_incoming(&c);
        let _ = self
            .send_call_ctrl(peer, &AppMsg::CallRinging { call_id }, pin)
            .await;
        self.emit_call_state(&c, None);
    }

    async fn on_call_ringing(&self, peer: PeerId, call_id: [u8; 16], sid: Option<SessionId>) {
        let mut sh = self.sh();
        let Some(c) = sh.calls.get_mut(&call_id) else {
            return;
        };
        if c.peer != peer
            || c.dir != CallDir::Out
            || !matches!(c.state, CallState::Outgoing)
            || sid.map(|s| s != c.local_sid).unwrap_or(false)
        {
            return;
        }
        c.state = CallState::Ringing;
        c.deliver_deadline = None;
        let c = c.clone();
        drop(sh);
        self.emit_call_state(&c, None);
    }

    async fn on_call_accept(
        self: &Arc<Self>,
        peer: PeerId,
        call_id: [u8; 16],
        cap: u8,
        sid: Option<SessionId>,
    ) {
        let (c, pinned) = {
            let mut sh = self.sh();
            let Some(c) = sh.calls.get_mut(&call_id) else {
                return;
            };
            if c.peer != peer
                || c.dir != CallDir::Out
                || !matches!(c.state, CallState::Outgoing | CallState::Ringing)
                || sid.map(|s| s != c.local_sid).unwrap_or(false)
            {
                return;
            }
            c.state = CallState::Active;
            c.cap &= cap;
            (c.clone(), c.local_sid)
        };
        self.start_media(call_id, peer, pinned);
        self.emit_call_state(&c, None);
    }

    async fn on_call_end(
        &self,
        peer: PeerId,
        call_id: [u8; 16],
        reason: u8,
        sid: Option<SessionId>,
    ) {
        let known = {
            let sh = self.sh();
            sh.calls
                .get(&call_id)
                .map(|c| c.peer == peer && sid.map(|s| s == c.local_sid).unwrap_or(true))
                .unwrap_or(false)
        };
        if !known {
            return;
        }
        self.end_call(
            &call_id,
            EndReason::from_u8(reason).unwrap_or(EndReason::Failed),
            false,
        )
        .await;
    }

    async fn on_call_frame(
        &self,
        peer: PeerId,
        call_id: [u8; 16],
        seq: u16,
        payload: Vec<u8>,
        sid: Option<SessionId>,
    ) {
        if payload.len() > call::MAX_OPUS_PACKET {
            return;
        }
        let rt = {
            let sh = self.sh();
            sh.calls
                .get(&call_id)
                .filter(|c| {
                    c.peer == peer
                        && c.state == CallState::Active
                        && sid.map(|s| s == c.local_sid).unwrap_or(true)
                })
                .and_then(|c| c.rt.clone())
        };
        let Some(rt) = rt else { return };
        rt.rx_frame(seq, payload);
    }

    // ----- file/image transfer engine (docs/plans/files-images.md) ----------

    /// Create/refresh the runtime entry and union the persisted bitmap in
    /// (it is a monotone floor — bits are only ever set, never cleared).
    fn xfer_hydrate(&self, row: &FileRow, peer: PeerId) {
        let mut sh = self.sh();
        let x = sh
            .xfers
            .entry(row.file_id.clone())
            .or_insert_with(|| Xfer::new(peer, row.chunks_total as u32));
        x.peer = peer;
        if x.have.len() < row.have.len() {
            x.have.resize(row.have.len(), 0);
        }
        for (i, b) in row.have.iter().enumerate() {
            x.have[i] |= *b;
        }
        if x.sent.len() != x.have.len() {
            x.sent.resize(x.have.len(), 0);
        }
    }

    /// `file_state` emit: every transition lands immediately; same-state
    /// progress updates are throttled to 1 Hz per transfer (§6).
    pub(crate) fn emit_xfer(&self, row: &FileRow, progress_have: &[u8]) {
        let state = match FileState::parse(&row.state) {
            Some(s) => s,
            None => return,
        };
        let now = Instant::now();
        let bytes_done = {
            let mut sh = self.sh();
            if !sh.xfers.contains_key(&row.file_id) && !state.is_terminal() {
                // Live-state row with no runtime entry (rehydrated test row,
                // post-restart emit before resume_xfers ran): recreate it —
                // NOT a zombie, the terminal transition will drop it again.
                if let Ok(peer) = peer_of_chat(&row.chat_id) {
                    let mut x = Xfer::new(peer, row.chunks_total.max(0) as u32);
                    if !row.have.is_empty() && row.have.len() == x.have.len() {
                        x.have.copy_from_slice(&row.have);
                    }
                    sh.xfers.insert(row.file_id.clone(), x);
                }
            }
            match sh.xfers.get_mut(&row.file_id) {
                Some(x) => {
                    if x.last_state == Some(state)
                        && x.last_emit
                            .map(|t| now.duration_since(t) < Duration::from_secs(1))
                            .unwrap_or(false)
                    {
                        return;
                    }
                    x.last_state = Some(state);
                    x.last_emit = Some(now);
                    if state == FileState::Complete {
                        row.size as u64
                    } else {
                        let have = if progress_have.is_empty() {
                            &x.have
                        } else {
                            progress_have
                        };
                        (xfer::popcount(have) as u64)
                            .saturating_mul(row.chunk_size.max(0) as u64)
                            .min(row.size.max(0) as u64)
                    }
                }
                // No runtime entry (terminal emit after xfer_finish dropped
                // it): still emit from the persisted row — recreating a
                // zombie Xfer here would leak it forever.
                None => {
                    if state == FileState::Complete {
                        row.size as u64
                    } else {
                        let have = if progress_have.is_empty() {
                            row.have.as_slice()
                        } else {
                            progress_have
                        };
                        (xfer::popcount(have) as u64)
                            .saturating_mul(row.chunk_size.max(0) as u64)
                            .min(row.size.max(0) as u64)
                    }
                }
            }
        };
        self.emit(
            "file_state",
            Some(&row.chat_id),
            json!({
                "file_id": row.file_id,
                "msg_id": row.msg_id,
                "chat_id": row.chat_id,
                "dir": dir_str(row.dir),
                "state": state.as_str(),
                "bytes_done": bytes_done,
                "bytes_total": row.size.max(0) as u64,
                "reason": row.reason,
            }),
        );
    }

    /// Terminal bookkeeping for a row: notify + drop the pump + emit. The
    /// `state`/`reason` write itself must already be committed.
    fn xfer_finish(&self, file_id: &str) {
        if let Some(x) = self.sh().xfers.remove(file_id) {
            if let Some(c) = x.cancel {
                c.notify_one();
            }
        }
    }

    /// Persisted state transition + emit + (terminal) pump shutdown.
    async fn xfer_set_state(&self, row: &FileRow, state: FileState, reason: Option<&str>) {
        let fid = row.file_id.clone();
        let st = state.as_str();
        let rs = reason.map(String::from);
        let _ = self
            .sblk(move |s| s.set_file_state(&fid, st, rs.as_deref()))
            .await;
        if state.is_terminal() {
            self.xfer_finish(&row.file_id);
        }
        let mut fresh = row.clone();
        fresh.state = st.to_string();
        fresh.reason = reason.map(String::from);
        self.emit_xfer(&fresh, &[]);
    }

    /// `.part` cleanup on terminal transitions for rows that own a staged
    /// blob (inbound staging + materialized `data` sends — never a source
    /// path, which only lives in `staged_path` as `xfer/…`).
    async fn xfer_unlink(&self, row: &FileRow) {
        let Some(rel) = row.staged_path.clone() else {
            return;
        };
        if !rel.starts_with("xfer/") {
            return;
        }
        let abs = xfer::stage_abs(&self.data_dir, &rel);
        let _ = tokio::task::spawn_blocking(move || xfer::stage_unlink(&abs)).await;
        let fid = row.file_id.clone();
        let _ = self
            .sblk(move |s| {
                s.set_file_staged(&fid, "") // cleared marker keeps the row
            })
            .await;
    }

    /// Receiver→sender: FileAccept carrying the current verified ranges —
    /// the accept and the resume re-accept share this exact frame.
    async fn send_file_accept(self: &Arc<Self>, row: &FileRow) {
        let Some(fid_raw) = file_id_raw(&row.file_id) else {
            return;
        };
        let ranges = {
            let sh = self.sh();
            let have = sh
                .xfers
                .get(&row.file_id)
                .map(|x| x.have.clone())
                .unwrap_or_else(|| row.have.clone());
            xfer::ranges_of(&have, row.chunks_total as u32)
        };
        let Some(peer) = peer_of_chat(&row.chat_id).ok() else {
            return;
        };
        self.deliver_or_queue(
            peer,
            PendingMsg {
                app: AppMsg::FileAccept {
                    file_id: fid_raw,
                    ranges,
                },
                msg_id: None,
            },
        )
        .await;
    }

    /// Accept path shared by the auto-accept policy and `file.accept`:
    /// cap re-check → `.part` create → state accepted → FileAccept{ranges}.
    async fn accept_flow(
        self: &Arc<Self>,
        row: &FileRow,
        peer: PeerId,
    ) -> std::result::Result<(), &'static str> {
        let caps = self.caps();
        let (inf_peer, inf_all, staged_bytes) = {
            let cid = row.chat_id.clone();
            self.sblk(move |s| {
                Ok::<_, StoreError>((
                    s.file_inflight("in", Some(&cid))?,
                    s.file_inflight("in", None)?,
                    s.file_staging_bytes()?,
                ))
            })
            .await
            .map_err(|_| "internal")?
        };
        // `row` counts itself: `>` not `>=` against the post-insert total.
        if inf_peer as usize > caps.max_inflight_peer || inf_all as usize > caps.max_inflight {
            return Err("policy");
        }
        if staged_bytes.saturating_add(row.size.max(0) as u64) > caps.stage_max_bytes {
            return Err("storage_full");
        }
        let (Some(fid_raw), Some(token)) = (
            file_id_raw(&row.file_id),
            row.fetch_token
                .as_deref()
                .and_then(|t| <[u8; 32]>::try_from(t).ok()),
        ) else {
            return Err("internal");
        };
        let rel = xfer::stage_rel(&fid_raw, &token);
        let dir = self.data_dir.clone();
        let rel2 = rel.clone();
        let size = row.size.max(0) as u64;
        tokio::task::spawn_blocking(move || xfer::stage_create(&dir, &rel2, size))
            .await
            .map_err(|_| "internal")?
            .map_err(|_| "internal")?;
        let fid = row.file_id.clone();
        let _ = self.sblk(move |s| s.set_file_staged(&fid, &rel)).await;
        self.xfer_set_state(row, FileState::Accepted, None).await;
        self.xfer_hydrate(row, peer);
        self.send_file_accept(row).await;
        if row.chunks_total == 0 {
            self.finalize_inbound(row, peer).await;
        }
        Ok(())
    }

    /// Inbound completion: whole-file sha256 → FileDone + complete (or
    /// FileCancel{HashMismatch} + failed and the blob dies).
    async fn finalize_inbound(self: &Arc<Self>, row: &FileRow, peer: PeerId) {
        let Some(rel) = row.staged_path.clone() else {
            self.xfer_set_state(row, FileState::Failed, Some("internal"))
                .await;
            return;
        };
        let abs = xfer::stage_abs(&self.data_dir, &rel);
        let digest = tokio::task::spawn_blocking(move || xfer::stage_sha256(&abs))
            .await
            .ok()
            .and_then(|r| r.ok());
        let fid_raw = file_id_raw(&row.file_id);
        match (digest, fid_raw) {
            (Some(d), Some(fid)) if d.as_slice() == row.sha256.as_slice() => {
                // Persist the full bitmap so chunks_done survives reopen.
                let full = vec![0xffu8; (row.chunks_total as usize).div_ceil(8)];
                {
                    let mut sh = self.sh();
                    if let Some(x) = sh.xfers.get_mut(&row.file_id) {
                        x.have = full.clone();
                    }
                }
                let fid2 = row.file_id.clone();
                let _ = self.sblk(move |s| s.set_file_have(&fid2, &full)).await;
                self.xfer_set_state(row, FileState::Complete, None).await;
                let sha: [u8; 32] = d;
                self.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::FileDone {
                            file_id: fid,
                            sha256: sha,
                        },
                        msg_id: None,
                    },
                )
                .await;
                // The anchor message's in-band ack.
                let mid = row.msg_id.clone();
                self.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::Ack { msg_ids: vec![mid] },
                        msg_id: None,
                    },
                )
                .await;
            }
            (Some(_), Some(fid)) => {
                self.xfer_set_state(row, FileState::Failed, Some("hash_mismatch"))
                    .await;
                self.xfer_unlink(row).await;
                self.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::FileCancel {
                            file_id: fid,
                            reason: FileCancelReason::HashMismatch,
                        },
                        msg_id: None,
                    },
                )
                .await;
            }
            _ => {
                self.xfer_set_state(row, FileState::Failed, Some("internal"))
                    .await;
                self.xfer_unlink(row).await;
            }
        }
    }

    // --- inbound wire handlers -------------------------------------------

    #[allow(clippy::too_many_arguments)] // wire fields map 1:1 to FileOffer
    async fn on_file_offer(
        self: &Arc<Self>,
        peer: PeerId,
        file_id: [u8; 32],
        fetch_token: [u8; 32],
        name: String,
        mime: String,
        size: u64,
        sha256: [u8; 32],
        chunk_size: u32,
        chunks_total: u32,
        kind: FileKind,
        thumb: Option<Vec<u8>>,
        audio: Option<AudioMeta>,
        caption: Option<String>,
        reply_msg: String,
    ) {
        let chat_id = chat_str(&peer);
        let caps = self.caps();
        // Hygiene bounds BEFORE any allocation/store write (§5.3).
        let sane = name.len() <= 255
            && mime.len() <= 64
            && caption.as_ref().map(|c| c.len()).unwrap_or(0) <= 1024
            && thumb.as_ref().map(|t| t.len()).unwrap_or(0) <= caps.thumb_max_bytes
            && size <= caps.max_bytes
            && (512..=caps.chunk_bytes).contains(&chunk_size)
            && chunks_total <= caps.max_chunks
            && chunks_total as u64 == size.div_ceil(chunk_size as u64)
            && reply_msg.len() <= 128
            && reply_msg.starts_with("msg:");
        if !sane {
            self.sh().dropped_inbound += 1;
            tracing::debug!(%peer, "malformed FileOffer; dropped");
            return;
        }
        // An Audio offer without metadata degrades to a plain file.
        let kind = if kind == FileKind::Audio && audio.is_none() {
            FileKind::File
        } else {
            kind
        };
        let file_id_s = file_id_str(&file_id);
        // Resume/tombstone check: a known file_id only continues when the
        // presented fetch_token binds to the row (constant-time compare).
        let existing = self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await;
        if let Ok(Some(row)) = existing {
            let tok_ok = row
                .fetch_token
                .as_deref()
                .and_then(|t| <[u8; 32]>::try_from(t).ok())
                .map(|t| xfer::ct_eq32(&t, &fetch_token))
                .unwrap_or(false);
            if !tok_ok {
                self.sh().dropped_inbound += 1;
                return;
            }
            // Re-offer while a transfer is live → re-accept with current
            // ranges. `Offered` is excluded on purpose: the row is live but
            // NOT yet accepted locally, so answering FileAccept here would
            // acknowledge a transfer the user never approved.
            let reaccept = FileState::parse(&row.state)
                .map(|s| {
                    matches!(
                        s,
                        FileState::Accepted | FileState::Transferring | FileState::Verifying
                    )
                })
                .unwrap_or(false);
            if reaccept && row.dir == Dir::In {
                self.send_file_accept(&row).await;
            }
            return;
        }
        if self.meta_state(&peer) != Some(MetaState::Active) {
            self.sh().dropped_inbound += 1;
            return;
        }
        let verified = self.contact_verified(&peer).await;
        let _ = self
            .sblk({
                let cid = chat_id.clone();
                move |s| s.upsert_chat(&cid, peer.as_bytes(), None, verified)
            })
            .await;
        let audio_json = audio.as_ref().and_then(|a| serde_json::to_string(a).ok());
        let tag = kind.tag();
        let body = if kind == FileKind::Audio {
            let dur = audio.as_ref().map(|a| xfer::fmt_duration(a.duration_ms));
            format!(
                "[voice {}] {} · {}",
                dur.unwrap_or_else(|| "0:00".into()),
                name,
                xfer::fmt_size(size)
            )
        } else {
            format!("[{tag}] {name} · {}", xfer::fmt_size(size))
        };
        let (mid, deduped) = self
            .sblk({
                let cid = chat_id.clone();
                move |s| {
                    s.insert_message(&NewMessage {
                        msg_id: reply_msg,
                        chat_id: cid,
                        dir: Dir::In,
                        kind: MsgKind::File,
                        body,
                        state: Some(MsgState::Delivered),
                        client_msg_id: None,
                        ts: now_ms(),
                        expires_at: None,
                    })
                }
            })
            .await
            .unwrap_or_else(|_| (String::new(), true));
        if mid.is_empty() {
            return;
        }
        let new_file = NewFile {
            file_id: file_id_s.clone(),
            msg_id: mid.clone(),
            chat_id: chat_id.clone(),
            dir: Dir::In,
            name: name.clone(),
            mime: mime.clone(),
            size: size as i64,
            sha256: sha256.to_vec(),
            chunk_size: chunk_size as i64,
            chunks_total: chunks_total as i64,
            state: FileState::Offered.as_str().into(),
            kind: kind.as_str().into(),
            thumb: thumb.clone(),
            audio_meta: audio_json,
            fetch_token: Some(fetch_token.to_vec()),
            caption: caption.clone(),
            staged_path: None,
            client_xfer_id: None,
        };
        let inserted = self.sblk(move |s| s.insert_file(&new_file)).await;
        let Ok((_, _)) = inserted else {
            return;
        };
        let row = match self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
        {
            Ok(Some(r)) => r,
            _ => return,
        };
        self.xfer_hydrate(&row, peer);
        if !deduped {
            if let Ok(Some(m)) = self.sblk(move |s| s.get_message(&mid)).await {
                self.emit(
                    "message.received",
                    Some(&chat_id),
                    json!({
                        "chat_id": chat_id,
                        "message": message_json(&m, &peer, &self.peer_id_str, Some(&row)),
                    }),
                );
            }
        }
        self.emit_xfer(&row, &[]);
        // Auto-accept policy (files.auto_accept meta): contacts|verified|manual.
        let policy = self
            .sblk(|s| s.meta("files.auto_accept"))
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "contacts".into());
        let auto = match policy.as_str() {
            "manual" => false,
            "verified" => verified,
            _ => true,
        };
        if auto {
            match self.accept_flow(&row, peer).await {
                Ok(()) => {}
                Err("storage_full") => self.decline_offer(&row, peer, "storage_full").await,
                Err("policy") => self.decline_offer(&row, peer, "policy").await,
                Err(_) => {
                    self.xfer_set_state(&row, FileState::Failed, Some("internal"))
                        .await;
                }
            }
        }
    }

    /// Decline on behalf of policy/caps: wire FileDecline + terminal row.
    async fn decline_offer(self: &Arc<Self>, row: &FileRow, peer: PeerId, reason: &'static str) {
        self.xfer_set_state(row, FileState::Declined, Some(reason))
            .await;
        if let Some(fid) = file_id_raw(&row.file_id) {
            self.deliver_or_queue(
                peer,
                PendingMsg {
                    app: AppMsg::FileDecline { file_id: fid },
                    msg_id: None,
                },
            )
            .await;
        }
    }

    async fn on_file_chunk(
        self: &Arc<Self>,
        peer: PeerId,
        file_id: [u8; 32],
        index: u32,
        data: Vec<u8>,
    ) {
        let file_id_s = file_id_str(&file_id);
        let chat_id = chat_str(&peer);
        let row = match self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
        {
            Ok(Some(r)) => r,
            _ => return,
        };
        if row.chat_id != chat_id || row.dir != Dir::In {
            return;
        }
        let state = FileState::parse(&row.state);
        if !matches!(
            state,
            Some(FileState::Accepted) | Some(FileState::Transferring)
        ) {
            // Terminal, offered-but-not-accepted, or tombstoned: drop.
            return;
        }
        let total = row.chunks_total as u32;
        let cs = row.chunk_size as u64;
        let size = row.size as u64;
        // Geometry: index in-range + exact expected length (last is partial).
        let expected = if index == total.saturating_sub(1) {
            size.saturating_sub(cs * total.saturating_sub(1) as u64)
        } else {
            cs
        };
        if index >= total || data.len() as u64 != expected || cs == 0 {
            let strikes = {
                self.xfer_hydrate(&row, peer);
                let mut sh = self.sh();
                let x = sh.xfers.get_mut(&file_id_s);
                x.map(|x| {
                    x.violations = x.violations.saturating_add(1);
                    x.violations
                })
                .unwrap_or(0)
            };
            if strikes >= 2 {
                self.xfer_set_state(&row, FileState::Failed, Some("policy"))
                    .await;
                self.xfer_unlink(&row).await;
                self.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::FileCancel {
                            file_id,
                            reason: FileCancelReason::Policy,
                        },
                        msg_id: None,
                    },
                )
                .await;
            }
            return;
        }
        let Some(rel) = row.staged_path.clone() else {
            self.xfer_set_state(&row, FileState::Failed, Some("internal"))
                .await;
            return;
        };
        let abs = xfer::stage_abs(&self.data_dir, &rel);
        let offset = index as u64 * cs;
        let wrote = tokio::task::spawn_blocking(move || xfer::stage_write_at(&abs, offset, &data))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        if !wrote {
            self.xfer_set_state(&row, FileState::Failed, Some("storage_full"))
                .await;
            self.xfer_unlink(&row).await;
            return;
        }
        self.xfer_hydrate(&row, peer);
        let caps = self.caps();
        let (done, flush_have) = {
            let mut sh = self.sh();
            let Some(x) = sh.xfers.get_mut(&file_id_s) else {
                return;
            };
            xfer::bit_set(&mut x.have, index);
            x.since_have = x.since_have.saturating_add(1);
            let done = xfer::all_set(&x.have, total);
            let flush = x.since_have >= caps.have_interval || done;
            if flush {
                x.since_have = 0;
            }
            (done, flush)
        };
        if flush_have {
            let fid = file_id_s.clone();
            let have = {
                let sh = self.sh();
                sh.xfers
                    .get(&file_id_s)
                    .map(|x| x.have.clone())
                    .unwrap_or_default()
            };
            let _ = self.sblk(move |s| s.set_file_have(&fid, &have)).await;
            if !done {
                if let Some(fid_raw) = file_id_raw(&file_id_s) {
                    let ranges = {
                        let sh = self.sh();
                        sh.xfers
                            .get(&file_id_s)
                            .map(|x| xfer::ranges_of(&x.have, total))
                            .unwrap_or_default()
                    };
                    self.deliver_or_queue(
                        peer,
                        PendingMsg {
                            app: AppMsg::FileHave {
                                file_id: fid_raw,
                                ranges,
                            },
                            msg_id: None,
                        },
                    )
                    .await;
                }
            }
        }
        if state == Some(FileState::Accepted) {
            self.xfer_set_state(&row, FileState::Transferring, None)
                .await;
        } else {
            let mut fresh = row.clone();
            fresh.state = FileState::Transferring.as_str().into();
            self.emit_xfer(&fresh, &[]);
        }
        if done {
            let mut fresh = row.clone();
            fresh.state = FileState::Verifying.as_str().into();
            self.xfer_set_state(&fresh, FileState::Verifying, None)
                .await;
            self.finalize_inbound(&fresh, peer).await;
        }
    }

    /// FileAccept + FileHave share the merge path; FileAccept additionally
    /// drives the offered→transferring transition and re-queues unacked
    /// `sent` bits.
    async fn on_file_ranges(
        self: &Arc<Self>,
        peer: PeerId,
        file_id: [u8; 32],
        ranges: Vec<(u32, u32)>,
        is_accept: bool,
    ) {
        let file_id_s = file_id_str(&file_id);
        let row = match self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
        {
            Ok(Some(r)) => r,
            _ => return,
        };
        if row.chat_id != chat_str(&peer) || row.dir != Dir::Out {
            return;
        }
        let state = FileState::parse(&row.state);
        if !matches!(
            state,
            Some(FileState::Offered) | Some(FileState::Accepted) | Some(FileState::Transferring)
        ) {
            return;
        }
        self.xfer_hydrate(&row, peer);
        let total = row.chunks_total as u32;
        {
            let mut sh = self.sh();
            let Some(x) = sh.xfers.get_mut(&file_id_s) else {
                return;
            };
            if xfer::merge_ranges(&mut x.have, total, &ranges).is_err() {
                tracing::debug!(%peer, "malformed ranges; dropped");
                return;
            }
            if is_accept {
                // Re-queue chunks the receiver still lacks (spec: resent
                // count ≤ missing count).
                for i in 0..x.have.len() {
                    x.sent[i] &= x.have[i];
                }
                x.mbox_full = false;
            }
        }
        let fid = file_id_s.clone();
        let have = {
            let sh = self.sh();
            sh.xfers
                .get(&file_id_s)
                .map(|x| x.have.clone())
                .unwrap_or_default()
        };
        let _ = self.sblk(move |s| s.set_file_have(&fid, &have)).await;
        if is_accept && state == Some(FileState::Offered) {
            self.xfer_set_state(&row, FileState::Transferring, None)
                .await;
        } else {
            self.emit_xfer(&row, &[]);
        }
        let nudge = {
            let sh = self.sh();
            sh.xfers.get(&file_id_s).map(|x| x.nudge.clone())
        };
        self.xfer_pump_start(&file_id_s, peer);
        if let Some(n) = nudge {
            n.notify_one();
        }
    }

    async fn on_file_done(self: &Arc<Self>, peer: PeerId, file_id: [u8; 32], sha256: [u8; 32]) {
        let file_id_s = file_id_str(&file_id);
        let row = match self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
        {
            Ok(Some(r)) => r,
            _ => return,
        };
        if row.chat_id != chat_str(&peer) || row.dir != Dir::Out {
            return;
        }
        let state = FileState::parse(&row.state);
        if !matches!(
            state,
            Some(FileState::Accepted) | Some(FileState::Transferring) | Some(FileState::Verifying)
        ) {
            return;
        }
        // Completion is bound by the offered digest — a peer-sent FileDone
        // claiming a different hash is dropped, never trusted.
        if !xfer::ct_eq32(
            &sha256,
            &<[u8; 32]>::try_from(row.sha256.as_slice()).unwrap_or([0u8; 32]),
        ) {
            self.sh().dropped_inbound += 1;
            return;
        }
        self.xfer_set_state(&row, FileState::Complete, None).await;
        self.transition(&row.msg_id, MsgState::Delivered, None)
            .await;
        self.xfer_unlink(&row).await;
        let mid = row.msg_id.clone();
        self.deliver_or_queue(
            peer,
            PendingMsg {
                app: AppMsg::Ack { msg_ids: vec![mid] },
                msg_id: None,
            },
        )
        .await;
    }

    async fn on_file_decline(&self, peer: PeerId, file_id: [u8; 32]) {
        let file_id_s = file_id_str(&file_id);
        let Some(row) = self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
            .ok()
            .flatten()
        else {
            return;
        };
        if row.chat_id != chat_str(&peer) || row.dir != Dir::Out {
            return;
        }
        if FileState::parse(&row.state)
            .map(|s| s.is_terminal())
            .unwrap_or(true)
        {
            return;
        }
        self.xfer_set_state(&row, FileState::Declined, Some("peer"))
            .await;
        self.xfer_unlink(&row).await;
    }

    async fn on_file_cancel_wire(&self, peer: PeerId, file_id: [u8; 32], reason: FileCancelReason) {
        let file_id_s = file_id_str(&file_id);
        let Some(row) = self
            .sblk({
                let f = file_id_s.clone();
                move |s| s.get_file(&f)
            })
            .await
            .ok()
            .flatten()
        else {
            return;
        };
        if row.chat_id != chat_str(&peer) {
            return;
        }
        if FileState::parse(&row.state)
            .map(|s| s.is_terminal())
            .unwrap_or(true)
        {
            return;
        }
        self.xfer_set_state(&row, FileState::Cancelled, Some(reason.as_str()))
            .await;
        self.xfer_unlink(&row).await;
    }

    // --- outbound pump -----------------------------------------------------

    /// Start the sender pump once per transfer; later calls no-op.
    fn xfer_pump_start(self: &Arc<Self>, file_id: &str, peer: PeerId) {
        let (cancel, nudge) = {
            let mut sh = self.sh();
            let Some(x) = sh.xfers.get_mut(file_id) else {
                return;
            };
            if x.cancel.is_some() {
                return;
            }
            let c = Arc::new(Notify::new());
            x.cancel = Some(c.clone());
            x.peer = peer;
            (c, x.nudge.clone())
        };
        let me = self.clone();
        let fid = file_id.to_string();
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tokio::spawn(async move {
                me.xfer_pump(fid, peer, cancel, nudge).await;
            }));
    }

    /// Sender pump: sends every chunk the receiver hasn't acked (resend set
    /// = `!have`), paced, parked on MailboxFull/NoPath until nudged.
    async fn xfer_pump(
        self: &Arc<Self>,
        file_id: String,
        peer: PeerId,
        cancel: Arc<Notify>,
        nudge: Arc<Notify>,
    ) {
        loop {
            let row = match self
                .sblk({
                    let f = file_id.clone();
                    move |s| s.get_file(&f)
                })
                .await
            {
                Ok(Some(r)) => r,
                _ => break,
            };
            if !matches!(
                FileState::parse(&row.state),
                Some(FileState::Accepted) | Some(FileState::Transferring)
            ) {
                break;
            }
            let total = row.chunks_total.max(0) as u32;
            let (all_acked, missing) = {
                let mut sh = self.sh();
                let Some(x) = sh.xfers.get_mut(&file_id) else {
                    break;
                };
                // Union the persisted floor into the live map (resume).
                if x.have.len() < row.have.len() {
                    x.have.resize(row.have.len(), 0);
                }
                for (i, b) in row.have.iter().enumerate() {
                    x.have[i] |= *b;
                }
                (
                    xfer::all_set(&x.have, total),
                    (0..total)
                        .filter(|i| !xfer::bit_get(&x.have, *i) && !xfer::bit_get(&x.sent, *i))
                        .collect::<Vec<u32>>(),
                )
            };
            if all_acked {
                break; // FileDone lands via handle_app.
            }
            let caps = self.caps();
            if missing.is_empty() {
                // Everything in flight is awaiting an ack; a FileAccept or
                // the resume tick re-queues real gaps.
                tokio::select! {
                    _ = cancel.notified() => break,
                    _ = nudge.notified() => continue,
                    _ = tokio::time::sleep(caps.resume_retry) => {
                        let mut sh = self.sh();
                        if let Some(x) = sh.xfers.get_mut(&file_id) {
                            for i in 0..x.sent.len() {
                                x.sent[i] &= x.have[i];
                            }
                        }
                    }
                }
                continue;
            }
            let Some(rel) = row.staged_path.clone() else {
                break;
            };
            let src = xfer::stage_abs(&self.data_dir, &rel);
            let size = row.size.max(0) as u64;
            let cs = row.chunk_size.max(0) as u64;
            let mut stalled = false;
            for idx in missing {
                let expected = if idx == total.saturating_sub(1) {
                    size.saturating_sub(cs * total.saturating_sub(1) as u64)
                } else {
                    cs
                };
                let offset = idx as u64 * cs;
                let (src2, exp) = (src.clone(), expected as usize);
                let buf = tokio::task::spawn_blocking(move || read_chunk_at(&src2, offset, exp))
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                let Some(data) = buf else {
                    self.xfer_set_state(&row, FileState::Failed, Some("internal"))
                        .await;
                    break;
                };
                let Some(fid_raw) = file_id_raw(&file_id) else {
                    break;
                };
                let verdict = self
                    .deliver_xfer(
                        peer,
                        &AppMsg::FileChunk {
                            file_id: fid_raw,
                            index: idx,
                            data,
                        },
                    )
                    .await;
                match verdict {
                    XferSend::Sent => {
                        {
                            let mut sh = self.sh();
                            if let Some(x) = sh.xfers.get_mut(&file_id) {
                                xfer::bit_set(&mut x.sent, idx);
                            }
                        }
                        tokio::time::sleep(caps.send_pace).await;
                    }
                    XferSend::MailboxFull => {
                        {
                            let mut sh = self.sh();
                            if let Some(x) = sh.xfers.get_mut(&file_id) {
                                x.mbox_full = true;
                            }
                        }
                        stalled = true;
                        break;
                    }
                    XferSend::NoPath => {
                        self.ensure_handshake(peer);
                        stalled = true;
                        break;
                    }
                }
                // Cancel check between chunks (the notify fires on finish).
                if self
                    .sh()
                    .xfers
                    .get(&file_id)
                    .map(|x| x.cancel.is_none())
                    .unwrap_or(true)
                {
                    return;
                }
            }
            if stalled {
                tokio::select! {
                    _ = cancel.notified() => break,
                    _ = nudge.notified() => {}
                    _ = tokio::time::sleep(caps.resume_retry) => {}
                }
            }
        }
        let mut sh = self.sh();
        if let Some(x) = sh.xfers.get_mut(&file_id) {
            if x.cancel
                .as_ref()
                .map(|c| Arc::ptr_eq(c, &cancel))
                .unwrap_or(false)
            {
                x.cancel = None;
            }
        }
    }

    /// Link-up resume: inbound rows re-emit FileAccept{ranges}; outbound
    /// rows re-emit their FileOffer (byte-identical — receiver dedups it).
    async fn resume_xfers(self: &Arc<Self>) {
        let rows = self.sblk(|s| s.files_resumable()).await.unwrap_or_default();
        for row in rows {
            let Ok(peer) = peer_of_chat(&row.chat_id) else {
                continue;
            };
            if self.meta_state(&peer) != Some(MetaState::Active) {
                continue;
            }
            self.xfer_hydrate(&row, peer);
            match (row.dir, FileState::parse(&row.state)) {
                (Dir::In, Some(FileState::Accepted))
                | (Dir::In, Some(FileState::Transferring))
                | (Dir::In, Some(FileState::Verifying)) => {
                    self.send_file_accept(&row).await;
                }
                (Dir::Out, Some(FileState::Offered))
                | (Dir::Out, Some(FileState::Accepted))
                | (Dir::Out, Some(FileState::Transferring)) => {
                    let offer = self.offer_app(&row);
                    if let Some(app) = offer {
                        self.deliver_or_queue(peer, PendingMsg { app, msg_id: None })
                            .await;
                    }
                    self.xfer_pump_start(&row.file_id, peer);
                }
                _ => {}
            }
        }
    }

    /// Rebuild the wire FileOffer from a stored outbound row (resume path).
    fn offer_app(&self, row: &FileRow) -> Option<AppMsg> {
        let file_id = file_id_raw(&row.file_id)?;
        let fetch_token = <[u8; 32]>::try_from(row.fetch_token.as_deref()?).ok()?;
        let sha256 = <[u8; 32]>::try_from(row.sha256.as_slice()).ok()?;
        let audio = row
            .audio_meta
            .as_deref()
            .and_then(|a| serde_json::from_str::<AudioMeta>(a).ok());
        Some(AppMsg::FileOffer {
            file_id,
            fetch_token,
            name: row.name.clone(),
            mime: row.mime.clone(),
            size: row.size.max(0) as u64,
            sha256,
            chunk_size: row.chunk_size.max(0) as u32,
            chunks_total: row.chunks_total.max(0) as u32,
            kind: FileKind::parse(&row.kind),
            thumb: row.thumb.clone(),
            audio,
            caption: row.caption.clone(),
            reply_msg: row.msg_id.clone(),
        })
    }

    /// Stale staging reaper (§9): non-terminal rows past FILE_STAGE_TTL go
    /// `expired`; completed rows are never touched. Orphan `.part` blobs
    /// (no matching row) are unlinked — the row set is the index of truth.
    async fn file_reap_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last = Instant::now() - Duration::from_secs(3600 * 24 * 365);
        loop {
            tick.tick().await;
            if self.shutdown_requested() {
                break;
            }
            let caps = self.caps();
            if last.elapsed() < caps.reap_every {
                continue;
            }
            last = Instant::now();
            let cutoff = now_ms() - caps.stage_ttl.as_millis() as i64;
            let stale = self
                .sblk(move |s| s.files_stale(cutoff))
                .await
                .unwrap_or_default();
            for row in stale {
                // Tombstoned sends also fail their anchor message so the UI
                // doesn't show it as forever-queued.
                self.xfer_set_state(&row, FileState::Expired, Some("timeout"))
                    .await;
                self.xfer_unlink(&row).await;
                if row.dir == Dir::Out {
                    let _ = self
                        .sblk({
                            let mid = row.msg_id.clone();
                            move |s| s.update_state(&mid, MsgState::Expired)
                        })
                        .await;
                }
            }
            // Orphan .part sweep: any blob not referenced by a row — but
            // only once it has aged past the grace window. A blob whose row
            // insert is still in flight (or a chunk writer mid-append)
            // looks orphaned to a naive sweep and would be reaped
            // mid-staging; the mtime check keeps young files.
            let keep: HashSet<String> = self
                .sblk(|s| s.file_staged_paths())
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            let dir = xfer::stage_dir(&self.data_dir);
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        let rel = format!("xfer/{name}");
                        if !name.ends_with(".part") || keep.contains(&rel) {
                            continue;
                        }
                        let young = e
                            .metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .map(|age| age < PART_ORPHAN_GRACE)
                            .unwrap_or(false);
                        if !young {
                            xfer::stage_unlink(&e.path());
                        }
                    }
                }
            })
            .await;
        }
    }

    /// Row → RPC JSON. `chunks_done` reads the live bitmap when present.
    fn file_json(&self, row: &FileRow) -> Value {
        let have = self
            .sh()
            .xfers
            .get(&row.file_id)
            .map(|x| x.have.clone())
            .unwrap_or_else(|| row.have.clone());
        let audio: Value = row
            .audio_meta
            .as_deref()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(Value::Null);
        json!({
            "file_id": row.file_id,
            "msg_id": row.msg_id,
            "chat_id": row.chat_id,
            "dir": dir_str(row.dir),
            "name": row.name,
            "mime": row.mime,
            "size": row.size,
            "sha256": data_encoding::BASE64.encode(&row.sha256),
            "kind": row.kind,
            "state": row.state,
            "reason": row.reason,
            "chunk_size": row.chunk_size,
            "chunks_total": row.chunks_total,
            "chunks_done": if row.state == "complete" {
                json!(row.chunks_total)
            } else {
                json!(xfer::popcount(&have))
            },
            "has_thumb": row.thumb.is_some(),
            "audio": audio,
            "caption": row.caption,
            "saved_path": row.saved_path,
            "created_at": rfc3339(row.created_at),
            "updated_at": rfc3339(row.updated_at),
        })
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

    /// Disappearing-message sweep: every EXPIRY_SWEEP tick tombstones rows
    /// past their deadline and emits `msg.expired` so the TUI and RPC
    /// subscribers can drop them live. The interval's first tick is
    /// immediate — messages that lapsed while the app was down are swept
    /// at startup.
    async fn expiry_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(EXPIRY_SWEEP);
        loop {
            tick.tick().await;
            if self.shutdown_requested() {
                break;
            }
            let Ok(expired) = self.sblk(|s| s.expire_due(now_ms())).await else {
                continue;
            };
            for (chat_id, msg_id) in expired {
                self.emit(
                    "msg.expired",
                    Some(&chat_id),
                    json!({"chat_id": chat_id, "msg_id": msg_id}),
                );
            }
        }
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
                Some((peer, ps)) => {
                    // The relay-stamped sender must own the sid — a foreign
                    // `from` under a bound stream id is a hijack attempt:
                    // drop WITHOUT decrypt and WITHOUT tombstoning, the
                    // stream still belongs to its live owner.
                    if peer != from {
                        sh.dropped_inbound += 1;
                        tracing::debug!(%from, %peer, ?session_id, "transport under sid bound to another peer; dropped");
                        return;
                    }
                    (ps, peer, false)
                }
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
        let app: AppMsg = match decode_app(&pt) {
            Some(a) => a,
            None => {
                tracing::debug!("AppMsg decode failed; dropped");
                return;
            }
        };
        // The session generation that carried the frame rides into the
        // app layer: call signalling pins to it (M7).
        self.handle_app(peer, app, Some(ps.session_id)).await;
    }

    async fn handle_app(self: &Arc<Self>, peer: PeerId, app: AppMsg, sid: Option<SessionId>) {
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
            AppMsg::Text {
                msg_id,
                body,
                expires_at,
            } => {
                let verified = self.contact_verified(&peer).await;
                let _ = self
                    .sblk({
                        let chat_id = chat_id.clone();
                        move |s| s.upsert_chat(&chat_id, peer.as_bytes(), None, verified)
                    })
                    .await;
                // Peer-supplied deadline is honored verbatim — the local
                // per-chat default never overrides an explicit wire TTL.
                let expires_at = expires_at.map(|e| e.min(i64::MAX as u64) as i64);
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
                                expires_at,
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
                        if m.deleted {
                            // Deadline already passed on receipt: persisted
                            // as a tombstone for audit, never displayed.
                            self.emit(
                                "msg.expired",
                                Some(&m.chat_id),
                                json!({"chat_id": m.chat_id, "msg_id": m.msg_id}),
                            );
                        } else {
                            self.emit(
                                "message.received",
                                Some(&m.chat_id),
                                json!({
                                    "chat_id": m.chat_id,
                                    "message": message_json(&m, &peer, &self.peer_id_str, None),
                                }),
                            );
                        }
                    }
                }
                self.deliver_now(
                    peer,
                    &PendingMsg {
                        app: AppMsg::Ack { msg_ids: vec![id] },
                        msg_id: None,
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
                                "message": message_json(&m, &peer, &self.peer_id_str, None),
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
                                "message": message_json(&m, &peer, &self.peer_id_str, None),
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
                            "message": message_json(&m, &peer, &self.peer_id_str, None),
                        }),
                    );
                }
            }
            AppMsg::FileOffer {
                file_id,
                fetch_token,
                name,
                mime,
                size,
                sha256,
                chunk_size,
                chunks_total,
                kind,
                thumb,
                audio,
                caption,
                reply_msg,
            } => {
                self.on_file_offer(
                    peer,
                    file_id,
                    fetch_token,
                    name,
                    mime,
                    size,
                    sha256,
                    chunk_size,
                    chunks_total,
                    kind,
                    thumb,
                    audio,
                    caption,
                    reply_msg,
                )
                .await;
            }
            AppMsg::FileChunk {
                file_id,
                index,
                data,
            } => {
                self.on_file_chunk(peer, file_id, index, data).await;
            }
            AppMsg::FileHave { file_id, ranges } => {
                self.on_file_ranges(peer, file_id, ranges, false).await;
            }
            AppMsg::FileAccept { file_id, ranges } => {
                self.on_file_ranges(peer, file_id, ranges, true).await;
            }
            AppMsg::FileDone { file_id, sha256 } => {
                self.on_file_done(peer, file_id, sha256).await;
            }
            AppMsg::FileCancel { file_id, reason } => {
                self.on_file_cancel_wire(peer, file_id, reason).await;
            }
            AppMsg::FileDecline { file_id } => {
                self.on_file_decline(peer, file_id).await;
            }
            // ---- Phase A call signalling ----
            AppMsg::CallRing {
                call_id,
                media,
                cap,
                expires_unix,
            } => {
                self.on_call_ring(peer, call_id, media, cap, expires_unix, sid)
                    .await;
            }
            AppMsg::CallRinging { call_id } => {
                self.on_call_ringing(peer, call_id, sid).await;
            }
            AppMsg::CallAccept { call_id, cap } => {
                self.on_call_accept(peer, call_id, cap, sid).await;
            }
            AppMsg::CallEnd { call_id, reason } => {
                self.on_call_end(peer, call_id, reason, sid).await;
            }
            AppMsg::CallFrame {
                call_id,
                seq,
                payload,
                ..
            } => {
                self.on_call_frame(peer, call_id, seq, payload, sid).await;
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

    /// Shared send path for `msg.send` and the TUI composer. `ttl_ms` is the
    /// per-message override: `None` falls back to the chat's local
    /// disappearing default, `Some(0)` sends a non-expiring message, and
    /// `Some(t)` puts `ts + t` on the wire inside the encrypted payload.
    async fn send_text(
        self: &Arc<Self>,
        peer: PeerId,
        body: &str,
        client_msg_id: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(String, String, Option<i64>), RpcError> {
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
        let ttl_ms = match ttl_ms {
            Some(t) => Some(t.min(i64::MAX as u64) as i64),
            None => {
                let cid = chat_id.clone();
                self.sblk(move |s| s.disappearing_ttl(&cid))
                    .await
                    .map_err(store_rpc)?
            }
        };
        let ts = now_ms();
        let expires_at = ttl_ms.filter(|t| *t > 0).map(|t| ts.saturating_add(t));
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
                        ts,
                        expires_at,
                    })
                }
            })
            .await
            .map_err(store_rpc)?;
        let (mid, deduped) = inserted;
        if deduped {
            // The stored row is authoritative — its deadline, not this
            // call's re-computed one.
            let stored_exp = self
                .sblk({
                    let id = mid.clone();
                    move |s| s.get_message(&id).map(|o| o.and_then(|m| m.expires_at))
                })
                .await
                .ok()
                .flatten();
            return Ok((mid, chat_str(&peer), stored_exp));
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
                expires_at: expires_at.map(|e| e.max(0) as u64),
            },
            msg_id: Some(mid.clone()),
        };
        let chat_id2 = chat_id.clone();
        tokio::spawn(async move {
            me.deliver_or_queue(peer, item).await;
        });
        Ok((mid, chat_id2, expires_at))
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
        MsgKind::File => "file",
    }
}

fn trust_str(t: Option<&TrustState>) -> &'static str {
    match t {
        Some(TrustState::Verified) => "verified",
        Some(TrustState::KeyChanged { .. }) => "key_changed",
        _ => "unverified",
    }
}

fn message_json(
    m: &crate::store::Message,
    sender: &PeerId,
    own_peer_str: &str,
    file: Option<&FileRow>,
) -> Value {
    let from = match m.dir {
        Dir::In => peer_str(sender),
        Dir::Out => own_peer_str.to_string(),
    };
    // body.file (files-images §6): every field is peer-claimed until the
    // transfer completes — `trusted:false` covers them all.
    let file_meta = file.map(|f| {
        let audio: Value = f
            .audio_meta
            .as_deref()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(Value::Null);
        json!({
            "file_id": f.file_id,
            "name": f.name,
            "mime": f.mime,
            "size": f.size,
            "kind": f.kind,
            "has_thumb": f.thumb.is_some(),
            "audio": audio,
            "caption": f.caption,
        })
    });
    json!({
        "msg_id": m.msg_id,
        "chat_id": m.chat_id,
        "dir": dir_str(m.dir),
        "from": from,
        "kind": kind_str(m.kind),
        "body": {"text": m.body, "trusted": m.dir == Dir::Out, "file": file_meta},
        "state": m.state.map(state_str),
        "ts": rfc3339(m.ts),
        "ts_ms": m.ts,
        "expires_at_ms": m.expires_at,
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
    last_file: Option<&FileRow>,
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
        "disappearing_ttl_ms": chat.disappear_ttl_ms,
        "last_activity": rfc3339(chat.last_activity),
        "last_message": chat.last_message.as_ref().map(|m| message_json(m, &peer, "", last_file)),
    })
}

fn store_rpc(e: StoreError) -> RpcError {
    match e {
        StoreError::Conflict => RpcError::new(
            ErrorKind::Conflict,
            "client_msg_id replayed with a different payload",
        ),
        StoreError::NotFound(id) => RpcError::new(ErrorKind::NotFound, id),
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
                expires_at: m["expires_at_ms"].as_i64(),
                file: tb_file_meta(&m["body"]["file"]),
            }))
        }
        "file_state" => Some(tb::Event::FileState {
            file_id: p["file_id"].as_str()?.to_string(),
            msg_id: p["msg_id"].as_str().unwrap_or("").to_string(),
            chat_id: p["chat_id"].as_str()?.to_string(),
            state: p["state"].as_str()?.to_string(),
            bytes_done: p["bytes_done"].as_u64().unwrap_or(0),
            bytes_total: p["bytes_total"].as_u64().unwrap_or(0),
            reason: p["reason"].as_str().map(String::from),
        }),
        "msg.expired" => Some(tb::Event::MsgExpired {
            chat_id: p["chat_id"].as_str()?.to_string(),
            msg_id: p["msg_id"].as_str()?.to_string(),
        }),
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
        "security.identity_changed" => {
            // chat:<b32> encodes the same PeerId as peer:<b32>.
            let chat_id = p["peer_id"]
                .as_str()
                .and_then(|s| s.strip_prefix("peer:"))
                .map(|rest| format!("chat:{rest}"))?;
            Some(tb::Event::IdentityChanged {
                contact_id: p["contact_id"].as_str()?.to_string(),
                chat_id,
            })
        }
        "security.verification_needed" => Some(tb::Event::VerificationNeeded {
            contact_id: p["contact_id"].as_str()?.to_string(),
            sas: p["sas_hint"]["emoji"].as_str().unwrap_or("").to_string(),
        }),
        "call.incoming" => Some(tb::Event::CallIncoming {
            call_id: p["call_id"].as_str()?.to_string(),
            contact_id: p["contact_id"].as_str()?.to_string(),
            peer_id: p["peer_id"].as_str()?.to_string(),
            chat_id: p["chat_id"].as_str()?.to_string(),
            media: p["media"].as_str().unwrap_or("audio").to_string(),
        }),
        "call.state" => Some(tb::Event::CallState {
            call_id: p["call_id"].as_str()?.to_string(),
            chat_id: p["chat_id"].as_str()?.to_string(),
            state: p["state"].as_str()?.to_string(),
            dir: p["dir"].as_str().unwrap_or("").to_string(),
            reason: p["reason"].as_str().map(String::from),
            quality: p["quality"].as_str().map(String::from),
        }),
        _ => None,
    }
}

/// `body.file` JSON → TUI metadata. The offered name carries no trust — the
/// renderer sanitizes before drawing.
fn tb_file_meta(f: &Value) -> Option<tb::FileMeta> {
    if f.is_null() {
        return None;
    }
    Some(tb::FileMeta {
        file_id: f["file_id"].as_str()?.to_string(),
        kind: f["kind"].as_str().unwrap_or("file").to_string(),
        name: f["name"].as_str().unwrap_or("file.bin").to_string(),
        size: f["size"].as_u64().unwrap_or(0),
        duration_ms: f["audio"]["duration_ms"].as_u64().map(|d| d as u32),
    })
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
        let (msg_id, chat_id, expires_at) = self
            .arc()
            .send_text(peer, &p.body, &p.client_msg_id, p.ttl_ms)
            .await?;
        Ok(json!({
            "msg_id": msg_id,
            "chat_id": chat_id,
            "expires_at": expires_at.map(rfc3339),
        }))
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
        if scope == "everyone" && m.dir != Dir::Out {
            // Direction check BEFORE the tombstone — a `scope=everyone`
            // call on a peer's message must fail clean, not delete the
            // local copy on its way out.
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "delete for everyone applies to own messages only",
            ));
        }
        self.sblk({
            let id = p.msg_id.clone();
            move |s| s.soft_delete(&id)
        })
        .await
        .map_err(store_rpc)?;
        if scope == "everyone" {
            let peer = peer_of_chat(&m.chat_id)?;
            let me = self.arc();
            let msg_id = m.msg_id.clone();
            tokio::spawn(async move {
                me.deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::Delete {
                            msg_id: msg_id.clone(),
                        },
                        msg_id: Some(msg_id),
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
        // Attach body.file to last_message previews (one batch lookup).
        let last_ids: Vec<String> = chats
            .iter()
            .filter_map(|c| c.last_message.as_ref().map(|m| m.msg_id.clone()))
            .collect();
        let files_by_msg = self
            .sblk(move |s| s.files_by_msgs(&last_ids))
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for (i, chat) in chats.iter().enumerate() {
            if i < offset {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            let peer = PeerId::from_bytes(chat.peer_id);
            let last_file = chat
                .last_message
                .as_ref()
                .and_then(|m| files_by_msg.get(&m.msg_id));
            out.push(chat_json(
                chat,
                metas.get(&peer),
                names.get(&peer).map(String::as_str),
                trusts.get(&peer),
                last_file,
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
        let msg_ids: Vec<String> = msgs.iter().map(|m| m.msg_id.clone()).collect();
        let files_by_msg = self
            .sblk(move |s| s.files_by_msgs(&msg_ids))
            .await
            .unwrap_or_default();
        let items: Vec<Value> = msgs
            .iter()
            .map(|m| message_json(m, &peer, &own, files_by_msg.get(&m.msg_id)))
            .collect();
        Ok(json!({
            "chat_id": p.chat_id,
            "messages": items,
            "has_more": has_more,
            "next_before": next,
        }))
    }

    /// `chat.set_disappearing` — local per-chat default TTL for `msg.send`.
    /// `ttl_ms = 0` disables. Local-only in v1: never put on the wire, so a
    /// peer's own default is unaffected (they keep honoring wire TTLs).
    async fn chat_set_disappearing(&self, p: ChatSetDisappearingParams) -> Result<Value, RpcError> {
        peer_of_chat(&p.chat_id)
            .map_err(|_| RpcError::new(ErrorKind::NotFound, "chat not found"))?;
        let ttl = p.ttl_ms.min(i64::MAX as u64) as i64;
        let cid = p.chat_id.clone();
        self.sblk(move |s| s.set_disappearing(&cid, ttl))
            .await
            .map_err(store_rpc)?;
        Ok(json!({
            "chat_id": p.chat_id,
            "ttl_ms": p.ttl_ms,
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
        // The link-up subscribe only covered contacts active at that
        // moment; a freshly accepted peer must be added so offline flips
        // (mailbox-vs-live routing, call availability) are observed.
        let _ = self.presence_subscribe().await;
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
            .iter()
            .next_back()
            .map(|(_, id)| id.clone())
            .unwrap_or_default();
        // `label` is a local bookkeeping tag (never sent to the invitee):
        // persist it against the minted invite id.
        if let Some(label) = p.label {
            let key = format!("invite.label.{id}");
            let _ = self.sblk(move |s| s.set_meta(&key, &label)).await;
        }
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

    /// `file.send` — stage, hash, row, offer. `path` streams once into a
    /// `xfer/*.part` blob while hashing (outbound shares the staging layout
    /// so resume/re-offer/save stay uniform); `data` decodes under the
    /// 1 MiB inline cap.
    async fn file_send(&self, p: FileSendParams) -> Result<Value, RpcError> {
        let peer = self.resolve_to(&p.to)?;
        self.check_send_allowed(&peer).await?;
        match (&p.path, &p.data) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "exactly one of `path` or `data` is required",
                ));
            }
        }
        if p.name.as_ref().map(|n| n.len()).unwrap_or(0) > 255
            || p.mime.as_ref().map(|m| m.len()).unwrap_or(0) > 64
            || p.caption.as_ref().map(|c| c.len()).unwrap_or(0) > 1024
            || p.client_xfer_id.is_empty()
            || p.client_xfer_id.len() > 128
        {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                "name/mime/caption/client_xfer_id field bound exceeded",
            ));
        }
        let kind = match p.kind.as_deref() {
            None | Some("file") => {
                if p.mime
                    .as_deref()
                    .map(|m| m.starts_with("image/"))
                    .unwrap_or(false)
                {
                    FileKind::Image
                } else {
                    FileKind::File
                }
            }
            Some("image") => FileKind::Image,
            Some("voice") => FileKind::Audio,
            Some(_) => {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "kind must be file|image|voice",
                ));
            }
        };
        if kind == FileKind::Audio && p.audio.is_none() {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                "kind \"voice\" requires `audio` metadata",
            ));
        }
        let audio = p.audio.as_ref().map(|a| AudioMeta {
            duration_ms: a.duration_ms,
            codec: a.codec.clone(),
            sample_rate_hz: a.sample_rate_hz,
            channels: a.channels,
        });
        let caps = self.caps();
        if let Some(a) = &audio {
            if a.duration_ms > caps.voice_max_ms {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "voice note exceeds the 300 s advisory cap",
                ));
            }
        }
        // Same offline gate as send_text: known-offline + no mailbox grant
        // and no live session ⇒ unreachable.
        {
            let sh = self.sh();
            let known_offline = sh.presence.get(&peer).map(|o| !*o).unwrap_or(false);
            let has_grant = sh.meta.get(&peer).and_then(|m| m.grant.as_ref()).is_some();
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
            if !live && known_offline && !has_grant {
                return Err(RpcError::new(
                    ErrorKind::PeerOffline,
                    "peer offline and no mailbox path",
                ));
            }
        }
        // Outbound inflight caps (§2): counted across active out rows; the
        // pending send itself joins on insert, so reject at the cap.
        {
            let cid = chat_str(&peer);
            let (peer_n, all_n) = self
                .sblk(move |s| {
                    Ok::<_, StoreError>((
                        s.file_inflight("out", Some(&cid))?,
                        s.file_inflight("out", None)?,
                    ))
                })
                .await
                .map_err(store_rpc)?;
            if peer_n as usize >= caps.max_inflight_peer || all_n as usize >= caps.max_inflight {
                return Err(RpcError::new(
                    ErrorKind::RateLimited,
                    "outbound transfer inflight cap reached; retry after a transfer completes",
                ));
            }
        }
        let (file_id_raw_v, file_id) = new_file_id();
        let fetch_token = *Token::random().as_bytes();
        let rel = xfer::stage_rel(&file_id_raw_v, &fetch_token);

        // Stage + hash in one blocking pass. For `path` sources the file is
        // verified regular + ≤ cap BEFORE the stream begins (size enforced
        // before allocation, §11).
        let data_dir = self.data_dir.clone();
        let src_path = p.path.as_deref().map(xfer::expand_tilde);
        let inline_data: Option<Vec<u8>> = match &p.data {
            Some(b64) => {
                let raw = data_encoding::BASE64.decode(b64.as_bytes()).map_err(|_| {
                    RpcError::new(ErrorKind::InvalidParams, "`data` is not valid base64")
                })?;
                if raw.len() > caps.inline_data_max {
                    return Err(RpcError::new(
                        ErrorKind::InvalidParams,
                        "`data` exceeds the 1 MiB inline cap",
                    ));
                }
                Some(raw)
            }
            None => None,
        };
        let (size, sha256) = {
            let dir = data_dir.clone();
            let rel2 = rel.clone();
            let src = src_path.clone();
            let inline = inline_data.clone();
            let max = caps.max_bytes;
            tokio::task::spawn_blocking(move || -> Result<(u64, [u8; 32]), String> {
                match (&src, &inline) {
                    (Some(path), None) => {
                        let meta =
                            std::fs::metadata(path).map_err(|e| format!("path unreadable: {e}"))?;
                        if !meta.is_file() {
                            return Err("path is not a regular file".into());
                        }
                        if meta.len() > max {
                            return Err("file exceeds the 64 MiB cap".into());
                        }
                        let abs = xfer::stage_create(&dir, &rel2, 0)
                            .map_err(|e| format!("staging: {e}"))?;
                        let mut r = std::fs::File::open(path)
                            .map_err(|e| format!("path unreadable: {e}"))?;
                        let mut w = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&abs)
                            .map_err(|e| format!("staging: {e}"))?;
                        let n = std::io::copy(&mut r, &mut w)
                            .map_err(|e| format!("staging copy: {e}"))?;
                        let digest =
                            xfer::stage_sha256(&abs).map_err(|e| format!("staging hash: {e}"))?;
                        Ok((n, digest))
                    }
                    (None, Some(bytes)) => {
                        if bytes.len() as u64 > max {
                            return Err("file exceeds the 64 MiB cap".into());
                        }
                        let abs = xfer::stage_create(&dir, &rel2, 0)
                            .map_err(|e| format!("staging: {e}"))?;
                        std::fs::write(&abs, bytes).map_err(|e| format!("staging write: {e}"))?;
                        let digest =
                            xfer::stage_sha256(&abs).map_err(|e| format!("staging hash: {e}"))?;
                        Ok((bytes.len() as u64, digest))
                    }
                    _ => Err("bad source".into()),
                }
            })
            .await
            .map_err(|_| RpcError::internal("staging task panicked"))?
            .map_err(|e: String| {
                if e.contains("cap") || e.contains("regular") || e.contains("unreadable") {
                    RpcError::new(ErrorKind::InvalidParams, e)
                } else {
                    RpcError::internal(format!("staging: {e}"))
                }
            })?
        };
        let chunk_size = caps.chunk_bytes;
        let chunks_total = size.div_ceil(chunk_size as u64) as u32;
        if chunks_total > caps.max_chunks {
            // The receiver would reject this offer outright — fail the
            // send before the file row ever lands.
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                "file exceeds the chunk-count cap",
            ));
        }
        // Name resolution: explicit override, else source basename, else
        // fallback — sanitized before it touches the wire OR the store.
        let name = p.name.clone().unwrap_or_else(|| {
            src_path
                .as_ref()
                .and_then(|sp| sp.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "file.bin".into())
        });
        let name = xfer::sanitize_name(&name);
        let mime = p
            .mime
            .clone()
            .or_else(|| mime_guess::from_path(&name).first_raw().map(String::from))
            .unwrap_or_else(|| "application/octet-stream".into());
        // Client-generated thumbnail ≤ caps (image sends only).
        let thumb = if kind == FileKind::Image {
            let abs = xfer::stage_abs(&data_dir, &rel);
            let caps2 = caps.clone();
            tokio::task::spawn_blocking(move || xfer::make_thumb(&abs, &caps2))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let chat_id = chat_str(&peer);
        let mid = new_msg_id();
        let tag = kind.tag();
        let body = if kind == FileKind::Audio {
            let dur = audio.as_ref().map(|a| xfer::fmt_duration(a.duration_ms));
            format!(
                "[voice {}] {} · {}",
                dur.unwrap_or_else(|| "0:00".into()),
                name,
                xfer::fmt_size(size)
            )
        } else {
            format!("[{tag}] {name} · {}", xfer::fmt_size(size))
        };
        let (mid, msg_deduped) = self
            .sblk({
                let cid = chat_id.clone();
                let cxid = p.client_xfer_id.clone();
                let mid2 = mid.clone();
                move |s| {
                    s.insert_message(&NewMessage {
                        msg_id: mid2,
                        chat_id: cid,
                        dir: Dir::Out,
                        kind: MsgKind::File,
                        body,
                        state: Some(MsgState::Queued),
                        client_msg_id: Some(cxid),
                        ts: now_ms(),
                        expires_at: None,
                    })
                }
            })
            .await
            .map_err(store_rpc)?;
        let offer = AppMsg::FileOffer {
            file_id: file_id_raw_v,
            fetch_token,
            name: name.clone(),
            mime: mime.clone(),
            size,
            sha256,
            chunk_size,
            chunks_total,
            kind,
            thumb: thumb.clone(),
            audio: audio.clone(),
            caption: p.caption.clone(),
            reply_msg: mid.clone(),
        };
        // Wire budget: the whole offer must fit the 8192 B plaintext bucket.
        // Oversize ⇒ the thumbnail goes first; still-over ⇒ invalid params.
        let mut offer = offer;
        if let Ok(b) = wire_serialize(&offer) {
            if b.len() > caps.offer_budget {
                if let AppMsg::FileOffer { thumb: t, .. } = &mut offer {
                    *t = None;
                }
            }
        }
        match wire_serialize(&offer) {
            Ok(b) if b.len() <= caps.offer_budget => {}
            Ok(_) => {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "offer metadata exceeds the wire budget",
                ));
            }
            Err(_) => return Err(RpcError::internal("offer encode failed")),
        }
        // The wire-budget pass may have trimmed the thumbnail — keep the row
        // consistent with what actually went out.
        let wire_thumb = match &offer {
            AppMsg::FileOffer { thumb, .. } => thumb.clone(),
            _ => None,
        };
        let (fid, file_deduped) = self
            .sblk({
                let cid = chat_id.clone();
                let cxid = p.client_xfer_id.clone();
                let mid3 = mid.clone();
                let audio_json = audio.as_ref().and_then(|a| serde_json::to_string(a).ok());
                let ft = fetch_token.to_vec();
                let cap = p.caption.clone();
                let nm = name.clone();
                let mm = mime.clone();
                move |s| {
                    s.insert_file(&NewFile {
                        file_id,
                        msg_id: mid3,
                        chat_id: cid,
                        dir: Dir::Out,
                        name: nm,
                        mime: mm,
                        size: size as i64,
                        sha256: sha256.to_vec(),
                        chunk_size: chunk_size as i64,
                        chunks_total: chunks_total as i64,
                        state: FileState::Offered.as_str().into(),
                        kind: kind.as_str().into(),
                        thumb: wire_thumb,
                        audio_meta: audio_json,
                        fetch_token: Some(ft),
                        caption: cap,
                        staged_path: Some(rel),
                        client_xfer_id: Some(cxid),
                    })
                }
            })
            .await
            .map_err(|e| match e {
                StoreError::Conflict => RpcError::new(
                    ErrorKind::Conflict,
                    "client_xfer_id replayed with a different payload",
                ),
                other => store_rpc(other),
            })?;
        if file_deduped {
            // Replay: the original row carries the authoritative ids.
            let row = self
                .sblk({
                    let f = fid.clone();
                    move |s| s.get_file(&f)
                })
                .await
                .map_err(store_rpc)?
                .ok_or_else(|| RpcError::internal("file row vanished"))?;
            return Ok(json!({
                "file_id": row.file_id,
                "msg_id": row.msg_id,
                "chat_id": row.chat_id,
                "state": row.state,
                "chunks_total": row.chunks_total,
                "deduped": true,
            }));
        }
        self.emit(
            "msg_state",
            Some(&chat_id),
            json!({
                "chat_id": chat_id,
                "msg_id": mid,
                "client_msg_id": p.client_xfer_id,
                "state": "queued",
            }),
        );
        if let Ok(Some(row)) = self
            .sblk({
                let f = fid.clone();
                move |s| s.get_file(&f)
            })
            .await
        {
            self.xfer_hydrate(&row, peer);
            self.emit_xfer(&row, &[]);
        }
        // A deduped ANCHOR (client_xfer_id replay whose file row was never
        // committed — e.g. crash mid-send) still owes the wire its FileOffer:
        // the file row is brand new, only the message insert deduped.
        let me = self.arc();
        let mid2 = mid.clone();
        tokio::spawn(async move {
            me.deliver_or_queue(
                peer,
                PendingMsg {
                    app: offer,
                    msg_id: Some(mid2),
                },
            )
            .await;
        });
        Ok(json!({
            "file_id": fid,
            "msg_id": mid,
            "chat_id": chat_id,
            "state": "offered",
            "chunks": chunks_total,
            "bytes": size,
            "deduped": msg_deduped,
        }))
    }

    async fn file_accept(&self, p: FileIdParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        if row.dir != Dir::In {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "only inbound offers can be accepted",
            ));
        }
        if FileState::parse(&row.state) != Some(FileState::Offered) {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                format!("cannot accept a transfer in state {}", row.state),
            ));
        }
        let peer = peer_of_chat(&row.chat_id)?;
        match self.arc().accept_flow(&row, peer).await {
            Ok(()) => Ok(json!({"file_id": row.file_id, "state": "transferring"})),
            Err("storage_full") => {
                self.arc().decline_offer(&row, peer, "storage_full").await;
                Err(RpcError::new(ErrorKind::RateLimited, "staging cap reached"))
            }
            Err("policy") => {
                self.arc().decline_offer(&row, peer, "policy").await;
                Err(RpcError::new(
                    ErrorKind::RateLimited,
                    "in-flight transfer cap reached",
                ))
            }
            Err(_) => Err(RpcError::internal("accept staging failed")),
        }
    }

    async fn file_decline(&self, p: FileIdParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        if row.dir != Dir::In {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "only inbound offers can be declined",
            ));
        }
        if FileState::parse(&row.state) != Some(FileState::Offered) {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                format!("cannot decline a transfer in state {}", row.state),
            ));
        }
        let peer = peer_of_chat(&row.chat_id)?;
        self.arc().decline_offer(&row, peer, "user").await;
        Ok(json!({"file_id": row.file_id, "state": "declined"}))
    }

    async fn file_cancel(&self, p: FileIdParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        if FileState::parse(&row.state)
            .map(|s| s.is_terminal())
            .unwrap_or(true)
        {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                format!("transfer already {}", row.state),
            ));
        }
        let peer = peer_of_chat(&row.chat_id)?;
        self.xfer_set_state(&row, FileState::Cancelled, Some("user"))
            .await;
        self.xfer_unlink(&row).await;
        if let Some(fid) = file_id_raw(&row.file_id) {
            self.arc()
                .deliver_or_queue(
                    peer,
                    PendingMsg {
                        app: AppMsg::FileCancel {
                            file_id: fid,
                            reason: FileCancelReason::User,
                        },
                        msg_id: None,
                    },
                )
                .await;
        }
        Ok(json!({"file_id": row.file_id, "state": "cancelled"}))
    }

    async fn file_info(&self, p: FileIdParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        Ok(self.file_json(&row))
    }

    async fn file_thumb(&self, p: FileIdParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        match row.thumb {
            Some(t) => Ok(json!({
                "file_id": row.file_id,
                "thumb_b64": data_encoding::BASE64.encode(&t),
                "mime": "image/jpeg",
            })),
            None => Ok(json!({
                "file_id": row.file_id,
                "thumb_b64": Value::Null,
                "mime": Value::Null,
            })),
        }
    }

    async fn file_list(&self, p: FileListParams) -> Result<Value, RpcError> {
        let limit = p.limit.unwrap_or(50).min(200);
        let offset = match &p.cursor {
            None => 0u64,
            Some(c) => c
                .strip_prefix("cur:off:")
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| RpcError::new(ErrorKind::InvalidParams, "invalid cursor"))?,
        };
        let state = p.state.clone();
        if let Some(s) = &state {
            if FileState::parse(s).is_none() {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "unknown state filter",
                ));
            }
        }
        let cid = p.chat_id.clone();
        let rows = self
            .sblk(move |s| s.list_files(cid.as_deref(), state.as_deref(), limit + 1, offset))
            .await
            .map_err(store_rpc)?;
        let has_more = rows.len() > limit as usize;
        let items: Vec<Value> = rows
            .iter()
            .take(limit as usize)
            .map(|r| self.file_json(r))
            .collect();
        let next_cursor = has_more.then(|| format!("cur:off:{}", offset + limit as u64));
        Ok(json!({"files": items, "next_cursor": next_cursor}))
    }

    /// `file.save` — the untrusted-name gauntlet (§5.4): sanitize, symlink
    /// containment, collision suffixes ≤64, 0600, re-hash before rename.
    async fn file_save(&self, p: FileSaveParams) -> Result<Value, RpcError> {
        let row = self.file_row(&p.file_id).await?;
        if FileState::parse(&row.state) != Some(FileState::Complete) {
            return Err(RpcError::new(ErrorKind::Conflict, "transfer not finished"));
        }
        if row.dir != Dir::In {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "only inbound transfers can be saved",
            ));
        }
        let rel = row
            .staged_path
            .clone()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| RpcError::internal("staged blob missing"))?;
        if p.name.as_ref().map(|n| n.len()).unwrap_or(0) > 255 {
            return Err(RpcError::new(ErrorKind::InvalidParams, "name too long"));
        }
        let explicit_dir = p.dir.is_some();
        let save_dir = match &p.dir {
            Some(d) => xfer::expand_tilde(d),
            None => dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| self.data_dir.join("saved")),
        };
        let data_dir = self.data_dir.clone();
        let result = tokio::task::spawn_blocking(
            move || -> std::result::Result<(PathBuf, u64), RpcError> {
                if !save_dir.exists() {
                    if explicit_dir {
                        return Err(RpcError::new(
                            ErrorKind::InvalidParams,
                            "save directory does not exist",
                        ));
                    }
                    std::fs::create_dir_all(&save_dir).map_err(|e| {
                        RpcError::new(ErrorKind::InvalidParams, format!("save dir: {e}"))
                    })?;
                }
                if !save_dir.is_dir() {
                    return Err(RpcError::new(
                        ErrorKind::InvalidParams,
                        "save path is not a directory",
                    ));
                }
                // The dir ITSELF may not be a symlink (a symlinked ancestor
                // is fine — /var → /private/var on macOS is everywhere); the
                // canonical base + separator-free names gives containment.
                let meta = std::fs::symlink_metadata(&save_dir).map_err(|e| {
                    RpcError::new(ErrorKind::InvalidParams, format!("save dir: {e}"))
                })?;
                if meta.file_type().is_symlink() {
                    return Err(RpcError::new(
                        ErrorKind::Conflict,
                        "save dir is a symlink; use a canonical path",
                    ));
                }
                let canon = std::fs::canonicalize(&save_dir).map_err(|e| {
                    RpcError::new(ErrorKind::InvalidParams, format!("save dir: {e}"))
                })?;
                let src = xfer::stage_abs(&data_dir, &rel);
                // Re-hash before write: a corrupted/tampered blob never lands
                // in the user's directory under the offered name.
                let digest = xfer::stage_sha256(&src)
                    .map_err(|e| RpcError::internal(format!("re-hash: {e}")))?;
                if digest.as_slice() != row.sha256.as_slice() {
                    return Err(RpcError::internal("staged blob failed re-hash"));
                }
                let base = xfer::sanitize_name(p.name.as_deref().unwrap_or(&row.name));
                for n in 0..64u32 {
                    let cand = xfer::collision_name(&base, n);
                    let dest = canon.join(&cand);
                    // Containment: canonical parent + separator-free name.
                    debug_assert!(dest.starts_with(&canon));
                    let mut opts = std::fs::OpenOptions::new();
                    opts.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        opts.mode(0o600);
                    }
                    match opts.open(&dest) {
                        Ok(mut w) => {
                            let mut r = std::fs::File::open(&src)
                                .map_err(|e| RpcError::internal(format!("staged blob: {e}")))?;
                            let bytes = std::io::copy(&mut r, &mut w)
                                .map_err(|e| RpcError::internal(format!("save write: {e}")))?;
                            return Ok((dest, bytes));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                            return Err(RpcError::new(
                                ErrorKind::InvalidParams,
                                "save directory is not writable",
                            ));
                        }
                        Err(e) => {
                            return Err(RpcError::internal(format!("save: {e}")));
                        }
                    }
                }
                Err(RpcError::new(
                    ErrorKind::Conflict,
                    "no free collision-safe name within 64 attempts",
                ))
            },
        )
        .await
        .map_err(|_| RpcError::internal("save task panicked"))??;
        let (dest, bytes) = result;
        let path_str = dest.to_string_lossy().to_string();
        let fid = row.file_id.clone();
        let p2 = path_str.clone();
        let _ = self.sblk(move |s| s.set_file_saved(&fid, &p2)).await;
        Ok(json!({
            "file_id": row.file_id,
            "path": path_str,
            "bytes": bytes,
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

    // ---------- Phase A voice calls ----------

    async fn call_ring(&self, p: CallRingParams) -> Result<Value, RpcError> {
        let peer = self.resolve_to(&p.to)?;
        if let Some(m) = &p.media {
            if m != "audio" {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "media must be \"audio\" (the only Phase A kind)",
                ));
            }
        }
        let caps = self.call_caps();
        // Contact + trust gates mirror msg.send.
        self.check_send_allowed(&peer).await?;
        if self.meta_state(&peer) != Some(MetaState::Active) {
            return Err(RpcError::new(
                ErrorKind::PeerOffline,
                "contact is not connected",
            ));
        }
        // Live path only — calls never queue or mailbox-deposit (CA18).
        let Some(ps) = self.call_live_path(&peer) else {
            return Err(RpcError::new(
                ErrorKind::PeerOffline,
                "no live session with peer (calls cannot be deposited)",
            ));
        };
        let call_id = *HsId::random().as_bytes();
        let expires_unix = (now_ms() + caps.ring_timeout.as_millis() as i64) as u64;
        let c = Call {
            id: call_id,
            peer,
            dir: CallDir::Out,
            state: CallState::Outgoing,
            local_sid: ps.session_id,
            cap: call::CAP_AUDIO_OPUS48K,
            since_ms: now_ms(),
            ring_deadline: Instant::now() + caps.ring_timeout,
            deliver_deadline: Some(Instant::now() + caps.deliver_timeout),
            max_deadline: Instant::now() + caps.max_duration,
            rt: None,
        };
        // Reserve the call BEFORE the ring goes on the wire: capacity
        // recheck + throttle + insert are atomic under the shared lock, so
        // two concurrent rings can't both pass (M11), and a peer answering
        // at wire speed always finds the row (M3).
        {
            let mut sh = self.sh();
            if sh
                .calls
                .values()
                .filter(|c| c.state != CallState::Ended)
                .count()
                >= caps.max_active
            {
                return Err(RpcError::new(
                    ErrorKind::Conflict,
                    "a call is already in progress (CALL_MAX_ACTIVE=1)",
                ));
            }
            if let Some(t) = sh.call_last_ring_out.get(&peer) {
                if t.elapsed() < caps.ring_min_gap {
                    return Err(RpcError::new(
                        ErrorKind::RateLimited,
                        format!(
                            "re-ring to this peer within {}s",
                            caps.ring_min_gap.as_secs()
                        ),
                    ));
                }
            }
            sh.calls.insert(call_id, c.clone());
            sh.call_last_ring_out.insert(peer, Instant::now());
        }
        let sent = self
            .send_call_ctrl(
                peer,
                &AppMsg::CallRing {
                    call_id,
                    media: call::MEDIA_AUDIO,
                    cap: call::CAP_AUDIO_OPUS48K,
                    expires_unix,
                },
                ps.session_id,
            )
            .await;
        if !sent {
            // Roll the reservation back — no state was emitted for it.
            self.sh().calls.remove(&call_id);
            return Err(RpcError::new(
                ErrorKind::PeerOffline,
                "ring could not be delivered on the live session",
            ));
        }
        self.emit_call_state(&c, None);
        Ok(json!({
            "call_id": Self::call_id_str(&call_id),
            "chat_id": chat_str(&peer),
            "state": "outgoing",
            "media": "audio",
        }))
    }

    async fn call_accept(&self, p: CallIdParams) -> Result<Value, RpcError> {
        let (call, sid) = {
            let mut sh = self.sh();
            let target = self.pick_call_locked(&mut sh, &p.call_id, |c| {
                c.dir == CallDir::In && c.state == CallState::Ringing
            })?;
            target.state = CallState::Active;
            (target.clone(), target.local_sid)
        };
        let negotiated = call.cap & call::CAP_AUDIO_OPUS48K;
        if negotiated == 0 {
            // Nothing left after the cap intersection — the call is
            // unsupportable, not a transport problem.
            let mut call = call;
            {
                let mut sh = self.sh();
                if let Some(c) = sh.calls.get_mut(&call.id) {
                    if c.state == CallState::Active {
                        c.state = CallState::Ended;
                    }
                }
            }
            call.state = CallState::Ended;
            let _ = self
                .send_call_ctrl(
                    call.peer,
                    &AppMsg::CallEnd {
                        call_id: call.id,
                        reason: EndReason::Failed.to_u8(),
                    },
                    sid,
                )
                .await;
            self.emit_call_state(&call, Some(EndReason::Failed));
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "no media capability survives negotiation",
            ));
        }
        let sent = self
            .send_call_ctrl(
                call.peer,
                &AppMsg::CallAccept {
                    call_id: call.id,
                    cap: negotiated,
                },
                sid,
            )
            .await;
        if !sent {
            let mut sh = self.sh();
            if let Some(c) = sh.calls.get_mut(&call.id) {
                // Revert ONLY our own mark: a concurrent terminal transition
                // (janitor, hangup, peer CallEnd) must never be flipped back
                // to Ringing — ended is absorbing.
                if c.state == CallState::Active {
                    c.state = CallState::Ringing;
                }
            }
            return Err(RpcError::new(
                ErrorKind::PeerOffline,
                "session dropped before accept could be sent",
            ));
        }
        self.arc().start_media(call.id, call.peer, sid);
        self.emit_call_state(&call, None);
        Ok(json!({
            "call_id": Self::call_id_str(&call.id),
            "state": "active",
            "media": "audio",
        }))
    }

    async fn call_decline(&self, p: CallIdParams) -> Result<Value, RpcError> {
        let call = {
            let mut sh = self.sh();
            let c = self.pick_call_locked(&mut sh, &p.call_id, |c| {
                c.dir == CallDir::In && c.state == CallState::Ringing
            })?;
            c.state = CallState::Ended;
            c.clone()
        };
        let _ = self
            .send_call_ctrl(
                call.peer,
                &AppMsg::CallEnd {
                    call_id: call.id,
                    reason: EndReason::Decline.to_u8(),
                },
                call.local_sid,
            )
            .await;
        self.emit_call_state(&call, Some(EndReason::Decline));
        Ok(json!({
            "call_id": Self::call_id_str(&call.id),
            "state": "ended",
            "reason": "decline",
        }))
    }

    async fn call_hangup(&self, p: CallIdParams) -> Result<Value, RpcError> {
        let call = {
            let mut sh = self.sh();
            let c = self.pick_call_locked(&mut sh, &p.call_id, |c| c.state != CallState::Ended)?;
            c.state = CallState::Ended;
            c.clone()
        };
        if let Some(rt) = &call.rt {
            rt.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = self
            .send_call_ctrl(
                call.peer,
                &AppMsg::CallEnd {
                    call_id: call.id,
                    reason: EndReason::Hangup.to_u8(),
                },
                call.local_sid,
            )
            .await;
        self.emit_call_state(&call, Some(EndReason::Hangup));
        Ok(json!({
            "call_id": Self::call_id_str(&call.id),
            "state": "ended",
            "reason": "hangup",
        }))
    }

    async fn call_info(&self, p: CallIdParams) -> Result<Value, RpcError> {
        let call = {
            let sh = self.sh();
            match &p.call_id {
                Some(s) => {
                    let id = Self::call_id_raw(s).ok_or_else(|| {
                        RpcError::new(ErrorKind::InvalidParams, "malformed call_id")
                    })?;
                    sh.calls
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "call_id not found"))?
                }
                None => sh
                    .calls
                    .values()
                    .filter(|c| c.state != CallState::Ended)
                    .max_by_key(|c| c.since_ms)
                    .or_else(|| sh.calls.values().max_by_key(|c| c.since_ms))
                    .cloned()
                    .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "no call exists"))?,
            }
        };
        let mut v = json!({
            "call_id": Self::call_id_str(&call.id),
            "state": call.state.as_str(),
            "dir": call.dir.as_str(),
            "peer_id": peer_str(&call.peer),
            "contact_id": contact_str(&call.peer),
            "chat_id": chat_str(&call.peer),
            "media": "audio",
            "since_ts": rfc3339(call.since_ms),
        });
        if let Some(rt) = &call.rt {
            use std::sync::atomic::Ordering::Relaxed;
            rt.sync_drops();
            v["stats"] = json!({
                "tx_frames": rt.tx_frames.load(Relaxed),
                "rx_frames": rt.rx_frames.load(Relaxed),
                "plc": rt.plc.load(Relaxed),
                "drops_out": rt.drops_out.load(Relaxed),
                "drops_late": rt.drops_late.load(Relaxed),
            });
            v["muted"] = Value::Bool(rt.muted.load(Relaxed));
            v["quality"] = Value::String(
                rt.quality
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_str()
                    .into(),
            );
        }
        Ok(v)
    }

    /// §7: mute is a LOCAL mic gate — no AppMsg goes on the wire (CA16).
    async fn call_mute(&self, p: CallMuteParams) -> Result<Value, RpcError> {
        let call = {
            let sh = self.sh();
            match &p.call_id {
                Some(s) => {
                    let id = Self::call_id_raw(s).ok_or_else(|| {
                        RpcError::new(ErrorKind::InvalidParams, "malformed call_id")
                    })?;
                    sh.calls
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "call_id not found"))?
                }
                None => sh
                    .calls
                    .values()
                    .find(|c| c.state == CallState::Active)
                    .cloned()
                    .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "no active call"))?,
            }
        };
        if call.state != CallState::Active {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "mute applies only to an active call",
            ));
        }
        let Some(rt) = &call.rt else {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "call has no media runtime (not active)",
            ));
        };
        let muted = p.muted.unwrap_or(true);
        rt.muted.store(muted, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({
            "call_id": Self::call_id_str(&call.id),
            "muted": muted,
            "note": "local mic gate only; no wire signal is emitted",
        }))
    }
}

impl Core {
    /// `file:` id → row or NotFound — shared by the file.* RPC methods.
    async fn file_row(&self, file_id: &str) -> Result<FileRow, RpcError> {
        let fid = file_id.to_string();
        self.sblk(move |s| s.get_file(&fid))
            .await
            .map_err(store_rpc)?
            .ok_or_else(|| RpcError::new(ErrorKind::NotFound, "file_id not found"))
    }

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
        let peer_id = core.peer_id().to_string();
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
        let msg_ids: Vec<String> = msgs.iter().map(|m| m.msg_id.clone()).collect();
        let files_by_msg = core
            .sblk(move |s| s.files_by_msgs(&msg_ids))
            .await
            .unwrap_or_default();
        let own = self.peer_id.clone();
        Ok(tb::HistoryPage {
            messages: msgs
                .into_iter()
                .map(|m| tb::Message {
                    file: files_by_msg.get(&m.msg_id).map(|f| {
                        let audio: Option<serde_json::Value> = f
                            .audio_meta
                            .as_deref()
                            .and_then(|a| serde_json::from_str(a).ok());
                        tb::FileMeta {
                            file_id: f.file_id.clone(),
                            kind: f.kind.clone(),
                            name: f.name.clone(),
                            size: f.size.max(0) as u64,
                            duration_ms: audio
                                .as_ref()
                                .and_then(|a| a["duration_ms"].as_u64())
                                .map(|d| d as u32),
                        }
                    }),
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
                    expires_at: m.expires_at,
                })
                .collect(),
            has_more: next.is_some(),
            next_before: next,
        })
    }

    async fn send(&mut self, chat_id: &str, body: &str) -> Result<tb::SendAck> {
        let peer = peer_of_chat(chat_id).map_err(|e| anyhow!("{e:?}"))?;
        let cmid = format!("tui:{}", b32(HsId::random().as_bytes()));
        let (msg_id, _, expires_at) = self
            .core
            .clone()
            .send_text(peer, body, &cmid, None)
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
            expires_at,
        })
    }

    async fn send_file(&mut self, chat_id: &str, path: &str) -> Result<tb::SendAck> {
        let cxid = format!("tui:{}", b32(HsId::random().as_bytes()));
        let v = CinderCore::file_send(
            self.core.as_ref(),
            FileSendParams {
                to: chat_id.to_string(),
                path: Some(path.to_string()),
                data: None,
                name: None,
                mime: None,
                kind: None,
                audio: None,
                caption: None,
                client_xfer_id: cxid.clone(),
            },
        )
        .await
        .map_err(|e| {
            anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )
        })?;
        Ok(tb::SendAck {
            msg_id: v["msg_id"].as_str().unwrap_or("").to_string(),
            client_msg_id: cxid,
            expires_at: None,
        })
    }

    async fn file_save(&mut self, file_id: &str, dir: Option<String>) -> Result<String> {
        let v = CinderCore::file_save(
            self.core.as_ref(),
            FileSaveParams {
                file_id: file_id.to_string(),
                dir,
                name: None,
            },
        )
        .await
        .map_err(|e| {
            anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )
        })?;
        Ok(v["path"].as_str().unwrap_or("").to_string())
    }

    async fn file_cancel(&mut self, file_id: &str) -> Result<()> {
        CinderCore::file_cancel(
            self.core.as_ref(),
            FileIdParams {
                file_id: file_id.to_string(),
            },
        )
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

    async fn file_accept(&mut self, file_id: &str) -> Result<()> {
        CinderCore::file_accept(
            self.core.as_ref(),
            FileIdParams {
                file_id: file_id.to_string(),
            },
        )
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

    async fn file_decline(&mut self, file_id: &str) -> Result<()> {
        CinderCore::file_decline(
            self.core.as_ref(),
            FileIdParams {
                file_id: file_id.to_string(),
            },
        )
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

    async fn set_disappearing(&mut self, chat_id: &str, ttl_ms: u64) -> Result<()> {
        peer_of_chat(chat_id).map_err(|e| anyhow!("{e:?}"))?;
        let ttl = ttl_ms.min(i64::MAX as u64) as i64;
        let cid = chat_id.to_string();
        self.core
            .sblk(move |s| s.set_disappearing(&cid, ttl))
            .await
            .map_err(|e| anyhow!("{e}"))?;
        Ok(())
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

    async fn call_ring(&mut self, to: &str) -> Result<String> {
        let v = CinderCore::call_ring(
            self.core.as_ref(),
            CallRingParams {
                to: to.to_string(),
                media: Some("audio".into()),
                client_call_id: None,
            },
        )
        .await
        .map_err(|e| {
            anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )
        })?;
        Ok(v["call_id"].as_str().unwrap_or("").to_string())
    }

    async fn call_accept(&mut self, call_id: Option<&str>) -> Result<()> {
        CinderCore::call_accept(
            self.core.as_ref(),
            CallIdParams {
                call_id: call_id.map(String::from),
            },
        )
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

    async fn call_decline(&mut self, call_id: Option<&str>) -> Result<()> {
        CinderCore::call_decline(
            self.core.as_ref(),
            CallIdParams {
                call_id: call_id.map(String::from),
            },
        )
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

    async fn call_hangup(&mut self, call_id: Option<&str>) -> Result<()> {
        CinderCore::call_hangup(
            self.core.as_ref(),
            CallIdParams {
                call_id: call_id.map(String::from),
            },
        )
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

    async fn call_mute(&mut self, muted: bool) -> Result<()> {
        CinderCore::call_mute(
            self.core.as_ref(),
            CallMuteParams {
                call_id: None,
                muted: Some(muted),
            },
        )
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

    async fn call_info(&mut self) -> Result<Option<String>> {
        match CinderCore::call_info(self.core.as_ref(), CallIdParams { call_id: None }).await {
            Ok(v) => Ok(Some(format!(
                "{} ({}, {})",
                v["call_id"].as_str().unwrap_or(""),
                v["state"].as_str().unwrap_or(""),
                v["dir"].as_str().unwrap_or("")
            ))),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!(
                "{}: {}",
                e.kind.as_str(),
                e.hint.as_deref().unwrap_or(&e.message)
            )),
        }
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
