//! Server side of the Guacamole handshake.
//!
//! ```text
//! client -> select,<protocol>;
//! guacd  -> args,VERSION_1_5_0,<arg names...>;
//! client -> size / audio / video / image / timezone / name (any order)
//! client -> connect,<version>,<values...>;
//! guacd  -> ready,<connection id>;
//! ```
//!
//! One connection parameter is not a setting of the remote desktop
//! protocol: the last name in `args` is
//! [`molcajete_protocol::binary::HANDSHAKE_ARG`]. A molcajete gateway whose
//! browser accepts raw stream data answers it with the framing version, and
//! everything after `ready` is then sent as records. Any other client sends
//! an empty value (or none) and gets the text protocol, like from guacd.

use std::collections::HashMap;

use molcajete_protocol::{Instruction, InstructionBuilder, ProtocolVersion, binary};

use crate::io::{GuacReader, GuacWriter};

/// What the client told us about itself during the handshake.
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
    pub version: ProtocolVersion,
    /// The client takes stream data as raw bytes
    /// (`molcajete_protocol::binary`) instead of base64 `blob` instructions.
    pub binary: bool,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 768,
            dpi: 96,
            audio: Vec::new(),
            video: Vec::new(),
            image: Vec::new(),
            timezone: None,
            name: None,
            version: ProtocolVersion::V1_0_0,
            binary: false,
        }
    }
}

/// Connection parameters by name.
#[derive(Debug, Clone, Default)]
pub struct Params(HashMap<String, String>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn bool(&self, name: &str) -> bool {
        matches!(self.get(name), Some("true"))
    }

    pub fn parse<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.get(name)?.trim().parse().ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("client disconnected during handshake")]
    Closed,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Reads the `select` instruction.
pub async fn read_select(reader: &mut GuacReader) -> Result<String, HandshakeError> {
    let ins = reader.next().await?.ok_or(HandshakeError::Closed)?;
    if ins.opcode != "select" {
        return Err(HandshakeError::Protocol(format!(
            "expected `select`, got `{}`",
            ins.opcode
        )));
    }
    Ok(ins.arg(0).to_owned())
}

