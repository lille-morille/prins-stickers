//! Fonts: finding the system's, measuring and drawing text in them, and Pillow's built-in bitmap font.
//!
//! Port of the font parts of `assets.py` and `craft.py`. The numbers mirror FreeType as Pillow uses it
//! (`getmetrics`, `getlength` with hinted whole-pixel advances); glyph shapes are unhinted outlines
//! rasterised with `ab_glyph`, so they look the same but are not bit-identical to Pillow's.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font as _, FontRef, Glyph, GlyphId as AbGlyphId, PxScale};
use image::GrayImage;

use crate::assets::{read_source, AssetError};

pub const PIXEL_FONT: &str = "pixel";
pub const PIXEL_HEIGHT: i32 = 11;

const FONT_EXTENSIONS: [&str; 3] = ["ttf", "ttc", "otf"];

/// Friendly names. Each is (name, file stem, face index in that file). Order is the catalogue's order.
pub const FONT_ALIASES: [(&str, &str, u32); 15] = [
    ("sans", "Helvetica", 0),
    ("sans-bold", "Helvetica", 1),
    ("serif", "Times", 0),
    ("serif-bold", "Times", 1),
    ("mono", "Menlo", 0),
    ("mono-bold", "Menlo", 1),
    ("hand", "Bradley Hand Bold", 0),
    ("script", "SnellRoundhand", 0),
    ("marker", "MarkerFelt", 0),
    ("display", "Impact", 0),
    ("typewriter", "AmericanTypewriter", 0),
    ("stencil", "Stencil", 0),
    ("comic", "Comic Sans MS", 0),
    ("gothic", "Copperplate", 0),
    ("chalk", "Chalkduster", 0),
];

/// craft's display faces: (name, file stem, style name inside that file or None if the file has one face).
pub const CRAFT_FACES: [(&str, &str, Option<&str>); 15] = [
    ("impact", "impact", None),
    ("din", "din condensed bold", None),
    ("futura", "futura", Some("Bold")),
    ("didot", "didot", Some("Bold")),
    ("didot-italic", "didot", Some("Italic")),
    ("bodoni", "bodoni 72", Some("Bold")),
    ("rockwell", "rockwell", Some("Bold")),
    ("phosphate", "phosphate", Some("Solid")),
    ("krungthep", "krungthep", None),
    ("black", "arial black", None),
    ("courier", "courier new bold", None),
    ("georgia", "georgia bold", None),
    ("typewriter", "americantypewriter", Some("Bold")),
    ("menlo", "menlo", Some("Bold")),
    ("helvetica", "helvetica", Some("Bold")),
];
/// For scripts the display faces don't have.
pub const FALLBACK_STEMS: [&str; 5] = [
    "arial unicode",
    "applesdgothicneo",
    "hiragino sans gb",
    "stheiti medium",
    "applegothic",
];
/// A code point in a private plane that no font maps: it draws the font's .notdef glyph.
const NO_SUCH_GLYPH: char = '\u{F0000}';

// --- finding fonts -------------------------------------------------------------------------

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("/System/Library/Fonts/Supplemental"),
        PathBuf::from("/Library/Fonts"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Library/Fonts"));
    }
    dirs.push(PathBuf::from("/usr/share/fonts"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".fonts"));
    }
    dirs
}

fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| FONT_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Like `Path.rglob("*")`: a folder's own entries first, then its subfolders.
fn scan(folder: &Path, found: &mut BTreeMap<String, PathBuf>) {
    let Ok(read) = fs::read_dir(folder) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut subfolders = Vec::new();
    for entry in entries {
        let path = entry.path();
        let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_real_dir {
            subfolders.push(path);
        } else if is_font_file(&path) && !path.is_dir() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                found.entry(stem.to_lowercase()).or_insert(path);
            }
        }
    }
    for sub in subfolders {
        scan(&sub, found);
    }
}

/// Lowercase file stem -> path, first found wins, folders in Python's order.
pub fn font_files() -> &'static BTreeMap<String, PathBuf> {
    static FILES: OnceLock<BTreeMap<String, PathBuf>> = OnceLock::new();
    FILES.get_or_init(|| {
        let mut found = BTreeMap::new();
        for folder in font_dirs() {
            if folder.exists() {
                scan(&folder, &mut found);
            }
        }
        found
    })
}

/// The pixel font, then each friendly name whose file is installed.
pub fn font_names() -> Vec<String> {
    let files = font_files();
    let mut names = vec![PIXEL_FONT.to_string()];
    names.extend(
        FONT_ALIASES
            .iter()
            .filter(|(_, stem, _)| files.contains_key(&stem.to_lowercase()))
            .map(|(name, _, _)| name.to_string()),
    );
    names
}

pub fn all_system_fonts() -> Vec<String> {
    font_files().keys().cloned().collect()
}

/// A font file's bytes, read once per process.
pub(crate) fn font_data(path: &Path) -> Result<Arc<Vec<u8>>, AssetError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<u8>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(data) = guard.get(path) {
            return Ok(data.clone());
        }
    }
    let bytes = fs::read(path)
        .map_err(|e| AssetError(format!("Couldn't read the font {}: {e}", path.display())))?;
    let data = Arc::new(bytes);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(path.to_path_buf(), data.clone());
    }
    Ok(data)
}

// --- FreeType's fixed-point arithmetic -------------------------------------------------------

/// FT_DivFix: a / b as 16.16.
fn div_fix(a: i64, b: i64) -> i64 {
    if b == 0 {
        return 0;
    }
    let neg = (a < 0) != (b < 0);
    let (a, b) = (a.abs(), b.abs());
    let q = ((a << 16) + b / 2) / b;
    if neg {
        -q
    } else {
        q
    }
}

/// FT_MulFix: a * b / 65536, rounded.
fn mul_fix(a: i64, b: i64) -> i64 {
    let neg = (a < 0) != (b < 0);
    let q = (a.abs() * b.abs() + 0x8000) >> 16;
    if neg {
        -q
    } else {
        q
    }
}

/// FT_MulDiv: a * b / c, rounded.
fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    if c == 0 {
        return 0;
    }
    let neg = ((a < 0) != (b < 0)) != (c < 0);
    let q = (a.abs() * b.abs() + c.abs() / 2) / c.abs();
    if neg {
        -q
    } else {
        q
    }
}

// --- vector faces --------------------------------------------------------------------------

#[derive(Clone)]
struct VectorFace {
    data: Arc<Vec<u8>>,
    index: u32,
    size: u32,
    upem: i64,
    /// FT size metrics x_scale / y_scale (16.16): size * 64 / units_per_em.
    scale: i64,
    ascent: i32,
    descent: i32,
    style: String,
    kern: bool,
    /// The `hdmx` advances (one byte per glyph) for exactly this size, if the font has them.
    hdmx: Option<Vec<u8>>,
}

/// The `hdmx` record for a pixel size, unless the font is fixed pitch (FreeType ignores it then).
fn hdmx_widths(ttf: &ttf_parser::Face, size: u32) -> Option<Vec<u8>> {
    let raw = ttf.raw_face();
    let post = raw.table(ttf_parser::Tag::from_bytes(b"post"))?;
    if post.get(12..16)? != [0, 0, 0, 0] {
        return None;
    }
    let table = raw.table(ttf_parser::Tag::from_bytes(b"hdmx"))?;
    let records = u16::from_be_bytes([*table.get(2)?, *table.get(3)?]) as usize;
    let record_size = u32::from_be_bytes([
        *table.get(4)?,
        *table.get(5)?,
        *table.get(6)?,
        *table.get(7)?,
    ]) as usize;
    let glyphs = ttf.number_of_glyphs() as usize;
    for i in 0..records {
        let start = 8 + i * record_size;
        if *table.get(start)? as u32 == size {
            return table.get(start + 2..start + 2 + glyphs).map(|w| w.to_vec());
        }
    }
    None
}

fn name_text(face: &ttf_parser::Face, ids: &[u16]) -> Option<String> {
    use ttf_parser::PlatformId;
    for &id in ids {
        let (mut win_en, mut win_any, mut mac, mut unicode) = (None, None, None, None);
        for name in face.names() {
            if name.name_id != id || name.name.is_empty() {
                continue;
            }
            match name.platform_id {
                PlatformId::Windows => {
                    if let Some(text) = name.to_string() {
                        if name.language_id & 0x3ff == 9 {
                            win_en.get_or_insert(text);
                        } else {
                            win_any.get_or_insert(text);
                        }
                    }
                }
                PlatformId::Macintosh if name.language_id == 0 => {
                    mac.get_or_insert(name.name.iter().map(|&b| b as char).collect::<String>());
                }
                PlatformId::Unicode => {
                    if let Some(text) = name.to_string() {
                        unicode.get_or_insert(text);
                    }
                }
                _ => {}
            }
        }
        if let Some(text) = win_en.or(win_any).or(mac).or(unicode) {
            return Some(text);
        }
    }
    None
}

