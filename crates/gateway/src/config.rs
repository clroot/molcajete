//! Gateway configuration (TOML).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub guacd: GuacdConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    /// Sign-in through an OpenID Connect identity provider.
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    #[serde(default)]
    pub recordings: RecordingsConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    /// The key stored connection credentials are encrypted with.
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub history: HistoryConfig,
    /// Administrators created in the database on startup when no user with
    /// that name exists yet. Existing users and passwords are left alone.
    #[serde(default)]
    pub users: Vec<UserConfig>,
    /// Connections imported into the database on startup when no
    /// connection with that id exists yet.
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `postgres://…`, `mysql://…` (or `mariadb://…`) or `sqlite://…`.
    /// Without one, `<server.data_dir>/molcajete.db` (SQLite) is used.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: default_max_connections(),
        }
    }
}

fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Lifetime of a login session.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u64,
    /// `[auth.throttle]`: limits on failed password sign-ins.
    #[serde(default)]
    pub throttle: ThrottleConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session_ttl_hours: default_session_ttl_hours(),
            throttle: ThrottleConfig::default(),
        }
    }
}

/// `[auth.throttle]`: how many failed password sign-ins are let through
/// before `429 Too Many Requests` (see `throttle.rs`). A limit of 0 turns
/// it off.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThrottleConfig {
    /// Failed sign-ins from one client address (an IPv6 /64), whatever the
    /// username, within `address_window_secs`.
    #[serde(default = "default_address_failures")]
    pub address_failures: u32,
    #[serde(default = "default_address_window_secs")]
    pub address_window_secs: u64,
    /// Failed sign-ins for one username, from anywhere, within
    /// `username_window_secs`. Not applied to addresses the user signed in
    /// from within `known_address_days`.
    #[serde(default = "default_username_failures")]
    pub username_failures: u32,
    #[serde(default = "default_username_window_secs")]
    pub username_window_secs: u64,
    /// How long an address the user signed in from stays exempt from the
    /// username limit.
    #[serde(default = "default_known_address_days")]
    pub known_address_days: u64,
    /// Password verifications (Argon2) running at once; 0 is the number of
    /// CPUs. Sign-ins beyond that wait up to `verification_wait_secs`, then
    /// get `503 Service Unavailable`.
    #[serde(default)]
    pub max_concurrent_verifications: usize,
    #[serde(default = "default_verification_wait_secs")]
    pub verification_wait_secs: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            address_failures: default_address_failures(),
            address_window_secs: default_address_window_secs(),
            username_failures: default_username_failures(),
            username_window_secs: default_username_window_secs(),
            known_address_days: default_known_address_days(),
            max_concurrent_verifications: 0,
            verification_wait_secs: default_verification_wait_secs(),
        }
    }
}

fn default_address_failures() -> u32 {
    10
}

fn default_address_window_secs() -> u64 {
    5 * 60
}

fn default_username_failures() -> u32 {
    20
}

fn default_username_window_secs() -> u64 {
    15 * 60
}

fn default_known_address_days() -> u64 {
    30
}

fn default_verification_wait_secs() -> u64 {
    10
}

fn default_session_ttl_hours() -> u64 {
    12
}

