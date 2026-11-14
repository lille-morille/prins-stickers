//! Clas Ohlson M110 label printer: image preparation, the byte stream, and printing over BLE.
//!
//! A port of `printer/m110.py` and the printing parts of `printer/server.py`. The Python is the
//! specification: the same commands, the same bytes, the same label geometry.
//!
//! Protocol (Phomemo-style ESC/POS raster, confirmed by real prints): write to FF02, replies on
//! FF03. The label is 312 px wide inside a 384 px head row (the last 312 columns land on the
//! label), 20 blank rows are sent first, and a print is six writes:
//!
//! 1. `1b 4e 0d <speed>`  2. `1b 4e 04 <density>`  3. `1f 11 0a` (gap labels)
//! 4. `1d 76 30 00 <row_bytes lo,hi> <rows lo,hi>` + bits (1 = black, MSB first)
//! 5. `1f f0 05 00`  6. `1f f0 03 00`
//!
//! Binary-compatible with Python means: [`encode`] gives the bytes `M110.print_image` would send.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Peripheral as _, ScanFilter, WriteType,
};
use futures::StreamExt;
use image::{GrayImage, Luma};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Notify;

use crate::ble::{self, Found, NOTIFY_UUID, WRITE_UUID};
use crate::config::PrinterRef;

/// Raster row width in bytes (8 px each): the print head is 384 px.
pub const WIDTH_BYTES: u32 = 48;
/// Printable label width in pixels (the grid width): the last 312 of the 384 head columns.
pub const LABEL_PX: u32 = 312;
/// Blank leading rows: the first ~18 rows printed fall before the label.
pub const TOP_PAD_PX: u32 = 20;
pub const DENSITY: u8 = 0x0A;
pub const SPEED: u8 = 0x05;
/// Media type: gap labels (observed to work with the roll in use).
pub const MEDIA_GAP_LABEL: u8 = 0x0A;
/// The reply that (probably) means "print finished".
pub const DONE_REPLY: [u8; 3] = [0x1a, 0x0f, 0x0c];

/// The grid a note is drawn on (`GRID_WIDTH` x `GRID_HEIGHT`), and the tallest print accepted.
pub const GRID_WIDTH: u32 = LABEL_PX;
pub const GRID_HEIGHT: u32 = 216;
/// A 240-row print was fed correctly during calibration; more would spill past the gap.
pub const MAX_HEIGHT: u32 = 240;

/// Bytes per Bluetooth write. Python uses `max(20, mtu - 3)` and the printer reports an MTU of 200
/// (so 197), but btleplug does not give the negotiated MTU on every platform. 180 fits an MTU of
/// 183 or more, which every phone and Mac negotiates, and stays under what CoreBluetooth accepts
/// for a write without response (about 182). It is fixed on purpose: guessing from a platform
/// default of 23 would make a print take half a minute.
pub const CHUNK_BYTES: usize = 180;
/// Pause after each write, as in Python.
pub const WRITE_PAUSE: Duration = Duration::from_millis(50);
/// How long to look for the printer before giving up.
pub const FIND_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the "done" reply. Missing it never fails a print.
pub const DONE_WAIT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors a caller may want to tell apart (an HTTP front end maps `NotFound` to 503 and the
/// others to 400). They travel inside `anyhow::Error`; use `downcast_ref::<M110Error>()`.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum M110Error {
    #[error("printer {0:?} not found (is it on and not connected to the phone?)")]
    NotFound(String),
    #[error("image is {width}x{height}; max is {GRID_WIDTH}x{MAX_HEIGHT}")]
    TooLarge { width: u32, height: u32 },
    #[error("body is not a readable image: {0}")]
    NotAnImage(String),
    #[error("image is empty ({width}x{height})")]
    Empty { width: u32, height: u32 },
}

// ---------------------------------------------------------------------------------------------
// Image preparation

fn round_half_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        (r - x.signum()) as i64
    } else {
        r as i64
    }
}

