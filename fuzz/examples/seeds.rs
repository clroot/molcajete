//! Writes the seed corpora in `fuzz/seeds/<target>/`: small, valid inputs
//! that reach deep into each parser, from which the fuzzer mutates.
//!
//! `cargo run --manifest-path fuzz/Cargo.toml --example seeds`

use std::fs;
use std::path::Path;

use flate2::{Compress, Compression, FlushCompress};
use molcajete_protocol::binary::RecordWriter;
use molcajete_protocol::video::encode_frame;
use molcajete_protocol::{Instruction, zimage};

fn write(target: &str, name: &str, data: &[u8]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("seeds")
        .join(target);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), data).unwrap();
}

/// Prefixes `data` with the byte several targets take to pick limits and a
/// split.
fn seeded(seed: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![seed];
    out.extend_from_slice(data);
    out
}

/// Encodes instructions given as opcode and arguments.
fn encode(instructions: &[&[&str]]) -> Vec<u8> {
    let mut out = String::new();
    for instruction in instructions {
        let (opcode, args) = instruction.split_first().expect("an opcode");
        Instruction::new(*opcode, args.iter().copied()).encode_into(&mut out);
    }
    out.into_bytes()
}

/// What guacd and molcajeted send in a short session.
const SERVER: &[&[&str]] = &[
    &["args", "VERSION_1_5_0", "hostname", "port"],
    &["ready", "$a1b2c3d4-0000-4000-8000-000000000000"],
    &["size", "0", "1024", "768"],
    &["name", "localhost"],
    &["img", "1", "14", "0", "image/png", "0", "0"],
    &["blob", "1", "AP+AOw=="],
    &["end", "1"],
    &[
        "img",
        "2",
        "12",
        "-1",
        "image/x-molcajete-zstd-v1",
        "10",
        "20",
    ],
    &["end", "2"],
    &["rect", "0", "0", "0", "64", "32"],
    &["cfill", "14", "0", "255", "128", "0", "255"],
    &["copy", "0", "0", "0", "64", "32", "14", "0", "0", "40"],
    &["arc", "0", "50", "50", "10", "0.0", "6.28", "0"],
    &["close", "0"],
    &["cursor", "0", "0", "-1", "0", "0", "16", "16"],
    &["shade", "0", "128"],
    &["dispose", "3"],
    &["move", "3", "0", "10", "10", "0"],
    &["audio", "4", "audio/L16;rate=44100,channels=2"],
    &["video", "5", "0", "video/h264"],
    &["clipboard", "6", "text/plain"],
    &["blob", "6", "aGVsbG8="],
    &["end", "6"],
    &["display-control", "1"],
    &["link", "https://example.com/a?b"],
    &["selection", "0"],
    &["sync", "1700000000", "1", "apply-us"],
    &["nop"],
    &["error", "Failure", "519"],
    &["disconnect"],
];

/// What a browser sends.
const CLIENT: &[&[&str]] = &[
    &["select", "vnc"],
    &["size", "1920", "1080", "96"],
    &["audio", "audio/L16"],
    &["video", "video/h264"],
    &[
        "image",
        "image/png",
        "image/jpeg",
        "image/x-molcajete-zstd-v1",
    ],
    &["timezone", "Asia/Seoul"],
    &["name", "guest"],
    &["connect", "VERSION_1_5_0", "host", "5900", "password", "1"],
    &["mouse", "100", "200", "1"],
    &["key", "65307", "1"],
    &["key", "65307", "0"],
    &["size", "1280", "720"],
    &["sync", "1700000000", "1", "2500"],
    &["ack", "1", "OK", "0"],
    &["clipboard", "7", "text/plain"],
    &["blob", "7", "7ZWc6rWt7Ja0"],
    &["end", "7"],
    &["argv", "8", "text/plain", "password"],
    &["blob", "8", "c2VjcmV0"],
    &["end", "8"],
    &["file", "9", "text/plain", "a.txt"],
    &["blob", "10", "AAAA"],
    &["disconnect"],
];

fn instructions() {
    let server = encode(SERVER);
    let client = encode(CLIENT);
    write("parser", "server-strict", &seeded(0, &server));
    write("parser", "server-lenient", &seeded(1, &server));
    write("parser", "client-split", &seeded(6, &client));
    // Lengths count code points, not bytes.
    let utf8 = encode(&[&["name", "한국어!", "日本"], &["clipboard", "😀😀ab"]]);
    write("parser", "utf8", &seeded(3, &utf8));
    write("handshake", "session", &client);
    write(
        "handshake",
        "binary-gateway",
        &encode(&[
            &["select", "ssh"],
            &["connect", "VERSION_1_5_0", "host", "22", "1"],
            &["size", "800", "600"],
            &["disconnect"],
        ]),
    );
    write(
        "handshake",
        "pre-1.1",
        &encode(&[
            &["select", "rdp"],
            &["size", "1024", "768"],
            &["connect", "host"],
            &["key", "97", "1"],
        ]),
    );
}