/// `[oidc]`: sign-in through an OpenID Connect identity provider
/// (authorization code flow with PKCE).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// Issuer URL; `<issuer>/.well-known/openid-configuration` is read.
    pub issuer: String,
    pub client_id: String,
    /// The client secret, for confidential clients. `client_secret_file`
    /// or the `MOLCAJETE_OIDC_CLIENT_SECRET` environment variable keep it
    /// out of the file; without any, the gateway is a public client.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_secret_file: Option<PathBuf>,
    /// The browser-visible URL of `/api/auth/oidc/callback`, registered
    /// with the provider.
    pub redirect_url: String,
    /// Scopes besides `openid`.
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    /// Claim holding the molcajete username.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    /// Claim holding the user's groups (a list of names).
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    /// Shown on the button: "Sign in with <display_name>".
    #[serde(default = "default_oidc_display_name")]
    pub display_name: String,
    /// Members of any of these provider groups get the `admin` role.
    #[serde(default)]
    pub admin_groups: Vec<String>,
    /// Members of any of these provider groups (and none of
    /// `admin_groups`) get the `connection_manager` role.
    #[serde(default)]
    pub connection_manager_groups: Vec<String>,
    /// With `admin_groups` or `connection_manager_groups`, set the user's
    /// own role from them on every sign-in, also lowering it to `user` for
    /// someone who left those groups. `false` only ever raises it.
    #[serde(default = "default_true")]
    pub sync_role: bool,
    /// Make the user a member of exactly the molcajete user groups named in
    /// `groups_claim` on every sign-in (memberships in other groups are
    /// removed).
    #[serde(default)]
    pub sync_groups: bool,
    /// With `sync_groups`, create a molcajete group (role `user`, no
    /// grants) for a provider group that has none; otherwise such names
    /// are skipped.
    #[serde(default)]
    pub create_groups: bool,
    /// With `sync_groups`, only provider groups whose names start with this
    /// are synced (names are kept as they are).
    #[serde(default)]
    pub group_prefix: Option<String>,
    /// Create users that sign in for the first time. `false` only lets in
    /// users an administrator created beforehand (matched by username).
    #[serde(default = "default_true")]
    pub auto_create_users: bool,
    /// Keep the username and password form. `false` hides it and refuses
    /// password sign-in.
    #[serde(default = "default_true")]
    pub password_login: bool,
    /// Where the provider sends the browser after signing out there
    /// (RP-initiated logout); must be registered with the provider.
    #[serde(default)]
    pub post_logout_redirect_url: Option<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec!["profile".into(), "email".into()]
}

fn default_username_claim() -> String {
    "preferred_username".into()
}

fn default_groups_claim() -> String {
    "groups".into()
}

fn default_oidc_display_name() -> String {
    "single sign-on".into()
}

pub const OIDC_SECRET_ENV: &str = "MOLCAJETE_OIDC_CLIENT_SECRET";

impl OidcConfig {
    /// The client secret from the environment, `client_secret_file` or
    /// `client_secret`, in that order.
    pub fn resolve_client_secret(&self) -> anyhow::Result<Option<String>> {
        if let Ok(secret) = std::env::var(OIDC_SECRET_ENV)
            && !secret.is_empty()
        {
            return Ok(Some(secret));
        }
        if let Some(path) = &self.client_secret_file {
            let secret = std::fs::read_to_string(path)
                .with_context(|| format!("reading oidc.client_secret_file {}", path.display()))?;
            return Ok(Some(secret.trim().to_owned()));
        }
        Ok(self.client_secret.clone().filter(|s| !s.is_empty()))
    }
}

/// `[audit]`: the audit log of administrative changes and sign-ins.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Entries older than this many days are deleted; 0 keeps them.
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: default_audit_retention_days(),
        }
    }
}

fn default_audit_retention_days() -> u64 {
    365
}

/// `[secrets]`: the key stored connection credentials (passwords, private
/// keys, …) are encrypted with; see [`crate::secrets`]. The
/// `MOLCAJETE_SECRET_KEY` environment variable wins over `key_file`, which
/// wins over `key`; without any, `<server.data_dir>/secret.key` is used and
/// generated on first start. Keys are base64 of 32 random bytes
/// (`openssl rand -base64 32`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// File holding the key.
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    /// The key itself.
    #[serde(default)]
    pub key: Option<String>,
    /// Retired keys that still decrypt values `rotate-key` has not
    /// re-encrypted yet.
    #[serde(default)]
    pub previous_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub previous_keys: Vec<String>,
}

/// `[history]`: the connection history (closed remote desktop sessions).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    /// Sessions that ended more than this many days ago are deleted; 0
    /// keeps them.
    #[serde(default = "default_history_retention_days")]
    pub retention_days: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            retention_days: default_history_retention_days(),
        }
    }
}

