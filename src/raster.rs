//! Rust equivalents of the Pillow image calls the design engine makes: `Image.paste`, `ImageChops`,
//! `Image.rotate/resize/transpose/crop`, `ImageFilter` and `ImageOps.autocontrast`.
//!
//! Everything works on `GrayImage`: 255 is white paper / a set mask pixel, 0 is black ink / a clear one.
//! Where Pillow rounds or clips in a particular way (`DIV255`, fixed-point resampling, rank-filter edges),
//! the same rule is used here, so results match Pillow pixel for pixel unless a function says otherwise.

use image::GrayImage;

/// The note grid: exactly what the M110's calibrated label area accepts.
pub const W: u32 = 312;
pub const H: u32 = 216;

/// Pillow's resampling filters of the same names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resample {
    Nearest,
    Box,
    Bilinear,
    Lanczos,
}

/// `crop` and `resize` refuse to allocate more pixels than this and return an empty image instead.
pub const MAX_PIXELS: u64 = 1 << 28;

/// The pixel bytes, row by row.
pub fn raw_mut(img: &mut GrayImage) -> &mut [u8] {
    &mut **img
}

// --- constructors ------------------------------------------------------------------------------

/// A new image, all 255 (`Image.new("L", size, 255)`).
pub fn paper(w: u32, h: u32) -> GrayImage {
    GrayImage::from_pixel(w, h, image::Luma([255]))
}

/// A new image, all 0 (`Image.new("L", size, 0)`).
pub fn blank(w: u32, h: u32) -> GrayImage {
    GrayImage::new(w, h)
}

/// An image of one value.
pub fn filled(w: u32, h: u32, value: u8) -> GrayImage {
    GrayImage::from_pixel(w, h, image::Luma([value]))
}

// --- paste -------------------------------------------------------------------------------------

/// Pillow's `DIV255`: rounds to nearest.
#[inline]
fn div255(a: u32) -> u32 {
    let tmp = a + 128;
    ((tmp >> 8) + tmp) >> 8
}

/// Pillow's `BLEND(mask, in1, in2)`: `in1` where the mask is 0, `in2` where it is 255.
#[inline]
fn blend(mask: u8, below: u8, above: u8) -> u8 {
    div255(below as u32 * (255 - mask as u32) + above as u32 * mask as u32) as u8
}

/// The part of a `sw` x `sh` source placed at (x, y) that lands inside a `dw` x `dh` destination:
/// (source x start, source y start, dest x start, dest y start, width, height).
fn clip_rect(
    dw: u32,
    dh: u32,
    sw: u32,
    sh: u32,
    x: i32,
    y: i32,
) -> Option<(u32, u32, u32, u32, u32, u32)> {
    let (x, y) = (x as i64, y as i64);
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + sw as i64).min(dw as i64);
    let y1 = (y + sh as i64).min(dh as i64);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        (x0 - x) as u32,
        (y0 - y) as u32,
        x0 as u32,
        y0 as u32,
        (x1 - x0) as u32,
        (y1 - y0) as u32,
    ))
}

/// `Image.paste(value, (x, y), mask)`: a flat value through a mask the size of the area. A mask of 255 sets
/// the value, 0 leaves the pixel, and anything between blends, as Pillow does for "L" masks.
pub fn paste_value(dst: &mut GrayImage, value: u8, x: i32, y: i32, mask: &GrayImage) {
    let Some((sx, sy, dx, dy, w, h)) =
        clip_rect(dst.width(), dst.height(), mask.width(), mask.height(), x, y)
    else {
        return;
    };
    let dw = dst.width() as usize;
    let mw = mask.width() as usize;
    let mraw = mask.as_raw();
    let draw = raw_mut(dst);
    for row in 0..h as usize {
        let d = (dy as usize + row) * dw + dx as usize;
        let m = (sy as usize + row) * mw + sx as usize;
        for col in 0..w as usize {
            let alpha = mraw[m + col];
            if alpha == 255 {
                draw[d + col] = value;
            } else if alpha != 0 {
                draw[d + col] = blend(alpha, draw[d + col], value);
            }
        }
    }
}

/// `Image.paste(value, (x0, y0, x1, y1))`: fill a box (end exclusive), clipped.
pub fn fill_box(dst: &mut GrayImage, value: u8, x0: i32, y0: i32, x1: i32, y1: i32) {
    let x0 = (x0 as i64).clamp(0, dst.width() as i64) as usize;
    let x1 = (x1 as i64).clamp(0, dst.width() as i64) as usize;
    let y0 = (y0 as i64).clamp(0, dst.height() as i64) as usize;
    let y1 = (y1 as i64).clamp(0, dst.height() as i64) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let dw = dst.width() as usize;
    let raw = raw_mut(dst);
    for row in y0..y1 {
        raw[row * dw + x0..row * dw + x1].fill(value);
    }
}

/// `Image.paste(src, (x, y), mask)`: copy `src` into `dst`, clipped to `dst`. With a mask (the same size as
/// `src`; where it is smaller the overlap is used) 255 copies, 0 leaves the pixel and values between blend.
pub fn paste(dst: &mut GrayImage, src: &GrayImage, x: i32, y: i32, mask: Option<&GrayImage>) {
    let (sw, sh) = match mask {
        Some(m) => (src.width().min(m.width()), src.height().min(m.height())),
        None => (src.width(), src.height()),
    };
    let Some((sx, sy, dx, dy, w, h)) = clip_rect(dst.width(), dst.height(), sw, sh, x, y) else {
        return;
    };
    let dw = dst.width() as usize;
    let srcw = src.width() as usize;
    let sraw = src.as_raw();
    let mraw = mask.map(|m| (m.as_raw(), m.width() as usize));
    let draw = raw_mut(dst);
    for row in 0..h as usize {
        let d = (dy as usize + row) * dw + dx as usize;
        let s = (sy as usize + row) * srcw + sx as usize;
        match mraw {
            None => draw[d..d + w as usize].copy_from_slice(&sraw[s..s + w as usize]),
            Some((m, mw)) => {
                let mo = (sy as usize + row) * mw + sx as usize;
                for col in 0..w as usize {
                    let alpha = m[mo + col];
                    if alpha == 255 {
                        draw[d + col] = sraw[s + col];
                    } else if alpha != 0 {
                        draw[d + col] = blend(alpha, draw[d + col], sraw[s + col]);
                    }
                }
            }
        }
    }
}

