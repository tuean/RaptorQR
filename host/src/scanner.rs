//! Capture + decode cadence: cheap probing while idle, full speed while a QR
//! stream is live.
//!
//! The receiver used to capture **every** display at 8 fps and run a
//! full-resolution ZXing decode on each frame, whether or not anything was being
//! sent. On three Retina displays that is ~470 MB/s of pixel traffic around the
//! clock. The cadence here is what makes idle cheap without capping throughput:
//!
//! * **Probe** (idle): every `PROBE_INTERVAL`, capture *one* display and decode
//!   it at half resolution (no `TryHarder`). Displays are visited round-robin, so
//!   a QR on any of them is seen within ~1 s.
//! * **Active** (a QR was just decoded): capture the display that produced it at
//!   `ACTIVE_FPS`, full resolution, `TryHarder` on.
//!
//! `ACTIVE_GRACE` without a single decoded symbol drops back to probe, so video
//! playback or a busy desktop does not pin the receiver at full speed — the
//! trigger is *decoding a QR*, not *the screen changing*.
//!
//! Two more cheap wins live here: a frame whose fingerprint did not change is not
//! decoded again (a static screen costs one hash), and a region selection limits
//! capture to that one display.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::capture::{self, ScreenRect};
use crate::qrscan::scan_qrs;

/// Capture rate while a QR stream is live. The sender shows a new frame every
/// 100 ms; 8 fps still sees every frame often enough for the tracker.
pub const ACTIVE_FPS: u32 = 8;
pub const ACTIVE_INTERVAL: Duration = Duration::from_millis(1000 / ACTIVE_FPS as u64);
/// Idle poll rate — one display per tick, round-robin.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// No decoded symbol for this long ⇒ back to probe.
pub const ACTIVE_GRACE: Duration = Duration::from_secs(2);
/// Preview frames are only produced while the region picker is open.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(350);
const PREVIEW_MAX_W: u32 = 1024;
/// While probing, retry a frame at full resolution at most this often.
const PROBE_FULL_RETRY: Duration = Duration::from_secs(1);
/// Probe decodes at half resolution above this many pixels; below it, full is
/// already cheap (e.g. a small selected region).
const PROBE_DOWNSCALE_ABOVE: u64 = 2_000_000;
/// Consecutive black frames before the UI blames Screen Recording permission.
const BLACK_FRAMES: u32 = 6;
/// Mean luma below this counts as a black frame.
const BLACK_MEAN: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Probe,
    Active,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Probe => "probe",
            Mode::Active => "active",
        }
    }
}

/// Probe/active hysteresis, kept free of I/O so it can be tested directly.
#[derive(Debug)]
pub struct Cadence {
    mode: Mode,
    last_symbol: Option<Instant>,
}

impl Cadence {
    pub fn new() -> Self {
        Self {
            mode: Mode::Probe,
            last_symbol: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn interval(&self) -> Duration {
        match self.mode {
            Mode::Probe => PROBE_INTERVAL,
            Mode::Active => ACTIVE_INTERVAL,
        }
    }

    /// A tick decoded this many symbols.
    pub fn on_symbols(&mut self, count: usize, now: Instant) {
        if count == 0 {
            return;
        }
        self.mode = Mode::Active;
        self.last_symbol = Some(now);
    }

    /// No symbols this tick: drop back to probe after the grace period.
    pub fn decay(&mut self, now: Instant) {
        if self.mode != Mode::Active {
            return;
        }
        match self.last_symbol {
            Some(at) if now.duration_since(at) < ACTIVE_GRACE => {}
            _ => {
                self.mode = Mode::Probe;
                self.last_symbol = None;
            }
        }
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Self::new()
    }
}

/// What to scan and what to preview for one tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanConfig {
    /// Scan only this display, and only this rect inside it.
    pub region: Option<(usize, ScreenRect)>,
    /// Scan only this display, full screen (headless `--display`).
    pub only_display: Option<usize>,
    /// Which display the preview shows.
    pub preview_display: usize,
    /// Produce a preview frame (the region picker is open).
    pub want_preview: bool,
    /// Ignore the preview throttle (the picker just opened).
    pub force_preview: bool,
}

/// A downscaled frame for the region picker.
pub struct PreviewFrame {
    pub display: usize,
    pub display_count: usize,
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// Physical size of the captured frame — the coordinate space regions use.
    pub full_w: u32,
    pub full_h: u32,
}

#[derive(Default)]
pub struct ScanOutput {
    /// Decoded QR payloads, in capture order.
    pub symbols: Vec<Vec<u8>>,
    pub display_count: usize,
    /// Frames captured this tick (0 ⇒ capture is failing).
    pub frames: u32,
    pub preview: Option<PreviewFrame>,
}

/// What the caller asked to scan. A change invalidates the fingerprint cache —
/// the pixels are no longer the ones that were hashed.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ScopeTag {
    region: Option<(usize, ScreenRect)>,
    display: Option<usize>,
}

