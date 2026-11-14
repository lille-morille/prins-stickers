//! Things that come from outside the canvas: images, SVGs, brand logos, dithers and emoji.
//!
//! Port of `assets.py` (fonts live in `fonts.rs`). The CLI is a trusted tool, so the guarded "web"
//! mode of the Python module (`check_public_url`, `fetch_public_page`, `refuse_outside_references`)
//! is not ported: with `external = true` everything is allowed.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::{GrayImage, Luma, Rgba, RgbaImage};
use regex::Regex;
use sha2::{Digest, Sha256};

pub const MAX_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;
const USER_AGENT: &str = "prins-note-designer/0.1";
/// Simple Icons: brand logos as single-colour SVGs. Pinned, so a logo can't change under a design.
const LOGO_URL: &str = "https://cdn.jsdelivr.net/npm/simple-icons@16.36.0/icons/{slug}.svg";
/// Pillow's `Image.MAX_IMAGE_PIXELS`; it refuses (rather than warns) at twice this.
const MAX_IMAGE_PIXELS: u64 = 64_000_000;
const EMOJI_FONT: &str = "/System/Library/Fonts/Apple Color Emoji.ttc";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AssetError(pub String);

/// Python's `repr()` of a string, for messages that quote what the caller gave.
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `round()`: halves go to the even neighbour.
fn py_round(value: f64) -> i64 {
    value.round_ties_even() as i64
}

fn expand_home(src: &str) -> PathBuf {
    if src == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(src));
    }
    if let Some(rest) = src.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(src)
}

// --- reading sources -------------------------------------------------------------------------

/// A URL, a local path, or inline `data:` / raw base64.
///
/// `external` allows everything (the CLI always passes true). Without it only inline data and SVG markup
/// are read, as in the Python module's non-web mode.
pub fn read_source(src: &str, external: bool) -> Result<Vec<u8>, AssetError> {
    if src.starts_with("data:") {
        let payload = src.split_once(',').map(|(_, rest)| rest).unwrap_or(src);
        return decode_base64(payload);
    }
    let trimmed = src.trim_start();
    if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
        return Ok(src.as_bytes().to_vec());
    }
    if src.starts_with("http://") || src.starts_with("https://") {
        if !external {
            return Err(AssetError(
                "This design can't load from the network.".into(),
            ));
        }
        return download(src, true);
    }
    if external && src.len() < 1024 {
        let path = expand_home(src);
        if path.is_file() {
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            if size > MAX_DOWNLOAD_BYTES as u64 {
                return Err(AssetError(format!("{src} is over 20 MB.")));
            }
            return std::fs::read(&path).map_err(|e| AssetError(format!("Can't read {src}: {e}")));
        }
    }
    if !external && src.len() < 1024 && src.contains('/') {
        return Err(AssetError("This design can't read files.".into()));
    }
    if src.len() >= 32 {
        return decode_base64(src);
    }
    Err(AssetError(format!(
        "Can't find {}. Give a URL, a file path, or base64.",
        py_repr(src)
    )))
}

fn base64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some((byte - b'A') as u32),
        b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
        b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `base64.b64decode(text, validate=False)`: characters outside the alphabet are skipped, padding ends the
/// data, and a short final group without its padding is an error.
pub fn decode_base64(text: &str) -> Result<Vec<u8>, AssetError> {
    let bad = || AssetError("That isn't valid base64.".into());
    if !text.is_ascii() {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let (mut acc, mut in_group, mut pads) = (0u32, 0u32, 0u32);
    let mut finished = false;
    for &byte in text.as_bytes() {
        if byte == b'=' {
            if in_group >= 2 {
                pads += 1;
                if in_group + pads >= 4 {
                    finished = true;
                    break;
                }
            }
            continue;
        }
        let Some(value) = base64_value(byte) else {
            continue;
        };
        pads = 0;
        acc = (acc << 6) | value;
        in_group += 1;
        if in_group == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            in_group = 0;
        }
    }
    if !finished && in_group != 0 {
        return Err(bad());
    }
    if in_group == 2 {
        out.push((acc >> 4) as u8);
    } else if in_group == 3 {
        out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]);
    }
    Ok(out)
}

fn cache_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".prins")
        .join("cache")
}

fn cache_path(url: &str) -> PathBuf {
    cache_dir().join(hex::encode(Sha256::digest(url.as_bytes())))
}

/// Fetch a URL, 15 s timeout, 20 MB cap. With `cache`, the same file Python would use
/// (`~/.prins/cache/<sha256 of the url>`) is read first and written after.
pub fn download(url: &str, cache: bool) -> Result<Vec<u8>, AssetError> {
    let cached = cache_path(url);
    if cache {
        let _ = std::fs::create_dir_all(cache_dir());
        if let Ok(data) = std::fs::read(&cached) {
            return Ok(data);
        }
    }
    let fail = |why: String| AssetError(format!("Couldn't download {url}: {why}"));
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(USER_AGENT)
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent.get(url).call().map_err(|e| fail(e.to_string()))?;
    let mut data = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_DOWNLOAD_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| fail(e.to_string()))?;
    if data.len() > MAX_DOWNLOAD_BYTES {
        return Err(AssetError(format!(
            "{url} is over {} MB.",
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        )));
    }
    if cache {
        // Written to a side file first so a reader never sees half of it.
        let side = cached.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&side, &data).is_ok() && std::fs::rename(&side, &cached).is_err() {
            let _ = std::fs::remove_file(&side);
        }
    }
    Ok(data)
}

/// Simple Icons' slug for a brand name: lower case, `+` to plus, `&` to and, `.` to dot, then letters and digits only.
pub fn logo_slug(name: &str) -> String {
    let slug = name
        .trim()
        .to_lowercase()
        .replace('+', "plus")
        .replace('&', "and")
        .replace('.', "dot");
    slug.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

pub fn logo_url(slug: &str) -> String {
    LOGO_URL.replace("{slug}", slug)
}

/// A brand logo by name. Tries Simple Icons' slug rules.
pub fn logo_svg(name: &str) -> Result<Vec<u8>, AssetError> {
    let slug = logo_slug(name);
    if slug.is_empty() {
        return Err(AssetError(format!("{} isn't a logo name.", py_repr(name))));
    }
    download(&logo_url(&slug), true).map_err(|_| {
        AssetError(format!(
            "No logo named {} in Simple Icons. Try draw_image or draw_svg with the logo's URL or path data.",
            py_repr(name)
        ))
    })
}

pub fn is_svg(data: &[u8]) -> bool {
    let head: Vec<u8> = data[..data.len().min(600)]
        .iter()
        .skip_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let contains =
        |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
    head.starts_with(b"<svg")
        || (head.starts_with(b"<?xml") && contains(&head, b"<svg"))
        || contains(&head[..head.len().min(300)], b"<svg")
}

// --- decoding pictures -------------------------------------------------------------------------

/// Any image or SVG, as RGBA. SVGs are rasterised at the size they'll be used; EXIF orientation is applied.
pub fn load_image(data: &[u8], w: u32, h: u32) -> Result<RgbaImage, AssetError> {
    if is_svg(data) {
        return render_svg(&String::from_utf8_lossy(data), w, h);
    }
    let unreadable = |why: String| AssetError(format!("Couldn't read that image: {why}"));
    let reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| unreadable(e.to_string()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| unreadable(e.to_string()))?;
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    if width as u64 * height as u64 > MAX_IMAGE_PIXELS * 2 {
        return Err(unreadable(format!("Image size ({width} x {height} = {} pixels) exceeds limit of {} pixels, could be decompression bomb DOS attack.", width as u64 * height as u64, MAX_IMAGE_PIXELS * 2)));
    }
    let orientation = image::ImageDecoder::orientation(&mut decoder)
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut picture =
        image::DynamicImage::from_decoder(decoder).map_err(|e| unreadable(e.to_string()))?;
    picture.apply_orientation(orientation);
    Ok(picture.to_rgba8())
}

fn viewbox_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"viewBox=["']\s*[-\d.]+[\s,]+[-\d.]+[\s,]+([\d.]+)[\s,]+([\d.]+)"#)
            .expect("static pattern")
    })
}

