//! Benchmarks of [`Display::flush`] on a 5K framebuffer, replaying the
//! updates a remote desktop produces: scrolling a document, dragging a
//! window, small changes far apart, and replacing a page of text.
//!
//! They are ignored tests so that they need no extra crates; run them with
//! optimisations and one at a time:
//!
//! ```text
//! cargo test --release -p molcajete-daemon display::bench -- --ignored --nocapture --test-threads=1
//! ```

use std::time::{Duration, Instant};

use super::*;

const WIDTH: u32 = 5120;
const HEIGHT: u32 = 2654;
const FRAMES: u32 = 20;

const LINE_HEIGHT: u32 = 28;
const GLYPH_WIDTH: u32 = 14;
const GLYPH_HEIGHT: u32 = 20;
const GLYPHS: usize = 64;
const MARGIN: u32 = 40;

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

/// An endless text document: anti-aliased glyphs in a few ink colours on
/// white, ragged line ends, blank lines between paragraphs and shaded code
/// blocks. Line `n` always renders the same pixels.
struct Document {
    /// Ink coverage (0..=255) per glyph pixel.
    glyphs: Vec<u8>,
}

impl Document {
    fn new() -> Self {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        let glyphs = (0..GLYPHS * (GLYPH_WIDTH * GLYPH_HEIGHT) as usize)
            .map(|_| match rng.below(100) {
                0..60 => 0,
                60..85 => 255,
                85..90 => 64,
                90..95 => 128,
                _ => 192,
            })
            .collect();
        Self { glyphs }
    }

    /// Renders rows `[y, y + h)` of the document, `w` pixels wide.
    fn render(&self, w: u32, y: u32, h: u32) -> Vec<u8> {
        let mut out = vec![255u8; (w * h * 4) as usize];
        let first = y / LINE_HEIGHT;
        let last = (y + h).div_ceil(LINE_HEIGHT);
        for line in first..last {
            let mut rng = Rng((line as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let kind = rng.below(12);
            if kind == 0 {
                continue; // blank line between paragraphs
            }
            let paper: [u8; 3] = if kind <= 2 {
                [240, 242, 245] // code block
            } else {
                [255, 255, 255]
            };
            let cells = (w - 2 * MARGIN) / GLYPH_WIDTH;
            let length = cells * (40 + rng.below(56)) / 100;
            let mut ink = [34u8, 34, 34];
            let mut word_left = 0;
            for cell in 0..cells {
                if word_left == 0 {
                    word_left = 2 + rng.below(10);
                    ink = [
                        [34u8, 34, 34],
                        [34, 34, 34],
                        [34, 34, 34],
                        [0, 92, 197],
                        [215, 58, 73],
                        [34, 134, 58],
                        [111, 66, 193],
                    ][rng.below(7) as usize];
                }
                word_left -= 1;
                let glyph = rng.below(GLYPHS as u32) as usize;
                let blank = word_left == 0 || cell >= length;
                for gy in 0..LINE_HEIGHT {
                    let row = line * LINE_HEIGHT + gy;
                    if row < y || row >= y + h {
                        continue;
                    }
                    for gx in 0..GLYPH_WIDTH {
                        let x = MARGIN + cell * GLYPH_WIDTH + gx;
                        let coverage = if blank || gy >= GLYPH_HEIGHT {
                            0
                        } else {
                            self.glyphs[glyph * (GLYPH_WIDTH * GLYPH_HEIGHT) as usize
                                + (gy * GLYPH_WIDTH + gx) as usize]
                                as u32
                        };
                        let o = (((row - y) * w + x) * 4) as usize;
                        for c in 0..3 {
                            out[o + c] = ((ink[c] as u32 * coverage
                                + paper[c] as u32 * (255 - coverage))
                                / 255) as u8;
                        }
                    }
                }
            }
            if paper != [255, 255, 255] {
                // Shade the margins of code lines too.
                for gy in 0..LINE_HEIGHT {
                    let row = line * LINE_HEIGHT + gy;
                    if row < y || row >= y + h {
                        continue;
                    }
                    for x in (0..MARGIN).chain(MARGIN + cells * GLYPH_WIDTH..w) {
                        let o = (((row - y) * w + x) * 4) as usize;
                        out[o..o + 3].copy_from_slice(&paper);
                    }
                }
            }
        }
        out
    }
}

/// The remote desktop: a photographic wallpaper with windows drawn on top.
struct Desktop {
    wallpaper: Vec<u8>,
    pixels: Vec<u8>,
}

impl Desktop {
    fn new() -> Self {
        let mut rng = Rng(0x0dd_ba11);
        let mut wallpaper = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let noise = rng.below(6) as u8;
                wallpaper.extend_from_slice(&[
                    (x * 200 / WIDTH) as u8 + noise,
                    (y * 200 / HEIGHT) as u8 + noise,
                    ((x + y) * 200 / (WIDTH + HEIGHT)) as u8 + noise,
                    255,
                ]);
            }
        }
        Self {
            pixels: wallpaper.clone(),
            wallpaper,
        }
    }

