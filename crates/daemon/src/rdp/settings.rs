//! RDP connection parameters, parsed from the values sent with `connect`.
//!
//! Parameter names and defaults follow guacd's RDP plugin so the same
//! connection configuration works with both daemons.

use ironrdp_pdu::rdp::client_info::PerformanceFlags;

use super::audio::AudioEncoding;
use crate::handshake::{ClientInfo, Params};

/// Read access to connection parameters (lets tests avoid a handshake).
pub trait ParamSource {
    /// Non-empty value of a parameter.
    fn value(&self, name: &str) -> Option<&str>;

    fn flag(&self, name: &str) -> bool {
        matches!(self.value(name), Some("true"))
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.value(name)?.trim().parse().ok()
    }
}

impl ParamSource for Params {
    fn value(&self, name: &str) -> Option<&str> {
        self.get(name)
    }
}

/// Connection parameters accepted by this protocol (named like guacd's).
pub const ARGS: &[&str] = &[
    "hostname",
    "port",
    "domain",
    "username",
    "password",
    "width",
    "height",
    "dpi",
    "color-depth",
    "security",
    "ignore-cert",
    "disable-auth",
    "client-name",
    "initial-program",
    "server-layout",
    "read-only",
    "disable-copy",
    "disable-paste",
    "normalize-clipboard",
    "resize-method",
    "enable-wallpaper",
    "enable-theming",
    "enable-font-smoothing",
    "enable-full-window-drag",
    "enable-desktop-composition",
    "enable-menu-animations",
    "force-lossless",
    "disable-audio",
];

const DEFAULT_PORT: u16 = 3389;
const DEFAULT_CLIENT_NAME: &str = "Guacamole RDP";
const DEFAULT_COLOR_DEPTH: u32 = 32;
/// Display Control limits ([MS-RDPEDISP] 2.2.2.2.1); also used for the
/// initial size so that later resize requests are consistent with it.
const MIN_DIMENSION: u32 = 200;
const MAX_DIMENSION: u32 = 8192;

/// Security protocols offered to the server during negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Let the server choose between NLA (CredSSP) and TLS.
    Any,
    /// Network Level Authentication only (CredSSP over TLS).
    Nla,
    /// TLS only; the server authenticates the user itself (graphical login
    /// or the credentials of the Client Info PDU).
    Tls,
}

impl Security {
    /// Whether CredSSP may be negotiated.
    pub fn allows_nla(self) -> bool {
        matches!(self, Self::Any | Self::Nla)
    }

    /// Whether plain TLS may be negotiated.
    pub fn allows_tls(self) -> bool {
        matches!(self, Self::Any | Self::Tls)
    }
}

/// How changes of the client's display size are applied to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMethod {
    /// The remote desktop keeps its size (the client scales it).
    None,
    /// Monitor layout updates over the Display Control channel.
    DisplayUpdate,
}

impl ResizeMethod {
    /// Reads a `resize-method` value. guacd's "reconnect" is not
    /// supported and, like anything else, leaves the size alone.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("display-update") => Self::DisplayUpdate,
            _ => Self::None,
        }
    }
}

/// Reads a `dpi` value: a positive number, or `None` for "follow the client".
pub fn parse_dpi(value: Option<&str>) -> Option<u32> {
    value?.trim().parse::<u32>().ok().filter(|&d| d > 0)
}

/// Desktop scale factor in percent ([MS-RDPBCGR] 2.2.1.3.2) for a DPI.
pub fn desktop_scale_factor(dpi: u32) -> u32 {
    (dpi.saturating_mul(100) / 96).clamp(100, 500)
}

/// Device scale factor in percent for a desktop scale factor: one of 100,
/// 140 or 180, the plateaus Windows uses (125 % gives 100, 175 % gives 140).
pub fn device_scale_factor(desktop_scale_factor: u32) -> u32 {
    match desktop_scale_factor {
        0..140 => 100,
        140..180 => 140,
        _ => 180,
    }
}

/// Keyboard layout of the remote session, which decides how keysyms are
/// turned into keyboard events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardLayout {
    /// US English: printable ASCII characters are sent as scancodes.
    EnUsQwerty,
    /// Korean: the same key positions as US English, with the session's
    /// input language set to Korean so that the remote IME composes Hangul
    /// from the keys and the Hangul and Hanja keys work.
    KoKrQwerty,
    /// Every printable character is sent as a Unicode event.
    Failsafe,
}

impl KeyboardLayout {
    /// Windows keyboard layout identifier advertised to the server.
    pub fn layout_id(self) -> u32 {
        match self {
            Self::KoKrQwerty => 0x0000_0412,
            // "failsafe" uses the US layout on the server side and only
            // changes how characters are sent.
            Self::EnUsQwerty | Self::Failsafe => 0x0000_0409,
        }
    }
}

