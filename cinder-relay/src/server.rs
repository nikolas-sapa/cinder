use cinder_core::frame::FrameCodec;
use cinder_core::proto::{
    caps, ClientMsg, ErrorCode, HandshakeMsg, HsId, PeerId, ServerMsg, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::{sleep, sleep_until, timeout, Instant as TokioInstant};

/// Token-bucket rate limit: `burst` tokens, refilled at `count` per `per`.
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    pub count: u32,
    pub per: Duration,
    pub burst: u32,
}

pub(crate) struct TokenBucket {
    tokens: f64,
    last: Instant,
    capacity: f64,
    refill_per_sec: f64,
}

impl TokenBucket {
    pub(crate) fn new(limit: RateLimit) -> Self {
        TokenBucket {
            tokens: limit.burst as f64,
            last: Instant::now(),
            capacity: limit.burst.max(1) as f64,
            refill_per_sec: limit.count as f64 / limit.per.as_secs_f64(),
        }
    }

    pub(crate) fn allow(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f64() * self.refill_per_sec;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// §12 limits, all injectable for tests.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// New TCP connections per IP (20/min).
    pub connect_rate: RateLimit,
    /// Handshake initiations: InviteAccept + Handshake{Msg1} (30/60s, burst 10).
    pub handshake_rate: RateLimit,
    /// Post-Hello inbound frames per connection (120/s).
    pub transport_rate: RateLimit,
    /// PresenceSubscribe frames per connection (4/min).
    pub presence_rate: RateLimit,
    /// Pending handshakes per IP (8).
    pub pending_hs_per_ip: usize,
    /// Pending handshakes global (512).
    pub pending_hs_global: usize,
    /// Pending-handshake TTL (10 s).
    pub handshake_timeout: Duration,
    /// Outbound queue cap per connection (256), whole-session-batch shed.
    pub outbound_queue: usize,
    /// Idle close (120 s without any inbound frame).
    pub idle_timeout: Duration,
    /// Presence subscription set cap (256).
    pub presence_max_peers: usize,
    /// RateLimited strikes within 60 s before the connection closes (5).
    pub rate_strikes: usize,
    /// Pending-handshake sweep cadence (internal).
    pub sweep_interval: Duration,
    /// Prune per-IP rate state idle this long with no pendings (internal).
    pub ip_idle_prune: Duration,
    /// BadToken strikes within 60 s per IP before the connection closes (10).
    pub bad_token_strikes: usize,
    /// Mailboxes per IP (64).
    pub mailboxes_per_ip: usize,
    /// Invite slots per connection (8).
    pub invites_per_conn: usize,
    /// Invite slots per IP (64).
    pub invites_per_ip: usize,
    /// Per-mailbox message cap (500).
    pub mailbox_max_msgs: usize,
    /// Per-mailbox stored-bytes cap (4 MiB).
    pub mailbox_max_bytes: usize,
    /// Mailbox TTL when ttl_secs == 0 (7 d).
    pub mailbox_default_ttl: Duration,
    /// Mailbox TTL upper clamp (30 d).
    pub mailbox_max_ttl: Duration,
    /// Per-envelope retention cap (30 d, §9/R7).
    pub envelope_ttl: Duration,
    /// Deposits per mailbox (60/min).
    pub deposit_rate: RateLimit,
    /// Invite TTL when ttl_secs == 0 (24 h).
    pub invite_default_ttl: Duration,
    /// Invite TTL upper clamp (72 h).
    pub invite_max_ttl: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig {
            connect_rate: RateLimit {
                count: 20,
                per: Duration::from_secs(60),
                burst: 20,
            },
            handshake_rate: RateLimit {
                count: 30,
                per: Duration::from_secs(60),
                burst: 10,
            },
            transport_rate: RateLimit {
                count: 120,
                per: Duration::from_secs(1),
                burst: 120,
            },
            presence_rate: RateLimit {
                count: 4,
                per: Duration::from_secs(60),
                burst: 4,
            },
            pending_hs_per_ip: 8,
            pending_hs_global: 512,
            handshake_timeout: caps::HANDSHAKE_TIMEOUT,
            outbound_queue: 256,
            idle_timeout: Duration::from_secs(120),
            presence_max_peers: 256,
            rate_strikes: 5,
            sweep_interval: Duration::from_millis(250),
            ip_idle_prune: Duration::from_secs(600),
            bad_token_strikes: 10,
            mailboxes_per_ip: 64,
            invites_per_conn: 8,
            invites_per_ip: 64,
            mailbox_max_msgs: caps::MAILBOX_MAX_MSGS,
            mailbox_max_bytes: caps::MAILBOX_MAX_BYTES,
            mailbox_default_ttl: Duration::from_secs(7 * 24 * 3600),
            mailbox_max_ttl: Duration::from_secs(caps::MAILBOX_MAX_TTL as u64),
            envelope_ttl: Duration::from_secs(caps::MAILBOX_MAX_TTL as u64),
            deposit_rate: RateLimit {
                count: 60,
                per: Duration::from_secs(60),
                burst: 60,
            },
            invite_default_ttl: Duration::from_secs(24 * 3600),
            invite_max_ttl: Duration::from_secs(caps::INVITE_MAX_TTL as u64),
        }
    }
}

