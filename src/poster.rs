//! Posters: a small model says what a note means, and this module does the designing.
//!
//! Port of `src/prins/design/poster.py`. A model drawing by coordinates, at low effort, with no look at
//! its result, produces a frame, an icon and some text. So the model only fills in meaning (`say`, `sub`,
//! `big`, `pic`, `data`, `items`) and may name a `look`. Every look takes the same fields, solves its own
//! layout for whatever text arrives, and never loses any of it. If a look can't hold the text legibly it
//! hands over to one that can, and the report says so.
//!
//! A look is a way of designing, not a template: type is sized from the words themselves, pictures come
//! from any subject, and the seed varies composition and detail.

use std::sync::OnceLock;

use image::GrayImage;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::assets::py_repr;
use crate::craft::{
    self, atkinson, bayer, caption, collapse, fdiv, paste, py_split, round_i, smallest, stack,
    tone, tone_page, word, xor, Dim, H, W,
};
use crate::draw::{self, Coord};
use crate::pyrandom::PyRandom;
use crate::raster::{self, Resample};

/// The label maker asks for at most 60% black, for the sake of the print head.
pub const MAX_INK: f64 = 0.59;
/// Capital height of 16 px text, the smallest that prints legibly.
pub const MIN_CAP: i32 = 11;
const MAX_TEXT: usize = 400;

/// This look can't hold this content legibly. The next best look gets to try.
#[derive(Debug)]
struct NoFit;

type Res<T = ()> = Result<T, NoFit>;

// --- the spec -----------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub look: String,
    pub say: String,
    /// Line breaks, when the model chose them.
    pub lines: Option<Vec<String>>,
    pub sub: String,
    pub big: String,
    pub pic: String,
    pub data: Vec<f64>,
    pub items: Vec<String>,
    pub qr: String,
    pub seed: i128,
    /// From the connector or the device's owner. Never from the model.
    pub sender: String,
    pub date: String,
}

/// `json.dumps(value, ensure_ascii=False)` for a string.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Spec {
    /// Seeded from the words and the seed, as `random.Random(sha256(json).hexdigest())`.
    pub fn rng(&self) -> PyRandom {
        let key = format!(
            "[{}, {}, {}, {}]",
            json_string(&self.say),
            json_string(&self.sub),
            json_string(&self.big),
            self.seed
        );
        let digest = Sha256::digest(key.as_bytes());
        PyRandom::from_str(&hex::encode(digest))
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub look: String,
    pub alternatives: Vec<String>,
    pub warnings: Vec<String>,
    pub shown: Vec<String>,
    pub min_cap: i32,
}

impl Report {
    pub fn new(look: &str) -> Report {
        Report {
            look: look.to_string(),
            alternatives: Vec::new(),
            warnings: Vec::new(),
            shown: Vec::new(),
            min_cap: 999,
        }
    }

    pub fn summary(&self) -> String {
        let mut text = format!("Drew look {}.", self.look);
        if !self.alternatives.is_empty() {
            text.push_str(&format!(" Also fits: {}.", self.alternatives.join(", ")));
        }
        let mut parts = vec![text];
        parts.extend(self.warnings.iter().cloned());
        parts.join(" ")
    }
}

// --- the page -----------------------------------------------------------------------------------

/// The canvas, plus a record of what text went on it and how small.
struct Page {
    image: GrayImage,
    report: Report,
}

fn rect<C: Coord>(img: &mut GrayImage, b: [C; 4], fill: u8) {
    draw::rectangle(img, b, Some(fill), None, 0);
}

fn outlined_rect<C: Coord>(img: &mut GrayImage, b: [C; 4], width: u32) {
    draw::rectangle(img, b, None, Some(0), width);
}

fn disc<C: Coord>(img: &mut GrayImage, b: [C; 4], fill: u8) {
    draw::ellipse(img, b, Some(fill), None, 0);
}

fn ring<C: Coord>(img: &mut GrayImage, b: [C; 4], fill: u8, outline: u8, width: u32) {
    draw::ellipse(img, b, Some(fill), Some(outline), width);
}

fn stroke<C: Coord>(img: &mut GrayImage, points: &[(C, C)], fill: u8, width: u32) {
    draw::line(img, points, fill, width, false);
}

impl Page {
    fn new(report: Report) -> Page {
        Page {
            image: raster::paper(W as u32, H as u32),
            report,
        }
    }

    fn text(
        &mut self,
        mask: &GrayImage,
        x: impl Into<f64>,
        y: impl Into<f64>,
        source: &str,
        ink: u8,
    ) {
        paste(&mut self.image, mask, x.into(), y.into(), ink);
        self.mark(source, mask.h());
    }

    fn mark(&mut self, source: &str, height: i32) {
        self.report.shown.push(source.to_string());
        self.report.min_cap = self.report.min_cap.min(height);
    }

    /// Sets lines full width. Returns the y just past the last one.
    #[allow(clippy::too_many_arguments)]
    fn stack(
        &mut self,
        lines: &[String],
        face: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        gap: i32,
        ink: u8,
        center: bool,
        stretch: f64,
    ) -> Res<i32> {
        let masks = stack(lines, face, width, height, gap, stretch);
        if smallest(&masks) < MIN_CAP {
            return Err(NoFit);
        }
        let total = masks.iter().map(|m| m.h()).sum::<i32>() + gap * (masks.len() as i32 - 1);
        let mut top = y + if center { fdiv(height - total, 2) } else { 0 };
        for (line, mask) in lines.iter().zip(&masks) {
            self.text(mask, x + fdiv(width - mask.w(), 2), top, line, ink);
            top += mask.h() + gap;
        }
        Ok(top - gap)
    }

    /// One line, as large as fits the box. Returns its height. Too small to read gives `NoFit`.
    #[allow(clippy::too_many_arguments)]
    fn fit_line(
        &mut self,
        text: &str,
        face: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        ink: u8,
        align: char,
    ) -> Res<i32> {
        let ratio = craft::aspect(text, face);
        let h = height.min(round_i(width as f64 / ratio.max(0.05)));
        if h < MIN_CAP {
            return Err(NoFit);
        }
        let mask = word(
            text,
            face,
            Some(width.min(round_i(h as f64 * ratio))),
            Some(h),
            false,
        );
        let left = match align {
            'c' => x + fdiv(width - mask.w(), 2),
            'r' => x + width - mask.w(),
            _ => x,
        };
        self.text(&mask, left, y, text, ink);
        Ok(mask.h())
    }

    fn line(
        &mut self,
        text: &str,
        face: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Res<i32> {
        self.fit_line(text, face, x, y, width, height, 0, 'l')
    }

    /// Words filling a box left aligned at a size that fits, wrapping as needed. Returns the bottom.
    #[allow(clippy::too_many_arguments)]
    fn block_text(
        &mut self,
        text: &str,
        face: &str,
        x: i32,
        mut y: i32,
        width: i32,
        height: i32,
        ink: u8,
        gap: i32,
    ) -> Res<i32> {
        if text.is_empty() {
            return Ok(y);
        }
        let mut chosen = None;
        for size in [26, 22, 18, craft::MIN_TEXT_PX] {
            let lines = craft::wrap_caption(text, width, face, size);
            let total = lines.iter().map(|m| m.h()).sum::<i32>() + gap * (lines.len() as i32 - 1);
            if total <= height {
                chosen = Some(lines);
                break;
            }
        }
        let Some(lines) = chosen else {
            return Err(NoFit);
        };
        for line in &lines {
            self.text(line, x, y, text, ink);
            y += line.h() + gap;
        }
        Ok(y - gap)
    }
}

// --- reading the fields -------------------------------------------------------------------------

/// Python's `repr()` of a float.
fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf" } else { "inf" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let fixed = format!("{f}");
        if fixed.contains('.') {
            fixed
        } else {
            format!("{fixed}.0")
        }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// Python's `str(value)` for a JSON value.
fn py_str(value: &Value) -> String {
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
                py_float_repr(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_item_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(k), py_item_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// `repr()` of a JSON value (strings quoted).
fn py_item_repr(value: &Value) -> String {
    match value {
        Value::String(s) => py_repr(s),
        other => py_str(other),
    }
}

/// `_string`: whitespace collapsed and cut to `MAX_TEXT` characters.
fn string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) if s.is_empty() => String::new(),
        Some(v) => collapse(&py_str(v)).chars().take(MAX_TEXT).collect(),
    }
}

fn truncate(text: String, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Python truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `float(value)` for the values a model might send.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(*b as u8 as f64),
        Value::String(s) => {
            let t = s.trim_matches(craft::py_isspace);
            let cleaned;
            let t = if t.contains('_') {
                let ok =
                    t.split('_').all(|p| !p.is_empty()) && !t.starts_with('_') && !t.ends_with('_');
                if !ok {
                    return None;
                }
                cleaned = t.replace('_', "");
                cleaned.as_str()
            } else {
                t
            };
            t.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// `int(value)` for a seed; anything odd is 0.
fn py_seed(value: Option<&Value>) -> i128 {
    if !truthy(value) {
        return 0;
    }
    match value {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i as i128
            } else if let Some(u) = n.as_u64() {
                u as i128
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_finite() {
                    f.trunc() as i128
                } else {
                    0
                }
            }
        }
        Some(Value::Bool(true)) => 1,
        Some(Value::String(s)) => {
            let t = s.trim_matches(craft::py_isspace);
            let (sign, digits) = match t.strip_prefix('-') {
                Some(rest) => (-1, rest),
                None => (1, t.strip_prefix('+').unwrap_or(t)),
            };
            let ok = !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
                && !digits.starts_with('_')
                && !digits.ends_with('_')
                && !digits.contains("__");
            if !ok {
                return 0;
            }
            digits
                .replace('_', "")
                .parse::<i128>()
                .map(|v| v * sign)
                .unwrap_or(0)
        }
        _ => 0,
    }
}

/// Forgiving by design: a small model's odd arguments still make a note. Sender is not read here.
pub fn parse(op: &Value) -> Spec {
    let get = |key: &str| op.get(key);
    let raw_say = match get("say") {
        Some(v) => Some(v),
        None => get("s"),
    };
    let mut lines = None;
    let say;
    if let Some(Value::Array(raw)) = raw_say {
        let found: Vec<String> = raw
            .iter()
            .map(|line| string(Some(line)))
            .filter(|line| !line.is_empty())
            .take(8)
            .collect();
        say = found.join(" ");
        lines = Some(found);
    } else {
        say = string(raw_say);
    }
    let mut data = Vec::new();
    if let Some(Value::Array(values)) = get("data") {
        for value in values.iter().take(64) {
            if let Some(number) = py_float(value) {
                if number.is_finite() {
                    data.push(number);
                }
            }
        }
    }
    let items: Vec<String> = match get("items") {
        Some(Value::Array(raw)) => raw
            .iter()
            .map(|item| string(Some(item)))
            .filter(|item| !item.is_empty())
            .take(12)
            .collect(),
        _ => Vec::new(),
    };
    let qr = if truthy(get("qr")) {
        truncate(py_str(get("qr").unwrap_or(&Value::Null)), 300)
    } else {
        String::new()
    };
    Spec {
        look: string(get("look")).to_lowercase(),
        say,
        lines,
        sub: string(get("sub")),
        big: truncate(string(get("big")), 12),
        pic: truncate(string(get("pic")), 40),
        data,
        items,
        qr,
        seed: py_seed(get("seed")),
        sender: String::new(),
        date: string(get("date")),
    }
}

macro_rules! re {
    ($pattern:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        CELL.get_or_init(|| Regex::new($pattern).expect("static pattern"))
    }};
}

fn emoji_pattern() -> &'static Regex {
    re!("[\\x{1F000}-\\x{1FAFF}\\x{2600}-\\x{27BF}\\x{2B00}-\\x{2BFF}\\x{FE0F}\\x{200D}]+")
}

fn move_emoji(spec: &mut Spec) {
    let pattern = emoji_pattern();
    let both = format!("{} {}", spec.say, spec.sub);
    let Some(first) = pattern.find(&both) else {
        return;
    };
    if spec.pic.is_empty() {
        spec.pic = first.as_str().to_string();
    }
    spec.say = collapse(&pattern.replace_all(&spec.say, " "));
    spec.sub = collapse(&pattern.replace_all(&spec.sub, " "));
    spec.lines = None;
}

fn number_pattern() -> &'static Regex {
    re!(r"\d{1,2}[:.]\d{2}|\d+(?:[.,]\d+)?%?")
}

fn time_search() -> &'static Regex {
    re!(r"\b\d{1,2}[:.]\d{2}\b")
}

