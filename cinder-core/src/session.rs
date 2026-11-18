use std::fmt;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use snow::{Builder, HandshakeState, TransportState};
use subtle::ConstantTimeEq;

use crate::contacts::ContactStore;
use crate::error::Error;
use crate::identity::{Identity, NOISE_PARAMS};
use crate::padding::{decode_plaintext, encode_plaintext};
use crate::proto::{caps, HandshakeMsg, PeerId, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    PeerIdMismatch,
    DecryptFatal,
    HandshakeFailed,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Handshaking,
    Trusted(PeerId),
    Closed(CloseReason),
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Trusted { peer_id: PeerId },
    Closed(CloseReason),
    RekeyDue,
}

#[derive(Debug, Clone, Copy)]
pub struct RekeyPolicy {
    pub msgs: u64,
    pub secs: u64,
}

impl Default for RekeyPolicy {
    fn default() -> Self {
        Self {
            msgs: 1024,
            secs: 600,
        }
    }
}

/// XX message order per role: initiator writes 1, reads 2, writes 3;
/// responder reads 1, writes 2, reads 3. `hs_step` counts completed ops.
pub struct Session {
    role: Role,
    state: SessionState,
    hs: Option<HandshakeState>,
    transport: Option<TransportState>,
    hs_step: u8,
    pin_ok: bool,
    our_static: [u8; 32],
    remote_static: Option<[u8; 32]>,
    remote_peer: Option<PeerId>,
    session_id: Option<SessionId>,
    handshake_hash: Option<[u8; 32]>,
    sas: Option<Sas>,
    sent: u64,
    received: u64,
    rekeys: u64,
    window_start: Instant,
    window_msgs: u64,
    policy: RekeyPolicy,
}

impl Session {
    pub fn initiator(identity: &Identity) -> Result<Self, Error> {
        Self::new(identity, Role::Initiator)
    }

    pub fn responder(identity: &Identity) -> Result<Self, Error> {
        Self::new(identity, Role::Responder)
    }

    fn new(identity: &Identity, role: Role) -> Result<Self, Error> {
        let params: snow::params::NoiseParams = NOISE_PARAMS.parse()?;
        let builder = Builder::new(params).local_private_key(&identity.keypair().private);
        let hs = match role {
            Role::Initiator => builder.build_initiator()?,
            Role::Responder => builder.build_responder()?,
        };
        Ok(Session {
            role,
            state: SessionState::Handshaking,
            hs: Some(hs),
            transport: None,
            hs_step: 0,
            pin_ok: false,
            our_static: identity.public(),
            remote_static: None,
            remote_peer: None,
            session_id: None,
            handshake_hash: None,
            sas: None,
            sent: 0,
            received: 0,
            rekeys: 0,
            window_start: Instant::now(),
            window_msgs: 0,
            policy: RekeyPolicy::default(),
        })
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if let SessionState::Closed(_) = self.state {
            return Err(Error::SessionClosed);
        }
        Ok(())
    }