/// Sends `args`, collects client information until `connect`, then answers
/// `ready`. When the client asked for it, the writer is switched to binary
/// records for everything that follows.
pub async fn negotiate(
    reader: &mut GuacReader,
    writer: &mut GuacWriter,
    arg_names: &[&str],
    connection_id: &str,
) -> Result<(ClientInfo, Params), HandshakeError> {
    writer.push(
        &InstructionBuilder::new("args")
            .str_arg(ProtocolVersion::LATEST.as_str())
            .args(arg_names)
            .str_arg(binary::HANDSHAKE_ARG)
            .finish(),
    );
    writer.flush().await?;

    let mut info = ClientInfo::default();
    let connect: Instruction = loop {
        let ins = reader.next().await?.ok_or(HandshakeError::Closed)?;
        match ins.opcode.as_str() {
            "size" => {
                info.width = ins.arg(0).parse().unwrap_or(info.width).clamp(1, 8192);
                info.height = ins.arg(1).parse().unwrap_or(info.height).clamp(1, 8192);
                info.dpi = ins.arg(2).parse().unwrap_or(info.dpi);
            }
            "audio" => info.audio = ins.args,
            "video" => info.video = ins.args,
            "image" => info.image = ins.args,
            "timezone" => info.timezone = Some(ins.arg(0).to_owned()),
            "name" => info.name = Some(ins.arg(0).to_owned()),
            "connect" => break ins,
            _ => {}
        }
    };

    let mut values = connect.args.into_iter();
    let mut pending_first = None;
    if let Some(first) = values.next() {
        match ProtocolVersion::parse(&first) {
            Some(v) => info.version = v.min(ProtocolVersion::LATEST),
            // Pre-1.1 clients send values without a version.
            None => pending_first = Some(first),
        }
    }
    let params = Params(
        arg_names
            .iter()
            .chain([&binary::HANDSHAKE_ARG])
            .map(|n| n.to_string())
            .zip(pending_first.into_iter().chain(values))
            .collect(),
    );
    info.binary = params.get(binary::HANDSHAKE_ARG) == Some(binary::HANDSHAKE_VERSION);

    writer.push(
        &InstructionBuilder::new("ready")
            .str_arg(connection_id)
            .finish(),
    );
    writer.flush().await?;
    if info.binary {
        writer.set_binary();
    }
    Ok((info, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use molcajete_protocol::{Parser, parse_all};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn negotiates_like_guacd() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (rd, wr) = sock.into_split();
            let mut reader = GuacReader::new(rd);
            let mut writer = GuacWriter::new(wr);
            let protocol = read_select(&mut reader).await.unwrap();
            let (info, params) = negotiate(
                &mut reader,
                &mut writer,
                &["hostname", "port", "password"],
                "$abc",
            )
            .await
            .unwrap();
            (protocol, info, params)
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"6.select,3.vnc;").await.unwrap();
        let mut parser = Parser::new();
        let mut buf = [0u8; 1024];
        let args = loop {
            if let Some(i) = parser.next_instruction().unwrap() {
                break i;
            }
            let n = client.read(&mut buf).await.unwrap();
            parser.feed(&buf[..n]);
        };
        assert_eq!(
            args.args,
            [
                "VERSION_1_5_0",
                "hostname",
                "port",
                "password",
                "molcajete-binary"
            ]
        );
        client
            .write_all(
                b"4.size,4.1920,4.1080,2.96;5.audio;5.video;5.image,9.image/png,10.image/jpeg;\
                  8.timezone,10.Asia/Seoul;7.connect,13.VERSION_1_5_0,4.host,4.5900,0.;",
            )
            .await
            .unwrap();
        let ready = loop {
            if let Some(i) = parser.next_instruction().unwrap() {
                break i;
            }
            let n = client.read(&mut buf).await.unwrap();
            parser.feed(&buf[..n]);
        };
        assert_eq!(ready.opcode, "ready");
        assert_eq!(ready.args, ["$abc"]);

        let (protocol, info, params) = server.await.unwrap();
        assert_eq!(protocol, "vnc");
        assert_eq!((info.width, info.height, info.dpi), (1920, 1080, 96));
        assert_eq!(info.image, ["image/png", "image/jpeg"]);
        assert_eq!(info.timezone.as_deref(), Some("Asia/Seoul"));
        assert_eq!(params.get("hostname"), Some("host"));
        assert_eq!(params.parse::<u16>("port"), Some(5900));
        assert_eq!(params.get("password"), None);
        // No value for the framing parameter: the text protocol.
        assert!(!info.binary);
        let _ = parse_all("");
    }

    /// Runs the handshake against `connect`, then lets the server send an
    /// instruction and a blob. Returns what the client reads after `ready`.
    async fn after_ready(connect: &'static str) -> (bool, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (rd, wr) = sock.into_split();
            let mut reader = GuacReader::new(rd);
            let mut writer = GuacWriter::new(wr);
            read_select(&mut reader).await.unwrap();
            let (info, _) = negotiate(&mut reader, &mut writer, &["hostname"], "$abc")
                .await
                .unwrap();
            writer.push("3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;");
            writer.buffer().blob(1, &[0, 0xFF, 0x80, b';']);
            writer.push("3.end,1.1;");
            writer.flush().await.unwrap();
            info.binary
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"6.select,3.vnc;").await.unwrap();
        client.write_all(connect.as_bytes()).await.unwrap();
        let binary = server.await.unwrap();
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        let ready = b"5.ready,4.$abc;";
        let at = received
            .windows(ready.len())
            .position(|w| w == ready)
            .expect("ready");
        (binary, received[at + ready.len()..].to_vec())
    }

    #[tokio::test]
    async fn stock_clients_get_base64_blobs() {
        let text = "3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;4.blob,1.1,8.AP+AOw==;3.end,1.1;";
        // The stock web application sends an empty value for a parameter it
        // has no setting for; older clients send fewer values than names.
        for connect in [
            "7.connect,13.VERSION_1_5_0,4.host,0.;",
            "7.connect,13.VERSION_1_5_0,4.host;",
            "7.connect,4.host;",
            // Not the version of the framing we speak.
            "7.connect,13.VERSION_1_5_0,4.host,1.2;",
            "7.connect,13.VERSION_1_5_0,4.host,4.true;",
        ] {
            let (binary, after) = after_ready(connect).await;
            assert!(!binary, "{connect}");
            assert_eq!(String::from_utf8(after).unwrap(), text, "{connect}");
        }
    }

    #[tokio::test]
    async fn molcajete_gateway_gets_raw_blobs() {
        use molcajete_protocol::binary::{Record, records};

        let (binary, after) = after_ready("7.connect,13.VERSION_1_5_0,4.host,1.1;").await;
        assert!(binary);
        let records: Vec<_> = records(&after).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            records,
            [
                Record::Instructions(b"3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;"),
                Record::Blob {
                    stream: 1,
                    data: &[0, 0xFF, 0x80, b';']
                },
                Record::Instructions(b"3.end,1.1;"),
            ]
        );
    }
}
