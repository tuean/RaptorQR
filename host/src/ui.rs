//! gpui receiver UI: live screen preview, drag-to-select region, status, and
//! transfer log. A background worker thread does capture + QR decode + FEC.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::capture::{self, ScreenRect};
use crate::qrscan::scan_qrs;
use crate::transfer::{save_to_downloads, TransferTracker};

// ─── Shared state between the UI and the capture worker ─────────────────────

pub struct SharedState {
    /// Decode region: (display index, rect in that display's pixels).
    pub region: Mutex<Option<(usize, ScreenRect)>>,
    pub running: AtomicBool,
    pub capture_fps: AtomicU32,
    /// Which display the preview shows (regions are selected on it).
    pub preview_display: AtomicUsize,
    pub display_count: AtomicUsize,
    /// Set when the UI wants a preview frame immediately.
    pub preview_request: AtomicBool,
}

// ─── Worker → UI events ─────────────────────────────────────────────────────

pub enum WorkerEvent {
    /// Downscaled preview frame of the full display. `full_w/full_h` are the
    /// captured frame's physical pixels — the coordinate space regions live in.
    Preview {
        display: usize,
        display_count: usize,
        rgba: Vec<u8>,
        w: u32,
        h: u32,
        full_w: u32,
        full_h: u32,
    },
    /// Periodic status snapshot.
    Status {
        fps: f64,
        packets: u64,
        qr_per_second: f64,
        scanning: String,
        /// True when the captured frames are black — macOS returns black
        /// frames when Screen Recording permission is missing.
        capture_black: bool,
        progress: f64,
        blocks: Vec<(u8, u32, u32)>,
        note: String,
    },
    /// A transfer completed. `written` is false when an identical file was
    /// already on disk (nothing new was written).
    Transfer {
        filename: String,
        path: PathBuf,
        size: usize,
        written: bool,
    },
    CaptureError(String),
}

const CAPTURE_FPS: u32 = 8;
const PREVIEW_MAX_W: u32 = 1024;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(350);
/// Minimum gap between QR decodes of one display.
const DECODE_INTERVAL: Duration = Duration::from_millis(100);
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

