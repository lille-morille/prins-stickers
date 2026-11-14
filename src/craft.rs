//! The craft behind posters: type that fits exactly, pictures of anything, and print-like shading.
//!
//! Port of `src/prins/design/craft.py`. Everything here is a decision a designer would make, so that
//! the model doesn't have to. Images are `GrayImage`s with 0 for black and 255 for white until the very
//! end, so shading can be computed before it is reduced to one bit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use image::{GrayImage, RgbaImage};

use crate::assets;
use crate::draw::py_round;
use crate::figures;
use crate::fonts;
use crate::patterns::BAYER_8;
use crate::raster::{self, Resample};

pub const W: i32 = 312;
pub const H: i32 = 216;
/// Matches the label maker's check: smaller smears on a 203 dpi thermal head.
pub const MIN_TEXT_PX: i32 = 16;
/// The size text is drawn at before it is reduced, which keeps its outlines clean.
const BIG: i32 = 240;
const LEAST_SQUASH: f64 = 0.88;

// --- small helpers ------------------------------------------------------------------------------

/// Pixel sizes as `i32`, which is what the layout arithmetic uses.
pub trait Dim {
    fn w(&self) -> i32;
    fn h(&self) -> i32;
}

impl Dim for GrayImage {
    fn w(&self) -> i32 {
        self.width() as i32
    }
    fn h(&self) -> i32 {
        self.height() as i32
    }
}

/// Python's `round()` to an int: halves go to the even neighbour.
pub fn round_i(v: f64) -> i32 {
    py_round(v).clamp(-1.0e9, 1.0e9) as i32
}

/// Python's `//` on ints: rounds toward negative infinity.
pub fn fdiv(a: i32, b: i32) -> i32 {
    if b == 0 {
        return 0;
    }
    a.div_euclid(b) - if b < 0 && a.rem_euclid(b) != 0 { 1 } else { 0 }
}

/// Python's float `%`: the result has the divisor's sign.
pub fn pymod(a: f64, b: f64) -> f64 {
    let m = a % b;
    if m != 0.0 && (b < 0.0) != (m < 0.0) {
        m + b
    } else {
        m
    }
}

/// `str.isspace` for one character.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.split()` with no argument: runs of whitespace separate the words.
pub fn py_split(text: &str) -> Vec<&str> {
    text.split(py_isspace).filter(|t| !t.is_empty()).collect()
}

/// `" ".join(text.split())`.
pub fn collapse(text: &str) -> String {
    py_split(text).join(" ")
}

// --- type ---------------------------------------------------------------------------------------

type InkCache = Mutex<HashMap<(String, String), Arc<GrayImage>>>;

/// The text's ink at a large size, cropped tight. Width and height are its true proportions.
fn ink(text: &str, face: &str) -> Arc<GrayImage> {
    static CACHE: OnceLock<InkCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (text.to_string(), face.to_string());
    if let Ok(guard) = cache.lock() {
        if let Some(found) = guard.get(&key) {
            return found.clone();
        }
    }
    let font = fonts::craft_face_for(face, text, BIG as u32);
    let width = (font.length(text) as i32)
        .saturating_add(BIG)
        .clamp(1, 1 << 20);
    let mut canvas = raster::blank(width as u32, (BIG * 2) as u32);
    font.draw(
        &mut canvas,
        (BIG / 3) as f64,
        (BIG / 3) as f64,
        text,
        255,
        false,
    );
    let tight = match raster::getbbox(&canvas) {
        Some((x0, y0, x1, y1)) => raster::crop(&canvas, x0 as i32, y0 as i32, x1 as i32, y1 as i32),
        None => raster::blank(1, 1),
    };
    let tight = Arc::new(tight);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 512 {
            guard.clear();
        }
        guard.insert(key, tight.clone());
    }
    tight
}

/// Width over height of the text's ink.
pub fn aspect(text: &str, face: &str) -> f64 {
    let i = ink(text, face);
    i.w() as f64 / i.h().max(1) as f64
}