/// Pillow's `Image.resize(size, Image.LANCZOS)` for 8-bit gray: two separable passes with a
/// Lanczos-3 kernel stretched by the shrink factor, 22-bit fixed-point coefficients. Written out
/// here (not `image::imageops`) so the prepared bytes match Python's.
fn lanczos_resize(src: &GrayImage, out_w: u32, out_h: u32) -> GrayImage {
    const PRECISION_BITS: i32 = 32 - 8 - 2;

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

    /// For each output index: first input index and the fixed-point weights.
    fn coeffs(in_size: u32, out_size: u32) -> Vec<(usize, Vec<i64>)> {
        let scale = in_size as f64 / out_size as f64;
        let filterscale = scale.max(1.0);
        let support = 3.0 * filterscale;
        let ss = 1.0 / filterscale;
        (0..out_size)
            .map(|xx| {
                let center = (xx as f64 + 0.5) * scale;
                let xmin = ((center - support + 0.5) as i64).max(0);
                let xmax = (((center + support + 0.5) as i64).min(in_size as i64) - xmin).max(0);
                let mut k: Vec<f64> = (0..xmax)
                    .map(|x| lanczos((x as f64 + xmin as f64 - center + 0.5) * ss))
                    .collect();
                let total: f64 = k.iter().sum();
                if total != 0.0 {
                    k.iter_mut().for_each(|w| *w /= total);
                }
                let fixed = k
                    .iter()
                    .map(|&w| {
                        let scaled = w * (1i64 << PRECISION_BITS) as f64;
                        (if w < 0.0 { -0.5 + scaled } else { 0.5 + scaled }) as i64
                    })
                    .collect();
                (xmin as usize, fixed)
            })
            .collect()
    }

    fn clip8(v: i64) -> u8 {
        (v >> PRECISION_BITS).clamp(0, 255) as u8
    }

    let (in_w, in_h) = src.dimensions();
    if in_w == 0 || in_h == 0 || out_w == 0 || out_h == 0 {
        return GrayImage::from_pixel(out_w, out_h, Luma([255]));
    }
    let (in_w_us, out_w_us, out_h_us) = (in_w as usize, out_w as usize, out_h as usize);
    let half = 1i64 << (PRECISION_BITS - 1);

    // Horizontal pass.
    let mut mid = vec![0u8; out_w_us * in_h as usize];
    if in_w == out_w {
        mid.copy_from_slice(src.as_raw());
    } else {
        let kx = coeffs(in_w, out_w);
        for y in 0..in_h as usize {
            let row = &src.as_raw()[y * in_w_us..(y + 1) * in_w_us];
            for (x, (xmin, k)) in kx.iter().enumerate() {
                let sum: i64 = k
                    .iter()
                    .enumerate()
                    .map(|(i, &w)| row[xmin + i] as i64 * w)
                    .sum();
                mid[y * out_w_us + x] = clip8(half + sum);
            }
        }
    }
    // Vertical pass.
    if in_h == out_h {
        return GrayImage::from_raw(out_w, out_h, mid)
            .unwrap_or_else(|| GrayImage::new(out_w, out_h));
    }
    let ky = coeffs(in_h, out_h);
    let mut out = vec![0u8; out_w_us * out_h_us];
    for (y, (ymin, k)) in ky.iter().enumerate() {
        for x in 0..out_w_us {
            let sum: i64 = k
                .iter()
                .enumerate()
                .map(|(i, &w)| mid[(ymin + i) * out_w_us + x] as i64 * w)
                .sum();
            out[y * out_w_us + x] = clip8(half + sum);
        }
    }
    GrayImage::from_raw(out_w, out_h, out).unwrap_or_else(|| GrayImage::new(out_w, out_h))
}

/// Pillow's `convert("1")` from "L": Floyd-Steinberg error diffusion, `> 128` is white. Pixels that
/// are already 0 or 255 come out unchanged. The result holds only 0 and 255.
pub fn dither_floyd_steinberg(gray: &GrayImage) -> GrayImage {
    let (w, h) = gray.dimensions();
    let mut out = GrayImage::new(w, h);
    let mut errors = vec![0i32; w as usize + 1];
    for y in 0..h {
        let (mut l, mut l0, mut l1) = (0i32, 0i32, 0i32);
        for x in 0..w {
            let xi = x as usize;
            let v = (gray.get_pixel(x, y).0[0] as i32 + (l + errors[xi + 1]) / 16).clamp(0, 255);
            let o = if v > 128 { 255 } else { 0 };
            out.put_pixel(x, y, Luma([o as u8]));
            l = v - o;
            errors[xi] = l0 + 3 * l;
            l0 = l1 + 5 * l;
            l1 = l;
            l *= 7;
        }
        errors[w as usize] = l0;
    }
    out
}

/// The server's `to_bitmap`: `convert("1")` with dithering, or a hard threshold (`v >= threshold`
/// is white). The result holds only 0 (black) and 255 (white).
pub fn to_bitmap(gray: &GrayImage, threshold: u8, dither: bool) -> GrayImage {
    if dither {
        return dither_floyd_steinberg(gray);
    }
    let mut out = gray.clone();
    out.pixels_mut()
        .for_each(|p| p.0[0] = if p.0[0] >= threshold { 255 } else { 0 });
    out
}

/// Number of black pixels (value 0 after binarising: anything under 128).
pub fn black_pixels(bitmap: &GrayImage) -> usize {
    bitmap.pixels().filter(|p| p.0[0] < 128).count()
}

