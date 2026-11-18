use std::net::SocketAddr;

use cinder_core::contacts::{ContactStore, TrustState};
use cinder_core::error::Error;
use cinder_core::frame::wire_deserialize;
use cinder_core::handshake_flow;
use cinder_core::identity::Identity;
use cinder_core::proto::{
    caps, ClientMsg, HandshakeMsg, HsId, InviteToken, PeerId, ServerMsg, SessionId,
};
use cinder_core::session::{CloseReason, RekeyPolicy, Sas, Session, SessionState};
use cinder_core::transport::{ClientEvent, Link, RelayTransport};
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::mpsc;

// ---- helpers ---------------------------------------------------------------

fn stepwise_handshake(
    a_id: &Identity,
    b_id: &Identity,
    pin_ab: &PeerId,
    pin_ba: Option<&PeerId>,
) -> (Session, Session) {
    let mut a = Session::initiator(a_id).unwrap();
    let mut b = Session::responder(b_id).unwrap();
    let m1 = a.produce_handshake().unwrap();
    b.accept_handshake(m1).unwrap();
    let m2 = b.produce_handshake().unwrap();
    a.accept_handshake(m2).unwrap();
    a.verify_peer_id(pin_ab).unwrap();
    let m3 = a.produce_handshake().unwrap();
    b.accept_handshake(m3).unwrap();
    match pin_ba {
        Some(p) => b.verify_peer_id(p).unwrap(),
        None => {
            b.accept_learned_peer().unwrap();
        }
    }
    a.finish_handshake().unwrap();
    b.finish_handshake().unwrap();
    (a, b)
}

fn fresh_pair() -> (Identity, Identity, Session, Session) {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let (a, b) = stepwise_handshake(&a_id, &b_id, &b_id.peer_id(), Some(&a_id.peer_id()));
    (a_id, b_id, a, b)
}

/// In-memory `Link` pair emulating the relay: a send on one end becomes a
/// `ClientEvent` on the other with `from` stamped (the relay's job — the
/// client only reads).
struct MemLink {
    me: PeerId,
    tx: mpsc::UnboundedSender<ClientEvent>,
    rx: mpsc::UnboundedReceiver<ClientEvent>,
}

fn mem_pair(a: PeerId, b: PeerId) -> (MemLink, MemLink) {
    let (tx_ab, rx_ab) = mpsc::unbounded_channel();
    let (tx_ba, rx_ba) = mpsc::unbounded_channel();
    (
        MemLink {
            me: a,
            tx: tx_ab,
            rx: rx_ba,
        },
        MemLink {
            me: b,
            tx: tx_ba,
            rx: rx_ab,
        },
    )
}

impl Link for MemLink {
    async fn send_handshake(
        &mut self,
        _to: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    ) -> Result<(), Error> {
        self.tx
            .send(ClientEvent::Handshake {
                from: self.me,
                hs_id,
                body,
            })
            .map_err(|_| Error::Transport("mem link closed".into()))
    }

    async fn send_invite_accept(
        &mut self,
        _token: InviteToken,
        hs_id: HsId,
        msg1: Vec<u8>,
    ) -> Result<(), Error> {
        // Relay consumes the slot and forwards the serialized Msg1 as a
        // Handshake frame to the slot owner.
        let body: HandshakeMsg = wire_deserialize(&msg1)?;
        self.tx
            .send(ClientEvent::Handshake {
                from: self.me,
                hs_id,
                body,
            })
            .map_err(|_| Error::Transport("mem link closed".into()))
    }

    async fn send_transport(
        &mut self,
        _to: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) -> Result<(), Error> {
        self.tx
            .send(ClientEvent::Transport {
                from: self.me,
                session_id,
                body,
            })
            .map_err(|_| Error::Transport("mem link closed".into()))
    }

    async fn recv(&mut self) -> Result<ClientEvent, Error> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| Error::Transport("mem link closed".into()))
    }
}

async fn spawn_relay() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = cinder_relay::server::serve(listener, cinder_relay::server::RelayConfig::default())
            .await;
    });
    addr
}

