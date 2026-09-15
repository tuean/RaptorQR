//! RaptorQR transport protocol, ported from
//! `packages/raptorqr-core/src/protocol/packet.ts` and `crc32c.ts`.
//!
//! Fixed 8-byte header (little-endian):
//! | 0 | 1 | magic 0x51 ('Q')
//! | 1 | 4 | packed_word (see below)
//! | 5 | 3 | data_length (24-bit)
//! | 8 | N | payload
//! | 8+N | 4 | CRC32C over bytes 0..8+N
//!
//! packed_word bits:
//!   0-11   generationIndex
//!   12-23  totalGenerations
//!   24-28  symbolIndex (31 = RaptorQ wasm packet)
//!   29     isText
//!   30     isLastGeneration
//!   31     compressed (deflate-raw)

pub const MAGIC_BYTE: u8 = 0x51;
pub const HEADER_SIZE: usize = 8;
pub const CRC32C_SIZE: usize = 4;
pub const RAPTORQ_SYMBOL_INDEX: u32 = 31;

/// Decoded transport header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub generation_index: u32,
    pub total_generations: u32,
    pub symbol_index: u32,
    pub is_text: bool,
    pub is_last_generation: bool,
    pub compressed: bool,
    pub data_length: u32,
}

/// A parsed transport packet: header + payload (CRC already verified).
#[derive(Debug, PartialEq, Eq)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    TooShort,
    BadMagic,
    CrcMismatch,
}

/// Parse and CRC-verify a complete transport packet.
pub fn parse_packet(data: &[u8]) -> Result<Packet, ParseError> {
    if data.len() < HEADER_SIZE + CRC32C_SIZE {
        return Err(ParseError::TooShort);
    }
    if data[0] != MAGIC_BYTE {
        return Err(ParseError::BadMagic);
    }
    let stored_crc = u32::from_le_bytes([
        data[data.len() - 4],
        data[data.len() - 3],
        data[data.len() - 2],
        data[data.len() - 1],
    ]);
    if crc32c(&data[..data.len() - 4]) != stored_crc {
        return Err(ParseError::CrcMismatch);
    }

    let word = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let header = PacketHeader {
        generation_index: word & 0xfff,
        total_generations: (word >> 12) & 0xfff,
        symbol_index: (word >> 24) & 0x1f,
        is_text: (word >> 29) & 1 == 1,
        is_last_generation: (word >> 30) & 1 == 1,
        compressed: (word >> 31) & 1 == 1,
        data_length: ((data[5] as u32) | ((data[6] as u32) << 8) | ((data[7] as u32) << 16))
            & 0xff_ffff,
    };

    Ok(Packet {
        header,
        payload: data[HEADER_SIZE..data.len() - CRC32C_SIZE].to_vec(),
    })
}

// ─── CRC32C (Castagnoli, reflected 0x82F63B78) ──────────────────────────────

use std::sync::OnceLock;

static TABLE: OnceLock<[u32; 256]> = OnceLock::new();

fn table() -> &'static [u32; 256] {
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, entry) in t.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0x82f6_3b78
                } else {
                    crc >> 1
                };
            }
            *entry = crc;
        }
        t
    })
}

/// CRC32C over `data` with standard init 0xFFFFFFFF and final XOR 0xFFFFFFFF,
/// matching `crc32c.ts`.
pub fn crc32c(data: &[u8]) -> u32 {
    let t = table();
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = t[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_values() {
        // Standard CRC32C test vectors.
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b"The quick brown fox jumps over the lazy dog"), 0x2262_0404);
    }

    #[test]
    fn packet_roundtrip_against_typescript_layout() {
        // Build the exact byte layout the TS sender produces for
        // generationIndex=0, totalGenerations=24, symbolIndex=31,
        // isText=false, isLastGeneration=true, compressed=false,
        // dataLength=1234, payload=[1,2,3,4,5].
        let mut word = 0u32;
        word |= 0 & 0xfff;
        word |= (24 & 0xfff) << 12;
        word |= (31 & 0x1f) << 24;
        word |= 0 << 29;
        word |= 1 << 30;
        word |= 0 << 31;
        let mut packet = Vec::new();
        packet.push(MAGIC_BYTE);
        packet.extend_from_slice(&word.to_le_bytes());
        let dl: u32 = 1234;
        packet.extend_from_slice(&[
            (dl & 0xff) as u8,
            ((dl >> 8) & 0xff) as u8,
            ((dl >> 16) & 0xff) as u8,
        ]);
        packet.extend_from_slice(&[1, 2, 3, 4, 5]);
        let crc = crc32c(&packet);
        packet.extend_from_slice(&crc.to_le_bytes());

        let parsed = parse_packet(&packet).unwrap();
        assert_eq!(parsed.header.generation_index, 0);
        assert_eq!(parsed.header.total_generations, 24);
        assert_eq!(parsed.header.symbol_index, 31);
        assert!(!parsed.header.is_text);
        assert!(parsed.header.is_last_generation);
        assert!(!parsed.header.compressed);
        assert_eq!(parsed.header.data_length, 1234);
        assert_eq!(parsed.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn packet_rejects_corruption() {
        let mut packet = Vec::new();
        packet.push(MAGIC_BYTE);
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&[0, 0, 0]);
        packet.extend_from_slice(&[9, 9]);
        let crc = crc32c(&packet);
        packet.extend_from_slice(&crc.to_le_bytes());
        assert!(parse_packet(&packet).is_ok());

        packet[8] ^= 0xff; // flip a payload bit
        assert_eq!(parse_packet(&packet), Err(ParseError::CrcMismatch));
        packet[8] ^= 0xff;

        packet[0] = 0x00;
        assert_eq!(parse_packet(&packet), Err(ParseError::BadMagic));
    }
}
