use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub const PROTOCOL_VERSION: u16 = 1;

pub mod caps {
    use std::time::Duration;

    pub const MAX_FRAME_HELLO: usize = 4096;
    pub const MAX_FRAME_HANDSHAKE: usize = 4096;
    pub const MAX_FRAME_TRANSPORT: usize = 16384;
    pub const MAX_PLAINTEXT: usize = 8192;
    pub const PAD_MIN: usize = 256;
    pub const PEER_ID_LEN: usize = 32;
    pub const HS_ID_LEN: usize = 16;
    pub const SESSION_ID_LEN: usize = 16;
    pub const TOKEN_LEN: usize = 32;
    pub const MAILBOX_ID_LEN: usize = 32;
    pub const INVITE_TOKEN_LEN: usize = 16;
    pub const MAILBOX_MAX_MSGS: usize = 500;
    pub const MAILBOX_MAX_BYTES: usize = 4 * 1024 * 1024;
    pub const MAILBOX_MAX_TTL: u32 = 2_592_000;
    pub const INVITE_MAX_TTL: u32 = 259_200;
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
}

macro_rules! id_type {
    ($name:ident, $len:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub [u8; $len]);

        impl $name {
            pub const LEN: usize = $len;

            pub fn random() -> Self {
                use rand::RngCore;
                let mut bytes = [0u8; $len];
                rand::rngs::OsRng.fill_bytes(&mut bytes);
                Self(bytes)
            }

            pub fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl From<[u8; $len]> for $name {
            fn from(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
    };
}

macro_rules! public_id {
    ($name:ident, $len:expr) => {
        id_type!($name, $len);

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "("))?;
                for b in &self.0[..4] {
                    write!(f, "{b:02x}")?;
                }
                write!(f, "…)")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for b in &self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    };
}

// Tokens are id+secret combined; Debug/Display stay redacted so they can't
// leak into logs or error details (§14 invariant 9).
macro_rules! secret_id {
    ($name:ident, $len:expr) => {
        id_type!($name, $len);

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "(<redacted>)"))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("<redacted>")
            }
        }
    };
}

public_id!(PeerId, caps::PEER_ID_LEN);
public_id!(HsId, caps::HS_ID_LEN);
public_id!(SessionId, caps::SESSION_ID_LEN);
public_id!(TokenHash, caps::TOKEN_LEN);
public_id!(MailboxId, caps::MAILBOX_ID_LEN);
secret_id!(Token, caps::TOKEN_LEN);
secret_id!(InviteToken, caps::INVITE_TOKEN_LEN);

