//! Server-side display: keeps the remote framebuffer, tracks what the client
//! is currently showing, and sends only what differs.
//!
//! Two copies of the default layer are kept:
//! - `pixels`: the truth, as reported by the remote desktop protocol;
//! - `sent`: a mirror of the client's layer, updated by replaying every
//!   instruction we emit (fills, images, copies).
//!
//! At flush time the regions touched since the last frame are compared
//! against the mirror. Content that moved vertically (scrolling) is found
//! with row hashes and turned into cheap `copy` instructions; what still
//! differs afterwards is trimmed to changed row bands and column runs and
//! encoded (solid fill, palette/RGB PNG, or JPEG for busy photographic areas,
//! later refined losslessly once the area calms down).
//!
//! Because emitted instructions are replayed on the mirror, every optimisation
//! is self-correcting: a wrong guess simply shows up as a difference in the
//! next comparison and gets sent as pixels.

pub mod encode;
mod metrics;
mod video;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use molcajete_protocol::InstructionBuilder;

use encode::Region;
use metrics::{DisplayMetrics, ImageKind, Phase, timed};

/// Largest raw chunk carried by one `blob` (same as libguac).
const BLOB_MAX_BYTES: usize = 6048;
/// Granularity of change tracking and of the heat map.
const TILE: u32 = 64;
/// Changed rows separated by fewer unchanged rows share one image.
const ROW_GAP_MERGE: u32 = 6;
/// Changed columns separated by fewer unchanged columns share one image.
const COLUMN_GAP_MERGE: u32 = 48;
/// Minimum number of rows a scroll copy must cover.
const MIN_SCROLL_ROWS: u32 = 8;
/// Bound on pixel comparisons per frame while searching for moved blocks.
const MAX_VERIFICATIONS: u32 = 4096;
/// Multipliers of the 2D rolling block hash (both odd).
const HASH_A: u64 = 0x0000_0100_0000_01b3;
const HASH_B: u64 = 0x9e37_79b9_7f4a_7c15;
/// Heat (≈ updates per second, decaying) above which an area counts as busy.
const HOT: f32 = 3.0;
/// Heat below which lossy areas are re-sent losslessly.
const COOL: f32 = 0.25;

/// Shortest interval between frames while content keeps changing (~30 fps).
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Frames carrying more encoded data than this count as heavy...
const LARGE_FRAME_BYTES: usize = 32 * 1024;
/// ...and are followed by a longer pause (~15 fps, similar to guacd under
/// floods of output), which bounds the bandwidth of continuous heavy updates.
const LARGE_FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// Upper bound for waiting on a slow client (same as guacd).
const MAX_LAG_COMPENSATION: Duration = Duration::from_millis(500);

const MODE_OVER: u8 = 0xE;
const MODE_SRC: u8 = 0xC;
const MODE_ROUT: u8 = 0x2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    fn right(&self) -> u32 {
        self.x + self.w
    }

    fn bottom(&self) -> u32 {
        self.y + self.h
    }

    fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Clips to a `width`×`height` surface.
    fn clip(&self, width: u32, height: u32) -> Rect {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Rect::new(
            x,
            y,
            self.right().min(width) - x,
            self.bottom().min(height) - y,
        )
    }
}

/// Image formats accepted by the connected client (from the `image`
/// handshake instruction).
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageFormats {
    pub jpeg: bool,
}

impl ImageFormats {
    pub fn from_mimetypes<S: AsRef<str>>(mimetypes: &[S]) -> Self {
        Self {
            jpeg: mimetypes.iter().any(|m| m.as_ref() == "image/jpeg"),
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct FlushStats {
    pub images: u32,
    pub fills: u32,
    pub copies: u32,
    /// Encoded image bytes (before base64).
    pub bytes: usize,
    /// Pixels sent as images or fills.
    pub pixels: u64,
}

impl FlushStats {
    fn add(&mut self, o: FlushStats) {
        self.images += o.images;
        self.fills += o.fills;
        self.copies += o.copies;
        self.bytes += o.bytes;
        self.pixels += o.pixels;
    }
}

/// Changed column range (inclusive) of one row.
#[derive(Clone, Copy)]
struct RowDiff {
    first: u32,
    last: u32,
}

pub struct Display {
    width: u32,
    height: u32,
    /// Remote framebuffer (RGBA, top row first).
    pixels: Vec<u8>,
    /// What the client shows (mirror of its default layer).
    sent: Vec<u8>,
    tile_cols: u32,
    tile_rows: u32,
    /// Tiles touched since the last flush.
    dirty: Vec<bool>,
    /// Tiles whose client content came from a lossy encoding.
    lossy: Vec<bool>,
    /// Recent update frequency per tile (decays over time).
    heat: Vec<f32>,
    /// Recent frames per tile that had to be sent as pixels (decays over
    /// time); finds areas worth a video stream.
    motion: Vec<f32>,
    last_decay: Instant,
    last_flush: Instant,
    formats: ImageFormats,
    lossless: bool,
    next_stream: i32,
    last_sync: Option<i64>,
    /// Frames sent and not yet acknowledged: (timestamp, sent at).
    pending_syncs: VecDeque<(i64, Instant)>,
    /// Smoothed time the client needs to acknowledge a frame.
    lag: Duration,
    last_frame: Option<Instant>,
    /// Encoded image bytes of the frame being built and of the last frame.
    frame_bytes: usize,
    last_frame_bytes: usize,
    /// Last cursor sent (hotspot, size, pixels), to drop redundant updates.
    last_cursor: Option<(u32, u32, u32, u32, Vec<u8>)>,
    /// Video encoding, when the client and the server both support it.
    video_support: Option<video::VideoSupport>,
    /// The video stream currently covering a busy area.
    video: Option<video::ActiveVideo>,
    metrics: DisplayMetrics,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.metrics.summary();
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// FNV-1a over 64-bit words: fast and good enough to find candidate rows;
/// matches are always confirmed by comparing the bytes.
fn row_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        let word = u64::from_le_bytes(w.try_into().expect("8-byte chunk"));
        h = (h ^ word).wrapping_mul(0x0100_0000_01b3);
    }
    for &b in words.remainder() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn pixel_value(p: &[u8]) -> u64 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as u64
}

fn pow(base: u64, exp: u32) -> u64 {
    (0..exp).fold(1u64, |acc, _| acc.wrapping_mul(base))
}

fn is_uniform(row: &[u8]) -> bool {
    let first = &row[..4];
    row.chunks_exact(4).all(|p| p == first)
}

/// Groups indices `0..len` for which `set(i)` holds into runs, joining runs
/// separated by at most `max_gap` unset indices. Returns half-open ranges.
fn runs(len: usize, max_gap: usize, set: impl Fn(usize) -> bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < len {
        if !set(i) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        let mut j = i + 1;
        while j < len && j - end <= max_gap {
            if set(j) {
                end = j + 1;
            }
            j += 1;
        }
        out.push((start, end));
        i = end;
    }
    out
}

impl Display {
    pub fn new(formats: ImageFormats, lossless: bool) -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            sent: Vec::new(),
            tile_cols: 0,
            tile_rows: 0,
            dirty: Vec::new(),
            lossy: Vec::new(),
            heat: Vec::new(),
            motion: Vec::new(),
            last_decay: Instant::now(),
            last_flush: Instant::now(),
            formats,
            lossless,
            next_stream: 0,
            last_sync: None,
            pending_syncs: VecDeque::new(),
            lag: Duration::ZERO,
            last_frame: None,
            frame_bytes: 0,
            last_frame_bytes: 0,
            last_cursor: None,
            video_support: None,
            video: None,
            metrics: DisplayMetrics::disabled(),
        }
    }