/// Line ending normalization applied to clipboard text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEndings {
    Preserve,
    Unix,
    Windows,
}

impl LineEndings {
    pub fn apply(self, text: &str) -> String {
        match self {
            Self::Preserve => text.to_owned(),
            Self::Unix => text.replace("\r\n", "\n"),
            Self::Windows => text.replace("\r\n", "\n").replace('\n', "\r\n"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error(
        "security mode \"{0}\" is not supported (Standard RDP Security and Hyper-V \
         connections are unavailable); use \"any\", \"nla\" or \"tls\""
    )]
    UnsupportedSecurity(String),
    #[error("unknown security mode \"{0}\"")]
    UnknownSecurity(String),
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub hostname: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub domain: Option<String>,
    pub security: Security,
    /// Accept any server certificate (self-signed, expired, wrong name).
    pub ignore_cert: bool,
    pub width: u16,
    pub height: u16,
    pub dpi: u32,
    pub color_depth: u32,
    pub client_name: String,
    pub initial_program: Option<String>,
    pub layout: KeyboardLayout,
    pub read_only: bool,
    pub disable_copy: bool,
    pub disable_paste: bool,
    pub line_endings: LineEndings,
    pub resize_method: ResizeMethod,
    pub performance_flags: PerformanceFlags,
    pub force_lossless: bool,
    /// How sound is sent to the client; `None` when audio is disabled or
    /// the client cannot play raw PCM.
    pub audio: Option<AudioEncoding>,
}

impl Settings {
    pub fn parse(params: &impl ParamSource, info: &ClientInfo) -> Result<Self, SettingsError> {
        let security = match params
            .value("security")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("any") => Security::Any,
            Some("nla" | "nla-ext") => Security::Nla,
            Some("tls") => Security::Tls,
            Some(mode @ ("rdp" | "vmconnect")) => {
                return Err(SettingsError::UnsupportedSecurity(mode.to_owned()));
            }
            Some(other) => return Err(SettingsError::UnknownSecurity(other.to_owned())),
        };
        // guacd's "disable-auth" turns NLA off.
        let security = if params.flag("disable-auth") {
            Security::Tls
        } else {
            security
        };

        let width = params.number::<u32>("width").unwrap_or(info.width);
        let height = params.number::<u32>("height").unwrap_or(info.height);
        let (width, height) = desktop_size(width, height);
        let dpi = parse_dpi(params.value("dpi")).unwrap_or(info.dpi).max(1);
        let resize_method = ResizeMethod::parse(params.value("resize-method"));
        let layout = match params.value("server-layout") {
            Some("failsafe") => KeyboardLayout::Failsafe,
            Some("ko-kr-qwerty") => KeyboardLayout::KoKrQwerty,
            _ => KeyboardLayout::EnUsQwerty,
        };
        let line_endings = match params.value("normalize-clipboard") {
            Some("unix") => LineEndings::Unix,
            Some("windows") => LineEndings::Windows,
            _ => LineEndings::Preserve,
        };

        Ok(Self {
            hostname: params.value("hostname").unwrap_or("localhost").to_owned(),
            port: params.number("port").unwrap_or(DEFAULT_PORT),
            username: params.value("username").map(str::to_owned),
            password: params.value("password").map(str::to_owned),
            domain: params.value("domain").map(str::to_owned),
            security,
            ignore_cert: params.flag("ignore-cert"),
            width,
            height,
            dpi,
            color_depth: color_depth(params.number("color-depth")),
            client_name: params
                .value("client-name")
                .unwrap_or(DEFAULT_CLIENT_NAME)
                .to_owned(),
            initial_program: params.value("initial-program").map(str::to_owned),
            layout,
            read_only: params.flag("read-only"),
            disable_copy: params.flag("disable-copy"),
            disable_paste: params.flag("disable-paste"),
            line_endings,
            resize_method,
            performance_flags: performance_flags(params),
            force_lossless: params.flag("force-lossless"),
            audio: if params.flag("disable-audio") {
                None
            } else {
                AudioEncoding::negotiate(&info.audio)
            },
        })
    }

    /// Desktop scale factor in percent ([MS-RDPBCGR] 2.2.1.3.2), from the DPI.
    pub fn desktop_scale_factor(&self) -> u32 {
        desktop_scale_factor(self.dpi)
    }

    /// Device scale factor in percent: one of 100, 140 or 180.
    pub fn device_scale_factor(&self) -> u32 {
        device_scale_factor(self.desktop_scale_factor())
    }
}

/// Clamps a requested desktop size to what RDP servers accept. The width is
/// made even, as required by the Display Control channel.
pub fn desktop_size(width: u32, height: u32) -> (u16, u16) {
    let width = width.clamp(MIN_DIMENSION, MAX_DIMENSION) & !1;
    let height = height.clamp(MIN_DIMENSION, MAX_DIMENSION);
    // Both values are at most 8192 and therefore fit.
    (width as u16, height as u16)
}

/// Color depths supported by the bitmap decoders; 8-bit palettes are not.
fn color_depth(requested: Option<u32>) -> u32 {
    match requested {
        Some(depth @ (15 | 16 | 24 | 32)) => depth,
        Some(8) => 16,
        _ => DEFAULT_COLOR_DEPTH,
    }
}

/// Visual effects, with guacd's defaults (everything costly disabled).
fn performance_flags(params: &impl ParamSource) -> PerformanceFlags {
    let mut flags = PerformanceFlags::empty();
    if !params.flag("enable-wallpaper") {
        flags |= PerformanceFlags::DISABLE_WALLPAPER;
    }
    if !params.flag("enable-theming") {
        flags |= PerformanceFlags::DISABLE_THEMING;
    }
    if !params.flag("enable-full-window-drag") {
        flags |= PerformanceFlags::DISABLE_FULLWINDOWDRAG;
    }
    if !params.flag("enable-menu-animations") {
        flags |= PerformanceFlags::DISABLE_MENUANIMATIONS;
    }
    if params.flag("enable-font-smoothing") {
        flags |= PerformanceFlags::ENABLE_FONT_SMOOTHING;
    }
    if params.flag("enable-desktop-composition") {
        flags |= PerformanceFlags::ENABLE_DESKTOP_COMPOSITION;
    }
    flags
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct Map(HashMap<&'static str, &'static str>);

    impl ParamSource for Map {
        fn value(&self, name: &str) -> Option<&str> {
            self.0.get(name).copied().filter(|v| !v.is_empty())
        }
    }

    fn params(pairs: &[(&'static str, &'static str)]) -> Map {
        Map(pairs.iter().copied().collect())
    }

    fn info() -> ClientInfo {
        ClientInfo {
            width: 1281,
            height: 777,
            dpi: 96,
            ..ClientInfo::default()
        }
    }

    #[test]
    fn korean_layout_sets_the_session_input_language() {
        let s = Settings::parse(
            &params(&[("hostname", "win"), ("server-layout", "ko-kr-qwerty")]),
            &info(),
        )
        .unwrap();
        assert_eq!(s.layout, KeyboardLayout::KoKrQwerty);
        assert_eq!(s.layout.layout_id(), 0x412);
        assert_eq!(KeyboardLayout::Failsafe.layout_id(), 0x409);
    }

    #[test]
    fn defaults_follow_guacd() {
        let s = Settings::parse(&params(&[("hostname", "win")]), &info()).unwrap();
        assert_eq!(s.hostname, "win");
        assert_eq!(s.port, 3389);
        assert_eq!(s.security, Security::Any);
        assert!(!s.ignore_cert);
        // Client size, with an even width.
        assert_eq!((s.width, s.height), (1280, 777));
        assert_eq!(s.color_depth, 32);
        assert_eq!(s.client_name, "Guacamole RDP");
        assert_eq!(s.layout, KeyboardLayout::EnUsQwerty);
        assert_eq!(s.resize_method, ResizeMethod::None);
        assert_eq!(s.username, None);
        // The client announced no audio support.
        assert_eq!(s.audio, None);
        assert!(
            s.performance_flags
                .contains(PerformanceFlags::DISABLE_WALLPAPER)
        );
        assert!(
            !s.performance_flags
                .contains(PerformanceFlags::ENABLE_FONT_SMOOTHING)
        );
    }

    #[test]
    fn explicit_values_override_client_info() {
        let s = Settings::parse(
            &params(&[
                ("hostname", "10.0.0.5"),
                ("port", "3390"),
                ("username", "alice"),
                ("password", "pw"),
                ("domain", "CORP"),
                ("security", "NLA"),
                ("ignore-cert", "true"),
                ("width", "1920"),
                ("height", "1080"),
                ("dpi", "192"),
                ("color-depth", "16"),
                ("server-layout", "failsafe"),
                ("resize-method", "display-update"),
                ("read-only", "true"),
                ("enable-wallpaper", "true"),
                ("enable-font-smoothing", "true"),
                ("normalize-clipboard", "windows"),
            ]),
            &info(),
        )
        .unwrap();
        assert_eq!(s.port, 3390);
        assert_eq!(s.username.as_deref(), Some("alice"));
        assert_eq!(s.password.as_deref(), Some("pw"));
        assert_eq!(s.domain.as_deref(), Some("CORP"));
        assert_eq!(s.security, Security::Nla);
        assert!(s.ignore_cert && s.read_only);
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.desktop_scale_factor(), 200);
        assert_eq!(s.device_scale_factor(), 180);
        assert_eq!(s.color_depth, 16);
        assert_eq!(s.layout, KeyboardLayout::Failsafe);
        assert_eq!(s.resize_method, ResizeMethod::DisplayUpdate);
        assert_eq!(s.line_endings, LineEndings::Windows);
        assert!(
            !s.performance_flags
                .contains(PerformanceFlags::DISABLE_WALLPAPER)
        );
        assert!(
            s.performance_flags
                .contains(PerformanceFlags::ENABLE_FONT_SMOOTHING)
        );
    }