/// `Image.composite(a, b, mask)`: `a` where the mask is 255 and `b` where it is 0.
pub fn composite(a: &GrayImage, b: &GrayImage, mask: &GrayImage) -> GrayImage {
    let mut out = b.clone();
    paste(&mut out, a, 0, 0, Some(mask));
    out
}

/// `Image.blend(a, b, alpha)`: `a + alpha * (b - a)`, truncated like Pillow.
pub fn blend_images(a: &GrayImage, b: &GrayImage, alpha: f64) -> GrayImage {
    if alpha == 0.0 {
        return a.clone();
    }
    if alpha == 1.0 {
        return b.clone();
    }
    zip_with(a, b, |p, q| {
        let (p, q) = (p as i32, q as i32);
        (p as f64 + alpha * (q - p) as f64) as i32 as u8
    })
}

// --- per-pixel maths (ImageChops) --------------------------------------------------------------

fn zip_with(a: &GrayImage, b: &GrayImage, f: impl Fn(u8, u8) -> u8) -> GrayImage {
    let w = a.width().min(b.width());
    let h = a.height().min(b.height());
    let mut out = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.put_pixel(
                x,
                y,
                image::Luma([f(a.get_pixel(x, y).0[0], b.get_pixel(x, y).0[0])]),
            );
        }
    }
    out
}

/// `ImageChops.invert` (255 - v).
pub fn invert(img: &GrayImage) -> GrayImage {
    point(img, |v| 255 - v)
}

/// `ImageChops.logical_and` on mode "1" images: set where both are set.
pub fn logical_and(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| if p != 0 && q != 0 { 255 } else { 0 })
}

/// `ImageChops.logical_or` on mode "1" images.
pub fn logical_or(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| if p != 0 || q != 0 { 255 } else { 0 })
}

/// `ImageChops.multiply`: `a * b / 255`, truncated.
pub fn multiply(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| (p as u32 * q as u32 / 255) as u8)
}

/// `ImageChops.subtract` (scale 1, offset 0): `a - b`, clipped at 0.
pub fn subtract(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| p.saturating_sub(q))
}

/// `ImageChops.add` (scale 1, offset 0): `a + b`, clipped at 255.
pub fn add(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| p.saturating_add(q))
}

/// `ImageChops.darker`: the smaller value.
pub fn darker(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| p.min(q))
}

/// `ImageChops.lighter`: the larger value.
pub fn lighter(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| p.max(q))
}

/// `ImageChops.difference`: `|a - b|`.
pub fn difference(a: &GrayImage, b: &GrayImage) -> GrayImage {
    zip_with(a, b, |p, q| p.abs_diff(q))
}

/// `ImageChops.offset`: shift the picture by (dx, dy), wrapping around the edges.
pub fn offset(img: &GrayImage, dx: i32, dy: i32) -> GrayImage {
    let (w, h) = (img.width() as i64, img.height() as i64);
    if w == 0 || h == 0 {
        return img.clone();
    }
    let mut out = GrayImage::new(w as u32, h as u32);
    for y in 0..h {
        let sy = (y - dy as i64).rem_euclid(h);
        for x in 0..w {
            let sx = (x - dx as i64).rem_euclid(w);
            out.put_pixel(x as u32, y as u32, *img.get_pixel(sx as u32, sy as u32));
        }
    }
    out
}

// --- geometry ----------------------------------------------------------------------------------

/// `Image.crop((x0, y0, x1, y1))`: pixels outside the image read as 0.
pub fn crop(img: &GrayImage, x0: i32, y0: i32, x1: i32, y1: i32) -> GrayImage {
    let w = (x1 as i64 - x0 as i64).max(0) as u32;
    let h = (y1 as i64 - y0 as i64).max(0) as u32;
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
        return GrayImage::new(0, 0);
    }
    let mut out = GrayImage::new(w, h);
    let src = img.as_raw();
    let sw = img.width() as i64;
    let sh = img.height() as i64;
    let ow = w as usize;
    let raw = raw_mut(&mut out);
    for oy in 0..h as i64 {
        let sy = y0 as i64 + oy;
        if sy < 0 || sy >= sh {
            continue;
        }
        let from = (x0 as i64).max(0);
        let to = (x0 as i64 + w as i64).min(sw);
        if to <= from {
            continue;
        }
        let o = oy as usize * ow + (from - x0 as i64) as usize;
        let s = sy as usize * sw as usize + from as usize;
        raw[o..o + (to - from) as usize].copy_from_slice(&src[s..s + (to - from) as usize]);
    }
    out
}

