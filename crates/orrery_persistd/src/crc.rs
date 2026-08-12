//! CRC-32C (Castagnoli) — the journal record payload checksum (D11 §4).
//!
//! Used to re-verify every journal record on replay; a mismatch is treated as
//! corruption. Implemented here with a runtime-computed 256-entry table to avoid
//! an extra dependency; correctness (not throughput) is what replay needs.

/// CRC-32C (Castagnoli, reflected polynomial 0x1EDC6F41).
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

// The 256-entry CRC-32C lookup table.
static TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Well-known CRC-32C vectors.
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(
            crc32c(b"The quick brown fox jumps over the lazy dog"),
            0x2262_0404
        );
    }

    #[test]
    fn detects_single_bit_change() {
        let a = crc32c(b"payload payload payload");
        let mut b = b"payload payload payload".to_vec();
        b[0] ^= 0x01;
        assert_ne!(a, crc32c(&b));
    }
}