fn records() {
    let mut writer = RecordWriter::new();
    writer.instructions(b"3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;");
    writer.blob(1, &(0..=255).collect::<Vec<u8>>());
    writer.instructions(b"3.end,1.1;4.sync,1.1,1.1;");
    writer.blob(-1, &[]);
    let mut data = writer.take();
    // A record of a kind readers skip.
    data.extend_from_slice(&[7, 3, 0, 0, 0, b'a', b'b', b'c']);
    write("records", "session", &seeded(0, &data));
    write("records", "session-split", &seeded(9, &data));
}

fn zstd_image(kind: &str, width: u32, height: u32, palette: Option<&[u8]>, raw: &[u8]) {
    let mut data = Vec::new();
    zimage::write_header(&mut data, width, height, palette);
    data.extend_from_slice(&zstd::bulk::compress(raw, 3).unwrap());
    write("zimage", kind, &data);
}

fn zimages() {
    let (width, height) = (13u32, 7u32);
    for colors in [2usize, 4, 16, 200] {
        let palette: Vec<u8> = (0..colors * 3).map(|i| (i * 37) as u8).collect();
        let bits = zimage::index_bits(colors) as usize;
        let row_bytes = (width as usize * bits).div_ceil(8);
        let raw: Vec<u8> = (0..row_bytes * height as usize)
            .map(|i| (i * 11) as u8)
            .collect();
        zstd_image(
            &format!("indexed-{colors}"),
            width,
            height,
            Some(&palette),
            &raw,
        );
    }
    let rgb: Vec<u8> = (0..width * height * 3).map(|i| (i * 7) as u8).collect();
    zstd_image("rgb", width, height, None, &rgb);
}

fn video() {
    let mut stream = Vec::new();
    encode_frame(&mut stream, 0, true, b"\x00\x00\x00\x01\x67\x42key-frame");
    encode_frame(&mut stream, 33, false, b"\x00\x00\x00\x01\x41delta");
    encode_frame(&mut stream, 66, false, b"");
    write("video_frames", "stream", &seeded(0, &stream));
    write("video_frames", "stream-split", &seeded(5, &stream));
}

fn u16be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn u32be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn rect(out: &mut Vec<u8>, x: u16, y: u16, w: u16, h: u16, encoding: i32) {
    for value in [x, y, w, h] {
        u16be(out, value);
    }
    out.extend_from_slice(&encoding.to_be_bytes());
}

/// ZRLE tiles of a 70x10 rectangle (two tiles): a packed palette tile and a
/// palette RLE tile.
fn zrle_tiles() -> Vec<u8> {
    let mut tiles = vec![2]; // packed palette of 2 colours, 1 bit per pixel
    tiles.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    tiles.extend((0..10 * 8).map(|i: u32| (i * 37) as u8));
    tiles.push(128 + 3); // palette RLE of 3 colours
    tiles.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    tiles.extend_from_slice(&[0x80, 10, 1, 0x82, 47]);
    tiles
}

/// Server messages after the handshake: one update with a rectangle in
/// every encoding and the pseudo-encodings, then a bell and clipboard text.
fn rfb_messages() -> Vec<u8> {
    let mut out = vec![0, 0];
    u16be(&mut out, 10);
    // Raw 2x2
    rect(&mut out, 0, 0, 2, 2, 0);
    out.extend((0..16).map(|i: u32| (i * 16) as u8));
    // CopyRect
    rect(&mut out, 10, 10, 4, 4, 1);
    u16be(&mut out, 0);
    u16be(&mut out, 0);
    // RRE with two subrectangles
    rect(&mut out, 0, 0, 8, 8, 2);
    u32be(&mut out, 2);
    out.extend_from_slice(&[0, 0, 255, 0]);
    for sub in [[1u16, 1, 3, 3], [4, 4, 10, 10]] {
        out.extend_from_slice(&[255, 0, 0, 0]);
        for value in sub {
            u16be(&mut out, value);
        }
    }
    // Hextile: one tile with background, foreground and subrectangles,
    // then a raw tile.
    rect(&mut out, 0, 0, 20, 4, 5);
    out.extend_from_slice(&[2 | 4 | 8, 1, 2, 3, 0, 4, 5, 6, 0, 1, 0x11]);
    out.push(1);
    out.extend((0..4 * 4 * 4).map(|i| i as u8));
    // ZRLE, through a zlib stream
    let tiles = zrle_tiles();
    let mut compressed = Vec::with_capacity(tiles.len() + 64);
    Compress::new(Compression::default(), true)
        .compress_vec(&tiles, &mut compressed, FlushCompress::Sync)
        .unwrap();
    rect(&mut out, 0, 0, 70, 10, 16);
    u32be(&mut out, compressed.len() as u32);
    out.extend_from_slice(&compressed);
    // Cursor 3x2 with its mask
    rect(&mut out, 1, 1, 3, 2, -239);
    out.extend((0..24).map(|i: u32| (i * 10) as u8));
    out.extend_from_slice(&[0b1010_0000, 0b0100_0000]);
    // ExtendedDesktopSize with one screen, DesktopSize, LastRect
    rect(&mut out, 0, 0, 800, 600, -308);
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 3, 32, 2, 88, 0, 0, 0, 0]);
    rect(&mut out, 0, 0, 640, 480, -223);
    rect(&mut out, 0, 0, 0, 0, -224);
    // SetColourMapEntries, Bell, ServerCutText
    out.extend_from_slice(&[1, 0, 0, 0, 0, 1, 1, 2, 3, 4, 5, 6]);
    out.push(2);
    out.extend_from_slice(&[3, 0, 0, 0]);
    u32be(&mut out, 5);
    out.extend_from_slice(b"caf\xe9!");
    out
}