impl VectorFace {
    fn open(data: Arc<Vec<u8>>, index: u32, size: u32) -> Option<VectorFace> {
        let size = size.max(1);
        let ttf = ttf_parser::Face::parse(&data, index).ok()?;
        FontRef::try_from_slice_and_index(&data, index).ok()?;
        let upem = ttf.units_per_em() as i64;
        if upem == 0 {
            return None;
        }
        let scale = div_fix((size as i64) << 6, upem);

        // FreeType takes hhea's ascender and descender, or OS/2's if hhea has none.
        let hhea = ttf.tables().hhea;
        let (mut asc, mut desc) = (hhea.ascender as i64, hhea.descender as i64);
        if asc == 0 && desc == 0 {
            if let Some(os2) = ttf.tables().os2 {
                let (typo_a, typo_d) = (
                    os2.typographic_ascender() as i64,
                    os2.typographic_descender() as i64,
                );
                if typo_a != 0 || typo_d != 0 {
                    (asc, desc) = (typo_a, typo_d);
                } else {
                    (asc, desc) = (
                        os2.windows_ascender() as i64,
                        -(os2.windows_descender() as i64),
                    );
                }
            }
        }
        // FreeType's scaled size metrics: the ascender goes up to a whole pixel and the descender down.
        let (a26, d26) = (mul_fix(asc, scale), mul_fix(desc, scale));
        let (ascent, descent) = (((a26 + 63) >> 6) as i32, (-(d26 >> 6)) as i32);

        let style = name_text(&ttf, &[22, 17, 2]).unwrap_or_else(|| "Regular".to_string());
        let kern = ttf
            .raw_face()
            .table(ttf_parser::Tag::from_bytes(b"kern"))
            .and_then(|t| t.get(0..2))
            .map(|b| b == [0, 0])
            .unwrap_or(false);
        let hdmx = hdmx_widths(&ttf, size);
        Some(VectorFace {
            data,
            index,
            size,
            upem,
            scale,
            ascent,
            descent,
            style,
            kern,
            hdmx,
        })
    }

    fn with_ttf<R>(&self, f: impl FnOnce(&ttf_parser::Face) -> R) -> Option<R> {
        ttf_parser::Face::parse(&self.data, self.index)
            .ok()
            .map(|face| f(&face))
    }

    fn glyph_of(&self, ttf: &ttf_parser::Face, c: char) -> ttf_parser::GlyphId {
        ttf.glyph_index(c).unwrap_or(ttf_parser::GlyphId(0))
    }

    /// The hinted advance FreeType reports: the scaled advance rounded to a whole pixel. In monochrome mode
    /// FreeType takes the font's own `hdmx` advance for this size when it has one (which is what its hinting
    /// would give). The bytecode interpreter itself is not ported, so other hinted advances can differ by a pixel.
    fn advance_px(&self, ttf: &ttf_parser::Face, gid: ttf_parser::GlyphId, mono: bool) -> i32 {
        if mono {
            if let Some(&width) = self
                .hdmx
                .as_ref()
                .and_then(|widths| widths.get(gid.0 as usize))
            {
                return width as i32;
            }
        }
        let units = ttf.glyph_hor_advance(gid).unwrap_or(0) as i64;
        ((mul_fix(units, self.scale) + 32) >> 6) as i32
    }

    /// Pair kerning from a legacy `kern` table, as `FT_Get_Kerning(FT_KERNING_DEFAULT)` gives it.
    fn kern_px(
        &self,
        ttf: &ttf_parser::Face,
        left: ttf_parser::GlyphId,
        right: ttf_parser::GlyphId,
    ) -> i32 {
        if !self.kern || left.0 == 0 || right.0 == 0 {
            return 0;
        }
        let Some(table) = ttf.tables().kern else {
            return 0;
        };
        let mut units = 0i64;
        for sub in table.subtables {
            if sub.horizontal
                && !sub.has_cross_stream
                && matches!(sub.format, ttf_parser::kern::Format::Format0(_))
            {
                units += sub.glyphs_kerning(left, right).unwrap_or(0) as i64;
            }
        }
        if units == 0 {
            return 0;
        }
        let mut scaled = mul_fix(units, self.scale);
        if self.size < 25 {
            scaled = mul_div(scaled, self.size as i64, 25);
        }
        ((scaled + 32) >> 6) as i32
    }

    /// (glyph, advance in 26.6 units). Pillow's basic layout adds a kerning pair's whole-pixel value to the
    /// previous glyph's advance as if it were already in 1/64 px, so kerning moves things by hundredths only.
    fn layout(
        &self,
        ttf: &ttf_parser::Face,
        text: &str,
        mono: bool,
    ) -> Vec<(ttf_parser::GlyphId, i64)> {
        let mut out: Vec<(ttf_parser::GlyphId, i64)> = Vec::with_capacity(text.len());
        for c in text.chars() {
            let gid = self.glyph_of(ttf, c);
            if let Some(&(last, _)) = out.last() {
                let kern = self.kern_px(ttf, last, gid) as i64;
                if let Some(prev) = out.last_mut() {
                    prev.1 += kern;
                }
            }
            out.push((gid, self.advance_px(ttf, gid, mono) as i64 * 64));
        }
        out
    }

    fn length(&self, text: &str, mono: bool) -> f64 {
        self.with_ttf(|ttf| {
            self.layout(ttf, text, mono)
                .iter()
                .map(|&(_, adv)| adv)
                .sum::<i64>() as f64
                / 64.0
        })
        .unwrap_or(0.0)
    }

    fn px_scale(&self, font: &FontRef) -> PxScale {
        let height = font.height_unscaled();
        PxScale::from(self.size as f32 * height / self.upem as f32)
    }

    fn draw(&self, img: &mut GrayImage, x: f64, y: f64, text: &str, fill: u8, mono: bool) {
        let Ok(font) = FontRef::try_from_slice_and_index(&self.data, self.index) else {
            return;
        };
        let scale = self.px_scale(&font);
        // Pillow takes int(xy) and passes the fraction on, where it ends up rounding the pen to a whole pixel.
        let origin = |v: f64| -> i32 {
            let whole = v.trunc();
            let frac = v - whole;
            whole as i32 + (frac + 0.5).floor() as i32
        };
        let start = origin(x);
        let baseline = origin(y) + self.ascent;
        let Some(glyphs) = self.with_ttf(|ttf| self.layout(ttf, text, mono)) else {
            return;
        };
        let mut pen = 0i64; // 26.6
        for (gid, advance) in glyphs {
            let pen_px = start + ((pen + 32) >> 6) as i32;
            let glyph = Glyph {
                id: AbGlyphId(gid.0),
                scale,
                position: point(pen_px as f32, baseline as f32),
            };
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let (left, top) = (bounds.min.x as i32, bounds.min.y as i32);
                outlined.draw(|gx, gy, coverage| {
                    let (px, py) = (left + gx as i32, top + gy as i32);
                    if px < 0 || py < 0 || px >= img.width() as i32 || py >= img.height() as i32 {
                        return;
                    }
                    let pixel = &mut img.get_pixel_mut(px as u32, py as u32).0[0];
                    if mono {
                        if coverage >= 0.5 {
                            *pixel = fill;
                        }
                    } else {
                        let m = (coverage.clamp(0.0, 1.0) * 255.0).round() as u32;
                        if m > 0 {
                            *pixel = blend(*pixel, fill, m);
                        }
                    }
                });
            }
            pen += advance;
        }
    }

    /// The glyph's coverage at this face's size as (width, height, bytes), None for an empty glyph.
    fn glyph_mask(&self, gid: ttf_parser::GlyphId) -> Option<(u32, u32, Vec<u8>)> {
        let font = FontRef::try_from_slice_and_index(&self.data, self.index).ok()?;
        let glyph = Glyph {
            id: AbGlyphId(gid.0),
            scale: self.px_scale(&font),
            position: point(0.0, 0.0),
        };
        let outlined = font.outline_glyph(glyph)?;
        let bounds = outlined.px_bounds();
        let (w, h) = (bounds.width().ceil() as u32, bounds.height().ceil() as u32);
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return None;
        }
        let mut bytes = vec![0u8; (w * h) as usize];
        outlined.draw(|x, y, c| {
            if x < w && y < h {
                bytes[(y * w + x) as usize] = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });
        if bytes.iter().all(|&b| b == 0) {
            return None;
        }
        Some((w, h, bytes))
    }

    fn has_glyph(&self, c: char) -> bool {
        self.with_ttf(|ttf| {
            let drawn = self.glyph_mask(self.glyph_of(ttf, c));
            let Some(drawn) = drawn else { return false };
            let missing = self.glyph_mask(self.glyph_of(ttf, NO_SUCH_GLYPH));
            missing.as_ref() != Some(&drawn)
        })
        .unwrap_or(false)
    }
}

/// Pillow's BLEND: dst * (255 - m) / 255 + fill * m / 255, with its rounding.
fn blend(dst: u8, fill: u8, m: u32) -> u8 {
    let tmp = dst as u32 * (255 - m) + fill as u32 * m + 128;
    (((tmp >> 8) + tmp) >> 8) as u8
}

// --- the pixel font ------------------------------------------------------------------------

/// One glyph of Pillow's default bitmap font: (advance, rectangle it covers as (x0, x1, y0, y1) relative to the
/// pen and the mask's top row, x1 and y1 exclusive, the 11 rows of pixels). A row's 8 bits are the columns
/// -1..=6 around the pen: capitals and some lower case letters reach one column into their left neighbour.
type PixelGlyph = (u8, (i32, i32, i32, i32), [u8; 11]);

