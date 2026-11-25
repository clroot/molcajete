//! SSH jump hosts: reaching a VNC, RDP or SSH server that only a bastion
//! can reach.
//!
//! A connection lists one or more jump hosts (`jump-hosts`). molcajeted
//! connects to the first over TCP, authenticates, and asks it for a
//! `direct-tcpip` channel to the next one; the SSH session with the next
//! hop runs inside that channel, and so on. The last hop's channel goes to
//! the target, and the protocol runs over it exactly as over a TCP socket
//! ([`Upstream`]).
//!
//! Every hop is a full SSH connection with the same handling as an SSH
//! target (`ssh::client`): host key pinning, private key, password and
//! keyboard-interactive authentication. Missing credentials are asked for
//! with `required` like the target's own ([`Chain::missing`]).
//!
//! Parameters (molcajeted only; guacd ignores them):
//!
//! - `jump-hosts`: the hops in the order they are passed through, separated
//!   by commas or new lines, each `[user@]host[:port]` (`[addr]:port` for
//!   IPv6).
//! - `jump-host-username`, `-password`, `-private-key`, `-passphrase`,
//!   `-host-key`: for every hop that does not have its own value. The host
//!   key takes the same format as the SSH connection's `host-key`.
//! - `jump-host-<n>-password`, `-private-key`, `-passphrase`, `-host-key`,
//!   `n` from 1 to [`MAX_JUMP_HOSTS`]: the value for the `n`th hop only.

mod stream;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use molcajete_protocol::Status;
use russh::ChannelOpenFailure;
use russh::client;
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::handshake::Params;
use crate::ssh::client::{self as ssh, ConnectError, Credentials, HostKeys, SshHandle};

pub use stream::{Upstream, UpstreamReader, UpstreamWriter};

/// Hops a connection may go through.
pub const MAX_JUMP_HOSTS: usize = 4;

/// Parameters announced in the handshake by every protocol.
pub const ARGS: &[&str] = &[
    "jump-hosts",
    "jump-host-username",
    "jump-host-password",
    "jump-host-private-key",
    "jump-host-passphrase",
    "jump-host-host-key",
    "jump-host-1-password",
    "jump-host-1-private-key",
    "jump-host-1-passphrase",
    "jump-host-1-host-key",
    "jump-host-2-password",
    "jump-host-2-private-key",
    "jump-host-2-passphrase",
    "jump-host-2-host-key",
    "jump-host-3-password",
    "jump-host-3-private-key",
    "jump-host-3-passphrase",
    "jump-host-3-host-key",
    "jump-host-4-password",
    "jump-host-4-private-key",
    "jump-host-4-passphrase",
    "jump-host-4-host-key",
];

/// `jump-host-<n>-password`, the name asked for when hop `n` has no
/// credentials and there is more than one hop.
const HOP_PASSWORDS: [&str; MAX_JUMP_HOSTS] = [
    "jump-host-1-password",
    "jump-host-2-password",
    "jump-host-3-password",
    "jump-host-4-password",
];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keeps idle hops (and NAT entries on the way) alive.
const HOP_KEEPALIVE: Duration = Duration::from_secs(30);

/// One jump host and the credentials for it.
#[derive(Clone, Default)]
pub struct JumpHost {
    pub username: Option<String>,
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    pub host_key: Option<String>,
}

impl JumpHost {
    fn has_credentials(&self) -> bool {
        self.password.is_some() || self.private_key.is_some()
    }

    fn address(&self) -> String {
        address(&self.host, self.port)
    }
}

impl fmt::Debug for JumpHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secret = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("JumpHost")
            .field("username", &self.username)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("password", &secret(&self.password))
            .field("private_key", &secret(&self.private_key))
            .field("passphrase", &secret(&self.passphrase))
            .field("host_key", &self.host_key)
            .finish()
    }
}

/// `host:port`, with brackets around IPv6 addresses.
fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The jump hosts of a connection, in the order they are passed through.
/// Empty when the target is reached directly.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    hops: Vec<JumpHost>,
}

impl Chain {
    pub fn from_params(params: &Params) -> Result<Self, String> {
        Self::from_lookup(|name| params.get(name))
    }

