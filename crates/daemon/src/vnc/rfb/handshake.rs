//! Connection setup: version negotiation, security handshake and initialisation messages.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

use super::auth::{self, CHALLENGE_LEN};
use super::{ConnectOptions, RfbError, ServerInfo, encoding, writer};

const SECURITY_INVALID: u8 = 0;
const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;

/// Upper bound for server-provided strings (failure reasons, desktop name).
const MAX_STRING_LEN: u32 = 64 * 1024;

/// Protocol versions this client speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ProtocolVersion {
    V3_3,
    V3_7,
    V3_8,
}

impl ProtocolVersion {
    fn banner(self) -> &'static [u8; 12] {
        match self {
            Self::V3_3 => b"RFB 003.003\n",
            Self::V3_7 => b"RFB 003.007\n",
            Self::V3_8 => b"RFB 003.008\n",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecurityType {
    None,
    VncAuth,
}

/// Runs the whole handshake and sends SetPixelFormat and SetEncodings.
pub(super) async fn run<R, W>(
    input: &mut R,
    output: &mut W,
    options: &ConnectOptions,
) -> Result<ServerInfo, RfbError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut banner = [0u8; 12];
    input.read_exact(&mut banner).await?;
    let version = select_version(&banner)?;
    output.write_all(version.banner()).await?;
    debug!(
        server = %String::from_utf8_lossy(&banner).trim_end(),
        ?version,
        "negotiated RFB version"
    );

    authenticate(input, output, version, options.password.as_deref()).await?;

    output.write_all(&[u8::from(options.shared)]).await?;
    let info = read_server_init(input).await?;
    debug!(width = info.width, height = info.height, name = %info.name, "received ServerInit");

    output.write_all(&writer::encode_set_pixel_format()).await?;
    let encodings = effective_encodings(options.encodings.as_deref());
    output
        .write_all(&writer::encode_set_encodings(&encodings)?)
        .await?;
    output.flush().await?;
    Ok(info)
}

/// Picks the version to speak from the server's ProtocolVersion banner: the lower of the
/// server's version and 3.8, with unknown 3.x minors mapped as RFC 6143 recommends.
pub(super) fn select_version(banner: &[u8; 12]) -> Result<ProtocolVersion, RfbError> {
    let invalid = || {
        RfbError::Protocol(format!(
            "invalid protocol version banner {:?}",
            String::from_utf8_lossy(banner)
        ))
    };
    if &banner[..4] != b"RFB " || banner[7] != b'.' || banner[11] != b'\n' {
        return Err(invalid());
    }
    let number = |digits: &[u8]| -> Option<u32> {
        if digits.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(digits).ok()?.parse().ok()
        } else {
            None
        }
    };
    let major = number(&banner[4..7]).ok_or_else(invalid)?;
    let minor = number(&banner[8..11]).ok_or_else(invalid)?;
    match (major, minor) {
        (0..=2, _) => Err(RfbError::Protocol(format!(
            "unsupported RFB version {major}.{minor}"
        ))),
        (3, 0..=6) => Ok(ProtocolVersion::V3_3),
        (3, 7) => Ok(ProtocolVersion::V3_7),
        _ => Ok(ProtocolVersion::V3_8),
    }
}