fn time_full(text: &str) -> bool {
    re!(r"\A(?:\b\d{1,2}[:.]\d{2}\b)\z").is_match(text)
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// The number the note is about. Takes it out of `say` so it isn't printed twice.
fn pull_number(spec: &mut Spec) -> String {
    if !spec.big.is_empty() {
        return spec.big.clone();
    }
    let Some(found) = number_pattern().find(&spec.say) else {
        return String::new();
    };
    if chars(found.as_str()) > 6 {
        return String::new();
    }
    let big = found.as_str().to_string();
    spec.say = collapse(&format!(
        "{} {}",
        &spec.say[..found.start()],
        &spec.say[found.end()..]
    ));
    spec.lines = None;
    big
}

/// Python's `str.isupper`.
fn py_isupper(text: &str) -> bool {
    let cased = text.chars().any(|c| c.is_uppercase() || c.is_lowercase());
    cased && !text.chars().any(|c| c.is_lowercase())
}

fn casefold(text: &str) -> String {
    text.to_lowercase().replace('ß', "ss").replace('ς', "σ")
}

fn lines_for(
    spec: &Spec,
    text: &str,
    face: &str,
    width: i32,
    height: i32,
    gap: i32,
    most: usize,
) -> Vec<String> {
    if let Some(lines) = &spec.lines {
        if !lines.is_empty()
            && lines.join(" ") == spec.say
            && text.to_uppercase() == spec.say.to_uppercase()
        {
            let upper = py_isupper(text);
            return lines
                .iter()
                .map(|l| if upper { l.to_uppercase() } else { l.clone() })
                .collect();
        }
    }
    craft::break_lines(text, face, width, height, gap, most)
}

/// Display faces read best in capitals. `upper` also gets ß and Ø right.
fn display(spec: &Spec) -> Res<String> {
    let text = if spec.say.is_empty() {
        &spec.big
    } else {
        &spec.say
    };
    if text.is_empty() {
        return Err(NoFit);
    }
    Ok(text.to_uppercase())
}

fn hard(mask: &GrayImage) -> GrayImage {
    raster::threshold(mask, 128)
}

fn picture(spec: &Spec, width: i32, height: i32, default: &str) -> Option<(GrayImage, GrayImage)> {
    let name = if spec.pic.is_empty() {
        default
    } else {
        &spec.pic
    };
    let (gray, mask) = craft::resolve_picture(name, width.max(height) * 2)?;
    Some((
        craft::fit(&gray, width, height),
        hard(&craft::fit(&mask, width, height)),
    ))
}

/// The small line. Set as large as it fits, wrapping when it needs to.
fn footer(page: &mut Page, spec: &Spec, x: i32, y: i32, width: i32, height: i32, ink: u8) -> Res {
    if spec.sub.is_empty() {
        return Ok(());
    }
    let ratio = craft::aspect(&spec.sub, "din");
    if height >= MIN_CAP && ratio * height.min(26) as f64 <= width as f64 * 1.35 {
        let h = height.min(26).min(round_i(width as f64 / ratio.max(0.05)));
        if h >= MIN_CAP {
            let mask = word(
                &spec.sub,
                "din",
                Some(width.min(round_i(h as f64 * ratio))),
                Some(h),
                false,
            );
            page.text(&mask, x, y + fdiv(height - mask.h(), 2), &spec.sub, ink);
            return Ok(());
        }
    }
    page.block_text(&spec.sub, "din", x, y, width, height, ink, 5)?;
    Ok(())
}

// --- random helpers -----------------------------------------------------------------------------

fn randrange(rng: &mut PyRandom, a: i64, b: i64) -> i32 {
    rng.randrange2(a, b).unwrap_or(a) as i32
}

fn randint(rng: &mut PyRandom, a: i64, b: i64) -> i32 {
    rng.randint(a, b).unwrap_or(a) as i32
}

// --- the looks ----------------------------------------------------------------------------------

/// Every line set to the full measure, with one reversed out of a black band.
fn block_look(page: &mut Page, spec: &mut Spec, alarm: bool) -> Res {
    let mut rng = spec.rng();
    let margin = 8;
    let mut top = 0;
    if alarm {
        let hazard = tone(|x, y| if (x + y) % 28 < 14 { 1.0 } else { 0.0 }, W, 14);
        raster::paste(&mut page.image, &hazard, 0, 0, None);
        rect(&mut page.image, [0, H - 6, W, H], 0);
        top = 20;
    }
    let mut bottom = H - if alarm { 10 } else { 8 };
    if !spec.sub.is_empty() {
        let mut sub_h = 25;
        let ratio = craft::aspect(&spec.sub, "din");
        if ratio * sub_h as f64 > (W - margin * 2) as f64
            && ratio * MIN_CAP as f64 > (W - margin * 2) as f64
        {
            sub_h = 46; // A long line wraps to two.
        }
        footer(page, spec, margin, bottom - sub_h, W - margin * 2, sub_h, 0)?;
        bottom -= sub_h + 8;
    }
    let area = bottom - top;
    let text = display(spec)?;
    let lines = lines_for(spec, &text, "impact", W - margin * 2, area - 12, 12, 5);
    let n = lines.len() as i32;
    let masks = stack(&lines, "impact", W - margin * 2, area - 12 * n, 12, 1.7);
    if smallest(&masks) < MIN_CAP * 2 && chars(&text) > 40 {
        return Err(NoFit);
    }
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let reverse = if n < 3 {
        n - 1
    } else {
        randrange(&mut rng, 1, n as i64)
    };
    let heights: i32 = masks.iter().map(|m| m.h()).sum();
    let room = area - heights;
    let gap = 6.max(16.min(fdiv(room, n)));
    let mut y = top + fdiv(area - heights - gap * (n - 1), 2);
    if n == 1 {
        // One line is all band, up to the height that keeps the ink safe.
        let band = area.min(masks[0].h() + 24).min(136);
        y = top + fdiv(area - band, 2);
        rect(&mut page.image, [0, y, W, y + band - 1], 0);
        y += fdiv(band - masks[0].h(), 2);
    }
    for (index, (line, mask)) in lines.iter().zip(&masks).enumerate() {
        let reversed_line = n == 1 || index as i32 == reverse;
        if reversed_line && n > 1 {
            rect(
                &mut page.image,
                [0, y - fdiv(gap, 2), W, y + mask.h() + fdiv(gap, 2) - 1],
                0,
            );
        }
        page.text(
            mask,
            fdiv(W - mask.w(), 2),
            y,
            line,
            if reversed_line { 255 } else { 0 },
        );
        y += mask.h() + gap;
    }
    Ok(())
}

fn look_block(page: &mut Page, spec: &mut Spec) -> Res {
    block_look(page, spec, false)
}

fn look_alarm(page: &mut Page, spec: &mut Spec) -> Res {
    block_look(page, spec, true)
}

/// A word lifts out of ruled lines. Nothing draws it: only a height map knows it is there.
fn look_relief(page: &mut Page, spec: &mut Spec) -> Res {
    let text = display(spec)?;
    if chars(&text) > 16 {
        return Err(NoFit);
    }
    let strip = if spec.sub.is_empty() { 0 } else { 34 };
    let area = H - strip - 36;
    let lines = lines_for(spec, &text, "impact", 284, area, 8, 2);
    if lines.len() > 2 {
        return Err(NoFit);
    }
    let mut source = raster::blank(W as u32, H as u32);
    let masks = stack(&lines, "impact", 284, area, 8, 1.9);
    if smallest(&masks) < 28 {
        // Stripes eat fine detail, so this look only suits big type.
        return Err(NoFit);
    }
    let total = masks.iter().map(|m| m.h()).sum::<i32>() + 8 * (masks.len() as i32 - 1);
    let mut y = 22 + fdiv(area - total, 2);
    for (line, mask) in lines.iter().zip(&masks) {
        raster::paste(&mut source, mask, fdiv(W - mask.w(), 2), y, None);
        page.mark(line, mask.h());
        y += mask.h() + 8;
    }
    let height = raster::gaussian_blur(&source, 1.3);
    let sample_at = |x: i32, row: i32| height.get_pixel(x as u32, row as u32).0[0];
    page.image = raster::blank(W as u32, H as u32);
    let image = &mut page.image;
    let mut row = 4;
    while row < H + 16 {
        let sample = (H - 1).min(row);
        let points: Vec<(f64, f64)> = (-1..W + 2)
            .map(|x| {
                let px = sample_at((W - 1).min(0.max(x)), sample);
                (x as f64, row as f64 - px as f64 / 255.0 * 11.0)
            })
            .collect();
        let mut poly = points.clone();
        poly.push(((W + 2) as f64, (H + 20) as f64));
        poly.push((-1.0, (H + 20) as f64));
        draw::polygon(image, &poly, Some(0), None, 0);
        for x in 0..W {
            let width = if sample_at(x, sample) > 120 { 3 } else { 1 };
            stroke(
                image,
                &[points[(x + 1) as usize], points[(x + 2) as usize]],
                255,
                width,
            );
        }
        row += 4;
    }
    if strip != 0 {
        rect(image, [0, H - strip, W, H], 255);
        footer(page, spec, 12, H - strip + 6, W - 24, strip - 12, 0)?;
    }
    Ok(())
}

/// Solid type receding to a vanishing point, under rays. Depth is free once the engine owns it.
fn look_monument(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    if !spec.big.is_empty() && !casefold(&spec.say).contains(&casefold(&spec.big)) {
        spec.say = format!("{} {}", spec.big, spec.say)
            .trim_matches(craft::py_isspace)
            .to_string();
        spec.big = String::new();
        spec.lines = None;
    }
    let text = display(spec)?;
    if chars(&text) > 26 {
        return Err(NoFit);
    }
    let vp = (randrange(&mut rng, 120, 192) as f64, 18.0f64);
    let base = bayer(&tone_page(|x, y| {
        let angle = (y as f64 - vp.1).atan2(x as f64 - vp.0) * 28.0 / std::f64::consts::PI;
        if craft::pymod(angle, 2.0) < 1.0 {
            0.7
        } else {
            1.0
        }
    }));
    raster::paste(&mut page.image, &base, 0, 0, None);
    let area = 112;
    let lines = lines_for(spec, &text, "impact", 292, area, 6, 2);
    let masks = stack(&lines, "impact", 292, area, 6, 1.7);
    if smallest(&masks) < 30 {
        return Err(NoFit);
    }
    let mut face = raster::blank(W as u32, H as u32);
    let mut y = H - 8 - masks.iter().map(|m| m.h()).sum::<i32>() - 6 * (masks.len() as i32 - 1);
    for (line, mask) in lines.iter().zip(&masks) {
        raster::paste(&mut face, mask, fdiv(W - mask.w(), 2), y, None);
        page.mark(line, mask.h());
        y += mask.h() + 6;
    }
    for depth in (1..=62).rev() {
        let scale = 1.0 - depth as f64 * 0.0098;
        let (sw, sh) = (
            round_i(W as f64 * scale).max(1),
            round_i(H as f64 * scale).max(1),
        );
        let shrunk = raster::resize(&face, sw as u32, sh as u32, Resample::Nearest);
        let mut layer = raster::blank(W as u32, H as u32);
        raster::paste(
            &mut layer,
            &shrunk,
            round_i(vp.0 - vp.0 * scale),
            round_i(vp.1 - vp.1 * scale),
            None,
        );
        raster::paste_value(
            &mut page.image,
            if depth % 20 != 0 { 0 } else { 255 },
            0,
            0,
            &layer,
        );
    }
    raster::paste_value(&mut page.image, 0, 0, 0, &raster::max_filter(&face, 5));
    raster::paste_value(&mut page.image, 255, 0, 0, &face);
    if !spec.sub.is_empty() {
        let lines_sub = craft::wrap_caption(&spec.sub, 280, "din", 18);
        let height = lines_sub.iter().map(|m| m.h() + 4).sum::<i32>() + 8;
        rect(&mut page.image, [0, 0, W, height], 255);
        let mut y = 6;
        for mask in &lines_sub {
            page.text(mask, fdiv(W - mask.w(), 2), y, &spec.sub, 0);
            y += mask.h() + 4;
        }
    }
    Ok(())
}

const RANSOM_FACES: [&str; 10] = [
    "impact",
    "didot",
    "rockwell",
    "courier",
    "futura",
    "typewriter",
    "black",
    "bodoni",
    "krungthep",
    "georgia",
];

/// Every letter cut from a different page.
fn look_ransom(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let text = display(spec)?;
    if chars(&text) > 28 {
        return Err(NoFit);
    }
    let paper = bayer(&tone_page(|_, _| 0.93));
    raster::paste(&mut page.image, &paper, 0, 0, None);
    let strip = if spec.sub.is_empty() { 0 } else { 28 };
    let available = H - strip - 10;
    let words: Vec<String> = py_split(&text).iter().map(|w| w.to_string()).collect();
    let mut best: Vec<String> = Vec::new();
    for rows in 1..=4usize {
        if rows > words.len() {
            break;
        }
        let candidate = even_rows(&words, rows);
        if best.is_empty() || tile_size(&candidate, available) > tile_size(&best, available) {
            best = candidate;
        }
    }
    let size = tile_size(&best, available);
    if size < 26 {
        return Err(NoFit);
    }
    let mut faces: Vec<&str> = RANSOM_FACES.to_vec();
    rng.shuffle(&mut faces);
    let n_rows = best.len() as i32;
    let total = n_rows * size + (n_rows - 1) * 4;
    let mut y = 5 + fdiv(available - total, 2);
    let mut index = 0usize;
    for row in &best {
        let letters: Vec<char> = row.chars().collect();
        let mut widths: Vec<i32> = letters
            .iter()
            .map(|&c| {
                if c != ' ' {
                    round_i(size as f64 * rng.uniform(0.84, 1.02))
                } else {
                    fdiv(size, 3)
                }
            })
            .collect();
        let count = letters.len() as i32;
        let fitted = 1.0f64
            .min((W - 12 - (count - 1) * 3) as f64 / widths.iter().sum::<i32>().max(1) as f64);
        widths = widths
            .iter()
            .map(|&w| 10.max((w as f64 * fitted) as i32))
            .collect();
        let mut x = fdiv(W - widths.iter().sum::<i32>() - 3 * (count - 1), 2);
        for (&c, &width) in letters.iter().zip(&widths) {
            if c != ' ' {
                let jitter = randint(&mut rng, -2, 2);
                tile(
                    page,
                    c,
                    faces[index % faces.len()],
                    x,
                    y + jitter,
                    width,
                    size,
                    &mut rng,
                );
                index += 1;
            }
            x += width + 3;
        }
        y += size + 4;
        page.mark(row, fdiv(size, 2));
    }
    if strip != 0 {
        rect(&mut page.image, [0, H - strip, W, H], 255);
        footer(page, spec, 10, H - strip + 5, W - 20, strip - 10, 0)?;
    }
    Ok(())
}

/// Greedy rows whose lengths are as close as possible.
fn even_rows(words: &[String], rows: usize) -> Vec<String> {
    let target = words.iter().map(|w| chars(w)).sum::<usize>() as f64 / rows as f64;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, item) in words.iter().enumerate() {
        let remaining_rows = rows as i64 - out.len() as i64 - 1;
        let words_left = (words.len() - i) as i64;
        if !current.is_empty()
            && ((chars(&current) + 1 + chars(item)) as f64 > target * 1.25
                || words_left <= remaining_rows)
            && remaining_rows > 0
        {
            out.push(std::mem::take(&mut current));
            current = item.clone();
        } else {
            current = format!("{current} {item}")
                .trim_matches(craft::py_isspace)
                .to_string();
        }
    }
    out.push(current);
    out
}

fn tile_size(rows: &[String], available: i32) -> i32 {
    let n = rows.len() as i32;
    let by_height = fdiv(available - (n - 1) * 4, n);
    let by_width = rows
        .iter()
        .map(|r| {
            let letters: Vec<char> = r.chars().collect();
            let sum: f64 = letters
                .iter()
                .map(|&c| if c != ' ' { 0.93 } else { 0.33 })
                .sum();
            (W - 12 - (letters.len() as i32 - 1) * 3) as f64 / sum.max(0.3)
        })
        .fold(f64::INFINITY, f64::min);
    (by_height as f64).min(by_width).min(76.0) as i32
}

#[allow(clippy::too_many_arguments)]
fn tile(page: &mut Page, ch: char, face: &str, x: i32, y: i32, w: i32, h: i32, rng: &mut PyRandom) {
    let styles = ["black", "white", "white", "black", "gray"];
    let style = rng.choice(&styles).copied().unwrap_or("white");
    let mut tile = raster::filled(w as u32, h as u32, if style == "black" { 0 } else { 255 });
    if style == "gray" {
        tile = bayer(&tone(|_, _| 0.62, w, h));
    }
    draw::rectangle(&mut tile, [0, 0, w - 1, h - 1], None, Some(0), 1);
    let letter = ch.to_string();
    let target = 8.max((h as f64 * rng.uniform(0.62, 0.78)) as i32);
    let mut glyph = word(&letter, face, None, Some(target), false);
    if glyph.w() > w - 8 {
        glyph = word(&letter, face, Some(4.max(w - 8)), None, false);
    }
    if style == "gray" {
        rect(&mut tile, [4, 4, w - 5, h - 5], 255);
    }
    raster::paste_value(
        &mut tile,
        if style == "black" { 255 } else { 0 },
        fdiv(w - glyph.w(), 2),
        fdiv(h - glyph.h(), 2),
        &glyph,
    );
    let angle = rng.uniform(-7.0, 7.0);
    let pad = 12;
    let mut big = raster::paper((w + pad * 2) as u32, (h + pad * 2) as u32);
    let mut alpha = raster::blank(big.width(), big.height());
    raster::paste(&mut big, &tile, pad, pad, None);
    rect(&mut alpha, [pad, pad, pad + w - 1, pad + h - 1], 255);
    let turned = raster::rotate(&big, angle, None, 0, Resample::Nearest);
    let turned_alpha = raster::rotate(&alpha, angle, None, 0, Resample::Nearest);
    raster::paste_value(&mut page.image, 0, x + 3 - pad, y + 3 - pad, &turned_alpha);
    raster::paste(
        &mut page.image,
        &turned,
        x - pad,
        y - pad,
        Some(&turned_alpha),
    );
}

/// One number, set absurdly large in a high-contrast face, printed through a halftone screen.
fn look_numeral(page: &mut Page, spec: &mut Spec) -> Res {
    let big = pull_number(spec);
    if big.is_empty() {
        return Err(NoFit);
    }
    let screen = craft::halftone(
        &tone_page(|_, y| 1.0 - (0.42 + 0.72 * 1.0f64.min(y as f64 / 210.0))),
        5.0,
        45.0,
    );
    let words = spec.say.clone();
    if chars(&big) <= 2 {
        let ratio = craft::aspect(&big, "bodoni");
        let mut height = if chars(&big) == 1 { 236 } else { 200 };
        let width = round_i(height as f64 * ratio).min(186);
        height = round_i(width as f64 / ratio);
        let mask = word(&big, "bodoni", Some(width), Some(height), false);
        craft::fill_mask(
            &mut page.image,
            &screen,
            &mask,
            if chars(&big) == 2 { 2.0 } else { 4.0 },
            fdiv(H - height, 2) as f64,
        );
        page.mark(&big, height);
        let left = width + 16;
        let room = W - left - 10;
        let foot = if spec.sub.is_empty() { 0 } else { 26 };
        if !words.is_empty() {
            let lines = lines_for(spec, &words, "didot-italic", room, H - 40 - foot, 12, 4);
            let mut masks: Vec<GrayImage> = lines
                .iter()
                .map(|line| word(line, "didot-italic", Some(room), None, true))
                .collect();
            let n = masks.len() as i32;
            if smallest(&masks) < MIN_CAP
                || masks.iter().map(|m| m.h()).sum::<i32>() + 12 * n > H - 24 - foot
            {
                masks = stack(&lines, "didot-italic", room, H - 40 - foot, 12, 1.6);
                if smallest(&masks) < MIN_CAP {
                    return Err(NoFit);
                }
            }
            let mut y = 20;
            for (i, (line, m)) in lines.iter().zip(&masks).enumerate() {
                page.text(m, left, y, line, 0);
                y += m.h() + 6;
                if i + 1 < lines.len() {
                    rect(&mut page.image, [left, y, W - 10, y + 1], 0);
                    y += 8;
                }
            }
        }
        if !spec.sub.is_empty() {
            footer(page, spec, left, H - foot - 8, room, foot, 0)?;
        }
    } else {
        let tall = if !words.is_empty() || !spec.sub.is_empty() {
            112
        } else {
            196
        };
        let mask = word(&big, "bodoni", Some(296), Some(tall), false);
        craft::fill_mask(&mut page.image, &screen, &mask, 8.0, 8.0);
        page.mark(&big, mask.h());
        let y = 8 + mask.h() + 8;
        let room = H - y - 8;
        let foot = if spec.sub.is_empty() { 0 } else { 24 };
        if !words.is_empty() {
            page.fit_line(
                &words,
                "didot-italic",
                8,
                y,
                296,
                room - foot - if foot != 0 { 4 } else { 0 },
                0,
                'l',
            )?;
        }
        if !spec.sub.is_empty() {
            footer(page, spec, 8, H - foot - 6, 296, foot, 0)?;
        }
    }
    Ok(())
}

/// Two shapes and a numeral, overlapped with XOR. The overlap does the design.
fn look_collide(page: &mut Page, spec: &mut Spec) -> Res {
    let mut big = pull_number(spec);
    if big.is_empty() {
        return Err(NoFit);
    }
    let mut tail = String::new();
    if let Some(m) = re!(r"\A(\d{1,2})([:.]\d{2})\z").captures(&big) {
        let (head, rest) = (m[1].to_string(), m[2].to_string());
        big = head;
        tail = rest;
    }
    disc(&mut page.image, [-66, -2, 152, 218], 0);
    if !tail.is_empty() || !spec.sub.is_empty() {
        rect(&mut page.image, [226, 108, W, H], 0);
        draw::pieslice(
            &mut page.image,
            [226, 66, W, 150],
            180.0,
            360.0,
            Some(0),
            None,
            0,
        );
    }
    let width = 196.min(round_i(122.0 * craft::aspect(&big, "futura")));
    let mut number = word(&big, "futura", Some(width), None, false);
    if number.h() > 150 {
        number = word(&big, "futura", None, Some(150), false);
    }
    xor(
        &mut page.image,
        &number,
        14.0,
        fdiv(H - number.h(), 2) as f64,
    );
    page.mark(&big, number.h());
    let heading = spec.say.clone();
    if !heading.is_empty() {
        let room = if !tail.is_empty() || !spec.sub.is_empty() {
            44
        } else {
            120
        };
        let lines = lines_for(spec, &heading.to_uppercase(), "futura", 84, room, 4, 4);
        let masks = stack(&lines, "futura", 84, room, 4, 1.3);
        if smallest(&masks) < MIN_CAP {
            return Err(NoFit);
        }
        let mut y = 12;
        for (line, mask) in lines.iter().zip(&masks) {
            page.text(mask, 222, y, line, 0);
            y += mask.h() + 4;
        }
    }
    let mut y = 118;
    if !tail.is_empty() {
        let mask = word(&tail, "futura", Some(68), None, false);
        page.text(&mask, 235, y, &tail, 255);
        y += mask.h() + 10;
    }
    if !spec.sub.is_empty() {
        page.block_text(&spec.sub, "din", 234, y, 72, H - y - 6, 255, 3)?;
    }
    Ok(())
}