    /// Labels this display's metrics with the remote desktop protocol.
    pub fn with_protocol(mut self, protocol: &'static str) -> Self {
        self.metrics = DisplayMetrics::new(protocol);
        self
    }

    /// Encodes `region` (timed), returning the bytes.
    fn encode_image(&mut self, kind: ImageKind, region: &Region) -> Option<Vec<u8>> {
        let (bytes, elapsed) = timed(|| match kind {
            ImageKind::Png => encode::encode_png(region).ok(),
            ImageKind::Jpeg => encode::encode_jpeg(region, 80).ok(),
        });
        let bytes = bytes?;
        let pixels = region.width as u64 * region.height as u64;
        self.metrics.image(kind, elapsed, bytes.len(), pixels);
        Some(bytes)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    fn offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.width as usize + x as usize) * 4
    }

    // ------------------------------------------------------------------
    // Updates from the remote side
    // ------------------------------------------------------------------

    /// Resizes the default layer, preserving the overlapping content (the
    /// client does the same with its layer).
    pub fn resize(&mut self, width: u32, height: u32, out: &mut String) {
        self.stop_video(out);
        let resized = |old: &[u8], old_w: u32, old_h: u32| {
            let mut buf = vec![0u8; (width * height * 4) as usize];
            let row = width.min(old_w) as usize * 4;
            for y in 0..height.min(old_h) as usize {
                let src = y * old_w as usize * 4;
                let dst = y * width as usize * 4;
                buf[dst..dst + row].copy_from_slice(&old[src..src + row]);
            }
            buf
        };
        self.pixels = resized(&self.pixels, self.width, self.height);
        self.sent = resized(&self.sent, self.width, self.height);
        self.width = width;
        self.height = height;
        self.tile_cols = width.div_ceil(TILE);
        self.tile_rows = height.div_ceil(TILE);
        let tiles = (self.tile_cols * self.tile_rows) as usize;
        // Everything is compared once against the mirror at the next flush.
        self.dirty = vec![true; tiles];
        self.lossy = vec![false; tiles];
        self.heat = vec![0.0; tiles];
        self.motion = vec![0.0; tiles];
        *out += &InstructionBuilder::new("size")
            .arg(0)
            .arg(width)
            .arg(height)
            .finish();
    }

    /// Tiles covered by `rect` as (column range, row range).
    fn tiles_of(&self, rect: &Rect) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
        (
            rect.x / TILE..rect.right().div_ceil(TILE).min(self.tile_cols),
            rect.y / TILE..rect.bottom().div_ceil(TILE).min(self.tile_rows),
        )
    }

    /// Halves heat and motion scores every second.
    fn decay(&mut self) {
        let elapsed = self.last_decay.elapsed().as_secs_f32();
        if elapsed > 0.1 {
            let factor = 0.5f32.powf(elapsed);
            self.heat.iter_mut().for_each(|h| *h *= factor);
            self.motion.iter_mut().for_each(|m| *m *= factor);
            self.last_decay = Instant::now();
        }
    }

    fn touch(&mut self, rect: &Rect) {
        self.decay();
        let (cols, rows) = self.tiles_of(rect);
        for ty in rows {
            for tx in cols.clone() {
                let i = (ty * self.tile_cols + tx) as usize;
                self.dirty[i] = true;
                self.heat[i] += 1.0;
            }
        }
    }

    fn average_heat(&self, rect: &Rect) -> f32 {
        let (cols, rows) = self.tiles_of(rect);
        let (mut sum, mut n) = (0.0, 0);
        for ty in rows {
            for tx in cols.clone() {
                sum += self.heat[(ty * self.tile_cols + tx) as usize];
                n += 1;
            }
        }
        if n == 0 { 0.0 } else { sum / n as f32 }
    }

    /// Writes RGBA pixels (top row first) into the framebuffer.
    pub fn put(&mut self, x: u32, y: u32, w: u32, h: u32, rgba: &[u8]) {
        let rect = Rect::new(x, y, w, h).clip(self.width, self.height);
        if rect.is_empty() || rgba.len() < (w * h * 4) as usize {
            return;
        }
        let row = rect.w as usize * 4;
        for dy in 0..rect.h as usize {
            let src = dy * w as usize * 4;
            let dst = self.offset(rect.x, rect.y + dy as u32);
            self.pixels[dst..dst + row].copy_from_slice(&rgba[src..src + row]);
        }
        self.touch(&rect);
    }

    pub fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        let rect = Rect::new(x, y, w, h).clip(self.width, self.height);
        if rect.is_empty() {
            return;
        }
        fill_rect(&mut self.pixels, self.width, &rect, color);
        self.touch(&rect);
    }

    /// Copies a framebuffer region (e.g. VNC CopyRect) and emits the same
    /// copy for the client.
    ///
    /// No flush is needed first: the copy is replayed on the mirror, so any
    /// pending change inside the source simply moves along and is detected
    /// as a difference at the next flush.
    #[allow(clippy::too_many_arguments)]
    pub fn copy(
        &mut self,
        sx: u32,
        sy: u32,
        dx: u32,
        dy: u32,
        w: u32,
        h: u32,
        out: &mut String,
    ) -> FlushStats {
        let src = Rect::new(sx, sy, w, h).clip(self.width, self.height);
        let dst = Rect::new(dx, dy, src.w, src.h).clip(self.width, self.height);
        if dst.is_empty() {
            return FlushStats::default();
        }
        let src = Rect::new(sx, sy, dst.w, dst.h);
        self.emit_copy(&src, dst.x, dst.y, out);
        copy_rect(&mut self.pixels, self.width, &src, dst.x, dst.y);

        // The destination is always re-compared: pending changes may have
        // moved into it, and transparent source pixels leave the client's
        // destination untouched under source-over.
        let (cols, rows) = self.tiles_of(&src);
        let mut src_lossy = false;
        for ty in rows {
            for tx in cols.clone() {
                src_lossy |= self.lossy[(ty * self.tile_cols + tx) as usize];
            }
        }
        let (cols, rows) = self.tiles_of(&dst);
        for ty in rows {
            for tx in cols.clone() {
                let i = (ty * self.tile_cols + tx) as usize;
                self.dirty[i] = true;
                self.lossy[i] |= src_lossy;
            }
        }
        FlushStats {
            copies: 1,
            ..FlushStats::default()
        }
    }

    // ------------------------------------------------------------------
    // Flush
    // ------------------------------------------------------------------

    /// Sends everything that differs between the framebuffer and the client.
    pub fn flush(&mut self, out: &mut String) -> FlushStats {
        let started = Instant::now();
        let mut stats = FlushStats::default();
        self.decay();
        self.last_flush = started;
        // Busy areas may be covered by a video stream instead of images.
        stats.add(self.update_video(out));
        let mut mark = Instant::now();
        self.metrics.phase(Phase::Video, mark - started);
        let mut phase_done = |metrics: &DisplayMetrics, phase: Phase| {
            let now = Instant::now();
            metrics.phase(phase, now - mark);
            mark = now;
        };
        let regions = self.dirty_regions();
        self.dirty.iter_mut().for_each(|d| *d = false);
        let mut sent_tiles = vec![false; self.dirty.len()];

        // 1. Vertical scrolling inside each region (terminals, documents).
        for region in &regions {
            if let Some(changed) = self.diff_bounds(region)
                && changed.h >= 2 * MIN_SCROLL_ROWS
                && changed.w >= 16
            {
                stats.add(self.detect_scroll(&changed, out));
            }
        }
        phase_done(&self.metrics, Phase::Scroll);
        // 2. Blocks that moved anywhere (dragged windows, panning).
        stats.add(self.detect_moves(&regions, out));
        phase_done(&self.metrics, Phase::Moves);
        // 3. Whatever still differs is sent as pixels.
        for region in regions {
            if let Some(changed) = self.diff_bounds(&region) {
                for rect in self.changed_rects(&changed) {
                    let (cols, rows) = self.tiles_of(&rect);
                    for ty in rows {
                        for tx in cols.clone() {
                            sent_tiles[(ty * self.tile_cols + tx) as usize] = true;
                        }
                    }
                    stats.add(self.emit_rect(rect, out));
                }
            }
        }
        for (motion, sent) in self.motion.iter_mut().zip(sent_tiles) {
            if sent {
                *motion += 1.0;
            }
        }
        phase_done(&self.metrics, Phase::Pixels);
        stats.add(self.refine_lossy(out));
        phase_done(&self.metrics, Phase::Refine);
        self.frame_bytes += stats.bytes;
        self.metrics.flushed(started.elapsed());
        stats
    }

    /// Merges dirty tiles into rectangles: horizontal runs, extended over
    /// consecutive tile rows with the same run.
    fn dirty_regions(&self) -> Vec<Rect> {
        // (first column, end column, first row, end row) in tiles.
        let mut open: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut done = Vec::new();
        for ty in 0..self.tile_rows {
            // Tiles under the video are left to it.
            let row_runs = runs(self.tile_cols as usize, 0, |tx| {
                self.dirty[(ty * self.tile_cols) as usize + tx]
                    && !self.tile_in_video(tx as u32, ty)
            });
            let mut next_open = Vec::new();
            for (a, b) in row_runs {
                let (a, b) = (a as u32, b as u32);
                match open.iter().position(|o| o.0 == a && o.1 == b) {
                    Some(pos) => {
                        let mut o = open.swap_remove(pos);
                        o.3 = ty + 1;
                        next_open.push(o);
                    }
                    None => next_open.push((a, b, ty, ty + 1)),
                }
            }
            done.append(&mut open);
            open = next_open;
        }
        done.append(&mut open);
        done.into_iter()
            .map(|(tx0, tx1, ty0, ty1)| {
                Rect::new(
                    tx0 * TILE,
                    ty0 * TILE,
                    (tx1 - tx0) * TILE,
                    (ty1 - ty0) * TILE,
                )
                .clip(self.width, self.height)
            })
            .filter(|r| !r.is_empty())
            .collect()
    }

    /// Changed column range of one row segment, if any.
    fn row_diff(&self, y: u32, x0: u32, x1: u32) -> Option<RowDiff> {
        let start = self.offset(x0, y);
        let end = self.offset(x1, y);
        let (a, b) = (&self.pixels[start..end], &self.sent[start..end]);
        if a == b {
            return None;
        }
        let pairs = || a.chunks_exact(4).zip(b.chunks_exact(4));
        let first = pairs().position(|(p, q)| p != q)? as u32;
        let last = pairs().rposition(|(p, q)| p != q)? as u32;
        Some(RowDiff {
            first: x0 + first,
            last: x0 + last,
        })
    }

    /// Bounding box of the differences inside `rect`.
    fn diff_bounds(&self, rect: &Rect) -> Option<Rect> {
        let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
        for y in rect.y..rect.bottom() {
            if let Some(d) = self.row_diff(y, rect.x, rect.right()) {
                x0 = x0.min(d.first);
                x1 = x1.max(d.last);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        (y0 != u32::MAX).then(|| Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }

    /// Hash of the `TILE`×`TILE` block at (x, y): Σ p·A^(63-col)·B^(63-row)
    /// mod 2^64 with odd A and B, so every pixel contributes. The sliding
    /// search in [`Display::detect_moves`] maintains the same value with
    /// a 2D rolling update.
    fn block_hash(&self, buf: &[u8], x: u32, y: u32) -> u64 {
        let mut cell = 0u64;
        for row in y..y + TILE {
            let mut acc = 0u64;
            for p in self.segment(buf, row, x, x + TILE).chunks_exact(4) {
                acc = acc.wrapping_mul(HASH_A).wrapping_add(pixel_value(p));
            }
            cell = cell.wrapping_mul(HASH_B).wrapping_add(acc);
        }
        cell
    }

    fn blocks_equal(&self, a: &[u8], ax: u32, ay: u32, b: &[u8], bx: u32, by: u32) -> bool {
        (0..TILE).all(|r| {
            self.segment(a, ay + r, ax, ax + TILE) == self.segment(b, by + r, bx, bx + TILE)
        })
    }

    /// Finds changed `TILE`-aligned blocks whose new content exists somewhere
    /// else on the client (at any offset) and copies them there.
    fn detect_moves(&mut self, regions: &[Rect], out: &mut String) -> FlushStats {
        let mut stats = FlushStats::default();
        // Changed, non-blank, fully visible tiles of the new frame.
        let mut wanted: Vec<(u32, u32, u64)> = Vec::new();
        for region in regions {
            let (cols, rows) = self.tiles_of(region);
            for ty in rows {
                for tx in cols.clone() {
                    let (x, y) = (tx * TILE, ty * TILE);
                    if x + TILE > self.width || y + TILE > self.height {
                        continue;
                    }
                    if self.blocks_equal(&self.pixels, x, y, &self.sent, x, y) {
                        continue;
                    }
                    let first = self.segment(&self.pixels, y, x, x + 1).to_vec();
                    let uniform = (y..y + TILE).all(|r| {
                        self.segment(&self.pixels, r, x, x + TILE)
                            .chunks_exact(4)
                            .all(|p| p == first.as_slice())
                    });
                    if !uniform {
                        wanted.push((x, y, self.block_hash(&self.pixels, x, y)));
                    }
                }
            }
        }
        // Searching costs O(area); only worth it for sizeable changes.
        if wanted.len() < 2 {
            return stats;
        }

        // Open-addressed table: hash -> index into `wanted`.
        let size = (wanted.len() * 4).next_power_of_two();
        let mask = size - 1;
        let home = |h: u64| (h ^ (h >> 29)) as usize & mask;
        let mut table: Vec<Option<(u64, usize)>> = vec![None; size];
        for (i, &(_, _, h)) in wanted.iter().enumerate() {
            let mut slot = home(h);
            while table[slot].is_some_and(|(stored, _)| stored != h) {
                slot = (slot + 1) & mask;
            }
            // Identical blocks share one entry; the first one wins.
            table[slot].get_or_insert((h, i));
        }

        // Slide a TILE×TILE window over the client's content in the area
        // spanned by the changes.
        let area = regions
            .iter()
            .copied()
            .reduce(|a, b| {
                let x = a.x.min(b.x);
                let y = a.y.min(b.y);
                Rect::new(
                    x,
                    y,
                    a.right().max(b.right()) - x,
                    a.bottom().max(b.bottom()) - y,
                )
            })
            .unwrap_or_default();
        if area.w < TILE || area.h < TILE {
            return stats;
        }
        let mut found: Vec<Option<(u32, u32)>> = vec![None; wanted.len()];
        let mut verifications = 0u32;
        let a_out = pow(HASH_A, TILE);
        let b_out = pow(HASH_B, TILE);
        let width = area.w as usize;
        // Row hashes of the last TILE rows (ring buffer) and running column sums.
        let mut ring = vec![0u64; width * TILE as usize];
        let mut columns = vec![0u64; width];
        let mut row_hashes = vec![0u64; width];
        for y in area.y..area.bottom() {
            let row = self.segment(&self.sent, y, area.x, area.right());
            let mut acc = 0u64;
            for i in 0..width {
                acc = acc
                    .wrapping_mul(HASH_A)
                    .wrapping_add(pixel_value(&row[i * 4..i * 4 + 4]));
                if i >= TILE as usize {
                    let old =
                        pixel_value(&row[(i - TILE as usize) * 4..(i - TILE as usize) * 4 + 4]);
                    acc = acc.wrapping_sub(old.wrapping_mul(a_out));
                }
                row_hashes[i] = acc;
            }
            let slot = ((y - area.y) % TILE) as usize * width;
            let full_rows = y - area.y >= TILE;
            for i in 0..width {
                let mut column = columns[i].wrapping_mul(HASH_B).wrapping_add(row_hashes[i]);
                if full_rows {
                    column = column.wrapping_sub(ring[slot + i].wrapping_mul(b_out));
                }
                columns[i] = column;
                ring[slot + i] = row_hashes[i];
                if i + 1 < TILE as usize || y + 1 < area.y + TILE {
                    continue;
                }
                let h = column;
                let mut slot = home(h);
                let idx = loop {
                    match table[slot] {
                        Some((stored, idx)) if stored == h => break Some(idx),
                        Some(_) => slot = (slot + 1) & mask,
                        None => break None,
                    }
                };
                let Some(idx) = idx else { continue };
                if found[idx].is_some() || verifications >= MAX_VERIFICATIONS {
                    continue;
                }
                let sx = area.x + i as u32 + 1 - TILE;
                let sy = y + 1 - TILE;
                let (dx, dy, _) = wanted[idx];
                verifications += 1;
                if (sx, sy) != (dx, dy)
                    && self.blocks_equal(&self.sent, sx, sy, &self.pixels, dx, dy)
                {
                    found[idx] = Some((sx, sy));
                }
            }
        }

        // Blocks moved by the same offset are merged into larger rectangles,
        // each sent as one copy (the client handles overlap within a copy).
        let mut by_offset: HashMap<(i64, i64), Vec<(u32, u32)>> = HashMap::new();
        for (idx, src) in found.into_iter().enumerate() {
            if let Some((sx, sy)) = src {
                let (dx, dy, _) = wanted[idx];
                let offset = (dx as i64 - sx as i64, dy as i64 - sy as i64);
                by_offset.entry(offset).or_default().push((dx, dy));
            }
        }
        for ((ox, oy), mut tiles) in by_offset {
            tiles.sort_by_key(|&(x, y)| (y, x));
            let mut rects = merge_tiles(&tiles);
            // Copy the rectangles furthest along the direction of motion first,
            // so no copy overwrites the source of a later one.
            rects.sort_by_key(|r| (-(oy.signum()) * r.y as i64, -(ox.signum()) * r.x as i64));
            for dst in rects {
                let src = Rect::new(
                    (dst.x as i64 - ox) as u32,
                    (dst.y as i64 - oy) as u32,
                    dst.w,
                    dst.h,
                );
                // Re-check against the current client state (earlier copies
                // may have changed it); only opaque sources replace pixels.
                let valid = (0..dst.h).all(|r| {
                    let s = self.segment(&self.sent, src.y + r, src.x, src.right());
                    s.chunks_exact(4).all(|p| p[3] == 255)
                        && s == self.segment(&self.pixels, dst.y + r, dst.x, dst.right())
                });
                if valid {
                    self.emit_copy(&src, dst.x, dst.y, out);
                    stats.copies += 1;
                }
            }
        }
        stats
    }

    /// Row segment `[x0, x1)` of row `y` in `buf`.
    fn segment<'a>(&self, buf: &'a [u8], y: u32, x0: u32, x1: u32) -> &'a [u8] {
        &buf[self.offset(x0, y)..self.offset(x1, y)]
    }

    /// Finds content that moved vertically inside `rect` (relative to what
    /// the client shows) and emits `copy` instructions for it.
    fn detect_scroll(&mut self, rect: &Rect, out: &mut String) -> FlushStats {
        let (x0, x1) = (rect.x, rect.right());

        // Index the client's rows around the region by hash.
        let lo = rect.y.saturating_sub(rect.h);
        let hi = (rect.bottom() + rect.h).min(self.height);
        let mut index: HashMap<u64, Vec<u32>> = HashMap::new();
        for y in lo..hi {
            let row = self.segment(&self.sent, y, x0, x1);
            if !is_uniform(row) {
                let list = index.entry(row_hash(row)).or_default();
                if list.len() < 4 {
                    list.push(y);
                }
            }
        }

        // Every changed, non-blank row votes for the shift that explains it.
        let mut votes: HashMap<i64, u32> = HashMap::new();
        for y in rect.y..rect.bottom() {
            let new = self.segment(&self.pixels, y, x0, x1);
            if new == self.segment(&self.sent, y, x0, x1) || is_uniform(new) {
                continue;
            }
            for &oy in index.get(&row_hash(new)).into_iter().flatten() {
                if oy != y && self.segment(&self.sent, oy, x0, x1) == new {
                    *votes.entry(oy as i64 - y as i64).or_default() += 1;
                }
            }
        }
        let Some((&dy, &count)) = votes.iter().max_by_key(|(dy, n)| (**n, -dy.abs())) else {
            return FlushStats::default();
        };
        if count < MIN_SCROLL_ROWS {
            return FlushStats::default();
        }

        // Rows of `rect` whose new content equals the client row `dy` away.
        let height = self.height;
        let matches = |y: u32| {
            let oy = y as i64 + dy;
            oy >= 0
                && (oy as u32) < height
                && self.segment(&self.sent, oy as u32, x0, x1)
                    == self.segment(&self.pixels, y, x0, x1)
        };
        let mut copies: Vec<(u32, u32)> = runs(rect.h as usize, 0, |i| matches(rect.y + i as u32))
            .into_iter()
            .filter(|(a, b)| b - a >= MIN_SCROLL_ROWS as usize)
            .map(|(a, b)| (rect.y + a as u32, rect.y + b as u32))
            .collect();
        // Copies must not read rows an earlier copy already overwrote.
        if dy < 0 {
            copies.reverse();
        }

        let mut stats = FlushStats::default();
        for (start, end) in copies {
            let src = Rect::new(x0, (start as i64 + dy) as u32, x1 - x0, end - start);
            // Only opaque sources replace the destination under source-over.
            let opaque = (src.y..src.bottom()).all(|y| {
                self.segment(&self.sent, y, x0, x1)
                    .chunks_exact(4)
                    .all(|p| p[3] == 255)
            });
            if opaque {
                self.emit_copy(&src, x0, start, out);
                stats.copies += 1;
            }
        }
        stats
    }

    /// Rectangles covering the differences inside `rect`: tiles without
    /// differences are skipped (e.g. the interior of a window that arrived
    /// by copy), adjacent changed tiles are merged, and each result is cut
    /// further into changed row bands and column runs.
    fn changed_rects(&self, rect: &Rect) -> Vec<Rect> {
        // (first tile column, end tile column, bounding box) per open run.
        let mut open: Vec<(u32, u32, Rect)> = Vec::new();
        let mut merged = Vec::new();
        let (cols, rows) = self.tiles_of(rect);
        let union = |a: Rect, b: Rect| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Rect::new(
                x,
                y,
                a.right().max(b.right()) - x,
                a.bottom().max(b.bottom()) - y,
            )
        };
        for ty in rows {
            let mut runs_here: Vec<(u32, u32, Rect)> = Vec::new();
            for tx in cols.clone() {
                let tile =
                    Rect::new(tx * TILE, ty * TILE, TILE, TILE).clip(self.width, self.height);
                let x = tile.x.max(rect.x);
                let y = tile.y.max(rect.y);
                let tile = Rect::new(
                    x,
                    y,
                    tile.right().min(rect.right()).saturating_sub(x),
                    tile.bottom().min(rect.bottom()).saturating_sub(y),
                );
                let Some(bounds) = (!tile.is_empty())
                    .then(|| self.diff_bounds(&tile))
                    .flatten()
                else {
                    continue;
                };
                match runs_here.last_mut() {
                    Some(run) if run.1 == tx => {
                        run.1 = tx + 1;
                        run.2 = union(run.2, bounds);
                    }
                    _ => runs_here.push((tx, tx + 1, bounds)),
                }
            }
            let mut next_open = Vec::new();
            for run in runs_here {
                match open.iter().position(|o| o.0 == run.0 && o.1 == run.1) {
                    Some(pos) => {
                        let o = open.swap_remove(pos);
                        next_open.push((o.0, o.1, union(o.2, run.2)));
                    }
                    None => next_open.push(run),
                }
            }
            merged.extend(open.drain(..).map(|o| o.2));
            open = next_open;
        }
        merged.extend(open.into_iter().map(|o| o.2));
        merged.iter().flat_map(|r| self.split_bands(r)).collect()
    }

    /// Splits the differences inside `rect` into bands of changed rows and,
    /// within each band, runs of changed columns.
    fn split_bands(&self, rect: &Rect) -> Vec<Rect> {
        let rows: Vec<bool> = (rect.y..rect.bottom())
            .map(|y| self.row_diff(y, rect.x, rect.right()).is_some())
            .collect();
        let mut rects = Vec::new();
        for (b0, b1) in runs(rows.len(), ROW_GAP_MERGE as usize, |i| rows[i]) {
            let (y0, y1) = (rect.y + b0 as u32, rect.y + b1 as u32);
            // Columns that changed anywhere in the band.
            let w = rect.w as usize;
            let mut changed = vec![false; w];
            for y in y0..y1 {
                let a = self.segment(&self.pixels, y, rect.x, rect.right());
                let b = self.segment(&self.sent, y, rect.x, rect.right());
                if a == b {
                    continue;
                }
                for (c, (p, q)) in a.chunks_exact(4).zip(b.chunks_exact(4)).enumerate() {
                    changed[c] |= p != q;
                }
            }
            for (c0, c1) in runs(w, COLUMN_GAP_MERGE as usize, |c| changed[c]) {
                rects.push(Rect::new(rect.x + c0 as u32, y0, (c1 - c0) as u32, y1 - y0));
            }
        }
        rects
    }

    /// Re-sends lossy areas losslessly once they stopped changing.
    fn refine_lossy(&mut self, out: &mut String) -> FlushStats {
        let mut stats = FlushStats::default();
        if self.lossless {
            return stats;
        }
        for ty in 0..self.tile_rows {
            for tx in 0..self.tile_cols {
                let i = (ty * self.tile_cols + tx) as usize;
                if !self.lossy[i] || self.heat[i] > COOL || self.tile_in_video(tx, ty) {
                    continue;
                }
                self.lossy[i] = false;
                let tile =
                    Rect::new(tx * TILE, ty * TILE, TILE, TILE).clip(self.width, self.height);
                let data = self.extract(&self.pixels, &tile);
                let region = Region {
                    width: tile.w,
                    height: tile.h,
                    rgba: &data,
                };
                if let Some(bytes) = self.encode_image(ImageKind::Png, &region) {
                    stats.images += 1;
                    stats.bytes += bytes.len();
                    stats.pixels += tile.area();
                    self.emit_image(
                        0,
                        MODE_OVER,
                        "image/png",
                        tile.x as i32,
                        tile.y as i32,
                        &bytes,
                        out,
                    );
                }
            }
        }
        stats
    }

    fn extract(&self, buf: &[u8], rect: &Rect) -> Vec<u8> {
        let mut data = Vec::with_capacity(rect.area() as usize * 4);
        for y in rect.y..rect.bottom() {
            data.extend_from_slice(self.segment(buf, y, rect.x, rect.right()));
        }
        data
    }

    /// Sends `rect` of the framebuffer and records it in the mirror.
    fn emit_rect(&mut self, rect: Rect, out: &mut String) -> FlushStats {
        let mut stats = FlushStats::default();
        let rect = rect.clip(self.width, self.height);
        if rect.is_empty() {
            return stats;
        }
        let data = self.extract(&self.pixels, &rect);
        let region = Region {
            width: rect.w,
            height: rect.h,
            rgba: &data,
        };
        stats.pixels = rect.area();
        let opaque = data.chunks_exact(4).all(|p| p[3] == 255);
        if !opaque {
            // Source-over would blend with what is there: clear first.
            self.emit_fill(&rect, [0, 0, 0, 255], MODE_ROUT, out);
        }

        if let Some(color) = encode::solid_color(&region)
            && color[3] == 255
        {
            self.emit_fill(&rect, color, MODE_OVER, out);
            stats.fills += 1;
            self.store(&rect, &data, false);
            return stats;
        }

        // JPEG only for large, busy, colourful, opaque areas.
        let lossy = !self.lossless
            && self.formats.jpeg
            && opaque
            && rect.area() >= 128 * 128
            && self.average_heat(&rect) >= HOT
            && encode::color_complexity(&region) > 256;
        let encoded = if lossy {
            self.encode_image(ImageKind::Jpeg, &region)
                .map(|b| ("image/jpeg", b, true))
        } else {
            None
        };
        let Some((mimetype, bytes, is_lossy)) = encoded.or_else(|| {
            self.encode_image(ImageKind::Png, &region)
                .map(|b| ("image/png", b, false))
        }) else {
            return stats;
        };
        stats.images += 1;
        stats.bytes += bytes.len();
        self.emit_image(
            0,
            MODE_OVER,
            mimetype,
            rect.x as i32,
            rect.y as i32,
            &bytes,
            out,
        );
        self.store(&rect, &data, is_lossy);
        stats
    }

    /// Records that the client now shows `data` in `rect`.
    fn store(&mut self, rect: &Rect, data: &[u8], lossy: bool) {
        let row = rect.w as usize * 4;
        for (i, y) in (rect.y..rect.bottom()).enumerate() {
            let start = self.offset(rect.x, y);
            self.sent[start..start + row].copy_from_slice(&data[i * row..(i + 1) * row]);
        }
        let (cols, rows) = self.tiles_of(rect);
        for ty in rows {
            for tx in cols.clone() {
                let i = (ty * self.tile_cols + tx) as usize;
                // A lossless update only clears the flag of fully covered tiles.
                let tile =
                    Rect::new(tx * TILE, ty * TILE, TILE, TILE).clip(self.width, self.height);
                let covered = rect.x <= tile.x
                    && rect.y <= tile.y
                    && rect.right() >= tile.right()
                    && rect.bottom() >= tile.bottom();
                if lossy {
                    self.lossy[i] = true;
                } else if covered {
                    self.lossy[i] = false;
                }
            }
        }
    }

    fn emit_fill(&mut self, rect: &Rect, color: [u8; 4], mode: u8, out: &mut String) {
        *out += &InstructionBuilder::new("rect")
            .arg(0)
            .arg(rect.x)
            .arg(rect.y)
            .arg(rect.w)
            .arg(rect.h)
            .finish();
        *out += &InstructionBuilder::new("cfill")
            .arg(mode)
            .arg(0)
            .arg(color[0])
            .arg(color[1])
            .arg(color[2])
            .arg(color[3])
            .finish();
        if mode == MODE_ROUT {
            fill_rect(&mut self.sent, self.width, rect, [0, 0, 0, 0]);
        }
    }

    /// Emits a layer-0 copy and replays it on the mirror.
    fn emit_copy(&mut self, src: &Rect, dx: u32, dy: u32, out: &mut String) {
        *out += &InstructionBuilder::new("copy")
            .arg(0)
            .arg(src.x)
            .arg(src.y)
            .arg(src.w)
            .arg(src.h)
            .arg(MODE_OVER)
            .arg(0)
            .arg(dx)
            .arg(dy)
            .finish();
        copy_rect_over(&mut self.sent, self.width, src, dx, dy);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_image(
        &mut self,
        layer: i32,
        mode: u8,
        mimetype: &str,
        x: i32,
        y: i32,
        bytes: &[u8],
        out: &mut String,
    ) {
        let stream = self.next_stream;
        self.next_stream = (self.next_stream + 1) % 64;
        *out += &InstructionBuilder::new("img")
            .arg(stream)
            .arg(mode)
            .arg(layer)
            .str_arg(mimetype)
            .arg(x)
            .arg(y)
            .finish();
        let b64 = base64::engine::general_purpose::STANDARD;
        for chunk in bytes.chunks(BLOB_MAX_BYTES) {
            *out += &InstructionBuilder::new("blob")
                .arg(stream)
                .str_arg(&b64.encode(chunk))
                .finish();
        }
        *out += &InstructionBuilder::new("end").arg(stream).finish();
    }

    // ------------------------------------------------------------------
    // Cursor and frames
    // ------------------------------------------------------------------

    /// Uploads a cursor image into buffer -1 and selects it.
    pub fn set_cursor(
        &mut self,
        hotspot_x: u32,
        hotspot_y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
        out: &mut String,
    ) {
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return;
        }
        let pixels = &rgba[..(width * height * 4) as usize];
        // Some servers (x11vnc) resend an unchanged cursor with every update.
        if let Some((hx, hy, w, h, last)) = &self.last_cursor
            && (*hx, *hy, *w, *h) == (hotspot_x, hotspot_y, width, height)
            && last == pixels
        {
            return;
        }
        self.last_cursor = Some((hotspot_x, hotspot_y, width, height, pixels.to_vec()));
        let region = Region {
            width,
            height,
            rgba: pixels,
        };
        let Ok(png_bytes) = encode::encode_png(&region) else {
            return;
        };
        *out += &InstructionBuilder::new("size")
            .arg(-1)
            .arg(width)
            .arg(height)
            .finish();
        self.emit_image(-1, MODE_SRC, "image/png", 0, 0, &png_bytes, out);
        *out += &InstructionBuilder::new("cursor")
            .arg(hotspot_x)
            .arg(hotspot_y)
            .arg(-1)
            .arg(0)
            .arg(0)
            .arg(width)
            .arg(height)
            .finish();
    }

    /// Ends a frame.
    pub fn sync(&mut self, out: &mut String) {
        let mut ts = now_ms();
        if let Some(last) = self.last_sync
            && ts <= last
        {
            ts = last + 1;
        }
        let now = Instant::now();
        self.last_sync = Some(ts);
        self.pending_syncs.push_back((ts, now));
        self.last_frame = Some(now);
        self.last_frame_bytes = std::mem::take(&mut self.frame_bytes);
        self.metrics
            .frame(self.last_frame_bytes, self.pending_syncs.len());
        *out += &InstructionBuilder::new("sync").arg(ts).arg(1).finish();
    }

    /// Handles a `sync` reply from the client.
    pub fn ack(&mut self, timestamp: i64) {
        while let Some(&(ts, sent_at)) = self.pending_syncs.front() {
            if ts > timestamp {
                break;
            }
            self.pending_syncs.pop_front();
            if ts == timestamp {
                let elapsed = sent_at.elapsed();
                self.metrics.acked(elapsed);
                let sample = elapsed.min(MAX_LAG_COMPENSATION);
                self.lag = (self.lag * 3 + sample) / 4;
            }
        }
    }

    /// Frames sent but not yet acknowledged by the client.
    pub fn unacked_frames(&self) -> u32 {
        self.pending_syncs.len() as u32
    }

    /// Smoothed time between sending a frame and the client acknowledging it.
    pub fn processing_lag(&self) -> Duration {
        self.lag
    }

    /// When a frame should be flushed even if nothing changes: to end an
    /// idle video, or to refine lossy areas once they cooled down.
    pub fn idle_deadline(&self) -> Option<Instant> {
        let hottest = (0..self.tile_rows)
            .flat_map(|ty| (0..self.tile_cols).map(move |tx| (tx, ty)))
            .filter(|&(tx, ty)| {
                self.lossy[(ty * self.tile_cols + tx) as usize] && !self.tile_in_video(tx, ty)
            })
            .map(|(tx, ty)| self.heat[(ty * self.tile_cols + tx) as usize])
            .reduce(f32::max);
        let refine = (!self.lossless).then_some(hottest).flatten().map(|heat| {
            let seconds = (heat / COOL).log2().max(0.0) + 0.05;
            self.last_decay + Duration::from_secs_f32(seconds)
        });
        let due = [self.video_deadline(), refine]
            .into_iter()
            .flatten()
            .min()?;
        // Never spin: at most a few housekeeping flushes per second.
        Some(due.max(self.last_flush + Duration::from_millis(200)))
    }

    /// When the next frame should be sent. Changes arriving earlier are
    /// accumulated into that frame, which saves bandwidth on busy screens
    /// (intermediate states are never sent) and lets slow clients catch up.
    /// After a quiet period this is in the past, so input stays responsive.
    pub fn next_frame_at(&self) -> Instant {
        let mut interval = self.lag.clamp(MIN_FRAME_INTERVAL, MAX_LAG_COMPENSATION);
        if self.last_frame_bytes > LARGE_FRAME_BYTES {
            interval = interval.max(LARGE_FRAME_INTERVAL);
        }
        match self.last_frame {
            Some(last) => last + interval,
            None => Instant::now(),
        }
    }
}

