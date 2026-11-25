//! Connection sequence: TCP, security negotiation, TLS upgrade, CredSSP and
//! the rest of the RDP connection sequence, driven by IronRDP's
//! [`ClientConnector`].
//!
//! The sequence is split in two ([`begin`] and [`Negotiated::finish`]) so
//! that missing credentials can be requested from the Guacamole user once it
//! is known that the server selected Network Level Authentication.

use std::io;
use std::time::Duration;

use ironrdp_connector::{
    BitmapConfig, ClientConnector, ConnectionOutcome, ConnectionResult, ConnectorError,
    ConnectorErrorKind, Credentials, DesktopSize, ServerName,
};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::rdp::capability_sets::{
    MajorPlatformType, RailSupportLevel, client_codecs_capabilities,
};
use ironrdp_pdu::rdp::client_info::TimezoneInfo;
use ironrdp_tokio::{NetworkClient, TokioFramed, Upgraded};
use molcajete_protocol::Status;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::audio::SoundSink;
use super::clipboard::GuacClipboard;
use super::settings::{ResizeMethod, Settings};
use super::tls;

/// The upgraded (TLS) transport of an RDP connection.
pub type RdpStream = TlsStream<TcpStream>;
pub type RdpFramed = TokioFramed<RdpStream>;

/// Client build number advertised to the server (FreeRDP's). Servers treat
/// very old builds specially: xrdp, for one, never resizes the desktop of a
/// client whose build is 419 or lower.
const CLIENT_BUILD: u32 = 18363;

const TCP_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound for each half of the connection sequence.
const SEQUENCE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("unable to reach {host}:{port}: {source}")]
    Unreachable {
        host: String,
        port: u16,
        source: io::Error,
    },
    #[error("timed out while connecting to the RDP server")]
    Timeout,
    #[error("TLS handshake failed: {0}")]
    Tls(io::Error),
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("RDP connection failed: {0}")]
    Protocol(String),
    #[error("{0}")]
    Unsupported(&'static str),
}

impl ConnectError {
    /// Guacamole status reported to the client.
    pub fn status(&self) -> Status {
        match self {
            Self::Unreachable { .. } => Status::UPSTREAM_NOT_FOUND,
            Self::Timeout => Status::UPSTREAM_TIMEOUT,
            Self::Authentication(_) => Status::CLIENT_UNAUTHORIZED,
            Self::Tls(_) | Self::Protocol(_) | Self::Unsupported(_) => Status::UPSTREAM_ERROR,
        }
    }

    fn from_connector(error: ConnectorError) -> Self {
        let message = report(&error);
        match error.kind() {
            ConnectorErrorKind::Credssp(_) | ConnectorErrorKind::AccessDenied => {
                Self::Authentication(message)
            }
            _ => Self::Protocol(message),
        }
    }
}

/// Formats an error with its chain of sources.
fn report(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(err) = source {
        let text = err.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = err.source();
    }
    message
}

/// Kerberos is not supported, so CredSSP never needs to reach a KDC.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!(
            "Kerberos is not supported (no KDC access)"
        ))
    }
}