/// A lit sphere with craters, in copperplate lines. Returned as (one-bit drawing, mask).
fn moon(size: i32, seed: &mut PyRandom) -> (GrayImage, GrayImage) {
    let radius = size as f64 / 2.0;
    let mut craters: Vec<(f64, f64, f64)> = Vec::new();
    for _ in 0..26 {
        let a = seed.uniform(-0.9, 0.9);
        let b = seed.uniform(-0.9, 0.9);
        let c = seed.uniform(0.05, 0.2);
        craters.push((a, b, c));
    }
    craters.retain(|c| c.0 * c.0 + c.1 * c.1 < 0.9);
    let (lx, ly, lz) = (-0.62, -0.46, 0.63);
    let tilt = (-24.0f64).to_radians();
    let (ct, st) = (tilt.cos(), tilt.sin());
    let n = size as usize;
    let mut out = vec![0u8; n * n];
    for y in 0..n {
        for x in 0..n {
            let (u, v) = ((x as f64 - radius) / radius, (y as f64 - radius) / radius);
            let d2 = u * u + v * v;
            if d2 >= 1.0 {
                continue;
            }
            let nz = (1.0 - d2).sqrt();
            let (mut nx, mut ny) = (u, v);
            let mut albedo = 1.0;
            for &(cu, cv, cr) in &craters {
                let (du, dv) = (u - cu, v - cv);
                let dist = (du * du + dv * dv).sqrt();
                if dist < cr * 1.25 {
                    let t = dist / cr;
                    let slope = if t < 1.0 {
                        0.75 * (1.0 - t)
                    } else {
                        -1.6 * (t - 1.0)
                    };
                    if dist > 1e-4 {
                        nx -= du / dist * slope * 0.8;
                        ny -= dv / dist * slope * 0.8;
                    }
                    if t < 0.9 {
                        albedo *= 0.82;
                    }
                }
            }
            if (u * 3.1 + v * 1.7 + 1.0).sin() + (v * 4.3 - u * 1.1).sin() * 0.7 > 0.75 {
                albedo *= 0.72;
            }
            let length = (nx * nx + ny * ny + nz * nz).sqrt();
            let lambert = ((nx * lx + ny * ly + nz * lz) / length).max(0.0);
            let bright = 1.0f64.min(0.1 + lambert.powf(0.82) * albedo * 1.15);
            let (ru, rv) = (u * ct - v * st, u * st + v * ct);
            let lat = rv.clamp(-1.0, 1.0).asin() / (std::f64::consts::PI / 2.0);
            let lon = ru / 0.05f64.max((1.0 - rv * rv).sqrt());
            let phase = craft::pymod(
                lat * 25.0 * size as f64 / 192.0 + 0.12 * (lon * 7.0).sin(),
                1.0,
            );
            out[y * n + x] = if (phase - 0.5).abs() < 0.5 * (1.0 - bright).powf(0.8) * 1.02 {
                0
            } else {
                255
            };
        }
    }
    let drawing =
        GrayImage::from_raw(size as u32, size as u32, out).unwrap_or_else(|| raster::blank(0, 0));
    let mut mask = raster::blank(size as u32, size as u32);
    disc(&mut mask, [0, 0, size - 1, size - 1], 255);
    (drawing, mask)
}

const SPHERES: [&str; 10] = [
    "moon", "🌙", "🌕", "🌑", "🌝", "🌚", "🌛", "🌜", "planet", "ball",
];

fn night_pattern() -> &'static Regex {
    re!(r"(?i)\b(good ?night|night|natt|god natt|sleep\w*|moon|sove|dreams?|bedtime)\b")
}

/// Elegant words beside a subject drawn in swelling lines, like a banknote. Black, with stars.
fn look_engraving(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let subject = if !spec.pic.is_empty() {
        spec.pic.to_lowercase()
    } else if night_pattern().is_match(&format!("{} {}", spec.say, spec.sub)) {
        "moon".to_string()
    } else {
        String::new()
    };
    if subject.is_empty() {
        return Err(NoFit);
    }
    let dusk = bayer(&tone_page(|x, y| {
        0.5f64.min(0.0f64.max((x as f64 - 118.0) / 420.0 + (y as f64 - 40.0) / 420.0))
    }));
    raster::paste(&mut page.image, &dusk, 0, 0, None);
    for _ in 0..90 {
        let x = randrange(&mut rng, 0, W as i64);
        let y = randrange(&mut rng, 0, H as i64);
        draw::point(&mut page.image, &[(x, y)], 255);
    }
    let picture_left = 118;
    if SPHERES.contains(&subject.as_str()) {
        let (drawing, mask) = moon(192, &mut rng);
        raster::paste(&mut page.image, &drawing, picture_left + 2, 8, Some(&mask));
    } else {
        let Some((gray, mask)) = picture(spec, 186, 196, "") else {
            return Err(NoFit);
        };
        // Lines on black need more light to read.
        let lifted = raster::point(&gray, |v| (95 + fdiv(v as i32 * 160, 255)) as u8);
        let drawing = craft::engrave(&lifted, 3.4, -24.0, Some(&craft::inflate(&mask)));
        raster::paste(
            &mut page.image,
            &drawing,
            picture_left + fdiv(190 - gray.w(), 2),
            fdiv(H - gray.h(), 2),
            Some(&mask),
        );
    }
    // Clear a little night around the words, so the stars never touch the letters.
    rect(&mut page.image, [0, 0, picture_left - 1, H], 0);
    let foot = if spec.sub.is_empty() { 0 } else { 30 };
    let text = spec.say.clone();
    if text.is_empty() {
        return Err(NoFit);
    }
    let area = H - 28 - foot;
    let lines = lines_for(spec, &text, "didot-italic", 104, area, 10, 4);
    let mut masks: Vec<GrayImage> = lines
        .iter()
        .map(|line| word(line, "didot-italic", Some(104), None, true))
        .collect();
    if masks.iter().map(|m| m.h()).sum::<i32>() + 10 * (masks.len() as i32 - 1) > area {
        masks = stack(&lines, "didot-italic", 104, area, 8, 1.6);
    }
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let mut y = 14;
    for (line, mask) in lines.iter().zip(&masks) {
        page.text(mask, 10, y, line, 255);
        y += mask.h() + 10;
    }
    if !spec.sub.is_empty() {
        let lines_sub = craft::wrap_caption(&spec.sub, 104, "din", 18);
        let mut y = H - 8 - lines_sub.iter().map(|m| m.h() + 3).sum::<i32>();
        for mask in &lines_sub {
            page.text(mask, 10, y, &spec.sub, 255);
            y += mask.h() + 3;
        }
    }
    Ok(())
}

struct Scene {
    ball: [f64; 3],
    radius: f64,
    sun: [f64; 3],
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn clamp01(v: f64) -> f64 {
    1.0f64.min(0.0f64.max(v))
}

impl Scene {
    fn floor(&self, px: f64, pz: f64) -> f64 {
        let check = ((px * 0.95).floor() as i64 + (pz * 0.95).floor() as i64).rem_euclid(2);
        let mut base = if check != 0 { 0.96 } else { 0.03 };
        let (dx, dz) = (px - self.ball[0] - 0.55, pz - self.ball[2] - 0.2);
        let distance = (dx * dx + dz * dz * 0.7).sqrt();
        base *= 0.08 + 0.92 * clamp01((distance - 0.75) / 0.95);
        let fog = clamp01((pz - 1.0) / 20.0);
        base * (1.0 - fog) + 0.34 * fog
    }

    fn trace(&self, origin: [f64; 3], direction: [f64; 3], depth: i32) -> f64 {
        if self.radius != 0.0 {
            let oc = [
                origin[0] - self.ball[0],
                origin[1] - self.ball[1],
                origin[2] - self.ball[2],
            ];
            let b = dot3(oc, direction);
            let disc = b * b - (dot3(oc, oc) - self.radius * self.radius);
            if disc > 0.0 && depth < 2 {
                let t = -b - disc.sqrt();
                if t > 1e-3 {
                    let p = [
                        origin[0] + direction[0] * t,
                        origin[1] + direction[1] * t,
                        origin[2] + direction[2] * t,
                    ];
                    let n = [
                        (p[0] - self.ball[0]) / self.radius,
                        (p[1] - self.ball[1]) / self.radius,
                        (p[2] - self.ball[2]) / self.radius,
                    ];
                    let dn = dot3(direction, n);
                    let reflected = [
                        direction[0] - 2.0 * dn * n[0],
                        direction[1] - 2.0 * dn * n[1],
                        direction[2] - 2.0 * dn * n[2],
                    ];
                    let colour = self.trace(p, reflected, depth + 1);
                    let glint = 0.0f64.max(dot3(reflected, self.sun)).powf(60.0);
                    let rim = clamp01((-dn - 0.08) / 0.2);
                    return 1.0f64.min(colour * rim + glint * 1.6);
                }
            }
        }
        if direction[1] < -1e-4 {
            let t = -origin[1] / direction[1];
            return self.floor(origin[0] + direction[0] * t, origin[2] + direction[2] * t);
        }
        if depth != 0 {
            // What the ball sees is a bright studio, not the black behind it.
            return 0.25 + 0.75 * clamp01(1.0 - direction[1] * 1.15);
        }
        0.0f64.max(0.42 - direction[1] * 1.5) // The sky glows a little at the horizon.
    }
}

/// A real ray trace: checkered floor, black sky, and a mirror ball or any subject on stage.
fn look_stage(page: &mut Page, spec: &mut Spec) -> Res {
    let found = if spec.pic.is_empty() {
        None
    } else {
        picture(spec, 150, 130, "")
    };
    let scale = 2;
    let (width, height) = (W * scale, H * scale);
    let cam = [0.0, 1.5, -4.8];
    let ball = [1.15, 1.2, -1.0];
    let radius = if found.is_none() { 1.2 } else { 0.0 };
    let sun: [f64; 3] = [-0.55, 0.75, -0.4];
    let norm = (sun[0] * sun[0] + sun[1] * sun[1] + sun[2] * sun[2]).sqrt();
    let sun = [sun[0] / norm, sun[1] / norm, sun[2] / norm];
    let scene = Scene { ball, radius, sun };

    let mut data = vec![0u8; (width * height) as usize];
    let focal = height as f64 * 0.82;
    for j in 0..height {
        for i in 0..width {
            let x = (i as f64 + 0.5 - width as f64 / 2.0) / focal;
            let y = (height as f64 / 2.0 - j as f64 - 0.5) / focal - 0.16;
            let length = (x * x + y * y + 1.0).sqrt();
            let value = scene.trace(cam, [x / length, y / length, 1.0 / length], 0);
            data[(j * width + i) as usize] = ((value * 255.0) as i64).clamp(0, 255) as u8;
        }
    }
    let big = GrayImage::from_raw(width as u32, height as u32, data)
        .unwrap_or_else(|| raster::blank(0, 0));
    let gray = raster::resize(&big, W as u32, H as u32, Resample::Box);
    raster::paste(&mut page.image, &atkinson(&gray), 0, 0, None);
    if let Some((pic, mask)) = &found {
        let (x, y) = (W - pic.w() - 14, 158 - pic.h());
        let drawing = atkinson(pic);
        // A spotlight keeps the subject clear of the checkerboard, as on a real stage.
        let (cx, cy) = (x + fdiv(pic.w(), 2), y + fdiv(pic.h(), 2));
        let (rx, ry) = (fdiv(pic.w(), 2) + 18, fdiv(pic.h(), 2) + 18);
        ring(
            &mut page.image,
            [cx - rx, cy - ry, cx + rx, cy + ry],
            255,
            0,
            2,
        );
        raster::paste_value(&mut page.image, 0, x, y, &raster::max_filter(mask, 3));
        raster::paste(&mut page.image, &drawing, x, y, Some(mask));
    }
    let text = if spec.say.is_empty() {
        display(spec)?
    } else {
        spec.say.to_uppercase()
    };
    let lines = lines_for(spec, &text, "futura", 122, 72, 5, 3);
    let masks = stack(&lines, "futura", 122, 72, 5, 1.25);
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let head: i32 = masks.iter().map(|m| m.h() + 5).sum();
    rect(
        &mut page.image,
        [
            0,
            0,
            146,
            12 + head + if spec.sub.is_empty() { 0 } else { 34 },
        ],
        0,
    );
    let mut y = 12;
    for (line, mask) in lines.iter().zip(&masks) {
        page.text(mask, 12, y, line, 255);
        y += mask.h() + 5;
    }
    if !spec.sub.is_empty() {
        let lines_sub = craft::wrap_caption(&spec.sub, 126, "din", 18);
        let total = lines_sub.iter().map(|m| m.h() + 3).sum::<i32>() + 8;
        rect(&mut page.image, [6, y, 142, y + total], 0);
        y += 5;
        for mask in &lines_sub {
            page.text(mask, 12, y, &spec.sub, 255);
            y += mask.h() + 3;
        }
    }
    Ok(())
}

/// The numbers, scaled from 0 to 1. Without any, a shape that starts low and builds.
fn series(spec: &Spec, default: usize) -> Vec<f64> {
    if spec.data.len() >= 2 {
        let low = spec.data.iter().cloned().fold(f64::INFINITY, f64::min);
        let high = spec.data.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut span = high - low;
        if span == 0.0 {
            span = 1.0;
        }
        return spec.data.iter().map(|v| (v - low) / span).collect();
    }
    (0..default)
        .map(|i| clamp01((i as f64 - default as f64 * 0.4) / (default as f64 * 0.5)))
        .collect()
}

/// Strokes as dense as the numbers say. The empty stretch of the series is where the words go.
fn look_rain(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let series = series(spec, 9);
    page.image = raster::blank(W as u32, H as u32);

    let level = |x: f64| -> f64 {
        let pos = x / W as f64 * (series.len() as f64 - 1.0);
        let i = (series.len() as i64 - 2).min(pos as i64).max(0) as usize;
        series[i] + (series[i + 1] - series[i]) * (pos - i as f64)
    };

    for _ in 0..3000 {
        let x = rng.uniform(0.0, (W + 20) as f64);
        let y = rng.uniform(-12.0, 200.0);
        let here = level(((W - 1) as f64).min(x));
        if rng.random() < here.powf(1.2) {
            let length = 6.0 + 14.0 * here;
            stroke(
                &mut page.image,
                &[(x, y), (x - length * 0.34, y + length)],
                255,
                1,
            );
        }
    }
    let labels: Vec<String> = spec.items.iter().take(9).cloned().collect();
    let axis = if labels.is_empty() { 200 } else { 184 };
    rect(&mut page.image, [0, axis, W, H], 255);
    let ticks = labels.len().max(series.len());
    for i in 0..ticks {
        let x = 8.0 + i as f64 * (W - 16) as f64 / (ticks as f64 - 1.0).max(1.0);
        stroke(
            &mut page.image,
            &[(x, axis as f64), (x, (axis + 5) as f64)],
            0,
            1,
        );
    }
    if !labels.is_empty() {
        let step = if labels.len() <= 5 { 1 } else { 2 };
        for (i, label) in labels.iter().step_by(step).enumerate() {
            let mask = caption(label, "din", 17, None);
            let x =
                8.0 + (i * step) as f64 * (W - 16) as f64 / (labels.len() as f64 - 1.0).max(1.0);
            let left = ((W - mask.w() - 4) as f64).min(4.0f64.max(x - mask.w() as f64 / 2.0));
            page.text(&mask, left, 194, label, 0);
        }
    }
    // The driest stretch of the width that the words need.
    let width = 134;
    let mut best_x = 0;
    let mut best_level = f64::INFINITY;
    let mut x = 0;
    while x < W - width - 8 {
        let mut total = 0.0;
        let mut k = 0;
        while k < width {
            total += level((x + k) as f64);
            k += 6;
        }
        let average = total / (width as f64 / 6.0);
        if average < best_level {
            best_x = x;
            best_level = average;
        }
        x += 8;
    }
    let x = best_x + 12;
    let area = 150 - if spec.sub.is_empty() { 0 } else { 24 };
    let text = display(spec)?;
    let lines = lines_for(spec, &text, "impact", width - 12, area, 6, 4);
    let masks = stack(&lines, "impact", width - 12, area, 6, 1.5);
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let total = masks.iter().map(|m| m.h()).sum::<i32>() + 6 * (masks.len() as i32 - 1);
    rect(
        &mut page.image,
        [
            x - 8,
            4,
            x + width - 4,
            12 + total + if spec.sub.is_empty() { 0 } else { 30 },
        ],
        0,
    );
    let mut y = 12;
    for (line, mask) in lines.iter().zip(&masks) {
        page.text(mask, x, y, line, 255);
        y += mask.h() + 6;
    }
    if !spec.sub.is_empty() {
        let mask = caption(&spec.sub, "din", 18, Some(width - 12));
        page.text(&mask, x, y + 2, &spec.sub, 255);
    }
    Ok(())
}

/// Real numbers as a mountain range. A chart nobody has to read.
fn look_ridges(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let big = pull_number(spec);
    let series = series(spec, 7);

    let curve = |x: f64| -> f64 {
        let pos = x / W as f64 * (series.len() as f64 - 1.0);
        let i = (series.len() as i64 - 2).min(pos as i64).max(0) as usize;
        let t = pos - i as f64;
        let t = t * t * (3.0 - 2.0 * t);
        series[i] * (1.0 - t) + series[i + 1] * t
    };

    let sky = bayer(&tone_page(|_, y| 1.0 - 0.0f64.max(0.34 - y as f64 / 380.0)));
    raster::paste(&mut page.image, &sky, 0, 0, None);
    let sun_x = randrange(&mut rng, 40, 240);
    ring(&mut page.image, [sun_x, 14, sun_x + 58, 72], 255, 0, 2);
    for k in 0..5i32 {
        let phase = rng.random() * 9.0;
        let (base, amp, shade) = (
            (62 + k * 20) as f64,
            (40 - k * 3) as f64,
            0.84 - k as f64 * 0.19,
        );
        let tops: Vec<f64> = (0..W)
            .map(|x| {
                let value = curve(((x + k * 41) % W) as f64);
                base - value * amp
                    + (x as f64 / (21 - k) as f64 + phase).sin() * 7.0
                    + (x as f64 / 6.3 + phase * 2.0).sin() * 2.2
            })
            .collect();
        let layer = bayer(&tone_page(|x, y| {
            let top = tops[x as usize];
            if y as f64 >= top {
                shade - (y as f64 - top) * 0.0022
            } else {
                1.0
            }
        }));
        let mask = tone_page(|x, y| {
            if y as f64 >= tops[x as usize] {
                1.0
            } else {
                0.0
            }
        });
        raster::paste(&mut page.image, &layer, 0, 0, Some(&mask));
        let points: Vec<(f64, f64)> = (0..W).map(|x| (x as f64, tops[x as usize])).collect();
        stroke(&mut page.image, &points, 0, 1);
    }
    let front: Vec<f64> = (0..W)
        .map(|x| 150.0 - curve(x as f64) * 18.0 + (x as f64 / 11.0).sin() * 1.5)
        .collect();
    let mut poly: Vec<(f64, f64)> = (0..W).map(|x| (x as f64, front[x as usize])).collect();
    poly.push((W as f64, H as f64));
    poly.push((0.0, H as f64));
    draw::polygon(&mut page.image, &poly, Some(0), None, 0);
    let mut left = 12;
    if !big.is_empty() {
        let mut mask = word(&big, "impact", None, Some(52), false);
        if mask.w() > 150 {
            mask = word(&big, "impact", Some(150), Some(52), false);
        }
        page.text(&mask, left, 154, &big, 255);
        left += mask.w() + 10;
    }
    let words = spec.say.to_uppercase();
    let room = W - left - 12;
    if !words.is_empty() && !spec.sub.is_empty() {
        page.fit_line(&words, "din", left, 154, room, 24, 255, 'l')?;
        page.fit_line(
            &spec.sub.to_uppercase(),
            "din",
            left,
            183,
            room,
            23,
            255,
            'l',
        )?;
    } else if !words.is_empty() {
        let lines = lines_for(spec, &words, "din", room, 52, 4, 2);
        page.stack(&lines, "din", left, 154, room, 52, 4, 255, true, 1.3)?;
    } else if !spec.sub.is_empty() {
        page.fit_line(
            &spec.sub.to_uppercase(),
            "din",
            left,
            166,
            room,
            28,
            255,
            'l',
        )?;
    }
    Ok(())
}

/// The concept is the picture: branches physically become one line. Items name the branches.
fn look_transit(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let mut branches: Vec<String> = if spec.items.is_empty() {
        vec![
            if spec.sub.is_empty() {
                "A".to_string()
            } else {
                spec.sub.clone()
            },
            "B".to_string(),
        ]
    } else {
        spec.items.clone()
    };
    branches.truncate(3);
    if branches.len() == 1 {
        branches.push(String::new());
    }
    let scale = 4.0f64;
    let mut big = raster::paper((W * 4) as u32, (H * 4) as u32);
    let merge_y = 62.0f64;
    let starts: Vec<f64> = if branches.len() == 2 {
        vec![26.0, 98.0]
    } else {
        vec![24.0, 62.0, 100.0]
    };

    fn path(img: &mut GrayImage, points: &[(f64, f64)], width: f64, scale: f64) {
        let scaled: Vec<(f64, f64)> = points
            .iter()
            .map(|&(x, y)| (x * scale, y * scale))
            .collect();
        draw::line(img, &scaled, 0, (width * scale) as u32, true);
        let radius = width * scale / 2.0;
        for (x, y) in [scaled[0], scaled[scaled.len() - 1]] {
            disc(img, [x - radius, y - radius, x + radius, y + radius], 0);
        }
    }

    fn dot(img: &mut GrayImage, x: f64, y: f64, radius: f64, fill: u8, scale: f64) {
        disc(
            img,
            [
                (x - radius) * scale,
                (y - radius) * scale,
                (x + radius) * scale,
                (y + radius) * scale,
            ],
            fill,
        );
    }

    for &start in &starts {
        let mut points: Vec<(f64, f64)> = vec![(-10.0, start), (66.0, start)];
        for t in 0..=120 {
            let t = t as f64;
            points.push((
                66.0 + (182.0 - 66.0) * t / 120.0,
                start + (merge_y - start) * ((t / 120.0).powf(2.0) * (3.0 - 2.0 * t / 120.0)),
            ));
        }
        path(&mut big, &points, 9.0, scale);
        for k in 0..randint(&mut rng, 2, 3) {
            let x = (12 + k * 20) as f64;
            dot(&mut big, x, start, 8.0, 0, scale);
            dot(&mut big, x, start, 5.0, 255, scale);
        }
    }
    path(&mut big, &[(178.0, merge_y), (262.0, merge_y)], 17.0, scale);
    dot(&mut big, 276.0, merge_y, 28.0, 0, scale);
    dot(&mut big, 276.0, merge_y, 18.0, 255, scale);
    dot(&mut big, 276.0, merge_y, 9.0, 0, scale);
    let small = raster::threshold(
        &raster::resize(&big, W as u32, H as u32, Resample::Lanczos),
        128,
    );
    raster::paste(&mut page.image, &small, 0, 0, None);
    for (&start, name) in starts.iter().zip(&branches) {
        if !name.is_empty() {
            let mask = caption(name, "menlo", 16, Some(184));
            let y = if start < merge_y {
                start as i32 - 21
            } else {
                start as i32 + 8
            };
            page.text(&mask, 62, y, name, 0);
        }
    }
    let text = display(spec)?;
    let top = 125;
    let room = if spec.sub.is_empty() { 82 } else { 58 };
    let lines = lines_for(spec, &text, "futura", 292, room, 4, 2);
    page.stack(&lines, "futura", 10, top, 292, room, 4, 0, false, 1.15)?;
    if !spec.sub.is_empty() {
        footer(page, spec, 10, H - 28, 292, 24, 0)?;
    }
    Ok(())
}

fn facts(spec: &Spec) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in &spec.items {
        match item.split_once(':') {
            Some((key, value))
                if !value.trim_matches(craft::py_isspace).is_empty() && chars(key) <= 12 =>
            {
                out.push((
                    key.trim_matches(craft::py_isspace).to_uppercase(),
                    value.trim_matches(craft::py_isspace).to_uppercase(),
                ));
            }
            _ => out.push((String::new(), item.to_uppercase())),
        }
    }
    out
}

