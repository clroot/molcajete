use std::collections::HashMap;

use base64::Engine;
use molcajete_protocol::{parse_all, zimage};

use super::*;

/// A minimal Guacamole client for layer 0: replays emitted instructions so
/// tests can check that what the client shows equals the framebuffer.
pub(crate) struct Client {
    width: u32,
    height: u32,
    pub(crate) pixels: Vec<u8>,
    rect: Option<Rect>,
    streams: HashMap<i32, (String, i32, i32, i32, Vec<u8>)>,
    /// The client's own decoder of zstd images.
    zstd: zimage::Decoder,
    zstd_images: usize,
}

impl Client {
    pub(crate) fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            rect: None,
            streams: HashMap::new(),
            zstd: zimage::Decoder::default(),
            zstd_images: 0,
        }
    }

    fn index(&self, x: u32, y: u32) -> usize {
        ((y * self.width + x) * 4) as usize
    }

    pub(crate) fn apply(&mut self, text: &str) {
        let int = |s: &str| s.parse::<i64>().expect("integer argument");
        for ins in parse_all(text).expect("valid protocol") {
            let a = |i: usize| int(ins.arg(i));
            match ins.opcode.as_str() {
                "size" if a(0) == 0 => {
                    let (w, h) = (a(1) as u32, a(2) as u32);
                    let mut px = vec![0u8; (w * h * 4) as usize];
                    for y in 0..h.min(self.height) {
                        let n = (w.min(self.width) * 4) as usize;
                        let s = self.index(0, y);
                        let d = ((y * w) * 4) as usize;
                        px[d..d + n].copy_from_slice(&self.pixels[s..s + n]);
                    }
                    self.width = w;
                    self.height = h;
                    self.pixels = px;
                }
                "rect" if a(0) == 0 => {
                    self.rect = Some(Rect::new(
                        a(1) as u32,
                        a(2) as u32,
                        a(3) as u32,
                        a(4) as u32,
                    ));
                }
                "cfill" if a(1) == 0 => {
                    let r = self.rect.take().expect("rect before cfill");
                    let color = [a(2) as u8, a(3) as u8, a(4) as u8, a(5) as u8];
                    match a(0) as u8 {
                        MODE_OVER => {
                            assert_eq!(color[3], 255, "translucent fill");
                            fill_rect(&mut self.pixels, self.width, &r, color);
                        }
                        MODE_ROUT => fill_rect(&mut self.pixels, self.width, &r, [0, 0, 0, 0]),
                        other => panic!("unexpected composite mode {other:#x}"),
                    }
                }
                "copy" if a(0) == 0 && a(6) == 0 => {
                    assert_eq!(a(5) as u8, MODE_OVER);
                    let src = Rect::new(a(1) as u32, a(2) as u32, a(3) as u32, a(4) as u32);
                    let snapshot: Vec<Vec<u8>> = (src.y..src.y + src.h)
                        .map(|y| {
                            let s = self.index(src.x, y);
                            self.pixels[s..s + (src.w * 4) as usize].to_vec()
                        })
                        .collect();
                    for (r, row) in snapshot.iter().enumerate() {
                        for (c, p) in row.chunks_exact(4).enumerate() {
                            if p[3] == 255 {
                                let d = self.index(a(7) as u32 + c as u32, a(8) as u32 + r as u32);
                                self.pixels[d..d + 4].copy_from_slice(p);
                            }
                        }
                    }
                }
                "img" => {
                    let entry = (
                        ins.arg(3).to_owned(),
                        a(2) as i32,
                        a(4) as i32,
                        a(5) as i32,
                        Vec::new(),
                    );
                    self.streams.insert(a(0) as i32, entry);
                }
                "blob" => {
                    let data = &mut self.streams.get_mut(&(a(0) as i32)).expect("open stream").4;
                    base64::engine::general_purpose::STANDARD
                        .decode_vec(ins.arg(1).as_bytes(), data)
                        .expect("base64");
                }
                "end" => {
                    let (mime, layer, x, y, data) =
                        self.streams.remove(&(a(0) as i32)).expect("open stream");
                    if layer != 0 {
                        continue;
                    }
                    let (w, h, rgba) = match mime.as_str() {
                        "image/png" => decode_png(&data),
                        zimage::MIMETYPE => {
                            let mut rgba = Vec::new();
                            let (w, h) = self.zstd.decode(&data, &mut rgba).expect("zstd image");
                            self.zstd_images += 1;
                            (w, h, rgba)
                        }
                        other => panic!("{other}: tests run lossless"),
                    };
                    for r in 0..h {
                        for c in 0..w {
                            let s = ((r * w + c) * 4) as usize;
                            if rgba[s + 3] == 255 {
                                let d = self.index(x as u32 + c, y as u32 + r);
                                self.pixels[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                            } else {
                                assert_eq!(rgba[s + 3], 0, "partial alpha not simulated");
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        other => panic!("unexpected {other:?}"),
    };
    (info.width, info.height, rgba)
}

/// Deterministic xorshift generator (no external crates).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % n.max(1) as u64) as u32
    }
}

/// A row of "text": a few colours in a pattern seeded by `seed`.
fn text_row(width: u32, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    (0..width)
        .flat_map(|_| {
            let v = [20u8, 200, 90][rng.below(3) as usize];
            [v, 255 - v, v / 2, 255]
        })
        .collect()
}

/// A block of screen content with more than 256 colours: runs of one
/// colour, a new colour for every run.
fn window_block(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut block = Vec::with_capacity((width * height * 4) as usize);
    let mut color = [0u8; 4];
    for i in 0..width * height {
        if i % 8 == 0 {
            let v = rng.next();
            color = [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255];
        }
        block.extend_from_slice(&color);
    }
    block
}

/// A block in which every pixel has another colour, like a photograph.
fn photo_block(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    (0..width * height)
        .flat_map(|_| {
            let v = rng.next();
            [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
        })
        .collect()
}

fn setup(w: u32, h: u32) -> (Display, Client, Output) {
    setup_for(w, h, false)
}

/// A display and its client, which accepts zstd images or PNG only.
fn setup_for(w: u32, h: u32, zstd: bool) -> (Display, Client, Output) {
    let mut d = Display::new(ImageFormats { jpeg: false, zstd }, true);
    let mut out = Output::text();
    d.resize(w, h, &mut out);
    let mut c = Client::new();
    c.apply(&out);
    (d, c, Output::text())
}

fn flush_into(d: &mut Display, c: &mut Client) -> (FlushStats, String) {
    let mut out = Output::text();
    let stats = d.flush(&mut out);
    c.apply(&out);
    assert!(c.pixels == d.pixels, "client diverged from the framebuffer");
    (stats, out.to_string())
}

#[test]
fn unchanged_updates_send_nothing() {
    let (mut d, mut c, _) = setup(128, 64);
    let row = text_row(128, 7);
    let block: Vec<u8> = row.iter().copied().cycle().take(128 * 64 * 4).collect();
    d.put(0, 0, 128, 64, &block);
    let (stats, _) = flush_into(&mut d, &mut c);
    assert_eq!(stats.images, 1);
    // A server resending the same pixels costs nothing.
    d.put(0, 0, 128, 64, &block);
    let (stats, out) = flush_into(&mut d, &mut c);
    assert_eq!((stats.images, stats.fills, stats.copies), (0, 0, 0));
    assert!(out.is_empty());
}

#[test]
fn changes_are_trimmed_to_the_changed_pixels() {
    let (mut d, mut c, _) = setup(256, 128);
    d.fill(0, 0, 256, 128, [255, 255, 255, 255]);
    flush_into(&mut d, &mut c);
    // The server sends a large rectangle in which only a small part changed.
    let mut block = vec![255u8; 256 * 128 * 4];
    for y in 40..48 {
        for x in 100..120 {
            let i = (y * 256 + x) * 4;
            block[i..i + 3].copy_from_slice(&[0, 0, 0]);
        }
    }
    d.put(0, 0, 256, 128, &block);
    let (stats, _) = flush_into(&mut d, &mut c);
    assert_eq!(stats.pixels, 20 * 8);
}

#[test]
fn lines_of_text_over_the_same_columns_share_an_image() {
    let (mut d, mut c, _) = setup(512, 256);
    d.fill(0, 0, 512, 256, [255, 255, 255, 255]);
    flush_into(&mut d, &mut c);
    // Four lines, 20 rows of text and 8 rows of leading each.
    let leading = 8;
    assert!(leading > ROW_GAP_MERGE);
    for line in 0..4u32 {
        let text: Vec<u8> = (0..20)
            .flat_map(|row| text_row(400, (line * 20 + row + 1) as u64))
            .collect();
        d.put(40, 20 + line * (20 + leading), 400, 20, &text);
    }
    let (stats, _) = flush_into(&mut d, &mut c);
    assert_eq!(stats.images, 1);
    // The image is as tall as the lines and no wider than them.
    assert!(stats.pixels <= 400 * (3 * 28 + 20));
}

#[test]
fn changes_beside_unchanged_text_stay_apart() {
    let (mut d, mut c, _) = setup(512, 256);
    let page: Vec<u8> = (0..256).flat_map(|row| text_row(512, row + 1)).collect();
    d.put(0, 0, 512, 256, &page);
    flush_into(&mut d, &mut c);
    // Two neighbouring lines change at different columns: one image for
    // both would resend the unchanged text between them.
    for (x, y) in [(0, 20), (300, 48)] {
        let text: Vec<u8> = (0..20)
            .flat_map(|row| text_row(100, 1000 + (y + row) as u64))
            .collect();
        d.put(x, y, 100, 20, &text);
    }
    let (stats, _) = flush_into(&mut d, &mut c);
    assert_eq!(stats.images, 2);
    assert!(stats.pixels <= 2 * 100 * 20);
}

#[test]
fn joined_images_stay_below_the_size_limit() {
    let (mut d, mut c, _) = setup(2048, 1024);
    d.fill(0, 0, 2048, 1024, [255, 255, 255, 255]);
    flush_into(&mut d, &mut c);
    // A page of text twice the size of the largest joined image.
    for line in 0..36u32 {
        let text: Vec<u8> = (0..20)
            .flat_map(|row| text_row(2048, (line * 20 + row + 1) as u64))
            .collect();
        d.put(0, line * 28, 2048, 20, &text);
    }
    let all = Rect::new(0, 0, 2048, 1024);
    let rects = d.changed_rects(&all, &d.region_diff(&all));
    assert!(
        rects.len() > 1 && rects.len() < 36,
        "{} images",
        rects.len()
    );
    assert!(rects.iter().all(|r| r.area() <= MAX_JOINED_PIXELS));
}

#[test]
fn scrolling_becomes_a_copy() {
    let (w, h) = (320u32, 240u32);
    let (mut d, mut c, _) = setup(w, h);
    let rows: Vec<Vec<u8>> = (0..h + 40)
        .map(|y| text_row(w, y as u64 * 31 + 5))
        .collect();
    let frame = |offset: u32| -> Vec<u8> {
        (0..h)
            .flat_map(|y| rows[(y + offset) as usize].clone())
            .collect()
    };
    d.put(0, 0, w, h, &frame(0));
    flush_into(&mut d, &mut c);

    // Content scrolls up by 16 rows; the server resends the whole area.
    d.put(0, 0, w, h, &frame(16));
    let (stats, out) = flush_into(&mut d, &mut c);
    assert!(
        stats.copies >= 1,
        "no copy in {}",
        &out[..out.len().min(200)]
    );
    assert_eq!(stats.pixels, (16 * w) as u64, "only the new rows are sent");
}

#[test]
fn scrolling_down_is_detected_too() {
    let (w, h) = (200u32, 160u32);
    let (mut d, mut c, _) = setup(w, h);
    let rows: Vec<Vec<u8>> = (0..h + 20)
        .map(|y| text_row(w, y as u64 * 13 + 1))
        .collect();
    let frame = |offset: u32| -> Vec<u8> {
        (0..h)
            .flat_map(|y| rows[(y + offset) as usize].clone())
            .collect()
    };
    d.put(0, 0, w, h, &frame(20));
    flush_into(&mut d, &mut c);
    d.put(0, 0, w, h, &frame(0));
    let (stats, _) = flush_into(&mut d, &mut c);
    assert!(stats.copies >= 1);
    assert_eq!(stats.pixels, (20 * w) as u64);
}

#[test]
fn moved_windows_become_copies() {
    let (w, h) = (512u32, 384u32);
    let (mut d, mut c, _) = setup(w, h);
    let background = [30u8, 60, 90, 255];
    let window: Vec<u8> = (0..200)
        .flat_map(|r| text_row(260, r as u64 * 7 + 3))
        .collect();
    let frame = |wx: u32, wy: u32| -> Vec<u8> {
        let mut px: Vec<u8> = background.repeat((w * h) as usize);
        for r in 0..200u32 {
            let d = (((wy + r) * w + wx) * 4) as usize;
            px[d..d + 260 * 4]
                .copy_from_slice(&window[(r * 260 * 4) as usize..((r + 1) * 260 * 4) as usize]);
        }
        px
    };
    d.put(0, 0, w, h, &frame(40, 30));
    flush_into(&mut d, &mut c);
    // The window is dragged by (+37, +23); the server repaints everything.
    d.put(0, 0, w, h, &frame(77, 53));
    let (stats, _) = flush_into(&mut d, &mut c);
    assert!(stats.copies >= 1, "copies: {}", stats.copies);
    // Most of the window arrives by copy instead of pixels.
    assert!(
        stats.pixels < (260 * 200) as u64 / 2,
        "pixels: {}",
        stats.pixels
    );
}

#[test]
fn protocol_copy_with_pending_changes_stays_consistent() {
    let (mut d, mut c, _) = setup(128, 128);
    d.fill(0, 0, 128, 128, [10, 10, 10, 255]);
    flush_into(&mut d, &mut c);
    // Change the source but do not flush before the copy.
    d.put(0, 0, 32, 32, &text_row(32, 3).repeat(32));
    let mut out = Output::text();
    d.copy(0, 0, 64, 64, 32, 32, &mut out);
    c.apply(&out);
    flush_into(&mut d, &mut c);
}

#[test]
fn solid_updates_become_fills() {
    let (mut d, mut c, _) = setup(100, 100);
    d.fill(10, 10, 20, 20, [1, 2, 3, 255]);
    let (stats, out) = flush_into(&mut d, &mut c);
    assert_eq!(stats.fills, 1);
    let ins = parse_all(&out).unwrap();
    assert_eq!(ins[0].args, ["0", "10", "10", "20", "20"]);
    assert_eq!(ins[1].args, ["14", "0", "1", "2", "3", "255"]);
}

#[test]
fn images_are_chunked_into_blobs() {
    let (mut d, mut c, _) = setup(300, 300);
    let mut rng = Rng(99);
    let noise: Vec<u8> = (0..300 * 300)
        .flat_map(|_| {
            let v = rng.next();
            [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
        })
        .collect();
    d.put(0, 0, 300, 300, &noise);
    let (stats, out) = flush_into(&mut d, &mut c);
    assert_eq!(stats.images, 1);
    let ins = parse_all(&out).unwrap();
    assert!(ins.iter().filter(|i| i.opcode == "blob").count() > 1);
    assert!(ins.iter().all(|i| i.encode().len() < 8192 + 64));
}

/// The protocol text a binary output stands for: its instructions, with
/// every blob record written as a `blob` instruction.
fn records_as_text(out: &Output) -> String {
    use molcajete_protocol::binary::{Record, records};

    let b64 = base64::engine::general_purpose::STANDARD;
    let mut text = String::new();
    for record in records(out.as_bytes()) {
        match record.expect("valid records") {
            Record::Instructions(part) => text += std::str::from_utf8(part).expect("UTF-8"),
            Record::Blob { stream, data } => {
                text += &InstructionBuilder::new("blob")
                    .arg(stream)
                    .str_arg(&b64.encode(data))
                    .finish();
            }
            Record::Unknown { tag, .. } => panic!("unknown record {tag}"),
        }
    }
    text
}

#[test]
fn binary_output_carries_the_same_frame_as_raw_bytes() {
    let mut rng = Rng(99);
    let noise: Vec<u8> = (0..300 * 300)
        .flat_map(|_| {
            let v = rng.next();
            [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
        })
        .collect();
    // The same screen, flushed once as text and once as records.
    let frame = |mut out: Output| {
        let mut d = Display::new(
            ImageFormats {
                jpeg: false,
                zstd: false,
            },
            true,
        );
        d.resize(300, 300, &mut out);
        d.put(0, 0, 300, 300, &noise);
        let stats = d.flush(&mut out);
        d.sync(&mut out);
        (d, stats, out)
    };
    let (_, text_stats, text) = frame(Output::text());
    let (d, stats, binary) = frame(Output::binary());
    assert!(binary.is_binary() && !text.is_binary());
    assert_eq!(stats.bytes, text_stats.bytes);

    // What a client draws from the records is the framebuffer.
    let mut c = Client::new();
    c.apply(&records_as_text(&binary));
    assert!(c.pixels == d.pixels, "client diverged from the framebuffer");

    // Same instructions and the same image data as in text mode, but the
    // data in one blob (it fits a record) instead of 6048-byte pieces, and
    // without base64: about a quarter fewer bytes.
    let split = |text: &str| {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut other = Vec::new();
        let mut data = Vec::new();
        let mut blobs = 0;
        for ins in parse_all(text).unwrap() {
            if ins.opcode == "blob" {
                data.extend(b64.decode(ins.arg(1)).unwrap());
                blobs += 1;
            } else if ins.opcode != "sync" {
                other.push(ins);
            }
        }
        (other, data, blobs)
    };
    let (text_ins, text_data, text_blobs) = split(&text);
    let (binary_ins, binary_data, binary_blobs) = split(&records_as_text(&binary));
    assert_eq!(binary_ins, text_ins);
    assert_eq!(binary_data, text_data);
    assert_eq!(binary_data.len(), stats.bytes);
    assert!(text_blobs > 1);
    assert_eq!(binary_blobs, binary_data.len().div_ceil(64 * 1024 - 9));
    assert!(
        binary.len() * 100 < text.len() * 76,
        "binary is 25% smaller"
    );
}

#[test]
fn busy_areas_go_lossy_then_get_refined() {
    let mut d = Display::new(
        ImageFormats {
            jpeg: true,
            zstd: false,
        },
        false,
    );
    let mut out = Output::text();
    d.resize(256, 256, &mut out);
    let mut rng = Rng(5);
    let mut mime = String::new();
    for _ in 0..6 {
        let noise: Vec<u8> = (0..256 * 256)
            .flat_map(|_| {
                let v = rng.next();
                [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
            })
            .collect();
        d.put(0, 0, 256, 256, &noise);
        out.clear();
        d.flush(&mut out);
        mime = parse_all(&out)
            .unwrap()
            .into_iter()
            .find(|i| i.opcode == "img")
            .map(|i| i.arg(3).to_owned())
            .unwrap_or_default();
    }
    assert_eq!(mime, "image/jpeg");
    // The area calms down: the next flush re-sends it losslessly.
    d.heat.iter_mut().for_each(|h| *h = 0.0);
    out.clear();
    let stats = d.flush(&mut out);
    assert_eq!(stats.images, 1, "adjacent tiles share an image");
    assert!(
        parse_all(&out)
            .unwrap()
            .iter()
            .filter(|i| i.opcode == "img")
            .all(|i| i.arg(3) == "image/png")
    );
    assert!(d.lossy.iter().all(|l| !l));
}

/// A display whose whole screen counts as sent lossy, and its client.
/// (The screen is a pattern of a few colours, which is quick to encode.)
fn lossy_display(w: u32, h: u32) -> (Display, Client) {
    let mut d = Display::new(
        ImageFormats {
            jpeg: true,
            zstd: false,
        },
        false,
    );
    let mut out = Output::text();
    d.resize(w, h, &mut out);
    let mut c = Client::new();
    c.apply(&out);
    let pattern: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [(x / 7 % 5 * 50) as u8, (y / 5 % 4 * 60) as u8, 90, 255]
        })
        .collect();
    d.put(0, 0, w, h, &pattern);
    flush_into(&mut d, &mut c);
    d.lossy.iter_mut().for_each(|l| *l = true);
    (d, c)
}

#[test]
fn a_large_cool_area_is_refined_in_blocks() {
    // 10 x 9 tiles, the last column and row cut by the edge.
    let (mut d, mut c) = lossy_display(600, 530);
    d.heat.iter_mut().for_each(|h| *h = 0.0);
    let (stats, _) = flush_into(&mut d, &mut c);
    // Blocks of 8 x 8 tiles: 2 x 2 of them.
    assert_eq!(stats.images, 4);
    assert_eq!(stats.pixels, 600 * 530);
    assert!(d.lossy.iter().all(|l| !l));
}

#[test]
fn only_the_cool_part_is_refined() {
    let (mut d, mut c) = lossy_display(512, 256);
    // The left half calms down, the right half is still busy.
    let cols = d.tile_cols;
    for (i, heat) in d.heat.iter_mut().enumerate() {
        *heat = if (i as u32 % cols) < 4 { 0.0 } else { HOT };
    }
    let (stats, _) = flush_into(&mut d, &mut c);
    assert_eq!(stats.images, 1);
    assert_eq!(stats.pixels, 256 * 256);
    for (i, &lossy) in d.lossy.iter().enumerate() {
        assert_eq!(lossy, (i as u32 % cols) >= 4, "tile {i}");
    }
}

#[test]
fn a_screen_that_cools_at_once_is_refined_over_several_flushes() {
    let (w, h) = (2560, 2048);
    let (mut d, mut c) = lossy_display(w, h);
    d.heat.iter_mut().for_each(|h| *h = 0.0);
    let mut flushes = 0;
    let mut pixels = 0;
    while d.lossy.iter().any(|&l| l) {
        // Nothing else is due: the refinement asks for the next flush.
        assert!(d.idle_deadline().is_some());
        let (stats, _) = flush_into(&mut d, &mut c);
        assert!(stats.pixels > 0);
        assert!(stats.pixels <= REFINE_PIXELS_PER_FLUSH + (TILE * TILE) as u64);
        pixels += stats.pixels;
        flushes += 1;
    }
    assert_eq!(pixels, (w * h) as u64, "every pixel refined once");
    assert_eq!(flushes, 2);
    assert_eq!(d.idle_deadline(), None);
}

#[test]
fn sync_and_ack_track_lag() {
    let (mut d, _, mut out) = setup(4, 4);
    d.sync(&mut out);
    d.sync(&mut out);
    assert_eq!(d.unacked_frames(), 2);
    let last = d.last_sync.unwrap();
    d.ack(last - 1, Instant::now());
    assert_eq!(d.unacked_frames(), 1);
    d.ack(last, Instant::now());
    assert_eq!(d.unacked_frames(), 0);
    assert!(d.next_frame_at() > Instant::now());
}

#[test]
fn time_an_ack_waited_unread_is_not_lag() {
    let (mut d, _, mut out) = setup(4, 4);
    d.sync(&mut out);
    let received = Instant::now();
    // The session was busy (building the next frame) before it read the ack.
    std::thread::sleep(Duration::from_millis(40));
    d.ack(d.last_sync.unwrap(), received);
    assert!(d.processing_lag() < Duration::from_millis(5));
}

#[test]
fn time_before_the_session_reads_input_is_not_ack_wait() {
    let (mut d, _, mut out) = setup(4, 4);
    d.sync(&mut out);
    let received = Instant::now();
    // The session connects to the remote host and reads no input meanwhile.
    std::thread::sleep(Duration::from_millis(40));
    d.reading_input();
    d.ack(d.last_sync.unwrap(), received);
    assert!(d.metrics.ack_wait_max() < Duration::from_millis(20));

    // From then on, a reply that waits for the session counts in full.
    d.sync(&mut out);
    let received = Instant::now();
    std::thread::sleep(Duration::from_millis(40));
    d.ack(d.last_sync.unwrap(), received);
    assert!(d.metrics.ack_wait_max() >= Duration::from_millis(40));
}

/// Random mixes of puts, fills, protocol copies, scrolls and resizes: after
/// every flush the simulated client must equal the framebuffer exactly.
#[test]
fn fuzz_client_always_matches_framebuffer() {
    let mut zstd_images = 0;
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (mut w, mut h) = (64 + rng.below(200), 64 + rng.below(160));
        // Every other client accepts zstd images.
        let zstd = seed % 2 == 0;
        let (mut d, mut c, _) = setup_for(w, h, zstd);
        for step in 0..60 {
            match rng.below(13) {
                11 | 12 => {
                    // Many colours: a window (zstd image, RGB) or a
                    // photograph (PNG for every client).
                    let (x, y) = (rng.below(w), rng.below(h));
                    let (rw, rh) = (1 + rng.below(w - x), 1 + rng.below(h - y));
                    let block = if rng.below(2) == 0 {
                        window_block(rw, rh, rng.next())
                    } else {
                        photo_block(rw, rh, rng.next())
                    };
                    d.put(x, y, rw, rh, &block);
                }
                0..=3 => {
                    let (x, y) = (rng.below(w), rng.below(h));
                    let (rw, rh) = (1 + rng.below(w - x), 1 + rng.below(h - y));
                    let seed = rng.next();
                    let block: Vec<u8> = (0..rh)
                        .flat_map(|r| text_row(rw, seed.wrapping_add(r as u64)))
                        .collect();
                    d.put(x, y, rw, rh, &block);
                }
                4 => {
                    let (x, y) = (rng.below(w), rng.below(h));
                    let shade = rng.below(256) as u8;
                    d.fill(
                        x,
                        y,
                        1 + rng.below(w - x),
                        1 + rng.below(h - y),
                        [shade, shade, 255 - shade, 255],
                    );
                }
                5 => {
                    let (rw, rh) = (1 + rng.below(w / 2), 1 + rng.below(h / 2));
                    let (sx, sy) = (rng.below(w - rw), rng.below(h - rh));
                    let (dx, dy) = (rng.below(w - rw), rng.below(h - rh));
                    let mut out = Output::text();
                    d.copy(sx, sy, dx, dy, rw, rh, &mut out);
                    c.apply(&out);
                }
                6 => {
                    // The remote side scrolls a band and repaints it.
                    let shift = 1 + rng.below(h / 3);
                    let mut band = d.pixels.clone();
                    let stride = (w * 4) as usize;
                    band.copy_within(shift as usize * stride.., 0);
                    let seed = rng.next();
                    for y in h - shift..h {
                        let row = text_row(w, seed.wrapping_add(y as u64));
                        band[y as usize * stride..(y as usize + 1) * stride].copy_from_slice(&row);
                    }
                    d.put(0, 0, w, h, &band);
                }
                8 => {
                    // A window is dragged and repainted without CopyRect.
                    let (rw, rh) = (8 + rng.below(w / 2), 8 + rng.below(h / 2));
                    let (rw, rh) = (rw.min(w), rh.min(h));
                    let (sx, sy) = (rng.below(w - rw + 1), rng.below(h - rh + 1));
                    let (dx, dy) = (rng.below(w - rw + 1), rng.below(h - rh + 1));
                    let block: Vec<u8> = (sy..sy + rh)
                        .flat_map(|y| {
                            let s = ((y * w + sx) * 4) as usize;
                            d.pixels[s..s + (rw * 4) as usize].to_vec()
                        })
                        .collect();
                    d.put(dx, dy, rw, rh, &block);
                }
                7 if step % 20 == 7 => {
                    w = 64 + rng.below(200);
                    h = 64 + rng.below(160);
                    let mut out = Output::text();
                    d.resize(w, h, &mut out);
                    c.apply(&out);
                }
                _ => {
                    flush_into(&mut d, &mut c);
                }
            }
        }
        flush_into(&mut d, &mut c);
        assert!(zstd || c.zstd_images == 0, "zstd image for a PNG client");
        zstd_images += c.zstd_images;
    }
    assert!(zstd_images > 50, "{zstd_images} zstd images");
}

/// The mimetypes of the images in `out`, in order.
fn image_types(out: &str) -> Vec<String> {
    parse_all(out)
        .expect("valid protocol")
        .iter()
        .filter(|i| i.opcode == "img")
        .map(|i| i.arg(3).to_owned())
        .collect()
}

#[test]
fn zstd_images_go_to_clients_that_announce_them() {
    let png = ["image/png", "image/jpeg", "image/webp"];
    let formats = ImageFormats::from_mimetypes(&png);
    assert!(formats.jpeg && !formats.zstd);
    let ours = [png.as_slice(), &[zimage::MIMETYPE]].concat();
    assert!(ImageFormats::from_mimetypes(&ours).zstd);
    // A later version of the type is another type.
    assert!(!ImageFormats::from_mimetypes(&["image/x-molcajete-zstd-v2"]).zstd);

    for zstd in [false, true] {
        let (mut d, mut c, _) = setup_for(200, 100, zstd);
        let text: Vec<u8> = (0..40).flat_map(|r| text_row(200, r)).collect();
        d.put(0, 0, 200, 40, &text);
        let (stats, out) = flush_into(&mut d, &mut c);
        assert_eq!(stats.images, 1);
        let expected = if zstd { zimage::MIMETYPE } else { "image/png" };
        assert_eq!(image_types(&out), [expected]);
        assert_eq!(c.zstd_images, zstd as usize);
    }
}

#[test]
fn photographs_and_transparency_stay_png() {
    let (mut d, mut c, _) = setup_for(320, 200, true);
    // A window with more than 256 colours is a zstd image (RGB).
    d.put(0, 0, 160, 60, &window_block(160, 60, 1));
    let (_, out) = flush_into(&mut d, &mut c);
    assert_eq!(image_types(&out), [zimage::MIMETYPE]);
    // A photograph is smaller as PNG.
    d.put(0, 100, 160, 60, &photo_block(160, 60, 2));
    let (_, out) = flush_into(&mut d, &mut c);
    assert_eq!(image_types(&out), ["image/png"]);
    // The format has no alpha: a region with transparent pixels is PNG.
    let mut holes = text_row(64, 3).repeat(8);
    holes[..4].fill(0);
    holes[64 * 4 * 5 + 4..][..4].fill(0);
    d.put(200, 0, 64, 8, &holes);
    let (_, out) = flush_into(&mut d, &mut c);
    assert_eq!(image_types(&out), ["image/png"]);
    // And so is the cursor, which is drawn with its alpha channel.
    let mut out = Output::text();
    d.set_cursor(0, 0, 16, 16, &[0x80; 16 * 16 * 4], &mut out);
    assert_eq!(image_types(&out), ["image/png"]);
}

#[test]
fn inverse_undoes_the_hash_multiplier() {
    assert_eq!(HASH_A.wrapping_mul(inverse(HASH_A)), 1);
    assert_eq!(HASH_B.wrapping_mul(inverse(HASH_B)), 1);
}

#[test]
fn far_apart_changes_form_separate_search_areas() {
    let rects = [
        Rect::new(0, 0, 64, 64),
        Rect::new(4000, 2000, 64, 64),
        Rect::new(128, 0, 64, 64),
        Rect::new(64, 200, 64, 64),
    ];
    let (areas, owner) = cluster(&rects, 4 * TILE);
    assert_eq!(owner[0], owner[2]);
    assert_eq!(owner[0], owner[3]);
    assert_ne!(owner[0], owner[1]);
    assert_eq!(areas[owner[0]], Rect::new(0, 0, 192, 264));
    assert_eq!(areas[owner[1]], rects[1]);
}

#[test]
fn clusters_joined_by_a_later_rectangle_merge() {
    let rects = [
        Rect::new(0, 0, 64, 64),
        Rect::new(1000, 0, 64, 64),
        Rect::new(64, 0, 936, 64),
    ];
    let (areas, owner) = cluster(&rects, TILE);
    assert!(owner.iter().all(|&o| o == owner[0]));
    assert_eq!(areas[owner[0]], Rect::new(0, 0, 1064, 64));
}

#[test]
fn row_differences_are_found_at_any_position() {
    let a: Vec<u8> = (0..1000u32).flat_map(|i| [i as u8, 3, 5, 255]).collect();
    assert_eq!(first_diff(&a, &a), None);
    assert_eq!(last_diff(&a, &a), None);
    for (first, last) in [(0usize, 0usize), (0, 999), (15, 16), (17, 500), (999, 999)] {
        let mut b = a.clone();
        // Change a different byte of the pixel each time.
        b[first * 4 + first % 4] ^= 0x40;
        b[last * 4 + last % 4] ^= 0x80;
        assert_eq!(first_diff(&a, &b), Some(first));
        assert_eq!(last_diff(&a, &b), Some(last));
    }
}

#[test]
fn windows_moved_on_a_large_screen_are_found_by_sampling() {
    // Larger than `MOVE_FULL_SEARCH` in both directions, so only samples of
    // the changed area are searched.
    let (w, h) = (2000u32, 1400u32);
    let (ww, wh) = (900u32, 700u32);
    let (mut d, mut c, _) = setup(w, h);
    let background = [30u8, 60, 90, 255];
    let window: Vec<u8> = (0..wh)
        .flat_map(|r| text_row(ww, r as u64 * 7 + 3))
        .collect();
    let frame = |wx: u32, wy: u32| -> Vec<u8> {
        let mut px: Vec<u8> = background.repeat((w * h) as usize);
        for r in 0..wh {
            let d = (((wy + r) * w + wx) * 4) as usize;
            let s = (r * ww * 4) as usize;
            px[d..d + (ww * 4) as usize].copy_from_slice(&window[s..s + (ww * 4) as usize]);
        }
        px
    };
    d.put(0, 0, w, h, &frame(300, 200));
    flush_into(&mut d, &mut c);
    d.put(0, 0, w, h, &frame(337, 223));
    let (stats, _) = flush_into(&mut d, &mut c);
    assert!(stats.copies >= 1, "copies: {}", stats.copies);
    assert!(
        stats.pixels < (ww * wh) as u64 / 4,
        "pixels: {}",
        stats.pixels
    );
}

/// Encoding the images of a frame side by side changes nothing in what is
/// sent: the same instructions in the same order as one after another.
#[test]
fn images_encoded_side_by_side_are_sent_in_order() {
    let pool: &'static rayon::ThreadPool = Box::leak(Box::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("pool"),
    ));
    let (w, h) = (480u32, 320u32);
    for zstd in [false, true] {
        let (mut serial, mut serial_client, _) = setup_for(w, h, zstd);
        let (mut parallel, mut parallel_client, _) = setup_for(w, h, zstd);
        serial.pool = None;
        parallel.pool = Some(pool);
        let mut rng = Rng(0x5eed + zstd as u64);
        let mut most_images = 0;
        for _ in 0..30 {
            for _ in 0..1 + rng.below(8) {
                let (x, y) = (rng.below(w), rng.below(h));
                let (rw, rh) = (
                    1 + rng.below((w - x).min(160)),
                    1 + rng.below((h - y).min(90)),
                );
                let block = match rng.below(4) {
                    0 => window_block(rw, rh, rng.next()),
                    1 => photo_block(rw, rh, rng.next()),
                    2 => [rng.below(256) as u8, 40, 80, 255].repeat((rw * rh) as usize),
                    _ => {
                        let seed = rng.next();
                        (0..rh)
                            .flat_map(|r| text_row(rw, seed.wrapping_add(r as u64)))
                            .collect()
                    }
                };
                serial.put(x, y, rw, rh, &block);
                parallel.put(x, y, rw, rh, &block);
            }
            let (expected, sent) = flush_into(&mut serial, &mut serial_client);
            let (stats, out) = flush_into(&mut parallel, &mut parallel_client);
            assert_eq!(out, sent);
            assert_eq!(stats.images, expected.images);
            most_images = most_images.max(stats.images);
        }
        assert!(most_images > 3, "at most {most_images} images per frame");
    }
}

#[test]
fn row_differences_of_a_segment_follow_from_the_whole_row() {
    let w = 300u32;
    let (mut d, _, _) = setup(w, 1);
    let mut rng = Rng(99);
    for _ in 0..200 {
        // A few changed pixels anywhere in the row.
        d.pixels.copy_from_slice(&d.sent);
        for _ in 0..1 + rng.below(4) {
            let x = rng.below(w) as usize * 4;
            d.pixels[x + rng.below(3) as usize] ^= 0x10;
        }
        let whole = d.row_diff(0, 0, w);
        for _ in 0..20 {
            let x0 = rng.below(w);
            let x1 = x0 + 1 + rng.below(w - x0);
            assert_eq!(d.diff_within(whole, 0, x0, x1), d.row_diff(0, x0, x1));
        }
    }
}