fn connector_config(settings: &Settings) -> ironrdp_connector::Config {
    let codecs = client_codecs_capabilities(&[]).unwrap_or_default();
    ironrdp_connector::Config {
        desktop_size: DesktopSize {
            width: settings.width,
            height: settings.height,
        },
        monitor_layout: None,
        desktop_scale_factor: settings.desktop_scale_factor(),
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: Some(settings.device_scale_factor()),
        monitor_extended_layout: None,
        relative_mouse_input: false,
        enable_tls: settings.security.allows_tls(),
        enable_credssp: settings.security.allows_nla(),
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: settings.username.clone().unwrap_or_default(),
            password: settings.password.clone().unwrap_or_default(),
        },
        domain: settings.domain.clone(),
        client_build: CLIENT_BUILD,
        client_name: settings.client_name.clone(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: settings.layout.layout_id(),
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: Some(BitmapConfig {
            lossy_compression: !settings.force_lossless,
            color_depth: settings.color_depth,
            codecs,
        }),
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: settings.initial_program.clone().unwrap_or_default(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        // Log on with the given password instead of showing a login screen.
        autologon: settings.password.is_some(),
        // Without this the server mutes the session instead of redirecting
        // its sound.
        enable_audio_playback: settings.audio.is_some(),
        enable_audio_capture: false,
        performance_flags: settings.performance_flags,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        // No bulk compression: IronRDP's reactivation sequence rejects
        // Share Data PDUs flagged as compressed, which xrdp sends after a
        // resize once compression is negotiated. Bitmaps stay compressed by
        // their codecs.
        compression_type: None,
        // Pointer bitmaps are forwarded to the client, not drawn into the image.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
    }
}

fn build_connector(
    settings: &Settings,
    client_addr: std::net::SocketAddr,
    sound: Option<&SoundSink>,
) -> ClientConnector {
    let mut drdynvc = DrdynvcClient::new();
    if settings.resize_method == ResizeMethod::DisplayUpdate {
        drdynvc = drdynvc.with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    }
    if let Some(sound) = sound {
        drdynvc = drdynvc.with_dynamic_channel(sound.dynamic_channel());
    }
    let clipboard = GuacClipboard::new(!settings.disable_copy);
    let connector = ClientConnector::new(connector_config(settings), client_addr)
        .with_static_channel(drdynvc)
        .with_static_channel(ironrdp_cliprdr::Cliprdr::new(Box::new(clipboard)));
    match sound {
        Some(sound) => connector.with_static_channel(sound.static_channel()),
        None => connector,
    }
}

/// A connection whose security layer is established, before CredSSP and
/// the remaining connection sequence.
pub struct Negotiated {
    connector: ClientConnector,
    framed: RdpFramed,
    upgraded: Upgraded,
    server_public_key: Vec<u8>,
    hostname: String,
}

/// Connects, negotiates the security protocol and upgrades to TLS. Sound is
/// redirected to `sound` if given.
pub async fn begin(
    settings: &Settings,
    sound: Option<&SoundSink>,
) -> Result<Negotiated, ConnectError> {
    let tcp = tokio::time::timeout(
        TCP_TIMEOUT,
        TcpStream::connect((settings.hostname.as_str(), settings.port)),
    )
    .await
    .map_err(|_| ConnectError::Timeout)?
    .map_err(|source| ConnectError::Unreachable {
        host: settings.hostname.clone(),
        port: settings.port,
        source,
    })?;
    let _ = tcp.set_nodelay(true);
    let client_addr = tcp
        .local_addr()
        .map_err(|source| ConnectError::Unreachable {
            host: settings.hostname.clone(),
            port: settings.port,
            source,
        })?;

    let mut connector = build_connector(settings, client_addr, sound);
    let mut framed = TokioFramed::new(tcp);
    let should_upgrade = with_timeout(ironrdp_tokio::connect_begin(&mut framed, &mut connector))
        .await?
        .map_err(ConnectError::from_connector)?;

    let (tcp, leftover) = framed.into_inner();
    let (tls_stream, server_public_key) =
        with_timeout(tls::upgrade(tcp, &settings.hostname, settings.ignore_cert))
            .await?
            .map_err(ConnectError::Tls)?;
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    Ok(Negotiated {
        connector,
        framed: TokioFramed::new_with_leftover(tls_stream, leftover),
        upgraded,
        server_public_key,
        hostname: settings.hostname.clone(),
    })
}

impl Negotiated {
    /// Credentials CredSSP will need but which are not known yet.
    pub fn missing_credentials(&self) -> Vec<&'static str> {
        missing_credentials(&self.connector)
    }

    /// Supplies a credential requested from the user.
    pub fn set_credential(&mut self, name: &str, value: String) {
        set_credential(&mut self.connector.config, name, value);
    }

    /// Runs CredSSP (if negotiated) and the rest of the connection sequence.
    pub async fn finish(mut self) -> Result<(ConnectionResult, RdpFramed), ConnectError> {
        let server_name = ServerName::new(self.hostname);
        let outcome = with_timeout(ironrdp_tokio::connect_finalize(
            self.upgraded,
            self.connector,
            &mut self.framed,
            &mut NoNetwork,
            server_name,
            self.server_public_key,
            None,
        ))
        .await?
        .map_err(ConnectError::from_connector)?;
        match outcome {
            ConnectionOutcome::Connected(result) => Ok((result, self.framed)),
            ConnectionOutcome::Redirected(_) => Err(ConnectError::Unsupported(
                "the server redirected the connection (load balancing), which is not supported",
            )),
        }
    }
}

/// Credentials a connector still lacks for CredSSP (none unless the server
/// selected NLA).
fn missing_credentials(connector: &ClientConnector) -> Vec<&'static str> {
    if !connector.should_perform_credssp() {
        return Vec::new();
    }
    let Credentials::UsernamePassword { username, password } = &connector.config.credentials else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    if username.is_empty() {
        missing.push("username");
    }
    if password.is_empty() {
        missing.push("password");
    }
    missing
}

fn set_credential(config: &mut ironrdp_connector::Config, name: &str, value: String) {
    match (name, &mut config.credentials) {
        ("domain", _) => config.domain = Some(value),
        ("username", Credentials::UsernamePassword { username, .. }) => *username = value,
        ("password", Credentials::UsernamePassword { password, .. }) => {
            *password = value;
            config.autologon = true;
        }
        _ => {}
    }
}

