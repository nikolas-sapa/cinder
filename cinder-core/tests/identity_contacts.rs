#![forbid(unsafe_code)]
#![allow(dead_code)]

// These modules are compiled here directly so this suite is independent of
// lib.rs wiring order; they are also reachable via cinder_core::* once the
// modules are exported.
#[path = "../src/contacts.rs"]
mod contacts;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/frame.rs"]
mod frame;
#[path = "../src/identity.rs"]
mod identity;
#[path = "../src/keystore.rs"]
mod keystore;
#[path = "../src/proto.rs"]
mod proto;

use std::fs;

use contacts::{ContactStore, MailboxGrant, PendingContact, PendingState, TrustState};
use error::Error;
use identity::Identity;
use keystore::{AgeFileStore, KeyStore, KeyringStore};
use proto::{InviteToken, PeerId};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn hex32(s: &str) -> [u8; 32] {
    let v: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect();
    v.try_into().unwrap()
}

#[test]
fn peer_id_is_sha256_of_static_pubkey() {
    // Hand-computed: sha256(b"\x00\x01..\x1f") via independent implementation.
    let public: [u8; 32] = std::array::from_fn(|i| i as u8);
    let expected = hex32("630dcd2966c4336691125448bbb25b4ff412a49c732db2c8abc1b8581bd710dd");
    let mut blob = vec![0xabu8; 32];
    blob.extend_from_slice(&public);
    let id = Identity::from_stored_blob(&blob).unwrap();
    assert_eq!(id.public(), public);
    assert_eq!(id.peer_id(), PeerId::from_bytes(expected));

    let id = Identity::generate().unwrap();
    let digest: [u8; 32] = Sha256::digest(id.public()).into();
    assert_eq!(id.peer_id(), PeerId::from_bytes(digest));
}

#[test]
fn identity_blob_roundtrip_and_rejects_bad_len() {
    let id = Identity::generate().unwrap();
    let blob = id.stored_blob();
    assert_eq!(blob.expose_secret().len(), 64);
    let id2 = Identity::from_stored_blob(blob.expose_secret()).unwrap();
    assert_eq!(id2.public(), id.public());
    assert_eq!(id2.peer_id(), id.peer_id());
    assert!(Identity::from_stored_blob(&[0u8; 63]).is_err());
    assert!(Identity::from_stored_blob(&[0u8; 65]).is_err());
}

#[test]
fn uri_roundtrip_with_and_without_rendezvous() {
    let id = Identity::generate().unwrap();
    let token = InviteToken::from_bytes(*b"0123456789abcdef");

    let uri = id.uri("relay.example.com:7443", Some(&token));
    let (pk, host, tok) = Identity::parse_uri(&uri).unwrap();
    assert_eq!(pk, id.public());
    assert_eq!(host, "relay.example.com:7443");
    assert_eq!(tok, Some(token));

    let uri = id.uri("relay.example.com", None);
    let (pk, host, tok) = Identity::parse_uri(&uri).unwrap();
    assert_eq!(pk, id.public());
    assert_eq!(host, "relay.example.com");
    assert_eq!(tok, None);

    let uri = id.uri("[::1]:9999", None);
    let (_, host, _) = Identity::parse_uri(&uri).unwrap();
    assert_eq!(host, "[::1]:9999");
}

