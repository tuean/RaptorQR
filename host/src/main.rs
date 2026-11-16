//! RaptorQR receiver entry point.
//!
//! All platforms: a window (pass `--headless` for the console receiver).
//! macOS/Linux use gpui; Windows uses iced with the CPU rasterizer, because gpui
//! has no Windows backend and the Windows receiver may have no usable GPU.

#[cfg(not(windows))]
use gpui::*;

fn main() {
    // DPI awareness has to be set before any window (or the first capture)
    // exists: otherwise GDI reports and grabs the desktop scaled to 96 DPI and
    // every region is off by the scale factor.
    #[cfg(windows)]
    raptorqr_host::capture_win::dpi_aware();

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

    if has("--headless") {
        raptorqr_host::console::run();
        return;
    }

    // Windows: iced on the software renderer. If the window cannot be created
    // at all (broken display driver, no interactive session), fall back to the
    // console receiver instead of dying with a message nobody sees.
    #[cfg(windows)]
    {
        // `--once` and `--out` only exist to make a run scriptable, and the
        // window has no equivalent; keeping them on the console path is what
        // makes "same command, same behaviour" hold after the GUI arrived.
        if has("--once") || has("--out") {
            raptorqr_host::console::run();
            return;
        }
        if let Err(error) = raptorqr_host::ui_win::run() {
            eprintln!("receiver window failed to start: {error}");
            eprintln!("falling back to the console receiver (--headless)");
            raptorqr_host::console::run();
        }
        return;
    }

    #[cfg(not(windows))]
    gui_main();
}

#[cfg(not(windows))]
fn gui_main() {
    let application = Application::new();

    // Closing the window destroys it (and the view, which stops the capture
    // thread). macOS keeps the app alive in the dock, so a second launch --
    // clicking the dock icon -- has to build a new window; without this the
    // icon just bounced and no window was ever created again.
    application.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_receiver_window(cx);
        } else {
            for handle in cx.windows() {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
                break;
            }
        }
        cx.activate(true);
    });

    application.run(|cx: &mut App| {
        open_receiver_window(cx);
    });
}

#[cfg(not(windows))]
fn open_receiver_window(cx: &mut App) {
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
}
