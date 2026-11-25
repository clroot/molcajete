//! Tests against an in-process mock RFB server, plus an ignored test against a real VNC server.
//!
//! Expected wire bytes and pixels are built independently of the implementation from the RFB
//! specification.

use std::future::Future;
use std::time::Duration;

use flate2::{Compress, Compression, FlushCompress};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::encoding::{
    COPY_RECT, CURSOR, DESKTOP_SIZE, EXTENDED_DESKTOP_SIZE, HEXTILE, LAST_RECT, RAW, RRE, ZRLE,
};
use super::handshake::{ProtocolVersion, select_version};
use super::*;

const TIMEOUT: Duration = Duration::from_secs(10);

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const CYAN: [u8; 4] = [0, 255, 255, 255];

/// SetPixelFormat: 32 bpp, depth 24, little-endian, true colour, max 255, shifts 16/8/0.
const EXPECTED_SET_PIXEL_FORMAT: [u8; 20] = [
    0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
];

// ---------------------------------------------------------------------------------------------
// Mock server plumbing
// ---------------------------------------------------------------------------------------------

/// Starts a one-shot mock server that runs `script` on the first accepted connection.
async fn mock_server<F, Fut>(script: F) -> (ConnectOptions, JoinHandle<()>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        stream.set_nodelay(true).unwrap();
        script(stream).await;
    });
    let options = ConnectOptions {
        host: "127.0.0.1".into(),
        port,
        password: None,
        shared: true,
        encodings: None,
    };
    (options, handle)
}

/// Connects over TCP to the server at the options' address.
async fn connect(options: &ConnectOptions) -> Result<(RfbReader, RfbWriter, ServerInfo), RfbError> {
    let tcp = TcpStream::connect((options.host.as_str(), options.port)).await?;
    super::connect(crate::jump::Upstream::Tcp(tcp), options).await
}

async fn timed<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("test timed out")
}

async fn next(reader: &mut RfbReader) -> RfbEvent {
    timed(reader.next_event()).await.expect("next_event failed")
}

/// Server side of a mock connection.
struct Peer {
    stream: TcpStream,
    /// Write one byte per segment to exercise the client's handling of fragmented reads.
    fragmented: bool,
}

impl Peer {
    fn new(stream: TcpStream, fragmented: bool) -> Self {
        Self { stream, fragmented }
    }

    async fn send(&mut self, data: &[u8]) {
        if self.fragmented {
            for byte in data {
                self.stream.write_all(&[*byte]).await.unwrap();
                self.stream.flush().await.unwrap();
                tokio::task::yield_now().await;
            }
        } else {
            self.stream.write_all(data).await.unwrap();
        }
    }

    async fn recv(&mut self, len: usize) -> Vec<u8> {
        let mut buffer = vec![0u8; len];
        self.stream.read_exact(&mut buffer).await.unwrap();
        buffer
    }

    async fn expect(&mut self, expected: &[u8]) {
        assert_eq!(self.recv(expected.len()).await, expected);
    }

    /// RFB 3.8 handshake with security type None and the shared flag set. Returns the
    /// encodings announced by the client.
    async fn handshake(&mut self, width: u16, height: u16) -> Vec<i32> {
        self.send(b"RFB 003.008\n").await;
        self.expect(b"RFB 003.008\n").await;
        self.send(&[1, 1]).await;
        self.expect(&[1]).await;
        self.send(&0u32.to_be_bytes()).await;
        self.expect(&[1]).await;
        self.server_init(width, height).await
    }

    /// Sends ServerInit and consumes SetPixelFormat and SetEncodings.
    async fn server_init(&mut self, width: u16, height: u16) -> Vec<i32> {
        let name = b"mock desktop";
        let mut init = Vec::new();
        init.extend_from_slice(&width.to_be_bytes());
        init.extend_from_slice(&height.to_be_bytes());
        // Native format: 16 bpp RGB565 big-endian, which the client must override.
        init.extend_from_slice(&[16, 16, 1, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0]);
        init.extend_from_slice(&(name.len() as u32).to_be_bytes());
        init.extend_from_slice(name);
        self.send(&init).await;

        self.expect(&EXPECTED_SET_PIXEL_FORMAT).await;
        let header = self.recv(4).await;
        assert_eq!(header[..2], [2, 0], "SetEncodings header");
        let count = usize::from(u16::from_be_bytes([header[2], header[3]]));
        self.recv(4 * count)
            .await
            .chunks_exact(4)
            .map(|chunk| i32::from_be_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    /// Waits until the client closes the connection and returns whatever it sent meanwhile.
    async fn finish(mut self) -> Vec<u8> {
        let mut rest = Vec::new();
        // A reset (the client dropped unread data) is as good as a close here.
        let _ = self.stream.read_to_end(&mut rest).await;
        rest
    }
}

// ---------------------------------------------------------------------------------------------
// Wire builders and expected images
// ---------------------------------------------------------------------------------------------

/// Deterministic, poorly compressible colour sequence.
fn colour(index: usize) -> [u8; 4] {
    let value = (index as u32).wrapping_add(1).wrapping_mul(2_654_435_761);
    [value as u8, (value >> 8) as u8, (value >> 16) as u8, 255]
}

/// A pixel in the requested wire format: B, G, R, padding (deliberately non-zero).
fn wire(rgba: [u8; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], 0xaa]
}

/// A ZRLE CPIXEL: the three least significant bytes of the little-endian pixel.
fn cpixel(rgba: [u8; 4]) -> [u8; 3] {
    [rgba[2], rgba[1], rgba[0]]
}

fn update(rects: &[Vec<u8>]) -> Vec<u8> {
    update_with_count(rects.len() as u16, rects)
}

fn update_with_count(count: u16, rects: &[Vec<u8>]) -> Vec<u8> {
    let mut message = vec![0, 0];
    message.extend_from_slice(&count.to_be_bytes());
    for rect in rects {
        message.extend_from_slice(rect);
    }
    message
}

fn rect(x: u16, y: u16, width: u16, height: u16, encoding: i32, body: &[u8]) -> Vec<u8> {
    let mut message = Vec::new();
    for value in [x, y, width, height] {
        message.extend_from_slice(&value.to_be_bytes());
    }
    message.extend_from_slice(&encoding.to_be_bytes());
    message.extend_from_slice(body);
    message
}

fn wire_pixels(pixels: &[[u8; 4]]) -> Vec<u8> {
    pixels.iter().flat_map(|&pixel| wire(pixel)).collect()
}

struct Image {
    width: usize,
    pixels: Vec<[u8; 4]>,
}

impl Image {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            pixels: vec![[0, 0, 0, 0]; width * height],
        }
    }

    fn set(&mut self, x: usize, y: usize, rgba: [u8; 4]) {
        self.pixels[y * self.width + x] = rgba;
    }

    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, rgba: [u8; 4]) {
        for row in y..y + height {
            for column in x..x + width {
                self.set(column, row, rgba);
            }
        }
    }

    fn rgba(&self) -> Vec<u8> {
        self.pixels.concat()
    }
}