/// `Image.getbbox()`: the box of the non-zero pixels, end exclusive; None when there are none.
pub fn getbbox(img: &GrayImage) -> Option<(u32, u32, u32, u32)> {
    let w = img.width() as usize;
    let raw = img.as_raw();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut any = false;
    for (i, &v) in raw.iter().enumerate() {
        if v != 0 {
            let (x, y) = ((i % w) as u32, (i / w) as u32);
            any = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    any.then_some((x0, y0, x1, y1))
}

/// `Image.histogram()`.
pub fn histogram(img: &GrayImage) -> [u32; 256] {
    let mut counts = [0u32; 256];
    for &v in img.as_raw() {
        counts[v as usize] += 1;
    }
    counts
}

/// `Image.getextrema()`: (lowest, highest); (0, 0) for an empty image.
pub fn extrema(img: &GrayImage) -> (u8, u8) {
    let mut low = 255u8;
    let mut high = 0u8;
    for &v in img.as_raw() {
        low = low.min(v);
        high = high.max(v);
    }
    if img.as_raw().is_empty() {
        (0, 0)
    } else {
        (low, high)
    }
}

/// `Image.point(f)` for a value-to-value function.
pub fn point(img: &GrayImage, f: impl Fn(u8) -> u8) -> GrayImage {
    let mut lut = [0u8; 256];
    for (v, slot) in lut.iter_mut().enumerate() {
        *slot = f(v as u8);
    }
    let mut out = img.clone();
    for p in raw_mut(&mut out) {
        *p = lut[*p as usize];
    }
    out
}

/// `point(lambda v: 255 if v >= cut else 0)`.
pub fn threshold(img: &GrayImage, cut: u8) -> GrayImage {
    point(img, |v| if v >= cut { 255 } else { 0 })
}

/// `Image.transpose(FLIP_LEFT_RIGHT)`.
pub fn flip_h(img: &GrayImage) -> GrayImage {
    image::imageops::flip_horizontal(img)
}

/// `Image.transpose(FLIP_TOP_BOTTOM)`.
pub fn flip_v(img: &GrayImage) -> GrayImage {
    image::imageops::flip_vertical(img)
}

/// `Image.transpose(ROTATE_180)`.
pub fn rotate_180(img: &GrayImage) -> GrayImage {
    image::imageops::rotate180(img)
}

/// `Image.rotate(degrees, resample, center=center, fillcolor=fill)`: counterclockwise, same size. Without a
/// centre the middle of the image is used and, like Pillow, 0 and 180 degrees (and 90/270 on a square image)
/// are exact transposes.
pub fn rotate(
    img: &GrayImage,
    degrees: f64,
    center: Option<(f64, f64)>,
    fill: u8,
    resample: Resample,
) -> GrayImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return img.clone();
    }
    let angle = if degrees.is_finite() {
        degrees.rem_euclid(360.0)
    } else {
        0.0
    };
    if center.is_none() {
        if angle == 0.0 {
            return img.clone();
        }
        if angle == 180.0 {
            return rotate_180(img);
        }
        if w == h && angle == 90.0 {
            return image::imageops::rotate270(img);
        }
        if w == h && angle == 270.0 {
            return image::imageops::rotate90(img);
        }
    }
    let (cx, cy) = center.unwrap_or((w as f64 / 2.0, h as f64 / 2.0));
    let a = -angle.to_radians();
    let round15 = |v: f64| (v * 1e15).round() / 1e15;
    let (cos, sin) = (round15(a.cos()), round15(a.sin()));
    // The matrix maps an output pixel to an input pixel: x_in = m0*x + m1*y + m2, y_in = m3*x + m4*y + m5.
    let m = [cos, sin, 0.0, -sin, cos, 0.0];
    let m2 = m[0] * -cx + m[1] * -cy + cx;
    let m5 = m[3] * -cx + m[4] * -cy + cy;
    let m = [m[0], m[1], m2, m[3], m[4], m5];
    affine(img, m, fill, resample)
}

/// Pillow's `Image.transform(size, AFFINE, m, resample, fillcolor)` for same-size output.
fn affine(img: &GrayImage, m: [f64; 6], fill: u8, resample: Resample) -> GrayImage {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let mut out = filled(img.width(), img.height(), fill);
    let src = img.as_raw();
    if matches!(resample, Resample::Nearest | Resample::Box) {
        // Pillow's `affine_fixed`: 16.16 fixed point, sampling at pixel centres.
        let fix = |v: f64| (v * 65536.0 + 0.5).floor() as i64;
        let [a0, a1, a2, a3, a4, a5] = m.map(fix);
        for y in 0..h {
            let mut xx = a2 + a1 * y + a1 / 2 + a0 / 2;
            let mut yy = a5 + a4 * y + a4 / 2 + a3 / 2;
            for x in 0..w {
                let (xin, yin) = (xx >> 16, yy >> 16);
                if xin >= 0 && xin < w && yin >= 0 && yin < h {
                    out.put_pixel(
                        x as u32,
                        y as u32,
                        image::Luma([src[(yin * w + xin) as usize]]),
                    );
                }
                xx += a0;
                yy += a3;
            }
        }
        return out;
    }
    for y in 0..h {
        for x in 0..w {
            let xx = m[0] * (x as f64 + 0.5) + m[1] * (y as f64 + 0.5) + m[2];
            let yy = m[3] * (x as f64 + 0.5) + m[4] * (y as f64 + 0.5) + m[5];
            let value = match resample {
                Resample::Nearest | Resample::Box => {
                    if xx < 0.0 || yy < 0.0 || xx >= w as f64 || yy >= h as f64 {
                        continue;
                    }
                    src[yy as usize * w as usize + xx as usize]
                }
                Resample::Bilinear | Resample::Lanczos => {
                    if xx < 0.0 || yy < 0.0 || xx >= w as f64 || yy >= h as f64 {
                        continue;
                    }
                    let (fx, fy) = (xx - 0.5, yy - 0.5);
                    let (x0, y0) = (fx.floor(), fy.floor());
                    let (dx, dy) = (fx - x0, fy - y0);
                    let get = |px: f64, py: f64| {
                        let px = (px as i64).clamp(0, w - 1) as usize;
                        let py = (py as i64).clamp(0, h - 1) as usize;
                        src[py * w as usize + px] as f64
                    };
                    let top = get(x0, y0) * (1.0 - dx) + get(x0 + 1.0, y0) * dx;
                    let bottom = get(x0, y0 + 1.0) * (1.0 - dx) + get(x0 + 1.0, y0 + 1.0) * dx;
                    (top * (1.0 - dy) + bottom * dy) as u8
                }
            };
            out.put_pixel(x as u32, y as u32, image::Luma([value]));
        }
    }
    out
}

// --- resize (Pillow's separable fixed-point resampler) -----------------------------------------

const PRECISION_BITS: u32 = 32 - 8 - 2;

fn filter_support(how: Resample) -> f64 {
    match how {
        Resample::Nearest => 0.5,
        Resample::Box => 0.5,
        Resample::Bilinear => 1.0,
        Resample::Lanczos => 3.0,
    }
}

