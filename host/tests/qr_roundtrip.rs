//! End-to-end QR path test: real fast_qr-rendered QR frames (same settings as
//! the web sender) → screen-style RGBA → luma → rxing decode → transport
//! packet → RaptorQ decode → original payload.
//!
//! Fixtures come from `node scripts/encode_fixture.mjs`; the test skips when
//! they are absent.

use std::fs;
use std::path::PathBuf;

use raptorqr_host::capture::rgba_to_luma;
use raptorqr_host::protocol::parse_packet;
use raptorqr_host::qrscan::scan_qrs;
use raptorqr_host::transfer::{decode_packets, FeedResult, CompletedTransfer, TransferTracker};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_frames(name: &str) -> Option<Vec<(u32, u32, Vec<u8>)>> {
    let bytes = fs::read(fixtures_dir().join(format!("{name}.frames.bin"))).ok()?;
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let mut frames = Vec::with_capacity(count);
    let mut offset = 4 + 8 * count;
    for i in 0..count {
        let w = u32::from_le_bytes([
            bytes[4 + i * 8],
            bytes[5 + i * 8],
            bytes[6 + i * 8],
            bytes[7 + i * 8],
        ]);
        let h = u32::from_le_bytes([
            bytes[8 + i * 8],
            bytes[9 + i * 8],
            bytes[10 + i * 8],
            bytes[11 + i * 8],
        ]);
        let len = (w * h * 4) as usize;
        frames.push((w, h, bytes[offset..offset + len].to_vec()));
        offset += len;
    }
    Some(frames)
}

/// Decode one rendered frame back to its transport packet bytes.
fn frame_to_packet(w: u32, h: u32, rgba: &[u8]) -> Vec<Vec<u8>> {
    let luma = rgba_to_luma(rgba);
    let results = scan_qrs(&luma, w, h);
    for r in &results {
        assert!(
            parse_packet(r).is_ok(),
            "rxing returned bytes that are not a valid transport packet ({} bytes)",
            r.len()
        );
    }
    results
}

/// Full chain for the small text fixture: every frame decoded and fed in order
/// must reconstruct the original text byte-for-byte.
#[test]
fn qr_frames_reconstruct_text_transfer() {
    let Some(frames) = load_frames("text") else {
        eprintln!("skipping: run `node scripts/encode_fixture.mjs` to generate fixtures");
        return;
    };
    let expected = fs::read(fixtures_dir().join("text.expected.bin")).unwrap();

    let mut tracker = TransferTracker::new();
    let mut completed: Option<CompletedTransfer> = None;
    for (i, (w, h, rgba)) in frames.iter().enumerate() {
        let packets = frame_to_packet(*w, *h, rgba);
        assert_eq!(packets.len(), 1, "frame {i}: expected exactly one QR code");
        if let FeedResult::Completed(t) = tracker.feed(&packets[0]) {
            completed = Some(t);
        }
    }

    let transfer = completed.expect("transfer did not complete from rendered QR frames");
    assert_eq!(transfer.body, expected, "reconstructed bytes mismatch");
    assert!(transfer.is_text);
    println!("qr text: ok ({} frames, {} bytes)", frames.len(), expected.len());
}

/// The binary fixture frames must survive the QR round trip byte-for-byte
/// (rxing raw-byte output, not lossy text decoding).
#[test]
fn qr_frames_preserve_binary_packets() {
    let Some(frames) = load_frames("file") else {
        eprintln!("skipping: run `node scripts/encode_fixture.mjs` to generate fixtures");
        return;
    };
    let packets_raw = fs::read(fixtures_dir().join("file.packets.bin")).unwrap();
    let packet_len =
        u32::from_le_bytes([packets_raw[0], packets_raw[1], packets_raw[2], packets_raw[3]])
            as usize;
    let originals: Vec<&[u8]> = packets_raw[4..].chunks(packet_len).collect();

    for (i, (w, h, rgba)) in frames.iter().enumerate() {
        let decoded = frame_to_packet(*w, *h, rgba);
        assert_eq!(decoded.len(), 1, "frame {i}: expected exactly one QR code");
        assert_eq!(
            decoded[0], originals[i],
            "frame {i}: QR-decoded packet differs from the encoded packet"
        );
    }
    println!("qr binary: ok ({} frames, byte-exact)", frames.len());
}

/// Decoding all frames in one batch keeps working (the tracker is
/// order-independent for RaptorQ).
#[test]
fn qr_frames_batch_decode_matches_stream() {
    let Some(frames) = load_frames("text") else {
        eprintln!("skipping: run `node scripts/encode_fixture.mjs` to generate fixtures");
        return;
    };
    let packets: Vec<Vec<u8>> = frames
        .iter()
        .flat_map(|(w, h, rgba)| frame_to_packet(*w, *h, rgba))
        .collect();
    let transfer = decode_packets(&packets).expect("batch decode failed");
    let expected = fs::read(fixtures_dir().join("text.expected.bin")).unwrap();
    assert_eq!(transfer.body, expected);
}
