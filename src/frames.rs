//! Frames: decorative borders that fill a box's edge. Port of `frames.py`.
//! Any figure name also works as a frame: `heart`, `star` and so on repeat around the edge.

use crate::draw;
use crate::figures;
use crate::raster;
use image::GrayImage;

type Box4 = (i32, i32, i32, i32);

/// Ink on a mask (Python draws `1` into a mode "1" image).
const ON: u8 = 255;
const OFF: u8 = 0;

fn rect_outline(img: &mut GrayImage, b: Box4, inset: i32, width: i32, radius: i32) {
    let (x0, y0, x1, y1) = b;
    let shape = [x0 + inset, y0 + inset, x1 - inset, y1 - inset];
    let width = width.max(0) as u32;
    if radius != 0 {
        draw::rounded_rectangle(img, shape, radius as f64, None, Some(ON), width);
    } else {
        draw::rectangle(img, shape, None, Some(ON), width);
    }
}

/// Python's `range(start, stop, step)` for a positive step.
fn span(start: i32, stop: i32, step: i32) -> impl Iterator<Item = i32> {
    let step = step.max(1) as usize;
    (start as i64..stop as i64).step_by(step).map(|v| v as i32)
}

fn along_edges(b: Box4, step: i32) -> Vec<(i32, i32)> {
    let (x0, y0, x1, y1) = b;
    let mut points = Vec::new();
    for x in span(x0, x1 + 1, step) {
        points.push((x, y0));
        points.push((x, y1));
    }
    for y in span(y0, y1 + 1, step) {
        points.push((x0, y));
        points.push((x1, y));
    }
    points
}

fn fill_rect(img: &mut GrayImage, b: [i32; 4], value: u8) {
    draw::rectangle(img, b, Some(value), None, 0);
}

fn dashed(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    for x in span(x0, x1 + 1, s * 2) {
        fill_rect(img, [x, y0, (x + s - 1).min(x1), y0 + 1], ON);
        fill_rect(img, [x, y1 - 1, (x + s - 1).min(x1), y1], ON);
    }
    for y in span(y0, y1 + 1, s * 2) {
        fill_rect(img, [x0, y, x0 + 1, (y + s - 1).min(y1)], ON);
        fill_rect(img, [x1 - 1, y, x1, (y + s - 1).min(y1)], ON);
    }
}

fn dotted(img: &mut GrayImage, b: Box4, s: i32) {
    for (x, y) in along_edges(b, s * 2) {
        draw::ellipse(img, [x - 1, y - 1, x + 1, y + 1], Some(ON), None, 0);
    }
}

fn ticket(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    let r = s * 3;
    draw::rectangle(img, [x0, y0, x1, y1], None, Some(ON), 2);
    for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        let ring = [cx - r, cy - r, cx + r, cy + r];
        draw::ellipse(img, ring, Some(OFF), None, 0);
        draw::arc(img, ring, 0.0, 360.0, ON, 2);
    }
}

fn stamp(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    fill_rect(img, [x0, y0, x1, y1], ON);
    fill_rect(img, [x0 + s * 3, y0 + s * 3, x1 - s * 3, y1 - s * 3], OFF);
    for (x, y) in along_edges(b, s * 3) {
        draw::ellipse(img, [x - s, y - s, x + s, y + s], Some(OFF), None, 0);
    }
}

fn film(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    let band = s * 5;
    for top in [y0, y1 - band] {
        fill_rect(img, [x0, top, x1, top + band], ON);
        for x in span(x0 + s, x1 - s, s * 3) {
            fill_rect(img, [x, top + s + 1, x + s, top + band - s - 1], OFF);
        }
    }
}

fn checker(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    for x in span(x0, x1 + 1, s) {
        for y in span(y0, y1 + 1, s) {
            let edge = x < x0 + s * 2 || x > x1 - s * 2 || y < y0 + s * 2 || y > y1 - s * 2;
            if edge && ((x - x0).div_euclid(s) + (y - y0).div_euclid(s)).rem_euclid(2) == 0 {
                fill_rect(img, [x, y, (x + s - 1).min(x1), (y + s - 1).min(y1)], ON);
            }
        }
    }
}