fn filter_value(how: Resample, x: f64) -> f64 {
    match how {
        Resample::Nearest | Resample::Box => {
            if x > -0.5 && x <= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        Resample::Bilinear => {
            let x = x.abs();
            if x < 1.0 {
                1.0 - x
            } else {
                0.0
            }
        }
        Resample::Lanczos => {
            fn sinc(x: f64) -> f64 {
                if x == 0.0 {
                    1.0
                } else {
                    let x = x * std::f64::consts::PI;
                    x.sin() / x
                }
            }
            if (-3.0..3.0).contains(&x) {
                sinc(x) * sinc(x / 3.0)
            } else {
                0.0
            }
        }
    }
}

struct Coeffs {
    ksize: usize,
    bounds: Vec<(usize, usize)>, // (first input index, count)
    weights: Vec<i32>,           // ksize per output pixel
}

fn precompute(in_size: usize, out_size: usize, how: Resample) -> Coeffs {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = filter_support(how) * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut weights = vec![0i32; ksize * out_size];
    let mut kk = vec![0f64; ksize];
    for xx in 0..out_size {
        let center = (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        let xmin = ((center - support + 0.5) as i64).max(0);
        let xmax = (((center + support + 0.5) as i64).min(in_size as i64) - xmin).max(0);
        let mut total = 0.0;
        for k in kk.iter_mut() {
            *k = 0.0;
        }
        for x in 0..xmax as usize {
            let w = filter_value(how, (x as f64 + xmin as f64 - center + 0.5) * ss);
            kk[x] = w;
            total += w;
        }
        if total != 0.0 {
            for k in kk.iter_mut().take(xmax as usize) {
                *k /= total;
            }
        }
        for (x, k) in kk.iter().enumerate() {
            let scaled = k * (1u64 << PRECISION_BITS) as f64;
            weights[xx * ksize + x] = (scaled + if *k < 0.0 { -0.5 } else { 0.5 }) as i32;
        }
        bounds.push((xmin as usize, xmax as usize));
    }
    Coeffs {
        ksize,
        bounds,
        weights,
    }
}

#[inline]
fn clip8(v: i64) -> u8 {
    (v >> PRECISION_BITS).clamp(0, 255) as u8
}

fn resample_horizontal(src: &[u8], sw: usize, sh: usize, ow: usize, how: Resample) -> Vec<u8> {
    let c = precompute(sw, ow, how);
    let mut out = vec![0u8; ow * sh];
    for y in 0..sh {
        let row = &src[y * sw..(y + 1) * sw];
        for x in 0..ow {
            let (xmin, n) = c.bounds[x];
            let k = &c.weights[x * c.ksize..];
            let mut acc: i64 = 1 << (PRECISION_BITS - 1);
            for i in 0..n {
                acc += row[xmin + i] as i64 * k[i] as i64;
            }
            out[y * ow + x] = clip8(acc);
        }
    }
    out
}

fn resample_vertical(src: &[u8], sw: usize, sh: usize, oh: usize, how: Resample) -> Vec<u8> {
    let c = precompute(sh, oh, how);
    let mut out = vec![0u8; sw * oh];
    for y in 0..oh {
        let (ymin, n) = c.bounds[y];
        let k = &c.weights[y * c.ksize..];
        for x in 0..sw {
            let mut acc: i64 = 1 << (PRECISION_BITS - 1);
            for i in 0..n {
                acc += src[(ymin + i) * sw + x] as i64 * k[i] as i64;
            }
            out[y * sw + x] = clip8(acc);
        }
    }
    out
}

/// `Image.resize((w, h), how)`. `Nearest` picks the source pixel under each output pixel centre; the others
/// are Pillow's separable convolution (area averaging when shrinking), horizontal pass first, in the same
/// fixed-point arithmetic, so the output matches Pillow exactly.
pub fn resize(img: &GrayImage, w: u32, h: u32, how: Resample) -> GrayImage {
    if w as u64 * h as u64 > MAX_PIXELS {
        return GrayImage::new(0, 0);
    }
    if w == 0 || h == 0 || img.width() == 0 || img.height() == 0 {
        return GrayImage::new(w, h);
    }
    if (img.width(), img.height()) == (w, h) {
        return img.clone();
    }
    let (sw, sh) = (img.width() as usize, img.height() as usize);
    if how == Resample::Nearest {
        // Pillow steps the source position by `scale` from half a step, so exact ties round the same way.
        let mut out = GrayImage::new(w, h);
        let xs = sw as f64 / w as f64;
        let ys = sh as f64 / h as f64;
        let raw = img.as_raw();
        let mut column = Vec::with_capacity(w as usize);
        let mut at = xs * 0.5;
        for _ in 0..w {
            column.push((at as usize).min(sw - 1));
            at += xs;
        }
        let mut at = ys * 0.5;
        for y in 0..h {
            let sy = (at as usize).min(sh - 1);
            at += ys;
            for x in 0..w {
                out.put_pixel(x, y, image::Luma([raw[sy * sw + column[x as usize]]]));
            }
        }
        return out;
    }
    let mut data: Vec<u8>;
    let mut cur_w = sw;
    if sw != w as usize {
        data = resample_horizontal(img.as_raw(), sw, sh, w as usize, how);
        cur_w = w as usize;
    } else {
        data = img.as_raw().clone();
    }
    if sh != h as usize {
        data = resample_vertical(&data, cur_w, sh, h as usize, how);
    }
    GrayImage::from_raw(w, h, data).unwrap_or_else(|| GrayImage::new(w, h))
}

// --- filters -----------------------------------------------------------------------------------

fn rank_filter(img: &GrayImage, size: u32, take_max: bool) -> GrayImage {
    let size = (size | 1) as i64;
    let margin = size / 2;
    let (w, h) = (img.width() as i64, img.height() as i64);
    if size <= 1 || w == 0 || h == 0 {
        return img.clone();
    }
    let src = img.as_raw();
    let pick = |a: u8, b: u8| if take_max { a.max(b) } else { a.min(b) };
    // The window is cut off at the border. It is separable, so do the rows and then the columns.
    let mut rows = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut v = src[(y * w + x) as usize];
            for k in (x - margin).max(0)..=(x + margin).min(w - 1) {
                v = pick(v, src[(y * w + k) as usize]);
            }
            rows[(y * w + x) as usize] = v;
        }
    }
    let mut out = img.clone();
    let raw = raw_mut(&mut out);
    for y in 0..h {
        for x in 0..w {
            let mut v = rows[(y * w + x) as usize];
            for k in (y - margin).max(0)..=(y + margin).min(h - 1) {
                v = pick(v, rows[(k * w + x) as usize]);
            }
            raw[(y * w + x) as usize] = v;
        }
    }
    out
}

/// `ImageFilter.MaxFilter(size)`: the largest value in a size x size window (edges are copied unchanged).
pub fn max_filter(img: &GrayImage, size: u32) -> GrayImage {
    rank_filter(img, size, true)
}

/// `ImageFilter.MinFilter(size)`.
pub fn min_filter(img: &GrayImage, size: u32) -> GrayImage {
    rank_filter(img, size, false)
}

