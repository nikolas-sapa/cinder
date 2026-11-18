#![no_main]

use std::sync::OnceLock;

use cinder_core::frame::wire_deserialize;
use cinder_core::proto::{HandshakeMsg, PeerId};
use cinder_core::{Identity, Session, SessionState};
use libfuzzer_sys::fuzz_target;

static IDENTITY: OnceLock<Identity> = OnceLock::new();

fn identity() -> &'static Identity {
    IDENTITY.get_or_init(|| Identity::generate().expect("x25519 keygen"))
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn take(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.data.len() - self.pos);
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        s
    }

    fn take_len16(&mut self) -> &'a [u8] {
        let hi = self.u8().unwrap_or(0) as usize;
        let lo = self.u8().unwrap_or(0) as usize;
        self.take((hi << 8) | lo)
    }
}

fn pinned_or_corrupt(session: &Session, pinned: bool) {
    match session.state() {
        SessionState::Handshaking => {
            assert!(session.session_id().is_none());
            assert!(session.sas().is_none());
            assert!(session.handshake_hash().is_none());
        }
        SessionState::Closed(_) => {}
        SessionState::Trusted(peer) => {
            assert!(pinned, "Trusted reached without a passing pin check");
            assert_eq!(session.remote_peer_id(), Some(peer));
            assert!(session.remote_static().is_some());
            assert!(session.session_id().is_some());
            assert!(session.sas().is_some());
            assert!(session.handshake_hash().is_some());
        }
    }
}

fn drive(session: &mut Session, data: &[u8]) {
    let mut cur = Cursor::new(data);
    let mut pinned = false;
    while let Some(op) = cur.u8() {
        let was_closed = session.is_closed();
        match op % 9 {
            v @ 0..=2 => {
                let payload = cur.take_len16();
                // If the payload happens to be a serialized HandshakeMsg, feed
                // that; otherwise wrap the raw bytes in the chosen variant.
                let msg = wire_deserialize::<HandshakeMsg>(payload).unwrap_or_else(|_| {
                    let p = payload.to_vec();
                    match v {
                        0 => HandshakeMsg::Msg1(p),
                        1 => HandshakeMsg::Msg2(p),
                        _ => HandshakeMsg::Msg3(p),
                    }
                });
                let res = session.accept_handshake(msg);
                if was_closed {
                    assert!(res.is_err());
                }
            }
            3 => {
                let res = session.produce_handshake();
                if was_closed {
                    assert!(res.is_err());
                }
            }
            4 => {
                let raw = cur.take(32);
                let mut pin = [0u8; 32];
                pin[..raw.len()].copy_from_slice(raw);
                if session.verify_peer_id(&PeerId::from_bytes(pin)).is_ok() {
                    pinned = true;
                }
            }
            5 => {
                if session.accept_learned_peer().is_ok() {
                    pinned = true;
                }
            }
            6 => {
                let res = session.finish_handshake();
                if was_closed {
                    assert!(res.is_err());
                }
            }
            7 => {
                let ct = cur.take_len16();
                let res = session.decrypt(ct);
                if was_closed {
                    assert!(res.is_err());
                }
            }
            _ => {
                let pt = cur.take_len16();
                let res = session.encrypt(pt);
                if was_closed {
                    assert!(res.is_err());
                }
            }
        }
        pinned_or_corrupt(session, pinned);
        if was_closed {
            assert!(session.is_closed());
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(mut responder) = Session::responder(identity()) {
        drive(&mut responder, data);
    }
    if let Ok(mut initiator) = Session::initiator(identity()) {
        let _ = initiator.produce_handshake();
        drive(&mut initiator, data);
    }
});
