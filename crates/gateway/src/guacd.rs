//! Connection to guacd and the Guacamole handshake.
//!
//! ```text
//! gateway -> guacd : select,<protocol>;
//! guacd -> gateway : args,VERSION_1_5_0,hostname,port,...;
//! gateway -> guacd : size / audio / video / image / timezone / name
//! gateway -> guacd : connect,VERSION_1_5_0,<values...>;
//! guacd -> gateway : ready,<connection id>;
//! ```
//!
//! molcajeted lists one more parameter in `args`,
//! [`molcajete_protocol::binary::HANDSHAKE_ARG`]. When the browser takes raw
//! stream data, the gateway answers it with the framing version and
//! molcajeted sends records after `ready` instead of text. Stock guacd does
//! not list the parameter and is never asked.

use std::collections::BTreeMap;
use std::time::Duration;

use molcajete_protocol::{
    Instruction, InstructionBuilder, Limits, Parser, ProtocolVersion, Status, binary,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Information about the client display and capabilities, sent during the
/// handshake.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub audio: Vec<String>,
    pub video: Vec<String>,
    pub image: Vec<String>,
    pub timezone: Option<String>,
    pub name: Option<String>,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 768,
            dpi: 96,
            audio: Vec::new(),
            video: Vec::new(),
            image: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
            timezone: None,
            name: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("cannot reach guacd: {0}")]
    Connect(#[source] std::io::Error),
    #[error("guacd I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("guacd closed the connection during the handshake")]
    Closed,
    #[error("guacd rejected the connection: {message} ({status})")]
    Rejected { status: Status, message: String },
    #[error("protocol error during handshake: {0}")]
    Protocol(String),
    #[error("handshake timed out")]
    Timeout,
}

impl HandshakeError {
    /// Guacamole status reported to the browser when the handshake fails.
    pub fn status(&self) -> Status {
        match self {
            Self::Connect(_) => Status::UPSTREAM_NOT_FOUND,
            Self::Io(_) | Self::Closed => Status::UPSTREAM_ERROR,
            Self::Rejected { status, .. } => *status,
            Self::Protocol(_) => Status::SERVER_ERROR,
            Self::Timeout => Status::UPSTREAM_TIMEOUT,
        }
    }
}

/// An established, handshaken guacd connection.
pub struct GuacdSession {
    pub stream: TcpStream,
    /// Parser holding any bytes guacd sent after `ready`. Those bytes belong
    /// to the session and must be forwarded to the client.
    pub parser: Parser,
    /// Connection ID assigned by guacd (usable for joining via `select`).
    pub id: String,
    pub version: ProtocolVersion,
    /// The daemon agreed to send binary records
    /// (`molcajete_protocol::binary`) from here on. The bytes left in
    /// `parser` are then the start of the first records.
    pub binary: bool,
}

/// Connects and performs the handshake. With `want_binary`, a daemon that
/// offers binary records is asked for them (see [`GuacdSession::binary`]).
pub async fn connect(
    addr: (&str, u16),
    select: &str,
    parameters: &BTreeMap<String, String>,
    info: &ClientInfo,
    want_binary: bool,
    timeout: Duration,
) -> Result<GuacdSession, HandshakeError> {
    let handshake = connect_inner(addr, select, parameters, info, want_binary);
    match tokio::time::timeout(timeout, handshake).await {
        Ok(result) => result,
        Err(_) => Err(HandshakeError::Timeout),
    }
}

async fn connect_inner(
    addr: (&str, u16),
    select: &str,
    parameters: &BTreeMap<String, String>,
    info: &ClientInfo,
    want_binary: bool,
) -> Result<GuacdSession, HandshakeError> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(HandshakeError::Connect)?;
    stream.set_nodelay(true)?;
    let mut parser = Parser::with_limits(Limits::LENIENT);

    stream
        .write_all(
            InstructionBuilder::new("select")
                .str_arg(select)
                .finish()
                .as_bytes(),
        )
        .await?;

    let args = expect(&mut stream, &mut parser, "args").await?;

    // The first `args` element is the protocol version when guacd is new
    // enough to announce one; we answer with the highest version both sides
    // support.
    let mut version = ProtocolVersion::V1_0_0;
    let mut values = Vec::with_capacity(args.args.len());
    let mut binary = false;
    for (i, name) in args.args.iter().enumerate() {
        if i == 0
            && let Some(v) = ProtocolVersion::parse(name)
        {
            version = v.min(ProtocolVersion::LATEST);
            values.push(version.as_str().to_owned());
            continue;
        }
        // The framing is the gateway's business alone: a connection
        // parameter of that name is never passed on.
        if name == binary::HANDSHAKE_ARG {
            binary = want_binary;
            let value = if binary {
                binary::HANDSHAKE_VERSION
            } else {
                ""
            };
            values.push(value.to_owned());
            continue;
        }
        values.push(parameters.get(name).cloned().unwrap_or_default());
    }

    let mut out = String::new();
    out += &InstructionBuilder::new("size")
        .arg(info.width)
        .arg(info.height)
        .arg(info.dpi)
        .finish();
    out += &InstructionBuilder::new("audio").args(&info.audio).finish();
    out += &InstructionBuilder::new("video").args(&info.video).finish();
    out += &InstructionBuilder::new("image").args(&info.image).finish();
    if version.supports_timezone_handshake()
        && let Some(tz) = &info.timezone
    {
        out += &InstructionBuilder::new("timezone").str_arg(tz).finish();
    }
    if version.supports_name_handshake()
        && let Some(name) = &info.name
    {
        out += &InstructionBuilder::new("name").str_arg(name).finish();
    }
    out += &InstructionBuilder::new("connect").args(&values).finish();
    stream.write_all(out.as_bytes()).await?;

    let ready = expect(&mut stream, &mut parser, "ready").await?;
    let id = ready
        .args
        .first()
        .cloned()
        .ok_or_else(|| HandshakeError::Protocol("`ready` without connection id".into()))?;

    Ok(GuacdSession {
        stream,
        parser,
        id,
        version,
        binary,
    })
}