/// A QR code like segno's `make(text, error="m", boost_error=False)`: the smallest Micro QR or QR
/// that holds the text in a single mode. Returns (side, row-major modules).
fn qr_matrix(text: &str) -> Option<(usize, Vec<bool>)> {
    use qrcode::bits::Bits;
    use qrcode::Version;

    let alnum = |c: char| c.is_ascii_digit() || c.is_ascii_uppercase() || " $%*+-./:".contains(c);
    let numeric = !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
    let alphanumeric = !text.is_empty() && text.chars().all(alnum);
    let bytes: Vec<u8> = if text.chars().all(|c| (c as u32) < 256) {
        text.chars().map(|c| c as u32 as u8).collect()
    } else {
        text.as_bytes().to_vec()
    };
    let mut versions: Vec<Version> = (2..=4).map(Version::Micro).collect();
    versions.extend((1..=40).map(Version::Normal));
    for version in versions {
        let mut bits = Bits::new(version);
        let pushed = if numeric {
            bits.push_numeric_data(text.as_bytes())
        } else if alphanumeric {
            bits.push_alphanumeric_data(text.as_bytes())
        } else {
            bits.push_byte_data(&bytes)
        };
        if pushed.is_err() {
            continue;
        }
        if let Some(found) = qr_with_segno_mask(bits) {
            return Some(found);
        }
    }
    None
}

/// The data codewords as segno writes them. Segno's padding quirk is kept: when the data already ends on a
/// codeword boundary it still adds a whole byte of zero bits, which leaves one pad codeword fewer.
fn segno_data(bits: qrcode::bits::Bits) -> Option<Vec<u8>> {
    use qrcode::{EcLevel, Version};
    let version = bits.version();
    let odd_micro = matches!(version, Version::Micro(1) | Version::Micro(3));
    let capacity = bits.max_len(EcLevel::M).ok()?;
    let len = bits.len();
    if len > capacity {
        return None;
    }
    let raw = bits.into_bytes();
    let mut stream: Vec<bool> = (0..len)
        .map(|i| (raw[i / 8] >> (7 - i % 8)) & 1 == 1)
        .collect();
    let terminator = match version {
        Version::Micro(a) => a as usize * 2 + 1,
        Version::Normal(_) => 4,
    };
    stream.extend(std::iter::repeat_n(false, terminator.min(capacity - len)));
    if !odd_micro {
        let pad = 8 - stream.len() % 8;
        stream.extend(std::iter::repeat_n(false, pad));
    }
    if odd_micro {
        let missing = capacity.saturating_sub(stream.len());
        stream.extend(std::iter::repeat_n(false, missing));
    } else {
        const PADS: [u8; 2] = [0xEC, 0x11];
        let count = (capacity / 8).saturating_sub(stream.len() / 8);
        for i in 0..count {
            let byte = PADS[i % 2];
            stream.extend((0..8).map(|k| (byte >> (7 - k)) & 1 == 1));
        }
    }
    let mut data: Vec<u8> = stream
        .chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (k, &b)| a | ((b as u8) << (7 - k)))
        })
        .collect();
    data.truncate(capacity.div_ceil(8));
    Some(data)
}

/// Places the data like the `qrcode` crate does, but chooses the mask like segno does: the penalty is
/// scored before the format information is written, and Micro QR codes take the highest score.
fn qr_with_segno_mask(bits: qrcode::bits::Bits) -> Option<(usize, Vec<bool>)> {
    use qrcode::canvas::{Canvas, MaskPattern};
    use qrcode::{ec, EcLevel, Version};

    let version = bits.version();
    let micro = matches!(version, Version::Micro(_));
    let data = segno_data(bits)?;
    let (encoded, parity) = ec::construct_codewords(&data, version, EcLevel::M).ok()?;
    let mut base = Canvas::new(version, EcLevel::M);
    base.draw_all_functional_patterns();
    base.draw_data(&encoded, &parity);
    let side = version.width() as usize;
    let masks: Vec<MaskPattern> = if micro {
        vec![
            MaskPattern::HorizontalLines,
            MaskPattern::LargeCheckerboard,
            MaskPattern::Diamonds,
            MaskPattern::Meadow,
        ]
    } else {
        vec![
            MaskPattern::Checkerboard,
            MaskPattern::HorizontalLines,
            MaskPattern::VerticalLines,
            MaskPattern::DiagonalLines,
            MaskPattern::LargeCheckerboard,
            MaskPattern::Fields,
            MaskPattern::Diamonds,
            MaskPattern::Meadow,
        ]
    };
    let mut best: Option<(i64, Vec<bool>)> = None;
    for mask in masks {
        let mut canvas = base.clone();
        canvas.apply_mask(mask);
        let modules: Vec<bool> = canvas
            .into_colors()
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect();
        let score = if micro {
            micro_penalty(&modules, side)
        } else {
            qr_penalty(&modules, side)
        };
        let better = match &best {
            None => true,
            Some((top, _)) => {
                if micro {
                    score > *top
                } else {
                    score < *top
                }
            }
        };
        if better {
            best = Some((score, modules));
        }
    }
    best.map(|(_, modules)| (side, modules))
}

/// segno's `evaluate_micro_mask`.
fn micro_penalty(modules: &[bool], side: usize) -> i64 {
    let sum1 = (1..side).filter(|&i| modules[i * side + side - 1]).count() as i64;
    let sum2 = (1..side)
        .filter(|&i| modules[(side - 1) * side + i])
        .count() as i64;
    if sum1 <= sum2 {
        sum1 * 16 + sum2
    } else {
        sum2 * 16 + sum1
    }
}

/// segno's `evaluate_mask` (ISO 18004 7.8.3), on the symbol before its format and version information is
/// written: those areas count as light modules.
fn qr_penalty(modules: &[bool], side: usize) -> i64 {
    let mut grid: Vec<u8> = modules.iter().map(|&d| d as u8).collect();
    let w = side;
    let blank = |grid: &mut Vec<u8>, row: usize, col: usize| {
        if row != 6 && col != 6 {
            grid[row * w + col] = 0;
        }
    };
    for i in 0..9 {
        blank(&mut grid, i, 8);
        blank(&mut grid, 8, i);
        if i > 0 {
            blank(&mut grid, w - i, 8);
            blank(&mut grid, 8, w - i);
        }
    }
    // The reserved areas keep their row/column 6 timing modules, as the timing pattern is added last.
    if w > 41 {
        for i in 0..6 {
            for k in 9..=11 {
                grid[i * w + w - k] = 0;
                grid[(w - k) * w + i] = 0;
            }
        }
    }
    let at = |r: usize, c: usize| grid[r * w + c];

    fn n3(seq: &[u8], size: usize) -> i64 {
        const PATTERN: [u8; 7] = [1, 0, 1, 1, 1, 0, 1];
        let find = |from: usize| -> Option<usize> {
            if from + 7 > seq.len() {
                return None;
            }
            (from..=seq.len() - 7).find(|&i| seq[i..i + 7] == PATTERN)
        };
        let any = |lo: usize, hi: usize| {
            seq[lo.min(seq.len())..hi.min(seq.len()).max(lo.min(seq.len()))]
                .iter()
                .any(|&v| v != 0)
        };
        let mut count = 0;
        let mut found = find(0);
        while let Some(idx) = found {
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
            found = find(offset);
        }
        count
    }

    let (mut n1_score, mut n2_score, mut n3_score) = (0i64, 0i64, 0i64);
    let mut dark = 0i64;
    for i in 0..w {
        let (mut row_prev, mut col_prev) = (-1i32, -1i32);
        let (mut row_run, mut col_run) = (0i64, 0i64);
        let mut column = vec![0u8; w];
        for (j, cell) in column.iter_mut().enumerate() {
            let (row_bit, col_bit) = (at(i, j) as i32, at(j, i) as i32);
            *cell = at(j, i);
            dark += row_bit as i64;
            if row_bit == row_prev {
                row_run += 1;
            } else {
                if row_run >= 5 {
                    n1_score += row_run - 2;
                }
                row_run = 1;
            }
            if col_bit == col_prev {
                col_run += 1;
            } else {
                if col_run >= 5 {
                    n1_score += col_run - 2;
                }
                col_run = 1;
            }
            if i > 0 && j > 0 {
                let (above, above_left) = (at(i - 1, j) as i32, at(i - 1, j - 1) as i32);
                if row_bit == row_prev && row_prev == above && above == above_left {
                    n2_score += 3;
                }
            }
            row_prev = row_bit;
            col_prev = col_bit;
        }
        n3_score += n3(&grid[i * w..(i + 1) * w], w);
        n3_score += n3(&column, w);
        if row_run >= 5 {
            n1_score += row_run - 2;
        }
        if col_run >= 5 {
            n1_score += col_run - 2;
        }
    }
    let percent = dark as f64 / (w * w) as f64;
    let n4_score = 10 * ((percent * 100.0 - 50.0).abs() / 5.0) as i64;
    n1_score + n2_score + n3_score + n4_score
}

/// A real object, not a layout. Items become printed fields, and a QR code can sit on the stub.
fn look_ticket(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    draw::rounded_rectangle(
        &mut page.image,
        [3, 3, W - 4, H - 4],
        10.0,
        None,
        Some(0),
        2,
    );
    draw::rounded_rectangle(&mut page.image, [3, 3, W - 4, 34], 10.0, Some(0), None, 0);
    rect(&mut page.image, [3, 16, W - 4, 34], 0);
    if !spec.say.is_empty() {
        page.fit_line(
            &spec.say.to_uppercase(),
            "din",
            14,
            11,
            W - 30,
            17,
            255,
            'l',
        )?;
    }
    let stub = 210;
    let mut y = 44;
    while y < H - 12 {
        rect(&mut page.image, [stub, y, stub + 1, y + 3], 0);
        y += 7;
    }
    for cy in [35, H - 4] {
        ring(
            &mut page.image,
            [stub - 6, cy - 6, stub + 7, cy + 6],
            255,
            0,
            2,
        );
    }

    let mut all = facts(spec);
    if !spec.big.is_empty() {
        all.insert(0, (String::new(), spec.big.to_uppercase()));
    }
    if !spec.sub.is_empty() {
        all.insert(0, (String::new(), spec.sub.to_uppercase()));
    }
    if all.is_empty() && spec.qr.is_empty() {
        return Err(NoFit);
    }
    let mut stub_fact = None;
    if spec.qr.is_empty() && all.len() >= 2 {
        stub_fact = all.pop();
    }

    fn field_at(
        page: &mut Page,
        key: &str,
        value: &str,
        x: i32,
        mut y: i32,
        width: i32,
        mut height: i32,
    ) -> Res {
        if !key.is_empty() {
            page.text(&caption(key, "din", 16, Some(width)), x, y, key, 0);
            y += 16;
            height -= 16;
        }
        page.line(value, "din", x, y, width, height)?;
        Ok(())
    }

    // Short values share a row. A long one gets the whole width, or it would be too small to read.
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for fact in all {
        let wide = craft::aspect(&fact.1, "din") * 22.0 > 88.0;
        let joins = match rows.last() {
            Some(last) => {
                !wide && last.len() == 1 && craft::aspect(&last[0].1, "din") * 22.0 <= 88.0
            }
            None => false,
        };
        if joins {
            if let Some(last) = rows.last_mut() {
                last.push(fact);
            }
        } else {
            rows.push(vec![fact]);
        }
    }
    if rows.len() > 3 {
        return Err(NoFit);
    }
    let row_height = 58.min(fdiv(124, (rows.len() as i32).max(1)));
    let mut y = 40;
    for (r, row) in rows.iter().enumerate() {
        let width = fdiv(186 - 10 * (row.len() as i32 - 1), row.len() as i32);
        for (i, (key, value)) in row.iter().enumerate() {
            field_at(
                page,
                key,
                value,
                14 + i as i32 * (width + 10),
                y,
                width,
                row_height - 4,
            )?;
        }
        y += row_height;
        if r + 1 < rows.len() {
            rect(&mut page.image, [14, y - 3, 200, y - 2], 0);
        }
    }
    if y < 166 {
        let mut x = 14;
        while x < 200 {
            let bar = *rng.choice(&[1, 1, 2, 3]).unwrap_or(&1);
            rect(
                &mut page.image,
                [x, (y + 2).max(170), (x + bar - 1).min(200), 202],
                0,
            );
            x += bar + *rng.choice(&[1, 2, 2, 3]).unwrap_or(&1);
        }
    }

    if !spec.qr.is_empty() {
        if let Some((side, matrix)) = qr_matrix(&spec.qr) {
            let side_i = side as i32;
            let mut module = fdiv(88, side_i);
            if module < 2 {
                page.report.warnings.push(
                    "That QR text is too long to scan at this size. Shorten the link.".into(),
                );
            }
            module = module.max(1);
            let x0 = 218 + fdiv(86 - side_i * module, 2);
            let y0 = 46;
            for j in 0..side {
                for i in 0..side {
                    if matrix[j * side + i] {
                        let (i, j) = (i as i32, j as i32);
                        rect(
                            &mut page.image,
                            [
                                x0 + i * module,
                                y0 + j * module,
                                x0 + (i + 1) * module - 1,
                                y0 + (j + 1) * module - 1,
                            ],
                            0,
                        );
                    }
                }
            }
        } else {
            page.report
                .warnings
                .push("That QR text is too long to scan at this size. Shorten the link.".into());
        }
        let qr = spec.qr.clone();
        page.mark(&qr, 99);
    }
    let mut below_fact = 46;
    if let Some((key, value)) = stub_fact {
        let y = if spec.qr.is_empty() { 46 } else { 148 };
        let height = if spec.qr.is_empty() { 44 } else { H - y - 12 };
        field_at(page, &key, &value, 220, y, 80, height)?;
        below_fact = 100;
    }
    if !spec.pic.is_empty() && spec.qr.is_empty() {
        if let Some((gray, mask)) = picture(spec, 80, H - below_fact - 16, "") {
            raster::paste(
                &mut page.image,
                &atkinson(&gray),
                218 + fdiv(84 - gray.w(), 2),
                below_fact + fdiv(H - below_fact - 12 - gray.h(), 2),
                Some(&mask),
            );
        }
    }
    Ok(())
}

/// Today as `%-d %b` in capitals, in the machine's time zone.
fn today_label() -> String {
    if let Ok(output) = std::process::Command::new("date")
        .arg("+%-d %b")
        .env("LC_ALL", "C")
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_uppercase();
            if !text.is_empty() {
                return text;
            }
        }
    }
    // UTC from the system clock, with the civil-from-days algorithm.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let z = secs.div_euclid(86400) + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    format!("{day} {}", MONTHS[(month - 1) as usize])
}