// ---- H1: XX both directions -------------------------------------------------

#[test]
fn xx_handshake_both_sides_trusted_and_roundtrip() {
    let (a_id, b_id, mut a, mut b) = fresh_pair();
    assert_eq!(a.state(), SessionState::Trusted(b_id.peer_id()));
    assert_eq!(b.state(), SessionState::Trusted(a_id.peer_id()));
    // remote static == peer's actual static on both sides
    assert_eq!(a.remote_static().unwrap(), b_id.public());
    assert_eq!(b.remote_static().unwrap(), a_id.public());
    // derived peer-id == contact identity both ways
    assert_eq!(a.remote_peer_id().unwrap(), b_id.peer_id());
    assert_eq!(b.remote_peer_id().unwrap(), a_id.peer_id());

    let ct = a.encrypt(b"ping").unwrap();
    assert_eq!(b.decrypt(&ct).unwrap(), b"ping");
    let ct2 = b.encrypt(b"pong").unwrap();
    assert_eq!(a.decrypt(&ct2).unwrap(), b"pong");
}

#[test]
fn handshake_messages_under_4kib_cap() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let mut a = Session::initiator(&a_id).unwrap();
    let mut b = Session::responder(&b_id).unwrap();
    let m1 = a.produce_handshake().unwrap();
    let m2 = {
        b.accept_handshake(m1.clone()).unwrap();
        b.produce_handshake().unwrap()
    };
    let m3 = {
        a.accept_handshake(m2.clone()).unwrap();
        a.verify_peer_id(&b_id.peer_id()).unwrap();
        a.produce_handshake().unwrap()
    };
    for m in [&m1, &m2, &m3] {
        let len = match m {
            HandshakeMsg::Msg1(p) | HandshakeMsg::Msg2(p) | HandshakeMsg::Msg3(p) => p.len(),
        };
        assert!(len <= caps::MAX_FRAME_HANDSHAKE, "hs msg {len} > cap");
    }
}

// ---- H2/H3: pinning both directions -----------------------------------------

#[test]
fn initiator_wrong_pin_aborts_before_msg3() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap(); // pinned, never answers
    let mut a = Session::initiator(&a_id).unwrap();
    let mut b = Session::responder(&b_id).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    a.accept_handshake(b.produce_handshake().unwrap()).unwrap();
    match a.verify_peer_id(&c_id.peer_id()) {
        Err(Error::PeerIdMismatch {
            expected,
            actual,
            actual_static,
        }) => {
            assert_eq!(expected, c_id.peer_id());
            assert_eq!(actual, b_id.peer_id());
            assert_eq!(actual_static, b_id.public());
        }
        other => panic!("expected PeerIdMismatch, got {other:?}"),
    }
    assert_eq!(a.state(), SessionState::Closed(CloseReason::PeerIdMismatch));
    // Msg3 must be impossible to produce now.
    assert!(a.produce_handshake().is_err());
    assert!(a.encrypt(b"x").is_err());
    assert!(a.decrypt(b"x").is_err());
    assert!(a.finish_handshake().is_err());
}

#[test]
fn responder_wrong_pin_aborts_after_msg3() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap();
    let mut a = Session::initiator(&a_id).unwrap();
    let mut b = Session::responder(&b_id).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    a.accept_handshake(b.produce_handshake().unwrap()).unwrap();
    a.verify_peer_id(&b_id.peer_id()).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    match b.verify_peer_id(&c_id.peer_id()) {
        Err(Error::PeerIdMismatch { .. }) => {}
        other => panic!("expected PeerIdMismatch, got {other:?}"),
    }
    assert_eq!(b.state(), SessionState::Closed(CloseReason::PeerIdMismatch));
    assert!(b.finish_handshake().is_err());
    assert!(b.encrypt(b"x").is_err());
}