fn default_history_retention_days() -> u64 {
    365
}

/// Session recording (connections with *Record sessions* on).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingsConfig {
    /// `false` records nothing, whatever the connections say.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Where recordings are written. Without one,
    /// `<server.data_dir>/recordings`.
    #[serde(default)]
    pub directory: Option<PathBuf>,
    /// Recordings older than this many days are deleted; 0 keeps them.
    #[serde(default)]
    pub max_age_days: u64,
    /// When all recordings together take more than this, the oldest are
    /// deleted: bytes, or a string such as `"50 GiB"`; 0 sets no limit.
    #[serde(default, deserialize_with = "byte_size")]
    pub max_total_size: u64,
}

impl Default for RecordingsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            max_age_days: 0,
            max_total_size: 0,
        }
    }
}

impl RecordingsConfig {
    pub fn directory(&self, server: &ServerConfig) -> PathBuf {
        self.directory
            .clone()
            .unwrap_or_else(|| server.data_dir.join("recordings"))
    }
}

/// Parses `"1.5 GiB"`, `"500MB"`, `"1024"` or a plain integer into bytes.
pub fn parse_byte_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "kib" => 1 << 10,
        "m" | "mb" => 1_000_000,
        "mib" => 1 << 20,
        "g" | "gb" => 1_000_000_000,
        "gib" => 1 << 30,
        "t" | "tb" => 1_000_000_000_000,
        "tib" => 1 << 40,
        _ => return None,
    };
    (number >= 0.0).then_some((number * factor as f64) as u64)
}

fn byte_size<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Bytes(u64),
        Text(String),
    }
    match Size::deserialize(deserializer)? {
        Size::Bytes(bytes) => Ok(bytes),
        Size::Text(text) => parse_byte_size(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("{text:?} is not a size such as \"50 GiB\""))
        }),
    }
}

/// `"10.0.0.0/8"` or a single address such as `"192.0.2.1"`.
pub fn parse_ip_network(text: &str) -> Option<ipnet::IpNet> {
    let text = text.trim();
    text.parse()
        .ok()
        .or_else(|| text.parse::<std::net::IpAddr>().ok().map(Into::into))
}

fn ip_networks<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ipnet::IpNet>, D::Error> {
    Vec::<String>::deserialize(deserializer)?
        .iter()
        .map(|text| {
            parse_ip_network(text).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "{text:?} is not an address or a CIDR range such as \"10.0.0.0/8\""
                ))
            })
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Directory holding the built web client (`index.html`, wasm package).
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
    /// Where the default SQLite database lives.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Offer the `molcajete-binary-v1` WebSocket subprotocol (stream data
    /// as raw bytes) to clients that ask for it. `false` keeps every
    /// tunnel on the Guacamole text protocol.
    #[serde(default = "default_true")]
    pub binary_tunnel: bool,
    /// Reverse proxies (addresses or CIDR ranges) whose `X-Real-IP` and
    /// `X-Forwarded-For` headers name the client. Requests from any other
    /// peer are attributed to the peer itself.
    #[serde(default, deserialize_with = "ip_networks")]
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            static_dir: default_static_dir(),
            data_dir: default_data_dir(),
            binary_tunnel: true,
            trusted_proxies: Vec::new(),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("web/dist")
}

fn default_true() -> bool {
    true
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuacdConfig {
    #[serde(default = "default_guacd_host")]
    pub host: String,
    #[serde(default = "default_guacd_port")]
    pub port: u16,
    /// Seconds to wait for the guacd handshake to complete.
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout: u64,
}

impl Default for GuacdConfig {
    fn default() -> Self {
        Self {
            host: default_guacd_host(),
            port: default_guacd_port(),
            handshake_timeout: default_handshake_timeout(),
        }
    }
}

/// A connection's own `guacd`: where to connect, and how long to wait for
/// the handshake instead of `[guacd] handshake_timeout`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionGuacdConfig {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub handshake_timeout: Option<u64>,
}