enum Push {
    Queued,
    Shed,
    Closed,
}

struct OutInner {
    q: VecDeque<ServerMsg>,
    closed: bool,
}

struct OutboundQueue {
    inner: Mutex<OutInner>,
    notify: Notify,
    cap: usize,
}

fn transport_sid(msg: &ServerMsg) -> Option<cinder_core::proto::SessionId> {
    match msg {
        ServerMsg::Transport { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

impl OutboundQueue {
    fn new(cap: usize) -> Self {
        OutboundQueue {
            inner: Mutex::new(OutInner {
                q: VecDeque::new(),
                closed: false,
            }),
            notify: Notify::new(),
            cap,
        }
    }

    /// §12 shed policy: on a full queue drop whole `session_id` batches first
    /// (a dropped frame bricks its session's nonce stream anyway, so every
    /// queued frame of a dropped batch is dead weight). Prefer sacrificing
    /// another session's batch over the incoming frame's; if nothing is
    /// droppable, close the slow consumer.
    fn push(&self, msg: ServerMsg) -> Push {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Push::Closed;
        }
        if inner.q.len() < self.cap {
            inner.q.push_back(msg);
            drop(inner);
            self.notify.notify_one();
            return Push::Queued;
        }
        let incoming_sid = transport_sid(&msg);
        let victim = inner
            .q
            .iter()
            .find_map(|m| transport_sid(m).filter(|sid| Some(*sid) != incoming_sid));
        if let Some(victim) = victim {
            inner.q.retain(|m| transport_sid(m) != Some(victim));
            inner.q.push_back(msg);
            drop(inner);
            self.notify.notify_one();
            Push::Shed
        } else if let Some(sid) = incoming_sid {
            inner.q.retain(|m| transport_sid(m) != Some(sid));
            drop(inner);
            Push::Shed
        } else {
            inner.closed = true;
            drop(inner);
            self.notify.notify_one();
            Push::Closed
        }
    }

    /// PeerStatus is resyncable state, not stream data: it may be dropped
    /// individually under pressure without violating whole-batch atomicity.
    fn push_lossy(&self, msg: ServerMsg) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed || inner.q.len() >= self.cap {
            return;
        }
        inner.q.push_back(msg);
        drop(inner);
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<ServerMsg> {
        self.inner.lock().unwrap().q.pop_front()
    }

    fn is_dead(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.closed && inner.q.is_empty()
    }

    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.q.clear();
        drop(inner);
        self.notify.notify_one();
    }
}

struct IpState {
    conn_bucket: TokenBucket,
    hs_bucket: TokenBucket,
    pending_hs: usize,
    bad_tokens: VecDeque<Instant>,
    last_seen: Instant,
}

impl IpState {
    fn new(cfg: &RelayConfig) -> Self {
        IpState {
            conn_bucket: TokenBucket::new(cfg.connect_rate),
            hs_bucket: TokenBucket::new(cfg.handshake_rate),
            pending_hs: 0,
            bad_tokens: VecDeque::new(),
            last_seen: Instant::now(),
        }
    }
}

struct Pending {
    initiator: Arc<OutboundQueue>,
    ip: IpAddr,
    at: Instant,
}

struct ConnState {
    out: Arc<OutboundQueue>,
    subscribed: Mutex<HashSet<PeerId>>,
}

/// Lock order wherever two maps are held at once: pending → ips, subs →
/// conns; the mail store is a leaf (mailbox.rs never takes relay locks).
struct RelayState {
    cfg: RelayConfig,
    peers: Mutex<HashMap<PeerId, Arc<OutboundQueue>>>,
    conns: Mutex<HashMap<u64, Arc<ConnState>>>,
    subs: Mutex<HashMap<PeerId, HashSet<u64>>>,
    pending: Mutex<HashMap<HsId, Pending>>,
    ips: Mutex<HashMap<IpAddr, IpState>>,
    mail: crate::mailbox::MailStore,
}

impl RelayState {
    fn new(cfg: RelayConfig) -> Self {
        RelayState {
            cfg,
            peers: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            ips: Mutex::new(HashMap::new()),
            mail: crate::mailbox::MailStore::new(),
        }
    }

    fn forward(&self, to: &PeerId, msg: ServerMsg) {
        let q = self.peers.lock().unwrap().get(to).cloned();
        if let Some(q) = q {
            q.push(msg);
        }
    }

    fn broadcast_status(&self, peer: PeerId, online: bool) {
        let subs = self.subs.lock().unwrap();
        let Some(ids) = subs.get(&peer) else { return };
        let conns = self.conns.lock().unwrap();
        for id in ids {
            if let Some(c) = conns.get(id) {
                c.out.push_lossy(ServerMsg::PeerStatus { peer, online });
            }
        }
    }
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn serve(listener: TcpListener, config: RelayConfig) -> std::io::Result<()> {
    let state = Arc::new(RelayState::new(config));
    {
        let state = state.clone();
        tokio::spawn(async move { sweeper(state).await });
    }
    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            handle_conn(state, stream, addr).await;
        });
    }
}