/// Merges `TILE`-aligned blocks (sorted by row, then column) into rectangles:
/// horizontal runs, extended downwards while the next row has the same run.
fn merge_tiles(tiles: &[(u32, u32)]) -> Vec<Rect> {
    let mut row_runs: Vec<Rect> = Vec::new();
    for &(x, y) in tiles {
        match row_runs.last_mut() {
            Some(r) if r.y == y && r.right() == x => r.w += TILE,
            _ => row_runs.push(Rect::new(x, y, TILE, TILE)),
        }
    }
    let mut merged: Vec<Rect> = Vec::new();
    for run in row_runs {
        match merged
            .iter_mut()
            .find(|m| m.x == run.x && m.w == run.w && m.bottom() == run.y)
        {
            Some(m) => m.h += TILE,
            None => merged.push(run),
        }
    }
    merged
}

fn fill_rect(buf: &mut [u8], width: u32, rect: &Rect, color: [u8; 4]) {
    let stride = width as usize * 4;
    for y in rect.y..rect.bottom() {
        let start = y as usize * stride + rect.x as usize * 4;
        for px in buf[start..start + rect.w as usize * 4].chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
    }
}

/// Plain copy within one buffer (rows walked in the overlap-safe order).
fn copy_rect(buf: &mut [u8], width: u32, src: &Rect, dx: u32, dy: u32) {
    let stride = width as usize * 4;
    let row = src.w as usize * 4;
    let rows: Box<dyn Iterator<Item = u32>> = if dy > src.y {
        Box::new((0..src.h).rev())
    } else {
        Box::new(0..src.h)
    };
    for r in rows {
        let s = (src.y + r) as usize * stride + src.x as usize * 4;
        let d = (dy + r) as usize * stride + dx as usize * 4;
        buf.copy_within(s..s + row, d);
    }
}

