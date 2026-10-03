//! CRC32 (IEEE 802.3, polynomial 0xEDB88320) – hand-written, table-driven.
//!
//! Used by GZIP (RFC 1952) and ZIP (APPNOTE) trailers/headers.

/// Precomputed CRC32 table for the IEEE polynomial.
const TABLE: [u32; 256] = make_table();

/// Slicing-by-8 tables derived from [`TABLE`] (8 KiB, built at compile time).
const TABLE8: [[u32; 256]; 8] = make_table8();

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

const fn make_table8() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0usize;
    while i < 256 {
        // Slice 0 equals the base table.
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
        tables[0][i] = crc;
        // Higher slices: apply the base transform to the previous slice.
        let mut s = 1usize;
        while s < 8 {
            let mut v = tables[s - 1][i];
            let mut k = 0;
            while k < 8 {
                if v & 1 == 1 {
                    v = 0xEDB8_8320 ^ (v >> 1);
                } else {
                    v >>= 1;
                }
                k += 1;
            }
            tables[s][i] = v;
            s += 1;
        }
        i += 1;
    }
    tables
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

    /// Feed bytes into the checksum (8 bytes per step, byte tail).
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let w = u64::from_le_bytes(c.try_into().unwrap());
            let mixed = crc as u64 ^ w;
            crc = TABLE8[7][(mixed & 0xFF) as usize]
                ^ TABLE8[6][((mixed >> 8) & 0xFF) as usize]
                ^ TABLE8[5][((mixed >> 16) & 0xFF) as usize]
                ^ TABLE8[4][((mixed >> 24) & 0xFF) as usize]
                ^ TABLE8[3][((mixed >> 32) & 0xFF) as usize]
                ^ TABLE8[2][((mixed >> 40) & 0xFF) as usize]
                ^ TABLE8[1][((mixed >> 48) & 0xFF) as usize]
                ^ TABLE8[0][((mixed >> 56) & 0xFF) as usize];
        }
        for &b in chunks.remainder() {
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

// ------------------------------------------------------------------ Adler-32

/// Streaming Adler-32 hasher (RFC 1950, the zlib stream checksum).
///
/// The largest `n` for which `255*n*(n+1)/2 + (n+1)*(BASE-1) <= 2^32-1` is
/// `5552`, so chunks are reduced at that bound to avoid overflow.
#[derive(Debug, Clone, Copy)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

/// Largest chunk that cannot overflow the 32-bit accumulators.
const ADLER_CHUNK: usize = 5552;

/// Initial value: `s1 = 1`, `s2 = 0`.
const ADLER_BASE: u32 = 65521;

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Adler32 {
    /// Create a new hasher with the standard initial state.
    pub fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    /// Feed bytes into the checksum.
    pub fn update(&mut self, data: &[u8]) {
        for chunk in data.chunks(ADLER_CHUNK) {
            for &byte in chunk {
                self.a += byte as u32;
                self.b += self.a;
            }
            self.a %= ADLER_BASE;
            self.b %= ADLER_BASE;
        }
    }

    /// Finalize and return the checksum value.
    pub fn finalize(self) -> u32 {
        (self.b << 16) | self.a
    }

    /// One-shot checksum over `data`.
    pub fn checksum(data: &[u8]) -> u32 {
        let mut h = Self::new();
        h.update(data);
        h.finalize()
    }
}

/// One-shot Adler-32 over `data`.
pub fn adler32(data: &[u8]) -> u32 {
    Adler32::checksum(data)
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

    #[test]
    fn adler32_known_vectors() {
        // RFC 1950 check value for "123456789".
        assert_eq!(adler32(b"123456789"), 0x091E01DE);
        assert_eq!(adler32(b""), 0x0000_0001);
        assert_eq!(adler32(b"a"), 0x0062_0062);
    }

    #[test]
    fn adler32_streaming_matches_oneshot() {
        let mut h = Adler32::new();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finalize(), adler32(b"hello world"));
    }

    #[test]
    fn adler32_handles_chunks_beyond_the_overflow_bound() {
        // 3 full chunks plus a remainder, so the modulo reduction runs more
        // than once.
        let data: Vec<u8> = (0..(ADLER_CHUNK * 3 + 17)).map(|i| (i % 251) as u8).collect();
        let mut h = Adler32::new();
        for piece in data.chunks(4096) {
            h.update(piece);
        }
        assert_eq!(h.finalize(), adler32(&data));
    }
}