/// Pillow's `ImageFont.load_default_imagefont()`: 6 px advance, 11 rows, one entry per code point 32..=255
/// (127..=160 have no glyph). Baked from the font's own metrics and bitmap sheet.
#[rustfmt::skip]
static PIXEL_GLYPHS: [PixelGlyph; 224] = [
    (6, (0, 1, 8, 9), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 3, 3, 9), [0x00, 0x00, 0x00, 0x30, 0x30, 0x30, 0x30, 0x00, 0x30, 0x00, 0x00]),
    (6, (1, 4, 3, 6), [0x00, 0x00, 0x00, 0x28, 0x28, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 5, 2, 10), [0x00, 0x00, 0x28, 0x28, 0x7C, 0x28, 0x28, 0x7C, 0x28, 0x28, 0x00]),
    (6, (0, 5, 1, 10), [0x00, 0x10, 0x3C, 0x64, 0x78, 0x3C, 0x0C, 0x6C, 0x78, 0x10, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x70, 0x54, 0x78, 0x10, 0x3C, 0x54, 0x1C, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0x38, 0x60, 0x30, 0x7C, 0xD8, 0x7C, 0x00, 0x00]),
    (6, (1, 4, 2, 5), [0x00, 0x00, 0x18, 0x10, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 4, 2, 10), [0x00, 0x00, 0x08, 0x10, 0x30, 0x30, 0x30, 0x30, 0x10, 0x08, 0x00]),
    (6, (1, 4, 2, 10), [0x00, 0x00, 0x20, 0x10, 0x18, 0x18, 0x18, 0x18, 0x10, 0x20, 0x00]),
    (6, (0, 4, 2, 6), [0x00, 0x00, 0x10, 0x78, 0x30, 0x48, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 5, 3, 8), [0x00, 0x00, 0x00, 0x10, 0x10, 0x7C, 0x10, 0x10, 0x00, 0x00, 0x00]),
    (6, (1, 4, 8, 11), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x10, 0x20]),
    (6, (0, 5, 5, 6), [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 3, 8, 9), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00]),
    (6, (1, 5, 2, 10), [0x00, 0x00, 0x04, 0x04, 0x08, 0x08, 0x10, 0x10, 0x20, 0x20, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x18, 0x78, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x0C, 0x18, 0x30, 0x6C, 0x7C, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x0C, 0x38, 0x0C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x0C, 0x1C, 0x2C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x7C, 0x60, 0x78, 0x6C, 0x0C, 0x4C, 0x78, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x60, 0x78, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x7C, 0x6C, 0x0C, 0x18, 0x18, 0x30, 0x30, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x6C, 0x38, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x38, 0x6C, 0x6C, 0x3C, 0x0C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (1, 3, 5, 9), [0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x30, 0x00, 0x00]),
    (6, (0, 3, 5, 11), [0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x30, 0x20, 0x40]),
    (6, (0, 4, 3, 8), [0x00, 0x00, 0x00, 0x18, 0x30, 0x60, 0x30, 0x18, 0x00, 0x00, 0x00]),
    (6, (0, 4, 4, 7), [0x00, 0x00, 0x00, 0x00, 0x78, 0x00, 0x78, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 5, 3, 8), [0x00, 0x00, 0x00, 0x30, 0x18, 0x0C, 0x18, 0x30, 0x00, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x38, 0x4C, 0x18, 0x30, 0x00, 0x30, 0x00, 0x00]),
    (6, (0, 6, 2, 10), [0x00, 0x00, 0x38, 0x64, 0x4C, 0x54, 0x54, 0x4E, 0x60, 0x38, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xF8, 0x6C, 0x78, 0x6C, 0x6C, 0xF8, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x3C, 0x6C, 0x60, 0x60, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xF8, 0x6C, 0x6C, 0x6C, 0x6C, 0xF8, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xFC, 0x60, 0x78, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xFC, 0x60, 0x78, 0x60, 0x60, 0xF0, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x38, 0x6C, 0x60, 0x7C, 0x6C, 0x3C, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xEE, 0x6C, 0x7C, 0x6C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (0, 4, 3, 9), [0x00, 0x00, 0x00, 0x78, 0x30, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0x3C, 0x18, 0x18, 0xD8, 0xD8, 0x70, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xEC, 0x68, 0x70, 0x78, 0x6C, 0xF6, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xF0, 0x60, 0x60, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xC4, 0x6C, 0x6C, 0x7C, 0x54, 0xD4, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xEE, 0x74, 0x74, 0x6C, 0x6C, 0xE4, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xF8, 0x6C, 0x6C, 0x78, 0x60, 0xF0, 0x00, 0x00]),
    (6, (0, 5, 3, 10), [0x00, 0x00, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x0C, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xF8, 0x6C, 0x6C, 0x78, 0x6C, 0xF6, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x3C, 0x64, 0x78, 0x1C, 0x4C, 0x78, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xFC, 0xB4, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xEE, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xEE, 0x6C, 0x28, 0x38, 0x38, 0x10, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xD6, 0x54, 0x54, 0x7C, 0x38, 0x28, 0x00, 0x00]),
    (6, (0, 6, 3, 9), [0x00, 0x00, 0x00, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x66, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0xE6, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x7C, 0x6C, 0x18, 0x30, 0x6C, 0x7C, 0x00, 0x00]),
    (6, (1, 4, 2, 10), [0x00, 0x00, 0x38, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x38, 0x00]),
    (6, (0, 4, 2, 10), [0x00, 0x00, 0x40, 0x40, 0x20, 0x20, 0x10, 0x10, 0x08, 0x08, 0x00]),
    (6, (1, 4, 2, 10), [0x00, 0x00, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x38, 0x00]),
    (6, (0, 5, 2, 5), [0x00, 0x00, 0x10, 0x38, 0x6C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 6, 10, 11), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E]),
    (6, (1, 4, 2, 5), [0x00, 0x00, 0x30, 0x10, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x38, 0x6C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (-1, 5, 2, 9), [0x00, 0x00, 0xE0, 0x60, 0x78, 0x6C, 0x6C, 0x6C, 0xF8, 0x00, 0x00]),
    (6, (0, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x38, 0x6C, 0x60, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x1C, 0x0C, 0x3C, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (0, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x38, 0x6C, 0x7C, 0x60, 0x3C, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x1C, 0x30, 0x7C, 0x30, 0x30, 0x30, 0x7C, 0x00, 0x00]),
    (6, (0, 6, 4, 11), [0x00, 0x00, 0x00, 0x00, 0x36, 0x6C, 0x6C, 0x6C, 0x3C, 0x0C, 0x78]),
    (6, (-1, 5, 2, 9), [0x00, 0x00, 0xE0, 0x60, 0x78, 0x6C, 0x6C, 0x6C, 0x6C, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x18, 0x00, 0x78, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 4, 2, 11), [0x00, 0x00, 0x18, 0x00, 0x78, 0x18, 0x18, 0x18, 0x18, 0x18, 0x70]),
    (6, (-1, 6, 2, 9), [0x00, 0x00, 0xE0, 0x60, 0x6C, 0x78, 0x70, 0x78, 0x6E, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x78, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (-1, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xF8, 0x7C, 0x54, 0x54, 0x54, 0x00, 0x00]),
    (6, (-1, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xD8, 0x6C, 0x6C, 0x6C, 0x6C, 0x00, 0x00]),
    (6, (0, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 5, 4, 11), [0x00, 0x00, 0x00, 0x00, 0xF8, 0x6C, 0x6C, 0x6C, 0x78, 0x60, 0xF0]),
    (6, (0, 6, 4, 11), [0x00, 0x00, 0x00, 0x00, 0x36, 0x6C, 0x6C, 0x6C, 0x3C, 0x0C, 0x1E]),
    (6, (0, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x6E, 0x3A, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (0, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x3C, 0x70, 0x3C, 0x0E, 0x7C, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x30, 0x30, 0x7C, 0x30, 0x30, 0x36, 0x1C, 0x00, 0x00]),
    (6, (-1, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (-1, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xEC, 0x6C, 0x38, 0x38, 0x10, 0x00, 0x00]),
    (6, (-1, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xD6, 0x54, 0x7C, 0x3C, 0x28, 0x00, 0x00]),
    (6, (0, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x76, 0x3C, 0x18, 0x3C, 0x6E, 0x00, 0x00]),
    (6, (-1, 6, 4, 11), [0x00, 0x00, 0x00, 0x00, 0xEE, 0x6C, 0x6C, 0x28, 0x38, 0x30, 0xE0]),
    (6, (0, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x7C, 0x58, 0x30, 0x6C, 0x7C, 0x00, 0x00]),
    (6, (1, 5, 2, 10), [0x00, 0x00, 0x0C, 0x18, 0x18, 0x30, 0x18, 0x18, 0x18, 0x0C, 0x00]),
    (6, (2, 3, 3, 10), [0x00, 0x00, 0x00, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x00]),
    (6, (0, 4, 2, 10), [0x00, 0x00, 0x60, 0x30, 0x30, 0x18, 0x30, 0x30, 0x30, 0x60, 0x00]),
    (6, (0, 5, 4, 6), [0x00, 0x00, 0x00, 0x00, 0x34, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (0, (0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 3, 4, 11), [0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x30, 0x30, 0x30, 0x30, 0x30]),
    (6, (0, 5, 2, 10), [0x00, 0x00, 0x10, 0x10, 0x3C, 0x64, 0x60, 0x3C, 0x10, 0x10, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x1C, 0x34, 0x10, 0x7C, 0x10, 0x32, 0x7C, 0x00, 0x00]),
    (6, (0, 5, 3, 8), [0x00, 0x00, 0x00, 0x44, 0x38, 0x28, 0x38, 0x44, 0x00, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x66, 0x24, 0x7E, 0x18, 0x7E, 0x18, 0x3C, 0x00, 0x00]),
    (6, (2, 3, 2, 11), [0x00, 0x00, 0x10, 0x10, 0x10, 0x10, 0x00, 0x10, 0x10, 0x10, 0x10]),
    (6, (0, 5, 2, 10), [0x00, 0x00, 0x3C, 0x24, 0x30, 0x48, 0x24, 0x18, 0x48, 0x78, 0x00]),
    (6, (1, 4, 3, 4), [0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x18, 0x24, 0x5A, 0x52, 0x5A, 0x24, 0x18, 0x00, 0x00]),
    (6, (1, 5, 2, 7), [0x00, 0x00, 0x38, 0x04, 0x34, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x00]),
    (6, (-1, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x36, 0x6C, 0xD8, 0x6C, 0x36, 0x00, 0x00]),
    (6, (0, 5, 4, 7), [0x00, 0x00, 0x00, 0x00, 0x7C, 0x04, 0x04, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 5, 5, 6), [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 7, 2, 9), [0x00, 0x00, 0x1C, 0x22, 0x5D, 0x59, 0x55, 0x22, 0x1C, 0x00, 0x00]),
    (6, (0, 4, 3, 4), [0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 4, 2, 5), [0x00, 0x00, 0x30, 0x48, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (0, 5, 3, 9), [0x00, 0x00, 0x00, 0x10, 0x10, 0x7C, 0x10, 0x00, 0x7C, 0x00, 0x00]),
    (6, (1, 4, 2, 6), [0x00, 0x00, 0x18, 0x28, 0x10, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 4, 2, 6), [0x00, 0x00, 0x38, 0x10, 0x08, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (2, 4, 2, 4), [0x00, 0x00, 0x08, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (-1, 6, 4, 11), [0x00, 0x00, 0x00, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x7E, 0x40, 0x40]),
    (6, (0, 6, 2, 10), [0x00, 0x00, 0x3E, 0x54, 0x54, 0x34, 0x14, 0x14, 0x14, 0x36, 0x00]),
    (6, (1, 3, 5, 6), [0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 4, 8, 11), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x08, 0x30]),
    (6, (1, 4, 2, 6), [0x00, 0x00, 0x30, 0x10, 0x10, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00]),
    (6, (1, 5, 2, 7), [0x00, 0x00, 0x18, 0x24, 0x18, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x00]),
    (6, (-1, 6, 4, 9), [0x00, 0x00, 0x00, 0x00, 0xD8, 0x6C, 0x36, 0x6C, 0xD8, 0x00, 0x00]),
    (6, (-1, 6, 2, 9), [0x00, 0x00, 0xC0, 0x44, 0x48, 0xF4, 0x2C, 0x5E, 0x04, 0x00, 0x00]),
    (6, (-1, 6, 2, 9), [0x00, 0x00, 0xC0, 0x44, 0x48, 0xF6, 0x2A, 0x44, 0x0E, 0x00, 0x00]),
    (6, (-1, 6, 2, 9), [0x00, 0x00, 0xE0, 0x44, 0x28, 0xD4, 0x2C, 0x5E, 0x04, 0x00, 0x00]),
    (6, (0, 5, 4, 11), [0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x18, 0x18, 0x30, 0x64, 0x38]),
    (6, (-1, 6, 0, 9), [0x20, 0x10, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x10, 0x20, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x10, 0x28, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x34, 0x48, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x28, 0x00, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x30, 0x48, 0x30, 0x78, 0x38, 0x28, 0x7C, 0x6C, 0xEE, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0x7E, 0x3A, 0x6C, 0x78, 0xDA, 0xDE, 0x00, 0x00]),
    (6, (0, 5, 3, 11), [0x00, 0x00, 0x00, 0x3C, 0x6C, 0x60, 0x60, 0x6C, 0x38, 0x08, 0x30]),
    (6, (-1, 5, 0, 9), [0x20, 0x10, 0x00, 0xFC, 0x64, 0x78, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (-1, 5, 0, 9), [0x10, 0x20, 0x00, 0xFC, 0x64, 0x78, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (-1, 5, 0, 9), [0x20, 0x50, 0x00, 0xFC, 0x64, 0x78, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (-1, 5, 1, 9), [0x00, 0x50, 0x00, 0xFC, 0x64, 0x78, 0x60, 0x6C, 0xFC, 0x00, 0x00]),
    (6, (0, 4, 0, 9), [0x20, 0x10, 0x00, 0x78, 0x30, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (0, 4, 0, 9), [0x10, 0x20, 0x00, 0x78, 0x30, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (0, 4, 0, 9), [0x20, 0x50, 0x00, 0x78, 0x30, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (0, 4, 1, 9), [0x00, 0x50, 0x00, 0x78, 0x30, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xF8, 0x6C, 0xF4, 0x64, 0x6C, 0xF8, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x34, 0x48, 0x00, 0xEE, 0x64, 0x74, 0x7C, 0x6C, 0xEC, 0x00, 0x00]),
    (6, (0, 5, 0, 9), [0x20, 0x10, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 0, 9), [0x10, 0x20, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 0, 9), [0x10, 0x28, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 0, 9), [0x34, 0x48, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x28, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 3, 8), [0x00, 0x00, 0x00, 0x44, 0x28, 0x10, 0x28, 0x44, 0x00, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0x3A, 0x6C, 0x7C, 0x6C, 0x6C, 0xB8, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x20, 0x10, 0x00, 0xEE, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x08, 0x10, 0x00, 0xEE, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x10, 0x28, 0x00, 0xEE, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x28, 0x00, 0xEE, 0x6C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 0, 9), [0x04, 0x08, 0x00, 0xE6, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x00, 0x00]),
    (6, (-1, 5, 3, 9), [0x00, 0x00, 0x00, 0xE0, 0x78, 0x6C, 0x6C, 0x78, 0xE0, 0x00, 0x00]),
    (6, (-1, 6, 3, 9), [0x00, 0x00, 0x00, 0x38, 0x68, 0x7C, 0x66, 0x66, 0xEC, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x10, 0x08, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x08, 0x10, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x10, 0x28, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x34, 0x48, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x28, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 0, 9), [0x18, 0x24, 0x18, 0x00, 0x38, 0x4C, 0x3C, 0x6C, 0x7E, 0x00, 0x00]),
    (6, (-1, 5, 4, 9), [0x00, 0x00, 0x00, 0x00, 0x6C, 0xB4, 0x7C, 0xB0, 0xDC, 0x00, 0x00]),
    (6, (0, 5, 4, 11), [0x00, 0x00, 0x00, 0x00, 0x38, 0x6C, 0x60, 0x6C, 0x38, 0x08, 0x30]),
    (6, (0, 5, 1, 9), [0x00, 0x20, 0x10, 0x00, 0x38, 0x6C, 0x7C, 0x60, 0x3C, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x10, 0x20, 0x00, 0x38, 0x6C, 0x7C, 0x60, 0x3C, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x10, 0x28, 0x00, 0x38, 0x6C, 0x7C, 0x60, 0x3C, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x28, 0x00, 0x38, 0x6C, 0x7C, 0x60, 0x3C, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x10, 0x08, 0x00, 0x38, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x08, 0x10, 0x00, 0x38, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 1, 9), [0x00, 0x10, 0x28, 0x00, 0x38, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 6, 2, 9), [0x00, 0x00, 0x28, 0x00, 0x38, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x68, 0x30, 0x58, 0x3C, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x34, 0x48, 0x00, 0xD8, 0x6C, 0x6C, 0x6C, 0x6E, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x20, 0x10, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x10, 0x20, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x10, 0x28, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 1, 9), [0x00, 0x34, 0x48, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 2, 9), [0x00, 0x00, 0x28, 0x00, 0x38, 0x6C, 0x6C, 0x6C, 0x38, 0x00, 0x00]),
    (6, (0, 5, 3, 8), [0x00, 0x00, 0x00, 0x10, 0x00, 0x7C, 0x00, 0x10, 0x00, 0x00, 0x00]),
    (6, (0, 5, 3, 10), [0x00, 0x00, 0x00, 0x04, 0x38, 0x6C, 0x7C, 0x6C, 0x38, 0x40, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x20, 0x10, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x10, 0x20, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (-1, 6, 1, 9), [0x00, 0x10, 0x28, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (-1, 6, 2, 9), [0x00, 0x00, 0x28, 0x00, 0xEC, 0x6C, 0x6C, 0x6C, 0x3E, 0x00, 0x00]),
    (6, (-1, 6, 1, 11), [0x00, 0x08, 0x10, 0x00, 0xEE, 0x6C, 0x6C, 0x28, 0x38, 0x30, 0xF0]),
    (6, (-1, 5, 2, 11), [0x00, 0x00, 0xE0, 0x60, 0x78, 0x6C, 0x6C, 0x6C, 0x78, 0x60, 0xF0]),
    (6, (-1, 6, 2, 11), [0x00, 0x00, 0x28, 0x00, 0xEE, 0x6C, 0x6C, 0x28, 0x38, 0x30, 0xF0]),
];

fn pixel_glyph(c: char) -> Option<&'static PixelGlyph> {
    let code = c as u32;
    if (32..=255).contains(&code) {
        Some(&PIXEL_GLYPHS[(code - 32) as usize])
    } else {
        None
    }
}

fn pixel_length(text: &str) -> f64 {
    text.chars()
        .filter_map(pixel_glyph)
        .map(|g| g.0 as f64)
        .sum()
}

fn pixel_draw(img: &mut GrayImage, x: f64, y: f64, text: &str, fill: u8) {
    // Pillow renders the text into a mask exactly as wide as its advance (11 rows high) by copying each
    // glyph's rectangle over it, blank pixels included, so a glyph can erase the overlap of its left
    // neighbour. Then the mask is pasted at int(x), int(y), where its set pixels take the fill.
    let width = pixel_length(text) as i32;
    if width == 0 {
        return;
    }
    let mut mask = vec![false; width as usize * PIXEL_HEIGHT as usize];
    let mut pen = 0i32;
    for c in text.chars() {
        let Some(&(advance, (x0, x1, y0, y1), rows)) = pixel_glyph(c) else {
            continue;
        };
        if advance == 0 {
            continue;
        }
        for row in y0.max(0)..y1.min(PIXEL_HEIGHT) {
            for column in x0..x1 {
                let at = pen + column;
                if at >= 0 && at < width {
                    mask[row as usize * width as usize + at as usize] =
                        rows[row as usize] & (0x80 >> (column + 1)) != 0;
                }
            }
        }
        pen += advance as i32;
    }
    let (left, top) = (x as i64, y as i64);
    for (i, &set) in mask.iter().enumerate() {
        let (px, py) = (
            left + (i as i64 % width as i64),
            top + (i as i64 / width as i64),
        );
        if set && px >= 0 && py >= 0 && px < img.width() as i64 && py < img.height() as i64 {
            img.put_pixel(px as u32, py as u32, image::Luma([fill]));
        }
    }
}

// --- Face ----------------------------------------------------------------------------------

/// A font at one pixel size.
#[derive(Clone)]
pub struct Face(Inner);

#[derive(Clone)]
enum Inner {
    Pixel,
    Vector(Arc<VectorFace>),
}

impl Face {
    /// `ImageFont.getlength`: the advance of the text in pixels, whole pixels per glyph like Pillow's
    /// hinted layout (plus legacy `kern` pairs). GPOS kerning and shaping are not applied, as in Pillow without raqm.
    pub fn length(&self, text: &str) -> f64 {
        match &self.0 {
            Inner::Pixel => pixel_length(text),
            Inner::Vector(face) => face.length(text, false),
        }
    }

    /// `ImageDraw.textlength` with `fontmode = "1"`: what the Python engine measures before it draws
    /// mono text (it differs from `length` where the font has its own `hdmx` advances for this size).
    pub fn length_mono(&self, text: &str) -> f64 {
        match &self.0 {
            Inner::Pixel => pixel_length(text),
            Inner::Vector(face) => face.length(text, true),
        }
    }

    /// `getmetrics()`: (ascent, descent) in whole pixels at this size.
    pub fn metrics(&self) -> (i32, i32) {
        match &self.0 {
            Inner::Pixel => (PIXEL_HEIGHT, 0),
            Inner::Vector(face) => (face.ascent, face.descent),
        }
    }

    /// `getname()[1]`, the style inside the file ("Bold", "Regular", ...).
    pub fn style_name(&self) -> String {
        match &self.0 {
            Inner::Pixel => String::new(),
            Inner::Vector(face) => face.style.clone(),
        }
    }

    /// Whether the font draws something for this character other than its missing-glyph box.
    pub fn has_glyph(&self, c: char) -> bool {
        match &self.0 {
            Inner::Pixel => pixel_glyph(c)
                .map(|g| g.0 > 0 && g.2.iter().any(|&r| r != 0))
                .unwrap_or(false),
            Inner::Vector(face) => face.has_glyph(c),
        }
    }

    /// `ImageDraw.text((x, y), text, font=face, fill=fill)` with anchor "la": x is the left origin, y the top
    /// of the ascender line. `mono` is `fontmode = "1"`: no anti-aliasing, a pixel is on or off.
    pub fn draw(&self, img: &mut GrayImage, x: f64, y: f64, text: &str, fill: u8, mono: bool) {
        match &self.0 {
            Inner::Pixel => pixel_draw(img, x, y, text, fill),
            Inner::Vector(face) => face.draw(img, x, y, text, fill, mono),
        }
    }

    /// The pixel size this face was opened at.
    pub fn size(&self) -> u32 {
        match &self.0 {
            Inner::Pixel => PIXEL_HEIGHT as u32,
            Inner::Vector(face) => face.size,
        }
    }

    pub fn is_pixel(&self) -> bool {
        matches!(self.0, Inner::Pixel)
    }
}

/// Pillow's built-in bitmap font.
pub fn pixel_face() -> Face {
    Face(Inner::Pixel)
}

/// `ImageFont.truetype(path, size, index=index)`.
pub fn open_face(path: &Path, size: u32, index: u32) -> Result<Face, AssetError> {
    let data = font_data(path)?;
    face_from_data(data, size, index).ok_or_else(|| {
        AssetError(format!(
            "Font {} can't render at size {size}.",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("?")
        ))
    })
}

fn face_from_data(data: Arc<Vec<u8>>, size: u32, index: u32) -> Option<Face> {
    VectorFace::open(data, index, size).map(|face| Face(Inner::Vector(Arc::new(face))))
}

/// Pillow's `load_default(size)` stands in for a font this machine lacks; here any sans face will do.
fn fallback_sans(size: u32) -> Face {
    const STEMS: [&str; 12] = [
        "helvetica",
        "arial",
        "dejavusans",
        "dejavu sans",
        "liberationsans-regular",
        "liberation sans",
        "notosans-regular",
        "noto sans",
        "freesans",
        "roboto-regular",
        "opensans-regular",
        "segoeui",
    ];
    let files = font_files();
    for stem in STEMS {
        if let Some(path) = files.get(stem) {
            if let Ok(face) = open_face(path, size, 0) {
                return face;
            }
        }
    }
    for (stem, path) in files {
        if stem.contains("sans") && !stem.contains("mono") && !stem.contains("symbol") {
            if let Ok(face) = open_face(path, size, 0) {
                return face;
            }
        }
    }
    pixel_face()
}

type LoadKey = (String, u32, bool);

/// `assets.load_font`: a friendly name, any installed font's name, or, for trusted callers, a path or URL.
pub fn load_font(name: &str, size: u32, external: bool) -> Result<Face, AssetError> {
    static CACHE: OnceLock<Mutex<HashMap<LoadKey, Face>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (name.to_string(), size, external);
    if let Ok(guard) = cache.lock() {
        if let Some(face) = guard.get(&key) {
            return Ok(face.clone());
        }
    }
    let face = load_font_uncached(name, size, external)?;
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 128 {
            guard.clear();
        }
        guard.insert(key, face.clone());
    }
    Ok(face)
}

fn load_font_uncached(name: &str, size: u32, external: bool) -> Result<Face, AssetError> {
    if name == PIXEL_FONT {
        return Ok(pixel_face());
    }
    let files = font_files();
    let alias = FONT_ALIASES.iter().find(|(n, _, _)| *n == name);
    if let Some((_, stem, index)) = alias {
        if let Some(path) = files.get(&stem.to_lowercase()) {
            return open_face(path, size, *index);
        }
    }
    if let Some(path) = files.get(&name.to_lowercase()) {
        return open_face(path, size, 0);
    }
    if external
        && (name.starts_with("http://")
            || name.starts_with("https://")
            || expand_home(name).exists())
    {
        let data = read_source(name, true)?;
        return face_from_data(Arc::new(data), size, 0)
            .ok_or_else(|| AssetError(format!("Couldn't read {name} as a font.")));
    }
    if alias.is_some() {
        return Ok(fallback_sans(size)); // Not on this machine: still prints.
    }
    Err(AssetError(format!(
        "Unknown font {}. See catalog fonts.",
        crate::assets::py_repr(name)
    )))
}

fn expand_home(name: &str) -> PathBuf {
    if let Some(rest) = name.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(name)
}

// --- craft's faces -------------------------------------------------------------------------

/// `craft._open_face`: a display face by name, the Bold/Italic/... of a collection picked by its style name.
pub fn craft_face(name: &str, size: u32) -> Face {
    static CACHE: OnceLock<Mutex<HashMap<(String, u32), Face>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (name.to_string(), size);
    if let Ok(guard) = cache.lock() {
        if let Some(face) = guard.get(&key) {
            return face.clone();
        }
    }
    let face = craft_face_uncached(name, size);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 128 {
            guard.clear();
        }
        guard.insert(key, face.clone());
    }
    face
}

fn craft_face_uncached(name: &str, size: u32) -> Face {
    let Some((_, stem, style)) = CRAFT_FACES.iter().find(|(n, _, _)| *n == name) else {
        return fallback_sans(size);
    };
    let Some(path) = font_files().get(*stem) else {
        return fallback_sans(size);
    };
    if let Some(style) = style {
        for index in 0..32 {
            match open_face(path, size, index) {
                Ok(face) => {
                    if face.style_name() == *style {
                        return face;
                    }
                }
                Err(_) => break,
            }
        }
    }
    open_face(path, size, 0).unwrap_or_else(|_| fallback_sans(size))
}

/// Python's `str.isspace` for one character.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `craft._has_glyph`: does this display face draw the character, measured at 64 px?
pub fn craft_has_glyph(name: &str, c: char) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<(String, char), bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (name.to_string(), c);
    if let Ok(guard) = cache.lock() {
        if let Some(&known) = guard.get(&key) {
            return known;
        }
    }
    let has = craft_face(name, 64).has_glyph(c);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, has);
    }
    has
}

/// `craft._face_for`: the display face, unless the text needs a script it lacks, then a face that has it.
pub fn craft_face_for(name: &str, text: &str, size: u32) -> Face {
    if text
        .chars()
        .all(|c| py_isspace(c) || craft_has_glyph(name, c))
    {
        return craft_face(name, size);
    }
    let files = font_files();
    for stem in FALLBACK_STEMS {
        if let Some(path) = files.get(stem) {
            if let Ok(face) = open_face(path, size, 0) {
                return face;
            }
        }
    }
    craft_face(name, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (file stem, face index, size, (ascent, descent), style, [(text, getlength, getlength with fontmode "1")])
    /// measured with Pillow 13 / FreeType 2.14 on macOS. Skipped for fonts this machine doesn't have.
    type Reference = (
        &'static str,
        u32,
        u32,
        (i32, i32),
        &'static str,
        &'static [(&'static str, f64, f64)],
    );
    #[rustfmt::skip]
    const REFERENCE: &[Reference] = &[
    ("helvetica", 0, 12, (10, 3), "Regular", &[("H", 9.0, 9.0), ("Hello", 29.0, 27.0), ("AVAV", 32.0, 34.0), ("To.", 17.0, 17.0), ("Hello World 123", 88.0, 86.0), ("Blåbær", 40.0, 39.0)]),
    ("helvetica", 0, 16, (13, 4), "Regular", &[("H", 12.0, 11.0), ("Hello", 38.0, 37.0), ("AVAV", 44.0, 46.0), ("To.", 23.0, 23.0), ("Hello World 123", 115.0, 115.0), ("Blåbær", 52.0, 52.0)]),
    ("helvetica", 0, 18, (14, 5), "Regular", &[("H", 13.0, 12.0), ("Hello", 41.0, 42.0), ("AVAV", 48.0, 52.0), ("To.", 26.0, 27.0), ("Hello World 123", 128.0, 131.0), ("Blåbær", 58.0, 59.0)]),
    ("helvetica", 0, 24, (19, 6), "Regular", &[("H", 17.0, 18.0), ("Hello", 53.0, 55.0), ("AVAV", 64.0, 68.0), ("To.", 35.0, 35.0), ("Hello World 123", 168.0, 172.0), ("Blåbær", 76.0, 76.0)]),
    ("helvetica", 0, 48, (37, 12), "Regular", &[("H", 35.0, 35.0), ("Hello", 111.0, 108.0), ("AVAV", 128.0, 130.0), ("To.", 69.0, 68.0), ("Hello World 123", 344.0, 338.0), ("Blåbær", 156.0, 156.0)]),
    ("helvetica", 0, 100, (77, 23), "Regular", &[("H", 72.0, 72.0), ("Hello", 228.0, 226.0), ("AVAV", 268.0, 266.0), ("To.", 145.0, 143.0), ("Hello World 123", 713.0, 707.0), ("Blåbær", 323.0, 322.0)]),
    ("helvetica", 1, 12, (10, 3), "Bold", &[("H", 9.0, 9.0), ("Hello", 29.0, 30.0), ("AVAV", 34.0, 34.0), ("To.", 17.0, 19.0), ("Hello World 123", 89.0, 91.0), ("Blåbær", 42.0, 42.0)]),
    ("helvetica", 1, 16, (13, 4), "Bold", &[("H", 12.0, 12.0), ("Hello", 39.0, 39.0), ("AVAV", 46.0, 46.0), ("To.", 24.0, 24.0), ("Hello World 123", 119.0, 119.0), ("Blåbær", 55.0, 55.0)]),
    ("helvetica", 1, 18, (14, 5), "Bold", &[("H", 13.0, 13.0), ("Hello", 44.0, 45.0), ("AVAV", 50.0, 50.0), ("To.", 27.0, 28.0), ("Hello World 123", 135.0, 138.0), ("Blåbær", 62.0, 62.0)]),
    ("helvetica", 1, 24, (19, 6), "Bold", &[("H", 17.0, 17.0), ("Hello", 59.0, 59.0), ("AVAV", 66.0, 66.0), ("To.", 37.0, 37.0), ("Hello World 123", 181.0, 181.0), ("Blåbær", 82.0, 82.0)]),
    ("helvetica", 1, 48, (37, 12), "Bold", &[("H", 35.0, 35.0), ("Hello", 117.0, 118.0), ("AVAV", 134.0, 134.0), ("To.", 71.0, 72.0), ("Hello World 123", 359.0, 361.0), ("Blåbær", 166.0, 166.0)]),
    ("helvetica", 1, 100, (77, 23), "Bold", &[("H", 72.0, 72.0), ("Hello", 245.0, 245.0), ("AVAV", 278.0, 278.0), ("To.", 150.0, 150.0), ("Hello World 123", 752.0, 752.0), ("Blåbær", 345.0, 345.0)]),
    ("times", 0, 12, (9, 3), "Regular", &[("H", 9.0, 9.0), ("Hello", 26.0, 26.0), ("AVAV", 36.0, 32.0), ("To.", 16.0, 16.0), ("Hello World 123", 80.0, 81.0), ("Blåbær", 34.0, 34.0)]),
    ("times", 0, 16, (12, 4), "Regular", &[("H", 12.0, 12.0), ("Hello", 35.0, 35.0), ("AVAV", 48.0, 46.0), ("To.", 22.0, 20.0), ("Hello World 123", 107.0, 107.0), ("Blåbær", 46.0, 47.0)]),
    ("times", 0, 18, (14, 5), "Regular", &[("H", 13.0, 13.0), ("Hello", 40.0, 38.0), ("AVAV", 52.0, 50.0), ("To.", 25.0, 24.0), ("Hello World 123", 123.0, 120.0), ("Blåbær", 52.0, 52.0)]),
    ("times", 0, 24, (18, 6), "Regular", &[("H", 17.0, 17.0), ("Hello", 54.0, 52.0), ("AVAV", 68.0, 68.0), ("To.", 33.0, 31.0), ("Hello World 123", 164.0, 162.0), ("Blåbær", 70.0, 69.0)]),
    ("times", 0, 48, (36, 12), "Regular", &[("H", 35.0, 35.0), ("Hello", 106.0, 104.0), ("AVAV", 140.0, 134.0), ("To.", 65.0, 61.0), ("Hello World 123", 324.0, 320.0), ("Blåbær", 138.0, 137.0)]),
    ("times", 0, 100, (75, 25), "Regular", &[("H", 72.0, 72.0), ("Hello", 222.0, 216.0), ("AVAV", 288.0, 274.0), ("To.", 136.0, 131.0), ("Hello World 123", 677.0, 670.0), ("Blåbær", 289.0, 285.0)]),
    ("menlo", 0, 12, (12, 3), "Regular", &[("H", 7.0, 7.0), ("Hello", 35.0, 35.0), ("AVAV", 28.0, 28.0), ("To.", 21.0, 21.0), ("Hello World 123", 105.0, 105.0), ("Blåbær", 42.0, 42.0)]),
    ("menlo", 0, 16, (15, 4), "Regular", &[("H", 10.0, 10.0), ("Hello", 50.0, 50.0), ("AVAV", 40.0, 40.0), ("To.", 30.0, 30.0), ("Hello World 123", 150.0, 150.0), ("Blåbær", 60.0, 61.0)]),
    ("menlo", 0, 18, (17, 5), "Regular", &[("H", 11.0, 11.0), ("Hello", 55.0, 55.0), ("AVAV", 44.0, 44.0), ("To.", 33.0, 33.0), ("Hello World 123", 165.0, 165.0), ("Blåbær", 66.0, 67.0)]),
    ("menlo", 0, 24, (23, 6), "Regular", &[("H", 14.0, 14.0), ("Hello", 70.0, 70.0), ("AVAV", 56.0, 56.0), ("To.", 42.0, 42.0), ("Hello World 123", 210.0, 210.0), ("Blåbær", 84.0, 85.0)]),
    ("menlo", 0, 48, (45, 12), "Regular", &[("H", 29.0, 29.0), ("Hello", 145.0, 145.0), ("AVAV", 116.0, 116.0), ("To.", 87.0, 87.0), ("Hello World 123", 435.0, 435.0), ("Blåbær", 174.0, 174.0)]),
    ("menlo", 0, 100, (93, 24), "Regular", &[("H", 60.0, 60.0), ("Hello", 300.0, 300.0), ("AVAV", 240.0, 240.0), ("To.", 180.0, 180.0), ("Hello World 123", 900.0, 900.0), ("Blåbær", 360.0, 361.0)]),
    ("impact", 0, 12, (13, 3), "Regular", &[("H", 7.0, 7.0), ("Hello", 25.0, 25.0), ("AVAV", 24.0, 24.0), ("To.", 14.0, 14.0), ("Hello World 123", 75.0, 75.0), ("Blåbær", 35.0, 35.0)]),
    ("impact", 0, 16, (17, 4), "Regular", &[("H", 9.0, 9.0), ("Hello", 33.0, 33.0), ("AVAV", 32.0, 28.0), ("To.", 18.0, 18.0), ("Hello World 123", 100.0, 100.0), ("Blåbær", 47.0, 47.0)]),
    ("impact", 0, 18, (19, 4), "Regular", &[("H", 10.0, 10.0), ("Hello", 38.0, 38.0), ("AVAV", 36.0, 36.0), ("To.", 20.0, 20.0), ("Hello World 123", 114.0, 114.0), ("Blåbær", 53.0, 53.0)]),
    ("impact", 0, 24, (25, 6), "Regular", &[("H", 13.0, 13.0), ("Hello", 51.0, 49.0), ("AVAV", 49.96875, 47.96875), ("To.", 27.0, 27.0), ("Hello World 123", 153.0, 150.0), ("Blåbær", 71.0, 70.0)]),
    ("impact", 0, 48, (49, 11), "Regular", &[("H", 27.0, 27.0), ("Hello", 103.0, 103.0), ("AVAV", 97.953125, 95.953125), ("To.", 56.0, 56.0), ("Hello World 123", 305.0, 304.0), ("Blåbær", 142.0, 142.0)]),
    ("impact", 0, 100, (101, 22), "Regular", &[("H", 56.0, 56.0), ("Hello", 212.0, 212.0), ("AVAV", 205.921875, 205.921875), ("To.", 115.0, 115.0), ("Hello World 123", 636.0, 636.0), ("Blåbær", 295.0, 295.0)]),
    ("futura", 1, 12, (13, 4), "Medium Italic", &[("H", 9.0, 9.0), ("Hello", 29.0, 29.0), ("AVAV", 32.0, 32.0), ("To.", 17.0, 17.0), ("Hello World 123", 91.984375, 91.984375), ("Blåbær", 40.0, 40.0)]),
    ("futura", 1, 16, (17, 5), "Medium Italic", &[("H", 11.0, 11.0), ("Hello", 38.0, 38.0), ("AVAV", 43.953125, 43.953125), ("To.", 23.0, 23.0), ("Hello World 123", 124.96875, 124.96875), ("Blåbær", 53.0, 53.0)]),
    ("futura", 1, 18, (19, 5), "Medium Italic", &[("H", 13.0, 13.0), ("Hello", 44.0, 44.0), ("AVAV", 49.953125, 49.953125), ("To.", 25.984375, 25.984375), ("Hello World 123", 141.96875, 141.96875), ("Blåbær", 60.0, 60.0)]),
    ("futura", 1, 24, (25, 7), "Medium Italic", &[("H", 17.0, 17.0), ("Hello", 56.0, 56.0), ("AVAV", 65.9375, 65.9375), ("To.", 32.984375, 32.984375), ("Hello World 123", 183.953125, 183.953125), ("Blåbær", 79.0, 79.0)]),
    ("futura", 1, 48, (50, 13), "Medium Italic", &[("H", 34.0, 34.0), ("Hello", 114.0, 114.0), ("AVAV", 131.84375, 131.84375), ("To.", 66.96875, 66.96875), ("Hello World 123", 372.875, 372.875), ("Blåbær", 158.0, 158.0)]),
    ("futura", 1, 100, (104, 27), "Medium Italic", &[("H", 72.0, 72.0), ("Hello", 240.0, 240.0), ("AVAV", 275.6875, 275.6875), ("To.", 138.921875, 138.921875), ("Hello World 123", 777.765625, 777.765625), ("Blåbær", 331.0, 331.0)]),
    ("arial black", 0, 12, (14, 4), "Regular", &[("H", 10.0, 10.0), ("Hello", 34.0, 34.0), ("AVAV", 36.0, 36.0), ("To.", 21.0, 21.0), ("Hello World 123", 103.0, 103.0), ("Blåbær", 46.0, 46.0)]),
    ("arial black", 0, 16, (18, 5), "Regular", &[("H", 13.0, 13.0), ("Hello", 45.0, 45.0), ("AVAV", 47.953125, 47.953125), ("To.", 27.984375, 27.984375), ("Hello World 123", 138.0, 138.0), ("Blåbær", 62.0, 62.0)]),
    ("arial black", 0, 18, (20, 6), "Regular", &[("H", 15.0, 15.0), ("Hello", 51.0, 51.0), ("AVAV", 55.953125, 55.953125), ("To.", 30.984375, 30.984375), ("Hello World 123", 155.0, 155.0), ("Blåbær", 70.0, 70.0)]),
    ("arial black", 0, 24, (27, 8), "Regular", &[("H", 20.0, 20.0), ("Hello", 68.0, 68.0), ("AVAV", 75.953125, 75.953125), ("To.", 40.96875, 40.96875), ("Hello World 123", 207.0, 207.0), ("Blåbær", 94.0, 94.0)]),
    ("arial black", 0, 48, (53, 15), "Regular", &[("H", 40.0, 40.0), ("Hello", 136.0, 136.0), ("AVAV", 147.859375, 147.859375), ("To.", 82.9375, 82.9375), ("Hello World 123", 413.015625, 413.015625), ("Blåbær", 186.0, 186.0)]),
    ("arial black", 0, 100, (111, 31), "Regular", &[("H", 83.0, 83.0), ("Hello", 283.0, 283.0), ("AVAV", 311.71875, 311.71875), ("To.", 171.859375, 171.859375), ("Hello World 123", 861.0, 861.0), ("Blåbær", 389.0, 389.0)]),
    ("courier new bold", 0, 12, (10, 4), "Bold", &[("H", 7.0, 7.0), ("Hello", 35.0, 35.0), ("AVAV", 28.0, 28.0), ("To.", 21.0, 21.0), ("Hello World 123", 105.0, 105.0), ("Blåbær", 42.0, 42.0)]),
    ("courier new bold", 0, 16, (14, 5), "Bold", &[("H", 10.0, 10.0), ("Hello", 50.0, 50.0), ("AVAV", 40.0, 40.0), ("To.", 30.0, 30.0), ("Hello World 123", 150.0, 150.0), ("Blåbær", 60.0, 60.0)]),
    ("courier new bold", 0, 18, (15, 6), "Bold", &[("H", 11.0, 11.0), ("Hello", 55.0, 55.0), ("AVAV", 44.0, 44.0), ("To.", 33.0, 33.0), ("Hello World 123", 165.0, 165.0), ("Blåbær", 66.0, 66.0)]),
    ("courier new bold", 0, 24, (20, 8), "Bold", &[("H", 14.0, 14.0), ("Hello", 70.0, 70.0), ("AVAV", 56.0, 56.0), ("To.", 42.0, 42.0), ("Hello World 123", 210.0, 210.0), ("Blåbær", 84.0, 84.0)]),
    ("courier new bold", 0, 48, (40, 15), "Bold", &[("H", 29.0, 29.0), ("Hello", 145.0, 145.0), ("AVAV", 116.0, 116.0), ("To.", 87.0, 87.0), ("Hello World 123", 435.0, 435.0), ("Blåbær", 174.0, 174.0)]),
    ("courier new bold", 0, 100, (84, 31), "Bold", &[("H", 60.0, 60.0), ("Hello", 300.0, 300.0), ("AVAV", 240.0, 240.0), ("To.", 180.0, 180.0), ("Hello World 123", 900.0, 900.0), ("Blåbær", 360.0, 360.0)]),
    ("georgia bold", 0, 12, (11, 3), "Bold", &[("H", 11.0, 11.0), ("Hello", 34.0, 35.0), ("AVAV", 36.0, 40.0), ("To.", 20.0, 20.0), ("Hello World 123", 102.0, 104.0), ("Blåbær", 44.0, 48.0)]),
    ("georgia bold", 0, 16, (15, 4), "Bold", &[("H", 15.0, 14.0), ("Hello", 46.0, 45.0), ("AVAV", 48.0, 48.0), ("To.", 26.0, 27.0), ("Hello World 123", 135.0, 135.0), ("Blåbær", 60.0, 59.0)]),
    ("georgia bold", 0, 18, (17, 4), "Bold", &[("H", 16.0, 16.0), ("Hello", 49.0, 49.0), ("AVAV", 56.0, 56.0), ("To.", 29.0, 29.0), ("Hello World 123", 148.0, 148.0), ("Blåbær", 67.0, 67.0)]),
    ("georgia bold", 0, 24, (23, 6), "Bold", &[("H", 22.0, 22.0), ("Hello", 67.0, 67.0), ("AVAV", 72.0, 72.0), ("To.", 39.0, 39.0), ("Hello World 123", 199.0, 199.0), ("Blåbær", 89.0, 89.0)]),
    ("georgia bold", 0, 48, (45, 11), "Bold", &[("H", 44.0, 44.0), ("Hello", 136.0, 136.0), ("AVAV", 146.0, 146.0), ("To.", 80.0, 80.0), ("Hello World 123", 403.0, 403.0), ("Blåbær", 179.0, 179.0)]),
    ("georgia bold", 0, 100, (92, 22), "Bold", &[("H", 91.0, 91.0), ("Hello", 280.0, 280.0), ("AVAV", 304.0, 304.0), ("To.", 165.0, 165.0), ("Hello World 123", 833.0, 833.0), ("Blåbær", 373.0, 373.0)]),
    ];

    /// (text, x, y, getlength, FNV-1a of the 110x16 "L" canvas after `ImageDraw.text` with fontmode "1")
    #[rustfmt::skip]
    const PIXEL_REFERENCE: &[(&str, f64, f64, f64, u64)] = &[
    ("Hello", 1.0, 1.0, 30.0, 3052975342922005146),
    ("Hgjy 123!", 2.7, 0.9, 54.0, 12617902170751950648),
    ("Blåbær é ÿ ¿", 0.0, 2.0, 72.0, 14651282585106225129),
    ("~{|}", 5.0, 1.0, 24.0, 9968698469817457904),
    ("clipped text here", -9.0, -3.0, 102.0, 14748156450197214774),
    ("", 3.0, 3.0, 0.0, 11047217907689823397),
    ];

    fn fnv(data: &[u8]) -> u64 {
        let mut h = 0xCBF29CE484222325u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001B3);
        }
        h
    }

    fn installed(stem: &str) -> Option<&'static PathBuf> {
        font_files().get(stem)
    }

    #[test]
    fn metrics_style_and_lengths_match_pillow() {
        let mut checked = 0;
        for &(stem, index, size, metrics, style, lengths) in REFERENCE {
            let Some(path) = installed(stem) else {
                continue;
            };
            let face = open_face(path, size, index).expect("opens");
            assert_eq!(face.metrics(), metrics, "{stem} #{index} at {size}");
            assert_eq!(face.style_name(), style, "{stem} #{index}");
            for &(text, normal, mono) in lengths {
                assert_eq!(
                    face.length(text),
                    normal,
                    "{stem} #{index} at {size}: {text:?}"
                );
                // Monochrome advances come from the font's hdmx table where it has one for this size and
                // otherwise from hinting, which isn't ported: a pixel or two per word at most.
                let got = face.length_mono(text);
                let allowed = 3.0 + mono * 0.06;
                assert!(
                    (got - mono).abs() <= allowed,
                    "{stem} #{index} at {size} mono: {text:?} want {mono} got {got}"
                );
            }
            checked += 1;
        }
        if installed("helvetica").is_some() {
            assert!(checked > 20, "only {checked} faces checked");
        }
    }

    #[test]
    fn mono_advances_use_hdmx_where_the_font_has_it() {
        // Helvetica's hdmx has 16 px: "H" is 11 wide when hinted for mono, 12 unhinted.
        let Some(path) = installed("helvetica") else {
            return;
        };
        let face = open_face(path, 16, 0).unwrap();
        assert_eq!((face.length("H"), face.length_mono("H")), (12.0, 11.0));
        assert_eq!(face.length_mono("Hello"), 37.0);
    }

    #[test]
    fn style_names_pick_bold_in_collections() {
        let Some(path) = installed("helvetica") else {
            return;
        };
        assert_eq!(open_face(path, 20, 0).unwrap().style_name(), "Regular");
        assert_eq!(open_face(path, 20, 1).unwrap().style_name(), "Bold");
        if installed("futura").is_some() {
            assert_eq!(craft_face("futura", 20).style_name(), "Bold");
        }
        if installed("didot").is_some() {
            assert_eq!(craft_face("didot", 20).style_name(), "Bold");
            assert_eq!(craft_face("didot-italic", 20).style_name(), "Italic");
        }
    }

    #[test]
    fn pixel_font_matches_pillows_builtin_bitmap_font() {
        let face = pixel_face();
        assert_eq!(PIXEL_HEIGHT, 11);
        assert_eq!(face.length("Hello"), 30.0);
        assert_eq!(face.metrics().0, 11);
        for &(text, x, y, length, hash) in PIXEL_REFERENCE {
            assert_eq!(face.length(text), length, "{text:?}");
            let mut canvas = GrayImage::new(110, 16);
            face.draw(&mut canvas, x, y, text, 255, true);
            assert_eq!(fnv(canvas.as_raw()), hash, "{text:?} at ({x}, {y})");
        }
    }

    #[test]
    fn pixel_font_has_glyphs_for_printable_latin1_only() {
        let face = pixel_face();
        assert!(
            face.has_glyph('A')
                && face.has_glyph('~')
                && face.has_glyph('é')
                && face.has_glyph('ÿ')
        );
        assert!(!face.has_glyph(' ') && !face.has_glyph('\u{7f}') && !face.has_glyph('日'));
        // Python raises on text outside Latin-1; here it just takes no room.
        assert_eq!(face.length("日本"), 0.0);
        let mut canvas = GrayImage::new(20, 14);
        face.draw(&mut canvas, 0.0, 0.0, "日本", 255, true);
        assert!(canvas.pixels().all(|p| p[0] == 0));
    }

    #[test]
    fn font_files_and_names_follow_the_python_rules() {
        let files = font_files();
        for (stem, path) in files {
            assert_eq!(stem, &stem.to_lowercase());
            assert!(is_font_file(path), "{}", path.display());
        }
        let names = font_names();
        assert_eq!(names[0], PIXEL_FONT);
        for name in &names[1..] {
            let (_, stem, _) = FONT_ALIASES
                .iter()
                .find(|(n, _, _)| n == name)
                .expect("an alias");
            assert!(files.contains_key(&stem.to_lowercase()));
        }
        let all = all_system_fonts();
        assert!(all.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }

    #[test]
    fn load_font_resolves_names_and_refuses_unknown_ones() {
        let err = load_font("no-such-font", 20, false).err().expect("unknown");
        assert_eq!(err.0, "Unknown font 'no-such-font'. See catalog fonts.");
        assert!(load_font(PIXEL_FONT, 20, false).unwrap().is_pixel());
        // Every friendly name gives some face, installed or not.
        for (name, _, _) in FONT_ALIASES {
            let face = load_font(name, 20, false).expect(name);
            assert!(face.length("Hello") > 10.0, "{name}");
        }
        // Installed fonts by their own name, in any case.
        if let Some(stem) = installed("arial") {
            assert!(stem.exists());
            assert!(load_font("ARIAL", 20, false).is_ok());
        }
        // A path only works for trusted callers.
        if let Some(path) = installed("helvetica") {
            let name = path.to_str().unwrap();
            assert!(load_font(name, 20, false).is_err());
            assert!(load_font(name, 20, true).is_ok());
        }
    }

    #[test]
    fn drawing_is_anti_aliased_or_one_bit_and_stays_inside_the_image() {
        let Some(face) = font_files()
            .values()
            .next()
            .and_then(|p| open_face(p, 24, 0).ok())
        else {
            return;
        };
        let face = load_font("sans-bold", 24, false).unwrap_or(face);
        let mut aa = GrayImage::new(120, 40);
        face.draw(&mut aa, 4.0, 4.0, "Hello", 255, false);
        let mut mono = GrayImage::new(120, 40);
        face.draw(&mut mono, 4.0, 4.0, "Hello", 255, true);
        assert!(
            aa.pixels().any(|p| p[0] > 0 && p[0] < 255),
            "anti-aliasing gives grey"
        );
        assert!(mono.pixels().all(|p| p[0] == 0 || p[0] == 255));
        assert!(mono.pixels().filter(|p| p[0] == 255).count() > 100);
        // Ink starts at the top: the ascender line is y.
        let top = (0..40)
            .find(|&y| (0..120).any(|x| aa.get_pixel(x, y)[0] > 0))
            .unwrap();
        assert!((4..12).contains(&top), "ink starts at row {top}");
        // Off the image, empty and odd text: nothing breaks.
        let mut small = GrayImage::new(10, 10);
        face.draw(&mut small, -50.0, -50.0, "Hello", 255, true);
        face.draw(&mut small, 5.0, 5.0, "", 255, false);
        face.draw(
            &mut small,
            5.0,
            5.0,
            "tab\tnew\nline \u{0} \u{FFFF} \u{1F600}",
            255,
            false,
        );
        face.draw(&mut small, f64::NAN, 3.0, "x", 255, false);
        face.draw(&mut small, 1e12, -1e12, "x", 255, false);
    }

    #[test]
    fn fill_value_is_composited_over_what_is_there() {
        let face = load_font("sans-bold", 30, false).unwrap();
        let mut canvas = GrayImage::from_pixel(80, 50, image::Luma([255]));
        face.draw(&mut canvas, 2.0, 2.0, "I", 0, false);
        let darkest = canvas.pixels().map(|p| p[0]).min().unwrap();
        assert!(
            darkest <= 5 || face.is_pixel(),
            "a solid stem draws the fill: {darkest}"
        );
        let mut white_on_black = GrayImage::new(80, 50);
        face.draw(&mut white_on_black, 2.0, 2.0, "I", 255, true);
        assert!(white_on_black.pixels().any(|p| p[0] == 255));
    }

    #[test]
    fn fractional_positions_round_like_pillow() {
        let Some(path) = installed("helvetica") else {
            return;
        };
        let face = open_face(path, 24, 1).unwrap();
        let ink_left = |x: f64| -> u32 {
            let mut img = GrayImage::new(30, 30);
            face.draw(&mut img, x, 2.0, "I", 255, true);
            (0..30)
                .find(|&c| (0..30).any(|r| img.get_pixel(c, r)[0] > 0))
                .unwrap()
        };
        // Pillow measured: 2.0 and 2.25 land on the same column, 2.5 and 3.0 on the next.
        assert_eq!(ink_left(2.0), ink_left(2.25));
        assert_eq!(ink_left(2.5), ink_left(3.0));
        assert_eq!(ink_left(3.0), ink_left(2.0) + 1);
    }

    #[test]
    fn text_height_comes_from_pillows_ascent_and_baseline() {
        let Some(path) = installed("helvetica") else {
            return;
        };
        // Pillow, Helvetica Bold 24, "I" at (4, 4): metrics (19, 6), the stem's foot is on baseline row 4 + 19 - 1.
        let face = open_face(path, 24, 1).unwrap();
        let mut img = GrayImage::new(30, 40);
        face.draw(&mut img, 4.0, 4.0, "I", 255, true);
        let rows: Vec<u32> = (0..40)
            .filter(|&y| (0..30).any(|x| img.get_pixel(x, y)[0] > 0))
            .collect();
        assert_eq!(*rows.last().unwrap(), 4 + 19 - 1);
    }

    #[test]
    fn broken_fonts_fail_cleanly() {
        assert!(face_from_data(Arc::new(vec![]), 20, 0).is_none());
        assert!(face_from_data(Arc::new(b"not a font at all".to_vec()), 20, 0).is_none());
        let mut truncated = vec![0, 1, 0, 0, 0, 12, 0, 128, 0, 3, 0, 32];
        truncated.extend_from_slice(b"OS/2garbage");
        assert!(face_from_data(Arc::new(truncated), 20, 0).is_none());
        assert!(open_face(Path::new("/no/such/font.ttf"), 20, 0).is_err());
        assert!(open_face(Path::new("/etc/hosts"), 20, 0).is_err());
        if let Some(path) = installed("helvetica") {
            assert!(
                open_face(path, 20, 99).is_err(),
                "no such face in the collection"
            );
        }
    }

    #[test]
    fn missing_glyphs_are_detected_like_python() {
        if installed("impact").is_some() {
            assert!(craft_has_glyph("impact", 'A'));
            assert!(craft_has_glyph("impact", 'é'));
            assert!(!craft_has_glyph("impact", '日'));
            assert!(!craft_has_glyph("impact", '\u{F0000}'));
        }
        if let Some(path) = installed("helvetica") {
            let face = open_face(path, 64, 0).unwrap();
            assert!(face.has_glyph('g') && !face.has_glyph('日'));
        }
    }

    #[test]
    fn craft_falls_back_for_scripts_a_display_face_lacks() {
        if installed("impact").is_none() || installed("arial unicode").is_none() {
            return;
        }
        let latin = craft_face_for("impact", "Hello world", 40);
        assert_eq!(latin.style_name(), craft_face("impact", 40).style_name());
        assert_eq!(
            latin.length("Hello"),
            craft_face("impact", 40).length("Hello")
        );
        let japanese = craft_face_for("impact", "日本語", 40);
        assert_ne!(
            japanese.length("日本語"),
            craft_face("impact", 40).length("日本語")
        );
        assert!(japanese.has_glyph('日'));
        // Spaces never count as missing.
        assert_eq!(
            craft_face_for("impact", "a b\tc\u{3000}", 40).length("a"),
            craft_face("impact", 40).length("a")
        );
    }

    #[test]
    fn craft_faces_degrade_to_a_sans_face() {
        // A name craft doesn't know, and the friendly ones that may be absent, still give a face that measures text.
        assert!(craft_face("no-such-face", 20).length("Hello") > 10.0);
        for (name, _, _) in CRAFT_FACES {
            assert!(craft_face(name, 18).length("Hello") > 10.0, "{name}");
        }
    }

    #[test]
    fn python_isspace() {
        assert!(
            py_isspace(' ') && py_isspace('\t') && py_isspace('\u{3000}') && py_isspace('\u{1f}')
        );
        assert!(!py_isspace('a') && !py_isspace('\u{200B}'));
    }

    #[test]
    fn freetype_fixed_point_matches() {
        // 12 px of a 2048 unit em: 12 * 64 / 2048 in 16.16.
        let scale = div_fix(12 << 6, 2048);
        assert_eq!(scale, 24576);
        assert_eq!(mul_fix(1577, scale), 591); // 9.24 px in 26.6
        assert_eq!(mul_fix(-471, scale), -177);
        assert_eq!(mul_div(-7, 3, 2), -11);
        assert_eq!(div_fix(1, 0), 0);
    }
}
