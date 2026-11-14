//! The design engine: applies small drawing operations to the 312x216 grid. Port of `canvas.py`.
//!
//! Ops are `serde_json::Value`s and are read the way Python reads them: numbers go through `round(float(v))`
//! (ints, floats, numeric strings and booleans are all accepted), text through `str(v)`, flags through
//! truthiness. A failing op leaves the picture untouched (Python can leave a half-drawn `repeat`; this
//! restores the page), and messages keep Python's wording.
//!
//! A page image is `GrayImage` with 255 = paper and 0 = ink; masks are 255 where something is set.

use base64::Engine as _;
use image::GrayImage;
use serde_json::{Map, Value};

use crate::pyrandom::PyRandom;
use crate::raster::{self, Resample, H, W};
use crate::{assets, draw, figures, fonts, formula, frames, patterns, poster};

pub const WIDTH: u32 = W;
pub const HEIGHT: u32 = H;
/// 39 bytes per row, 8424 in all.
pub const BITMAP_BYTES: usize = (WIDTH as usize).div_ceil(8) * HEIGHT as usize;
pub const MAX_OPS: usize = 2000;
const MAX_REPEAT: i64 = 500;
/// Python integers have no limit; coordinates here are clamped to something that cannot overflow.
const INT_LIMIT: f64 = (1u64 << 40) as f64;

/// What Python raises as `DesignError`: the message is meant for the agent that wrote the op.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DesignError(pub String);

impl From<assets::AssetError> for DesignError {
    fn from(e: assets::AssetError) -> Self {
        DesignError(e.0)
    }
}

impl From<formula::FormulaError> for DesignError {
    fn from(e: formula::FormulaError) -> Self {
        DesignError(e.0)
    }
}

/// Failure inside an op handler: a `DesignError` passes through; `Py` stands for the TypeError, KeyError and
/// ValueError that Python's `_run` wraps as `Bad <op> op: <message>`.
enum Fail {
    Design(DesignError),
    Py(String),
}

impl From<DesignError> for Fail {
    fn from(e: DesignError) -> Self {
        Fail::Design(e)
    }
}

impl From<assets::AssetError> for Fail {
    fn from(e: assets::AssetError) -> Self {
        Fail::Design(e.into())
    }
}

impl From<formula::FormulaError> for Fail {
    fn from(e: formula::FormulaError) -> Self {
        Fail::Design(e.into())
    }
}

fn design(message: impl Into<String>) -> DesignError {
    DesignError(message.into())
}

// ================================================================================================
// Python values
// ================================================================================================

/// The bits of Python's data model that op reading needs: `str()`, `repr()`, `float()`, `int()`, truthiness.
pub mod pyval {
    use serde_json::Value;

    /// `bool(value)`.
    pub fn truthy(value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    i != 0
                } else if let Some(u) = n.as_u64() {
                    u != 0
                } else {
                    n.as_f64().map(|f| f != 0.0).unwrap_or(true)
                }
            }
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        }
    }

    /// `type(value).__name__`.
    pub fn type_name(value: &Value) -> &'static str {
        match value {
            Value::Null => "NoneType",
            Value::Bool(_) => "bool",
            Value::Number(n) => {
                if n.is_f64() {
                    "float"
                } else {
                    "int"
                }
            }
            Value::String(_) => "str",
            Value::Array(_) => "list",
            Value::Object(_) => "dict",
        }
    }

    /// `repr(float)`: the shortest text that reads back, `1e+16` style beyond 16 digits.
    pub fn float_repr(f: f64) -> String {
        if f.is_nan() {
            return "nan".into();
        }
        if f.is_infinite() {
            return if f > 0.0 { "inf".into() } else { "-inf".into() };
        }
        if f == 0.0 {
            return if f.is_sign_negative() {
                "-0.0".into()
            } else {
                "0.0".into()
            };
        }
        let sci = format!("{:e}", f.abs());
        let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        let sign = if f < 0.0 { "-" } else { "" };
        let n = digits.len() as i32;
        let decpt = exp + 1;
        if (-4 < decpt) && (decpt <= 16) {
            if decpt <= 0 {
                format!("{sign}0.{}{digits}", "0".repeat((-decpt) as usize))
            } else if decpt >= n {
                format!("{sign}{digits}{}.0", "0".repeat((decpt - n) as usize))
            } else {
                format!(
                    "{sign}{}.{}",
                    &digits[..decpt as usize],
                    &digits[decpt as usize..]
                )
            }
        } else {
            let (first, rest) = digits.split_at(1);
            let m = if rest.is_empty() {
                first.to_string()
            } else {
                format!("{first}.{rest}")
            };
            format!(
                "{sign}{m}e{}{:02}",
                if exp < 0 { "-" } else { "+" },
                exp.abs()
            )
        }
    }

    /// `str(value)`.
    pub fn to_str(value: &Value) -> String {
        match value {
            Value::String(s) => s.clone(),
            other => repr(other),
        }
    }

    /// `repr(value)`.
    pub fn repr(value: &Value) -> String {
        match value {
            Value::Null => "None".into(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    i.to_string()
                } else if let Some(u) = n.as_u64() {
                    u.to_string()
                } else {
                    float_repr(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(s) => crate::assets::py_repr(s),
            Value::Array(items) => {
                let inner: Vec<String> = items.iter().map(repr).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Object(map) => {
                let inner: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{}: {}", crate::assets::py_repr(k), repr(v)))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
        }
    }

    /// Python's rule for underscores in number text: only between digits.
    fn clean_underscores(text: &str) -> Option<String> {
        if !text.contains('_') {
            return Some(text.to_string());
        }
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        for (i, c) in chars.iter().enumerate() {
            if *c == '_' {
                let before = i.checked_sub(1).and_then(|j| chars.get(j));
                let after = chars.get(i + 1);
                if !(before.is_some_and(|b| b.is_ascii_digit())
                    && after.is_some_and(|a| a.is_ascii_digit()))
                {
                    return None;
                }
            } else {
                out.push(*c);
            }
        }
        Some(out)
    }

    /// `float("text")`.
    pub fn parse_float(text: &str) -> Option<f64> {
        let cleaned = clean_underscores(text.trim())?;
        if cleaned.is_empty() {
            return None;
        }
        cleaned.parse::<f64>().ok()
    }

    /// `float(value)`; the error is Python's message.
    pub fn to_float(value: &Value) -> Result<f64, String> {
        match value {
            Value::Number(n) => Ok(n.as_f64().unwrap_or(f64::NAN)),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::String(s) => parse_float(s).ok_or_else(|| {
                format!(
                    "could not convert string to float: {}",
                    crate::assets::py_repr(s)
                )
            }),
            other => Err(format!(
                "float() argument must be a string or a real number, not '{}'",
                type_name(other)
            )),
        }
    }

    /// `int(value)`; the error is Python's message.
    pub fn to_int(value: &Value) -> Result<i64, String> {
        let from_float = |f: f64| -> Result<i64, String> {
            if f.is_nan() {
                Err("cannot convert float NaN to integer".into())
            } else if f.is_infinite() {
                Err("cannot convert float infinity to integer".into())
            } else {
                Ok(f.trunc().clamp(-9.0e18, 9.0e18) as i64)
            }
        };
        match value {
            Value::Number(n) => match n.as_i64() {
                Some(i) => Ok(i),
                None => from_float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::Bool(b) => Ok(*b as i64),
            Value::String(s) => {
                let bad = || {
                    format!(
                        "invalid literal for int() with base 10: {}",
                        crate::assets::py_repr(s)
                    )
                };
                let cleaned = clean_underscores(s.trim()).ok_or_else(bad)?;
                let digits = cleaned.strip_prefix(['+', '-']).unwrap_or(&cleaned);
                if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                    return Err(bad());
                }
                Ok(cleaned
                    .parse::<i64>()
                    .unwrap_or(if cleaned.starts_with('-') {
                        i64::MIN
                    } else {
                        i64::MAX
                    }))
            }
            other => Err(format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                type_name(other)
            )),
        }
    }

    /// `value[index]` for a list, str or dict, with Python's errors.
    pub fn index(value: &Value, i: usize) -> Result<Value, String> {
        match value {
            Value::Array(items) => items
                .get(i)
                .cloned()
                .ok_or_else(|| "list index out of range".to_string()),
            Value::String(s) => s
                .chars()
                .nth(i)
                .map(|c| Value::String(c.to_string()))
                .ok_or_else(|| "string index out of range".to_string()),
            Value::Object(_) => Err(i.to_string()),
            other => Err(format!(
                "'{}' object is not subscriptable",
                type_name(other)
            )),
        }
    }

    /// Python's `str.split()` whitespace (Unicode white space plus the four separator controls).
    pub fn is_space(c: char) -> bool {
        c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
    }
}

use pyval::{repr as py_repr_value, to_float, to_str, truthy};

fn py_repr(text: &str) -> String {
    assets::py_repr(text)
}

/// `_int`: `round(float(op.get(key, default)))`, clamped to a range that cannot overflow.
fn int(op: &Value, key: &str, default: i64) -> Result<i64, DesignError> {
    let fallback;
    let value = match op.get(key) {
        Some(v) => v,
        None => {
            fallback = Value::from(default);
            &fallback
        }
    };
    let not_a_number = || {
        design(format!(
            "{key} must be a number, not {}.",
            py_repr_value(value)
        ))
    };
    let float = to_float(value).map_err(|_| not_a_number())?;
    if float.is_nan() {
        return Err(not_a_number());
    }
    if float.is_infinite() {
        return Err(design("cannot convert float infinity to integer"));
    }
    Ok(float.round_ties_even().clamp(-INT_LIMIT, INT_LIMIT) as i64)
}

/// A float option, `default` when missing.
fn float_opt(op: &Value, key: &str, default: f64) -> Result<f64, DesignError> {
    match op.get(key) {
        None => Ok(default),
        Some(value) => match to_float(value) {
            Ok(v) if v.is_finite() => Ok(v),
            _ => Err(design(format!(
                "{key} must be a number, not {}.",
                py_repr_value(value)
            ))),
        },
    }
}

/// Pillow's `ImageEnhance.Contrast` then `Brightness` on a grey picture.
fn enhance(gray: &GrayImage, contrast: f64, bright: f64) -> GrayImage {
    let count = (gray.width() as u64 * gray.height() as u64).max(1);
    let sum: u64 = gray.as_raw().iter().map(|&v| v as u64).sum();
    let mean = (sum as f64 / count as f64 + 0.5).floor();
    let mut out = gray.clone();
    out.pixels_mut().for_each(|p| {
        let v = mean + contrast * (p[0] as f64 - mean);
        p[0] = (v * bright).round().clamp(0.0, 255.0) as u8;
    });
    out
}

/// A coordinate for the raster functions.
fn c32(v: i64) -> i32 {
    v.clamp(-(1 << 30), 1 << 30) as i32
}

fn number(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// ================================================================================================
// Bitmaps
// ================================================================================================

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    )
}

/// The wire format: base64 of packed rows, most significant bit first, 1 = black.
pub fn image_to_bitmap(image: &GrayImage) -> Result<String, DesignError> {
    if image.dimensions() != (WIDTH, HEIGHT) {
        return Err(design(format!(
            "A note is exactly {WIDTH}x{HEIGHT}, not {}x{}.",
            image.width(),
            image.height()
        )));
    }
    let row_bytes = (WIDTH as usize).div_ceil(8);
    let mut packed = vec![0u8; BITMAP_BYTES];
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0[0] < 128 {
            packed[y as usize * row_bytes + x as usize / 8] |= 0x80 >> (x % 8);
        }
    }
    Ok(b64().encode(packed))
}

pub fn bitmap_to_image(bitmap: &str) -> Result<GrayImage, DesignError> {
    let packed = b64()
        .decode(bitmap)
        .map_err(|_| design("That isn't a valid note bitmap."))?;
    if packed.len() != BITMAP_BYTES {
        return Err(design(format!(
            "A note bitmap is {BITMAP_BYTES} bytes, not {}.",
            packed.len()
        )));
    }
    Ok(unpack(&packed, WIDTH as usize, HEIGHT as usize))
}

/// Packed rows (1 = black) as a mask-free picture: black ink on white paper.
fn unpack(packed: &[u8], width: usize, height: usize) -> GrayImage {
    let row_bytes = width.div_ceil(8);
    let mut image = raster::paper(width as u32, height as u32);
    for y in 0..height {
        for x in 0..width {
            if packed[y * row_bytes + x / 8] & (0x80 >> (x % 8)) != 0 {
                image.put_pixel(x as u32, y as u32, image::Luma([0]));
            }
        }
    }
    image
}

// ================================================================================================
// Canvas
// ================================================================================================

pub struct Canvas {
    pub image: GrayImage,
    pub ops: Vec<Value>,
    pub report: Option<poster::Report>,
    /// Who a poster may say the note is from: set by the connector, never by a design.
    pub sender: String,
    /// False stops a design reading files or the network, for untrusted callers.
    pub external: bool,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas::new(true, "")
    }
}

impl Canvas {
    pub fn new(external: bool, sender: &str) -> Canvas {
        Canvas {
            image: raster::paper(WIDTH, HEIGHT),
            ops: Vec::new(),
            report: None,
            sender: sender.to_string(),
            external,
        }
    }

    // --- document -------------------------------------------------------------------------

