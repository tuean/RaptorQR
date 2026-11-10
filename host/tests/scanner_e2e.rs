//! The scan cadence against a replaying screen: the first QR must be found by
//! the cheap probe, and the scanner must then switch to the full-rate path.
//!
//! `RAPTORQR_FAKE_SCREEN` replays captured sender frames instead of grabbing the
//! screen, so this covers the state machine without a display or permission.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use raptorqr_host::scanner::{ScanConfig, Scanner, ACTIVE_INTERVAL, Mode};

fn frames_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/singlefile/frames")
}

fn config(want_preview: bool) -> ScanConfig {
    ScanConfig {
        region: None,
        only_display: Some(0),
        preview_display: 0,
        want_preview,
        force_preview: false,
    }
}

#[test]
fn probe_finds_the_stream_and_ramps_to_full_rate() {
    let dir = frames_dir();
    if !dir.is_dir() {
        eprintln!(
            "skipping: generate fixtures with\n  \
             node scripts/capture_singlefile_frames.mjs <app.html> tests/fixtures/singlefile"
        );
        return;
    }
    // This test binary has a single test, so nothing else observes the process
    // environment concurrently.
    std::env::set_var("RAPTORQR_FAKE_SCREEN", &dir);

    let mut scanner = Scanner::new();
    assert_eq!(scanner.mode(), Mode::Probe);

    let t0 = Instant::now();
    let mut decoded = 0usize;
    let mut ticks = 0;
    // The probe visits one display per tick; the fake screen is display 0.
    while decoded == 0 && ticks < 8 {
        let out = scanner.tick(t0 + Duration::from_millis(250 * ticks), &config(false));
        assert_eq!(out.frames, 1, "the fake screen must always yield a frame");
        assert_eq!(out.display_count, 1, "the fake screen is one display");
        assert!(
            out.preview.is_none(),
            "no preview frame when the picker is closed"
        );
        decoded += out.symbols.len();
        ticks += 1;
    }

    assert!(decoded > 0, "the probe never decoded a QR from the fixtures");
    assert_eq!(
        scanner.mode(),
        Mode::Active,
        "a decoded QR must switch the scanner to the full-rate path"
    );
    assert_eq!(scanner.interval(), ACTIVE_INTERVAL);

    // A preview is only built on request — that is what keeps the idle path from
    // rendering frames nobody looks at.
    let out = scanner.tick(t0 + Duration::from_secs(1), &config(true));
    let preview = out.preview.expect("preview requested but none produced");
    assert_eq!(preview.display, 0);
    assert!(preview.w > 0 && preview.h > 0);
    assert!(preview.w <= 1024, "preview must be downscaled for the picker");
    assert!(preview.full_w >= preview.w);
    assert!(preview.full_h >= preview.h);
}
