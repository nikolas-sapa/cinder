use bytes::BytesMut;
use cinder_core::frame::{wire_deserialize, wire_serialize, FrameCodec, FrameError};
use cinder_core::padding::{decode_plaintext, encode_plaintext};
use cinder_core::proto::{caps, ClientMsg, ErrorCode, HandshakeMsg, PeerId, ServerMsg};
use proptest::prelude::*;
use tokio_util::codec::{Decoder, Encoder};

fn hello() -> ClientMsg {
    ClientMsg::Hello {
        version: 1,
        peer_id: PeerId::random(),
    }
}

fn frame_bytes<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let payload = wire_serialize(msg).unwrap();
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

// F1 — oversize handshake-phase frame rejected pre-alloc, on the header alone.
#[test]
fn f1_oversize_hello_frame_rejected_pre_alloc() {
    let mut codec = FrameCodec::<ClientMsg>::pre_hello();
    let mut buf = BytesMut::from(&4097u32.to_be_bytes()[..]);
    match codec.decode(&mut buf) {
        Err(FrameError::Oversize { len, cap }) => {
            assert_eq!(len, 4097);
            assert_eq!(cap as usize, caps::MAX_FRAME_HELLO);
        }
        other => panic!("expected Oversize, got {other:?}"),
    }
}

// F2 — oversize transport-phase frame rejected pre-alloc.
#[test]
fn f2_oversize_transport_frame_rejected_pre_alloc() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let mut buf = BytesMut::from(&16385u32.to_be_bytes()[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(FrameError::Oversize { .. })
    ));
}

// F3 — distinguishes "validate then alloc" from "alloc then validate":
// exactly-at-cap header with no body waits; over-cap header errors alone.
#[test]
fn f3_len_validated_before_alloc() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let mut buf = BytesMut::from(&16384u32.to_be_bytes()[..]);
    assert!(matches!(codec.decode(&mut buf), Ok(None)));
    let mut buf = BytesMut::from(&16385u32.to_be_bytes()[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(FrameError::Oversize { .. })
    ));
}

// len == 0 → ProtocolViolation (§2 step 3).
#[test]
fn zero_length_frame_is_protocol_violation() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let mut buf = BytesMut::from(&0u32.to_be_bytes()[..]);
    match codec.decode(&mut buf) {
        Err(FrameError::Violation(_)) => {}
        other => panic!("expected Violation, got {other:?}"),
    }
    assert_eq!(
        FrameError::Violation("x").code(),
        ErrorCode::ProtocolViolation
    );
}

// Oversize map to the §10 close-code.
#[test]
fn oversize_maps_to_error_code() {
    assert_eq!(
        FrameError::Oversize { len: 1, cap: 0 }.code(),
        ErrorCode::OversizeFrame
    );
    assert!(ErrorCode::OversizeFrame.closes_connection());
}

// Exact cap boundaries: a serialized frame at exactly the cap decodes.
#[test]
fn frame_at_exact_cap_decodes() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let msg = ClientMsg::Transport {
        to: PeerId::random(),
        session_id: cinder_core::proto::SessionId::random(),
        body: vec![0xAB; 8200],
    };
    let bytes = frame_bytes(&msg);
    assert!(bytes.len() - 4 <= caps::MAX_FRAME_TRANSPORT);
    let mut buf = BytesMut::from(&bytes[..]);
    assert_eq!(codec.decode(&mut buf).unwrap(), Some(msg));
}

// Phase upgrade: pre-hello codec rejects a >4KiB frame, accepts it after
// set_max_frame bumps to the transport cap.
#[test]
fn phase_cap_upgrade() {
    let msg = ClientMsg::PresenceSubscribe {
        peers: vec![PeerId::random(); 200],
    };
    let bytes = frame_bytes(&msg);
    assert!(bytes.len() - 4 > caps::MAX_FRAME_HELLO);

    let mut codec = FrameCodec::<ClientMsg>::pre_hello();
    let mut buf = BytesMut::from(&bytes[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(FrameError::Oversize { .. })
    ));

    let mut codec = FrameCodec::<ClientMsg>::pre_hello();
    codec.set_max_frame(caps::MAX_FRAME_TRANSPORT as u32);
    let mut buf = BytesMut::from(&bytes[..]);
    assert_eq!(codec.decode(&mut buf).unwrap(), Some(msg));
}