fn zigzag(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    let step = s * 2;
    let odd = |v: i32, from: i32| (v - from).div_euclid(step).rem_euclid(2) == 1;
    let top: Vec<(i32, i32)> = span(x0, x1 + 1, step)
        .map(|x| (x, y0 + if odd(x, x0) { s } else { 0 }))
        .collect();
    let bottom: Vec<(i32, i32)> = span(x0, x1 + 1, step)
        .map(|x| (x, y1 - if odd(x, x0) { s } else { 0 }))
        .collect();
    let left: Vec<(i32, i32)> = span(y0, y1 + 1, step)
        .map(|y| (x0 + if odd(y, y0) { s } else { 0 }, y))
        .collect();
    let right: Vec<(i32, i32)> = span(y0, y1 + 1, step)
        .map(|y| (x1 - if odd(y, y0) { s } else { 0 }, y))
        .collect();
    for points in [top, bottom, left, right] {
        draw::line(img, &points, ON, 2, false);
    }
}

fn scallop(img: &mut GrayImage, b: Box4, s: i32) {
    let r = s * 2;
    for (x, y) in along_edges(b, r * 2) {
        draw::ellipse(img, [x - r, y - r, x + r, y + r], None, Some(ON), 2);
    }
}

fn brackets(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    let arm = s * 6;
    for (cx, sx) in [(x0, 1), (x1, -1)] {
        for (cy, sy) in [(y0, 1), (y1, -1)] {
            draw::line(
                img,
                &[(cx + sx * arm, cy), (cx, cy), (cx, cy + sy * arm)],
                ON,
                3,
                false,
            );
        }
    }
}

fn ornate(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    rect_outline(img, b, 0, 2, 0);
    rect_outline(img, b, s + 2, 1, 0);
    let r = s * 3;
    for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        let cx = if cx == x0 { cx + r } else { cx - r };
        let cy = if cy == y0 { cy + r } else { cy - r };
        draw::rectangle(
            img,
            [cx - r, cy - r, cx + r, cy + r],
            Some(OFF),
            Some(ON),
            1,
        );
        draw::polygon(
            img,
            &[
                (cx, cy - r + 2),
                (cx + r - 2, cy),
                (cx, cy + r - 2),
                (cx - r + 2, cy),
            ],
            Some(ON),
            None,
            0,
        );
    }
}

fn shadow(img: &mut GrayImage, b: Box4, s: i32) {
    let (x0, y0, x1, y1) = b;
    fill_rect(img, [x0 + s, y0 + s, x1, y1], ON);
    draw::rectangle(img, [x0, y0, x1 - s, y1 - s], Some(OFF), Some(ON), 2);
}

type FrameFn = fn(&mut GrayImage, Box4, i32);

fn lookup(name: &str) -> Option<FrameFn> {
    let f: FrameFn = match name {
        "line" => |d, b, s| rect_outline(d, b, 0, (s / 2).max(1), 0),
        "thick" => |d, b, s| rect_outline(d, b, 0, s * 2, 0),
        "double" => |d, b, s| {
            rect_outline(d, b, 0, 2, 0);
            rect_outline(d, b, s + 3, 1, 0);
        },
        "rounded" => |d, b, s| rect_outline(d, b, 0, 2, s * 4),
        "dashed" => dashed,
        "dotted" => dotted,
        "ticket" => ticket,
        "stamp" => stamp,
        "film" => film,
        "checker" => checker,
        "zigzag" => zigzag,
        "scallop" => scallop,
        "brackets" => brackets,
        "ornate" => ornate,
        "shadow" => shadow,
        _ => return None,
    };
    Some(f)
}

#[rustfmt::skip]
const ORDER: &[&str] = &[
    "line",
    "thick",
    "double",
    "rounded",
    "dashed",
    "dotted",
    "ticket",
    "stamp",
    "film",
    "checker",
    "zigzag",
    "scallop",
    "brackets",
    "ornate",
    "shadow",
];

/// The frames of `frames.py`, sorted (the catalog lists `sorted(FRAMES)`). Figure names also work as
/// frames, see `has_frame`.
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = ORDER.iter().map(|n| n.to_string()).collect();
    names.sort();
    names
}

/// True for a frame name or any figure name.
pub fn has_frame(name: &str) -> bool {
    lookup(name).is_some() || figures::has_figure(name)
}

