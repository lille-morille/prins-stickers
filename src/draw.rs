//! Pillow's `ImageDraw` for 8-bit images: rectangle, rounded_rectangle, ellipse, polygon, line (with width
//! and the curved joint), arc, pieslice, chord and point.
//!
//! This is a port of Pillow's own algorithm (`Draw.c` and the Python layer of `ImageDraw.py`), not an
//! imitation: the same integer ellipse walker, scanline polygon filler, wide-line quad and angle clipping
//! tree, so shapes land on the same pixels. Conventions follow Pillow:
//!
//! - boxes are `[x0, y0, x1, y1]` with both corners inclusive; floats are truncated like C's `(int)`;
//! - `fill` / `outline` are pixel values (255 for ink on a mask, 0 for black on a page), `None` skips that part;
//! - an outline equal to the fill value is skipped (as Pillow does), a width of 0 draws no outline;
//! - angles are degrees, clockwise from 3 o'clock;
//! - a box with x1 < x0 or y1 < y0, which Pillow rejects with `ValueError`, draws nothing.

use image::GrayImage;

/// Any number type a caller might have coordinates in.
pub trait Coord: Copy {
    fn to_f64(self) -> f64;
}

macro_rules! coord_impl {
    ($($t:ty),*) => { $(impl Coord for $t { fn to_f64(self) -> f64 { self as f64 } })* };
}
coord_impl!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

/// C's `(int)` on a double, saturating instead of undefined.
fn ci(v: f64) -> i32 {
    v as i32
}

fn box4<C: Coord>(xy: [C; 4]) -> (i32, i32, i32, i32, bool) {
    let f = xy.map(|v| v.to_f64());
    let ordered = !(f[2] < f[0] || f[3] < f[1]);
    (ci(f[0]), ci(f[1]), ci(f[2]), ci(f[3]), ordered)
}

fn pts_i32<C: Coord>(xy: &[(C, C)]) -> Vec<i32> {
    let mut out = Vec::with_capacity(xy.len() * 2);
    for &(x, y) in xy {
        out.push(ci(x.to_f64()));
        out.push(ci(y.to_f64()));
    }
    out
}

// --- low level surface ----------------------------------------------------------------------------

struct Surface<'a> {
    img: &'a mut GrayImage,
    /// Only where this mask is non-zero (polygon outlines with a width).
    mask: Option<&'a GrayImage>,
}

impl<'a> Surface<'a> {
    fn new(img: &'a mut GrayImage) -> Self {
        Surface { img, mask: None }
    }

    fn point(&mut self, x: i32, y: i32, ink: u8) {
        if x >= 0 && (x as u32) < self.img.width() && y >= 0 && (y as u32) < self.img.height() {
            self.img.put_pixel(x as u32, y as u32, image::Luma([ink]));
        }
    }

    fn hline(&mut self, x0: i32, y: i32, x1: i32, ink: u8) {
        let (w, h) = (self.img.width() as i32, self.img.height() as i32);
        if y < 0 || y >= h {
            return;
        }
        let mut x0 = x0;
        let mut x1 = x1;
        if x0 < 0 {
            x0 = 0;
        } else if x0 >= w {
            return;
        }
        if x1 < 0 {
            return;
        } else if x1 >= w {
            x1 = w - 1;
        }
        let mask = self.mask;
        let mut x = x0;
        while x <= x1 {
            if mask.is_none_or(|m| m.get_pixel(x as u32, y as u32).0[0] != 0) {
                self.img.put_pixel(x as u32, y as u32, image::Luma([ink]));
            }
            x += 1;
        }
    }

    /// Pillow's thin line: the start point is drawn, the end point is not.
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, ink: u8) {
        let (mut x0, mut y0) = (x0 as i64, y0 as i64);
        let (x1, y1) = (x1 as i64, y1 as i64);
        let mut dy = y1 - y0;
        let ys: i64;
        if dy < 0 {
            dy = -dy;
            ys = -1;
        } else if dy == 0 {
            if x1 > x0 {
                self.hline(x0 as i32, y0 as i32, (x1 - 1) as i32, ink);
            } else if x0 > x1 {
                self.hline((x1 + 1) as i32, y0 as i32, x0 as i32, ink);
            }
            return;
        } else {
            ys = 1;
        }
        let mut dx = x1 - x0;
        let xs: i64;
        if dx < 0 {
            dx = -dx;
            xs = -1;
        } else {
            xs = 1;
        }
        // Absurdly long segments would take minutes; nothing sensible draws them.
        if dx.max(dy) > 20_000_000 {
            return;
        }
        let clip = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        if dx == 0 {
            for _ in 0..dy {
                self.point(clip(x0), clip(y0), ink);
                y0 += ys;
            }
        } else if dx > dy {
            let n = dx;
            dy += dy;
            let mut e = dy - dx;
            dx += dx;
            for _ in 0..n {
                self.point(clip(x0), clip(y0), ink);
                if e >= 0 {
                    y0 += ys;
                    e -= dx;
                }
                e += dy;
                x0 += xs;
            }
        } else {
            let n = dy;
            dx += dx;
            let mut e = dx - dy;
            dy += dy;
            for _ in 0..n {
                self.point(clip(x0), clip(y0), ink);
                if e >= 0 {
                    x0 += xs;
                    e -= dy;
                }
                e += dx;
                y0 += ys;
            }
        }
    }
}

// --- polygons ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Edge {
    x0: i32,
    y0: i32,
    xmin: i32,
    ymin: i32,
    xmax: i32,
    ymax: i32,
    dx: f32,
}

fn add_edge(x0: i32, y0: i32, x1: i32, y1: i32) -> Edge {
    let (xmin, xmax) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    let (ymin, ymax) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
    let dx = if y0 == y1 {
        0.0
    } else {
        (x1 as i64 - x0 as i64) as f32 / (y1 as i64 - y0 as i64) as f32
    };
    Edge {
        x0,
        y0,
        xmin,
        ymin,
        xmax,
        ymax,
        dx,
    }
}

/// `ROUND_UP` / `ROUND_DOWN` of Pillow, for a C float argument.
fn round_up_f32(f: f32) -> i32 {
    if f >= 0.0 {
        (f + 0.5).floor() as i32
    } else {
        -((f.abs() + 0.5).floor() as i32)
    }
}

fn round_down_f32(f: f32) -> i32 {
    if f >= 0.0 {
        (f - 0.5).ceil() as i32
    } else {
        -((f.abs() - 0.5).ceil() as i32)
    }
}

fn round_up_f64(f: f64) -> i32 {
    if f >= 0.0 {
        (f + 0.5).floor() as i32
    } else {
        -((f.abs() + 0.5).floor() as i32)
    }
}

fn round_down_f64(f: f64) -> i32 {
    if f >= 0.0 {
        (f - 0.5).ceil() as i32
    } else {
        -((f.abs() - 0.5).ceil() as i32)
    }
}