/// `M110.prepare`: fit into the label width (LANCZOS shrink when wider than 312 px), centre it in
/// the label columns, pad to the 384 px head width with 20 blank rows on top, and make it one-bit.
///
/// The result is `384 x (height + 20)`, only 0 (black) and 255 (white). An image that is 312 wide
/// lands at x = 72, a narrower one at `72 + (312 - w) / 2`.
pub fn prepare(img: &GrayImage) -> GrayImage {
    let head_px = WIDTH_BYTES * 8;
    let (w, h) = img.dimensions();
    let resized;
    let img = if w > LABEL_PX {
        let new_h = round_half_even(h as f64 * LABEL_PX as f64 / w as f64).max(1) as u32;
        resized = lanczos_resize(img, LABEL_PX, new_h);
        &resized
    } else {
        img
    };
    let (w, h) = img.dimensions();
    let mut canvas = GrayImage::from_pixel(head_px, h + TOP_PAD_PX, Luma([255]));
    let x0 = head_px - LABEL_PX + (LABEL_PX - w) / 2;
    for y in 0..h {
        for x in 0..w {
            canvas.put_pixel(x0 + x, TOP_PAD_PX + y, *img.get_pixel(x, y));
        }
    }
    dither_floyd_steinberg(&canvas)
}

// ---------------------------------------------------------------------------------------------
// The byte stream

/// The six writes of one print, from an already prepared image (see [`prepare`]). Rows beyond
/// 65535 are cut, because the header holds the row count in two bytes.
pub fn commands(prepared: &GrayImage) -> Vec<Vec<u8>> {
    let row_bytes = prepared.width().div_ceil(8) as usize;
    let rows = prepared.height().min(0xFFFF) as usize;
    let mut data = Vec::with_capacity(row_bytes * rows);
    for y in 0..rows as u32 {
        for bx in 0..row_bytes as u32 {
            let mut byte = 0u8;
            for bit in 0..8 {
                let x = bx * 8 + bit;
                // 1 = black, MSB first; the padding bits of a short row are white.
                if x < prepared.width() && prepared.get_pixel(x, y).0[0] < 128 {
                    byte |= 0x80 >> bit;
                }
            }
            data.push(byte);
        }
    }
    let mut raster = vec![
        0x1D,
        0x76,
        0x30,
        0x00,
        (row_bytes & 0xFF) as u8,
        (row_bytes >> 8) as u8,
        (rows & 0xFF) as u8,
        (rows >> 8) as u8,
    ];
    raster.extend(data);
    vec![
        vec![0x1B, 0x4E, 0x0D, SPEED],
        vec![0x1B, 0x4E, 0x04, DENSITY],
        vec![0x1F, 0x11, MEDIA_GAP_LABEL],
        raster,
        vec![0x1F, 0xF0, 0x05, 0x00],
        vec![0x1F, 0xF0, 0x03, 0x00],
    ]
}

/// The exact bytes of an already prepared image: the six writes of [`commands`], joined.
pub fn encode_prepared(prepared: &GrayImage) -> Vec<u8> {
    commands(prepared).concat()
}

/// The exact bytes `M110.print_image(img)` writes for `img`: [`prepare`] first, then the five
/// commands and the raster. Pass the original image (a note), not a prepared one.
pub fn encode(img: &GrayImage) -> Vec<u8> {
    encode_prepared(&prepare(img))
}

// ---------------------------------------------------------------------------------------------
// Dry run and the server's image rules

/// What a print would send, without touching Bluetooth.
#[derive(Debug, Clone)]
pub struct DryRun {
    /// The 384 px wide, padded, one-bit image that would be rasterised.
    pub prepared: GrayImage,
    /// Total bytes that would be written (commands and raster).
    pub bytes: usize,
    /// Rows in the raster, including the 20 blank rows on top.
    pub rows: usize,
}

/// Tunables of a print. The defaults are the Python ones.
#[derive(Debug, Clone)]
pub struct PrintOptions {
    /// How long to wait for the done reply. A timeout is not an error.
    pub wait: Duration,
    /// How long to look for the printer.
    pub find_timeout: Duration,
    /// Pause after each write.
    pub pause: Duration,
    /// Gray values below this print black (the server's `threshold`).
    pub threshold: u8,
    /// Floyd-Steinberg instead of the threshold.
    pub dither: bool,
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions {
            wait: DONE_WAIT,
            find_timeout: FIND_TIMEOUT,
            pause: WRITE_PAUSE,
            threshold: 128,
            dither: false,
        }
    }
}

fn prepared_for(image: &GrayImage, opts: &PrintOptions) -> GrayImage {
    prepare(&to_bitmap(image, opts.threshold, opts.dither))
}

/// Refuse a print that is wider than the grid or taller than 240 rows, or empty (the server's
/// check, with its message).
pub fn check_size(width: u32, height: u32) -> Result<(), M110Error> {
    if width == 0 || height == 0 {
        return Err(M110Error::Empty { width, height });
    }
    if width > GRID_WIDTH || height > MAX_HEIGHT {
        return Err(M110Error::TooLarge { width, height });
    }
    Ok(())
}

/// Report what [`print`] would send for `image` (default options).
pub fn dry_run(image: &GrayImage) -> DryRun {
    dry_run_with(image, &PrintOptions::default())
}

/// [`dry_run`] with the threshold and dither of `opts`.
pub fn dry_run_with(image: &GrayImage, opts: &PrintOptions) -> DryRun {
    let prepared = prepared_for(image, opts);
    DryRun {
        bytes: encode_prepared(&prepared).len(),
        rows: prepared.height() as usize,
        prepared,
    }
}