/// `RAPTORQR_TRACE=1` logs per-tick capture/decode cost on stderr.
fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RAPTORQR_TRACE").is_some())
}

struct Captured {
    index: usize,
    /// Still in the compositor's buffer; converted only if the frame changed.
    frame: capture::RawFrame,
}

/// Stateful capture/decode loop. One instance per worker thread.
pub struct Scanner {
    cadence: Cadence,
    /// Display that last produced a symbol; captured alone while active.
    focus: Option<usize>,
    probe_cursor: usize,
    /// Per-display fingerprint of the last captured frame.
    last_fp: Vec<Option<u64>>,
    last_preview: Option<Instant>,
    last_full_retry: Option<Instant>,
    /// Scope the cached fingerprints belong to.
    scope: Option<ScopeTag>,
    black_streak: u32,
    last_frame_black: bool,
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            cadence: Cadence::new(),
            focus: None,
            probe_cursor: 0,
            last_fp: Vec::new(),
            last_preview: None,
            last_full_retry: None,
            scope: None,
            black_streak: 0,
            last_frame_black: false,
        }
    }

    pub fn mode(&self) -> Mode {
        self.cadence.mode()
    }

    /// Sleep this long before the next [`tick`](Self::tick).
    pub fn interval(&self) -> Duration {
        self.cadence.interval()
    }

    /// True when the last several decodes were black — macOS hands out black
    /// frames when Screen Recording permission is missing.
    pub fn capture_black(&self) -> bool {
        self.black_streak >= BLACK_FRAMES
    }

    pub fn tick(&mut self, now: Instant, cfg: &ScanConfig) -> ScanOutput {
        let mut out = ScanOutput::default();
        let traced = trace_enabled();

        let displays = capture::display_list();
        out.display_count = displays.len();
        if displays.is_empty() {
            return out;
        }

        let tag = ScopeTag {
            region: cfg.region,
            display: cfg.only_display,
        };
        if self.scope != Some(tag) {
            self.last_fp.clear();
            self.scope = Some(tag);
        }

        let mode = self.cadence.mode();
        let targets = self.targets(cfg, displays.len(), mode);

        let started = Instant::now();
        let mut frames: Vec<Captured> = Vec::new();
        for index in targets {
            if let Some(frame) = capture::capture_raw(index) {
                out.frames += 1;
                frames.push(Captured { index, frame });
            }
        }
        let capture_ms = started.elapsed().as_secs_f64() * 1000.0;
        if frames.is_empty() {
            return out;
        }

        let decode_started = Instant::now();
        let mut found = 0usize;
        for frame in &frames {
            if let Some(symbols) = self.decode(frame, cfg, mode, now) {
                if !symbols.is_empty() {
                    found += symbols.len();
                    self.focus = Some(frame.index);
                    out.symbols.extend(symbols);
                }
            }
        }
        if traced {
            eprintln!(
                "[scan] mode={} display={:?} capture={:.1}ms decode={:.1}ms symbols={}",
                mode.label(),
                frames.iter().map(|f| f.index).collect::<Vec<_>>(),
                capture_ms,
                decode_started.elapsed().as_secs_f64() * 1000.0,
                found,
            );
        }
        self.cadence.on_symbols(found, now);
        if found == 0 {
            self.cadence.decay(now);
        }

        if cfg.want_preview {
            let due = cfg.force_preview
                || self
                    .last_preview
                    .map_or(true, |at| now.duration_since(at) >= PREVIEW_INTERVAL);
            if due {
                out.preview = self.preview(cfg, &frames, displays.len(), &mut out.frames);
                self.last_preview = Some(now);
            }
        }

        out
    }

    /// Which displays to capture this tick.
    fn targets(&mut self, cfg: &ScanConfig, count: usize, mode: Mode) -> Vec<usize> {
        if let Some((display, _)) = cfg.region {
            return vec![display.min(count - 1)];
        }
        if let Some(display) = cfg.only_display {
            return vec![display.min(count - 1)];
        }
        if mode == Mode::Active {
            if let Some(focus) = self.focus {
                return vec![focus.min(count - 1)];
            }
        }
        let index = self.probe_cursor % count;
        self.probe_cursor = self.probe_cursor.wrapping_add(1);
        vec![index]
    }

    /// Decode one captured frame, or `None` when it was skipped as unchanged.
    fn decode(
        &mut self,
        frame: &Captured,
        cfg: &ScanConfig,
        mode: Mode,
        now: Instant,
    ) -> Option<Vec<Vec<u8>>> {
        if self.last_fp.len() <= frame.index {
            self.last_fp.resize(frame.index + 1, None);
        }
        let fp = frame.frame.fingerprint();
        let changed = self.last_fp[frame.index] != Some(fp);
        self.last_fp[frame.index] = Some(fp);
        // A black frame never changes, so an unchanged frame is only skipped when
        // the previous decode saw real content — otherwise the missing-permission
        // warning would never fire.
        if !changed && !self.last_frame_black {
            return None;
        }

        let bounds = ScreenRect {
            x: 0,
            y: 0,
            w: frame.frame.w,
            h: frame.frame.h,
        };
        let scope = match cfg.region {
            Some((display, rect)) if display == frame.index => rect.intersect(&bounds),
            _ => bounds,
        };
        if scope.area() == 0 {
            return None;
        }

        let (full_luma, fw, fh) = frame.frame.to_luma()?;
        let half = mode == Mode::Probe && scope.area() > PROBE_DOWNSCALE_ABOVE;
        let (luma, lw, lh) = capture::luma_rect(&full_luma, fw, fh, &scope)?;
        let (luma, lw, lh) = if half {
            capture::downscale_luma(&luma, lw, lh)
        } else {
            (luma, lw, lh)
        };
        self.note_brightness(mean_luma(&luma));

        let mut symbols = scan_qrs(luma, lw, lh, !half);
        if symbols.is_empty() && half && self.full_retry_due(now) {
            // Half resolution is the cheap probe; a small QR may only resolve at
            // native size, so retry occasionally before giving up on the frame.
            self.last_full_retry = Some(now);
            if let Some((luma, lw, lh)) = capture::luma_rect(&full_luma, fw, fh, &scope) {
                symbols = scan_qrs(luma, lw, lh, true);
            }
        }
        Some(symbols)
    }

    fn full_retry_due(&self, now: Instant) -> bool {
        self.last_full_retry
            .map_or(true, |at| now.duration_since(at) >= PROBE_FULL_RETRY)
    }

    fn note_brightness(&mut self, mean: f64) {
        self.last_frame_black = mean < BLACK_MEAN;
        if self.last_frame_black {
            self.black_streak = self.black_streak.saturating_add(1);
        } else {
            self.black_streak = 0;
        }
    }

    fn preview(
        &self,
        cfg: &ScanConfig,
        frames: &[Captured],
        count: usize,
        counter: &mut u32,
    ) -> Option<PreviewFrame> {
        let display = cfg.preview_display.min(count - 1);
        // The scan path only keeps luma now, so the picker preview is a separate
        // capture — it only runs while the picker is open.
        let _ = frames;
        let (rgba, w, h) = capture::capture_display(display)?;
        *counter += 1;
        Some(preview_frame(display, count, &rgba, w, h))
    }
}