async fn sweeper(state: Arc<RelayState>) {
    loop {
        sleep(state.cfg.sweep_interval).await;
        state.mail.sweep(&state.cfg);
        let now = Instant::now();
        let mut expired = Vec::new();
        {
            let mut pending = state.pending.lock().unwrap();
            let mut ips = state.ips.lock().unwrap();
            pending.retain(|_, p| {
                if now.duration_since(p.at) >= state.cfg.handshake_timeout {
                    if let Some(e) = ips.get_mut(&p.ip) {
                        e.pending_hs = e.pending_hs.saturating_sub(1);
                    }
                    expired.push(p.initiator.clone());
                    false
                } else {
                    true
                }
            });
            ips.retain(|_, e| {
                e.pending_hs > 0 || now.duration_since(e.last_seen) < state.cfg.ip_idle_prune
            });
        }
        for initiator in expired {
            initiator.push(ServerMsg::err(
                ErrorCode::HandshakeTimeout,
                "handshake attempt expired",
            ));
        }
    }
}

enum Act {
    Keep,
    SendErrorThenClose(ErrorCode, &'static str),
    /// Write these frames straight to the socket, bypassing the outbound
    /// queue: a full-mailbox fetch is ~260 spec-legal frames which must not
    /// trip the 256-frame shed cap nor split across interleaved forwards.
    SendNow(Vec<ServerMsg>),
}

struct Conn {
    id: u64,
    peer_id: PeerId,
    ip: IpAddr,
    out: Arc<OutboundQueue>,
    strikes: VecDeque<Instant>,
    transport_bucket: TokenBucket,
    presence_bucket: TokenBucket,
}

impl Conn {
    fn strike(&mut self, state: &RelayState, detail: &'static str) -> Act {
        let now = Instant::now();
        self.strikes.push_back(now);
        while self
            .strikes
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            self.strikes.pop_front();
        }
        if self.strikes.len() >= state.cfg.rate_strikes {
            Act::SendErrorThenClose(ErrorCode::RateLimited, "rate limit strikes exceeded")
        } else {
            self.out
                .push(ServerMsg::err(ErrorCode::RateLimited, detail));
            Act::Keep
        }
    }
}

