//! Headless receiver: the same capture → QR → RaptorQ → save pipeline as the
//! gpui UI, without a window.
//!
//! This is the **Windows** receiver (gpui has no Windows backend) and doubles
//! as `--headless` on macOS/Linux for VMs, servers and tests.
//!
//! ```text
//! raptorqr-host --headless [--out DIR] [--display N] [--region x,y,w,h] [--once]
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::capture::{self, ScreenRect};
use crate::notify::human_bytes;
use crate::qrscan::scan_qrs;
use crate::transfer::{save_to_dir, save_to_downloads, FeedResult, TransferTracker};

/// Minimum gap between QR decodes of one display.
const DECODE_INTERVAL: Duration = Duration::from_millis(100);
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
/// Do not spin when capture returns nothing (no display / no permission).
const IDLE_SLEEP: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Clone)]
pub struct Args {
    /// Where received files go; `None` = the user's Downloads folder.
    pub out_dir: Option<PathBuf>,
    /// Scan only this region of `display` (physical pixels, monitor-local).
    pub region: Option<ScreenRect>,
    /// Scan only this display (0-based, as listed by `--check`).
    pub display: Option<usize>,
    /// Exit after the first file is received (scripting/tests).
    pub once: bool,
}

/// Run the headless receiver until the user stops it (or `--once` completes).
pub fn run() {
    let args = parse_args();
    if let Some(dir) = &args.out_dir {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!("✗ cannot create {}: {err}", dir.display());
            std::process::exit(1);
        }
    }

    let dest = match &args.out_dir {
        Some(dir) => dir.display().to_string(),
        None => "~/Downloads".into(),
    };
    println!("RaptorQR receiver — headless");
    println!("scanning: {}", scan_scope(&args));
    println!("saving to: {dest}");
    println!("press Ctrl-C to stop\n");

    let mut tracker = TransferTracker::new();
    let mut last_decode: Vec<Instant> = Vec::new();
    let mut packets = 0u64;
    let mut symbols = 0u64;
    let mut last_status = Instant::now();
    let mut status_symbols = 0u64;
    let mut black_note = false;

    'receive: loop {
        let tick = Instant::now();
        let displays = capture::display_list();
        if displays.is_empty() {
            eprintln!("✗ no display to capture — is this a headless session?");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let mut captured_any = false;
        for display in &displays {
            // A configured region always wins over `--display`: it can only
            // live on one display, so the others are not scanned at all.
            match (args.region, args.display) {
                (Some(_), _) | (None, Some(_)) => {
                    let wanted = args.display.unwrap_or(display.index);
                    if display.index != wanted {
                        continue;
                    }
                }
                (None, None) => {}
            }

            let Some((rgba, w, h)) = capture::capture_display(display.index) else {
                continue;
            };
            captured_any = true;

            let full = ScreenRect { x: 0, y: 0, w, h };
            let scope = args
                .region
                .map(|rect| rect.intersect(&full))
                .unwrap_or(full);
            if scope.area() == 0 {
                continue;
            }

            if last_decode.len() <= display.index {
                // Start "due" so a new display is decoded immediately.
                let due = tick.checked_sub(DECODE_INTERVAL).unwrap_or(tick);
                last_decode.resize(display.index + 1, due);
            }
            if tick.duration_since(last_decode[display.index]) < DECODE_INTERVAL {
                continue;
            }
            last_decode[display.index] = tick;

            let Some((crgba, cw, ch)) = capture::crop(&rgba, w, h, &scope) else {
                continue;
            };
            let luma = capture::rgba_to_luma(&crgba);

            // Sampled brightness: black frames mean the OS is not handing over
            // the screen (missing macOS Screen Recording permission).
            let samples = luma.len() / 97 + 1;
            let mean = luma.iter().step_by(97).map(|&v| v as u64).sum::<u64>() / samples as u64;
            if mean < 2 && !black_note {
                black_note = true;
                println!("{}", black_frame_hint());
            }

            let found = scan_qrs(&luma, cw, ch);
            symbols += found.len() as u64;
            for raw in found {
                match tracker.feed(&raw) {
                    FeedResult::Ignored | FeedResult::Duplicate => {}
                    FeedResult::Progress => packets += 1,
                    FeedResult::Completed(transfer) => {
                        packets += 1;
                        let (path, written) = match &args.out_dir {
                            Some(dir) => save_to_dir(dir, &transfer.filename, &transfer.body),
                            None => save_to_downloads(&transfer.filename, &transfer.body),
                        };
                        tracker.mark_saved(path.clone(), transfer.body.len());
                        println!(
                            "\n✓ 已接收 {} ({}) → {}{}",
                            transfer.filename,
                            human_bytes(transfer.body.len()),
                            path.display(),
                            if written {
                                ""
                            } else {
                                "  [内容相同，未重复写入]"
                            }
                        );
                        if written {
                            crate::notify::notify_transfer_received(
                                &transfer.filename,
                                &path.display().to_string(),
                                transfer.body.len(),
                            );
                        }
                        if args.once {
                            break 'receive;
                        }
                    }
                }
            }
        }

        if !captured_any {
            eprintln!("✗ screen capture failed — {}", capture_failure_hint());
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        if tick.duration_since(last_status) >= STATUS_INTERVAL {
            let seconds = tick.duration_since(last_status).as_secs_f64();
            let rate = (symbols - status_symbols) as f64 / seconds;
            status_symbols = symbols;
            last_status = tick;
            let progress = match tracker.status() {
                Some(status) if status.complete.is_some() => "100% (stream looping)".to_string(),
                Some(status) => format!("{:.0}%", status.progress * 100.0),
                None => "waiting for QR".into(),
            };
            print!("\r[{progress}] symbols {symbols} ({rate:.1}/s) · packets {packets}   ");
            let _ = std::io::stdout().flush();
        }

        std::thread::sleep(IDLE_SLEEP);
    }
}

