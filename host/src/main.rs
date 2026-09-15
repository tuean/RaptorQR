//! RaptorQR receiver entry point.
//!
//! macOS/Linux: gpui window (pass `--headless` for the console receiver).
//! Windows: always the console receiver — gpui has no Windows backend.

#[cfg(not(windows))]
use gpui::*;

fn main() {
    let has = |flag: &str| std::env::args().any(|a| a == flag);

    // `--check` diagnoses the capture path (screen-recording permission, QR
    // visibility) without opening a window.
    if has("--check") {
        raptorqr_host::console::check_capture();
        return;
    }

    // `--notify-test` fires the completion notification with sample data, so
    // notification permission can be verified without running a transfer.
    if has("--notify-test") {
        raptorqr_host::notify::notify_transfer_received(
            "示例文件.bin",
            "~/Downloads/示例文件.bin",
            1_500_000,
        );
        std::thread::sleep(std::time::Duration::from_millis(1500));
        println!("notification sent (no output means osascript accepted it)");
        return;
    }

    // Windows has no gpui backend, so it always runs headless.
    if cfg!(windows) || has("--headless") {
        raptorqr_host::console::run();
        return;
    }

    #[cfg(not(windows))]
    gui_main();
}

#[cfg(not(windows))]
fn gui_main() {
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
