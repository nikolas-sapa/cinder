use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretSlice, SecretString};

use crate::error::Error;

pub trait KeyStore {
    fn get(&self) -> Result<Option<SecretSlice<u8>>, Error>;
    fn put(&self, secret: &[u8]) -> Result<(), Error>;
}

pub struct KeyringStore {
    entry: keyring::Entry,
}

impl KeyringStore {
    pub fn new(service: &str, account: &str) -> Result<Self, Error> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| Error::KeyUnavailable(e.to_string()))?;
        Ok(Self { entry })
    }
}

impl KeyStore for KeyringStore {
    fn get(&self) -> Result<Option<SecretSlice<u8>>, Error> {
        match self.entry.get_password() {
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
        self.entry
            .set_password(&data_encoding::BASE64.encode(secret))
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
            return Ok(SecretString::new(p.expose_secret().into()));
        }
        let p = rpassword::prompt_password("cinder passphrase: ")
            .map_err(|e| Error::KeyUnavailable(format!("passphrase prompt failed: {e}")))?;
        if confirm {
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
            .map_err(|_| {
                Error::KeyUnavailable("passphrase incorrect or file corrupt".into())
            })?;
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