    /// Validates the op, runs it and records it. A failing op changes nothing.
    pub fn apply(&mut self, op: Value) -> Result<(), DesignError> {
        if !op.as_object().is_some_and(|m| m.contains_key("op")) {
            return Err(design("Each operation needs an \"op\" name."));
        }
        if self.ops.len() >= MAX_OPS {
            return Err(design(format!("A design can have {MAX_OPS} operations.")));
        }
        let backup = self.image.clone();
        match self.run(&op) {
            Ok(()) => {
                self.ops.push(op);
                Ok(())
            }
            Err(Fail::Design(e)) => {
                self.image = backup;
                Err(e)
            }
            Err(Fail::Py(message)) => {
                self.image = backup;
                Err(design(message))
            }
        }
    }

    pub fn apply_all(&mut self, ops: &[Value]) -> Result<(), DesignError> {
        for op in ops {
            self.apply(op.clone())?;
        }
        Ok(())
    }

    /// Drops the last `count` ops (at least one) and replays the rest. If a replay fails (a file went away),
    /// the canvas keeps what drew; `try_undo` reports that.
    pub fn undo(&mut self, count: usize) {
        let _ = self.try_undo(count);
    }

    pub fn try_undo(&mut self, count: usize) -> Result<(), DesignError> {
        let keep_len = self.ops.len().saturating_sub(count.max(1));
        let keep: Vec<Value> = self.ops[..keep_len].to_vec();
        self.clear();
        self.apply_all(&keep)
    }

    pub fn clear(&mut self) {
        self.image = raster::paper(WIDTH, HEIGHT);
        self.ops.clear();
    }

    pub fn blank(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn bitmap(&self) -> Result<String, DesignError> {
        image_to_bitmap(&self.image)
    }

    /// `_run`: finds the handler, runs it, and words the failures. The result is only `Fail::Py` for a missing
    /// "op" key, which Python raises as a bare KeyError (the enclosing repeat or scatter words it).
    fn run(&mut self, op: &Value) -> Result<(), Fail> {
        let name = match op.get("op") {
            Some(name) => name,
            None => return Err(Fail::Py("'op'".into())),
        };
        let key = to_str(name).replace('-', "_");
        let outcome = match key.as_str() {
            "text" => self.op_text(op),
            "poster" => self.op_poster(op),
            "figure" => self.op_figure(op),
            "pattern" => self.op_pattern(op),
            "frame" => self.op_frame(op),
            "shape" => self.op_shape(op),
            "image" => self.op_image(op),
            "svg" => self.op_svg(op),
            "logo" => self.op_logo(op),
            "pixels" => self.op_pixels(op),
            "formula" => self.op_formula(op),
            "qr" => self.op_qr(op),
            "transform" => self.op_transform(op),
            "repeat" => self.op_repeat(op),
            "scatter" => self.op_scatter(op),
            "clear" => self.op_clear(op),
            _ => {
                return Err(Fail::Design(design(format!(
                    "Unknown op {}. Ops: {}.",
                    py_repr_value(name),
                    OPS.join(", ")
                ))))
            }
        };
        match outcome {
            Err(Fail::Py(message)) => Err(Fail::Design(design(format!(
                "Bad {} op: {message}",
                to_str(name)
            )))),
            other => other,
        }
    }

    // --- painting -------------------------------------------------------------------------

    /// `_box`: x, y, w, h, with the whole rest of the grid as the default size.
    fn box_of(&self, op: &Value) -> Result<(i64, i64, i64, i64), DesignError> {
        let x = int(op, "x", 0)?;
        let y = int(op, "y", 0)?;
        let width = int(op, "w", WIDTH as i64 - x)?;
        let height = int(op, "h", HEIGHT as i64 - y)?;
        if width <= 0 || height <= 0 {
            return Err(design(format!("Box has no area: w={width}, h={height}.")));
        }
        if width > 4096 || height > 4096 {
            return Err(design("Box is too big."));
        }
        Ok((x, y, width, height))
    }

    /// `_paint`: the mask is canvas-sized. Ink is black, white, invert, or any pattern.
    fn paint(&mut self, mask: &GrayImage, ink: Option<&Value>) -> Result<(), Fail> {
        let ink = match ink {
            None | Some(Value::Null) => "black".to_string(),
            Some(Value::String(s)) if s.is_empty() => "black".to_string(),
            Some(other) => to_str(other),
        };
        match ink.as_str() {
            "black" | "k" => raster::paste_value(&mut self.image, 0, 0, 0, mask),
            "white" | "w" => raster::paste_value(&mut self.image, 255, 0, 0, mask),
            "invert" | "x" => {
                let inverted = raster::invert(&self.image);
                raster::paste(&mut self.image, &inverted, 0, 0, Some(mask));
            }
            _ if patterns::has_pattern(&ink) => {
                let tone = patterns::try_pattern_image(&ink, WIDTH, HEIGHT).map_err(Fail::Py)?;
                let both = raster::logical_and(mask, &tone);
                raster::paste_value(&mut self.image, 0, 0, 0, &both);
            }
            _ => {
                return Err(Fail::Design(design(format!(
                    "Unknown ink {}. Use black, white, invert, or a pattern name.",
                    py_repr(&ink)
                ))))
            }
        }
        Ok(())
    }

    /// `_place`: puts a small mask on the canvas, rotated about its own center if asked.
    fn place(&mut self, part: &GrayImage, x: i64, y: i64, op: &Value) -> Result<(), Fail> {
        let mut mask = raster::blank(WIDTH, HEIGHT);
        raster::paste_value(&mut mask, 255, c32(x), c32(y), part);
        if let Some(rotation) = op.get("rot") {
            if truthy(rotation) {
                let degrees = to_float(rotation).map_err(Fail::Py)?;
                let center = (
                    x as f64 + part.width() as f64 / 2.0,
                    y as f64 + part.height() as f64 / 2.0,
                );
                mask = raster::rotate(&mask, -degrees, Some(center), 0, Resample::Nearest);
            }
        }
        self.paint(&mask, op.get("ink"))
    }

    // --- operations -----------------------------------------------------------------------

    fn op_text(&mut self, op: &Value) -> Result<(), Fail> {
        let text = op.get("s").map(to_str).unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        let (x, y, width, height) = self.box_of(op)?;
        let font = op
            .get("font")
            .map(to_str)
            .unwrap_or_else(|| "sans-bold".into());
        let size = int(op, "size", 0)?;
        let align = op.get("align").map(to_str).unwrap_or_else(|| "c".into());
        let valign = op.get("valign").map(to_str).unwrap_or_else(|| "m".into());
        let wrap = op.get("wrap").map(truthy).unwrap_or(true);
        let spacing = match op.get("spacing") {
            Some(v) => to_float(v).map_err(Fail::Py)?,
            None => 1.15,
        };
        if spacing.is_nan() {
            return Err(Fail::Py("cannot convert float NaN to integer".into()));
        }
        if spacing.is_infinite() {
            return Err(Fail::Design(design(
                "cannot convert float infinity to integer",
            )));
        }
        let part = render_text(
            &text,
            width as u32,
            height as u32,
            &font,
            size.clamp(-1, 4096) as i32,
            &align,
            &valign,
            wrap,
            spacing,
            self.external,
        )?;
        self.place(&part, x, y, op)
    }

    /// Replaces the whole note. Other ops can draw on top afterwards.
    fn op_poster(&mut self, op: &Value) -> Result<(), Fail> {
        let (image, report) = poster::render(op, &self.sender).map_err(Fail::Py)?;
        self.image = image;
        self.report = Some(report);
        Ok(())
    }

    fn op_figure(&mut self, op: &Value) -> Result<(), Fail> {
        let name = op.get("name").map(to_str).unwrap_or_default();
        let (x, y, width, height) = self.box_of(op)?;
        if let Some(emoji) = name.strip_prefix("emoji:") {
            let image =
                assets::emoji_image(emoji, width as u32, height as u32).ok_or_else(|| {
                    design("This machine has no emoji font. Use a figure or draw_image.")
                })?;
            let method = op
                .get("dither")
                .map(to_str)
                .unwrap_or_else(|| "atkinson".into());
            let mask = assets::dither_to_mask(&image, &method, 128, false, true)?;
            return self.place(&mask, x, y, op);
        }
        if !figures::has_figure(&name) {
            return Err(Fail::Design(design(format!(
                "Unknown figure {}. See catalog figures, or use emoji:\u{1f382}.",
                py_repr(&name)
            ))));
        }
        let mask = figures::figure_mask(&name, width as u32, height as u32);
        self.place(&mask, x, y, op)
    }

    fn op_pattern(&mut self, op: &Value) -> Result<(), Fail> {
        let name = op.get("name").map(to_str).unwrap_or_default();
        if !patterns::has_pattern(&name) {
            return Err(Fail::Design(design(format!(
                "Unknown pattern {}. See catalog patterns.",
                py_repr(&name)
            ))));
        }
        let (x, y, width, height) = self.box_of(op)?;
        let (w, h) = (width as u32, height as u32);
        let mut part = raster::blank(w, h);
        let ellipse = matches!(op.get("shape"), Some(Value::String(s)) if s == "ellipse");
        let corners = [0i64, 0, width - 1, height - 1];
        if ellipse {
            draw::ellipse(&mut part, corners, Some(255), None, 0);
        } else {
            draw::rectangle(&mut part, corners, Some(255), None, 0);
        }
        let tone = patterns::try_pattern_image(&name, w, h).map_err(Fail::Py)?;
        let shaped = raster::logical_and(&part, &tone);
        self.place(&shaped, x, y, op)
    }

    fn op_frame(&mut self, op: &Value) -> Result<(), Fail> {
        let name = op.get("name").map(to_str).unwrap_or_else(|| "line".into());
        if !frames::has_frame(&name) {
            return Err(Fail::Design(design(format!(
                "Unknown frame {}. See catalog frames. Figure names also work.",
                py_repr(&name)
            ))));
        }
        let inset = int(op, "inset", 0)?;
        let (x, y, width, height) = self.box_of(op)?;
        let boxed = (
            c32(x + inset),
            c32(y + inset),
            c32(x + width - 1 - inset),
            c32(y + height - 1 - inset),
        );
        // Python would draw a frame of any size; beyond this the stamps would be absurd.
        let size = int(op, "size", 3)?.clamp(1, 1024) as i32;
        // An inset that turns the box inside out: Pillow refuses in the frames that draw rectangles.
        let (x0, y0, x1, y1) = (
            x + inset,
            y + inset,
            x + width - 1 - inset,
            y + height - 1 - inset,
        );
        let checks_x = matches!(
            name.as_str(),
            "double"
                | "film"
                | "line"
                | "ornate"
                | "rounded"
                | "shadow"
                | "stamp"
                | "thick"
                | "ticket"
        );
        let checks_y = checks_x && name != "film";
        if checks_x && x1 < x0 {
            return Err(Fail::Py("x1 must be greater than or equal to x0".into()));
        }
        if checks_y && y1 < y0 {
            return Err(Fail::Py("y1 must be greater than or equal to y0".into()));
        }
        let mask = frames::frame_mask(&name, (WIDTH, HEIGHT), boxed, size);
        self.paint(&mask, op.get("ink"))
    }

    fn op_shape(&mut self, op: &Value) -> Result<(), Fail> {
        let kind = op.get("kind").map(to_str).unwrap_or_else(|| "rect".into());
        let mut mask = raster::blank(WIDTH, HEIGHT);
        let line_width = int(op, "lw", 1)?.clamp(1, 1 << 20) as u32;
        let filled = op.get("fill").map(truthy).unwrap_or(true);
        let fill = if filled { Some(255u8) } else { None };
        let outline = if filled { None } else { Some(255u8) };

        if matches!(kind.as_str(), "line" | "polygon" | "curve") {
            let mut points = point_pairs(op.get("pts"))?;
            if kind == "curve" {
                points = smooth(&points, 12);
            }
            // Nothing visible lives beyond this, and it keeps the integer maths in `draw` clear of overflow.
            for point in points.iter_mut() {
                *point = (point.0.clamp(-1.0e9, 1.0e9), point.1.clamp(-1.0e9, 1.0e9));
            }
            if kind == "polygon" {
                draw::polygon(&mut mask, &points, fill, outline, line_width);
            } else {
                draw::line(&mut mask, &points, 255, line_width, true);
            }
        } else {
            let (x, y, width, height) = self.box_of(op)?;
            let boxed = [c32(x), c32(y), c32(x + width - 1), c32(y + height - 1)];
            let start = match op.get("a0") {
                Some(v) => to_float(v).map_err(Fail::Py)?,
                None => 0.0,
            };
            let end = match op.get("a1") {
                Some(v) => to_float(v).map_err(Fail::Py)?,
                None => 360.0,
            };
            match kind.as_str() {
                "rect" => draw::rectangle(&mut mask, boxed, fill, outline, line_width),
                "rrect" => {
                    let radius = int(op, "r", 8)? as f64;
                    draw::rounded_rectangle(&mut mask, boxed, radius, fill, outline, line_width);
                }
                "ellipse" => draw::ellipse(&mut mask, boxed, fill, outline, line_width),
                "arc" => draw::arc(&mut mask, boxed, start, end, 255, line_width),
                "pie" => draw::pieslice(&mut mask, boxed, start, end, fill, outline, line_width),
                "chord" => draw::chord(&mut mask, boxed, start, end, fill, outline, line_width),
                _ => {
                    return Err(Fail::Design(design(format!(
                        "Unknown shape {}. Use rect, rrect, ellipse, line, curve, polygon, arc, pie or chord.",
                        py_repr(&kind)
                    ))))
                }
            }
        }
        self.paint(&mask, op.get("ink"))
    }

    fn op_image(&mut self, op: &Value) -> Result<(), Fail> {
        let src = op.get("src").filter(|v| truthy(v));
        let Some(src) = src else {
            return Err(Fail::Design(design(
                "image needs a src: URL, file path, or base64.",
            )));
        };
        let data = assets::read_source(&to_str(src), self.external)?;
        self.draw_raster(&data, op, "atkinson", "contain")
    }

    fn op_svg(&mut self, op: &Value) -> Result<(), Fail> {
        let data: Vec<u8> = if let Some(path) = op.get("d").filter(|v| truthy(v)) {
            let default_box = Value::String("0 0 24 24".into());
            let viewbox = clean_viewbox(op.get("vb").unwrap_or(&default_box))?;
            let rule = if op.get("evenodd").is_some_and(truthy) {
                " fill-rule=\"evenodd\""
            } else {
                ""
            };
            let path = clean_path(&to_str(path))?;
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{viewbox}\"><path{rule} d=\"{path}\"/></svg>"
            )
            .into_bytes()
        } else if let Some(markup) = op.get("svg").filter(|v| truthy(v)) {
            to_str(markup).into_bytes()
        } else if let Some(src) = op.get("src").filter(|v| truthy(v)) {
            assets::read_source(&to_str(src), self.external)?
        } else {
            return Err(Fail::Design(design(
                "svg needs d (path data), svg (markup), or src (URL or path).",
            )));
        };
        self.draw_raster(&data, op, "threshold", "contain")
    }