fn register_ip_conn(state: &RelayState, ip: IpAddr) -> bool {
    let mut ips = state.ips.lock().unwrap();
    let e = ips.entry(ip).or_insert_with(|| IpState::new(&state.cfg));
    e.last_seen = Instant::now();
    e.conn_bucket.allow()
}

async fn handle_conn(state: Arc<RelayState>, stream: TcpStream, addr: SocketAddr) {
    let ip = addr.ip();
    let (rh, wh) = stream.into_split();
    let mut writer = tokio_util::codec::FramedWrite::new(wh, FrameCodec::<ServerMsg>::transport());
    let mut reader = tokio_util::codec::FramedRead::new(rh, FrameCodec::<ClientMsg>::pre_hello());

    if !register_ip_conn(&state, ip) {
        let _ = writer
            .send(ServerMsg::err(
                ErrorCode::RateLimited,
                "connection rate exceeded",
            ))
            .await;
        let _ = writer.flush().await;
        return;
    }

    let hello = match timeout(state.cfg.idle_timeout, reader.next()).await {
        Err(_) => return,
        Ok(None) => return,
        Ok(Some(Err(e))) => {
            let _ = writer.send(ServerMsg::err(e.code(), "invalid frame")).await;
            let _ = writer.flush().await;
            return;
        }
        Ok(Some(Ok(msg))) => msg,
    };

    let peer_id = match hello {
        ClientMsg::Hello { version, peer_id } => {
            if version != PROTOCOL_VERSION {
                let _ = writer
                    .send(ServerMsg::err(
                        ErrorCode::VersionMismatch,
                        "unsupported protocol version",
                    ))
                    .await;
                let _ = writer.flush().await;
                return;
            }
            peer_id
        }
        _ => {
            let _ = writer
                .send(ServerMsg::err(
                    ErrorCode::ProtocolViolation,
                    "first frame must be Hello",
                ))
                .await;
            let _ = writer.flush().await;
            return;
        }
    };

    let out = Arc::new(OutboundQueue::new(state.cfg.outbound_queue));
    let registered = {
        match state.peers.lock().unwrap().entry(peer_id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(out.clone());
                true
            }
        }
    };
    if !registered {
        let _ = writer
            .send(ServerMsg::err(
                ErrorCode::PeerIdInUse,
                "peer-id already connected",
            ))
            .await;
        let _ = writer.flush().await;
        return;
    }

    reader
        .decoder_mut()
        .set_max_frame(caps::MAX_FRAME_TRANSPORT as u32);

    if writer
        .send(ServerMsg::ServerHello {
            version: PROTOCOL_VERSION,
            now_unix: now_unix(),
        })
        .await
        .is_err()
    {
        state.peers.lock().unwrap().remove(&peer_id);
        return;
    }

    let conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let conn_state = Arc::new(ConnState {
        out: out.clone(),
        subscribed: Mutex::new(HashSet::new()),
    });
    state
        .conns
        .lock()
        .unwrap()
        .insert(conn_id, conn_state.clone());
    state.broadcast_status(peer_id, true);

    let mut conn = Conn {
        id: conn_id,
        peer_id,
        ip,
        out: out.clone(),
        strikes: VecDeque::new(),
        transport_bucket: TokenBucket::new(state.cfg.transport_rate),
        presence_bucket: TokenBucket::new(state.cfg.presence_rate),
    };

    let mut last_frame = Instant::now();
    loop {
        let idle_at = TokioInstant::from_std(last_frame + state.cfg.idle_timeout);
        let act = tokio::select! {
            biased;
            _ = out.notify.notified() => {
                let mut io_err = false;
                while let Some(msg) = out.pop() {
                    if writer.feed(msg).await.is_err() {
                        io_err = true;
                        break;
                    }
                }
                if io_err || writer.flush().await.is_err() || out.is_dead() {
                    break;
                }
                Act::Keep
            }
            frame = reader.next() => match frame {
                None => break,
                Some(Err(e)) => {
                    let _ = writer.send(ServerMsg::err(e.code(), "invalid frame")).await;
                    let _ = writer.flush().await;
                    break;
                }
                Some(Ok(msg)) => {
                    last_frame = Instant::now();
                    dispatch(&state, &mut conn, &conn_state, msg)
                }
            },
            _ = sleep_until(idle_at) => break,
        };
        match act {
            Act::Keep => {}
            Act::SendErrorThenClose(code, detail) => {
                let _ = writer.send(ServerMsg::err(code, detail)).await;
                let _ = writer.flush().await;
                break;
            }
            Act::SendNow(msgs) => {
                // Drain queued outbound first so reply ordering is causal.
                let mut io_err = false;
                while let Some(m) = out.pop() {
                    if writer.feed(m).await.is_err() {
                        io_err = true;
                        break;
                    }
                }
                if !io_err {
                    for m in msgs {
                        if writer.feed(m).await.is_err() {
                            io_err = true;
                            break;
                        }
                    }
                }
                if io_err || writer.flush().await.is_err() || out.is_dead() {
                    break;
                }
            }
        }
    }

    cleanup(&state, &conn);
}

