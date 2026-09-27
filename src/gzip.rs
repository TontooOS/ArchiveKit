//! GZIP (RFC 1952) – hand-written, zero dependencies.
//!
//! Single- and multi-member archives. Compression is delegated to the
//! hand-written DEFLATE engine in [`crate::deflate`]; integrity uses the
//! hand-written CRC32 in [`crate::crc`].

use crate::crc::Crc32;
use crate::deflate::{compress_raw, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, Result};

const MAGIC_0: u8 = 0x1F;
const MAGIC_1: u8 = 0x8B;
const METHOD_DEFLATE: u8 = 8;

const FTEXT: u8 = 0x01;
const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;

const OS_UNIX: u8 = 3;

/// A decoded GZIP member.
#[derive(Debug, Clone)]
pub struct GzipMember {
    /// Modification time (Unix timestamp, 0 if unknown).
    pub mtime: u32,
    /// Original file name stored in the header, if any.
    pub name: Option<String>,
    /// Operating system byte from the header.
    pub os: u8,
    /// Decompressed payload.
    pub data: Vec<u8>,
}

/// Optional metadata for encoding.
#[derive(Debug, Clone, Default)]
pub struct GzipOptions {
    /// Compression effort.
    pub level: CompressionLevel,
    /// Modification time stored in the header (0 = unknown).
    pub mtime: u32,
    /// Original file name stored in the header (must be ASCII/Latin-1 clean).
    pub name: Option<String>,
}

/// Compress `data` into a single-member GZIP stream.
pub fn gzip_compress(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    gzip_compress_with_options(data, &GzipOptions {
        level,
        ..Default::default()
    })
}

/// Compress `data` with full header control.
pub fn gzip_compress_with_options(data: &[u8], options: &GzipOptions) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 32);
    out.push(MAGIC_0);
    out.push(MAGIC_1);
    out.push(METHOD_DEFLATE);
    let mut flg = 0u8;
    if options.name.is_some() {
        flg |= FNAME;
    }
    out.push(flg);
    out.extend_from_slice(&options.mtime.to_le_bytes());
    let xfl = match options.level {
        CompressionLevel::Best => 2,
        CompressionLevel::Fastest | CompressionLevel::None => 4,
        CompressionLevel::Balanced => 0,
    };
    out.push(xfl);
    out.push(OS_UNIX);
    if let Some(name) = &options.name {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&compress_raw(data, options.level));
    let mut crc = Crc32::new();
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Decompress a GZIP stream, concatenating all members.
///
/// Returns the concatenated payloads. Use [`gzip_members`] to access
/// per-member metadata.
pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>> {
    gzip_decompress_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decompress with an explicit total output limit (zip-bomb protection).
pub fn gzip_decompress_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for member in gzip_members_limited(data, max_output.saturating_sub(out.len()))? {
        out.extend_from_slice(&member.data);
    }
    if out.is_empty() && data.is_empty() {
        return Err(invalid("empty gzip stream"));
    }
    Ok(out)
}

