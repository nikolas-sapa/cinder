#![forbid(unsafe_code)]

pub mod contacts;
pub mod error;
pub mod frame;
pub mod handshake_flow;
pub mod identity;
pub mod keystore;
pub mod padding;
pub mod proto;
pub mod session;
pub mod transport;

pub use contacts::{Contact, ContactStore, PendingContact, PendingState, TrustState};
pub use error::Error;
pub use identity::Identity;
pub use proto::{ClientMsg, ErrorCode, PeerId, ServerMsg};
pub use session::{CloseReason, RekeyPolicy, Sas, Session, SessionEvent, SessionState};
pub use transport::{ClientEvent, Link, RelayTransport};
