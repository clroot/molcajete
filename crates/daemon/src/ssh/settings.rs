//! SSH connection parameters, named and defaulted like guacd's SSH plugin.

use std::path::PathBuf;
use std::time::Duration;

use crate::handshake::{ClientInfo, Params};

/// Connection parameters accepted by the SSH protocol. All names except
/// `font-path` (an extension: font files or directories searched before
/// the system fonts, separated by `:`) are guacd's.
pub const ARGS: &[&str] = &[
    "hostname",
    "port",
    "host-key",
    "username",
    "password",
    "private-key",
    "passphrase",
    "command",
    "font-name",
    "font-size",
    "font-path",
    "color-scheme",
    "scrollback",
    "backspace",
    "terminal-type",
    "locale",
    "timezone",
    "read-only",
    "disable-copy",
    "disable-paste",
    "server-alive-interval",
];

/// Terminal type announced when the PTY is requested. guacd uses `linux`;
/// the emulator here is xterm-compatible (256 colours, true colour,
/// alternate screen, bracketed paste, SGR mouse), so advertising
/// `xterm-256color` lets applications use all of that.
pub const DEFAULT_TERMINAL_TYPE: &str = "xterm-256color";

#[derive(Clone, Debug)]
pub struct Settings {
    pub hostname: String,
    pub port: u16,
    /// Expected host key(s), known_hosts style (see `client::HostKeys`).
    pub host_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    /// Command run instead of a login shell.
    pub command: Option<String>,
    pub font_name: Option<String>,
    /// Font size in points.
    pub font_size: f32,
    pub font_path: Vec<PathBuf>,
    pub color_scheme: Option<String>,
    /// Lines of scrollback history.
    pub scrollback: usize,
    /// Byte sent for BackSpace (127 or 8).
    pub backspace: u8,
    pub terminal_type: String,
    /// Sent as `LANG`.
    pub locale: Option<String>,
    /// Sent as `TZ` (the parameter, else the client's time zone).
    pub timezone: Option<String>,
    pub read_only: bool,
    pub disable_copy: bool,
    pub disable_paste: bool,
    pub server_alive_interval: Option<Duration>,
}

impl Settings {
    pub fn from_params(params: &Params, info: &ClientInfo) -> Self {
        Self::from_lookup(|name| params.get(name), info)
    }

    /// Builds the settings from a parameter lookup that returns `None` for
    /// missing or empty values (like [`Params::get`]).
    fn from_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>, info: &ClientInfo) -> Self {
        let owned = |name: &str| get(name).map(str::to_owned);
        let flag = |name: &str| get(name) == Some("true");
        Self {
            hostname: get("hostname").unwrap_or("localhost").trim().to_owned(),
            port: parse(get("port")).unwrap_or(22),
            host_key: owned("host-key"),
            username: owned("username"),
            password: owned("password"),
            private_key: owned("private-key"),
            passphrase: owned("passphrase"),
            command: owned("command"),
            font_name: owned("font-name"),
            font_size: get("font-size").and_then(font_size).unwrap_or(12.0),
            font_path: get("font-path")
                .map(|v| {
                    v.split(':')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
            color_scheme: owned("color-scheme"),
            scrollback: parse::<usize>(get("scrollback"))
                .unwrap_or(1000)
                .min(1_000_000),
            backspace: parse::<u8>(get("backspace"))
                .filter(|b| *b < 0x80)
                .unwrap_or(0x7F),
            terminal_type: owned("terminal-type")
                .unwrap_or_else(|| DEFAULT_TERMINAL_TYPE.to_owned()),
            locale: owned("locale"),
            timezone: owned("timezone").or_else(|| info.timezone.clone()),
            read_only: flag("read-only"),
            disable_copy: flag("disable-copy"),
            disable_paste: flag("disable-paste"),
            server_alive_interval: parse::<u64>(get("server-alive-interval"))
                .filter(|s| *s > 0)
                .map(Duration::from_secs),
        }
    }

    /// Whether keys follow the Linux console rather than xterm.
    pub fn linux_keys(&self) -> bool {
        self.terminal_type.starts_with("linux")
    }
}

/// A `font-size` value: points, more than none.
pub fn font_size(value: &str) -> Option<f32> {
    parse::<f32>(Some(value)).filter(|s| s.is_finite() && *s > 0.0)
}

fn parse<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)], info: &ClientInfo) -> Settings {
        Settings::from_lookup(
            |name| {
                pairs
                    .iter()
                    .find(|(n, v)| *n == name && !v.is_empty())
                    .map(|(_, v)| *v)
            },
            info,
        )
    }

    #[test]
    fn defaults_match_guacd() {
        let s = settings(&[("hostname", "example")], &ClientInfo::default());
        assert_eq!(s.hostname, "example");
        assert_eq!(s.port, 22);
        assert_eq!(s.font_size, 12.0);
        assert_eq!(s.scrollback, 1000);
        assert_eq!(s.backspace, 127);
        assert_eq!(s.terminal_type, "xterm-256color");
        assert!(!s.read_only && !s.disable_copy && !s.disable_paste);
        assert!(s.server_alive_interval.is_none());
        assert!(s.font_path.is_empty());
        assert!(!s.linux_keys());
    }

    #[test]
    fn parses_values() {
        let info = ClientInfo {
            timezone: Some("Asia/Seoul".into()),
            ..ClientInfo::default()
        };
        let s = settings(
            &[
                ("port", "2222"),
                ("font-size", "10.5"),
                ("font-path", "/a/b.ttf: /fonts"),
                ("scrollback", "50"),
                ("backspace", "8"),
                ("terminal-type", "linux"),
                ("read-only", "true"),
                ("server-alive-interval", "30"),
            ],
            &info,
        );
        assert_eq!(s.port, 2222);
        assert_eq!(s.font_size, 10.5);
        assert_eq!(
            s.font_path,
            [PathBuf::from("/a/b.ttf"), PathBuf::from("/fonts")]
        );
        assert_eq!(s.scrollback, 50);
        assert_eq!(s.backspace, 8);
        assert!(s.linux_keys());
        assert!(s.read_only);
        assert_eq!(s.timezone.as_deref(), Some("Asia/Seoul"));
        assert_eq!(s.server_alive_interval, Some(Duration::from_secs(30)));
    }
}
