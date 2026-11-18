pub mod codec;
pub mod core;
pub mod describe;
pub mod dispatch;
pub mod events;
pub mod methods;
// peercred does a getpeereid/SO_PEERCRED ioctl — the second allow-listed
// unsafe site under `#![deny(unsafe_code)]` (BRIEF: hardening + peercred).
#[cfg(unix)]
#[allow(unsafe_code)]
pub mod peercred;
pub mod protocol;
pub mod server;

// Re-exports are the public API surface for the `cinder rpc`/`cinder daemon`
// binary wiring (main.rs, M3); today only tests consume them, so unused-import
// lints fire when the module is compiled standalone per integration test.
#[allow(unused_imports)]
pub use codec::{is_blank, FrameWriter, LineRead, LineReader, MAX_LINE_BYTES};
#[allow(unused_imports)]
pub use core::{CinderCore, EventSink};
#[allow(unused_imports)]
pub use describe::{describe, EventDef, MethodDef, EVENTS, METHODS, PROTOCOL_VERSION};
#[allow(unused_imports)]
pub use dispatch::{serve_connection, Engine, EngineConfig, Session, Transport, Verdict};
#[allow(unused_imports)]
pub use events::{EventManager, EventRecord, Filters, InMemoryReplayLog, OutMsg, ReplayLog};
#[allow(unused_imports)]
pub use protocol::{
    error_response, event_notification, result_response, ErrorKind, RpcError, CONFLICT,
    CONTACT_BLOCKED, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, KEY_CHANGED_BLOCKED,
    MAILBOX_FULL, METHOD_NOT_FOUND, NOT_CONNECTED, NOT_FOUND, PARSE_ERROR, PEER_OFFLINE,
    RATE_LIMITED, UNAUTHORIZED_SCOPE,
};
#[allow(unused_imports)]
pub use server::DaemonConfig;

#[cfg(unix)]
#[allow(unused_imports)]
pub use peercred::{own_uid, peer_uid};
#[cfg(unix)]
#[allow(unused_imports)]
pub use server::run_daemon;