/// Box radius for one of three passes that together approximate a Gaussian of the given radius (Pillow).
fn gaussian_box_radius(radius: f64, passes: u32) -> f32 {
    // Pillow keeps these in C floats.
    let radius = radius as f32;
    let sigma2: f32 = radius * radius / passes as f32;
    let big_l: f32 = (12.0 * sigma2 as f64 + 1.0).sqrt() as f32;
    let l: f32 = ((big_l as f64 - 1.0) / 2.0).floor() as f32;
    let mut a: f32 =
        ((2.0 * l + 1.0) as f64 * (l as f64 * (l as f64 + 1.0) - 3.0 * sigma2 as f64)) as f32;
    a = (a as f64 / (6.0 * (sigma2 as f64 - (l as f64 + 1.0) * (l as f64 + 1.0)))) as f32;
    l + a
}

/// One horizontal box blur with fractional radius and replicated edges (Pillow's `ImagingHorizontalBoxBlur`).
fn box_blur_rows(data: &mut [u8], w: usize, h: usize, float_radius: f32) {
    if w == 0 || float_radius <= 0.0 {
        return;
    }
    let radius = float_radius as i64;
    let ww = ((1u32 << 24) as f32 / (float_radius * 2.0 + 1.0)) as u32 as i64;
    let fw = ((1i64 << 24) - (radius * 2 + 1) * ww) / 2;
    let last = w as i64 - 1;
    let mut line = vec![0u8; w];
    for y in 0..h {
        let row = &mut data[y * w..(y + 1) * w];
        let at = |i: i64| row[i.clamp(0, last) as usize] as i64;
        let mut acc: i64 = 0;
        for i in -radius..=radius {
            acc += at(i);
        }
        for x in 0..w as i64 {
            let bulk = acc * ww + (at(x - radius - 1) + at(x + radius + 1)) * fw;
            line[x as usize] = ((bulk + (1 << 23)) >> 24).clamp(0, 255) as u8;
            acc += at(x + radius + 1) - at(x - radius);
        }
        row.copy_from_slice(&line);
    }
}

fn transpose(data: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; data.len()];
    for y in 0..h {
        for x in 0..w {
            out[x * h + y] = data[y * w + x];
        }
    }
    out
}

/// `ImageFilter.GaussianBlur(radius)`: Pillow approximates it with three box blurs per axis, horizontal
/// first, then vertical. This follows the same scheme (fractional box edges, replicated borders).
pub fn gaussian_blur(img: &GrayImage, radius: f64) -> GrayImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 || radius.is_nan() || radius <= 0.0 || !radius.is_finite() {
        return img.clone();
    }
    let passes = 3;
    let r = gaussian_box_radius(radius, passes);
    let mut data = img.as_raw().clone();
    for _ in 0..passes {
        box_blur_rows(&mut data, w, h, r);
    }
    let mut t = transpose(&data, w, h);
    for _ in 0..passes {
        box_blur_rows(&mut t, h, w, r);
    }
    let data = transpose(&t, h, w);
    GrayImage::from_raw(w as u32, h as u32, data).unwrap_or_else(|| img.clone())
}

/// `ImageOps.autocontrast(img, cutoff=percent)`.
pub fn autocontrast(img: &GrayImage, cutoff: f64) -> GrayImage {
    let mut hist = histogram(img).map(|c| c as i64);
    if cutoff != 0.0 {
        let n: i64 = hist.iter().sum();
        let mut cut = (n as f64 * cutoff / 100.0).floor() as i64;
        for lo in 0..256 {
            if cut > hist[lo] {
                cut -= hist[lo];
                hist[lo] = 0;
            } else {
                hist[lo] -= cut;
                cut = 0;
            }
            if cut <= 0 {
                break;
            }
        }
        let mut cut = (n as f64 * cutoff / 100.0).floor() as i64;
        for hi in (0..256).rev() {
            if cut > hist[hi] {
                cut -= hist[hi];
                hist[hi] = 0;
            } else {
                hist[hi] -= cut;
                cut = 0;
            }
            if cut <= 0 {
                break;
            }
        }
    }
    let lo = hist.iter().position(|&c| c != 0).unwrap_or(0);
    let hi = hist.iter().rposition(|&c| c != 0).unwrap_or(0);
    if hi <= lo {
        return img.clone();
    }
    let scale = 255.0 / (hi - lo) as f64;
    let offset = -(lo as f64) * scale;
    point(img, |v| {
        let ix = (v as f64 * scale + offset) as i64;
        ix.clamp(0, 255) as u8
    })
}