async fn with_timeout<T>(future: impl Future<Output = T>) -> Result<T, ConnectError> {
    tokio::time::timeout(SEQUENCE_TIMEOUT, future)
        .await
        .map_err(|_| ConnectError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::ClientInfo;
    use crate::rdp::audio::AudioEncoding;
    use crate::rdp::settings::ParamSource;

    struct Empty;

    impl ParamSource for Empty {
        fn value(&self, _: &str) -> Option<&str> {
            None
        }
    }

    fn settings() -> Settings {
        Settings::parse(&Empty, &ClientInfo::default()).unwrap()
    }

    #[test]
    fn config_reflects_settings() {
        let mut s = settings();
        s.password = Some("pw".into());
        s.color_depth = 16;
        let config = connector_config(&s);
        assert!(config.enable_tls && config.enable_credssp);
        assert!(!config.enable_standard_rdp_security);
        assert!(config.autologon);
        assert_eq!(config.bitmap.as_ref().map(|b| b.color_depth), Some(16));
        assert_eq!(config.keyboard_layout, 0x409);
        assert!(config.client_build > 419, "xrdp only resizes newer clients");
        assert!(config.enable_server_pointer && !config.pointer_software_rendering);
        assert_eq!(
            (config.desktop_size.width, config.desktop_size.height),
            (1024, 768)
        );
    }

    #[test]
    fn connector_starts_in_initial_state() {
        let connector = build_connector(&settings(), "127.0.0.1:1".parse().unwrap(), None);
        assert!(matches!(
            connector.state,
            ironrdp_connector::ClientConnectorState::ConnectionInitiationSendRequest
        ));
        assert!(!connector.config.enable_audio_playback);
    }

    #[test]
    fn audio_joins_the_rdpsnd_channel() {
        let mut s = settings();
        s.audio = Some(AudioEncoding::L16);
        let (sink, _output) = crate::rdp::audio::output(AudioEncoding::L16);
        let connector = build_connector(&s, "127.0.0.1:1".parse().unwrap(), Some(&sink));
        assert!(connector.config.enable_audio_playback);
        assert!(
            connector
                .static_channels
                .get_by_type::<ironrdp_rdpsnd::client::Rdpsnd>()
                .is_some()
        );
    }

    #[test]
    fn credentials_are_requested_only_for_nla() {
        let mut connector = build_connector(&settings(), "127.0.0.1:1".parse().unwrap(), None);
        // TLS (or still negotiating): the server authenticates by itself.
        assert!(missing_credentials(&connector).is_empty());
        connector.state = ironrdp_connector::ClientConnectorState::Credssp {
            selected_protocol: ironrdp_pdu::nego::SecurityProtocol::HYBRID,
        };
        assert_eq!(missing_credentials(&connector), ["username", "password"]);
        set_credential(&mut connector.config, "username", "alice".into());
        set_credential(&mut connector.config, "domain", "CORP".into());
        assert_eq!(missing_credentials(&connector), ["password"]);
        assert!(!connector.config.autologon);
        set_credential(&mut connector.config, "password", "secret".into());
        assert!(missing_credentials(&connector).is_empty());
        assert!(connector.config.autologon);
        assert_eq!(connector.config.domain.as_deref(), Some("CORP"));
    }

    #[test]
    fn errors_map_to_guacamole_statuses() {
        let unreachable = ConnectError::Unreachable {
            host: "h".into(),
            port: 1,
            source: io::Error::from(io::ErrorKind::ConnectionRefused),
        };
        assert_eq!(unreachable.status(), Status::UPSTREAM_NOT_FOUND);
        let denied = ConnectError::from_connector(ConnectorError::new(
            "CredSSP",
            ConnectorErrorKind::AccessDenied,
        ));
        assert_eq!(denied.status(), Status::CLIENT_UNAUTHORIZED);
        let general = ConnectError::from_connector(ConnectorError::new(
            "decode",
            ConnectorErrorKind::General,
        ));
        assert_eq!(general.status(), Status::UPSTREAM_ERROR);
        assert_eq!(ConnectError::Timeout.status(), Status::UPSTREAM_TIMEOUT);
    }

    #[tokio::test]
    async fn refused_connections_are_unreachable() {
        // Bind then drop a listener to get a port that refuses connections.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut s = settings();
        s.hostname = "127.0.0.1".into();
        s.port = port;
        let error = begin(&s, None).await.err().expect("connection must fail");
        assert_eq!(error.status(), Status::UPSTREAM_NOT_FOUND, "{error}");
    }
}