/// Reference implementation of the VNC authentication response, written independently of
/// `auth.rs`.
fn reference_vnc_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::generic_array::GenericArray;
    use des::cipher::{BlockEncrypt, KeyInit};

    let mut key = [0u8; 8];
    for (index, key_byte) in key.iter_mut().enumerate() {
        let byte = password.get(index).copied().unwrap_or(0);
        for bit in 0..8 {
            if byte & (1 << bit) != 0 {
                *key_byte |= 0x80 >> bit;
            }
        }
    }
    let cipher = des::Des::new_from_slice(&key).unwrap();
    let mut response = *challenge;
    for chunk in response.chunks_exact_mut(8) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------------------------

#[test]
fn version_selection() {
    let cases: [(&[u8; 12], Option<ProtocolVersion>); 10] = [
        (b"RFB 003.003\n", Some(ProtocolVersion::V3_3)),
        (b"RFB 003.005\n", Some(ProtocolVersion::V3_3)),
        (b"RFB 003.007\n", Some(ProtocolVersion::V3_7)),
        (b"RFB 003.008\n", Some(ProtocolVersion::V3_8)),
        (b"RFB 003.889\n", Some(ProtocolVersion::V3_8)),
        (b"RFB 004.001\n", Some(ProtocolVersion::V3_8)),
        (b"RFB 002.000\n", None),
        (b"XYZ 003.008\n", None),
        (b"RFB 003.00a\n", None),
        (b"RFB 003.008 ", None),
    ];
    for (banner, expected) in cases {
        let actual = select_version(banner).ok();
        assert_eq!(actual, expected, "{}", String::from_utf8_lossy(banner));
    }
}

#[test]
fn vnc_auth_known_vectors() {
    // Computed with `openssl enc -des-ecb -nopad` using the bit-reversed password as key.
    let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
    assert_eq!(
        auth::vnc_auth_response("secret", &challenge),
        [
            0xee, 0x22, 0x53, 0x9f, 0x33, 0xa5, 0x98, 0x3e, 0xc1, 0x2f, 0x9c, 0x2e, 0xdb, 0xc9,
            0x95, 0xdd
        ]
    );
    // Passwords longer than eight bytes are truncated.
    let challenge: [u8; 16] = std::array::from_fn(|i| 0xff - 0x11 * i as u8);
    assert_eq!(
        auth::vnc_auth_response("password123", &challenge),
        [
            0x08, 0x6c, 0x5f, 0xea, 0xe2, 0x1b, 0xeb, 0x59, 0x73, 0x8c, 0xfb, 0x96, 0x21, 0x67,
            0x89, 0x19
        ]
    );
    for password in ["", "a", "secret", "12345678", "longer password"] {
        assert_eq!(
            auth::vnc_auth_response(password, &challenge),
            reference_vnc_response(password.as_bytes(), &challenge),
            "{password:?}"
        );
    }
}

#[tokio::test]
async fn handshake_with_none_security() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        let encodings = peer.handshake(1024, 768).await;
        assert_eq!(encodings, [16, 5, 1, 0, -239, -223, -308, -224]);
        peer.finish().await;
    })
    .await;

    let (reader, writer, info) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        info,
        ServerInfo {
            width: 1024,
            height: 768,
            name: "mock desktop".into()
        }
    );
    assert_eq!(reader.framebuffer_size(), (1024, 768));
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn exclusive_session_with_filtered_encodings() {
    let (mut options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[1, 1]).await;
        peer.expect(&[1]).await;
        peer.send(&0u32.to_be_bytes()).await;
        peer.expect(&[0]).await; // shared flag cleared
        // Tight (7) and an unknown pseudo-encoding are dropped; compression level -250 is kept.
        assert_eq!(peer.server_init(640, 480).await, [16, -239, -250]);
        peer.finish().await;
    })
    .await;
    options.shared = false;
    options.encodings = Some(vec![16, 7, -239, -250, -999]);

    let connection = timed(connect(&options)).await.unwrap();
    drop(connection);
    timed(server).await.unwrap();
}