    fn op_logo(&mut self, op: &Value) -> Result<(), Fail> {
        let name = op.get("name").filter(|v| truthy(v));
        let Some(name) = name else {
            return Err(Fail::Design(design("logo needs a name, such as spotify.")));
        };
        let data = assets::logo_svg(&to_str(name))?;
        self.draw_raster(&data, op, "threshold", "contain")
    }

    fn draw_raster(
        &mut self,
        data: &[u8],
        op: &Value,
        default_dither: &str,
        fit: &str,
    ) -> Result<(), Fail> {
        let (x, y, width, height) = self.box_of(op)?;
        let (w, h) = (width as u32, height as u32);
        let image = assets::load_image(data, w, h)?;
        let fit = op.get("fit").map(to_str).unwrap_or_else(|| fit.to_string());
        let fitted = assets::fit_image(&image, w, h, assets::Fit::from_str(&fit));
        // Thermal dots spread into their neighbours, so photos dither best at a coarser grid.
        let dot = int(op, "dot", 1)?.clamp(1, 8) as u32;
        let contrast = float_opt(op, "contrast", 1.0)?;
        let bright = float_opt(op, "bright", 1.0)?;
        let fitted = if dot > 1 {
            raster::resize(&fitted, w.div_ceil(dot), h.div_ceil(dot), Resample::Lanczos)
        } else {
            fitted
        };
        let fitted = if contrast != 1.0 || bright != 1.0 {
            enhance(&fitted, contrast, bright)
        } else {
            fitted
        };
        let method = op
            .get("dither")
            .map(to_str)
            .unwrap_or_else(|| default_dither.to_string());
        let threshold = int(op, "threshold", 128)?;
        let invert = op.get("invert").is_some_and(truthy);
        let mask = if method == "threshold" && !(1..=255).contains(&threshold) {
            // `v < threshold` for any integer: nothing (<= 0) or everything (>= 256) once inverted away.
            let mut gray = fitted.clone();
            if invert {
                gray = raster::invert(&gray);
            }
            let value = if threshold > 255 { 255 } else { 0 };
            raster::filled(gray.width(), gray.height(), value)
        } else {
            assets::dither_to_mask(
                &fitted,
                &method,
                threshold.clamp(0, 255) as u8,
                invert,
                true,
            )?
        };
        let mask = if dot > 1 {
            let big = raster::resize(
                &mask,
                mask.width() * dot,
                mask.height() * dot,
                Resample::Nearest,
            );
            image::imageops::crop_imm(&big, 0, 0, w, h).to_image()
        } else {
            mask
        };
        self.place(&mask, x, y, op)
    }

    fn op_pixels(&mut self, op: &Value) -> Result<(), Fail> {
        let x = int(op, "x", 0)?;
        let y = int(op, "y", 0)?;
        let scale = int(op, "scale", 1)?.max(1);
        let mut part;
        if let Some(rows) = op.get("rows") {
            let rows = match rows {
                Value::Array(rows) if !rows.is_empty() => rows,
                _ => return Err(Fail::Design(design("rows must be a list of strings."))),
            };
            let texts: Vec<Vec<char>> = rows
                .iter()
                .map(|row| to_str(row).chars().collect())
                .collect();
            let width = texts.iter().map(Vec::len).max().unwrap_or(0);
            part = raster::blank(width as u32, texts.len() as u32);
            for (j, row) in texts.iter().enumerate() {
                for (i, c) in row.iter().enumerate() {
                    if !matches!(c, '.' | ' ' | '0' | '_') {
                        part.put_pixel(i as u32, j as u32, image::Luma([255]));
                    }
                }
            }
        } else if let Some(b64_value) = op.get("b64") {
            let width = int(op, "w", WIDTH as i64)?;
            let data = assets::decode_base64(&to_str(b64_value))?;
            let row_bytes = (width + 7).div_euclid(8);
            let height = int(op, "h", data.len() as i64 / row_bytes.max(1))?;
            if (data.len() as i64) < row_bytes * height || height <= 0 {
                return Err(Fail::Design(design(format!(
                    "b64 has {} bytes, but {width}x{height} needs {}.",
                    data.len(),
                    row_bytes * height
                ))));
            }
            if width < 0 {
                return Err(Fail::Py("Width and height must be >= 0".into()));
            }
            part = if width == 0 {
                GrayImage::new(0, 0)
            } else {
                unpack(
                    &data[..(row_bytes * height) as usize],
                    width as usize,
                    height as usize,
                )
            };
            // `unpack` gives black ink on white paper; the part is a mask.
            part = raster::invert(&part);
        } else {
            return Err(Fail::Design(design(
                "pixels needs rows (strings where . is off) or b64 (packed bits, 1 = black).",
            )));
        }
        if scale > 1 {
            let w = (part.width() as u64)
                .saturating_mul(scale as u64)
                .min(u32::MAX as u64) as u32;
            let h = (part.height() as u64)
                .saturating_mul(scale as u64)
                .min(u32::MAX as u64) as u32;
            part = raster::resize(&part, w, h, Resample::Nearest);
        }
        self.place(&part, x, y, op)
    }

    fn op_formula(&mut self, op: &Value) -> Result<(), Fail> {
        let source = op.get("f").map(to_str).unwrap_or_default();
        let test = formula::compile_formula(&source)?;
        let (x, y, width, height) = self.box_of(op)?;
        let (w, h) = (width as i32, height as i32);
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for j in 0..h {
            for i in 0..w {
                pixels.push(if test.test(i, j, w, h) { 255 } else { 0 });
            }
        }
        let part =
            GrayImage::from_raw(w as u32, h as u32, pixels).unwrap_or_else(|| GrayImage::new(0, 0));
        self.place(&part, x, y, op)
    }

    fn op_qr(&mut self, op: &Value) -> Result<(), Fail> {
        let data = op.get("s").map(to_str).unwrap_or_default();
        if data.is_empty() {
            return Err(Fail::Design(design(
                "qr needs s, the text or URL to encode.",
            )));
        }
        let (x, y, width, height) = self.box_of(op)?;
        let ecc = op.get("ecc").map(to_str).unwrap_or_else(|| "m".into());
        let matrix = qr_matrix(&data, &ecc).map_err(Fail::Py)?;
        let side = matrix.len() as i64;
        let scale = width.min(height) / side;
        if scale < 1 {
            return Err(Fail::Design(design(format!(
                "This QR needs at least {side}x{side} pixels. Give it a bigger box or less text."
            ))));
        }
        let mut small = raster::blank(side as u32, side as u32);
        for (j, row) in matrix.iter().enumerate() {
            for (i, cell) in row.iter().enumerate() {
                if *cell {
                    small.put_pixel(i as u32, j as u32, image::Luma([255]));
                }
            }
        }
        let size = (side * scale) as u32;
        let part = raster::resize(&small, size, size, Resample::Nearest);
        let px = x + (width - part.width() as i64).div_euclid(2);
        let py = y + (height - part.height() as i64).div_euclid(2);
        self.place(&part, px, py, op)
    }

    fn op_transform(&mut self, op: &Value) -> Result<(), Fail> {
        let kind = op
            .get("kind")
            .map(to_str)
            .unwrap_or_else(|| "invert".into());
        let (x, y, width, height) = self.box_of(op)?;
        let (x0, y0) = (c32(x), c32(y));
        let mut region = raster::crop(&self.image, x0, y0, c32(x + width), c32(y + height));
        let (w, h) = (width as u32, height as u32);
        match kind.as_str() {
            "invert" => region = raster::invert(&region),
            "flip-h" => region = raster::flip_h(&region),
            "flip-v" => region = raster::flip_v(&region),
            "rotate" => {
                let degrees = match op.get("deg") {
                    Some(v) => to_float(v).map_err(Fail::Py)?,
                    None => 180.0,
                };
                region = raster::rotate(&region, -degrees, None, 255, Resample::Nearest);
            }
            "mirror-h" => {
                // The left half reflected onto the right.
                let half = raster::flip_h(&raster::crop(&region, 0, 0, (w / 2) as i32, h as i32));
                raster::paste(&mut region, &half, (w - half.width()) as i32, 0, None);
            }
            "mirror-v" => {
                let half = raster::flip_v(&raster::crop(&region, 0, 0, w as i32, (h / 2) as i32));
                raster::paste(&mut region, &half, 0, (h - half.height()) as i32, None);
            }
            "shift" => {
                let dx = int(op, "dx", 0)?;
                let dy = int(op, "dy", 0)?;
                region = raster::offset(&region, c32(dx), c32(dy));
            }
            "pixelate" => {
                let block = int(op, "size", 4)?.max(2) as u32;
                let small = raster::resize(&region, (w / block).max(1), (h / block).max(1), Resample::Box);
                let big = raster::resize(&small, w, h, Resample::Nearest);
                region = raster::threshold(&big, 128);
            }
            "outline" => {
                let eroded = raster::max_filter(&region, 3);
                let edge = raster::invert(&raster::subtract(&eroded, &region));
                region = raster::threshold(&edge, 128);
            }
            _ => {
                return Err(Fail::Design(design(format!(
                    "Unknown transform {}. Use invert, flip-h, flip-v, rotate, mirror-h, mirror-v, shift, pixelate or outline.",
                    py_repr(&kind)
                ))))
            }
        }
        raster::paste(&mut self.image, &region, x0, y0, None);
        Ok(())
    }

    /// Runs `ops` n times, moving by dx, dy each time.
    fn op_repeat(&mut self, op: &Value) -> Result<(), Fail> {
        let count = int(op, "n", 2)?.min(MAX_REPEAT);
        let dx = int(op, "dx", 0)?;
        let dy = int(op, "dy", 0)?;
        for index in 0..count.max(0) {
            for inner in iterate(op.get("ops")).map_err(Fail::Py)? {
                let moved = moved(&inner, dx * index, dy * index)?;
                self.run(&moved)?;
            }
        }
        Ok(())
    }

    /// Runs `ops` n times at random spots. The same seed gives the same result.
    fn op_scatter(&mut self, op: &Value) -> Result<(), Fail> {
        let seed = op
            .get("seed")
            .map(to_float)
            .unwrap_or(Ok(1.0))
            .map_err(|_| design_not_a_number(op, "seed"))?;
        let seed = round_seed(seed, op)?;
        let mut rng = PyRandom::from_u128(seed);
        let (x, y, width, height) = self.box_of(op)?;
        for _ in 0..int(op, "n", 20)?.clamp(0, MAX_REPEAT) {
            let px = x + rng.randrange(width).map_err(|e| Fail::Py(e.to_string()))?;
            let py = y + rng.randrange(height).map_err(|e| Fail::Py(e.to_string()))?;
            for inner in iterate(op.get("ops")).map_err(Fail::Py)? {
                let moved = moved(&inner, px, py)?;
                self.run(&moved)?;
            }
        }
        Ok(())
    }

    fn op_clear(&mut self, op: &Value) -> Result<(), Fail> {
        let white = Value::String("white".into());
        let full = raster::paper(WIDTH, HEIGHT);
        self.paint(&full, Some(op.get("ink").unwrap_or(&white)))
    }
}

fn design_not_a_number(op: &Value, key: &str) -> DesignError {
    let shown = op.get(key).map(py_repr_value).unwrap_or_default();
    design(format!("{key} must be a number, not {shown}."))
}

/// `round(float(seed))` as the absolute value Python's seeding uses.
fn round_seed(seed: f64, op: &Value) -> Result<u128, DesignError> {
    if seed.is_nan() {
        return Err(design_not_a_number(op, "seed"));
    }
    if seed.is_infinite() {
        return Err(design("cannot convert float infinity to integer"));
    }
    Ok(seed.round_ties_even().abs() as u128)
}

/// `for inner in op.get("ops", [])`: what Python would iterate over.
fn iterate(value: Option<&Value>) -> Result<Vec<Value>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::Object(map)) => Ok(map.keys().map(|k| Value::String(k.clone())).collect()),
        Some(Value::String(s)) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Some(other) => Err(format!(
            "'{}' object is not iterable",
            pyval::type_name(other)
        )),
    }
}