#[test]
fn mitm_static_substitution_detected_every_time() {
    for _ in 0..50 {
        let a_id = Identity::generate().unwrap();
        let b_id = Identity::generate().unwrap();
        let attacker = Identity::generate().unwrap();
        let mut a = Session::initiator(&a_id).unwrap();
        let mut b = Session::responder(&b_id).unwrap();
        b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
        a.accept_handshake(b.produce_handshake().unwrap()).unwrap();
        // Pin the attacker's peer-id: check must fail (remote is B).
        assert!(matches!(
            a.verify_peer_id(&attacker.peer_id()),
            Err(Error::PeerIdMismatch { .. })
        ));
        assert!(!matches!(a.state(), SessionState::Trusted(_)));
    }
}

#[test]
fn trusted_unreachable_without_pin_check() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let mut a = Session::initiator(&a_id).unwrap();
    let mut b = Session::responder(&b_id).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    a.accept_handshake(b.produce_handshake().unwrap()).unwrap();
    // Skip verify_peer_id entirely: Msg3 gated, finish refused.
    assert!(a.produce_handshake().is_err());
    assert!(a.finish_handshake().is_err());
    a.verify_peer_id(&b_id.peer_id()).unwrap();
    let m3 = a.produce_handshake().unwrap();
    assert!(matches!(m3, HandshakeMsg::Msg3(_)));
    // Responder skipping verify: finish refused too.
    b.accept_handshake(m3).unwrap();
    assert!(b.finish_handshake().is_err());
    b.accept_learned_peer().unwrap();
    b.finish_handshake().unwrap();
    assert!(matches!(b.state(), SessionState::Trusted(_)));
}

// ---- SAS --------------------------------------------------------------------

#[test]
fn sas_identical_on_both_sides() {
    let (_, _, a, b) = fresh_pair();
    let sa = a.sas().unwrap();
    let sb = b.sas().unwrap();
    assert_eq!(sa.digest(), sb.digest());
    assert_eq!(sa.emoji(), sb.emoji());
    assert_eq!(sa.digits(), sb.digits());
    assert_eq!(sa.emoji().split(' ').count(), 7);
    assert_eq!(a.handshake_hash().unwrap(), b.handshake_hash().unwrap());
}

#[test]
fn sas_order_symmetric() {
    let hh = [0xAB; 32];
    for _ in 0..100 {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let s1 = Sas::derive(&hh, &a.public(), &b.public());
        let s2 = Sas::derive(&hh, &b.public(), &a.public());
        assert_eq!(s1.digest(), s2.digest());
        assert_eq!(s1.emoji(), s2.emoji());
        assert_eq!(s1.digits(), s2.digits());
    }
}

#[test]
fn sas_hand_vectors() {
    // Inlined copy of the session's emoji table — the point of a hand-vector
    // test is that the algorithm is pinned, not the table location.
    const EMOJI: [&str; 64] = [
        "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵",
        "🐔", "🐧", "🐦", "🐤", "🦆", "🦅", "🦉", "🦇", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋",
        "🐌", "🐞", "🐜", "🐢", "🐍", "🦎", "🐙", "🦑", "🦐", "🦀", "🐡", "🐠", "🐟", "🐬", "🐳",
        "🐋", "🦈", "🐊", "🐅", "🐆", "🦓", "🦍", "🐘", "🦛", "🦏", "🐪", "🐫", "🦒", "🦘", "🐃",
        "🐂", "🐄", "🐎", "🐖",
    ];
    // Fixtures: (handshake_hash, pk_a, pk_b).
    let fixtures: [([u8; 32], [u8; 32], [u8; 32]); 5] = [
        ([0x00; 32], [0x01; 32], [0x02; 32]),
        ([0xFF; 32], [0x10; 32], [0x20; 32]),
        (std::array::from_fn(|i| i as u8), [0xAA; 32], [0x55; 32]),
        ([0x77; 32], [0x00; 32], [0xFF; 32]),
        ([0x42; 32], [0x42; 32], [0x42; 32]),
    ];
    for (hh, a, b) in fixtures {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let mut digest_input = Vec::with_capacity(96);
        digest_input.extend_from_slice(&hh);
        digest_input.extend_from_slice(&lo);
        digest_input.extend_from_slice(&hi);
        let digest: [u8; 32] = Sha256::digest(&digest_input).into();

        let sas = Sas::derive(&hh, &a, &b);
        assert_eq!(sas.digest(), &digest);

        let expected_emoji: String = (0..7)
            .map(|i| EMOJI[(digest[i] & 0x3f) as usize])
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(sas.emoji(), expected_emoji);

        let raw: String = digest[..20].iter().map(|b| format!("{b:03}")).collect();
        let expected_digits: String = raw
            .as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(sas.digits(), expected_digits);
    }
}