#[tokio::test]
async fn rfb_3_3_with_vnc_auth() {
    let challenge: [u8; 16] = std::array::from_fn(|i| (i * 17 + 3) as u8);
    let (mut options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.003\n").await;
        peer.expect(b"RFB 003.003\n").await;
        peer.send(&2u32.to_be_bytes()).await;
        peer.send(&challenge).await;
        peer.expect(&reference_vnc_response(b"secret", &challenge))
            .await;
        peer.send(&0u32.to_be_bytes()).await;
        peer.expect(&[1]).await;
        peer.server_init(800, 600).await;
        peer.finish().await;
    })
    .await;
    options.password = Some("secret".into());

    let (_, _, info) = timed(connect(&options)).await.unwrap();
    assert_eq!((info.width, info.height), (800, 600));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn rfb_3_7_none_security_has_no_security_result() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.007\n").await;
        peer.expect(b"RFB 003.007\n").await;
        // VNC auth is preferred by the server but no password is configured.
        peer.send(&[2, 2, 1]).await;
        peer.expect(&[1]).await;
        // RFB 3.7 sends no SecurityResult for None: ClientInit follows directly.
        peer.expect(&[1]).await;
        peer.server_init(320, 200).await;
        peer.finish().await;
    })
    .await;

    let (_, _, info) = timed(connect(&options)).await.unwrap();
    assert_eq!((info.width, info.height), (320, 200));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn rfb_3_8_vnc_auth() {
    let challenge: [u8; 16] = std::array::from_fn(|i| (200 - i * 9) as u8);
    let (mut options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[2, 2, 1]).await;
        peer.expect(&[2]).await;
        peer.send(&challenge).await;
        // Only the first eight password bytes are used.
        peer.expect(&reference_vnc_response(b"secret12", &challenge))
            .await;
        peer.send(&0u32.to_be_bytes()).await;
        peer.expect(&[1]).await;
        peer.server_init(1280, 800).await;
        peer.finish().await;
    })
    .await;
    options.password = Some("secret12345".into());

    let (_, _, info) = timed(connect(&options)).await.unwrap();
    assert_eq!((info.width, info.height), (1280, 800));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn auth_failure_reports_reason() {
    let (mut options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[1, 2]).await;
        peer.expect(&[2]).await;
        peer.send(&[7; 16]).await;
        peer.recv(16).await;
        let reason = b"Authentication failed";
        peer.send(&1u32.to_be_bytes()).await;
        peer.send(&(reason.len() as u32).to_be_bytes()).await;
        peer.send(reason).await;
        peer.finish().await;
    })
    .await;
    options.password = Some("wrong".into());

    match timed(connect(&options)).await {
        Err(RfbError::AuthFailed(reason)) => assert_eq!(reason, "Authentication failed"),
        other => panic!("unexpected result: {:?}", other.map(|(_, _, info)| info)),
    }
    timed(server).await.unwrap();
}

#[tokio::test]
async fn missing_password_fails_before_choosing_security() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[1, 2]).await;
        assert!(peer.finish().await.is_empty());
    })
    .await;

    let result = timed(connect(&options)).await;
    assert!(matches!(result, Err(RfbError::AuthFailed(_))));
    drop(result);
    timed(server).await.unwrap();
}

#[tokio::test]
async fn unsupported_security_types() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[2, 16, 19]).await;
        assert!(peer.finish().await.is_empty());
    })
    .await;

    let result = timed(connect(&options)).await;
    assert!(matches!(result, Err(RfbError::UnsupportedSecurity)));
    drop(result);
    timed(server).await.unwrap();
}

