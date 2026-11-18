use crate::server::{RelayConfig, TokenBucket};
use cinder_core::frame::wire_serialized_size;
use cinder_core::proto::{
    caps, DepositEnvelope, ErrorCode, InviteToken, MailboxId, PeerId, ServerMsg, StoredEnvelope,
    Token, TokenHash,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

/// §2 max transport ciphertext: u16 pt_len + 8192 padded pt + 16 B AEAD tag.
pub(crate) const MAX_ENVELOPE_BODY: usize = caps::MAX_PLAINTEXT + 18;

type Fail = (ErrorCode, &'static str);

struct Entry {
    env: StoredEnvelope,
    at: Instant,
}

/// The relay stores token HASHES only (§8): raw deposit/fetch tokens never
/// rest in this struct — they transit in request frames and are SHA-256'd
/// for comparison. Mailbox ids are client-chosen CSPRNG values; the relay
/// stores whatever it is handed (§9).
struct Mailbox {
    h_deposit: TokenHash,
    h_fetch: TokenHash,
    owner_ip: IpAddr,
    expires_at: Instant,
    next_msg_id: u64,
    bytes: usize,
    msgs: VecDeque<Entry>,
    deposit_bucket: TokenBucket,
}

impl Mailbox {
    fn expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }

    /// §9: eviction is whole-`session_id` atomic. A single expired envelope
    /// condemns its entire session batch — delivering a partial batch would
    /// desync the Noise nonce stream and brick the session anyway.
    fn evict_expired(&mut self, cfg: &RelayConfig, now: Instant) {
        let mut condemned = HashSet::new();
        for e in &self.msgs {
            if e.at + cfg.envelope_ttl <= now {
                condemned.insert(e.env.session_id);
            }
        }
        if condemned.is_empty() {
            return;
        }
        self.msgs.retain(|e| !condemned.contains(&e.env.session_id));
        self.bytes = self.msgs.iter().map(|e| e.env.body.len()).sum();
    }
}

/// §7 rendezvous slot: the token is lookup key AND secret combined — relay
/// stores it directly (§13 lists the token→owner mapping as relay-visible),
/// single-use, TTL-bound. Survives owner disconnect; an offline owner at
/// accept time means the forwarded Msg1 is dropped.
struct InviteSlot {
    owner: PeerId,
    owner_conn: u64,
    owner_ip: IpAddr,
    expires_at: Instant,
}

struct MailInner {
    boxes: HashMap<MailboxId, Mailbox>,
    // Raw invite tokens as map keys is deliberate — the rendezvous slot must
    // be lookup-able by the bearer token itself (PROTOCOL §13 accepts this
    // residual: a malicious relay can already deny/registrar-squat).
    invites: HashMap<InviteToken, InviteSlot>,
}

/// In-memory mailbox + invite store. The relay is stateless-restart by
/// design (BRIEF: mailbox loss on restart acceptable) — no persistence.
pub(crate) struct MailStore {
    inner: Mutex<MailInner>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// §8: SHA-256(presented) constant-time compared against the stored hash.
fn token_ok(presented: &Token, stored: &TokenHash) -> bool {
    let digest = Sha256::digest(presented.as_bytes());
    bool::from(digest[..].ct_eq(&stored.as_bytes()[..]))
}

fn live_box<'a>(inner: &'a mut MailInner, id: &MailboxId, now: Instant) -> Option<&'a mut Mailbox> {
    match inner.boxes.get(id) {
        Some(mb) if !mb.expired(now) => {}
        Some(_) => {
            inner.boxes.remove(id);
            return None;
        }
        None => return None,
    }
    inner.boxes.get_mut(id)
}

/// §5: one fetch produces a sequence of MailboxMessages frames. Whole
/// envelopes packed under MAX_FRAME_TRANSPORT; `more:false` terminates.
fn pack_messages(envs: Vec<StoredEnvelope>) -> Vec<ServerMsg> {
    let frame_cap = caps::MAX_FRAME_TRANSPORT as u64;
    let overhead = wire_serialized_size(&ServerMsg::MailboxMessages {
        msgs: Vec::new(),
        more: false,
    })
    .unwrap_or(13);
    let mut frames = Vec::new();
    let mut cur: Vec<StoredEnvelope> = Vec::new();
    let mut size = overhead;
    for env in envs {
        let sz = wire_serialized_size(&env).unwrap_or(env.body.len() as u64 + 72);
        if !cur.is_empty() && size + sz > frame_cap {
            frames.push(ServerMsg::MailboxMessages {
                msgs: std::mem::take(&mut cur),
                more: true,
            });
            size = overhead;
        }
        size += sz;
        cur.push(env);
    }
    frames.push(ServerMsg::MailboxMessages {
        msgs: cur,
        more: false,
    });
    frames
}

