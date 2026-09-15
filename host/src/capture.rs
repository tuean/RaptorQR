//! Screen capture via xcap (CGWindowListCreateImage on macOS).
//!
//! Display `index` follows xcap's order with the primary display first, so
//! `index = 0` is the main screen. Every function works in **monitor-local
//! physical pixels**: `(0,0)` is that display's top-left corner, and a Retina
//! display captures at 2× its logical point size.

use xcap::Monitor;

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
    let mut monitors = Monitor::all().unwrap_or_default();
    // The primary display is index 0 so the default preview is the main screen.
    monitors.sort_by_key(|m| !m.is_primary().unwrap_or(false));

    monitors
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

fn display_at(index: usize) -> Option<Monitor> {
    let mut monitors = Monitor::all().ok()?;
    monitors.sort_by_key(|m| !m.is_primary().unwrap_or(false));
    monitors.into_iter().nth(index)
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