/// `_render_svg`: the size comes from the viewBox's proportions, then resvg fits the drawing into it.
fn render_svg(svg: &str, width: u32, height: u32) -> Result<RgbaImage, AssetError> {
    let from_view_box = viewbox_pattern().captures(svg).and_then(|m| {
        let (w, h) = (m[1].parse::<f64>().ok()?, m[2].parse::<f64>().ok()?);
        (w != 0.0 && h != 0.0).then_some(w / h)
    });
    let ratio = from_view_box.unwrap_or(width as f64 / height.max(1) as f64);
    let scale = (width as f64 / ratio).min(height as f64);
    let target_h = py_round(scale).max(1) as u32;
    let target_w = py_round(scale * ratio).max(1) as u32;
    let unrenderable = |why: String| AssetError(format!("Couldn't render that SVG: {why}"));

    let mut options = resvg::usvg::Options::default();
    if svg.contains("<text") {
        // Loading every system font is slow, and almost no drawing needs it.
        let mut fonts = resvg::usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        options.fontdb = Arc::new(fonts);
    }
    let tree =
        resvg::usvg::Tree::from_str(svg, &options).map_err(|e| unrenderable(e.to_string()))?;
    // resvg's own "fit to width x height": scale the drawing to fit inside, keeping its proportions.
    let natural = tree.size().to_int_size();
    let wanted = resvg::tiny_skia::IntSize::from_wh(target_w, target_h)
        .ok_or_else(|| unrenderable("bad size".into()))?;
    let out = natural.scale_to(wanted);
    if out.width() as u64 * out.height() as u64 > MAX_IMAGE_PIXELS {
        return Err(unrenderable(format!(
            "{} x {} pixels is too large",
            out.width(),
            out.height()
        )));
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(out.width(), out.height())
        .ok_or_else(|| unrenderable("bad size".into()))?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        out.width() as f32 / natural.width() as f32,
        out.height() as f32 / natural.height() as f32,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut picture = RgbaImage::new(out.width(), out.height());
    for (target, source) in picture.pixels_mut().zip(pixmap.pixels()) {
        let straight = source.demultiply();
        *target = Rgba([
            straight.red(),
            straight.green(),
            straight.blue(),
            straight.alpha(),
        ]);
    }
    Ok(picture)
}

// --- resampling (Pillow's, so that LANCZOS looks the same) ---------------------------------------------

const PRECISION_BITS: u32 = 32 - 8 - 2;

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let x = x * std::f64::consts::PI;
        x.sin() / x
    }
}

