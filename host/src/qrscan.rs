//! QR symbol decoding via rxing (pure-Rust ZXing port).

use rxing::helpers::detect_multiple_in_luma_with_hints;
use rxing::BarcodeFormat;

/// Decode every QR code in a luma image; returns their raw payload bytes
/// (binary-safe, unlike `getText`).
pub fn scan_qrs(luma: &[u8], width: u32, height: u32) -> Vec<Vec<u8>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut hints = rxing::DecodeHints::default();
    hints.PossibleFormats = Some(std::collections::HashSet::from([BarcodeFormat::QR_CODE]));
    // TryHarder helps with slightly blurry screen captures.
    hints.TryHarder = Some(true);

    match detect_multiple_in_luma_with_hints(luma.to_vec(), width, height, &mut hints) {
        Ok(results) => results
            .into_iter()
            .map(|r| r.getRawBytes().to_vec())
            .filter(|b| !b.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}