fn preview_frame(
    display: usize,
    count: usize,
    rgba: &[u8],
    full_w: u32,
    full_h: u32,
) -> PreviewFrame {
    let (rgba, w, h) = capture::downscale(rgba, full_w, full_h, PREVIEW_MAX_W);
    PreviewFrame {
        display,
        display_count: count,
        rgba,
        w,
        h,
        full_w,
        full_h,
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn mean_luma(luma: &[u8]) -> f64 {
    if luma.is_empty() {
        return 0.0;
    }
    // Every 16th pixel is plenty for a black-frame check.
    let mut sum = 0u64;
    let mut n = 0u64;
    for &px in luma.iter().step_by(16) {
        sum += px as u64;
        n += 1;
    }
    sum as f64 / n.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_ramps_on_symbols_and_decays_without_them() {
        let t0 = Instant::now();
        let mut cadence = Cadence::new();
        assert_eq!(cadence.mode(), Mode::Probe);
        assert_eq!(cadence.interval(), PROBE_INTERVAL);

        cadence.on_symbols(0, t0);
        assert_eq!(cadence.mode(), Mode::Probe, "no symbol must not ramp");

        cadence.on_symbols(1, t0);
        assert_eq!(cadence.mode(), Mode::Active);
        assert_eq!(cadence.interval(), ACTIVE_INTERVAL);

        // Still inside the grace window.
        cadence.decay(t0 + ACTIVE_GRACE - Duration::from_millis(1));
        assert_eq!(cadence.mode(), Mode::Active);

        cadence.decay(t0 + ACTIVE_GRACE);
        assert_eq!(cadence.mode(), Mode::Probe);
        assert_eq!(cadence.interval(), PROBE_INTERVAL);
    }

    #[test]
    fn active_interval_is_eight_fps() {
        assert_eq!(ACTIVE_INTERVAL, Duration::from_millis(125));
    }

    #[test]
    fn downscale_luma_averages_2x2_blocks() {
        // 4×2 image: each 2×2 block becomes one pixel.
        let luma = vec![0, 0, 100, 100, 0, 0, 100, 100];
        let (out, w, h) = capture::downscale_luma(&luma, 4, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![0, 100]);
    }

    #[test]
    fn fingerprint_tracks_frame_content() {
        let mut frame = vec![0u8; 64 * 1024];
        let a = capture::fingerprint(&frame);
        // The hash samples every 4096th byte, so poke one of the samples.
        frame[4096 * 3] = 200;
        assert_ne!(a, capture::fingerprint(&frame));
        assert_eq!(capture::fingerprint(&frame), capture::fingerprint(&frame));
    }

    #[test]
    fn luma_rect_reads_only_the_region() {
        // 2×2 RGBA: red, green / blue, white.
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let rect = ScreenRect {
            x: 1,
            y: 0,
            w: 1,
            h: 2,
        };
        let (luma, w, h) = capture::rgba_to_luma_rect(&rgba, 2, 2, &rect).unwrap();
        assert_eq!((w, h), (1, 2));
        // Green ≈ 149, white ≈ 255.
        assert_eq!(luma.len(), 2);
        assert!(luma[0] > 140 && luma[0] < 160, "green luma {}", luma[0]);
        assert!(luma[1] > 250, "white luma {}", luma[1]);
    }
}