#[test]
fn sas_distinctness_sanity() {
    let hh = [0x5A; 32];
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        seen.insert(Sas::derive(&hh, &a.public(), &b.public()).emoji());
    }
    assert!(seen.len() >= 990, "too many sas collisions: {}", seen.len());
}

// ---- SE2/SE3: decrypt-fatal ---------------------------------------------------

#[test]
fn decrypt_failure_is_fatal_exactly_once() {
    let (_, _, mut a, mut b) = fresh_pair();
    let good = a.encrypt(b"hello").unwrap();
    let mut bad = good.clone();
    bad[3] ^= 0xFF;

    let mut errors = 0;
    if b.decrypt(&bad).is_err() {
        errors += 1;
    }
    assert_eq!(errors, 1, "exactly one surfaced error, no retry path");
    assert!(b.is_closed());
    assert_eq!(b.state(), SessionState::Closed(CloseReason::DecryptFatal));

    for _ in 0..10 {
        assert!(b.decrypt(&good).is_err());
        assert!(b.encrypt(b"x").is_err());
        assert!(b.produce_handshake().is_err());
        assert!(b.accept_handshake(HandshakeMsg::Msg1(vec![])).is_err());
        assert!(b.verify_peer_id(&PeerId::random()).is_err());
        assert!(b.finish_handshake().is_err());
    }
}

#[test]
fn no_decrypt_after_close_100_frames() {
    let (_, _, mut a, mut b) = fresh_pair();
    let cts: Vec<Vec<u8>> = (0..100)
        .map(|i| a.encrypt(format!("m{i}").as_bytes()).unwrap())
        .collect();
    let mut bad = cts[0].clone();
    bad[0] ^= 1;
    assert!(b.decrypt(&bad).is_err());
    let mut successes = 0;
    for ct in &cts {
        if b.decrypt(ct).is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 0);
}

#[test]
fn malformed_padded_unit_rejected_before_plaintext() {
    // §2/§11: a pt_len whose implied block size mismatches, or nonzero padding
    // bytes, is an integrity failure routed to the same fatal path as AEAD.
    let mut unit = vec![0u8; 2 + 256];
    unit[0] = 0;
    unit[1] = 10;
    unit[2..12].copy_from_slice(b"0123456789");
    unit[100] = 0x01; // nonzero padding byte
    assert!(cinder_core::padding::decode_plaintext(&unit).is_err());
    let mut wrong_len = vec![0u8; 2 + 128];
    wrong_len[0] = 0;
    wrong_len[1] = 10; // implies 256 block, unit is 128
    assert!(cinder_core::padding::decode_plaintext(&wrong_len).is_err());
}

// ---- SE1: monotonic nonces ----------------------------------------------------

#[test]
fn two_hundred_messages_no_desync() {
    let (_, _, mut a, mut b) = fresh_pair();
    for i in 0..200u32 {
        let pt = format!("msg-{i}");
        let ct = a.encrypt(pt.as_bytes()).unwrap();
        assert_eq!(b.decrypt(&ct).unwrap(), pt.as_bytes());
    }
    assert_eq!(a.sent_count(), 200);
    assert_eq!(b.received_count(), 200);
    assert_eq!(a.sending_nonce(), Some(200));
    // replayed ciphertext rejected (AEAD desync) and kills the session
    let ct = a.encrypt(b"one more").unwrap();
    let dup = ct.clone();
    assert!(b.decrypt(&ct).is_ok());
    assert!(b.decrypt(&dup).is_err());
    assert!(b.is_closed());
}