    fn blit(&mut self, rect: &Rect, rgba: &[u8]) {
        let row = rect.w as usize * 4;
        for r in 0..rect.h as usize {
            let d = ((rect.y as usize + r) * WIDTH as usize + rect.x as usize) * 4;
            self.pixels[d..d + row].copy_from_slice(&rgba[r * row..(r + 1) * row]);
        }
    }

    fn restore_wallpaper(&mut self, rect: &Rect) {
        let row = rect.w as usize * 4;
        for r in 0..rect.h as usize {
            let d = ((rect.y as usize + r) * WIDTH as usize + rect.x as usize) * 4;
            self.pixels[d..d + row].copy_from_slice(&self.wallpaper[d..d + row]);
        }
    }

    /// Reports `rect` of the desktop to the display, as the remote desktop
    /// protocol would.
    fn update(&self, display: &mut Display, rect: &Rect) {
        let row = rect.w as usize * 4;
        let mut rgba = Vec::with_capacity(row * rect.h as usize);
        for r in 0..rect.h as usize {
            let s = ((rect.y as usize + r) * WIDTH as usize + rect.x as usize) * 4;
            rgba.extend_from_slice(&self.pixels[s..s + row]);
        }
        display.put(rect.x, rect.y, rect.w, rect.h, &rgba);
    }
}

fn union(a: &Rect, b: &Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        a.right().max(b.right()) - x,
        a.bottom().max(b.bottom()) - y,
    )
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Sends the initial desktop, then times the flush of every frame produced
/// by `step` (which changes the desktop and reports the changes).
fn run(name: &str, mut step: impl FnMut(u32, &mut Desktop, &mut Display)) {
    let mut desktop = Desktop::new();
    let mut display = Display::new(ImageFormats { jpeg: true }, false);
    let mut out = String::new();
    display.resize(WIDTH, HEIGHT, &mut out);
    step(0, &mut desktop, &mut display);
    let full = Rect::new(0, 0, WIDTH, HEIGHT);
    desktop.update(&mut display, &full);
    display.flush(&mut out);

    let phases = [Phase::Scroll, Phase::Moves, Phase::Pixels];
    let before = phases.map(|p| display.metrics.phase_total(p));
    let (png_before, png_time_before) = display.metrics.png_total();
    let mut times = Vec::new();
    let mut stats = FlushStats::default();
    for frame in 1..=FRAMES {
        step(frame, &mut desktop, &mut display);
        out.clear();
        let started = Instant::now();
        stats.add(display.flush(&mut out));
        times.push(started.elapsed());
        assert!(
            display.pixels == display.sent || !display.lossless,
            "mirror diverged"
        );
    }
    times.sort();
    let n = FRAMES as f64;
    let phase = |i: usize| ms(display.metrics.phase_total(phases[i]) - before[i]) / n;
    let (png, png_time) = display.metrics.png_total();
    println!(
        "bench {name:<8} flush avg {:7.2} p50 {:7.2} max {:7.2} ms | scroll {:6.2} moves {:6.2} pixels {:6.2} (png {:6.2}) ms | images {:6.1} copies {:5.1} fills {:5.1} kB {:7.1} per frame",
        ms(times.iter().sum::<Duration>()) / n,
        ms(times[times.len() / 2]),
        ms(*times.last().expect("frames")),
        phase(0),
        phase(1),
        phase(2),
        ms(png_time - png_time_before) / n,
        (stats.images as f64) / n,
        stats.copies as f64 / n,
        stats.fills as f64 / n,
        stats.bytes as f64 / 1024.0 / n,
    );
    // Images sent as JPEG stay lossy; everything else must match exactly.
    let _ = png - png_before;
}

/// A large document window scrolls by a few lines per frame.
#[test]
#[ignore = "benchmark"]
fn scroll() {
    let doc = Document::new();
    let window = Rect::new(400, 200, 3200, 2200);
    run("scroll", |frame, desktop, display| {
        let steps = [84, 56, 112, 28, 140];
        let offset: u32 = (0..frame).map(|f| steps[f as usize % steps.len()]).sum();
        desktop.blit(&window, &doc.render(window.w, offset, window.h));
        if frame > 0 {
            desktop.update(display, &window);
        }
    });
}

/// A window is dragged diagonally over the wallpaper.
#[test]
#[ignore = "benchmark"]
fn drag() {
    let doc = Document::new();
    let (w, h) = (1800u32, 1200u32);
    let content = doc.render(w, 0, h);
    let at = |frame: u32| Rect::new(300 + frame * 23, 200 + frame * 17, w, h);
    run("drag", |frame, desktop, display| {
        if frame > 0 {
            desktop.restore_wallpaper(&at(frame - 1));
        }
        desktop.blit(&at(frame), &content);
        if frame > 0 {
            desktop.update(display, &union(&at(frame - 1), &at(frame)));
        }
    });
}

