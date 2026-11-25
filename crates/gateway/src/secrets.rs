//! Connection credentials at rest.
//!
//! The values of sensitive connection parameters ([`is_sensitive`]) are
//! stored encrypted with AES-256-GCM under a key the database does not hold
//! (`[secrets]` in the configuration). They are decrypted only to build the
//! guacd handshake; the API never returns them.
//!
//! A stored value looks like `enc:v1:<key id>:<base64 of nonce, ciphertext
//! and tag>`. `v1` names the format (AES-256-GCM with a random 96-bit nonce);
//! the key id is derived from the key, so a gateway that knows several keys
//! (during rotation) picks the right one and one that has the wrong key says
//! so instead of sending garbage to guacd. The connection id and parameter
//! name are authenticated with the value, so an encrypted value copied to
//! another row does not decrypt.
//!
//! AES-256-GCM rather than XChaCha20-Poly1305: it is the common choice that
//! auditors expect, CPUs accelerate it, and the RustCrypto implementation
//! is pure Rust. Its 96-bit random nonces are safe for about 2³² values per
//! key, far more than a gateway stores; rotating the key starts afresh.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::Config;

/// Connection parameters whose values are credentials. These are encrypted
/// in the database and never returned by the API.
///
/// Keep in sync with `SENSITIVE_PARAMETERS` in
/// `web/src/connections/secrets.ts`. Adding a name is enough: plain values
/// already stored under it are encrypted on the next start.
pub const SENSITIVE_PARAMETERS: &[&str] = &[
    // Every protocol.
    "password",
    // SSH.
    "passphrase",
    "private-key",
    // SFTP file transfer next to VNC, RDP and SSH.
    "sftp-password",
    "sftp-passphrase",
    "sftp-private-key",
    // RDP: Remote Desktop Gateway.
    "gateway-password",
    // Kubernetes: the client certificate's private key.
    "client-key",
    // SSH jump hosts: the single hop, and hops 1 to 4 of a chain. Their
    // `-host-key` parameters are public keys.
    "jump-host-password",
    "jump-host-private-key",
    "jump-host-passphrase",
    "jump-host-1-password",
    "jump-host-1-private-key",
    "jump-host-1-passphrase",
    "jump-host-2-password",
    "jump-host-2-private-key",
    "jump-host-2-passphrase",
    "jump-host-3-password",
    "jump-host-3-private-key",
    "jump-host-3-passphrase",
    "jump-host-4-password",
    "jump-host-4-private-key",
    "jump-host-4-passphrase",
];

/// Whether `name` is a connection parameter that holds a credential.
pub fn is_sensitive(name: &str) -> bool {
    SENSITIVE_PARAMETERS.contains(&name)
}

/// Environment variable holding the key (base64 of 32 bytes).
pub const KEY_ENV: &str = "MOLCAJETE_SECRET_KEY";

/// Name of the key file generated in `data_dir` when no key is configured.
pub const DEFAULT_KEY_FILE: &str = "secret.key";

const PREFIX: &str = "enc:v1:";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error(
        "a stored credential is encrypted with key {key_id}, which is not configured; \
         configure that key ([secrets] key_file, key or {KEY_ENV}, or previous_key_files \
         for a retired key)"
    )]
    UnknownKey { key_id: String },
    #[error(
        "a stored credential could not be decrypted with key {key_id}: the value was \
         altered or belongs to another connection"
    )]
    Undecryptable { key_id: String },
    #[error("a stored credential is not a valid encrypted value")]
    Malformed,
    #[error("no secret key is configured for stored credentials")]
    NoKey,
}

struct Key {
    id: String,
    cipher: Aes256Gcm,
}