#[test]
fn plaintext_cap_enforced() {
    let (_, _, mut a, _) = fresh_pair();
    assert!(a.encrypt(&vec![0u8; caps::MAX_PLAINTEXT]).is_ok());
    assert!(a.encrypt(&vec![0u8; caps::MAX_PLAINTEXT + 1]).is_err());
    assert!(
        !a.is_closed(),
        "oversize plaintext is a caller error, not fatal"
    );
}

// ---- identity ↔ session / key-change ------------------------------------------

#[test]
fn session_remote_static_matches_contact_pubkey() {
    let dir = TempDir::new().unwrap();
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let mut store = ContactStore::open(dir.path(), a_id.private().expose_secret()).unwrap();
    store.add_unverified(b_id.public(), "b").unwrap();
    let (a, _b) = stepwise_handshake(&a_id, &b_id, &b_id.peer_id(), None);
    let contact = store.get(&b_id.peer_id()).unwrap();
    assert_eq!(a.remote_static().unwrap(), contact.static_pubkey);
    assert_eq!(a.remote_peer_id().unwrap(), contact.peer_id);
}

#[test]
fn responder_rekey_detected_via_contacts_path() {
    let dir = TempDir::new().unwrap();
    let a_id = Identity::generate().unwrap();
    let b_old = Identity::generate().unwrap();
    let mut store = ContactStore::open(dir.path(), a_id.private().expose_secret()).unwrap();
    store.add_unverified(b_old.public(), "b").unwrap();

    // B re-keys: next handshake presents a different static.
    let b_new = Identity::generate().unwrap();
    let mut a = Session::initiator(&a_id).unwrap();
    let mut b = Session::responder(&b_new).unwrap();
    b.accept_handshake(a.produce_handshake().unwrap()).unwrap();
    a.accept_handshake(b.produce_handshake().unwrap()).unwrap();

    // Pin vs the contact's peer-id fails loudly…
    assert!(matches!(
        a.verify_peer_id(&b_old.peer_id()),
        Err(Error::PeerIdMismatch { .. })
    ));
    // …and the contacts path records the key change + blocks send.
    assert!(a.detect_key_change(&mut store, &b_old.peer_id()).unwrap());
    assert!(store.is_send_blocked(&b_old.peer_id()));
    match &store.get(&b_old.peer_id()).unwrap().state {
        TrustState::KeyChanged { old, dismissed } => {
            assert_eq!(old, &b_old.public());
            assert!(!dismissed);
        }
        other => panic!("expected KeyChanged, got {other:?}"),
    }
}

#[test]
fn same_static_is_not_a_key_change() {
    let dir = TempDir::new().unwrap();
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let mut store = ContactStore::open(dir.path(), a_id.private().expose_secret()).unwrap();
    store.add_unverified(b_id.public(), "b").unwrap();
    let (a, _b) = stepwise_handshake(&a_id, &b_id, &b_id.peer_id(), None);
    assert!(!a.detect_key_change(&mut store, &b_id.peer_id()).unwrap());
    assert!(!store.is_send_blocked(&b_id.peer_id()));
}

// ---- reconnect ----------------------------------------------------------------

#[test]
fn dead_session_rehandshakes_to_fresh_session_id() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let (mut a1, _b1) = stepwise_handshake(&a_id, &b_id, &b_id.peer_id(), None);
    let sid1 = a1.session_id().unwrap();
    a1.close();
    assert!(a1.is_closed());
    assert!(a1.encrypt(b"x").is_err());

    let (mut a2, mut b2) = stepwise_handshake(&a_id, &b_id, &b_id.peer_id(), None);
    let sid2 = a2.session_id().unwrap();
    assert_ne!(sid1, sid2, "new handshake must mint a new session_id");
    // Fresh nonce stream: first message of new session decrypts fine.
    let ct = a2.encrypt(b"after reconnect").unwrap();
    assert_eq!(b2.decrypt(&ct).unwrap(), b"after reconnect");
    assert_eq!(a2.sending_nonce(), Some(1));
}

// ---- rekey policy ---------------------------------------------------------------