/// Two small areas at opposite corners change every frame (a typed line and
/// a clock): the bounding box of the changes is the whole screen.
#[test]
#[ignore = "benchmark"]
fn corners() {
    let doc = Document::new();
    let typed = Rect::new(96, 112, 1400, LINE_HEIGHT * 3);
    let clock = Rect::new(4700, 2560, 360, LINE_HEIGHT * 2);
    run("corners", |frame, desktop, display| {
        desktop.blit(
            &typed,
            &doc.render(typed.w, frame * 500 * LINE_HEIGHT, typed.h),
        );
        desktop.blit(
            &clock,
            &doc.render(clock.w, frame * 900 * LINE_HEIGHT, clock.h),
        );
        if frame > 0 {
            desktop.update(display, &typed);
            desktop.update(display, &clock);
        }
    });
}

/// A window shows a different page of text every frame: nothing can be
/// copied, everything is encoded.
#[test]
#[ignore = "benchmark"]
fn page() {
    let doc = Document::new();
    let window = Rect::new(400, 200, 3200, 2200);
    run("page", |frame, desktop, display| {
        desktop.blit(
            &window,
            &doc.render(window.w, frame * 5000 * LINE_HEIGHT, window.h),
        );
        if frame > 0 {
            desktop.update(display, &window);
        }
    });
}

/// Many separate small updates per frame (a busy terminal multiplexer or a
/// dashboard): scattered lines of text change all over a window.
#[test]
#[ignore = "benchmark"]
fn scatter() {
    let doc = Document::new();
    let window = Rect::new(400, 200, 3200, 2200);
    run("scatter", |frame, desktop, display| {
        if frame == 0 {
            desktop.blit(&window, &doc.render(window.w, 0, window.h));
            return;
        }
        let mut rng = Rng(frame as u64 * 7919 + 1);
        for _ in 0..40 {
            let w = 200 + rng.below(900);
            let line = rng.below(window.h / LINE_HEIGHT);
            let x = rng.below(window.w - w);
            let rect = Rect::new(window.x + x, window.y + line * LINE_HEIGHT, w, LINE_HEIGHT);
            let source = doc.render(window.w, (frame * 3000 + line) * LINE_HEIGHT, LINE_HEIGHT);
            let row = rect.w as usize * 4;
            let part: Vec<u8> = (0..LINE_HEIGHT as usize)
                .flat_map(|r| {
                    let s = r * window.w as usize * 4 + MARGIN as usize * 4;
                    source[s..s + row].to_vec()
                })
                .collect();
            desktop.blit(&rect, &part);
            desktop.update(display, &rect);
        }
    });
}

/// Time and size of PNG encoding per DEFLATE setting, on the kinds of
/// images a session sends: a page of text, single lines and a photograph.
#[test]
#[ignore = "benchmark"]
fn png_levels() {
    use png::DeflateCompression::{FdeflateUltraFast, Level};

    let doc = Document::new();
    let desktop = Desktop::new();
    let page = doc.render(3200, 0, 2200);
    let lines: Vec<Vec<u8>> = (0..200)
        .map(|i| doc.render(900, i * 40 * LINE_HEIGHT, LINE_HEIGHT))
        .collect();
    let photo: Vec<u8> = desktop.wallpaper[..(WIDTH * 512 * 4) as usize].to_vec();
    let mut images = vec![
        ("page", vec![(3200, 2200, page)]),
        (
            "lines",
            lines.into_iter().map(|l| (900, LINE_HEIGHT, l)).collect(),
        ),
        ("photo", vec![(WIDTH, 512, photo)]),
    ];
    // Pages are sent in tile bands rather than as one image.
    let band = doc.render(3200, 0, 2200);
    images.push((
        "bands",
        band.chunks(3200 * 64 * 4)
            .map(|b| (3200, (b.len() / (3200 * 4)) as u32, b.to_vec()))
            .collect(),
    ));
    for (name, set) in &images {
        for compression in [
            FdeflateUltraFast,
            Level(1),
            Level(2),
            Level(3),
            Level(4),
            Level(6),
        ] {
            let started = Instant::now();
            let mut bytes = 0;
            for (width, height, rgba) in set {
                let region = Region {
                    width: *width,
                    height: *height,
                    rgba,
                };
                bytes += encode::encode_png_with(&region, compression)
                    .expect("png")
                    .len();
            }
            println!(
                "png {name:<6} {:<18} {:8.2} ms {:9.1} kB",
                format!("{compression:?}"),
                ms(started.elapsed()),
                bytes as f64 / 1024.0
            );
        }
    }
}
