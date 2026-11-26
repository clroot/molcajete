//! Relay of an RDP connection for a client that runs RDP itself: IronRDP's
//! web client, which reaches the server through an RDCleanPath proxy
//! ([`ironrdp_rdcleanpath`]).
//!
//! ```text
//! browser (IronRDP) ── WebSocket ── gateway ── TCP ── molcajeted ── TCP/TLS ── RDP server
//!                      raw bytes             raw bytes after `ready`
//! ```
//!
//! The browser cannot open TCP connections or run TLS itself, so the proxy
//! does both: the client sends its X.224 Connection Request in an
//! RDCleanPath request, the proxy connects, completes the negotiation and
//! the TLS handshake, and answers with the server's X.224 Connection
//! Confirm and a certificate chain. From then on the browser's bytes are
//! the content of the TLS stream.
//!
//! The connection's stored credentials never reach the browser. It gets a
//! one-time credential instead (`relay-username`, `relay-password`), and
//! molcajeted puts the real credentials in where the server reads them:
//!
//! - **NLA** (HYBRID, HYBRID_EX): molcajeted sends the browser its own
//!   certificate, so the browser's CredSSP, bound to that certificate's
//!   key, ends here. molcajeted checks the one-time credential as a CredSSP
//!   server and then authenticates to the server as a CredSSP client with
//!   the stored credentials. The server's Early User Authorization Result
//!   is passed on to the browser.
//! - **Every protocol**: the Client Info PDU, the one PDU of the stream that
//!   carries the user name and password, is decoded, its credentials are
//!   replaced by the stored ones (with auto-logon when a password is known)
//!   and it is encoded again. Without NLA this is the only place the
//!   server learns the credentials.
//!
//! After the Client Info PDU both directions are copied unchanged.
//! Standard RDP Security (RC4) is refused: there the Client Info PDU is
//! encrypted between the browser and the server.

use std::io;
use std::time::Duration;

use ironrdp_connector::credssp::CredsspSequence as ClientCredssp;
use ironrdp_connector::sspi::credssp::EarlyUserAuthResult;
use ironrdp_connector::sspi::generator::GeneratorState;
use ironrdp_connector::sspi::{AuthIdentity, Username};
use ironrdp_connector::{Credentials, ServerName};
use ironrdp_core::{WriteBuf, decode, encode_vec};
use ironrdp_pdu::mcs::SendDataRequest;
use ironrdp_pdu::nego::{ConnectionConfirm, ConnectionRequest, NegoRequestData, SecurityProtocol};
use ironrdp_pdu::rdp::ClientInfoPdu;
use ironrdp_pdu::rdp::client_info::ClientInfoFlags;
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{Action, PduHint};
use ironrdp_rdcleanpath::{DetectionResult, RDCleanPath, RDCleanPathPdu};
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tracing::{info, warn};

use super::connect::RdpStream;
use super::settings::{Security, Settings};
use super::tls;
use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::jump::Chain;
use crate::remote::{self, CountingStream};

/// The one-time credential the browser authenticates with.
pub const ARGS: &[&str] = &["relay-username", "relay-password"];

/// Upper bound for each step before the stream is spliced.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest RDCleanPath request accepted from the browser.
const MAX_REQUEST: usize = 64 * 1024;

/// Name molcajeted's certificate is issued to. The browser does not check
/// it: it only binds CredSSP to the certificate's key.
const CERTIFICATE_NAME: &str = "molcajete";