#[tokio::test]
async fn server_refusal_reason() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        let reason = b"Too many security failures";
        peer.send(&[0]).await;
        peer.send(&(reason.len() as u32).to_be_bytes()).await;
        peer.send(reason).await;
        peer.finish().await;
    })
    .await;

    match timed(connect(&options)).await {
        Err(RfbError::Protocol(message)) => {
            assert!(message.contains("Too many security failures"), "{message}")
        }
        other => panic!("unexpected result: {:?}", other.map(|(_, _, info)| info)),
    }
    timed(server).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Rectangle encodings
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn raw_rect() {
    let pixels: Vec<[u8; 4]> = (0..6).map(colour).collect();
    let body = wire_pixels(&pixels);
    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(8, 8).await;
        peer.send(&update(&[
            rect(1, 2, 3, 2, RAW, &body),
            rect(4, 4, 2, 2, RAW, &wire_pixels(&[RED; 4])),
            rect(0, 0, 0, 0, RAW, &[]),
        ]))
        .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 2,
            rgba: pixels.concat()
        }
    );
    // Single-colour content is reported as a fill; empty rectangles produce nothing.
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Fill {
            x: 4,
            y: 4,
            width: 2,
            height: 2,
            rgba: RED
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn copy_rect() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(16, 16).await;
        peer.send(&update(&[rect(5, 6, 4, 3, COPY_RECT, &[0, 1, 0, 2])]))
            .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::CopyRect {
            src_x: 1,
            src_y: 2,
            x: 5,
            y: 6,
            width: 4,
            height: 3
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn rre_rect() {
    let mut body = Vec::new();
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&wire(BLUE));
    for (pixel, geometry) in [(RED, [1u16, 1, 2, 2]), (GREEN, [4, 0, 5, 10])] {
        body.extend_from_slice(&wire(pixel));
        for value in geometry {
            body.extend_from_slice(&value.to_be_bytes());
        }
    }
    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(16, 16).await;
        peer.send(&update(&[rect(2, 2, 6, 4, RRE, &body)])).await;
        peer.finish().await;
    })
    .await;

    let mut expected = Image::new(6, 4);
    expected.fill(0, 0, 6, 4, BLUE);
    expected.fill(1, 1, 2, 2, RED);
    expected.fill(4, 0, 2, 4, GREEN); // clipped to the rectangle

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Rect {
            x: 2,
            y: 2,
            width: 6,
            height: 4,
            rgba: expected.rgba()
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn hextile() {
    run_hextile(false).await;
}

#[tokio::test]
async fn hextile_fragmented() {
    run_hextile(true).await;
}

async fn run_hextile(fragmented: bool) {
    // A 35x18 rectangle splits into tiles of 16x16, 16x16, 3x16 and 16x2, 16x2, 3x2.
    let raw_tile: Vec<[u8; 4]> = (0..48).map(|i| colour(500 + i)).collect();
    let mut body = Vec::new();
    // Tile 0: background only.
    body.push(2);
    body.extend_from_slice(&wire(RED));
    // Tile 1: inherited background, new foreground, two foreground subrects.
    body.push(4 | 8);
    body.extend_from_slice(&wire(BLUE));
    body.extend_from_slice(&[2, 0x12, 0x23, 0xa0, 0x5f]);
    // Tile 2: raw.
    body.push(1);
    body.extend_from_slice(&wire_pixels(&raw_tile));
    // Tile 3: background plus two coloured subrects.
    body.push(2 | 8 | 16);
    body.extend_from_slice(&wire(GREEN));
    body.push(2);
    body.extend_from_slice(&wire(WHITE));
    body.extend_from_slice(&[0x00, 0x11]);
    body.extend_from_slice(&wire(YELLOW));
    body.extend_from_slice(&[0x51, 0x30]);
    // Tile 4: no flags, everything inherited.
    body.push(0);
    // Tile 5: inherited foreground; the second subrect overhangs the 3x2 tile.
    body.push(8);
    body.extend_from_slice(&[2, 0x10, 0x11, 0x21, 0x33]);

    let mut expected = Image::new(35, 18);
    expected.fill(0, 0, 16, 16, RED);
    expected.fill(16, 0, 16, 16, RED);
    expected.fill(17, 2, 3, 4, BLUE);
    expected.fill(26, 0, 6, 16, BLUE);
    for (index, &pixel) in raw_tile.iter().enumerate() {
        expected.set(32 + index % 3, index / 3, pixel);
    }
    expected.fill(0, 16, 16, 2, GREEN);
    expected.fill(0, 16, 2, 2, WHITE);
    expected.fill(5, 17, 4, 1, YELLOW);
    expected.fill(16, 16, 16, 2, GREEN);
    expected.fill(32, 16, 3, 2, GREEN);
    expected.fill(33, 16, 2, 2, BLUE);
    expected.fill(34, 17, 1, 1, BLUE);

    let mut solid = vec![2];
    solid.extend_from_slice(&wire(CYAN));

    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, fragmented);
        peer.handshake(40, 20).await;
        peer.send(&update(&[
            rect(2, 1, 35, 18, HEXTILE, &body),
            rect(0, 0, 4, 4, HEXTILE, &solid),
        ]))
        .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    match next(&mut reader).await {
        RfbEvent::Rect {
            x: 2,
            y: 1,
            width: 35,
            height: 18,
            rgba,
        } => assert!(rgba == expected.rgba(), "hextile pixels differ"),
        other => panic!("unexpected event {other:?}"),
    }
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Fill {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
            rgba: CYAN
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

/// One ZRLE tile, encoded from the specification.
enum ZTile {
    Solid([u8; 4]),
    Raw(Vec<[u8; 4]>),
    Packed {
        palette: Vec<[u8; 4]>,
        indices: Vec<usize>,
    },
    PlainRle(Vec<([u8; 4], usize)>),
    PaletteRle {
        palette: Vec<[u8; 4]>,
        runs: Vec<(usize, usize)>,
    },
}

fn push_run_length(out: &mut Vec<u8>, length: usize) {
    let mut rest = length - 1;
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}

impl ZTile {
    fn encode(&self, width: usize, out: &mut Vec<u8>) {
        match self {
            Self::Solid(pixel) => {
                out.push(1);
                out.extend_from_slice(&cpixel(*pixel));
            }
            Self::Raw(pixels) => {
                out.push(0);
                pixels
                    .iter()
                    .for_each(|&pixel| out.extend_from_slice(&cpixel(pixel)));
            }
            Self::Packed { palette, indices } => {
                out.push(palette.len() as u8);
                palette
                    .iter()
                    .for_each(|&pixel| out.extend_from_slice(&cpixel(pixel)));
                let bits = match palette.len() {
                    2 => 1,
                    3 | 4 => 2,
                    _ => 4,
                };
                for row in indices.chunks(width) {
                    let (mut byte, mut used) = (0u8, 0);
                    for &index in row {
                        byte |= (index as u8) << (8 - bits - used);
                        used += bits;
                        if used == 8 {
                            out.push(byte);
                            (byte, used) = (0, 0);
                        }
                    }
                    if used > 0 {
                        out.push(byte);
                    }
                }
            }
            Self::PlainRle(runs) => {
                out.push(128);
                for &(pixel, length) in runs {
                    out.extend_from_slice(&cpixel(pixel));
                    push_run_length(out, length);
                }
            }
            Self::PaletteRle { palette, runs } => {
                out.push(128 + palette.len() as u8);
                palette
                    .iter()
                    .for_each(|&pixel| out.extend_from_slice(&cpixel(pixel)));
                for &(index, length) in runs {
                    if length == 1 {
                        out.push(index as u8);
                    } else {
                        out.push(index as u8 | 0x80);
                        push_run_length(out, length);
                    }
                }
            }
        }
    }

    fn pixels(&self, count: usize) -> Vec<[u8; 4]> {
        let pixels = match self {
            Self::Solid(pixel) => vec![*pixel; count],
            Self::Raw(pixels) => pixels.clone(),
            Self::Packed { palette, indices } => indices.iter().map(|&i| palette[i]).collect(),
            Self::PlainRle(runs) => runs
                .iter()
                .flat_map(|&(pixel, length)| std::iter::repeat_n(pixel, length))
                .collect(),
            Self::PaletteRle { palette, runs } => runs
                .iter()
                .flat_map(|&(index, length)| std::iter::repeat_n(palette[index], length))
                .collect(),
        };
        assert_eq!(pixels.len(), count, "test tile has the wrong pixel count");
        pixels
    }
}

/// Encodes tiles (in ZRLE tile order) into uncompressed ZRLE data and the expected RGBA.
fn zrle_data(tiles: &[ZTile], width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
    let mut data = Vec::new();
    let mut expected = Image::new(width, height);
    let mut tiles = tiles.iter();
    for tile_y in (0..height).step_by(64) {
        for tile_x in (0..width).step_by(64) {
            let (tile_width, tile_height) = (64.min(width - tile_x), 64.min(height - tile_y));
            let tile = tiles.next().expect("too few test tiles");
            tile.encode(tile_width, &mut data);
            for (index, pixel) in tile
                .pixels(tile_width * tile_height)
                .into_iter()
                .enumerate()
            {
                expected.set(
                    tile_x + index % tile_width,
                    tile_y + index / tile_width,
                    pixel,
                );
            }
        }
    }
    assert!(tiles.next().is_none(), "too many test tiles");
    (data, expected.rgba())
}

/// Compresses one rectangle's data with the connection-wide compressor and a sync flush, as
/// ZRLE servers do.
fn zlib_chunk(compressor: &mut Compress, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1024);
    let mut consumed = 0;
    loop {
        let before = compressor.total_in();
        compressor
            .compress_vec(&data[consumed..], &mut out, FlushCompress::Sync)
            .unwrap();
        consumed += (compressor.total_in() - before) as usize;
        if consumed == data.len() && out.len() < out.capacity() {
            return out;
        }
        out.reserve(4096);
    }
}

fn zrle_body(compressed: &[u8]) -> Vec<u8> {
    let mut body = (compressed.len() as u32).to_be_bytes().to_vec();
    body.extend_from_slice(compressed);
    body
}

#[tokio::test]
async fn zrle() {
    run_zrle(false).await;
}

#[tokio::test]
async fn zrle_fragmented() {
    run_zrle(true).await;
}

async fn run_zrle(fragmented: bool) {
    let noise: Vec<[u8; 4]> = (0..64 * 64).map(|i| colour(i * 7 + 1)).collect();

    // Rect A, 128x64: a solid tile and a raw tile.
    let (data_a, expected_a) = zrle_data(&[ZTile::Solid(RED), ZTile::Raw(noise.clone())], 128, 64);
    // Rect B, 100x40: plain RLE (with multi-byte run lengths) and palette RLE tiles.
    let (data_b, expected_b) = zrle_data(
        &[
            ZTile::PlainRle(vec![(BLUE, 1000), (GREEN, 1), (BLACK, 256), (WHITE, 1303)]),
            ZTile::PaletteRle {
                palette: vec![BLACK, YELLOW, CYAN],
                runs: vec![(0, 1), (1, 700), (2, 1), (0, 2), (1, 736)],
            },
        ],
        100,
        40,
    );
    // Rect C, 141x20: packed palettes with 1, 2 and 4 bits per index (the last with padded
    // rows).
    let (data_c, expected_c) = zrle_data(
        &[
            ZTile::Packed {
                palette: vec![RED, BLUE],
                indices: (0..64 * 20).map(|i| (i % 64 + i / 64) % 2).collect(),
            },
            ZTile::Packed {
                palette: vec![RED, GREEN, WHITE],
                indices: (0..64 * 20).map(|i| (i % 64 + 2 * (i / 64)) % 3).collect(),
            },
            ZTile::Packed {
                palette: (0..9).map(|i| colour(1000 + i)).collect(),
                indices: (0..13 * 20).map(|i| (3 * (i % 13) + i / 13) % 9).collect(),
            },
        ],
        141,
        20,
    );
    // Rect D (second update), 64x64: the same raw tile as rect A, which the persistent
    // compressor encodes as back-references into rect A's data.
    let (data_d, expected_d) = zrle_data(&[ZTile::Raw(noise)], 64, 64);
    // Rect E (second update): a single solid tile.
    let (data_e, _) = zrle_data(&[ZTile::Solid(GREEN)], 56, 30);

    let mut compressor = Compress::new(Compression::default(), true);
    let chunks: Vec<Vec<u8>> = [&data_a, &data_b, &data_c, &data_d, &data_e]
        .into_iter()
        .map(|data| zlib_chunk(&mut compressor, data))
        .collect();
    assert!(
        chunks[3].len() < 1024,
        "rect D should compress to back-references, got {} bytes",
        chunks[3].len()
    );

    let first_update = update(&[
        rect(0, 0, 128, 64, ZRLE, &zrle_body(&chunks[0])),
        rect(128, 0, 100, 40, ZRLE, &zrle_body(&chunks[1])),
        rect(0, 64, 141, 20, ZRLE, &zrle_body(&chunks[2])),
    ]);
    let second_update = update(&[
        rect(128, 64, 64, 64, ZRLE, &zrle_body(&chunks[3])),
        rect(200, 0, 56, 30, ZRLE, &zrle_body(&chunks[4])),
    ]);
    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, fragmented);
        peer.handshake(256, 128).await;
        peer.send(&first_update).await;
        peer.send(&second_update).await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    for (x, y, width, height, expected) in [
        (0, 0, 128, 64, &expected_a),
        (128, 0, 100, 40, &expected_b),
        (0, 64, 141, 20, &expected_c),
    ] {
        assert_rect(next(&mut reader).await, (x, y, width, height), expected);
    }
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_rect(next(&mut reader).await, (128, 64, 64, 64), &expected_d);
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Fill {
            x: 200,
            y: 0,
            width: 56,
            height: 30,
            rgba: GREEN
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

fn assert_rect(event: RfbEvent, area: (u16, u16, u16, u16), expected: &[u8]) {
    match event {
        RfbEvent::Rect {
            x,
            y,
            width,
            height,
            rgba,
        } if (x, y, width, height) == area => {
            if let Some(index) =
                (0..rgba.len() / 4).find(|i| rgba[i * 4..][..4] != expected[i * 4..][..4])
            {
                panic!(
                    "pixel {index} of rect {area:?} is {:?}, expected {:?}",
                    &rgba[index * 4..][..4],
                    &expected[index * 4..][..4]
                );
            }
            assert_eq!(rgba.len(), expected.len());
        }
        other => panic!("expected a rect at {area:?}, got {other:?}"),
    }
}

#[test]
fn zrle_rejects_malformed_tiles() {
    let cases: [(&str, Vec<u8>); 5] = [
        ("unused subencoding", vec![17]),
        ("unused subencoding 129", vec![129]),
        ("run past the tile", vec![128, 1, 2, 3, 20]),
        ("truncated solid tile", vec![1, 1, 2]),
        ("palette index out of range", vec![130, 1, 2, 3, 4, 5, 6, 5]),
    ];
    for (name, data) in cases {
        assert!(
            matches!(zrle::decode_tiles(&data, 4, 4), Err(RfbError::Protocol(_))),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Pseudo-encodings
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn desktop_size() {
    let pixels: Vec<[u8; 4]> = (0..4).map(colour).collect();
    let body = wire_pixels(&pixels);
    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(32, 32).await;
        peer.send(&update(&[
            rect(0, 0, 64, 48, DESKTOP_SIZE, &[]),
            // Only valid within the new size.
            rect(60, 40, 2, 2, RAW, &body),
        ]))
        .await;
        // An unchanged size produces no event.
        peer.send(&update(&[rect(0, 0, 64, 48, DESKTOP_SIZE, &[])]))
            .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Resize {
            width: 64,
            height: 48
        }
    );
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Rect {
            x: 60,
            y: 40,
            width: 2,
            height: 2,
            rgba: pixels.concat()
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(reader.framebuffer_size(), (64, 48));
    drop((reader, writer));
    timed(server).await.unwrap();
}

fn extended_desktop_size_body(screen_id: u32, width: u16, height: u16, flags: u32) -> Vec<u8> {
    let mut body = vec![1, 0, 0, 0];
    body.extend_from_slice(&screen_id.to_be_bytes());
    for value in [0, 0, width, height] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    body.extend_from_slice(&flags.to_be_bytes());
    body
}

#[tokio::test]
async fn extended_desktop_size_and_set_desktop_size() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(64, 48).await;
        // Announce support with the current layout (reason 0, status 0).
        peer.send(&update(&[rect(
            0,
            0,
            64,
            48,
            EXTENDED_DESKTOP_SIZE,
            &extended_desktop_size_body(0x1234, 64, 48, 5),
        )]))
        .await;
        peer.expect(&[
            251, 0, 0, 100, 0, 80, 1, 0, 0, 0, 0x12, 0x34, 0, 0, 0, 0, 0, 100, 0, 80, 0, 0, 0, 5,
        ])
        .await;
        // Accept the request (reason 1, status 0).
        peer.send(&update(&[rect(
            1,
            0,
            100,
            80,
            EXTENDED_DESKTOP_SIZE,
            &extended_desktop_size_body(0x1234, 100, 80, 5),
        )]))
        .await;
        // Reject a later request (reason 1, status 1).
        peer.send(&update(&[rect(
            1,
            1,
            120,
            90,
            EXTENDED_DESKTOP_SIZE,
            &extended_desktop_size_body(0x1234, 120, 90, 5),
        )]))
        .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, mut writer, _) = timed(connect(&options)).await.unwrap();
    assert!(!writer.set_desktop_size(100, 80).await.unwrap());
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert!(writer.set_desktop_size(100, 80).await.unwrap());
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Resize {
            width: 100,
            height: 80
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(reader.framebuffer_size(), (100, 80));
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn cursor() {
    // 10x2 cursor so each mask row spans two bytes.
    let pixels: Vec<[u8; 4]> = (0..20).map(colour).collect();
    let visible = |x: usize, y: usize| !(x + y).is_multiple_of(3);
    let mut body = wire_pixels(&pixels);
    for y in 0..2 {
        let mut row = [0u8; 2];
        for x in (0..10).filter(|&x| visible(x, y)) {
            row[x / 8] |= 0x80 >> (x % 8);
        }
        body.extend_from_slice(&row);
    }
    let expected: Vec<u8> = pixels
        .iter()
        .enumerate()
        .flat_map(|(i, &pixel)| {
            if visible(i % 10, i / 10) {
                pixel
            } else {
                [0; 4]
            }
        })
        .collect();

    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(16, 16).await;
        peer.send(&update(&[
            rect(3, 1, 10, 2, CURSOR, &body),
            rect(0, 0, 0, 0, CURSOR, &[]),
        ]))
        .await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Cursor {
            hotspot_x: 3,
            hotspot_y: 1,
            width: 10,
            height: 2,
            rgba: expected
        }
    );
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Cursor {
            hotspot_x: 0,
            hotspot_y: 0,
            width: 0,
            height: 0,
            rgba: Vec::new()
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn last_rect_and_empty_updates() {
    let pixels = [RED, BLUE];
    let body = wire_pixels(&pixels);
    let (options, server) = mock_server(move |stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(8, 8).await;
        peer.send(&update(&[])).await;
        peer.send(&update_with_count(
            0xffff,
            &[
                rect(0, 0, 2, 1, RAW, &body),
                rect(0, 0, 0, 0, LAST_RECT, &[]),
            ],
        ))
        .await;
        peer.send(&[2]).await; // Bell, to show the stream is still in sync
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            rgba: pixels.concat()
        }
    );
    assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
    assert_eq!(next(&mut reader).await, RfbEvent::Bell);
    drop((reader, writer));
    timed(server).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Other messages and errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn bell_clipboard_and_unknown_message() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(8, 8).await;
        // SetColourMapEntries with two entries is skipped.
        peer.send(&[1, 0, 0, 0, 0, 2]).await;
        peer.send(&[0xab; 12]).await;
        peer.send(&[2]).await;
        peer.send(&[3, 0, 0, 0, 0, 0, 0, 5, b'c', b'a', b'f', 0xe9, b'!'])
            .await;
        peer.send(&[99]).await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert_eq!(next(&mut reader).await, RfbEvent::Bell);
    assert_eq!(
        next(&mut reader).await,
        RfbEvent::ClipboardText("café!".into())
    );
    assert!(matches!(
        timed(reader.next_event()).await,
        Err(RfbError::Protocol(_))
    ));
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn rect_outside_framebuffer_is_rejected() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(8, 8).await;
        peer.send(&update(&[rect(7, 0, 2, 1, RAW, &[0; 8])])).await;
        peer.finish().await;
    })
    .await;

    let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
    assert!(matches!(
        timed(reader.next_event()).await,
        Err(RfbError::Protocol(_))
    ));
    drop((reader, writer));
    timed(server).await.unwrap();
}

/// A framebuffer of more than 8192 x 8192 pixels would let every rectangle allocate gigabytes.
#[tokio::test]
async fn oversized_framebuffer_is_refused() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.send(b"RFB 003.008\n").await;
        peer.expect(b"RFB 003.008\n").await;
        peer.send(&[1, 1]).await;
        peer.expect(&[1]).await;
        peer.send(&0u32.to_be_bytes()).await;
        peer.expect(&[1]).await;
        let mut init = vec![0xff, 0xff, 0xff, 0xff];
        init.extend_from_slice(&EXPECTED_SET_PIXEL_FORMAT[4..]);
        init.extend_from_slice(&0u32.to_be_bytes());
        peer.send(&init).await;
        peer.finish().await;
    })
    .await;

    assert!(matches!(
        timed(connect(&options)).await,
        Err(RfbError::Protocol(_))
    ));
    timed(server).await.unwrap();
}