    /// Reads the `jump-host*` parameters from a lookup that returns `None`
    /// for missing or empty values (like `Params::get`).
    pub fn from_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, String> {
        let Some(list) = get("jump-hosts") else {
            return Ok(Self::default());
        };
        let entries: Vec<&str> = list
            .split([',', '\n'])
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .collect();
        if entries.len() > MAX_JUMP_HOSTS {
            return Err(format!(
                "jump-hosts lists {} hosts; at most {MAX_JUMP_HOSTS} are supported",
                entries.len()
            ));
        }
        let owned = |name: &str| get(name).map(str::to_owned);
        let mut hops = Vec::with_capacity(entries.len());
        for (i, entry) in entries.into_iter().enumerate() {
            let (username, host, port) = parse_hop(entry)?;
            let own = |field: &str| owned(&format!("jump-host-{}-{field}", i + 1));
            let shared = |field: &str| owned(&format!("jump-host-{field}"));
            let either = |field: &str| own(field).or_else(|| shared(field));
            // A hop's own key or password replaces both shared ones, so a
            // shared key is not tried on a hop that has its own password.
            let (password, private_key) = match (own("password"), own("private-key")) {
                (None, None) => (shared("password"), shared("private-key")),
                own => own,
            };
            hops.push(JumpHost {
                username: username.or_else(|| shared("username")),
                host,
                port,
                password,
                private_key,
                passphrase: either("passphrase"),
                host_key: either("host-key"),
            });
        }
        let chain = Self { hops };
        for hop in &chain.hops {
            if let Some(spec) = &hop.host_key {
                HostKeys::parse(spec)
                    .map_err(|err| format!("invalid host key for jump host {}: {err}", hop.host))?;
            }
        }
        Ok(chain)
    }

    pub fn hops(&self) -> &[JumpHost] {
        &self.hops
    }

    /// Parameters to ask the user for: a username when a hop has none, and
    /// a password for every hop without a password or key
    /// (`jump-host-password` when there is a single hop,
    /// `jump-host-<n>-password` otherwise).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.hops.iter().any(|hop| hop.username.is_none()) {
            missing.push("jump-host-username");
        }
        for (i, hop) in self.hops.iter().enumerate() {
            if !hop.has_credentials() {
                missing.push(if self.hops.len() == 1 {
                    "jump-host-password"
                } else {
                    HOP_PASSWORDS[i]
                });
            }
        }
        missing
    }

    /// Applies a value the user entered for one of [`Chain::missing`].
    /// Returns whether the name was a jump host parameter.
    pub fn set(&mut self, name: &str, value: String) -> bool {
        match name {
            "jump-host-username" => {
                for hop in self.hops.iter_mut().filter(|h| h.username.is_none()) {
                    hop.username = Some(value.clone());
                }
            }
            "jump-host-password" => {
                for hop in self.hops.iter_mut().filter(|h| !h.has_credentials()) {
                    hop.password = Some(value.clone());
                }
            }
            _ => {
                let Some(i) = HOP_PASSWORDS.iter().position(|n| *n == name) else {
                    return false;
                };
                if let Some(hop) = self.hops.get_mut(i) {
                    hop.password = Some(value);
                }
            }
        }
        true
    }

    /// Opens a connection to `host:port`: directly over TCP without jump
    /// hosts, else through every hop in turn.
    pub async fn connect(&self, host: &str, port: u16) -> Result<Upstream, UpstreamError> {
        let target = address(host, port);
        let Some((first, rest)) = self.hops.split_first() else {
            let tcp = tcp_connect(host, port, &target).await?;
            return Ok(Upstream::Tcp(tcp));
        };
        let mut handles: Vec<SshHandle> = Vec::with_capacity(self.hops.len());
        let mut place = Place::new(1, first);
        let tcp = tcp_connect(&first.host, first.port, &place.to_string()).await?;
        let mut handle = hop_session(tcp, first, &place).await?;
        for (i, hop) in rest.iter().enumerate() {
            let channel = forward(&handle, &place, &hop.host, hop.port).await?;
            handles.push(handle);
            place = Place::new(i + 2, hop);
            handle = hop_session(channel, hop, &place).await?;
        }
        let channel = forward(&handle, &place, host, port).await?;
        handles.push(handle);
        info!(%target, hops = handles.len(), "connected through jump hosts");
        Ok(Upstream::tunnel(channel, handles))
    }
}

/// Splits `[user@]host[:port]`.
fn parse_hop(entry: &str) -> Result<(Option<String>, String, u16), String> {
    let invalid = || format!("invalid jump host {entry:?}: expected [user@]host[:port]");
    let (username, rest) = match entry.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
        Some(_) => return Err(invalid()),
        None => (None, entry),
    };
    let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or_else(invalid)?;
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':').ok_or_else(invalid)?)),
        }
    } else {
        match rest.split_once(':') {
            // More than one colon: a bare IPv6 address without a port.
            Some((_, after)) if after.contains(':') => (rest, None),
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(invalid());
    }
    let port = match port {
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(invalid)?,
        None => 22,
    };
    Ok((username, host.to_owned(), port))
}