#[derive(Debug, thiserror::Error)]
enum RelayError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unable to connect to the RDP server: {0}")]
    Upstream(String),
    #[error("timed out")]
    Timeout,
    #[error("the server refused the security negotiation: {0}")]
    Negotiation(String),
    #[error("TLS handshake with the server failed: {0}")]
    Tls(io::Error),
    #[error("the browser did not authenticate with its one-time credential: {0}")]
    Browser(String),
    #[error("authentication with the server failed: {0}")]
    Server(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Serves one relayed connection after the handshake. `ready` has been
/// sent, so everything from here on, errors included, is RDCleanPath and
/// RDP.
pub async fn run(reader: GuacReader, writer: GuacWriter, info: ClientInfo, params: Params) {
    let mut socket = writer.into_inner();
    let (settings, jump, relay) = match prepare(&params, &info) {
        Ok(prepared) => prepared,
        Err(err) => {
            warn!(error = %err, "relay refused");
            if let Ok(der) = RDCleanPathPdu::new_general_error().to_der() {
                let _ = socket.write_all(&der).await;
            }
            return;
        }
    };
    let (read_half, leftover) = reader.into_parts();
    let browser_read = io::Cursor::new(leftover).chain(read_half);
    let browser = tokio::io::join(browser_read, socket);
    match relay_connection(browser, &settings, &jump, &relay).await {
        Ok(()) => info!("relay closed"),
        Err(err) => warn!(error = %err, "relay failed"),
    }
}

fn prepare(params: &Params, info: &ClientInfo) -> Result<(Settings, Chain, OneTime), String> {
    let settings = Settings::parse(params, info).map_err(|err| err.to_string())?;
    let jump = Chain::from_params(params)?;
    let (Some(username), Some(password)) =
        (params.get("relay-username"), params.get("relay-password"))
    else {
        return Err("the relay needs a one-time credential".to_owned());
    };
    let relay = OneTime {
        username: username.to_owned(),
        password: password.to_owned(),
    };
    Ok((settings, jump, relay))
}

/// The browser's one-time credential.
struct OneTime {
    username: String,
    password: String,
}

async fn step<T>(future: impl Future<Output = Result<T, RelayError>>) -> Result<T, RelayError> {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .map_err(|_| RelayError::Timeout)?
}

async fn relay_connection<B>(
    browser: B,
    settings: &Settings,
    jump: &Chain,
    relay: &OneTime,
) -> Result<(), RelayError>
where
    B: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    let mut browser = TokioFramed::new(browser);

    // 1. The RDCleanPath request. Its destination and proxy token are not
    // used: the gateway chose the connection and authorized the user.
    let request = step(async {
        let bytes = browser.read_by_hint(&RDCLEANPATH_HINT).await?;
        let pdu = RDCleanPathPdu::from_der(&bytes)
            .map_err(|err| RelayError::BadRequest(format!("invalid RDCleanPath PDU: {err}")))?;
        match pdu.into_enum() {
            Ok(RDCleanPath::Request {
                x224_connection_request,
                ..
            }) => Ok(x224_connection_request.as_bytes().to_vec()),
            _ => Err(RelayError::BadRequest(
                "expected an RDCleanPath request".to_owned(),
            )),
        }
    })
    .await?;

    match negotiate(&mut browser, &request, settings, jump, relay).await {
        // 6. Up to the Client Info PDU, then both directions unchanged.
        Ok(server) => splice(browser, server, settings, relay).await,
        Err(Failed::BeforeResponse(err)) => {
            // The browser is still waiting for the RDCleanPath response and
            // shows this error.
            if let Ok(der) = RDCleanPathPdu::new_general_error().to_der() {
                let _ = browser.write_all(&der).await;
            }
            Err(err)
        }
        Err(Failed::After(err)) => Err(err),
    }
}

/// An error, and whether the browser has had the RDCleanPath response.
enum Failed {
    BeforeResponse(RelayError),
    After(RelayError),
}