/// A one-bit mask of the text's ink, exactly w by h. Give one side to keep its proportions.
pub fn word(text: &str, face: &str, w: Option<i32>, h: Option<i32>, heavy: bool) -> GrayImage {
    let i = ink(text, face);
    let ratio = i.w() as f64 / i.h().max(1) as f64;
    // Python tests `if w and not h`: a zero counts as not given.
    let (mut w, mut h) = (w.filter(|v| *v != 0), h.filter(|v| *v != 0));
    if w.is_some() && h.is_none() {
        h = Some(round_i(w.unwrap_or(0) as f64 / ratio));
    } else if h.is_some() && w.is_none() {
        w = Some(round_i(h.unwrap_or(0) as f64 * ratio));
    }
    let nw = w.unwrap_or(i.w()).max(1);
    let nh = h.unwrap_or(i.h()).max(1);
    let small = raster::resize(&i, nw as u32, nh as u32, Resample::Lanczos);
    let cut = if heavy { 62 } else { 118 }; // A heavier cut keeps the hairlines of a delicate face.
    raster::threshold(&small, cut)
}

/// Small text, hinted straight to pixels so it stays sharp. Never below the legible size.
///
/// If it can't fit `width` at that size it is squeezed sideways, down to what a condensed face can
/// take, and past that the caller should have made it wrap.
pub fn caption(text: &str, face: &str, size: i32, width: Option<i32>) -> GrayImage {
    let size = size.max(MIN_TEXT_PX);
    let font = fonts::craft_face_for(face, text, size as u32);
    let cw = (font.length(text) as i32)
        .saturating_add(size * 2)
        .clamp(1, 1 << 20);
    let mut canvas = raster::blank(cw as u32, (size * 2) as u32);
    font.draw(
        &mut canvas,
        (size / 2) as f64,
        (size / 4) as f64,
        text,
        255,
        true,
    );
    let mut mask = match raster::getbbox(&canvas) {
        Some((x0, y0, x1, y1)) => raster::crop(&canvas, x0 as i32, y0 as i32, x1 as i32, y1 as i32),
        None => raster::blank(1, 1),
    };
    if let Some(width) = width.filter(|w| *w != 0) {
        if mask.w() > width {
            mask = raster::resize(&mask, width.max(1) as u32, mask.height(), Resample::Nearest);
        }
    }
    mask
}