/// Gray of an RGBA pixel composited on white (transparent pixels print white). Pillow's "L" is
/// `(19595 R + 38470 G + 7471 B + 0x8000) >> 16`.
fn flatten_pixel(r: u8, g: u8, b: u8, a: u8) -> u8 {
    let blend = |c: u8| (c as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255;
    let (r, g, b) = (blend(r), blend(g), blend(b));
    ((19595 * r + 38470 * g + 7471 * b + 0x8000) >> 16) as u8
}

/// `/print/png`: decode a picture, refuse it when it is wider than 312 or taller than 240 (with
/// the Python message), put it on white and make it one-bit with the threshold or dithering. The
/// result keeps the picture's size; narrower pictures are centred later, by [`prepare`].
pub fn bitmap_from_image_bytes(
    body: &[u8],
    threshold: u8,
    dither: bool,
) -> Result<GrayImage, M110Error> {
    let img = image::load_from_memory(body).map_err(|e| M110Error::NotAnImage(e.to_string()))?;
    check_size(img.width(), img.height())?;
    let rgba = img.to_rgba8();
    let mut gray = GrayImage::new(rgba.width(), rgba.height());
    for (x, y, p) in rgba.enumerate_pixels() {
        gray.put_pixel(x, y, Luma([flatten_pixel(p.0[0], p.0[1], p.0[2], p.0[3])]));
    }
    Ok(to_bitmap(&gray, threshold, dither))
}

// ---------------------------------------------------------------------------------------------
// Printing over Bluetooth

/// Where the time went, in milliseconds.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    pub find_ms: u64,
    pub connect_ms: u64,
    pub send_ms: u64,
    pub wait_ms: u64,
    pub total_ms: u64,
}

/// What a print did.
#[derive(Debug, Clone, Serialize)]
pub struct PrintReport {
    /// The device that was used.
    pub printer_id: String,
    pub printer_name: Option<String>,
    /// Every reply from the printer, as hex bytes like `01 01`, in order.
    pub replies: Vec<String>,
    /// True when the done reply (`1a 0f 0c`) arrived. False is not a failure.
    pub done: bool,
    pub bytes: usize,
    pub rows: usize,
    pub writes: usize,
    pub timings: Timings,
}

/// One print at a time per process, like the server's `print_lock`.
static PRINT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn hex_spaced(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ms(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

/// Scan until the saved printer shows up (checked every 200 ms), or `timeout` passes.
async fn find_printer(
    adapter: &btleplug::platform::Adapter,
    printer: &PrinterRef,
    timeout: Duration,
) -> Result<(Found, btleplug::platform::Peripheral)> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("Cannot start the Bluetooth scan")?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        let seen = ble::snapshot(adapter).await;
        match seen {
            Ok(seen) => {
                let found: Vec<Found> = seen.iter().map(|(f, _)| f.clone()).collect();
                if let Some(i) = ble::resolve_index(&found, printer) {
                    break Ok(seen.into_iter().nth(i));
                }
            }
            Err(e) => break Err(e),
        }
        if Instant::now() >= deadline {
            break Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = adapter.stop_scan().await;
    match result? {
        Some(hit) => Ok(hit),
        None => Err(M110Error::NotFound(
            printer.name.clone().unwrap_or_else(|| printer.id.clone()),
        )
        .into()),
    }
}

fn find_char(p: &btleplug::platform::Peripheral, uuid: uuid::Uuid) -> Result<Characteristic> {
    p.characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid)
        .ok_or_else(|| {
            anyhow!("The device has no characteristic {uuid}. It does not look like an M110: check `prins printer show`.")
        })
}

/// Print `image` on the saved printer: scan for it (up to `opts.find_timeout`), connect, subscribe
/// to the replies, write the six commands in [`CHUNK_BYTES`] pieces with a pause after each,
/// wait up to `opts.wait` for the done reply, and disconnect. Binarises with `opts` first.
pub async fn print_async(
    image: &GrayImage,
    printer: &PrinterRef,
    opts: PrintOptions,
) -> Result<PrintReport> {
    if image.width() == 0 || image.height() == 0 {
        bail!(M110Error::Empty {
            width: image.width(),
            height: image.height()
        });
    }
    let prepared = prepared_for(image, &opts);
    if prepared.height() > 0xFFFF {
        bail!(
            "The image is too tall to print ({} rows; the limit is 65535).",
            prepared.height()
        );
    }
    let commands = commands(&prepared);
    let total_bytes: usize = commands.iter().map(Vec::len).sum();
    let rows = prepared.height() as usize;

    let _guard = PRINT_LOCK.lock().await;
    let started = Instant::now();
    let adapter = ble::adapter().await?;
    let (found, peripheral) = find_printer(&adapter, printer, opts.find_timeout).await?;
    let find_ms = ms(started.elapsed());

    let connect_started = Instant::now();
    peripheral
        .connect_with_timeout(CONNECT_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "Cannot connect to {}. Is it on and not connected to the phone app?",
                found.label()
            )
        })?;

    let outcome = print_connected(&peripheral, &commands, &opts, connect_started).await;
    let _ = peripheral.disconnect().await;
    let (replies, done, writes, connect_ms, send_ms, wait_ms) = outcome?;

    Ok(PrintReport {
        printer_id: found.id,
        printer_name: found.name,
        replies: replies.iter().map(|r| hex_spaced(r)).collect(),
        done,
        bytes: total_bytes,
        rows,
        writes,
        timings: Timings {
            find_ms,
            connect_ms,
            send_ms,
            wait_ms,
            total_ms: ms(started.elapsed()),
        },
    })
}

