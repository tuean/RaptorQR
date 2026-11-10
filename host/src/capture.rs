//! Screen capture via xcap (CGWindowListCreateImage on macOS).
//!
//! Display `index` follows xcap's order with the primary display first, so
//! `index = 0` is the main screen. Every function works in **monitor-local
//! physical pixels**: `(0,0)` is that display's top-left corner, and a Retina
//! display captures at 2× its logical point size.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use xcap::Monitor;

/// How long a [`Monitor::all`] result is reused. Enumerating displays walks
/// CoreGraphics, and the capture loop used to do it once per display per tick.
const MONITOR_CACHE_TTL: Duration = Duration::from_secs(2);

thread_local! {
    static MONITORS: RefCell<Option<(Instant, Vec<Monitor>)>> = RefCell::new(None);
}

/// A rectangle in monitor-local pixels: (x, y, width, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl ScreenRect {
    pub fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// Keep only the part of `self` inside `bounds`.
    pub fn intersect(&self, bounds: &ScreenRect) -> ScreenRect {
        let x0 = self.x.max(bounds.x);
        let y0 = self.y.max(bounds.y);
        let x1 = (self.x + self.w).min(bounds.x + bounds.w);
        let y1 = (self.y + self.h).min(bounds.y + bounds.h);
        ScreenRect {
            x: x0,
            y: y0,
            w: x1.saturating_sub(x0),
            h: y1.saturating_sub(y0),
        }
    }
}

/// One connected display, in capture order (primary first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    /// Logical (point) size — what macOS window/coordinate APIs use.
    pub logical_w: u32,
    pub logical_h: u32,
    pub primary: bool,
}

/// Connected displays, primary first. `index` values match [`capture_display`].
pub fn display_list() -> Vec<DisplayInfo> {
    if std::env::var_os("RAPTORQR_FAKE_SCREEN").is_some() {
        // The fake screen is a single virtual display; asking CoreGraphics would
        // report the real ones (or nothing at all on a headless box).
        return vec![DisplayInfo {
            index: 0,
            name: "Fake screen".to_string(),
            logical_w: 0,
            logical_h: 0,
            primary: true,
        }];
    }

    monitors()
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| DisplayInfo {
            index,
            name: monitor
                .name()
                .unwrap_or_else(|_| format!("Display {}", index + 1)),
            logical_w: monitor.width().unwrap_or(0),
            logical_h: monitor.height().unwrap_or(0),
            primary: monitor.is_primary().unwrap_or(false),
        })
        .collect()
}

/// Cached monitor list, primary first, cloned out of the thread-local slot.
fn monitors() -> Vec<Monitor> {
    MONITORS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let stale = match slot.as_ref() {
            None => true,
            Some((at, list)) => at.elapsed() > MONITOR_CACHE_TTL || list.is_empty(),
        };
        if stale {
            let mut list = Monitor::all().unwrap_or_default();
            // The primary display is index 0 so the default preview is the main
            // screen.
            list.sort_by_key(|m| !m.is_primary().unwrap_or(false));
            *slot = Some((Instant::now(), list));
        }
        slot.as_ref()
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    })
}

fn display_at(index: usize) -> Option<Monitor> {
    monitors().into_iter().nth(index)
}

/// Logical size of display `index`, for display/diagnostics only.
pub fn logical_size(index: usize) -> Option<(u32, u32)> {
    let monitor = display_at(index)?;
    Some((monitor.width().ok()?, monitor.height().ok()?))
}

/// Capture display `index`, returning RGBA pixels + **physical** dimensions.
///
/// `RAPTORQR_FAKE_SCREEN=<dir|file>` replays PNG frames instead of grabbing the
/// screen (debug/demo hook: lets the whole receive path run without a display,
/// e.g. from the fixtures captured by `scripts/capture_singlefile_frames.mjs`).
pub fn capture_display(index: usize) -> Option<(Vec<u8>, u32, u32)> {
    if let Some(source) = std::env::var_os("RAPTORQR_FAKE_SCREEN") {
        // The fake screen is a single virtual display.
        return if index == 0 {
            replay_frame(std::path::Path::new(&source))
        } else {
            None
        };
    }

    let img = display_at(index)?.capture_image().ok()?;
    let w = img.width();
    let h = img.height();
    Some((img.into_raw(), w, h))
}