/// Share of pixels that are 0 (black) in a page image.
pub fn ink_fraction(img: &GrayImage) -> f64 {
    let total = img.as_raw().len();
    if total == 0 {
        return 0.0;
    }
    img.as_raw().iter().filter(|&&v| v == 0).count() as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            image::Luma([((x * 7 + y * 13 + x * y) % 256) as u8])
        })
    }

    #[test]
    fn paste_value_clips_and_blends() {
        let mut dst = paper(10, 10);
        let mask = filled(4, 4, 255);
        paste_value(&mut dst, 0, -2, 8, &mask);
        assert_eq!(dst.get_pixel(0, 8).0[0], 0);
        assert_eq!(dst.get_pixel(1, 9).0[0], 0);
        assert_eq!(dst.get_pixel(2, 8).0[0], 255);
        assert_eq!(dst.get_pixel(0, 7).0[0], 255);
        let mut dst = paper(4, 1);
        paste_value(
            &mut dst,
            0,
            0,
            0,
            &GrayImage::from_raw(4, 1, vec![0, 64, 128, 255]).unwrap(),
        );
        assert_eq!(dst.as_raw(), &vec![255, 191, 127, 0]);
    }

    #[test]
    fn chops_match_pillow_rules() {
        let a = GrayImage::from_raw(3, 1, vec![200, 10, 255]).unwrap();
        let b = GrayImage::from_raw(3, 1, vec![100, 20, 255]).unwrap();
        assert_eq!(subtract(&a, &b).as_raw(), &vec![100, 0, 0]);
        assert_eq!(add(&a, &b).as_raw(), &vec![255, 30, 255]);
        assert_eq!(multiply(&a, &b).as_raw(), &vec![78, 0, 255]);
        assert_eq!(darker(&a, &b).as_raw(), &vec![100, 10, 255]);
        assert_eq!(invert(&a).as_raw(), &vec![55, 245, 0]);
    }

    #[test]
    fn offset_wraps() {
        let img = GrayImage::from_raw(4, 1, vec![1, 2, 3, 4]).unwrap();
        assert_eq!(offset(&img, 1, 0).as_raw(), &vec![4, 1, 2, 3]);
        assert_eq!(offset(&img, -1, 0).as_raw(), &vec![2, 3, 4, 1]);
        assert_eq!(offset(&img, 5, 0).as_raw(), &vec![4, 1, 2, 3]);
    }

    #[test]
    fn crop_outside_reads_zero_and_bbox() {
        let img = paper(4, 4);
        let c = crop(&img, -1, -1, 2, 2);
        assert_eq!(c.as_raw(), &vec![0, 0, 0, 0, 255, 255, 0, 255, 255]);
        assert_eq!(getbbox(&c), Some((1, 1, 3, 3)));
        assert_eq!(getbbox(&blank(3, 3)), None);
    }

    #[test]
    fn nearest_resize_scales_whole_pixels() {
        let img = GrayImage::from_raw(2, 1, vec![10, 20]).unwrap();
        let big = resize(&img, 6, 3, Resample::Nearest);
        assert_eq!(&big.as_raw()[..6], &[10, 10, 10, 20, 20, 20]);
    }

    #[test]
    fn autocontrast_stretches() {
        let img = GrayImage::from_raw(4, 1, vec![50, 100, 150, 200]).unwrap();
        assert_eq!(autocontrast(&img, 0.0).as_raw(), &vec![0, 85, 170, 255]);
    }

    #[test]
    fn rank_filters_cut_the_window_at_the_border() {
        let mut img = blank(7, 7);
        img.put_pixel(3, 3, image::Luma([255]));
        let m = max_filter(&img, 3);
        let count = m.as_raw().iter().filter(|&&v| v == 255).count();
        assert_eq!(count, 9);
        let back = min_filter(&m, 3);
        assert_eq!(back.get_pixel(3, 3).0[0], 255);
        assert_eq!(back.as_raw().iter().filter(|&&v| v == 255).count(), 1);
    }

    #[test]
    fn rotate_right_angles() {
        let img = gradient(5, 5);
        let r = rotate(&img, 90.0, None, 0, Resample::Nearest);
        // 90 degrees counterclockwise: the right column becomes the top row.
        for i in 0..5 {
            assert_eq!(r.get_pixel(i, 0), img.get_pixel(4, i));
        }
        let again = rotate(&img, 90.0, Some((2.5, 2.5)), 0, Resample::Nearest);
        assert_eq!(r, again);
    }

    #[test]
    fn empty_and_tiny_images_and_wild_arguments_never_panic() {
        for (w, h) in [(0, 0), (0, 4), (4, 0), (1, 1), (3, 2)] {
            let img = gradient(w, h);
            let other = gradient(2, 5);
            let mut dst = gradient(w, h);
            paste_value(&mut dst, 9, -5, i32::MAX, &other);
            paste_value(&mut dst, 9, i32::MIN, 1, &other);
            paste(&mut dst, &other, 1, -1, Some(&other));
            paste(&mut dst, &other, i32::MAX, i32::MIN, None);
            fill_box(&mut dst, 3, i32::MIN, -3, i32::MAX, 99);
            for how in [
                Resample::Nearest,
                Resample::Box,
                Resample::Bilinear,
                Resample::Lanczos,
            ] {
                resize(&img, 0, 3, how);
                resize(&img, 7, 5, how);
                resize(&img, 1, 1, how);
                rotate(&img, 33.0, Some((f64::NAN, 2.0)), 1, how);
                rotate(&img, f64::INFINITY, None, 1, how);
            }
            offset(&img, i32::MIN, i32::MAX);
            crop(&img, i32::MIN, i32::MIN, i32::MAX, i32::MAX);
            crop(&img, 5, 5, 1, 1);
            getbbox(&img);
            histogram(&img);
            extrema(&img);
            max_filter(&img, 0);
            min_filter(&img, 100);
            gaussian_blur(&img, f64::NAN);
            gaussian_blur(&img, 1e6);
            gaussian_blur(&img, 0.01);
            autocontrast(&img, 99.0);
            autocontrast(&img, 150.0);
            ink_fraction(&img);
            flip_h(&img);
            flip_v(&img);
            multiply(&img, &other);
            blend_images(&img, &other, 0.3);
            composite(&img, &other, &other);
        }
    }

    #[test]
    fn blur_of_flat_is_flat() {
        let img = filled(20, 20, 77);
        assert_eq!(gaussian_blur(&img, 2.0), img);
        assert_eq!(gaussian_blur(&img, 0.0), img);
    }

    fn make(w: u32, h: u32, seed: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            image::Luma([((x * 37 + y * 91 + ((x * y) % 17) * 11 + seed * 53) % 256) as u8])
        })
    }

    fn fnv(img: &GrayImage) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in img.as_raw() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    // The tables below are hashes of what Pillow 13 produced for the same inputs (see `make`): every one of
    // these is bit-exact, including the resamplers, the Gaussian blur's box approximation and nearest rotation.

    #[test]
    fn resize_matches_pillow() {
        for (sw, sh, ow, oh, how, hash) in RESIZE {
            let how = match *how {
                "nearest" => Resample::Nearest,
                "box" => Resample::Box,
                "bilinear" => Resample::Bilinear,
                _ => Resample::Lanczos,
            };
            let out = resize(&make(*sw, *sh, *sw), *ow, *oh, how);
            assert_eq!(fnv(&out), *hash, "{sw}x{sh} -> {ow}x{oh} {how:?}");
        }
    }

    #[test]
    fn rotate_matches_pillow() {
        for (w, h, angle, center, fill, hash) in ROTATE {
            let out = rotate(&make(*w, *h, 3), *angle, *center, *fill, Resample::Nearest);
            assert_eq!(fnv(&out), *hash, "{w}x{h} {angle} {center:?} {fill}");
        }
    }

    #[test]
    fn gaussian_blur_matches_pillow() {
        for (radius, hash) in BLUR {
            assert_eq!(
                fnv(&gaussian_blur(&make(40, 30, 5), *radius)),
                *hash,
                "radius {radius}"
            );
        }
    }

    #[test]
    fn rank_filters_match_pillow() {
        for (kind, size, hash) in RANK {
            let src = make(25, 18, 2);
            let out = if *kind == "max" {
                max_filter(&src, *size)
            } else {
                min_filter(&src, *size)
            };
            assert_eq!(fnv(&out), *hash, "{kind} {size}");
        }
    }

    #[test]
    fn autocontrast_matches_pillow() {
        for (cutoff, hash) in AUTOCONTRAST {
            let src = point(&make(30, 20, 9), |v| (40 + v as u32 * 100 / 255) as u8);
            assert_eq!(fnv(&autocontrast(&src, *cutoff)), *hash, "cutoff {cutoff}");
        }
    }

    #[rustfmt::skip]
    const RESIZE: &[(u32, u32, u32, u32, &str, u64)] = &[
        (40, 30, 13, 9, "nearest", 0x7ef2c853691038ef),
        (40, 30, 13, 9, "box", 0x2407a1b145e26be1),
        (40, 30, 13, 9, "bilinear", 0x756bc571a79413b1),
        (40, 30, 13, 9, "lanczos", 0x5a0e2bd71d82a2bf),
        (40, 30, 80, 60, "nearest", 0x8dd5a7d11792c829),
        (40, 30, 80, 60, "box", 0x8dd5a7d11792c829),
        (40, 30, 80, 60, "bilinear", 0x9e6e247a1cf0dafd),
        (40, 30, 80, 60, "lanczos", 0x72f8108a2a207f88),
        (40, 30, 40, 11, "nearest", 0xbb1a851bef3973e3),
        (40, 30, 40, 11, "box", 0x83398f06fcd462d9),
        (40, 30, 40, 11, "bilinear", 0x5139314d6a5efee1),
        (40, 30, 40, 11, "lanczos", 0x67e3d30b9cc29802),
        (31, 17, 31, 40, "nearest", 0xea9a49c179a20dbe),
        (31, 17, 31, 40, "box", 0xea9a49c179a20dbe),
        (31, 17, 31, 40, "bilinear", 0xd92d47262dcb7bc6),
        (31, 17, 31, 40, "lanczos", 0xf88b2bd3cf1f0ba3),
        (64, 48, 7, 5, "nearest", 0xf0ee713473f94b5d),
        (64, 48, 7, 5, "box", 0x8b4704e17c965785),
        (64, 48, 7, 5, "bilinear", 0xbc5971dc784d33fa),
        (64, 48, 7, 5, "lanczos", 0xab6879f9df35b606),
        (10, 10, 33, 27, "nearest", 0x307f9c808de7dec0),
        (10, 10, 33, 27, "box", 0x050c0d8c1492445a),
        (10, 10, 33, 27, "bilinear", 0xfd1cb101404f1c9f),
        (10, 10, 33, 27, "lanczos", 0x7cf2b7575d0d6883),
        (50, 40, 49, 41, "nearest", 0xe1dc8df07ad6750a),
        (50, 40, 49, 41, "box", 0x812193ae45759822),
        (50, 40, 49, 41, "bilinear", 0x064683ecdac4107f),
        (50, 40, 49, 41, "lanczos", 0x30eda0f6f6d2f2c3),
        (20, 20, 3, 60, "nearest", 0x398a5634af612242),
        (20, 20, 3, 60, "box", 0xb868a4258941b929),
        (20, 20, 3, 60, "bilinear", 0x854b3108452d6c34),
        (20, 20, 3, 60, "lanczos", 0x3870b08ce8601e45),
    ];
    #[rustfmt::skip]
    const ROTATE: &[(u32, u32, f64, Option<(f64, f64)>, u8, u64)] = &[
        (30, 20, 30.0, None, 0, 0x8c508acd61d40dc5),
        (24, 24, 30.0, None, 0, 0x406cd60c6406f56a),
        (30, 20, -30.0, None, 255, 0xfd75c4e4d5bafde1),
        (24, 24, -30.0, None, 255, 0xff8181df36520fce),
        (30, 20, 45.0, Some((10.0, 12.0)), 7, 0x689afe113b55fcf5),
        (24, 24, 45.0, Some((10.0, 12.0)), 7, 0x41b191249c841a0e),
        (30, 20, 90.0, None, 0, 0x3ac3f9486b91d0c9),
        (24, 24, 90.0, None, 0, 0xf26df19514cfa12b),
        (30, 20, 90.0, Some((15.0, 15.0)), 9, 0xa655ecbe1eb41361),
        (24, 24, 90.0, Some((15.0, 15.0)), 9, 0x17c16a91f2643b18),
        (30, 20, 270.0, None, 0, 0x6677d2416666838d),
        (24, 24, 270.0, None, 0, 0xde44f402ce91f3d3),
        (30, 20, 180.0, None, 0, 0xa2b67b2eb0a6186b),
        (24, 24, 180.0, None, 0, 0x98635323f89deb37),
        (30, 20, 180.0, Some((7.0, 8.0)), 1, 0x435649e832782193),
        (24, 24, 180.0, Some((7.0, 8.0)), 1, 0xe828974b517ff807),
        (30, 20, 7.5, Some((31.0, 3.0)), 200, 0xe6e5a35a2d57f06a),
        (24, 24, 7.5, Some((31.0, 3.0)), 200, 0xb0dfa521dc323c1a),
        (30, 20, -90.0, None, 3, 0x5c998ba52a4a5da9),
        (24, 24, -90.0, None, 3, 0xde44f402ce91f3d3),
        (30, 20, 123.4, None, 0, 0xd2a8bd019d9c8b79),
        (24, 24, 123.4, None, 0, 0xbeefbfffa2bc7b5a),
        (30, 20, 0.0, Some((5.0, 5.0)), 0, 0x93c71a0ebb6547bb),
        (24, 24, 0.0, Some((5.0, 5.0)), 0, 0xcb0f0932efde151f),
        (30, 20, 359.0, Some((20.5, 10.5)), 0, 0x93c71a0ebb6547bb),
        (24, 24, 359.0, Some((20.5, 10.5)), 0, 0xcb0f0932efde151f),
    ];
    #[rustfmt::skip]
    const BLUR: &[(f64, u64)] = &[
        (0.5, 0xce95caf53b339a86),
        (1.0, 0x1d3c55f99328800c),
        (1.3, 0xf09665009eebd0da),
        (2.0, 0x6a201815d0c04d73),
        (3.7, 0x00b26030c5f1f3a9),
        (6.0, 0x8fe7d2cc19240ffe),
    ];
    #[rustfmt::skip]
    const RANK: &[(&str, u32, u64)] = &[
        ("max", 3, 0xb0b8e48c7ba70f3d),
        ("min", 3, 0xb88ecb7a8265df6d),
        ("max", 5, 0xf5dbcefe6c5e223a),
        ("min", 5, 0xcdeb1d7f87568791),
        ("max", 7, 0x7920c238829049b5),
        ("min", 7, 0x86e0f597f80396fc),
    ];
    #[rustfmt::skip]
    const AUTOCONTRAST: &[(f64, u64)] = &[
        (0.0, 0x3507673d380a1955),
        (1.0, 0x757f8de0fb34200d),
        (5.0, 0x48012f5633d4eac8),
    ];

    #[test]
    fn paste_matches_pillow() {
        for (x, y, hash) in PASTE_VALUE {
            let mut d = make(30, 20, 1);
            paste_value(&mut d, 77, *x, *y, &make(12, 9, 4));
            assert_eq!(fnv(&d), *hash, "paste value at {x},{y}");
        }
        for (x, y, hash) in PASTE_IMAGE {
            let mut d = make(30, 20, 1);
            paste(&mut d, &make(12, 9, 2), *x, *y, Some(&make(12, 9, 4)));
            assert_eq!(fnv(&d), *hash, "paste image with mask at {x},{y}");
        }
        for (x, y, hash) in PASTE_PLAIN {
            let mut d = make(30, 20, 1);
            paste(&mut d, &make(12, 9, 2), *x, *y, None);
            assert_eq!(fnv(&d), *hash, "paste image at {x},{y}");
        }
    }

    #[test]
    fn crop_offset_and_chops_match_pillow() {
        for ((x0, y0, x1, y1), w, h, hash) in CROP {
            let c = crop(&make(30, 20, 1), *x0, *y0, *x1, *y1);
            assert_eq!(
                (c.width(), c.height(), fnv(&c)),
                (*w, *h, *hash),
                "crop {x0},{y0},{x1},{y1}"
            );
        }
        for (dx, dy, hash) in OFFSET {
            assert_eq!(
                fnv(&offset(&make(30, 20, 1), *dx, *dy)),
                *hash,
                "offset {dx},{dy}"
            );
        }
        let (a, b) = (make(30, 20, 1), make(30, 20, 6));
        for (name, hash) in CHOPS {
            let out = match *name {
                "multiply" => multiply(&a, &b),
                "subtract" => subtract(&a, &b),
                "add" => add(&a, &b),
                "darker" => darker(&a, &b),
                "lighter" => lighter(&a, &b),
                "difference" => difference(&a, &b),
                "invert" => invert(&a),
                "composite" => composite(&a, &b, &make(30, 20, 8)),
                _ => blend_images(&a, &b, 0.45),
            };
            assert_eq!(fnv(&out), *hash, "{name}");
        }
    }

    #[rustfmt::skip]
    const PASTE_VALUE: &[(i32, i32, u64)] = &[
        (-3, -2, 0x702da043fbfbb463),
        (25, 15, 0x1a97a092de664588),
        (5, 5, 0x11ef2f52588c2707),
        (-20, 0, 0xc2fa909ec52f603f),
        (0, -20, 0xc2fa909ec52f603f),
        (29, 19, 0xc2fab09ec52f969f),
        (-11, -8, 0xd0dda5a02925e85c),
        (18, 11, 0x83b8e68beec205ba),
    ];
    #[rustfmt::skip]
    const PASTE_IMAGE: &[(i32, i32, u64)] = &[
        (-3, -2, 0x21e606848fe505e0),
        (25, 15, 0x9501fcc590725287),
        (5, 5, 0x93fb7d0c5dd80d46),
        (-20, 0, 0xc2fa909ec52f603f),
        (0, -20, 0xc2fa909ec52f603f),
        (29, 19, 0xc2faa89ec52f8907),
        (-11, -8, 0xeb474ca6111b6960),
        (18, 11, 0xdf8a3dffe50dfd58),
    ];
    #[rustfmt::skip]
    const PASTE_PLAIN: &[(i32, i32, u64)] = &[
        (-3, -2, 0x2ac02d3f24000769),
        (25, 15, 0xc922d5dd953f69ce),
        (5, 5, 0xa95e23bc712cf079),
        (-20, 0, 0xc2fa909ec52f603f),
        (0, -20, 0xc2fa909ec52f603f),
        (29, 19, 0xc2faa69ec52f85a1),
        (-11, -8, 0xe00c46e7daf48a28),
        (18, 11, 0x46c179777687fc8c),
    ];
    #[rustfmt::skip]
    const CROP: &[((i32, i32, i32, i32), u32, u32, u64)] = &[
        ((-5, -3, 10, 7), 15, 10, 0x99c957ec6df0956d),
        ((20, 10, 40, 30), 20, 20, 0x25191647ff8c3e15),
        ((3, 3, 3, 3), 0, 0, 0xcbf29ce484222325),
        ((0, 0, 30, 20), 30, 20, 0xc2fa909ec52f603f),
        ((-30, -30, -1, -1), 29, 29, 0x50232e49adab1cbf),
    ];
    #[rustfmt::skip]
    const OFFSET: &[(i32, i32, u64)] = &[
        (0, 0, 0xc2fa909ec52f603f),
        (3, 0, 0x21563d35aba50cab),
        (0, -4, 0x4327499c62e62437),
        (-7, 25, 0x11e9de1926786ae7),
        (35, -33, 0x0e38a85a452bc3cf),
    ];
    #[rustfmt::skip]
    const CHOPS: &[(&str, u64)] = &[
        ("multiply", 0xb0e7f578b1df79e1),
        ("subtract", 0x76a7cc924742c8e6),
        ("add", 0x65c5fe8e02768b1d),
        ("darker", 0x6735d0145a5bafe2),
        ("lighter", 0xedcc7b7fef410272),
        ("difference", 0x0e9177cba5339c1b),
        ("invert", 0x74e391743c73146f),
        ("composite", 0x684fdcac07a82e51),
        ("blend", 0x7772b38e7fc99263),
    ];
}
