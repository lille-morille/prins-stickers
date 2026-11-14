//! Ready-made figures, drawn in a 100x100 space so they stay crisp at any size. Port of `figures.py`.
//!
//! A figure is a list of steps. Lowercase paints and uppercase erases:
//!   e/E x0 y0 x1 y1        ellipse
//!   r/R x0 y0 x1 y1        rectangle
//!   p/P x y x y ...        polygon
//!   l/L width x y x y ...  line

use crate::draw;
use image::GrayImage;
use std::f64::consts::PI;

/// One drawing step. `paint` false erases.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Ellipse([f64; 4]),
    Rect([f64; 4]),
    Polygon(Vec<f64>),
    Line(f64, Vec<f64>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FigureStep {
    pub paint: bool,
    pub step: Step,
}

fn star(points: u32, inner: f64, cx: f64, cy: f64, radius: f64) -> String {
    let mut coords: Vec<f64> = Vec::new();
    for i in 0..points * 2 {
        let r = if i % 2 == 0 { radius } else { radius * inner };
        let angle = -PI / 2.0 + i as f64 * PI / points as f64;
        coords.push(cx + r * angle.cos());
        coords.push(cy + r * angle.sin());
    }
    polygon_spec('p', &coords)
}

fn regular(sides: u32) -> String {
    let mut coords: Vec<f64> = Vec::new();
    for i in 0..sides {
        let angle = -PI / 2.0 + i as f64 * 2.0 * PI / sides as f64;
        coords.push(50.0 + 50.0 * angle.cos());
        coords.push(50.0 + 50.0 * angle.sin());
    }
    polygon_spec('p', &coords)
}

fn heart() -> String {
    let mut coords: Vec<f64> = Vec::new();
    for i in 0..64 {
        let t = i as f64 / 64.0 * 2.0 * PI;
        coords.push(50.0 + 3.0 * 16.0 * t.sin().powi(3));
        coords.push(
            46.0 - 3.0
                * (13.0 * t.cos()
                    - 5.0 * (2.0 * t).cos()
                    - 2.0 * (3.0 * t).cos()
                    - (4.0 * t).cos()),
        );
    }
    polygon_spec('p', &coords)
}

fn spiral() -> String {
    let mut coords: Vec<f64> = Vec::new();
    for i in 0..160 {
        let angle = i as f64 / 160.0 * 6.0 * PI;
        let radius = 2.0 + i as f64 / 160.0 * 46.0;
        coords.push(50.0 + radius * angle.cos());
        coords.push(50.0 + radius * angle.sin());
    }
    let mut spec = String::from("l 5");
    for c in coords {
        spec.push_str(&format!(" {c:?}"));
    }
    spec
}

fn rays(count: u32, inner: f64, outer: f64, width: f64) -> String {
    let mut steps: Vec<String> = Vec::new();
    for i in 0..count {
        let angle = i as f64 * 2.0 * PI / count as f64;
        steps.push(format!(
            "l {width:?} {:?} {:?} {:?} {:?}",
            50.0 + inner * angle.cos(),
            50.0 + inner * angle.sin(),
            50.0 + outer * angle.cos(),
            50.0 + outer * angle.sin()
        ));
    }
    steps.join("; ")
}

fn polygon_spec(letter: char, coords: &[f64]) -> String {
    let mut spec = String::from(letter);
    for c in coords {
        spec.push_str(&format!(" {c:?}"));
    }
    spec
}

/// The step list of a figure as text (`"e 0 0 100 100; E 18 18 82 82"`), or None for an unknown name.
fn spec(name: &str) -> Option<String> {
    let s = match name {
        "heart" => heart(),
        "star" => star(5, 0.42, 50.0, 52.0, 50.0),
        "star4" => star(4, 0.25, 50.0, 52.0, 50.0),
        "star6" => star(6, 0.55, 50.0, 52.0, 50.0),
        "sparkle" => star(4, 0.14, 50.0, 50.0, 50.0),
        "burst" => star(16, 0.68, 50.0, 50.0, 50.0),
        "hexagon" => regular(6),
        "octagon" => regular(8),
        "spiral" => spiral(),
        "sun" => format!("e 26 26 74 74; {}", rays(12, 34.0, 49.0, 6.0)),
        "circle" => "e 0 0 100 100".into(),
        "ring" => "e 0 0 100 100; E 18 18 82 82".into(),
        "target" => "e 0 0 100 100; E 14 14 86 86; e 28 28 72 72; E 40 40 60 60".into(),
        "square" => "r 0 0 100 100".into(),
        "diamond" => "p 50 0 100 50 50 100 0 50".into(),
        "triangle" => "p 50 4 100 96 0 96".into(),
        "plus" => "r 35 0 65 100; r 0 35 100 65".into(),
        "cross" => "l 20 10 10 90 90; l 20 90 10 10 90".into(),
        "check" => "l 18 8 55 38 85 92 18".into(),
        "arrow-right" => "r 0 38 60 62; p 55 12 100 50 55 88".into(),
        "arrow-left" => "r 40 38 100 62; p 45 12 0 50 45 88".into(),
        "arrow-up" => "r 38 40 62 100; p 12 45 50 0 88 45".into(),
        "arrow-down" => "r 38 0 62 60; p 12 55 50 100 88 55".into(),
        "moon" => "e 8 4 96 92; E 34 -4 112 74".into(),
        "cloud" => "e 2 44 42 84; e 26 22 72 68; e 58 40 98 84; r 22 60 78 84".into(),
        "rain" => "e 2 22 42 58; e 26 4 72 46; e 58 20 98 58; r 22 38 78 58; l 6 28 68 20 92; l 6 52 68 44 92; l 6 76 68 68 92".into(),
        "bolt" => "p 58 0 18 56 46 56 34 100 84 38 54 38".into(),
        "drop" => "p 50 0 80 52 20 52; e 18 36 82 100".into(),
        "flame" => "p 52 0 84 48 76 40 66 24; e 18 36 84 100; p 18 62 34 22 52 50; E 38 64 64 92".into(),
        "leaf" => "p 96 4 88 56 50 86 16 84 14 50 44 16; l 4 6 96 60 40".into(),
        "tree" => "p 50 0 84 40 66 40 92 76 8 76 34 40 16 40; r 42 76 58 100".into(),
        "flower" => "e 34 0 66 36; e 34 64 66 100; e 0 34 36 66; e 64 34 100 66; e 8 8 40 40; e 60 8 92 40; e 8 60 40 92; e 60 60 92 92; E 34 34 66 66; e 40 40 60 60".into(),
        "mountain" => "p 0 92 36 22 54 54 70 34 100 92".into(),
        "wave" => "l 8 0 50 12 34 25 30 38 34 50 50 62 66 75 70 88 66 100 50".into(),
        "smile" => "e 0 0 100 100; E 26 28 40 46; E 60 28 74 46; L 7 24 62 36 74 50 78 64 74 76 62".into(),
        "frown" => "e 0 0 100 100; E 26 28 40 46; E 60 28 74 46; L 7 24 78 36 66 50 62 64 66 76 78".into(),
        "wink" => "e 0 0 100 100; L 7 24 38 40 38; E 60 28 74 46; L 7 24 62 36 74 50 78 64 74 76 62".into(),
        "skull" => "e 12 0 88 72; r 30 60 70 96; E 24 30 44 52; E 56 30 76 52; P 50 54 44 66 56 66; R 39 78 43 96; R 57 78 61 96; R 48 78 52 96".into(),
        "ghost" => "e 14 0 86 72; p 14 36 86 36 86 100 74 86 62 100 50 86 38 100 26 86 14 100; E 30 26 44 44; E 56 26 70 44".into(),
        "cat" => "e 12 26 88 94; p 14 44 16 2 44 28; p 86 44 84 2 56 28; E 28 48 42 62; E 58 48 72 62; P 44 68 56 68 50 76".into(),
        "fish" => "e 4 22 76 78; p 66 50 100 22 100 78; E 18 38 28 48".into(),
        "bird" => "e 14 28 78 82; e 52 12 92 52; p 88 28 100 34 88 40; p 22 56 0 40 6 78; E 70 24 78 32".into(),
        "crown" => "p 0 28 25 56 50 8 75 56 100 28 90 86 10 86".into(),
        "bell" => "e 22 8 78 64; p 22 36 78 36 92 80 8 80; e 40 78 60 98; r 46 0 54 12".into(),
        "gift" => "r 6 38 94 100; r 0 22 100 42; R 44 22 56 100; l 6 50 22 28 4 40 22 50 22; l 6 50 22 72 4 60 22 50 22".into(),
        "cake" => "r 8 50 92 94; r 0 92 100 100; R 8 66 92 72; r 28 26 36 50; r 46 26 54 50; r 64 26 72 50; e 27 12 37 26; e 45 12 55 26; e 63 12 73 26".into(),
        "balloon" => "e 16 0 84 78; p 42 76 58 76 50 86; l 3 50 86 44 94 54 100".into(),
        "trophy" => "p 22 0 78 0 74 40 58 58 42 58 26 40; r 45 58 55 80; r 26 80 74 100; l 6 22 8 4 8 8 34 28 40; l 6 78 8 96 8 92 34 72 40".into(),
        "flag" => "r 10 0 18 100; p 18 4 92 4 74 28 92 52 18 52".into(),
        "shield" => "p 50 0 92 14 92 52 72 82 50 100 28 82 8 52 8 14".into(),
        "key" => "e 0 22 50 72; E 14 36 36 58; r 46 42 100 54; r 74 54 84 72; r 90 54 100 66".into(),
        "lock" => "r 12 44 88 100; e 26 0 74 60; E 38 12 62 56; r 12 44 88 60; E 42 62 58 78; R 46 72 54 90".into(),
        "mail" => "r 0 16 100 84; L 6 2 20 50 56 98 20".into(),
        "house" => "p 50 2 100 50 86 50 86 98 14 98 14 50 0 50; R 40 62 60 98".into(),
        "rocket" => "p 50 0 70 30 70 74 30 74 30 30; p 30 48 10 80 30 74; p 70 48 90 80 70 74; p 38 74 62 74 50 100; E 41 28 59 46".into(),
        "coffee" => "r 14 36 72 92; e 62 44 94 76; E 70 52 86 68; r 6 92 80 100; l 5 30 28 34 18 28 8; l 5 54 28 58 18 52 8".into(),
        "music" => "e 2 68 34 96; e 60 58 92 86; r 26 14 34 82; r 84 4 92 72; p 26 14 92 4 92 24 26 34".into(),
        "eye" => "p 0 50 22 26 50 18 78 26 100 50 78 74 50 82 22 74; E 28 26 72 74; e 38 36 62 60".into(),
        "pin" => "e 18 0 82 64; p 26 48 74 48 50 100; E 38 20 62 44".into(),
        "infinity" => "l 10 50 50 36 28 18 28 6 50 18 72 36 72 50 50 64 28 82 28 94 50 82 72 64 72 50 50".into(),
        _ => return None,
    };
    Some(s)
}

/// Every figure name, in the order `figures.py` lists them.
#[rustfmt::skip]
const ORDER: &[&str] = &[
    "heart",
    "star",
    "star4",
    "star6",
    "sparkle",
    "burst",
    "circle",
    "ring",
    "target",
    "square",
    "diamond",
    "triangle",
    "hexagon",
    "octagon",
    "plus",
    "cross",
    "check",
    "arrow-right",
    "arrow-left",
    "arrow-up",
    "arrow-down",
    "sun",
    "moon",
    "cloud",
    "rain",
    "bolt",
    "drop",
    "flame",
    "leaf",
    "tree",
    "flower",
    "mountain",
    "wave",
    "smile",
    "frown",
    "wink",
    "skull",
    "ghost",
    "cat",
    "fish",
    "bird",
    "crown",
    "bell",
    "gift",
    "cake",
    "balloon",
    "trophy",
    "flag",
    "shield",
    "key",
    "lock",
    "mail",
    "house",
    "rocket",
    "coffee",
    "music",
    "eye",
    "pin",
    "spiral",
    "infinity",
];

/// Figure names, sorted (the catalog lists `sorted(FIGURES)`).
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = ORDER.iter().map(|n| n.to_string()).collect();
    names.sort();
    names
}

