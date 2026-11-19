use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretSlice, SecretString};

use crate::error::Error;

pub trait KeyStore {
    fn get(&self) -> Result<Option<SecretSlice<u8>>, Error>;
    fn put(&self, secret: &[u8]) -> Result<(), Error>;
}

pub struct KeyringStore {
    service: String,
    account: String,
}

// OS keychain calls can block indefinitely on an unseen permission dialog
// (unsigned binaries on macOS prompt per-access). Bound the wait so callers
// fall back to the age-file store instead of wedging the whole process.
const KEYRING_OP_TIMEOUT: Duration = Duration::from_secs(8);

impl KeyringStore {
    pub fn new(service: &str, account: &str) -> Result<Self, Error> {
        keyring::Entry::new(service, account).map_err(|e| Error::KeyUnavailable(e.to_string()))?;
        Ok(Self {
            service: service.into(),
            account: account.into(),
        })
    }

    fn with_entry<T: Send + 'static>(
        &self,
        f: impl FnOnce(keyring::Entry) -> Result<T, keyring::Error> + Send + 'static,
    ) -> Result<T, keyring::Error> {
        let entry = keyring::Entry::new(&self.service, &self.account)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        std::thread::spawn(move || drop(tx.send(f(entry))));
        rx.recv_timeout(KEYRING_OP_TIMEOUT)
            .unwrap_or(Err(keyring::Error::PlatformFailure(
                "keyring access timed out".into(),
            )))
    }
}

impl KeyStore for KeyringStore {
    fn get(&self) -> Result<Option<SecretSlice<u8>>, Error> {
        match self.with_entry(|e| e.get_password()) {
            Ok(s) => {
                let raw = data_encoding::BASE64.decode(s.as_bytes()).map_err(|_| {
                    Error::IdentityStore("keyring entry is not valid base64".into())
                })?;
                Ok(Some(raw.into()))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::KeyUnavailable(e.to_string())),
        }
    }

    fn put(&self, secret: &[u8]) -> Result<(), Error> {
        let b64 = data_encoding::BASE64.encode(secret);
        self.with_entry(move |e| e.set_password(&b64))
            .map_err(|e| Error::KeyUnavailable(e.to_string()))
    }
}

pub struct AgeFileStore {
    path: PathBuf,
    passphrase: Option<SecretString>,
}

impl AgeFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            passphrase: None,
        }
    }

    pub fn with_passphrase(path: impl Into<PathBuf>, passphrase: SecretString) -> Self {
        Self {
            path: path.into(),
            passphrase: Some(passphrase),
        }
    }

    fn passphrase(&self, confirm: bool) -> Result<SecretString, Error> {
        if let Some(p) = &self.passphrase {
            // `confirm` marks a NEW passphrase being set (encrypting a
            // just-created file): programmatic passphrases meet the same
            // strength floor as interactive ones — with_passphrase is not
            // a backdoor around it. Decrypt of an existing store reaches
            // here with confirm=false and is intentionally unchecked, so
            // older weaker passphrases keep working.
            if confirm {
                check_passphrase_strength(p.expose_secret())?;
            }
            return Ok(SecretString::new(p.expose_secret().into()));
        }
        let p = rpassword::prompt_password("cinder passphrase: ")
            .map_err(|e| Error::KeyUnavailable(format!("passphrase prompt failed: {e}")))?;
        if confirm {
            check_passphrase_strength(&p)?;
            let p2 = rpassword::prompt_password("confirm passphrase: ")
                .map_err(|e| Error::KeyUnavailable(format!("passphrase prompt failed: {e}")))?;
            if p != p2 {
                return Err(Error::KeyUnavailable("passphrases do not match".into()));
            }
        }
        Ok(SecretString::new(p.into_boxed_str()))
    }
}