/// One freshly grabbed frame, still in the compositor's own buffer.
///
/// `xcap` turns a grab into an RGBA `Vec` with three full passes over the pixels
/// (provider bytes → row-stripped copy → BGRA→RGBA swap). The scan loop needs
/// luma, and only when the frame actually changed, so the frame is kept as the
/// borrowed provider buffer and converted on demand.
pub struct RawFrame {
    #[cfg(target_os = "macos")]
    provider: Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFData>>,
    owned: Option<Vec<u8>>,
    stride: usize,
    pub w: u32,
    pub h: u32,
    /// True for BGRA grabs (CoreGraphics), false for RGBA replays.
    bgra: bool,
}

impl RawFrame {
    fn bytes(&self) -> &[u8] {
        #[cfg(target_os = "macos")]
        if let Some(data) = &self.provider {
            // SAFETY: the provider buffer is immutable while the `CFData` is
            // alive, and it outlives this borrow.
            return unsafe { data.as_bytes_unchecked() };
        }
        self.owned.as_deref().unwrap_or(&[])
    }

    /// Cheap content hash of every 4096th byte — a static screen hashes the same
    /// every tick.
    pub fn fingerprint(&self) -> u64 {
        fingerprint_bytes(self.bytes())
    }

    /// Convert to a luma plane (BT.601), one pass over the pixels.
    pub fn to_luma(&self) -> Option<(Vec<u8>, u32, u32)> {
        let (w, h) = (self.w, self.h);
        if w == 0 || h == 0 {
            return None;
        }
        let bytes = self.bytes();
        let row_bytes = w as usize * 4;
        if self.stride < row_bytes || bytes.len() < self.stride * (h as usize - 1) + row_bytes {
            return None;
        }
        let (r_shift, b_shift) = if self.bgra { (16, 0) } else { (0, 16) };
        let mut luma = vec![0u8; (w as u64 * h as u64) as usize];
        for y in 0..h as usize {
            let src = &bytes[y * self.stride..y * self.stride + row_bytes];
            let dst = &mut luma[y * (w as usize)..(y + 1) * (w as usize)];
            for (pixel, out) in src.chunks_exact(4).zip(dst.iter_mut()) {
                let packed = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
                let r = (packed >> r_shift) & 0xff;
                let g = (packed >> 8) & 0xff;
                let b = (packed >> b_shift) & 0xff;
                // Same BT.601 weights as `rgba_to_luma`.
                *out = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
            }
        }
        Some((luma, w, h))
    }
}

/// Grab display `index` without converting it.
///
/// `CGWindowListCreateImage` is the fixed cost here (~10–20 ms per grab on this
/// machine, and it barely shrinks for a smaller rect), so the loop's only real
/// lever is grabbing less often — which is what the probe cadence does.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // CGWindowListCreateImage is deprecated in favour of
                     // ScreenCaptureKit, which we have not adopted yet.
pub fn capture_raw(index: usize) -> Option<RawFrame> {
    use objc2_core_graphics::{
        CGDataProvider, CGDisplayBounds, CGImage, CGWindowImageOption, CGWindowListCreateImage,
        CGWindowListOption,
    };

    if std::env::var_os("RAPTORQR_FAKE_SCREEN").is_some() {
        let source = std::env::var_os("RAPTORQR_FAKE_SCREEN")?;
        if index != 0 {
            return None;
        }
        let (rgba, w, h) = replay_frame(std::path::Path::new(&source))?;
        return Some(RawFrame {
            provider: None,
            owned: Some(rgba),
            stride: w as usize * 4,
            w,
            h,
            bgra: false,
        });
    }

    let display_id = display_at(index)?.id().ok()?;
    let bounds = CGDisplayBounds(display_id);
    let image = CGWindowListCreateImage(
        bounds,
        CGWindowListOption::OptionAll,
        0,
        CGWindowImageOption::Default,
    )?;

    let w = CGImage::width(Some(&image)) as u32;
    let h = CGImage::height(Some(&image)) as u32;
    if w == 0 || h == 0 {
        return None;
    }
    let provider = CGImage::data_provider(Some(&image));
    let data = CGDataProvider::data(provider.as_deref())?;
    if CGImage::bytes_per_row(Some(&image)) < (w as usize) * 4 {
        return None;
    }
    Some(RawFrame {
        provider: Some(data),
        owned: None,
        stride: CGImage::bytes_per_row(Some(&image)),
        w,
        h,
        bgra: true,
    })
}

/// Grab display `index` to luma directly (no cached frame kept).
#[cfg(target_os = "macos")]
pub fn capture_luma(index: usize) -> Option<(Vec<u8>, u32, u32)> {
    capture_raw(index)?.to_luma()
}