    #[test]
    fn security_modes() {
        let parse = |mode: &'static str| Settings::parse(&params(&[("security", mode)]), &info());
        assert_eq!(parse("tls").unwrap().security, Security::Tls);
        assert_eq!(parse("nla-ext").unwrap().security, Security::Nla);
        assert_eq!(parse("any").unwrap().security, Security::Any);
        assert!(matches!(
            parse("rdp"),
            Err(SettingsError::UnsupportedSecurity(_))
        ));
        assert!(matches!(
            parse("bogus"),
            Err(SettingsError::UnknownSecurity(_))
        ));
        let no_auth = Settings::parse(&params(&[("disable-auth", "true")]), &info()).unwrap();
        assert_eq!(no_auth.security, Security::Tls);
        assert!(Security::Any.allows_nla() && Security::Any.allows_tls());
        assert!(!Security::Tls.allows_nla() && !Security::Nla.allows_tls());
    }

    #[test]
    fn sizes_and_depths_are_clamped() {
        assert_eq!(desktop_size(10, 10), (200, 200));
        assert_eq!(desktop_size(99_999, 99_999), (8192, 8192));
        assert_eq!(desktop_size(1023, 767), (1022, 767));
        assert_eq!(color_depth(Some(24)), 24);
        assert_eq!(color_depth(Some(8)), 16);
        assert_eq!(color_depth(Some(7)), 32);
        assert_eq!(color_depth(None), 32);
    }