impl MailStore {
    pub(crate) fn new() -> Self {
        MailStore {
            inner: Mutex::new(MailInner {
                boxes: HashMap::new(),
                invites: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MailInner> {
        self.inner.lock().unwrap()
    }

    pub(crate) fn sweep(&self, cfg: &RelayConfig) {
        let now = Instant::now();
        let mut inner = self.lock();
        inner.boxes.retain(|_, mb| mb.expires_at > now);
        for mb in inner.boxes.values_mut() {
            mb.evict_expired(cfg, now);
        }
        inner.invites.retain(|_, s| s.expires_at > now);
    }

    pub(crate) fn open(
        &self,
        cfg: &RelayConfig,
        ip: IpAddr,
        mailbox_id: MailboxId,
        h_deposit: TokenHash,
        h_fetch: TokenHash,
        ttl_secs: u32,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        if let Some(mb) = inner.boxes.get(&mailbox_id) {
            if !mb.expired(now) {
                return Err((ErrorCode::MailboxExists, "mailbox id already registered"));
            }
            inner.boxes.remove(&mailbox_id);
        }
        let owned = inner
            .boxes
            .values()
            .filter(|mb| mb.owner_ip == ip && !mb.expired(now))
            .count();
        if owned >= cfg.mailboxes_per_ip {
            return Err((ErrorCode::RateLimited, "mailbox count per IP exceeded"));
        }
        let ttl = if ttl_secs == 0 {
            cfg.mailbox_default_ttl
        } else {
            std::time::Duration::from_secs(ttl_secs as u64).min(cfg.mailbox_max_ttl)
        };
        inner.boxes.insert(
            mailbox_id,
            Mailbox {
                h_deposit,
                h_fetch,
                owner_ip: ip,
                expires_at: now + ttl,
                next_msg_id: 1,
                bytes: 0,
                msgs: VecDeque::new(),
                deposit_bucket: TokenBucket::new(cfg.deposit_rate),
            },
        );
        Ok(vec![ServerMsg::MailboxOpened {
            mailbox_id,
            expires_at: now_unix() + ttl.as_secs().max(1),
        }])
    }

    pub(crate) fn deposit(
        &self,
        cfg: &RelayConfig,
        from: PeerId,
        mailbox_id: MailboxId,
        token: Token,
        env: DepositEnvelope,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        let Some(mb) = live_box(&mut inner, &mailbox_id, now) else {
            return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired"));
        };
        if !token_ok(&token, &mb.h_deposit) {
            return Err((ErrorCode::BadToken, "deposit token mismatch"));
        }
        if !mb.deposit_bucket.allow() {
            return Err((ErrorCode::RateLimited, "deposit rate exceeded"));
        }
        // §9: all-or-reject — never truncate, never evict to make room.
        if mb.msgs.len() >= cfg.mailbox_max_msgs
            || mb.bytes + env.body.len() > cfg.mailbox_max_bytes
        {
            return Err((ErrorCode::MailboxFull, "mailbox at capacity"));
        }
        let msg_id = mb.next_msg_id;
        mb.next_msg_id += 1;
        mb.bytes += env.body.len();
        mb.msgs.push_back(Entry {
            env: StoredEnvelope {
                msg_id,
                from,
                session_id: env.session_id,
                body: env.body,
                received_at: now_unix(),
            },
            at: now,
        });
        Ok(vec![ServerMsg::MailboxDeposited { msg_id }])
    }

    pub(crate) fn fetch(
        &self,
        cfg: &RelayConfig,
        mailbox_id: MailboxId,
        token: Token,
        max_msgs: u16,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        let Some(mb) = live_box(&mut inner, &mailbox_id, now) else {
            return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired"));
        };
        if !token_ok(&token, &mb.h_fetch) {
            return Err((ErrorCode::BadToken, "fetch token mismatch"));
        }
        mb.evict_expired(cfg, now);
        let envs: Vec<StoredEnvelope> = mb
            .msgs
            .iter()
            .take(max_msgs as usize)
            .map(|e| e.env.clone())
            .collect();
        Ok(pack_messages(envs))
    }

    pub(crate) fn ack(
        &self,
        mailbox_id: MailboxId,
        token: Token,
        msg_ids: Vec<u64>,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        let Some(mb) = live_box(&mut inner, &mailbox_id, now) else {
            return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired"));
        };
        if !token_ok(&token, &mb.h_fetch) {
            return Err((ErrorCode::BadToken, "fetch token mismatch"));
        }
        let ids: HashSet<u64> = msg_ids.into_iter().collect();
        mb.msgs.retain(|e| !ids.contains(&e.env.msg_id));
        mb.bytes = mb.msgs.iter().map(|e| e.env.body.len()).sum();
        Ok(vec![ServerMsg::Ok])
    }

    /// §8: atomic swap of both hashes; old tokens die immediately.
    pub(crate) fn rotate(
        &self,
        mailbox_id: MailboxId,
        token: Token,
        new_h_deposit: TokenHash,
        new_h_fetch: TokenHash,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        let Some(mb) = live_box(&mut inner, &mailbox_id, now) else {
            return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired"));
        };
        if !token_ok(&token, &mb.h_fetch) {
            return Err((ErrorCode::BadToken, "fetch token mismatch"));
        }
        mb.h_deposit = new_h_deposit;
        mb.h_fetch = new_h_fetch;
        Ok(vec![ServerMsg::Ok])
    }

    pub(crate) fn close(
        &self,
        mailbox_id: MailboxId,
        token: Token,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        match inner.boxes.get(&mailbox_id) {
            Some(mb) if !mb.expired(now) => {
                if !token_ok(&token, &mb.h_fetch) {
                    return Err((ErrorCode::BadToken, "fetch token mismatch"));
                }
            }
            Some(_) => {
                inner.boxes.remove(&mailbox_id);
                return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired"));
            }
            None => return Err((ErrorCode::UnknownMailbox, "mailbox unknown or expired")),
        }
        inner.boxes.remove(&mailbox_id);
        Ok(vec![ServerMsg::Ok])
    }

    pub(crate) fn invite_create(
        &self,
        cfg: &RelayConfig,
        conn_id: u64,
        ip: IpAddr,
        owner: PeerId,
        token: InviteToken,
        ttl_secs: u32,
    ) -> Result<Vec<ServerMsg>, Fail> {
        let mut inner = self.lock();
        let now = Instant::now();
        if let Some(s) = inner.invites.get(&token) {
            if s.expires_at > now {
                return Err((ErrorCode::InviteInvalid, "invite token already registered"));
            }
            inner.invites.remove(&token);
        }
        let per_conn = inner
            .invites
            .values()
            .filter(|s| s.owner_conn == conn_id && s.expires_at > now)
            .count();
        let per_ip = inner
            .invites
            .values()
            .filter(|s| s.owner_ip == ip && s.expires_at > now)
            .count();
        if per_conn >= cfg.invites_per_conn || per_ip >= cfg.invites_per_ip {
            return Err((ErrorCode::RateLimited, "invite slot limit exceeded"));
        }
        let ttl = if ttl_secs == 0 {
            cfg.invite_default_ttl
        } else {
            std::time::Duration::from_secs(ttl_secs as u64).min(cfg.invite_max_ttl)
        };
        inner.invites.insert(
            token,
            InviteSlot {
                owner,
                owner_conn: conn_id,
                owner_ip: ip,
                expires_at: now + ttl,
            },
        );
        Ok(vec![ServerMsg::InviteCreated {
            token,
            expires_at: now_unix() + ttl.as_secs().max(1),
        }])
    }

    /// §7: single-use atomic consume. Returns the slot owner's peer-id.
    pub(crate) fn invite_consume(&self, token: &InviteToken) -> Option<PeerId> {
        let mut inner = self.lock();
        match inner.invites.get(token) {
            Some(s) if s.expires_at > Instant::now() => {
                let owner = s.owner;
                inner.invites.remove(token);
                Some(owner)
            }
            Some(_) => {
                inner.invites.remove(token);
                None
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_ids_are_unguessable() {
        let mut ids = HashSet::new();
        let peers: HashSet<[u8; 32]> = (0..100).map(|_| *PeerId::random().as_bytes()).collect();
        for _ in 0..100 {
            let id = MailboxId::random();
            assert_eq!(id.as_bytes().len(), caps::MAILBOX_ID_LEN);
            assert!(!peers.contains(id.as_bytes()));
            assert!(ids.insert(*id.as_bytes()));
        }
    }

    #[test]
    fn pack_messages_never_splits_envelopes() {
        let env = |n: u8| StoredEnvelope {
            msg_id: n as u64,
            from: PeerId::random(),
            session_id: cinder_core::proto::SessionId::random(),
            body: vec![n; 7000],
            received_at: 0,
        };
        let frames = pack_messages((0..5).map(env).collect());
        assert!(frames.len() >= 3);
        for (i, f) in frames.iter().enumerate() {
            let ServerMsg::MailboxMessages { msgs, more } = f else {
                panic!("wrong variant")
            };
            assert!(wire_serialized_size(f).unwrap() <= caps::MAX_FRAME_TRANSPORT as u64);
            assert!(!msgs.is_empty());
            assert_eq!(*more, i + 1 < frames.len());
        }
    }
}