/// Greedy lines of legible caption text that fit width.
pub fn wrap_caption(text: &str, width: i32, face: &str, size: i32) -> Vec<GrayImage> {
    let size = size.max(MIN_TEXT_PX);
    let font = fonts::craft_face_for(face, text, size as u32);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for token in py_split(text) {
        let trial = format!("{current} {token}");
        let trial = trial.trim_matches(py_isspace).to_string();
        if font.length(&trial) <= width as f64 || current.is_empty() {
            current = trial;
        } else {
            lines.push(std::mem::take(&mut current));
            current = token.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
        .iter()
        .map(|line| caption(line, face, size, Some(width)))
        .collect()
}

/// Where a designer would break these words so they fill the box when each is set full width.
///
/// Tries every set of breaks up to `most` lines. The best one comes closest to the box's height
/// without leaving a line far smaller than the others.
pub fn break_lines(
    text: &str,
    face: &str,
    width: i32,
    height: i32,
    gap: i32,
    most: usize,
) -> Vec<String> {
    let words = py_split(text);
    if words.len() <= 1 {
        return if words.is_empty() {
            vec![String::new()]
        } else {
            words.iter().map(|w| w.to_string()).collect()
        };
    }
    if words.len() > 14 {
        // Past this many, trying every break gets slow and greedy wrapping is fine.
        let most_each = (words.len().div_ceil(most.max(1))).max(1);
        return words.chunks(most_each).map(|c| c.join(" ")).collect();
    }

    let mut best: Vec<String> = vec![words.join(" ")];
    let mut best_score = f64::INFINITY;
    let count = words.len();
    let height_f = height as f64;
    for mask in 0u32..(1u32 << (count - 1)) {
        if mask.count_ones() as usize + 1 > most {
            continue;
        }
        let mut lines: Vec<String> = Vec::new();
        let mut start = 0;
        for i in 0..count - 1 {
            if (mask >> i) & 1 == 1 {
                lines.push(words[start..=i].join(" "));
                start = i + 1;
            }
        }
        lines.push(words[start..].join(" "));
        let heights: Vec<f64> = lines
            .iter()
            .map(|line| width as f64 / aspect(line, face).max(0.05))
            .collect();
        let total: f64 = heights.iter().sum::<f64>() + (gap * (lines.len() as i32 - 1)) as f64;
        let overflow = (total - height_f).max(0.0) / height_f;
        let empty = (height_f - total).max(0.0) / height_f;
        let tallest = heights.iter().cloned().fold(f64::MIN, f64::max);
        let shortest = heights.iter().cloned().fold(f64::MAX, f64::min);
        let balance = tallest / shortest.max(1.0);
        let score =
            overflow * 3.0 + empty + (balance - 2.2).max(0.0) * 0.25 + lines.len() as f64 * 0.002;
        if score < best_score {
            best = lines;
            best_score = score;
        }
    }
    best
}

/// Each line set to the full width, with heights adjusted so the stack fills the height.
///
/// Letters can take being made taller, and a little shorter. Past that they look distorted, so a short
/// word that would need it gets narrower instead, and the caller centers it.
pub fn stack(
    lines: &[String],
    face: &str,
    width: i32,
    height: i32,
    gap: i32,
    stretch: f64,
) -> Vec<GrayImage> {
    let natural: Vec<f64> = lines
        .iter()
        .map(|line| width as f64 / aspect(line, face).max(0.05))
        .collect();
    let room = (height - gap * (lines.len() as i32 - 1)).max(8);
    let sum: f64 = natural.iter().sum();
    let mut scale = stretch.min(room as f64 / sum.max(1.0));
    let mut narrow = 1.0;
    if scale < LEAST_SQUASH {
        narrow = scale / LEAST_SQUASH;
        scale = LEAST_SQUASH;
    }
    lines
        .iter()
        .zip(natural)
        .map(|(line, n)| {
            word(
                line,
                face,
                Some(round_i(width as f64 * narrow).max(8)),
                Some(round_i(n * scale * narrow).max(8)),
                false,
            )
        })
        .collect()
}

pub fn smallest(masks: &[GrayImage]) -> i32 {
    masks.iter().map(|m| m.h()).min().unwrap_or(0)
}

// --- pasting ------------------------------------------------------------------------------------

/// `canvas.paste(ink, (round(x), round(y)), mask)`.
pub fn paste(canvas: &mut GrayImage, mask: &GrayImage, x: f64, y: f64, value: u8) {
    raster::paste_value(canvas, value, round_i(x), round_i(y), mask);
}

/// The mask laid on an empty layer the size of the canvas.
fn layer_of(canvas: &GrayImage, mask: &GrayImage, x: f64, y: f64) -> GrayImage {
    let mut layer = raster::blank(canvas.width(), canvas.height());
    raster::paste(&mut layer, mask, round_i(x), round_i(y), None);
    layer
}

/// Inverts the canvas wherever the mask is.
pub fn xor(canvas: &mut GrayImage, mask: &GrayImage, x: f64, y: f64) {
    let layer = layer_of(canvas, mask, x, y);
    let inverted = raster::invert(canvas);
    raster::paste(canvas, &inverted, 0, 0, Some(&layer));
}

/// Pastes `fill` through the mask.
pub fn fill_mask(canvas: &mut GrayImage, fill: &GrayImage, mask: &GrayImage, x: f64, y: f64) {
    let layer = layer_of(canvas, mask, x, y);
    raster::paste(canvas, fill, 0, 0, Some(&layer));
}

// --- tone ---------------------------------------------------------------------------------------

/// `f(x, y)` gives brightness from 0 to 1.
pub fn tone(f: impl Fn(i32, i32) -> f64, w: i32, h: i32) -> GrayImage {
    let (w, h) = (w.max(0), h.max(0));
    let mut data = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            data.push(((f(x, y) * 255.0) as i64).clamp(0, 255) as u8);
        }
    }
    GrayImage::from_raw(w as u32, h as u32, data).unwrap_or_else(|| raster::blank(0, 0))
}

/// A tone over the whole note.
pub fn tone_page(f: impl Fn(i32, i32) -> f64) -> GrayImage {
    tone(f, W, H)
}