// Trailing bytes after a valid message → ProtocolViolation (§2 step 7).
#[test]
fn trailing_bytes_rejected() {
    let payload = wire_serialize(&hello()).unwrap();
    let mut padded = payload.clone();
    padded.push(0x00);
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&(padded.len() as u32).to_be_bytes());
    buf.extend_from_slice(&padded);
    let mut codec = FrameCodec::<ClientMsg>::transport();
    assert!(matches!(codec.decode(&mut buf), Err(FrameError::Malformed)));
    assert!(wire_deserialize::<ClientMsg>(&padded).is_err());
}

// Encode → decode roundtrip through the codec itself.
#[test]
fn codec_roundtrip() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let msg = ClientMsg::Handshake {
        to: PeerId::random(),
        hs_id: cinder_core::proto::HsId::random(),
        body: HandshakeMsg::Msg1(vec![7; 32]),
    };
    let mut buf = BytesMut::new();
    codec.encode(msg.clone(), &mut buf).unwrap();
    assert_eq!(codec.decode(&mut buf).unwrap(), Some(msg));
    assert!(buf.is_empty());
}

// Split delivery: header and body arriving in pieces must reassemble.
#[test]
fn fragmented_frame_reassembles() {
    let mut codec = FrameCodec::<ClientMsg>::transport();
    let msg = hello();
    let bytes = frame_bytes(&msg);
    let mut buf = BytesMut::new();
    for chunk in bytes.chunks(7) {
        buf.extend_from_slice(chunk);
        match codec.decode(&mut buf) {
            Ok(Some(decoded)) => {
                assert_eq!(decoded, msg);
                return;
            }
            Ok(None) => {}
            Err(e) => panic!("decode error: {e}"),
        }
    }
    panic!("frame never completed");
}

// F5 — padding: block sizes are powers of two ≥256, prefix sits outside.
#[test]
fn f5_padding_pow2_buckets() {
    let lens: Vec<usize> = (1..=8192)
        .step_by(37)
        .chain([1, 255, 256, 257, 4095, 4096, 4097, 8191, 8192])
        .collect();
    for len in lens {
        let pt = vec![0xAAu8; len];
        let padded = encode_plaintext(&pt).unwrap();
        let block = padded.len() - 2;
        assert!(
            matches!(block, 256 | 512 | 1024 | 2048 | 4096 | 8192),
            "pt_len {len} → block {block}"
        );
        assert!(block >= len);
        if len > 256 {
            assert!(block < 2 * len, "pt_len {len} → block {block}");
        }
        assert_eq!(&padded[..2], &(len as u16).to_be_bytes());
        assert!(padded[2 + len..].iter().all(|b| *b == 0));
        assert_eq!(decode_plaintext(&padded).unwrap(), &pt[..]);
    }
}

// Padding: the 8191–8192 edge that broke the old inside-the-bucket layout.
#[test]
fn padding_edge_8191_8192() {
    for len in [8191usize, 8192] {
        let pt = vec![0x11u8; len];
        let padded = encode_plaintext(&pt).unwrap();
        assert_eq!(padded.len(), 2 + 8192);
        assert_eq!(decode_plaintext(&padded).unwrap(), &pt[..]);
    }
}

// Unpad validation: wrong block size, nonzero pad, truncated unit all fail.
#[test]
fn unpad_validation() {
    let padded = encode_plaintext(b"hello").unwrap();
    assert!(decode_plaintext(&padded[..padded.len() - 1]).is_err());

    let mut tampered = padded.clone();
    let last = tampered.len() - 1;
    tampered[last] = 0x01;
    assert!(decode_plaintext(&tampered).is_err());

    assert!(decode_plaintext(&[0x00]).is_err());
    assert!(decode_plaintext(&[]).is_err());
}

// Empty plaintext pads to the minimum bucket.
#[test]
fn pad_empty() {
    let padded = encode_plaintext(&[]).unwrap();
    assert_eq!(padded.len(), 2 + 256);
    assert!(decode_plaintext(&padded).unwrap().is_empty());
}

