//! Patterns: each decides, for one pixel, whether it is black. Port of `patterns.py`.
//! Use as a fill anywhere via ink: `name`, `name:size`, and `gray:N` for a flat N% tone.

use image::GrayImage;

pub const BAYER_8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// Ordered dithering: darkness 0 is white and 1 is black.
pub fn bayer(x: i32, y: i32, darkness: f64) -> bool {
    (BAYER_8[y.rem_euclid(8) as usize][x.rem_euclid(8) as usize] as f64 + 0.5) / 64.0 < darkness
}

/// Stable random value from 0 to 1 for an integer cell (`patterns._hash`).
pub fn hash(x: i64, y: i64) -> f64 {
    let value = (x
        .wrapping_mul(374_761_393)
        .wrapping_add(y.wrapping_mul(668_265_263))) as u32;
    let value = (value ^ (value >> 13)).wrapping_mul(1_274_126_177);
    (value ^ (value >> 16)) as f64 / 4_294_967_295.0
}

/// Stable random value from 0 to 1 for a pixel (the coordinates are truncated, like Python's `int()`).
pub fn noise(x: f64, y: f64) -> f64 {
    hash(x as i64, y as i64)
}

/// x, y, size, canvas width, canvas height.
type PatternFn = fn(i64, i64, i64, i64, i64) -> bool;

fn hypot(a: f64, b: f64) -> f64 {
    a.hypot(b)
}

/// Python's float `%` with a positive divisor.
fn fmod(a: f64, b: f64) -> f64 {
    a.rem_euclid(b)
}

fn bayer_f(x: i64, y: i64, darkness: f64) -> bool {
    bayer(x as i32, y as i32, darkness)
}

/// Pattern names in the order `patterns.py` lists them.
#[rustfmt::skip]
const ORDER: &[&str] = &[
    "checker",
    "stripes",
    "stripes-back",
    "hlines",
    "vlines",
    "grid",
    "dots",
    "polka",
    "hatch",
    "crosshatch",
    "brick",
    "waves",
    "zigzag",
    "scales",
    "diamonds",
    "plaid",
    "weave",
    "confetti",
    "noise",
    "rings",
    "rays",
    "spiral",
    "moire",
    "fade-right",
    "fade-left",
    "fade-down",
    "fade-up",
    "glow",
    "vignette",
];