/// Capture + decode loop, runs on a background thread.
///
/// Set `RAPTORQR_DEBUG=1` to trace capture/decode progress on stderr.
pub fn run_worker(shared: Arc<SharedState>, tx: mpsc::Sender<WorkerEvent>) {
    let debug = std::env::var_os("RAPTORQR_DEBUG").is_some();
    let mut tracker = TransferTracker::new();
    let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
    let mut last_status = Instant::now();
    let mut status_frames = 0u64;
    let mut packets_decoded = 0u64;
    let mut qr_symbols = 0u64;
    // Throttled per display: a single shared deadline would let the first
    // display consume the budget and starve the others.
    let mut last_decode: Vec<Instant> = Vec::new();
    // Consecutive decodes whose frame was black (permission missing).
    let mut black_frames = 0u32;

    loop {
        if !shared.running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let tick = Instant::now();

        // Every display is scanned, so the QR can live on any of them; the
        // preview (and therefore region selection) follows one of them.
        let displays = capture::display_list();
        let display_count = displays.len().max(1);
        shared.display_count.store(display_count, Ordering::Relaxed);
        let preview_display = shared
            .preview_display
            .load(Ordering::Relaxed)
            .min(display_count - 1);
        let region = *shared.region.lock().unwrap();

        let mut captured_any = false;
        for display in &displays {
            let Some((rgba, w, h)) = capture::capture_display(display.index) else {
                continue;
            };
            captured_any = true;
            if display.index == preview_display {
                status_frames += 1;
            }

            // Preview (throttled), only for the selected display.
            let preview_wanted = shared.preview_request.swap(false, Ordering::Relaxed);
            if display.index == preview_display
                && (preview_wanted || tick.duration_since(last_preview) >= PREVIEW_INTERVAL)
            {
                let (prgba, pw, ph) = capture::downscale(&rgba, w, h, PREVIEW_MAX_W);
                let _ = tx.send(WorkerEvent::Preview {
                    display: display.index,
                    display_count,
                    rgba: prgba,
                    w: pw,
                    h: ph,
                    full_w: w,
                    full_h: h,
                });
                last_preview = tick;
            }

            // Decode: the whole display, or just the selected region when the
            // region belongs to this display.
            let full = ScreenRect { x: 0, y: 0, w, h };
            let scope = match region {
                Some((region_display, rect)) if region_display == display.index => {
                    rect.intersect(&full)
                }
                _ => full,
            };
            if last_decode.len() <= display.index {
                // Start "due" so a new display is decoded immediately.
                let due = tick.checked_sub(DECODE_INTERVAL).unwrap_or(tick);
                last_decode.resize(display.index + 1, due);
            }
            if scope.area() == 0 || tick.duration_since(last_decode[display.index]) < DECODE_INTERVAL
            {
                continue;
            }
            last_decode[display.index] = tick;

            if let Some((crgba, cw, ch)) = capture::crop(&rgba, w, h, &scope) {
                let decode_started = Instant::now();
                let luma = capture::rgba_to_luma(&crgba);
                // Sampled brightness: a fully black frame means macOS is not
                // giving us the screen (missing Screen Recording permission).
                let samples = luma.len() / 97 + 1;
                let mean = luma.iter().step_by(97).map(|&v| v as u64).sum::<u64>()
                    / samples as u64;
                if mean < 2 {
                    black_frames += 1;
                } else {
                    black_frames = 0;
                }
                let found = scan_qrs(&luma, cw, ch);
                qr_symbols += found.len() as u64;
                if debug {
                    let (progress, note) = match tracker.status() {
                        Some(status) if status.complete.is_some() => {
                            (100.0, "complete (stream looping)")
                        }
                        Some(status) => (status.progress * 100.0, "receiving"),
                        None => (0.0, "idle"),
                    };
                    eprintln!(
                        "[recv] display {} region {}×{} qr_found={} packets={} progress={progress:.1}% {note} decode={}ms session={} key={:?} esi={:?}",
                        display.index + 1,
                        cw,
                        ch,
                        found.len(),
                        packets_decoded,
                        decode_started.elapsed().as_millis(),
                        tracker.session_seq(),
                        tracker.session_key(),
                        tracker.received_esis()
                    );
                }
                for raw in found {
                    match tracker.feed(&raw) {
                        crate::transfer::FeedResult::Ignored => {}
                        crate::transfer::FeedResult::Progress => {
                            packets_decoded += 1;
                        }
                        crate::transfer::FeedResult::Duplicate => {
                            // Same stream looping again — already saved.
                        }
                        crate::transfer::FeedResult::Completed(t) => {
                            packets_decoded += 1;
                            let (path, written) = save_to_downloads(&t.filename, &t.body);
                            tracker.mark_saved(path.clone(), t.body.len());
                            if debug {
                                eprintln!(
                                    "[recv] COMPLETE {} → {} ({} bytes, {})",
                                    t.filename,
                                    path.display(),
                                    t.body.len(),
                                    if written { "saved" } else { "identical file already there" }
                                );
                            }
                            // Announce every receipt — including one whose
                            // identical file is already on disk, otherwise the
                            // user sees nothing at all.
                            if written {
                                crate::notify::notify_transfer_received(
                                    &t.filename,
                                    &path.display().to_string(),
                                    t.body.len(),
                                );
                            }
                            let _ = tx.send(WorkerEvent::Transfer {
                                filename: t.filename,
                                path,
                                size: t.body.len(),
                                written,
                            });
                        }
                    }
                }
            }
        }

        if !captured_any {
            let _ = tx.send(WorkerEvent::CaptureError(
                "screen capture failed — grant Screen Recording permission?".into(),
            ));
            std::thread::sleep(Duration::from_millis(1000));
        }

        // Status (throttled).
        if tick.duration_since(last_status) >= STATUS_INTERVAL {
            let elapsed_status = tick.duration_since(last_status).as_secs_f64();
            let fps = status_frames as f64 / elapsed_status;
            let qr_per_second = (qr_symbols as f64 / elapsed_status * 10.0).round() / 10.0;
            let (progress, blocks, note) = match tracker.status() {
                Some(status) => match status.complete {
                    Some((filename, path, bytes)) => (
                        1.0,
                        status.blocks,
                        format!(
                            "已完成：{filename} ({} B) 已保存到 {} — 发送端仍在循环播放",
                            crate::notify::human_bytes(bytes),
                            path.parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default()
                        ),
                    ),
                    None => (
                        status.progress,
                        status.blocks,
                        format!(
                            "receiving {} bytes @ {} B/symbol",
                            status.key.data_length,
                            status.key.transport_payload_size - 4
                        ),
                    ),
                },
                None => (0.0, Vec::new(), "waiting for QR stream…".into()),
            };
            let scanning = match region {
                Some((index, rect)) => format!(
                    "display {}/{} region {}×{}",
                    index + 1,
                    display_count,
                    rect.w,
                    rect.h
                ),
                None => format!(
                    "all {} display(s), full screen (preview {}/{})",
                    display_count,
                    preview_display + 1,
                    display_count
                ),
            };
            let _ = tx.send(WorkerEvent::Status {
                fps: (fps * 10.0).round() / 10.0,
                packets: packets_decoded,
                qr_per_second,
                scanning,
                capture_black: black_frames >= 6,
                progress,
                blocks,
                note,
            });
            status_frames = 0;
            qr_symbols = 0;
            last_status = tick;
        }

        let elapsed = tick.elapsed();
        let period =
            Duration::from_micros(1_000_000 / shared.capture_fps.load(Ordering::Relaxed) as u64);
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }
    }
}

