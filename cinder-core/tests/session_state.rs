use cinder_core::error::Error;
use cinder_core::identity::Identity;
use cinder_core::proto::PeerId;
use cinder_core::session::{RekeyPolicy, Session, SessionState};
use proptest::prelude::*;

fn linked(a_id: &Identity, b_id: &Identity) -> (Session, Session) {
    let mut a = Session::initiator(a_id).unwrap();
    let mut b = Session::responder(b_id).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    a.accept_handshake(b.produce_handshake().unwrap()).unwrap();
    a.verify_peer_id(&b_id.peer_id()).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    b.verify_peer_id(&a_id.peer_id()).unwrap();
    a.finish_handshake().unwrap();
    b.finish_handshake().unwrap();
    (a, b)
}

#[derive(Debug, Clone)]
enum Op {
    Send,
    Recv,
    CorruptRecv,
    Close,
    RekeyCheck,
    BadPinHandshake,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => Just(Op::Send),
        4 => Just(Op::Recv),
        1 => Just(Op::CorruptRecv),
        1 => Just(Op::Close),
        1 => Just(Op::RekeyCheck),
        1 => Just(Op::BadPinHandshake),
    ]
}

const REKEY_MSGS: u64 = 7;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]
    #[test]
    fn session_state_invariants(ops in prop::collection::vec(op(), 1..40)) {
        let a_id = Identity::generate().unwrap();
        let b_id = Identity::generate().unwrap();
        let (mut a, mut b) = linked(&a_id, &b_id);
        a.set_rekey_policy(RekeyPolicy { msgs: REKEY_MSGS, secs: u64::MAX });
        let mut queue: Vec<Vec<u8>> = Vec::new();
        let mut post_close_decrypt_successes = 0u32;

        for op in ops {
            match op {
                Op::Send => {
                    match a.encrypt(b"prop") {
                        Ok(ct) => {
                            queue.push(ct);
                            if let Ok(Some(ctrl)) = a.maybe_rekey() {
                                queue.push(ctrl);
                            }
                        }
                        Err(_) => prop_assert!(a.is_closed()),
                    }
                }
                Op::Recv => {
                    if let Some(ct) = queue.first().cloned() {
                        queue.remove(0);
                        match b.decrypt(&ct) {
                            Ok(pt) => {
                                prop_assert!(!b.is_closed());
                                // Empty plaintext = a rekey control unit
                                // consumed; data always carries b"prop".
                                prop_assert!(pt.is_empty() || pt.as_slice() == b"prop");
                                if a.is_closed() || b.is_closed() {
                                    post_close_decrypt_successes += 1;
                                }
                            }
                            Err(_) => prop_assert!(b.is_closed()),
                        }
                    }
                }
                Op::CorruptRecv => {
                    if let Some(ct) = queue.first().cloned() {
                        queue.remove(0);
                        let mut bad = ct;
                        bad[0] ^= 0xA5;
                        prop_assert!(b.decrypt(&bad).is_err());
                        prop_assert!(b.is_closed());
                    }
                }
                Op::Close => {
                    b.close();
                    prop_assert!(b.is_closed());
                    prop_assert!(b.encrypt(b"x").is_err());
                    prop_assert!(b.decrypt(b"x").is_err());
                }
                Op::RekeyCheck => {
                    // V4: rekey_count <= floor(sent / threshold) + 1
                    prop_assert!(
                        a.rekey_count() <= a.sent_count() / REKEY_MSGS + 1,
                        "rekey_count {} exceeds bound for sent {}",
                        a.rekey_count(),
                        a.sent_count()
                    );
                }
                Op::BadPinHandshake => {
                    // V3: a session with a mismatched pin NEVER reaches Trusted.
                    let c_id = Identity::generate().unwrap();
                    let mut na = Session::initiator(&a_id).unwrap();
                    let mut nb = Session::responder(&b_id).unwrap();
                    nb.accept_handshake(na.produce_handshake().unwrap()).unwrap();
                    na.accept_handshake(nb.produce_handshake().unwrap()).unwrap();
                    let is_mismatch = matches!(
                        na.verify_peer_id(&c_id.peer_id()),
                        Err(Error::PeerIdMismatch { .. })
                    );
                    prop_assert!(is_mismatch);
                    prop_assert!(!matches!(na.state(), SessionState::Trusted(_)));
                    prop_assert!(na.produce_handshake().is_err());
                    // …and the responder side pins independently: a correctly
                    // pinned initiator produces a valid Msg3, the responder's
                    // wrong pin still kills it.
                    let mut na2 = Session::initiator(&a_id).unwrap();
                    let mut nb2 = Session::responder(&b_id).unwrap();
                    nb2.accept_handshake(na2.produce_handshake().unwrap()).unwrap();
                    na2.accept_handshake(nb2.produce_handshake().unwrap()).unwrap();
                    na2.verify_peer_id(&b_id.peer_id()).unwrap();
                    nb2.accept_handshake(na2.produce_handshake().unwrap()).unwrap();
                    let wrong = PeerId::random();
                    let is_mismatch = matches!(
                        nb2.verify_peer_id(&wrong),
                        Err(Error::PeerIdMismatch { .. })
                    );
                    prop_assert!(is_mismatch);
                    prop_assert!(!matches!(nb2.state(), SessionState::Trusted(_)));
                }
            }
        }

        // V1: after Close, zero successful decrypts — drain whatever remains.
        if b.is_closed() {
            for ct in queue {
                prop_assert!(b.decrypt(&ct).is_err());
            }
            prop_assert_eq!(post_close_decrypt_successes, 0);
        }
    }
}