fn cleanup(state: &RelayState, conn: &Conn) {
    {
        let mut pending = state.pending.lock().unwrap();
        let mut ips = state.ips.lock().unwrap();
        pending.retain(|_, p| {
            if Arc::ptr_eq(&p.initiator, &conn.out) {
                if let Some(e) = ips.get_mut(&p.ip) {
                    e.pending_hs = e.pending_hs.saturating_sub(1);
                }
                false
            } else {
                true
            }
        });
    }
    {
        let mut peers = state.peers.lock().unwrap();
        if peers
            .get(&conn.peer_id)
            .is_some_and(|q| Arc::ptr_eq(q, &conn.out))
        {
            peers.remove(&conn.peer_id);
        }
    }
    {
        let mut subs = state.subs.lock().unwrap();
        let mut conns = state.conns.lock().unwrap();
        if let Some(cs) = conns.remove(&conn.id) {
            for p in cs.subscribed.lock().unwrap().iter() {
                if let Some(set) = subs.get_mut(p) {
                    set.remove(&conn.id);
                }
            }
        }
    }
    state.broadcast_status(conn.peer_id, false);
    conn.out.close();
}

/// §12: a handshake initiation (Handshake{Msg1} or InviteAccept) consumes
/// per-IP rate budget and occupies a pending_hs slot, registered atomically
/// under the same lock pair as the cap check.
fn admit_initiation(state: &RelayState, conn: &Conn, hs_id: HsId) -> bool {
    let mut pending = state.pending.lock().unwrap();
    let mut ips = state.ips.lock().unwrap();
    let e = ips
        .entry(conn.ip)
        .or_insert_with(|| IpState::new(&state.cfg));
    e.last_seen = Instant::now();
    let under_caps =
        e.pending_hs < state.cfg.pending_hs_per_ip && pending.len() < state.cfg.pending_hs_global;
    if !(e.hs_bucket.allow() && under_caps) {
        return false;
    }
    e.pending_hs += 1;
    pending.insert(
        hs_id,
        Pending {
            initiator: conn.out.clone(),
            ip: conn.ip,
            at: Instant::now(),
        },
    );
    true
}

/// Roll back a pending_hs registration when an admitted initiation fails
/// before any forward (e.g. InviteAccept with a dead token).
fn abort_initiation(state: &RelayState, conn: &Conn, hs_id: HsId) {
    let mut pending = state.pending.lock().unwrap();
    let Some(p) = pending.remove(&hs_id) else {
        return;
    };
    if Arc::ptr_eq(&p.initiator, &conn.out) {
        let mut ips = state.ips.lock().unwrap();
        if let Some(e) = ips.get_mut(&p.ip) {
            e.pending_hs = e.pending_hs.saturating_sub(1);
        }
    } else {
        pending.insert(hs_id, p);
    }
}