/// Scanline polygon filler (`polygon_generic`).
fn polygon_generic(s: &mut Surface, edges: &[Edge], ink: u8) {
    if edges.is_empty() {
        return;
    }
    let height = s.img.height() as i32;
    let mut ymin: i32 = height - 1;
    let mut ymax: i32 = 0;
    let mut table: Vec<&Edge> = Vec::with_capacity(edges.len());
    for e in edges {
        ymin = ymin.min(e.ymin);
        ymax = ymax.max(e.ymax);
        if e.ymin == e.ymax {
            s.hline(e.xmin, e.ymin, e.xmax, ink);
            continue;
        }
        table.push(e);
    }
    if ymin < 0 {
        ymin = 0;
    }
    if ymax > height {
        ymax = height;
    }
    let mut xx: Vec<f32> = Vec::with_capacity(table.len() * 2);
    let x_at = |e: &Edge, y: i32| ((y as i64 - e.y0 as i64) as f32).mul_add(e.dx, e.x0 as f32);
    for y in ymin..=ymax {
        xx.clear();
        for (i, current) in table.iter().enumerate() {
            if y >= current.ymin && y <= current.ymax {
                xx.push(x_at(current, y));
                let j = xx.len();
                if y == current.ymax && y < ymax {
                    // Needed to draw consistent polygons
                    let v = xx[j - 1];
                    xx.push(v);
                } else if (y == current.ymin || y == current.ymax) && current.dx != 0.0 {
                    // Connect discontiguous corners
                    for other in table.iter().take(i) {
                        if (y != other.ymin && y != other.ymax) || other.dx == 0.0 {
                            continue;
                        }
                        if xx[j - 1].round() == x_at(other, y).round() {
                            let offset = if y == current.ymax { -1 } else { 1 };
                            let adjacent = x_at(current, y + offset);
                            if y + offset >= other.ymin && y + offset <= other.ymax {
                                let adjacent_other = x_at(other, y + offset);
                                if xx[j - 1] > adjacent + 1.0 && xx[j - 1] > adjacent_other + 1.0 {
                                    xx[j - 1] = adjacent.max(adjacent_other).round() + 1.0;
                                } else if xx[j - 1] < adjacent - 1.0
                                    && xx[j - 1] < adjacent_other - 1.0
                                {
                                    xx[j - 1] = adjacent.min(adjacent_other).round() - 1.0;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        xx.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 1;
        while i < xx.len() {
            s.hline(round_up_f32(xx[i - 1]), y, round_down_f32(xx[i]), ink);
            i += 2;
        }
    }
}

/// `ImagingDrawWideLine`: a line of the given width as a filled quad (no joins).
fn wide_line(s: &mut Surface, x0: i32, y0: i32, x1: i32, y1: i32, ink: u8, width: i32) {
    let dx = x1 as i64 - x0 as i64;
    let dy = y1 as i64 - y0 as i64;
    if dx == 0 && dy == 0 {
        s.point(x0, y0, ink);
        return;
    }
    let big = (dx as f64).hypot(dy as f64);
    let small = (width as f64 - 1.0) / 2.0;
    let ratio_max = round_up_f64(small) as f64 / big;
    let ratio_min = round_down_f64(small) as f64 / big;
    let dxmin = round_down_f64(ratio_min * dy as f64) as i64;
    let dxmax = round_down_f64(ratio_max * dy as f64) as i64;
    let dymin = round_down_f64(ratio_min * dx as f64) as i64;
    let dymax = round_down_f64(ratio_max * dx as f64) as i64;
    let c = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let v = [
        (c(x0 as i64 - dxmin), c(y0 as i64 + dymax)),
        (c(x1 as i64 - dxmin), c(y1 as i64 + dymax)),
        (c(x1 as i64 + dxmax), c(y1 as i64 - dymin)),
        (c(x0 as i64 + dxmax), c(y0 as i64 - dymin)),
    ];
    let edges = [
        add_edge(v[0].0, v[0].1, v[1].0, v[1].1),
        add_edge(v[1].0, v[1].1, v[2].0, v[2].1),
        add_edge(v[2].0, v[2].1, v[3].0, v[3].1),
        add_edge(v[3].0, v[3].1, v[0].0, v[0].1),
    ];
    polygon_generic(s, &edges, ink);
}

/// `ImagingDrawPolygon`, on flattened integer coordinates.
fn draw_polygon(s: &mut Surface, xy: &[i32], ink: u8, fill: bool, width: i32) {
    let count = xy.len() / 2;
    if count == 0 {
        return;
    }
    if fill {
        let mut edges: Vec<Edge> = Vec::with_capacity(count);
        let mut i = 0;
        while i + 1 < count {
            let (x0, y0, x1, y1) = (xy[i * 2], xy[i * 2 + 1], xy[i * 2 + 2], xy[i * 2 + 3]);
            if y0 == y1 && i != 0 && y0 == xy[i * 2 - 1] {
                // A horizontal line that immediately follows another horizontal line.
                if let Some(last) = edges.last_mut() {
                    if x1 > x0 && x0 > xy[i * 2 - 2] {
                        last.xmax = x1;
                        i += 1;
                        continue;
                    } else if x1 < x0 && x0 < xy[i * 2 - 2] {
                        last.xmin = x1;
                        i += 1;
                        continue;
                    }
                }
            }
            edges.push(add_edge(x0, y0, x1, y1));
            i += 1;
        }
        if xy[i * 2] != xy[0] || xy[i * 2 + 1] != xy[1] {
            edges.push(add_edge(xy[i * 2], xy[i * 2 + 1], xy[0], xy[1]));
        }
        polygon_generic(s, &edges, ink);
    } else if width == 1 {
        for i in 0..count - 1 {
            s.line(xy[i * 2], xy[i * 2 + 1], xy[i * 2 + 2], xy[i * 2 + 3], ink);
        }
        let i = count - 1;
        s.line(xy[i * 2], xy[i * 2 + 1], xy[0], xy[1], ink);
    } else {
        for i in 0..count - 1 {
            wide_line(
                s,
                xy[i * 2],
                xy[i * 2 + 1],
                xy[i * 2 + 2],
                xy[i * 2 + 3],
                ink,
                width,
            );
        }
        let i = count - 1;
        wide_line(s, xy[i * 2], xy[i * 2 + 1], xy[0], xy[1], ink, width);
    }
}

// --- rectangle -----------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw_rectangle(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    ink: u8,
    fill: bool,
    width: i32,
) {
    let (mut y0, mut y1) = (y0, y1);
    if y0 > y1 {
        std::mem::swap(&mut y0, &mut y1);
    }
    let height = s.img.height() as i32;
    if fill {
        if y0 < 0 {
            y0 = 0;
        } else if y0 >= height {
            return;
        }
        if y1 < 0 {
            return;
        } else if y1 > height {
            y1 = height;
        }
        for y in y0..=y1 {
            s.hline(x0, y, x1, ink);
        }
    } else {
        let width = if width == 0 { 1 } else { width };
        let (w, h) = (s.img.width() as i64, s.img.height() as i64);
        // Pillow draws `width` rings, each a pair of rows and a pair of columns. A ring whose rows and
        // columns are all off the canvas draws nothing, so only visit the others (a huge width is common
        // for "fill the box" and would otherwise loop for minutes).
        let last = width as i64 - 1;
        let (y0l, y1l, x0l, x1l) = (y0 as i64, y1 as i64, x0 as i64, x1 as i64);
        let mut rings: Vec<i64> = Vec::new();
        for (lo, hi) in [
            (-y0l, h - 1 - y0l),
            (y1l - (h - 1), y1l),
            (x1l - (w - 1), x1l),
            (-x0l, w - 1 - x0l),
        ] {
            let (lo, hi) = (lo.max(0), hi.min(last));
            if lo <= hi {
                rings.extend(lo..=hi);
            }
        }
        rings.sort_unstable();
        rings.dedup();
        for i in rings {
            let i = i as i32;
            s.hline(x0, y0.saturating_add(i), x1, ink);
            s.hline(x0, y1.saturating_sub(i), x1, ink);
            s.line(
                x1.saturating_sub(i),
                y0.saturating_add(width),
                x1.saturating_sub(i),
                y1.saturating_sub(width).saturating_add(1),
                ink,
            );
            s.line(
                x0.saturating_add(i),
                y0.saturating_add(width),
                x0.saturating_add(i),
                y1.saturating_sub(width).saturating_add(1),
                ink,
            );
        }
    }
}

// --- the integer ellipse walker ---------------------------------------------------------------------------

/// A quarter of an ellipse as integer points on a doubled grid (`quarter_state`).
#[derive(Clone, Default)]
struct Quarter {
    cx: i32,
    cy: i32,
    ex: i32,
    ey: i32,
    a2: i64,
    b2: i64,
    a2b2: i64,
    finished: bool,
}

impl Quarter {
    fn new(a: i32, b: i32) -> Quarter {
        if a < 0 || b < 0 {
            return Quarter {
                finished: true,
                ..Default::default()
            };
        }
        let a2 = a as i64 * a as i64;
        let b2 = b as i64 * b as i64;
        Quarter {
            cx: a,
            cy: b % 2,
            ex: a % 2,
            ey: b,
            a2,
            b2,
            a2b2: a2 * b2,
            finished: false,
        }
    }

    fn delta(&self, x: i64, y: i64) -> i64 {
        (self.a2 * y * y + self.b2 * x * x - self.a2b2).abs()
    }

    fn next(&mut self) -> Option<(i32, i32)> {
        if self.finished {
            return None;
        }
        let ret = (self.cx, self.cy);
        if self.cx == self.ex && self.cy == self.ey {
            self.finished = true;
        } else {
            let mut nx = self.cx;
            let mut ny = self.cy + 2;
            let mut ndelta = self.delta(nx as i64, ny as i64);
            if nx > 1 {
                let newdelta = self.delta(self.cx as i64 - 2, self.cy as i64 + 2);
                if ndelta > newdelta {
                    nx = self.cx - 2;
                    ny = self.cy + 2;
                    ndelta = newdelta;
                }
                let newdelta = self.delta(self.cx as i64 - 2, self.cy as i64);
                if ndelta > newdelta {
                    nx = self.cx - 2;
                    ny = self.cy;
                }
            }
            self.cx = nx;
            self.cy = ny;
        }
        Some(ret)
    }
}

/// Horizontal segments of a complete ellipse with a given thickness (`ellipse_state`).
struct Ellipse {
    st_o: Quarter,
    st_i: Quarter,
    py: i32,
    pl: i32,
    pr: i32,
    cy: [i32; 4],
    cl: [i32; 4],
    cr: [i32; 4],
    bufcnt: usize,
    finished: bool,
    leftmost: i32,
}

impl Ellipse {
    fn new(a: i32, b: i32, w: i32) -> Ellipse {
        let mut s = Ellipse {
            st_o: Quarter::new(a, b),
            st_i: Quarter::default(),
            py: 0,
            pl: 0,
            pr: 0,
            cy: [0; 4],
            cl: [0; 4],
            cr: [0; 4],
            bufcnt: 0,
            finished: true,
            leftmost: a % 2,
        };
        if w < 1 {
            return s;
        }
        match s.st_o.next() {
            None => {}
            Some((x, y)) => {
                s.pr = x;
                s.py = y;
                s.finished = false;
                let shrink = 2 * (w as i64 - 1);
                let ai = (a as i64 - shrink).max(-1) as i32;
                let bi = (b as i64 - shrink).max(-1) as i32;
                s.st_i = Quarter::new(ai, bi);
                s.pl = s.leftmost;
            }
        }
        s
    }

    fn next(&mut self) -> Option<(i32, i32, i32)> {
        if self.bufcnt == 0 {
            if self.finished {
                return None;
            }
            let y = self.py;
            let mut l = self.pl;
            let r = self.pr;
            let mut last = None;
            loop {
                match self.st_o.next() {
                    Some((cx, cy)) if cy <= y => {
                        last = Some((cx, cy));
                        continue;
                    }
                    Some((cx, cy)) => {
                        self.pr = cx;
                        self.py = cy;
                    }
                    None => {
                        self.finished = true;
                    }
                }
                break;
            }
            let _ = last;
            let mut cx_after = 0;
            let mut ended = true;
            loop {
                match self.st_i.next() {
                    Some((cx, cy)) if cy <= y => {
                        l = cx;
                        continue;
                    }
                    Some((cx, _)) => {
                        cx_after = cx;
                        ended = false;
                    }
                    None => {}
                }
                break;
            }
            self.pl = if ended { self.leftmost } else { cx_after };

            let push = |s: &mut Ellipse, cl: i32, cy: i32, cr: i32| {
                s.cl[s.bufcnt] = cl;
                s.cy[s.bufcnt] = cy;
                s.cr[s.bufcnt] = cr;
                s.bufcnt += 1;
            };
            if (l > 0 || l < r) && y > 0 {
                push(self, if l == 0 { 2 } else { l }, y, r);
            }
            if y > 0 {
                push(self, -r, y, -l);
            }
            if l > 0 || l < r {
                push(self, if l == 0 { 2 } else { l }, -y, r);
            }
            push(self, -r, -y, -l);
        }
        self.bufcnt -= 1;
        Some((
            self.cl[self.bufcnt],
            self.cy[self.bufcnt],
            self.cr[self.bufcnt],
        ))
    }
}

/// Biggest ellipse the integer walker is asked for. Beyond this a canvas-sized picture cannot show
/// the curve anyway, and the arithmetic would overflow.
const MAX_ELLIPSE: i64 = 30_000;

#[allow(clippy::too_many_arguments)]
fn ellipse_new(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    ink: u8,
    fill: bool,
    width: i32,
) {
    let a = x1 as i64 - x0 as i64;
    let b = y1 as i64 - y0 as i64;
    if !(0..=MAX_ELLIPSE).contains(&a) || !(0..=MAX_ELLIPSE).contains(&b) {
        return;
    }
    let (a, b) = (a as i32, b as i32);
    let width = if fill { a + b } else { width };
    let mut st = Ellipse::new(a, b, width);
    while let Some((xa, y, xb)) = st.next() {
        s.hline(x0 + (xa + a) / 2, y0 + (y + b) / 2, x0 + (xb + a) / 2, ink);
    }
}

// --- clipping tree for arcs, chords and pies ------------------------------------------------------------------

enum Clip {
    And(Box<Clip>, Box<Clip>),
    Or(Box<Clip>, Box<Clip>),
    /// Half plane `a*x + b*y + c >= 0`.
    Half {
        a: f64,
        b: f64,
        c: f64,
    },
}

impl Clip {
    fn transpose(&mut self) {
        match self {
            Clip::Half { a, b, .. } => std::mem::swap(a, b),
            Clip::And(l, r) | Clip::Or(l, r) => {
                l.transpose();
                r.transpose();
            }
        }
    }
}

/// Sorted open/close events (x, +1 or -1) of the disjoint pieces of a horizontal segment inside the tree.
fn do_clip(root: &Option<Clip>, x0: i32, y: i32, x1: i32) -> Vec<(i32, i8)> {
    match root {
        None => vec![(x0, 1), (x1, -1)],
        Some(node) => clip_node(node, x0, y, x1),
    }
}

fn clip_node(node: &Clip, x0: i32, y: i32, x1: i32) -> Vec<(i32, i8)> {
    match node {
        Clip::Half { a, b, c } => {
            let eps = 1e-9;
            let (mut x0, mut x1) = (x0, x1);
            let (a, b, c) = (*a, *b, *c);
            if a.abs() < eps {
                if b * y as f64 + c < -eps {
                    x0 = 1;
                    x1 = 0;
                }
            } else {
                let ix = -(b * y as f64 + c) / a;
                if a * x0 as f64 + b * y as f64 + c < eps {
                    x0 = (x0 as f64).max(ix).round() as i32;
                }
                if a * x1 as f64 + b * y as f64 + c < eps {
                    x1 = (x1 as f64).min(ix).round() as i32;
                }
            }
            if x0 <= x1 {
                vec![(x0, 1), (x1, -1)]
            } else {
                Vec::new()
            }
        }
        Clip::And(l, r) | Clip::Or(l, r) => {
            let is_or = matches!(node, Clip::Or(..));
            let l1 = clip_node(l, x0, y, x1);
            let l2 = clip_node(r, x0, y, x1);
            let (mut i1, mut i2) = (0usize, 0usize);
            let (mut k1, mut k2) = (0i32, 0i32);
            let mut out: Vec<(i32, i8)> = Vec::new();
            while i1 < l1.len() || i2 < l2.len() {
                let take_first = i2 >= l2.len()
                    || (i1 < l1.len()
                        && (l1[i1].0 < l2[i2].0 || (l1[i1].0 == l2[i2].0 && l1[i1].1 > l2[i2].1)));
                let t = if take_first {
                    let t = l1[i1];
                    k1 += t.1 as i32;
                    i1 += 1;
                    t
                } else {
                    let t = l2[i2];
                    k2 += t.1 as i32;
                    i2 += 1;
                    t
                };
                let tail_type = out.last().map(|e| e.1);
                let keep = if is_or {
                    (t.1 == 1 && (tail_type.is_none() || tail_type == Some(-1)))
                        || (t.1 == -1 && k1 == 0 && k2 == 0)
                } else {
                    (t.1 == 1 && (tail_type.is_none() || tail_type == Some(-1)) && k1 > 0 && k2 > 0)
                        || (t.1 == -1 && tail_type == Some(1) && (k1 == 0 || k2 == 0))
                };
                if keep {
                    out.push(t);
                }
            }
            out
        }
    }
}

/// An ellipse together with a clipping tree: the segments of a ring or disc cut to a wedge.
struct ClipEllipse {
    ellipse: Ellipse,
    root: Option<Clip>,
    pending: std::collections::VecDeque<(i32, i32, i32)>,
}

impl ClipEllipse {
    fn next(&mut self) -> Option<(i32, i32, i32)> {
        loop {
            if let Some(seg) = self.pending.pop_front() {
                return Some(seg);
            }
            let (x0, y, x1) = self.ellipse.next()?;
            let events = do_clip(&self.root, x0, y, x1);
            for pair in events.chunks(2) {
                if pair.len() == 2 {
                    self.pending.push_back((pair[0].0, y, pair[1].0));
                }
            }
        }
    }
}

fn half(a: f64, b: f64, c: f64) -> Clip {
    Clip::Half { a, b, c }
}

fn bx(c: Clip) -> Box<Clip> {
    Box::new(c)
}

/// Resulting angles satisfy 0 <= al < 360, al <= ar <= al + 360 (C floats).
fn normalize_angles(al: &mut f32, ar: &mut f32) {
    if *ar - *al >= 360.0 {
        *al = 0.0;
        *ar = 360.0;
    } else {
        let a = *al;
        let inner: f64 = if a < 0.0 {
            360.0 - fmod(-(a as f64), 360.0)
        } else {
            a as f64
        };
        let new_al = fmod(inner, 360.0) as f32;
        *al = new_al;
        let r = *ar;
        let diff: f64 = if r < new_al {
            360.0 - fmod((new_al - r) as f64, 360.0)
        } else {
            (r - new_al) as f64
        };
        *ar = (new_al as f64 + fmod(diff, 360.0)) as f32;
    }
}

/// C's `fmod`.
fn fmod(a: f64, b: f64) -> f64 {
    a % b
}

const PI: f64 = std::f64::consts::PI;

fn arc_tree(a: i32, b: i32, al: f32, ar: f32) -> Option<Clip> {
    if a < b {
        let mut root = arc_tree(b, a, 90.0 - ar, 90.0 - al);
        if let Some(r) = root.as_mut() {
            r.transpose();
        }
        return root;
    }
    let (mut al, mut ar) = (al, ar);
    normalize_angles(&mut al, &mut ar);
    if ar == al + 360.0 {
        return None;
    }
    let (af, bf) = (a as f64, b as f64);
    let (alf, arf) = (al as f64, ar as f64);
    let lc = half(
        -af * (alf * PI / 180.0).sin(),
        bf * (alf * PI / 180.0).cos(),
        (af * af - bf * bf) * (alf * PI / 90.0).sin() / 2.0,
    );
    let rc = half(
        af * (arf * PI / 180.0).sin(),
        -bf * (arf * PI / 180.0).cos(),
        (bf * bf - af * af) * (arf * PI / 90.0).sin() / 2.0,
    );
    let and_or = |narrow: bool, l: Clip, r: Clip| {
        if narrow {
            Clip::And(bx(l), bx(r))
        } else {
            Clip::Or(bx(l), bx(r))
        }
    };
    let narrow = ar - al < 180.0;
    if fmod(alf, 180.0) == 0.0 || fmod(arf, 180.0) == 0.0 {
        Some(and_or(narrow, lc, rc))
    } else if (((al / 180.0) as i32) + ((ar / 180.0) as i32)) % 2 == 1 {
        let lb = if ((al / 180.0) as i32) % 2 == 0 {
            1.0
        } else {
            -1.0
        };
        let rb = if ((ar / 180.0) as i32) % 2 == 0 {
            1.0
        } else {
            -1.0
        };
        Some(Clip::Or(
            bx(Clip::And(bx(half(0.0, lb, 0.0)), bx(lc))),
            bx(Clip::And(bx(half(0.0, rb, 0.0)), bx(rc))),
        ))
    } else {
        let rb = if !(180.0..=540.0).contains(&ar) {
            1.0
        } else {
            -1.0
        };
        Some(and_or(narrow, and_or(narrow, lc, rc), half(0.0, rb, 0.0)))
    }
}

fn chord_line_tree(a: i32, b: i32, w: i32, al: f32, ar: f32) -> Option<Clip> {
    let (af, bf) = (a as f64, b as f64);
    let xl = af * (al as f64 * PI / 180.0).cos();
    let xr = af * (ar as f64 * PI / 180.0).cos();
    let yl = bf * (al as f64 * PI / 180.0).sin();
    let yr = bf * (ar as f64 * PI / 180.0).sin();
    let la = yr - yl;
    let lb = xl - xr;
    let lcc = -(la * xl + lb * yl);
    let rcc = 2.0 * w as f64 * (la.powi(2) + lb.powi(2)).sqrt() - lcc;
    Some(Clip::And(bx(half(la, lb, lcc)), bx(half(-la, -lb, rcc))))
}

fn pie_side_tree(a: i32, b: i32, w: i32, al: f32) -> Option<Clip> {
    let xl = a as f64 * (al as f64 * PI / 180.0).cos();
    let yl = b as f64 * (al as f64 * PI / 180.0).sin();
    let a1 = -yl;
    let b1 = xl;
    let c1 = w as f64 * (a1 * a1 + b1 * b1).sqrt();
    Some(Clip::And(
        bx(Clip::And(bx(half(a1, b1, c1)), bx(half(-a1, -b1, c1)))),
        bx(half(b1, -a1, 0.0)),
    ))
}

fn chord_tree(a: i32, b: i32, al: f32, ar: f32) -> Option<Clip> {
    let (af, bf) = (a as f64, b as f64);
    let xl = af * (al as f64 * PI / 180.0).cos();
    let xr = af * (ar as f64 * PI / 180.0).cos();
    let yl = bf * (al as f64 * PI / 180.0).sin();
    let yr = bf * (ar as f64 * PI / 180.0).sin();
    let ca = yr - yl;
    let cb = xl - xr;
    Some(half(ca, cb, -(ca * xl + cb * yl)))
}

fn pie_tree(a: i32, b: i32, al: f32, ar: f32) -> Option<Clip> {
    let (af, bf) = (a as f64, b as f64);
    let xl = af * (al as f64 * PI / 180.0).cos();
    let xr = af * (ar as f64 * PI / 180.0).cos();
    let yl = bf * (al as f64 * PI / 180.0).sin();
    let yr = bf * (ar as f64 * PI / 180.0).sin();
    let lc = half(-yl, xl, 0.0);
    let rc = half(yr, -xr, 0.0);
    let root = if ar - al < 180.0 {
        Clip::And(bx(lc), bx(rc))
    } else {
        Clip::Or(bx(lc), bx(rc))
    };
    if ar - al < 90.0 {
        // One more half plane to avoid spikes.
        let spike = half((xl + xr) / 2.0, (yl + yr) / 2.0, 0.0);
        Some(Clip::And(bx(root), bx(spike)))
    } else {
        Some(root)
    }
}

#[derive(Clone, Copy)]
enum Wedge {
    Arc,
    Chord,
    ChordLine,
    Pie,
    PieSide,
}

#[allow(clippy::too_many_arguments)]
fn clip_ellipse_new(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    start: f32,
    end: f32,
    ink: u8,
    width: i32,
    kind: Wedge,
) {
    let a = x1 as i64 - x0 as i64;
    let b = y1 as i64 - y0 as i64;
    if !(0..=MAX_ELLIPSE).contains(&a) || !(0..=MAX_ELLIPSE).contains(&b) {
        return;
    }
    let (a, b) = (a as i32, b as i32);
    let (ellipse, root) = match kind {
        Wedge::Arc => (Ellipse::new(a, b, width), arc_tree(a, b, start, end)),
        Wedge::Chord => (Ellipse::new(a, b, width), chord_tree(a, b, start, end)),
        Wedge::ChordLine => (
            Ellipse::new(a, b, a + b + 1),
            chord_line_tree(a, b, width, start, end),
        ),
        Wedge::Pie => (Ellipse::new(a, b, width), pie_tree(a, b, start, end)),
        Wedge::PieSide => (
            Ellipse::new(a, b, a + b + 1),
            pie_side_tree(a, b, width, start),
        ),
    };
    let mut st = ClipEllipse {
        ellipse,
        root,
        pending: Default::default(),
    };
    while let Some((xa, y, xb)) = st.next() {
        s.hline(x0 + (xa + a) / 2, y0 + (y + b) / 2, x0 + (xb + a) / 2, ink);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_arc(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    start: f32,
    end: f32,
    ink: u8,
    width: i32,
) {
    let (mut start, mut end) = (start, end);
    normalize_angles(&mut start, &mut end);
    if start + 360.0 == end {
        return ellipse_new(s, x0, y0, x1, y1, ink, false, width);
    }
    if start == end {
        return;
    }
    clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, width, Wedge::Arc);
}

#[allow(clippy::too_many_arguments)]
fn draw_chord(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    start: f32,
    end: f32,
    ink: u8,
    fill: bool,
    width: i32,
) {
    let (mut start, mut end) = (start, end);
    normalize_angles(&mut start, &mut end);
    if start + 360.0 == end {
        return ellipse_new(s, x0, y0, x1, y1, ink, fill, width);
    }
    if start == end {
        return;
    }
    if fill {
        let w = (x1 as i64 - x0 as i64 + y1 as i64 - y0 as i64 + 1).min(i32::MAX as i64) as i32;
        clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, w, Wedge::Chord);
    } else {
        clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, width, Wedge::ChordLine);
        clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, width, Wedge::Chord);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pieslice(
    s: &mut Surface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    start: f32,
    end: f32,
    ink: u8,
    fill: bool,
    width: i32,
) {
    let (mut start, mut end) = (start, end);
    normalize_angles(&mut start, &mut end);
    if start + 360.0 == end {
        return ellipse_new(s, x0, y0, x1, y1, ink, fill, width);
    }
    if start == end {
        return;
    }
    if fill {
        let w = (x1 as i64 + y1 as i64 - x0 as i64 - y0 as i64).min(i32::MAX as i64) as i32;
        clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, w, Wedge::Pie);
    } else {
        clip_ellipse_new(s, x0, y0, x1, y1, start, 0.0, ink, width, Wedge::PieSide);
        clip_ellipse_new(s, x0, y0, x1, y1, end, 0.0, ink, width, Wedge::PieSide);
        let xc = ((x0 as f64 + x1 as f64 - width as f64) / 2.0).round() as i32;
        let yc = ((y0 as f64 + y1 as f64 - width as f64) / 2.0).round() as i32;
        ellipse_new(
            s,
            xc,
            yc,
            xc.saturating_add(width - 1),
            yc.saturating_add(width - 1),
            ink,
            true,
            0,
        );
        clip_ellipse_new(s, x0, y0, x1, y1, start, end, ink, width, Wedge::Pie);
    }
}

// --- Python level (ImageDraw.py) -----------------------------------------------------------------------------

/// Python's `round()` on a float: half to even.
pub fn py_round(v: f64) -> f64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 {
        2.0 * (v / 2.0).round()
    } else {
        r
    }
}

/// Pillow takes angles as C floats.
fn f32a(v: f64) -> f32 {
    v as f32
}

/// `ImageDraw.point`: single pixels.
pub fn point<C: Coord>(img: &mut GrayImage, xy: &[(C, C)], fill: u8) {
    let mut s = Surface::new(img);
    for p in pts_i32(xy).chunks(2) {
        s.point(p[0], p[1], fill);
    }
}

/// `ImageDraw.rectangle`.
pub fn rectangle<C: Coord>(
    img: &mut GrayImage,
    xy: [C; 4],
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    let (x0, y0, x1, y1, ordered) = box4(xy);
    if !ordered {
        return;
    }
    let mut s = Surface::new(img);
    let width = width.min(i32::MAX as u32) as i32;
    if let Some(f) = fill {
        draw_rectangle(&mut s, x0, y0, x1, y1, f, true, 0);
    }
    if let Some(o) = outline {
        if Some(o) != fill && width != 0 {
            draw_rectangle(&mut s, x0, y0, x1, y1, o, false, width);
        }
    }
}

/// `ImageDraw.ellipse`.
pub fn ellipse<C: Coord>(
    img: &mut GrayImage,
    xy: [C; 4],
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    let (x0, y0, x1, y1, ordered) = box4(xy);
    if !ordered {
        return;
    }
    let mut s = Surface::new(img);
    let width = width.min(i32::MAX as u32) as i32;
    if let Some(f) = fill {
        ellipse_new(&mut s, x0, y0, x1, y1, f, true, 0);
    }
    if let Some(o) = outline {
        if Some(o) != fill && width != 0 {
            ellipse_new(&mut s, x0, y0, x1, y1, o, false, width);
        }
    }
}

/// `ImageDraw.arc`: `fill` is the line colour.
pub fn arc<C: Coord>(img: &mut GrayImage, xy: [C; 4], start: f64, end: f64, fill: u8, width: u32) {
    let (x0, y0, x1, y1, ordered) = box4(xy);
    if !ordered || width == 0 {
        return;
    }
    let width = width.min(i32::MAX as u32) as i32;
    draw_arc(
        &mut Surface::new(img),
        x0,
        y0,
        x1,
        y1,
        f32a(start),
        f32a(end),
        fill,
        width,
    );
}

/// `ImageDraw.pieslice`.
pub fn pieslice<C: Coord>(
    img: &mut GrayImage,
    xy: [C; 4],
    start: f64,
    end: f64,
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    let (x0, y0, x1, y1, ordered) = box4(xy);
    if !ordered {
        return;
    }
    let mut s = Surface::new(img);
    let width = width.min(i32::MAX as u32) as i32;
    if let Some(f) = fill {
        draw_pieslice(&mut s, x0, y0, x1, y1, f32a(start), f32a(end), f, true, 0);
    }
    if let Some(o) = outline {
        if Some(o) != fill && width != 0 {
            draw_pieslice(
                &mut s,
                x0,
                y0,
                x1,
                y1,
                f32a(start),
                f32a(end),
                o,
                false,
                width,
            );
        }
    }
}

/// `ImageDraw.chord`.
pub fn chord<C: Coord>(
    img: &mut GrayImage,
    xy: [C; 4],
    start: f64,
    end: f64,
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    let (x0, y0, x1, y1, ordered) = box4(xy);
    if !ordered {
        return;
    }
    let mut s = Surface::new(img);
    let width = width.min(i32::MAX as u32) as i32;
    if let Some(f) = fill {
        draw_chord(&mut s, x0, y0, x1, y1, f32a(start), f32a(end), f, true, 0);
    }
    if let Some(o) = outline {
        if Some(o) != fill && width != 0 {
            draw_chord(
                &mut s,
                x0,
                y0,
                x1,
                y1,
                f32a(start),
                f32a(end),
                o,
                false,
                width,
            );
        }
    }
}

/// `ImageDraw.polygon`. With a width above 1 the outline is drawn only inside the polygon, as Pillow does.
pub fn polygon<C: Coord>(
    img: &mut GrayImage,
    xy: &[(C, C)],
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    if xy.is_empty() {
        return;
    }
    let pts = pts_i32(xy);
    let width = width.min(i32::MAX as u32 / 2) as i32;
    if let Some(f) = fill {
        draw_polygon(&mut Surface::new(img), &pts, f, true, 0);
    }
    if let Some(o) = outline {
        if Some(o) != fill && width != 0 {
            if width == 1 {
                draw_polygon(&mut Surface::new(img), &pts, o, false, 1);
            } else {
                // To avoid expanding the polygon outwards, use the fill as a mask.
                let mut mask = GrayImage::new(img.width(), img.height());
                draw_polygon(&mut Surface::new(&mut mask), &pts, 255, true, 0);
                let mut s = Surface {
                    img,
                    mask: Some(&mask),
                };
                draw_polygon(&mut s, &pts, o, false, width * 2 - 1);
            }
        }
    }
}

/// `ImageDraw.rounded_rectangle` (all four corners rounded).
pub fn rounded_rectangle<C: Coord>(
    img: &mut GrayImage,
    xy: [C; 4],
    radius: f64,
    fill: Option<u8>,
    outline: Option<u8>,
    width: u32,
) {
    let f = xy.map(|v| v.to_f64());
    let (fx0, fy0, fx1, fy1) = (f[0], f[1], f[2], f[3]);
    if fx1 < fx0 || fy1 < fy0 {
        return; // Pillow raises ValueError.
    }
    let mut d = (fx1 - fx0).min(fy1 - fy0).min(radius * 2.0);
    // (Clamped so that the sums below cannot overflow; nothing visible lives out there anyway.)
    let whole = |v: f64| py_round(v).clamp(-1.0e15, 1.0e15) as i64;
    let (x0, y0, x1, y1) = (whole(fx0), whole(fy0), whole(fx1), whole(fy1));
    let full_x = d >= (x1 - x0) as f64 - 1.0;
    if full_x {
        // The two left and two right corners are joined.
        d = (x1 - x0) as f64;
    }
    let full_y = d >= (y1 - y0) as f64 - 1.0;
    if full_y {
        // The two top and two bottom corners are joined.
        d = (y1 - y0) as f64;
    }
    if full_x && full_y {
        // If all corners are joined, that is a circle.
        return ellipse(img, xy, fill, outline, width);
    }
    if d == 0.0 || d.is_nan() {
        return rectangle(img, xy, fill, outline, width);
    }
    let r = (d / 2.0).floor().clamp(-1.0e15, 1.0e15) as i64;
    let w = width.min(i32::MAX as u32 / 4) as i64;
    let fx = |v: i64| v as f64;
    // (box, start, end)
    let parts: Vec<([f64; 4], f64, f64)> = if full_x {
        vec![
            ([fx(x0), fx(y0), x0 as f64 + d, y0 as f64 + d], 180.0, 360.0),
            ([fx(x0), y1 as f64 - d, x0 as f64 + d, fx(y1)], 0.0, 180.0),
        ]
    } else if full_y {
        vec![
            ([fx(x0), fx(y0), x0 as f64 + d, y0 as f64 + d], 90.0, 270.0),
            ([x1 as f64 - d, fx(y0), fx(x1), y0 as f64 + d], 270.0, 90.0),
        ]
    } else {
        vec![
            ([fx(x0), fx(y0), x0 as f64 + d, y0 as f64 + d], 180.0, 270.0),
            ([x1 as f64 - d, fx(y0), fx(x1), y0 as f64 + d], 270.0, 360.0),
            ([x1 as f64 - d, y1 as f64 - d, fx(x1), fx(y1)], 0.0, 90.0),
            ([fx(x0), y1 as f64 - d, x0 as f64 + d, fx(y1)], 90.0, 180.0),
        ]
    };
    let rect = |img: &mut GrayImage, b: [i64; 4], ink: u8| {
        rectangle(img, b.map(|v| v as f64), Some(ink), None, 0);
    };
    if let Some(fill_ink) = fill {
        for (b, a0, a1) in &parts {
            pieslice(img, *b, *a0, *a1, Some(fill_ink), None, 0);
        }
        if full_x {
            rect(img, [x0, y0 + r + 1, x1, y1 - r - 1], fill_ink);
        } else if x1 - r - 1 >= x0 + r + 1 {
            rect(img, [x0 + r + 1, y0, x1 - r - 1, y1], fill_ink);
        }
        if !full_x && !full_y {
            rect(img, [x0, y0 + r + 1, x0 + r, y1 - r - 1], fill_ink);
            rect(img, [x1 - r, y0 + r + 1, x1, y1 - r - 1], fill_ink);
        }
    }
    if let Some(ink) = outline {
        if Some(ink) != fill && width != 0 {
            for (b, a0, a1) in &parts {
                arc(img, *b, *a0, *a1, ink, width);
            }
            if !full_x {
                rect(img, [x0 + r + 1, y0, x1 - r - 1, y0 + w - 1], ink);
                rect(img, [x0 + r + 1, y1 - w + 1, x1 - r - 1, y1], ink);
            }
            if !full_y {
                rect(img, [x0, y0 + r + 1, x0 + w - 1, y1 - r - 1], ink);
                rect(img, [x1 - w + 1, y0 + r + 1, x1, y1 - r - 1], ink);
            }
        }
    }
}

/// `ImageDraw.line`: a polyline. `width` 1 is Pillow's thin line; a bigger width draws each segment as a
/// quad, and `curve_joint` (Pillow's `joint="curve"`, only for widths above 4) rounds the corners.
pub fn line<C: Coord>(img: &mut GrayImage, xy: &[(C, C)], fill: u8, width: u32, curve_joint: bool) {
    if width == 0 || xy.is_empty() {
        return;
    }
    let width_i = width.min(i32::MAX as u32 / 4) as i32;
    {
        let pts = pts_i32(xy);
        let mut s = Surface::new(img);
        let n = pts.len() / 2;
        if width_i == 1 {
            for i in 0..n.saturating_sub(1) {
                s.line(
                    pts[i * 2],
                    pts[i * 2 + 1],
                    pts[i * 2 + 2],
                    pts[i * 2 + 3],
                    fill,
                );
            }
            if n >= 2 {
                let i = n - 2;
                s.point(pts[i * 2 + 2], pts[i * 2 + 3], fill);
            }
        } else {
            for i in 0..n.saturating_sub(1) {
                wide_line(
                    &mut s,
                    pts[i * 2],
                    pts[i * 2 + 1],
                    pts[i * 2 + 2],
                    pts[i * 2 + 3],
                    fill,
                    width_i,
                );
            }
        }
    }
    if curve_joint && width_i > 4 {
        let points: Vec<(f64, f64)> = xy.iter().map(|&(x, y)| (x.to_f64(), y.to_f64())).collect();
        let wf = width_i as f64;
        for i in 1..points.len().saturating_sub(1) {
            let point = points[i];
            let angle_of = |start: (f64, f64), end: (f64, f64)| {
                (end.0 - start.0)
                    .atan2(start.1 - end.1)
                    .to_degrees()
                    .rem_euclid(360.0)
            };
            let angles = [
                angle_of(points[i - 1], point),
                angle_of(point, points[i + 1]),
            ];
            if angles[0] == angles[1] {
                // This is a straight line, so no joint is required.
                continue;
            }
            let coord_at_angle = |coord: (f64, f64), angle: f64| {
                let angle = angle - 90.0;
                let distance = wf / 2.0 - 1.0;
                let step = |p: f64, d: f64| p + if d > 0.0 { d.floor() } else { d.ceil() };
                (
                    step(coord.0, distance * angle.to_radians().cos()),
                    step(coord.1, distance * angle.to_radians().sin()),
                )
            };
            let flipped = (angles[1] > angles[0] && angles[1] - 180.0 > angles[0])
                || (angles[1] < angles[0] && angles[1] + 180.0 > angles[0]);
            let coords = [
                point.0 - wf / 2.0 + 1.0,
                point.1 - wf / 2.0 + 1.0,
                point.0 + wf / 2.0 - 1.0,
                point.1 + wf / 2.0 - 1.0,
            ];
            let (start, end) = if flipped {
                (angles[1] + 90.0, angles[0] + 90.0)
            } else {
                (angles[0] - 90.0, angles[1] - 90.0)
            };
            pieslice(img, coords, start - 90.0, end - 90.0, Some(fill), None, 1);
            if width_i > 8 {
                // Cover potential gaps between the line and the joint.
                let gap = if flipped {
                    [
                        coord_at_angle(point, angles[0] + 90.0),
                        point,
                        coord_at_angle(point, angles[1] + 90.0),
                    ]
                } else {
                    [
                        coord_at_angle(point, angles[0] - 90.0),
                        point,
                        coord_at_angle(point, angles[1] - 90.0),
                    ]
                };
                line(img, &gap, fill, 3, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn f(v: &Value) -> f64 {
        v.as_f64().unwrap_or(0.0)
    }
    fn opt(v: &Value) -> Option<u8> {
        v.as_u64().map(|x| x as u8)
    }
    fn bx(v: &Value) -> [f64; 4] {
        [f(&v[0]), f(&v[1]), f(&v[2]), f(&v[3])]
    }
    fn pts(v: &Value) -> Vec<(f64, f64)> {
        v.as_array()
            .map(|a| a.iter().map(|p| (f(&p[0]), f(&p[1]))).collect())
            .unwrap_or_default()
    }
    fn width(v: &Value) -> u32 {
        v.as_u64().unwrap_or(1) as u32
    }
    fn fnv(img: &GrayImage) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in img.as_raw() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Every row is a random shape on a 70x60 canvas (partly off canvas, float coordinates, widths up to 40)
    /// and the FNV hash of what Pillow 13 drew. All of them match pixel for pixel.
    #[test]
    fn shapes_match_pillow_pixel_for_pixel() {
        for (op, args, hash) in CASES {
            let a: Value = serde_json::from_str(args).unwrap();
            let mut img = GrayImage::new(70, 60);
            match *op {
                "rect" => rectangle(&mut img, bx(&a[0]), opt(&a[1]), opt(&a[2]), width(&a[3])),
                "ellipse" => ellipse(&mut img, bx(&a[0]), opt(&a[1]), opt(&a[2]), width(&a[3])),
                "arc" => arc(&mut img, bx(&a[0]), f(&a[1]), f(&a[2]), 255, width(&a[4])),
                "pieslice" => pieslice(
                    &mut img,
                    bx(&a[0]),
                    f(&a[1]),
                    f(&a[2]),
                    opt(&a[3]),
                    opt(&a[4]),
                    width(&a[5]),
                ),
                "chord" => chord(
                    &mut img,
                    bx(&a[0]),
                    f(&a[1]),
                    f(&a[2]),
                    opt(&a[3]),
                    opt(&a[4]),
                    width(&a[5]),
                ),
                "polygon" => polygon(&mut img, &pts(&a[0]), opt(&a[1]), opt(&a[2]), width(&a[3])),
                "line" => line(
                    &mut img,
                    &pts(&a[0]),
                    255,
                    width(&a[2]),
                    a[3].as_bool().unwrap_or(false),
                ),
                "rrect" => rounded_rectangle(
                    &mut img,
                    bx(&a[0]),
                    f(&a[1]),
                    opt(&a[2]),
                    opt(&a[3]),
                    width(&a[4]),
                ),
                _ => point(&mut img, &pts(&a[0]), 255),
            }
            assert_eq!(fnv(&img), *hash, "{op} {args}");
        }
    }

    #[test]
    fn a_circle_of_five_is_the_familiar_one() {
        let mut img = GrayImage::new(5, 5);
        ellipse(&mut img, [0, 0, 4, 4], Some(255), None, 0);
        let rows: Vec<String> = (0..5)
            .map(|y| {
                (0..5)
                    .map(|x| {
                        if img.get_pixel(x, y).0[0] != 0 {
                            '#'
                        } else {
                            '.'
                        }
                    })
                    .collect()
            })
            .collect();
        assert_eq!(rows, [".###.", "#####", "#####", "#####", ".###."]);
    }

    #[test]
    fn rectangle_outline_is_inclusive_and_clipped() {
        let mut img = GrayImage::new(10, 10);
        rectangle(&mut img, [2, 2, 5, 4], None, Some(255), 1);
        // A 4 x 3 box: the top and bottom rows, and the two ends of the middle row.
        assert_eq!(img.as_raw().iter().filter(|&&v| v != 0).count(), 4 + 4 + 2);
        let mut img = GrayImage::new(10, 10);
        rectangle(&mut img, [-5, -5, 20, 20], Some(255), None, 0);
        assert!(img.as_raw().iter().all(|&v| v == 255));
    }

    #[test]
    fn reversed_boxes_draw_nothing() {
        // Pillow raises ValueError for these; the port just leaves the image alone.
        let mut img = GrayImage::new(10, 10);
        ellipse(&mut img, [5, 5, 2, 2], Some(255), None, 0);
        rectangle(&mut img, [5, 5, 2, 2], Some(255), None, 0);
        rounded_rectangle(&mut img, [5, 5, 2, 2], 2.0, Some(255), None, 0);
        arc(&mut img, [5, 5, 2, 2], 0.0, 90.0, 255, 1);
        assert!(img.as_raw().iter().all(|&v| v == 0));
    }

    #[test]
    fn random_wild_input_never_panics() {
        let pool = [
            -1.0e9,
            -300.0,
            -33.5,
            -1.0,
            0.0,
            0.5,
            3.0,
            9.0,
            17.0,
            29.0,
            31.5,
            64.0,
            300.0,
            1.0e9,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        let widths = [0u32, 1, 2, 3, 5, 9, 40, 1000, u32::MAX];
        let mut seed: u64 = 99;
        let mut next = |n: usize| -> usize {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        for _ in 0..1500 {
            let mut img = GrayImage::new(32, 24);
            let b = [
                pool[next(17)],
                pool[next(17)],
                pool[next(17)],
                pool[next(17)],
            ];
            let pts: Vec<(f64, f64)> = (0..next(6))
                .map(|_| (pool[next(17)], pool[next(17)]))
                .collect();
            let (a0, a1) = (pool[next(17)], pool[next(17)]);
            let w = widths[next(9)];
            let fill = [None, Some(255u8), Some(0)][next(3)];
            let outline = [None, Some(255u8), Some(100)][next(3)];
            rectangle(&mut img, b, fill, outline, w);
            ellipse(&mut img, b, fill, outline, w);
            arc(&mut img, b, a0, a1, 255, w);
            pieslice(&mut img, b, a0, a1, fill, outline, w);
            chord(&mut img, b, a0, a1, fill, outline, w);
            polygon(&mut img, &pts, fill, outline, w);
            line(&mut img, &pts, 255, w, next(2) == 0);
            rounded_rectangle(&mut img, b, pool[next(17)], fill, outline, w);
            point(&mut img, &pts, 255);
        }
    }

    #[test]
    fn huge_and_odd_coordinates_do_not_panic() {
        let mut img = GrayImage::new(20, 20);
        ellipse(
            &mut img,
            [-1_000_000_000i64, -5, 1_000_000_000, 5],
            Some(255),
            Some(1),
            3,
        );
        arc(&mut img, [0.0, 0.0, f64::NAN, 5.0], 0.0, 90.0, 255, 2);
        line(&mut img, &[(0.0, 0.0), (1e12, 1e12)], 255, 3, true);
        line(
            &mut img,
            &[(f64::INFINITY, 0.0), (5.0, f64::NAN)],
            255,
            1,
            false,
        );
        polygon(
            &mut img,
            &[(0, 0), (i32::MAX, i32::MIN), (7, 7)],
            Some(255),
            Some(1),
            4,
        );
        rectangle(&mut img, [0, 0, 10, 10], None, Some(255), u32::MAX);
        pieslice(
            &mut img,
            [0, 0, 10, 10],
            f64::NAN,
            f64::INFINITY,
            Some(255),
            Some(1),
            5,
        );
        rounded_rectangle(&mut img, [0, 0, 19, 19], -5.0, Some(255), Some(1), 2);
        let empty: [(i32, i32); 0] = [];
        polygon(&mut img, &empty, Some(255), Some(1), 1);
        line(&mut img, &empty, 255, 3, true);
    }

    #[rustfmt::skip]
    const CASES: &[(&str, &str, u64)] = &[
        ("rect", r#"[[-9,28,14,40],200,200,9]"#, 0x8b9d89a926f88dbd),
        ("rect", r#"[[5.25,45.25,21.5,80],200,200,1]"#, 0x023fa87e2d1b97ad),
        ("rect", r#"[[-1,14.75,20.5,34],200,null,0]"#, 0xd2f285c57de234ad),
        ("rect", r#"[[10,12,28,18],200,null,1]"#, 0x3b7f54bfbe7361dd),
        ("rect", r#"[[19,32,21,46],null,100,2]"#, 0x80763513a97d97d1),
        ("rect", r#"[[61,13,77,50],200,200,1]"#, 0x15a7670a13903595),
        ("rect", r#"[[11.25,28.5,11.5,37.25],200,100,0]"#, 0x815f3a3bba0c2cf5),
        ("rect", r#"[[46,0.5,70.25,11],200,200,0]"#, 0x8d2dae97901a9f45),
        ("rect", r#"[[52.75,31.25,101.5,70],null,null,40]"#, 0x880bb47fa003a145),
        ("rect", r#"[[32.25,19.5,45.25,29],null,null,0]"#, 0x880bb47fa003a145),
        ("rect", r#"[[13,2,29,34],200,null,40]"#, 0x658b30870f48982d),
        ("rect", r#"[[21,13,22,36],200,null,9]"#, 0xdff3c9ad67e69ac5),
        ("rect", r#"[[63.75,21.5,89,53],200,200,2]"#, 0xd718bee52237e10d),
        ("rect", r#"[[63,15,110,23],null,100,3]"#, 0xfb9e1e1034767331),
        ("rect", r#"[[55.75,-3,71.5,46],null,null,5]"#, 0x880bb47fa003a145),
        ("rect", r#"[[-6,25,41,53],200,100,3]"#, 0x0fe2e90fe2a377f9),
        ("rect", r#"[[23,29,70,60],200,null,3]"#, 0x4197b722ac3b8cbd),
        ("rect", r#"[[18,34,26,34],null,null,1]"#, 0x880bb47fa003a145),
        ("rect", r#"[[14,40,45,51],200,100,2]"#, 0x2c319fc58a80acc5),
        ("rect", r#"[[30,39,34,78],200,100,0]"#, 0x9d1f82d3221c052d),
        ("rect", r#"[[10,0,19,19],null,100,9]"#, 0x4567eccc816bd465),
        ("rect", r#"[[9,4,41,6],200,100,5]"#, 0xda2a34ce6c4567d9),
        ("rect", r#"[[20,2,51,11],null,100,5]"#, 0x131b6603c6dd6a45),
        ("rect", r#"[[26.25,-5.25,31,0.25],200,200,1]"#, 0xc8e77aaa5da08c65),
        ("rect", r#"[[46,-7,69,35],null,100,5]"#, 0x92550f31ad532abd),
        ("rect", r#"[[59,-8,79,22],200,null,5]"#, 0xbf84dffcad5c60cd),
        ("rect", r#"[[28,6,52,42],null,200,1]"#, 0xe1eb4b51cc89e145),
        ("rect", r#"[[5,2,44,13],null,null,3]"#, 0x880bb47fa003a145),
        ("rect", r#"[[57,2,86,33],200,null,9]"#, 0x23149966a0797245),
        ("rect", r#"[[18,32,65,82],null,100,2]"#, 0x876eff80780874e5),
        ("rect", r#"[[50,13,63,48],null,null,40]"#, 0x880bb47fa003a145),
        ("rect", r#"[[-9,14,-5,64],null,200,1]"#, 0x880bb47fa003a145),
        ("rect", r#"[[62.25,20.75,75.5,37.75],200,200,2]"#, 0xe018b2eea388dec5),
        ("rect", r#"[[54,3,87,4],200,null,9]"#, 0x263a73db9c0cfb45),
        ("rect", r#"[[0,43,29,50],200,null,40]"#, 0x02a90d13a72e7045),
        ("rect", r#"[[62,5,72,18],200,100,1]"#, 0xe06eaac81d76b265),
        ("ellipse", r#"[[57,4,72,48],200,200,40]"#, 0xa0a6c9c88682baad),
        ("ellipse", r#"[[36,2,57,16],200,200,1]"#, 0xf3012aee680e68a5),
        ("ellipse", r#"[[1,50,46,58],200,null,9]"#, 0x2fd03839b8783a15),
        ("ellipse", r#"[[-8,35,25,82],200,100,0]"#, 0x84d0e9358ef5db0d),
        ("ellipse", r#"[[10,-2,44,18],200,100,40]"#, 0xaaf1a03b42357b59),
        ("ellipse", r#"[[10,4,45,50],null,100,3]"#, 0x8ef9006e7af2ba6d),
        ("ellipse", r#"[[41,25,52,58],200,100,0]"#, 0x800e5734050aab45),
        ("ellipse", r#"[[35,19,53,43],200,null,0]"#, 0x8f07ff4aeffc390d),
        ("ellipse", r#"[[33.75,1.75,74.5,26.75],200,100,0]"#, 0xbe5c3149a5d73275),
        ("ellipse", r#"[[65,29,93,52],null,100,2]"#, 0x0e36580542c7fcd5),
        ("ellipse", r#"[[58,22,65,42],200,100,40]"#, 0x2866ea062536cb75),
        ("ellipse", r#"[[-8,49,39,53],null,200,2]"#, 0xd8efb45b29aaf905),
        ("ellipse", r#"[[28,26,76,68],200,200,9]"#, 0x98f20381a2a02b75),
        ("ellipse", r#"[[8.25,9.75,30.25,13.25],200,null,40]"#, 0x293bf227ffb9058d),
        ("ellipse", r#"[[65,35,77,49],null,200,9]"#, 0xa270184116132a6d),
        ("ellipse", r#"[[42.5,26.25,63.25,39.75],null,null,1]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[27,-5,66,22],null,100,1]"#, 0x9ab7881da406072d),
        ("ellipse", r#"[[-4,45,37,76],null,100,5]"#, 0xb4c99db5ce8f3e79),
        ("ellipse", r#"[[0,9.5,29.5,46.5],200,200,1]"#, 0x34c22fbbadc890c5),
        ("ellipse", r#"[[32,30,60,57],null,null,5]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[43,11,78,49],null,100,0]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[11,-6,23,31],null,null,0]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[60,-7,86,22],null,200,40]"#, 0x90e55b438b58c7c5),
        ("ellipse", r#"[[55,40,68,86],null,null,2]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[0,2,24,43],null,200,0]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[-2,2,23,3],null,null,9]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[47,52,85,95],null,null,0]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[-10,24,6,65],null,100,1]"#, 0xce715d752d3857a9),
        ("ellipse", r#"[[37,14,86,39],null,null,40]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[15,-2,27,28],200,100,1]"#, 0x5136ecea3a96e489),
        ("ellipse", r#"[[21,25,23,49],null,100,9]"#, 0x10594eab46e78c31),
        ("ellipse", r#"[[16,8,49,51],null,null,1]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[11,46,42,85],200,null,5]"#, 0xe7d433c3f703e0c5),
        ("ellipse", r#"[[0,15,5,48],200,200,5]"#, 0x61200d0feba1b8c5),
        ("ellipse", r#"[[17.25,15.5,37.25,60.25],200,100,9]"#, 0x6f997f6e56dbbb29),
        ("ellipse", r#"[[61,47,72,51],null,100,2]"#, 0x1df84c9cfe794ab1),
        ("ellipse", r#"[[57,6,105,42],200,null,1]"#, 0xfe34a48ea9c7b80d),
        ("ellipse", r#"[[45,50,51,55],null,100,5]"#, 0x56e6813e158bff25),
        ("ellipse", r#"[[26,17,71,62],200,null,40]"#, 0x6d7f9b22c9396875),
        ("ellipse", r#"[[63,39,111,46],200,200,40]"#, 0x4225e2d088877805),
        ("ellipse", r#"[[2,16,50,31],null,100,1]"#, 0xbb045bfe3ec5e9e5),
        ("ellipse", r#"[[-4,13,13,36],200,null,5]"#, 0x286d1ff15897a155),
        ("ellipse", r#"[[61,1,63,5],null,100,9]"#, 0xa858effdd0c20bd1),
        ("ellipse", r#"[[11,41,21,62],200,100,3]"#, 0xce77b879282a9b21),
        ("ellipse", r#"[[51,38,78,76],200,200,3]"#, 0xe191fd221b21145d),
        ("ellipse", r#"[[52.5,18,55.75,34.25],null,null,1]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[14.25,9.75,38.75,53.5],200,100,3]"#, 0x3f604377b3fc7c2d),
        ("ellipse", r#"[[-8.75,26.75,12,42.75],null,100,9]"#, 0x6647390631f55101),
        ("ellipse", r#"[[21,9,40,21],200,null,2]"#, 0xe7ac3ad1ca7ee7c5),
        ("ellipse", r#"[[60,13,85,59],null,200,40]"#, 0x82d2d082eb374a25),
        ("ellipse", r#"[[57,-9,70,5],null,null,3]"#, 0x880bb47fa003a145),
        ("ellipse", r#"[[17.75,53,32,58.25],200,null,1]"#, 0x648cd0b903235745),
        ("ellipse", r#"[[44.25,15.75,83.5,16.5],200,200,1]"#, 0xada2870229b40645),
        ("ellipse", r#"[[64,9,69,11],null,100,2]"#, 0xd7f1374b6557c31d),
        ("ellipse", r#"[[22,11,46,45],200,null,5]"#, 0xfc66664f35d4b8bd),
        ("ellipse", r#"[[34,55,70,79],200,null,40]"#, 0xfa43d20cdf6f678d),
        ("ellipse", r#"[[12,22,16,35],null,200,2]"#, 0x08d47b47fe80a615),
        ("ellipse", r#"[[2.5,37.75,10.75,52.75],200,100,9]"#, 0x4456f52f1118ed85),
        ("ellipse", r#"[[49,4,51,41],null,100,1]"#, 0x73c4bc405b1e0445),
        ("ellipse", r#"[[21,50,44,87],200,100,5]"#, 0x409f17498b609865),
        ("arc", r#"[[0,16,4,41],400,0,255,3]"#, 0xe09f377b78e356dd),
        ("arc", r#"[[-9,50,1,70],90,0,255,1]"#, 0xbba95cc2f712593a),
        ("arc", r#"[[21,22,47,54],30,90,255,15]"#, 0x9a2cfe72ed8e0f72),
        ("arc", r#"[[24,41,27,48],0,360,255,2]"#, 0xd245bf2069156f1d),
        ("arc", r#"[[5,19,49,36],270,360,255,2]"#, 0xc723ef21e5fd771e),
        ("arc", r#"[[3,44,11,56],359.5,270,255,6]"#, 0x4ef65940e850214d),
        ("arc", r#"[[47,20,94,26],359.5,90,255,15]"#, 0x880bb47fa003a145),
        ("arc", r#"[[51,31,93,57],359.5,45,255,15]"#, 0x880bb47fa003a145),
        ("arc", r#"[[11,24,19,69],-45,0,255,1]"#, 0xed0a23cc8583e70b),
        ("arc", r#"[[9,31,14,48],270,360,255,15]"#, 0xb9e3a02ea8de7ca5),
        ("arc", r#"[[18,35,44,42],0,270,255,1]"#, 0xb4c4265d09adf682),
        ("arc", r#"[[43,-5,57,18],270,0,255,1]"#, 0x071c386d8143a802),
        ("arc", r#"[[43,3,59,19],359.5,0,255,1]"#, 0x899ce960b64b5630),
        ("arc", r#"[[53,42,96,47],180,180,255,15]"#, 0x880bb47fa003a145),
        ("arc", r#"[[63,34,83,54],30,90,255,15]"#, 0x880bb47fa003a145),
        ("arc", r#"[[-1,53,43,87],270,180,255,3]"#, 0xbb16c0b89d60100d),
        ("arc", r#"[[46,5,65,39],-45,90,255,1]"#, 0x020f72c26cdea6c9),
        ("arc", r#"[[14,42,30,80],90,45,255,3]"#, 0xa30c6bd8a25c4e0e),
        ("arc", r#"[[15,46,31,46],90,0,255,6]"#, 0x975c877105f70b48),
        ("arc", r#"[[13,28,39,68],400,-233.9458174387442,255,2]"#, 0x880bb47fa003a145),
        ("arc", r#"[[-9,-5,19,-5],400,90,255,2]"#, 0x880bb47fa003a145),
        ("arc", r#"[[-2,-3,12,16],90,90,255,6]"#, 0x880bb47fa003a145),
        ("arc", r#"[[-3,24,33,30],90,90,255,1]"#, 0x880bb47fa003a145),
        ("arc", r#"[[39,26,46,28],-44.708190514932255,-391.91015008843425,255,1]"#, 0xac7c3451b439bfe8),
        ("arc", r#"[[12,40,58,76],359.5,90,255,1]"#, 0x0ecc999a93a252fd),
        ("arc", r#"[[38,4,68,54],30,45,255,1]"#, 0xa900f9fe7cdcb9dc),
        ("arc", r#"[[17,32,31,62],400,270,255,15]"#, 0xf29e4c95f4d72f98),
        ("arc", r#"[[9,40,55,59],180,90,255,15]"#, 0x9a2bff28d61618b4),
        ("arc", r#"[[-2,28,18,36],0,-32.62021835116616,255,1]"#, 0x34a0c6326b443eac),
        ("arc", r#"[[58,50,85,84],30,360,255,2]"#, 0x35f36e29cf796161),
        ("arc", r#"[[12,35,57,75],0,90,255,1]"#, 0x019d76b4cd7358a8),
        ("arc", r#"[[32,45,70,72],163.90030824300106,-324.9185237466829,255,1]"#, 0x591f4730624cc2dc),
        ("arc", r#"[[22,20,69,20],400,270,255,15]"#, 0x98beed1785d97753),
        ("arc", r#"[[28,-2,45,30],108.6308642443613,90,255,6]"#, 0x24bd8fb78ac1fec0),
        ("arc", r#"[[38,-9,64,25],359.5,180,255,1]"#, 0xa9c26b0a7e23d420),
        ("arc", r#"[[51,40,89,66],-45,173.3620843877402,255,1]"#, 0x86013e142301f196),
        ("arc", r#"[[18,23,66,29],30,200,255,2]"#, 0x0446a2c6d08410b2),
        ("arc", r#"[[30,23,69,69],270,90,255,6]"#, 0x32ce1c1d139cfa18),
        ("arc", r#"[[20,51,43,66],-45,0,255,1]"#, 0xf407e5debdfb82c3),
        ("arc", r#"[[-2,5,35,7],400,200,255,1]"#, 0xbd7b9c13e5af94c4),
        ("arc", r#"[[48,0,63,33],-45,200,255,2]"#, 0xe08790a87af85877),
        ("arc", r#"[[5,35,25,57],-45,360,255,15]"#, 0x8d6d552826afce04),
        ("arc", r#"[[50,13,51,58],270,0,255,1]"#, 0xcbb8d513f69f8058),
        ("arc", r#"[[43,51,89,62],400,45,255,6]"#, 0x880bb47fa003a145),
        ("arc", r#"[[55,20,93,21],400,360,255,1]"#, 0xe6dd9f7aaf0f337d),
        ("pieslice", r#"[[-10,54,15,80],30,200,null,100,1]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[42,20,58,69],-45,45,200,null,9]"#, 0x985f7cd89341ab15),
        ("pieslice", r#"[[5,-5,10,8],270,360,null,100,9]"#, 0x9c02323e618b00f5),
        ("pieslice", r#"[[10,44,56,62],90,45,200,100,2]"#, 0x5512950e5f9e35f1),
        ("pieslice", r#"[[-8,23,7,31],30,270,null,null,9]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[63,36,109,62],30,381.9382355841061,null,100,9]"#, 0x04c21d7947895061),
        ("pieslice", r#"[[39,50,72,50],-45,45,null,null,4]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[55,5,87,29],119.75547222731154,245.28251426920326,200,null,1]"#, 0x26a3d2bff236f8d5),
        ("pieslice", r#"[[3,26,25,51],90,360,null,100,1]"#, 0xa476158885f391fd),
        ("pieslice", r#"[[5,27,11,45],180,0,200,null,9]"#, 0x97fa9cc79f0f4175),
        ("pieslice", r#"[[25,6,34,16],90,360,null,null,2]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[23,37,45,59],90,200,200,null,9]"#, 0xe4a542a6fc9af595),
        ("pieslice", r#"[[34,34,73,68],270,360,200,null,1]"#, 0x1f488187337c74ed),
        ("pieslice", r#"[[-9,35,1,62],-45,200,200,null,2]"#, 0x14bb7ee00b5f6085),
        ("pieslice", r#"[[3,-3,37,21],90,45,200,null,2]"#, 0xbe8a9de3628996c5),
        ("pieslice", r#"[[38,26,42,61],-45,311.1737348424024,200,null,9]"#, 0x59066c3bb492b415),
        ("pieslice", r#"[[-8,45,6,76],270,200,null,null,2]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[47,54,51,68],-45,180,null,100,1]"#, 0x0742c5163caf4461),
        ("pieslice", r#"[[3,39,41,84],0,283.89153824710365,null,100,1]"#, 0x6d31499341aa6271),
        ("pieslice", r#"[[0,10,20,13],180,360,200,100,1]"#, 0x70c9c0e4749bee71),
        ("pieslice", r#"[[48,-3,87,22],180,180,null,null,1]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[22,18,72,60],180,0,200,null,1]"#, 0x91f905cf1fd02bfd),
        ("pieslice", r#"[[-2,19,47,33],0,143.58406555989166,null,null,9]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[-1,53,12,95],30,90,200,null,9]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[-2,15,6,36],0,200,200,100,4]"#, 0x81d13ecd8de234f9),
        ("pieslice", r#"[[47,33,82,60],90,180,200,100,9]"#, 0xf01608fcb5fea41d),
        ("pieslice", r#"[[2,9,22,51],30,55.660598595261945,200,100,9]"#, 0x492e0bb51a788739),
        ("pieslice", r#"[[22,27,51,31],90,-132.69618432359573,200,null,2]"#, 0xff9eb450a910fe95),
        ("pieslice", r#"[[0,44,38,79],90,200,null,null,4]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[24,34,41,53],-45,200,null,null,4]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[26,32,67,41],90,90,null,100,4]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[52,26,77,43],270,200,200,null,9]"#, 0x29c0d362b7ef3e6d),
        ("pieslice", r#"[[60,53,66,68],90,270,null,100,4]"#, 0x365ac2ae99b5ad7d),
        ("pieslice", r#"[[-6,9,5,18],133.22226736129141,90,null,null,1]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[1,-6,4,13],0.2673734692580183,90,null,100,4]"#, 0x0f2e1dbafb7a36d9),
        ("pieslice", r#"[[62,16,89,39],270,360,null,100,9]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[5,8,33,47],0,270,200,100,2]"#, 0x1e9ee643c800a541),
        ("pieslice", r#"[[-6,19,30,63],90,200,null,null,1]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[54,29,90,39],30,-398.6778008923676,null,null,2]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[0,5,12,38],30,-144.55107173721115,null,100,4]"#, 0x15e174041cbdda29),
        ("pieslice", r#"[[48,49,68,89],180,270,200,100,9]"#, 0xc4380dc4df634199),
        ("pieslice", r#"[[6,35,25,82],270,360,200,100,9]"#, 0x0acab411032efce9),
        ("pieslice", r#"[[12,27,51,51],270,200,200,100,9]"#, 0x045cf27a30dede15),
        ("pieslice", r#"[[7,4,10,25],0,360,null,null,1]"#, 0x880bb47fa003a145),
        ("pieslice", r#"[[8,-4,48,10],180,45,null,null,9]"#, 0x880bb47fa003a145),
        ("chord", r#"[[39,9,48,28],270,90,200,null,9]"#, 0x1abb0a50dbd53ea5),
        ("chord", r#"[[-3,-3,27,24],180,180,null,null,9]"#, 0x880bb47fa003a145),
        ("chord", r#"[[15,2,50,27],180,90,200,null,1]"#, 0xcd259557dc791425),
        ("chord", r#"[[52,55,69,72],30,90,null,null,1]"#, 0x880bb47fa003a145),
        ("chord", r#"[[48,3,92,46],0,0,200,null,4]"#, 0x880bb47fa003a145),
        ("chord", r#"[[52,-2,66,32],0,90,200,null,1]"#, 0xa52affb95f35213d),
        ("chord", r#"[[51,44,71,67],22.941598219022467,360,null,100,1]"#, 0x94f4300342e3b615),
        ("chord", r#"[[17,33,24,39],-45,0,200,100,2]"#, 0x77607186ff346605),
        ("chord", r#"[[9,31,20,62],180,200,null,100,1]"#, 0xbd2cf353cf659d65),
        ("chord", r#"[[40,16,65,58],-45,45,200,100,4]"#, 0x1f5612bc761e4e59),
        ("chord", r#"[[2,10,25,15],-350.7698209481137,-236.70475200660607,200,100,1]"#, 0x24b3272b66845cbd),
        ("chord", r#"[[52,26,56,32],30,270,200,100,9]"#, 0xb728a013df4678a9),
        ("chord", r#"[[14,22,16,33],180,200,200,100,4]"#, 0x273304d79a73adc5),
        ("chord", r#"[[18,9,54,23],0,171.62167984539235,null,null,1]"#, 0x880bb47fa003a145),
        ("chord", r#"[[19,37,22,60],30,328.05994276800834,null,100,2]"#, 0x2a044660fea5b4dd),
        ("chord", r#"[[38,34,60,38],0,0,200,null,1]"#, 0x880bb47fa003a145),
        ("chord", r#"[[20,-5,63,37],180,90,200,null,4]"#, 0xdecc921340f07e7d),
        ("chord", r#"[[65,-9,105,-7],30,-356.4282039093361,200,null,2]"#, 0x880bb47fa003a145),
        ("chord", r#"[[29,4,29,39],-9.804509616331018,360,null,null,2]"#, 0x880bb47fa003a145),
        ("chord", r#"[[44,14,80,37],30,0,200,null,4]"#, 0xec6f6df5bb227155),
        ("chord", r#"[[65,53,84,95],30,180,null,100,4]"#, 0x880bb47fa003a145),
        ("chord", r#"[[11,49,34,65],-45,0,null,100,9]"#, 0x5413f2e88085bc2d),
        ("chord", r#"[[61,48,79,84],-331.18595866681767,90,200,null,4]"#, 0x25359a626b5ad43d),
        ("chord", r#"[[7,2,23,47],-45,270,null,null,1]"#, 0x880bb47fa003a145),
        ("chord", r#"[[52,48,65,81],30,200,200,100,1]"#, 0x48751fa08cbf8441),
        ("chord", r#"[[59,37,60,48],-45,45,200,null,1]"#, 0x457dfc90b4120f65),
        ("chord", r#"[[43,-10,56,17],90,200,null,null,2]"#, 0x880bb47fa003a145),
        ("chord", r#"[[2,9,15,59],-85.80885721475317,90,200,null,2]"#, 0x3ff92f5ca6ad90ed),
        ("chord", r#"[[8,29,8,74],90,0,200,100,9]"#, 0x945b91c0cc1ec761),
        ("chord", r#"[[-1,32,17,42],270,200,null,100,9]"#, 0x0552be27108a5c7d),
        ("chord", r#"[[33,2,38,50],30,0,null,null,9]"#, 0x880bb47fa003a145),
        ("chord", r#"[[10,-4,10,-4],270,45,200,null,9]"#, 0x880bb47fa003a145),
        ("chord", r#"[[36,10,84,50],-45,180,null,100,2]"#, 0x8b1aa9f8f90e56c9),
        ("chord", r#"[[62,55,96,98],90,90,200,100,4]"#, 0x880bb47fa003a145),
        ("chord", r#"[[48,-1,54,1],270,90,200,null,2]"#, 0xd5fe54a0feaee18d),
        ("chord", r#"[[23,36,52,73],30,0,200,null,4]"#, 0x79e25704b815f445),
        ("chord", r#"[[3,-4,25,17],180,75.80221729227708,200,100,9]"#, 0x69a272149e2560b5),
        ("chord", r#"[[47,50,81,95],-216.1935562283124,0,200,null,4]"#, 0xbc0220ff5f4d900d),
        ("chord", r#"[[-10,-5,16,16],30,90,200,null,1]"#, 0x6d331895c173424d),
        ("chord", r#"[[44,25,88,69],0,270,200,null,9]"#, 0x8e5aef2bd1a6497d),
        ("chord", r#"[[11,18,58,47],0,90,null,null,9]"#, 0x880bb47fa003a145),
        ("chord", r#"[[17,33,21,50],270,200,200,100,1]"#, 0xdfce2657c6bf8bfd),
        ("chord", r#"[[58,51,95,65],270,90,200,100,4]"#, 0x880bb47fa003a145),
        ("chord", r#"[[52,-6,84,22],30,180,200,100,2]"#, 0x57257055b690b571),
        ("chord", r#"[[65,17,114,20],-45,0,200,null,2]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[70.5,45],[24.5,24],[39,63],[5,37],[0,49],[11.9,66]],200,100,1]"#, 0xe74b78fd10d73d6d),
        ("polygon", r#"[[[24,52],[75,15],[35.5,65],[6,-2],[5.5,47],[61.9,39],[31.9,24],[63,65]],200,null,3]"#, 0xd4ec6c6bcec951e5),
        ("polygon", r#"[[[40.9,51],[36.9,35],[58,29],[10,21],[23,41],[41.9,59],[42,28]],200,100,2]"#, 0xc42e9fb24ebc40f1),
        ("polygon", r#"[[[19.9,-2],[46,36],[3,47],[9.5,58],[75.9,15]],200,null,2]"#, 0x422bafbfe751ccb5),
        ("polygon", r#"[[[40.9,17],[32,68],[46.9,5],[49,65],[48,9],[67.5,1],[18,-6],[-7.1,-3]],null,100,1]"#, 0xf2ee396569daf261),
        ("polygon", r#"[[[41,4],[59.5,32],[64,21],[60,28],[52,60],[56,19],[68.5,40]],200,100,3]"#, 0x0831d64a6ad74245),
        ("polygon", r#"[[[15,32],[3,38],[-4,0]],200,100,3]"#, 0xb8622e76dd1f2629),
        ("polygon", r#"[[[-3,54],[72,11],[33,5],[16.5,40],[13.9,23]],200,null,1]"#, 0x3c7ae49cdb32e5d5),
        ("polygon", r#"[[[18,18],[13.9,28],[3,58],[75,3],[42,38],[67,11]],200,null,2]"#, 0x859afb95b0a466f5),
        ("polygon", r#"[[[-3,4],[75,58],[53.9,-6]],200,100,6]"#, 0x299f5ee02887a14d),
        ("polygon", r#"[[[12.5,21],[32,2],[45,51],[18,15],[43,-6]],200,100,3]"#, 0xf0d5305bc04df9c5),
        ("polygon", r#"[[[35.9,37],[64,36],[-5,-3],[-7.1,45],[36,18],[21.9,43],[25.9,0]],null,null,2]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[17,61],[-1,-5],[8,-7]],200,100,6]"#, 0xc3bbe5bb30bd5df1),
        ("polygon", r#"[[[-1,68],[34.9,1],[55,40],[9,2]],null,100,1]"#, 0x11681b0fe500d089),
        ("polygon", r#"[[[32,57],[-4.5,-4],[55,43],[43.9,23],[18,31],[42,44],[34.9,7],[36,63]],null,null,1]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[49.5,50],[-5.1,8],[67,52]],null,100,1]"#, 0xf007d8de1ee20f29),
        ("polygon", r#"[[[64.5,-1],[67.5,62],[17.9,2],[10,62],[5,22],[67,20],[56.9,38],[34.5,5]],null,null,1]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[44,13],[11,47],[77.5,60]],200,null,6]"#, 0x93786a7446210f55),
        ("polygon", r#"[[[43,16],[19.9,56],[18,-4]],null,100,6]"#, 0x094a51963ea8c5f1),
        ("polygon", r#"[[[71,29],[-5,46],[4,14],[57.9,15],[51,30]],null,100,6]"#, 0x555ab3f935e2de79),
        ("polygon", r#"[[[65,30],[34,1],[-3.5,60]],200,null,1]"#, 0x2bcd6bfbe1a3112d),
        ("polygon", r#"[[[59.5,41],[17.5,51],[59.5,8],[70,5],[4.5,7]],200,100,2]"#, 0x4a1dc3d113fbcac9),
        ("polygon", r#"[[[14,59],[26,55]],null,null,2]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[27,63],[76.9,44],[72,65],[28.9,29],[6.5,-2],[18,37],[0,30]],null,100,1]"#, 0x4a700e6d6cd09109),
        ("polygon", r#"[[[75,26],[56.9,45],[11.9,42],[44,14]],200,100,1]"#, 0xe430b8c526c90ce1),
        ("polygon", r#"[[[74,59],[74,56],[28.9,61],[20.5,-4],[78.9,4]],null,null,6]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[17,5],[53,44],[59,12],[61.9,-3],[73.9,9],[15,32]],200,null,3]"#, 0x704f732680958665),
        ("polygon", r#"[[[62.5,35],[36.5,25],[61.9,15]],null,100,1]"#, 0x363b86d24eac1925),
        ("polygon", r#"[[[-2,66],[21.9,50],[25.5,30],[34.9,38],[46.9,29],[22,-5],[15,60]],null,100,3]"#, 0x2723b24054fb0d79),
        ("polygon", r#"[[[27,67],[-4,63],[49,25],[-0.5,59],[66.5,37],[47,-6],[41.5,30],[34,11]],null,100,2]"#, 0x2c40f4df8db191fd),
        ("polygon", r#"[[[15.9,9],[48,58]],null,null,1]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[22.5,14],[28.5,44],[49.9,19],[62,61],[66.5,11],[38,19],[61,36]],200,null,6]"#, 0x0c4e502865e02365),
        ("polygon", r#"[[[41,65],[22.5,-2],[3,21],[45,11]],null,null,1]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[53.9,29],[71.5,6],[66,39],[22,67],[35.5,32],[36.5,10],[52.5,57]],200,null,2]"#, 0xcad04417d3fa1395),
        ("polygon", r#"[[[-4.5,37],[20.9,16],[51.5,54],[15.9,47],[19,17]],null,100,3]"#, 0xe7eb02c28bbf9445),
        ("polygon", r#"[[[-3,22],[1.9,60],[16,62],[76,36],[-2.5,51],[3,13],[18,47]],null,100,1]"#, 0x191e663dab7ae869),
        ("polygon", r#"[[[77,31],[63,-6],[61.9,55],[54,26],[43.9,27],[2,56],[-2.1,65]],null,100,6]"#, 0x70d6364a570e61a1),
        ("polygon", r#"[[[43.9,21],[53.9,35]],null,100,2]"#, 0x1b25616f8aeafba1),
        ("polygon", r#"[[[-8,60],[66,49],[31,7]],null,100,1]"#, 0x70f177d54653fc49),
        ("polygon", r#"[[[37,58],[36.9,10],[67,-4],[74,1],[0,-1],[5.9,61],[55.9,23],[53.5,0]],200,null,6]"#, 0x51f6ec2e9bba8ac5),
        ("polygon", r#"[[[15.9,21],[39.5,14],[77.9,-7],[24,8],[22.5,36],[38,14],[5,64]],null,null,2]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[30,5],[42.5,18],[75.9,13]],null,100,1]"#, 0x7dbff7cfc62150d5),
        ("polygon", r#"[[[8,47],[65.5,17],[65.9,17],[6,54],[3.9,67]],200,100,1]"#, 0xc22e3038df6641d9),
        ("polygon", r#"[[[36.5,52],[26,48],[57,18],[52.9,39]],200,null,3]"#, 0x12e9a95b8f3cab8d),
        ("polygon", r#"[[[34.5,25],[34,27],[64,57],[28,6]],200,null,3]"#, 0xf531f7f0f464c95d),
        ("polygon", r#"[[[3.9,19],[15,-4],[38.9,46],[24,-2]],200,null,3]"#, 0x021e037eadccd58d),
        ("polygon", r#"[[[-8,2],[39.9,-4],[-3.5,13],[5.9,51],[0.5,68],[24.9,18],[7.9,45]],null,null,3]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[53,15],[59.5,49],[43,1],[64,17]],200,100,1]"#, 0x2d2b89aca9fe9cf1),
        ("polygon", r#"[[[12.5,31],[74,6],[58.5,-3],[45.9,1],[43,45],[-2,-8],[22,61]],200,100,2]"#, 0x0054e1efb4ae75c9),
        ("polygon", r#"[[[34,38],[37,5],[51.5,58]],200,100,1]"#, 0xb1574dbb78fb07a1),
        ("polygon", r#"[[[65.9,52],[36,8],[32.5,62],[53.9,55],[26,19],[-0.09999999999999998,22]],200,null,3]"#, 0xdb1049e842326305),
        ("polygon", r#"[[[-7.1,9],[56,41],[18.5,37],[64,-8],[51.9,52],[78.9,17],[32,37]],null,100,6]"#, 0x981e664e52ce3521),
        ("polygon", r#"[[[8.5,14],[44.5,-2],[72.9,40],[60,57],[33,54],[-6.1,53]],null,null,1]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[30,13],[12,43],[43,31],[75,48],[14.5,25],[46,58]],200,null,3]"#, 0x7958244028b53c0d),
        ("polygon", r#"[[[74.9,47],[60.9,67],[44,-3]],200,100,6]"#, 0x5848abdf348db839),
        ("polygon", r#"[[[67,1],[55.9,2],[19.9,30],[70.5,17]],200,100,1]"#, 0x1be9fe9655e6042d),
        ("polygon", r#"[[[1.5,-5],[49.5,-4],[51,39],[56.5,65],[53,-7],[75.9,13],[8.9,11],[35,65]],null,null,2]"#, 0x880bb47fa003a145),
        ("polygon", r#"[[[28.9,67],[73.9,8],[60,24],[33.5,55],[35,8],[34.9,18],[3.5,45],[22.9,-6]],null,100,6]"#, 0x344f793d7b7015fd),
        ("polygon", r#"[[[48.9,38],[62,25],[58,46],[48,16],[66.5,8],[-7.5,27]],null,100,6]"#, 0x85343dd960c77e61),
        ("polygon", r#"[[[23.9,40],[48.5,30]],null,100,2]"#, 0x48959a66540b3459),
        ("line", r#"[[[18,-4],[62,13],[44,10],[50,19],[38,23]],255,1,true]"#, 0x0b9476184ba44ec5),
        ("line", r#"[[[20,31]],255,12,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[0,49],[47,5]],255,5,true]"#, 0x6ae134013e5bbe8a),
        ("line", r#"[[[37,1],[29,-6]],255,1,true]"#, 0x44e0ba88e311e4d3),
        ("line", r#"[[[5,59],[49,19],[-5,43],[13,48]],255,4,true]"#, 0xa6a466b666ffece9),
        ("line", r#"[[[38,62],[45,0],[67,28]],255,9,false]"#, 0xfccabdd5d4b85b9a),
        ("line", r#"[[[45,31],[12,68],[20,54],[38,33],[44,27],[58,22]],255,6,true]"#, 0x2d232dd3cb323a7f),
        ("line", r#"[[[0,62],[1,13]],255,12,false]"#, 0x9e01a366c4ef21d4),
        ("line", r#"[[[35,8]],255,1,false]"#, 0x880bb47fa003a145),
        ("line", r#"[[[9,27],[61,38],[13,17]],255,1,false]"#, 0xfb4cd27e75cef660),
        ("line", r#"[[[1,61],[14,68],[76,28],[26,27]],255,1,false]"#, 0xde12fb2c35dcaf47),
        ("line", r#"[[[52,65],[7,31],[25,16],[-1,39],[32,54]],255,2,false]"#, 0x86ecf80f80600238),
        ("line", r#"[[[48,43],[11,31]],255,6,false]"#, 0xc83a463214dfea9b),
        ("line", r#"[[[35,1],[32,65],[6,24]],255,1,false]"#, 0xb52b2fdaf83b7c74),
        ("line", r#"[[[76,2],[-6,-2],[13,-2],[32,65],[9,36],[66,58]],255,6,true]"#, 0x4aaa234103851f8a),
        ("line", r#"[[[57,47],[25,12],[48,64]],255,9,true]"#, 0x78492559ec28cd81),
        ("line", r#"[[[40,20],[53,68],[77,48]],255,2,true]"#, 0x0ee8e89bf721b714),
        ("line", r#"[[[62,-5],[32,21],[69,13]],255,6,false]"#, 0x766173d8d843e578),
        ("line", r#"[[[29,15],[14,2],[2,2]],255,3,true]"#, 0x162fde92aa9a85d7),
        ("line", r#"[[[23,8],[36,22],[4,46],[66,66],[54,37],[19,32]],255,2,false]"#, 0x94276defdb1a671f),
        ("line", r#"[[[11,32],[-7,4],[25,26]],255,9,true]"#, 0x45da61ef7f799b36),
        ("line", r#"[[[63,2],[-7,29]],255,5,true]"#, 0x355135e3e55a5d60),
        ("line", r#"[[[65,39],[19,25],[44,44]],255,5,false]"#, 0xea72291922b868c4),
        ("line", r#"[[[20,43],[55,10],[57,63],[3,53]],255,1,true]"#, 0x623bd4630ce9e9f8),
        ("line", r#"[[[73,21]],255,12,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[33,67],[30,6],[-1,16],[71,50]],255,6,false]"#, 0x2b05a6c2a117e1ab),
        ("line", r#"[[[7,46]],255,1,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[15,43],[8,65],[30,62]],255,1,true]"#, 0xc3426b9a70e53c34),
        ("line", r#"[[[14,-6],[32,38],[6,64],[37,37],[63,5],[6,3]],255,5,true]"#, 0xe40dfbfef474e8d0),
        ("line", r#"[[[-4,62]],255,1,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[17,24],[9,42],[15,46]],255,9,false]"#, 0xff0fb7cde24c22a4),
        ("line", r#"[[[-3,9]],255,1,false]"#, 0x880bb47fa003a145),
        ("line", r#"[[[59,61],[21,28]],255,12,false]"#, 0xa75c9f159c68a35a),
        ("line", r#"[[[50,30],[37,48]],255,9,false]"#, 0xf8026d87be0f1a65),
        ("line", r#"[[[6,32],[-7,54],[61,63],[9,11]],255,4,true]"#, 0xb0e0a39def499aeb),
        ("line", r#"[[[62,45],[72,58]],255,9,false]"#, 0xd6613984613ec834),
        ("line", r#"[[[27,-5]],255,4,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[5,23],[5,39],[13,33],[57,5],[72,24],[39,33]],255,4,true]"#, 0xdfc06c70263b47d6),
        ("line", r#"[[[20,25],[28,-3],[50,54],[59,40],[25,5]],255,2,false]"#, 0x958a24743ede5b92),
        ("line", r#"[[[16,68],[31,62],[-8,44],[3,21],[52,35]],255,9,true]"#, 0x967af8ee1f709a0d),
        ("line", r#"[[[1,68],[46,53],[48,2],[9,48],[67,5]],255,1,true]"#, 0xa6b5ce654d66d778),
        ("line", r#"[[[27,47],[-7,17],[69,48],[32,12],[43,12]],255,5,false]"#, 0xd0ae19ebc8209183),
        ("line", r#"[[[68,35],[46,55],[78,12],[50,45],[17,38],[39,20]],255,2,true]"#, 0x48b7a8805994b7ae),
        ("line", r#"[[[72,4],[-4,33],[17,22],[35,30]],255,1,true]"#, 0xde105323c40c3ca4),
        ("line", r#"[[[5,34],[-1,5]],255,2,true]"#, 0x5eb917272b7324ba),
        ("line", r#"[[[42,56],[19,52]],255,9,false]"#, 0x6a126acd61ec6176),
        ("line", r#"[[[-8,35],[36,10],[18,2]],255,3,false]"#, 0x63d2364b828e2b06),
        ("line", r#"[[[10,28],[57,13],[45,20],[16,45]],255,5,true]"#, 0xd9ec44aa399ad34e),
        ("line", r#"[[[50,26],[9,7],[0,-6],[11,16]],255,6,false]"#, 0xa3843852698df8a4),
        ("line", r#"[[[27,10]],255,1,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[42,8],[71,47],[43,25],[0,9]],255,9,false]"#, 0x30a134e4b4256d23),
        ("line", r#"[[[43,38],[16,54],[7,54],[16,30],[67,63],[33,60]],255,1,false]"#, 0x9b7215df0e42e539),
        ("line", r#"[[[49,46],[27,-1],[44,53]],255,6,false]"#, 0x157d6c7c091648d3),
        ("line", r#"[[[-2,20],[71,56],[14,16],[78,35],[49,35]],255,6,true]"#, 0xcf80f558d0d41776),
        ("line", r#"[[[78,44],[13,21],[22,52],[68,6],[72,62]],255,5,true]"#, 0x9640f5c8cdbb65bd),
        ("line", r#"[[[42,41],[70,3],[25,26],[35,44]],255,6,true]"#, 0xd9a3a0a4fe059177),
        ("line", r#"[[[47,19]],255,5,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[9,48],[35,27]],255,12,true]"#, 0x5f031c63bb156649),
        ("line", r#"[[[59,34]],255,1,true]"#, 0x880bb47fa003a145),
        ("line", r#"[[[13,54],[38,54],[63,50],[-4,41],[63,33],[42,10]],255,12,false]"#, 0x6992e73ce2aed680),
        ("rrect", r#"[[43,30,84,59],4.5,null,100,3]"#, 0xa6d1354f40d6d795),
        ("rrect", r#"[[48,43,84,73],30,200,100,3]"#, 0x8e6e38a012a38379),
        ("rrect", r#"[[60,23,92,50],0,200,null,5]"#, 0xa1dc33dc006ef945),
        ("rrect", r#"[[-8,55,29,92],0,null,100,3]"#, 0x4ab8f654e90f8e2d),
        ("rrect", r#"[[18,-10,49,30],4.5,200,100,5]"#, 0xe4c8e5f87a900a65),
        ("rrect", r#"[[34,29,56,78],1,null,null,2]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[10,5,36,14],4.5,200,100,1]"#, 0x7bf5b1eaf648cc65),
        ("rrect", r#"[[53,40,79,57],30,200,100,3]"#, 0x1e3c0fc9a2e70b05),
        ("rrect", r#"[[6,23,18,43],12,null,null,5]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[8,30,17,64],30,null,null,2]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[18,45,68,91],30,null,100,3]"#, 0xe6c2e522d0c90c19),
        ("rrect", r#"[[48,28,93,41],1,null,100,2]"#, 0x3cf99c7fdc001be5),
        ("rrect", r#"[[-4,-5,41,43],30,200,100,1]"#, 0xb87d2aae65be8b79),
        ("rrect", r#"[[47,24,86,48],30,null,100,5]"#, 0xb8125a38185bf9c9),
        ("rrect", r#"[[19,-5,39,-2],0,null,null,2]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[29,45,49,89],8,200,100,3]"#, 0x3e56c710b58f8f79),
        ("rrect", r#"[[50,26,69,68],8,null,100,1]"#, 0x4119bee237569485),
        ("rrect", r#"[[-6,25,11,46],5,200,null,1]"#, 0x209db67fefdc2265),
        ("rrect", r#"[[6,12,10,53],12,200,100,1]"#, 0xf09a0b02289bf525),
        ("rrect", r#"[[51,42,91,85],8,200,null,3]"#, 0xdf29f40b226ddc1d),
        ("rrect", r#"[[11,25,11,64],1,null,100,2]"#, 0xe8910be932437f09),
        ("rrect", r#"[[46,26,81,32],8,null,null,5]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[9,13,32,40],12,null,100,2]"#, 0x2ef381236d7cfc75),
        ("rrect", r#"[[6,14,43,52],8,null,100,1]"#, 0x18e4992d4497c88d),
        ("rrect", r#"[[9,40,16,81],1,null,100,5]"#, 0x550be1db64a4679d),
        ("rrect", r#"[[-3,-7,28,14],3,200,null,2]"#, 0x1a470b0162c03095),
        ("rrect", r#"[[65,54,89,69],4.5,null,null,5]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[39,-2,51,25],12,null,null,3]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[1,39,19,76],30,200,null,5]"#, 0xfed99d92fa63d4ad),
        ("rrect", r#"[[16,31,63,81],4.5,200,100,1]"#, 0xc2f0fad5b1de4e0d),
        ("rrect", r#"[[44,21,55,21],5,200,null,5]"#, 0xbdccaa12ae066bc5),
        ("rrect", r#"[[62,25,91,25],3,200,null,1]"#, 0x288ad9a5676c8cc5),
        ("rrect", r#"[[20,19,65,29],5,null,null,3]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[14,17,52,38],12,200,null,3]"#, 0xb29c19e9180b21b5),
        ("rrect", r#"[[15,49,30,99],3,200,100,1]"#, 0x98f785cd70f67925),
        ("rrect", r#"[[42,-6,71,30],0,null,null,5]"#, 0x880bb47fa003a145),
        ("rrect", r#"[[3,12,37,29],30,200,100,1]"#, 0x94858c80ada7dbc5),
        ("rrect", r#"[[13,14,13,39],4.5,200,null,5]"#, 0xd341f5456d46d795),
        ("rrect", r#"[[26,35,73,42],12,200,null,5]"#, 0xb8d48174e88546f5),
        ("rrect", r#"[[21,14,61,53],1,200,100,1]"#, 0x8e1cf54665241d35),
        ("rrect", r#"[[31,2,70,4],0,null,100,3]"#, 0xbd84a9c68d44a09d),
        ("rrect", r#"[[9,28,46,34],4.5,200,null,2]"#, 0x248bdb7721418255),
        ("rrect", r#"[[56,0,68,24],8,200,100,3]"#, 0x38053f23a72c92cd),
        ("rrect", r#"[[65,42,79,56],3,null,100,1]"#, 0x115060ac6b88fdf9),
        ("rrect", r#"[[61,28,68,43],5,200,100,1]"#, 0xb5e038dd037dc295),
        ("point", r#"[[[48,32],[68,20],[40,5],[36,30],[46,32]],255]"#, 0x2bf9842c99b8ba2a),
        ("point", r#"[[[12,18],[55,20],[0,3],[12,50],[33,58]],255]"#, 0x0c86d1e6f35008e6),
        ("point", r#"[[[53,10],[51,9],[55,60],[66,33],[47,15]],255]"#, 0x5c308b0a3b77c81f),
    ];
}
