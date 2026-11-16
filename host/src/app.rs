//! Capture + decode worker and the event/state types it shares with a UI shell.
//!
//! Nothing here depends on a GUI library: the gpui shell (`ui.rs`, macOS/Linux)
//! and the iced shell (`ui_win.rs`, Windows) both drive the same worker, which is
//! what keeps the two front ends from drifting apart.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture::ScreenRect;
use crate::scanner::{ScanConfig, Scanner};
use crate::transfer::{save_completed, Delivery, TransferTracker};

// ─── Shared state between the UI and the capture worker ─────────────────────

pub struct SharedState {
    /// Decode region: (display index, rect in that display's pixels).
    pub region: Mutex<Option<(usize, ScreenRect)>>,
    pub running: AtomicBool,
    /// Which display the preview shows (regions are selected on it).
    pub preview_display: AtomicUsize,
    pub display_count: AtomicUsize,
    /// Set when the UI wants a preview frame immediately.
    pub preview_request: AtomicBool,
    /// True while the region picker is open. The worker only builds preview
    /// frames then — they have no other consumer.
    pub selecting: AtomicBool,
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
        /// Set for text transfers: the decoded text, which the UI thread puts on
        /// the clipboard. Text is never written to disk.
        text: Option<String>,
    },
    CaptureError(String),
}

/// Last committed region, kept for the lifetime of the process.
///
/// The window (and with it this view) is destroyed when it is closed, and the
/// dock icon builds a fresh one; without this a reopen reset the decode region
/// and the same corner had to be selected again by hand.
pub static LAST_REGION: Mutex<Option<(usize, ScreenRect)>> = Mutex::new(None);

/// Lines of a received text shown in the card; the clipboard gets all of it.
pub const TEXT_CARD_LINES: usize = 8;
/// Status events drive the repaint, so this is also the idle redraw rate: twice
/// a second is plenty for a rate readout and halves the UI's CPU share.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(500);