#[tokio::test]
async fn resize_beyond_the_limit_is_refused() {
    for oversized in [
        rect(0, 0, 8193, 8192, DESKTOP_SIZE, &[]),
        rect(
            0,
            0,
            0xffff,
            0xffff,
            EXTENDED_DESKTOP_SIZE,
            &extended_desktop_size_body(1, 0xffff, 0xffff, 0),
        ),
    ] {
        let (options, server) = mock_server(move |stream| async move {
            let mut peer = Peer::new(stream, false);
            peer.handshake(32, 32).await;
            // As large as allowed, and in any shape.
            peer.send(&update(&[rect(0, 0, 0xffff, 1024, DESKTOP_SIZE, &[])]))
                .await;
            peer.send(&update(&[oversized])).await;
            peer.finish().await;
        })
        .await;

        let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
        assert_eq!(
            next(&mut reader).await,
            RfbEvent::Resize {
                width: 0xffff,
                height: 1024
            }
        );
        assert_eq!(next(&mut reader).await, RfbEvent::UpdateEnd);
        assert!(matches!(
            timed(reader.next_event()).await,
            Err(RfbError::Protocol(_))
        ));
        assert_eq!(reader.framebuffer_size(), (0xffff, 1024));
        drop((reader, writer));
        timed(server).await.unwrap();
    }
}