// ─── UI view ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TransferItem {
    filename: String,
    path: PathBuf,
    size: usize,
    /// false = identical file already existed, nothing new was written
    written: bool,
}

pub struct ReceiverView {
    shared: Arc<SharedState>,
    events: Mutex<mpsc::Receiver<WorkerEvent>>,
    running: bool,
    preview: Option<Arc<RenderImage>>,
    preview_dims: (u32, u32),
    preview_pending_drop: Option<Arc<RenderImage>>,
    /// Physical pixel size of the previewed display (region coordinate space).
    full: Option<ScreenRect>,
    /// Which display `full`/`preview` belong to and how many exist.
    preview_display: usize,
    display_count: usize,
    /// Decode region: (display index, rect in that display's pixels).
    region: Option<(usize, ScreenRect)>,
    /// Human-readable scan scope, e.g. "all 2 display(s), full screen".
    scanning: String,
    /// Captured frames are black → Screen Recording permission is missing.
    capture_black: bool,
    /// Region picker is open (the preview is only shown there — it is
    /// redundant the rest of the time, since every display is scanned).
    selecting: bool,
    /// Canvas bounds in window coords, captured during paint for mouse mapping.
    canvas_bounds: Option<Bounds<Pixels>>,
    /// Drag state in preview fractions (0..1).
    drag_start: Option<(f32, f32)>,
    drag_current: Option<(f32, f32)>,
    fps: f64,
    packets: u64,
    qr_rate: f64,
    progress: f64,
    blocks: Vec<(u8, u32, u32)>,
    note: String,
    transfers: Vec<TransferItem>,
    last_error: Option<String>,
}