fn lanczos(x: f64) -> f64 {
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

struct Coefficients {
    /// (first input pixel, how many) per output pixel.
    bounds: Vec<(usize, usize)>,
    weights: Vec<i32>,
    size: usize,
}

/// Pillow's `precompute_coeffs` for LANCZOS, with the 8-bit fixed-point weights.
fn coefficients(in_size: usize, in0: f32, in1: f32, out_size: usize) -> Coefficients {
    let scale = ((in1 - in0) / out_size as f32) as f64;
    let filter_scale = scale.max(1.0);
    let support = 3.0 * filter_scale;
    let size = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut weights = vec![0i32; out_size * size];
    for xx in 0..out_size {
        let center = in0 as f64 + (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filter_scale;
        let xmin = ((center - support + 0.5) as i64).max(0) as usize;
        let xmax = (((center + support + 0.5) as i64).max(0) as usize).min(in_size);
        let count = xmax.saturating_sub(xmin);
        let mut raw: Vec<f64> = (0..count)
            .map(|x| lanczos((x as f64 + xmin as f64 - center + 0.5) * ss))
            .collect();
        let total: f64 = raw.iter().sum();
        if total != 0.0 {
            raw.iter_mut().for_each(|w| *w /= total);
        }
        for (x, w) in raw.iter().enumerate() {
            let fixed = if *w < 0.0 {
                (-0.5 + w * (1u64 << PRECISION_BITS) as f64) as i32
            } else {
                (0.5 + w * (1u64 << PRECISION_BITS) as f64) as i32
            };
            weights[xx * size + x] = fixed;
        }
        bounds.push((xmin, count));
    }
    Coefficients {
        bounds,
        weights,
        size,
    }
}

fn clip8(sum: i64) -> u8 {
    (sum >> PRECISION_BITS).clamp(0, 255) as u8
}

/// `Image.resize((w, h), LANCZOS, box=...)` on four 8-bit channels (already premultiplied).
fn resample_channels(
    src: &[u8],
    sw: usize,
    sh: usize,
    ow: usize,
    oh: usize,
    view: (f32, f32, f32, f32),
) -> Vec<u8> {
    let need_horizontal = ow != sw || view.0 != 0.0 || view.2 != sw as f32;
    let need_vertical = oh != sh || view.1 != 0.0 || view.3 != sh as f32;
    let half = 1i64 << (PRECISION_BITS - 1);
    let mut current: Vec<u8> = src.to_vec();
    let (mut cw, mut ch) = (sw, sh);
    if need_horizontal {
        let c = coefficients(sw, view.0, view.2, ow);
        let mut next = vec![0u8; ow * ch * 4];
        for y in 0..ch {
            for (x, &(xmin, count)) in c.bounds.iter().enumerate() {
                let mut sums = [half; 4];
                for i in 0..count {
                    let w = c.weights[x * c.size + i] as i64;
                    let at = (y * cw + xmin + i) * 4;
                    for (k, sum) in sums.iter_mut().enumerate() {
                        *sum += current[at + k] as i64 * w;
                    }
                }
                for (k, sum) in sums.iter().enumerate() {
                    next[(y * ow + x) * 4 + k] = clip8(*sum);
                }
            }
        }
        current = next;
        cw = ow;
    }
    if need_vertical {
        let c = coefficients(sh, view.1, view.3, oh);
        let mut next = vec![0u8; cw * oh * 4];
        for (y, &(ymin, count)) in c.bounds.iter().enumerate() {
            for x in 0..cw {
                let mut sums = [half; 4];
                for i in 0..count {
                    let w = c.weights[y * c.size + i] as i64;
                    let at = ((ymin + i) * cw + x) * 4;
                    for (k, sum) in sums.iter_mut().enumerate() {
                        *sum += current[at + k] as i64 * w;
                    }
                }
                for (k, sum) in sums.iter().enumerate() {
                    next[(y * cw + x) * 4 + k] = clip8(*sum);
                }
            }
        }
        current = next;
        ch = oh;
    }
    debug_assert_eq!((cw, ch), (ow, oh));
    current
}

fn muldiv255(a: u32, b: u32) -> u32 {
    let tmp = a * b + 128;
    ((tmp >> 8) + tmp) >> 8
}

/// `Image.resize` for RGBA: premultiplied while it filters, as Pillow does.
fn resize_rgba(img: &RgbaImage, w: u32, h: u32, view: (f32, f32, f32, f32)) -> RgbaImage {
    let (sw, sh) = (img.width() as usize, img.height() as usize);
    if (w, h) == img.dimensions() && view == (0.0, 0.0, sw as f32, sh as f32) {
        return img.clone();
    }
    let mut premultiplied = Vec::with_capacity(sw * sh * 4);
    for p in img.pixels() {
        let a = p[3] as u32;
        premultiplied.extend_from_slice(&[
            muldiv255(p[0] as u32, a) as u8,
            muldiv255(p[1] as u32, a) as u8,
            muldiv255(p[2] as u32, a) as u8,
            p[3],
        ]);
    }
    let resized = resample_channels(&premultiplied, sw, sh, w as usize, h as usize, view);
    let mut out = RgbaImage::new(w, h);
    for (target, chunk) in out.pixels_mut().zip(resized.chunks_exact(4)) {
        let a = chunk[3] as u32;
        *target = if a == 255 || a == 0 {
            Rgba([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            let un = |c: u8| ((255 * c as u32) / a).min(255) as u8;
            Rgba([un(chunk[0]), un(chunk[1]), un(chunk[2]), chunk[3]])
        };
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Contain,
    Cover,
    Stretch,
}

impl Fit {
    /// "contain" | "cover" | "stretch"; anything else is contain.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Fit {
        match name {
            "stretch" => Fit::Stretch,
            "cover" => Fit::Cover,
            _ => Fit::Contain,
        }
    }
}

/// `ImageOps.contain`: the largest size inside w x h with the picture's proportions.
fn contain_size(iw: u32, ih: u32, w: u32, h: u32) -> (u32, u32) {
    let im_ratio = iw as f64 / ih as f64;
    let dest_ratio = w as f64 / h as f64;
    if im_ratio != dest_ratio {
        if im_ratio > dest_ratio {
            let new_h = py_round(ih as f64 / iw as f64 * w as f64) as u32;
            if new_h != h {
                return (w, new_h);
            }
        } else {
            let new_w = py_round(iw as f64 / ih as f64 * h as f64) as u32;
            if new_w != w {
                return (new_w, h);
            }
        }
    }
    (w, h)
}

/// On a white background, as grey. Exactly w x h.
pub fn fit_image(img: &RgbaImage, w: u32, h: u32, fit: Fit) -> GrayImage {
    let mut canvas = GrayImage::from_pixel(w.max(1), h.max(1), Luma([255]));
    if w == 0 || h == 0 || img.width() == 0 || img.height() == 0 {
        return canvas;
    }
    let fitted = match fit {
        Fit::Stretch => resize_rgba(
            img,
            w,
            h,
            (0.0, 0.0, img.width() as f32, img.height() as f32),
        ),
        Fit::Cover => {
            let (iw, ih) = (img.width() as f64, img.height() as f64);
            let output_ratio = w as f64 / h as f64;
            let live_ratio = iw / ih;
            let (crop_w, crop_h) = if live_ratio == output_ratio {
                (iw, ih)
            } else if live_ratio >= output_ratio {
                (output_ratio * ih, ih)
            } else {
                (iw, iw / output_ratio)
            };
            let left = (iw - crop_w) * 0.5;
            let top = (ih - crop_h) * 0.5;
            resize_rgba(
                img,
                w,
                h,
                (
                    left as f32,
                    top as f32,
                    (left + crop_w) as f32,
                    (top + crop_h) as f32,
                ),
            )
        }
        Fit::Contain => {
            let (cw, ch) = contain_size(img.width(), img.height(), w, h);
            if cw == 0 || ch == 0 {
                return canvas;
            }
            resize_rgba(
                img,
                cw,
                ch,
                (0.0, 0.0, img.width() as f32, img.height() as f32),
            )
        }
    };
    let (ox, oy) = (
        (w as i64 - fitted.width() as i64).div_euclid(2),
        (h as i64 - fitted.height() as i64).div_euclid(2),
    );
    for (x, y, p) in fitted.enumerate_pixels() {
        let (tx, ty) = (ox + x as i64, oy + y as i64);
        if tx < 0 || ty < 0 || tx >= w as i64 || ty >= h as i64 {
            continue;
        }
        let a = p[3] as u32;
        // alpha_composite over opaque white, then convert("RGB").convert("L").
        let over = |c: u8| -> u32 {
            if a == 0 {
                return 255;
            }
            let tmp = c as u32 * a * 128 + 255 * (255 - a) * 128 + (0x80 << 7);
            (((tmp >> 8) + tmp) >> 8) >> 7
        };
        let (r, g, b) = (over(p[0]), over(p[1]), over(p[2]));
        let gray = (r * 19595 + g * 38470 + b * 7471 + 0x8000) >> 16;
        canvas.put_pixel(tx as u32, ty as u32, Luma([gray.min(255) as u8]));
    }
    canvas
}

// --- dithers -------------------------------------------------------------------------------------

pub const DITHERS: [&str; 5] = ["atkinson", "floyd", "bayer", "halftone", "threshold"];

const BAYER_8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// `ImageOps.autocontrast(gray, cutoff=percent)` with a whole-number percent, on an "L" image.
#[allow(clippy::needless_range_loop)]
fn autocontrast(gray: &GrayImage, cutoff: u64) -> GrayImage {
    let mut histogram = [0u64; 256];
    for p in gray.pixels() {
        histogram[p[0] as usize] += 1;
    }
    let n: u64 = histogram.iter().sum();
    if cutoff > 0 {
        let mut cut = n * cutoff / 100;
        for lo in 0..256 {
            if cut > histogram[lo] {
                cut -= histogram[lo];
                histogram[lo] = 0;
            } else {
                histogram[lo] -= cut;
                cut = 0;
            }
            if cut == 0 {
                break;
            }
        }
        let mut cut = n * cutoff / 100;
        for hi in (0..256).rev() {
            if cut > histogram[hi] {
                cut -= histogram[hi];
                histogram[hi] = 0;
            } else {
                histogram[hi] -= cut;
                cut = 0;
            }
            if cut == 0 {
                break;
            }
        }
    }
    let lo = (0..256).find(|&i| histogram[i] != 0).unwrap_or(0);
    let hi = (0..256).rev().find(|&i| histogram[i] != 0).unwrap_or(0);
    let mut lut: [u8; 256] = std::array::from_fn(|i| i as u8);
    if hi > lo {
        let scale = 255.0 / (hi - lo) as f64;
        let offset = -(lo as f64) * scale;
        for (ix, entry) in lut.iter_mut().enumerate() {
            *entry = ((ix as f64 * scale + offset) as i64).clamp(0, 255) as u8;
        }
    }
    let mut out = gray.clone();
    out.pixels_mut().for_each(|p| p[0] = lut[p[0] as usize]);
    out
}

/// A mask (255 = black dot) from a grey picture.
pub fn dither_to_mask(
    gray: &GrayImage,
    method: &str,
    threshold: u8,
    invert: bool,
    contrast: bool,
) -> Result<GrayImage, AssetError> {
    let mut gray = if contrast && method != "threshold" {
        autocontrast(gray, 1)
    } else {
        gray.clone()
    };
    if invert {
        gray.pixels_mut().for_each(|p| p[0] = 255 - p[0]);
    }
    let (width, height) = gray.dimensions();
    match method {
        "threshold" => {
            let mut mask = gray;
            mask.pixels_mut()
                .for_each(|p| p[0] = if p[0] < threshold { 255 } else { 0 });
            Ok(mask)
        }
        "floyd" => Ok(floyd_steinberg(&gray)),
        "bayer" => {
            let mut mask = GrayImage::new(width, height);
            for (x, y, p) in gray.enumerate_pixels() {
                let darkness = 1.0 - p[0] as f64 / 255.0;
                if (BAYER_8[(y % 8) as usize][(x % 8) as usize] as f64 + 0.5) / 64.0 < darkness {
                    mask.put_pixel(x, y, Luma([255]));
                }
            }
            Ok(mask)
        }
        "halftone" => Ok(halftone(&gray, 5)),
        "atkinson" => Ok(atkinson(&gray)),
        _ => Err(AssetError(format!(
            "Unknown dither {}. Use one of {}.",
            py_repr(method),
            DITHERS.join(", ")
        ))),
    }
}

/// Pillow's `convert("1")` (Floyd-Steinberg with its integer error terms), as ink: 255 where it makes black.
fn floyd_steinberg(gray: &GrayImage) -> GrayImage {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut mask = GrayImage::new(w as u32, h as u32);
    let mut below = vec![0i32; w + 2];
    for y in 0..h {
        let mut next = vec![0i32; w + 2];
        let mut right = 0i32;
        for x in 0..w {
            // C division: truncates toward zero, which is what `/` does on i32.
            let value = (gray.get_pixel(x as u32, y as u32)[0] as i32
                + (right + below[x + 1]) / 16)
                .clamp(0, 255);
            let on = value > 128;
            if !on {
                mask.put_pixel(x as u32, y as u32, Luma([255]));
            }
            let error = value - if on { 255 } else { 0 };
            right = 7 * error;
            next[x] += 3 * error;
            next[x + 1] += 5 * error;
            next[x + 2] += error;
        }
        below = next;
    }
    mask
}

/// Keeps more contrast than Floyd-Steinberg, which suits thermal paper.
fn atkinson(gray: &GrayImage) -> GrayImage {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut values: Vec<f64> = gray.as_raw().iter().map(|&v| v as f64).collect();
    let mut mask = GrayImage::new(w as u32, h as u32);
    const NEIGHBOURS: [(i64, i64); 6] = [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)];
    for y in 0..h {
        for x in 0..w {
            let index = y * w + x;
            let old = values[index];
            let new = if old >= 128.0 { 255.0 } else { 0.0 };
            if new == 0.0 {
                mask.put_pixel(x as u32, y as u32, Luma([255]));
            }
            let error = (old - new) / 8.0;
            for (dx, dy) in NEIGHBOURS {
                let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                if nx >= 0 && (nx as usize) < w && (ny as usize) < h {
                    values[ny as usize * w + nx as usize] += error;
                }
            }
        }
    }
    mask
}

fn halftone(gray: &GrayImage, cell: u32) -> GrayImage {
    let (w, h) = gray.dimensions();
    let mut mask = GrayImage::new(w, h);
    let mut top = 0;
    while top < h {
        let mut left = 0;
        while left < w {
            let (right, bottom) = ((left + cell).min(w), (top + cell).min(h));
            let mut sum = 0u64;
            for y in top..bottom {
                for x in left..right {
                    sum += gray.get_pixel(x, y)[0] as u64;
                }
            }
            let count = ((right - left) * (bottom - top)) as f64;
            let radius = (1.0 - sum as f64 / count / 255.0).powf(0.5) * cell as f64 * 0.72;
            let (cx, cy) = (
                left as f64 + cell as f64 / 2.0,
                top as f64 + cell as f64 / 2.0,
            );
            for y in top..bottom {
                for x in left..right {
                    let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
                    if dx * dx + dy * dy <= radius * radius {
                        mask.put_pixel(x, y, Luma([255]));
                    }
                }
            }
            left += cell;
        }
        top += cell;
    }
    mask
}

// --- emoji -------------------------------------------------------------------------------------------

/// The font ships bitmaps at this size; Pillow is asked for it too.
const EMOJI_PPEM: u16 = 160;
/// What Pillow reports for `getmetrics()` of the emoji font at 160: the baseline sits this far below the top.
const EMOJI_ASCENT: i32 = 160;

fn read_u16(buf: &[u8], at: usize) -> Option<u16> {
    buf.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    buf.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The emoji font without its (180 MB) `sbix` bitmaps: an sfnt of the other tables to shape with, and where
/// the bitmaps are in the file, so they can be read one glyph at a time.
struct EmojiFont {
    file: PathBuf,
    shaping: Vec<u8>,
    sbix_offset: u64,
    sbix_length: u64,
}

fn load_emoji_font(path: &Path) -> Option<EmojiFont> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 16];
    file.read_exact(&mut head).ok()?;
    let face_offset = if &head[0..4] == b"ttcf" {
        read_u32(&head, 12)? as u64
    } else {
        0
    };
    file.seek(SeekFrom::Start(face_offset)).ok()?;
    let mut table_head = [0u8; 12];
    file.read_exact(&mut table_head).ok()?;
    let version = &table_head[0..4];
    let count = read_u16(&table_head, 4)? as usize;
    let mut directory = vec![0u8; count * 16];
    file.read_exact(&mut directory).ok()?;
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    let (mut sbix_offset, mut sbix_length) = (0u64, 0u64);
    for record in directory.chunks_exact(16) {
        let tag: [u8; 4] = [record[0], record[1], record[2], record[3]];
        let (offset, length) = (read_u32(record, 8)? as u64, read_u32(record, 12)? as u64);
        if &tag == b"sbix" {
            (sbix_offset, sbix_length) = (offset, length);
            continue;
        }
        if length > 64 * 1024 * 1024 {
            continue;
        }
        let mut data = vec![0u8; length as usize];
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut data).ok()?;
        tables.push((tag, data));
    }
    if sbix_length == 0 {
        return None;
    }
    // Rebuild a plain sfnt from the tables we kept.
    let mut out = Vec::new();
    out.extend_from_slice(version);
    out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0u8; 6]);
    let mut at = 12 + tables.len() * 16;
    let mut body = Vec::new();
    for (tag, data) in &tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(at as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        let pad = (4 - data.len() % 4) % 4;
        body.extend(std::iter::repeat_n(0u8, pad));
        at += data.len() + pad;
    }
    out.extend_from_slice(&body);
    Some(EmojiFont {
        file: path.to_path_buf(),
        shaping: out,
        sbix_offset,
        sbix_length,
    })
}