/// One send step: encrypt → deliver data → drain any rekey control. Control
/// frames always travel AFTER the data frame that triggered them and BEFORE
/// the next data frame — TCP order preserves the boundary.
fn send_one(a: &mut Session, b: &mut Session, pt: &[u8]) {
    let ct = a.encrypt(pt).unwrap();
    let ctrl = a.maybe_rekey().unwrap();
    b.decrypt(&ct).unwrap();
    if let Some(c) = ctrl {
        assert!(b.decrypt(&c).unwrap().is_empty(), "control unit consumed");
    }
}

#[test]
fn rekey_trigger_counts_messages_and_time() {
    let (_, _, mut a, _) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy { msgs: 4, secs: 600 });
    for _ in 0..4 {
        a.encrypt(b"x").unwrap();
    }
    assert_eq!(a.rekey_count(), 0, "rekey is caller-driven via maybe_rekey");
    assert!(a.maybe_rekey().unwrap().is_some());
    assert_eq!(a.rekey_count(), 1);
    for _ in 0..4 {
        a.encrypt(b"x").unwrap();
    }
    assert!(a.maybe_rekey().unwrap().is_some());
    assert_eq!(a.rekey_count(), 2);
    assert!(!a.rekey_needed());

    let (_, _, mut t, _) = fresh_pair();
    t.set_rekey_policy(RekeyPolicy {
        msgs: u64::MAX,
        secs: 0,
    });
    assert!(t.rekey_needed(), "secs:0 window already elapsed");
    assert!(t.maybe_rekey().unwrap().is_some());
    assert_eq!(t.rekey_count(), 1);
}

// ---- M6: ratchet-lite rekey ---------------------------------------------------

#[test]
fn rekey_msg_threshold_rotates_keys() {
    let (_, _, mut a, mut b) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy {
        msgs: 4,
        secs: u64::MAX,
    });
    // Parallel session on the same plaintexts with rekey disabled.
    let (_, _, mut a2, mut b2) = fresh_pair();
    a2.set_rekey_policy(RekeyPolicy {
        msgs: u64::MAX,
        secs: u64::MAX,
    });

    let msgs: [&[u8]; 4] = [b"m0", b"m1", b"m2", b"m3"];
    for m in msgs {
        send_one(&mut a, &mut b, m);
        let ct2 = a2.encrypt(m).unwrap();
        b2.decrypt(&ct2).unwrap();
    }
    assert_eq!(a.rekey_count(), 1, "boundary hit after 4 sends");
    assert_eq!(b.rekey_count(), 1, "receiver applied rekey on control");

    // Msg 5+ decrypt fine in both directions on the rekeyed session…
    send_one(&mut a, &mut b, b"m4-post-rekey");
    send_one(&mut b, &mut a, b"ack-post-rekey");

    // …but the 5th wire frame of the rekeyed stream is NOT interchangeable
    // with the no-rekey stream: rotated keys, different ciphertext.
    let ct_rekeyed = a.encrypt(b"same-pt").unwrap();
    let ct_plain = a2.encrypt(b"same-pt").unwrap();
    assert_ne!(ct_rekeyed, ct_plain, "post-rekey ciphertext must differ");
    // The no-rekey peer cannot read the rekeyed frame at all.
    assert!(b2.decrypt(&ct_rekeyed).is_err());
    assert_eq!(b2.state(), SessionState::Closed(CloseReason::DecryptFatal));
}

#[test]
fn rekey_time_trigger_emits_control_and_peer_applies() {
    let (_, _, mut a, mut b) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy {
        msgs: u64::MAX,
        secs: 0,
    });
    let ct = a.encrypt(b"before").unwrap();
    let ctrl = a
        .maybe_rekey()
        .unwrap()
        .expect("secs:0 fires on elapsed window");
    b.decrypt(&ct).unwrap();
    assert!(b.decrypt(&ctrl).unwrap().is_empty());
    assert_eq!(a.rekey_count(), 1);
    assert_eq!(b.rekey_count(), 1);
    send_one(&mut a, &mut b, b"after");
    send_one(&mut b, &mut a, b"back");
}