impl ReceiverView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let shared = Arc::new(SharedState {
            region: Mutex::new(None),
            running: AtomicBool::new(true),
            capture_fps: AtomicU32::new(CAPTURE_FPS),
            preview_display: AtomicUsize::new(0),
            display_count: AtomicUsize::new(1),
            preview_request: AtomicBool::new(true),
        });
        let (tx, rx) = mpsc::channel::<WorkerEvent>();
        let worker_shared = shared.clone();
        std::thread::spawn(move || run_worker(worker_shared, tx));

        cx.spawn(async |this: WeakEntity<ReceiverView>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.drain_events();
                    cx.notify();
                });
            }
        })
        .detach();

        ReceiverView {
            shared,
            events: Mutex::new(rx),
            running: true,
            preview: None,
            preview_dims: (0, 0),
            preview_pending_drop: None,
            full: None,
            preview_display: 0,
            display_count: 1,
            region: None,
            scanning: String::new(),
            capture_black: false,
            selecting: std::env::var_os("RAPTORQR_START_IN_PICKER").is_some(),
            canvas_bounds: None,
            drag_start: None,
            drag_current: None,
            fps: 0.0,
            packets: 0,
            qr_rate: 0.0,
            progress: 0.0,
            blocks: Vec::new(),
            note: "capturing…".into(),
            transfers: Vec::new(),
            last_error: None,
        }
    }

    fn drain_events(&mut self) {
        let rx = self.events.lock().unwrap();
        for event in rx.try_iter() {
            match event {
                WorkerEvent::Preview {
                    display,
                    display_count,
                    rgba,
                    w,
                    h,
                    full_w,
                    full_h,
                } => {
                    // gpui's RenderImage stores BGRA, the capture is RGBA.
                    let mut bgra = rgba;
                    for pixel in bgra.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                    let frame = image::Frame::new(
                        image::RgbaImage::from_raw(w, h, bgra).unwrap_or_default(),
                    );
                    let img = Arc::new(RenderImage::new(vec![frame]));
                    self.preview_pending_drop = self.preview.take();
                    self.preview = Some(img);
                    self.preview_dims = (w, h);
                    self.preview_display = display;
                    self.display_count = display_count;
                    self.full = Some(ScreenRect {
                        x: 0,
                        y: 0,
                        w: full_w,
                        h: full_h,
                    });
                }
                WorkerEvent::Status {
                    fps,
                    packets,
                    qr_per_second,
                    scanning,
                    capture_black,
                    progress,
                    blocks,
                    note,
                } => {
                    self.fps = fps;
                    self.packets = packets;
                    self.qr_rate = qr_per_second;
                    self.scanning = scanning;
                    self.capture_black = capture_black;
                    self.progress = progress;
                    self.blocks = blocks;
                    self.note = note;
                    self.last_error = None;
                }
                WorkerEvent::Transfer {
                    filename,
                    path,
                    size,
                    written,
                } => {
                    self.transfers.push(TransferItem {
                        filename,
                        path,
                        size,
                        written,
                    });
                    if self.transfers.len() > 50 {
                        self.transfers.remove(0);
                    }
                }
                WorkerEvent::CaptureError(err) => {
                    self.last_error = Some(err);
                }
            }
        }
    }

    fn toggle_running(&mut self, _: &mut Context<Self>) {
        self.running = !self.running;
        self.shared.running.store(self.running, Ordering::Relaxed);
    }

    fn open_region_picker(&mut self, _: &mut Context<Self>) {
        self.selecting = true;
        self.drag_start = None;
        self.drag_current = None;
        // Don't wait for the next throttled preview tick.
        self.shared.preview_request.store(true, Ordering::Relaxed);
    }

    fn close_region_picker(&mut self, _: &mut Context<Self>) {
        self.selecting = false;
        self.drag_start = None;
        self.drag_current = None;
        // Keep the last preview: reopening the picker then shows a picture
        // immediately instead of a blank area with nothing to drag on.
    }

    fn reset_region(&mut self, _: &mut Context<Self>) {
        // `None` means "scan whole displays" for the worker.
        self.region = None;
        *self.shared.region.lock().unwrap() = None;
        self.drag_start = None;
        self.drag_current = None;
    }

    /// Show the next display in the preview, so a region can be framed on any
    /// screen. Selection state is dropped to avoid a stale overlay.
    fn select_display(&mut self, display: usize, _: &mut Context<Self>) {
        let count = self.display_count.max(1);
        let next = display.min(count - 1);
        self.preview_display = next;
        self.shared.preview_display.store(next, Ordering::Relaxed);
        self.preview = None;
        self.preview_dims = (0, 0);
        self.full = None;
        self.drag_start = None;
        self.drag_current = None;
    }

    /// Map a window-space point to preview fractions using the stored canvas bounds.
    fn point_to_fraction(&self, pos: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.canvas_bounds?;
        let (pw, ph) = self.preview_dims;
        if pw == 0 || ph == 0 {
            return None;
        }
        let (ox, oy, dw, dh) = self.contain_rect(bounds)?;
        let fx = (f32::from(pos.x) - ox) / dw;
        let fy = (f32::from(pos.y) - oy) / dh;
        if (0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy) {
            Some((fx, fy))
        } else {
            None
        }
    }

    /// Object-fit-contain display rect of the preview inside `bounds`.
    fn contain_rect(&self, bounds: Bounds<Pixels>) -> Option<(f32, f32, f32, f32)> {
        let (pw, ph) = self.preview_dims;
        if pw == 0 || ph == 0 {
            return None;
        }
        let bw = f32::from(bounds.size.width).max(1.0);
        let bh = f32::from(bounds.size.height).max(1.0);
        let scale = (bw / pw as f32).min(bh / ph as f32);
        let dw = pw as f32 * scale;
        let dh = ph as f32 * scale;
        let ox = f32::from(bounds.origin.x) + (bw - dw) / 2.0;
        let oy = f32::from(bounds.origin.y) + (bh - dh) / 2.0;
        Some((ox, oy, dw, dh))
    }

    fn fraction_to_region(&self, a: (f32, f32), b: (f32, f32)) -> Option<ScreenRect> {
        let full = self.full?;
        let fx0 = a.0.min(b.0).clamp(0.0, 1.0);
        let fy0 = a.1.min(b.1).clamp(0.0, 1.0);
        let fx1 = a.0.max(b.0).clamp(0.0, 1.0);
        let fy1 = a.1.max(b.1).clamp(0.0, 1.0);
        if (fx1 - fx0) * (fy1 - fy0) < 0.002 {
            return None;
        }
        Some(ScreenRect {
            x: (fx0 * full.w as f32) as u32,
            y: (fy0 * full.h as f32) as u32,
            w: ((fx1 - fx0) * full.w as f32) as u32,
            h: ((fy1 - fy0) * full.h as f32) as u32,
        })
    }

    fn on_preview_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left {
            self.drag_start = self.point_to_fraction(event.position);
            self.drag_current = self.drag_start;
            cx.notify();
        }
    }

    fn on_preview_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_start.is_some() {
            self.drag_current = self.point_to_fraction(event.position);
            cx.notify();
        }
    }

    fn on_preview_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left {
            if let (Some(a), Some(b)) = (self.drag_start, self.drag_current) {
                if let Some(region) = self.fraction_to_region(a, b) {
                    let scoped = (self.preview_display, region);
                    self.region = Some(scoped);
                    *self.shared.region.lock().unwrap() = Some(scoped);
                    self.selecting = false;
                    self.preview_pending_drop = self.preview.take();
                    self.preview_dims = (0, 0);
                }
            }
            self.drag_start = None;
            self.drag_current = None;
            cx.notify();
        }
    }

    /// Called from the canvas paint closure. `bounds` are the canvas bounds.
    fn paint_preview(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        self.canvas_bounds = Some(bounds);
        window.paint_quad(fill(bounds, rgb(0x010409)));

        // Draw at the same rect `point_to_fraction` maps against, so what the
        // user sees is exactly what they select.
        if let (Some((ox, oy, dw, dh)), Some(img)) = (self.contain_rect(bounds), self.preview.clone())
        {
            let image_bounds = Bounds::new(point(px(ox), px(oy)), size(px(dw), px(dh)));
            let _ = window.paint_image(image_bounds, (0.).into(), img, 0, false);
        }
        // Drop the previous frame's atlas tile now that the new one is painted.
        if let Some(old) = self.preview_pending_drop.take() {
            let _ = window.drop_image(old);
        }
        self.paint_selection(bounds, window);
    }

    fn paint_selection(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        // Selection rectangle during a drag, otherwise the committed region.
        let rect = if let (Some(a), Some(b)) = (self.drag_start, self.drag_current) {
            self.fraction_to_region(a, b)
                .and_then(|r| self.region_to_canvas_rect(&r, bounds))
        } else {
            match self.region {
                Some((display, rect)) if display == self.preview_display => {
                    self.region_to_canvas_rect(&rect, bounds)
                }
                _ => None,
            }
        };
        if let Some(r) = rect {
            if f32::from(r.size.width) > 0.0 && f32::from(r.size.height) > 0.0 {
                window.paint_quad(outline(r, rgb(0x4f9dff), BorderStyle::Solid));
            }
        }
    }

    fn region_to_canvas_rect(
        &self,
        region: &ScreenRect,
        bounds: Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        let full = self.full?;
        let (ox, oy, dw, dh) = self.contain_rect(bounds)?;
        let x = ox + region.x as f32 / full.w as f32 * dw;
        let y = oy + region.y as f32 / full.h as f32 * dh;
        let w = region.w as f32 / full.w as f32 * dw;
        let h = region.h as f32 / full.h as f32 * dh;
        Some(Bounds::new(
            point(px(x), px(y)),
            size(px(w), px(h)),
        ))
    }

    /// Short state chips shown in the header.
    fn chips(&self) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        out.push((
            if self.running { "● 捕获中".into() } else { "○ 已暂停".into() },
            if self.running { 0x3fb950 } else { 0x8b949e },
        ));
        if self.capture_black {
            out.push(("缺少屏幕录制权限".into(), 0xff7b72));
        } else if self.qr_rate > 0.0 {
            out.push((format!("{:.1} QR/s", self.qr_rate), 0x58a6ff));
        } else {
            out.push(("未发现二维码".into(), 0x8b949e));
        }
        out.push((format!("{:.1} fps", self.fps), 0x6a6a6e));
        let scope = match self.region {
            Some((display, r)) => format!("显示 {} · {}×{} 区域", display + 1, r.w, r.h),
            None => format!("全部 {} 块屏", self.display_count.max(1)),
        };
        out.push((scope, 0x6a6a6e));
        out
    }
}