/// `_moved`: a copy of the op shifted by dx, dy (points included).
fn moved(op: &Value, dx: i64, dy: i64) -> Result<Value, Fail> {
    let Value::Object(source) = op else {
        // `dict(op)` first: what it says about things that are not mappings.
        let length_error = |n: usize| {
            format!("dictionary update sequence element #0 has length {n}; 2 is required")
        };
        return Err(Fail::Py(match op {
            Value::Null | Value::Bool(_) | Value::Number(_) => {
                format!("'{}' object is not iterable", pyval::type_name(op))
            }
            Value::String(s) if !s.is_empty() => length_error(1),
            Value::Array(items) => match items.first() {
                Some(Value::Null | Value::Bool(_) | Value::Number(_)) => {
                    "cannot convert dictionary update sequence element #0 to a sequence".to_string()
                }
                Some(Value::Array(pair)) if pair.len() != 2 => length_error(pair.len()),
                Some(Value::String(s)) if s.chars().count() != 2 => length_error(s.chars().count()),
                Some(Value::Object(map)) if map.len() != 2 => length_error(map.len()),
                _ => format!("'{}' object has no attribute 'get'", pyval::type_name(op)),
            },
            other => format!(
                "'{}' object has no attribute 'get'",
                pyval::type_name(other)
            ),
        }));
    };
    let mut copy: Map<String, Value> = source.clone();
    copy.insert("x".into(), Value::from(int(op, "x", 0)? + dx));
    copy.insert("y".into(), Value::from(int(op, "y", 0)? + dy));
    if let Some(pts @ Value::Array(_)) = op.get("pts") {
        let shifted: Vec<Value> = point_pairs(Some(pts))?
            .into_iter()
            .map(|(px, py)| Value::Array(vec![number(px + dx as f64), number(py + dy as f64)]))
            .collect();
        copy.insert("pts".into(), Value::Array(shifted));
    }
    Ok(Value::Object(copy))
}

/// `_point_pairs`: `[[x,y],[x,y]]` or `[x,y,x,y]` as floats.
fn point_pairs(raw: Option<&Value>) -> Result<Vec<(f64, f64)>, Fail> {
    let items = match raw {
        Some(Value::Array(items)) if items.len() >= 2 => items,
        _ => {
            return Err(Fail::Design(design(
                "pts needs at least two points: [[x,y],[x,y]] or [x,y,x,y].",
            )))
        }
    };
    let float = |v: &Value| to_float(v).map_err(Fail::Py);
    let mut out = Vec::new();
    if matches!(items[0], Value::Number(_) | Value::Bool(_)) {
        let mut i = 0;
        while i + 1 < items.len() {
            out.push((float(&items[i])?, float(&items[i + 1])?));
            i += 2;
        }
    } else {
        for p in items {
            let first = pyval::index(p, 0).map_err(Fail::Py)?;
            let second = pyval::index(p, 1).map_err(Fail::Py)?;
            out.push((float(&first)?, float(&second)?));
        }
    }
    Ok(out)
}

/// `_smooth`: a Catmull-Rom curve through the points.
fn smooth(points: &[(f64, f64)], steps: usize) -> Vec<(f64, f64)> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut padded = Vec::with_capacity(points.len() + 2);
    padded.push(points[0]);
    padded.extend_from_slice(points);
    padded.push(points[points.len() - 1]);
    let mut out = Vec::new();
    for i in 1..padded.len() - 2 {
        let (p0, p1, p2, p3) = (padded[i - 1], padded[i], padded[i + 1], padded[i + 2]);
        for step in 0..steps {
            let t = step as f64 / steps as f64;
            let at = |a: f64, b: f64, c: f64, d: f64| {
                0.5 * (2.0 * b
                    + (c - a) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t.powf(2.0)
                    + (3.0 * b - a - 3.0 * c + d) * t.powf(3.0))
            };
            out.push((at(p0.0, p1.0, p2.0, p3.0), at(p0.1, p1.1, p2.1, p3.1)));
        }
    }
    out.push(points[points.len() - 1]);
    out
}

/// `_clean_viewbox`
fn clean_viewbox(value: &Value) -> Result<String, DesignError> {
    let text = match value {
        Value::Array(items) => items.iter().map(to_str).collect::<Vec<_>>().join(" "),
        other => to_str(other),
    };
    if !text.chars().all(|c| "0123456789.,- ".contains(c)) {
        return Err(design("vb must be four numbers."));
    }
    Ok(text)
}

/// `_clean_path`: path data has no reason to contain quotes or tags. This keeps it inside the attribute.
fn clean_path(value: &str) -> Result<String, DesignError> {
    if value.chars().any(|c| "<>\"'&".contains(c)) {
        return Err(design("d must be plain SVG path data."));
    }
    Ok(value.to_string())
}

/// Every op name, sorted.
pub const OPS: [&str; 16] = [
    "clear",
    "figure",
    "formula",
    "frame",
    "image",
    "logo",
    "pattern",
    "pixels",
    "poster",
    "qr",
    "repeat",
    "scatter",
    "shape",
    "svg",
    "text",
    "transform",
];

pub fn ops() -> Vec<&'static str> {
    OPS.to_vec()
}

// ================================================================================================
// QR codes
// ================================================================================================

/// `segno.make(data, error=ecc, boost_error=False).matrix`: the smallest version (Micro QR first, as segno
/// does), one segment in the simplest mode that holds the data, and the best mask. Errors are Python's messages.
///
/// Text outside Latin-1 is encoded as UTF-8; segno tries Shift JIS first, which is not ported.
fn qr_matrix(data: &str, ecc: &str) -> Result<Vec<Vec<bool>>, String> {
    use qrcode::bits::Bits;
    use qrcode::{EcLevel, Version};

    let level = match ecc.to_uppercase().as_str() {
        "L" => EcLevel::L,
        "M" => EcLevel::M,
        "Q" => EcLevel::Q,
        "H" => EcLevel::H,
        _ => {
            return Err(format!(
                "Illegal error correction level: \"{ecc}\". Supported levels: L, M, Q, H"
            ))
        }
    };
    let bytes: Vec<u8> = if data.chars().all(|c| (c as u32) < 256) {
        data.chars().map(|c| c as u8).collect()
    } else {
        data.as_bytes().to_vec()
    };
    #[derive(Clone, Copy)]
    enum Mode {
        Numeric,
        Alphanumeric,
        Kanji,
        Byte,
    }
    let alphanumeric =
        |b: &u8| b.is_ascii_digit() || b.is_ascii_uppercase() || b" $%*+-./:".contains(b);
    let kanji = bytes.len().is_multiple_of(2)
        && !bytes.is_empty()
        && bytes.chunks(2).all(|pair| {
            let code = (pair[0] as u32) << 8 | pair[1] as u32;
            (0x8140..=0x9ffc).contains(&code) || (0xe040..=0xebbf).contains(&code)
        });
    let mode = if bytes.iter().all(u8::is_ascii_digit) {
        Mode::Numeric
    } else if bytes.iter().all(alphanumeric) {
        Mode::Alphanumeric
    } else if kanji {
        Mode::Kanji
    } else {
        Mode::Byte
    };
    let versions = (2..=4)
        .map(Version::Micro)
        .chain((1..=40).map(Version::Normal));
    for version in versions {
        let mut bits = Bits::new(version);
        let pushed = match mode {
            Mode::Numeric => bits.push_numeric_data(&bytes),
            Mode::Alphanumeric => bits.push_alphanumeric_data(&bytes),
            Mode::Kanji => bits.push_kanji_data(&bytes),
            Mode::Byte => bits.push_byte_data(&bytes),
        };
        let fits = match bits.max_len(level) {
            Ok(capacity) => bits.len() <= capacity,
            Err(_) => false,
        };
        if pushed.is_err() || !fits {
            continue;
        }
        return finish_qr(bits, version, level);
    }
    Err("Data too large. No (Micro) QR Code can handle the provided data".into())
}

/// The data codewords as segno lays them out. Its padding step adds a whole zero byte when the terminator
/// ends exactly on a codeword boundary (`8 - length % 8` is 8, not 0), so the pad codewords start one byte
/// later than the standard's. Readers stop at the terminator, so both scan; the bytes differ.
fn segno_data_codewords(
    bits: qrcode::bits::Bits,
    version: qrcode::Version,
    level: qrcode::EcLevel,
) -> Result<Vec<u8>, String> {
    let capacity = bits.max_len(level).map_err(|e| e.to_string())?;
    let length = bits.len();
    let terminator = match version {
        qrcode::Version::Micro(n) => n as usize * 2 + 1,
        qrcode::Version::Normal(_) => 4,
    };
    let half_codeword = matches!(
        version,
        qrcode::Version::Micro(1) | qrcode::Version::Micro(3)
    );
    let bytes = bits.into_bytes();
    let mut stream: Vec<bool> = (0..length)
        .map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
        .collect();
    stream.resize(
        length + terminator.min(capacity.saturating_sub(length)),
        false,
    );
    if half_codeword {
        stream.resize(capacity, false);
    } else {
        let aligned = stream.len() + (8 - stream.len() % 8);
        stream.resize(aligned, false);
        let pads = (capacity / 8).saturating_sub(stream.len() / 8);
        for i in 0..pads {
            let pad: u8 = if i % 2 == 0 { 0xEC } else { 0x11 };
            stream.extend((0..8).map(|b| pad & (0x80 >> b) != 0));
        }
    }
    stream.truncate(capacity.div_ceil(8) * 8);
    let mut packed = vec![0u8; stream.len().div_ceil(8)];
    for (i, bit) in stream.iter().enumerate() {
        if *bit {
            packed[i / 8] |= 0x80 >> (i % 8);
        }
    }
    Ok(packed)
}

type MaskFn = fn(i64, i64) -> bool;

/// Places the codewords and picks the mask the way segno does. The `qrcode` crate scores masks with a
/// linear balance term (segno steps by 5%, as the standard does), so its choice differs for about half of
/// the codes, and it ends M3-L on a whole codeword where the standard (and segno) use a half; so the data
/// goes in here and only the finder patterns, error correction and format information are the crate's.
fn finish_qr(
    bits: qrcode::bits::Bits,
    version: qrcode::Version,
    level: qrcode::EcLevel,
) -> Result<Vec<Vec<bool>>, String> {
    use qrcode::canvas::{Canvas as QrCanvas, MaskPattern, Module};
    use qrcode::Version;

    let data = segno_data_codewords(bits, version, level)?;
    let (codewords, correction) =
        qrcode::ec::construct_codewords(&data, version, level).map_err(|e| e.to_string())?;
    let mut canvas = QrCanvas::new(version, level);
    canvas.draw_all_functional_patterns();
    let width = version.width();
    // The crate only knows M3-M ends on a half codeword; M3-L does too (84 bits), so place the data here.
    let half_codeword = matches!(version, Version::Micro(1) | Version::Micro(3));
    let mut stream: Vec<bool> = Vec::new();
    for (i, byte) in codewords.iter().enumerate() {
        let last_half = half_codeword && i + 1 == codewords.len();
        let count = if last_half { 4 } else { 8 };
        stream.extend((0..count).map(|b| byte & (0x80 >> b) != 0));
    }
    for byte in &correction {
        stream.extend((0..8).map(|b| byte & (0x80 >> b) != 0));
    }
    let mut next = stream.into_iter();
    let micro_code = matches!(version, Version::Micro(_));
    let (mut x, mut upward) = (width - 1, true);
    while x >= 0 {
        if !micro_code && x == 6 {
            x -= 1;
            continue;
        }
        let rows: Vec<i16> = if upward {
            (0..width).rev().collect()
        } else {
            (0..width).collect()
        };
        for y in rows {
            for column in [x, x - 1] {
                if column >= 0 && canvas.get(column, y) == Module::Empty {
                    let dark = next.next().unwrap_or(false);
                    *canvas.get_mut(column, y) = Module::Unmasked(if dark {
                        qrcode::Color::Dark
                    } else {
                        qrcode::Color::Light
                    });
                }
            }
        }
        upward = !upward;
        x -= 2;
    }

    let micro = matches!(version, Version::Micro(_));
    let patterns: Vec<(MaskPattern, MaskFn)> = if micro {
        vec![
            (MaskPattern::HorizontalLines, |_x, y| y % 2 == 0),
            (MaskPattern::LargeCheckerboard, |x, y| {
                (y / 2 + x / 3) % 2 == 0
            }),
            (MaskPattern::Diamonds, |x, y| {
                ((x * y) % 2 + (x * y) % 3) % 2 == 0
            }),
            (MaskPattern::Meadow, |x, y| {
                ((x + y) % 2 + (x * y) % 3) % 2 == 0
            }),
        ]
    } else {
        vec![
            (MaskPattern::Checkerboard, |x, y| (x + y) % 2 == 0),
            (MaskPattern::HorizontalLines, |_x, y| y % 2 == 0),
            (MaskPattern::VerticalLines, |x, _y| x % 3 == 0),
            (MaskPattern::DiagonalLines, |x, y| (x + y) % 3 == 0),
            (MaskPattern::LargeCheckerboard, |x, y| {
                (y / 2 + x / 3) % 2 == 0
            }),
            (MaskPattern::Fields, |x, y| (x * y) % 2 + (x * y) % 3 == 0),
            (MaskPattern::Diamonds, |x, y| {
                ((x * y) % 2 + (x * y) % 3) % 2 == 0
            }),
            (MaskPattern::Meadow, |x, y| {
                ((x + y) % 2 + (x * y) % 3) % 2 == 0
            }),
        ]
    };
    // Format and version information are not on the page when segno scores the masks.
    let reserved = |x: i16, y: i16| {
        let near = |v: i16| (0..9).contains(&v);
        let far = |v: i16| v >= width - 8;
        // Where the timing pattern crosses a format strip it stays on the page.
        if micro {
            (x == 8 && near(y) && y != 0) || (y == 8 && near(x) && x != 0)
        } else {
            (x == 8 && (near(y) || far(y)) && y != 6)
                || (y == 8 && (near(x) || far(x)) && x != 6)
                || (width > 41
                    && ((y < 6 && (width - 11..width - 8).contains(&x))
                        || (x < 6 && (width - 11..width - 8).contains(&y))))
        }
    };
    let mut best: Option<(usize, i64)> = None;
    for (number, (_, selected)) in patterns.iter().enumerate() {
        let mut matrix = vec![vec![0u8; width as usize]; width as usize];
        for y in 0..width {
            for x in 0..width {
                let module = canvas.get(x, y);
                let dark = module.is_dark();
                let value = if matches!(module, Module::Unmasked(_)) {
                    dark ^ selected(x as i64, y as i64)
                } else {
                    dark && !reserved(x, y)
                };
                matrix[y as usize][x as usize] = value as u8;
            }
        }
        let score = if micro {
            micro_score(&matrix)
        } else {
            penalty_score(&matrix)
        };
        let better = match best {
            None => true,
            Some((_, top)) => {
                if micro {
                    score > top
                } else {
                    score < top
                }
            }
        };
        if better {
            best = Some((number, score));
        }
    }
    let (chosen, _) = best.ok_or_else(|| "no mask".to_string())?;
    canvas.apply_mask(patterns[chosen].0);
    let side = width as usize;
    let cells: Vec<bool> = canvas
        .into_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    Ok(cells.chunks(side).map(|row| row.to_vec()).collect())
}