async fn negotiate<B>(
    browser: &mut TokioFramed<B>,
    request: &[u8],
    settings: &Settings,
    jump: &Chain,
    relay: &OneTime,
) -> Result<TokioFramed<RdpStream>, Failed>
where
    B: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    use Failed::{After, BeforeResponse};

    let request = server_request(request, settings).map_err(BeforeResponse)?;
    let upstream = step(async {
        jump.connect(&settings.hostname, settings.port)
            .await
            .map_err(|err| RelayError::Upstream(err.to_string()))
    })
    .await
    .map_err(BeforeResponse)?;
    let mut server = TokioFramed::new(CountingStream::new(
        upstream,
        remote::received_bytes("rdp-relay"),
    ));

    // 2. X.224 negotiation with the server.
    let confirm_bytes = step(async {
        server
            .write_all(&encode_vec(&X224(request)).map_err(io::Error::other)?)
            .await?;
        let (_, bytes) = server.read_pdu().await?;
        Ok(bytes.to_vec())
    })
    .await
    .map_err(BeforeResponse)?;
    let protocol = match decode::<X224<ConnectionConfirm>>(&confirm_bytes) {
        Ok(X224(ConnectionConfirm::Response { protocol, .. })) => protocol,
        Ok(X224(ConnectionConfirm::Failure { code })) => {
            // The browser explains the failure code itself.
            if let Ok(der) = RDCleanPathPdu::new_negotiation_error(confirm_bytes.clone())
                .and_then(|p| p.to_der())
            {
                let _ = browser.write_all(&der).await;
            }
            return Err(After(RelayError::Negotiation(format!("{code:?}"))));
        }
        Err(err) => {
            return Err(BeforeResponse(RelayError::Negotiation(err.to_string())));
        }
    };
    let nla = protocol.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX);
    if !nla && !protocol.contains(SecurityProtocol::SSL) {
        return Err(BeforeResponse(RelayError::Negotiation(
            "the server selected Standard RDP Security, which the relay does not support"
                .to_owned(),
        )));
    }
    if nla && (settings.username.is_none() || settings.password.is_none()) {
        return Err(BeforeResponse(RelayError::Server(
            "the server requires NLA and the connection has no stored user name and password"
                .to_owned(),
        )));
    }

    // 3. TLS with the server.
    let (tcp, leftover) = server.into_inner();
    let (tls_stream, server_key) = step(async {
        tls::upgrade(tcp, &settings.hostname, settings.ignore_cert)
            .await
            .map_err(RelayError::Tls)
    })
    .await
    .map_err(BeforeResponse)?;
    let mut server = TokioFramed::new_with_leftover(tls_stream, leftover);

    // 4. The RDCleanPath response, with molcajeted's certificate.
    let certificate = certificate().map_err(BeforeResponse)?;
    let response = RDCleanPathPdu::new_response(
        format!("{}:{}", settings.hostname, settings.port),
        confirm_bytes,
        [certificate.der.clone()],
    )
    .and_then(|pdu| pdu.to_der())
    .map_err(|err| BeforeResponse(RelayError::Io(io::Error::other(err))))?;
    browser
        .write_all(&response)
        .await
        .map_err(|err| After(err.into()))?;
    info!(?protocol, "RDP relay negotiated");

    // 5. CredSSP on both sides.
    if nla {
        step(accept_browser(
            browser,
            relay,
            certificate.public_key.clone(),
        ))
        .await
        .map_err(After)?;
        // IronRDP's CredSSP client reads the server's Early User
        // Authorization Result (HYBRID_EX) itself, so the browser, whose
        // CredSSP ended here, gets one from molcajeted.
        let result = step(authenticate_server(
            &mut server,
            settings,
            protocol,
            server_key,
        ))
        .await;
        if protocol.contains(SecurityProtocol::HYBRID_EX) {
            let early = if result.is_ok() {
                EarlyUserAuthResult::Success
            } else {
                EarlyUserAuthResult::AccessDenied
            };
            let mut bytes = Vec::with_capacity(4);
            early
                .to_buffer(&mut bytes)
                .map_err(|err| After(err.into()))?;
            browser
                .write_all(&bytes)
                .await
                .map_err(|err| After(err.into()))?;
        }
        result.map_err(After)?;
    }

    Ok(server)
}

/// The Connection Request sent to the server: the browser's, without its
/// cookie (which names the one-time user), asking only for the protocols
/// both the browser and the connection's `security` allow.
fn server_request(request: &[u8], settings: &Settings) -> Result<ConnectionRequest, RelayError> {
    let X224(mut request) = decode::<X224<ConnectionRequest>>(request).map_err(|err| {
        RelayError::BadRequest(format!("invalid X.224 Connection Request: {err}"))
    })?;
    let mut allowed = SecurityProtocol::empty();
    if settings.security.allows_tls() {
        allowed |= SecurityProtocol::SSL;
    }
    if settings.security.allows_nla() {
        allowed |= SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX;
    }
    request.protocol &= allowed;
    if request.protocol.is_empty() {
        return Err(RelayError::BadRequest(format!(
            "the browser offers none of the security protocols the connection allows ({})",
            match settings.security {
                Security::Tls => "TLS",
                Security::Nla => "NLA",
                Security::Any => "TLS or NLA",
            }
        )));
    }
    request.nego_data = settings
        .username
        .as_ref()
        .map(|username| NegoRequestData::cookie(username.clone()));
    Ok(request)
}