struct EmojiBitmap {
    x: i32,
    y: i32,
    png: Vec<u8>,
}

impl EmojiFont {
    /// The 160 ppem bitmap for a glyph, following `dupe` records.
    fn bitmap(&self, file: &mut std::fs::File, glyph: u16, depth: u32) -> Option<EmojiBitmap> {
        if depth > 4 {
            return None;
        }
        let mut header = vec![0u8; 8];
        file.seek(SeekFrom::Start(self.sbix_offset)).ok()?;
        file.read_exact(&mut header).ok()?;
        let strikes = read_u32(&header, 4)? as usize;
        let mut offsets = vec![0u8; strikes * 4];
        file.read_exact(&mut offsets).ok()?;
        // The strike nearest to 160 ppem.
        let mut best: Option<(u32, u64)> = None;
        for i in 0..strikes {
            let strike_offset = read_u32(&offsets, i * 4)? as u64;
            let mut strike_head = [0u8; 4];
            file.seek(SeekFrom::Start(self.sbix_offset + strike_offset))
                .ok()?;
            file.read_exact(&mut strike_head).ok()?;
            let ppem = read_u16(&strike_head, 0)? as i32;
            let distance = (ppem - EMOJI_PPEM as i32).unsigned_abs();
            if best.map(|(d, _)| distance < d).unwrap_or(true) {
                best = Some((distance, strike_offset));
            }
        }
        let strike = self.sbix_offset + best?.1;
        let mut pair = [0u8; 8];
        file.seek(SeekFrom::Start(strike + 4 + glyph as u64 * 4))
            .ok()?;
        file.read_exact(&mut pair).ok()?;
        let (start, end) = (read_u32(&pair, 0)? as u64, read_u32(&pair, 4)? as u64);
        if end <= start + 8 || end - start > self.sbix_length {
            return None;
        }
        let mut record = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(strike + start)).ok()?;
        file.read_exact(&mut record).ok()?;
        let x = read_u16(&record, 0)? as i16 as i32;
        let y = read_u16(&record, 2)? as i16 as i32;
        match &record[4..8] {
            b"png " => Some(EmojiBitmap {
                x,
                y,
                png: record[8..].to_vec(),
            }),
            b"dupe" => self.bitmap(file, read_u16(&record, 8)?, depth + 1),
            _ => None,
        }
    }
}

fn alpha_over(canvas: &mut RgbaImage, sprite: &RgbaImage, ox: i32, oy: i32) {
    for (x, y, s) in sprite.enumerate_pixels() {
        let (tx, ty) = (ox as i64 + x as i64, oy as i64 + y as i64);
        if tx < 0
            || ty < 0
            || tx >= canvas.width() as i64
            || ty >= canvas.height() as i64
            || s[3] == 0
        {
            continue;
        }
        let d = canvas.get_pixel_mut(tx as u32, ty as u32);
        if d[3] == 0 || s[3] == 255 {
            *d = *s;
            continue;
        }
        let (sa, da) = (s[3] as f32 / 255.0, d[3] as f32 / 255.0);
        let out_a = sa + da * (1.0 - sa);
        for k in 0..3 {
            d[k] = ((s[k] as f32 * sa + d[k] as f32 * da * (1.0 - sa)) / out_a)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        d[3] = (out_a * 255.0).round() as u8;
    }
}

/// An emoji cropped tight, on transparency. None if this machine has no emoji font.
///
/// The text is shaped (ZWJ families, flags, skin tones, keycaps, variation selectors), then each glyph's
/// 160 px bitmap is placed on a pen that moves by its advance, as one `ImageDraw.text` call would.
pub fn emoji_rgba(text: &str, _size: u32) -> Option<RgbaImage> {
    let path = Path::new(EMOJI_FONT);
    if !path.exists() {
        return None;
    }
    static FONT: OnceLock<Option<EmojiFont>> = OnceLock::new();
    let font = FONT.get_or_init(|| load_emoji_font(path)).as_ref()?;
    let face = rustybuzz::Face::from_slice(&font.shaping, 0)?;
    let upem = face.units_per_em().max(1) as f32;

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.set_direction(rustybuzz::Direction::LeftToRight);
    let shaped = rustybuzz::shape(&face, &[], buffer);
    let mut file = std::fs::File::open(&font.file).ok()?;

    let chars = text.chars().count().clamp(1, 256) as u32;
    let mut canvas = RgbaImage::from_pixel(200 * chars, 200, Rgba([255, 255, 255, 0]));
    let mut pen = 0f32;
    for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        let glyph = info.glyph_id as u16;
        if let Some(bitmap) = font.bitmap(&mut file, glyph, 0) {
            if let Ok(decoded) =
                image::load_from_memory_with_format(&bitmap.png, image::ImageFormat::Png)
            {
                let sprite = decoded.to_rgba8();
                // originOffset is the bitmap's bottom-left corner relative to the baseline.
                let top = EMOJI_ASCENT - (bitmap.y + sprite.height() as i32);
                alpha_over(&mut canvas, &sprite, pen.round() as i32 + bitmap.x, top);
            }
        }
        pen += position.x_advance as f32 * EMOJI_PPEM as f32 / upem;
    }
    let bounds = alpha_bounds(&canvas)?;
    Some(
        image::imageops::crop_imm(
            &canvas,
            bounds.0,
            bounds.1,
            bounds.2 - bounds.0,
            bounds.3 - bounds.1,
        )
        .to_image(),
    )
}