impl Key {
    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() != KEY_LEN {
            bail!("the key must be {KEY_LEN} bytes, not {}", bytes.len());
        }
        let mut hash = Sha256::new();
        hash.update(b"molcajete secret key id\0");
        hash.update(bytes);
        Ok(Self {
            id: hex::encode(&hash.finalize()[..4]),
            cipher: Aes256Gcm::new_from_slice(bytes).expect("checked length"),
        })
    }

    /// A key written as base64 (padding and surrounding whitespace allowed).
    fn from_base64(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let bytes = STANDARD
            .decode(text)
            .or_else(|_| STANDARD_NO_PAD.decode(text))
            .context("the key is not base64 (create one with `openssl rand -base64 32`)")?;
        Self::from_bytes(&bytes)
    }

    fn from_file(path: &Path) -> anyhow::Result<Self> {
        warn_if_readable_by_others(path);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the secret key file {}", path.display()))?;
        Self::from_base64(&text).with_context(|| format!("secret key file {}", path.display()))
    }
}

/// The keys stored credentials are encrypted with: the current one, which
/// encrypts, and previous ones that still decrypt values not yet rotated.
pub struct SecretKeys {
    current: Key,
    previous: Vec<Key>,
}

impl std::fmt::Debug for SecretKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKeys")
            .field("current", &self.current.id)
            .field(
                "previous",
                &self.previous.iter().map(|k| &k.id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Authenticated with each value: it only decrypts in its own row.
fn associated_data(connection_id: &str, name: &str) -> Vec<u8> {
    let mut aad = b"molcajete connection parameter\0".to_vec();
    aad.extend_from_slice(connection_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(name.as_bytes());
    aad
}

/// Whether a stored value is an encrypted one (as opposed to a plain value
/// stored before encryption, or by an older gateway).
pub fn is_sealed(value: &str) -> bool {
    parse(value).is_some()
}

/// The id of the key a stored value was encrypted with.
pub fn key_id(value: &str) -> Option<&str> {
    parse(value).map(|(id, _)| id)
}

fn parse(value: &str) -> Option<(&str, &str)> {
    let (id, data) = value.strip_prefix(PREFIX)?.split_once(':')?;
    let id_ok = id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit());
    let data_ok = data.len() >= (NONCE_LEN + TAG_LEN) * 4 / 3
        && data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    (id_ok && data_ok).then_some((id, data))
}

impl SecretKeys {
    /// Keys from base64 text: the current one first.
    pub fn from_base64(current: &str, previous: &[&str]) -> anyhow::Result<Self> {
        Ok(Self {
            current: Key::from_base64(current)?,
            previous: previous
                .iter()
                .map(|k| Key::from_base64(k))
                .collect::<anyhow::Result<_>>()?,
        })
    }

    /// The id of the key new values are encrypted with.
    pub fn current_id(&self) -> &str {
        &self.current.id
    }

    pub fn knows(&self, key_id: &str) -> bool {
        self.key(key_id).is_some()
    }

    fn key(&self, key_id: &str) -> Option<&Key> {
        std::iter::once(&self.current)
            .chain(&self.previous)
            .find(|k| k.id == key_id)
    }

    /// Encrypts the value of parameter `name` of connection `connection_id`
    /// with the current key.
    pub fn seal(&self, connection_id: &str, name: &str, plaintext: &str) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let aad = associated_data(connection_id, name);
        let ciphertext = self
            .current
            .cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            .expect("AES-GCM encrypts any value this size");
        let mut data = nonce.to_vec();
        data.extend_from_slice(&ciphertext);
        format!(
            "{PREFIX}{}:{}",
            self.current.id,
            STANDARD_NO_PAD.encode(data)
        )
    }

    /// Decrypts a value made by [`SecretKeys::seal`] for the same row.
    pub fn open(
        &self,
        connection_id: &str,
        name: &str,
        sealed: &str,
    ) -> Result<String, SecretError> {
        let (key_id, data) = parse(sealed).ok_or(SecretError::Malformed)?;
        let key = self.key(key_id).ok_or_else(|| SecretError::UnknownKey {
            key_id: key_id.to_owned(),
        })?;
        let data = STANDARD_NO_PAD
            .decode(data)
            .map_err(|_| SecretError::Malformed)?;
        if data.len() < NONCE_LEN + TAG_LEN {
            return Err(SecretError::Malformed);
        }
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        let aad = associated_data(connection_id, name);
        let plaintext = key
            .cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::Undecryptable {
                key_id: key_id.to_owned(),
            })?;
        String::from_utf8(plaintext).map_err(|_| SecretError::Malformed)
    }
}

