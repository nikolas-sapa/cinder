use cinder_core::frame::FrameCodec;
use cinder_core::proto::{
    caps, ClientMsg, ErrorCode, HandshakeMsg, HsId, PeerId, ServerMsg, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
    /// Live mailboxes global (4096) — per-IP caps alone can't bound store
    /// size under source-IP spray.
    pub mailboxes_global: usize,
    /// Live invite slots global (2048).
    pub invites_global: usize,
    /// Concurrent TCP connections global (2048); each conn costs a task +
    /// codec buffers, so the count must be capped, not just rate-limited.
    pub conns_global: usize,
    /// Max tracked per-IP(/64) rate-state entries (65536); evict-oldest on
    /// insert so an IP spray can't grow the map unbounded.
    pub ips_map_max: usize,
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
            mailboxes_global: 4096,
            invites_global: 2048,
            conns_global: 2048,
            ips_map_max: 65536,
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

    fn has_pending(&self) -> bool {
        !self.inner.lock().unwrap().q.is_empty()
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

/// Decrements the global live-connection count on drop. The count is taken
/// before `Hello` so pre-auth connection spray is bounded too.
struct ActiveConn(Arc<RelayState>);

impl Drop for ActiveConn {
    fn drop(&mut self) {
        self.0.conns_active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Per-IP accounting key: IPv4 stays exact; IPv6 collapses to its /64
/// prefix — per-address caps are meaningless when a spray can mint a fresh
/// address per connection inside one delegated prefix.
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::V6(octets.into())
        }
    }
}

/// Bounded `ips` insert: at `ips_map_max`, evict the oldest-`last_seen`
/// entry — preferring ones holding no pending handshakes — so the map size
/// stays capped under source-IP spray.
fn ip_state<'a>(
    ips: &'a mut HashMap<IpAddr, IpState>,
    cfg: &RelayConfig,
    key: IpAddr,
) -> &'a mut IpState {
    if !ips.contains_key(&key) && ips.len() >= cfg.ips_map_max {
        if let Some(victim) = ips
            .iter()
            .min_by_key(|(_, e)| (e.pending_hs > 0, e.last_seen))
            .map(|(k, _)| *k)
        {
            ips.remove(&victim);
        }
    }
    ips.entry(key).or_insert_with(|| IpState::new(cfg))
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
    /// Keyed `(initiator conn_id, hs_id)` — an hs_id alone can collide
    /// across conns, and a blind `insert` would evict the original attempt
    /// while leaving the original initiator's `pending_hs` slot burned.
    pending: Mutex<HashMap<(u64, HsId), Pending>>,
    ips: Mutex<HashMap<IpAddr, IpState>>,
    mail: crate::mailbox::MailStore,
    conns_active: AtomicUsize,
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
            conns_active: AtomicUsize::new(0),
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

/// Max outbound frames written per notify wake — bounds one wake's work so
/// a flooded queue can't monopolize the conn loop under unbiased select.
const OUT_DRAIN_BATCH: usize = 64;

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
    let e = ip_state(&mut ips, &state.cfg, ip);
    e.last_seen = Instant::now();
    e.conn_bucket.allow()
}

