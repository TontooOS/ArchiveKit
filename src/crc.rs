//! CRC32 (IEEE 802.3, polynomial 0xEDB88320) – hand-written, table-driven.
//!
//! Used by GZIP (RFC 1952) and ZIP (APPNOTE) trailers/headers.

/// Precomputed CRC32 table for the IEEE polynomial.
const TABLE: [u32; 256] = make_table();

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut k = 0;
        while k < 8 {
            if crc & 1 == 1 {
                crc = 0xEDB8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            k += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Streaming CRC32 hasher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    /// Create a new hasher with the standard initial state.
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Feed bytes into the checksum.
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        for &b in data {
            let idx = ((crc ^ b as u32) & 0xFF) as usize;
            crc = TABLE[idx] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// Finalize and return the checksum value.
    pub fn finalize(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }

    /// One-shot checksum over `data`.
    pub fn checksum(data: &[u8]) -> u32 {
        let mut h = Self::new();
        h.update(data);
        h.finalize()
    }
}

/// One-shot CRC32 over `data`.
pub fn crc32(data: &[u8]) -> u32 {
    Crc32::checksum(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Standard check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"hello"), 0x3610_A686);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let mut h = Crc32::new();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finalize(), crc32(b"hello world"));
    }
}
