//! Fonts for the terminal: the bundled DejaVu Sans Mono, optional
//! user-selected fonts, fallback fonts for glyphs the primary font lacks
//! (CJK, Hangul…), cell metrics and a cache of rasterised glyphs.
//!
//! Glyphs are rasterised into coverage masks the exact size of the cells
//! they occupy, so drawing a cell is a matter of blending its mask between
//! the background and foreground colours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{Font, FontArc, FontVec, GlyphId, PxScale, ScaleFont, point};
use tracing::{debug, warn};

use super::boxdraw;

const DEJAVU_REGULAR: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");
const DEJAVU_BOLD: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono-Bold.ttf");
/// Built-in CJK fallback (Hangul, Hanja, kana), so Korean renders without
/// any font installed on the host. SIL Open Font License 1.1.
const NANUM_GOTHIC_CODING: &[u8] =
    include_bytes!("../../assets/fonts/NanumGothicCoding-Regular.ttf");
/// Pseudo path under which the built-in fallback appears in the fallback list.
const BUILTIN_CJK: &str = "builtin:NanumGothicCoding-Regular.ttf";

/// Font families served by the bundled DejaVu Sans Mono.
const BUNDLED_NAMES: &[&str] = &["monospace", "mono", "dejavusansmono", "dejavu"];

/// File-name fragments of fonts worth trying as fallbacks, best first:
/// coding fonts with Hangul/CJK, then general CJK fonts, then broad
/// symbol coverage.
const FALLBACK_PRIORITY: &[&str] = &[
    "nanumgothiccoding",
    "d2coding",
    "notosansmonocjk",
    "notosanscjk",
    "sourcehansans",
    "notosanskr",
    "notosansjp",
    "notosanssc",
    "nanumgothic",
    "droidsansfallbackfull",
    "droidsansfallback",
    "droidsansjapanese",
    "wqy",
    "unifont",
    "notosanssymbols",
    "notosansmath",
    "dejavusans",
    "notosansmono",
    "notosans",
];

/// Style words that mark a non-regular face (skipped as fallbacks).
const STYLE_WORDS: &[&str] = &[
    "bold", "italic", "oblique", "light", "thin", "black", "heavy", "medium", "semibold",
];

/// Upper bound on fallback fonts tried for a missing glyph.
const MAX_FALLBACKS: usize = 24;
/// Upper bound on cached glyph masks per session.
const MAX_CACHED_GLYPHS: usize = 8192;

/// A coverage mask, one byte per pixel (0 = background, 255 = ink).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width * height) as usize],
        }
    }

    /// Sets full coverage in a rectangle (clipped to the mask).
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (mw, mh) = (self.width as i32, self.height as i32);
        let (x0, y0) = (x.clamp(0, mw), y.clamp(0, mh));
        let (x1, y1) = ((x + w).clamp(0, mw), (y + h).clamp(0, mh));
        for row in y0..y1 {
            let start = (row * mw) as usize;
            self.data[start + x0 as usize..start + x1 as usize].fill(255);
        }
    }

    /// Combines another mask of the same size into this one.
    pub fn merge(&mut self, other: &Mask) {
        for (a, b) in self.data.iter_mut().zip(&other.data) {
            *a = (*a).max(*b);
        }
    }

    /// Synthetic bold: smears the coverage one pixel to the right.
    fn embolden(&mut self) {
        let w = self.width as usize;
        for row in self.data.chunks_exact_mut(w) {
            for x in (1..w).rev() {
                row[x] = row[x].max(row[x - 1]);
            }
        }
    }

    fn is_blank(&self) -> bool {
        self.data.iter().all(|&a| a == 0)
    }
}

/// Size of one terminal cell in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Distance from the top of the cell to the baseline.
    pub baseline: u32,
}

/// What the user asked for.
#[derive(Clone, Debug, Default)]
pub struct FontOptions {
    /// guacd's `font-name` (a family name such as `monospace`).
    pub name: Option<String>,
    /// Font size in points.
    pub size_pt: f32,
    /// Client resolution (pixels per inch).
    pub dpi: u32,
    /// Extra font files or directories (`font-path`), searched first.
    pub search_paths: Vec<PathBuf>,
}