impl PeerId {
    pub fn from_static_key(public_key: &[u8; 32]) -> Self {
        Self(Sha256::digest(public_key).into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeMsg {
    Msg1(Vec<u8>),
    Msg2(Vec<u8>),
    Msg3(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepositEnvelope {
    pub session_id: SessionId,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEnvelope {
    pub msg_id: u64,
    pub from: PeerId,
    pub session_id: SessionId,
    pub body: Vec<u8>,
    pub received_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// FIRST frame only. version = PROTOCOL_VERSION. Serialized ≤ 64 B; hard cap 4 KiB.
    Hello { version: u16, peer_id: PeerId },
    /// One-time rendezvous slot. ttl_secs ≤ INVITE_MAX_TTL (72 h); 0 → relay default (24 h). ≤ 64 B.
    InviteCreate { token: InviteToken, ttl_secs: u32 },
    /// Consume a rendezvous slot AND deliver Noise Msg1 in one frame. ≤ 4 KiB.
    InviteAccept {
        token: InviteToken,
        hs_id: HsId,
        msg1: Vec<u8>,
    },
    /// Any Noise XX handshake message routed by destination peer-id. ≤ 4 KiB.
    Handshake {
        to: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    },
    /// Opaque Noise transport ciphertext. body ≤ 8210 B; whole frame ≤ 16 KiB.
    Transport {
        to: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    },
    /// Register a mailbox (token HASHES only). ttl_secs ≤ MAILBOX_MAX_TTL (30 d); 0 → default 7 d. ≤ 160 B.
    MailboxOpen {
        mailbox_id: MailboxId,
        h_deposit: TokenHash,
        h_fetch: TokenHash,
        ttl_secs: u32,
    },
    /// Deposit an opaque envelope (raw deposit token). Frame ≤ 16 KiB.
    MailboxDeposit {
        mailbox_id: MailboxId,
        token: Token,
        env: DepositEnvelope,
    },
    /// Non-destructive fetch, max_msgs ≤ 500. ≤ 96 B.
    MailboxFetch {
        mailbox_id: MailboxId,
        token: Token,
        max_msgs: u16,
    },
    /// Delete fetched envelopes by msg_id. msg_ids.len() ≤ 500. ≤ ~4 KiB.
    MailboxAck {
        mailbox_id: MailboxId,
        token: Token,
        msg_ids: Vec<u64>,
    },
    /// Rotate capability tokens, authenticated by CURRENT fetch token. ≤ 160 B.
    MailboxRotate {
        mailbox_id: MailboxId,
        token: Token,
        new_h_deposit: TokenHash,
        new_h_fetch: TokenHash,
    },
    /// Delete the whole mailbox, authenticated by fetch token. ≤ 96 B.
    MailboxClose { mailbox_id: MailboxId, token: Token },
    /// Subscribe to online/offline status. peers.len() ≤ 256; replaces the previous set. ≤ ~8.4 KiB.
    PresenceSubscribe { peers: Vec<PeerId> },
    /// Liveness probe; relay answers Pong. nonce is an opaque echo token, NOT a Noise nonce.
    Ping { nonce: u64 },
}

impl ClientMsg {
    /// Serialized size of the Noise handshake payload this frame carries
    /// (§2: `Handshake.body` / `InviteAccept.msg1` must each stay ≤ MAX_FRAME_HANDSHAKE).
    pub fn handshake_payload_len(&self) -> Option<u64> {
        match self {
            ClientMsg::Handshake { body, .. } => crate::frame::wire_serialized_size(body).ok(),
            ClientMsg::InviteAccept { msg1, .. } => crate::frame::wire_serialized_size(msg1).ok(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Reply to Hello. now_unix = relay wall clock (informational).
    ServerHello { version: u16, now_unix: u64 },
    /// Invite slot registered; expires_at authoritative (ttl clamped).
    InviteCreated { token: InviteToken, expires_at: u64 },
    /// Forwarded handshake message. from = originator's CLAIMED peer-id, stamped by relay.
    Handshake {
        from: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    },
    /// Forwarded transport ciphertext. from is CLAIMED and unauthenticated.
    Transport {
        from: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    },
    /// Mailbox registered; expires_at authoritative after TTL clamp.
    MailboxOpened {
        mailbox_id: MailboxId,
        expires_at: u64,
    },
    /// Deposit accepted; msg_id is the stored envelope's id.
    MailboxDeposited { msg_id: u64 },
    /// Non-destructive fetch result, oldest-first, whole envelopes only.
    /// `more:false` terminates the response sequence.
    MailboxMessages {
        msgs: Vec<StoredEnvelope>,
        more: bool,
    },
    /// Generic success for MailboxAck / MailboxRotate / MailboxClose / PresenceSubscribe.
    Ok,
    /// Presence event for a subscribed peer-id.
    PeerStatus { peer: PeerId, online: bool },
    /// Answer to Ping.
    Pong { nonce: u64 },
    /// Any failure. detail ≤ 256 B, never carries secrets.
    Error { code: ErrorCode, detail: String },
}

impl ServerMsg {
    pub fn err(code: ErrorCode, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > 256 {
            let mut end = 256;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        ServerMsg::Error { code, detail }
    }
}

/// §10 numeric codes are the stable wire contract. serde/bincode encodes the
/// positional variant index, so `Reserved` occupies index 0 to keep
/// index == discriminant == code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum ErrorCode {
    #[doc(hidden)]
    Reserved = 0x0000,
    /// Hello.version != PROTOCOL_VERSION. Close.
    VersionMismatch = 0x0001,
    /// len prefix > phase cap. Close (framing desync).
    OversizeFrame = 0x0002,
    /// Any §12 limit exceeded. Stays open; ≥5 strikes/60 s → close.
    RateLimited = 0x0003,
    /// mailbox_id unknown/expired. Stays open.
    UnknownMailbox = 0x0004,
    /// Token hash mismatch. Stays open; ≥10 strikes/60 s per IP → close.
    BadToken = 0x0005,
    /// Deposit over 500 msgs or 4 MiB. Stays open.
    MailboxFull = 0x0006,
    /// Pending hs_id evicted after 10 s. Stays open (attempt aborted).
    HandshakeTimeout = 0x0007,
    /// Wrong first frame, len == 0, trailing bytes, handshake body >4 KiB, garbage. Close.
    ProtocolViolation = 0x0008,
    /// Invite token unknown/consumed/expired. Stays open.
    InviteInvalid = 0x0009,
    /// Hello claims an already-connected peer-id. Close.
    PeerIdInUse = 0x000A,
    /// MailboxOpen with a live mailbox_id. Stays open.
    MailboxExists = 0x000B,
}

impl ErrorCode {
    pub fn code(self) -> u16 {
        self as u16
    }

    /// §10 "Connection after error" column.
    pub fn closes_connection(self) -> bool {
        matches!(
            self,
            ErrorCode::VersionMismatch
                | ErrorCode::OversizeFrame
                | ErrorCode::ProtocolViolation
                | ErrorCode::PeerIdInUse
                | ErrorCode::Reserved
        )
    }
}
