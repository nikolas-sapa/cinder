use crate::rpc::protocol::RpcError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct MsgSendParams {
    pub to: String,
    pub body: String,
    pub client_msg_id: String,
    /// Per-message TTL override (ms). Absent → the chat's local
    /// `chat.set_disappearing` default applies; `0` sends a non-expiring
    /// message; `t > 0` sets `expires_at = send_ts + t` on the wire.
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct MsgEditParams {
    pub msg_id: String,
    pub body: String,
    pub client_op_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MsgDeleteParams {
    pub msg_id: String,
    pub scope: Option<String>,
    pub client_op_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MsgReactParams {
    pub msg_id: String,
    pub emoji: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatListParams {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatHistoryParams {
    pub chat_id: String,
    pub before: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatSetDisappearingParams {
    pub chat_id: String,
    /// Local default TTL for messages sent in this chat; `0` disables.
    pub ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct ContactListParams {
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContactAddParams {
    pub link: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContactIdParams {
    pub contact_id: String,
}

/// `contact.deny` — permanent compromised-peer containment. The param is
/// the same shape as the other contact ops; the SEMANTICS are the
/// irreversible part: the peer-id lands on the denylist and no path can
/// re-establish it.
#[derive(Debug, Deserialize)]
pub struct ContactDenyParams {
    pub contact_id: String,
}

/// `chat.wipe` — destroy a conversation locally and tell the peer to do
/// the same when a live session carries the notice.
#[derive(Debug, Deserialize)]
pub struct ChatWipeParams {
    pub chat_id: String,
}

#[derive(Debug, Deserialize)]
pub struct InviteCreateParams {
    pub label: Option<String>,
    pub ttl_sec: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct InviteAcceptParams {
    pub link: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdentVerifyParams {
    pub contact_id: String,
    pub sas: String,
}

/// `file.send` — exactly one of `path` / `data` (base64). `kind` is
/// `file` | `image` | `voice`; `voice` requires `audio`. `client_xfer_id`
/// is the idempotency key, mirroring `client_msg_id`.
#[derive(Debug, Deserialize)]
pub struct FileSendParams {
    pub to: String,
    pub path: Option<String>,
    pub data: Option<String>,
    pub name: Option<String>,
    pub mime: Option<String>,
    pub kind: Option<String>,
    pub audio: Option<AudioMetaParams>,
    pub caption: Option<String>,
    pub client_xfer_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AudioMetaParams {
    pub duration_ms: u32,
    pub codec: String,
    #[serde(default)]
    pub sample_rate_hz: u32,
    #[serde(default)]
    pub channels: u8,
}

#[derive(Debug, Deserialize)]
pub struct FileIdParams {
    pub file_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FileListParams {
    pub chat_id: Option<String>,
    pub state: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileSaveParams {
    pub file_id: String,
    pub dir: Option<String>,
    pub name: Option<String>,
}

// ---------- Phase A voice calls (docs/plans/voice-video-calls.md §5) ----------

#[derive(Debug, Deserialize)]
pub struct CallRingParams {
    /// `contact:`/`peer:`/`chat:` id of the callee.
    pub to: String,
    /// Only "audio" exists in Phase A.
    pub media: Option<String>,
    /// Client-side idempotency key for the outbound ring.
    pub client_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallIdParams {
    pub call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallMuteParams {
    pub call_id: Option<String>,
    pub muted: Option<bool>,
}

/// Domain seam to the messenger core. One async call per RPC method; the
/// engine owns session.hello / rpc.describe / events.* / shutdown.request
/// but consults the core for identity, backlog counters, and shutdown.
#[async_trait]
pub trait CinderCore: Send + Sync {
    /// `{peer_id, display_name}` for session.hello's `account` object.
    async fn session_identity(&self) -> Result<Value, RpcError>;

    /// Returns `{msg_id, chat_id}` on acceptance; engine wraps with
    /// `state`/`deduped`.
    async fn msg_send(&self, params: MsgSendParams) -> Result<Value, RpcError>;
    async fn msg_edit(&self, params: MsgEditParams) -> Result<Value, RpcError>;
    async fn msg_delete(&self, params: MsgDeleteParams) -> Result<Value, RpcError>;
    async fn msg_react(&self, params: MsgReactParams) -> Result<Value, RpcError>;

    async fn chat_list(&self, params: ChatListParams) -> Result<Value, RpcError>;
    async fn chat_history(&self, params: ChatHistoryParams) -> Result<Value, RpcError>;
    /// Sets the local per-chat disappearing-message default; returns
    /// `{chat_id, ttl_ms}`. Local-only — not synced to the peer.
    async fn chat_set_disappearing(
        &self,
        params: ChatSetDisappearingParams,
    ) -> Result<Value, RpcError>;
    /// Hard-deletes a conversation locally and notifies the peer to wipe
    /// theirs when a live session exists. Returns `{chat_id, wiped,
    /// peer_notified}`; `peer_notified:false` means the peer keeps their
    /// copy (remote destruction is best-effort).
    async fn chat_wipe(&self, params: ChatWipeParams) -> Result<Value, RpcError>;

    async fn contact_list(&self, params: ContactListParams) -> Result<Value, RpcError>;
    async fn contact_add(&self, params: ContactAddParams) -> Result<Value, RpcError>;
    async fn contact_accept(&self, params: ContactIdParams) -> Result<Value, RpcError>;
    async fn contact_decline(&self, params: ContactIdParams) -> Result<Value, RpcError>;
    /// Permanently denylist the contact's peer-id: kills live calls and
    /// sessions, fails queued outbound `denied`, revokes grants, refuses
    /// every future session/envelope/re-add from that identity. There is
    /// no un-deny. Returns `{contact_id, peer_id, state:"denied"}`.
    async fn contact_deny(&self, params: ContactDenyParams) -> Result<Value, RpcError>;

    async fn invite_create(&self, params: InviteCreateParams) -> Result<Value, RpcError>;
    async fn invite_accept(&self, params: InviteAcceptParams) -> Result<Value, RpcError>;

    async fn ident_verify(&self, params: IdentVerifyParams) -> Result<Value, RpcError>;

    /// `{file_id, msg_id, chat_id, state, chunks_total, deduped}` on
    /// acceptance; the offer rides the existing outbound lane.
    async fn file_send(&self, params: FileSendParams) -> Result<Value, RpcError>;
    async fn file_accept(&self, params: FileIdParams) -> Result<Value, RpcError>;
    async fn file_decline(&self, params: FileIdParams) -> Result<Value, RpcError>;
    async fn file_cancel(&self, params: FileIdParams) -> Result<Value, RpcError>;
    async fn file_info(&self, params: FileIdParams) -> Result<Value, RpcError>;
    async fn file_list(&self, params: FileListParams) -> Result<Value, RpcError>;
    async fn file_save(&self, params: FileSaveParams) -> Result<Value, RpcError>;
    /// `{file_id, thumb_b64, mime}` — peer-controlled bytes; consumers MUST
    /// decode under the same size guards as `xfer::make_thumb`.
    async fn file_thumb(&self, params: FileIdParams) -> Result<Value, RpcError>;

    // ---------- Phase A voice calls ----------
    /// `{call_id, chat_id, state}` on acceptance. Call signalling rides the
    /// live Noise session only — never queued, never mailbox-deposited.
    async fn call_ring(&self, params: CallRingParams) -> Result<Value, RpcError>;
    async fn call_accept(&self, params: CallIdParams) -> Result<Value, RpcError>;
    async fn call_decline(&self, params: CallIdParams) -> Result<Value, RpcError>;
    async fn call_hangup(&self, params: CallIdParams) -> Result<Value, RpcError>;
    /// The live (or queried) call row: `{call_id, state, dir, peer_id,
    /// contact_id, chat_id, media, since_ts, quality, muted, stats}`.
    async fn call_info(&self, params: CallIdParams) -> Result<Value, RpcError>;
    /// Local-only mic gate — emits no wire message (§7).
    async fn call_mute(&self, params: CallMuteParams) -> Result<Value, RpcError>;

    /// `{unread_chats, pending_contact_requests, queued_outbound}` for the
    /// session.backlog summary.
    async fn backlog_summary(&self) -> Result<Value, RpcError>;

    /// Initiate core-side drain; the transport closes independently after.
    async fn shutdown(&self, grace_ms: u64) -> Result<(), RpcError>;
}

/// Event names the domain core may emit (engine-owned markers excluded).
pub const CORE_EVENT_KINDS: &[&str] = &[
    "message.received",
    "msg_state",
    "msg.expired",
    "contact.request",
    "contact.connected",
    "contact.denied",
    "chat.wiped",
    "security.identity_changed",
    "security.verification_needed",
    "file_state",
    "call.incoming",
    "call.state",
];

/// Push channel the core uses to report domain events. Emission assigns the
/// global monotonic `seq`, stamps `ts`, persists to the replay log, and fans
/// out to matching subscriptions — all without blocking the caller.
pub trait EventSink: Send + Sync {
    /// `kind` must be one of CORE_EVENT_KINDS; `chat_id` is required for
    /// kinds that honor subscription `chats` filters. `payload` carries the
    /// event-specific fields (envelope fields are added by the engine).
    fn emit(&self, kind: &str, chat_id: Option<&str>, payload: Value);
}