    /// Next outbound XX message, in role order. Initiator's Msg3 is gated on a
    /// passed pin check (§6: a mismatch means Msg3 is NEVER sent).
    pub fn produce_handshake(&mut self) -> Result<HandshakeMsg, Error> {
        self.ensure_open()?;
        let variant = match (self.role, self.hs_step) {
            (Role::Initiator, 0) => 0u8,
            (Role::Responder, 1) => 1u8,
            (Role::Initiator, 2) => 2u8,
            _ => {
                self.fatal_close(CloseReason::HandshakeFailed);
                return Err(Error::UnexpectedMsg("handshake write out of turn".into()));
            }
        };
        if variant == 2 && !self.pin_ok {
            return Err(Error::Protocol(
                "peer-id not verified: Msg3 may not be sent".into(),
            ));
        }
        let mut buf = vec![0u8; caps::MAX_FRAME_HANDSHAKE];
        let res = {
            let hs = self
                .hs
                .as_mut()
                .ok_or_else(|| Error::Protocol("no handshake in progress".into()))?;
            hs.write_message(&[], &mut buf)
        };
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                self.fatal_close(CloseReason::HandshakeFailed);
                return Err(Error::Noise(e));
            }
        };
        buf.truncate(n);
        self.hs_step += 1;
        Ok(match variant {
            0 => HandshakeMsg::Msg1(buf),
            1 => HandshakeMsg::Msg2(buf),
            _ => HandshakeMsg::Msg3(buf),
        })
    }

    /// Consume the inbound XX message for this step. A wrong variant or an AEAD
    /// failure kills the session — a handshake anomaly is never recoverable.
    pub fn accept_handshake(&mut self, msg: HandshakeMsg) -> Result<(), Error> {
        self.ensure_open()?;
        let ok = matches!(
            (self.role, self.hs_step, &msg),
            (Role::Responder, 0, HandshakeMsg::Msg1(_))
                | (Role::Initiator, 1, HandshakeMsg::Msg2(_))
                | (Role::Responder, 2, HandshakeMsg::Msg3(_))
        );
        if !ok {
            self.fatal_close(CloseReason::HandshakeFailed);
            return Err(Error::UnexpectedMsg(
                "unexpected handshake message for this step".into(),
            ));
        }
        let payload = match &msg {
            HandshakeMsg::Msg1(p) | HandshakeMsg::Msg2(p) | HandshakeMsg::Msg3(p) => p,
        };
        if payload.len() > caps::MAX_FRAME_HANDSHAKE {
            self.fatal_close(CloseReason::HandshakeFailed);
            return Err(Error::Protocol("handshake message exceeds 4 KiB".into()));
        }
        let mut out = vec![0u8; payload.len()];
        let res = {
            let hs = self
                .hs
                .as_mut()
                .ok_or_else(|| Error::Protocol("no handshake in progress".into()))?;
            hs.read_message(payload, &mut out)
        };
        match res {
            Ok(_) => {
                self.hs_step += 1;
                self.capture_remote();
                Ok(())
            }
            Err(e) => {
                self.fatal_close(CloseReason::HandshakeFailed);
                Err(Error::Noise(e))
            }
        }
    }

    /// THE peer-id pin check (§6): SHA-256(remote_static) constant-time
    /// compared against the expected peer-id. Mismatch closes the session and
    /// returns a loud `PeerIdMismatch`. This is the ONLY gate to `Trusted`.
    pub fn verify_peer_id(&mut self, expected: &PeerId) -> Result<(), Error> {
        self.ensure_open()?;
        let actual = self.remote_peer.ok_or_else(|| {
            Error::Protocol("remote static not yet known — nothing to pin".into())
        })?;
        if actual.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1 {
            self.pin_ok = true;
            return Ok(());
        }
        self.fatal_close(CloseReason::PeerIdMismatch);
        Err(Error::PeerIdMismatch {
            expected: *expected,
            actual,
            actual_static: self.remote_static.unwrap_or_default(),
        })
    }

    /// TOFU pin for a responder talking to a new contact (§6): adopt the
    /// learned peer-id. Goes through `verify_peer_id` so the only code path
    /// that sets `pin_ok` — and thus reaches `Trusted` — is the pin check.
    pub fn accept_learned_peer(&mut self) -> Result<PeerId, Error> {
        self.ensure_open()?;
        let learned = self.remote_peer.ok_or_else(|| {
            Error::Protocol("remote static not yet known — nothing to pin".into())
        })?;
        self.verify_peer_id(&learned)?;
        Ok(learned)
    }

    /// Consumes the finished handshake. Refuses unless `pin_ok` is set —
    /// `Trusted` is unreachable without peer-id verification (§6, §14.2).
    pub fn finish_handshake(&mut self) -> Result<(), Error> {
        self.ensure_open()?;
        if !self.pin_ok {
            return Err(Error::Protocol(
                "peer-id verification is the only path to Trusted".into(),
            ));
        }
        let finished = self
            .hs
            .as_ref()
            .map(HandshakeState::is_handshake_finished)
            .unwrap_or(false);
        if !finished {
            return Err(Error::UnexpectedMsg("handshake incomplete".into()));
        }
        let hs = self.hs.take().expect("checked above");
        let hh: [u8; 32] = hs
            .get_handshake_hash()
            .try_into()
            .map_err(|_| Error::Protocol("unexpected handshake hash length".into()))?;
        let remote = self
            .remote_static
            .ok_or_else(|| Error::Protocol("finished handshake without remote static".into()))?;
        let transport = match hs.into_transport_mode() {
            Ok(t) => t,
            Err(e) => {
                self.fatal_close(CloseReason::HandshakeFailed);
                return Err(Error::Noise(e));
            }
        };
        self.transport = Some(transport);
        self.session_id = Some(SessionId::random());
        self.handshake_hash = Some(hh);
        self.sas = Some(Sas::derive(&hh, &self.our_static, &remote));
        self.state = SessionState::Trusted(self.remote_peer.expect("set with remote_static"));
        Ok(())
    }

    /// §11: an empty plaintext is RESERVED — it encodes the rekey control
    /// unit on the wire, so data encrypts reject it to keep the two
    /// unambiguous to the receiver.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        match self.state {
            SessionState::Closed(_) => return Err(Error::SessionClosed),
            SessionState::Handshaking => {
                return Err(Error::Protocol("session not established".into()))
            }
            SessionState::Trusted(_) => {}
        }
        if plaintext.is_empty() {
            return Err(Error::Padding);
        }
        let padded = encode_plaintext(plaintext)?;
        let mut buf = vec![0u8; padded.len() + 16];
        let res = {
            let ts = self.transport.as_mut().expect("trusted implies transport");
            ts.write_message(&padded, &mut buf)
        };
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                self.fatal_close(CloseReason::DecryptFatal);
                return Err(Error::Noise(e));
            }
        };
        buf.truncate(n);
        self.sent += 1;
        self.window_msgs += 1;
        Ok(buf)
    }

    /// §11: ANY decrypt failure is fatal — the session is destroyed and no
    /// retry-skip path exists. A frame under a different session_id is the
    /// caller's problem (dropped before reaching here).
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        match self.state {
            SessionState::Closed(_) => return Err(Error::SessionClosed),
            SessionState::Handshaking => {
                return Err(Error::Protocol("session not established".into()))
            }
            SessionState::Trusted(_) => {}
        }
        let mut buf = vec![0u8; ciphertext.len()];
        let res = {
            let ts = self.transport.as_mut().expect("trusted implies transport");
            ts.read_message(ciphertext, &mut buf)
        };
        let n = match res {
            Ok(n) => n,
            Err(_) => {
                self.fatal_close(CloseReason::DecryptFatal);
                return Err(Error::DecryptFatal);
            }
        };
        buf.truncate(n);
        match decode_plaintext(&buf) {
            Ok(&[]) => {
                self.rekey(true)?;
                Ok(Vec::new())
            }
            Ok(pt) => {
                self.received += 1;
                Ok(pt.to_vec())
            }
            Err(_) => {
                self.fatal_close(CloseReason::DecryptFatal);
                Err(Error::DecryptFatal)
            }
        }
    }

    pub fn close(&mut self) {
        self.fatal_close(CloseReason::Shutdown);
    }

    /// Compare the learned remote static against the stored contact key for
    /// the peer-id we expected. A difference is a key change: recorded (loud,
    /// blocks send) via the contacts path, and `true` is returned. Called by
    /// the core after every successful session establishment (§11: pinning
    /// re-checks against the contacts store on each new XX).
    pub fn detect_key_change(
        &self,
        store: &mut ContactStore,
        expected: &PeerId,
    ) -> Result<bool, Error> {
        let Some(remote) = self.remote_static else {
            return Ok(false);
        };
        match store.get(expected) {
            Some(c) if c.static_pubkey.ct_eq(&remote).unwrap_u8() != 1 => {
                store.record_key_change(expected, remote)
            }
            _ => Ok(false),
        }
    }

    pub fn set_rekey_policy(&mut self, policy: RekeyPolicy) {
        self.policy = policy;
    }

    /// §11 ratchet-lite rekey. `incoming = false` performs the OUTBOUND
    /// rekey: the returned control frame is sealed under the CURRENT egress
    /// key, then `rekey_outgoing` rotates it — the frame MUST be transmitted
    /// in order (after every frame produced so far, before the next
    /// `encrypt` output). `incoming = true` applies `rekey_incoming`; the
    /// call path is a control unit inside `decrypt`. Transport-level only:
    /// no handshake, no re-verification (pinning applies at each NEW XX),
    /// and no post-compromise healing — only a new handshake heals
    /// (THREAT-MODEL §5).
    pub fn rekey(&mut self, incoming: bool) -> Result<Vec<u8>, Error> {
        match self.state {
            SessionState::Closed(_) => return Err(Error::SessionClosed),
            SessionState::Handshaking => {
                return Err(Error::Protocol("session not established".into()))
            }
            SessionState::Trusted(_) => {}
        }
        if incoming {
            self.transport
                .as_mut()
                .expect("trusted implies transport")
                .rekey_incoming();
            self.rekeys += 1;
            return Ok(Vec::new());
        }
        let padded = encode_plaintext(&[])?;
        let mut buf = vec![0u8; padded.len() + 16];
        let res = {
            let ts = self.transport.as_mut().expect("trusted implies transport");
            ts.write_message(&padded, &mut buf)
        };
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                self.fatal_close(CloseReason::DecryptFatal);
                return Err(Error::Noise(e));
            }
        };
        buf.truncate(n);
        self.transport
            .as_mut()
            .expect("trusted implies transport")
            .rekey_outgoing();
        self.rekeys += 1;
        self.window_msgs = 0;
        self.window_start = Instant::now();
        Ok(buf)
    }

    /// §11 trigger check, caller-driven. On a hit the returned control frame
    /// is the last frame under the old egress key and MUST go on the wire
    /// before the next `encrypt` output — TCP ordering carries the boundary
    /// to the receiver, which applies `rekey_incoming` on the control unit.
    pub fn maybe_rekey(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if !self.rekey_needed() {
            return Ok(None);
        }
        self.rekey(false).map(Some)
    }

    pub fn rekey_needed(&self) -> bool {
        self.window_msgs >= self.policy.msgs
            || self.window_start.elapsed() >= Duration::from_secs(self.policy.secs)
    }

    fn capture_remote(&mut self) {
        let Some(hs) = &self.hs else { return };
        let Some(rs) = hs.get_remote_static() else {
            return;
        };
        let mut raw = [0u8; 32];
        raw.copy_from_slice(rs);
        self.remote_static = Some(raw);
        self.remote_peer = Some(PeerId::from_static_key(&raw));
    }

    /// snow exposes no zeroization of its key material; taking + dropping the
    /// states is the best available bound, and `Closed` makes every subsequent
    /// op error so the keys can never be touched again.
    fn fatal_close(&mut self, reason: CloseReason) {
        self.hs = None;
        self.transport = None;
        self.sas = None;
        self.state = SessionState::Closed(reason);
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, SessionState::Closed(_))
    }

    /// Peer-id derived from the remote's static (available after the remote's
    /// static-bearing message: Msg2 for initiator, Msg3 for responder).
    pub fn remote_peer_id(&self) -> Option<PeerId> {
        self.remote_peer
    }

    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.remote_static
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    pub fn sas(&self) -> Option<&Sas> {
        self.sas.as_ref()
    }

    pub fn handshake_hash(&self) -> Option<[u8; 32]> {
        self.handshake_hash
    }

    pub fn sent_count(&self) -> u64 {
        self.sent
    }

    pub fn received_count(&self) -> u64 {
        self.received
    }

    pub fn rekey_count(&self) -> u64 {
        self.rekeys
    }

    pub fn sending_nonce(&self) -> Option<u64> {
        self.transport.as_ref().map(TransportState::sending_nonce)
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("role", &self.role)
            .field("state", &self.state)
            .field("hs_step", &self.hs_step)
            .field("pin_ok", &self.pin_ok)
            .field("sent", &self.sent)
            .field("received", &self.received)
            .field("rekeys", &self.rekeys)
            .finish()
    }
}