// ErrorCode wire contract: positional variant index == §10 numeric code.
#[test]
fn error_code_wire_values() {
    let cases = [
        (ErrorCode::VersionMismatch, 0x0001u32),
        (ErrorCode::OversizeFrame, 0x0002),
        (ErrorCode::RateLimited, 0x0003),
        (ErrorCode::UnknownMailbox, 0x0004),
        (ErrorCode::BadToken, 0x0005),
        (ErrorCode::MailboxFull, 0x0006),
        (ErrorCode::HandshakeTimeout, 0x0007),
        (ErrorCode::ProtocolViolation, 0x0008),
        (ErrorCode::InviteInvalid, 0x0009),
        (ErrorCode::PeerIdInUse, 0x000A),
        (ErrorCode::MailboxExists, 0x000B),
    ];
    for (code, expected) in cases {
        assert_eq!(code.code() as u32, expected);
        let bytes = wire_serialize(&code).unwrap();
        assert_eq!(bytes, expected.to_le_bytes(), "{code:?} wire value");
        assert_eq!(wire_deserialize::<ErrorCode>(&bytes).unwrap(), code);
    }
}

fn arb_peer_id() -> BoxedStrategy<PeerId> {
    any::<[u8; 32]>().prop_map(PeerId::from_bytes).boxed()
}

fn arb_client_msg() -> impl Strategy<Value = ClientMsg> {
    let peer = arb_peer_id();
    let hs_id = any::<[u8; 16]>().prop_map(cinder_core::proto::HsId::from_bytes);
    let session_id = any::<[u8; 16]>().prop_map(cinder_core::proto::SessionId::from_bytes);
    let token = any::<[u8; 32]>().prop_map(cinder_core::proto::Token::from_bytes);
    let token_hash = any::<[u8; 32]>().prop_map(cinder_core::proto::TokenHash::from_bytes);
    let mailbox_id = any::<[u8; 32]>().prop_map(cinder_core::proto::MailboxId::from_bytes);
    let invite = any::<[u8; 16]>().prop_map(cinder_core::proto::InviteToken::from_bytes);
    let hs_msg = prop_oneof![
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg1),
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg2),
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg3),
    ];

    prop_oneof![
        (any::<u16>(), peer.clone())
            .prop_map(|(version, peer_id)| ClientMsg::Hello { version, peer_id }),
        (invite.clone(), any::<u32>())
            .prop_map(|(token, ttl_secs)| ClientMsg::InviteCreate { token, ttl_secs }),
        (invite.clone(), hs_id.clone(), any::<Vec<u8>>())
            .prop_map(|(token, hs_id, msg1)| ClientMsg::InviteAccept { token, hs_id, msg1 }),
        (peer.clone(), hs_id.clone(), hs_msg.clone())
            .prop_map(|(to, hs_id, body)| ClientMsg::Handshake { to, hs_id, body }),
        (peer.clone(), session_id.clone(), any::<Vec<u8>>()).prop_map(|(to, session_id, body)| {
            ClientMsg::Transport {
                to,
                session_id,
                body,
            }
        }),
        (
            mailbox_id.clone(),
            token_hash.clone(),
            token_hash.clone(),
            any::<u32>()
        )
            .prop_map(|(mailbox_id, h_deposit, h_fetch, ttl_secs)| {
                ClientMsg::MailboxOpen {
                    mailbox_id,
                    h_deposit,
                    h_fetch,
                    ttl_secs,
                }
            }),
        (
            mailbox_id.clone(),
            token.clone(),
            session_id.clone(),
            any::<Vec<u8>>()
        )
            .prop_map(
                |(mailbox_id, token, session_id, body)| ClientMsg::MailboxDeposit {
                    mailbox_id,
                    token,
                    env: cinder_core::proto::DepositEnvelope { session_id, body },
                }
            ),
        (mailbox_id.clone(), token.clone(), any::<u16>()).prop_map(
            |(mailbox_id, token, max_msgs)| ClientMsg::MailboxFetch {
                mailbox_id,
                token,
                max_msgs
            }
        ),
        (mailbox_id.clone(), token.clone(), any::<Vec<u64>>()).prop_map(
            |(mailbox_id, token, msg_ids)| ClientMsg::MailboxAck {
                mailbox_id,
                token,
                msg_ids
            }
        ),
        (
            mailbox_id.clone(),
            token.clone(),
            token_hash.clone(),
            token_hash.clone()
        )
            .prop_map(|(mailbox_id, token, new_h_deposit, new_h_fetch)| {
                ClientMsg::MailboxRotate {
                    mailbox_id,
                    token,
                    new_h_deposit,
                    new_h_fetch,
                }
            }),
        (mailbox_id.clone(), token.clone())
            .prop_map(|(mailbox_id, token)| ClientMsg::MailboxClose { mailbox_id, token }),
        prop::collection::vec(arb_peer_id(), 0..300)
            .prop_map(|peers| ClientMsg::PresenceSubscribe { peers }),
        any::<u64>().prop_map(|nonce| ClientMsg::Ping { nonce }),
    ]
}

