//! ZLIB (RFC 1950) – hand-written, zero dependencies.
//!
//! A zlib stream is a two-byte header, a raw DEFLATE payload and a four-byte
//! Adler-32 trailer. Compression is delegated to [`crate::deflate`] and the
//! checksum to [`crate::crc`], so no third-party crate is involved.
//!
//! PNG IDAT chunks, PDF `/FlateDecode` streams and zlib-wrapped
//! [`crate::app`] bundles all use this framing.

use crate::crc::Adler32;
use crate::deflate::{compress_raw, decompress_raw_limited, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, Result};

/// Compression method: DEFLATE with a 32 KiB window.
const CM_DEFLATE: u8 = 8;
/// Window size exponent for a 32 KiB window (`2^(CINFO + 8)`).
const CINFO_32K: u8 = 7;

/// Header byte 0: `CM` in the low nibble, `CINFO` in the high nibble.
const CMF: u8 = (CINFO_32K << 4) | CM_DEFLATE;

/// A decoded zlib stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZlibStream {
    /// Uncompressed payload.
    pub data: Vec<u8>,
    /// Compression method from the header (always `8` for DEFLATE).
    pub method: u8,
    /// True when the header carried a preset dictionary (`FDICT`).
    pub preset_dictionary: bool,
    /// Window size exponent from the header.
    pub window_bits: u8,
}

impl ZlibStream {
    /// True when the stream was compressed with the `CMF`/`FLG` pair this
    /// crate writes (`0x78 0x9C`, a 32 KiB window at the default level).
    pub fn is_default_header(&self) -> bool {
        self.method == CM_DEFLATE && self.window_bits == CINFO_32K
    }
}

/// FLEVEL bits derived from the compression level.
fn flevel(level: CompressionLevel) -> u8 {
    match level {
        CompressionLevel::Fastest => 0,
        CompressionLevel::None => 0,
        CompressionLevel::Balanced => 2,
        CompressionLevel::Best => 3,
    }
}

/// Compress `data` into a zlib stream.
///
/// The header is the common `0x78 0x9C` pair (32 KiB window, default level)
/// with `FLEVEL` filled in for the requested level, followed by a
/// big-endian Adler-32 trailer.
pub fn zlib_compress(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    let payload = compress_raw(data, level);
    let mut out = Vec::with_capacity(payload.len() + 6);
    out.push(CMF);
    // FCHECK: the two header bytes read as a big-endian u16 divisible by 31.
    let flevel_flg = flevel(level) << 6;
    let fcheck = (31 - ((CMF as u16 * 256 + flevel_flg as u16) % 31)) % 31;
    out.push((flevel_flg | fcheck as u8) as u8);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&Adler32::checksum(data).to_be_bytes());
    out
}

/// Decompress a zlib stream.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    zlib_decompress_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decompress with an explicit output limit (decompression-bomb protection).
///
/// Returns `Err` when the header is not a valid zlib header, when
/// `FDICT` is set (preset dictionaries are not supported), when the Adler-32
/// trailer does not match, or when the output would exceed `max_output`.
pub fn zlib_decompress_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    Ok(zlib_stream_limited(data, max_output)?.data)
}

/// Decompress a zlib stream without checking the Adler-32 trailer.
///
/// PDF producers frequently leave the trailer zeroed or stale, so formats
/// that must stay compatible with sloppy writers (PDF `/FlateDecode`,
/// PNG `IDAT`) use this. The payload is still fully inflated; only the
/// integrity check is skipped.
pub fn zlib_decompress_unverified(data: &[u8]) -> Result<Vec<u8>> {
    zlib_decompress_unverified_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decompress without verifying the Adler-32 trailer, with an output limit.
pub fn zlib_decompress_unverified_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    Ok(zlib_stream_inner(data, max_output, false)?.data)
}

