use std::path::Path;

use secrecy::{ExposeSecret, SecretSlice};
use zeroize::Zeroize;

use crate::error::Error;
use crate::keystore::{AgeFileStore, KeyStore, KeyringStore};
use crate::proto::{InviteToken, PeerId};

pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

const KEYRING_SERVICE: &str = "cinder";
const KEYRING_ACCOUNT: &str = "identity-v1";
const IDENTITY_FILE: &str = "identity.age";

// Stored form: private(32) ‖ public(32).
const IDENTITY_BLOB_LEN: usize = 64;

pub struct Identity {
    keypair: snow::Keypair,
}

impl Identity {
    pub fn generate() -> Result<Self, Error> {
        let params: snow::params::NoiseParams = NOISE_PARAMS.parse()?;
        let keypair = snow::Builder::new(params).generate_keypair()?;
        Ok(Self { keypair })
    }

    pub fn from_stored_blob(blob: &[u8]) -> Result<Self, Error> {
        if blob.len() != IDENTITY_BLOB_LEN {
            return Err(Error::IdentityStore(format!(
                "identity blob is {} bytes, expected {IDENTITY_BLOB_LEN}",
                blob.len()
            )));
        }
        Ok(Self {
            keypair: snow::Keypair {
                private: blob[..32].to_vec(),
                public: blob[32..].to_vec(),
            },
        })
    }

    pub fn stored_blob(&self) -> SecretSlice<u8> {
        let mut v = Vec::with_capacity(IDENTITY_BLOB_LEN);
        v.extend_from_slice(&self.keypair.private);
        v.extend_from_slice(&self.keypair.public);
        v.into()
    }

    pub fn public(&self) -> [u8; 32] {
        self.keypair
            .public
            .as_slice()
            .try_into()
            .expect("x25519 static pubkey is 32 bytes")
    }

    pub fn private(&self) -> SecretSlice<u8> {
        self.keypair.private.clone().into()
    }

    pub fn keypair(&self) -> &snow::Keypair {
        &self.keypair
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId::from_static_key(&self.public())
    }

    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        if let Ok(ks) = KeyringStore::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
            match ks.get() {
                Ok(Some(blob)) => return Self::from_stored_blob(blob.expose_secret()),
                Ok(None) => {
                    let id = Self::generate()?;
                    if ks.put(id.stored_blob().expose_secret()).is_ok() {
                        return Ok(id);
                    }
                    return Self::create_in_file(dir, id);
                }
                Err(_) => {}
            }
        }
        let store = AgeFileStore::new(dir.join(IDENTITY_FILE));
        match store.get()? {
            Some(blob) => Self::from_stored_blob(blob.expose_secret()),
            None => {
                let id = Self::generate()?;
                store.put(id.stored_blob().expose_secret())?;
                Ok(id)
            }
        }
    }

    fn create_in_file(dir: &Path, id: Identity) -> Result<Self, Error> {
        AgeFileStore::new(dir.join(IDENTITY_FILE)).put(id.stored_blob().expose_secret())?;
        Ok(id)
    }

    pub fn uri(&self, relay_host: &str, rendezvous: Option<&InviteToken>) -> String {
        let mut s = format!("cinder://{}@{relay_host}", b32_encode(&self.public()));
        if let Some(t) = rendezvous {
            s.push_str("?rendezvous=");
            s.push_str(&b32_encode(t.as_bytes()));
        }
        s
    }

    /// Hostless static form — printed when no relay is known so the output
    /// is never a misleading `cinder://<key>@` with an empty host.
    pub fn uri_static(&self) -> String {
        format!("cinder://{}", b32_encode(&self.public()))
    }

    pub fn parse_uri(s: &str) -> Result<([u8; 32], String, Option<InviteToken>), Error> {
        let rest = s
            .strip_prefix("cinder://")
            .ok_or_else(|| Error::UriParse("missing cinder:// scheme".into()))?;
        let (authority, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let (pk_str, hostport) = authority
            .split_once('@')
            .ok_or_else(|| Error::UriParse("missing @ separator".into()))?;
        if hostport.contains('@') {
            return Err(Error::UriParse("unexpected extra @".into()));
        }
        let pubkey = b32_decode_fixed::<32>(pk_str)?;
        let host = validate_hostport(hostport)?;
        let token = match query {
            None => None,
            Some(q) => {
                let t = q
                    .strip_prefix("rendezvous=")
                    .ok_or_else(|| Error::UriParse("unknown or missing query key".into()))?;
                Some(InviteToken::from_bytes(b32_decode_fixed::<16>(t)?))
            }
        };
        Ok((pubkey, host, token))
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.keypair.private.zeroize();
    }
}

fn b32_encode(data: &[u8]) -> String {
    data_encoding::BASE32_NOPAD.encode(data).to_lowercase()
}

fn b32_decode_fixed<const N: usize>(s: &str) -> Result<[u8; N], Error> {
    let want = (N * 8).div_ceil(5);
    if s.len() != want {
        return Err(Error::UriParse(format!(
            "field is {} chars, expected {want}",
            s.len()
        )));
    }
    if !s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')) {
        return Err(Error::UriParse(
            "invalid base32 (must be lowercase rfc4648 a-z2-7)".into(),
        ));
    }
    let v = data_encoding::BASE32_NOPAD
        .decode(s.to_uppercase().as_bytes())
        .map_err(|e| Error::UriParse(format!("base32 decode: {e}")))?;
    v.try_into()
        .map_err(|_| Error::UriParse("decoded length mismatch".into()))
}

fn validate_hostport(hostport: &str) -> Result<String, Error> {
    let bad = || Error::UriParse(format!("invalid host: {hostport:?}"));
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(bad)?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p),
            None if tail.is_empty() => None,
            _ => return Err(bad()),
        };
        (host, port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => {
                if h.contains(':') {
                    return Err(bad()); // unbracketed multi-colon host
                }
                (h, Some(p))
            }
            None => (hostport, None),
        }
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        return Err(bad());
    }
    if let Some(p) = port {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().is_err() {
            return Err(bad());
        }
    }
    Ok(hostport.to_string())
}