/// Ordered dither with the 8x8 Bayer matrix.
pub fn bayer(gray: &GrayImage) -> GrayImage {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let src = gray.as_raw();
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let cut = (BAYER_8[y % 8][x % 8] as f64 + 0.5) / 64.0;
            out.push(if src[y * w + x] as f64 / 255.0 > cut {
                255
            } else {
                0
            });
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap_or_else(|| raster::blank(0, 0))
}

/// Atkinson error diffusion.
pub fn atkinson(gray: &GrayImage) -> GrayImage {
    let (w, h) = (gray.width() as i64, gray.height() as i64);
    let mut values: Vec<f64> = gray.as_raw().iter().map(|&v| v as f64).collect();
    let mut out = vec![0u8; (w * h) as usize];
    const SPREAD: [(i64, i64); 6] = [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let new = if values[i] >= 128.0 { 255.0 } else { 0.0 };
            out[i] = new as u8;
            let error = (values[i] - new) / 8.0;
            for (dx, dy) in SPREAD {
                let (nx, ny) = (x + dx, y + dy);
                if 0 <= nx && nx < w && ny < h {
                    values[(ny * w + nx) as usize] += error;
                }
            }
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap_or_else(|| raster::blank(0, 0))
}

/// Round dots on a rotated screen, as in real print.
pub fn halftone(gray: &GrayImage, cell: f64, angle: f64) -> GrayImage {
    let (w, h) = (gray.width() as i64, gray.height() as i64);
    let source = gray.as_raw();
    let a = angle.to_radians();
    let (ca, sa) = (a.cos(), a.sin());
    let mut out = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let (xf, yf) = (x as f64, y as f64);
            let (u, v) = ((xf * ca + yf * sa) / cell, (-xf * sa + yf * ca) / cell);
            let (iu, iv) = (u.floor(), v.floor());
            let (fu, fv) = (u - iu - 0.5, v - iv - 0.5);
            let (cu, cv) = ((iu + 0.5) * cell, (iv + 0.5) * cell);
            let sx = ((cu * ca - cv * sa) as i64).clamp(0, w - 1);
            let sy = ((cu * sa + cv * ca) as i64).clamp(0, h - 1);
            let radius = (1.0 - source[(sy * w + sx) as usize] as f64 / 255.0).sqrt() * 0.76;
            out[(y * w + x) as usize] = if fu * fu + fv * fv < radius * radius {
                0
            } else {
                255
            };
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap_or_else(|| raster::blank(0, 0))
}

/// Copperplate: tone drawn only in lines that swell in the dark and thin out in the light.
///
/// `wobble` is a height map that bends the lines, so they follow a form instead of lying flat.
pub fn engrave(
    gray: &GrayImage,
    spacing: f64,
    angle: f64,
    wobble: Option<&GrayImage>,
) -> GrayImage {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let source = gray.as_raw();
    let bend = wobble.map(|b| b.as_raw());
    let a = angle.to_radians();
    let (ca, sa) = (a.cos(), a.sin());
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut along = x as f64 * sa + y as f64 * ca;
            if let Some(bend) = bend {
                // The wobble map may be smaller than the picture; Python would raise, so read 0 there.
                let b = bend.get(y * w + x).copied().unwrap_or(0);
                along += b as f64 / 255.0 * spacing * 2.6;
            }
            let phase = (pymod(along / spacing, 1.0) - 0.5).abs();
            let dark = (1.0 - source[y * w + x] as f64 / 255.0).powf(0.85);
            out[y * w + x] = if phase < 0.5 * dark { 0 } else { 255 };
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap_or_else(|| raster::blank(0, 0))
}

/// Stretches contrast so a picture uses every tone from black to white.
pub fn bright_up(gray: &GrayImage, floor: i32, ceiling: i32) -> GrayImage {
    let (low, high) = raster::extrema(gray);
    if high <= low {
        return gray.clone();
    }
    let (low, high) = (low as i32, high as i32);
    raster::point(gray, |v| {
        let v = v as i32;
        let value = floor + fdiv((v - low) * (ceiling - floor), high - low);
        value.clamp(0, 255) as u8
    })
}

/// A height map that swells a flat silhouette like a pillow, so light can fall on it.
pub fn inflate(mask: &GrayImage) -> GrayImage {
    let size = mask.w().max(mask.h()) as f64;
    let mut height = raster::blank(mask.width(), mask.height());
    for (radius, weight) in [(size * 0.02, 0.45), (size * 0.05, 0.35), (size * 0.1, 0.2)] {
        let blurred = raster::gaussian_blur(mask, radius);
        let blurred = raster::point(&blurred, |v| (v as f64 * weight) as i64 as u8);
        height = raster::add(&height, &blurred);
    }
    raster::multiply(&height, mask)
}

/// Shades a height map as if lit from the upper left (strength 7 is Python's default; the light is
/// at (-0.6, -0.6, 0.53)).
pub fn light(height: &GrayImage, strength: f64) -> GrayImage {
    light_from(height, strength, -0.6, -0.6, 0.53)
}

pub fn light_from(height: &GrayImage, strength: f64, lx: f64, ly: f64, lz: f64) -> GrayImage {
    let (w, h) = (height.width() as usize, height.height() as usize);
    let px = height.as_raw();
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let at = |xx: usize, yy: usize| px[yy * w + xx] as f64;
            let nx = (at(x.saturating_sub(1), y) - at((x + 1).min(w - 1), y)) / 255.0 * strength;
            let ny = (at(x, y.saturating_sub(1)) - at(x, (y + 1).min(h - 1))) / 255.0 * strength;
            let length = (nx * nx + ny * ny + 1.0).sqrt();
            let value = (nx * lx + ny * ly + lz) / length;
            out[y * w + x] = (((40.0 + value * 300.0) as i64).clamp(0, 255)) as u8;
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap_or_else(|| raster::blank(0, 0))
}

// --- pictures -----------------------------------------------------------------------------------

/// Every word in the names of the emoji blocks, to the shortest-named emoji that has it.
fn emoji_words() -> &'static HashMap<String, char> {
    static WORDS: OnceLock<HashMap<String, char>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let mut index: HashMap<String, (usize, char)> = HashMap::new();
        let ranges = [0x1F300..0x1F650u32, 0x1F680..0x1F700, 0x1F900..0x1FB00];
        for code in ranges.into_iter().flatten() {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            let Some(name) = unicode_names2::name(c) else {
                continue;
            };
            let name = name.to_string();
            let words: Vec<&str> = name.split_whitespace().collect();
            for token in &words {
                let better = match index.get(*token) {
                    Some((count, _)) => words.len() < *count,
                    None => true,
                };
                if better {
                    index.insert(token.to_string(), (words.len(), c));
                }
            }
        }
        index.into_iter().map(|(k, (_, c))| (k, c)).collect()
    })
}

