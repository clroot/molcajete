//! The SSH side: connecting, host key verification, authentication and
//! the interactive session channel (PTY + shell or command).

use std::sync::Arc;
use std::time::Duration;

use molcajete_protocol::Status;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::ssh_key::public::KeyData;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelWriteHalf, Pty};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::settings::Settings;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Rounds of keyboard-interactive prompts answered before giving up.
const MAX_KEYBOARD_INTERACTIVE_ROUNDS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("cannot reach {0}: {1}")]
    Unreachable(String, std::io::Error),
    #[error("timed out connecting to {0}")]
    Timeout(String),
    #[error("invalid host-key parameter: {0}")]
    InvalidHostKey(String),
    #[error("the server's host key does not match the configured host-key")]
    HostKeyMismatch,
    #[error("SSH handshake failed: {0}")]
    Handshake(russh::Error),
    #[error("the private key cannot be decoded: {0}")]
    InvalidPrivateKey(String),
    #[error("authentication failed")]
    AuthFailed,
}

impl ConnectError {
    /// Guacamole status reported to the client.
    pub fn status(&self) -> Status {
        match self {
            Self::Unreachable(..) => Status::UPSTREAM_NOT_FOUND,
            Self::Timeout(_) => Status::UPSTREAM_TIMEOUT,
            Self::InvalidHostKey(_) => Status::CLIENT_BAD_REQUEST,
            Self::HostKeyMismatch | Self::Handshake(_) => Status::UPSTREAM_ERROR,
            Self::InvalidPrivateKey(_) | Self::AuthFailed => Status::CLIENT_UNAUTHORIZED,
        }
    }
}

impl From<russh::Error> for ConnectError {
    fn from(err: russh::Error) -> Self {
        match err {
            russh::Error::UnknownKey => Self::HostKeyMismatch,
            other => Self::Handshake(other),
        }
    }
}

/// Host keys the server must present (the `host-key` parameter).
///
/// Accepted format, one entry per line: a known_hosts line
/// (`[host] <key-type> <base64> [comment]`, the host patterns are not
/// checked since the parameter belongs to one connection), a bare public
/// key (`<key-type> <base64>`), or a SHA-256 fingerprint
/// (`SHA256:<base64>`, as printed by `ssh-keygen -lf`). Empty lines and
/// `#` comments are ignored; `@revoked` lines are rejected.
#[derive(Clone, Debug, Default)]
pub struct HostKeys {
    keys: Vec<KeyData>,
    fingerprints: Vec<String>,
}

impl HostKeys {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for line in spec.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("@revoked") {
                return Err("@revoked entries are not supported".into());
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if let [fingerprint] = tokens.as_slice()
                && fingerprint.starts_with("SHA256:")
            {
                out.fingerprints
                    .push(fingerprint.trim_end_matches('=').to_owned());
                continue;
            }
            // The key type is the first token that parses together with
            // the next one as an OpenSSH public key.
            let key = tokens
                .windows(2)
                .find_map(|pair| PublicKey::from_openssh(&format!("{} {}", pair[0], pair[1])).ok())
                .ok_or_else(|| format!("no public key found in {line:?}"))?;
            out.keys.push(key.key_data().clone());
        }
        if out.keys.is_empty() && out.fingerprints.is_empty() {
            return Err("no host key given".into());
        }
        Ok(out)
    }

    pub fn accepts(&self, key: &KeyData) -> bool {
        if self.keys.contains(key) {
            return true;
        }
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let fingerprint = fingerprint.trim_end_matches('=');
        self.fingerprints.iter().any(|f| f == fingerprint)
    }
}

/// russh callbacks: only host key verification is needed.
pub struct ClientHandler {
    host_keys: Option<HostKeys>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.key_data(),
            PublicKeyOrCertificate::Certificate(cert) => cert.public_key(),
        };
        let Some(expected) = &self.host_keys else {
            debug!(fingerprint = %key.fingerprint(HashAlg::Sha256), "accepting host key (no host-key configured)");
            return Ok(true);
        };
        let accepted = expected.accepts(key);
        if !accepted {
            warn!(fingerprint = %key.fingerprint(HashAlg::Sha256), "host key mismatch");
        }
        Ok(accepted)
    }
}

pub type SshHandle = Handle<ClientHandler>;

/// Opens the TCP connection and performs the SSH handshake.
pub async fn connect(settings: &Settings) -> Result<SshHandle, ConnectError> {
    let host_keys = settings
        .host_key
        .as_deref()
        .map(HostKeys::parse)
        .transpose()
        .map_err(ConnectError::InvalidHostKey)?;
    let target = format!("{}:{}", settings.hostname, settings.port);
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((settings.hostname.as_str(), settings.port)),
    )
    .await
    .map_err(|_| ConnectError::Timeout(target.clone()))?
    .map_err(|err| ConnectError::Unreachable(target.clone(), err))?;
    let _ = stream.set_nodelay(true);

    let config = client::Config {
        keepalive_interval: settings.server_alive_interval,
        ..Default::default()
    };
    let handler = ClientHandler { host_keys };
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client::connect_stream(Arc::new(config), stream, handler),
    )
    .await
    .map_err(|_| ConnectError::Timeout(target))?
    .map_err(ConnectError::from)
}

/// Credentials to try, in order: private key, password, then
/// keyboard-interactive answering every prompt with the password ("none"
/// when neither a key nor a password is available).
pub struct Credentials<'a> {
    pub username: &'a str,
    pub password: Option<&'a str>,
    pub private_key: Option<&'a str>,
    pub passphrase: Option<&'a str>,
}