/// Lengths of pixel data, ZRLE payloads and clipboard text that the server announces but does
/// not send end the connection without the reader having allocated for them.
#[tokio::test]
async fn announced_lengths_wait_for_their_data() {
    let raw = rect(0, 0, 8192, 8192, RAW, &[1, 2, 3, 4]);
    let zrle = rect(0, 0, 8192, 8192, ZRLE, &(64u32 << 20).to_be_bytes());
    let mut cut_text = vec![3, 0, 0, 0];
    cut_text.extend_from_slice(&(15u32 << 20).to_be_bytes());
    cut_text.extend_from_slice(b"short");
    for message in [update(&[raw]), update(&[zrle]), cut_text] {
        let (options, server) = mock_server(move |stream| async move {
            let mut peer = Peer::new(stream, false);
            peer.handshake(8192, 8192).await;
            peer.send(&message).await;
            // Closing the connection is the end of the data.
            drop(peer);
        })
        .await;

        let (mut reader, writer, _) = timed(connect(&options)).await.unwrap();
        match timed(reader.next_event()).await {
            Err(RfbError::Io(err)) => {
                assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
            }
            other => panic!("expected the end of the data, got {other:?}"),
        }
        drop((reader, writer));
        timed(server).await.unwrap();
    }
}

#[tokio::test]
async fn client_message_encoding() {
    let (options, server) = mock_server(|stream| async move {
        let mut peer = Peer::new(stream, false);
        peer.handshake(1280, 800).await;
        let expected: Vec<u8> = [
            &[3, 1, 0, 1, 0, 2, 0x01, 0x2c, 0x01, 0x90][..],
            &[3, 0, 0, 0, 0, 0, 0x05, 0x00, 0x03, 0x20],
            &[5, 9, 0, 10, 0, 20],
            &[4, 1, 0, 0, 0, 0, 0xff, 0x0d],
            &[4, 0, 0, 0, 0, 0, 0, 0x61],
            &[
                6, 0, 0, 0, 0, 0, 0, 7, b'h', 0xe9, b'l', b'l', b'o', b' ', b'?',
            ],
        ]
        .concat();
        assert_eq!(peer.finish().await, expected);
    })
    .await;

    let (reader, mut writer, _) = timed(connect(&options)).await.unwrap();
    writer.request_update(true, 1, 2, 300, 400).await.unwrap();
    writer.request_update(false, 0, 0, 1280, 800).await.unwrap();
    writer.pointer(10, 20, 1 | 8).await.unwrap();
    writer.key(0xff0d, true).await.unwrap();
    writer.key(0x61, false).await.unwrap();
    writer.cut_text("héllo €").await.unwrap();
    drop((reader, writer));
    timed(server).await.unwrap();
}