/// `dragon` finds the dragon emoji from Unicode's own names, with no table to keep up.
pub fn named_emoji(name: &str) -> Option<char> {
    let lowered: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let clean = lowered.trim_matches(py_isspace).to_uppercase();
    if clean.is_empty() || clean.chars().count() > 40 {
        return None;
    }
    for candidate in [clean.clone(), format!("{clean} FACE")] {
        if let Some(c) = unicode_names2::character(&candidate) {
            if c as u32 > 0x2000 {
                return Some(c);
            }
        }
    }
    let words = emoji_words();
    for token in py_split(&clean) {
        for form in [token, token.trim_end_matches('S')] {
            if let Some(c) = words.get(form) {
                return Some(*c);
            }
        }
    }
    None
}

/// A picture of the subject as (gray, mask), both the same size, or None if we have none.
///
/// The subject is a figure name, an emoji, or a word Unicode has an emoji for, or logo:brand.
pub fn resolve_picture(name: &str, size: i32) -> Option<(GrayImage, GrayImage)> {
    let name = name.trim_matches(py_isspace);
    if name.is_empty() {
        return None;
    }
    let size = size.clamp(1, 4096) as u32;
    if name.to_lowercase().starts_with("logo:") {
        let rest: String = name.chars().skip(5).collect();
        let image = assets::logo_svg(&rest)
            .and_then(|data| assets::load_image(&data, size, size))
            .ok()?;
        return Some(from_rgba(&image));
    }
    let key = name.to_lowercase().replace(' ', "-");
    let emoji: Option<String> = if name.chars().any(|c| c as u32 > 0x2000) {
        Some(name.to_string())
    } else {
        named_emoji(name).map(|c| c.to_string())
    };
    if let Some(emoji) = emoji {
        if let Some(image) = assets::emoji_rgba(&emoji, size) {
            return Some(from_rgba(&image));
        }
    }
    if figures::has_figure(&key) {
        let mask = figures::figure_mask(&key, size, size);
        let mask = match raster::getbbox(&mask) {
            Some((x0, y0, x1, y1)) => {
                raster::crop(&mask, x0 as i32, y0 as i32, x1 as i32, y1 as i32)
            }
            None => mask,
        };
        return Some((light(&inflate(&mask), 9.0), mask));
    }
    None
}