/// Which face to rasterise a glyph from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Primary,
    Fallback(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct GlyphKey {
    c: char,
    bold: bool,
    cells: u8,
}

pub struct Fonts {
    regular: FontArc,
    /// `None`: no bold face, bold is synthesised.
    bold: Option<FontArc>,
    /// Client resolution (pixels per inch) the point size is scaled by.
    dpi: f32,
    px_per_em: f32,
    metrics: CellMetrics,
    fallback_paths: Vec<PathBuf>,
    /// Lazily loaded fallback faces (`None` inside: failed to load).
    fallback_fonts: Vec<Option<Option<FontArc>>>,
    /// Face chosen for each character seen so far (`None`: no face has it).
    choices: HashMap<char, Option<Source>>,
    glyphs: HashMap<GlyphKey, Option<Arc<Mask>>>,
}

impl Fonts {
    /// Loads the primary font and lists fallback candidates. Performs file
    /// system access, so call it from a blocking context.
    pub fn load(options: &FontOptions) -> Self {
        let user_files = scan_paths(&options.search_paths);
        let (regular, bold) = options
            .name
            .as_deref()
            .and_then(|name| find_named_font(name, &user_files))
            .unwrap_or_else(bundled_fonts);

        // guacd assumes 96 dpi when the client does not say.
        let dpi = match options.dpi {
            0 => 96.0,
            dpi => dpi.clamp(48, 480) as f32,
        };
        let px_per_em = px_per_em(options.size_pt, dpi);
        let metrics = cell_metrics(&regular, px_per_em);

        // User fonts first, then the built-in CJK face, then system fonts.
        let mut fallback_paths = user_files;
        fallback_paths.push(PathBuf::from(BUILTIN_CJK));
        fallback_paths.extend(rank_fallbacks(system_fonts()));
        fallback_paths.dedup();
        fallback_paths.truncate(MAX_FALLBACKS);
        debug!(?metrics, px_per_em, fallbacks = ?fallback_paths, "fonts loaded");
        Self {
            fallback_fonts: vec![None; fallback_paths.len()],
            regular,
            bold,
            dpi,
            px_per_em,
            metrics,
            fallback_paths,
            choices: HashMap::new(),
            glyphs: HashMap::new(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Changes the font size (points) of the loaded faces. Returns whether
    /// the size differs from before: the cells then have new metrics, and
    /// glyphs are rasterised again as they are needed.
    pub fn set_size(&mut self, size_pt: f32) -> bool {
        let px_per_em = px_per_em(size_pt, self.dpi);
        if px_per_em == self.px_per_em {
            return false;
        }
        self.px_per_em = px_per_em;
        self.metrics = cell_metrics(&self.regular, px_per_em);
        self.glyphs.clear();
        true
    }

    /// Coverage mask for `c` drawn across `cells` cells, or `None` for
    /// glyphs without ink (spaces).
    pub fn glyph(&mut self, c: char, bold: bool, cells: u8) -> Option<Arc<Mask>> {
        let key = GlyphKey { c, bold, cells };
        if let Some(mask) = self.glyphs.get(&key) {
            return mask.clone();
        }
        if self.glyphs.len() >= MAX_CACHED_GLYPHS {
            self.glyphs.clear();
        }
        let mask = self.rasterize(key).filter(|m| !m.is_blank()).map(Arc::new);
        self.glyphs.insert(key, mask.clone());
        mask
    }

    fn rasterize(&mut self, key: GlyphKey) -> Option<Mask> {
        let GlyphKey { c, bold, cells } = key;
        if c.is_whitespace() || c.is_control() {
            return None;
        }
        let width = self.metrics.width * cells.max(1) as u32;
        let height = self.metrics.height;
        if boxdraw::handles(c) {
            return boxdraw::render(c, width, height);
        }
        let (font, synthetic_bold, centred) = match self.source_for(c) {
            Some(Source::Primary) | None => match (&self.bold, bold) {
                (Some(face), true) => (face.clone(), false, false),
                _ => (self.regular.clone(), bold, false),
            },
            Some(Source::Fallback(i)) => (self.fallback(i)?, bold, true),
        };
        // `None` from `source_for` draws the primary font's .notdef box.
        let id = font.glyph_id(c);
        let mut mask = draw_glyph(
            &font,
            id,
            self.px_per_em,
            width,
            height,
            self.metrics.baseline,
            centred,
        );
        if synthetic_bold {
            mask.embolden();
        }
        Some(mask)
    }

    /// Picks the first face that has a glyph for `c`.
    fn source_for(&mut self, c: char) -> Option<Source> {
        if let Some(choice) = self.choices.get(&c) {
            return *choice;
        }
        let choice = if self.regular.glyph_id(c) != GlyphId(0) {
            Some(Source::Primary)
        } else {
            (0..self.fallback_paths.len())
                .find(|&i| {
                    self.fallback(i)
                        .is_some_and(|f| f.glyph_id(c) != GlyphId(0))
                })
                .map(Source::Fallback)
        };
        if choice.is_none() {
            debug!(?c, "no font has a glyph for this character");
        }
        self.choices.insert(c, choice);
        choice
    }

    fn fallback(&mut self, index: usize) -> Option<FontArc> {
        let slot = &mut self.fallback_fonts[index];
        slot.get_or_insert_with(|| load_font_file(&self.fallback_paths[index]))
            .clone()
    }
}

fn bundled_fonts() -> (FontArc, Option<FontArc>) {
    let regular = FontArc::try_from_slice(DEJAVU_REGULAR).expect("bundled font is valid");
    let bold = FontArc::try_from_slice(DEJAVU_BOLD).expect("bundled font is valid");
    (regular, Some(bold))
}

/// The ab_glyph scale (ascent − descent in pixels) for an em size.
fn scale_for(font: &FontArc, px_per_em: f32) -> PxScale {
    let units = font.units_per_em().unwrap_or(1000.0);
    PxScale::from(px_per_em * font.height_unscaled() / units)
}

/// Cell size: the advance of `0` by the font's ascent + descent.
/// Pixels per em of a point size at a resolution; 12 pt for a size that
/// is not one.
fn px_per_em(size_pt: f32, dpi: f32) -> f32 {
    let size_pt = if size_pt > 0.0 { size_pt } else { 12.0 };
    (size_pt * dpi / 72.0).clamp(4.0, 200.0)
}

fn cell_metrics(font: &FontArc, px_per_em: f32) -> CellMetrics {
    let scaled = font.as_scaled(scale_for(font, px_per_em));
    let advance = scaled.h_advance(font.glyph_id('0'));
    let advance = if advance > 0.0 {
        advance
    } else {
        px_per_em * 0.6
    };
    CellMetrics {
        width: (advance.round() as u32).max(1),
        height: ((scaled.ascent() - scaled.descent()).ceil() as u32).max(1),
        baseline: scaled.ascent().round().max(0.0) as u32,
    }
}

/// Rasterises one glyph into a `width`×`height` mask with its baseline at
/// `baseline`. Glyphs wider than the cell(s) are scaled down to fit; glyphs
/// from fallback fonts are centred horizontally.
fn draw_glyph(
    font: &FontArc,
    id: GlyphId,
    px_per_em: f32,
    width: u32,
    height: u32,
    baseline: u32,
    centred: bool,
) -> Mask {
    let mut scale = scale_for(font, px_per_em);
    let mut advance = font.as_scaled(scale).h_advance(id);
    if advance > width as f32 + 0.5 {
        let shrink = width as f32 / advance;
        scale = PxScale::from(scale.y * shrink);
        advance *= shrink;
    }
    let x = if centred {
        ((width as f32 - advance) / 2.0).max(0.0)
    } else {
        0.0
    };
    let glyph = id.with_scale_and_position(scale, point(x, baseline as f32));
    let mut mask = Mask::new(width, height);
    if let Some(outlined) = font.outline_glyph(glyph) {
        let bounds = outlined.px_bounds();
        let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
        outlined.draw(|gx, gy, coverage| {
            let (px, py) = (ox + gx as i32, oy + gy as i32);
            if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                let alpha = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
                let i = (py as u32 * width + px as u32) as usize;
                mask.data[i] = mask.data[i].max(alpha);
            }
        });
    }
    mask
}

/// Loads a font file (index 0 of collections), sharing parsed fonts
/// between sessions.
fn load_font_file(path: &Path) -> Option<FontArc> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Option<FontArc>>>> = OnceLock::new();
    let cache = LOADED.get_or_init(Default::default);
    if let Some(font) = cache.lock().ok()?.get(path) {
        return font.clone();
    }
    let data = if path.as_os_str() == BUILTIN_CJK {
        Ok(NANUM_GOTHIC_CODING.to_vec())
    } else {
        std::fs::read(path)
    };
    let font = data
        .map_err(|e| e.to_string())
        .and_then(|data| FontVec::try_from_vec_and_index(data, 0).map_err(|e| e.to_string()))
        .map(FontArc::new)
        .inspect_err(|err| warn!(path = %path.display(), error = %err, "cannot load font"))
        .ok();
    cache.lock().ok()?.insert(path.to_owned(), font.clone());
    font
}

/// Resolves a `font-name` to font files: the bundled font for generic
/// names, otherwise a file whose name matches (`Family[-Regular].ttf` and
/// `Family-Bold.ttf`) in `font-path` or the system font directories.
fn find_named_font(name: &str, user_files: &[PathBuf]) -> Option<(FontArc, Option<FontArc>)> {
    let wanted = normalize(name);
    if wanted.is_empty() || BUNDLED_NAMES.contains(&wanted.as_str()) {
        return None;
    }
    let candidates = || user_files.iter().chain(system_fonts());
    let find = |suffixes: &[&str]| {
        candidates().find(|path| {
            let stem = normalize(&file_stem(path));
            suffixes.iter().any(|s| stem == format!("{wanted}{s}"))
        })
    };
    let Some(regular) = find(&["", "regular", "book", "roman"]).and_then(|p| load_font_file(p))
    else {
        warn!(font = name, "font not found, using DejaVu Sans Mono");
        return None;
    };
    let bold = find(&["bold"]).and_then(|p| load_font_file(p));
    Some((regular, bold))
}

/// File name up to the first dot (`NotoSansCJK-Regular.ttc` →
/// `NotoSansCJK-Regular`).
fn file_stem(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.split('.').next().unwrap_or(n).to_owned())
        .unwrap_or_default()
}

/// Hidden files (e.g. macOS `._Font.ttf` resource forks) are not fonts.
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Lower-case ASCII letters and digits only (`Noto Sans-Mono` →
/// `notosansmono`).
fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_font_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "ttf" | "otf" | "ttc" | "otc"
        )
    })
}