/// ServerInit for a 100x80 framebuffer.
fn server_init(out: &mut Vec<u8>) {
    u16be(out, 100);
    u16be(out, 80);
    out.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    u32be(out, 4);
    out.extend_from_slice(b"desk");
}

fn rfb() {
    // Mode 0, RFB 3.8 with VNC authentication.
    let mut data = vec![0];
    data.extend_from_slice(b"RFB 003.008\n");
    data.extend_from_slice(&[2, 1, 2]);
    data.extend(0..16u8);
    u32be(&mut data, 0);
    server_init(&mut data);
    data.extend(rfb_messages());
    write("rfb", "rfb38-vnc-auth", &data);

    // Mode 0, RFB 3.3 without authentication.
    let mut data = vec![0];
    data.extend_from_slice(b"RFB 003.003\n");
    u32be(&mut data, 1);
    server_init(&mut data);
    data.extend(rfb_messages());
    write("rfb", "rfb33-none", &data);

    // Mode 1: messages on a 100x80 framebuffer.
    let mut data = vec![1];
    u16be(&mut data, 100);
    u16be(&mut data, 80);
    data.extend(rfb_messages());
    write("rfb", "messages", &data);

    // Mode 2: ZRLE tiles.
    let mut data = vec![2];
    u16be(&mut data, 70);
    u16be(&mut data, 10);
    data.extend(zrle_tiles());
    write("rfb", "zrle-tiles", &data);
}

fn terminal() {
    let sessions: [(&str, &[u8]); 6] = [
        (
            "shell",
            b"\x1b]0;user@host: ~\x07\x1b[?2004huser@host:~$ ls --color\r\n\
              \x1b[0m\x1b[01;34mdir\x1b[0m  \x1b[01;32mrun.sh\x1b[0m  file.txt\r\n\
              user@host:~$ \x1b[?2004l",
        ),
        (
            "colors",
            b"\x1b[1;3;4;9m bold \x1b[38;5;196mindexed\x1b[48;2;10;20;30m true \
              \x1b[7mreverse\x1b[0m\x1b[2J\x1b[H\x1b[5;10Hmoved\x1b[K\x1b[2;4r\
              \x1b[S\x1b[T\x1b[3L\x1b[2M\x1b[4@\x1b[3P\x1b[1X\x1bM\x1bD\x1b7\x1b8",
        ),
        (
            "fullscreen",
            b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?25l\x1b[22;0;0t\
              \x1b(0lqqk\r\nx  x\r\nmqqj\x1b(B\xff\x1b[?2026h\x1b[1;1Hsync\x1b[?2026l\
              \xff\x1b[?1049l\x1b[?25h\x1b[23;0;0t",
        ),
        (
            "unicode",
            "한국어 日本語 かな 中文 ┌─┐│└┘ ▀▄█ 😀 e\u{301} \u{200b}x\r\n\
             ab\tc\x08\x08d\r\n"
                .as_bytes(),
        ),
        (
            "links",
            b"see https://example.com/path_(x)?q=1, or mailto:a@b.c.\r\n\
              \x1b]8;id=1;https://example.org/\x1b\\link text\x1b]8;;\x1b\\\r\n\
              \x1b]52;c;aGVsbG8=\x07\x1b]4;1;?\x07\x1b]10;?\x1b\\\x1b[c\x1b[6n\x1b[18t",
        ),
        (
            "wrap",
            b"0123456789012345678901234567890123456789012345678901234567890123456789\
              0123456789\x1b[?7l0123456789012345678901234567890123456789\x1b[?7h",
        ),
    ];
    for (name, output) in sessions {
        let mut data = vec![40, 12];
        data.extend_from_slice(output);
        write("terminal", name, &data);
    }
}

fn main() {
    instructions();
    records();
    zimages();
    video();
    rfb();
    terminal();
}