pub async fn authenticate(
    handle: &mut SshHandle,
    creds: &Credentials<'_>,
) -> Result<(), ConnectError> {
    let user = creds.username;
    if let Some(pem) = creds.private_key {
        let key = russh::keys::decode_secret_key(pem, creds.passphrase)
            .map_err(|e| ConnectError::InvalidPrivateKey(e.to_string()))?;
        let hash = if key.algorithm().is_rsa() {
            match handle.best_supported_rsa_hash().await {
                Ok(Some(hash)) => hash,
                _ => Some(HashAlg::Sha256),
            }
        } else {
            None
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        if handle.authenticate_publickey(user, key).await?.success() {
            return Ok(());
        }
        debug!(user, "public key authentication rejected");
    }
    if let Some(password) = creds.password {
        if handle
            .authenticate_password(user, password)
            .await?
            .success()
        {
            return Ok(());
        }
        debug!(user, "password authentication rejected");
        if keyboard_interactive(handle, user, password).await? {
            return Ok(());
        }
    }
    if creds.password.is_none()
        && creds.private_key.is_none()
        && handle.authenticate_none(user).await?.success()
    {
        return Ok(());
    }
    Err(ConnectError::AuthFailed)
}

async fn keyboard_interactive(
    handle: &mut SshHandle,
    user: &str,
    password: &str,
) -> Result<bool, russh::Error> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await?;
    for _ in 0..MAX_KEYBOARD_INTERACTIVE_ROUNDS {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_owned()).collect();
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
    Ok(false)
}

/// PTY dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u32,
    pub rows: u32,
    pub width: u32,
    pub height: u32,
}

/// Opens the session channel: environment, PTY, then the shell or command.
/// The reply to the shell/exec request arrives on the channel as
/// `ChannelMsg::Success` or `ChannelMsg::Failure`.
pub async fn open_session(
    handle: &SshHandle,
    settings: &Settings,
    size: PtySize,
) -> Result<Channel<Msg>, russh::Error> {
    let channel = handle.channel_open_session().await?;
    let env = [("LANG", &settings.locale), ("TZ", &settings.timezone)];
    for (name, value) in env {
        if let Some(value) = value {
            channel.set_env(false, name, value.as_str()).await?;
        }
    }
    let modes = [(Pty::VERASE, settings.backspace as u32), (Pty::IUTF8, 1)];
    channel
        .request_pty(
            false,
            &settings.terminal_type,
            size.cols,
            size.rows,
            size.width,
            size.height,
            &modes,
        )
        .await?;
    match &settings.command {
        Some(command) => channel.exec(true, command.as_str()).await?,
        None => channel.request_shell(true).await?,
    }
    Ok(channel)
}

/// Input for the remote side, written by a dedicated task so that a full
/// SSH window never stalls the session loop.
#[derive(Debug)]
pub enum Outgoing {
    Data(Vec<u8>),
    Resize(PtySize),
}

pub fn spawn_writer(
    channel: ChannelWriteHalf<Msg>,
) -> (mpsc::UnboundedSender<Outgoing>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let result = match message {
                Outgoing::Data(bytes) => channel.data_bytes(bytes).await,
                Outgoing::Resize(s) => {
                    channel
                        .window_change(s.cols, s.rows, s.width, s.height)
                        .await
                }
            };
            if let Err(err) = result {
                debug!(error = %err, "SSH channel write failed");
                break;
            }
        }
    });
    (tx, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIO96c7xdMKI1bXREpWNz3B+hvwc7QNP3y1qsCAjhZlV1";

    fn key_data() -> KeyData {
        PublicKey::from_openssh(&format!("ssh-ed25519 {ED25519}"))
            .unwrap()
            .key_data()
            .clone()
    }

    #[test]
    fn host_keys_in_known_hosts_format() {
        let key = key_data();
        for spec in [
            format!("[127.0.0.1]:2222 ssh-ed25519 {ED25519}"),
            format!("ssh-ed25519 {ED25519} comment"),
            format!("# comment\n\nhost1,host2 ssh-ed25519 {ED25519}\n"),
            format!("@cert-authority * ssh-ed25519 {ED25519}"),
        ] {
            let keys = HostKeys::parse(&spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert!(keys.accepts(&key), "{spec}");
        }
    }

    #[test]
    fn host_key_fingerprints() {
        let key = key_data();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        assert!(fingerprint.starts_with("SHA256:"));
        assert!(HostKeys::parse(&fingerprint).unwrap().accepts(&key));
        let other = HostKeys::parse("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert!(!other.accepts(&key));
    }

    #[test]
    fn rejects_bad_host_key_specs() {
        assert!(HostKeys::parse("").is_err());
        assert!(HostKeys::parse("host ssh-ed25519 not-base64").is_err());
        assert!(HostKeys::parse(&format!("@revoked * ssh-ed25519 {ED25519}")).is_err());
    }

    #[test]
    fn error_statuses() {
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(
            ConnectError::Unreachable("h:22".into(), io).status(),
            Status::UPSTREAM_NOT_FOUND
        );
        assert_eq!(
            ConnectError::AuthFailed.status(),
            Status::CLIENT_UNAUTHORIZED
        );
        assert_eq!(
            ConnectError::from(russh::Error::UnknownKey).status(),
            Status::UPSTREAM_ERROR
        );
    }
}
