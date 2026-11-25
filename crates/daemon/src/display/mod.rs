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
//! encoded (solid fill, palette/RGB PNG or a zstd image, or JPEG for busy
//! photographic areas, later refined losslessly once the area calms down).
//!
//! Because emitted instructions are replayed on the mirror, every optimisation
//! is self-correcting: a wrong guess simply shows up as a difference in the
//! next comparison and gets sent as pixels.

pub mod encode;
mod metrics;
mod pacing;
mod pool;
mod source;
mod video;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use molcajete_protocol::InstructionBuilder;
use molcajete_protocol::{apply_time, zimage};
use rayon::prelude::*;

use crate::io::Output;

use encode::Region;
use metrics::{DisplayMetrics, ImageKind, Phase, timed};
use pacing::FrameClock;
pub use pacing::{Pacing, SyncReply};
pub use pool::set_threads as set_encode_threads;
pub use source::{ChannelOrder, SourceImage};

/// Granularity of change tracking and of the heat map.
const TILE: u32 = 64;
/// Changed rows separated by fewer unchanged rows share one image.
const ROW_GAP_MERGE: u32 = 6;
/// Changed columns separated by fewer unchanged columns share one image.
const COLUMN_GAP_MERGE: u32 = 48;
/// What one more image is taken to cost, in bytes of image data: its PNG
/// signature, chunk headers, checksums and palette, the `img`, `blob` and
/// `end` instructions around it, base64 padding, and a decode on the client
/// (which dominates: a client is slowed by the number of images of a frame
/// long before their size). Changed rectangles are joined when the unchanged
/// pixels between them are estimated to cost less than this.
const IMAGE_OVERHEAD_BYTES: u64 = 1024;
/// Estimated pixels per encoded byte where a row is one colour or repeats
/// the row above it (blank lines, margins, rules).
const FLAT_PIXELS_PER_BYTE: u64 = 128;
/// Estimated pixels per encoded byte anywhere else (text, pictures).
const BUSY_PIXELS_PER_BYTE: u64 = 8;
/// Rectangles are not joined beyond this many pixels, so that a large
/// update stays several images the client decodes side by side.
const MAX_JOINED_PIXELS: u64 = 1 << 20;
/// Minimum number of rows a scroll copy must cover.
const MIN_SCROLL_ROWS: u32 = 8;
/// Bound on pixel comparisons per frame while searching for moved blocks.
const MAX_VERIFICATIONS: u32 = 4096;
/// Search areas up to this size (per axis) are searched completely; larger
/// ones are sampled every `MOVE_SAMPLE_PERIOD` pixels.
const MOVE_FULL_SEARCH: u32 = 512;
const MOVE_SAMPLE_PERIOD: u32 = 384;
/// Side of a sample: holds one window position for each of the TILE×TILE
/// ways a moved block can be offset against the tile grid.
const MOVE_SAMPLE: u32 = 2 * TILE - 1;
/// Number of distinct displacements tried per search area.
const MAX_MOVE_OFFSETS: usize = 8;
/// Changed regions at most this far apart are searched for moved blocks as
/// one area.
const MOVE_SEARCH_GAP: u32 = 4 * TILE;
/// Multipliers of the 2D rolling block hash (both odd).
const HASH_A: u64 = 0x0000_0100_0000_01b3;
const HASH_B: u64 = 0x9e37_79b9_7f4a_7c15;
/// Heat (≈ updates per second, decaying) above which an area counts as busy.
const HOT: f32 = 3.0;
/// Heat below which lossy areas are re-sent losslessly.
const COOL: f32 = 0.25;

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
    /// The zstd image type (`molcajete_protocol::zimage`), which only
    /// molcajete clients announce.
    pub zstd: bool,
}

/// Whether zstd images are sent to the clients that accept them
/// (`--image-zstd`).
static ZSTD_IMAGES: AtomicBool = AtomicBool::new(true);

/// Turns zstd images on or off for the connections that follow.
pub fn set_zstd_images(enabled: bool) {
    ZSTD_IMAGES.store(enabled, Ordering::Relaxed);
}