/// A hop as named in errors and logs: `jump host 2 (admin@10.0.0.1:22)`.
#[derive(Clone, Debug)]
pub struct Place(String);

impl Place {
    fn new(index: usize, hop: &JumpHost) -> Self {
        let user = hop
            .username
            .as_deref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        Self(format!("jump host {index} ({user}{})", hop.address()))
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("cannot reach {0}: {1}")]
    Unreachable(String, std::io::Error),
    #[error("timed out connecting to {0}")]
    Timeout(String),
    #[error("no username for {0}")]
    NoUsername(Place),
    #[error("{0} presented a host key that does not match its configured host key")]
    HostKeyMismatch(Place),
    #[error("{0}: {1}")]
    Ssh(Place, ConnectError),
    #[error("{0} cannot open a connection to {1}: {2}")]
    Forward(Place, String, String),
}

impl UpstreamError {
    /// Guacamole status reported to the client.
    pub fn status(&self) -> Status {
        match self {
            Self::Unreachable(..) | Self::Forward(..) => Status::UPSTREAM_NOT_FOUND,
            Self::Timeout(_) => Status::UPSTREAM_TIMEOUT,
            Self::NoUsername(_) => Status::CLIENT_UNAUTHORIZED,
            Self::HostKeyMismatch(_) => Status::UPSTREAM_ERROR,
            Self::Ssh(_, err) => err.status(),
        }
    }
}

async fn tcp_connect(host: &str, port: u16, name: &str) -> Result<TcpStream, UpstreamError> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| UpstreamError::Timeout(name.to_owned()))?
        .map_err(|err| UpstreamError::Unreachable(name.to_owned(), err))?;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

/// SSH handshake and authentication with one hop over `stream`.
async fn hop_session<S>(
    stream: S,
    hop: &JumpHost,
    place: &Place,
) -> Result<SshHandle, UpstreamError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let username = hop
        .username
        .as_deref()
        .ok_or_else(|| UpstreamError::NoUsername(place.clone()))?;
    let host_keys = hop
        .host_key
        .as_deref()
        .map(HostKeys::parse)
        .transpose()
        .map_err(|err| UpstreamError::Ssh(place.clone(), ConnectError::InvalidHostKey(err)))?;
    let config = client::Config {
        keepalive_interval: Some(HOP_KEEPALIVE),
        ..Default::default()
    };
    let handler = ssh::ClientHandler::new(host_keys);
    let mut handle = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client::connect_stream(Arc::new(config), stream, handler),
    )
    .await
    .map_err(|_| UpstreamError::Timeout(place.to_string()))?
    .map_err(|err| match ConnectError::from(err) {
        ConnectError::HostKeyMismatch => UpstreamError::HostKeyMismatch(place.clone()),
        err => UpstreamError::Ssh(place.clone(), err),
    })?;
    let credentials = Credentials {
        username,
        password: hop.password.as_deref(),
        private_key: hop.private_key.as_deref(),
        passphrase: hop.passphrase.as_deref(),
    };
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        ssh::authenticate(&mut handle, &credentials),
    )
    .await
    .map_err(|_| UpstreamError::Timeout(place.to_string()))?
    .map_err(|err| UpstreamError::Ssh(place.clone(), err))?;
    debug!(%place, "jump host authenticated");
    Ok(handle)
}

/// Asks the hop behind `handle` for a channel to `host:port`.
async fn forward(
    handle: &SshHandle,
    place: &Place,
    host: &str,
    port: u16,
) -> Result<russh::ChannelStream<client::Msg>, UpstreamError> {
    let target = address(host, port);
    let failed = |reason: String| UpstreamError::Forward(place.clone(), target.clone(), reason);
    let channel = tokio::time::timeout(
        CONNECT_TIMEOUT,
        handle.channel_open_direct_tcpip(host, port.into(), "127.0.0.1", 0),
    )
    .await
    .map_err(|_| failed("timed out".into()))?
    .map_err(|err| {
        failed(match err {
            russh::Error::ChannelOpenFailure(reason) => open_failure(&reason),
            other => other.to_string(),
        })
    })?;
    Ok(channel.into_stream())
}

fn open_failure(reason: &ChannelOpenFailure) -> String {
    match reason {
        ChannelOpenFailure::ConnectFailed => "the jump host could not connect to it".into(),
        ChannelOpenFailure::AdministrativelyProhibited => {
            "the jump host does not allow forwarding (AllowTcpForwarding)".into()
        }
        ChannelOpenFailure::UnknownChannelType => {
            "the jump host does not support forwarding".into()
        }
        ChannelOpenFailure::ResourceShortage => "the jump host is out of resources".into(),
        ChannelOpenFailure::Other { reason, .. } => reason.clone(),
    }
}