/// A note that arrives as post: airmail edge, a stamp of the subject, and a postmark.
fn look_post(page: &mut Page, spec: &mut Spec) -> Res {
    let mut i = -20;
    while i < W + 20 {
        let ink = if fdiv(i, 12) % 2 != 0 { 0 } else { 255 };
        draw::polygon(
            &mut page.image,
            &[(i, 0), (i + 6, 0), (i, 8), (i - 6, 8)],
            Some(ink),
            None,
            0,
        );
        draw::polygon(
            &mut page.image,
            &[(i, H - 8), (i + 6, H - 8), (i, H), (i - 6, H)],
            Some(ink),
            None,
            0,
        );
        i += 12;
    }
    let (sx, sy, sw, sh) = (214, 16, 86, 104);
    rect(&mut page.image, [sx, sy, sx + sw, sy + sh], 0);
    let mut x = sx;
    while x < sx + sw + 1 {
        for y in [sy, sy + sh] {
            disc(&mut page.image, [x - 3, y - 3, x + 3, y + 3], 255);
        }
        x += 8;
    }
    let mut y = sy;
    while y < sy + sh + 1 {
        for x in [sx, sx + sw] {
            disc(&mut page.image, [x - 3, y - 3, x + 3, y + 3], 255);
        }
        y += 8;
    }
    rect(
        &mut page.image,
        [sx + 7, sy + 7, sx + sw - 7, sy + sh - 7],
        255,
    );
    let default = if re!(r"(?i)birthday|bursdag|fødselsdag").is_match(&spec.say) {
        "🎂"
    } else {
        "💌"
    };
    let heart = Spec {
        pic: "heart".into(),
        ..Spec::default()
    };
    let found =
        picture(spec, sw - 18, sh - 18, default).or_else(|| picture(&heart, sw - 18, sh - 18, ""));
    if let Some((gray, mask)) = found {
        raster::paste(
            &mut page.image,
            &atkinson(&gray),
            sx + 9 + fdiv(sw - 18 - gray.w(), 2),
            sy + 9 + fdiv(sh - 18 - gray.h(), 2),
            Some(&mask),
        );
    }
    let (px, py) = (214, 150);
    let mut mark = raster::blank(W as u32, H as u32);
    draw::ellipse(
        &mut mark,
        [px - 34, py - 34, px + 34, py + 34],
        None,
        Some(255),
        3,
    );
    draw::ellipse(
        &mut mark,
        [px - 25, py - 25, px + 25, py + 25],
        None,
        Some(255),
        1,
    );
    for k in 0..5 {
        let points: Vec<(f64, f64)> = (px + 36..W + 2)
            .step_by(2)
            .map(|x| {
                (
                    x as f64,
                    (py - 22 + k * 11) as f64 + (x as f64 / 9.0).sin() * 3.0,
                )
            })
            .collect();
        stroke(&mut mark, &points, 255, 2);
    }
    let inverted = raster::invert(&page.image);
    raster::paste(&mut page.image, &inverted, 0, 0, Some(&mark));
    disc(&mut page.image, [px - 24, py - 24, px + 24, py + 24], 255);
    let date = if spec.date.is_empty() {
        today_label()
    } else {
        spec.date.clone()
    };
    let date: String = date.chars().take(8).collect();
    paste(
        &mut page.image,
        &word("PRINS", "din", Some(34), None, false),
        (px - 17) as f64,
        (py - 15) as f64,
        0,
    );
    paste(
        &mut page.image,
        &word(&date, "din", Some(34), None, false),
        (px - 17) as f64,
        (py + 3) as f64,
        0,
    );
    let (column, top) = (158, 18);
    let foot = if spec.sender.is_empty() { 0 } else { 26 };
    let extra = if spec.sub.is_empty() { 0 } else { 28 };
    let area = H - top - 14 - foot - extra;
    let text = spec.say.clone();
    if text.is_empty() {
        return Err(NoFit);
    }
    let lines = lines_for(spec, &text, "didot-italic", column, area, 8, 3);
    let n = lines.len() as i32;
    let mut masks: Vec<GrayImage> = lines
        .iter()
        .map(|line| {
            word(
                line,
                "didot-italic",
                Some(column.min(round_i(44.0 * craft::aspect(line, "didot-italic")))),
                None,
                true,
            )
        })
        .collect();
    if masks.iter().map(|m| m.h()).sum::<i32>() + 8 * (n - 1) > area {
        masks = lines
            .iter()
            .map(|line| {
                word(
                    line,
                    "didot-italic",
                    None,
                    Some(8.max(fdiv(area - 8 * (n - 1), n))),
                    true,
                )
            })
            .collect();
        masks = lines
            .iter()
            .zip(masks)
            .map(|(line, m)| {
                if m.w() <= column {
                    m
                } else {
                    word(line, "didot-italic", Some(column), None, true)
                }
            })
            .collect();
    }
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let mut y = top;
    for (line, mask) in lines.iter().zip(&masks) {
        page.text(mask, 14, y, line, 0);
        y += mask.h() + 8;
    }
    if !spec.sub.is_empty() {
        page.block_text(
            &spec.sub,
            "din",
            14,
            (y + 2).min(H - 12 - foot - extra),
            column,
            extra,
            0,
            5,
        )?;
    }
    if !spec.sender.is_empty() {
        let y = H - 12 - foot;
        let label = caption("FROM", "din", 16, None);
        page.text(&label, 14, y + 4, "FROM", 0);
        page.fit_line(
            &spec.sender.to_uppercase(),
            "din",
            14 + label.w() + 8,
            y,
            column - label.w() - 8,
            22,
            0,
            'l',
        )?;
    }
    Ok(())
}

/// Generative tiles behind a plate. The words seed the pattern, so no two are the same.
fn look_cover(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let scale = 3;
    let mut big = raster::paper((W * scale) as u32, (H * scale) as u32);
    let cell = *rng.choice(&[24, 26, 28]).unwrap_or(&24) * scale;
    let band = (cell as f64 * 0.34) as i32;
    let half = fdiv(cell, 2);
    let mut gy = 0;
    while gy < H * scale {
        let mut gx = 0;
        while gx < W * scale + cell {
            let mode = rng.random();
            let band_w = band.max(0) as u32;
            if mode < 0.46 {
                draw::arc(
                    &mut big,
                    [gx - half, gy - half, gx + half, gy + half],
                    0.0,
                    90.0,
                    0,
                    band_w,
                );
                draw::arc(
                    &mut big,
                    [gx + half, gy + half, gx + cell + half, gy + cell + half],
                    180.0,
                    270.0,
                    0,
                    band_w,
                );
            } else if mode < 0.92 {
                draw::arc(
                    &mut big,
                    [gx + half, gy - half, gx + cell + half, gy + half],
                    90.0,
                    180.0,
                    0,
                    band_w,
                );
                draw::arc(
                    &mut big,
                    [gx - half, gy + half, gx + half, gy + cell + half],
                    270.0,
                    360.0,
                    0,
                    band_w,
                );
            } else {
                stroke(
                    &mut big,
                    &[(gx + half, gy), (gx + half, gy + cell)],
                    0,
                    band_w,
                );
                stroke(
                    &mut big,
                    &[(gx, gy + half), (gx + cell, gy + half)],
                    0,
                    band_w,
                );
            }
            gx += cell;
        }
        gy += cell;
    }
    let tiles = raster::threshold(
        &raster::resize(&big, W as u32, H as u32, Resample::Lanczos),
        128,
    );
    raster::paste(&mut page.image, &tiles, 0, 0, None);
    let text = display(spec)?;
    let width = 196;
    let lines = lines_for(spec, &text, "futura", width, 60, 6, 3);
    let masks = stack(&lines, "futura", width, 60, 6, 1.2);
    if smallest(&masks) < MIN_CAP {
        return Err(NoFit);
    }
    let head = masks.iter().map(|m| m.h()).sum::<i32>() + 6 * (masks.len() as i32 - 1);
    let sub_masks = if spec.sub.is_empty() {
        Vec::new()
    } else {
        craft::wrap_caption(&spec.sub, width, "din", 22)
    };
    let sub_h: i32 = sub_masks.iter().map(|m| m.h() + 4).sum();
    let total = head + sub_h + if sub_masks.is_empty() { 0 } else { 8 } + 24;
    let top = fdiv(H - total, 2);
    rect(&mut page.image, [44, top - 4, W - 44, top + total + 4], 255);
    rect(&mut page.image, [48, top, W - 48, top + total], 0);
    let mut y = top + 12;
    for (line, mask) in lines.iter().zip(&masks) {
        page.text(mask, 58, y, line, 255);
        y += mask.h() + 6;
    }
    y += 2;
    for mask in &sub_masks {
        page.text(mask, 58, y, &spec.sub, 255);
        y += mask.h() + 4;
    }
    Ok(())
}

/// A title band and ruled lines to tick off. Rows share out whatever height is left.
fn look_list(page: &mut Page, spec: &mut Spec) -> Res {
    let items: Vec<String> = if !spec.items.is_empty() {
        spec.items.clone()
    } else if !spec.sub.is_empty() {
        vec![spec.sub.clone()]
    } else {
        Vec::new()
    };
    if items.is_empty() {
        return Err(NoFit);
    }
    let head = if spec.say.is_empty() { 0 } else { 44 };
    if !spec.say.is_empty() {
        rect(&mut page.image, [0, 0, W, head - 1], 0);
        let title = spec.say.to_uppercase();
        let ratio = craft::aspect(&title, "impact");
        let height = 30.min(round_i(296.0 / ratio));
        if height < MIN_CAP {
            return Err(NoFit);
        }
        let mask = word(
            &title,
            "impact",
            Some(296.min(round_i(height as f64 * ratio))),
            Some(height),
            false,
        );
        page.text(&mask, 8, fdiv(head - mask.h(), 2), &title, 255);
    }
    let foot = if !spec.sub.is_empty() && !spec.items.is_empty() {
        24
    } else {
        0
    };
    let columns = if items.len() > 5 { 2 } else { 1 };
    let per_column = items.len().div_ceil(columns) as i32;
    let room = H - head - foot - 8;
    let row = fdiv(room, per_column);
    if row < 20 {
        return Err(NoFit);
    }
    let width = fdiv(W - 16 - 10 * (columns as i32 - 1), columns as i32);
    let size = craft::MIN_TEXT_PX.max(34.min(row - 8));
    for (index, item) in items.iter().enumerate() {
        let index = index as i32;
        let column = fdiv(index, per_column);
        let x = 8 + column * (width + 10);
        let y = head + 4 + (index % per_column) * row;
        let boxed = 10.max((row - 10).min(18));
        let top = y + fdiv(row - boxed, 2);
        outlined_rect(&mut page.image, [x, top, x + boxed, top + boxed], 2);
        let mask = caption(item, "din", size, Some(width - boxed - 8));
        page.text(&mask, x + boxed + 8, y + fdiv(row - mask.h(), 2), item, 0);
        if (index % per_column) < per_column - 1 {
            stroke(
                &mut page.image,
                &[(x, y + row - 1), (x + width, y + row - 1)],
                0,
                1,
            );
        }
    }
    if foot != 0 {
        footer(page, spec, 8, H - foot - 2, W - 16, foot - 4, 0)?;
    }
    Ok(())
}

/// Longer words, set like a page. The last resort too, so it always fits, and says if it can't.
fn look_quote(page: &mut Page, spec: &mut Spec) -> Res {
    if !spec.say.is_empty() && chars(&spec.sub) as f64 > chars(&spec.say) as f64 * 1.5 {
        std::mem::swap(&mut spec.say, &mut spec.sub); // The longer one is the quote; the other, who said it.
    }
    let text = [
        spec.say.clone(),
        spec.sub.clone(),
        spec.big.clone(),
        spec.items.join(" "),
        spec.pic.clone(),
        spec.qr.clone(),
    ]
    .into_iter()
    .find(|t| !t.is_empty())
    .unwrap_or_default();
    let foot = if !spec.sub.is_empty() && !spec.say.is_empty() {
        28
    } else {
        0
    };
    rect(&mut page.image, [10, 10, 13, H - 12 - foot], 0);
    let (x, width) = (22, W - 34);
    let area = H - 22 - foot;
    let mut chosen: Vec<GrayImage> = Vec::new();
    let mut gap = 3;
    for size in [60, 52, 44, 38, 32, 28, 24, 21, 18, craft::MIN_TEXT_PX] {
        let face = if size >= 38 {
            "didot-italic"
        } else {
            "georgia"
        };
        let lines = craft::wrap_caption(&text, width, face, size);
        gap = 3.max(fdiv(size, 6));
        let total = lines.iter().map(|m| m.h()).sum::<i32>() + gap * (lines.len() as i32 - 1);
        if total <= area {
            chosen = lines;
            break;
        }
    }
    // Python tests `if not chosen`, which an empty list also fails.
    if chosen.is_empty() {
        let size = craft::MIN_TEXT_PX;
        gap = 3;
        let lines = craft::wrap_caption(&text, width, "georgia", size);
        let mut kept = Vec::new();
        let mut total = 0;
        for mask in lines {
            if total + mask.h() + gap > area {
                break;
            }
            total += mask.h() + gap;
            kept.push(mask);
        }
        chosen = kept;
        page.report
            .warnings
            .push("The text was too long to fit, so the end was cut off. Shorten it.".into());
    }
    let mut y = 10
        + fdiv(
            area - chosen.iter().map(|m| m.h()).sum::<i32>() - gap * (chosen.len() as i32 - 1),
            2,
        );
    for mask in &chosen {
        page.text(mask, x, y, &text, 0);
        y += mask.h() + gap;
    }
    if foot != 0 {
        footer(page, spec, x, H - foot - 2, width, foot - 6, 0)?;
    }
    Ok(())
}

/// The subject, huge and printed, with stacked type beside it. For any picture someone asks for.
fn look_picture(page: &mut Page, spec: &mut Spec) -> Res {
    let mut rng = spec.rng();
    let Some((mut gray, mut mask)) = picture(spec, 172, 204, "") else {
        return Err(NoFit);
    };
    let finish = *rng.choice(&["atkinson", "halftone"]).unwrap_or(&"atkinson");
    let mut drawing = craft::finish_picture(&gray, &mask, finish);
    let words_left = rng.random() < 0.5;
    let mut text_w = W - gray.w() - 26;
    if text_w < 100 {
        gray = craft::fit(&gray, 170, 190);
        mask = hard(&craft::fit(&mask, 170, 190));
        drawing = craft::finish_picture(&gray, &mask, finish);
        text_w = W - gray.w() - 26;
    }
    let pic_x = if words_left { W - gray.w() - 8 } else { 8 };
    let text_x = if words_left { 8 } else { gray.w() + 18 };
    let (cx, cy) = (pic_x as f64 + gray.w() as f64 / 2.0, H as f64 / 2.0);
    let rays = randrange(&mut rng, 14, 24);
    let behind = tone_page(|x, y| {
        let angle = (y as f64 - cy).atan2(x as f64 - cx) * rays as f64 / std::f64::consts::PI;
        if craft::pymod(angle, 2.0) < 1.0 {
            0.72
        } else {
            1.0
        }
    });
    let panel = bayer(&behind);
    let left_edge = if words_left { pic_x - 8 } else { 0 };
    let right_edge = if words_left { W } else { pic_x + gray.w() + 8 };
    raster::paste(
        &mut page.image,
        &raster::crop(&panel, left_edge, 0, right_edge, H),
        left_edge,
        0,
        None,
    );
    let top_y = fdiv(H - gray.h(), 2);
    raster::paste_value(
        &mut page.image,
        0,
        pic_x,
        top_y,
        &raster::max_filter(&mask, 5),
    );
    raster::paste(&mut page.image, &drawing, pic_x, top_y, Some(&mask));
    let sub_lines = if spec.sub.is_empty() {
        Vec::new()
    } else {
        craft::wrap_caption(&spec.sub, text_w, "din", 20)
    };
    let foot: i32 = sub_lines.iter().map(|m| m.h() + 4).sum();
    if foot > 80 {
        return Err(NoFit);
    }
    let mut top;
    let gap_foot = if foot != 0 { 6 } else { 0 };
    if !spec.say.is_empty() {
        let text = spec.say.to_uppercase();
        let room = H - 18 - foot - gap_foot;
        let lines = lines_for(spec, &text, "impact", text_w, room, 6, 5);
        let masks = stack(&lines, "impact", text_w, room, 6, 1.3);
        if smallest(&masks) < MIN_CAP {
            return Err(NoFit);
        }
        let total = masks.iter().map(|m| m.h()).sum::<i32>() + 6 * (masks.len() as i32 - 1);
        top = fdiv(H - total - foot - gap_foot, 2);
        for (line, mask) in lines.iter().zip(&masks) {
            page.text(mask, text_x + fdiv(text_w - mask.w(), 2), top, line, 0);
            top += mask.h() + 6;
        }
    } else {
        top = fdiv(H - foot, 2);
    }
    for mask in &sub_lines {
        page.text(mask, text_x + fdiv(text_w - mask.w(), 2), top, &spec.sub, 0);
        top += mask.h() + 4;
    }
    Ok(())
}

// --- the catalogue of looks ---------------------------------------------------------------------

struct Look {
    name: &'static str,
    run: fn(&mut Page, &mut Spec) -> Res,
    /// One line for the model: when this look is the right one.
    when: &'static str,
    /// Fields it shows besides `say` and `sub`.
    shows: &'static [&'static str],
}

impl Look {
    fn shows(&self, field: &str) -> bool {
        field == "say" || field == "sub" || self.shows.contains(&field)
    }
}

static LOOKS: [Look; 18] = [
    Look {
        name: "block",
        run: look_block,
        when: "loud statement",
        shows: &[],
    },
    Look {
        name: "alarm",
        run: look_alarm,
        when: "urgent, broken, blocked",
        shows: &[],
    },
    Look {
        name: "relief",
        run: look_relief,
        when: "one calm word",
        shows: &[],
    },
    Look {
        name: "monument",
        run: look_monument,
        when: "a win, a launch",
        shows: &["big"],
    },
    Look {
        name: "ransom",
        run: look_ransom,
        when: "playful nag, under 5 words",
        shows: &[],
    },
    Look {
        name: "numeral",
        run: look_numeral,
        when: "countdown, key number",
        shows: &["big"],
    },
    Look {
        name: "collide",
        run: look_collide,
        when: "appointment at a time",
        shows: &["big"],
    },
    Look {
        name: "picture",
        run: look_picture,
        when: "a thing or creature",
        shows: &["pic"],
    },
    Look {
        name: "engraving",
        run: look_engraving,
        when: "night, quiet, elegant",
        shows: &["pic"],
    },
    Look {
        name: "stage",
        run: look_stage,
        when: "event, show",
        shows: &["pic"],
    },
    Look {
        name: "post",
        run: look_post,
        when: "greeting, thanks, birthday",
        shows: &["pic"],
    },
    Look {
        name: "ticket",
        run: look_ticket,
        when: "meeting, trip: items KEY: VALUE",
        shows: &["big", "items", "qr", "pic"],
    },
    Look {
        name: "rain",
        run: look_rain,
        when: "weather, trend: data, items label it",
        shows: &["data", "items"],
    },
    Look {
        name: "ridges",
        run: look_ridges,
        when: "a total: big, data",
        shows: &["big", "data"],
    },
    Look {
        name: "transit",
        run: look_transit,
        when: "merge, route: items name lines",
        shows: &["items"],
    },
    Look {
        name: "cover",
        run: look_cover,
        when: "start of a day",
        shows: &[],
    },
    Look {
        name: "list",
        run: look_list,
        when: "several items",
        shows: &["items"],
    },
    Look {
        name: "quote",
        run: look_quote,
        when: "sentences, poem",
        shows: &[],
    },
];

