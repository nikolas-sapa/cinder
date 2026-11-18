use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretSlice};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::Error;
use crate::keystore::{ensure_private_dir, write_atomic_0600};
use crate::proto::PeerId;

const CONTACTS_FILE: &str = "contacts.json";
const MAC_INFO: &[u8] = b"cinder-contacts-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxGrant {
    #[serde(with = "serde_b64")]
    pub mailbox_id: [u8; 32],
    #[serde(with = "serde_b64")]
    pub deposit_token_hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub fetch_token_hash: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TrustState {
    Unverified,
    Verified,
    KeyChanged {
        #[serde(with = "serde_b64")]
        old: [u8; 32],
        dismissed: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    #[serde(with = "serde_peer_b64")]
    pub peer_id: PeerId,
    #[serde(with = "serde_b64")]
    pub static_pubkey: [u8; 32],
    pub name: String,
    pub state: TrustState,
    pub mailbox_grant: Option<MailboxGrant>,
    pub added_at: u64,
    pub last_seen: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PendingState {
    Connecting,
    Retry { at: u64 },
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingContact {
    #[serde(with = "serde_peer_b64_opt")]
    pub peer_id: Option<PeerId>,
    pub uri: String,
    pub state: PendingState,
}

#[derive(Serialize)]
struct PayloadRef<'a> {
    contacts: &'a [Contact],
    pending: &'a [PendingContact],
}

#[derive(Deserialize)]
struct PayloadOwned {
    contacts: Vec<Contact>,
    #[serde(default)]
    pending: Vec<PendingContact>,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    hmac: String,
    payload: String,
}

pub struct ContactStore {
    path: PathBuf,
    mac_key: SecretSlice<u8>,
    contacts: Vec<Contact>,
    pending: Vec<PendingContact>,
}

impl ContactStore {
    pub fn open(dir: &Path, static_secret: &[u8]) -> Result<Self, Error> {
        let mut store = Self {
            path: dir.join(CONTACTS_FILE),
            mac_key: mac_key(static_secret),
            contacts: Vec::new(),
            pending: Vec::new(),
        };
        match fs::read_to_string(&store.path) {
            Ok(text) => store.load(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(store)
    }

    fn load(&mut self, text: &str) -> Result<(), Error> {
        let file: StoreFile = serde_json::from_str(text).map_err(|_| Error::ContactsIntegrity)?;
        let mac = data_encoding::BASE64
            .decode(file.hmac.as_bytes())
            .map_err(|_| Error::ContactsIntegrity)?;
        let expected = self.mac(file.payload.as_bytes());
        if mac.as_slice().ct_eq(expected.as_slice()).unwrap_u8() != 1 {
            return Err(Error::ContactsIntegrity);
        }
        let payload: PayloadOwned =
            serde_json::from_str(&file.payload).map_err(|_| Error::ContactsIntegrity)?;
        self.contacts = payload.contacts;
        self.pending = payload.pending;
        Ok(())
    }

    pub fn save(&self) -> Result<(), Error> {
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir)?;
        }
        let payload = serde_json::to_string(&PayloadRef {
            contacts: &self.contacts,
            pending: &self.pending,
        })
        .map_err(|e| Error::IdentityStore(format!("contacts serialize: {e}")))?;
        let mac = self.mac(payload.as_bytes());
        let file = StoreFile {
            hmac: data_encoding::BASE64.encode(&mac),
            payload,
        };
        let text = serde_json::to_string(&file)
            .map_err(|e| Error::IdentityStore(format!("contacts serialize: {e}")))?;
        write_atomic_0600(&self.path, text.as_bytes())
    }

    fn mac(&self, data: &[u8]) -> [u8; 32] {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(self.mac_key.expose_secret())
            .expect("hmac accepts keys of any length");
        m.update(data);
        let out = m.finalize().into_bytes();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(out.as_slice());
        arr
    }

    pub fn add_unverified(
        &mut self,
        static_pubkey: [u8; 32],
        name: impl Into<String>,
    ) -> Result<PeerId, Error> {
        let peer_id = PeerId::from_static_key(&static_pubkey);
        if self.get(&peer_id).is_some() {
            return Ok(peer_id);
        }
        self.contacts.push(Contact {
            peer_id,
            static_pubkey,
            name: name.into(),
            state: TrustState::Unverified,
            mailbox_grant: None,
            added_at: now_unix(),
            last_seen: None,
        });
        self.save()?;
        Ok(peer_id)
    }

    pub fn mark_verified(&mut self, peer_id: &PeerId) -> Result<(), Error> {
        let c = self.get_mut(peer_id).ok_or(Error::UnknownContact)?;
        c.state = TrustState::Verified;
        self.save()
    }

    // Same-key calls are a no-op: trust state can never be changed by
    // re-presenting the pinned key.
    pub fn record_key_change(
        &mut self,
        peer_id: &PeerId,
        new_static: [u8; 32],
    ) -> Result<bool, Error> {
        let c = self.get_mut(peer_id).ok_or(Error::UnknownContact)?;
        if c.static_pubkey.ct_eq(&new_static).unwrap_u8() == 1 {
            return Ok(false);
        }
        let old = c.static_pubkey;
        c.static_pubkey = new_static;
        c.state = TrustState::KeyChanged {
            old,
            dismissed: false,
        };
        self.save()?;
        Ok(true)
    }

    pub fn dismiss_key_change(&mut self, peer_id: &PeerId) -> Result<(), Error> {
        let c = self.get_mut(peer_id).ok_or(Error::UnknownContact)?;
        if let TrustState::KeyChanged { dismissed, .. } = &mut c.state {
            *dismissed = true;
        }
        self.save()
    }

    pub fn is_send_blocked(&self, peer_id: &PeerId) -> bool {
        matches!(
            self.get(peer_id).map(|c| &c.state),
            Some(TrustState::KeyChanged {
                dismissed: false,
                ..
            })
        )
    }

    pub fn list(&self) -> &[Contact] {
        &self.contacts
    }

    pub fn get(&self, peer_id: &PeerId) -> Option<&Contact> {
        self.contacts.iter().find(|c| c.peer_id == *peer_id)
    }

    fn get_mut(&mut self, peer_id: &PeerId) -> Option<&mut Contact> {
        self.contacts.iter_mut().find(|c| c.peer_id == *peer_id)
    }

    pub fn set_mailbox_grant(
        &mut self,
        peer_id: &PeerId,
        grant: MailboxGrant,
    ) -> Result<(), Error> {
        let c = self.get_mut(peer_id).ok_or(Error::UnknownContact)?;
        c.mailbox_grant = Some(grant);
        self.save()
    }

    pub fn record_seen(&mut self, peer_id: &PeerId) -> Result<(), Error> {
        let c = self.get_mut(peer_id).ok_or(Error::UnknownContact)?;
        c.last_seen = Some(now_unix());
        self.save()
    }

    pub fn add_pending(&mut self, pending: PendingContact) -> Result<(), Error> {
        self.pending.retain(|p| p.uri != pending.uri);
        self.pending.push(pending);
        self.save()
    }

    pub fn pending(&self) -> &[PendingContact] {
        &self.pending
    }

    pub fn set_pending_state(&mut self, uri: &str, state: PendingState) -> Result<(), Error> {
        let p = self
            .pending
            .iter_mut()
            .find(|p| p.uri == uri)
            .ok_or(Error::UnknownContact)?;
        p.state = state;
        self.save()
    }

    pub fn remove_pending(&mut self, uri: &str) -> Result<Option<PendingContact>, Error> {
        let removed = self
            .pending
            .iter()
            .position(|p| p.uri == uri)
            .map(|i| self.pending.remove(i));
        self.save()?;
        Ok(removed)
    }
}

fn mac_key(static_secret: &[u8]) -> SecretSlice<u8> {
    let hk = Hkdf::<Sha256>::new(None, static_secret);
    let mut key = vec![0u8; 32];
    hk.expand(MAC_INFO, &mut key)
        .expect("32 bytes is a valid okm length");
    key.into()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn b64_decode_32(s: &str) -> Result<[u8; 32], serde::de::value::Error> {
    let v = data_encoding::BASE64
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)?;
    v.try_into()
        .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
}

mod serde_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&data_encoding::BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        super::b64_decode_32(&s).map_err(serde::de::Error::custom)
    }
}

mod serde_peer_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::proto::PeerId;

    pub fn serialize<S: Serializer>(id: &PeerId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&data_encoding::BASE64.encode(id.as_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PeerId, D::Error> {
        let s = String::deserialize(d)?;
        super::b64_decode_32(&s)
            .map(PeerId::from_bytes)
            .map_err(serde::de::Error::custom)
    }
}

mod serde_peer_b64_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::proto::PeerId;

    pub fn serialize<S: Serializer>(v: &Option<PeerId>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(id) => data_encoding::BASE64.encode(id.as_bytes()).serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PeerId>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(None),
            Some(s) => super::b64_decode_32(&s)
                .map(|b| Some(PeerId::from_bytes(b)))
                .map_err(serde::de::Error::custom),
        }
    }
}
