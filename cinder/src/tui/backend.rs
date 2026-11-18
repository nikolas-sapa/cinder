use anyhow::Result;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactState {
    PendingOut,
    PendingIn,
    Active,
    Declined,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct Contact {
    pub contact_id: String,
    pub peer_id: String,
    pub display_name: Option<String>,
    pub state: ContactState,
    pub trust: Trust,
    pub chat_id: Option<String>,
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
    ContactConnected {
        contact_id: String,
        peer_id: String,
        chat_id: String,
        display_name: Option<String>,
        trust: Trust,
    },
    IdentityChanged {
        contact_id: String,
        peer_id: String,
        chat_id: String,
    },
    VerificationNeeded {
        contact_id: String,
        peer_id: String,
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
    async fn contacts(&mut self) -> Result<Vec<Contact>>;
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

const DEMO_LINES: &[&str] = &[
    "relay is blind; all it sees is ciphertext",
    "peer-id pinned after handshake, no silent rebind",
    "meet at the usual place, 0400",
    "the tape generates the demo gif in CI",
    "padding to next power of two, 256 minimum",
    "SAS check out-of-band when you can",
    "mailbox deposit is sent, not delivered",
    "read receipts are a v2 question, not v1",
    "frames are len-prefixed, validated before alloc",
    "decrypt failure is fatal, never retry-skip",
];

pub struct MockBackend {
    peer_id: String,
    conn: ConnState,
    chats: Vec<ChatSummary>,
    contacts: Vec<Contact>,
    store: HashMap<String, Vec<Message>>,
    event_tx: mpsc::Sender<Event>,
    event_rx: Option<mpsc::Receiver<Event>>,
    send_ctr: u64,
}

impl MockBackend {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        let now = now_ts();
        let peers = [
            (
                "peer:oyv4kqz3n5m2w7j1h8",
                "eidolon",
                Trust::Verified,
                35,
                2u32,
            ),
            (
                "peer:x9w2kvn4m7q1c8d3f5g",
                "murmur",
                Trust::Unverified,
                20,
                0,
            ),
            (
                "peer:z4mkp8r2t6y0b3n7q9w",
                "nightjar",
                Trust::KeyChanged,
                7,
                5,
            ),
        ];
        let mut chats = Vec::new();
        let mut contacts = Vec::new();
        let mut store = HashMap::new();
        for (i, (peer_id, name, trust, count, unread)) in peers.iter().enumerate() {
            let chat_id = format!("chat:{:08x}", i + 1);
            let contact_id = format!("contact:{:08x}", i + 1);
            let mut msgs = Vec::with_capacity(*count);
            for j in 0..*count {
                let out = (i + j) % 3 == 0;
                let mut text =
                    format!("{} ({})", DEMO_LINES[(i * 7 + j) % DEMO_LINES.len()], j + 1);
                if i == 0 && j == count - 1 {
                    text.push_str(" \x1b]8;;https://example.invalid\x07link\x1b[2J");
                }
                msgs.push(Message {
                    msg_id: format!("msg:{:04x}{:04x}", i, j),
                    chat_id: chat_id.clone(),
                    dir: if out { Dir::Out } else { Dir::In },
                    from: if out {
                        "peer:self0000000000000".to_string()
                    } else {
                        peer_id.to_string()
                    },
                    text,
                    state: MsgState::Delivered,
                    ts: now - ((*count - j) as i64) * 97,
                });
            }
            let last = msgs.last().cloned();
            store.insert(chat_id.clone(), msgs);
            chats.push(ChatSummary {
                chat_id: chat_id.clone(),
                contact_id: contact_id.clone(),
                peer_id: peer_id.to_string(),
                display_name: Some(name.to_string()),
                trust: *trust,
                unread: *unread,
                last_activity: last.as_ref().map(|m| m.ts).unwrap_or(now),
                last_message: last.map(|m| m.text),
            });
            contacts.push(Contact {
                contact_id,
                peer_id: peer_id.to_string(),
                display_name: Some(name.to_string()),
                state: ContactState::Active,
                trust: *trust,
                chat_id: Some(chat_id),
            });
        }
        chats.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        Self {
            peer_id: "peer:self0000000000000".to_string(),
            conn: ConnState::Connected,
            chats,
            contacts,
            store,
            event_tx,
            event_rx: Some(event_rx),
            send_ctr: 0,
        }
    }
}

impl Backend for MockBackend {
    fn peer_id(&self) -> &str {
        &self.peer_id
    }

    fn conn_state(&self) -> ConnState {
        self.conn
    }

    async fn list_chats(&mut self) -> Result<Vec<ChatSummary>> {
        Ok(self.chats.clone())
    }

    async fn history(
        &mut self,
        chat_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage> {
        let all = self
            .store
            .get(chat_id)
            .ok_or_else(|| anyhow::anyhow!("unknown chat {chat_id}"))?;
        let end = match before {
            Some(cur) => cur
                .strip_prefix("cur:")
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| anyhow::anyhow!("bad cursor"))?,
            None => all.len(),
        };
        let start = end.saturating_sub(limit);
        let mut messages: Vec<Message> = all[start..end].to_vec();
        messages.reverse();
        Ok(HistoryPage {
            messages,
            has_more: start > 0,
            next_before: (start > 0).then(|| format!("cur:{start}")),
        })
    }

    async fn send(&mut self, chat_id: &str, body: &str) -> Result<SendAck> {
        let chat = self
            .chats
            .iter_mut()
            .find(|c| c.chat_id == chat_id)
            .ok_or_else(|| anyhow::anyhow!("unknown chat {chat_id}"))?;
        if chat.trust == Trust::KeyChanged {
            anyhow::bail!("key_changed_blocked");
        }
        self.send_ctr += 1;
        let ack = SendAck {
            msg_id: format!("msg:local{:04x}", self.send_ctr),
            client_msg_id: format!("tui-{:04x}", self.send_ctr),
        };
        let msg = Message {
            msg_id: ack.msg_id.clone(),
            chat_id: chat_id.to_string(),
            dir: Dir::Out,
            from: self.peer_id.clone(),
            text: body.to_string(),
            state: MsgState::Queued,
            ts: now_ts(),
        };
        chat.last_activity = msg.ts;
        chat.last_message = Some(body.to_string());
        self.store.entry(chat_id.to_string()).or_default().push(msg);
        let (tx, mid, cmid, cid) = (
            self.event_tx.clone(),
            ack.msg_id.clone(),
            ack.client_msg_id.clone(),
            chat_id.to_string(),
        );
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = tx
                .send(Event::MsgState {
                    msg_id: mid.clone(),
                    client_msg_id: cmid.clone(),
                    chat_id: cid.clone(),
                    state: MsgState::Sent,
                })
                .await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            let _ = tx
                .send(Event::MsgState {
                    msg_id: mid,
                    client_msg_id: cmid,
                    chat_id: cid,
                    state: MsgState::Delivered,
                })
                .await;
        });
        Ok(ack)
    }

    async fn contacts(&mut self) -> Result<Vec<Contact>> {
        Ok(self.contacts.clone())
    }

    async fn add_contact(&mut self, uri: &str) -> Result<String> {
        if !uri.starts_with("cinder://") {
            anyhow::bail!("malformed link: expected cinder://<pubkey>@<relay>");
        }
        let n = self.contacts.len() + 1;
        let contact_id = format!("contact:{n:08x}");
        let chat_id = format!("chat:{n:08x}");
        let peer_id = format!("peer:new{n:014x}");
        let tx = self.event_tx.clone();
        let (cid, pid, chid) = (contact_id.clone(), peer_id.clone(), chat_id.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let _ = tx
                .send(Event::ContactConnected {
                    contact_id: cid,
                    peer_id: pid,
                    chat_id: chid,
                    display_name: None,
                    trust: Trust::Unverified,
                })
                .await;
        });
        self.contacts.push(Contact {
            contact_id: contact_id.clone(),
            peer_id,
            display_name: None,
            state: ContactState::PendingOut,
            trust: Trust::Unverified,
            chat_id: Some(chat_id),
        });
        Ok(contact_id)
    }

    async fn verify_contact(&mut self, contact_id: &str, sas: &str) -> Result<()> {
        if sas.trim().is_empty() {
            anyhow::bail!("sas required");
        }
        let contact = self
            .contacts
            .iter_mut()
            .find(|c| c.contact_id == contact_id)
            .ok_or_else(|| anyhow::anyhow!("unknown contact"))?;
        contact.trust = Trust::Verified;
        if let Some(chat_id) = &contact.chat_id {
            if let Some(chat) = self.chats.iter_mut().find(|c| &c.chat_id == chat_id) {
                chat.trust = Trust::Verified;
            }
        }
        Ok(())
    }

    async fn sas_hint(&mut self, contact_id: &str) -> Result<String> {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in contact_id.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        let digits = (0..5)
            .map(|_| {
                h ^= h << 13;
                h ^= h >> 7;
                h ^= h << 17;
                format!("{:02}", h % 100)
            })
            .collect::<Vec<_>>()
            .join("-");
        Ok(digits)
    }

    fn subscribe(&mut self) -> mpsc::Receiver<Event> {
        self.event_rx
            .take()
            .expect("subscribe called more than once")
    }
}