fn from_rgba(image: &RgbaImage) -> (GrayImage, GrayImage) {
    let (iw, ih) = (image.width(), image.height());
    let alpha: Vec<u8> = image
        .pixels()
        .map(|p| if p.0[3] >= 110 { 255 } else { 0 })
        .collect();
    let mut alpha = GrayImage::from_raw(iw, ih, alpha).unwrap_or_else(|| raster::blank(0, 0));
    let (mut left, mut top, mut right, mut bottom) = (0, 0, iw, ih);
    if let Some((x0, y0, x1, y1)) = raster::getbbox(&alpha) {
        (left, top, right, bottom) = (x0, y0, x1, y1);
        alpha = raster::crop(&alpha, x0 as i32, y0 as i32, x1 as i32, y1 as i32);
    }
    let (cw, ch) = (right - left, bottom - top);
    let mut luma = Vec::with_capacity((cw * ch) as usize);
    let mut darkest = Vec::with_capacity((cw * ch) as usize);
    for y in top..bottom {
        for x in left..right {
            let p = image.get_pixel(x, y).0;
            let (r, g, b) = (p[0] as u32, p[1] as u32, p[2] as u32);
            luma.push(((r * 19595 + g * 38470 + b * 7471 + 0x8000) >> 16) as u8);
            darkest.push(p[0].min(p[1]).min(p[2]));
        }
    }
    let luma = GrayImage::from_raw(cw, ch, luma).unwrap_or_else(|| raster::blank(0, 0));
    let darkest = GrayImage::from_raw(cw, ch, darkest).unwrap_or_else(|| raster::blank(0, 0));
    let gray = raster::blend_images(&luma, &darkest, 0.45);
    let stretched = bright_up(&mean_inside(&gray, &alpha), 0, 245);
    (stretched, alpha)
}

/// Whatever is outside the subject shouldn't count as its darkest or brightest tone.
fn mean_inside(gray: &GrayImage, alpha: &GrayImage) -> GrayImage {
    let (mut sum, mut count) = (0u64, 0u64);
    for (g, a) in gray.as_raw().iter().zip(alpha.as_raw()) {
        if *a != 0 {
            sum += *g as u64;
            count += 1;
        }
    }
    let average = (sum / count.max(1)) as u8;
    raster::composite(
        gray,
        &raster::filled(gray.width(), gray.height(), average),
        alpha,
    )
}

/// Scales the picture to fit inside width by height, keeping its proportions.
pub fn fit(image: &GrayImage, width: i32, height: i32) -> GrayImage {
    let ratio =
        (width as f64 / image.w().max(1) as f64).min(height as f64 / image.h().max(1) as f64);
    let nw = round_i(image.w() as f64 * ratio).max(1);
    let nh = round_i(image.h() as f64 * ratio).max(1);
    raster::resize(image, nw as u32, nh as u32, Resample::Lanczos)
}