/// §10: BadToken stays open until ≥10 strikes/60 s per IP, then close.
fn bad_token(state: &RelayState, conn: &mut Conn, detail: &'static str) -> Act {
    let close = {
        let mut ips = state.ips.lock().unwrap();
        let e = ips
            .entry(conn.ip)
            .or_insert_with(|| IpState::new(&state.cfg));
        e.last_seen = Instant::now();
        let now = Instant::now();
        e.bad_tokens.push_back(now);
        while e
            .bad_tokens
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            e.bad_tokens.pop_front();
        }
        e.bad_tokens.len() >= state.cfg.bad_token_strikes
    };
    if close {
        Act::SendErrorThenClose(ErrorCode::BadToken, "token strikes exceeded")
    } else {
        conn.out.push(ServerMsg::err(ErrorCode::BadToken, detail));
        Act::Keep
    }
}

fn mail_err(state: &RelayState, conn: &mut Conn, code: ErrorCode, detail: &'static str) -> Act {
    match code {
        ErrorCode::BadToken => bad_token(state, conn, detail),
        ErrorCode::RateLimited => conn.strike(state, detail),
        _ => {
            conn.out.push(ServerMsg::err(code, detail));
            Act::Keep
        }
    }
}

fn push_msgs(conn: &Conn, msgs: Vec<ServerMsg>) -> Act {
    for m in msgs {
        conn.out.push(m);
    }
    Act::Keep
}