impl KeyStore for AgeFileStore {
    fn get(&self) -> Result<Option<SecretSlice<u8>>, Error> {
        let blob = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let decryptor = age::Decryptor::new(&blob[..])
            .map_err(|e| Error::IdentityStore(format!("unreadable age file: {e}")))?;
        if !decryptor.is_scrypt() {
            return Err(Error::IdentityStore(
                "age file is recipient-encrypted, expected passphrase".into(),
            ));
        }
        let pw = self.passphrase(false)?;
        let identity = age::scrypt::Identity::new(age::secrecy::SecretString::from(
            pw.expose_secret().to_owned(),
        ));
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|_| Error::KeyUnavailable("passphrase incorrect or file corrupt".into()))?;
        let mut plaintext = Vec::with_capacity(blob.len());
        reader.read_to_end(&mut plaintext)?;
        Ok(Some(plaintext.into()))
    }

    fn put(&self, secret: &[u8]) -> Result<(), Error> {
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir)?;
        }
        let pw = self.passphrase(!self.path.exists())?;
        let encryptor = age::Encryptor::with_user_passphrase(age::secrecy::SecretString::from(
            pw.expose_secret().to_owned(),
        ));
        let mut out = Vec::with_capacity(secret.len() + 1024);
        {
            let mut w = encryptor
                .wrap_output(&mut out)
                .map_err(|e| Error::IdentityStore(format!("age encrypt init: {e}")))?;
            w.write_all(secret)?;
            w.finish()
                .map_err(|e| Error::IdentityStore(format!("age encrypt finish: {e}")))?;
        }
        write_atomic_0600(&self.path, &out)
    }
}

/// Minimum strength for a newly set age passphrase: ≥12 characters and
/// ≥2 of the four character classes (lower / upper / digit / other).
fn check_passphrase_strength(p: &str) -> Result<(), Error> {
    let classes = [
        p.chars().any(|c| c.is_lowercase()),
        p.chars().any(|c| c.is_uppercase()),
        p.chars().any(|c| c.is_ascii_digit()),
        p.chars().any(|c| !c.is_alphanumeric()),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    if p.chars().count() >= 12 && classes >= 2 {
        Ok(())
    } else {
        Err(Error::KeyUnavailable(
            "passphrase too weak: need at least 12 characters spanning at \
             least 2 character classes (lowercase, uppercase, digit, other)"
                .into(),
        ))
    }
}

pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<(), Error> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::IdentityStore("path has no parent directory".into()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check_passphrase_strength, AgeFileStore, KeyStore};
    use secrecy::{ExposeSecret, SecretString};

    #[test]
    fn passphrase_strength_rule() {
        // ≥12 chars and ≥2 classes is required.
        assert!(check_passphrase_strength("correct horse battery").is_ok());
        assert!(check_passphrase_strength("abcDEFGHIJKL").is_ok());
        assert!(check_passphrase_strength("abcdefghij!1").is_ok());
        // Too short, even with multiple classes.
        assert!(check_passphrase_strength("aB3!").is_err());
        // Long but single-class.
        assert!(check_passphrase_strength("abcdefghijklmnop").is_err());
        assert!(check_passphrase_strength("123456789012").is_err());
        assert!(check_passphrase_strength("").is_err());
        // Non-ASCII still counts as its classes (lower/upper apply to
        // unicode letters; anything else lands in "other").
        assert!(check_passphrase_strength("pässwörd_12345").is_ok());
    }

    // Programmatic passphrases are NOT a backdoor around the floor: a weak
    // with_passphrase can't CREATE a store (confirm path), but it can still
    // DECRYPT one written under it (open path is intentionally unchecked —
    // existing weaker stores keep working).
    #[test]
    fn with_passphrase_strength_enforced_on_create() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("identity.age");
        let weak = AgeFileStore::with_passphrase(&path, SecretString::new("hunter2".into()));
        assert!(
            weak.put(&[0x5au8; 64]).is_err(),
            "weak programmatic passphrase must not create a store"
        );
        assert!(!path.exists(), "failed put must leave no file behind");

        let strong =
            AgeFileStore::with_passphrase(&path, SecretString::new("hunter2-correct-horse".into()));
        strong.put(&[0x5au8; 64]).unwrap();
        // The same weak phrase on the OPEN path is unchecked — it just
        // fails to decrypt like any wrong passphrase.
        let weak_open = AgeFileStore::with_passphrase(&path, SecretString::new("hunter2".into()));
        assert!(weak_open.get().is_err());
        assert_eq!(
            strong.get().unwrap().unwrap().expose_secret(),
            &[0x5au8; 64]
        );
    }
}