/// Checks the browser's one-time credential as a CredSSP server.
async fn accept_browser<B>(
    browser: &mut TokioFramed<B>,
    relay: &OneTime,
    public_key: Vec<u8>,
) -> Result<(), RelayError>
where
    B: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    let username =
        Username::new(&relay.username, None).map_err(|err| RelayError::Browser(err.to_string()))?;
    let identity = AuthIdentity {
        username,
        password: relay.password.clone().into(),
    };
    let mut sequence = ironrdp_acceptor::credssp::CredsspSequence::init(
        &identity,
        ServerName::new(CERTIFICATE_NAME),
        public_key,
        None,
    )
    .map_err(|err| RelayError::Browser(err.to_string()))?;
    let mut buf = WriteBuf::new();
    loop {
        let hint = match sequence.next_pdu_hint() {
            Ok(Some(hint)) => hint,
            Ok(None) => return Ok(()),
            Err(err) => return Err(RelayError::Browser(err.to_string())),
        };
        let pdu = browser.read_by_hint(hint).await?;
        let Some(request) = sequence
            .decode_client_message(&pdu)
            .map_err(|err| RelayError::Browser(err.to_string()))?
        else {
            return Ok(());
        };
        let result = {
            let mut generator = sequence.process_ts_request(request);
            match generator.start() {
                GeneratorState::Completed(result) => result,
                // NTLM needs no network; Kerberos is not offered.
                GeneratorState::Suspended(_) => {
                    return Err(RelayError::Browser(
                        "CredSSP asked for a network request".to_owned(),
                    ));
                }
            }
        };
        buf.clear();
        let (written, _) = sequence
            .handle_process_result(result, &mut buf)
            .map_err(|err| RelayError::Browser(err.to_string()))?;
        if let Some(len) = written.size() {
            browser.write_all(&buf[..len]).await?;
        }
    }
}

/// Authenticates to the server as a CredSSP client with the stored
/// credentials.
async fn authenticate_server<S>(
    server: &mut TokioFramed<S>,
    settings: &Settings,
    protocol: SecurityProtocol,
    server_key: Vec<u8>,
) -> Result<(), RelayError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    let credentials = Credentials::UsernamePassword {
        username: settings.username.clone().unwrap_or_default(),
        password: settings.password.clone().unwrap_or_default(),
    };
    let (sequence, ts_request) = ClientCredssp::init(
        credentials,
        settings.domain.as_deref(),
        protocol,
        ServerName::new(settings.hostname.clone()),
        server_key,
        None,
    )
    .map_err(|err| RelayError::Server(err.to_string()))?;
    let mut buf = WriteBuf::new();
    ironrdp_tokio::perform_credssp(
        server,
        &mut super::connect::NoNetwork,
        &mut buf,
        sequence,
        ts_request,
    )
    .await
    .map_err(|err| RelayError::Server(super::connect::report(&err)))
}