#[test]
fn rekey_desync_is_fatal() {
    let (_, _, mut a, mut b) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy {
        msgs: 2,
        secs: u64::MAX,
    });
    send_one(&mut a, &mut b, b"one");
    let ct = a.encrypt(b"two").unwrap();
    b.decrypt(&ct).unwrap();
    // Boundary fires; the control frame is dropped — a rekeyed egress, b
    // kept the old ingress key. Next frame MUST fail fatally (§11).
    let dropped = a.maybe_rekey().unwrap().expect("boundary");
    drop(dropped);
    let ct = a.encrypt(b"post-rekey").unwrap();
    assert!(b.decrypt(&ct).is_err());
    assert_eq!(b.state(), SessionState::Closed(CloseReason::DecryptFatal));
}

#[test]
fn rekey_200_msgs_zero_desync_monotonic_nonces() {
    let (_, _, mut a, mut b) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy {
        msgs: 16,
        secs: u64::MAX,
    });
    b.set_rekey_policy(RekeyPolicy {
        msgs: 16,
        secs: u64::MAX,
    });
    let mut last_a_nonce = 0u64;
    for i in 0..200u64 {
        let pt = format!("m{i}");
        let ct = a.encrypt(pt.as_bytes()).unwrap();
        let ctrl = a.maybe_rekey().unwrap();
        assert_eq!(b.decrypt(&ct).unwrap(), pt.as_bytes());
        if let Some(c) = ctrl {
            assert!(b.decrypt(&c).unwrap().is_empty());
        }
        let n = a.sending_nonce().unwrap();
        assert!(
            n > last_a_nonce,
            "nonce regressed at msg {i}: {n} <= {last_a_nonce}"
        );
        last_a_nonce = n;
        send_one(&mut b, &mut a, b"ack");
    }
    // 12 outbound boundaries + 12 inbound controls per side.
    assert_eq!(
        a.rekey_count(),
        24,
        "200 msgs / 16-per-window, both directions"
    );
    assert_eq!(b.rekey_count(), 24);
    assert_eq!(a.sent_count(), 200);
    assert_eq!(b.received_count(), 200);
    assert!(!a.is_closed() && !b.is_closed());
}

#[test]
fn rekey_preserves_pinned_identity_and_sas() {
    let (_, _, mut a, mut b) = fresh_pair();
    a.set_rekey_policy(RekeyPolicy {
        msgs: 2,
        secs: u64::MAX,
    });
    let sas_a = *a.sas().unwrap().digest();
    let sas_b = *b.sas().unwrap().digest();
    let peer_seen_by_a = a.remote_peer_id().unwrap();
    let static_seen_by_a = a.remote_static().unwrap();

    for i in 0..5u8 {
        send_one(&mut a, &mut b, &[i]);
    }
    assert!(a.rekey_count() >= 2 && b.rekey_count() >= 2);
    assert_eq!(a.sas().unwrap().digest(), &sas_a);
    assert_eq!(b.sas().unwrap().digest(), &sas_b);
    assert_eq!(a.remote_peer_id().unwrap(), peer_seen_by_a);
    assert_eq!(a.remote_static().unwrap(), static_seen_by_a);
}

// ---- async drivers over mem link -----------------------------------------------