/// What the receiver is scanning, for the banner.
fn scan_scope(args: &Args) -> String {
    match (args.region, args.display) {
        (Some(rect), _) => format!(
            "display {} region {}×{} at ({}, {})",
            args.display.unwrap_or(0) + 1,
            rect.w,
            rect.h,
            rect.x,
            rect.y
        ),
        (None, Some(index)) => format!("display {} (full screen)", index + 1),
        (None, None) => "all displays (full screen)".into(),
    }
}

fn black_frame_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "⚠ 捕获到的画面是全黑 —— 请在「系统设置 → 隐私与安全性 → 屏幕录制」里授权本程序"
    } else {
        "⚠ 捕获到的画面是全黑 —— 可能是硬件加速/受保护内容的窗口，换个窗口或用 --region 缩小范围试试"
    }
}

fn capture_failure_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "请授予屏幕录制权限（系统设置 → 隐私与安全性 → 屏幕录制）"
    } else {
        "确认程序有权访问桌面（Windows 下若在受保护进程/远程会话中运行会失败）"
    }
}

// ─── --check: diagnose the capture path without receiving anything ──────────

/// Capture every display, print frame statistics, and list decoded QR codes.
pub fn check_capture() {
    let args = parse_args();
    let displays = capture::display_list();
    if displays.is_empty() {
        eprintln!("✗ no display found");
        std::process::exit(1);
    }

    println!("displays: {}", displays.len());
    for d in &displays {
        println!(
            "  [{}] {} — {}×{} logical{}",
            d.index + 1,
            d.name,
            d.logical_w,
            d.logical_h,
            if d.primary { " (primary)" } else { "" }
        );
    }

    let mut total_qr = 0usize;
    let mut any_pixels = false;

    for display in &displays {
        if let Some(only) = args.display {
            if display.index != only {
                continue;
            }
        }
        println!("\n[display {}] {}", display.index + 1, display.name);

        let Some((rgba, w, h)) = capture::capture_display(display.index) else {
            println!("  ✗ capture failed — {}", capture_failure_hint());
            continue;
        };
        any_pixels = true;

        let (rgba, w, h) = match &args.region {
            Some(rect) => {
                let Some((cropped, cw, ch)) = capture::crop(&rgba, w, h, rect) else {
                    println!(
                        "  ✗ region {}×{} at ({}, {}) is empty",
                        rect.w, rect.h, rect.x, rect.y
                    );
                    continue;
                };
                println!("  region {}×{} at ({}, {})", cw, ch, rect.x, rect.y);
                (cropped, cw, ch)
            }
            None => (rgba, w, h),
        };

        let luma = capture::rgba_to_luma(&rgba);
        let mean = luma.iter().map(|&v| v as u64).sum::<u64>() / luma.len().max(1) as u64;
        let non_black = luma.iter().filter(|&&v| v > 8).count() * 100 / luma.len().max(1);
        println!("  frame: {w}×{h} px, mean luma {mean}, {non_black}% non-black");
        if mean == 0 || non_black == 0 {
            println!("  ✗ {}", black_frame_hint());
            continue;
        }

        let packets = scan_qrs(&luma, w, h);
        total_qr += packets.len();
        println!("  QR codes decoded: {}", packets.len());
        for (i, packet) in packets.iter().enumerate() {
            match crate::protocol::parse_packet(packet) {
                Ok(p) => {
                    // The 4-byte RaptorQ Payload ID is block number + 24-bit ESI.
                    let (block, esi) = if p.payload.len() >= 4 {
                        (
                            p.payload[0],
                            ((p.payload[1] as u32) << 16)
                                | ((p.payload[2] as u32) << 8)
                                | p.payload[3] as u32,
                        )
                    } else {
                        (0, 0)
                    };
                    println!(
                        "    #{i}: block {block} esi {esi} · {} B payload, dataLength {}, compressed {}, text {}",
                        p.payload.len(),
                        p.header.data_length,
                        p.header.compressed,
                        p.header.is_text
                    );
                }
                Err(err) => println!("    #{i}: not a RaptorQR packet ({err:?})"),
            }
        }
    }

    if !any_pixels {
        eprintln!("\n✗ no display could be captured");
        std::process::exit(1);
    }
    if total_qr == 0 {
        println!("\n(no QR codes on screen right now — start a transfer and re-run)");
    }
}