impl Render for ReceiverView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0d1117))
            .text_color(rgb(0xd6d6d6))
            .font_family("PingFang SC")
            .child(self.render_header(cx))
            .child(if self.selecting {
                self.render_region_picker(cx)
            } else {
                self.render_body()
            })
            .child(self.render_footer())
    }
}

impl ReceiverView {
    fn render_header(&self, cx: &mut Context<Self>) -> Div {
        let mut chips = div().flex().flex_row().items_center().gap_2().flex_grow();
        for (label, colour) in self.chips() {
            chips = chips.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_full()
                    .bg(rgb(0x161b22))
                    .text_size(px(12.))
                    .text_color(rgb(colour))
                    .child(label),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(0x21262d))
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(rgb(0x58a6ff))
                    .child("◈ RaptorQR"),
            )
            .child(chips)
            .child(
                button(if self.running { "暂停" } else { "继续" })
                    .id("pause")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.toggle_running(cx)
                    })),
            )
            .child(
                primary_button("框选区域")
                    .id("pick-region")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.open_region_picker(cx)
                    })),
            )
            .when(self.region.is_some(), |header| {
                header.child(
                    button("清除区域")
                        .id("clear-region")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.reset_region(cx)
                        })),
                )
            })
    }

    /// Region picker: the only place the screen preview is shown.
    fn render_region_picker(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_grow()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .bg(rgb(0x161b22))
                    .child(
                        div()
                            .flex_grow()
                            .text_size(px(12.))
                            .text_color(rgb(0x8b949e))
                            .child(format!(
                                "在画面上拖拽框选二维码区域 · 当前显示第 {}/{} 块屏",
                                self.preview_display + 1,
                                self.display_count.max(1)
                            )),
                    )
                    .child(
                        button("◀")
                            .id("prev-display")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                let count = this.display_count.max(1);
                                let next = (this.preview_display + count - 1) % count;
                                this.select_display(next, cx);
                            })),
                    )
                    .child(
                        button("▶")
                            .id("next-display")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                let count = this.display_count.max(1);
                                let next = (this.preview_display + 1) % count;
                                this.select_display(next, cx);
                            })),
                    )
                    .child(
                        button("取消")
                            .id("cancel-region")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.close_region_picker(cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_grow()
                    .relative()
                    .overflow_hidden()
                    .bg(rgb(0x010409))
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                            this.on_preview_mouse_down(e, window, cx)
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                        this.on_preview_mouse_move(e, window, cx)
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseUpEvent, window, cx| {
                            this.on_preview_mouse_up(e, window, cx)
                        }),
                    )
                    .child(
                        canvas(
                            |_bounds, _window, _cx| (),
                            move |bounds, (), window, cx| {
                                if let Some(Some(view)) = window.root::<ReceiverView>() {
                                    view.update(cx, |this, _| this.paint_preview(bounds, window));
                                }
                            },
                        )
                        // Without an explicit size the canvas lays out at 0×0,
                        // which paints nothing and breaks the coordinate mapping.
                        .size_full(),
                    ),
            )
    }

    /// Default view: what is being received, and what already arrived.
    fn render_body(&self) -> Div {
        let mut body = div()
            .flex()
            .flex_col()
            .flex_grow()
            .gap_4()
            .p_4()
            .overflow_hidden();

        body = body.child(self.render_progress_card());

        // Received files — the actual useful content of this window.
        let mut list = div().flex().flex_col().gap_1();
        if self.transfers.is_empty() {
            list = list.child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(0x6a6a6e))
                    .child("还没有收到文件。发送端开始播放二维码后，文件会自动出现在这里，并保存到 ~/Downloads。"),
            );
        } else {
            for item in self.transfers.iter().rev().take(12) {
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x161b22))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgb(0x3fb950))
                                .child(format!("✓ {}", item.filename)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x8b949e))
                                .child(crate::notify::human_bytes(item.size)),
                        )
                        .child(
                            div()
                                .flex_grow()
                                .text_size(px(11.))
                                .text_color(rgb(0x6a6a6e))
                                .child(item.path.display().to_string()),
                        )
                        .when(!item.written, |row| {
                            row.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x8b949e))
                                    .child("（已存在，未重复保存）"),
                            )
                        }),
                );
            }
        }

        body.child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x8b949e))
                        .child("已接收文件"),
                )
                .child(list),
        )
    }

    fn render_progress_card(&self) -> Div {
        let card = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(0x161b22))
            .border_1()
            .border_color(rgb(0x21262d));

        // Finished transfer: green confirmation instead of a progress bar.
        if let Some(done) = self.transfers.last() {
            return card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(0x3fb950))
                            .child(format!("✓ 已接收 {}", done.filename)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0x8b949e))
                            .child(crate::notify::human_bytes(done.size)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x6a6a6e))
                            .child(done.path.display().to_string()),
                    ),
            );
        }

        let pct = (self.progress * 100.0).round() as u32;
        let mut card = card.child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(0xd6d6d6))
                        .child(if self.progress > 0.0 {
                            self.note.clone()
                        } else {
                            "等待二维码…（把发送端窗口置于可见位置）".into()
                        }),
                )
                .child(div().flex_grow())
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(0x58a6ff))
                        .child(format!("{pct}%")),
                ),
        );

        // Bar: overall progress.
        card = card.child(progress_bar(1.0, self.progress, 0x58a6ff));

        // Bar per RaptorQ source block (only when there is more than one).
        if self.blocks.len() > 1 {
            let mut row = div().flex().flex_row().gap_3();
            for (block, have, total) in &self.blocks {
                let fraction = if *total == 0 { 0.0 } else { *have as f64 / *total as f64 };
                row = row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .flex_grow()
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(0x6a6a6e))
                                .child(format!("block {block} {have}/{total}")),
                        )
                        .child(progress_bar(fraction, fraction, 0x2ea043)),
                );
            }
            card = card.child(row);
        }

        card
    }

    fn render_footer(&self) -> Div {
        let mut footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(rgb(0x21262d))
            .text_size(px(11.))
            .text_color(rgb(0x6a6a6e));

        if self.capture_black {
            footer = footer.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0xff7b72))
                    .child(
                        "⚠ 抓到的是全黑画面 —— 到 系统设置 → 隐私与安全性 → 屏幕录制 勾选 \
                         RaptorQR Receiver，再完全退出并重开本 App",
                    ),
            );
        } else if let Some(err) = &self.last_error {
            footer = footer.child(div().text_size(px(12.)).text_color(rgb(0xff7b72)).child(err.clone()));
        } else {
            footer = footer.child(div().child(
                "自动扫描所有显示器；需要缩小范围时点右上角「框选区域」。文件保存到 ~/Downloads。",
            ));
        }

        footer
    }
}

/// A rounded progress bar of width `span` (0..=1) filled to `fraction`.
fn progress_bar(span: f64, fraction: f64, colour: u32) -> Div {
    let span = span.clamp(0.0, 1.0);
    let filled = (span * fraction.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
    div()
        .flex()
        .flex_row()
        .w_full()
        .h(px(6.))
        .rounded_full()
        .bg(rgb(0x21262d))
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(rgb(colour))
                .w(relative(filled as f32)),
        )
}


fn button(label: impl Into<SharedString>) -> Div {
    let label = label.into();
    div()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(0x21262d))
        .hover(|s| s.bg(rgb(0x30363d)))
        .text_size(px(12.))
        .text_color(rgb(0xc9d1d9))
        .cursor_pointer()
        .child(label)
}

fn primary_button(label: impl Into<SharedString>) -> Div {
    let label = label.into();
    div()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(0x1f6feb))
        .hover(|s| s.bg(rgb(0x388bfd)))
        .text_size(px(12.))
        .text_color(rgb(0xffffff))
        .cursor_pointer()
        .child(label)
}
