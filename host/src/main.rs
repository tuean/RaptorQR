//! RaptorQR host receiver: capture QR codes from a screen region, decode the
//! RaptorQR transport protocol, and reconstruct files — no camera needed.

use gpui::*;

fn main() {
    // `--check` diagnoses the capture path (screen-recording permission, QR
    // visibility) without opening a window.
    if std::env::args().any(|a| a == "--check") {
        check_capture();
        return;
    }

    // `--notify-test` fires the completion notification with sample data, so
    // notification permission can be verified without running a transfer.
    if std::env::args().any(|a| a == "--notify-test") {
        raptorqr_host::notify::notify_transfer_received(
            "示例文件.bin",
            "~/Downloads/示例文件.bin",
            1_500_000,
        );
        std::thread::sleep(std::time::Duration::from_millis(1500));
        println!("notification sent (no output means osascript accepted it)");
        return;
    }

    Application::new().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(900.), px(560.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("RaptorQR Receiver".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.), px(480.))),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| raptorqr_host::ui::ReceiverView::new(cx)),
        )
        .expect("failed to open the receiver window");
    });
}

/// Capture each display, report statistics, and list any QR codes found.
fn check_capture() {
    use raptorqr_host::capture::{capture_display, crop, display_list, rgba_to_luma};
    use raptorqr_host::qrscan::scan_qrs;

    let displays = display_list();
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

    let region = region_arg();
    let only = display_arg();
    let mut total_qr = 0usize;
    let mut any_pixels = false;

    for display in &displays {
        if let Some(only) = only {
            if display.index != only {
                continue;
            }
        }
        println!("\n[display {}] {}", display.index + 1, display.name);

        let Some((rgba, w, h)) = capture_display(display.index) else {
            println!("  ✗ capture failed — Screen Recording permission?");
            continue;
        };
        any_pixels = true;

        let (rgba, w, h) = match &region {
            Some(r) => {
                let (cropped, cw, ch) = crop(&rgba, w, h, r).expect("empty region");
                println!("  region {}×{} at ({}, {})", cw, ch, r.x, r.y);
                (cropped, cw, ch)
            }
            None => (rgba, w, h),
        };

        let luma = rgba_to_luma(&rgba);
        let mean = luma.iter().map(|&v| v as u64).sum::<u64>() / luma.len().max(1) as u64;
        let non_black = luma.iter().filter(|&&v| v > 8).count() * 100 / luma.len().max(1);
        println!("  frame: {w}×{h} px, mean luma {mean}, {non_black}% non-black");
        if mean == 0 || non_black == 0 {
            println!("  ✗ frame is black — grant Screen Recording permission for this app");
            continue;
        }

        let packets = scan_qrs(&luma, w, h);
        total_qr += packets.len();
        println!("  QR codes decoded: {}", packets.len());
        for (i, packet) in packets.iter().enumerate() {
            match raptorqr_host::protocol::parse_packet(packet) {
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

/// Parse `--display N` (1-based, as printed by `--check`).
fn display_arg() -> Option<usize> {
    let mut args = std::env::args().skip_while(|a| a != "--display").skip(1);
    let value = args.next()?;
    match value.parse::<usize>() {
        Ok(n) if n >= 1 => Some(n - 1),
        _ => {
            eprintln!("--display expects a 1-based display number");
            std::process::exit(2);
        }
    }
}

/// Parse `--region x,y,w,h` (pixels of the display it applies to).
fn region_arg() -> Option<raptorqr_host::capture::ScreenRect> {
    let mut args = std::env::args().skip_while(|a| a != "--region").skip(1);
    let value = args.next()?;
    let parts: Vec<u32> = value
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if parts.len() != 4 {
        eprintln!("--region expects x,y,w,h");
        std::process::exit(2);
    }
    Some(raptorqr_host::capture::ScreenRect {
        x: parts[0],
        y: parts[1],
        w: parts[2],
        h: parts[3],
    })
}