/// Reads the next instruction and checks its opcode. An `error` instruction
/// is turned into [`HandshakeError::Rejected`].
async fn expect(
    stream: &mut TcpStream,
    parser: &mut Parser,
    opcode: &str,
) -> Result<Instruction, HandshakeError> {
    let ins = read_instruction(stream, parser).await?;
    if ins.opcode == "error" {
        let status = Status::parse(ins.arg(1)).unwrap_or(Status::SERVER_ERROR);
        return Err(HandshakeError::Rejected {
            status,
            message: ins.arg(0).to_owned(),
        });
    }
    if ins.opcode != opcode {
        return Err(HandshakeError::Protocol(format!(
            "expected `{opcode}`, got `{}`",
            ins.opcode
        )));
    }
    Ok(ins)
}

async fn read_instruction(
    stream: &mut TcpStream,
    parser: &mut Parser,
) -> Result<Instruction, HandshakeError> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(ins) = parser
            .next_instruction()
            .map_err(|e| HandshakeError::Protocol(e.to_string()))?
        {
            return Ok(ins);
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(HandshakeError::Closed);
        }
        parser.feed(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Minimal fake guacd that records what the gateway sends.
    async fn fake_guacd(
        listener: TcpListener,
        reply_args: &'static str,
        after_connect: &'static str,
    ) -> Vec<Instruction> {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut parser = Parser::new();
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            while let Some(ins) = parser.next_instruction().unwrap() {
                let opcode = ins.opcode.clone();
                received.push(ins);
                match opcode.as_str() {
                    "select" => sock.write_all(reply_args.as_bytes()).await.unwrap(),
                    "connect" => {
                        sock.write_all(after_connect.as_bytes()).await.unwrap();
                        return received;
                    }
                    _ => {}
                }
            }
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                return received;
            }
            parser.feed(&buf[..n]);
        }
    }

    #[tokio::test]
    async fn performs_handshake_and_keeps_trailing_data() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(fake_guacd(
            listener,
            "4.args,13.VERSION_1_5_0,8.hostname,4.port,8.password;",
            "5.ready,5.$abcd;4.size,1.0,2.80,2.24;",
        ));

        let mut params = BTreeMap::new();
        params.insert("hostname".to_string(), "example".to_string());
        params.insert("port".to_string(), "5900".to_string());
        let info = ClientInfo {
            timezone: Some("Asia/Seoul".into()),
            name: Some("tester".into()),
            ..ClientInfo::default()
        };

        let mut session = connect(
            ("127.0.0.1", port),
            "vnc",
            &params,
            &info,
            // Stock guacd does not offer binary records, so asking changes
            // nothing.
            true,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(session.id, "$abcd");
        assert_eq!(session.version, ProtocolVersion::V1_5_0);
        assert!(!session.binary);
        let trailing = session.parser.next_instruction().unwrap().unwrap();
        assert_eq!(trailing, Instruction::new("size", ["0", "80", "24"]));

        let received = server.await.unwrap();
        let opcodes: Vec<_> = received.iter().map(|i| i.opcode.as_str()).collect();
        assert_eq!(
            opcodes,
            [
                "select", "size", "audio", "video", "image", "timezone", "name", "connect"
            ]
        );
        let connect = received.last().unwrap();
        assert_eq!(connect.args, ["VERSION_1_5_0", "example", "5900", ""]);
    }

    #[tokio::test]
    async fn asks_molcajeted_for_binary_records_only_when_wanted() {
        // A connection parameter of the same name must not decide.
        let mut params = BTreeMap::new();
        params.insert("hostname".to_string(), "example".to_string());
        params.insert(binary::HANDSHAKE_ARG.to_string(), "1".to_string());
        for (want_binary, value) in [(true, "1"), (false, "")] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(fake_guacd(
                listener,
                "4.args,13.VERSION_1_5_0,8.hostname,16.molcajete-binary;",
                // Followed by the start of a record.
                "5.ready,5.$abcd;\0\x06\0\0",
            ));
            let session = connect(
                ("127.0.0.1", port),
                "vnc",
                &params,
                &ClientInfo::default(),
                want_binary,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(session.binary, want_binary);
            assert_eq!(session.parser.pending(), b"\0\x06\0\0");
            let received = server.await.unwrap();
            let connect = received.last().unwrap();
            assert_eq!(connect.args, ["VERSION_1_5_0", "example", value]);
        }
    }

    #[tokio::test]
    async fn surfaces_guacd_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"5.error,23.No such protocol plugin,3.519;")
                .await
                .unwrap();
        });
        let err = connect(
            ("127.0.0.1", port),
            "nope",
            &BTreeMap::new(),
            &ClientInfo::default(),
            false,
            Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.status(), Status(519));
    }

    #[tokio::test]
    async fn unreachable_guacd_maps_to_upstream_not_found() {
        // Bind then drop to obtain a port nobody listens on.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let err = connect(
            ("127.0.0.1", port),
            "vnc",
            &BTreeMap::new(),
            &ClientInfo::default(),
            false,
            Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.status(), Status::UPSTREAM_NOT_FOUND);
    }
}