/// A mask (255 on the frame) the size of the canvas. An unknown name gives an empty mask.
pub fn frame_mask(
    name: &str,
    canvas: (u32, u32),
    box_: (i32, i32, i32, i32),
    size: i32,
) -> GrayImage {
    let mut mask = GrayImage::new(canvas.0, canvas.1);
    let size = size.max(1);
    if let Some(f) = lookup(name) {
        f(&mut mask, box_, size);
        return mask;
    }
    if !figures::has_figure(name) {
        return mask;
    }
    let tile = size * 3;
    let stamp = figures::figure_mask(name, tile as u32, tile as u32);
    let (x0, y0, x1, y1) = box_;
    let columns = ((x1 - x0 - tile).div_euclid(tile + 2)).max(1);
    let rows = ((y1 - y0 - tile).div_euclid(tile + 2)).max(1);
    for i in 0..=columns {
        let x = x0 + (i * (x1 - x0 - tile)).div_euclid(columns);
        raster::paste_value(&mut mask, ON, x, y0, &stamp);
        raster::paste_value(&mut mask, ON, x, y1 - tile, &stamp);
    }
    for j in 0..=rows {
        let y = y0 + (j * (y1 - y0 - tile)).div_euclid(rows);
        raster::paste_value(&mut mask, ON, x0, y, &stamp);
        raster::paste_value(&mut mask, ON, x1 - tile, y, &stamp);
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn black(m: &GrayImage) -> usize {
        m.as_raw().iter().filter(|&&v| v != 0).count()
    }

    fn fnv(m: &GrayImage) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in m.as_raw() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    #[test]
    fn there_are_fifteen_frames() {
        assert_eq!(names().len(), 15);
        assert!(has_frame("ticket") && has_frame("heart") && !has_frame("nope"));
    }

    #[test]
    fn every_frame_draws() {
        // Same box as the Python test: an "inset" of 4 on the 312x216 grid.
        for name in names() {
            let m = frame_mask(&name, (312, 216), (4, 4, 307, 211), 3);
            assert!(black(&m) > 100, "{name}");
        }
    }

    #[test]
    fn a_figure_works_as_a_frame() {
        let m = frame_mask("heart", (312, 216), (0, 0, 311, 215), 4);
        assert!(black(&m) > 200);
    }

    #[test]
    fn unknown_names_give_an_empty_mask() {
        assert_eq!(black(&frame_mask("nope", (50, 50), (0, 0, 49, 49), 3)), 0);
    }

    #[test]
    fn masks_match_pillow() {
        for (name, box_, size, count, hash) in REF {
            let m = frame_mask(name, (312, 216), *box_, *size);
            assert_eq!(
                (black(&m), fnv(&m)),
                (*count, *hash),
                "{name} {box_:?} {size}"
            );
        }
    }

    #[rustfmt::skip]
    const REF: &[(&str, (i32, i32, i32, i32), i32, usize, u64)] = &[
        ("brackets", (4, 4, 307, 211), 3, 440, 0xbcc2c96599c58c85),
        ("brackets", (20, 30, 150, 100), 2, 296, 0x712786db78768ce5),
        ("brackets", (0, 0, 311, 215), 5, 480, 0x6e050a76a03ee205),
        ("brackets", (10, 10, 60, 40), 1, 152, 0xcc070b4714fdcf95),
        ("brackets", (40, 40, 300, 200), 4, 584, 0x10f6e8e9e244d2c5),
        ("checker", (4, 4, 307, 211), 3, 2512, 0xca06acf93470336b),
        ("checker", (20, 30, 150, 100), 2, 683, 0x4578a903c9340e5a),
        ("checker", (0, 0, 311, 215), 5, 4200, 0x23c9fe7ba3cb6895),
        ("checker", (10, 10, 60, 40), 1, 156, 0x8cb63615235be5c5),
        ("checker", (40, 40, 300, 200), 4, 2660, 0x13a7f126f23f54d1),
        ("dashed", (4, 4, 307, 211), 3, 1023, 0xf3e24a494bcf8e80),
        ("dashed", (20, 30, 150, 100), 2, 399, 0xbd97083d5f9dbfea),
        ("dashed", (0, 0, 311, 215), 5, 1056, 0x77d020f875e41379),
        ("dashed", (10, 10, 60, 40), 1, 164, 0x93a3f6d638931175),
        ("dashed", (40, 40, 300, 200), 4, 843, 0x92758804c6350da2),
        ("dotted", (4, 4, 307, 211), 3, 855, 0x75686b4978239c40),
        ("dotted", (20, 30, 150, 100), 2, 503, 0xb127c302ccf68618),
        ("dotted", (0, 0, 311, 215), 5, 423, 0x1372e1491ebccfd0),
        ("dotted", (10, 10, 60, 40), 1, 320, 0x690b12e6ecd7e7ad),
        ("dotted", (40, 40, 300, 200), 4, 530, 0x703938cb34d60055),
        ("double", (4, 4, 307, 211), 3, 3004, 0x6ad982c9fe71c9c9),
        ("double", (20, 30, 150, 100), 2, 1152, 0xac707f0370f9b87d),
        ("double", (0, 0, 311, 215), 5, 3084, 0x12eef62de3ed4379),
        ("double", (10, 10, 60, 40), 1, 440, 0x0d98ec7bf880ec15),
        ("double", (40, 40, 300, 200), 4, 2456, 0x14d7f6f5eb8101d5),
        ("film", (4, 4, 307, 211), 3, 7616, 0xa9494d2e94bf2765),
        ("film", (20, 30, 150, 100), 2, 2252, 0x7dda2ee46e9ef62d),
        ("film", (0, 0, 311, 215), 5, 12696, 0x44853937fa56b88d),
        ("film", (10, 10, 60, 40), 1, 484, 0xaa1d0fe78652d0e5),
        ("film", (40, 40, 300, 200), 4, 8652, 0x30b3c7d4e6b1ba15),
        ("line", (4, 4, 307, 211), 3, 1020, 0x9d9048df373ea589),
        ("line", (20, 30, 150, 100), 2, 400, 0x7b636303964689dd),
        ("line", (0, 0, 311, 215), 5, 2096, 0xa72320df7b3467f5),
        ("line", (10, 10, 60, 40), 1, 160, 0x50698ded3d82691d),
        ("line", (40, 40, 300, 200), 4, 1672, 0x492deec910cd3e9d),
        ("ornate", (4, 4, 307, 211), 3, 3356, 0xb583b7529e27c4d1),
        ("ornate", (20, 30, 150, 100), 2, 1256, 0x724b552c0d7c8245),
        ("ornate", (0, 0, 311, 215), 5, 4364, 0x306849b1b6247291),
        ("ornate", (10, 10, 60, 40), 1, 440, 0xd81b978e179993ad),
        ("ornate", (40, 40, 300, 200), 4, 3200, 0x1b710eedf03457fd),
        ("rounded", (4, 4, 307, 211), 3, 1988, 0xf1bd086f9941fc19),
        ("rounded", (20, 30, 150, 100), 2, 756, 0xfd97913ce6aae2dd),
        ("rounded", (0, 0, 311, 215), 5, 2012, 0x6b18fba00efed969),
        ("rounded", (10, 10, 60, 40), 1, 296, 0x7496e1c31fd6c19d),
        ("rounded", (40, 40, 300, 200), 4, 1604, 0xb58385d62b5245ed),
        ("scallop", (4, 4, 307, 211), 3, 3698, 0x6855c8196c1bbca5),
        ("scallop", (20, 30, 150, 100), 2, 1868, 0x26d0171305a8cc0b),
        ("scallop", (0, 0, 311, 215), 5, 3077, 0x488f696d523c6754),
        ("scallop", (10, 10, 60, 40), 1, 681, 0x83f8504c1a74aeca),
        ("scallop", (40, 40, 300, 200), 4, 4252, 0xc0f8b8e0e61bc835),
        ("shadow", (4, 4, 307, 211), 3, 3517, 0x4c9907414f512fe4),
        ("shadow", (20, 30, 150, 100), 2, 1168, 0x2e3e745a7c4727ff),
        ("shadow", (0, 0, 311, 215), 5, 4621, 0xd655066492477b4e),
        ("shadow", (10, 10, 60, 40), 1, 383, 0xbb2c4e13c1fc1b20),
        ("shadow", (40, 40, 300, 200), 4, 3280, 0xd6e77ceda8323089),
        ("stamp", (4, 4, 307, 211), 3, 6424, 0x2f9520d805ca5541),
        ("stamp", (20, 30, 150, 100), 2, 1428, 0x4d69a66cc46720bd),
        ("stamp", (0, 0, 311, 215), 5, 11202, 0xfc5a1fcc3910e0cd),
        ("stamp", (10, 10, 60, 40), 1, 246, 0x5658baa0c2102abd),
        ("stamp", (40, 40, 300, 200), 4, 7130, 0xc4b679ece4bd018f),
        ("thick", (4, 4, 307, 211), 3, 6000, 0x4c6676b0ad9460b5),
        ("thick", (20, 30, 150, 100), 2, 1552, 0x5aa5e572a05a01f5),
        ("thick", (0, 0, 311, 215), 5, 10160, 0x347ebf0460e5a675),
        ("thick", (10, 10, 60, 40), 1, 312, 0x895cb2e973070b9d),
        ("thick", (40, 40, 300, 200), 4, 6496, 0xaa3e640a2f3017c5),
        ("ticket", (4, 4, 307, 211), 3, 2060, 0x58e12e11386c0c31),
        ("ticket", (20, 30, 150, 100), 2, 936, 0x7494b36a77d0496d),
        ("ticket", (0, 0, 311, 215), 5, 2036, 0x517f41366a0f8241),
        ("ticket", (10, 10, 60, 40), 1, 376, 0x9e02a43ba978f01d),
        ("ticket", (40, 40, 300, 200), 4, 2026, 0xb5ac9f4ab2c62bcd),
        ("zigzag", (4, 4, 307, 211), 3, 1517, 0x4ef4efacb7f5febc),
        ("zigzag", (20, 30, 150, 100), 2, 593, 0x1768c889ece66c88),
        ("zigzag", (0, 0, 311, 215), 5, 1537, 0x11b24d5a53efb786),
        ("zigzag", (10, 10, 60, 40), 1, 238, 0x9476884a6e56623d),
        ("zigzag", (40, 40, 300, 200), 4, 1251, 0x0eb7b4354d9e5866),
        ("heart", (4, 4, 307, 211), 3, 4752, 0x5e3528a7b495d805),
        ("heart", (20, 30, 150, 100), 2, 1380, 0x6b2715142b4a4139),
        ("heart", (0, 0, 311, 215), 5, 7728, 0xbb73ac945a65c465),
        ("heart", (10, 10, 60, 40), 1, 252, 0x9d090eb9ad5c2ee9),
        ("heart", (40, 40, 300, 200), 4, 4968, 0xf3092bde26f88dad),
        ("star", (4, 4, 307, 211), 3, 3256, 0x7e7e1691c0a79d3d),
        ("star", (20, 30, 150, 100), 2, 828, 0xb654f2e05c3a58a5),
        ("star", (0, 0, 311, 215), 5, 4760, 0x7f59a8630440c2dd),
        ("star", (10, 10, 60, 40), 1, 196, 0x06df20dfc5437619),
        ("star", (40, 40, 300, 200), 4, 3240, 0xd4d329abbb3d1b89),
        ("sun", (4, 4, 307, 211), 3, 6424, 0xc7f691a1107ddf4d),
        ("sun", (20, 30, 150, 100), 2, 1472, 0x60e9da4e1523cc85),
        ("sun", (0, 0, 311, 215), 5, 8792, 0xb8698fe0c62df5cd),
        ("sun", (10, 10, 60, 40), 1, 252, 0x9d090eb9ad5c2ee9),
        ("sun", (40, 40, 300, 200), 4, 5616, 0x4b84ba30841d18d5),
        ("square", (4, 4, 307, 211), 3, 7128, 0x3b87f3e4a27fda6d),
        ("square", (20, 30, 150, 100), 2, 1656, 0x0fe62d35023b3e5d),
        ("square", (0, 0, 311, 215), 5, 12600, 0x3fa456c3718a737d),
        ("square", (10, 10, 60, 40), 1, 252, 0x9d090eb9ad5c2ee9),
        ("square", (40, 40, 300, 200), 4, 7776, 0x58f581a721a92cc5),
        ("cat", (4, 4, 307, 211), 3, 3344, 0xfded8221c7848a85),
        ("cat", (20, 30, 150, 100), 2, 920, 0x2775977f492eec0d),
        ("cat", (0, 0, 311, 215), 5, 7280, 0x8b80993d0fe5ea55),
        ("cat", (10, 10, 60, 40), 1, 84, 0x59e65d3aa407bf91),
        ("cat", (40, 40, 300, 200), 4, 4266, 0x1cc09722f93e415d),
    ];
}
