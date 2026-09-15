//! True end-to-end test: the single-file guest sender (as shipped to the VM)
//! is driven in headless Chromium, its **canvas frames are captured as PNGs**,
//! and the Rust receiver must reconstruct the original payload from them.
//!
//! This covers the whole chain except the OS screen grab itself:
//!   single-file HTML → encode worker → RaptorQ WASM → fast_qr canvas frames
//!   → PNG → luma → rxing QR decode → transport protocol → RaptorQ decode
//!   → inflate/preamble → original bytes
//!
//! Regenerate fixtures with:
//!   node scripts/capture_singlefile_frames.mjs <app.html> tests/fixtures/singlefile

use std::fs;
use std::path::PathBuf;

use raptorqr_host::capture::rgba_to_luma;
use raptorqr_host::qrscan::scan_qrs;
use raptorqr_host::transfer::{FeedResult, TransferTracker};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/singlefile")
}

#[test]
fn single_file_sender_frames_reconstruct_payload() {
    let dir = fixture_dir();
    let payload_path = dir.join("payload.txt");
    if !payload_path.exists() {
        eprintln!(
            "skipping: generate fixtures with\n  \
             node scripts/capture_singlefile_frames.mjs <app.html> tests/fixtures/singlefile"
        );
        return;
    }
    let expected = fs::read(&payload_path).unwrap();

    let frames_dir = dir.join("frames");
    let mut frames: Vec<PathBuf> = fs::read_dir(&frames_dir)
        .expect("frames directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    frames.sort();
    assert!(!frames.is_empty(), "no captured frames");

    let mut tracker = TransferTracker::new();
    let mut completed = None;
    let mut scanned = 0usize;

    for frame in &frames {
        // Screen-capture style input: an RGBA image of the displayed region.
        let img = image::open(frame).expect("decode png").to_rgba8();
        let (w, h) = img.dimensions();
        let luma = rgba_to_luma(img.as_raw());

        let packets = scan_qrs(&luma, w, h);
        scanned += packets.len();
        if packets.is_empty() {
            eprintln!("{}: no QR symbols (blank/first frame?)", frame.display());
            continue;
        }

        for packet in packets {
            if let FeedResult::Completed(t) = tracker.feed(&packet) {
                completed = Some(t);
            }
        }
    }

    let transfer = completed.unwrap_or_else(|| {
        panic!(
            "transfer never completed from {} frames ({} QR symbols decoded)",
            frames.len(),
            scanned
        )
    });
    assert!(
        !transfer.is_text,
        "the payload was sent through the sender's file picker"
    );
    assert_eq!(transfer.filename, "e2e-payload.txt");
    assert_eq!(
        String::from_utf8_lossy(&transfer.body),
        String::from_utf8_lossy(&expected),
        "reconstructed text differs from the payload given to the sender"
    );
    println!(
        "single-file e2e: ok ({} frames, {} QR symbols, {} bytes, filename {})",
        frames.len(),
        scanned,
        transfer.body.len(),
        transfer.filename
    );
}