    #[test]
    fn audio_follows_client_support_unless_disabled() {
        let info = ClientInfo {
            audio: vec!["audio/L8".into(), "audio/L16".into()],
            ..info()
        };
        let enabled = Settings::parse(&params(&[]), &info).unwrap();
        assert_eq!(enabled.audio, Some(AudioEncoding::L16));
        let disabled = Settings::parse(&params(&[("disable-audio", "true")]), &info).unwrap();
        assert_eq!(disabled.audio, None);
    }

    #[test]
    fn dpi_follows_the_client_unless_set() {
        let client = ClientInfo { dpi: 192, ..info() };
        let auto = Settings::parse(&params(&[]), &client).unwrap();
        assert_eq!((auto.dpi, auto.desktop_scale_factor()), (192, 200));
        let fixed = Settings::parse(&params(&[("dpi", "144")]), &client).unwrap();
        assert_eq!((fixed.dpi, fixed.desktop_scale_factor()), (144, 150));
        for junk in ["0", "-96", "auto"] {
            let s = Settings::parse(&params(&[("dpi", junk)]), &client).unwrap();
            assert_eq!(s.dpi, 192, "dpi={junk}");
        }
    }

    #[test]
    fn resize_method_is_off_unless_display_update() {
        assert_eq!(ResizeMethod::parse(None), ResizeMethod::None);
        assert_eq!(ResizeMethod::parse(Some("")), ResizeMethod::None);
        assert_eq!(ResizeMethod::parse(Some("reconnect")), ResizeMethod::None);
        assert_eq!(
            ResizeMethod::parse(Some("display-update")),
            ResizeMethod::DisplayUpdate
        );
    }

    #[test]
    fn scale_factors_of_the_offered_scales() {
        // (percent, DPI as the web client stores it, device scale factor)
        let cases = [
            (100, 96, 100),
            (125, 120, 100),
            (150, 144, 140),
            (175, 168, 140),
            (200, 192, 180),
            (250, 240, 180),
            (300, 288, 180),
        ];
        for (percent, dpi, device) in cases {
            assert_eq!(desktop_scale_factor(dpi), percent, "dpi {dpi}");
            assert_eq!(device_scale_factor(percent), device, "{percent} %");
        }
        // Out of range values are clamped to what the protocol allows.
        assert_eq!(desktop_scale_factor(48), 100);
        assert_eq!(desktop_scale_factor(1200), 500);
        assert_eq!(desktop_scale_factor(u32::MAX), 500);
    }

    #[test]
    fn line_endings() {
        assert_eq!(LineEndings::Windows.apply("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(LineEndings::Unix.apply("a\r\nb\nc"), "a\nb\nc");
        assert_eq!(LineEndings::Preserve.apply("a\r\nb\n"), "a\r\nb\n");
    }
}