/// Where the key comes from, in order: `MOLCAJETE_SECRET_KEY`,
/// `[secrets] key_file`, `[secrets] key`, `<data_dir>/secret.key`.
enum KeySource {
    Env(String),
    File(PathBuf),
    Inline(String),
    /// `<data_dir>/secret.key`, generated when missing.
    Default(PathBuf),
}

fn key_source(config: &Config) -> KeySource {
    if let Ok(key) = std::env::var(KEY_ENV)
        && !key.trim().is_empty()
    {
        return KeySource::Env(key);
    }
    let secrets = &config.secrets;
    if let Some(path) = &secrets.key_file {
        return KeySource::File(path.clone());
    }
    if let Some(key) = secrets.key.as_ref().filter(|k| !k.trim().is_empty()) {
        return KeySource::Inline(key.clone());
    }
    KeySource::Default(config.server.data_dir.join(DEFAULT_KEY_FILE))
}

/// Loads the configured keys. Without a configured key, the key in
/// `<data_dir>/secret.key` is used, generated on first start; it is not
/// generated when the database already holds encrypted credentials
/// (`has_sealed` says so), since a new key could not decrypt them.
pub fn load(config: &Config, has_sealed: bool) -> anyhow::Result<SecretKeys> {
    let current = match key_source(config) {
        KeySource::Env(text) => Key::from_base64(&text).with_context(|| KEY_ENV.to_owned())?,
        KeySource::File(path) => Key::from_file(&path)?,
        KeySource::Inline(text) => Key::from_base64(&text).context("[secrets] key")?,
        KeySource::Default(path) => {
            if path.exists() {
                Key::from_file(&path)?
            } else if has_sealed {
                bail!(
                    "the database holds encrypted connection credentials but no secret key is \
                     configured and {} does not exist; restore the key file that was backed up \
                     with the database, or configure the key ([secrets] key_file, key or {KEY_ENV})",
                    path.display()
                );
            } else {
                generate_key_file(&path)?
            }
        }
    };
    let mut previous = Vec::new();
    for path in &config.secrets.previous_key_files {
        previous.push(Key::from_file(path)?);
    }
    for (i, text) in config.secrets.previous_keys.iter().enumerate() {
        previous
            .push(Key::from_base64(text).with_context(|| format!("[secrets] previous_keys[{i}]"))?);
    }
    previous.retain(|k| k.id != current.id);
    let keys = SecretKeys { current, previous };
    info!(
        key = %keys.current.id,
        previous = ?keys.previous.iter().map(|k| &k.id).collect::<Vec<_>>(),
        "secret key for stored credentials loaded"
    );
    Ok(keys)
}

