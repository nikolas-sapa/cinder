use crate::rpc::protocol::RpcError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct MsgSendParams {
    pub to: String,
    pub body: String,
    pub client_msg_id: String,
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

    async fn contact_list(&self, params: ContactListParams) -> Result<Value, RpcError>;
    async fn contact_add(&self, params: ContactAddParams) -> Result<Value, RpcError>;
    async fn contact_accept(&self, params: ContactIdParams) -> Result<Value, RpcError>;
    async fn contact_decline(&self, params: ContactIdParams) -> Result<Value, RpcError>;

    async fn invite_create(&self, params: InviteCreateParams) -> Result<Value, RpcError>;
    async fn invite_accept(&self, params: InviteAcceptParams) -> Result<Value, RpcError>;

    async fn ident_verify(&self, params: IdentVerifyParams) -> Result<Value, RpcError>;

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
    "contact.request",
    "contact.connected",
    "security.identity_changed",
    "security.verification_needed",
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