/// SAS = H(handshake_hash ‖ min(pk_a,pk_b) ‖ max(pk_a,pk_b)), §6. Sorted
/// statics mean both sides derive identical bytes without role negotiation.
#[derive(Clone, PartialEq, Eq)]
pub struct Sas([u8; 32]);

impl Sas {
    pub fn derive(handshake_hash: &[u8; 32], pk_a: &[u8; 32], pk_b: &[u8; 32]) -> Self {
        let (lo, hi) = if pk_a <= pk_b {
            (pk_a, pk_b)
        } else {
            (pk_b, pk_a)
        };
        let mut h = Sha256::new();
        h.update(handshake_hash);
        h.update(lo);
        h.update(hi);
        Self(h.finalize().into())
    }

    pub fn digest(&self) -> &[u8; 32] {
        &self.0
    }

    /// 7 emoji: digest[i] & 0x3f indexes SAS_EMOJI for i in 0..7.
    pub fn emoji(&self) -> String {
        (0..7)
            .map(|i| SAS_EMOJI[(self.0[i] & 0x3f) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Digit fallback: 20 bytes × 3 digits = 60 digits, grouped in 5s.
    pub fn digits(&self) -> String {
        let raw: String = self.0[..20].iter().map(|b| format!("{b:03}")).collect();
        raw.as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).expect("ascii digits"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl fmt::Debug for Sas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sas({})", self.emoji())
    }
}

impl fmt::Display for Sas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.emoji())
    }
}

const SAS_EMOJI: [&str; 64] = [
    "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🐔",
    "🐧", "🐦", "🐤", "🦆", "🦅", "🦉", "🦇", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋", "🐌", "🐞",
    "🐜", "🐢", "🐍", "🦎", "🐙", "🦑", "🦐", "🦀", "🐡", "🐠", "🐟", "🐬", "🐳", "🐋", "🦈", "🐊",
    "🐅", "🐆", "🦓", "🦍", "🐘", "🦛", "🦏", "🐪", "🐫", "🦒", "🦘", "🐃", "🐂", "🐄", "🐎", "🐖",
];