/// segno's `evaluate_micro_mask`: the larger of the two edge sums counts for less.
fn micro_score(matrix: &[Vec<u8>]) -> i64 {
    let side = matrix.len();
    let sum1: i64 = (1..side).map(|i| matrix[i][side - 1] as i64).sum();
    let sum2: i64 = (1..side).map(|i| matrix[side - 1][i] as i64).sum();
    if sum1 <= sum2 {
        sum1 * 16 + sum2
    } else {
        sum2 * 16 + sum1
    }
}

/// segno's `mask_scores` summed: runs, 2x2 blocks, finder-like patterns and balance.
fn penalty_score(matrix: &[Vec<u8>]) -> i64 {
    let size = matrix.len();
    let pattern = [1u8, 0, 1, 1, 1, 0, 1];
    let occurrences = |seq: &[u8]| -> i64 {
        let find = |from: usize| -> Option<usize> {
            if from + 7 > seq.len() {
                return None;
            }
            (from..=seq.len() - 7).find(|&i| seq[i..i + 7] == pattern)
        };
        let any = |lo: usize, hi: usize| {
            seq[lo.min(seq.len())..hi.min(seq.len()).max(lo.min(seq.len()))]
                .iter()
                .any(|v| *v != 0)
        };
        let mut count = 0;
        let mut at = find(0);
        while let Some(idx) = at {
            let mut offset = idx + 7;
            if idx == 0
                || idx == size - 7
                || !any(idx.saturating_sub(4), idx.min(size))
                || !any(offset, (offset + 4).min(size))
            {
                count += 40;
            } else {
                offset = idx + 4;
            }
            at = find(offset);
        }
        count
    };
    let (mut n1, mut n2, mut n3) = (0i64, 0i64, 0i64);
    let mut dark = 0i64;
    let mut column = vec![0u8; size];
    for i in 0..size {
        let row = &matrix[i];
        let (mut row_prev, mut col_prev) = (-1i32, -1i32);
        let (mut row_run, mut col_run) = (0i64, 0i64);
        for j in 0..size {
            let row_now = row[j] as i32;
            let col_now = matrix[j][i] as i32;
            column[j] = col_now as u8;
            dark += row_now as i64;
            if row_now == row_prev {
                row_run += 1;
            } else {
                if row_run >= 5 {
                    n1 += row_run - 2;
                }
                row_run = 1;
            }
            if col_now == col_prev {
                col_run += 1;
            } else {
                if col_run >= 5 {
                    n1 += col_run - 2;
                }
                col_run = 1;
            }
            if i > 0 && j > 0 {
                let last = &matrix[i - 1];
                if row_now == row_prev && row_prev == last[j] as i32 && last[j] == last[j - 1] {
                    n2 += 3;
                }
            }
            row_prev = row_now;
            col_prev = col_now;
        }
        n3 += occurrences(row);
        n3 += occurrences(&column);
        if row_run >= 5 {
            n1 += row_run - 2;
        }
        if col_run >= 5 {
            n1 += col_run - 2;
        }
    }
    let percent = dark as f64 / (size * size) as f64;
    let n4 = 10 * ((percent * 100.0 - 50.0).abs() / 5.0) as i64;
    n1 + n2 + n3 + n4
}

// ================================================================================================
// Text
// ================================================================================================

/// Pillow raises division by zero when measuring an empty string.
fn length(face: &fonts::Face, text: &str) -> f64 {
    if text.is_empty() {
        0.0
    } else {
        face.length_mono(text)
    }
}

/// A mode "1" mask of exactly width x height. Size 0 picks the largest size that fits.
#[allow(clippy::too_many_arguments)]
pub fn render_text(
    text: &str,
    width: u32,
    height: u32,
    font: &str,
    size: i32,
    align: &str,
    valign: &str,
    wrap: bool,
    spacing: f64,
    external: bool,
) -> Result<GrayImage, DesignError> {
    if font == fonts::PIXEL_FONT {
        return Ok(render_pixel_text(
            text, width, height, size, align, valign, wrap,
        ));
    }
    let spacing = if spacing.is_finite() { spacing } else { 1.15 };

    struct Layout {
        face: fonts::Face,
        lines: Vec<String>,
        line_height: i64,
        widest: i64,
    }
    let layout = |candidate: i32| -> Result<Layout, DesignError> {
        let face = fonts::load_font(font, candidate.max(1) as u32, external)?;
        let lines = if wrap {
            wrap_lines(text, &face, width as f64)
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let (ascent, descent) = face.metrics();
        let line_height =
            draw::py_round(((ascent + descent) as f64) * spacing).clamp(1.0, 1.0e12) as i64;
        let widest = lines.iter().map(|l| length(&face, l)).fold(0.0, f64::max);
        Ok(Layout {
            face,
            lines,
            line_height,
            widest: widest as i64,
        })
    };

    let chosen = if size > 0 {
        layout(size)?
    } else {
        let (mut low, mut high, mut best) = (6i32, (height as i32 + 8).min(400), 6i32);
        while low <= high {
            let mid = (low + high) / 2;
            let probe = layout(mid)?;
            let room = height as f64 + (spacing - 1.0) * mid as f64;
            if (probe.line_height as f64 * probe.lines.len() as f64) <= room
                && probe.widest <= width as i64
            {
                best = mid;
                low = mid + 1;
            } else {
                high = mid - 1;
            }
        }
        layout(best)?
    };

    let mut mask = raster::blank(width, height);
    let total = chosen.line_height * chosen.lines.len() as i64;
    let free = height as i64 - total;
    let top = match valign {
        "t" => 0,
        "m" => free.div_euclid(2),
        "b" => free,
        _ => 0,
    };
    for (index, line) in chosen.lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let measured = length(&chosen.face, line);
        let left = match align {
            "l" => 0.0,
            "c" => (width as f64 - measured) / 2.0,
            "r" => width as f64 - measured,
            _ => 0.0,
        };
        let y = top + index as i64 * chosen.line_height;
        chosen.face.draw(&mut mask, left, y as f64, line, 255, true);
    }
    Ok(mask)
}

fn wrap_lines(text: &str, face: &fonts::Face, width: f64) -> Vec<String> {
    let measure = |chars: &[char]| -> f64 {
        let s: String = chars.iter().collect();
        face.length_mono(&s)
    };
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(pyval::is_space).filter(|w| !w.is_empty()) {
            let mut word: Vec<char> = word.chars().collect();
            while measure(&word) > width && word.len() > 1 {
                let mut cut = word.len() - 1;
                while cut > 1 && measure(&word[..cut]) > width {
                    cut -= 1;
                }
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(word[..cut].iter().collect());
                word = word[cut..].to_vec();
            }
            let word: String = word.into_iter().collect();
            let candidate = if line.is_empty() {
                word.clone()
            } else {
                format!("{line} {word}")
                    .trim_matches(pyval::is_space)
                    .to_string()
            };
            if face.length_mono(&candidate) <= width {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word;
            }
        }
        lines.push(line);
    }
    lines
}

/// Pillow's bitmap font, scaled in whole steps so every pixel stays square.
fn render_pixel_text(
    text: &str,
    width: u32,
    height: u32,
    size: i32,
    align: &str,
    valign: &str,
    wrap: bool,
) -> GrayImage {
    let face = fonts::pixel_face();
    let char_height = fonts::PIXEL_HEIGHT as i64;
    let layout = |scale: i64| -> Vec<String> {
        if wrap {
            wrap_lines(text, &face, (width as i64 / scale) as f64)
        } else {
            text.split('\n').map(str::to_string).collect()
        }
    };
    let scale = if size > 0 {
        (draw::py_round(size as f64 / char_height as f64) as i64).max(1)
    } else {
        let mut scale = 1;
        for candidate in 1..64i64 {
            let lines = layout(candidate);
            let widest = lines.iter().map(|l| length(&face, l)).fold(0.0, f64::max);
            let fits = lines.len() as i64 * (char_height + 1) * candidate <= height as i64
                && widest * candidate as f64 <= width as f64;
            if fits {
                scale = candidate;
            } else {
                break;
            }
        }
        scale
    }
    .min(4096);

    let lines = layout(scale);
    let (nw, nh) = (
        (width as i64 / scale).max(1) as u32,
        (height as i64 / scale).max(1) as u32,
    );
    let mut native = raster::blank(nw, nh);
    let total = lines.len() as i64 * (char_height + 1);
    let free = nh as i64 - total;
    let top = match valign {
        "t" => 0,
        "m" => free.div_euclid(2),
        "b" => free,
        _ => 0,
    };
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let measured = length(&face, line);
        let left = match align {
            "l" => 0.0,
            "c" => (nw as f64 - measured) / 2.0,
            "r" => nw as f64 - measured,
            _ => 0.0,
        };
        let y = top + index as i64 * (char_height + 1);
        face.draw(&mut native, left, y as f64, line, 255, true);
    }
    let scaled = raster::resize(
        &native,
        nw * scale as u32,
        nh * scale as u32,
        Resample::Nearest,
    );
    let mut mask = raster::blank(width, height);
    raster::paste(&mut mask, &scaled, 0, 0, None);
    mask
}

// ================================================================================================
// Catalog
// ================================================================================================