impl ConnectionGuacdConfig {
    /// The daemon to use instead of `[guacd]`: given a host or a port, the
    /// other defaults to `127.0.0.1` or `4822`; given only a timeout, none.
    pub fn address(&self) -> Option<(String, u16)> {
        if self.host.is_none() && self.port.is_none() {
            return None;
        }
        Some((
            self.host.clone().unwrap_or_else(default_guacd_host),
            self.port.unwrap_or_else(default_guacd_port),
        ))
    }
}

fn default_guacd_host() -> String {
    "127.0.0.1".into()
}

fn default_guacd_port() -> u16 {
    4822
}

fn default_handshake_timeout() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub id: String,
    /// Human readable name shown in the connection list.
    #[serde(default)]
    pub name: Option<String>,
    /// guacd protocol plugin: `vnc`, `rdp`, `ssh`, `telnet`, `kubernetes`.
    pub protocol: String,
    /// Optional guacd override for this connection.
    #[serde(default)]
    pub guacd: Option<ConnectionGuacdConfig>,
    /// Protocol parameters sent in the `connect` instruction.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// molcajete's own settings, such as `record-sessions = "true"`.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

impl ConnectionConfig {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for c in &self.connections {
            if !seen.insert(c.id.as_str()) {
                bail!("duplicate connection id {:?}", c.id);
            }
            let timeout = c.guacd.as_ref().and_then(|g| g.handshake_timeout);
            if timeout.is_some_and(|t| !(1..=3600).contains(&t)) {
                bail!(
                    "connection {:?}: guacd handshake_timeout needs 1–3600 seconds",
                    c.id
                );
            }
        }
        if self.secrets.key.is_some() && self.secrets.key_file.is_some() {
            bail!("secrets: set key or key_file, not both");
        }
        if let Some(oidc) = &self.oidc {
            if oidc.client_secret.is_some() && oidc.client_secret_file.is_some() {
                bail!("oidc: set client_secret or client_secret_file, not both");
            }
            if oidc.username_claim.is_empty() {
                bail!("oidc: username_claim must not be empty");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "0.0.0.0:9000"

            [[users]]
            username = "admin"
            password = "secret"

            [[connections]]
            id = "desktop"
            name = "Desktop"
            protocol = "vnc"
            [connections.parameters]
            hostname = "10.0.0.5"
            port = "5900"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.server.listen.port(), 9000);
        assert_eq!(config.guacd.port, 4822);
        assert_eq!(config.connections[0].parameters["hostname"], "10.0.0.5");
        assert_eq!(config.server.static_dir, PathBuf::from("web/dist"));
        assert_eq!(config.database.url, None);
        assert_eq!(config.auth.session_ttl_hours, 12);
        assert!(config.oidc.is_none());
        assert!(config.recordings.enabled);
        assert_eq!(
            config.recordings.directory(&config.server),
            PathBuf::from("./recordings")
        );
        assert_eq!(config.recordings.max_total_size, 0);
        assert_eq!(config.audit.retention_days, 365);
        assert!(config.server.trusted_proxies.is_empty());
        assert_eq!(config.auth.throttle.address_failures, 10);
        assert_eq!(config.auth.throttle.username_window_secs, 900);
        let config: Config = toml::from_str("[audit]\nretention_days = 0\n").unwrap();
        assert_eq!(config.audit.retention_days, 0);
        assert_eq!(config.history.retention_days, 365);
        let config: Config = toml::from_str("[history]\nretention_days = 30\n").unwrap();
        assert_eq!(config.history.retention_days, 30);
    }