/// The picture reduced to one bit, in a printer's technique. White outside the subject.
pub fn finish_picture(gray: &GrayImage, mask: &GrayImage, finish: &str) -> GrayImage {
    let hard = raster::threshold(mask, 128);
    let drawn = match finish {
        "engrave" => engrave(gray, 3.6, -24.0, Some(&inflate(&hard))),
        "halftone" => halftone(gray, 4.2, 45.0),
        _ => atkinson(gray),
    };
    raster::composite(&drawn, &raster::paper(gray.width(), gray.height()), &hard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_division_and_modulo_follow_python() {
        assert_eq!(fdiv(-3, 2), -2);
        assert_eq!(fdiv(7, 2), 3);
        assert_eq!(fdiv(-4, 2), -2);
        assert!((pymod(-0.25, 1.0) - 0.75).abs() < 1e-12);
        assert_eq!(round_i(2.5), 2);
        assert_eq!(round_i(3.5), 4);
    }

    #[test]
    fn a_word_is_exactly_the_size_asked_for() {
        let mask = word("Hi", "impact", Some(120), Some(40), false);
        assert_eq!((mask.w(), mask.h()), (120, 40));
    }

    #[test]
    fn a_short_word_isnt_stretched_out_of_shape() {
        let mask = &stack(&["OSLO".to_string()], "impact", 296, 100, 6, 1.6)[0];
        let natural = aspect("OSLO", "impact");
        let got = mask.w() as f64 / mask.h() as f64;
        assert!(
            ((got - natural / 0.88) / (natural / 0.88)).abs() < 0.05,
            "{got} {natural}"
        );
    }

    #[test]
    fn lines_break_where_they_fill_the_box() {
        assert_eq!(
            break_lines("DEPLOY BLOCKED", "impact", 296, 150, 6, 6),
            vec!["DEPLOY", "BLOCKED"]
        );
        assert_eq!(
            break_lines("one two three four five six", "impact", 296, 150, 6, 6).join(" "),
            "one two three four five six"
        );
    }

    #[test]
    fn nonsense_finds_no_picture() {
        assert!(resolve_picture("qwertyuiop", 200).is_none());
        assert!(resolve_picture("", 200).is_none());
        assert!(resolve_picture("   ", 200).is_none());
    }

    #[test]
    fn a_subject_can_be_a_figure_or_a_plain_word() {
        // Figures need no emoji font.
        let (gray, mask) = resolve_picture("star", 80).expect("a figure");
        assert_eq!(gray.dimensions(), mask.dimensions());
        assert!(raster::getbbox(&mask).is_some());
        if assets::emoji_rgba("\u{1F409}", 160).is_some() {
            for subject in ["\u{1F409}", "dragon", "pizza", "tooth", "cat"] {
                let (gray, mask) = resolve_picture(subject, 80).expect(subject);
                assert_eq!(gray.dimensions(), mask.dimensions());
                assert!(raster::getbbox(&mask).is_some(), "{subject}");
            }
        }
    }

    #[test]
    fn plain_words_find_their_emoji_from_unicode_names() {
        assert_eq!(named_emoji("dragon"), Some('\u{1F409}'));
        assert_eq!(named_emoji("Pizza"), Some('\u{1F355}'));
        assert_eq!(named_emoji("cat"), Some('\u{1F408}'));
        assert_eq!(named_emoji("tooth"), Some('\u{1F9B7}'));
        // From Python's `_named_emoji` (Unicode 15.0 names).
        assert_eq!(named_emoji("spiral"), Some('\u{1F41A}'));
        assert_eq!(named_emoji("star"), Some('\u{1F31F}'));
        assert_eq!(named_emoji("moon"), Some('\u{1F319}'));
        assert_eq!(named_emoji("infinity"), Some('\u{221E}'));
        for none in [
            "burst", "hexagon", "star4", "plus", "flame", "wink", "mail", "coffee", "music",
            "target", "octagon", "star6",
        ] {
            assert_eq!(named_emoji(none), None, "{none}");
        }
        assert_eq!(named_emoji("qwertyuiop"), None);
        assert_eq!(named_emoji(""), None);
    }

    #[test]
    fn the_emoji_word_table_matches_python_for_a_few_words() {
        // Generated with Python 3.12 (Unicode 15.0): _emoji_words()[token].
        let words = emoji_words();
        assert_eq!(words.get("CAKE").copied(), Some('\u{1F382}'));
        assert_eq!(words.get("ROCKET").copied(), Some('\u{1F680}'));
    }

    #[test]
    fn halftone_and_dithers_make_one_bit_images() {
        let gray = tone(|x, _| x as f64 / 60.0, 60, 40);
        for out in [
            bayer(&gray),
            atkinson(&gray),
            halftone(&gray, 5.0, 45.0),
            engrave(&gray, 4.0, -24.0, None),
        ] {
            assert!(out.as_raw().iter().all(|&v| v == 0 || v == 255));
            assert_eq!(out.dimensions(), gray.dimensions());
        }
    }
}