type Connected = (Vec<Vec<u8>>, bool, usize, u64, u64, u64);

async fn print_connected(
    peripheral: &btleplug::platform::Peripheral,
    commands: &[Vec<u8>],
    opts: &PrintOptions,
    connect_started: Instant,
) -> Result<Connected> {
    peripheral
        .discover_services_with_timeout(DISCOVER_TIMEOUT)
        .await
        .context("Cannot read the printer's services")?;
    let write_char = find_char(peripheral, WRITE_UUID)?;
    let notify_char = find_char(peripheral, NOTIFY_UUID)?;
    if !write_char
        .properties
        .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
    {
        bail!("The printer's write characteristic does not accept writes without response. It does not look like an M110.");
    }

    // Collect replies from the first write on, like Python's notification callback.
    let replies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let done = Arc::new(Notify::new());
    let mut stream = peripheral
        .notifications()
        .await
        .context("Cannot listen for the printer's replies")?;
    let collector = {
        let (replies, done) = (replies.clone(), done.clone());
        tokio::spawn(async move {
            while let Some(n) = stream.next().await {
                if n.uuid != NOTIFY_UUID {
                    continue;
                }
                let finished = n.value == DONE_REPLY;
                if let Ok(mut r) = replies.lock() {
                    r.push(n.value);
                }
                if finished {
                    done.notify_one();
                }
            }
        })
    };
    let result = async {
        peripheral
            .subscribe(&notify_char)
            .await
            .context("Cannot subscribe to the printer's replies")?;
        let connect_ms = ms(connect_started.elapsed());

        let send_started = Instant::now();
        let mut writes = 0usize;
        for payload in commands {
            for piece in payload.chunks(CHUNK_BYTES) {
                peripheral
                    .write(&write_char, piece, WriteType::WithoutResponse)
                    .await
                    .context("Writing to the printer failed")?;
                writes += 1;
                tokio::time::sleep(opts.pause).await;
            }
        }
        let send_ms = ms(send_started.elapsed());

        let wait_started = Instant::now();
        // The done reply is only inferred: waiting for it must never fail the print.
        let got_done = tokio::time::timeout(opts.wait, done.notified())
            .await
            .is_ok();
        Ok::<_, anyhow::Error>((
            got_done,
            writes,
            connect_ms,
            send_ms,
            ms(wait_started.elapsed()),
        ))
    }
    .await;
    let _ = peripheral.unsubscribe(&notify_char).await;
    collector.abort();
    let (got_done, writes, connect_ms, send_ms, wait_ms) = result?;
    let replies = replies.lock().map(|r| r.clone()).unwrap_or_default();
    Ok((replies, got_done, writes, connect_ms, send_ms, wait_ms))
}

/// Blocking version of [`print_async`]. A printer that is not found is an
/// [`M110Error::NotFound`] inside the error.
pub fn print(image: &GrayImage, printer: &PrinterRef, opts: PrintOptions) -> Result<PrintReport> {
    ble::block_on(print_async(image, printer, opts))
}

// ---------------------------------------------------------------------------------------------
// Calibration and test images

/// A 5x7 pixel letter, one row per entry with the left pixel in bit 4. Lower case draws as upper.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        _ => [0; 7],
    }
}

fn put(img: &mut GrayImage, x: i64, y: i64, value: u8) {
    if x >= 0 && y >= 0 && (x as u64) < img.width() as u64 && (y as u64) < img.height() as u64 {
        img.put_pixel(x as u32, y as u32, Luma([value]));
    }
}

fn fill_rect(img: &mut GrayImage, x0: i64, y0: i64, x1: i64, y1: i64, value: u8) {
    for y in y0..=y1 {
        for x in x0..=x1 {
            put(img, x, y, value);
        }
    }
}

/// Black outline of `width` px inside the inclusive box.
fn frame(img: &mut GrayImage, x0: i64, y0: i64, x1: i64, y1: i64, width: i64) {
    fill_rect(img, x0, y0, x1, y0 + width - 1, 0);
    fill_rect(img, x0, y1 - width + 1, x1, y1, 0);
    fill_rect(img, x0, y0, x0 + width - 1, y1, 0);
    fill_rect(img, x1 - width + 1, y0, x1, y1, 0);
}