    #[test]
    fn a_connection_may_set_its_own_handshake_timeout() {
        let parse = |guacd: &str| {
            let config: Config = toml::from_str(&format!(
                "[[connections]]\nid = \"d\"\nprotocol = \"vnc\"\nguacd = {guacd}\n"
            ))
            .unwrap();
            config.validate().map(|()| config)
        };
        // A timeout alone keeps `[guacd]` as the daemon.
        let own = parse("{ handshake_timeout = 60 }").unwrap();
        let guacd = own.connections[0].guacd.as_ref().unwrap();
        assert_eq!(guacd.handshake_timeout, Some(60));
        assert_eq!(guacd.address(), None);
        let without = parse("{ port = 4823 }").unwrap();
        let guacd = without.connections[0].guacd.as_ref().unwrap();
        assert_eq!(guacd.handshake_timeout, None);
        assert_eq!(guacd.address(), Some(("127.0.0.1".into(), 4823)));
        assert!(parse("{ handshake_timeout = 0 }").is_err());
        assert!(parse("{ handshake_timeout = 3601 }").is_err());
    }

    #[test]
    fn parses_recording_limits() {
        let config: Config = toml::from_str(
            "[recordings]\ndirectory = \"/srv/rec\"\nmax_age_days = 30\nmax_total_size = \"1.5 GiB\"\n",
        )
        .unwrap();
        assert_eq!(config.recordings.max_age_days, 30);
        assert_eq!(config.recordings.max_total_size, 3 << 29);
        assert_eq!(
            config.recordings.directory(&config.server),
            PathBuf::from("/srv/rec")
        );
        let config: Config = toml::from_str("[recordings]\nmax_total_size = 4096\n").unwrap();
        assert_eq!(config.recordings.max_total_size, 4096);
        assert!(toml::from_str::<Config>("[recordings]\nmax_total_size = \"lots\"\n").is_err());

        assert_eq!(parse_byte_size("500MB"), Some(500_000_000));
        assert_eq!(parse_byte_size(" 2 tib "), Some(2 << 40));
        assert_eq!(parse_byte_size("10 KiB"), Some(10_240));
        assert_eq!(parse_byte_size("-1"), None);
        assert_eq!(parse_byte_size("1 parsec"), None);
    }

    #[test]
    fn parses_trusted_proxies_and_throttle() {
        let config: Config = toml::from_str(
            "[server]\ntrusted_proxies = [\"10.0.0.0/8\", \"192.0.2.1\", \"fd00::/8\"]\n\
             [auth.throttle]\naddress_failures = 3\nmax_concurrent_verifications = 2\n",
        )
        .unwrap();
        let proxies: Vec<String> = config
            .server
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(proxies, ["10.0.0.0/8", "192.0.2.1/32", "fd00::/8"]);
        assert_eq!(config.auth.throttle.address_failures, 3);
        assert_eq!(config.auth.throttle.username_failures, 20);
        assert_eq!(config.auth.throttle.max_concurrent_verifications, 2);
        assert!(toml::from_str::<Config>("[server]\ntrusted_proxies = [\"nginx\"]\n").is_err());
        assert!(toml::from_str::<Config>("[auth.throttle]\nlimit = 1\n").is_err());
    }

    #[test]
    fn parses_oidc_with_defaults() {
        let config: Config = toml::from_str(
            r#"
            [oidc]
            issuer = "https://idp.example/realms/x"
            client_id = "molcajete"
            redirect_url = "https://gw.example/api/auth/oidc/callback"
            admin_groups = ["molcajete-admins"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let oidc = config.oidc.unwrap();
        assert_eq!(oidc.username_claim, "preferred_username");
        assert_eq!(oidc.groups_claim, "groups");
        assert_eq!(oidc.scopes, ["profile", "email"]);
        assert!(oidc.password_login && oidc.auto_create_users && oidc.sync_role);
        assert!(!oidc.sync_groups && !oidc.create_groups && oidc.group_prefix.is_none());

        let both: Config = toml::from_str(
            r#"
            [oidc]
            issuer = "https://idp.example"
            client_id = "c"
            client_secret = "s"
            client_secret_file = "/run/secret"
            redirect_url = "https://gw.example/api/auth/oidc/callback"
            "#,
        )
        .unwrap();
        assert!(both.validate().is_err());
    }
}