async fn authenticate<R, W>(
    input: &mut R,
    output: &mut W,
    version: ProtocolVersion,
    password: Option<&str>,
) -> Result<(), RfbError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let security = if version == ProtocolVersion::V3_3 {
        // RFB 3.3: the server alone decides the security type.
        match u8::try_from(input.read_u32().await?) {
            Ok(SECURITY_INVALID) => return Err(refused(read_string(input).await?)),
            Ok(SECURITY_NONE) => SecurityType::None,
            Ok(SECURITY_VNC_AUTH) => SecurityType::VncAuth,
            _ => return Err(RfbError::UnsupportedSecurity),
        }
    } else {
        let count = input.read_u8().await?;
        if count == 0 {
            return Err(refused(read_string(input).await?));
        }
        let mut offered = vec![0u8; usize::from(count)];
        input.read_exact(&mut offered).await?;
        let security = choose_security(&offered, password.is_some())?;
        let chosen = match security {
            SecurityType::None => SECURITY_NONE,
            SecurityType::VncAuth => SECURITY_VNC_AUTH,
        };
        output.write_all(&[chosen]).await?;
        security
    };
    debug!(?security, "selected security type");

    if security == SecurityType::VncAuth {
        let password = password.ok_or_else(missing_password)?;
        let mut challenge = [0u8; CHALLENGE_LEN];
        input.read_exact(&mut challenge).await?;
        output
            .write_all(&auth::vnc_auth_response(password, &challenge))
            .await?;
    }
    output.flush().await?;

    // Before 3.8 the server only sends SecurityResult after an actual authentication scheme.
    if version == ProtocolVersion::V3_8 || security == SecurityType::VncAuth {
        let result = input.read_u32().await?;
        if result != 0 {
            let reason = if version == ProtocolVersion::V3_8 {
                read_string(input).await?
            } else {
                "authentication rejected by server".to_owned()
            };
            return Err(RfbError::AuthFailed(reason));
        }
    }
    Ok(())
}

/// Chooses a security type from the server's list, honouring the server's preference order and
/// skipping VNC authentication when no password is available.
fn choose_security(offered: &[u8], have_password: bool) -> Result<SecurityType, RfbError> {
    let chosen = offered.iter().find_map(|&security| match security {
        SECURITY_NONE => Some(SecurityType::None),
        SECURITY_VNC_AUTH if have_password => Some(SecurityType::VncAuth),
        _ => None,
    });
    match chosen {
        Some(security) => Ok(security),
        None if offered.contains(&SECURITY_VNC_AUTH) => Err(missing_password()),
        None => {
            warn!(?offered, "server offers no supported security type");
            Err(RfbError::UnsupportedSecurity)
        }
    }
}

fn missing_password() -> RfbError {
    RfbError::AuthFailed("the server requires a password but none was provided".into())
}

fn refused(reason: String) -> RfbError {
    RfbError::Protocol(format!("server refused the connection: {reason}"))
}

async fn read_server_init<R>(input: &mut R) -> Result<ServerInfo, RfbError>
where
    R: AsyncRead + Unpin,
{
    let width = input.read_u16().await?;
    let height = input.read_u16().await?;
    super::check_framebuffer_size(width, height)?;
    let mut native_format = [0u8; 16];
    input.read_exact(&mut native_format).await?;
    debug!(?native_format, "server's native pixel format");
    let name = read_string(input).await?;
    Ok(ServerInfo {
        width,
        height,
        name,
    })
}

/// Reads a string prefixed by a u32 length. RFB does not specify an encoding; UTF-8 is used
/// when valid, Latin-1 otherwise.
async fn read_string<R>(input: &mut R) -> Result<String, RfbError>
where
    R: AsyncRead + Unpin,
{
    let len = input.read_u32().await?;
    if len > MAX_STRING_LEN {
        return Err(RfbError::Protocol(format!(
            "server string of {len} bytes exceeds the limit"
        )));
    }
    let mut bytes = vec![0u8; len as usize];
    input.read_exact(&mut bytes).await?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|err| err.into_bytes().iter().copied().map(char::from).collect()))
}

/// Returns the encodings to announce, dropping any this client cannot decode.
pub(super) fn effective_encodings(requested: Option<&[i32]>) -> Vec<i32> {
    match requested {
        None => encoding::DEFAULT.to_vec(),
        Some(requested) => requested
            .iter()
            .copied()
            .filter(|&code| {
                let supported = encoding::is_supported(code);
                if !supported {
                    warn!(encoding = code, "ignoring unsupported encoding");
                }
                supported
            })
            .collect(),
    }
}