#[test]
fn reader_and_writer_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    fn assert_send_type<T: Send + 'static>() {}
    fn futures(options: &ConnectOptions, reader: &mut RfbReader, writer: &mut RfbWriter) {
        assert_send(&connect(options));
        assert_send(&reader.next_event());
        assert_send(&writer.request_update(false, 0, 0, 1, 1));
        assert_send(&writer.pointer(0, 0, 0));
        assert_send(&writer.key(0, false));
        assert_send(&writer.cut_text(""));
        assert_send(&writer.set_desktop_size(1, 1));
    }
    assert_send_type::<RfbReader>();
    assert_send_type::<RfbWriter>();
    let _ = futures;
}

// ---------------------------------------------------------------------------------------------
// Real server
// ---------------------------------------------------------------------------------------------

/// Framebuffer reconstructed from events, tracking which pixels have been painted.
struct Canvas {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    painted: Vec<bool>,
}

impl Canvas {
    fn new(width: u16, height: u16) -> Self {
        let (width, height) = (usize::from(width), usize::from(height));
        Self {
            width,
            height,
            rgba: vec![0; width * height * 4],
            painted: vec![false; width * height],
        }
    }

    fn paint(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        source: impl Fn(usize, usize) -> [u8; 4],
    ) {
        let (x, y) = (usize::from(x), usize::from(y));
        for row in 0..usize::from(height) {
            for column in 0..usize::from(width) {
                let index = (y + row) * self.width + x + column;
                self.rgba[index * 4..][..4].copy_from_slice(&source(column, row));
                self.painted[index] = true;
            }
        }
    }

    fn apply(&mut self, event: &RfbEvent) {
        match event {
            RfbEvent::Rect {
                x,
                y,
                width,
                height,
                rgba,
            } => {
                let stride = usize::from(*width);
                self.paint(*x, *y, *width, *height, |column, row| {
                    rgba[(row * stride + column) * 4..][..4].try_into().unwrap()
                });
            }
            RfbEvent::Fill {
                x,
                y,
                width,
                height,
                rgba,
            } => self.paint(*x, *y, *width, *height, |_, _| *rgba),
            RfbEvent::CopyRect {
                src_x,
                src_y,
                x,
                y,
                width,
                height,
            } => {
                let source = self.rgba.clone();
                let (src_x, src_y, stride) = (usize::from(*src_x), usize::from(*src_y), self.width);
                self.paint(*x, *y, *width, *height, |column, row| {
                    source[((src_y + row) * stride + src_x + column) * 4..][..4]
                        .try_into()
                        .unwrap()
                });
            }
            _ => {}
        }
    }

    fn fully_painted(&self) -> bool {
        self.painted.iter().all(|&painted| painted)
    }
}