async fn handle_conn(state: Arc<RelayState>, stream: TcpStream, addr: SocketAddr) {
    let ip = ip_key(addr.ip());
    let (rh, wh) = stream.into_split();
    let mut writer = tokio_util::codec::FramedWrite::new(wh, FrameCodec::<ServerMsg>::transport());
    let mut reader = tokio_util::codec::FramedRead::new(rh, FrameCodec::<ClientMsg>::pre_hello());

    // Global connection cap first: the per-IP rate below can't bound total
    // conns under source-IP spray, and every conn costs a task + buffers.
    if state.conns_active.fetch_add(1, Ordering::Relaxed) >= state.cfg.conns_global {
        state.conns_active.fetch_sub(1, Ordering::Relaxed);
        let _ = writer
            .send(ServerMsg::err(
                ErrorCode::RateLimited,
                "relay connection capacity reached",
            ))
            .await;
        let _ = writer.flush().await;
        return;
    }
    let _active = ActiveConn(state.clone());

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
        // No `biased`: a steady stream of forwarded frames must not pin the
        // notify branch and starve the reader + idle branches (conn would
        // livelock — never reading, never timing out). The drain is also
        // bounded per wake so one wake can't monopolize the loop.
        let act = tokio::select! {
            _ = out.notify.notified() => {
                let mut io_err = false;
                for _ in 0..OUT_DRAIN_BATCH {
                    let Some(msg) = out.pop() else { break };
                    if writer.feed(msg).await.is_err() {
                        io_err = true;
                        break;
                    }
                }
                if io_err || writer.flush().await.is_err() || out.is_dead() {
                    break;
                }
                // The batch cap may leave frames queued after the consumed
                // permit — re-arm so the queue can't stall; unbiased
                // polling still lets reader/idle branches interleave.
                if out.has_pending() {
                    out.notify.notify_one();
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
                // Flush already-queued outbound first (bounded at the queue
                // cap): a close must not silently drop the error trail
                // behind it — e.g. the strike series that triggered it.
                let mut io_err = false;
                for _ in 0..out.cap {
                    let Some(m) = out.pop() else { break };
                    if writer.feed(m).await.is_err() {
                        io_err = true;
                        break;
                    }
                }
                if !io_err {
                    let _ = writer.send(ServerMsg::err(code, detail)).await;
                    let _ = writer.flush().await;
                }
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
        pending.retain(|(cid, _), p| {
            if *cid == conn.id {
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
    let e = ip_state(&mut ips, &state.cfg, conn.ip);
    e.last_seen = Instant::now();
    let under_caps =
        e.pending_hs < state.cfg.pending_hs_per_ip && pending.len() < state.cfg.pending_hs_global;
    if !(e.hs_bucket.allow() && under_caps) {
        return false;
    }
    // Refuse hs_id reuse on this conn rather than evicting the live attempt
    // (an evicted entry would never free its pending_hs slot nor fire its
    // HandshakeTimeout — a wedge at pending_hs_per_ip reuses).
    if pending.contains_key(&(conn.id, hs_id)) {
        return false;
    }
    e.pending_hs += 1;
    pending.insert(
        (conn.id, hs_id),
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
    let Some(p) = pending.remove(&(conn.id, hs_id)) else {
        return;
    };
    let mut ips = state.ips.lock().unwrap();
    if let Some(e) = ips.get_mut(&p.ip) {
        e.pending_hs = e.pending_hs.saturating_sub(1);
    }
}

/// §10: BadToken stays open until ≥10 strikes/60 s per IP, then close.
fn bad_token(state: &RelayState, conn: &mut Conn, detail: &'static str) -> Act {
    let close = {
        let mut ips = state.ips.lock().unwrap();
        let e = ip_state(&mut ips, &state.cfg, conn.ip);
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
                // Msg3 comes from the initiator, so conn.id scopes the key
                // back to the exact attempt that Msg1 registered.
                if let Some(p) = pending.remove(&(conn.id, hs_id)) {
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
            // §2 semantic cap: the codec bounds the whole frame at 16 KiB
            // but a transport ciphertext must stay ≤ 8210 B — an oversized
            // body is a ProtocolViolation close, not just dead ciphertext.
            if body.len() > crate::mailbox::MAX_ENVELOPE_BODY {
                return Act::SendErrorThenClose(
                    ErrorCode::ProtocolViolation,
                    "transport body exceeds 8210 B",
                );
            }
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
            // Intentional presence oracle (PROTOCOL §13 lists interest sets
            // as relay-visible metadata): any client may watch arbitrary
            // peer-ids' online status — that IS the contact-presence
            // feature. Bounded by presence_rate + the set cap below.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn ip_key_buckets_ipv6_by_slash64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        // Same /64 → same bucket (rotation inside a prefix shares limits).
        assert_eq!(ip_key(a), ip_key(b));
        // Different /64 → different bucket.
        assert_ne!(ip_key(a), ip_key(c));
        // IPv4 stays exact.
        let v4 = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        assert_eq!(ip_key(v4), v4);
    }

    #[test]
    fn ip_state_evicts_oldest_at_map_cap() {
        let cfg = RelayConfig {
            ips_map_max: 3,
            ..RelayConfig::default()
        };
        let mut ips = HashMap::new();
        for i in 0..3u8 {
            let key = IpAddr::V4(Ipv4Addr::new(10, 0, 0, i));
            let mut e = IpState::new(&cfg);
            // Distinct ages: 10.0.0.0 is the oldest.
            e.last_seen = Instant::now() - Duration::from_secs(10 - u64::from(i));
            ips.insert(key, e);
        }
        let new_key = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        let _ = ip_state(&mut ips, &cfg, new_key);
        assert_eq!(ips.len(), 3);
        assert!(ips.contains_key(&new_key));
        assert!(!ips.contains_key(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0))));
    }

    #[test]
    fn ip_state_prefers_evicting_idle_entries() {
        let cfg = RelayConfig {
            ips_map_max: 2,
            ..RelayConfig::default()
        };
        let mut ips = HashMap::new();
        // Older entry holds a pending handshake — the fresher idle one is
        // the correct victim.
        let busy = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let idle = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let mut e = IpState::new(&cfg);
        e.last_seen = Instant::now() - Duration::from_secs(10);
        e.pending_hs = 1;
        ips.insert(busy, e);
        let mut e = IpState::new(&cfg);
        e.last_seen = Instant::now();
        ips.insert(idle, e);
        let new_key = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        let _ = ip_state(&mut ips, &cfg, new_key);
        assert!(ips.contains_key(&busy));
        assert!(!ips.contains_key(&idle));
    }
}