// ─── flags ─────────────────────────────────────────────────────────────────

fn parse_args() -> Args {
    let mut args = Args::default();
    for arg in std::env::args().skip(1) {
        if arg == "--once" {
            args.once = true;
        }
    }
    args.region = region_arg();
    args.display = display_arg();
    args.out_dir = value_arg("--out").map(PathBuf::from);
    args
}

/// Value of `--flag value` (or `--flag=value`).
fn value_arg(flag: &str) -> Option<String> {
    let eq = format!("{flag}=");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix(&eq) {
            return Some(value.to_string());
        }
        if arg == flag {
            return args.next();
        }
    }
    None
}

/// `--display N` — 1-based, matching the numbering printed by `--check`.
fn display_arg() -> Option<usize> {
    let value = value_arg("--display")?;
    match value.parse::<usize>() {
        Ok(n) if n >= 1 => Some(n - 1),
        _ => {
            eprintln!("--display expects a 1-based display number");
            std::process::exit(2);
        }
    }
}

/// `--region x,y,w,h` — pixels of the display it applies to.
fn region_arg() -> Option<ScreenRect> {
    let value = value_arg("--region")?;
    let parts: Vec<u32> = value
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if parts.len() != 4 {
        eprintln!("--region expects x,y,w,h");
        std::process::exit(2);
    }
    Some(ScreenRect {
        x: parts[0],
        y: parts[1],
        w: parts[2],
        h: parts[3],
    })
}