#[test]
fn uri_rejects_garbage() {
    let id = Identity::generate().unwrap();
    let good = id.uri(
        "relay.example.com:7443",
        Some(&InviteToken::from_bytes(*b"0123456789abcdef")),
    );
    let pk52 = good
        .strip_prefix("cinder://")
        .unwrap()
        .split('@')
        .next()
        .unwrap()
        .to_string();

    let cases = [
        String::new(),
        "http://x".to_string(),
        "cinder://".to_string(),
        format!("cinder://{pk52}"),
        format!("cinder://{pk52}@"),
        format!("cinder://{}@host", &pk52[..51]),
        format!("cinder://{}x@host", &pk52[..51]),
        format!("cinder://{}@host", pk52.to_uppercase()), // strict lowercase
        format!("cinder://{pk52}@@host"),
        format!("cinder://{pk52}@ho st"),
        format!("cinder://{pk52}@host/extra"),
        format!("cinder://{pk52}@host:abc"),
        format!("cinder://{pk52}@host:99999"),
        format!("cinder://{pk52}@::1:7443"), // unbracketed ipv6
        format!("cinder://{pk52}@host?"),
        format!("cinder://{pk52}@host?foo=bar"),
        format!("cinder://{pk52}@host?rendezvous=abc"),
        format!("cinder://{pk52}@host?rendezvous=AAAAAAAAAAAAAAAAAAAAAAAAAA"),
        format!("{good}&extra=1"),
        format!("{good}?rendezvous=bbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ];
    for (i, c) in cases.iter().enumerate() {
        assert!(
            Identity::parse_uri(c).is_err(),
            "case {i} must be rejected: {c:?}"
        );
    }
}

#[test]
fn contacts_hmac_fails_closed_on_tamper() {
    let dir = TempDir::new().unwrap();
    let secret = [0x77u8; 32];
    let path = dir.path().join("contacts.json");

    let mut store = ContactStore::open(dir.path(), &secret).unwrap();
    store.add_unverified([1u8; 32], "alice").unwrap();
    drop(store);

    let text = fs::read_to_string(&path).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    let hmac_bytes = data_encoding::BASE64
        .decode(file["hmac"].as_str().unwrap().as_bytes())
        .unwrap();
    assert_eq!(hmac_bytes.len(), 32);

    // Every byte of the stored MAC: flip -> load must fail closed. 32/32.
    for i in 0..32 {
        let mut bad = hmac_bytes.clone();
        bad[i] ^= 0x01;
        let mut v = file.clone();
        v["hmac"] = serde_json::Value::String(data_encoding::BASE64.encode(&bad));
        fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        match ContactStore::open(dir.path(), &secret) {
            Err(Error::ContactsIntegrity) => {}
            Err(e) => panic!("hmac byte {i} flip must fail with ContactsIntegrity, got {e}"),
            Ok(_) => panic!("hmac byte {i} flip must fail closed"),
        }
    }

    // Payload tamper under the valid envelope also fails.
    let mut v = file.clone();
    let mut payload = file["payload"].as_str().unwrap().to_string();
    payload.push(' ');
    v["payload"] = serde_json::Value::String(payload);
    fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    match ContactStore::open(dir.path(), &secret) {
        Err(Error::ContactsIntegrity) => {}
        Err(e) => panic!("payload tamper must fail with ContactsIntegrity, got {e}"),
        Ok(_) => panic!("payload tamper must fail closed"),
    }

    // Pristine file loads fine.
    fs::write(&path, &text).unwrap();
    let store = ContactStore::open(dir.path(), &secret).unwrap();
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].name, "alice");
}

#[test]
fn contacts_wrong_key_fails_closed() {
    let dir = TempDir::new().unwrap();
    let mut store = ContactStore::open(dir.path(), &[1u8; 32]).unwrap();
    store.add_unverified([2u8; 32], "bob").unwrap();
    drop(store);
    match ContactStore::open(dir.path(), &[2u8; 32]) {
        Err(Error::ContactsIntegrity) => {}
        Err(e) => panic!("wrong secret must fail with ContactsIntegrity, got {e}"),
        Ok(_) => panic!("wrong secret must fail closed"),
    }
}

