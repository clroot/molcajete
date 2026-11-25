//! Benchmark of taking RDP bitmap updates into the display on a 5K desktop:
//! the copy straight from IronRDP's image (`source_at` +
//! `Display::put_image`) against the previous `extract_rgba` +
//! `Display::put`, which copied every rectangle twice.
//!
//! Like `display::bench`, it is an ignored test; run it with optimisations:
//!
//! ```text
//! cargo test --release -p molcajete-daemon rdp::bench -- --ignored --nocapture --test-threads=1
//! ```

use std::time::{Duration, Instant};

use ironrdp_graphics::image_processing::PixelFormat;

use super::graphics::{Area, extract_rgba, source_at};
use crate::display::{Display, ImageFormats};
use crate::io::Output;

const WIDTH: u32 = 5120;
const HEIGHT: u32 = 2654;
const RUNS: usize = 15;

/// Rectangles the way servers send them: the whole screen in 64×64 tiles
/// (classic bitmap updates are at most 64 pixels on a side on the wire),
/// or scattered small changes (typing, menus, cursors in documents).
fn tiles() -> Vec<Area> {
    let mut areas = Vec::new();
    for y in (0..HEIGHT).step_by(64) {
        for x in (0..WIDTH).step_by(64) {
            areas.push(Area {
                x,
                y,
                width: 64.min(WIDTH - x),
                height: 64.min(HEIGHT - y),
            });
        }
    }
    areas
}

fn scattered() -> Vec<Area> {
    let mut seed = 0x5eed_1234_abcd_ef01u64;
    let mut next = move |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % u64::from(n)) as u32
    };
    (0..4000)
        .map(|_| {
            let width = 8 + next(248);
            let height = 8 + next(56);
            Area {
                x: next(WIDTH - width),
                y: next(HEIGHT - height),
                width,
                height,
            }
        })
        .collect()
}

/// A desktop of noise, so that no copy can be skipped.
fn image() -> Vec<u8> {
    let mut seed = 0x1234_5678_9abc_def1u64;
    (0..WIDTH as usize * HEIGHT as usize * 4)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 56) as u8
        })
        .collect()
}

fn run(name: &str, areas: &[Area], format: PixelFormat) {
    let data = image();
    let stride = WIDTH as usize * 4;
    let mut display = Display::new(ImageFormats::default(), true);
    display.resize(WIDTH, HEIGHT, &mut Output::text());
    let megapixels = areas
        .iter()
        .map(|a| f64::from(a.width) * f64::from(a.height))
        .sum::<f64>()
        / 1e6;

    let pass = |display: &mut Display, apply: &dyn Fn(&mut Display, Area)| {
        let started = Instant::now();
        for &area in areas {
            apply(display, area);
        }
        started.elapsed()
    };
    let old_copy = |display: &mut Display, area: Area| {
        let rgba = extract_rgba(&data, stride, format, area);
        display.put(area.x, area.y, area.width, area.height, &rgba);
    };
    let new_copy = |display: &mut Display, area: Area| {
        let source = source_at(&data, stride, format, area).expect("4 bytes per pixel");
        display.put_image(area.x, area.y, area.width, area.height, &source);
    };
    // Alternated, so that both see the same machine load.
    let (mut old, mut new): (Vec<Duration>, Vec<Duration>) = (0..RUNS)
        .map(|_| (pass(&mut display, &old_copy), pass(&mut display, &new_copy)))
        .unzip();
    let median = |runs: &mut Vec<Duration>| {
        runs.sort();
        runs[RUNS / 2].as_secs_f64() * 1000.0
    };
    let (old, new) = (median(&mut old), median(&mut new));

    let mut reference = Display::new(ImageFormats::default(), true);
    reference.resize(WIDTH, HEIGHT, &mut Output::text());
    pass(&mut reference, &old_copy);
    assert!(
        reference.framebuffer() == display.framebuffer(),
        "copies differ"
    );
    println!(
        "bench {name:<9} {format:<8} {:5} rects {megapixels:5.1} MP | extract+put {old:7.2} ms ({:5.2} ms/MP) | put_image {new:7.2} ms ({:5.2} ms/MP) | {:+.0}%",
        areas.len(),
        old / megapixels,
        new / megapixels,
        (new / old - 1.0) * 100.0,
        format = format!("{format:?}"),
    );
}

#[test]
#[ignore = "benchmark"]
fn bitmap_updates() {
    let (tiles, scattered) = (tiles(), scattered());
    // RgbA32 is what the session decodes into; BgrA32 takes the shuffle.
    for format in [PixelFormat::RgbA32, PixelFormat::BgrA32] {
        run("tiles", &tiles, format);
        run("scattered", &scattered, format);
    }
}
