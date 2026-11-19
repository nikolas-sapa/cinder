use anyhow::Result;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Unverified,
    Verified,
    KeyChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    Connecting,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgState {
    Queued,
    Sent,
    Delivered,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ChatSummary {
    pub chat_id: String,
    pub contact_id: String,
    pub peer_id: String,
    pub display_name: Option<String>,
    pub trust: Trust,
    pub unread: u32,
    pub last_activity: i64,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub msg_id: String,
    pub chat_id: String,
    pub dir: Dir,
    pub from: String,
    pub text: String,
    pub state: MsgState,
    pub ts: i64,
    /// unix-ms deadline for a disappearing message; `None` never expires.
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub has_more: bool,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SendAck {
    pub msg_id: String,
    pub client_msg_id: String,
    /// unix-ms deadline when the send carried a TTL; `None` otherwise.
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum Event {
    MessageReceived(Message),
    MsgState {
        msg_id: String,
        client_msg_id: String,
        chat_id: String,
        state: MsgState,
    },
    /// A disappearing message's deadline lapsed; drop it from the buffer.
    MsgExpired {
        chat_id: String,
        msg_id: String,
    },
    ContactConnected {
        contact_id: String,
        peer_id: String,
        chat_id: String,
        display_name: Option<String>,
        trust: Trust,
    },
    IdentityChanged {
        contact_id: String,
        chat_id: String,
    },
    VerificationNeeded {
        contact_id: String,
        sas: String,
    },
    ConnChanged(ConnState),
}

#[allow(async_fn_in_trait)]
pub trait Backend {
    fn peer_id(&self) -> &str;
    fn conn_state(&self) -> ConnState;
    async fn list_chats(&mut self) -> Result<Vec<ChatSummary>>;
    async fn history(
        &mut self,
        chat_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage>;
    async fn send(&mut self, chat_id: &str, body: &str) -> Result<SendAck>;
    /// Set the local per-chat disappearing default (ms); `0` disables.
    async fn set_disappearing(&mut self, chat_id: &str, ttl_ms: u64) -> Result<()>;
    async fn add_contact(&mut self, uri: &str) -> Result<String>;
    async fn verify_contact(&mut self, contact_id: &str, sas: &str) -> Result<()>;
    async fn sas_hint(&mut self, contact_id: &str) -> Result<String>;
    fn subscribe(&mut self) -> mpsc::Receiver<Event>;
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