fn lookup(name: &str) -> Option<PatternFn> {
    let f: PatternFn = match name {
        "checker" => |x, y, s, _, _| (x.div_euclid(s) + y.div_euclid(s)) % 2 == 0,
        "stripes" => |x, y, s, _, _| (x + y).rem_euclid(s * 2) < s,
        "stripes-back" => |x, y, s, _, _| (x - y).rem_euclid(s * 2) < s,
        "hlines" => |_, y, s, _, _| y.rem_euclid(s * 2) < s,
        "vlines" => |x, _, s, _, _| x.rem_euclid(s * 2) < s,
        "grid" => |x, y, s, _, _| x.rem_euclid(s * 2) == 0 || y.rem_euclid(s * 2) == 0,
        "dots" => |x, y, s, _, _| {
            let dx = (x.rem_euclid(s * 2) - s) as f64;
            let dy = (y.rem_euclid(s * 2) - s) as f64;
            let r = s as f64 * 0.55;
            dx * dx + dy * dy < r * r
        },
        "polka" => |x, y, s, _, _| {
            let shift = y.div_euclid(s * 2).rem_euclid(2) * s;
            let dx = ((x + shift).rem_euclid(s * 2) - s) as f64;
            let dy = (y.rem_euclid(s * 2) - s) as f64;
            let r = s as f64 * 0.6;
            dx * dx + dy * dy < r * r
        },
        "hatch" => |x, y, s, _, _| (x + y).rem_euclid(s * 2) == 0,
        "crosshatch" => {
            |x, y, s, _, _| (x + y).rem_euclid(s * 2) == 0 || (x - y).rem_euclid(s * 2) == 0
        }
        "brick" => |x, y, s, _, _| {
            y.rem_euclid(s * 2) == 0
                || (x + y.div_euclid(s * 2).rem_euclid(2) * s * 2).rem_euclid(s * 4) == 0
        },
        "waves" => |x, y, s, _, _| {
            let sf = s as f64;
            fmod(
                y as f64 + (x as f64 / (sf * 1.6)).sin() * sf * 1.5,
                (s * 3) as f64,
            ) < sf
        },
        "zigzag" => |x, y, s, _, _| (y + (x.rem_euclid(s * 4) - s * 2).abs()).rem_euclid(s * 3) < s,
        "scales" => |x, y, s, _, _| {
            let shift = y.div_euclid(s * 2).rem_euclid(2) * s;
            let dx = ((x + shift).rem_euclid(s * 2) - s) as f64;
            let dy = y.rem_euclid(s * 2) as f64;
            (hypot(dx, dy) - s as f64 * 0.95).abs() < 1.0
        },
        "diamonds" => |x, y, s, _, _| {
            let d = (x.rem_euclid(s * 2) - s).abs() + (y.rem_euclid(s * 2) - s).abs();
            (d as f64) < s as f64 * 0.7
        },
        "plaid" => |x, y, s, _, _| {
            x.rem_euclid(s * 4) < s || y.rem_euclid(s * 4) < s || (x + y).rem_euclid(4) == 0
        },
        "weave" => |x, y, s, _, _| {
            let checker = (x.div_euclid(s) + y.div_euclid(s)).rem_euclid(2) == 0;
            let half = s as f64 / 2.0;
            let lit = if x.div_euclid(s).rem_euclid(2) != 0 {
                (x.rem_euclid(s) as f64) < half
            } else {
                (y.rem_euclid(s) as f64) < half
            };
            checker == lit
        },
        "confetti" => |x, y, s, _, _| hash(x.div_euclid(s), y.div_euclid(s)) < 0.14,
        "noise" => |x, y, s, _, _| hash(x.div_euclid(s), y.div_euclid(s)) < 0.5,
        "rings" => |x, y, s, w, h| {
            fmod(
                hypot(x as f64 - w as f64 / 2.0, y as f64 - h as f64 / 2.0),
                (s * 2) as f64,
            ) < s as f64
        },
        "rays" => |x, y, s, w, h| {
            let angle = (y as f64 - h as f64 / 2.0).atan2(x as f64 - w as f64 / 2.0);
            fmod((angle + std::f64::consts::PI) * s as f64, 2.0) < 1.0
        },
        "spiral" => |x, y, s, w, h| {
            let dx = x as f64 - w as f64 / 2.0;
            let dy = y as f64 - h as f64 / 2.0;
            let turn = hypot(dx, dy) / s as f64 + dy.atan2(dx) / std::f64::consts::PI * 2.0;
            fmod(turn, 2.0) < 1.0
        },
        // Python chains the comparison: `A < s != (B < s)` means `A < s and s != B`, with B as 0 or 1.
        "moire" => |x, y, s, w, h| {
            let sf = s as f64;
            let a = fmod(
                hypot(x as f64 - w as f64 * 0.35, y as f64 - h as f64 / 2.0),
                (s * 2) as f64,
            ) < sf;
            let b = fmod(
                hypot(x as f64 - w as f64 * 0.65, y as f64 - h as f64 / 2.0),
                (s * 2) as f64,
            ) < sf;
            a && s != b as i64
        },
        "fade-right" => |x, y, _, w, _| bayer_f(x, y, x as f64 / (w - 1).max(1) as f64),
        "fade-left" => |x, y, _, w, _| bayer_f(x, y, 1.0 - x as f64 / (w - 1).max(1) as f64),
        "fade-down" => |x, y, _, _, h| bayer_f(x, y, y as f64 / (h - 1).max(1) as f64),
        "fade-up" => |x, y, _, _, h| bayer_f(x, y, 1.0 - y as f64 / (h - 1).max(1) as f64),
        "glow" => |x, y, _, w, h| {
            let d = hypot(x as f64 - w as f64 / 2.0, y as f64 - h as f64 / 2.0)
                / (hypot(w as f64, h as f64) / 2.0);
            bayer_f(x, y, 1.0 - d.min(1.0))
        },
        "vignette" => |x, y, _, w, h| {
            let d = hypot(x as f64 - w as f64 / 2.0, y as f64 - h as f64 / 2.0)
                / (hypot(w as f64, h as f64) / 2.0);
            bayer_f(x, y, d.min(1.0))
        },
        _ => return None,
    };
    Some(f)
}

