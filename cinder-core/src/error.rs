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
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("sas rejected")]
    SasRejected,
}