pub fn has_figure(name: &str) -> bool {
    ORDER.contains(&name)
}

/// The steps of a figure in its 100x100 space.
pub fn steps(name: &str) -> Option<Vec<FigureStep>> {
    let text = spec(name)?;
    let mut out = Vec::new();
    for part in text.split(';') {
        let mut tokens = part.split_whitespace();
        let Some(letter) = tokens.next().and_then(|t| t.chars().next()) else {
            continue;
        };
        let nums: Vec<f64> = tokens.filter_map(|t| t.parse().ok()).collect();
        let paint = letter.is_ascii_lowercase();
        let step = match letter.to_ascii_lowercase() {
            'e' if nums.len() >= 4 => Step::Ellipse([nums[0], nums[1], nums[2], nums[3]]),
            'r' if nums.len() >= 4 => Step::Rect([nums[0], nums[1], nums[2], nums[3]]),
            'p' => Step::Polygon(nums),
            'l' if !nums.is_empty() => Step::Line(nums[0], nums[1..].to_vec()),
            _ => continue,
        };
        out.push(FigureStep { paint, step });
    }
    Some(out)
}

/// A mask (255 where the figure is). Figures keep their proportions and are centered in the box.
/// An unknown name gives an empty mask.
pub fn figure_mask(name: &str, width: u32, height: u32) -> GrayImage {
    let mut mask = GrayImage::new(width, height);
    let Some(list) = steps(name) else {
        return mask;
    };
    let side = width.min(height) as f64;
    let scale = side / 100.0;
    let (dx, dy) = ((width as f64 - side) / 2.0, (height as f64 - side) / 2.0);
    let at = |coords: &[f64]| -> Vec<(f64, f64)> {
        coords
            .chunks_exact(2)
            .map(|c| (dx + c[0] * scale, dy + c[1] * scale))
            .collect()
    };
    for FigureStep { paint, step } in list {
        let value: u8 = if paint { 255 } else { 0 };
        match step {
            Step::Ellipse(b) => {
                let p = at(&b);
                draw::ellipse(
                    &mut mask,
                    [p[0].0, p[0].1, p[1].0, p[1].1],
                    Some(value),
                    None,
                    0,
                );
            }
            Step::Rect(b) => {
                let p = at(&b);
                draw::rectangle(
                    &mut mask,
                    [p[0].0, p[0].1, p[1].0, p[1].1],
                    Some(value),
                    None,
                    0,
                );
            }
            Step::Polygon(c) => {
                draw::polygon(&mut mask, &at(&c), Some(value), None, 0);
            }
            Step::Line(w, c) => {
                let points = at(&c);
                let line_width = draw::py_round(w * scale).max(1.0) as u32;
                draw::line(&mut mask, &points, value, line_width, true);
                for &(x, y) in [points.first(), points.last()].into_iter().flatten() {
                    let r = line_width as f64 / 2.0;
                    draw::ellipse(
                        &mut mask,
                        [x - r, y - r, x + r, y + r],
                        Some(value),
                        None,
                        0,
                    );
                }
            }
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn black(m: &GrayImage) -> usize {
        m.as_raw().iter().filter(|&&v| v != 0).count()
    }

    #[test]
    fn there_are_sixty_figures() {
        assert_eq!(names().len(), 60);
        assert!(has_figure("heart") && !has_figure("nope"));
        for n in names() {
            assert!(steps(&n).is_some(), "{n}");
        }
    }

    #[test]
    fn every_figure_draws_inside_its_box() {
        for n in names() {
            let mut canvas = GrayImage::new(312, 216);
            crate::raster::paste_value(&mut canvas, 255, 100, 50, &figure_mask(&n, 80, 80));
            assert!(black(&canvas) > 30, "{n}");
        }
    }

    #[test]
    fn figures_keep_proportions_and_stay_centred() {
        let wide = figure_mask("square", 100, 40);
        // A 40x40 square in the middle of a 100x40 box.
        let bbox = crate::raster::getbbox(&wide).unwrap();
        assert_eq!(bbox, (30, 0, 71, 40));
        assert!(figure_mask("nope", 10, 10).as_raw().iter().all(|&v| v == 0));
    }

    #[test]
    fn masks_match_pillow() {
        for (name, w, h, count, hash) in REF {
            let m = figure_mask(name, *w, *h);
            assert_eq!((black(&m), fnv(&m)), (*count, *hash), "{name} {w}x{h}");
        }
    }

    fn fnv(m: &GrayImage) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in m.as_raw() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    #[rustfmt::skip]
    const REF: &[(&str, u32, u32, usize, u64)] = &[
        ("arrow-down", 100, 100, 3168, 0xc09822e8410a5be5),
        ("arrow-down", 80, 80, 2041, 0x757ead3111968934),
        ("arrow-down", 40, 40, 531, 0x7266d5e5dd7f41ec),
        ("arrow-down", 60, 24, 203, 0x950a3d477ad9db2e),
        ("arrow-left", 100, 100, 3136, 0x6518e5bf0bf55559),
        ("arrow-left", 80, 80, 2020, 0x8ef9b44fb2befae7),
        ("arrow-left", 40, 40, 520, 0x79e030c662115f11),
        ("arrow-left", 60, 24, 199, 0xbdcc8c079dd97ef6),
        ("arrow-right", 100, 100, 3160, 0xe4b7d9868b19ff25),
        ("arrow-right", 80, 80, 2039, 0xad73c20962741440),
        ("arrow-right", 40, 40, 529, 0x896b24a4eda6392a),
        ("arrow-right", 60, 24, 202, 0xaecb819df7ea8293),
        ("arrow-up", 100, 100, 3144, 0x13203cdbcb68c0ed),
        ("arrow-up", 80, 80, 2022, 0xb810925a9793f5ef),
        ("arrow-up", 40, 40, 522, 0x1cd38254f5c6a389),
        ("arrow-up", 60, 24, 193, 0xe70dd89c1a2beb62),
        ("balloon", 100, 100, 4404, 0x9b2206d85c327665),
        ("balloon", 80, 80, 2832, 0xc51bec3e5dbde9e7),
        ("balloon", 40, 40, 717, 0x411cfc3384803d48),
        ("balloon", 60, 24, 280, 0xe3498d65b7543175),
        ("bell", 100, 100, 4831, 0x6eefc5d7a759d2da),
        ("bell", 80, 80, 3134, 0xfbc21ce4f1f74467),
        ("bell", 40, 40, 815, 0x71ca84e4becfc0da),
        ("bell", 60, 24, 310, 0x292fcffa76e12bb3),
        ("bird", 100, 100, 4038, 0x585d8eceab998033),
        ("bird", 80, 80, 2583, 0xb0669180e3497446),
        ("bird", 40, 40, 665, 0x1142795bc7ce0284),
        ("bird", 60, 24, 262, 0x746b91818fd8d2cf),
        ("bolt", 100, 100, 2027, 0x97da7e406445e2a6),
        ("bolt", 80, 80, 1328, 0x0af6948c0046e723),
        ("bolt", 40, 40, 347, 0x6f5aac520058e15e),
        ("bolt", 60, 24, 139, 0xe9d1d9d3890c08d8),
        ("burst", 100, 100, 5521, 0xa3961a166fffd992),
        ("burst", 80, 80, 3554, 0x54ad58fdcac50ceb),
        ("burst", 40, 40, 945, 0xf03a46ad6a16131e),
        ("burst", 60, 24, 362, 0x6878b7c7d6284aa5),
        ("cake", 100, 100, 4789, 0xf9227aa7fa754366),
        ("cake", 80, 80, 3079, 0xb27c2772a3cd5880),
        ("cake", 40, 40, 788, 0x11fd1e2b9388394b),
        ("cake", 60, 24, 286, 0x09ca6cf6ca03ec99),
        ("cat", 100, 100, 4860, 0x03c7ceeb06a89dad),
        ("cat", 80, 80, 3166, 0x6ad9f02b4ffdbe43),
        ("cat", 40, 40, 826, 0xc9987f7476e26a7f),
        ("cat", 60, 24, 299, 0x714c5bc6ede38742),
        ("check", 100, 100, 2516, 0xbe06b9dfaaff1d27),
        ("check", 80, 80, 1542, 0xf19426ab49a5691f),
        ("check", 40, 40, 377, 0x71e03a3f8ef08e18),
        ("check", 60, 24, 156, 0x7d820add21ddf211),
        ("circle", 100, 100, 7975, 0x307f2c9e6a8a27f6),
        ("circle", 80, 80, 5119, 0x98a651b15772978e),
        ("circle", 40, 40, 1295, 0xe4a612f201f336aa),
        ("circle", 60, 24, 482, 0xecec3284623d24b5),
        ("cloud", 100, 100, 4494, 0xaf9ace671df28fa7),
        ("cloud", 80, 80, 2906, 0xba6e46647dfa56ff),
        ("cloud", 40, 40, 752, 0x95e6779448636ffb),
        ("cloud", 60, 24, 288, 0x51f33002893faa43),
        ("coffee", 100, 100, 4575, 0x11f928777cac1f9c),
        ("coffee", 80, 80, 2978, 0x85194fac7edce4dd),
        ("coffee", 40, 40, 750, 0xfb2617445763544d),
        ("coffee", 60, 24, 284, 0xfa52803bfa60b9d1),
        ("cross", 100, 100, 4645, 0xfc069a12357e7506),
        ("cross", 80, 80, 3101, 0x4faa62b5586ebd82),
        ("cross", 40, 40, 757, 0x3cb4bd4972d7526e),
        ("cross", 60, 24, 244, 0x472c227ee53ee6b5),
        ("crown", 100, 100, 4490, 0x8fafd3a05eac8d8d),
        ("crown", 80, 80, 2888, 0xb1211c8d03f327d5),
        ("crown", 40, 40, 755, 0x9352d3c45c307692),
        ("crown", 60, 24, 284, 0x9bfc622a27a90e99),
        ("diamond", 100, 100, 5099, 0x55d5fa8b16383fe6),
        ("diamond", 80, 80, 3279, 0x190d97d1e13ffaca),
        ("diamond", 40, 40, 839, 0x9befb39c1a5237d2),
        ("diamond", 60, 24, 312, 0x2411fb2a9e72057d),
        ("drop", 100, 100, 4250, 0x2ccad0360202a795),
        ("drop", 80, 80, 2750, 0xe60832385fd78cf1),
        ("drop", 40, 40, 705, 0x9cf55376cc6d5e72),
        ("drop", 60, 24, 262, 0x79f1c987d11fa969),
        ("eye", 100, 100, 3005, 0xcddee2e457b5040c),
        ("eye", 80, 80, 1952, 0x42386b3a9b0c960b),
        ("eye", 40, 40, 494, 0xedf46ca341340d27),
        ("eye", 60, 24, 162, 0x1b3d84f3609059c3),
        ("fish", 100, 100, 4027, 0xfbefe6a7401c21f4),
        ("fish", 80, 80, 2593, 0x7eee0e66ab75e78a),
        ("fish", 40, 40, 676, 0x56ab471a230a172f),
        ("fish", 60, 24, 266, 0xa85327175d775aef),
        ("flag", 100, 100, 4088, 0x5e4c3190673f2c35),
        ("flag", 80, 80, 2595, 0xfd701e45c6e2a528),
        ("flag", 40, 40, 673, 0xf7efbb2a27ea8304),
        ("flag", 60, 24, 275, 0xb21f64283af5a31c),
        ("flame", 100, 100, 3110, 0x1fda7bd09962a859),
        ("flame", 80, 80, 2061, 0x86e483aa376d21ca),
        ("flame", 40, 40, 523, 0x75040d3c7976a0fe),
        ("flame", 60, 24, 202, 0x7aea75da3bc965bf),
        ("flower", 100, 100, 6731, 0x1703110729aa5cea),
        ("flower", 80, 80, 4314, 0xc6ebf9c75dcdfedb),
        ("flower", 40, 40, 1096, 0x48ee899ac9ab196b),
        ("flower", 60, 24, 436, 0xe8520e25124da90f),
        ("frown", 100, 100, 7075, 0x30388640208fc550),
        ("frown", 80, 80, 4528, 0x310e2c4c350be7d5),
        ("frown", 40, 40, 1131, 0x3fd67f9c2a53dc0a),
        ("frown", 60, 24, 411, 0x48fb0f0b0dd2afc4),
        ("ghost", 100, 100, 5855, 0xd6dddb3548c18abe),
        ("ghost", 80, 80, 3674, 0x42009c30d56b2767),
        ("ghost", 40, 40, 968, 0xde0e40006b614fdb),
        ("ghost", 60, 24, 346, 0x055bbc2556256d7f),
        ("gift", 100, 100, 6665, 0x0b4a4d3f7e09230c),
        ("gift", 80, 80, 4382, 0xe2eac2f807b1c829),
        ("gift", 40, 40, 1070, 0xbb6ae6994a994e6b),
        ("gift", 60, 24, 382, 0x56464fe6365c61b1),
        ("heart", 100, 100, 5166, 0x6110cc9037ee156d),
        ("heart", 80, 80, 3348, 0x99c29b0457a38ea1),
        ("heart", 40, 40, 858, 0x8988936580e7f01b),
        ("heart", 60, 24, 322, 0x7cdf57ea27ccdf03),
        ("hexagon", 100, 100, 6645, 0xe3a8f02995851c12),
        ("hexagon", 80, 80, 4233, 0x9a9a4ca6459a4868),
        ("hexagon", 40, 40, 1098, 0x87ae8a8cc410f8a9),
        ("hexagon", 60, 24, 407, 0xeeefacbf87f2d9fe),
        ("house", 100, 100, 5223, 0xfa4c8819a779bfaa),
        ("house", 80, 80, 3333, 0x144b9eeca44f8e52),
        ("house", 40, 40, 866, 0x3590aa25eb820db7),
        ("house", 60, 24, 307, 0x40e1d95d773f0834),
        ("infinity", 100, 100, 2563, 0xcb36c2dd42fb4cea),
        ("infinity", 80, 80, 1633, 0xbe4e764a7c3ab658),
        ("infinity", 40, 40, 419, 0xd268594a0ec2a1fe),
        ("infinity", 60, 24, 126, 0x0041317540ede82b),
        ("key", 100, 100, 2571, 0x93948140eca609cc),
        ("key", 80, 80, 1672, 0xb009de821bbbf30f),
        ("key", 40, 40, 442, 0x20bc7fff4b4e6567),
        ("key", 60, 24, 173, 0x758652b2a00fa0d6),
        ("leaf", 100, 100, 4605, 0xbcc5fc2f81610e30),
        ("leaf", 80, 80, 2947, 0x40a69c7fe5010b12),
        ("leaf", 40, 40, 769, 0x6d60bc657bfc8e1c),
        ("leaf", 60, 24, 301, 0x1f8190fa7fc1b270),
        ("lock", 100, 100, 5117, 0x404c3c2bcb3adaaa),
        ("lock", 80, 80, 3292, 0xa2c160e68f4f3b3d),
        ("lock", 40, 40, 840, 0x571b6875b23434e9),
        ("lock", 60, 24, 316, 0x9ef579f78fa3b50d),
        ("mail", 100, 100, 6203, 0xb25df20bfb1c922a),
        ("mail", 80, 80, 3983, 0xf7b52334b43c3d94),
        ("mail", 40, 40, 1022, 0xac33a128b2d7278d),
        ("mail", 60, 24, 422, 0xd84dc6b52d2316b7),
        ("moon", 100, 100, 2747, 0x5d5d6af713090f40),
        ("moon", 80, 80, 1738, 0xc4c1ba5ad5843d9f),
        ("moon", 40, 40, 430, 0x5ee652e59f406441),
        ("moon", 60, 24, 196, 0xbcca6c039fbfc54f),
        ("mountain", 100, 100, 4009, 0x1c3352c4b1b84b3a),
        ("mountain", 80, 80, 2572, 0x44e4054f54369ad9),
        ("mountain", 40, 40, 666, 0xcf014bc8133dff7b),
        ("mountain", 60, 24, 260, 0x2aaabefc75966ab9),
        ("music", 100, 100, 3557, 0x61ec9361a9d5b00a),
        ("music", 80, 80, 2292, 0x0f84a47d86c8c8fb),
        ("music", 40, 40, 599, 0x76b125c8ab128956),
        ("music", 60, 24, 255, 0xcec4bb2ee9163b7e),
        ("octagon", 100, 100, 7191, 0x717f2788c7bbd650),
        ("octagon", 80, 80, 4636, 0x64391d1e429dfeff),
        ("octagon", 40, 40, 1195, 0x338a64759818901a),
        ("octagon", 60, 24, 429, 0x30d244e60c1d3ab0),
        ("pin", 100, 100, 3499, 0xf1ddc2a9a6639eba),
        ("pin", 80, 80, 2260, 0x7c5d6a3ca37f7cb1),
        ("pin", 40, 40, 570, 0xb61db8253f1ab8bf),
        ("pin", 60, 24, 212, 0x5404b6fc3e145a95),
        ("plus", 100, 100, 5239, 0x881172286d3fb986),
        ("plus", 80, 80, 3375, 0xe20e28897c05df12),
        ("plus", 40, 40, 871, 0xc5d514be9187a036),
        ("plus", 60, 24, 328, 0x46cd3e69a752b8e5),
        ("rain", 100, 100, 4523, 0x9ff5b1ebbde20154),
        ("rain", 80, 80, 2951, 0x5788ee6e07cf395e),
        ("rain", 40, 40, 745, 0x09921ef7b2e169f8),
        ("rain", 60, 24, 278, 0xff694e050ca56a41),
        ("ring", 100, 100, 4670, 0x64f919139d214a1d),
        ("ring", 80, 80, 2999, 0xcb3a1407a84f1c02),
        ("ring", 40, 40, 771, 0xc13084fca712cf5e),
        ("ring", 60, 24, 282, 0x1fc7094871117aa5),
        ("rocket", 100, 100, 3031, 0xcf7de18e0940f8f6),
        ("rocket", 80, 80, 1944, 0xc576e3fb9ef97f9b),
        ("rocket", 40, 40, 495, 0x06f6902719e088ae),
        ("rocket", 60, 24, 173, 0xc10901c472c7c5a4),
        ("shield", 100, 100, 6196, 0x335a5f969a27b1ed),
        ("shield", 80, 80, 3945, 0x163946c564368022),
        ("shield", 40, 40, 985, 0x615cd63d1745bbce),
        ("shield", 60, 24, 384, 0x3e43b334097de4fd),
        ("skull", 100, 100, 4331, 0x2985622e681adbee),
        ("skull", 80, 80, 2783, 0xd78ac0ceeca25b6a),
        ("skull", 40, 40, 695, 0x46e82d8fca5b402a),
        ("skull", 60, 24, 254, 0x6975d0ffb4a1f03b),
        ("smile", 100, 100, 7067, 0x78f06bcbcfd5c0f8),
        ("smile", 80, 80, 4525, 0x6bda45da51dda028),
        ("smile", 40, 40, 1131, 0x41aeb1527681a752),
        ("smile", 60, 24, 407, 0x51a383262008b028),
        ("sparkle", 100, 100, 997, 0x562ce2214d357e9c),
        ("sparkle", 80, 80, 637, 0x44c61008a09de536),
        ("sparkle", 40, 40, 158, 0xc6d5eb34b6cd9e43),
        ("sparkle", 60, 24, 95, 0xb532fa5cefc4fa18),
        ("spiral", 100, 100, 2299, 0x82f3932a5447506c),
        ("spiral", 80, 80, 1455, 0xc149db4c0a3e12ec),
        ("spiral", 40, 40, 370, 0x5f87b3c9645d34d1),
        ("spiral", 60, 24, 116, 0x6c5fcb4a56e80e5d),
        ("square", 100, 100, 10000, 0x6b6e1740c2d8d355),
        ("square", 80, 80, 6400, 0x3b11dc0958493825),
        ("square", 40, 40, 1600, 0x62b839addbbefbe5),
        ("square", 60, 24, 600, 0xee3a808bbc0652e5),
        ("star", 100, 100, 3162, 0x3d83d6e44b061c4d),
        ("star", 80, 80, 2011, 0xb25c00708750ac30),
        ("star", 40, 40, 531, 0xec10c4ad86095316),
        ("star", 60, 24, 208, 0xf4e6a7ce6fde2a5d),
        ("star4", 100, 100, 1794, 0x2f7bf67427c49ca3),
        ("star4", 80, 80, 1238, 0xab9aebeacb29bed5),
        ("star4", 40, 40, 316, 0x16400caf32866cb1),
        ("star4", 60, 24, 130, 0xe1692bd145fef83d),
        ("star6", 100, 100, 4197, 0x91417845ff03fc22),
        ("star6", 80, 80, 2742, 0x7a56d5175ab13eeb),
        ("star6", 40, 40, 724, 0xf3b37d3fd30805a9),
        ("star6", 60, 24, 281, 0x88fd9c364f8414d8),
        ("sun", 100, 100, 3395, 0x328473a10b5f4b84),
        ("sun", 80, 80, 2246, 0x1d2e20cd8f155453),
        ("sun", 40, 40, 518, 0x09b34df277f7c0c7),
        ("sun", 60, 24, 220, 0x528e36447871f6a5),
        ("target", 100, 100, 5026, 0x7e38ca31cdb37785),
        ("target", 80, 80, 3278, 0x692e2a6c75dcdee5),
        ("target", 40, 40, 794, 0x035897301c112031),
        ("target", 60, 24, 314, 0x571b4e6002ffd445),
        ("tree", 100, 100, 3972, 0x65f4a4b1d5c16219),
        ("tree", 80, 80, 2534, 0xf1ac615b2c352db3),
        ("tree", 40, 40, 663, 0x12bc22db4287a4d0),
        ("tree", 60, 24, 257, 0xc06b2f53e2781e6a),
        ("triangle", 100, 100, 4740, 0xd17f0286e7dc5cc1),
        ("triangle", 80, 80, 3033, 0xd92299f76158de9a),
        ("triangle", 40, 40, 797, 0xbaa3ebdfb5f147de),
        ("triangle", 60, 24, 312, 0x69412527e724c6ed),
        ("trophy", 100, 100, 4704, 0xae4cecf798815dc1),
        ("trophy", 80, 80, 3059, 0x4cd40c7b9f0ab84c),
        ("trophy", 40, 40, 751, 0x0631d594d9e4733c),
        ("trophy", 60, 24, 274, 0x66e8cc5d227ca8b3),
        ("wave", 100, 100, 974, 0x3ffbdb4946ea083d),
        ("wave", 80, 80, 608, 0xcbc5e1757d8f36e1),
        ("wave", 40, 40, 158, 0x82151a94f2411987),
        ("wave", 60, 24, 60, 0x9f1ecb94b42b272b),
        ("wink", 100, 100, 7120, 0x98447bcfccb86c09),
        ("wink", 80, 80, 4558, 0x998a165796872c31),
        ("wink", 40, 40, 1143, 0x54d4e029899230ca),
        ("wink", 60, 24, 413, 0x26e3a0981f7a7654),
    ];
}
