use crate::proto::PeerId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("wire: {0}")]
    Wire(#[from] bincode::Error),
    #[error("peer offline or unknown")]
    PeerOffline,
    #[error("frame: {0}")]
    Frame(#[from] crate::frame::FrameError),
    #[error("invalid plaintext padding")]
    Padding,
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("sas rejected")]
    SasRejected,
    #[error("contacts store integrity check failed")]
    ContactsIntegrity,
    #[error("identity store: {0}")]
    IdentityStore(String),
    #[error("invalid cinder uri: {0}")]
    UriParse(String),
    #[error("key material unavailable: {0}")]
    KeyUnavailable(String),
    #[error("unknown contact")]
    UnknownContact,
    #[error("peer-id mismatch: expected {expected}, derived {actual}")]
    PeerIdMismatch { expected: PeerId, actual: PeerId },
    #[error("decrypt failure: session destroyed")]
    DecryptFatal,
    #[error("handshake timed out")]
    HandshakeTimeout,
    #[error("unexpected message: {0}")]
    UnexpectedMsg(String),
    #[error("session is closed")]
    SessionClosed,
    #[error("transport: {0}")]
    Transport(String),
}
