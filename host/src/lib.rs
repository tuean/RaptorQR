//! RaptorQR host receiver library: screen capture, QR decoding, and RaptorQ
//! transport protocol decoding (a Rust port of the TS core protocol).

pub mod capture;
pub mod console;
pub mod notify;
pub mod protocol;
pub mod qrscan;
pub mod raptorq;
pub mod transfer;

#[cfg(not(windows))]
pub mod ui;