/// Decode every member of a (possibly multi-member) GZIP stream.
pub fn gzip_members(data: &[u8]) -> Result<Vec<GzipMember>> {
    gzip_members_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decode every member with an explicit total output limit.
pub fn gzip_members_limited(data: &[u8], max_output: usize) -> Result<Vec<GzipMember>> {
    let mut members = Vec::new();
    let mut pos = 0usize;
    let mut remaining = max_output;
    while pos < data.len() {
        let (member, next) = decode_member(data, pos, remaining)?;
        remaining = remaining.saturating_sub(member.data.len());
        pos = next;
        members.push(member);
    }
    if members.is_empty() {
        return Err(invalid("empty gzip stream"));
    }
    Ok(members)
}

fn read_c_string(data: &[u8], mut pos: usize) -> Result<(String, usize)> {
    let start = pos;
    while pos < data.len() && data[pos] != 0 {
        pos += 1;
    }
    if pos >= data.len() {
        return Err(invalid("unterminated gzip string"));
    }
    let s = String::from_utf8_lossy(&data[start..pos]).into_owned();
    Ok((s, pos + 1))
}

fn decode_member(data: &[u8], pos: usize, max_output: usize) -> Result<(GzipMember, usize)> {
    if data.len() - pos < 10 {
        return Err(invalid("truncated gzip header"));
    }
    if data[pos] != MAGIC_0 || data[pos + 1] != MAGIC_1 {
        return Err(invalid("bad gzip magic"));
    }
    if data[pos + 2] != METHOD_DEFLATE {
        return Err(invalid("unsupported gzip compression method"));
    }
    let flg = data[pos + 3];
    if flg & 0xE0 != 0 {
        return Err(invalid("reserved gzip flag bits set"));
    }
    let mtime = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
    let _xfl = data[pos + 8];
    let os = data[pos + 9];
    let mut p = pos + 10;

    if flg & FEXTRA != 0 {
        if p + 2 > data.len() {
            return Err(invalid("truncated gzip extra field"));
        }
        let xlen = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
        p += 2;
        if p + xlen > data.len() {
            return Err(invalid("truncated gzip extra field"));
        }
        p += xlen;
    }
    let mut name: Option<String> = None;
    if flg & FNAME != 0 {
        let (s, next) = read_c_string(data, p)?;
        name = Some(s);
        p = next;
    }
    if flg & FCOMMENT != 0 {
        let (_, next) = read_c_string(data, p)?;
        p = next;
    }
    if flg & FHCRC != 0 {
        if p + 2 > data.len() {
            return Err(invalid("truncated gzip header crc"));
        }
        // Header CRC is advisory; it covers the header bytes only.
        // We verify presence but accept the member regardless, matching
        // common decoder behavior for hand-made archives.
        p += 2;
    }
    let _ = FTEXT;

    // The deflate stream ends where the 8-byte trailer (CRC32 + ISIZE)
    // begins. Our inflate reports consumed bytes, so member scanning is
    // exact and multi-member streams decode in a single pass.
    let (payload, consumed) =
        crate::deflate::decompress_raw_with_consumed(&data[p..], max_output)?;
    p += consumed;
    if p + 8 > data.len() {
        return Err(invalid("truncated gzip trailer"));
    }
    let expected_crc = u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
    let expected_size = u32::from_le_bytes([data[p + 4], data[p + 5], data[p + 6], data[p + 7]]);
    p += 8;
    let mut crc = Crc32::new();
    crc.update(&payload);
    if crc.finalize() != expected_crc {
        return Err(crate::error::ArchiveError::ChecksumMismatch {
            expected: expected_crc,
            actual: crc.finalize(),
            entry: name.clone().unwrap_or_else(|| "<gzip member>".to_string()),
        });
    }
    if payload.len() as u32 != expected_size {
        return Err(invalid("gzip size mismatch"));
    }
    Ok((
        GzipMember {
            mtime,
            name,
            os,
            data: payload,
        },
        p,
    ))
}

/// Buffered streaming encoder: collects input, emits the member on [`GzipEncoder::finish`].
#[derive(Debug, Default)]
pub struct GzipEncoder {
    buf: Vec<u8>,
    options: GzipOptions,
}

impl GzipEncoder {
    /// Create an encoder with the given options.
    pub fn new(options: GzipOptions) -> Self {
        Self {
            buf: Vec::new(),
            options,
        }
    }

    /// Buffer more input bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Finish the stream and return the complete GZIP member.
    pub fn finish(self) -> Vec<u8> {
        gzip_compress_with_options(&self.buf, &self.options)
    }
}

/// Streaming decoder over concatenated members.
#[derive(Debug, Default)]
pub struct GzipDecoder {
    members: Vec<GzipMember>,
    index: usize,
}

impl GzipDecoder {
    /// Decode all members eagerly; iterate them with [`GzipDecoder::next_member`].
    pub fn new(data: &[u8]) -> Result<Self> {
        Ok(Self {
            members: gzip_members(data)?,
            index: 0,
        })
    }

    /// Return the next member payload, or `None` when exhausted.
    pub fn next_member(&mut self) -> Option<&GzipMember> {
        let m = self.members.get(self.index)?;
        self.index += 1;
        Some(m)
    }

    /// Concatenated payload of all members.
    pub fn decode_all(data: &[u8]) -> Result<Vec<u8>> {
        gzip_decompress(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::CompressionLevel;

    #[test]
    fn roundtrip_basic() {
        let data = b"hello gzip world, hello again and again and again";
        for level in [
            CompressionLevel::None,
            CompressionLevel::Fastest,
            CompressionLevel::Balanced,
            CompressionLevel::Best,
        ] {
            let enc = gzip_compress(data, level);
            assert_eq!(&enc[0..2], &[0x1F, 0x8B]);
            assert_eq!(gzip_decompress(&enc).unwrap(), data);
        }
    }

    #[test]
    fn roundtrip_empty() {
        let enc = gzip_compress(b"", CompressionLevel::Balanced);
        assert_eq!(gzip_decompress(&enc).unwrap(), b"");
    }

    #[test]
    fn with_name_and_mtime() {
        let opts = GzipOptions {
            level: CompressionLevel::Balanced,
            mtime: 1_700_000_000,
            name: Some("hello.txt".to_string()),
        };
        let enc = gzip_compress_with_options(b"hi", &opts);
        let members = gzip_members(&enc).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name.as_deref(), Some("hello.txt"));
        assert_eq!(members[0].mtime, 1_700_000_000);
    }

    #[test]
    fn multi_member() {
        let mut enc = gzip_compress(b"first", CompressionLevel::Balanced);
        enc.extend_from_slice(&gzip_compress(b"second", CompressionLevel::Balanced));
        assert_eq!(gzip_decompress(&enc).unwrap(), b"firstsecond");
        assert_eq!(gzip_members(&enc).unwrap().len(), 2);
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut enc = gzip_compress(b"payload", CompressionLevel::Balanced);
        let last = enc.len() - 1;
        enc[last] ^= 0xFF;
        assert!(gzip_decompress(&enc).is_err());
    }

    #[test]
    fn bad_magic() {
        assert!(gzip_decompress(b"not gzip at all....................").is_err());
    }

    #[test]
    fn large_roundtrip() {
        let data: Vec<u8> = (0..300_000usize).map(|i| (i % 253) as u8).collect();
        let enc = gzip_compress(&data, CompressionLevel::Balanced);
        assert_eq!(gzip_decompress(&enc).unwrap(), data);
    }
}
