//! RaptorQR host receiver library: screen capture, QR decoding, and RaptorQ
//! transport protocol decoding (a Rust port of the TS core protocol).

pub mod app;
pub mod capture;
pub mod clipboard;
pub mod console;
pub mod notify;
pub mod protocol;
pub mod qrscan;
pub mod raptorq;
pub mod scanner;
pub mod transfer;

#[cfg(windows)]
pub mod capture_win;

#[cfg(not(windows))]
pub mod ui;

/// iced receiver shell: CPU-rasterized (tiny-skia + softbuffer → GDI), so it
/// works on Windows machines without a usable GPU. gpui has no Windows backend.
#[cfg(windows)]
pub mod ui_win;