fn generate_key_file(path: &Path) -> anyhow::Result<Key> {
    let mut bytes = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut bytes);
    let creating = || {
        format!(
            "no secret key is configured, and generating {} failed; make data_dir writable \
             or configure the key ([secrets] key_file, key or {KEY_ENV})",
            path.display()
        )
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(creating)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(creating)?;
    std::io::Write::write_all(
        &mut file,
        format!("{}\n", STANDARD.encode(bytes)).as_bytes(),
    )
    .and_then(|()| file.sync_all())
    .with_context(|| format!("writing the secret key file {}", path.display()))?;
    warn!(
        path = %path.display(),
        "generated a new secret key for the connection credentials stored in the database; \
         back it up together with the database: without it the stored credentials cannot \
         be decrypted"
    );
    Key::from_bytes(&bytes)
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        warn!(
            path = %path.display(),
            "the secret key file can be read by other users; restrict it with chmod 600"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const KEY_A: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    pub const KEY_B: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";

    pub fn keys() -> SecretKeys {
        SecretKeys::from_base64(KEY_A, &[]).unwrap()
    }

    #[test]
    fn seals_and_opens_in_the_same_row_only() {
        let keys = keys();
        let sealed = keys.seal("desk", "password", "hunter2");
        assert!(sealed.starts_with(&format!("enc:v1:{}:", keys.current_id())));
        assert!(!sealed.contains("hunter2"));
        assert!(is_sealed(&sealed));
        assert_eq!(key_id(&sealed), Some(keys.current_id()));
        assert_eq!(keys.open("desk", "password", &sealed).unwrap(), "hunter2");
        // Fresh nonce every time.
        assert_ne!(sealed, keys.seal("desk", "password", "hunter2"));
        assert!(matches!(
            keys.open("other", "password", &sealed),
            Err(SecretError::Undecryptable { .. })
        ));
        assert!(matches!(
            keys.open("desk", "passphrase", &sealed),
            Err(SecretError::Undecryptable { .. })
        ));
        let empty = keys.seal("desk", "password", "");
        assert_eq!(keys.open("desk", "password", &empty).unwrap(), "");
    }

    #[test]
    fn recognizes_plain_values() {
        for plain in [
            "hunter2",
            "",
            "enc:v1:",
            "enc:v1:nothex!!:AAAA",
            "enc:v1:0011aabb:é",
        ] {
            assert!(!is_sealed(plain), "{plain}");
        }
        assert!(matches!(
            keys().open("c", "password", "hunter2"),
            Err(SecretError::Malformed)
        ));
    }

    #[test]
    fn previous_keys_still_decrypt() {
        let old = SecretKeys::from_base64(KEY_A, &[]).unwrap();
        let sealed = old.seal("c", "password", "old secret");
        let rotated = SecretKeys::from_base64(KEY_B, &[KEY_A]).unwrap();
        assert_ne!(rotated.current_id(), old.current_id());
        assert!(rotated.knows(old.current_id()));
        assert_eq!(
            rotated.open("c", "password", &sealed).unwrap(),
            "old secret"
        );
        let new_only = SecretKeys::from_base64(KEY_B, &[]).unwrap();
        assert!(matches!(
            new_only.open("c", "password", &sealed),
            Err(SecretError::UnknownKey { .. })
        ));
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(SecretKeys::from_base64("not base64!", &[]).is_err());
        assert!(SecretKeys::from_base64("AAAA", &[]).is_err());
        // Without padding and with whitespace is fine.
        SecretKeys::from_base64(&format!(" {}\n", KEY_A.trim_end_matches('=')), &[]).unwrap();
    }

    #[test]
    fn generates_a_key_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(&format!(
            "[server]\ndata_dir = {:?}\n",
            dir.path().display().to_string()
        ))
        .unwrap();
        let path = dir.path().join(DEFAULT_KEY_FILE);
        // Encrypted values in the database and no key: refuse to make one.
        let err = load(&config, true).unwrap_err().to_string();
        assert!(err.contains("restore the key file"), "{err}");
        assert!(!path.exists());

        let first = load(&config, false).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = load(&config, true).unwrap();
        assert_eq!(first.current_id(), again.current_id());
    }

    #[test]
    fn configured_keys() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.key");
        std::fs::write(&old, KEY_A).unwrap();
        let config: Config = toml::from_str(&format!(
            "[server]\ndata_dir = {:?}\n[secrets]\nkey = {KEY_B:?}\nprevious_key_files = [{:?}]\n",
            dir.path().display().to_string(),
            old.display().to_string()
        ))
        .unwrap();
        let keys = load(&config, true).unwrap();
        assert_eq!(
            keys.current_id(),
            SecretKeys::from_base64(KEY_B, &[]).unwrap().current_id()
        );
        let old = SecretKeys::from_base64(KEY_A, &[]).unwrap();
        assert!(keys.knows(old.current_id()));
        assert!(!dir.path().join(DEFAULT_KEY_FILE).exists());

        let missing: Config =
            toml::from_str("[secrets]\nkey_file = \"/nonexistent/key\"\n").unwrap();
        assert!(load(&missing, false).is_err());
    }
}
