use std::collections::HashMap;

use base64::Engine;
use molcajete_protocol::parse_all;

use super::*;

/// A minimal Guacamole client for layer 0: replays emitted instructions so
/// tests can check that what the client shows equals the framebuffer.
struct Client {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    rect: Option<Rect>,
    streams: HashMap<i32, (String, i32, i32, i32, Vec<u8>)>,
}

impl Client {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            rect: None,
            streams: HashMap::new(),
        }
    }

    fn index(&self, x: u32, y: u32) -> usize {
        ((y * self.width + x) * 4) as usize
    }

    fn apply(&mut self, text: &str) {
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
                    assert_eq!(mime, "image/png", "tests run lossless");
                    let (w, h, rgba) = decode_png(&data);
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

fn setup(w: u32, h: u32) -> (Display, Client, String) {
    let mut d = Display::new(ImageFormats { jpeg: false }, true);
    let mut out = String::new();
    d.resize(w, h, &mut out);
    let mut c = Client::new();
    c.apply(&out);
    (d, c, String::new())
}

fn flush_into(d: &mut Display, c: &mut Client) -> (FlushStats, String) {
    let mut out = String::new();
    let stats = d.flush(&mut out);
    c.apply(&out);
    assert!(c.pixels == d.pixels, "client diverged from the framebuffer");
    (stats, out)
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
    let mut out = String::new();
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

#[test]
fn busy_areas_go_lossy_then_get_refined() {
    let mut d = Display::new(ImageFormats { jpeg: true }, false);
    let mut out = String::new();
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
    assert_eq!(stats.images, 16, "one PNG per 64x64 tile");
    assert!(
        parse_all(&out)
            .unwrap()
            .iter()
            .filter(|i| i.opcode == "img")
            .all(|i| i.arg(3) == "image/png")
    );
    assert!(d.lossy.iter().all(|l| !l));
}

#[test]
fn sync_and_ack_track_lag() {
    let (mut d, _, mut out) = setup(4, 4);
    d.sync(&mut out);
    d.sync(&mut out);
    assert_eq!(d.unacked_frames(), 2);
    let last = d.last_sync.unwrap();
    d.ack(last - 1);
    assert_eq!(d.unacked_frames(), 1);
    d.ack(last);
    assert_eq!(d.unacked_frames(), 0);
    assert!(d.next_frame_at() > Instant::now());
}

/// Random mixes of puts, fills, protocol copies, scrolls and resizes: after
/// every flush the simulated client must equal the framebuffer exactly.
#[test]
fn fuzz_client_always_matches_framebuffer() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (mut w, mut h) = (64 + rng.below(200), 64 + rng.below(160));
        let (mut d, mut c, _) = setup(w, h);
        for step in 0..60 {
            match rng.below(11) {
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
                    let mut out = String::new();
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
                    let mut out = String::new();
                    d.resize(w, h, &mut out);
                    c.apply(&out);
                }
                _ => {
                    flush_into(&mut d, &mut c);
                }
            }
        }
        flush_into(&mut d, &mut c);
    }
}