/// Decompress and report the header fields. Verifies the Adler-32 trailer.
pub fn zlib_stream(data: &[u8]) -> Result<ZlibStream> {
    zlib_stream_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decompress with an explicit output limit and report the header fields.
/// Verifies the Adler-32 trailer.
pub fn zlib_stream_limited(data: &[u8], max_output: usize) -> Result<ZlibStream> {
    zlib_stream_inner(data, max_output, true)
}

fn zlib_stream_inner(data: &[u8], max_output: usize, verify: bool) -> Result<ZlibStream> {
    if data.len() < 2 {
        return Err(invalid("zlib stream shorter than its header"));
    }
    let cmf = data[0];
    let flg = data[1];
    let method = cmf & 0x0F;
    let window_bits = cmf >> 4;
    let preset_dictionary = flg & 0x20 != 0;
    if method != CM_DEFLATE {
        return Err(invalid(format!(
            "unsupported zlib compression method {method}"
        )));
    }
    if window_bits > 7 {
        return Err(invalid(format!(
            "zlib window size exponent {window_bits} is out of range"
        )));
    }
    if (cmf as u16 * 256 + flg as u16) % 31 != 0 {
        return Err(invalid("zlib header check bits are wrong"));
    }
    if preset_dictionary {
        return Err(crate::error::unsupported(
            "zlib preset dictionaries are not supported",
        ));
    }
    if data.len() < 6 {
        return Err(invalid("zlib stream too short for its trailer"));
    }
    // The trailing four bytes are the Adler-32 of the uncompressed data; the
    // DEFLATE payload sits between them.
    let body_end = data.len() - 4;
    let out = decompress_raw_limited(&data[2..body_end], max_output)?;
    if verify {
        let expected = u32::from_be_bytes([
            data[body_end],
            data[body_end + 1],
            data[body_end + 2],
            data[body_end + 3],
        ]);
        let actual = Adler32::checksum(&out);
        if expected != actual {
            return Err(crate::error::ArchiveError::ChecksumMismatch {
                expected,
                actual,
                entry: "zlib stream".to_string(),
            });
        }
    }
    Ok(ZlibStream {
        data: out,
        method,
        preset_dictionary,
        window_bits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::decompress_raw;

    /// Python: `zlib.compress(b"the quick brown fox jumps over the lazy dog", 9)`.
    #[test]
    fn decodes_streams_produced_by_python_zlib() {
        let python = [
            0x78, 0xDA, 0x2B, 0xC9, 0x48, 0x55, 0x28, 0x2C, 0xCD, 0x4C, 0xCE, 0x56, 0x48, 0x2A, 0xCA,
            0x2F, 0xCF, 0x53, 0x48, 0xCB, 0xAF, 0x50, 0xC8, 0x2A, 0xCD, 0x2D, 0x28, 0x56, 0xC8, 0x2F,
            0x4B, 0x2D, 0x52, 0x28, 0x01, 0x4A, 0xE7, 0x24, 0x56, 0x55, 0x2A, 0xA4, 0xE4, 0xA7, 0x03,
            0x00, 0x61, 0x3C, 0x0F, 0xFA,
        ];
        assert_eq!(
            zlib_decompress(&python).unwrap(),
            b"the quick brown fox jumps over the lazy dog"
        );
    }

    #[test]
    fn roundtrips_every_level() {
        let cases: [&[u8]; 5] = [
            b"",
            b"a",
            b"the quick brown fox jumps over the lazy dog",
            &[0x5Au8; 100_000],
            &(0..=255u8).cycle().take(70_000).collect::<Vec<u8>>(),
        ];
        for level in [
            CompressionLevel::None,
            CompressionLevel::Fastest,
            CompressionLevel::Balanced,
            CompressionLevel::Best,
        ] {
            for data in cases {
                let packed = zlib_compress(data, level);
                assert_eq!(zlib_decompress(&packed).unwrap(), data, "level {level:?}");
            }
        }
    }

    #[test]
    fn header_is_the_standard_pair_and_divisible_by_31() {
        let packed = zlib_compress(b"hello", CompressionLevel::Balanced);
        assert_eq!(packed[0], 0x78, "CMF: DEFLATE with a 32 KiB window");
        assert_eq!(packed[1] & 0x20, 0, "FDICT must be clear");
        assert_eq!(
            (u16::from(packed[0]) * 256 + u16::from(packed[1])) % 31,
            0,
            "header must be divisible by 31"
        );
    }

    #[test]
    fn trailer_matches_the_adler32_of_the_payload() {
        let data = b"trailer check";
        let packed = zlib_compress(data, CompressionLevel::Balanced);
        let trailer = u32::from_be_bytes(packed[packed.len() - 4..].try_into().unwrap());
        assert_eq!(trailer, crate::crc::adler32(data));
    }

    #[test]
    fn payload_between_header_and_trailer_is_raw_deflate() {
        let data = b"raw deflate payload check";
        let packed = zlib_compress(data, CompressionLevel::Balanced);
        let body = &packed[2..packed.len() - 4];
        assert_eq!(decompress_raw(body).unwrap(), data);
    }

    #[test]
    fn stream_reports_header_fields() {
        let packed = zlib_compress(b"fields", CompressionLevel::Balanced);
        let stream = zlib_stream(&packed).unwrap();
        assert_eq!(stream.method, 8);
        assert_eq!(stream.window_bits, 7);
        assert!(!stream.preset_dictionary);
        assert!(stream.is_default_header());
        assert_eq!(stream.data, b"fields");
    }

    #[test]
    fn rejects_bad_headers() {
        for bad in [
            vec![],
            vec![0x78],
            // Divisibility check fails.
            vec![0x78, 0x9D, 0x00],
            // Compression method 7 is not DEFLATE.
            vec![0x77, 0x84, 0x00, 0x00, 0x00, 0x00],
            // FDICT set (0x78 0xBB is divisible by 31).
            vec![0x78, 0xBB, 0x00, 0x00, 0x00, 0x00],
        ] {
            assert!(zlib_decompress(&bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_a_corrupt_trailer() {
        let mut packed = zlib_compress(b"checksum", CompressionLevel::Balanced);
        let last = packed.len() - 1;
        packed[last] ^= 0xFF;
        let err = zlib_decompress(&packed).unwrap_err();
        assert!(
            matches!(err, crate::error::ArchiveError::ChecksumMismatch { .. }),
            "expected a checksum mismatch, got {err:?}"
        );
    }

    #[test]
    fn unverified_skips_the_trailer_check() {
        // PDF producers often leave the Adler-32 zeroed or stale.
        let mut packed = zlib_compress(b"sloppy writer", CompressionLevel::Balanced);
        let len = packed.len();
        packed[len - 4..].copy_from_slice(&[0x0F, 0x0F, 0x0F, 0x0F]);
        assert!(zlib_decompress(&packed).is_err());
        assert_eq!(
            zlib_decompress_unverified(&packed).unwrap(),
            b"sloppy writer"
        );
        // The header is still validated.
        assert!(zlib_decompress_unverified(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn output_limit_is_enforced() {
        let packed = zlib_compress(&[7u8; 200_000], CompressionLevel::Balanced);
        assert!(zlib_decompress_limited(&packed, 1_000).is_err());
        assert!(zlib_decompress_limited(&packed, 200_000).is_ok());
    }

    #[test]
    fn empty_payload_roundtrips() {
        let packed = zlib_compress(b"", CompressionLevel::Balanced);
        assert!(zlib_decompress(&packed).unwrap().is_empty());
    }
}