/// Pattern names, sorted (the catalog lists `sorted(PATTERNS)`); `gray` is not in the list, it is `gray:N`.
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = ORDER.iter().map(|n| n.to_string()).collect();
    names.sort();
    names
}

pub fn has_pattern(spec: &str) -> bool {
    let name = spec.split(':').next().unwrap_or("");
    name == "gray" || lookup(name).is_some()
}

/// Python's `int(text)` for the part after the colon: optional sign, digits with single underscores, spaces around.
fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let mut value: i64 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        value = value
            .saturating_mul(10)
            .saturating_add(c.to_digit(10)? as i64);
    }
    Some(if negative { -value } else { value })
}

/// `name` or `name:size` as a mask image, 255 where the pattern is black. Errors carry Python's wording.
pub fn try_pattern_image(spec: &str, width: u32, height: u32) -> Result<GrayImage, String> {
    let (name, argument) = match spec.split_once(':') {
        Some((name, rest)) => (name, Some(rest)),
        None => (spec, None),
    };
    let size = match argument {
        Some(a) if !a.is_empty() => parse_int(a)
            .map(|v| v.clamp(1, 1_000_000_000))
            .ok_or_else(|| format!("Bad pattern size in {}", python_repr(spec)))?,
        _ => {
            if name == "gray" {
                50
            } else {
                4
            }
        }
    };
    let mut image = GrayImage::new(width, height);
    if name == "gray" {
        let level = size.min(100) as f64 / 100.0;
        for (x, y, p) in image.enumerate_pixels_mut() {
            if bayer(x as i32, y as i32, level) {
                p.0[0] = 255;
            }
        }
    } else {
        let pattern =
            lookup(name).ok_or_else(|| format!("Unknown pattern {}", python_repr(name)))?;
        let (w, h) = (width as i64, height as i64);
        for (x, y, p) in image.enumerate_pixels_mut() {
            if pattern(x as i64, y as i64, size, w, h) {
                p.0[0] = 255;
            }
        }
    }
    Ok(image)
}

/// Same as `try_pattern_image`, but a bad or unknown spec gives an empty mask. Check `has_pattern` first, and
/// use `try_pattern_image` when the error message should reach the user.
pub fn pattern_image(spec: &str, width: u32, height: u32) -> GrayImage {
    try_pattern_image(spec, width, height).unwrap_or_else(|_| GrayImage::new(width, height))
}