/// Names that can be used. Kept out of tool schemas, so they only cost tokens when asked for.
pub fn catalog(kind: Option<&str>) -> Result<Value, DesignError> {
    let list = |names: Vec<String>| Value::Array(names.into_iter().map(Value::String).collect());
    let sorted = |mut names: Vec<String>| {
        names.sort();
        names
    };
    let mut pattern_names = sorted(patterns::names());
    pattern_names.push("gray:N".into());
    let mut everything: Vec<(&str, Value)> = vec![
        ("figures", list(sorted(figures::names()))),
        ("patterns", list(pattern_names)),
        ("frames", list(sorted(frames::names()))),
        ("fonts", list(fonts::font_names())),
        (
            "looks",
            list(
                poster::looks()
                    .into_iter()
                    .map(|(name, when)| format!("{name}: {when}"))
                    .collect(),
            ),
        ),
        (
            "dithers",
            list(assets::DITHERS.iter().map(|s| s.to_string()).collect()),
        ),
        ("ops", list(OPS.iter().map(|s| s.to_string()).collect())),
        ("system_fonts", list(fonts::all_system_fonts())),
    ];
    if let Some(kind) = kind.filter(|k| !k.is_empty()) {
        let Some(position) = everything.iter().position(|(name, _)| *name == kind) else {
            let names: Vec<&str> = everything.iter().map(|(name, _)| *name).collect();
            return Err(design(format!(
                "Unknown catalog {}. Use one of {}.",
                py_repr(kind),
                names.join(", ")
            )));
        };
        let (name, value) = everything.swap_remove(position);
        let mut map = Map::new();
        map.insert(name.to_string(), value);
        return Ok(Value::Object(map));
    }
    everything.pop(); // system_fonts
    Ok(Value::Object(
        everything
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    fn black(canvas: &Canvas) -> usize {
        canvas.image.pixels().filter(|p| p.0[0] < 128).count()
    }

    fn drawn(ops: &[Value]) -> Canvas {
        let mut canvas = Canvas::default();
        canvas.apply_all(ops).expect("ops apply");
        canvas
    }

    fn failure(canvas: &mut Canvas, op: Value) -> String {
        canvas.apply(op).expect_err("op should fail").0
    }

    fn merged(base: &Value, extra: Value) -> Value {
        let mut map = base.as_object().cloned().unwrap_or_default();
        map.extend(extra.as_object().cloned().unwrap_or_default());
        Value::Object(map)
    }

    #[test]
    fn the_grid_is_exactly_312_by_216() {
        assert_eq!((WIDTH, HEIGHT), (312, 216));
        assert_eq!(BITMAP_BYTES, 8424);
        assert_eq!(Canvas::default().image.dimensions(), (312, 216));
    }

    #[test]
    fn bitmap_round_trip_keeps_every_pixel() {
        let canvas = drawn(&[json!({"op": "formula", "f": "(x*7+y*3)%5<2"})]);
        let bitmap = canvas.bitmap().unwrap();
        assert_eq!(b64().decode(&bitmap).unwrap().len(), 8424);
        assert_eq!(
            bitmap_to_image(&bitmap).unwrap().as_raw(),
            canvas.image.as_raw()
        );
    }

    #[test]
    fn bitmap_bits_mean_1_is_black() {
        let canvas = drawn(&[json!({"op": "pixels", "rows": ["#"], "x": 0, "y": 0})]);
        let packed = b64().decode(canvas.bitmap().unwrap()).unwrap();
        assert_eq!(packed[0], 0x80);
        assert_eq!(packed.iter().map(|b| *b as u32).sum::<u32>(), 0x80);
    }

    #[test]
    fn bad_bitmaps_are_refused() {
        for bad in ["", "not base64!!", &b64().encode(b"short")] {
            assert!(bitmap_to_image(bad).is_err(), "{bad}");
        }
        assert_eq!(
            bitmap_to_image(&b64().encode(b"short")).unwrap_err().0,
            "A note bitmap is 8424 bytes, not 5."
        );
        assert_eq!(
            bitmap_to_image("not base64!!").unwrap_err().0,
            "That isn't a valid note bitmap."
        );
    }

    #[test]
    fn wrong_size_images_are_refused() {
        let error = image_to_bitmap(&raster::paper(100, 100)).unwrap_err();
        assert_eq!(error.0, "A note is exactly 312x216, not 100x100.");
    }

    #[test]
    fn every_figure_draws_inside_its_box() {
        for name in figures::names() {
            let canvas = drawn(&[
                json!({"op": "figure", "name": name, "x": 100, "y": 50, "w": 80, "h": 80}),
            ]);
            assert!(black(&canvas) > 30, "{name}");
        }
    }

    #[test]
    fn every_pattern_draws() {
        for name in patterns::names() {
            let canvas = drawn(&[json!({"op": "pattern", "name": format!("{name}:5")})]);
            let ink = black(&canvas);
            assert!(0 < ink && ink < 312 * 216, "{name}");
        }
    }

    #[test]
    fn every_frame_draws() {
        for name in frames::names() {
            let canvas = drawn(&[json!({"op": "frame", "name": name, "inset": 4})]);
            assert!(black(&canvas) > 100, "{name}");
        }
    }

    #[test]
    fn a_figure_works_as_a_frame() {
        assert!(
            black(&drawn(&[
                json!({"op": "frame", "name": "heart", "size": 4})
            ])) > 200
        );
    }

    #[test]
    fn gray_tone_has_about_the_right_darkness() {
        let share =
            black(&drawn(&[json!({"op": "pattern", "name": "gray:25"})])) as f64 / (312.0 * 216.0);
        assert!((share - 0.25).abs() < 0.02, "{share}");
    }

    #[test]
    fn every_font_prints_text() {
        let found = catalog(Some("fonts")).unwrap();
        for font in found["fonts"].as_array().unwrap() {
            let canvas = drawn(&[json!({"op": "text", "s": "Hello 123", "font": font})]);
            assert!(black(&canvas) > 100, "{font}");
        }
    }

    #[test]
    fn text_auto_size_fills_more_of_a_bigger_box() {
        let small = black(&drawn(&[
            json!({"op": "text", "s": "Hi", "w": 60, "h": 30}),
        ]));
        let big = black(&drawn(&[
            json!({"op": "text", "s": "Hi", "w": 300, "h": 200}),
        ]));
        assert!(big > small * 4, "{big} vs {small}");
    }

    fn ink_box(canvas: &Canvas) -> (u32, u32, u32, u32) {
        raster::getbbox(&raster::invert(&canvas.image)).expect("something drawn")
    }

    #[test]
    fn long_text_stays_in_its_box() {
        let canvas = drawn(&[
            json!({"op": "text", "s": "word ".repeat(400), "x": 50, "y": 50, "w": 100, "h": 60}),
        ]);
        let (x0, y0, x1, y1) = ink_box(&canvas);
        assert!(
            x0 >= 50 && x1 <= 150 && y0 >= 50 && y1 <= 112,
            "{:?}",
            (x0, y0, x1, y1)
        );
    }

    #[test]
    fn ink_white_and_invert_and_pattern() {
        let base = json!({"op": "shape", "kind": "rect", "x": 0, "y": 0, "w": 100, "h": 100});
        assert_eq!(black(&drawn(std::slice::from_ref(&base))), 100 * 100);
        assert_eq!(
            black(&drawn(&[
                base.clone(),
                merged(&base, json!({"ink": "white"}))
            ])),
            0
        );
        assert_eq!(
            black(&drawn(&[
                base.clone(),
                merged(&base, json!({"w": 50, "ink": "invert"}))
            ])),
            50 * 100
        );
        assert_eq!(
            black(&drawn(&[merged(&base, json!({"ink": "invert"}))])),
            100 * 100
        );
        let dotted = black(&drawn(&[merged(&base, json!({"ink": "dots:4"}))]));
        assert!(0 < dotted && dotted < 100 * 100);
    }

    #[test]
    fn shapes_all_draw() {
        let shapes = [
            json!({"kind": "rect", "w": 50, "h": 50}),
            json!({"kind": "rrect", "w": 50, "h": 50}),
            json!({"kind": "ellipse", "w": 50, "h": 50}),
            json!({"kind": "line", "pts": [[0, 0], [100, 100]], "lw": 3}),
            json!({"kind": "curve", "pts": [0, 0, 50, 90, 100, 0]}),
            json!({"kind": "polygon", "pts": [[0, 0], [90, 0], [0, 90]]}),
            json!({"kind": "arc", "w": 50, "h": 50, "a1": 180}),
            json!({"kind": "pie", "w": 50, "h": 50, "a1": 90}),
            json!({"kind": "chord", "w": 50, "h": 50, "a1": 200}),
        ];
        for shape in shapes {
            let op = merged(&json!({"op": "shape"}), shape.clone());
            assert!(black(&drawn(&[op])) > 10, "{shape}");
        }
    }

    #[test]
    fn pixel_art_lands_exactly_where_asked() {
        let canvas =
            drawn(&[json!({"op": "pixels", "rows": ["#.#", ".#."], "x": 10, "y": 20, "scale": 2})]);
        let at = |x: u32, y: u32| canvas.image.get_pixel(x, y).0[0];
        assert!(at(10, 20) == 0 && at(11, 21) == 0);
        assert_ne!(at(12, 20), 0);
        assert_eq!(at(14, 20), 0);
        assert_eq!(black(&canvas), 3 * 4);
    }

    #[test]
    fn full_raw_bitmap_gives_total_control() {
        let source = drawn(&[json!({"op": "formula", "f": "x%3==0"})]);
        let copy = drawn(&[json!({"op": "pixels", "b64": source.bitmap().unwrap(), "w": 312})]);
        assert_eq!(copy.image.as_raw(), source.image.as_raw());
    }

    #[test]
    fn svg_path_draws_crisply() {
        let canvas = drawn(&[
            json!({"op": "svg", "d": "M0 0H24V24H0Z", "x": 10, "y": 10, "w": 48, "h": 48}),
        ]);
        assert_eq!(black(&canvas), 48 * 48);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prins-canvas-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn gradient_png(dir: &Path, size: u32) -> PathBuf {
        let image = GrayImage::from_fn(size, size, |_x, y| {
            image::Luma([(y * 255 / (size - 1)) as u8])
        });
        let path = dir.join("gray.png");
        image.save(&path).unwrap();
        path
    }

    #[test]
    fn full_svg_markup_and_inline_images() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><circle cx="5" cy="5" r="5"/></svg>"#;
        assert!(
            black(&drawn(&[
                json!({"op": "svg", "svg": svg, "w": 50, "h": 50})
            ])) > 1500
        );

        let dir = temp_dir("image");
        let path = gradient_png(&dir, 64);
        let from_file =
            drawn(&[json!({"op": "image", "src": path.to_str().unwrap(), "w": 64, "h": 64})]);
        assert!(
            (500..3600).contains(&black(&from_file)),
            "{}",
            black(&from_file)
        );
        let inline = b64().encode(std::fs::read(&path).unwrap());
        assert_eq!(
            black(&drawn(&[
                json!({"op": "image", "src": inline, "w": 64, "h": 64})
            ])),
            black(&from_file)
        );
    }

    #[test]
    fn every_dither_gives_a_black_and_white_picture() {
        let dir = temp_dir("dither");
        let path = gradient_png(&dir, 100);
        for dither in ["atkinson", "floyd", "bayer", "halftone", "threshold"] {
            let canvas = drawn(&[
                json!({"op": "image", "src": path.to_str().unwrap(), "w": 100, "h": 100, "dither": dither}),
            ]);
            let total = black(&canvas);
            assert!(0 < total && total < 100 * 100, "{dither}");
            let band = |y0: i32, y1: i32| {
                raster::crop(&canvas.image, 0, y0, 100, y1)
                    .pixels()
                    .filter(|p| p.0[0] == 0)
                    .count()
            };
            assert!(band(80, 100) < band(0, 20), "{dither}");
        }
    }

    #[test]
    fn image_dot_dithers_on_square_blocks() {
        let dir = temp_dir("dot");
        let path = gradient_png(&dir, 100);
        for dot in [2u32, 3] {
            let canvas = drawn(&[
                json!({"op": "image", "src": path.to_str().unwrap(), "w": 99, "h": 99, "dot": dot, "contrast": 1.5, "bright": 1.1}),
            ]);
            assert!(black(&canvas) > 0, "dot {dot}");
            for y in 0..99 {
                for x in 0..99 {
                    let corner = canvas.image.get_pixel(x / dot * dot, y / dot * dot).0[0];
                    assert_eq!(canvas.image.get_pixel(x, y).0[0], corner, "dot {dot} at {x},{y}");
                }
            }
        }
        let mut fresh = Canvas::default();
        let message = failure(
            &mut fresh,
            json!({"op": "image", "src": path.to_str().unwrap(), "contrast": "lots"}),
        );
        assert!(message.contains("contrast must be a number"), "{message}");
    }

    #[test]
    fn qr_code_is_drawn_with_a_scale_of_whole_pixels() {
        let canvas = drawn(&[
            json!({"op": "qr", "s": "https://example.com", "x": 10, "y": 10, "w": 150, "h": 150}),
        ]);
        assert!(black(&canvas) > 2000);
        let mut fresh = Canvas::default();
        let message = failure(
            &mut fresh,
            json!({"op": "qr", "s": "x".repeat(300), "w": 20, "h": 20}),
        );
        assert!(message.starts_with("This QR needs at least"), "{message}");
    }

    #[test]
    fn transforms() {
        let base = vec![json!({"op": "shape", "kind": "rect", "x": 0, "y": 0, "w": 50, "h": 216})];
        let kinds = [
            json!({"kind": "invert"}),
            json!({"kind": "flip-h"}),
            json!({"kind": "flip-v"}),
            json!({"kind": "rotate", "deg": 90}),
            json!({"kind": "mirror-h"}),
            json!({"kind": "mirror-v"}),
            json!({"kind": "shift", "dx": 10}),
            json!({"kind": "pixelate", "size": 8}),
            json!({"kind": "outline"}),
        ];
        for kind in kinds {
            let mut ops = base.clone();
            ops.push(merged(&json!({"op": "transform"}), kind.clone()));
            assert_eq!(drawn(&ops).image.dimensions(), (312, 216), "{kind}");
        }
        let mut invert = base.clone();
        invert.push(json!({"op": "transform", "kind": "invert"}));
        assert_eq!(black(&drawn(&invert)), 262 * 216);
        let mut mirror = base;
        mirror.push(json!({"op": "transform", "kind": "mirror-h"}));
        assert_eq!(black(&drawn(&mirror)), 100 * 216);
    }

    #[test]
    fn repeat_and_scatter() {
        let square = json!({"op": "shape", "kind": "rect", "w": 10, "h": 10});
        assert_eq!(
            black(&drawn(&[
                json!({"op": "repeat", "n": 5, "dx": 20, "ops": [square.clone()]})
            ])),
            5 * 100
        );
        let scatter = json!({"op": "scatter", "n": 30, "seed": 3, "ops": [square]});
        let one = drawn(std::slice::from_ref(&scatter));
        let two = drawn(&[scatter]);
        assert_eq!(one.image.as_raw(), two.image.as_raw());
        assert!(black(&one) > 500);
    }

    #[test]
    fn undo_returns_the_previous_picture() {
        let mut canvas = drawn(&[json!({"op": "figure", "name": "heart", "w": 80, "h": 80})]);
        let before = canvas.image.clone();
        canvas
            .apply(json!({"op": "pattern", "name": "checker"}))
            .unwrap();
        canvas.undo(1);
        assert_eq!(canvas.image.as_raw(), before.as_raw());
        assert_eq!(canvas.ops.len(), 1);
    }

    #[test]
    fn bad_ops_give_a_clear_error_and_change_nothing() {
        let bad_ops = [
            json!({"op": "nope"}),
            json!({"s": "no op"}),
            json!({"op": "figure", "name": "nope"}),
            json!({"op": "pattern", "name": "nope"}),
            json!({"op": "frame", "name": "nope"}),
            json!({"op": "text", "s": "x", "font": "no-such-font"}),
            json!({"op": "text", "s": "x", "ink": "nope"}),
            json!({"op": "text", "s": "x", "w": 0}),
            json!({"op": "shape", "kind": "nope"}),
            json!({"op": "shape", "kind": "line", "pts": [[1, 1]]}),
            json!({"op": "image", "src": "/no/such/file.png"}),
            json!({"op": "svg"}),
            json!({"op": "pixels"}),
            json!({"op": "svg", "d": "M0\"><script>"}),
            json!({"op": "formula", "f": "__import__('os')"}),
            json!({"op": "transform", "kind": "nope"}),
            json!({"op": "text", "s": "x", "x": "abc"}),
        ];
        for bad in bad_ops {
            let mut canvas = drawn(&[json!({"op": "figure", "name": "star", "w": 50, "h": 50})]);
            let before = canvas.image.clone();
            assert!(canvas.apply(bad.clone()).is_err(), "{bad}");
            assert_eq!(canvas.image.as_raw(), before.as_raw(), "{bad}");
            assert_eq!(canvas.ops.len(), 1, "{bad}");
        }
    }

    #[test]
    fn formulas_cant_escape_the_whitelist() {
        for attack in [
            "__import__('os').system('true')",
            "().__class__",
            "open('/etc/passwd')",
            "x.real",
            "[i for i in (1,)]",
            "(lambda: 1)()",
            "eval('1')",
            "getattr(x,'real')",
            "'text'",
            "x if y else __builtins__",
            "f'{x}'",
        ] {
            let mut canvas = Canvas::default();
            assert!(
                canvas.apply(json!({"op": "formula", "f": attack})).is_err(),
                "{attack}"
            );
        }
    }

    #[test]
    fn untrusted_designs_cant_read_files_or_the_network() {
        let dir = temp_dir("untrusted");
        let path = gradient_png(&dir, 8);
        let mut canvas = Canvas::new(false, "");
        for op in [
            json!({"op": "image", "src": path.to_str().unwrap()}),
            json!({"op": "image", "src": "https://example.com/x.png"}),
            json!({"op": "svg", "src": "https://example.com/x.svg"}),
            json!({"op": "text", "s": "x", "font": path.to_str().unwrap()}),
        ] {
            assert!(canvas.apply(op.clone()).is_err(), "{op}");
        }
    }

    #[test]
    fn messages_keep_pythons_wording() {
        let mut canvas = Canvas::default();
        let cases = [
            (json!({"op": "nope"}), format!("Unknown op 'nope'. Ops: {}.", OPS.join(", "))),
            (json!({"s": "x"}), "Each operation needs an \"op\" name.".to_string()),
            (json!({"op": "text", "s": "x", "x": "abc"}), "x must be a number, not 'abc'.".to_string()),
            (json!({"op": "text", "s": "x", "w": 0}), "Box has no area: w=0, h=216.".to_string()),
            (json!({"op": "text", "s": "x", "w": 5000}), "Box is too big.".to_string()),
            (
                json!({"op": "text", "s": "x", "ink": "nope"}),
                "Unknown ink 'nope'. Use black, white, invert, or a pattern name.".to_string(),
            ),
            (
                json!({"op": "text", "s": "x", "spacing": "wide"}),
                "Bad text op: could not convert string to float: 'wide'".to_string(),
            ),
            (
                json!({"op": "text", "s": "x", "spacing": null}),
                "Bad text op: float() argument must be a string or a real number, not 'NoneType'".to_string(),
            ),
            (
                json!({"op": "text", "s": "x", "font": "no-such-font"}),
                "Unknown font 'no-such-font'. See catalog fonts.".to_string(),
            ),
            (
                json!({"op": "figure", "name": "nope"}),
                "Unknown figure 'nope'. See catalog figures, or use emoji:\u{1f382}.".to_string(),
            ),
            (
                json!({"op": "pattern", "name": "nope"}),
                "Unknown pattern 'nope'. See catalog patterns.".to_string(),
            ),
            (
                json!({"op": "frame", "name": "nope"}),
                "Unknown frame 'nope'. See catalog frames. Figure names also work.".to_string(),
            ),
            (
                json!({"op": "shape", "kind": "nope"}),
                "Unknown shape 'nope'. Use rect, rrect, ellipse, line, curve, polygon, arc, pie or chord.".to_string(),
            ),
            (
                json!({"op": "shape", "kind": "line", "pts": [[1, 1]]}),
                "pts needs at least two points: [[x,y],[x,y]] or [x,y,x,y].".to_string(),
            ),
            (
                json!({"op": "shape", "kind": "line", "pts": [[1, 1], 5]}),
                "Bad shape op: 'int' object is not subscriptable".to_string(),
            ),
            (
                json!({"op": "shape", "ink": "dots:x"}),
                "Bad shape op: Bad pattern size in 'dots:x'".to_string(),
            ),
            (json!({"op": "image"}), "image needs a src: URL, file path, or base64.".to_string()),
            (
                json!({"op": "svg"}),
                "svg needs d (path data), svg (markup), or src (URL or path).".to_string(),
            ),
            (
                json!({"op": "svg", "d": "M0\"><script>"}),
                "d must be plain SVG path data.".to_string(),
            ),
            (
                json!({"op": "svg", "d": "M0 0", "vb": "0 0 a b"}),
                "vb must be four numbers.".to_string(),
            ),
            (json!({"op": "logo"}), "logo needs a name, such as spotify.".to_string()),
            (
                json!({"op": "pixels"}),
                "pixels needs rows (strings where . is off) or b64 (packed bits, 1 = black).".to_string(),
            ),
            (json!({"op": "pixels", "rows": []}), "rows must be a list of strings.".to_string()),
            (
                json!({"op": "pixels", "b64": b64().encode([1u8, 2, 3]), "w": 8, "h": 9}),
                "b64 has 3 bytes, but 8x9 needs 9.".to_string(),
            ),
            (json!({"op": "qr"}), "qr needs s, the text or URL to encode.".to_string()),
            (
                json!({"op": "qr", "s": "x", "ecc": "z"}),
                "Bad qr op: Illegal error correction level: \"z\". Supported levels: L, M, Q, H".to_string(),
            ),
            (
                json!({"op": "transform", "kind": "nope"}),
                "Unknown transform 'nope'. Use invert, flip-h, flip-v, rotate, mirror-h, mirror-v, shift, pixelate or outline."
                    .to_string(),
            ),
            (json!({"op": "repeat", "ops": [{"s": 1}]}), "Bad repeat op: 'op'".to_string()),
            (
                json!({"op": "repeat", "ops": 5}),
                "Bad repeat op: 'int' object is not iterable".to_string(),
            ),
            (
                json!({"op": "repeat", "ops": [{"op": "text", "s": "x", "ink": "nope"}]}),
                "Unknown ink 'nope'. Use black, white, invert, or a pattern name.".to_string(),
            ),
            (
                json!({"op": "poster"}),
                "Bad poster op: A poster needs something to say. Set say, or another field.".to_string(),
            ),
        ];
        for (op, expected) in cases {
            assert_eq!(failure(&mut canvas, op.clone()), expected, "{op}");
        }
        assert!(canvas.ops.is_empty());
    }

    #[test]
    fn numbers_read_like_python_float() {
        let canvas = drawn(&[
            json!({"op": "shape", "kind": "rect", "x": "10", "y": 20.4, "w": "50.5", "h": true}),
        ]);
        assert_eq!(black(&canvas), 50);
        assert_eq!(canvas.image.get_pixel(10, 20).0[0], 0);
        assert_eq!(canvas.image.get_pixel(59, 20).0[0], 0);
        assert_eq!(canvas.image.get_pixel(60, 20).0[0], 255);
        let mut fresh = Canvas::default();
        assert_eq!(
            failure(&mut fresh, json!({"op": "shape", "x": "nan"})),
            "x must be a number, not 'nan'."
        );
        assert_eq!(
            failure(&mut fresh, json!({"op": "shape", "x": null})),
            "x must be a number, not None."
        );
        assert_eq!(
            failure(&mut fresh, json!({"op": "shape", "x": [1]})),
            "x must be a number, not [1]."
        );
    }

    #[test]
    fn python_value_text() {
        assert_eq!(pyval::to_str(&json!(5)), "5");
        assert_eq!(pyval::to_str(&json!(5.0)), "5.0");
        assert_eq!(pyval::to_str(&json!(1e16)), "1e+16");
        assert_eq!(pyval::to_str(&json!(0.00001)), "1e-05");
        assert_eq!(pyval::to_str(&json!(0.0001)), "0.0001");
        assert_eq!(
            pyval::to_str(&json!(123456789012345.6)),
            "123456789012345.6"
        );
        assert_eq!(pyval::to_str(&json!(-1.5)), "-1.5");
        assert_eq!(pyval::to_str(&json!(null)), "None");
        assert_eq!(pyval::to_str(&json!(true)), "True");
        assert_eq!(pyval::to_str(&json!([1, "a", null])), "[1, 'a', None]");
        assert_eq!(pyval::to_str(&json!({"a": 1})), "{'a': 1}");
        assert_eq!(pyval::parse_float(" 1_000.5 "), Some(1000.5));
        assert_eq!(pyval::parse_float("1__0"), None);
        assert_eq!(pyval::parse_float("_1"), None);
        assert_eq!(pyval::parse_float("inf"), Some(f64::INFINITY));
        assert_eq!(pyval::parse_float(""), None);
        assert_eq!(pyval::to_int(&json!("12")), Ok(12));
        assert_eq!(pyval::to_int(&json!(5.9)), Ok(5));
        assert_eq!(
            pyval::to_int(&json!("x")),
            Err("invalid literal for int() with base 10: 'x'".to_string())
        );
    }

    #[test]
    fn ops_and_the_ink_system() {
        let mut canvas = Canvas::default();
        canvas
            .apply(json!({"op": "shape", "kind": "rect", "w": 20, "h": 20, "ink": "k"}))
            .unwrap();
        assert_eq!(black(&canvas), 400);
        canvas
            .apply(json!({"op": "shape", "kind": "rect", "w": 10, "h": 20, "ink": "x"}))
            .unwrap();
        assert_eq!(black(&canvas), 200);
        canvas
            .apply(json!({"op": "shape", "kind": "rect", "w": 5, "h": 5, "ink": null}))
            .unwrap();
        canvas
            .apply(json!({"op": "shape", "kind": "rect", "w": 5, "h": 5, "ink": ""}))
            .unwrap();
        canvas.apply(json!({"op": "clear"})).unwrap();
        assert_eq!(black(&canvas), 0);
        assert_eq!(canvas.ops.len(), 5);
        let mut sorted = OPS.to_vec();
        sorted.sort();
        assert_eq!(OPS.to_vec(), sorted);
    }

    #[test]
    fn a_repeat_that_fails_halfway_draws_nothing() {
        let mut canvas = Canvas::default();
        let op = json!({"op": "repeat", "n": 3, "dx": 30, "ops": [
            {"op": "shape", "kind": "rect", "w": 10, "h": 10},
            {"op": "shape", "kind": "nope"}
        ]});
        assert!(canvas.apply(op).is_err());
        assert_eq!(black(&canvas), 0);
    }

    #[test]
    fn the_op_limit_is_enforced() {
        let mut canvas = Canvas::default();
        for _ in 0..MAX_OPS {
            canvas
                .apply(json!({"op": "shape", "kind": "rect", "w": 1, "h": 1}))
                .unwrap();
        }
        assert_eq!(
            failure(&mut canvas, json!({"op": "clear"})),
            "A design can have 2000 operations."
        );
    }

    #[test]
    fn undo_replays_the_kept_ops() {
        let mut canvas = drawn(&[
            json!({"op": "shape", "kind": "rect", "w": 10, "h": 10}),
            json!({"op": "shape", "kind": "rect", "x": 50, "w": 10, "h": 10}),
            json!({"op": "shape", "kind": "rect", "x": 100, "w": 10, "h": 10}),
        ]);
        canvas.undo(2);
        assert_eq!(canvas.ops.len(), 1);
        assert_eq!(black(&canvas), 100);
        canvas.undo(0);
        assert!(canvas.blank());
        canvas.undo(5);
        assert!(canvas.blank());
    }

    #[test]
    fn odd_input_never_panics() {
        let ops = [
            json!({"op": "shape", "kind": "polygon", "pts": [[1e300, -1e300], [5, 5], [300, 1e12]], "lw": 1e9, "fill": false}),
            json!({"op": "shape", "kind": "curve", "pts": [[0, 0], [1e18, 5], [3, "x"]]}),
            json!({"op": "shape", "kind": "line", "pts": [[0, 0], [1e30, 1e30]], "lw": 50}),
            json!({"op": "shape", "kind": "pie", "x": -1e12, "y": 1e12, "a0": "inf", "a1": "nan"}),
            json!({"op": "shape", "kind": "rrect", "r": -5, "w": 3, "h": 3}),
            json!({"op": "shape", "kind": "rrect", "r": 1e30}),
            json!({"op": "shape", "x": 1e18}),
            json!({"op": "shape", "w": "inf"}),
            json!({"op": "shape", "rot": "nan"}),
            json!({"op": "shape", "rot": 1e300}),
            json!({"op": "text", "s": "x", "size": 1e12}),
            json!({"op": "text", "s": "x", "size": -3, "spacing": 1e30}),
            json!({"op": "text", "s": "x", "spacing": "nan"}),
            json!({"op": "text", "s": "x", "spacing": "inf"}),
            json!({"op": "text", "s": "x", "font": "pixel", "size": 1e12}),
            json!({"op": "text", "s": [1, 2], "x": {"a": 1}}),
            json!({"op": "frame", "size": 1e12, "inset": -1e12}),
            json!({"op": "frame", "inset": 1e12}),
            json!({"op": "figure", "name": "star", "x": -1e15, "y": 1e15}),
            json!({"op": "pixels", "rows": ["#"], "scale": 1e12}),
            json!({"op": "pixels", "b64": "AAAA", "w": -5}),
            json!({"op": "pixels", "b64": "AAAA", "w": 0, "h": 1e12}),
            json!({"op": "pixels", "b64": "AAAA", "w": 1e12}),
            json!({"op": "transform", "kind": "shift", "dx": 1e18, "dy": -1e18}),
            json!({"op": "transform", "kind": "rotate", "deg": "nan"}),
            json!({"op": "transform", "kind": "pixelate", "size": 1e12}),
            json!({"op": "transform", "x": -1e12, "w": 4000, "h": 4000}),
            json!({"op": "qr", "s": "x", "w": 1, "h": 1}),
            json!({"op": "repeat", "n": 1e18, "ops": []}),
            json!({"op": "repeat", "n": 2, "ops": ["", 5, [1], {"x": {}}]}),
            json!({"op": "scatter", "seed": -5e30, "n": 3, "ops": [{"op": "clear"}]}),
            json!({"op": "scatter", "seed": "x"}),
            json!({"op": 5}),
            json!({"op": null}),
            json!({"op": ["a"]}),
            json!({"op": "formula", "f": null}),
            json!({"op": "pattern", "name": "dots:99999999999999999999"}),
            json!({"op": "image", "src": 12345}),
            json!({"op": "svg", "d": 5, "vb": [0, 0, 24, 24]}),
            json!({"op": "svg", "d": "M0 0", "vb": [0, 0, 24.5, 1e-7]}),
        ];
        for op in ops {
            let mut canvas = Canvas::default();
            let _ = canvas.apply(op);
        }
    }

    /// Random ops with random types in every slot: whatever happens, it is an `Ok` or a message.
    #[test]
    fn random_ops_never_panic() {
        use crate::pyrandom::PyRandom;
        fn value(rng: &mut PyRandom, depth: u32, words: &[&str]) -> Value {
            let numbers = [
                0.0, 1.0, -1.0, 2.0, 3.0, 7.0, 10.0, 25.0, 50.0, 100.0, 300.0, 5000.0, -50.0, 0.5,
                1e12, -1e12, 1e30,
            ];
            match rng.randrange(9).unwrap() {
                0..=2 => json!(numbers[rng.randrange(numbers.len() as i64).unwrap() as usize]),
                3 => json!(rng.randrange(40).unwrap()),
                4 | 5 => json!(words[rng.randrange(words.len() as i64).unwrap() as usize]),
                6 => Value::Null,
                7 => json!(rng.randrange(2).unwrap() == 0),
                _ if depth > 0 => {
                    let count = rng.randrange(5).unwrap();
                    Value::Array((0..count).map(|_| value(rng, depth - 1, words)).collect())
                }
                _ => json!(1),
            }
        }
        let seed: i64 = std::env::var("FUZZ_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2024);
        let rounds: usize = std::env::var("FUZZ_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);
        let mut rng = PyRandom::from_int(seed);
        let names = [
            "text",
            "figure",
            "pattern",
            "frame",
            "shape",
            "svg",
            "pixels",
            "formula",
            "qr",
            "transform",
            "repeat",
            "scatter",
            "clear",
            "image",
            "logo",
            "poster",
            "nope",
        ];
        let keys = [
            "x",
            "y",
            "w",
            "h",
            "s",
            "name",
            "kind",
            "ink",
            "rot",
            "size",
            "pts",
            "fill",
            "lw",
            "r",
            "a0",
            "a1",
            "inset",
            "rows",
            "scale",
            "b64",
            "d",
            "vb",
            "f",
            "ecc",
            "deg",
            "dx",
            "dy",
            "n",
            "seed",
            "ops",
            "align",
            "valign",
            "wrap",
            "spacing",
            "font",
            "shape",
            "dither",
            "threshold",
            "invert",
            "src",
            "say",
            "evenodd",
        ];
        let words = [
            "star",
            "heart",
            "dots:4",
            "gray:30",
            "stripes",
            "line",
            "rect",
            "ellipse",
            "curve",
            "polygon",
            "pie",
            "rrect",
            "chord",
            "arc",
            "invert",
            "flip-h",
            "rotate",
            "mirror-h",
            "shift",
            "pixelate",
            "outline",
            "black",
            "white",
            "pixel",
            "sans",
            "x+y>w",
            "sin(x)>0",
            "M0 0L9 9",
            "hello world",
            "l",
            "r",
            "t",
            "b",
            "m",
            "c",
            "12",
            "inf",
            "nan",
            "",
            "AAAA",
            "emoji:x",
            "threshold",
            "atkinson",
            "q",
            "h",
        ];
        for round in 0..rounds {
            let mut op = Map::new();
            let name = names[rng.randrange(names.len() as i64).unwrap() as usize];
            op.insert("op".into(), json!(name));
            for _ in 0..rng.randrange(7).unwrap() {
                let key = keys[rng.randrange(keys.len() as i64).unwrap() as usize];
                let chosen = if key == "ops" && rng.randrange(2).unwrap() == 0 {
                    json!([{"op": "shape", "kind": "rect", "w": 5, "h": 5}])
                } else if key == "pts" && rng.randrange(2).unwrap() == 0 {
                    json!([[1, 2], [30, 40], [5, 90]])
                } else {
                    value(&mut rng, 2, &words)
                };
                op.insert(key.into(), chosen);
            }
            let mut canvas = Canvas::default();
            let start = std::time::Instant::now();
            let _ = canvas.apply(Value::Object(op.clone()));
            assert!(
                start.elapsed().as_secs() < 20,
                "round {round} took {:?}: {}",
                start.elapsed(),
                Value::Object(op)
            );
        }
    }

    #[test]
    fn qr_picks_micro_codes_like_segno() {
        assert_eq!(qr_matrix("1", "l").unwrap().len(), 13);
        assert_eq!(qr_matrix("hello", "m").unwrap().len(), 15);
        assert_eq!(qr_matrix("12345", "h").unwrap().len(), 21);
        assert_eq!(qr_matrix("1", "q").unwrap().len(), 17);
        assert_eq!(qr_matrix("https://example.com", "m").unwrap().len(), 25);
        assert_eq!(qr_matrix(&"a".repeat(100), "m").unwrap().len(), 41);
        assert_eq!(
            qr_matrix(&"a".repeat(8000), "m").unwrap_err(),
            "Data too large. No (Micro) QR Code can handle the provided data"
        );
    }

    /// Equal, except for what is not ours to match: Python integers beyond 2^40 are printed in full here only up
    /// to the clamp, and the image and SVG libraries word their own complaints.
    fn same_message(ours: &str, python: &str) -> bool {
        if ours == python {
            return true;
        }
        let long_numbers = regex::Regex::new(r"\d{13,}").unwrap();
        if long_numbers.replace_all(ours, "#") == long_numbers.replace_all(python, "#") {
            return true;
        }
        ["Couldn't read that image:", "Couldn't render that SVG:"]
            .iter()
            .any(|prefix| ours.starts_with(prefix) && python.starts_with(prefix))
    }

    const ERROR_CASES: &str = include_str!("../tests/fixtures/canvas_errors.json");

    /// Every message the Python engine gives for 200+ ops that go wrong (or oddly right).
    #[test]
    fn error_messages_are_what_python_says() {
        let cases: Vec<Value> = serde_json::from_str(ERROR_CASES).unwrap();
        let mut wrong = Vec::new();
        for case in &cases {
            let op = case["op"].clone();
            let mut canvas = Canvas::default();
            let outcome = canvas.apply(op.clone());
            if let Some(raw) = case["raw"].as_str() {
                // Python let this one escape as a plain exception; here it is an error or a drawing, never a panic.
                let _ = (outcome, raw);
            } else if let Some(expected) = case["error"].as_str() {
                match outcome {
                    Err(e) if same_message(&e.0, expected) => {}
                    Err(e) => wrong.push(format!("{op}\n    python: {expected}\n    rust:   {e}")),
                    Ok(()) => {
                        wrong.push(format!("{op}\n    python: {expected}\n    rust:   worked"))
                    }
                }
            } else if let Err(e) = outcome {
                wrong.push(format!("{op}\n    python: worked\n    rust:   {e}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    const QR_CASES: &str = include_str!("../tests/fixtures/qr_matrices.json");

    /// Every module of 260 codes (all four levels, Micro QR and versions up to 24) against `segno.make`.
    #[test]
    fn qr_codes_are_module_for_module_what_segno_makes() {
        let cases: Vec<Value> = serde_json::from_str(QR_CASES).unwrap();
        assert!(cases.len() > 200);
        let mut wrong = Vec::new();
        for case in &cases {
            let (text, level) = (case["s"].as_str().unwrap(), case["e"].as_str().unwrap());
            let mine = qr_matrix(text, level);
            if let Some(error) = case.get("err") {
                if mine.as_ref().err().map(String::as_str) != error.as_str() {
                    wrong.push(format!(
                        "{text:?} {level}: expected error {error}, got {:?}",
                        mine.err()
                    ));
                }
                continue;
            }
            let rows: Vec<String> = match &mine {
                Ok(matrix) => matrix
                    .iter()
                    .map(|row| row.iter().map(|c| if *c { '1' } else { '0' }).collect())
                    .collect(),
                Err(error) => {
                    wrong.push(format!("{text:?} {level}: {error}"));
                    continue;
                }
            };
            let hash = hex::encode(Sha256::digest(rows.join("\n")));
            if hash != case["sha256"].as_str().unwrap() || case["side"] != rows.len() {
                wrong.push(format!(
                    "{:?} {level} {}",
                    text.chars().take(16).collect::<String>(),
                    case["designator"]
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn catalog_lists_names() {
        let everything = catalog(None).unwrap();
        let mut keys: Vec<&String> = everything.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["dithers", "figures", "fonts", "frames", "looks", "ops", "patterns"]
        );
        assert!(everything["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "gray:N"));
        assert_eq!(
            everything["dithers"],
            json!(["atkinson", "floyd", "bayer", "halftone", "threshold"])
        );
        let one = catalog(Some("ops")).unwrap();
        assert_eq!(one["ops"].as_array().unwrap().len(), 16);
        assert!(catalog(Some("system_fonts"))
            .unwrap()
            .get("system_fonts")
            .is_some());
        assert_eq!(
            catalog(Some("nope")).unwrap_err().0,
            "Unknown catalog 'nope'. Use one of figures, patterns, frames, fonts, looks, dithers, ops, system_fonts."
        );
        assert!(catalog(Some("")).unwrap().get("figures").is_some());
    }

    const CASES: &str = include_str!("../tests/fixtures/canvas_cases.json");

    fn packed_hash(canvas: &Canvas) -> String {
        let packed = b64().decode(canvas.bitmap().unwrap()).unwrap();
        hex::encode(Sha256::digest(packed))
    }

    #[test]
    fn matches_the_python_engine() {
        let cases: Vec<Value> = serde_json::from_str(CASES).unwrap();
        let mut wrong: Vec<String> = Vec::new();
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let kind = case["kind"].as_str().unwrap();
            let ops = case["ops"].as_array().unwrap();
            let mut canvas = Canvas::default();
            if let Err(e) = canvas.apply_all(ops) {
                wrong.push(format!("{name}: {e}"));
                continue;
            }
            let expected_black = case["black"].as_u64().unwrap() as f64;
            let ink = black(&canvas) as f64;
            match kind {
                "exact" => {
                    if packed_hash(&canvas) != case["sha256"].as_str().unwrap() {
                        wrong.push(format!(
                            "{name}: bitmap differs ({ink} black, Python {expected_black})"
                        ));
                    }
                }
                "text" => {
                    let (x0, y0, x1, y1) = ink_box(&canvas);
                    let want: Vec<i64> = case["bbox"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_i64().unwrap())
                        .collect();
                    let got = [x0 as i64, y0 as i64, x1 as i64, y1 as i64];
                    let off = got
                        .iter()
                        .zip(&want)
                        .map(|(a, b)| (a - b).abs())
                        .max()
                        .unwrap();
                    let share = (ink - expected_black).abs() / expected_black.max(1.0);
                    if off > 3 || share > 0.30 {
                        wrong.push(format!(
                            "{name}: ink box {got:?} vs Python {want:?}, ink {ink} vs {expected_black}"
                        ));
                    }
                }
                _ => {
                    let share = (ink - expected_black).abs() / expected_black.max(1.0);
                    if share > 0.35 {
                        wrong.push(format!("{name}: ink {ink} vs Python {expected_black}"));
                    }
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} cases differ:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    #[test]
    #[ignore]
    fn dump_cases() {
        let Ok(dir) = std::env::var("CANVAS_COMPARE_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cases: Vec<Value> = serde_json::from_str(CASES).unwrap();
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let mut canvas = Canvas::default();
            let _ = canvas.apply_all(case["ops"].as_array().unwrap());
            canvas.image.save(dir.join(format!("{name}.png"))).unwrap();
            std::fs::write(
                dir.join(format!("{name}.bits")),
                b64().decode(canvas.bitmap().unwrap()).unwrap(),
            )
            .unwrap();
        }
    }
}