/// `"; ".join(f"{name}={look.when}" ...)`, the text used inside draw_poster's `look` description.
pub const LOOK_GUIDE: &str = "block=loud statement; alarm=urgent, broken, blocked; relief=one calm word; monument=a win, a launch; ransom=playful nag, under 5 words; numeral=countdown, key number; collide=appointment at a time; picture=a thing or creature; engraving=night, quiet, elegant; stage=event, show; post=greeting, thanks, birthday; ticket=meeting, trip: items KEY: VALUE; rain=weather, trend: data, items label it; ridges=a total: big, data; transit=merge, route: items name lines; cover=start of a day; list=several items; quote=sentences, poem";

/// (name, when) for every look, in order.
pub fn looks() -> Vec<(String, String)> {
    LOOKS
        .iter()
        .map(|l| (l.name.to_string(), l.when.to_string()))
        .collect()
}

pub fn look_names() -> Vec<String> {
    LOOKS.iter().map(|l| l.name.to_string()).collect()
}

fn look_index(name: &str) -> Option<usize> {
    LOOKS.iter().position(|l| l.name == name)
}

// --- choosing -----------------------------------------------------------------------------------

fn urgent() -> &'static Regex {
    re!(
        r"(?i)!|\b(urgent|asap|blocked|broken|down|failed|failing|outage|deadline|overdue|stop|now|haster|nede|feil)\b"
    )
}
fn win() -> &'static Regex {
    re!(
        r"(?i)\b(ship|shipped|launched|released|done|won|winner|congrat\w*|hooray|finally|gratulerer|ferdig)\b"
    )
}
fn greet() -> &'static Regex {
    re!(
        r"(?i)\b(happy|birthday|thanks|thank you|welcome|hello|hi|love|miss you|takk|bursdag|gratulerer|hei|merry|sorry)\b"
    )
}
fn calm() -> &'static Regex {
    re!(r"(?i)\b(focus|breathe|rest|quiet|calm|relax|pause|silence|fokus|pust|ro)\b")
}
fn merge() -> &'static Regex {
    re!(r"(?i)\b(merged?|branch|route|line|journey|train|metro|commute)\b")
}
fn nag() -> &'static Regex {
    re!(r"(?i)\b(call|reply|pay|remember|don'?t forget|ring|svar|husk)\b")
}
fn day() -> &'static Regex {
    re!(
        r"(?i)\b(mon|tues|wednes|thurs|fri|satur|sun)day\b|\b(mandag|tirsdag|onsdag|torsdag|fredag|lørdag|søndag|good morning|god morgen|today|i dag)\b"
    )
}
fn event() -> &'static Regex {
    re!(r"(?i)\b(demo|show|party|concert|gig|launch party|premiere|festival|talk)\b")
}
fn weather() -> &'static Regex {
    re!(r"(?i)\b(rain|snow|storm|wind|forecast|weather|regn|snø|vær)\b")
}
fn count() -> &'static Regex {
    re!(r"(?i)\b\d+\s*(days?|hours?|weeks?|left|dager?|timer?|uker?|igjen)\b|^\d+%?$")
}