/// Grab display `index` to luma, via the RGBA path on platforms without the
/// direct CoreGraphics reader.
#[cfg(not(target_os = "macos"))]
pub fn capture_luma(index: usize) -> Option<(Vec<u8>, u32, u32)> {
    let (rgba, w, h) = capture_display(index)?;
    let rect = ScreenRect { x: 0, y: 0, w, h };
    rgba_to_luma_rect(&rgba, w, h, &rect)
}

/// Grab display `index` without converting it (RGBA platforms).
#[cfg(not(target_os = "macos"))]
pub fn capture_raw(index: usize) -> Option<RawFrame> {
    let (rgba, w, h) = capture_display(index)?;
    Some(RawFrame {
        owned: Some(rgba),
        stride: w as usize * 4,
        w,
        h,
        bgra: false,
    })
}


/// Copy just `rect` out of a luma plane (clamped to the plane bounds).
pub fn luma_rect(luma: &[u8], w: u32, h: u32, rect: &ScreenRect) -> Option<(Vec<u8>, u32, u32)> {
    let x0 = rect.x.min(w);
    let y0 = rect.y.min(h);
    let x1 = (rect.x + rect.w).min(w);
    let y1 = (rect.y + rect.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let cw = (x1 - x0) as usize;
    let ch = (y1 - y0) as usize;
    let mut out = Vec::with_capacity(cw * ch);
    for y in y0 as usize..y1 as usize {
        let row = &luma[y * w as usize..(y + 1) * w as usize];
        out.extend_from_slice(&row[x0 as usize..x1 as usize]);
    }
    Some((out, cw as u32, ch as u32))
}

/// Crop an RGBA buffer to `rect` (clamped to the image bounds).
pub fn crop(rgba: &[u8], w: u32, h: u32, rect: &ScreenRect) -> Option<(Vec<u8>, u32, u32)> {
    let x0 = rect.x.min(w);
    let y0 = rect.y.min(h);
    let x1 = (rect.x + rect.w).min(w);
    let y1 = (rect.y + rect.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let cw = x1 - x0;
    let ch = y1 - y0;
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y1 {
        let row_start = ((y * w + x0) * 4) as usize;
        let row_end = ((y * w + x1) * 4) as usize;
        out.extend_from_slice(&rgba[row_start..row_end]);
    }
    Some((out, cw, ch))
}

/// Downscale an RGBA buffer so its width is at most `max_w` (nearest-neighbor).
pub fn downscale(rgba: &[u8], w: u32, h: u32, max_w: u32) -> (Vec<u8>, u32, u32) {
    if w <= max_w || h == 0 {
        return (rgba.to_vec(), w, h);
    }
    let dw = max_w;
    let dh = ((h as u64 * dw as u64) / w as u64) as u32;
    let mut out = Vec::with_capacity((dw * dh * 4) as usize);
    for y in 0..dh {
        let sy = (y as u64 * h as u64 / dh as u64) as u32;
        let row = &rgba[(sy * w * 4) as usize..((sy + 1) * w * 4) as usize];
        for x in 0..dw {
            let sx = (x as u64 * w as u64 / dw as u64) as u32;
            let px = &row[(sx * 4) as usize..((sx + 1) * 4) as usize];
            out.extend_from_slice(px);
        }
    }
    (out, dw, dh)
}

/// Convert RGBA to luma for the QR decoder.
pub fn rgba_to_luma(rgba: &[u8]) -> Vec<u8> {
    rgba
        .chunks_exact(4)
        .map(|px| {
            // ITU-R BT.601 luma.
            ((px[0] as u32 * 77 + px[1] as u32 * 150 + px[2] as u32 * 29) >> 8) as u8
        })
        .collect()
}

/// Convert just `rect` of an RGBA frame to luma, in one pass.
///
/// This replaces the old `crop` (full RGBA copy) + `rgba_to_luma` (second full
/// copy) pair: the scan loop only ever needed the luma plane of the region.
pub fn rgba_to_luma_rect(
    rgba: &[u8],
    w: u32,
    h: u32,
    rect: &ScreenRect,
) -> Option<(Vec<u8>, u32, u32)> {
    let x0 = rect.x.min(w);
    let y0 = rect.y.min(h);
    let x1 = (rect.x + rect.w).min(w);
    let y1 = (rect.y + rect.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let cw = x1 - x0;
    let ch = y1 - y0;
    let mut out = Vec::with_capacity((cw as u64 * ch as u64) as usize);
    for y in y0..y1 {
        let row = &rgba[((y * w) as usize) * 4..(((y + 1) * w) as usize) * 4];
        for px in row[(x0 as usize) * 4..(x1 as usize) * 4].chunks_exact(4) {
            out.push(((px[0] as u32 * 77 + px[1] as u32 * 150 + px[2] as u32 * 29) >> 8) as u8);
        }
    }
    Some((out, cw, ch))
}

/// Half-resolution luma by averaging 2×2 blocks.
///
/// Probing at half size cuts the decode work by 4× and is still plenty for a QR
/// that fills a usable part of the screen; the full-speed path always decodes at
/// native resolution.
pub fn downscale_luma(luma: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    if w < 2 || h < 2 {
        return (luma.to_vec(), w, h);
    }
    let dw = w / 2;
    let dh = h / 2;
    let mut out = Vec::with_capacity((dw as u64 * dh as u64) as usize);
    for y in 0..dh {
        let row0 = &luma[((2 * y) as usize) * w as usize..((2 * y + 1) as usize) * w as usize];
        let row1 = &luma[((2 * y + 1) as usize) * w as usize..((2 * y + 2) as usize) * w as usize];
        for x in 0..dw {
            let i = (2 * x) as usize;
            let sum = row0[i] as u32
                + row0[i + 1] as u32
                + row1[i] as u32
                + row1[i + 1] as u32;
            out.push((sum / 4) as u8);
        }
    }
    (out, dw, dh)
}

/// Cheap frame fingerprint: FNV-1a over every 4096th byte.
///
/// A static screen (idle desktop, paused video) hashes the same every tick, so
/// the scan loop can skip the expensive decode. Sampling every 4 KiB still
/// notices any QR-sized change on screen.
pub fn fingerprint(rgba: &[u8]) -> u64 {
    fingerprint_bytes(rgba)
}

fn fingerprint_bytes(rgba: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in rgba.iter().step_by(4096) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Next PNG frame from `RAPTORQR_FAKE_SCREEN`, cycling forever like a looping
/// sender.
fn replay_frame(source: &std::path::Path) -> Option<(Vec<u8>, u32, u32)> {
    use std::sync::{Mutex, OnceLock};

    static STATE: OnceLock<Mutex<(Vec<std::path::PathBuf>, usize)>> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        let mut frames: Vec<std::path::PathBuf> = Vec::new();
        if source.is_dir() {
            if let Ok(entries) = std::fs::read_dir(source) {
                frames = entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
                    })
                    .collect();
                frames.sort();
            }
        } else {
            frames.push(source.to_path_buf());
        }
        Mutex::new((frames, 0))
    });

    let mut guard = state.lock().ok()?;
    let (frames, index) = &mut *guard;
    if frames.is_empty() {
        return None;
    }
    let path = frames[*index % frames.len()].clone();
    *index += 1;

    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pixels: &[[u8; 4]], w: u32, h: u32, bgra: bool) -> RawFrame {
        let mut bytes = Vec::new();
        for pixel in pixels {
            bytes.extend_from_slice(pixel);
        }
        RawFrame {
            #[cfg(target_os = "macos")]
            provider: None,
            owned: Some(bytes),
            stride: w as usize * 4,
            w,
            h,
            bgra,
        }
    }

    #[test]
    fn to_luma_reads_both_channel_orders() {
        let rgba = [
            [255u8, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let from_rgba = frame(&rgba, 2, 2, false).to_luma().unwrap().0;
        let bgra: Vec<[u8; 4]> = rgba.iter().map(|p| [p[2], p[1], p[0], p[3]]).collect();
        let from_bgra = frame(&bgra, 2, 2, true).to_luma().unwrap().0;
        assert_eq!(from_rgba, from_bgra);
        // BT.601: red (~0.30) carries more luma than blue (~0.11).
        assert!(from_rgba[0] > from_rgba[2], "{from_rgba:?}");
        assert_eq!(from_rgba[3], 255);
    }

    #[test]
    fn to_luma_respects_row_padding() {
        // Two rows of 2 px, each padded with 4 junk bytes.
        let mut bytes = Vec::new();
        for row in 0..2u8 {
            for px in 0..2u8 {
                bytes.extend_from_slice(&[0, 0, 0, 255]);
                let _ = px;
            }
            bytes.extend_from_slice(&[1, 2, 3, 4]);
            let _ = row;
        }
        let padded = RawFrame {
            #[cfg(target_os = "macos")]
            provider: None,
            owned: Some(bytes),
            stride: 2 * 4 + 4,
            w: 2,
            h: 2,
            bgra: false,
        };
        assert_eq!(padded.to_luma().unwrap().0, vec![0; 4]);
    }
}