/// Copy with source-over semantics, like a client-side `copy` in OVER mode
/// (opaque pixels replace, transparent ones keep the destination).
fn copy_rect_over(buf: &mut [u8], width: u32, src: &Rect, dx: u32, dy: u32) {
    let stride = width as usize * 4;
    let row_len = src.w as usize * 4;
    // Snapshot the source so overlapping copies read the original pixels.
    let mut snapshot = Vec::with_capacity(row_len * src.h as usize);
    for y in src.y..src.bottom() {
        let s = y as usize * stride + src.x as usize * 4;
        snapshot.extend_from_slice(&buf[s..s + row_len]);
    }
    for r in 0..src.h as usize {
        let d = (dy as usize + r) * stride + dx as usize * 4;
        let source = &snapshot[r * row_len..(r + 1) * row_len];
        if source.chunks_exact(4).all(|p| p[3] == 255) {
            buf[d..d + row_len].copy_from_slice(source);
            continue;
        }
        for (dst, s) in buf[d..d + row_len]
            .chunks_exact_mut(4)
            .zip(source.chunks_exact(4))
        {
            let a = s[3] as u32;
            if a == 255 {
                dst.copy_from_slice(s);
            } else if a > 0 {
                for c in 0..3 {
                    dst[c] = ((s[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
                }
                dst[3] = (a + dst[3] as u32 * (255 - a) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests;