/// Forwards the browser's PDUs, replacing the credentials of the Client
/// Info PDU, then copies both directions unchanged until either side ends.
async fn splice<B, S>(
    browser: TokioFramed<B>,
    server: TokioFramed<S>,
    settings: &Settings,
    relay: &OneTime,
) -> Result<(), RelayError>
where
    B: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    let (browser, browser_leftover) = browser.into_inner();
    let (server, server_leftover) = server.into_inner();
    let (browser_read, mut browser_write) = tokio::io::split(browser);
    let (server_read, mut server_write) = tokio::io::split(server);

    let to_browser = async move {
        browser_write.write_all(&server_leftover).await?;
        let mut server_read = server_read;
        tokio::io::copy(&mut server_read, &mut browser_write).await?;
        browser_write.shutdown().await
    };
    let to_server = async move {
        let mut framed = TokioFramed::new_with_leftover(browser_read, browser_leftover);
        loop {
            let (action, pdu) = framed.read_pdu().await?;
            if action == Action::X224
                && let Some(rewritten) = rewrite_client_info(&pdu, settings, relay)?
            {
                server_write.write_all(&rewritten).await?;
                break;
            }
            server_write.write_all(&pdu).await?;
        }
        let (mut browser_read, rest) = framed.into_inner();
        server_write.write_all(&rest).await?;
        tokio::io::copy(&mut browser_read, &mut server_write).await?;
        server_write.shutdown().await
    };
    // Either side closing ends the relay.
    tokio::select! {
        result = to_browser => result?,
        result = to_server => result?,
    }
    Ok(())
}

/// The PDU with the stored credentials if it is the Client Info PDU,
/// `None` for any other PDU. A Client Info PDU without the one-time
/// credential is refused.
fn rewrite_client_info(
    pdu: &[u8],
    settings: &Settings,
    relay: &OneTime,
) -> io::Result<Option<Vec<u8>>> {
    let Ok(X224(data)) = decode::<X224<SendDataRequest<'_>>>(pdu) else {
        return Ok(None);
    };
    let Ok(mut info) = decode::<ClientInfoPdu>(&data.user_data) else {
        return Ok(None);
    };
    let credentials = &mut info.client_info.credentials;
    if credentials.username != relay.username {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Client Info PDU does not carry the one-time user",
        ));
    }
    credentials.username = settings.username.clone().unwrap_or_default();
    credentials.password = settings.password.clone().unwrap_or_default();
    credentials.domain = settings.domain.clone();
    info.client_info.password_cookie = None;
    // Log on with the stored password instead of showing a login screen,
    // as molcajeted's own RDP sessions do.
    info.client_info
        .flags
        .set(ClientInfoFlags::AUTOLOGON, settings.password.is_some());
    let user_data = encode_vec(&info).map_err(io::Error::other)?;
    let request = SendDataRequest {
        initiator_id: data.initiator_id,
        channel_id: data.channel_id,
        user_data: user_data.into(),
    };
    encode_vec(&X224(request))
        .map(Some)
        .map_err(io::Error::other)
}

/// molcajeted's certificate for the browser side, created once per process.
struct Certificate {
    der: Vec<u8>,
    public_key: Vec<u8>,
}

fn certificate() -> Result<&'static Certificate, RelayError> {
    static CERTIFICATE: std::sync::OnceLock<Result<Certificate, String>> =
        std::sync::OnceLock::new();
    CERTIFICATE
        .get_or_init(|| {
            let key = rcgen::generate_simple_self_signed(vec![CERTIFICATE_NAME.to_owned()])
                .map_err(|err| err.to_string())?;
            let der = key.cert.der().to_vec();
            let public_key = tls::public_key(&der).map_err(|err| err.to_string())?;
            Ok(Certificate { der, public_key })
        })
        .as_ref()
        .map_err(|err| RelayError::Io(io::Error::other(err.clone())))
}

/// Size of an RDCleanPath PDU (DER), bounded by [`MAX_REQUEST`].
#[derive(Debug)]
struct RdCleanPathHint;

const RDCLEANPATH_HINT: RdCleanPathHint = RdCleanPathHint;

impl PduHint for RdCleanPathHint {
    fn find_size(&self, bytes: &[u8]) -> ironrdp_core::DecodeResult<Option<(bool, usize)>> {
        match RDCleanPathPdu::detect(bytes) {
            DetectionResult::Detected { total_length, .. } if total_length <= MAX_REQUEST => {
                Ok(Some((true, total_length)))
            }
            DetectionResult::NotEnoughBytes => Ok(None),
            _ => Err(ironrdp_core::other_err!(
                "RDCleanPath",
                "not an RDCleanPath PDU"
            )),
        }
    }
}
