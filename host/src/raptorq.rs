//! RaptorQ FEC decode session, mirroring the sender-side wasm wrapper
//! (`packages/raptorqr-raptorq-wasm/src/build_raptorq_wasm_colab.py`).
//!
//! A transport payload is 4 bytes of RaptorQ Payload ID (source block
//! number + 24-bit ESI, big-endian) followed by one `T`-byte symbol, where
//! `T = max_transport_payload_size - 4`.

use std::collections::HashSet;

use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation};

pub const PAYLOAD_ID_BYTES: usize = 4;
pub const MAX_SOURCE_SYMBOLS_PER_BLOCK: u64 = 56_403;

/// Source-block geometry derived from transfer length and symbol size,
/// identical to the wasm wrapper's `raptorq_config`.
pub fn block_geometry(data_len: u64, symbol_size: u64) -> u8 {
    let total_symbols = ceil_div(data_len, symbol_size).max(1);
    let source_blocks = ceil_div(total_symbols, MAX_SOURCE_SYMBOLS_PER_BLOCK).max(1);
    debug_assert!(source_blocks <= u8::MAX as u64, "too many source blocks");
    source_blocks as u8
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        (value - 1) / divisor + 1
    }
}

/// One in-progress transfer: keeps a RaptorQ decoder plus progress bookkeeping.
pub struct RaptorQSession {
    decoder: Decoder,
    symbol_size: u16,
    source_blocks: u8,
    /// Total source symbols across all blocks (for progress display).
    pub total_source_symbols: u64,
    /// (block, esi) pairs already fed (dedupe; the decoder handles dupes, this
    /// keeps the progress counters meaningful).
    seen: HashSet<(u8, u32)>,
    /// Unique symbols received per block, indexed by block number.
    per_block_seen: Vec<u32>,
    per_block_k: Vec<u32>,
    /// Completed once all source blocks decoded.
    pub done: bool,
}

impl RaptorQSession {
    /// Create a session from the transfer metadata carried in the transport
    /// header: exact preprocessed data length and the transport payload size
    /// (4-byte payload id + symbol).
    pub fn new(data_length: u64, transport_payload_size: usize) -> Self {
        assert!(transport_payload_size > PAYLOAD_ID_BYTES);
        let symbol_size = (transport_payload_size - PAYLOAD_ID_BYTES) as u16;
        let source_blocks = block_geometry(data_length, symbol_size as u64);

        let config = ObjectTransmissionInformation::new(
            data_length,
            symbol_size,
            source_blocks,
            1,
            1,
        );
        let total_source_symbols = ceil_div(data_length, symbol_size as u64).max(1);

        // RFC 6330 4.4.1.2 partition: KL = ceil(Kt/Z), KS = KL-1,
        // ZL = Kt - KS*Z, ZS = Z - ZL.
        let z = source_blocks as u64;
        let kt = total_source_symbols;
        let kl = ceil_div(kt, z);
        let ks = kl - 1;
        let zl = kt - ks * z;
        let per_block_k: Vec<u32> = (0..z)
            .map(|i| if i < zl { kl } else { ks } as u32)
            .collect();

        RaptorQSession {
            decoder: Decoder::new(config),
            symbol_size,
            source_blocks,
            total_source_symbols: kt,
            seen: HashSet::new(),
            per_block_seen: vec![0; z as usize],
            per_block_k,
            done: false,
        }
    }

    /// Feed one RaptorQ transport payload (4-byte payload id + symbol).
    /// Returns the fully decoded object once every block is solved.
    pub fn push(&mut self, transport_payload: &[u8]) -> Option<Vec<u8>> {
        if transport_payload.len() != self.symbol_size as usize + PAYLOAD_ID_BYTES {
            return None;
        }
        let block = transport_payload[0];
        let esi = ((transport_payload[1] as u32) << 16)
            | ((transport_payload[2] as u32) << 8)
            | transport_payload[3] as u32;
        if block >= self.source_blocks {
            return None;
        }
        if !self.seen.insert((block, esi)) {
            return None;
        }
        if esi < self.per_block_k[block as usize] {
            self.per_block_seen[block as usize] += 1;
        }

        let packet = EncodingPacket::deserialize(transport_payload);
        if let Some(decoded) = self.decoder.decode(packet) {
            self.done = true;
            Some(decoded)
        } else {
            None
        }
    }

    /// Every (block, ESI) accepted so far, sorted — diagnostics only.
    pub fn received_esis(&self) -> Vec<(u8, u32)> {
        let mut list: Vec<(u8, u32)> = self.seen.iter().copied().collect();
        list.sort_unstable();
        list
    }

    /// Unique source symbols received per block, for progress bars.
    pub fn per_block_progress(&self) -> Vec<(u8, u32, u32)> {
        self.per_block_k
            .iter()
            .enumerate()
            .map(|(i, k)| (i as u8, self.per_block_seen[i].min(*k), *k))
            .collect()
    }

    /// Fraction of source symbols received, 0.0..=1.0.
    pub fn progress(&self) -> f64 {
        let have: u64 = self.per_block_seen.iter().map(|&s| s as u64).sum();
        have as f64 / self.total_source_symbols.max(1) as f64
    }
}

/// Post-decode payload unwrapping: inflate (raw deflate) and strip the
/// filename/MIME preamble, mirroring `preprocess_payload.ts` in reverse.
pub fn unwrap_payload(
    data: Vec<u8>,
    compressed: bool,
    is_text: bool,
) -> (String, Vec<u8>) {
    let inflated = if compressed {
        inflate_raw(&data)
    } else {
        data
    };

    if is_text {
        (String::from_utf8_lossy(&inflated).into_owned(), inflated)
    } else {
        let mut name = String::new();
        let mut rest = inflated.as_slice();
        if !inflated.is_empty() {
            let name_len = rest[0] as usize;
            rest = &rest[1..];
            let name_len = name_len.min(rest.len());
            name = String::from_utf8_lossy(&rest[..name_len]).into_owned();
            rest = &rest[name_len..];
            if !rest.is_empty() {
                let mime_len = rest[0] as usize;
                rest = &rest[1..];
                let mime_len = mime_len.min(rest.len());
                rest = &rest[mime_len..];
            }
        }
        (name, rest.to_vec())
    }
}

fn inflate_raw(data: &[u8]) -> Vec<u8> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;

    let mut out = Vec::new();
    DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap_or_default();
    out
}

/// Session lookup key: a transfer is identified by (data_length, symbol size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub data_length: u32,
    pub transport_payload_size: usize,
}