/// The box (x0, y0, x1, y1) of pixels with any alpha.
fn alpha_bounds(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] != 0 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// Any emoji, drawn by the system's emoji font and fitted (contain) onto white. None if this machine has no such font.
pub fn emoji_image(text: &str, w: u32, h: u32) -> Option<GrayImage> {
    emoji_rgba(text, 160).map(|picture| fit_image(&picture, w, h, Fit::Contain))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn fnv(data: &[u8]) -> u64 {
        let mut h = 0xCBF29CE484222325u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001B3);
        }
        h
    }

    fn network_tests() -> bool {
        std::env::var("PRINS_TEST_NETWORK")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prins-assets-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- base64 and sources ---

    #[test]
    fn base64_follows_pythons_lenient_decoder() {
        // (text, what base64.b64decode(text, validate=False) gives; None is an error)
        let cases: &[(&str, Option<&[u8]>)] = &[
            ("QUJD", Some(b"ABC")),
            ("QQ==", Some(b"A")),
            ("QQ", None),
            ("Q", None),
            ("QUJD====", Some(b"ABC")),
            ("QU JD\n", Some(b"ABC")),
            ("!!!!", Some(b"")),
            ("", Some(b"")),
            ("QUJDRA==", Some(b"ABCD")),
            ("QUJDRA=", None),
            ("QUJDRA", None),
            ("QUJDREU=", Some(b"ABCDE")),
            ("QUJDREU", None),
            ("=QUJD", Some(b"ABC")),
            ("QQ==QUJD", Some(b"A")),
            ("QQ=QUJD", None),
            ("QUJD=", Some(b"ABC")),
            ("QUJDRA===", Some(b"ABCD")),
            ("Q=", None),
            ("QQ=", None),
            ("data:xx", None),
            ("AAAA AAAA", Some(&[0; 6])),
            ("QUJD\u{0}RA==", Some(b"ABCD")),
            ("é", None),
            ("QUJDREVG", Some(b"ABCDEF")),
        ];
        for &(text, want) in cases {
            match (decode_base64(text), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{text:?}"),
                (Err(e), None) => assert_eq!(e.0, "That isn't valid base64.", "{text:?}"),
                (got, want) => panic!("{text:?}: got {got:?}, python gives {want:?}"),
            }
        }
    }

    #[test]
    fn read_source_handles_inline_data_markup_files_and_the_rest() {
        assert_eq!(
            read_source("data:text/plain;base64,QUJD", false).unwrap(),
            b"ABC"
        );
        assert_eq!(read_source("data:,QUJD", true).unwrap(), b"ABC");
        let svg = "  <svg xmlns='http://www.w3.org/2000/svg'/>";
        assert_eq!(read_source(svg, false).unwrap(), svg.as_bytes());
        assert_eq!(
            read_source("<?xml version='1.0'?><svg/>", true).unwrap(),
            b"<?xml version='1.0'?><svg/>"
        );
        // Raw base64 needs 32 characters to count as base64.
        let long = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=";
        assert_eq!(
            read_source(long, false).unwrap(),
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        );
        assert_eq!(
            read_source("short", true).unwrap_err().0,
            "Can't find 'short'. Give a URL, a file path, or base64."
        );
        assert_eq!(
            read_source("it's", true).unwrap_err().0,
            "Can't find \"it's\". Give a URL, a file path, or base64."
        );
        // Without `external`: no network and no files.
        assert_eq!(
            read_source("https://example.com/a.png", false)
                .unwrap_err()
                .0,
            "This design can't load from the network."
        );
        assert_eq!(
            read_source("/etc/hosts", false).unwrap_err().0,
            "This design can't read files."
        );
        assert_eq!(
            read_source("a/b.png", false).unwrap_err().0,
            "This design can't read files."
        );
        // With it, files are read, `~` is expanded, and big ones refused.
        let dir = scratch_dir("source");
        let small = dir.join("small.bin");
        std::fs::write(&small, b"hello").unwrap();
        assert_eq!(
            read_source(small.to_str().unwrap(), true).unwrap(),
            b"hello"
        );
        let big = dir.join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_DOWNLOAD_BYTES as u64 + 1)
            .unwrap();
        assert!(read_source(big.to_str().unwrap(), true)
            .unwrap_err()
            .0
            .ends_with("is over 20 MB."));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read_source(&format!("{}/definitely/not/here.png", dir.display()), true).is_err());
    }

    #[test]
    fn python_repr_and_round() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\"b'c"), "'a\"b\\'c'");
        assert_eq!(py_repr("a\nb\\"), "'a\\nb\\\\'");
        assert_eq!(
            [0.5, 1.5, 2.5, -0.5, 2.4, 2.6].map(py_round),
            [0, 2, 2, 0, 2, 3]
        );
    }

    // --- downloads and logos ---

    #[test]
    fn the_cache_file_is_the_one_python_uses() {
        // hashlib.sha256(b"https://example.com/x.svg").hexdigest()
        let name = cache_path("https://example.com/x.svg");
        assert_eq!(
            name.file_name().unwrap().to_str().unwrap(),
            "99be11a8a8d7fabe02e761ef27a1e49711f0ca365b0e7c827dd00a3ad2b25d91"
        );
        assert!(name.parent().unwrap().ends_with(".prins/cache"));
    }

    #[test]
    fn logo_names_follow_simple_icons_slug_rules() {
        for (name, slug) in [
            ("Spotify", "spotify"),
            ("  GitHub ", "github"),
            ("Coca-Cola", "cocacola"),
            ("Node.js", "nodedotjs"),
            ("C++", "cplusplus"),
            ("AT&T", "atandt"),
            ("Mc Donald's", "mcdonalds"),
            ("ÅÄÖ", ""),
            ("", ""),
        ] {
            assert_eq!(logo_slug(name), slug, "{name:?}");
        }
        assert_eq!(
            logo_url("spotify"),
            "https://cdn.jsdelivr.net/npm/simple-icons@16.36.0/icons/spotify.svg"
        );
        assert_eq!(logo_svg("").unwrap_err().0, "'' isn't a logo name.");
        assert_eq!(logo_svg("%%%").unwrap_err().0, "'%%%' isn't a logo name.");
    }

    /// Serves `response` to every connection on a free local port and sends back what was requested.
    fn serve(response: Vec<u8>, connections: usize) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let Ok(mut stream) = stream else { continue };
                let mut request = vec![0u8; 4096];
                let n = stream.read(&mut request).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&request[..n]).to_string());
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        (url, rx)
    }

    fn http(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn download_sends_the_user_agent_and_reads_the_body() {
        let (url, requests) = serve(http("200 OK", b"<svg/>"), 1);
        assert_eq!(
            download(&format!("{url}/logo.svg"), false).unwrap(),
            b"<svg/>"
        );
        let request = requests.recv().unwrap().to_lowercase();
        assert!(request.starts_with("get /logo.svg "), "{request}");
        assert!(
            request.contains("user-agent: prins-note-designer/0.1"),
            "{request}"
        );
    }

    #[test]
    fn download_reports_http_errors_and_dead_servers() {
        let (url, _) = serve(http("404 Not Found", b"nope"), 1);
        let message = download(&format!("{url}/missing"), false).unwrap_err().0;
        assert!(
            message.starts_with(&format!("Couldn't download {url}/missing: ")),
            "{message}"
        );
        // Nothing listening.
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/x", listener.local_addr().unwrap())
        };
        assert!(download(&closed, false)
            .unwrap_err()
            .0
            .starts_with("Couldn't download "));
        assert!(download("not a url", false).is_err());
        assert!(read_source("https://127.0.0.1:1/x.png", true).is_err());
    }

    #[test]
    fn download_refuses_more_than_20_megabytes() {
        let (url, _) = serve(http("200 OK", &vec![0u8; MAX_DOWNLOAD_BYTES + 10]), 2);
        let message = download(&format!("{url}/huge"), false).unwrap_err().0;
        assert!(message.ends_with("/huge is over 20 MB."), "{message}");
        // Exactly at the cap is fine.
        let (url, _) = serve(http("200 OK", &vec![7u8; MAX_DOWNLOAD_BYTES]), 1);
        assert_eq!(
            download(&format!("{url}/exact"), false).unwrap().len(),
            MAX_DOWNLOAD_BYTES
        );
    }

    #[test]
    fn real_logos_render() {
        if !network_tests() {
            return;
        }
        for name in [
            "spotify",
            "Spotify",
            "github",
            "nike",
            "apple",
            "Coca-Cola",
            "Node.js",
        ] {
            let svg = logo_svg(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(is_svg(&svg), "{name}");
            let picture = load_image(&svg, 150, 150).unwrap();
            let gray = fit_image(&picture, 150, 150, Fit::Contain);
            let mask = dither_to_mask(&gray, "threshold", 128, false, true).unwrap();
            assert!(
                mask.pixels().filter(|p| p[0] == 255).count() > 800,
                "{name}"
            );
        }
        let err = logo_svg("definitely-not-a-real-brand-xyz").unwrap_err().0;
        assert!(err.contains("draw_image or draw_svg"), "{err}");
    }

    // --- pictures ---

    #[test]
    fn svg_detection() {
        assert!(is_svg(b"<svg xmlns='x'/>"));
        assert!(is_svg(b" \n\t<SVG/>"));
        assert!(is_svg(b"<?xml version='1.0'?><svg/>"));
        assert!(is_svg(b"<!-- c --><svg/>"));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_svg(b"<?xml version='1.0'?><html/>"));
        assert!(!is_svg(b""));
        let mut late = vec![b' '; 10];
        late.extend(std::iter::repeat_n(b'x', 400));
        late.extend_from_slice(b"<svg/>");
        assert!(!is_svg(&late));
    }

    /// Pillow's `_render_svg` through resvg-py: (name, svg, wanted w, wanted h, output size, pixels at least half black).
    #[test]
    fn svgs_come_out_the_size_python_gives() {
        const NS: &str = "xmlns=\"http://www.w3.org/2000/svg\"";
        let cases = [
            ("vb24", format!("<svg {NS} viewBox=\"0 0 24 24\"><path d=\"M0 0H24V24H0Z\"/></svg>"), 150, 150, (150, 150), 22500),
            ("vb10x3", format!("<svg {NS} viewBox=\"0 0 10 3\"><rect width=\"10\" height=\"3\"/></svg>"), 100, 50, (100, 30), 3000),
            ("vb10x3b", format!("<svg {NS} viewBox=\"0 0 10 3\"><rect width=\"10\" height=\"3\"/></svg>"), 33, 77, (33, 10), 330),
            ("wh_only", format!("<svg {NS} width=\"200\" height=\"100\"><rect width=\"200\" height=\"100\"/></svg>"), 80, 80, (80, 40), 3200),
            ("vb_and_wh", format!("<svg {NS} width=\"48\" height=\"48\" viewBox=\"0 0 24 24\"><circle cx=\"12\" cy=\"12\" r=\"12\"/></svg>"), 100, 60, (60, 60), 2835),
            ("circle10", format!("<svg {NS} viewBox=\"0 0 10 10\"><circle cx=\"5\" cy=\"5\" r=\"5\"/></svg>"), 50, 50, (50, 50), 1970),
            ("nosize", format!("<svg {NS}><circle cx=\"5\" cy=\"5\" r=\"5\"/></svg>"), 50, 50, (50, 50), 1970),
            ("vb_odd", format!("<svg {NS} viewBox=\"0 0 17 13\"><rect width=\"17\" height=\"13\"/></svg>"), 123, 45, (59, 46), 2714),
            ("vb_neg", format!("<svg {NS} viewBox=\"-5 -5 10 10\"><circle r=\"5\"/></svg>"), 40, 40, (40, 40), 1266),
            ("vb_comma", format!("<svg {NS} viewBox=\"0,0,24,12\"><rect width=\"24\" height=\"12\"/></svg>"), 90, 90, (90, 45), 4050),
            ("wh_vb_mismatch", format!("<svg {NS} width=\"100\" height=\"50\" viewBox=\"0 0 24 24\"><rect width=\"24\" height=\"24\"/></svg>"), 60, 60, (60, 30), 900),
        ];
        for (name, svg, w, h, size, black) in cases {
            let picture =
                load_image(svg.as_bytes(), w, h).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(picture.dimensions(), size, "{name}");
            let got = picture
                .pixels()
                .filter(|p| p[3] > 127 && p[0] < 128)
                .count() as i64;
            assert!(
                (got - black).abs() <= black / 50 + 4,
                "{name}: {got} black pixels, Python has {black}"
            );
        }
    }

    #[test]
    fn bad_svg_is_an_error_not_a_panic() {
        let err = load_image(b"<svg><<<", 50, 50).unwrap_err().0;
        assert!(err.starts_with("Couldn't render that SVG: "), "{err}");
        let _ = load_image(
            b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 0 0'/>",
            10,
            10,
        );
        // Absurd sizes are refused rather than allocated.
        let huge = load_image(b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'><rect width='1' height='1'/></svg>", 100_000, 100_000);
        assert!(huge
            .unwrap_err()
            .0
            .starts_with("Couldn't render that SVG: "));
        assert!(load_image(
            b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'/>",
            0,
            0
        )
        .is_ok());
    }

    #[test]
    fn ordinary_images_decode_with_their_orientation() {
        let png = {
            let mut bytes = Vec::new();
            let img = RgbaImage::from_fn(5, 3, |x, _| Rgba([x as u8 * 50, 10, 20, 255]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let decoded = load_image(&png, 99, 99).unwrap();
        assert_eq!(decoded.dimensions(), (5, 3));
        assert_eq!(decoded.get_pixel(4, 2).0, [200, 10, 20, 255]);
        // A 16x8 JPEG, red left and blue right, with EXIF orientation 6: it comes out 8x16, red on top.
        let jpeg: Vec<u8> = (0..EXIF_JPEG_HEX.len() / 2)
            .map(|i| u8::from_str_radix(&EXIF_JPEG_HEX[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let turned = load_image(&jpeg, 99, 99).unwrap();
        assert_eq!(turned.dimensions(), (8, 16));
        let (top, bottom) = (turned.get_pixel(1, 1), turned.get_pixel(1, 14));
        assert!(top[0] > 200 && top[2] < 60, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "{bottom:?}");
    }

    #[test]
    fn broken_images_are_errors() {
        for data in [
            &b""[..],
            b"not an image",
            b"\x89PNG\r\n\x1a\n",
            b"\xff\xd8\xff\xe0",
            b"GIF89a",
        ] {
            let err = load_image(data, 10, 10).unwrap_err().0;
            assert!(err.starts_with("Couldn't read that image: "), "{err}");
        }
        // Truncated halfway through the pixels.
        let mut bytes = Vec::new();
        let img = RgbaImage::from_fn(64, 64, |x, y| Rgba([(x * y) as u8, x as u8, y as u8, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes.truncate(bytes.len() / 2);
        assert!(load_image(&bytes, 10, 10).is_err());
    }

    #[test]
    fn fitting_matches_pillows_lanczos() {
        let source = RgbaImage::from_fn(50, 30, |x, y| {
            Rgba([
                ((x * 5) % 256) as u8,
                ((y * 8) % 256) as u8,
                (((x + y) * 3) % 256) as u8,
                (255 - ((x * y) % 200)) as u8,
            ])
        });
        // (fit, w, h, sum of the grey pixels, FNV of them) from ImageOps.contain/fit/resize + alpha_composite on white.
        for (fit, w, h, sum, hash) in FIT_REFERENCE {
            let got = fit_image(&source, *w, *h, Fit::from_str(fit));
            assert_eq!(got.dimensions(), (*w, *h), "{fit}");
            let total: u64 = got.as_raw().iter().map(|&v| v as u64).sum();
            if fnv(got.as_raw()) != *hash {
                // One cover case differs by 2 levels on a few pixels, from Pillow's float32 crop box.
                assert!(
                    ((total as i64) - (*sum as i64)).abs() < (*sum as i64) / 500,
                    "{fit} {w}x{h}: sum {total}, Pillow {sum}"
                );
            }
        }
        let exact = FIT_REFERENCE
            .iter()
            .filter(|(fit, w, h, _, hash)| {
                fnv(fit_image(&source, *w, *h, Fit::from_str(fit)).as_raw()) == *hash
            })
            .count();
        assert!(
            exact >= FIT_REFERENCE.len() - 1,
            "{exact} of {} bit-exact",
            FIT_REFERENCE.len()
        );
    }

    #[test]
    fn fit_modes() {
        assert_eq!(
            [
                Fit::from_str("contain"),
                Fit::from_str("cover"),
                Fit::from_str("stretch"),
                Fit::from_str("?"),
                Fit::from_str("")
            ],
            [
                Fit::Contain,
                Fit::Cover,
                Fit::Stretch,
                Fit::Contain,
                Fit::Contain
            ]
        );
        let red = RgbaImage::from_pixel(40, 10, Rgba([255, 0, 0, 255]));
        let contain = fit_image(&red, 20, 20, Fit::Contain);
        assert_eq!(contain.dimensions(), (20, 20));
        assert_eq!(
            (contain.get_pixel(10, 0)[0], contain.get_pixel(10, 10)[0]),
            (255, 76)
        ); // white above, red (grey 76) in the middle
        let cover = fit_image(&red, 20, 20, Fit::Cover);
        assert!(cover.pixels().all(|p| p[0] == 76));
        // Transparent pixels are white; nothing is left empty.
        let clear = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        assert!(fit_image(&clear, 16, 4, Fit::Contain)
            .pixels()
            .all(|p| p[0] == 255));
        // Odd sizes don't panic.
        for (w, h) in [(0, 0), (1, 1), (0, 5), (5, 0), (3000, 1)] {
            let out = fit_image(&red, w, h, Fit::Contain);
            assert_eq!(out.width(), w.max(1));
        }
        assert_eq!(
            fit_image(&RgbaImage::new(0, 0), 7, 7, Fit::Cover).dimensions(),
            (7, 7)
        );
    }

    // --- dithers ---

    fn test_picture(name: &str) -> GrayImage {
        match name {
            "ramp" => {
                GrayImage::from_fn(37, 29, |x, y| image::Luma([((x * 7 + y * 13) % 256) as u8]))
            }
            "gradient" => GrayImage::from_fn(60, 40, |_, y| image::Luma([((y * 255) / 39) as u8])),
            _ => {
                let mut v: u64 = 12345;
                let mut img = GrayImage::new(50, 33);
                for p in img.pixels_mut() {
                    v = (v * 1103515245 + 12345) & 0x7FFFFFFF;
                    p.0[0] = ((v >> 16) & 0xFF) as u8;
                }
                img
            }
        }
    }

    #[test]
    fn every_dither_is_bit_exact_with_python() {
        for &(picture, method, threshold, invert, contrast, black, hash) in DITHER_REFERENCE {
            let mask = dither_to_mask(&test_picture(picture), method, threshold, invert, contrast)
                .unwrap();
            assert!(mask.pixels().all(|p| p[0] == 0 || p[0] == 255));
            let count = mask.pixels().filter(|p| p[0] == 255).count();
            assert_eq!(
                count, black,
                "{picture} {method} invert={invert} contrast={contrast}: black dots"
            );
            assert_eq!(
                fnv(mask.as_raw()),
                hash,
                "{picture} {method} invert={invert} contrast={contrast}"
            );
        }
    }

    #[test]
    fn dither_tone_and_errors() {
        let ramp = GrayImage::from_fn(100, 100, |_, y| image::Luma([(y * 255 / 99) as u8]));
        for method in DITHERS {
            let mask = dither_to_mask(&ramp, method, 128, false, true).unwrap();
            assert_eq!(mask.dimensions(), (100, 100));
            let dark = |top: u32| {
                (top..top + 20)
                    .map(|y| (0..100).filter(|&x| mask.get_pixel(x, y)[0] == 255).count())
                    .sum::<usize>()
            };
            assert!(dark(0) > dark(80), "{method}: the dark end has more ink");
            assert!(dark(0) > 0 && dark(80) < 2000, "{method}");
        }
        let err = dither_to_mask(&ramp, "stucki", 128, false, true)
            .unwrap_err()
            .0;
        assert_eq!(
            err,
            "Unknown dither 'stucki'. Use one of atkinson, floyd, bayer, halftone, threshold."
        );
        // White gives no ink, black gives all of it, for every method.
        for method in DITHERS {
            let white = dither_to_mask(
                &GrayImage::from_pixel(20, 20, image::Luma([255])),
                method,
                128,
                false,
                false,
            )
            .unwrap();
            let black = dither_to_mask(
                &GrayImage::from_pixel(20, 20, image::Luma([0])),
                method,
                128,
                false,
                false,
            )
            .unwrap();
            // (Halftone keeps Python's `<=`: a white cell still leaves the dot of radius 0 at its centre pixel.)
            let allowed = if method == "halftone" { 16 } else { 0 };
            assert!(
                white.pixels().filter(|p| p[0] == 255).count() <= allowed,
                "{method}"
            );
            assert!(
                black.pixels().filter(|p| p[0] == 255).count() >= 300,
                "{method}"
            );
        }
        // Empty and tiny pictures.
        for method in DITHERS {
            assert_eq!(
                dither_to_mask(&GrayImage::new(0, 0), method, 128, true, true)
                    .unwrap()
                    .dimensions(),
                (0, 0)
            );
            assert_eq!(
                dither_to_mask(&GrayImage::new(1, 1), method, 128, false, true)
                    .unwrap()
                    .dimensions(),
                (1, 1)
            );
        }
    }

    #[test]
    fn autocontrast_stretches_after_trimming_one_percent() {
        // 98 mid greys between two extreme outliers: cutoff=1 trims the outliers' ends away.
        let mut img = GrayImage::from_pixel(10, 10, image::Luma([100]));
        img.put_pixel(0, 0, image::Luma([0]));
        img.put_pixel(1, 0, image::Luma([255]));
        for x in 2..52 {
            img.put_pixel(x % 10, x / 10, image::Luma([150]));
        }
        let out = autocontrast(&img, 1);
        assert_eq!(out.get_pixel(9, 9)[0], 0); // the 100s are now the floor
        assert_eq!(out.get_pixel(5, 2)[0], 255); // and the 150s the ceiling
                                                 // Flat pictures are left alone.
        let flat = GrayImage::from_pixel(4, 4, image::Luma([77]));
        assert_eq!(autocontrast(&flat, 1), flat);
    }

    // --- emoji ---

    fn has_emoji_font() -> bool {
        Path::new(EMOJI_FONT).exists()
    }

    /// Sizes from Pillow drawing Apple Color Emoji at 160 px and cropping to the box of the ink.
    #[test]
    fn single_emoji_are_the_size_pillow_gives() {
        if !has_emoji_font() {
            assert!(emoji_rgba("\u{1F600}", 160).is_none());
            return;
        }
        for (name, text, size) in [
            ("smile", "\u{1F600}", (160, 160)),
            ("heart", "\u{2764}\u{FE0F}", (151, 149)),
            ("dragon", "\u{1F409}", (160, 156)),
            ("star", "\u{2B50}", (159, 150)),
            ("two thumbs", "\u{1F44D}\u{1F44D}", (307, 159)),
        ] {
            let picture = emoji_rgba(text, 160).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(picture.dimensions(), size, "{name}");
            assert!(picture.pixels().any(|p| p[3] == 255));
        }
    }

    /// Pillow without raqm draws these code point by code point (a flag is nothing, a family is three people
    /// side by side); here they are shaped into the one glyph the font has for the sequence.
    #[test]
    fn sequences_are_shaped_into_one_glyph() {
        if !has_emoji_font() {
            return;
        }
        for (name, text, size) in [
            ("flag", "\u{1F1F3}\u{1F1F4}", (142, 102)),
            (
                "family",
                "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
                (160, 160),
            ),
            ("skin tone", "\u{1F44D}\u{1F3FD}", (147, 159)),
            ("keycap", "1\u{FE0F}\u{20E3}", (160, 160)),
            (
                "rainbow flag",
                "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
                (160, 160),
            ),
        ] {
            let picture = emoji_rgba(text, 160).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(picture.dimensions(), size, "{name}");
        }
        // The skin tone version is not the plain thumb: a different glyph.
        let plain = emoji_rgba("\u{1F44D}", 160).unwrap();
        let toned = emoji_rgba("\u{1F44D}\u{1F3FD}", 160).unwrap();
        assert_ne!(plain.as_raw(), toned.as_raw());
    }

    #[test]
    fn emoji_images_fit_the_box_on_white() {
        if !has_emoji_font() {
            assert!(emoji_image("\u{1F600}", 50, 50).is_none());
            return;
        }
        let image = emoji_image("\u{1F409}", 60, 40).unwrap();
        assert_eq!(image.dimensions(), (60, 40));
        assert_eq!(image.get_pixel(0, 0)[0], 255, "white outside the picture");
        assert!(image.pixels().any(|p| p[0] < 200));
        // Nothing to draw, or nothing the font knows: no picture, no panic.
        assert!(emoji_rgba("", 160).is_none());
        let _ = emoji_rgba("\u{0}", 160);
        let _ = emoji_rgba(&"\u{1F600}".repeat(50), 160);
        let _ = emoji_rgba("\u{F0000}\u{FFFF}", 160);
    }

    #[test]
    fn emoji_font_loading_survives_junk() {
        assert!(load_emoji_font(Path::new("/no/such/file.ttc")).is_none());
        assert!(load_emoji_font(Path::new("/etc/hosts")).is_none());
        let dir = scratch_dir("junk");
        for (name, bytes) in [
            ("empty.ttc", vec![]),
            ("tiny.ttc", b"ttcf\0\x01\0\0\0\0\0\x01\0\0\0\x10".to_vec()),
            ("zeros.ttc", vec![0u8; 4096]),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(load_emoji_font(&path).is_none(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const EXIF_JPEG_HEX: &str = "ffd8ffe000104a46494600010100000100010000ffe100224578696600004d4d002a00000008000101120003000000010006000000000000ffdb0043000201010101010201010102020202020403020202020504040304060506060605060606070908060709070606080b08090a0a0a0a0a06080b0c0b0a0c090a0a0affdb004301020202020202050303050a0706070a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0affc00011080008001003012200021101031101ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b5100002010303020403050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a737475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffc4001f0100030101010101010101010000000000000102030405060708090a0bffc400b51100020102040403040705040400010277000102031104052131061241510761711322328108144291a1b1c109233352f0156272d10a162434e125f11718191a262728292a35363738393a434445464748494a535455565758595a636465666768696a737475767778797a82838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9faffda000c03010002110311003f00f8bebe43a28afea8fd997ff355ff00dc8ffeee1fda9fb433fe699ffb9dff00dd43ffd9";

    /// (picture, method, threshold, invert, contrast, black dots, FNV-1a of the mask) from `assets.dither_to_mask`.
    #[rustfmt::skip]
    const DITHER_REFERENCE: &[(&str, &str, u8, bool, bool, usize, u64)] = &[
    ("ramp", "atkinson", 128, false, true, 517, 5240217886020564322u64),
    ("ramp", "atkinson", 128, false, false, 515, 7805380923421853624u64),
    ("ramp", "atkinson", 128, true, true, 558, 1602420121738817117u64),
    ("ramp", "atkinson", 128, true, false, 559, 8879629993324427614u64),
    ("ramp", "floyd", 128, false, true, 532, 1880132274118061281u64),
    ("ramp", "floyd", 128, false, false, 529, 12446636487093502970u64),
    ("ramp", "floyd", 128, true, true, 544, 14874877633823474621u64),
    ("ramp", "floyd", 128, true, false, 546, 8589406947928992947u64),
    ("ramp", "bayer", 128, false, true, 551, 15713395429293884902u64),
    ("ramp", "bayer", 128, false, false, 549, 1321460438728386846u64),
    ("ramp", "bayer", 128, true, true, 541, 18328726983431205322u64),
    ("ramp", "bayer", 128, true, false, 542, 6163544811385355119u64),
    ("ramp", "halftone", 128, false, true, 810, 12464354727473375053u64),
    ("ramp", "halftone", 128, false, false, 814, 9118784101759102133u64),
    ("ramp", "halftone", 128, true, true, 785, 15425152952814891518u64),
    ("ramp", "halftone", 128, true, false, 793, 9958085339311537190u64),
    ("ramp", "threshold", 128, false, true, 539, 13222598550685150822u64),
    ("ramp", "threshold", 128, false, false, 539, 13222598550685150822u64),
    ("ramp", "threshold", 128, true, true, 534, 16832620641968870919u64),
    ("ramp", "threshold", 128, true, false, 534, 16832620641968870919u64),
    ("ramp", "threshold", 90, false, true, 378, 1792364203381784623u64),
    ("gradient", "atkinson", 128, false, true, 1178, 1939610982988499249u64),
    ("gradient", "atkinson", 128, false, false, 1178, 1939610982988499249u64),
    ("gradient", "atkinson", 128, true, true, 1224, 1656541637058801075u64),
    ("gradient", "atkinson", 128, true, false, 1224, 1656541637058801075u64),
    ("gradient", "floyd", 128, false, true, 1194, 15294802089112514775u64),
    ("gradient", "floyd", 128, false, false, 1194, 15294802089112514775u64),
    ("gradient", "floyd", 128, true, true, 1208, 4243598053342441083u64),
    ("gradient", "floyd", 128, true, false, 1208, 4243598053342441083u64),
    ("gradient", "bayer", 128, false, true, 1200, 5688725888452095621u64),
    ("gradient", "bayer", 128, false, false, 1200, 5688725888452095621u64),
    ("gradient", "bayer", 128, true, true, 1200, 4796288440971775541u64),
    ("gradient", "bayer", 128, true, false, 1200, 4796288440971775541u64),
    ("gradient", "halftone", 128, false, true, 1680, 13280209145191950877u64),
    ("gradient", "halftone", 128, false, false, 1680, 13280209145191950877u64),
    ("gradient", "halftone", 128, true, true, 1632, 9788469947435969237u64),
    ("gradient", "halftone", 128, true, false, 1632, 9788469947435969237u64),
    ("gradient", "threshold", 128, false, true, 1200, 2949379001996854773u64),
    ("gradient", "threshold", 128, false, false, 1200, 2949379001996854773u64),
    ("gradient", "threshold", 128, true, true, 1200, 11353634731354794485u64),
    ("gradient", "threshold", 128, true, false, 1200, 11353634731354794485u64),
    ("gradient", "threshold", 90, false, true, 840, 9734424438540962525u64),
    ("noise", "atkinson", 128, false, true, 813, 13832435459736635780u64),
    ("noise", "atkinson", 128, false, false, 813, 3180707541894963862u64),
    ("noise", "atkinson", 128, true, true, 837, 6135271653909047906u64),
    ("noise", "atkinson", 128, true, false, 837, 4520601171670479926u64),
    ("noise", "floyd", 128, false, true, 813, 10680685801735974134u64),
    ("noise", "floyd", 128, false, false, 814, 2573633353444367205u64),
    ("noise", "floyd", 128, true, true, 837, 10371279843925780696u64),
    ("noise", "floyd", 128, true, false, 838, 6933928035449352219u64),
    ("noise", "bayer", 128, false, true, 810, 13549098691614536425u64),
    ("noise", "bayer", 128, false, false, 809, 12059639409824561922u64),
    ("noise", "bayer", 128, true, true, 826, 7965335311278067959u64),
    ("noise", "bayer", 128, true, false, 827, 2570466412727686616u64),
    ("noise", "halftone", 128, false, true, 1370, 10164321609335592143u64),
    ("noise", "halftone", 128, false, false, 1370, 10164321609335592143u64),
    ("noise", "halftone", 128, true, true, 1400, 9412982813451132267u64),
    ("noise", "halftone", 128, true, false, 1400, 9412982813451132267u64),
    ("noise", "threshold", 128, false, true, 805, 10330378892015867618u64),
    ("noise", "threshold", 128, false, false, 805, 10330378892015867618u64),
    ("noise", "threshold", 128, true, true, 845, 3876691118989762984u64),
    ("noise", "threshold", 128, true, false, 845, 3876691118989762984u64),
    ("noise", "threshold", 90, false, true, 550, 16906789055626764997u64),
    ];

    /// (fit, w, h, sum, FNV-1a) of `assets.fit_image(...).convert("L")`.
    #[rustfmt::skip]
    const FIT_REFERENCE: &[(&str, u32, u32, u64, u64)] = &[
    ("contain", 40, 40, 317198, 2095483091954675735u64),
    ("contain", 100, 60, 964986, 10755455747710958967u64),
    ("contain", 25, 50, 283354, 11563931234306969717u64),
    ("contain", 50, 30, 240810, 7097732932240116317u64),
    ("contain", 13, 7, 15314, 9197718307833226909u64),
    ("cover", 40, 40, 265549, 7169653238746740738u64),
    ("cover", 100, 60, 964986, 10755455747710958967u64),
    ("cover", 25, 50, 207861, 5460313248687149674u64),
    ("cover", 50, 30, 240810, 7097732932240116317u64),
    ("cover", 13, 7, 14875, 13402465616516943728u64),
    ("stretch", 40, 40, 256843, 5836801056013302478u64),
    ("stretch", 100, 60, 964986, 10755455747710958967u64),
    ("stretch", 25, 50, 200666, 14560461274102991721u64),
    ("stretch", 50, 30, 240810, 7097732932240116317u64),
    ("stretch", 13, 7, 14647, 13950543042157054250u64),
    ];
}