/// Python's `repr()` of a str, for error messages.
pub(crate) fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn black(image: &GrayImage) -> usize {
        image.pixels().filter(|p| p.0[0] == 255).count()
    }

    #[test]
    fn every_pattern_draws_something_but_not_everything() {
        for name in names() {
            let image = pattern_image(&format!("{name}:5"), 312, 216);
            let count = black(&image);
            assert!(count > 0 && count < 312 * 216, "{name}: {count}");
        }
    }

    #[test]
    fn there_are_29_patterns_plus_gray() {
        assert_eq!(names().len(), 29);
        assert!(has_pattern("gray:30") && has_pattern("dots:4") && !has_pattern("nope:3"));
    }

    #[test]
    fn gray_tone_has_about_the_right_darkness() {
        let image = pattern_image("gray:25", 312, 216);
        let share = black(&image) as f64 / (312.0 * 216.0);
        assert!((share - 0.25).abs() < 0.02, "{share}");
    }

    #[test]
    fn bad_sizes_and_names_are_errors() {
        assert_eq!(
            try_pattern_image("dots:big", 10, 10).unwrap_err(),
            "Bad pattern size in 'dots:big'"
        );
        assert!(try_pattern_image("nope", 10, 10).is_err());
        // Sizes below 1 become 1, as `max(1, int(...))` does.
        assert_eq!(
            try_pattern_image("checker:0", 8, 8).unwrap(),
            try_pattern_image("checker:1", 8, 8).unwrap()
        );
        assert!(try_pattern_image("checker:1_0", 8, 8).is_ok());
        assert!(try_pattern_image("checker: 5 ", 8, 8).is_ok());
    }

    #[test]
    fn hash_and_noise_match_python() {
        // Python: patterns._hash(3, 5), _hash(0, 0), _hash(123, 456), _hash(-4, 7), noise(3.9, 5.2)
        assert_eq!(hash(0, 0), 0.0);
        assert_eq!(hash(3, 5), REF_HASH_3_5);
        assert_eq!(hash(123, 456), REF_HASH_123_456);
        assert_eq!(hash(-4, 7), REF_HASH_M4_7);
        assert_eq!(noise(3.9, 5.2), hash(3, 5));
    }

    // Pixel counts of a 312x216 render at size 5 (and gray:25), from the Python module.
    #[test]
    fn pixel_counts_match_python() {
        for (spec, expected) in REF_COUNTS {
            let image = pattern_image(spec, 312, 216);
            assert_eq!(black(&image), *expected, "{spec}");
        }
    }

    // First 24 pixels of row 3 and 11, as bits, for a few patterns at size 3 on a 40x20 canvas.
    #[test]
    fn small_renders_match_python() {
        for (spec, rows) in REF_ROWS {
            let image = pattern_image(spec, 40, 20);
            for (y, expected) in rows.iter() {
                let got: String = (0..40)
                    .map(|x| {
                        if image.get_pixel(x, *y).0[0] == 255 {
                            '#'
                        } else {
                            '.'
                        }
                    })
                    .collect();
                assert_eq!(&got, expected, "{spec} row {y}");
            }
        }
    }

    const REF_HASH_3_5: f64 = 0.094906086822717;
    const REF_HASH_123_456: f64 = 0.9080349362706847;
    const REF_HASH_M4_7: f64 = 0.2891092524605592;
    #[rustfmt::skip]
    const REF_COUNTS: &[(&str, usize)] = &[
        ("brick:5", 9968),
        ("checker:5", 33700),
        ("confetti:5", 9000),
        ("crosshatch:5", 12093),
        ("diamonds:5", 16771),
        ("dots:5", 14074),
        ("fade-down:5", 33696),
        ("fade-left:5", 33723),
        ("fade-right:5", 33669),
        ("fade-up:5", 33696),
        ("glow:5", 31173),
        ("grid:5", 13072),
        ("hatch:5", 6739),
        ("hlines:5", 34320),
        ("moire:5", 33668),
        ("noise:5", 33810),
        ("plaid:5", 39510),
        ("polka:5", 16846),
        ("rays:5", 34788),
        ("rings:5", 33651),
        ("scales:5", 18546),
        ("spiral:5", 33761),
        ("stripes:5", 33699),
        ("stripes-back:5", 33693),
        ("vignette:5", 36220),
        ("vlines:5", 33912),
        ("waves:5", 22263),
        ("weave:5", 33634),
        ("zigzag:5", 22250),
        ("gray:25", 16848),
        ("gray", 33696),
        ("dots", 13689),
        ("checker:1", 33696),
        ("polka:7", 19383),
        ("weave:6", 33696),
        ("scales:3", 31824),
        ("fade-right:3", 33669),
    ];
    #[rustfmt::skip]
    const REF_ROWS: &[(&str, &[(u32, &str)])] = &[
        ("checker:3", &[ (3, "...###...###...###...###...###...###...#"), (11, "...###...###...###...###...###...###...#")]),
        ("waves:3", &[ (3, "................###........####........."), (11, "##.............###..........####........")]),
        ("scales:3", &[ (3, ".#####.#####.#####.#####.#####.#####.###"), (11, "........................................")]),
        ("weave:4", &[ (3, "......##......##......##......##......##"), (11, "......##......##......##......##......##")]),
        ("moire:3", &[ (3, ".####....###########....####...###...###"), (11, "###...###...#####...###...###...###...##")]),
        ("spiral:3", &[ (3, ".####...#####...........####....###...##"), (11, "##...###...###...##..#....###...###...##")]),
        ("glow", &[ (3, "...........#.#.#.#.#.#.#.#.#.#.#........"), (11, ".......#.#.#.#.#.###.###.#.#.#.#...#....")]),
        ("rays:2", &[ (3, "########........####....######.........."), (11, "#################..#...#################")]),
        ("polka:3", &[ (3, "..###...###...###...###...###...###...##"), (11, "........................................")]),
        ("brick:3", &[ (3, "#...........#...........#...........#..."), (11, "......#...........#...........#.........")]),
        ("plaid:3", &[ (3, "###..#...#..###..#...#..###..#...#..###."), (11, "###..#...#..###..#...#..###..#...#..###.")]),
        ("confetti:2", &[ (3, "............##....##........##......##.."), (11, "..######....##..##......................")]),
    ];
}