fn dispatch(state: &Arc<RelayState>, conn: &mut Conn, cs: &Arc<ConnState>, msg: ClientMsg) -> Act {
    if !conn.transport_bucket.allow() {
        return conn.strike(state, "frame rate exceeded");
    }
    if let Some(len) = msg.handshake_payload_len() {
        if len > caps::MAX_FRAME_HANDSHAKE as u64 {
            return Act::SendErrorThenClose(
                ErrorCode::ProtocolViolation,
                "handshake payload exceeds 4 KiB",
            );
        }
    }
    match msg {
        ClientMsg::Hello { .. } => {
            Act::SendErrorThenClose(ErrorCode::ProtocolViolation, "Hello already completed")
        }
        ClientMsg::Handshake { to, hs_id, body } => {
            let initiating = matches!(body, HandshakeMsg::Msg1(_));
            if initiating {
                if !admit_initiation(state, conn, hs_id) {
                    return conn.strike(state, "handshake rate exceeded");
                }
            } else if matches!(body, HandshakeMsg::Msg3(_)) {
                let mut pending = state.pending.lock().unwrap();
                if let Some(p) = pending.remove(&hs_id) {
                    let mut ips = state.ips.lock().unwrap();
                    if let Some(e) = ips.get_mut(&p.ip) {
                        e.pending_hs = e.pending_hs.saturating_sub(1);
                    }
                }
            }
            state.forward(
                &to,
                ServerMsg::Handshake {
                    from: conn.peer_id,
                    hs_id,
                    body,
                },
            );
            Act::Keep
        }
        ClientMsg::Transport {
            to,
            session_id,
            body,
        } => {
            state.forward(
                &to,
                ServerMsg::Transport {
                    from: conn.peer_id,
                    session_id,
                    body,
                },
            );
            Act::Keep
        }
        ClientMsg::InviteAccept { token, hs_id, msg1 } => {
            // Counted as a handshake initiation per §12 — charged before the
            // slot check so probing invites costs rate budget either way.
            if !admit_initiation(state, conn, hs_id) {
                return conn.strike(state, "handshake rate exceeded");
            }
            match state.mail.invite_consume(&token) {
                Some(owner) => {
                    // §7: an offline owner means the Msg1 is dropped, not
                    // failed — the attempt dies by handshake timeout.
                    state.forward(
                        &owner,
                        ServerMsg::Handshake {
                            from: conn.peer_id,
                            hs_id,
                            body: HandshakeMsg::Msg1(msg1),
                        },
                    );
                }
                None => {
                    abort_initiation(state, conn, hs_id);
                    conn.out.push(ServerMsg::err(
                        ErrorCode::InviteInvalid,
                        "invite token unknown, consumed, or expired",
                    ));
                }
            }
            Act::Keep
        }
        ClientMsg::InviteCreate { token, ttl_secs } => {
            match state.mail.invite_create(
                &state.cfg,
                conn.id,
                conn.ip,
                conn.peer_id,
                token,
                ttl_secs,
            ) {
                Ok(msgs) => push_msgs(conn, msgs),
                Err((code, detail)) => mail_err(state, conn, code, detail),
            }
        }
        ClientMsg::MailboxOpen {
            mailbox_id,
            h_deposit,
            h_fetch,
            ttl_secs,
        } => match state.mail.open(
            &state.cfg, conn.ip, mailbox_id, h_deposit, h_fetch, ttl_secs,
        ) {
            Ok(msgs) => push_msgs(conn, msgs),
            Err((code, detail)) => mail_err(state, conn, code, detail),
        },
        ClientMsg::MailboxDeposit {
            mailbox_id,
            token,
            env,
        } => {
            if env.body.len() > crate::mailbox::MAX_ENVELOPE_BODY {
                return Act::SendErrorThenClose(
                    ErrorCode::ProtocolViolation,
                    "envelope body exceeds 8210 B",
                );
            }
            match state
                .mail
                .deposit(&state.cfg, conn.peer_id, mailbox_id, token, env)
            {
                Ok(msgs) => push_msgs(conn, msgs),
                Err((code, detail)) => mail_err(state, conn, code, detail),
            }
        }
        ClientMsg::MailboxFetch {
            mailbox_id,
            token,
            max_msgs,
        } => {
            if max_msgs as usize > state.cfg.mailbox_max_msgs {
                return Act::SendErrorThenClose(
                    ErrorCode::ProtocolViolation,
                    "max_msgs exceeds 500",
                );
            }
            match state.mail.fetch(&state.cfg, mailbox_id, token, max_msgs) {
                Ok(msgs) => Act::SendNow(msgs),
                Err((code, detail)) => mail_err(state, conn, code, detail),
            }
        }
        ClientMsg::MailboxAck {
            mailbox_id,
            token,
            msg_ids,
        } => {
            if msg_ids.len() > state.cfg.mailbox_max_msgs {
                return Act::SendErrorThenClose(
                    ErrorCode::ProtocolViolation,
                    "ack set exceeds 500 msg_ids",
                );
            }
            match state.mail.ack(mailbox_id, token, msg_ids) {
                Ok(msgs) => push_msgs(conn, msgs),
                Err((code, detail)) => mail_err(state, conn, code, detail),
            }
        }
        ClientMsg::MailboxRotate {
            mailbox_id,
            token,
            new_h_deposit,
            new_h_fetch,
        } => match state
            .mail
            .rotate(mailbox_id, token, new_h_deposit, new_h_fetch)
        {
            Ok(msgs) => push_msgs(conn, msgs),
            Err((code, detail)) => mail_err(state, conn, code, detail),
        },
        ClientMsg::MailboxClose { mailbox_id, token } => {
            match state.mail.close(mailbox_id, token) {
                Ok(msgs) => push_msgs(conn, msgs),
                Err((code, detail)) => mail_err(state, conn, code, detail),
            }
        }
        ClientMsg::PresenceSubscribe { peers } => {
            if !conn.presence_bucket.allow() {
                return conn.strike(state, "presence subscribe rate exceeded");
            }
            if peers.len() > state.cfg.presence_max_peers {
                return Act::SendErrorThenClose(
                    ErrorCode::ProtocolViolation,
                    "subscription set exceeds 256 peers",
                );
            }
            {
                let mut subs = state.subs.lock().unwrap();
                let new: HashSet<PeerId> = peers.into_iter().collect();
                let mut sub = cs.subscribed.lock().unwrap();
                for p in sub.difference(&new) {
                    if let Some(set) = subs.get_mut(p) {
                        set.remove(&conn.id);
                    }
                }
                for p in new.difference(&sub) {
                    subs.entry(*p).or_default().insert(conn.id);
                }
                *sub = new.clone();
                drop(sub);
                drop(subs);
                conn.out.push(ServerMsg::Ok);
                let peers_reg = state.peers.lock().unwrap();
                for p in &new {
                    cs.out.push_lossy(ServerMsg::PeerStatus {
                        peer: *p,
                        online: peers_reg.contains_key(p),
                    });
                }
            }
            Act::Keep
        }
        ClientMsg::Ping { nonce } => {
            conn.out.push(ServerMsg::Pong { nonce });
            Act::Keep
        }
    }
}