/// Capture + decode loop, runs on a background thread.
///
/// Set `RAPTORQR_DEBUG=1` to trace capture/decode progress on stderr.
pub fn run_worker(shared: Arc<SharedState>, tx: mpsc::Sender<WorkerEvent>) {
    let debug = std::env::var_os("RAPTORQR_DEBUG").is_some();
    let mut tracker = TransferTracker::new();
    let mut scanner = Scanner::new();
    let mut last_status = Instant::now();
    let mut status_frames = 0u64;
    let mut packets_decoded = 0u64;
    let mut qr_symbols = 0u64;

    loop {
        if !shared.running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let tick = Instant::now();

        let region = *shared.region.lock().unwrap();
        let preview_display = shared.preview_display.load(Ordering::Relaxed);
        let cfg = ScanConfig {
            region,
            only_display: None,
            preview_display,
            want_preview: shared.selecting.load(Ordering::Relaxed),
            force_preview: shared.preview_request.swap(false, Ordering::Relaxed),
        };

        // One cadence tick: probe a single display while idle, or follow the
        // display that carries the live QR stream at full rate.
        let out = scanner.tick(tick, &cfg);
        let display_count = out.display_count.max(1);
        shared.display_count.store(display_count, Ordering::Relaxed);
        status_frames += out.frames as u64;
        qr_symbols += out.symbols.len() as u64;

        if let Some(preview) = out.preview {
            let _ = tx.send(WorkerEvent::Preview {
                display: preview.display,
                display_count: preview.display_count,
                rgba: preview.rgba,
                w: preview.w,
                h: preview.h,
                full_w: preview.full_w,
                full_h: preview.full_h,
            });
        }

        if out.frames == 0 {
            // Same failure, different cause: macOS hides frames until the app is
            // granted Screen Recording, while GDI on Windows simply returns an
            // empty bitmap (locked session, exclusive full-screen, DRM).
            let hint = if cfg!(windows) {
                "screen capture failed — GDI returned an empty frame"
            } else {
                "screen capture failed — grant Screen Recording permission?"
            };
            let _ = tx.send(WorkerEvent::CaptureError(hint.into()));
            std::thread::sleep(Duration::from_millis(1000));
        }

        let found = out.symbols;
        if debug && !found.is_empty() {
            let (progress, note) = match tracker.status() {
                Some(status) if status.complete.is_some() => (100.0, "complete (stream looping)"),
                Some(status) => (status.progress * 100.0, "receiving"),
                None => (0.0, "idle"),
            };
            eprintln!(
                "[recv] {} qr_found={} packets={} progress={progress:.1}% {note} session={} key={:?} esi={:?}",
                scanner.mode().label(),
                found.len(),
                packets_decoded,
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
                    match save_completed(&t, None) {
                        Some((path, written)) => {
                            tracker.mark_saved(path.clone(), t.body.len());
                            if debug {
                                eprintln!(
                                    "[recv] COMPLETE {} → {} ({} bytes, {})",
                                    t.filename,
                                    path.display(),
                                    t.body.len(),
                                    if written {
                                        "saved"
                                    } else {
                                        "identical file already there"
                                    }
                                );
                            }
                            // Announce every receipt — including one
                            // whose identical file is already on disk,
                            // otherwise the user sees nothing at all.
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
                                text: None,
                            });
                        }
                        // Text never touches the disk: the UI thread
                        // puts it on the clipboard and shows it.
                        None => {
                            let text = t.as_text().unwrap_or_default();
                            if debug {
                                eprintln!(
                                    "[recv] COMPLETE text ({} bytes, {} chars)",
                                    t.body.len(),
                                    text.chars().count()
                                );
                            }
                            crate::notify::notify_text_received(&text);
                            let _ = tx.send(WorkerEvent::Transfer {
                                filename: t.filename,
                                path: PathBuf::new(),
                                size: t.body.len(),
                                written: true,
                                text: Some(text),
                            });
                        }
                    }
                }
            }
        }

        // Status (throttled).
        if tick.duration_since(last_status) >= STATUS_INTERVAL {
            let elapsed_status = tick.duration_since(last_status).as_secs_f64();
            let fps = status_frames as f64 / elapsed_status;
            let qr_per_second = (qr_symbols as f64 / elapsed_status * 10.0).round() / 10.0;
            let (progress, blocks, note) = match tracker.status() {
                Some(status) => match status.complete {
                    Some((label, path, bytes, delivery)) => (
                        1.0,
                        status.blocks,
                        match delivery {
                            // Text is pasted, not filed.
                            Delivery::Clipboard => {
                                format!("已完成：{label} 已复制到剪贴板 — 发送端仍在循环播放")
                            }
                            Delivery::File => format!(
                                "已完成：{label} ({} B) 已保存到 {} — 发送端仍在循环播放",
                                crate::notify::human_bytes(bytes),
                                path.parent()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default()
                            ),
                        },
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
                    preview_display.min(display_count - 1) + 1,
                    display_count
                ),
            };
            let _ = tx.send(WorkerEvent::Status {
                fps: (fps * 10.0).round() / 10.0,
                packets: packets_decoded,
                qr_per_second,
                scanning,
                capture_black: scanner.capture_black(),
                progress,
                blocks,
                note,
            });
            status_frames = 0;
            qr_symbols = 0;
            last_status = tick;
        }

        let elapsed = tick.elapsed();
        let period = scanner.interval();
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }
    }
}

// ─── UI view ────────────────────────────────────────────────────────────────

/// One row of the transfer log. Shared by both UI shells; the shapes are the
/// same, only the widgets differ.
#[derive(Clone)]
pub struct TransferItem {
    pub filename: String,
    pub path: PathBuf,
    pub size: usize,
    /// false = identical file already existed, nothing new was written
    pub written: bool,
    /// Decoded text of a text transfer (`None` for files).
    pub text: Option<String>,
    /// Whether the text reached the clipboard.
    pub copied: bool,
}
