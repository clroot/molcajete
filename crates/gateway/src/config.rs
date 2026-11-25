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
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session_ttl_hours: default_session_ttl_hours(),
        }
    }
}

fn default_session_ttl_hours() -> u64 {
    12
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            static_dir: default_static_dir(),
            data_dir: default_data_dir(),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("web/dist")
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
    pub guacd: Option<GuacdConfig>,
    /// Protocol parameters sent in the `connect` instruction.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
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
    }
}