/// Black text in the built-in 5x7 letters, each pixel `scale` px square, 1 px gap scaled.
fn draw_text(img: &mut GrayImage, x: i64, y: i64, text: &str, scale: i64) {
    for (i, c) in text.chars().enumerate() {
        let gx = x + i as i64 * 6 * scale;
        for (row, bits) in glyph(c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0x10 >> col) != 0 {
                    let (px, py) = (gx + col * scale, y + row as i64 * scale);
                    fill_rect(img, px, py, px + scale - 1, py + scale - 1, 0);
                }
            }
        }
    }
}

fn text_width(text: &str, scale: i64) -> i64 {
    (text.chars().count() as i64 * 6 - 1).max(0) * scale
}

/// Port of Python's `calibration_image`: a full-width frame with ticks every 8 px (long every 64)
/// and L/R markers, `width_bytes * 8` wide and 240 high, 0 = black. Letters are drawn with a
/// built-in 5x7 font at 4x (Python uses Pillow's default font at 28 px), so they look blockier.
///
/// Printing it goes through [`prepare`], which shrinks anything wider than 312 px to 312, so the
/// 384 px frame comes out scaled. That is what the Python does too.
pub fn calibration_image(width_bytes: u32) -> GrayImage {
    let (w, height) = (width_bytes.saturating_mul(8).max(8), 240u32);
    let mut img = GrayImage::from_pixel(w, height, Luma([255]));
    let (wi, hi) = (w as i64, height as i64);
    frame(&mut img, 0, 0, wi - 1, hi - 1, 2);
    for x in (0..wi).step_by(8) {
        fill_rect(&mut img, x, 0, x, if x % 64 == 0 { 24 } else { 10 }, 0);
    }
    draw_text(&mut img, 10, 60, "L", 4);
    draw_text(&mut img, wi - 36, 60, "R", 4);
    let label = format!("{w}px");
    draw_text(&mut img, wi / 2 - text_width(&label, 4) / 2, 110, &label, 4);
    img
}