#[tokio::test]
async fn drivers_complete_handshake_over_mem_link() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let (mut la, mut lb) = mem_pair(a_id.peer_id(), b_id.peer_id());
    let b_peer = b_id.peer_id();
    let (ra, rb) = tokio::join!(
        handshake_flow::initiator(&a_id, &mut la, b_peer, &b_peer, None),
        handshake_flow::responder(&b_id, &mut lb, |p| Some(*p)),
    );
    let mut a = ra.unwrap();
    let mut b = rb.unwrap();
    assert!(matches!(a.state(), SessionState::Trusted(_)));
    assert!(matches!(b.state(), SessionState::Trusted(_)));

    let ct = a.encrypt(b"via mem link").unwrap();
    la.send_transport(b_peer, a.session_id().unwrap(), ct)
        .await
        .unwrap();
    match lb.recv().await.unwrap() {
        ClientEvent::Transport {
            body, session_id, ..
        } => {
            assert_eq!(session_id, a.session_id().unwrap());
            assert_eq!(b.decrypt(&body).unwrap(), b"via mem link");
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn drivers_initiator_via_invite_folds_msg1() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let (mut la, mut lb) = mem_pair(a_id.peer_id(), b_id.peer_id());
    let b_peer = b_id.peer_id();
    let token = InviteToken::random();
    let (ra, rb) = tokio::join!(
        handshake_flow::initiator(&a_id, &mut la, b_peer, &b_peer, Some(token)),
        handshake_flow::responder(&b_id, &mut lb, |_| None), // TOFU
    );
    ra.unwrap();
    rb.unwrap();
}

#[tokio::test]
async fn driver_initiator_pin_mismatch_kills_session() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap();
    let (mut la, mut lb) = mem_pair(a_id.peer_id(), b_id.peer_id());
    let b_peer = b_id.peer_id();
    let wrong = c_id.peer_id();
    let init = handshake_flow::initiator(&a_id, &mut la, b_peer, &wrong, None);
    let resp = handshake_flow::responder(&b_id, &mut lb, |p| Some(*p));
    tokio::pin!(init);
    tokio::pin!(resp);
    // Initiator must error first; the responder future (awaiting a Msg3 that
    // per §6 is never sent) is then dropped rather than awaited to timeout.
    let res = tokio::select! {
        r = &mut init => r,
        r = &mut resp => panic!("responder completed first: {r:?}"),
    };
    assert!(matches!(res, Err(Error::PeerIdMismatch { .. })));
}

// ---- real relay E2E ---------------------------------------------------------------

#[tokio::test]
async fn e2e_xx_over_real_relay() {
    let addr = spawn_relay().await;
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let a_peer = a_id.peer_id();
    let b_peer = b_id.peer_id();
    let mut ta = RelayTransport::connect(addr, a_peer).await.unwrap();
    let mut tb = RelayTransport::connect(addr, b_peer).await.unwrap();
    assert_ne!(ta.connection_id(), tb.connection_id());

    let (ra, rb) = tokio::join!(
        handshake_flow::initiator(&a_id, &mut ta, b_peer, &b_peer, None),
        handshake_flow::responder(&b_id, &mut tb, |p| Some(*p)),
    );
    let mut a = ra.unwrap();
    let mut b = rb.unwrap();
    assert_eq!(a.state(), SessionState::Trusted(b_peer));
    assert_eq!(b.state(), SessionState::Trusted(a_peer));
    assert_eq!(a.sas().unwrap().emoji(), b.sas().unwrap().emoji());

    // Transport ciphertext routed by the relay.
    let ct = a.encrypt(b"e2e over relay").unwrap();
    let sid = a.session_id().unwrap();
    ta.send_transport(b_peer, sid, ct).await.unwrap();
    match tb.recv().await.unwrap() {
        ClientEvent::Transport {
            from,
            session_id,
            body,
        } => {
            assert_eq!(from, a_peer);
            assert_eq!(session_id, sid);
            assert_eq!(b.decrypt(&body).unwrap(), b"e2e over relay");
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}

// Wire-level: a ClientMsg::Handshake frame serializes to a ServerMsg::Handshake
// the relay stamps — sanity that Link typing matches the proto.
#[test]
fn handshake_wire_roundtrip_typing() {
    let msg = ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id: HsId::random(),
        body: HandshakeMsg::Msg2(vec![1, 2, 3]),
    };
    let bytes = cinder_core::frame::wire_serialize(&msg).unwrap();
    let back: ClientMsg = wire_deserialize(&bytes).unwrap();
    assert_eq!(msg, back);
    let evt: ClientEvent = ServerMsg::Handshake {
        from: PeerId::random(),
        hs_id: HsId::random(),
        body: HandshakeMsg::Msg3(vec![9]),
    }
    .into();
    assert!(matches!(evt, ClientEvent::Handshake { .. }));
}