impl ImageFormats {
    pub fn from_mimetypes<S: AsRef<str>>(mimetypes: &[S]) -> Self {
        let has = |mimetype: &str| mimetypes.iter().any(|m| m.as_ref() == mimetype);
        Self {
            jpeg: has("image/jpeg"),
            zstd: has(zimage::MIMETYPE) && ZSTD_IMAGES.load(Ordering::Relaxed),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowDiff {
    first: u32,
    last: u32,
}

/// The changed column range of every row of one dirty region, from one
/// comparison of the framebuffer with the client copy per flush. Scroll
/// detection, move detection and the pixel phase all read it; the copies
/// the first two emit update the rows they cover (see
/// [`Display::copied`]), so it always describes the client copy as it is.
struct RegionDiff {
    rect: Rect,
    /// One entry per row of `rect`, top first.
    rows: Vec<Option<RowDiff>>,
}

impl RegionDiff {
    /// Changed columns of row `y`, which must lie inside the region.
    fn row(&self, y: u32) -> Option<RowDiff> {
        self.rows[(y - self.rect.y) as usize]
    }

    /// Bounding box of the differences.
    fn bounds(&self) -> Option<Rect> {
        let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
        for (y, d) in (self.rect.y..).zip(&self.rows) {
            if let Some(d) = d {
                x0 = x0.min(d.first);
                x1 = x1.max(d.last);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        (y0 != u32::MAX).then(|| Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }
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
    /// Whether zstd images are sent (the client accepts them). Every
    /// encoding thread has its own encoder ([`pool::with_zstd`]).
    zstd: bool,
    /// Where the images of a frame are encoded side by side, or `None` to
    /// encode them one after another on the session's thread.
    pool: Option<&'static rayon::ThreadPool>,
    lossless: bool,
    next_stream: i32,
    last_sync: Option<i64>,
    /// Frames sent, their acknowledgements, and when the next may go out.
    clock: FrameClock,
    /// Encoded image bytes of the frame being built.
    frame_bytes: usize,
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

/// FNV-1a over 64-bit words in four independent lanes (so the multiplies of
/// consecutive words do not wait for each other): fast and good enough to
/// find candidate rows; matches are always confirmed by comparing the bytes.
fn row_hash(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let word = |w: &[u8]| u64::from_le_bytes(w.try_into().expect("8-byte chunk"));
    let mut lanes = [SEED, SEED ^ 1, SEED ^ 2, SEED ^ 3];
    let mut blocks = bytes.chunks_exact(32);
    for block in &mut blocks {
        for (lane, w) in lanes.iter_mut().zip(block.chunks_exact(8)) {
            *lane = (*lane ^ word(w)).wrapping_mul(PRIME);
        }
    }
    let mut h = lanes
        .into_iter()
        .fold(SEED, |h, lane| (h ^ lane).wrapping_mul(PRIME));
    let mut words = blocks.remainder().chunks_exact(8);
    for w in &mut words {
        h = (h ^ word(w)).wrapping_mul(PRIME);
    }
    for &b in words.remainder() {
        h = (h ^ b as u64).wrapping_mul(PRIME);
    }
    h
}

/// Hash standing for a row when rows are matched against each other, or
/// `None` for rows that say nothing about where content moved (a single
/// colour). Wide rows are judged by evenly spaced samples: reading every
/// byte of every row costs more than the rest of scroll detection, and rows
/// that only look alike in the samples are sorted out by the comparison that
/// follows every match.
fn row_signature(row: &[u8]) -> Option<u64> {
    const SAMPLES: usize = 16;
    const SAMPLE: usize = 64 * 4;
    if row.len() <= 2 * SAMPLES * SAMPLE {
        return (!is_uniform(row)).then(|| row_hash(row));
    }
    let step = (row.len() - SAMPLE) / (SAMPLES - 1) / 4 * 4;
    let sample = |i: usize| &row[i * step..i * step + SAMPLE];
    let first = &row[..4];
    if (0..SAMPLES).all(|i| is_uniform_with(sample(i), first)) {
        return None;
    }
    Some((0..SAMPLES).fold(0, |h, i| {
        (h ^ row_hash(sample(i))).wrapping_mul(0x0100_0000_01b3)
    }))
}

fn pixel_value(p: &[u8; 4]) -> u64 {
    u32::from_le_bytes(*p) as u64
}

fn pow(base: u64, exp: u32) -> u64 {
    (0..exp).fold(1u64, |acc, _| acc.wrapping_mul(base))
}

/// Multiplicative inverse of an odd number modulo 2^64 (Newton's method:
/// every step doubles the number of correct low bits, starting from 3).
fn inverse(a: u64) -> u64 {
    (0..5).fold(a, |x, _| {
        x.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(x)))
    })
}

/// `HASH_A^(TILE-1-column)` for every column of a block: the weight of a
/// pixel within the hash of its row.
fn row_weights() -> [u64; TILE as usize] {
    let mut weights = [1u64; TILE as usize];
    for i in (0..TILE as usize - 1).rev() {
        weights[i] = weights[i + 1].wrapping_mul(HASH_A);
    }
    weights
}

/// Whether every pixel of `row` equals `first`. A row equal to itself
/// shifted by one pixel has a single colour, which is one `memcmp`.
fn is_uniform_with(row: &[u8], first: &[u8]) -> bool {
    row.len() < 4 || (row[..4] == *first && row[4..] == row[..row.len() - 4])
}

fn is_uniform(row: &[u8]) -> bool {
    row.len() < 4 || row[4..] == row[..row.len() - 4]
}

/// Index of the first pixel that differs between two rows of equal length.
/// Whole blocks are compared first (one `memcmp` each), since long equal
/// stretches are the common case.
fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    const BLOCK: usize = 64;
    let mut start = 0;
    for (x, y) in a.chunks(BLOCK).zip(b.chunks(BLOCK)) {
        if x != y {
            let byte = x.iter().zip(y).position(|(p, q)| p != q)?;
            return Some((start + byte) / 4);
        }
        start += x.len();
    }
    None
}

/// Index of the last pixel that differs between two rows of equal length.
fn last_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    const BLOCK: usize = 64;
    let mut end = a.len();
    for (x, y) in a.rchunks(BLOCK).zip(b.rchunks(BLOCK)) {
        end -= x.len();
        if x != y {
            let byte = x.iter().zip(y).rposition(|(p, q)| p != q)?;
            return Some((end + byte) / 4);
        }
    }
    None
}

/// Whether no pixel of `rgba` is transparent. (Without an early exit, so
/// that the loop is vectorised: transparency is rare.)
pub(crate) fn is_opaque(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .fold(u8::MAX, |alpha, p| alpha & p[3])
        == u8::MAX
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        a.right().max(b.right()) - x,
        a.bottom().max(b.bottom()) - y,
    )
}

/// Joins rectangles closer than `gap` (in both directions) into their
/// bounding boxes, until the results are further apart than that. Returns
/// the boxes and, for every input rectangle, the index of its box.
fn cluster(rects: &[Rect], gap: u32) -> (Vec<Rect>, Vec<usize>) {
    let near = |a: &Rect, b: &Rect| {
        a.x < b.right() + gap
            && b.x < a.right() + gap
            && a.y < b.bottom() + gap
            && b.y < a.bottom() + gap
    };
    let mut boxes: Vec<Rect> = Vec::new();
    let mut owner: Vec<usize> = Vec::with_capacity(rects.len());
    for rect in rects {
        let mut merged = *rect;
        // Growing a box may bring further boxes within reach.
        let mut target = None;
        while let Some(hit) = (0..boxes.len())
            .find(|&i| Some(i) != target && !boxes[i].is_empty() && near(&boxes[i], &merged))
        {
            merged = union(merged, boxes[hit]);
            match target {
                Some(t) => {
                    // Fold `hit` into the box we already joined.
                    boxes[hit] = Rect::default();
                    owner.iter_mut().filter(|o| **o == hit).for_each(|o| *o = t);
                }
                None => target = Some(hit),
            }
        }
        let index = target.unwrap_or_else(|| {
            boxes.push(merged);
            boxes.len() - 1
        });
        boxes[index] = merged;
        owner.push(index);
    }
    (boxes, owner)
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

/// Looks up the changed blocks of the new frame by hash.
struct BlockIndex {
    /// Open-addressed table: hash -> index of the block.
    table: Vec<Option<(u64, u32)>>,
    mask: usize,
    /// One bit per hash: rejects nearly every window position without
    /// touching the table.
    filter: Vec<u64>,
    filter_shift: u32,
}

impl BlockIndex {
    fn new(wanted: &[(u32, u32, u64)]) -> Self {
        let size = (wanted.len() * 4).next_power_of_two();
        let filter_bits = (wanted.len() * 32).next_power_of_two().max(1024);
        let mut index = Self {
            table: vec![None; size],
            mask: size - 1,
            filter: vec![0u64; filter_bits / 64],
            // The high bits of the hash are its best mixed ones.
            filter_shift: 64 - filter_bits.trailing_zeros(),
        };
        for (i, &(_, _, h)) in wanted.iter().enumerate() {
            let mut slot = index.home(h);
            while index.table[slot].is_some_and(|(stored, _)| stored != h) {
                slot = (slot + 1) & index.mask;
            }
            // Identical blocks share one entry; the first one wins.
            index.table[slot].get_or_insert((h, i as u32));
            let bit = (h >> index.filter_shift) as usize;
            index.filter[bit / 64] |= 1 << (bit % 64);
        }
        index
    }

    fn home(&self, h: u64) -> usize {
        (h ^ (h >> 29)) as usize & self.mask
    }

    fn get(&self, h: u64) -> Option<usize> {
        let bit = (h >> self.filter_shift) as usize;
        if self.filter[bit / 64] & (1 << (bit % 64)) == 0 {
            return None;
        }
        let mut slot = self.home(h);
        loop {
            match self.table[slot] {
                Some((stored, idx)) if stored == h => return Some(idx as usize),
                Some(_) => slot = (slot + 1) & self.mask,
                None => return None,
            }
        }
    }
}

/// State of the search for the old places of the changed blocks of one area
/// (see [`Display::find_blocks`]).
struct BlockSearch {
    index: BlockIndex,
    /// Blocks whose old place is still unknown.
    missing: Vec<bool>,
    /// (source, destination) of the blocks found.
    found: Vec<((u32, u32), (u32, u32))>,
    /// Powers of `HASH_A` and of its inverse, by column.
    up: Vec<u64>,
    down: Vec<u64>,
    /// Scratch buffers of the sliding window.
    prefix: Vec<u64>,
    ring: Vec<u64>,
    columns: Vec<u64>,
}

impl BlockSearch {
    fn new(wanted: &[(u32, u32, u64)]) -> Self {
        Self {
            index: BlockIndex::new(wanted),
            missing: vec![true; wanted.len()],
            found: Vec::new(),
            up: Vec::new(),
            down: Vec::new(),
            prefix: Vec::new(),
            ring: Vec::new(),
            columns: Vec::new(),
        }
    }

    /// Makes room for searching an area `width` pixels wide.
    fn reserve(&mut self, width: usize) {
        if self.up.len() >= width {
            return;
        }
        let a_inverse = inverse(HASH_A);
        let (mut u, mut d) = (1u64, 1u64);
        self.up.clear();
        self.down.clear();
        for _ in 0..width {
            self.up.push(u);
            self.down.push(d);
            u = u.wrapping_mul(HASH_A);
            d = d.wrapping_mul(a_inverse);
        }
        self.prefix = vec![0; width + TILE as usize];
        self.ring = vec![0; width * TILE as usize];
        self.columns = vec![0; width];
    }
}

/// Spans of `[start, start + len)` searched for moved blocks: all of it when
/// short, otherwise samples at regular distances.
fn sample_spans(start: u32, len: u32) -> Vec<(u32, u32)> {
    if len <= MOVE_FULL_SEARCH {
        return vec![(start, len)];
    }
    (0..len - TILE)
        .step_by(MOVE_SAMPLE_PERIOD as usize)
        .map(|offset| (start + offset, MOVE_SAMPLE.min(len - offset)))
        .collect()
}

/// The parts of `area` searched for moved blocks.
fn samples(area: &Rect) -> Vec<Rect> {
    let columns = sample_spans(area.x, area.w);
    sample_spans(area.y, area.h)
        .into_iter()
        .flat_map(|(y, h)| columns.iter().map(move |&(x, w)| Rect::new(x, y, w, h)))
        .collect()
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
            zstd: formats.zstd,
            pool: pool::shared(),
            lossless,
            next_stream: 0,
            last_sync: None,
            clock: FrameClock::new(Pacing::TERMINAL),
            frame_bytes: 0,
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

    /// Chooses how fast frames may follow each other. The default is
    /// [`Pacing::TERMINAL`], the more careful of the two.
    pub fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.clock.set_pacing(pacing);
        self
    }

    /// Records the images encoded for one rectangle of `pixels` pixels.
    fn record_images(&mut self, encoded: &[EncodedImage], pixels: u64) {
        for &(kind, elapsed, bytes) in encoded {
            self.metrics.image(kind, elapsed, bytes, pixels);
        }
    }

    /// Runs `f` on the encoding pool, or here without one.
    fn on_pool<T: Send>(&self, f: impl FnOnce() -> T + Send) -> T {
        match self.pool {
            Some(pool) => pool.install(f),
            None => f(),
        }
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
    pub fn resize(&mut self, width: u32, height: u32, out: &mut Output) {
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

    /// Writes a `w`×`h` rectangle of `source` into the framebuffer at
    /// (`x`, `y`) as opaque RGBA, converting the channels in the same pass
    /// (one copy, unlike converting into a buffer for [`Self::put`]). The
    /// rectangle is clipped to the framebuffer; nothing is written if
    /// `source` is too short for the part that remains.
    pub fn put_image(&mut self, x: u32, y: u32, w: u32, h: u32, source: &SourceImage<'_>) {
        let rect = Rect::new(x, y, w, h).clip(self.width, self.height);
        if rect.is_empty() || !source.holds(rect.w, rect.h) {
            return;
        }
        let row = rect.w as usize * 4;
        for dy in 0..rect.h as usize {
            let src = dy * source.stride;
            let dst = self.offset(rect.x, rect.y + dy as u32);
            source.channels.convert_row(
                &source.data[src..src + row],
                &mut self.pixels[dst..dst + row],
            );
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
        out: &mut Output,
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
    pub fn flush(&mut self, out: &mut Output) -> FlushStats {
        let started = Instant::now();
        let mut stats = FlushStats::default();
        self.decay();
        self.last_flush = started;
        // Busy areas may be covered by a video stream instead of images.
        stats.add(self.update_video(out));
        let mut mark = Instant::now();
        self.metrics.phase(Phase::Video, mark - started);
        let mut phase_done = |metrics: &mut DisplayMetrics, phase: Phase| {
            let now = Instant::now();
            metrics.phase(phase, now - mark);
            mark = now;
        };
        let regions = self.dirty_regions();
        self.dirty.iter_mut().for_each(|d| *d = false);
        let mut sent_tiles = vec![false; self.dirty.len()];
        // The one full comparison with the client copy in this flush.
        let mut diffs: Vec<RegionDiff> = regions.iter().map(|r| self.region_diff(r)).collect();

        // 1. Vertical scrolling inside each region (terminals, documents).
        for diff in &mut diffs {
            if let Some(changed) = diff.bounds()
                && changed.h >= 2 * MIN_SCROLL_ROWS
                && changed.w >= 16
            {
                let found = self.detect_scroll(&changed, diff, out);
                self.metrics.copies(Phase::Scroll, found.copies);
                stats.add(found);
            }
        }
        phase_done(&mut self.metrics, Phase::Scroll);
        // 2. Blocks that moved anywhere (dragged windows, panning).
        let found = self.detect_moves(&mut diffs, out);
        self.metrics.copies(Phase::Moves, found.copies);
        stats.add(found);
        phase_done(&mut self.metrics, Phase::Moves);
        // 3. Whatever still differs is sent as pixels. (Regions do not
        // overlap, so sending the rectangles of one does not change those
        // of another.)
        let mut rects = Vec::new();
        for diff in &diffs {
            if let Some(changed) = diff.bounds() {
                rects.extend(self.changed_rects(&changed, diff));
            }
        }
        for rect in &rects {
            let (cols, rows) = self.tiles_of(rect);
            for ty in rows {
                for tx in cols.clone() {
                    sent_tiles[(ty * self.tile_cols + tx) as usize] = true;
                }
            }
        }
        stats.add(self.emit_rects(rects, out));
        for (motion, sent) in self.motion.iter_mut().zip(sent_tiles) {
            if sent {
                *motion += 1.0;
            }
        }
        phase_done(&mut self.metrics, Phase::Pixels);
        stats.add(self.refine_lossy(out));
        phase_done(&mut self.metrics, Phase::Refine);
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
        let first = first_diff(a, b)? as u32;
        let last = last_diff(a, b)? as u32;
        Some(RowDiff {
            first: x0 + first,
            last: x0 + last,
        })
    }

    /// Compares every row of `rect` with the client copy.
    fn region_diff(&self, rect: &Rect) -> RegionDiff {
        RegionDiff {
            rect: *rect,
            rows: (rect.y..rect.bottom())
                .map(|y| self.row_diff(y, rect.x, rect.right()))
                .collect(),
        }
    }

    /// Changed column range of the segment `[x0, x1)` of row `y`, given the
    /// changed range `row` of a wider segment around it. Nothing outside
    /// `row` differs, so only the part of the segment inside it is compared
    /// again, and nothing at all when it lies inside the segment.
    fn diff_within(&self, row: Option<RowDiff>, y: u32, x0: u32, x1: u32) -> Option<RowDiff> {
        let row = row?;
        let (a, b) = (x0.max(row.first), x1.min(row.last + 1));
        if a >= b {
            None
        } else if (a, b) == (row.first, row.last + 1) {
            Some(row)
        } else {
            self.row_diff(y, a, b)
        }
    }

    /// Bounding box of the differences inside `rect`, a part of the region
    /// of `diff`.
    fn diff_bounds(&self, diff: &RegionDiff, rect: &Rect) -> Option<Rect> {
        let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
        for y in rect.y..rect.bottom() {
            if let Some(d) = self.diff_within(diff.row(y), y, rect.x, rect.right()) {
                x0 = x0.min(d.first);
                x1 = x1.max(d.last);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        (y0 != u32::MAX).then(|| Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }

    /// Updates `diffs` after a copy to `dst` whose content now equals the
    /// framebuffer there (every copy of scroll and move detection is
    /// checked for that before it is sent). Only the rows it covers can
    /// have changed, and in them only the columns that differed before and
    /// lie outside `dst` can still differ.
    fn copied(&self, diffs: &mut [RegionDiff], dst: &Rect) {
        for diff in diffs {
            let r = diff.rect;
            if dst.x >= r.right()
                || r.x >= dst.right()
                || dst.y >= r.bottom()
                || r.y >= dst.bottom()
            {
                continue;
            }
            for y in dst.y.max(r.y)..dst.bottom().min(r.bottom()) {
                let slot = &mut diff.rows[(y - r.y) as usize];
                *slot = slot.and_then(|d| {
                    if dst.x <= d.first && d.last < dst.right() {
                        None
                    } else {
                        self.row_diff(y, d.first, d.last + 1)
                    }
                });
                debug_assert_eq!(*slot, self.row_diff(y, r.x, r.right()));
            }
        }
    }

    /// Hash of the `TILE`×`TILE` block at (x, y): Σ p·A^(63-col)·B^(63-row)
    /// mod 2^64 with odd A and B, so every pixel contributes. The sliding
    /// search in [`Display::find_blocks`] produces the same value for every
    /// window position. `weights` comes from [`row_weights`]: weighting the
    /// pixels (instead of Horner's rule) keeps the multiplies independent.
    fn block_hash(&self, buf: &[u8], x: u32, y: u32, weights: &[u64; TILE as usize]) -> u64 {
        let mut cell = 0u64;
        for row in y..y + TILE {
            let pixels = self.segment(buf, row, x, x + TILE).as_chunks::<4>().0;
            let acc = pixels.iter().zip(weights).fold(0u64, |acc, (p, w)| {
                acc.wrapping_add(pixel_value(p).wrapping_mul(*w))
            });
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
    ///
    /// Content that moved leaves a change both where it was and where it is
    /// now, so the old place is searched for only around the changes: regions
    /// close to each other form one search area, regions far apart are
    /// searched separately (two small changes at opposite corners must not
    /// cost a scan of the whole screen).
    ///
    /// Large areas are not scanned pixel by pixel either. What moved did so
    /// by one displacement, so it is enough to find that displacement: a
    /// sparse grid of samples is searched, each large enough to catch one
    /// block of anything that moved across it, and the displacements found
    /// are then tried on every changed block. Only things smaller than the
    /// grid spacing can slip through, and those are cheap to send as pixels.
    fn detect_moves(&mut self, diffs: &mut [RegionDiff], out: &mut Output) -> FlushStats {
        let mut stats = FlushStats::default();
        let regions: Vec<Rect> = diffs.iter().map(|d| d.rect).collect();
        let (areas, owner) = cluster(&regions, MOVE_SEARCH_GAP);
        let weights = row_weights();
        // Changed, non-blank, fully visible tiles of the new frame, per area.
        let mut wanted: Vec<Vec<(u32, u32, u64)>> = vec![Vec::new(); areas.len()];
        for (diff, &area) in diffs.iter().zip(&owner) {
            let (cols, rows) = self.tiles_of(&diff.rect);
            for ty in rows {
                for tx in cols.clone() {
                    let (x, y) = (tx * TILE, ty * TILE);
                    if x + TILE > self.width || y + TILE > self.height {
                        continue;
                    }
                    let unchanged = (y..y + TILE)
                        .all(|r| self.diff_within(diff.row(r), r, x, x + TILE).is_none());
                    if unchanged {
                        continue;
                    }
                    let first = self.segment(&self.pixels, y, x, x + 1);
                    let uniform = (y..y + TILE).all(|r| {
                        is_uniform_with(self.segment(&self.pixels, r, x, x + TILE), first)
                    });
                    if !uniform {
                        wanted[area].push((x, y, self.block_hash(&self.pixels, x, y, &weights)));
                    }
                }
            }
        }
        // (source, destination) of every block found.
        let mut found: Vec<((u32, u32), (u32, u32))> = Vec::new();
        let mut verifications = 0u32;
        for (area, wanted) in areas.iter().zip(&wanted) {
            // Searching costs O(area); only worth it for sizeable changes.
            if wanted.len() < 2 || area.w < TILE || area.h < TILE {
                continue;
            }
            let mut search = BlockSearch::new(wanted);
            for sample in samples(area) {
                self.find_blocks(&sample, wanted, &mut search, &mut verifications);
            }
            // Displacements seen, most frequent first.
            let mut offsets: Vec<((i64, i64), u32)> = Vec::new();
            for &((sx, sy), (dx, dy)) in &search.found {
                let offset = (dx as i64 - sx as i64, dy as i64 - sy as i64);
                match offsets.iter_mut().find(|(o, _)| *o == offset) {
                    Some((_, n)) => *n += 1,
                    None => offsets.push((offset, 1)),
                }
            }
            offsets.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
            offsets.truncate(MAX_MOVE_OFFSETS);
            // Every changed block that one of them explains moved too.
            for (i, &(dx, dy, _)) in wanted.iter().enumerate() {
                if !search.missing[i] {
                    continue;
                }
                for &((ox, oy), _) in &offsets {
                    let (sx, sy) = (dx as i64 - ox, dy as i64 - oy);
                    if sx >= 0
                        && sy >= 0
                        && sx as u32 + TILE <= self.width
                        && sy as u32 + TILE <= self.height
                        && self.blocks_equal(&self.sent, sx as u32, sy as u32, &self.pixels, dx, dy)
                    {
                        search.found.push(((sx as u32, sy as u32), (dx, dy)));
                        break;
                    }
                }
            }
            found.append(&mut search.found);
        }

        // Blocks moved by the same offset are merged into larger rectangles,
        // each sent as one copy (the client handles overlap within a copy).
        // (In a fixed order, so that the output does not depend on a hash
        // seed.)
        let mut by_offset: BTreeMap<(i64, i64), Vec<(u32, u32)>> = BTreeMap::new();
        for ((sx, sy), (dx, dy)) in found {
            let offset = (dx as i64 - sx as i64, dy as i64 - sy as i64);
            by_offset.entry(offset).or_default().push((dx, dy));
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
                    s == self.segment(&self.pixels, dst.y + r, dst.x, dst.right()) && is_opaque(s)
                });
                if valid {
                    self.emit_opaque_copy(&src, dst.x, dst.y, out);
                    self.copied(diffs, &dst);
                    stats.copies += 1;
                }
            }
        }
        stats
    }

    /// Slides a `TILE`×`TILE` window over the client's content inside `area`
    /// and records in `search` where the `wanted` blocks (x, y, hash) of the
    /// new frame are found.
    fn find_blocks(
        &self,
        area: &Rect,
        wanted: &[(u32, u32, u64)],
        search: &mut BlockSearch,
        verifications: &mut u32,
    ) {
        const T: usize = TILE as usize;
        let b_out = pow(HASH_B, TILE);
        let width = area.w as usize;
        // The hash of the TILE pixels ending at column i of a row is
        // A^i · (S(i) − S(i − TILE)) with S(i) = Σ p(j)·A^(−j) for j ≤ i:
        // a prefix sum, where a rolling hash would chain one multiply per
        // pixel.
        search.reserve(width);
        let BlockSearch {
            up,
            down,
            prefix,
            ring,
            columns,
            ..
        } = search;
        let (up, down) = (&up[..width], &down[..width]);
        // S, shifted by TILE so that S(i − TILE) is zero for the first columns.
        let prefix = &mut prefix[..width + T];
        // Row hashes of the last TILE rows (ring buffer, initially zero so
        // that leaving rows subtract nothing) and running column sums.
        let ring = &mut ring[..width * T];
        let columns = &mut columns[..width];
        ring.fill(0);
        columns.fill(0);
        for y in area.y..area.bottom() {
            let row = self
                .segment(&self.sent, y, area.x, area.right())
                .as_chunks::<4>()
                .0;
            let mut sum = 0u64;
            for ((slot, p), &d) in prefix[T..].iter_mut().zip(row).zip(down) {
                sum = sum.wrapping_add(pixel_value(p).wrapping_mul(d));
                *slot = sum;
            }
            // Hash of the TILE rows ending at this one, per column.
            let slot = ((y - area.y) as usize % T) * width;
            for ((((column, leaving), &high), &low), &u) in columns
                .iter_mut()
                .zip(&mut ring[slot..slot + width])
                .zip(&prefix[T..])
                .zip(&prefix[..width])
                .zip(up)
            {
                let hash = high.wrapping_sub(low).wrapping_mul(u);
                *column = column
                    .wrapping_mul(HASH_B)
                    .wrapping_add(hash)
                    .wrapping_sub(leaving.wrapping_mul(b_out));
                *leaving = hash;
            }
            if y + 1 < area.y + TILE {
                continue;
            }
            for (i, &h) in columns.iter().enumerate().skip(T - 1) {
                let Some(idx) = search.index.get(h) else {
                    continue;
                };
                if !search.missing[idx] {
                    continue;
                }
                if *verifications >= MAX_VERIFICATIONS {
                    return;
                }
                let sx = area.x + (i + 1 - T) as u32;
                let sy = y + 1 - TILE;
                let (dx, dy, _) = wanted[idx];
                *verifications += 1;
                if (sx, sy) != (dx, dy)
                    && self.blocks_equal(&self.sent, sx, sy, &self.pixels, dx, dy)
                {
                    search.missing[idx] = false;
                    search.found.push(((sx, sy), (dx, dy)));
                }
            }
        }
    }

    /// Row segment `[x0, x1)` of row `y` in `buf`.
    fn segment<'a>(&self, buf: &'a [u8], y: u32, x0: u32, x1: u32) -> &'a [u8] {
        &buf[self.offset(x0, y)..self.offset(x1, y)]
    }

    /// Finds content that moved vertically inside `rect`, a part of the
    /// region of `diff` (relative to what the client shows), and emits
    /// `copy` instructions for it.
    fn detect_scroll(
        &mut self,
        rect: &Rect,
        diff: &mut RegionDiff,
        out: &mut Output,
    ) -> FlushStats {
        let (x0, x1) = (rect.x, rect.right());

        // Index the client's rows around the region by hash.
        let lo = rect.y.saturating_sub(rect.h);
        let hi = (rect.bottom() + rect.h).min(self.height);
        let mut index: Vec<(u64, u32)> = Vec::with_capacity((hi - lo) as usize);
        for y in lo..hi {
            if let Some(hash) = row_signature(self.segment(&self.sent, y, x0, x1)) {
                index.push((hash, y));
            }
        }
        index.sort_unstable();

        // Every changed, non-blank row votes for the shift that explains it.
        let mut votes: HashMap<i64, u32> = HashMap::new();
        // (row, shift) of the votes, whose rows need not be compared again.
        let mut verified: Vec<(u32, i64)> = Vec::new();
        for y in rect.y..rect.bottom() {
            if diff.row(y).is_none() {
                continue;
            }
            let new = self.segment(&self.pixels, y, x0, x1);
            let Some(hash) = row_signature(new) else {
                continue;
            };
            let first = index.partition_point(|&(h, _)| h < hash);
            // Repeated rows (rulers, borders) match everywhere: a few suffice.
            let same = index[first..].iter().take_while(|&&(h, _)| h == hash);
            for &(_, oy) in same.take(4) {
                if oy != y && self.segment(&self.sent, oy, x0, x1) == new {
                    let shift = oy as i64 - y as i64;
                    *votes.entry(shift).or_default() += 1;
                    verified.push((y, shift));
                }
            }
        }
        // Ties go to the smaller shift, and then to the downward one, so
        // that the result does not depend on the order of the map.
        let Some((&dy, &count)) = votes.iter().max_by_key(|&(&dy, &n)| (n, -dy.abs(), dy)) else {
            return FlushStats::default();
        };
        if count < MIN_SCROLL_ROWS {
            return FlushStats::default();
        }

        // Rows of `rect` whose new content equals the client row `dy` away.
        let mut known = vec![false; rect.h as usize];
        for &(y, shift) in &verified {
            if shift == dy {
                known[(y - rect.y) as usize] = true;
            }
        }
        let height = self.height;
        let matches = |y: u32| {
            let oy = y as i64 + dy;
            known[(y - rect.y) as usize]
                || (oy >= 0
                    && (oy as u32) < height
                    && self.segment(&self.sent, oy as u32, x0, x1)
                        == self.segment(&self.pixels, y, x0, x1))
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
            let opaque =
                (src.y..src.bottom()).all(|y| is_opaque(self.segment(&self.sent, y, x0, x1)));
            if opaque {
                self.emit_opaque_copy(&src, x0, start, out);
                self.copied(
                    std::slice::from_mut(diff),
                    &Rect::new(x0, start, x1 - x0, end - start),
                );
                stats.copies += 1;
            }
        }
        stats
    }

    /// Rectangles covering the differences inside `rect`: tiles without
    /// differences are skipped (e.g. the interior of a window that arrived
    /// by copy), adjacent changed tiles are merged, each result is cut
    /// further into changed row bands and column runs, and those are joined
    /// again wherever one image is cheaper than two.
    /// `rect` is a part of the region of `diff`.
    fn changed_rects(&self, rect: &Rect, diff: &RegionDiff) -> Vec<Rect> {
        // (first tile column, end tile column, bounding box) per open run.
        let mut open: Vec<(u32, u32, Rect)> = Vec::new();
        let mut merged = Vec::new();
        let (cols, rows) = self.tiles_of(rect);
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
                    .then(|| self.diff_bounds(diff, &tile))
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
        self.join_by_cost(
            merged
                .iter()
                .flat_map(|r| self.split_bands(r, diff))
                .collect(),
        )
    }

    /// Joins rectangles into their bounding box while the unchanged pixels
    /// this adds are estimated to cost less than the image it saves (see
    /// [`IMAGE_OVERHEAD_BYTES`]). Lines of text that changed over the same
    /// columns end up in one image, as the leading between them is flat;
    /// changes side by side over unchanged text stay apart.
    fn join_by_cost(&self, mut rects: Vec<Rect>) -> Vec<Rect> {
        rects.sort_by_key(|r| (r.y, r.x));
        let mut joined: Vec<Rect> = Vec::with_capacity(rects.len());
        for mut rect in rects {
            // Growing a rectangle may bring further ones within reach.
            while let Some((_, i)) = joined
                .iter()
                .enumerate()
                .filter_map(|(i, other)| self.join_cost(other, &rect).map(|cost| (cost, i)))
                .min()
            {
                rect = union(rect, joined.swap_remove(i));
            }
            joined.push(rect);
        }
        joined
    }

    /// Estimated cost of sending the bounding box of `a` and `b` instead of
    /// both, in flat pixels (see [`FLAT_PIXELS_PER_BYTE`]), or `None` if
    /// that is more than an image is worth or the result would be too large.
    fn join_cost(&self, a: &Rect, b: &Rect) -> Option<u64> {
        let budget = IMAGE_OVERHEAD_BYTES * FLAT_PIXELS_PER_BYTE;
        let busy = FLAT_PIXELS_PER_BYTE / BUSY_PIXELS_PER_BYTE;
        let both = union(*a, *b);
        if both.area() > MAX_JOINED_PIXELS {
            return None;
        }
        let overlap = (a.right().min(b.right()).saturating_sub(a.x.max(b.x)) as u64)
            * (a.bottom().min(b.bottom()).saturating_sub(a.y.max(b.y)) as u64);
        // Even if every added pixel were flat.
        if both.area() + overlap - a.area() - b.area() > budget {
            return None;
        }
        let (left, right) = if a.x <= b.x { (a, b) } else { (b, a) };
        let mut cost = 0;
        for y in both.y..both.bottom() {
            // The parts of this row that neither rectangle covers.
            let mut x = both.x;
            for covered in [left, right] {
                if covered.y <= y && y < covered.bottom() {
                    cost += self.row_cost(y, x, covered.x, both.y, busy);
                    x = x.max(covered.right());
                }
            }
            cost += self.row_cost(y, x, both.right(), both.y, busy);
            if cost > budget {
                return None;
            }
        }
        Some(cost)
    }

    /// Estimated cost of sending the unchanged pixels `[x0, x1)` of row `y`
    /// in an image whose first row is `top`: `busy` per pixel, or one where
    /// the row is a single colour or repeats the row above.
    fn row_cost(&self, y: u32, x0: u32, x1: u32, top: u32, busy: u64) -> u64 {
        if x0 >= x1 {
            return 0;
        }
        let row = self.segment(&self.pixels, y, x0, x1);
        let flat = is_uniform(row) || (y > top && row == self.segment(&self.pixels, y - 1, x0, x1));
        (x1 - x0) as u64 * if flat { 1 } else { busy }
    }

    /// Splits the differences inside `rect` (a part of the region of
    /// `diff`) into bands of changed rows and, within each band, runs of
    /// changed columns.
    fn split_bands(&self, rect: &Rect, diff: &RegionDiff) -> Vec<Rect> {
        let rows: Vec<Option<RowDiff>> = (rect.y..rect.bottom())
            .map(|y| self.diff_within(diff.row(y), y, rect.x, rect.right()))
            .collect();
        let mut rects = Vec::new();
        for (b0, b1) in runs(rows.len(), ROW_GAP_MERGE as usize, |i| rows[i].is_some()) {
            let (y0, y1) = (rect.y + b0 as u32, rect.y + b1 as u32);
            // Columns that changed anywhere in the band.
            let w = rect.w as usize;
            let mut changed = vec![false; w];
            for (y, d) in (y0..y1).zip(&rows[b0..b1]) {
                // Only the changed range of the row can differ.
                let Some(d) = d else {
                    continue;
                };
                let a = self.segment(&self.pixels, y, d.first, d.last + 1);
                let b = self.segment(&self.sent, y, d.first, d.last + 1);
                let changed = &mut changed[(d.first - rect.x) as usize..];
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
    fn refine_lossy(&mut self, out: &mut Output) -> FlushStats {
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
                let opaque = is_opaque(&data);
                let zstd = self.zstd;
                let mut images = Vec::new();
                let encoded = self.on_pool(|| encode_lossless(&region, opaque, zstd, &mut images));
                self.record_images(&images, tile.area());
                if let Some((mimetype, bytes)) = encoded {
                    stats.images += 1;
                    stats.bytes += bytes.len();
                    stats.pixels += tile.area();
                    self.emit_image(
                        0,
                        MODE_OVER,
                        mimetype,
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
        extract(buf, self.width, rect)
    }

    /// Sends `rects` of the framebuffer, in this order, and records them in
    /// the mirror.
    ///
    /// The rectangles are encoded side by side on the encoding pool: that
    /// reads only the framebuffer. Writing the instructions and updating
    /// the mirror then happen here, one rectangle after the other in the
    /// original order, so the output is the same as encoding them one by
    /// one.
    fn emit_rects(&mut self, rects: Vec<Rect>, out: &mut Output) -> FlushStats {
        let jobs: Vec<(Rect, bool)> = rects
            .into_iter()
            .map(|rect| rect.clip(self.width, self.height))
            .filter(|rect| !rect.is_empty())
            .map(|rect| {
                // JPEG only for large, busy, colourful, opaque areas (the
                // last two are up to the encoding).
                let jpeg = !self.lossless
                    && self.formats.jpeg
                    && rect.area() >= 128 * 128
                    && self.average_heat(&rect) >= HOT;
                (rect, jpeg)
            })
            .collect();
        let (pixels, width, zstd) = (&self.pixels[..], self.width, self.zstd);
        let encode = |&(rect, jpeg): &(Rect, bool)| encode_rect(pixels, width, rect, jpeg, zstd);
        let encoded: Vec<EncodedRect> = match self.pool {
            Some(pool) if jobs.len() > 1 => pool.install(|| jobs.par_iter().map(encode).collect()),
            _ => self.on_pool(|| jobs.iter().map(encode).collect()),
        };
        let mut stats = FlushStats::default();
        for rect in encoded {
            stats.add(self.emit_encoded(rect, out));
        }
        stats
    }

    /// Sends a rectangle encoded by [`encode_rect`] and records it in the
    /// mirror.
    fn emit_encoded(&mut self, encoded: EncodedRect, out: &mut Output) -> FlushStats {
        let EncodedRect {
            rect,
            data,
            opaque,
            content,
            images,
        } = encoded;
        let mut stats = FlushStats {
            pixels: rect.area(),
            ..FlushStats::default()
        };
        self.record_images(&images, rect.area());
        if !opaque {
            // Source-over would blend with what is there: clear first.
            self.emit_fill(&rect, [0, 0, 0, 255], MODE_ROUT, out);
        }
        match content {
            Content::Fill(color) => {
                self.emit_fill(&rect, color, MODE_OVER, out);
                stats.fills += 1;
                self.store(&rect, &data, false);
            }
            Content::Image {
                mimetype,
                bytes,
                lossy,
            } => {
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
                self.store(&rect, &data, lossy);
            }
            Content::Failed => {}
        }
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

    fn emit_fill(&mut self, rect: &Rect, color: [u8; 4], mode: u8, out: &mut Output) {
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
    fn emit_copy(&mut self, src: &Rect, dx: u32, dy: u32, out: &mut Output) {
        Self::copy_instruction(src, dx, dy, out);
        copy_rect_over(&mut self.sent, self.width, src, dx, dy);
    }

    /// Emits a layer-0 copy from a source known to be opaque, which
    /// replaces the destination as it is (no row needs blending on the
    /// mirror).
    fn emit_opaque_copy(&mut self, src: &Rect, dx: u32, dy: u32, out: &mut Output) {
        Self::copy_instruction(src, dx, dy, out);
        copy_rect(&mut self.sent, self.width, src, dx, dy);
    }

    fn copy_instruction(src: &Rect, dx: u32, dy: u32, out: &mut Output) {
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
        out: &mut Output,
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
        out.blob(stream, bytes);
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
        out: &mut Output,
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
    pub fn sync(&mut self, out: &mut Output) {
        let mut ts = now_ms();
        if let Some(last) = self.last_sync
            && ts <= last
        {
            ts = last + 1;
        }
        let now = Instant::now();
        self.last_sync = Some(ts);
        let bytes = std::mem::take(&mut self.frame_bytes);
        self.clock.frame_sent(ts, now, bytes);
        self.metrics.frame(bytes, self.clock.unacked());
        // Asks a molcajete client how long it takes to apply the frame;
        // stock clients ignore the extra argument.
        *out += &apply_time::sync_request(ts, 1);
    }

    /// Handles a `sync` reply from the client, which arrived at
    /// `received.at`.
    ///
    /// The frame's lag is counted up to that moment, not up to now: the
    /// reply may have waited while this session built the next frame, and
    /// that wait says nothing about the client or the network. Counting it
    /// would slow the frame rate down by our own CPU time.
    pub fn ack(&mut self, timestamp: i64, received: impl Into<SyncReply>) {
        let received = received.into();
        if let Some(elapsed) = self.clock.acked(timestamp, received) {
            self.metrics
                .acked(elapsed, received.at.elapsed(), received.apply);
        }
    }

    /// Frames sent but not yet acknowledged by the client.
    pub fn unacked_frames(&self) -> u32 {
        self.clock.unacked() as u32
    }

    /// Smoothed time between sending a frame and the client acknowledging it.
    pub fn processing_lag(&self) -> Duration {
        self.clock.lag()
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
    ///
    /// An acknowledgement can move this earlier (see [`Pacing`]), so a
    /// session waiting for it asks again after every [`Display::ack`].
    pub fn next_frame_at(&self) -> Instant {
        // Rate control spends the video's bitrate per frame: more frames
        // than it was told to expect would be more bits per second.
        let slowest = match self.video {
            Some(_) => Duration::from_secs(1) / video::FPS,
            None => Duration::ZERO,
        };
        self.clock
            .next_frame_at(slowest)
            .unwrap_or_else(Instant::now)
    }
}

/// Image encoded for a rectangle, for the metrics: (format, time, bytes).
type EncodedImage = (ImageKind, Duration, usize);

/// What a rectangle of the pixel phase is sent as.
enum Content {
    /// A solid, opaque colour.
    Fill([u8; 4]),
    Image {
        mimetype: &'static str,
        bytes: Vec<u8>,
        lossy: bool,
    },
    /// No encoder took it; nothing is sent.
    Failed,
}

/// A rectangle of the framebuffer, encoded ([`encode_rect`]).
struct EncodedRect {
    rect: Rect,
    /// Its pixels, for the mirror.
    data: Vec<u8>,
    opaque: bool,
    content: Content,
    /// Images encoded on the way (a JPEG or zstd attempt may be followed
    /// by another format).
    images: Vec<EncodedImage>,
}

/// The pixels of `rect` of `buf`, a surface `width` pixels wide.
fn extract(buf: &[u8], width: u32, rect: &Rect) -> Vec<u8> {
    let stride = width as usize * 4;
    let row = rect.w as usize * 4;
    let mut data = Vec::with_capacity(rect.area() as usize * 4);
    for y in rect.y as usize..rect.bottom() as usize {
        let start = y * stride + rect.x as usize * 4;
        data.extend_from_slice(&buf[start..start + row]);
    }
    data
}

/// Encodes `rect` of the framebuffer `pixels` (`width` pixels wide): as a
/// fill if it is one opaque colour, as JPEG if `jpeg` allows it and it is
/// opaque and colourful, otherwise losslessly. Reads nothing else, so that
/// the rectangles of a frame can be encoded on several threads.
fn encode_rect(pixels: &[u8], width: u32, rect: Rect, jpeg: bool, zstd: bool) -> EncodedRect {
    let data = extract(pixels, width, &rect);
    let opaque = is_opaque(&data);
    let mut images = Vec::new();
    let content = {
        let region = Region {
            width: rect.w,
            height: rect.h,
            rgba: &data,
        };
        if let Some(color) = encode::solid_color(&region)
            && color[3] == 255
        {
            Content::Fill(color)
        } else if jpeg
            && opaque
            && encode::color_complexity(&region) > 256
            && let Some(bytes) = encode_image(ImageKind::Jpeg, &region, &mut images)
        {
            Content::Image {
                mimetype: "image/jpeg",
                bytes,
                lossy: true,
            }
        } else if let Some((mimetype, bytes)) = encode_lossless(&region, opaque, zstd, &mut images)
        {
            Content::Image {
                mimetype,
                bytes,
                lossy: false,
            }
        } else {
            Content::Failed
        }
    };
    EncodedRect {
        rect,
        data,
        opaque,
        content,
        images,
    }
}

/// Encodes `region` (timed, recorded in `images`), returning the bytes.
fn encode_image(
    kind: ImageKind,
    region: &Region,
    images: &mut Vec<EncodedImage>,
) -> Option<Vec<u8>> {
    let (bytes, elapsed) = timed(|| match kind {
        ImageKind::Png => encode::encode_png(region).ok(),
        ImageKind::Jpeg => encode::encode_jpeg(region, 80).ok(),
        ImageKind::Zstd => pool::with_zstd(|z| z.encode(region)),
    });
    let bytes = bytes?;
    images.push((kind, elapsed, bytes.len()));
    Some(bytes)
}

/// Encodes `region` losslessly, returning the mimetype and the bytes: a
/// zstd image if `zstd` images are sent, the region is opaque (the format
/// has no alpha) and the encoder takes it, otherwise PNG.
fn encode_lossless(
    region: &Region,
    opaque: bool,
    zstd: bool,
    images: &mut Vec<EncodedImage>,
) -> Option<(&'static str, Vec<u8>)> {
    if zstd
        && opaque
        && let Some(bytes) = encode_image(ImageKind::Zstd, region, images)
    {
        return Some((zimage::MIMETYPE, bytes));
    }
    encode_image(ImageKind::Png, region, images).map(|bytes| ("image/png", bytes))
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
    let mut blended = Vec::new();
    // Rows are moved in the order that reads every source row before it is
    // overwritten; within a row `copy_within` handles the overlap.
    let rows = 0..src.h as usize;
    let order: Box<dyn Iterator<Item = usize>> = if dy <= src.y {
        Box::new(rows)
    } else {
        Box::new(rows.rev())
    };
    for r in order {
        let s = (src.y as usize + r) * stride + src.x as usize * 4;
        let d = (dy as usize + r) * stride + dx as usize * 4;
        if is_opaque(&buf[s..s + row_len]) {
            buf.copy_within(s..s + row_len, d);
            continue;
        }
        blended.clear();
        blended.extend_from_slice(&buf[s..s + row_len]);
        for (dst, s) in buf[d..d + row_len]
            .chunks_exact_mut(4)
            .zip(blended.chunks_exact(4))
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
mod bench;
#[cfg(test)]
mod tests;