#[test]
fn contacts_save_is_atomic_and_private() {
    let dir = TempDir::new().unwrap();
    let mut store = ContactStore::open(dir.path(), &[9u8; 32]).unwrap();
    let pid = store.add_unverified([3u8; 32], "carol").unwrap();

    let path = dir.path().join("contacts.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let text = fs::read_to_string(&path).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(file["hmac"].is_string());
    assert!(file["payload"].is_string());

    let store = ContactStore::open(dir.path(), &[9u8; 32]).unwrap();
    assert_eq!(store.get(&pid).unwrap().name, "carol");
}

#[test]
fn trust_transitions_block_and_unblock_send() {
    let dir = TempDir::new().unwrap();
    let mut store = ContactStore::open(dir.path(), &[5u8; 32]).unwrap();

    let key_a = [0xaau8; 32];
    let key_b = [0xbbu8; 32];
    let key_c = [0xccu8; 32];
    let pid = store.add_unverified(key_a, "dave").unwrap();
    assert!(matches!(
        store.get(&pid).unwrap().state,
        TrustState::Unverified
    ));
    assert!(!store.is_send_blocked(&pid));

    store.mark_verified(&pid).unwrap();
    assert!(matches!(
        store.get(&pid).unwrap().state,
        TrustState::Verified
    ));

    // Same key re-presented: no-op, state untouched, send stays unblocked.
    assert!(!store.record_key_change(&pid, key_a).unwrap());
    assert!(matches!(
        store.get(&pid).unwrap().state,
        TrustState::Verified
    ));

    // Different key: loud state, blocks send.
    assert!(store.record_key_change(&pid, key_b).unwrap());
    match &store.get(&pid).unwrap().state {
        TrustState::KeyChanged { old, dismissed } => {
            assert_eq!(old, &key_a);
            assert!(!dismissed);
        }
        s => panic!("expected KeyChanged, got {s:?}"),
    }
    assert!(store.is_send_blocked(&pid));
    assert_eq!(store.get(&pid).unwrap().static_pubkey, key_b);

    store.dismiss_key_change(&pid).unwrap();
    assert!(!store.is_send_blocked(&pid));
    assert!(matches!(
        store.get(&pid).unwrap().state,
        TrustState::KeyChanged {
            dismissed: true,
            ..
        }
    ));

    // Another change re-blocks; `old` tracks the key it replaced.
    assert!(store.record_key_change(&pid, key_c).unwrap());
    match &store.get(&pid).unwrap().state {
        TrustState::KeyChanged { old, dismissed } => {
            assert_eq!(old, &key_b);
            assert!(!dismissed);
        }
        s => panic!("expected KeyChanged, got {s:?}"),
    }
    assert!(store.is_send_blocked(&pid));

    assert!(matches!(
        store.mark_verified(&PeerId::from_bytes([0u8; 32])),
        Err(Error::UnknownContact)
    ));
}

#[test]
fn mailbox_grant_and_pending_roundtrip() {
    let dir = TempDir::new().unwrap();
    let mut store = ContactStore::open(dir.path(), &[7u8; 32]).unwrap();
    let pid = store.add_unverified([4u8; 32], "erin").unwrap();
    let grant = MailboxGrant {
        mailbox_id: [0x11u8; 32],
        deposit_token_hash: [0x22u8; 32],
        fetch_token_hash: [0x33u8; 32],
    };
    store.set_mailbox_grant(&pid, grant.clone()).unwrap();
    store
        .add_pending(PendingContact {
            peer_id: Some(pid),
            uri: "cinder://x@y".into(),
            state: PendingState::Connecting,
        })
        .unwrap();

    let store = ContactStore::open(dir.path(), &[7u8; 32]).unwrap();
    let g = store.get(&pid).unwrap().mailbox_grant.as_ref().unwrap();
    assert_eq!(g.mailbox_id, grant.mailbox_id);
    assert_eq!(g.deposit_token_hash, grant.deposit_token_hash);
    assert_eq!(g.fetch_token_hash, grant.fetch_token_hash);
    assert_eq!(store.pending().len(), 1);
    assert_eq!(store.pending()[0].peer_id, Some(pid));
}

#[test]
fn age_file_store_roundtrip_and_perms() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("identity.age");
    let store = AgeFileStore::with_passphrase(&path, SecretString::new("hunter2".into()));
    assert!(store.get().unwrap().is_none());

    let secret = [0x5au8; 64];
    store.put(&secret).unwrap();
    let got = store.get().unwrap().unwrap();
    assert_eq!(got.expose_secret(), &secret[..]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn age_file_wrong_passphrase_errors_not_panics() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("identity.age");
    AgeFileStore::with_passphrase(&path, SecretString::new("right".into()))
        .put(&[0xabu8; 64])
        .unwrap();
    let wrong = AgeFileStore::with_passphrase(&path, SecretString::new("wrong".into()));
    match wrong.get() {
        Err(Error::KeyUnavailable(_)) | Err(Error::IdentityStore(_)) => {}
        Err(e) => panic!("wrong passphrase must be a typed error, got {e}"),
        Ok(_) => panic!("wrong passphrase must not decrypt"),
    }
}

#[test]
#[ignore = "requires a live OS keychain; run: CINDER_TEST_KEYRING=1 cargo test -p cinder-core --test identity_contacts -- --ignored"]
fn keyring_store_roundtrip() {
    if std::env::var("CINDER_TEST_KEYRING").is_err() {
        return;
    }
    let ks = KeyringStore::new("cinder-test", "identity-v1").unwrap();
    let secret = [0x42u8; 64];
    ks.put(&secret).unwrap();
    let got = ks.get().unwrap().unwrap();
    assert_eq!(got.expose_secret(), &secret[..]);
}

#[test]
#[ignore = "requires a live OS keychain; run: CINDER_TEST_KEYRING=1 cargo test -p cinder-core --test identity_contacts -- --ignored"]
fn load_or_create_roundtrip_via_keyring() {
    if std::env::var("CINDER_TEST_KEYRING").is_err() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let a = Identity::load_or_create(dir.path()).unwrap();
    let b = Identity::load_or_create(dir.path()).unwrap();
    assert_eq!(a.peer_id(), b.peer_id());
}