/// Connects to the live server, requests a full update and returns the reconstructed screen.
async fn capture_live(label: &str, encodings: Option<Vec<i32>>) -> Canvas {
    let options = ConnectOptions {
        host: "127.0.0.1".into(),
        port: 5900,
        password: Some("secret".into()),
        shared: true,
        encodings,
    };
    let (mut reader, mut writer, info) =
        tokio::time::timeout(Duration::from_secs(5), connect(&options))
            .await
            .expect("timed out connecting to 127.0.0.1:5900")
            .unwrap_or_else(|err| panic!("cannot connect to x11vnc at 127.0.0.1:5900: {err}"));
    assert_eq!(
        (info.width, info.height),
        (1280, 800),
        "unexpected screen size"
    );

    let started = std::time::Instant::now();
    let mut canvas = Canvas::new(info.width, info.height);
    // One line per FramebufferUpdate, e.g. "Cursor Rect(1280x800)".
    let mut trace = vec![String::new()];
    writer
        .request_update(false, 0, 0, info.width, info.height)
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), reader.next_event())
            .await
            .expect("timed out waiting for the full update")
            .expect("next_event failed");
        canvas.apply(&event);
        let summary = match &event {
            RfbEvent::Rect { width, height, .. } => format!("Rect({width}x{height}) "),
            RfbEvent::Fill { width, height, .. } => format!("Fill({width}x{height}) "),
            RfbEvent::CopyRect { .. } => "CopyRect ".to_owned(),
            RfbEvent::Cursor {
                width,
                height,
                rgba,
                ..
            } => {
                assert_eq!(rgba.len(), usize::from(*width) * usize::from(*height) * 4);
                format!("Cursor({width}x{height}) ")
            }
            RfbEvent::Resize { width, height } => panic!("unexpected resize to {width}x{height}"),
            RfbEvent::UpdateEnd => String::new(),
            RfbEvent::Bell => "Bell ".to_owned(),
            RfbEvent::ClipboardText(_) => "ClipboardText ".to_owned(),
        };
        trace.last_mut().unwrap().push_str(&summary);
        if event == RfbEvent::UpdateEnd {
            if canvas.fully_painted() {
                break;
            }
            // libvncserver answers a non-incremental request from an ExtendedDesktopSize
            // client with a size-only update first while the requested region stays pending.
            // Another non-incremental request would restart that, so ask incrementally.
            assert!(
                trace.len() < 5,
                "screen not covered after updates {trace:?}"
            );
            trace.push(String::new());
            writer
                .request_update(true, 0, 0, info.width, info.height)
                .await
                .unwrap();
        }
    }
    println!("{label}: {:?} updates {trace:?}", started.elapsed());
    canvas
}

#[tokio::test]
#[ignore = "needs the x11vnc server from docker/compose.yml on 127.0.0.1:5900 (password `secret`)"]
async fn live_x11vnc_full_update() {
    const TILE: usize = 64;
    let raw = || Some(vec![RAW, CURSOR, DESKTOP_SIZE, LAST_RECT]);

    // Raw captures interleaved with the decoders under test locate the animated parts of the
    // desktop (clock, scrolling terminals).
    let raw_1 = capture_live("raw #1", raw()).await;
    let raw_2 = capture_live("raw #2", raw()).await;
    let zrle = capture_live("default (ZRLE)", None).await;
    let raw_3 = capture_live("raw #3", raw()).await;
    let hextile = capture_live(
        "hextile",
        Some(vec![HEXTILE, COPY_RECT, CURSOR, DESKTOP_SIZE, LAST_RECT]),
    )
    .await;
    let raw_4 = capture_live("raw #4", raw()).await;
    let raws = [&raw_1, &raw_2, &raw_3, &raw_4];

    let (width, height) = (raw_1.width, raw_1.height);
    assert_eq!((width, height), (1280, 800));
    let distinct: std::collections::HashSet<&[u8]> = raw_1.rgba.chunks_exact(4).collect();
    assert!(distinct.len() > 2, "screen looks blank");

    let pixel = |canvas: &Canvas, x: usize, y: usize| -> [u8; 4] {
        canvas.rgba[(y * width + x) * 4..][..4].try_into().unwrap()
    };
    let (columns, rows) = (width.div_ceil(TILE), height.div_ceil(TILE));
    let tile_pixels = |column: usize, row: usize| {
        let (x0, y0) = (column * TILE, row * TILE);
        (y0..(y0 + TILE).min(height))
            .flat_map(move |y| (x0..(x0 + TILE).min(width)).map(move |x| (x, y)))
    };
    // A tile is animated if any raw capture differs in it; neighbours are excluded as well.
    let changed: Vec<bool> = (0..rows * columns)
        .map(|i| {
            tile_pixels(i % columns, i / columns).any(|(x, y)| {
                raws.iter()
                    .any(|raw| pixel(raw, x, y) != pixel(&raw_1, x, y))
            })
        })
        .collect();
    let stable: Vec<(usize, usize)> = (0..rows)
        .flat_map(|row| (0..columns).map(move |column| (column, row)))
        .filter(|&(column, row)| {
            (row.saturating_sub(1)..(row + 2).min(rows)).all(|r| {
                (column.saturating_sub(1)..(column + 2).min(columns))
                    .all(|c| !changed[r * columns + c])
            })
        })
        .collect();
    println!(
        "{} of {} tiles are static and compared exactly; {} distinct colours on screen",
        stable.len(),
        rows * columns,
        distinct.len()
    );
    assert!(
        stable.len() * 2 > rows * columns,
        "the desktop changed too much to compare captures"
    );

    for (label, canvas) in [("ZRLE", &zrle), ("hextile", &hextile)] {
        let mismatches: Vec<(usize, usize)> = stable
            .iter()
            .flat_map(|&(column, row)| tile_pixels(column, row))
            .filter(|&(x, y)| pixel(canvas, x, y) != pixel(&raw_1, x, y))
            .collect();
        if let Some(&(x, y)) = mismatches.first() {
            panic!(
                "{label}: {} pixels in static tiles differ from raw, first at ({x}, {y}): \
                 {:?} vs {:?}",
                mismatches.len(),
                pixel(canvas, x, y),
                pixel(&raw_1, x, y)
            );
        }
        println!("{label}: identical to raw in all static tiles");
    }
}