/// A small bordered pattern for checking that a print lands on the label: a 312x216 note with a
/// 3 px frame on all four edges (so a shifted print shows which edge is cut), corner squares, a
/// cross through the centre and some text. 0 = black.
pub fn test_pattern() -> GrayImage {
    let (w, h) = (GRID_WIDTH as i64, GRID_HEIGHT as i64);
    let mut img = GrayImage::from_pixel(GRID_WIDTH, GRID_HEIGHT, Luma([255]));
    frame(&mut img, 0, 0, w - 1, h - 1, 3);
    frame(&mut img, 10, 10, w - 11, h - 11, 1);
    for (cx, cy) in [(16, 16), (w - 30, 16), (16, h - 30), (w - 30, h - 30)] {
        fill_rect(&mut img, cx, cy, cx + 13, cy + 13, 0);
    }
    for x in (40..w - 40).step_by(2) {
        put(&mut img, x, h / 2, 0);
    }
    for y in (40..h - 40).step_by(2) {
        put(&mut img, w / 2, y, 0);
    }
    for (text, y, scale) in [
        ("PRINS", 56, 5),
        ("TEST LABEL", 110, 3),
        ("312X216 PX", 150, 2),
    ] {
        draw_text(
            &mut img,
            w / 2 - text_width(text, scale) / 2,
            y,
            text,
            scale,
        );
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The images the Python reference was run on (see `python_reference` below).
    fn pattern(w: u32, h: u32, kind: &str) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            let (x, y) = (x as i64, y as i64);
            let v = match kind {
                // Black or white.
                "stripes" => {
                    if (x * 7 + y * 13) % 5 < 2 {
                        0
                    } else {
                        255
                    }
                }
                "frame" => {
                    if x < 2 || y < 2 || x >= w as i64 - 2 || y >= h as i64 - 2 || (x + y) % 11 == 0
                    {
                        0
                    } else {
                        255
                    }
                }
                // Gray ramps: these dither.
                _ => ((x * 255) / (w as i64 - 1).max(1) + (y % 7) * 9) % 256,
            };
            Luma([v as u8])
        })
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn the_five_commands_and_the_raster_header_for_a_tiny_image() {
        let img = GrayImage::from_pixel(8, 1, Luma([0])); // one black row, 8 px
        let bytes = encode(&img);
        let head = hex::encode(&bytes[..4 + 4 + 3]);
        assert_eq!(head, "1b4e0d051b4e040a1f110a");
        // Raster header: 48 bytes per row, 21 rows (20 blank + 1).
        assert_eq!(hex::encode(&bytes[11..19]), "1d76300030001500");
        assert_eq!(bytes.len(), 11 + 8 + 48 * 21 + 8);
        assert_eq!(hex::encode(&bytes[bytes.len() - 8..]), "1ff005001ff00300");
        // The 8 black pixels land at x = 72 + (312 - 8) / 2 = 224 = byte 28, in row 20.
        let row20 = &bytes[19 + 48 * 20..19 + 48 * 21];
        assert_eq!(row20[28], 0xFF);
        assert_eq!(row20.iter().filter(|&&b| b != 0).count(), 1);
        // The blank rows send zeros (white), not ones.
        assert!(bytes[19..19 + 48 * 20].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_312_wide_image_lands_in_the_last_312_columns_below_20_blank_rows() {
        let img = GrayImage::from_pixel(312, 216, Luma([0]));
        let p = prepare(&img);
        assert_eq!(p.dimensions(), (384, 236));
        for y in 0..236 {
            for x in 0..384 {
                let black = y >= 20 && x >= 72;
                assert_eq!(p.get_pixel(x, y).0[0] == 0, black, "({x},{y})");
            }
        }
    }

    #[test]
    fn a_narrow_image_is_centred_in_the_label_columns() {
        let img = GrayImage::from_pixel(101, 3, Luma([0]));
        let p = prepare(&img);
        let x0 = 72 + (312 - 101) / 2;
        assert_eq!(x0, 177);
        let cols: Vec<u32> = (0..384).filter(|&x| p.get_pixel(x, 20).0[0] == 0).collect();
        assert_eq!(
            (cols[0], *cols.last().unwrap(), cols.len()),
            (x0, x0 + 100, 101)
        );
        assert!((0..20).all(|y| (0..384).all(|x| p.get_pixel(x, y).0[0] == 255)));
    }

    #[test]
    fn a_wide_image_is_shrunk_to_312() {
        let img = GrayImage::from_pixel(624, 300, Luma([0]));
        let p = prepare(&img);
        assert_eq!(p.dimensions(), (384, 150 + 20));
        assert_eq!(p.get_pixel(71, 40).0[0], 255);
        assert_eq!(p.get_pixel(72, 40).0[0], 0);
        assert_eq!(p.get_pixel(383, 169).0[0], 0);
    }

    #[test]
    fn prepare_only_makes_black_and_white() {
        let p = prepare(&pattern(300, 100, "ramp"));
        assert!(p.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
    }

    #[test]
    fn odd_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 500), (700, 1), (313, 1), (384, 240), (5000, 3)] {
            let p = prepare(&GrayImage::from_pixel(w, h, Luma([100])));
            assert_eq!(p.width(), 384);
            assert!(p.height() >= 21);
        }
        let empty = prepare(&GrayImage::new(0, 0));
        assert_eq!(empty.dimensions(), (384, 20));
        let _ = encode(&GrayImage::new(0, 0));
    }

    /// `(kind, width, height, stream length, sha256)`, produced by the real `printer/m110.py`
    /// (`M110().prepare`, then the same payload rebuild as `print_image`) with Pillow 13:
    /// the script is described in the test below.
    const REFERENCE: &[(&str, u32, u32, usize, &str)] = &[
        (
            "stripes",
            312,
            216,
            11355,
            "7c5b8f63394fc87372e9b4e9f75ac68fc0ea185547d8051a9926ab27321d2b8d",
        ),
        (
            "frame",
            100,
            50,
            3387,
            "9cb413bf1a343fe58eaf6aaf6970bffcb48c43354a31067706d2af802b8d8c44",
        ),
        (
            "frame",
            312,
            240,
            12507,
            "6aaa3e16c92adb857e1105c781beda819a27e349fdb31d4f18b5cb21d3a09863",
        ),
        (
            "ramp",
            624,
            300,
            8187,
            "bd918958aa0c8ea6c2e627b579a392ba1731f33777f12908e5a0cd68645e712b",
        ),
        (
            "stripes",
            400,
            240,
            9963,
            "67cc7a4df4d922abc6197f2faaabe3873c8db9964352825eb09771cf2f03323c",
        ),
        (
            "ramp",
            312,
            216,
            11355,
            "75842e7ad3ec4041a18cc2818475669c5126dc2a0cfa470bd3787f7d3e8dabe7",
        ),
        (
            "ramp",
            313,
            217,
            11355,
            "5875ef3e4738a0778fe8d3eb99bb839cbb922226a1e76f414c2a8cad2103c0ce",
        ),
        (
            "stripes",
            1,
            1,
            1035,
            "8c1c0f575ea9ec0070bef59056b01e0a7ab919328d3c45b7b27ad54741b20bb1",
        ),
        (
            "stripes",
            8,
            3,
            1131,
            "c712509fb8ab178a7e572fcca68bb9bc952781f69920d3ddda5a702bb1860ed7",
        ),
        (
            "ramp",
            500,
            37,
            2091,
            "58e1209d6b1b87278722dc61dbb18d64d173423e6568a68ee6fdd448a42933a2",
        ),
        (
            "ramp",
            384,
            240,
            10347,
            "fb39a85d26c7f78c1ef852155ec1cdf1e470dfd2cc293dd11ff544cc68e100ae",
        ),
        (
            "frame",
            311,
            215,
            11307,
            "8e641b7db26bbb48f7ece19d1ab04f08edafa894963709df59e23720a82cd5e3",
        ),
        (
            "ramp",
            1000,
            20,
            1275,
            "91459e8c0353b9cbe2cf34c8cae279647e8b966bd121a3556521c8cb2a181886",
        ),
    ];

    #[test]
    fn encode_matches_the_python_driver_byte_for_byte() {
        for (kind, w, h, len, digest) in REFERENCE {
            let bytes = encode(&pattern(*w, *h, kind));
            assert_eq!(bytes.len(), *len, "{kind} {w}x{h}");
            assert_eq!(sha(&bytes), *digest, "{kind} {w}x{h}");
        }
    }

    #[test]
    fn bitmap_rules_of_the_server() {
        let gray = GrayImage::from_fn(4, 1, |x, _| Luma([[0, 127, 128, 255][x as usize]]));
        let b = to_bitmap(&gray, 128, false);
        assert_eq!(b.as_raw(), &vec![0, 0, 255, 255]);
        let b = to_bitmap(&gray, 100, false);
        assert_eq!(b.as_raw(), &vec![0, 255, 255, 255]);
        assert_eq!(black_pixels(&to_bitmap(&gray, 128, false)), 2);
    }

    #[test]
    fn size_rules_use_the_python_message() {
        assert!(check_size(312, 240).is_ok());
        assert!(check_size(1, 1).is_ok());
        assert_eq!(
            check_size(313, 216).unwrap_err().to_string(),
            "image is 313x216; max is 312x240"
        );
        assert_eq!(
            check_size(312, 241).unwrap_err().to_string(),
            "image is 312x241; max is 312x240"
        );
        assert!(check_size(0, 5).is_err());
    }

    #[test]
    fn png_bodies_are_checked_and_flattened_on_white() {
        let mut rgba = image::RgbaImage::from_pixel(10, 4, image::Rgba([0, 0, 0, 0]));
        rgba.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        rgba.put_pixel(1, 0, image::Rgba([0, 0, 0, 40]));
        rgba.put_pixel(2, 0, image::Rgba([0, 0, 0, 220]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let bitmap = bitmap_from_image_bytes(&png, 128, false).unwrap();
        assert_eq!(bitmap.dimensions(), (10, 4));
        assert_eq!(bitmap.get_pixel(0, 0).0[0], 0); // opaque black
        assert_eq!(bitmap.get_pixel(1, 0).0[0], 255); // mostly transparent: white
        assert_eq!(bitmap.get_pixel(2, 0).0[0], 0); // mostly opaque: black
        assert_eq!(bitmap.get_pixel(5, 3).0[0], 255); // transparent: white

        let big = image::GrayImage::new(313, 10);
        let mut png = Vec::new();
        image::DynamicImage::ImageLuma8(big)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(
            bitmap_from_image_bytes(&png, 128, false).unwrap_err(),
            M110Error::TooLarge {
                width: 313,
                height: 10
            }
        );
        assert!(matches!(
            bitmap_from_image_bytes(b"not an image", 128, false),
            Err(M110Error::NotAnImage(_))
        ));
    }

    #[test]
    fn dry_run_counts_what_would_be_sent() {
        let d = dry_run(&test_pattern());
        assert_eq!(d.rows, 216 + 20);
        assert_eq!(d.prepared.dimensions(), (384, 236));
        assert_eq!(d.bytes, 4 + 4 + 3 + 8 + 48 * 236 + 4 + 4);
    }

    #[test]
    fn the_test_pattern_has_a_border_on_all_four_edges() {
        let t = test_pattern();
        assert_eq!(t.dimensions(), (312, 216));
        for x in [0, 100, 311] {
            assert_eq!((t.get_pixel(x, 0).0[0], t.get_pixel(x, 215).0[0]), (0, 0));
        }
        for y in [0, 100, 215] {
            assert_eq!((t.get_pixel(0, y).0[0], t.get_pixel(311, y).0[0]), (0, 0));
        }
        assert!(black_pixels(&t) > 1000);
    }

    #[test]
    fn the_calibration_image_has_ticks_and_a_frame() {
        let c = calibration_image(48);
        assert_eq!(c.dimensions(), (384, 240));
        assert_eq!(c.get_pixel(64, 24).0[0], 0); // long tick
        assert_eq!(c.get_pixel(72, 24).0[0], 255); // short tick ends at 10
        assert_eq!(c.get_pixel(72, 10).0[0], 0);
        assert_eq!(c.get_pixel(383, 239).0[0], 0);
        assert_eq!(c.get_pixel(204, 1).0[0], 0);
        assert_eq!(c.get_pixel(204, 2).0[0], 255);
        let tiny = calibration_image(0);
        assert!(tiny.width() >= 8);
    }

    #[test]
    fn write_chunks_stay_in_the_documented_range() {
        assert!((20..=180).contains(&CHUNK_BYTES));
    }

    #[test]
    fn round_half_even_matches_python() {
        assert_eq!(round_half_even(0.5), 0);
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(2.4), 2);
        assert_eq!(round_half_even(2.6), 3);
    }
}