fn arb_server_msg() -> impl Strategy<Value = ServerMsg> {
    let peer = arb_peer_id();
    let hs_id = any::<[u8; 16]>().prop_map(cinder_core::proto::HsId::from_bytes);
    let session_id = any::<[u8; 16]>().prop_map(cinder_core::proto::SessionId::from_bytes);
    let mailbox_id = any::<[u8; 32]>().prop_map(cinder_core::proto::MailboxId::from_bytes);
    let invite = any::<[u8; 16]>().prop_map(cinder_core::proto::InviteToken::from_bytes);
    let hs_msg = prop_oneof![
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg1),
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg2),
        any::<Vec<u8>>().prop_map(HandshakeMsg::Msg3),
    ];
    let env = (
        any::<u64>(),
        peer.clone(),
        session_id.clone(),
        any::<Vec<u8>>(),
        any::<u64>(),
    )
        .prop_map(|(msg_id, from, session_id, body, received_at)| {
            cinder_core::proto::StoredEnvelope {
                msg_id,
                from,
                session_id,
                body,
                received_at,
            }
        });
    let code = prop_oneof![
        Just(ErrorCode::VersionMismatch),
        Just(ErrorCode::OversizeFrame),
        Just(ErrorCode::RateLimited),
        Just(ErrorCode::UnknownMailbox),
        Just(ErrorCode::BadToken),
        Just(ErrorCode::MailboxFull),
        Just(ErrorCode::HandshakeTimeout),
        Just(ErrorCode::ProtocolViolation),
        Just(ErrorCode::InviteInvalid),
        Just(ErrorCode::PeerIdInUse),
        Just(ErrorCode::MailboxExists),
    ];

    prop_oneof![
        (any::<u16>(), any::<u64>())
            .prop_map(|(version, now_unix)| ServerMsg::ServerHello { version, now_unix }),
        (invite, any::<u64>())
            .prop_map(|(token, expires_at)| ServerMsg::InviteCreated { token, expires_at }),
        (peer.clone(), hs_id, hs_msg).prop_map(|(from, hs_id, body)| ServerMsg::Handshake {
            from,
            hs_id,
            body
        }),
        (peer.clone(), session_id, any::<Vec<u8>>()).prop_map(|(from, session_id, body)| {
            ServerMsg::Transport {
                from,
                session_id,
                body,
            }
        }),
        (mailbox_id, any::<u64>()).prop_map(|(mailbox_id, expires_at)| ServerMsg::MailboxOpened {
            mailbox_id,
            expires_at,
        }),
        any::<u64>().prop_map(|msg_id| ServerMsg::MailboxDeposited { msg_id }),
        (prop::collection::vec(env, 0..20), any::<bool>())
            .prop_map(|(msgs, more)| ServerMsg::MailboxMessages { msgs, more }),
        Just(ServerMsg::Ok),
        (peer.clone(), any::<bool>())
            .prop_map(|(peer, online)| ServerMsg::PeerStatus { peer, online }),
        any::<u64>().prop_map(|nonce| ServerMsg::Pong { nonce }),
        (code, any::<String>()).prop_map(|(code, detail)| ServerMsg::Error { code, detail }),
    ]
}

// F6 — every generated ClientMsg/ServerMsg survives encode→decode byte-identical.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn f6_client_msg_serde_roundtrip(msg in arb_client_msg()) {
        let bytes = wire_serialize(&msg).unwrap();
        prop_assert_eq!(wire_deserialize::<ClientMsg>(&bytes).unwrap(), msg);
    }

    #[test]
    fn f6_server_msg_serde_roundtrip(msg in arb_server_msg()) {
        let bytes = wire_serialize(&msg).unwrap();
        prop_assert_eq!(wire_deserialize::<ServerMsg>(&bytes).unwrap(), msg);
    }
}