/// Font files under the given files/directories, keeping their order.
fn scan_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            scan_dir(path, 0, &mut found);
            // Regular faces first: they are the better fallbacks.
            found.sort_by_cached_key(|p| (is_styled(p), p.clone()));
            out.extend(found);
        } else if is_font_file(path) {
            out.push(path.clone());
        }
    }
    out
}

fn scan_dir(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: u32 = 6;
    const MAX_FILES: usize = 20_000;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            if depth < MAX_DEPTH {
                scan_dir(&path, depth + 1, out);
            }
        } else if is_font_file(&path) {
            out.push(path);
        }
    }
}

/// All font files in the usual system and user font directories (scanned
/// once per process).
fn system_fonts() -> &'static [PathBuf] {
    static FONTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut dirs = vec![
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
        let fonts = scan_paths(&dirs);
        debug!(count = fonts.len(), "system fonts found");
        fonts
    })
}

/// Whether a font file name denotes a bold/italic/light… face.
fn is_styled(path: &Path) -> bool {
    let stem = normalize(&file_stem(path));
    STYLE_WORDS.iter().any(|w| stem.contains(w))
}

/// Fallback candidates among `fonts`, best first (see
/// [`FALLBACK_PRIORITY`]); regular faces only.
fn rank_fallbacks(fonts: &[PathBuf]) -> Vec<PathBuf> {
    let mut ranked: Vec<(usize, String, &PathBuf)> = fonts
        .iter()
        .filter_map(|path| {
            let stem = normalize(&file_stem(path));
            if is_styled(path) || stem.contains("emoji") {
                return None;
            }
            let rank = FALLBACK_PRIORITY.iter().position(|p| stem.contains(p))?;
            Some((rank, stem, path))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, _, p)| p.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled(size_pt: f32, dpi: u32) -> Fonts {
        Fonts::load(&FontOptions {
            name: None,
            size_pt,
            dpi,
            search_paths: Vec::new(),
        })
    }

    #[test]
    fn cell_metrics_follow_point_size_and_dpi() {
        // 12 pt at 96 dpi = 16 px per em; DejaVu Sans Mono's advance is
        // 1233/2048 em and its height (ascent + descent) 2384/2048 em.
        let m = bundled(12.0, 96).metrics();
        assert_eq!((m.width, m.height, m.baseline), (10, 19, 15));
        let m = bundled(12.0, 192).metrics();
        assert_eq!((m.width, m.height), (19, 38));
        let m = bundled(9.0, 96).metrics();
        assert_eq!((m.width, m.height), (7, 14));
    }

    #[test]
    fn size_changes_without_loading_the_fonts_again() {
        let mut fonts = bundled(12.0, 96);
        let wide = fonts.glyph('A', false, 1).unwrap();
        assert!(!fonts.set_size(12.0), "the same size changes nothing");
        assert!(fonts.set_size(9.0));
        let m = fonts.metrics();
        assert_eq!((m.width, m.height), (7, 14));
        // Glyphs drawn at the old size are not handed out again.
        let narrow = fonts.glyph('A', false, 1).unwrap();
        assert_eq!((narrow.width, narrow.height), (7, 14));
        assert_ne!(wide.width, narrow.width);
        // A size that is not one falls back to 12 pt, as at the start.
        assert!(fonts.set_size(f32::NAN));
        assert_eq!(fonts.metrics().width, 10);
    }

    #[test]
    fn glyphs_fill_their_cells() {
        let mut fonts = bundled(12.0, 96);
        let m = fonts.metrics();
        let a = fonts.glyph('A', false, 1).unwrap();
        assert_eq!((a.width, a.height), (m.width, m.height));
        assert!(fonts.glyph(' ', false, 1).is_none());
        let bold = fonts.glyph('A', true, 1).unwrap();
        let ink = |mask: &Mask| mask.data.iter().map(|&a| a as u32).sum::<u32>();
        assert!(ink(&bold) > ink(&a), "bold face has more ink");
        // Cached masks are shared.
        assert!(Arc::ptr_eq(&a, &fonts.glyph('A', false, 1).unwrap()));
        // Wide glyphs get a two-cell mask; box drawing is procedural.
        assert_eq!(fonts.glyph('W', false, 2).unwrap().width, m.width * 2);
        let line = fonts.glyph('─', false, 1).unwrap();
        assert_eq!(*line, boxdraw::render('─', m.width, m.height).unwrap());
    }

    #[test]
    fn falls_back_in_order_and_skips_broken_fonts() {
        let mut fonts = bundled(12.0, 96);
        // Fallbacks: an unreadable path, then a font without Hangul (the
        // bundled bold face, from a temporary file).
        let dir = std::env::temp_dir().join(format!("guacd-font-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Fallback.ttf");
        std::fs::write(&path, DEJAVU_BOLD).unwrap();
        fonts.fallback_paths = vec![PathBuf::from("/nonexistent/font.ttf"), path];
        fonts.fallback_fonts = vec![None, None];
        assert_eq!(fonts.source_for('a'), Some(Source::Primary));
        fonts.choices.insert('b', None);
        assert_eq!(fonts.source_for('b'), None, "choices are cached");
        // DejaVu has no Hangul: no face at all, the .notdef box is drawn.
        assert_eq!(fonts.source_for('한'), None);
        assert!(fonts.glyph('한', false, 2).is_some());
        assert!(
            matches!(fonts.fallback_fonts[0], Some(None)),
            "broken font remembered"
        );
        assert!(matches!(fonts.fallback_fonts[1], Some(Some(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hangul_renders_with_the_builtin_fallback() {
        let mut fonts = Fonts::load(&FontOptions::default());
        let Some(Source::Fallback(i)) = fonts.source_for('한') else {
            panic!("no face for Hangul");
        };
        assert_eq!(fonts.fallback_paths[i], PathBuf::from(BUILTIN_CJK));
        let m = fonts.metrics();
        let mask = fonts.glyph('한', false, 2).unwrap();
        assert_eq!((mask.width, mask.height), (m.width * 2, m.height));
        // Centred: ink on both halves.
        let half = (m.width) as usize;
        let ink = |range: std::ops::Range<usize>| {
            mask.data
                .chunks_exact(mask.width as usize)
                .map(|row| row[range.clone()].iter().map(|&a| a as u32).sum::<u32>())
                .sum::<u32>()
        };
        assert!(ink(0..half) > 0 && ink(half..2 * half) > 0);
    }

    #[test]
    fn ranks_fallback_candidates() {
        let paths: Vec<PathBuf> = [
            "/f/NotoSans-Regular.ttf",
            "/f/DroidSansFallbackFull.ttf",
            "/f/NanumGothicCoding-Bold.ttf",
            "/f/NanumGothicCoding.ttf",
            "/f/Cantarell.otf",
            "/f/NotoColorEmoji.ttf",
            "/f/NotoSansCJK-Regular.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let ranked = rank_fallbacks(&paths);
        let names: Vec<String> = ranked.iter().map(|p| file_stem(p)).collect();
        assert_eq!(
            names,
            [
                "NanumGothicCoding",
                "NotoSansCJK-Regular",
                "DroidSansFallbackFull",
                "NotoSans-Regular"
            ]
        );
    }

    #[test]
    fn scans_font_directories() {
        let dir = std::env::temp_dir().join(format!("guacd-font-scan-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(sub.join(".hidden")).unwrap();
        for name in [
            "Font-Bold.ttf",
            "Font.ttf",
            "readme.txt",
            "sub/Other.otf",
            "sub/._Font.ttf",
            "sub/.hidden/X.ttf",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let found = scan_paths(&[dir.clone(), PathBuf::from("/nonexistent")]);
        let names: Vec<_> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["Font.ttf", "sub/Other.otf", "Font-Bold.ttf"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn named_fonts() {
        assert!(find_named_font("monospace", &[]).is_none());
        assert!(find_named_font("DejaVu Sans Mono", &[]).is_none());
        assert_eq!(normalize("Noto Sans-Mono CJK"), "notosansmonocjk");
        assert_eq!(
            file_stem(Path::new("/a/NotoSansMonoCJK-VF.otf.ttc")),
            "NotoSansMonoCJK-VF"
        );
    }

    #[test]
    fn mask_operations() {
        let mut m = Mask::new(4, 2);
        m.fill(-1, 0, 2, 1);
        assert_eq!(m.data, [255, 0, 0, 0, 0, 0, 0, 0]);
        m.embolden();
        assert_eq!(m.data, [255, 255, 0, 0, 0, 0, 0, 0]);
        let mut other = Mask::new(4, 2);
        other.fill(3, 1, 5, 5);
        m.merge(&other);
        assert_eq!(m.data, [255, 255, 0, 0, 0, 0, 0, 255]);
    }
}
