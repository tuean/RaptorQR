//! QR symbol decoding via rxing (pure-Rust ZXing port).

use rxing::helpers::detect_multiple_in_luma_with_hints;
use rxing::BarcodeFormat;

/// Decode every QR code in a luma image; returns their raw payload bytes
/// (binary-safe, unlike `getText`).
///
/// Takes the buffer by value so the caller can hand over its luma plane without
/// a full copy. `try_harder` costs real time (it retries rotated/blurred
/// variants), so the cheap probe path passes `false` and only the full-speed
/// path pays for it.
pub fn scan_qrs(luma: Vec<u8>, width: u32, height: u32, try_harder: bool) -> Vec<Vec<u8>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut hints = rxing::DecodeHints::default();
    hints.PossibleFormats = Some(std::collections::HashSet::from([BarcodeFormat::QR_CODE]));
    // TryHarder helps with slightly blurry screen captures.
    hints.TryHarder = Some(try_harder);

    match detect_multiple_in_luma_with_hints(luma, width, height, &mut hints) {
        Ok(results) => results
            .into_iter()
            .map(|r| r.getRawBytes().to_vec())
            .filter(|b| !b.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}