/// The looks most likely to suit this content, best first. Cheap: no drawing, no model.
pub fn rank(spec: &Spec) -> Vec<String> {
    let text = format!("{} {}", spec.say, spec.sub);
    let words = py_split(&spec.say).len();
    let mut score = [0.0f64; 18];
    let at = |name: &str| look_index(name).unwrap_or(0);
    score[at("block")] = 1.0;
    let mut add = |pairs: &[(&str, f64)]| {
        for (name, value) in pairs {
            score[at(name)] += value;
        }
    };

    if chars(&spec.say) > 44 || words > 8 {
        add(&[("quote", 9.0), ("block", -1.0)]);
    }
    if !spec.pic.is_empty() {
        add(&[
            ("picture", 4.0),
            ("engraving", 2.0),
            ("stage", 1.0),
            ("post", 1.0),
        ]);
    }
    if !spec.items.is_empty() {
        let kv = spec.items.iter().filter(|i| i.contains(':')).count();
        add(&[
            ("ticket", if kv >= 2 { 6.0 } else { 0.0 }),
            ("list", if kv < 2 { 5.0 } else { 1.0 }),
            ("transit", 1.0),
        ]);
    }
    if !spec.qr.is_empty() {
        add(&[("ticket", 8.0)]);
    }
    if !spec.data.is_empty() {
        let has_number = !spec.big.is_empty() || number_pattern().is_match(&spec.say);
        add(&[
            ("ridges", if has_number { 5.0 } else { 3.0 }),
            ("rain", 4.0),
        ]);
    }
    if weather().is_match(&text) {
        add(&[
            ("rain", if spec.data.is_empty() { 3.0 } else { 5.0 }),
            ("picture", 1.0),
        ]);
    }
    if urgent().is_match(&text) {
        add(&[("alarm", 5.0)]);
    }
    if win().is_match(&text) {
        add(&[("monument", 5.0)]);
    }
    if greet().is_match(&text) {
        add(&[("post", 6.0)]);
    }
    if calm().is_match(&text) && words <= 2 {
        add(&[("relief", 6.0)]);
    }
    if merge().is_match(&text) {
        add(&[("transit", 5.0)]);
    }
    if nag().is_match(&text) && words <= 5 {
        add(&[("ransom", 4.0)]);
    }
    if day().is_match(&text) {
        add(&[("cover", 4.0)]);
    }
    if event().is_match(&text) {
        add(&[("stage", 4.0)]);
    }
    if night_pattern().is_match(&text) {
        add(&[("engraving", 5.0)]);
    }
    if (count().is_match(&spec.say) || (!spec.big.is_empty() && !time_full(&spec.big)))
        && spec.data.is_empty()
    {
        add(&[("numeral", 5.0)]);
    }
    if time_search().is_match(&text) || (!spec.big.is_empty() && time_full(&spec.big)) {
        add(&[("collide", 4.0)]);
    }
    if words == 1 && spec.pic.is_empty() {
        add(&[("relief", 1.0), ("monument", 0.5)]);
    }

    // Looks missing what they need sit at the back, so they can never be picked first by accident.
    if spec.items.is_empty() {
        add(&[("list", -9.0)]);
        if spec.sub.is_empty() {
            add(&[("ticket", -3.0)]);
        }
    }
    if spec.pic.is_empty() {
        add(&[("picture", -9.0)]);
        if !night_pattern().is_match(&text) {
            add(&[("engraving", -9.0)]);
        }
    }
    if spec.big.is_empty() && !number_pattern().is_match(&spec.say) {
        add(&[("numeral", -9.0), ("collide", -9.0)]);
    }
    if spec.data.is_empty() {
        add(&[("ridges", -2.0), ("rain", -1.0)]);
    }
    let mut order: Vec<usize> = (0..LOOKS.len()).collect();
    order.sort_by(|&a, &b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    order
        .into_iter()
        .map(|i| LOOKS[i].name.to_string())
        .collect()
}

/// Whatever this look can't show moves somewhere it can, so no words are ever lost.
fn fold(spec: &Spec, look: &Look) -> Spec {
    let mut copy = spec.clone();
    if !copy.big.is_empty() && !look.shows("big") {
        let known = casefold(&format!("{} {}", copy.say, copy.sub));
        if copy.say.is_empty() {
            copy.say = copy.big.clone();
        } else if !known.contains(&casefold(&copy.big)) {
            copy.sub = format!("{} {}", copy.big, copy.sub)
                .trim_matches(craft::py_isspace)
                .to_string();
        }
        copy.big = String::new();
    }
    if !copy.items.is_empty() && !look.shows("items") {
        let joined = copy.items.join(", ");
        copy.sub = format!("{} - {joined}", copy.sub)
            .trim_matches(|c: char| c == ' ' || c == '-')
            .to_string();
        copy.items = Vec::new();
    }
    copy
}

// --- output -------------------------------------------------------------------------------------

/// Keeps the print head safe by opening up the middle of big black areas with fine dots.
///
/// Edges and everything within a few pixels of white stay solid, so type stays sharp. On paper this
/// reads as black with a little texture, like ink on rough stock.
pub fn thin_ink(image: GrayImage, limit: f64) -> GrayImage {
    let black = raster::histogram(&image)[0] as f64;
    let total = (W * H) as f64;
    if black / total <= limit {
        return image;
    }
    let solid = raster::min_filter(&raster::point(&image, |v| if v < 128 { 255 } else { 0 }), 7);
    let interior = raster::histogram(&solid)[255] as f64;
    if interior == 0.0 {
        return image;
    }
    let fraction = 0.42f64.min((black - limit * total) / interior);
    let screen = bayer(&raster::filled(
        W as u32,
        H as u32,
        (fraction * 255.0) as u8,
    ));
    let mut out = image.clone();
    raster::paste_value(&mut out, 255, 0, 0, &raster::multiply(&screen, &solid));
    out
}

/// A finished one-bit note (0 / 255, W x H) and a report. Never fails for odd content: it finds a
/// look that fits. `Err` only when there is nothing to say.
pub fn render(op: &Value, sender: &str) -> Result<(GrayImage, Report), String> {
    let mut spec = parse(op);
    move_emoji(&mut spec);
    if spec.say.is_empty()
        && spec.sub.is_empty()
        && spec.big.is_empty()
        && spec.pic.is_empty()
        && spec.items.is_empty()
        && spec.data.is_empty()
        && spec.qr.is_empty()
    {
        return Err("A poster needs something to say. Set say, or another field.".into());
    }
    spec.sender = truncate(collapse(sender), 40);
    let ranked = rank(&spec);
    let mut wanted = if look_index(&spec.look).is_some() {
        spec.look.clone()
    } else {
        String::new()
    };
    if wanted == "block"
        && !spec.pic.is_empty()
        && !spec.say.is_empty()
        && craft::resolve_picture(&spec.pic, 40).is_some()
    {
        wanted = "picture".into(); // A small model often sets both. It means the subject should show.
    }
    let mut warnings: Vec<String> = Vec::new();
    if !spec.look.is_empty() && wanted.is_empty() && spec.look != "auto" {
        warnings.push(format!(
            "No look called {}, so I chose one.",
            py_repr(&spec.look)
        ));
    }
    let mut order: Vec<String> = Vec::new();
    if !wanted.is_empty() {
        order.push(wanted.clone());
    }
    order.extend(ranked.iter().filter(|n| **n != wanted).cloned());
    let position = |name: &str| order.iter().position(|n| n == name).unwrap_or(usize::MAX);
    let block_at = position("block");
    let mut filtered: Vec<String> = order
        .iter()
        .filter(|name| name.as_str() != "quote" || position(name) < block_at)
        .cloned()
        .collect();
    filtered.push("quote".into());
    let order = filtered;
    for name in &order {
        let Some(index) = look_index(name) else {
            continue;
        };
        let look = &LOOKS[index];
        let mut page = Page::new(Report::new(name));
        let mut folded = fold(&spec, look);
        if (look.run)(&mut page, &mut folded).is_err() {
            if *name == wanted {
                warnings.push(format!(
                    "Look {name} can't hold this legibly, so I used another. Fewer words would let it fit."
                ));
            }
            continue;
        }
        let Page { image, mut report } = page;
        let image = thin_ink(raster::threshold(&image, 128), MAX_INK);
        if raster::histogram(&image)[0] as f64 / (W * H) as f64 > 0.6 && name != "quote" {
            if *name == wanted {
                warnings.push(format!(
                    "Look {name} would be too dark with this content, so I used another."
                ));
            }
            continue;
        }
        let mut all = warnings.clone();
        all.append(&mut report.warnings);
        report.warnings = all;
        report.alternatives = ranked
            .iter()
            .take(4)
            .filter(|other| *other != name)
            .take(3)
            .cloned()
            .collect();
        if !spec.pic.is_empty() && !look.shows("pic") {
            report
                .warnings
                .push(format!("Look {name} doesn't show a pic."));
        }
        if !spec.qr.is_empty() && !look.shows("qr") {
            report
                .warnings
                .push(format!("Look {name} doesn't show a QR code. Use ticket."));
        }
        if !spec.pic.is_empty()
            && (name == "picture" || name == "stage")
            && craft::resolve_picture(&spec.pic, 40).is_none()
        {
            report.warnings.push(format!(
                "No picture for {}. Try an emoji.",
                py_repr(&spec.pic)
            ));
        }
        return Ok((image, report));
    }
    Err("quote always fits".into())
}

/// The same content in the looks that suit it best, numbered, on one sheet to look at. Returns the
/// PNG and the list of what is on it.
pub fn compare(op: &Value, count: usize, sender: &str) -> Result<(Vec<u8>, String), String> {
    let spec = parse(op);
    let n = count.clamp(2, 6);
    let names: Vec<String> = rank(&spec).into_iter().take(n).collect();
    let mut drawn: Vec<(String, GrayImage)> = Vec::new();
    for name in names {
        let mut with_look = op.clone();
        if let Value::Object(map) = &mut with_look {
            map.insert("look".into(), Value::String(name.clone()));
        } else {
            with_look = serde_json::json!({ "look": name });
        }
        let (image, report) = render(&with_look, sender)?;
        if report.look == name {
            drawn.push((name, image));
        }
    }
    let columns = if drawn.len() <= 4 { 2 } else { 3 };
    let rows = drawn.len().div_ceil(columns);
    let gap = 8;
    let sheet_w = columns as i32 * (W + gap) + gap;
    let sheet_h = rows as i32 * (H + gap + 16) + gap;
    let mut sheet = raster::filled(sheet_w as u32, sheet_h as u32, 200);
    let font = crate::fonts::pixel_face();
    for (index, (name, image)) in drawn.iter().enumerate() {
        let index = index as i32;
        let x = gap + index % columns as i32 * (W + gap);
        let y = gap + fdiv(index, columns as i32) * (H + gap + 16);
        font.draw(
            &mut sheet,
            (x + 2) as f64,
            y as f64,
            &format!("{} {}", index + 1, name),
            0,
            false,
        );
        raster::paste(&mut sheet, image, x, y + 14, None);
    }
    let mut png = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(
            sheet.as_raw(),
            sheet.width(),
            sheet.height(),
            image::ExtendedColorType::L8,
        )
        .map_err(|e| e.to_string())?;
    }
    let listing = drawn
        .iter()
        .enumerate()
        .map(|(i, (name, _))| format!("{} {}", i + 1, name))
        .collect::<Vec<_>>()
        .join(", ");
    Ok((png, listing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn black(image: &GrayImage) -> f64 {
        raster::ink_fraction(image)
    }

    fn emoji_available() -> bool {
        assets::emoji_rgba("\u{1F409}", 160).is_some()
    }

    /// The display faces the Python reference was drawn with.
    fn display_fonts_available() -> bool {
        let files = crate::fonts::font_files();
        [
            "impact",
            "din condensed bold",
            "futura",
            "didot",
            "bodoni 72",
            "georgia bold",
            "rockwell",
            "menlo",
            "arial black",
            "courier new bold",
            "krungthep",
            "americantypewriter",
        ]
        .iter()
        .all(|stem| files.contains_key(*stem))
    }

    fn full() -> Value {
        let mut op = json!({
            "say": "Deploy blocked", "sub": "Approve the migration", "big": "14:00",
            "data": [1, 5, 2, 8, 3], "items": ["ROOM: 4", "WITH: Ida"], "qr": "https://example.com/x"
        });
        if emoji_available() {
            op["pic"] = json!("\u{1F409}");
        }
        op
    }

    fn awkward() -> Vec<Value> {
        vec![
            json!({"say": "x"}),
            json!({"say": "A".repeat(120)}),
            json!({"say": "word ".repeat(200)}),
            json!({"say": "Husk å ringe mormor på søndag", "sub": "Øl og blåbær, ß"}),
            json!({"say": "締め切りは金曜日", "sub": "レポート"}),
            json!({"say": "مرحبا بالعالم"}),
            json!({"say": "🎉🎉🎉"}),
            json!({"say": "line\nbreak\ttab"}),
            json!({"say": ["TWO", "LINES"], "sub": "a list of lines"}),
            json!({"say": 12345, "items": "nope", "data": ["x", null, f64::NAN], "seed": "z", "big": 7}),
            json!({"say": "Pic that doesn't exist", "pic": "qwertyuiop"}),
            json!({"items": (0..12).map(|n| format!("Thing {n}")).collect::<Vec<_>>()}),
            json!({"data": [5]}),
            json!({"data": [3, 3, 3, 3]}),
            json!({"say": "Long detail", "sub": "word ".repeat(60)}),
            json!({"say": "Many", "items": vec!["a very long item that goes on and on past any line width at all"; 6]}),
        ]
    }

    fn with_look(look: &str, mut op: Value) -> Value {
        op["look"] = json!(look);
        op
    }

    fn shown(report: &Report) -> String {
        report.shown.join(" ")
    }

    // --- the promises every look makes ----------------------------------------------------------

    #[test]
    fn every_look_with_everything_is_on_the_grid_and_safe_to_print() {
        let all = full();
        for look in LOOKS.iter() {
            let mut op = serde_json::Map::new();
            for (key, value) in all.as_object().unwrap() {
                let data_look = key == "data" && (look.name == "rain" || look.name == "ridges");
                if look.shows(key) || data_look {
                    op.insert(key.clone(), value.clone());
                }
            }
            let (image, report) = render(&with_look(look.name, Value::Object(op)), "").unwrap();
            assert_eq!(image.dimensions(), (312, 216));
            assert!(image.as_raw().iter().all(|&v| v == 0 || v == 255));
            assert_eq!(report.look, look.name, "{}", report.summary());
            assert!(
                black(&image) <= 0.60,
                "{} is {:.0}% black",
                look.name,
                black(&image) * 100.0
            );
            assert!(
                report.min_cap >= MIN_CAP,
                "{} cap {}",
                look.name,
                report.min_cap
            );
        }
    }

    #[test]
    fn no_content_can_break_a_look() {
        let names = look_names();
        for look in &names {
            for (n, case) in awkward().into_iter().enumerate() {
                let (image, report) = render(&with_look(look, case), "").unwrap();
                assert_eq!(image.dimensions(), (312, 216));
                assert!(names.contains(&report.look));
                assert!(black(&image) <= 0.60, "{look} case {n}");
                assert!(black(&image) > 0.002, "{look} case {n} came out blank");
            }
        }
    }

    #[test]
    fn no_words_are_lost_in_any_look() {
        let fields = json!({"say": "Launch review", "sub": "Bring the slides", "big": "14:00", "items": ["ROOM: 4", "WITH: Ida"]});
        for name in look_names() {
            let (_, report) = render(&with_look(&name, fields.clone()), "").unwrap();
            let text = shown(&report).to_uppercase();
            let warnings = report.warnings.join(" ");
            assert!(
                report.look != name || !warnings.contains("cut off"),
                "{name}"
            );
            for word in ["LAUNCH", "REVIEW", "SLIDES", "14", "IDA"] {
                assert!(
                    text.contains(word),
                    "{word} missing from {} (asked for {name}): {text}",
                    report.look
                );
            }
        }
    }

    #[test]
    fn nothing_to_say_is_an_error_the_model_can_fix() {
        for empty in [
            json!({}),
            json!({"say": ""}),
            json!({"say": "   ", "items": [], "data": []}),
        ] {
            let error = render(&empty, "").unwrap_err();
            assert!(error.contains("something to say"), "{error}");
        }
    }

    #[test]
    fn an_emoji_in_the_words_becomes_the_picture() {
        let (_, report) = render(&json!({"look": "picture", "say": "Dragon show 🐉"}), "").unwrap();
        if emoji_available() {
            assert_eq!(report.look, "picture");
        }
        assert!(!report.shown.iter().any(|t| t.contains('🐉')));
    }

    #[test]
    fn a_qr_code_outside_a_ticket_is_flagged_not_printed_as_a_url() {
        let (_, report) = render(
            &json!({"look": "block", "say": "Join", "qr": "https://example.com"}),
            "",
        )
        .unwrap();
        assert!(report.summary().contains("QR"));
        assert!(!report.shown.iter().any(|t| t.contains("example.com")));
    }

    #[test]
    fn a_number_a_look_cant_feature_isnt_printed_twice() {
        let (_, report) = render(
            &json!({"look": "alarm", "say": "CI failing", "sub": "since 3pm", "big": "3pm"}),
            "",
        )
        .unwrap();
        assert_eq!(shown(&report).to_uppercase().matches("3PM").count(), 1);
        let (_, report) = render(
            &json!({"look": "block", "say": "10,000 users", "big": "10,000"}),
            "",
        )
        .unwrap();
        assert_eq!(shown(&report).matches("10,000").count(), 1);
    }

    #[test]
    fn a_long_value_is_never_cut_short() {
        let (_, report) = render(
            &json!({"look": "alarm", "say": "Tax return", "big": "April 30"}),
            "",
        )
        .unwrap();
        assert!(shown(&report).to_uppercase().contains("APRIL 30"));
    }

    #[test]
    fn a_win_features_its_number() {
        let (_, report) = render(
            &json!({"look": "monument", "say": "Sales up", "big": "24%"}),
            "",
        )
        .unwrap();
        assert_eq!(report.look, "monument");
        assert!(shown(&report).contains("24%"));
    }

    #[test]
    fn a_ticket_holds_a_date_a_place_and_its_fields() {
        let (_, report) = render(
            &json!({"look": "ticket", "say": "Concert", "sub": "Rockefeller", "big": "14 March", "items": ["Doors: 19:30", "Row: 4", "Seat: 12"]}),
            "",
        )
        .unwrap();
        assert_eq!(report.look, "ticket", "{}", report.summary());
    }

    #[test]
    fn a_subject_on_a_plain_statement_asks_for_a_picture() {
        if assets::emoji_rgba("🦈", 160).is_none() {
            return; // this machine has no emoji font
        }
        let (_, report) =
            render(&json!({"look": "block", "say": "Bite me", "pic": "🦈"}), "").unwrap();
        assert_eq!(report.look, "picture");
    }

    #[test]
    fn the_longer_field_is_the_quote_whichever_way_round() {
        let (_, report) = render(
            &json!({"look": "quote", "say": "Seneca", "sub": "We suffer more in imagination than in reality"}),
            "",
        )
        .unwrap();
        assert_eq!(report.look, "quote");
        assert!(shown(&report).to_uppercase().contains("SENECA"));
    }

    #[test]
    fn tonight_isnt_about_the_night_sky() {
        let spec = parse(&json!({"say": "Feed the starter", "sub": "Tonight", "pic": "🍞"}));
        assert_eq!(rank(&spec)[0], "picture");
    }

    #[test]
    fn text_too_long_for_any_note_says_so() {
        let (_, report) = render(&json!({"say": "word ".repeat(200)}), "").unwrap();
        assert_eq!(report.look, "quote");
        assert!(report.summary().contains("cut off"));
    }

    #[test]
    fn a_look_that_cant_fit_hands_over_and_says_why() {
        let (_, report) = render(
            &json!({"look": "relief", "say": "This is far too many words for relief to hold at any size"}),
            "",
        )
        .unwrap();
        assert_ne!(report.look, "relief");
        assert!(report.summary().to_lowercase().contains("fewer words"));
    }

    #[test]
    fn an_unknown_look_still_makes_a_note() {
        let (image, report) = render(&json!({"look": "wat", "say": "Hello"}), "").unwrap();
        assert!(look_names().contains(&report.look));
        assert!(report.summary().contains("wat"));
        assert!(black(&image) > 0.0);
    }

    #[test]
    fn the_same_fields_draw_the_same_pixels_and_a_seed_varies_them() {
        let fields = json!({"look": "cover", "say": "Monday", "date": "1 JAN"});
        let a = render(&fields, "").unwrap().0;
        let b = render(&fields, "").unwrap().0;
        assert_eq!(a.as_raw(), b.as_raw());
        let mut other = fields.clone();
        other["seed"] = json!(2);
        assert_ne!(a.as_raw(), render(&other, "").unwrap().0.as_raw());
    }

    #[test]
    fn a_model_cant_say_who_a_note_is_from() {
        let forged = json!({"look": "post", "say": "Hi", "date": "1 JAN", "sender": "Your boss", "from": "Your boss"});
        let plain = render(&json!({"look": "post", "say": "Hi", "date": "1 JAN"}), "")
            .unwrap()
            .0;
        assert_eq!(render(&forged, "").unwrap().0.as_raw(), plain.as_raw());
        assert_eq!(parse(&forged).sender, "");
        let (_, report) = render(&forged, "Sam").unwrap();
        assert!(report.shown.iter().any(|t| t == "SAM"));
        assert!(!report.shown.iter().any(|t| t == "YOUR BOSS"));
    }

    #[test]
    fn an_untrusted_design_cant_reach_files_through_a_poster() {
        let dir = std::env::temp_dir().join(format!("prins-poster-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret.png");
        std::fs::write(&secret, b"not an image").unwrap();
        let path = secret.to_string_lossy().to_string();
        assert!(craft::resolve_picture(&path, 200).is_none());
        let (_, report) = render(&json!({"look": "picture", "say": "x", "pic": path}), "").unwrap();
        assert_ne!(report.look, "picture");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn any_language_avoids_missing_glyph_boxes() {
        let latin = crate::fonts::craft_face_for("impact", "ÆØÅ", 40);
        assert_eq!(
            latin.style_name(),
            crate::fonts::craft_face("impact", 40).style_name()
        );
        if crate::fonts::craft_has_glyph("impact", '締') {
            return; // this machine's Impact covers CJK, nothing to fall back from
        }
        let cjk = crate::fonts::craft_face_for("impact", "締切", 40);
        assert!(cjk.has_glyph('締'), "no fallback face draws the characters");
    }

    // --- choosing -------------------------------------------------------------------------------

    #[test]
    fn without_a_look_the_engine_chooses_well() {
        let cases: Vec<(Value, &str)> = vec![
            (json!({"say": "Server down!"}), "alarm"),
            (json!({"say": "We shipped it"}), "monument"),
            (json!({"say": "Happy birthday Nora"}), "post"),
            (json!({"say": "Focus"}), "relief"),
            (json!({"say": "Dentist 09:30"}), "collide"),
            (json!({"say": "3 days left"}), "numeral"),
            (json!({"say": "Goodnight"}), "engraving"),
            (json!({"say": "Vet visit", "pic": "🐈"}), "picture"),
            (
                json!({"say": "Groceries", "items": ["Milk", "Eggs"]}),
                "list",
            ),
            (
                json!({"say": "Train", "items": ["FROM: Oslo", "TO: Bergen"]}),
                "ticket",
            ),
            (json!({"say": "Link", "qr": "https://x.no"}), "ticket"),
            (
                json!({"big": "147", "say": "commits", "data": [1, 2, 3]}),
                "ridges",
            ),
            (json!({"say": "Rain later", "data": [0, 0, 1]}), "rain"),
            (json!({"say": "Merged"}), "transit"),
            (json!({"say": "Call Sam back"}), "ransom"),
            (json!({"say": "Demo tomorrow"}), "stage"),
            (json!({"say": "Thursday"}), "cover"),
            (
                json!({"say": "The best time to plant a tree was twenty years ago, the second best is now"}),
                "quote",
            ),
            (json!({"say": "Something plain"}), "block"),
        ];
        for (fields, expected) in cases {
            assert_eq!(rank(&parse(&fields))[0], expected, "{fields}");
            if fields.get("pic").is_some() && !emoji_available() {
                continue;
            }
            assert_eq!(render(&fields, "").unwrap().1.look, expected, "{fields}");
        }
    }

    #[test]
    fn looks_that_lack_what_they_need_never_come_first() {
        let first = &rank(&parse(&json!({"say": "Hello there"})))[0];
        assert!(!["picture", "list", "numeral", "collide"].contains(&first.as_str()));
    }

    #[test]
    fn the_look_guide_and_catalogue_list_every_look() {
        let joined: Vec<String> = LOOKS
            .iter()
            .map(|l| format!("{}={}", l.name, l.when))
            .collect();
        assert_eq!(LOOK_GUIDE, joined.join("; "));
        assert_eq!(looks().len(), 18);
        assert_eq!(look_names().len(), 18);
        assert!(looks()
            .iter()
            .all(|(name, when)| !name.is_empty() && !when.is_empty()));
    }

    #[test]
    fn compare_shows_looks_without_touching_the_note() {
        let (png, listing) = compare(&json!({"say": "Dentist 09:30"}), 3, "").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(listing.contains("collide"), "{listing}");
        assert!(compare(&json!({}), 3, "").is_err());
    }

    // --- reading the fields ---------------------------------------------------------------------

    #[test]
    fn parse_is_forgiving_in_the_same_ways_python_is() {
        let spec = parse(&json!({
            "say": ["a", "b "], "s": "x", "data": ["1.5", " 2 ", true, "x", null, [1]],
            "items": ["a", "", 3, null, 1.5], "seed": "12", "qr": " u r l ", "big": "1234567890123456",
            "pic": "p".repeat(50), "look": " BLOCK ", "date": " 1  JAN "
        }));
        assert_eq!(spec.look, "block");
        assert_eq!(spec.say, "a b");
        assert_eq!(spec.lines, Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(spec.big, "123456789012");
        assert_eq!(spec.pic, "p".repeat(40));
        assert_eq!(spec.data, vec![1.5, 2.0, 1.0]);
        assert_eq!(spec.items, vec!["a", "3", "1.5"]);
        assert_eq!(spec.qr, " u r l ");
        assert_eq!(spec.seed, 12);
        assert_eq!(spec.date, "1 JAN");
        let spec =
            parse(&json!({"s": "short", "seed": 2.9, "items": "nope", "data": "no", "say": null}));
        assert_eq!(
            (
                spec.say.as_str(),
                spec.seed,
                spec.items.len(),
                spec.data.len()
            ),
            ("", 2, 0, 0)
        );
        let spec = parse(&json!({"s": "short"}));
        assert_eq!(spec.say, "short");
    }

    #[test]
    fn seeds_give_the_same_random_numbers_as_python() {
        // (say, sub, big, seed, random(), randrange(1, 6), uniform(0, 1), randint(-2, 2)), from Python 3.12.
        let cases: [(&str, &str, &str, i128, f64, i32, f64, i32); 4] = [
            (
                "Monday",
                "",
                "",
                0,
                0.29102212718507503,
                3,
                0.19683645878325218,
                1,
            ),
            (
                "Monday",
                "",
                "",
                2,
                0.9725288568529397,
                2,
                0.5389372632136745,
                2,
            ),
            (
                "Husk å ringe 締め",
                "Øl",
                "3",
                7,
                0.8941019671981603,
                2,
                0.600485071143344,
                -2,
            ),
            (
                "q\"uote\\ \u{1} é",
                "",
                "",
                -3,
                0.956355214374731,
                1,
                0.46264201175199027,
                0,
            ),
        ];
        for (say, sub, big, seed, a, b, c, d) in cases {
            let spec = Spec {
                say: say.into(),
                sub: sub.into(),
                big: big.into(),
                seed,
                ..Spec::default()
            };
            let mut rng = spec.rng();
            assert_eq!(rng.random(), a, "{say}");
            assert_eq!(randrange(&mut rng, 1, 6), b);
            assert_eq!(rng.uniform(0.0, 1.0), c);
            assert_eq!(randint(&mut rng, -2, 2), d);
        }
    }

    #[test]
    fn numbers_times_and_emoji_are_picked_out_of_the_words() {
        let mut spec = parse(&json!({"say": "Dentist 09:30 today"}));
        assert_eq!(pull_number(&mut spec), "09:30");
        assert_eq!(spec.say, "Dentist today");
        let mut spec = parse(&json!({"say": "Up 24% this year"}));
        assert_eq!(pull_number(&mut spec), "24%");
        let mut spec = parse(&json!({"say": "Order 1234567 shipped"}));
        assert_eq!(pull_number(&mut spec), "");
        assert!(time_full("10:15") && !time_full("10:155") && !time_full("at 10:15"));
        let mut spec = parse(&json!({"say": "Party 🎉 tonight", "sub": "bring 🍕"}));
        move_emoji(&mut spec);
        assert_eq!(
            (spec.pic.as_str(), spec.say.as_str(), spec.sub.as_str()),
            ("🎉", "Party tonight", "bring")
        );
    }

    #[test]
    fn a_ticket_qr_matches_what_segno_makes() {
        // Matrices from segno 1.6.6, error="m", boost_error=False: (text, side, sha256 of the rows joined by "/").
        // Micro QR for short text, then QR with segno's padding and mask choice.
        let sides = [
            ("https://x.no", 17usize),
            ("HELLO", 13),
            ("12345", 13),
            ("a", 15),
            ("https://example.com/x", 25),
            ("https://example.com/some/long/path?x=1&y=2", 29),
        ];
        for (text, side) in sides {
            assert_eq!(qr_matrix(text).unwrap().0, side, "{text}");
        }
        let (side, modules) = qr_matrix("HELLO").unwrap();
        let rows: Vec<String> = (0..side)
            .map(|j| {
                (0..side)
                    .map(|i| if modules[j * side + i] { '#' } else { '.' })
                    .collect()
            })
            .collect();
        assert_eq!(rows[0], "#######.#.#.#");
        assert_eq!(rows[1], "#.....#..#...");
        assert_eq!(rows[2], "#.###.#.#####");
    }

    // --- Python's own renders -------------------------------------------------------------------

    const REFERENCE: &str = r##"{"n":"block1","op":{"look":"block","say":"Ship it today"},"s":"","look":"block","alt":["monument","cover","alarm"],"warn":[],"cap":83,"ink":0.4557}
{"n":"block2","op":{"look":"block","say":"Deploy blocked","sub":"Approve the migration"},"s":"","look":"block","alt":["alarm","relief","monument"],"warn":[],"cap":25,"ink":0.4077}
{"n":"block3","op":{"look":"block","say":"Call mom","sub":"before dinner tonight please remember"},"s":"","look":"block","alt":["ransom","alarm","relief"],"warn":[],"cap":23,"ink":0.3184}
{"n":"alarm1","op":{"look":"alarm","say":"Server down!","sub":"since 3pm"},"s":"","look":"alarm","alt":["block","relief","monument"],"warn":[],"cap":25,"ink":0.2935}
{"n":"alarm2","op":{"look":"alarm","say":"Tax return","big":"April 30"},"s":"","look":"alarm","alt":["numeral","block","relief"],"warn":[],"cap":25,"ink":0.329}
{"n":"alarm3","op":{"say":["TWO","LINES"],"look":"alarm"},"s":"","look":"alarm","alt":["block","relief","monument"],"warn":[],"cap":69,"ink":0.4769}
{"n":"relief1","op":{"look":"relief","say":"Focus"},"s":"","look":"relief","alt":["block","monument","alarm"],"warn":[],"cap":178,"ink":0.4407}
{"n":"relief2","op":{"look":"relief","say":"Breathe in","sub":"and out"},"s":"","look":"relief","alt":["block","alarm","monument"],"warn":[],"cap":22,"ink":0.453}
{"n":"relief3","op":{"look":"relief","say":"This is far too many words for relief to hold at any size"},"s":"","look":"quote","alt":["block","alarm","relief"],"warn":["Look relief can't hold this legibly, so I used another. Fewer words would let it fit."],"cap":37,"ink":0.0736}
{"n":"monument1","op":{"look":"monument","say":"Sales up","big":"24%"},"s":"","look":"monument","alt":["numeral","block","alarm"],"warn":[],"cap":76,"ink":0.3715}
{"n":"monument2","op":{"look":"monument","say":"We shipped it","sub":"v2.0 is out","seed":3},"s":"","look":"monument","alt":["block","alarm","relief"],"warn":[],"cap":12,"ink":0.3623}
{"n":"ransom1","op":{"look":"ransom","say":"Call Sam back"},"s":"","look":"ransom","alt":["block","alarm","relief"],"warn":[],"cap":33,"ink":0.4001}
{"n":"ransom2","op":{"look":"ransom","say":"Pay rent","sub":"by Friday","seed":5},"s":"","look":"ransom","alt":["cover","block","alarm"],"warn":[],"cap":18,"ink":0.4086}
{"n":"numeral1","op":{"look":"numeral","say":"days left","big":"3"},"s":"","look":"numeral","alt":["block","alarm","relief"],"warn":[],"cap":68,"ink":0.2739}
{"n":"numeral2","op":{"look":"numeral","say":"to launch","big":"14","sub":"sleep well"},"s":"","look":"numeral","alt":["engraving","block","alarm"],"warn":[],"cap":26,"ink":0.168}
{"n":"numeral3","op":{"look":"numeral","say":"commits","big":"1,247"},"s":"","look":"numeral","alt":["block","relief","monument"],"warn":[],"cap":65,"ink":0.161}
{"n":"numeral4","op":{"look":"numeral","say":"3 days left"},"s":"","look":"numeral","alt":["block","alarm","relief"],"warn":[],"cap":68,"ink":0.2739}
{"n":"collide1","op":{"look":"collide","say":"Dentist 09:30"},"s":"","look":"collide","alt":["block","alarm","relief"],"warn":[],"cap":20,"ink":0.5264}
{"n":"collide2","op":{"look":"collide","say":"Standup","big":"10:15","sub":"Room 4"},"s":"","look":"collide","alt":["block","relief","monument"],"warn":[],"cap":17,"ink":0.5382}
{"n":"picture1","op":{"look":"picture","say":"Dragon show","pic":"🐉"},"s":"","look":"picture","alt":["stage","engraving","block"],"warn":[],"cap":39,"ink":0.3444}
{"n":"picture2","op":{"look":"picture","say":"Cake time","pic":"cake","sub":"at four"},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":14,"ink":0.2887}
{"n":"picture3","op":{"look":"picture","pic":"star"},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":999,"ink":0.2027}
{"n":"picture4","op":{"look":"picture","say":"Vet visit","pic":"🐈","seed":9},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":59,"ink":0.3161}
{"n":"engraving1","op":{"look":"engraving","say":"Goodnight"},"s":"","look":"engraving","alt":["block","relief","monument"],"warn":[],"cap":25,"ink":0.5868}
{"n":"engraving2","op":{"look":"engraving","say":"Sleep well","pic":"🐉","sub":"see you"},"s":"","look":"engraving","alt":["picture","block","stage"],"warn":[],"cap":13,"ink":0.5881}
{"n":"engraving3","op":{"look":"engraving","say":"Quiet now","pic":"moon","sub":"tonight"},"s":"","look":"engraving","alt":["relief","alarm","picture"],"warn":[],"cap":16,"ink":0.5874}
{"n":"stage1","op":{"look":"stage","say":"Demo tomorrow"},"s":"","look":"stage","alt":["block","alarm","relief"],"warn":[],"cap":18,"ink":0.588}
{"n":"stage2","op":{"look":"stage","say":"Concert","pic":"🎸","sub":"Rockefeller"},"s":"","look":"stage","alt":["picture","engraving","block"],"warn":[],"cap":12,"ink":0.5119}
{"n":"post1","op":{"look":"post","say":"Happy birthday Nora","date":"1 JAN"},"s":"Sam","look":"post","alt":["block","alarm","relief"],"warn":[],"cap":11,"ink":0.1432}
{"n":"post2","op":{"look":"post","say":"Thanks a lot","sub":"for the help","date":"24 DEC"},"s":"Sam","look":"post","alt":["block","alarm","relief"],"warn":[],"cap":11,"ink":0.1355}
{"n":"ticket1","op":{"look":"ticket","say":"Concert","sub":"Rockefeller","big":"14 March","items":["Doors: 19:30","Row: 4","Seat: 12"]},"s":"","look":"ticket","alt":["numeral","stage","block"],"warn":[],"cap":11,"ink":0.3131}
{"n":"ticket2","op":{"look":"ticket","say":"Train","items":["FROM: Oslo","TO: Bergen"],"pic":"🚆"},"s":"","look":"ticket","alt":["transit","picture","engraving"],"warn":[],"cap":11,"ink":0.2875}
{"n":"ticket3","op":{"look":"ticket","say":"Link","qr":"https://example.com/x"},"s":"","look":"ticket","alt":["block","relief","monument"],"warn":[],"cap":17,"ink":0.2448}
{"n":"rain1","op":{"look":"rain","say":"Rain later","data":[0,0,1]},"s":"","look":"rain","alt":["ridges","block","alarm"],"warn":[],"cap":64,"ink":0.5912}
{"n":"rain2","op":{"look":"rain","say":"Weekly","data":[1,5,2,8,3],"items":["Mon","Tue","Wed","Thu","Fri"],"sub":"visits"},"s":"","look":"rain","alt":["list","ridges","block"],"warn":[],"cap":12,"ink":0.5897}
{"n":"ridges1","op":{"look":"ridges","big":"147","say":"commits","data":[1,2,3]},"s":"","look":"ridges","alt":["rain","block","relief"],"warn":[],"cap":52,"ink":0.5484}
{"n":"ridges2","op":{"look":"ridges","say":"Total","sub":"this year","data":[3,1,4,1,5,9]},"s":"","look":"ridges","alt":["rain","block","relief"],"warn":[],"cap":23,"ink":0.5901}
{"n":"transit1","op":{"look":"transit","say":"Merged","items":["main","dev"]},"s":"","look":"transit","alt":["list","block","relief"],"warn":[],"cap":12,"ink":0.2401}
{"n":"transit2","op":{"look":"transit","say":"Route open","sub":"commute","items":["A","B","C"]},"s":"","look":"transit","alt":["list","block","alarm"],"warn":[],"cap":12,"ink":0.2165}
{"n":"cover1","op":{"look":"cover","say":"Monday","date":"1 JAN"},"s":"","look":"cover","alt":["block","relief","monument"],"warn":[],"cap":37,"ink":0.4286}
{"n":"cover2","op":{"look":"cover","say":"Good morning","sub":"coffee first","seed":2},"s":"","look":"cover","alt":["block","alarm","relief"],"warn":[],"cap":15,"ink":0.4616}
{"n":"list1","op":{"look":"list","say":"Groceries","items":["Milk","Eggs","Bread"]},"s":"","look":"list","alt":["block","relief","transit"],"warn":[],"cap":25,"ink":0.1967}
{"n":"list2","op":{"look":"list","say":"Todo","items":["Thing 0","Thing 1","Thing 2","Thing 3","Thing 4","Thing 5","Thing 6","Thing 7"],"sub":"today"},"s":"","look":"list","alt":["cover","block","relief"],"warn":[],"cap":20,"ink":0.2772}
{"n":"quote1","op":{"look":"quote","say":"The best time to plant a tree was twenty years ago, the second best is now","sub":"Proverb"},"s":"","look":"quote","alt":["alarm","block","relief"],"warn":[],"cap":22,"ink":0.151}
{"n":"quote2","op":{"look":"quote","say":"Seneca","sub":"We suffer more in imagination than in reality"},"s":"","look":"quote","alt":["block","relief","monument"],"warn":[],"cap":22,"ink":0.0733}
{"n":"quote3","op":{"say":"word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word "},"s":"","look":"quote","alt":["block","alarm","relief"],"warn":["The text was too long to fit, so the end was cut off. Shorten it."],"cap":12,"ink":0.1676}
{"n":"auto1","op":{"say":"Server down!"},"s":"","look":"alarm","alt":["block","relief","monument"],"warn":[],"cap":72,"ink":0.4725}
{"n":"auto2","op":{"say":"Happy birthday Nora","date":"2 FEB"},"s":"","look":"post","alt":["block","alarm","relief"],"warn":[],"cap":44,"ink":0.1339}
{"n":"auto3","op":{"say":"Dentist 09:30"},"s":"","look":"collide","alt":["block","alarm","relief"],"warn":[],"cap":20,"ink":0.5264}
{"n":"auto4","op":{"say":"3 days left"},"s":"","look":"numeral","alt":["block","alarm","relief"],"warn":[],"cap":68,"ink":0.2739}
{"n":"auto5","op":{"say":"Groceries","items":["Milk","Eggs"]},"s":"","look":"list","alt":["block","relief","transit"],"warn":[],"cap":25,"ink":0.1807}
{"n":"auto6","op":{"say":"Something plain"},"s":"","look":"block","alt":["alarm","relief","monument"],"warn":[],"cap":57,"ink":0.4179}
{"n":"auto7","op":{"say":"Husk å ringe mormor på søndag","sub":"Øl og blåbær, ß"},"s":"","look":"cover","alt":["block","alarm","relief"],"warn":[],"cap":18,"ink":0.5208}
{"n":"auto8","op":{"say":"締め切りは金曜日","sub":"レポート"},"s":"","look":"block","alt":["relief","monument","alarm"],"warn":[],"cap":25,"ink":0.3163}
{"n":"auto9","op":{"say":"مرحبا بالعالم"},"s":"","look":"block","alt":["alarm","relief","monument"],"warn":[],"cap":69,"ink":0.4182}
{"n":"auto10","op":{"say":"🎉🎉🎉"},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":999,"ink":0.1589}
{"n":"auto11","op":{"say":"Dragon show 🐉"},"s":"","look":"stage","alt":["picture","engraving","block"],"warn":[],"cap":25,"ink":0.5312}
{"n":"auto12","op":{"say":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},"s":"","look":"quote","alt":["relief","monument","block"],"warn":[],"cap":44,"ink":0.0412}
{"n":"auto13","op":{"say":"x"},"s":"","look":"block","alt":["relief","monument","alarm"],"warn":[],"cap":196,"ink":0.4264}
{"n":"auto14","op":{"data":[5]},"s":"","look":"ridges","alt":["rain","block","alarm"],"warn":[],"cap":999,"ink":0.5889,"sha":"8accb322c30b251118f96c4b4f9d49615e3ce675cbec1cca399a4959d9790d09"}
{"n":"auto15","op":{"data":[3,3,3,3]},"s":"","look":"ridges","alt":["rain","block","alarm"],"warn":[],"cap":999,"ink":0.5922,"sha":"e52fdffe9c8bfef173dc741aa1aa4c2e01c6aba2f8446e8d1be79a3adff29537"}
{"n":"auto16","op":{"say":"Pic that doesn't exist","pic":"qwertyuiop"},"s":"","look":"block","alt":["picture","engraving","stage"],"warn":["Look block doesn't show a pic."],"cap":66,"ink":0.482}
{"n":"auto17","op":{"say":12345,"items":"nope","data":["x",null,"nan"],"seed":"z","big":7},"s":"","look":"numeral","alt":["block","relief","monument"],"warn":[],"cap":37,"ink":0.1468}
{"n":"auto18","op":{"say":"Join","qr":"https://example.com","look":"block"},"s":"","look":"block","alt":["ticket","relief","monument"],"warn":["Look block doesn't show a QR code. Use ticket."],"cap":196,"ink":0.1757}
{"n":"auto19","op":{"say":"CI failing","sub":"since 3pm","big":"3pm","look":"alarm"},"s":"","look":"alarm","alt":["numeral","block","relief"],"warn":[],"cap":25,"ink":0.3652}
{"n":"auto20","op":{"say":"Many","items":["a very long item that goes on and on past any line width at all","a very long item that goes on and on past any line width at all","a very long item that goes on and on past any line width at all","a very long item that goes on and on past any line width at all","a very long item that goes on and on past any line width at all","a very long item that goes on and on past any line width at all"]},"s":"","look":"list","alt":["block","relief","transit"],"warn":[],"cap":30,"ink":0.2961}
{"n":"auto21","op":{"say":"Bite me","pic":"🦈","look":"block"},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":70,"ink":0.4048}
{"n":"auto22","op":{"say":"Long detail","sub":"word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word "},"s":"","look":"monument","alt":["block","alarm","relief"],"warn":[],"cap":12,"ink":0.2468}
{"n":"auto23","op":{"look":"wat","say":"Hello"},"s":"","look":"post","alt":["block","relief","monument"],"warn":["No look called 'wat', so I chose one."],"cap":44,"ink":0.1031}
{"n":"auto24","op":{"say":"Focus"},"s":"","look":"relief","alt":["block","monument","alarm"],"warn":[],"cap":178,"ink":0.4407}
{"n":"auto25","op":{"say":"Rain later","data":[0,0,1]},"s":"","look":"rain","alt":["ridges","block","alarm"],"warn":[],"cap":64,"ink":0.5912}
{"n":"auto26","op":{"say":"We shipped it","pic":"pizza"},"s":"","look":"monument","alt":["picture","engraving","block"],"warn":["Look monument doesn't show a pic."],"cap":72,"ink":0.374}
{"n":"auto27","op":{"say":"Hello","qr":"HELLO"},"s":"","look":"post","alt":["ticket","block","relief"],"warn":["Look post doesn't show a QR code. Use ticket."],"cap":44,"ink":0.1031}
{"n":"plain1","op":{"look":"picture","pic":"hexagon"},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":999,"ink":0.11,"sha":"3e13a73e64b03325dfc7c88184878e4d20298429cef30375d5d1168738f10067"}
{"n":"plain2","op":{"look":"picture","pic":"burst","seed":4},"s":"","look":"picture","alt":["engraving","block","stage"],"warn":[],"cap":999,"ink":0.1265,"sha":"713132c8ac8b61654cf8cf734dced0b91dbed74cd2712c29cf93ba4f70602327"}
{"n":"plain3","op":{"look":"ridges","data":[3,1,4,1,5,9,2,6]},"s":"","look":"ridges","alt":["rain","block","alarm"],"warn":[],"cap":999,"ink":0.59,"sha":"e2d401b8a8c58274b732e0c99d3e7a295b17947d379cf3db671101b9644922e0"}
{"n":"plain4","op":{"look":"ticket","qr":"HELLO"},"s":"","look":"ticket","alt":["block","alarm","relief"],"warn":[],"cap":99,"ink":0.2553,"sha":"43687a7709d35f0f9c10bbdd117a0c5fe3ed177ba811b4bba1022a5c588a0881"}
{"n":"plain5","op":{"look":"ticket","qr":"https://x.no"},"s":"","look":"ticket","alt":["block","alarm","relief"],"warn":[],"cap":99,"ink":0.2578,"sha":"89ac7578617b023d2da0f38b3eb24ebf9254f64a432c2fe91bc8213aa150b24d"}
{"n":"plain6","op":{"look":"ticket","qr":"https://example.com/some/long/path?x=1&y=2"},"s":"","look":"ticket","alt":["block","alarm","relief"],"warn":[],"cap":99,"ink":0.2638,"sha":"e4036ed26fdfc244777369632aafddde7e18a4bf92a7bf0b1205cc938d1b2866"}"##;

    /// Every spec rendered by Python 3.12 / Pillow 13 on macOS: the same look must win with the same
    /// alternatives and warnings, ink within 3 points of black (text edges differ from FreeType), the
    /// smallest capital within 2 px, and specs that draw no text or emoji pixel for pixel.
    #[test]
    fn the_same_specs_pick_the_same_looks_as_python() {
        if !emoji_available() || !display_fonts_available() {
            eprintln!("skipped: needs macOS's display fonts and emoji font");
            return;
        }
        let mut worst_ink = 0.0f64;
        for line in REFERENCE.lines() {
            let r: Value = serde_json::from_str(line).unwrap();
            let name = r["n"].as_str().unwrap();
            let strings = |key: &str| -> Vec<String> {
                r[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            };
            let (image, report) = render(&r["op"], r["s"].as_str().unwrap()).unwrap();
            assert_eq!(report.look, r["look"].as_str().unwrap(), "{name}");
            assert_eq!(report.alternatives, strings("alt"), "{name}");
            assert_eq!(report.warnings, strings("warn"), "{name}");
            let cap = r["cap"].as_i64().unwrap() as i32;
            if cap == 999 {
                assert_eq!(report.min_cap, 999, "{name}");
            } else {
                assert!(
                    (report.min_cap - cap).abs() <= 2,
                    "{name}: cap {} vs {cap}",
                    report.min_cap
                );
            }
            let ink_gap = (black(&image) - r["ink"].as_f64().unwrap()).abs();
            worst_ink = worst_ink.max(ink_gap);
            assert!(
                ink_gap <= 0.03,
                "{name}: ink {:.3} vs {:.3}",
                black(&image),
                r["ink"]
            );
            if let Some(sha) = r.get("sha") {
                let digest = hex::encode(Sha256::digest(image.as_raw()));
                assert_eq!(
                    digest,
                    sha.as_str().unwrap(),
                    "{name} should match Python pixel for pixel"
                );
            }
        }
        eprintln!("worst ink gap {worst_ink:.4}");
    }

    /// Needs the network: `PRINS_TEST_NETWORK=1 cargo test logo`.
    #[test]
    fn a_brand_logo_is_a_picture() {
        if std::env::var("PRINS_TEST_NETWORK")
            .map(|v| v != "1")
            .unwrap_or(true)
        {
            return;
        }
        let (gray, mask) = craft::resolve_picture("logo:spotify", 200).expect("the logo downloads");
        assert_eq!((gray.width(), gray.height()), (200, 200));
        // Python (resvg-py) draws 25839 mask pixels, all of the logo as black.
        let set = mask.as_raw().iter().filter(|&&v| v != 0).count() as f64;
        assert!((set - 25839.0).abs() / 25839.0 < 0.02, "{set} mask pixels");
        let (_, report) = render(
            &json!({"look": "picture", "say": "Now playing", "pic": "logo:spotify", "seed": 1}),
            "",
        )
        .unwrap();
        assert_eq!(report.look, "picture");
        assert!(report.warnings.is_empty());
        let (_, report) = render(
            &json!({"say": "Listen", "pic": "logo:nonexistentbrandxyz"}),
            "",
        )
        .unwrap();
        assert_eq!(report.look, "block");
        assert_eq!(report.warnings, ["Look block doesn't show a pic."]);
    }

    #[test]
    fn today_is_a_day_and_a_month_in_capitals() {
        let label = today_label();
        assert!(re!(r"\A\d{1,2} [A-Z]{3}\z").is_match(&label), "{label}");
    }

    #[test]
    fn rendering_every_look_is_fast_enough() {
        for name in look_names() {
            let started = std::time::Instant::now();
            let fields = json!({"say": "Launch review", "sub": "Bring the slides", "big": "14:00", "items": ["ROOM: 4", "WITH: Ida"], "data": [1, 3, 2, 5]});
            let (_, report) = render(&with_look(&name, fields), "").unwrap();
            eprintln!(
                "{name:10} -> {:10} {:>4} ms",
                report.look,
                started.elapsed().as_millis()
            );
        }
    }

    #[test]
    fn the_look_set_is_the_one_python_has() {
        let expected: BTreeSet<&str> = [
            "block",
            "alarm",
            "relief",
            "monument",
            "ransom",
            "numeral",
            "collide",
            "picture",
            "engraving",
            "stage",
            "post",
            "ticket",
            "rain",
            "ridges",
            "transit",
            "cover",
            "list",
            "quote",
        ]
        .into_iter()
        .collect();
        let got = look_names();
        assert_eq!(
            got.iter().map(|s| s.as_str()).collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(got[0], "block");
        assert_eq!(got[17], "quote");
    }
}
