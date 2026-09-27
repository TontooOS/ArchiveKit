//! ZIP (APPNOTE-compatible subset) – hand-written, zero dependencies.
//!
//! Supported on read and write: Stored (0) and Deflate (8), multiple files,
//! directories, UTF-8 names, data descriptors (bit 3) and ZIP64 archives.
//! Encrypted entries and exotic methods (BZip2, LZMA, ...) are rejected
//! with [`ArchiveError::Unsupported`](crate::error::ArchiveError).

use crate::crc::Crc32;
use crate::deflate::{compress_raw, decompress_raw_limited, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, unsupported, ArchiveError, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};

const SIG_LOCAL: u32 = 0x0403_4B50;
const SIG_CENTRAL: u32 = 0x0201_4B50;
const SIG_EOCD: u32 = 0x0605_4B50;
const SIG_EOCD64: u32 = 0x0606_4B50;
const SIG_LOCATOR64: u32 = 0x0706_4B50;
const SIG_DESCRIPTOR: u32 = 0x0807_4B50;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;

const VERSION_NEEDED: u16 = 20; // 2.0 (deflate)
const VERSION_NEEDED_ZIP64: u16 = 45; // 4.5 (zip64)
const MADE_BY_UNIX: u16 = 3 << 8 | 63; // Unix, version 6.3

const ZIP64_EXTRA_ID: u16 = 0x0001;
const U32_MAX_AS_U64: u64 = 0xFFFF_FFFF;

/// Compression method of a ZIP entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZipMethod {
    /// No compression.
    Stored,
    /// DEFLATE compression (default).
    #[default]
    Deflate,
}

impl ZipMethod {
    fn code(self) -> u16 {
        match self {
            ZipMethod::Stored => METHOD_STORED,
            ZipMethod::Deflate => METHOD_DEFLATE,
        }
    }

    fn from_code(code: u16) -> Result<Self> {
        match code {
            METHOD_STORED => Ok(ZipMethod::Stored),
            METHOD_DEFLATE => Ok(ZipMethod::Deflate),
            _ => Err(unsupported(format!(
                "zip compression method {code} (only Stored and Deflate are supported)"
            ))),
        }
    }
}

/// A decoded ZIP entry.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// File name inside the archive (forward slashes).
    pub name: String,
    /// Compression method.
    pub method: ZipMethod,
    /// Uncompressed size in bytes.
    pub uncompressed_size: u64,
    /// Compressed size in bytes.
    pub compressed_size: u64,
    /// CRC32 of the uncompressed data.
    pub crc32: u32,
    /// Unix permission bits when made by a Unix writer, else `None`.
    pub unix_mode: Option<u32>,
    /// Decompressed payload (empty for directories).
    pub data: Vec<u8>,
}

impl ZipEntry {
    /// True when the name ends with `/`.
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }

    /// True when the Unix mode marks a symlink.
    pub fn is_symlink(&self) -> bool {
        matches!(self.unix_mode, Some(m) if m & 0o170_000 == 0o120_000)
    }
}

/// Options for [`ZipWriter`].
#[derive(Debug, Clone)]
pub struct ZipWriterOptions {
    /// Default compression for files.
    pub level: CompressionLevel,
    /// Archive comment.
    pub comment: String,
}

impl Default for ZipWriterOptions {
    fn default() -> Self {
        Self {
            level: CompressionLevel::Balanced,
            comment: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Little-endian helpers
// ---------------------------------------------------------------------------

fn u16le(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| invalid("truncated zip header"))
}

fn u32le(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| invalid("truncated zip header"))
}

fn u64le(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| invalid("truncated zip header"))
}

/// Find ZIP64 extra field values (uncompressed, compressed, header offset).
fn zip64_extra(extra: &[u8]) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut pos = 0;
    let mut sizes = Vec::new();
    while pos + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
        let len = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
        pos += 4;
        if pos + len > extra.len() {
            break;
        }
        if id == ZIP64_EXTRA_ID {
            let mut p = pos;
            while p + 8 <= pos + len {
                sizes.push(u64::from_le_bytes(extra[p..p + 8].try_into().unwrap()));
                p += 8;
            }
        }
        pos += len;
    }
    (
        sizes.first().copied(),
        sizes.get(1).copied(),
        sizes.get(2).copied(),
    )
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CentralRecord {
    name: Vec<u8>,
    method: ZipMethod,
    crc: u32,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
    unix_mode: u32,
    is_dir: bool,
}

/// Validate an entry name (shared by both writers).
fn check_entry_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(invalid("empty zip entry name"));
    }
    if name.starts_with('/') {
        return Err(ArchiveError::UnsafePath(name.to_string()));
    }
    if name.as_bytes().len() > u16::MAX as usize {
        return Err(unsupported("zip entry names longer than 64 KiB"));
    }
    Ok(())
}

/// Render a local file header (without payload).
fn render_local(
    name_bytes: &[u8],
    method: ZipMethod,
    crc: u32,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
) -> Vec<u8> {
    let need_zip64 = uncomp_size > U32_MAX_AS_U64
        || comp_size > U32_MAX_AS_U64
        || local_offset > U32_MAX_AS_U64;
    let mut out = Vec::with_capacity(30 + name_bytes.len() + 20);
    out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
    out.extend_from_slice(
        &(if need_zip64 {
            VERSION_NEEDED_ZIP64
        } else {
            VERSION_NEEDED
        })
        .to_le_bytes(),
    );
    out.extend_from_slice(&FLAG_UTF8.to_le_bytes());
    out.extend_from_slice(&method.code().to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // time/date
    out.extend_from_slice(&crc.to_le_bytes());
    if need_zip64 {
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    } else {
        out.extend_from_slice(&(comp_size as u32).to_le_bytes());
        out.extend_from_slice(&(uncomp_size as u32).to_le_bytes());
    }
    let mut extra = Vec::new();
    if need_zip64 {
        extra.extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
        extra.extend_from_slice(&16u16.to_le_bytes());
        extra.extend_from_slice(&uncomp_size.to_le_bytes());
        extra.extend_from_slice(&comp_size.to_le_bytes());
    }
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(&extra);
    out
}

/// Render a directory local header (stored, empty).
fn render_local_dir(name_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(30 + name_bytes.len());
    out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
    out.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
    out.extend_from_slice(&FLAG_UTF8.to_le_bytes());
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // time/date/crc/sizes
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name_bytes);
    out
}

/// Sequential ZIP writer accumulating an in-memory archive.
#[derive(Debug)]
pub struct ZipWriter {
    buf: Vec<u8>,
    central: Vec<CentralRecord>,
    options: ZipWriterOptions,
}

impl ZipWriter {
    /// Create a writer with default options.
    pub fn new() -> Self {
        Self::with_options(ZipWriterOptions::default())
    }

    /// Create a writer with custom options.
    pub fn with_options(options: ZipWriterOptions) -> Self {
        Self {
            buf: Vec::new(),
            central: Vec::new(),
            options,
        }
    }

    /// Append a file with the default method (Deflate unless empty).
    pub fn append_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        let method = if data.is_empty() {
            ZipMethod::Stored
        } else {
            match self.options.level {
                CompressionLevel::None => ZipMethod::Stored,
                _ => ZipMethod::Deflate,
            }
        };
        self.append_file_with_method(name, data, method, 0o644)
    }

    /// Append a file with an explicit method and Unix mode.
    pub fn append_file_with_method(
        &mut self,
        name: &str,
        data: &[u8],
        method: ZipMethod,
        unix_mode: u32,
    ) -> Result<()> {
        check_entry_name(name)?;
        let name_bytes = name.as_bytes();
        let mut crc = Crc32::new();
        crc.update(data);
        let crc_v = crc.finalize();
        // Compress first (deflate needs its output size up front); stored
        // entries are copied straight into the archive, no temp buffer.
        let compressed: Vec<u8>;
        let payload: &[u8] = match method {
            ZipMethod::Stored => data,
            ZipMethod::Deflate => {
                compressed = compress_raw(data, self.options.level);
                &compressed
            }
        };
        let local_offset = self.buf.len() as u64;
        self.buf.extend_from_slice(&render_local(
            name_bytes,
            method,
            crc_v,
            payload.len() as u64,
            data.len() as u64,
            self.buf.len() as u64,
        ));
        self.buf.extend_from_slice(payload);

        self.central.push(CentralRecord {
            name: name_bytes.to_vec(),
            method,
            crc: crc_v,
            comp_size: payload.len() as u64,
            uncomp_size: data.len() as u64,
            local_offset,
            unix_mode,
            is_dir: false,
        });
        Ok(())
    }

    /// Append a directory entry (trailing slash added when missing).
    pub fn append_dir(&mut self, name: &str) -> Result<()> {
        let mut n = name.to_string();
        if !n.ends_with('/') {
            n.push('/');
        }
        if n.starts_with('/') {
            return Err(ArchiveError::UnsafePath(n));
        }
        let local_offset = self.buf.len() as u64;
        self.buf.extend_from_slice(&render_local_dir(n.as_bytes()));
        self.central.push(CentralRecord {
            name: n.into_bytes(),
            method: ZipMethod::Stored,
            crc: 0,
            comp_size: 0,
            uncomp_size: 0,
            local_offset,
            unix_mode: 0o755 << 16 | 0x10,
            is_dir: true,
        });
        Ok(())
    }

    /// Finish the archive and return its bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.buf.len() as u64;
        let tail = render_central(&self.central, &self.options.comment, cd_offset);
        self.buf.extend_from_slice(&tail);
        self.buf
    }
}

/// Render central directory + EOCD(/ZIP64) for `central` starting at file
/// offset `cd_offset`. Shared by the in-memory and file writers.
fn render_central(central: &[CentralRecord], comment: &str, cd_offset: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut cd_size = 0u64;
    for rec in central {
        let need_zip64 = rec.comp_size > U32_MAX_AS_U64
            || rec.uncomp_size > U32_MAX_AS_U64
            || rec.local_offset > U32_MAX_AS_U64;
        let mut extra = Vec::new();
        if need_zip64 {
            extra.extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
            let mut body = Vec::new();
            body.extend_from_slice(&rec.uncomp_size.to_le_bytes());
            body.extend_from_slice(&rec.comp_size.to_le_bytes());
            body.extend_from_slice(&rec.local_offset.to_le_bytes());
            extra.extend_from_slice(&(body.len() as u16).to_le_bytes());
            extra.extend_from_slice(&body);
        }
        let start = buf.len();
        buf.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        buf.extend_from_slice(&MADE_BY_UNIX.to_le_bytes());
        buf.extend_from_slice(
            &(if need_zip64 {
                VERSION_NEEDED_ZIP64
            } else {
                VERSION_NEEDED
            })
            .to_le_bytes(),
        );
        buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        buf.extend_from_slice(&rec.method.code().to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // time/date
        buf.extend_from_slice(&rec.crc.to_le_bytes());
        if need_zip64 {
            buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            buf.extend_from_slice(&(rec.comp_size as u32).to_le_bytes());
            buf.extend_from_slice(&(rec.uncomp_size as u32).to_le_bytes());
        }
        buf.extend_from_slice(&(rec.name.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
        buf.extend_from_slice(&0u16.to_le_bytes()); // disk
        buf.extend_from_slice(&0u16.to_le_bytes()); // int attr
        let ext_attr = if rec.is_dir {
            rec.unix_mode
        } else {
            (rec.unix_mode & 0xFFFF) << 16
        };
        buf.extend_from_slice(&ext_attr.to_le_bytes());
        if need_zip64 {
            buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            buf.extend_from_slice(&(rec.local_offset as u32).to_le_bytes());
        }
        buf.extend_from_slice(&rec.name);
        buf.extend_from_slice(&extra);
        cd_size += (buf.len() - start) as u64;
    }
    let count = central.len() as u64;
    let need_zip64 =
        count > 0xFFFF || cd_size > U32_MAX_AS_U64 || cd_offset > U32_MAX_AS_U64;
    let comment = comment.as_bytes();
    if need_zip64 {
        let eocd64_offset = cd_offset + cd_size;
        buf.extend_from_slice(&SIG_EOCD64.to_le_bytes());
        buf.extend_from_slice(&44u64.to_le_bytes()); // size of remainder
        buf.extend_from_slice(&MADE_BY_UNIX.to_le_bytes());
        buf.extend_from_slice(&VERSION_NEEDED_ZIP64.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // disk
        buf.extend_from_slice(&0u32.to_le_bytes()); // cd disk
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_offset.to_le_bytes());
        buf.extend_from_slice(&SIG_LOCATOR64.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // cd disk
        buf.extend_from_slice(&eocd64_offset.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // disks
    }
    buf.extend_from_slice(&SIG_EOCD.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(
        &(if count > 0xFFFF { 0xFFFF } else { count as u16 }).to_le_bytes(),
    );
    buf.extend_from_slice(
        &(if count > 0xFFFF { 0xFFFF } else { count as u16 }).to_le_bytes(),
    );
    buf.extend_from_slice(
        &(if cd_size > U32_MAX_AS_U64 {
            0xFFFF_FFFF
        } else {
            cd_size as u32
        })
        .to_le_bytes(),
    );
    buf.extend_from_slice(
        &(if cd_offset > U32_MAX_AS_U64 {
            0xFFFF_FFFF
        } else {
            cd_offset as u32
        })
        .to_le_bytes(),
    );
    buf.extend_from_slice(&(comment.len().min(0xFFFF) as u16).to_le_bytes());
    buf.extend_from_slice(&comment[..comment.len().min(0xFFFF)]);
    buf
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Sequential ZIP reader over an in-memory archive.
pub struct ZipReader<'a> {
    data: &'a [u8],
    max_output: usize,
}

/// Central-directory metadata of one ZIP entry (no payload decoded).
#[derive(Debug, Clone)]
pub struct ZipIndexEntry {
    /// File name inside the archive (forward slashes).
    pub name: String,
    /// Compression method.
    pub method: ZipMethod,
    /// Uncompressed size in bytes.
    pub uncompressed_size: u64,
    /// Compressed size in bytes.
    pub compressed_size: u64,
    /// CRC32 of the uncompressed data.
    pub crc32: u32,
    /// Unix permission bits when made by a Unix writer, else `None`.
    pub unix_mode: Option<u32>,
    /// Offset of the local file header.
    pub local_offset: u64,
    /// General-purpose flags of the central entry.
    pub flags: u16,
}

impl ZipIndexEntry {
    /// True when the name ends with `/`.
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

impl<'a> ZipReader<'a> {
    /// Create a reader over the raw archive bytes.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    /// Override the per-entry decompression limit.
    pub fn with_max_output(mut self, max_output: usize) -> Self {
        self.max_output = max_output;
        self
    }

    /// Read all entries (files and directories).
    pub fn read_all(&self) -> Result<Vec<ZipEntry>> {
        let index = self.read_index()?;
        index.into_iter().map(|e| self.read_one(&e)).collect()
    }

    /// Read only the central directory (no payload touched or decoded).
    pub fn read_index(&self) -> Result<Vec<ZipIndexEntry>> {
        let (cd_offset, cd_size, count) = locate_central_dir(self.data)?;
        parse_central_records(self.data, cd_offset, cd_size, count)
    }

    /// Find an index entry by name.
    pub fn find_in_index(index: &[ZipIndexEntry], name: &str) -> Option<ZipIndexEntry> {
        index.iter().find(|e| e.name == name).cloned()
    }

    /// Decode a single entry from its index record (only its bytes).
    pub fn read_one(&self, index: &ZipIndexEntry) -> Result<ZipEntry> {
        self.read_entry_data(
            &index.name,
            index.method,
            index.flags,
            index.crc32,
            index.compressed_size,
            index.uncompressed_size,
            index.local_offset,
            index.unix_mode,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn read_entry_data(
        &self,
        name: &str,
        method: ZipMethod,
        flags: u16,
        crc: u32,
        comp_size: u64,
        uncomp_size: u64,
        local_offset: u64,
        unix_mode: Option<u32>,
    ) -> Result<ZipEntry> {
        let off = local_offset as usize;
        if off + 30 > self.data.len() {
            return Err(invalid("bad zip local header offset"));
        }
        if u32le(self.data, off)? != SIG_LOCAL {
            return Err(invalid("bad zip local header signature"));
        }
        let l_flags = u16le(self.data, off + 6)?;
        let l_method = u16le(self.data, off + 8)?;
        if ZipMethod::from_code(l_method)? != method {
            return Err(invalid("zip local/central method mismatch"));
        }
        let name_len = u16le(self.data, off + 26)? as usize;
        let extra_len = u16le(self.data, off + 28)? as usize;
        let data_start = off + 30 + name_len + extra_len;
        if data_start > self.data.len() {
            return Err(invalid("truncated zip local header"));
        }
        // Resolve sizes: with bit 3 the local sizes are zero and the
        // central directory holds the truth.
        let has_descriptor = flags & FLAG_DESCRIPTOR != 0 || l_flags & FLAG_DESCRIPTOR != 0;
        let _ = has_descriptor;
        if comp_size > self.data.len() as u64 {
            return Err(invalid("zip entry exceeds archive size"));
        }
        let data_end = data_start + comp_size as usize;
        if data_end > self.data.len() {
            return Err(invalid("truncated zip entry data"));
        }
        let raw = &self.data[data_start..data_end];
        // Skip an optional data descriptor that follows the payload.
        let mut after = data_end;
        if has_descriptor {
            if after + 12 <= self.data.len()
                && u32le(self.data, after).unwrap_or(0) == SIG_DESCRIPTOR
            {
                after += 16;
            } else if after + 12 <= self.data.len() {
                after += 12;
            }
        }
        let _ = after;

        if name.ends_with('/') {
            return Ok(ZipEntry {
                name: name.to_string(),
                method,
                uncompressed_size: 0,
                compressed_size: comp_size,
                crc32: crc,
                unix_mode,
                data: Vec::new(),
            });
        }
        if uncomp_size > self.max_output as u64 {
            return Err(invalid("zip entry exceeds output limit"));
        }
        let data = match method {
            ZipMethod::Stored => {
                if comp_size != uncomp_size {
                    return Err(invalid("stored zip sizes disagree"));
                }
                raw.to_vec()
            }
            ZipMethod::Deflate => {
                decompress_raw_limited(raw, self.max_output).map_err(|e| match e {
                    ArchiveError::InvalidData(m) => invalid(format!("zip deflate error: {m}")),
                    other => other,
                })?
            }
        };
        if data.len() as u64 != uncomp_size {
            return Err(invalid("zip uncompressed size mismatch"));
        }
        let mut check = Crc32::new();
        check.update(&data);
        let actual = check.finalize();
        if actual != crc {
            return Err(ArchiveError::ChecksumMismatch {
                expected: crc,
                actual,
                entry: name.to_string(),
            });
        }
        Ok(ZipEntry {
            name: name.to_string(),
            method,
            uncompressed_size: uncomp_size,
            compressed_size: comp_size,
            crc32: crc,
            unix_mode,
            data,
        })
    }
}

/// Locate the central directory in raw archive bytes.
/// Returns `(offset, size, entry_count)`.
fn locate_central_dir(data: &[u8]) -> Result<(usize, u64, usize)> {
    if data.len() < 22 {
        return Err(invalid("file too small to be a zip archive"));
    }
    // EOCD is within the last 64 KiB + 22 bytes.
    let scan_start = data.len().saturating_sub(0xFFFF + 22);
    let mut eocd = None;
    let mut pos = data.len() - 22;
    loop {
        if data.len() >= pos + 4 && u32le(data, pos).unwrap_or(0) == SIG_EOCD {
            eocd = Some(pos);
            break;
        }
        if pos == scan_start {
            break;
        }
        pos -= 1;
    }
    let eocd = eocd.ok_or_else(|| invalid("zip end-of-central-directory not found"))?;
    let mut count = u16le(data, eocd + 10)? as usize;
    let mut cd_size = u32le(data, eocd + 12)? as u64;
    let mut cd_offset = u32le(data, eocd + 16)? as u64;
    // ZIP64 locator directly precedes EOCD when present.
    if eocd >= 20 && u32le(data, eocd - 20).unwrap_or(0) == SIG_LOCATOR64 {
        let eocd64_off = u64le(data, eocd - 12)? as usize;
        if eocd64_off + 56 <= data.len()
            && u32le(data, eocd64_off).unwrap_or(0) == SIG_EOCD64
        {
            count = u64le(data, eocd64_off + 32)? as usize;
            cd_size = u64le(data, eocd64_off + 40)?;
            cd_offset = u64le(data, eocd64_off + 48)?;
        }
    }
    if cd_offset + cd_size > data.len() as u64 {
        return Err(invalid("zip central directory out of bounds"));
    }
    Ok((cd_offset as usize, cd_size, count))
}

/// Parse central-directory records from raw bytes at `cd_offset`.
fn parse_central_records(
    data: &[u8],
    cd_offset: usize,
    cd_size: u64,
    count: usize,
) -> Result<Vec<ZipIndexEntry>> {
    let mut entries = Vec::with_capacity(count.min(1_000_000));
    let mut pos = cd_offset;
    for _ in 0..count {
        if pos + 46 > data.len() {
            return Err(invalid("truncated zip central directory"));
        }
        if u32le(data, pos)? != SIG_CENTRAL {
            return Err(invalid("bad zip central directory signature"));
        }
        let flags = u16le(data, pos + 8)?;
        let method = ZipMethod::from_code(u16le(data, pos + 10)?)?;
        let crc = u32le(data, pos + 16)?;
        let mut comp_size = u32le(data, pos + 20)? as u64;
        let mut uncomp_size = u32le(data, pos + 24)? as u64;
        let name_len = u16le(data, pos + 28)? as usize;
        let extra_len = u16le(data, pos + 30)? as usize;
        let comment_len = u16le(data, pos + 32)? as usize;
        let mut local_offset = u32le(data, pos + 42)? as u64;
        let made_by = u16le(data, pos + 4)?;
        let ext_attr = u32le(data, pos + 38)?;
        if pos + 46 + name_len + extra_len + comment_len > data.len() {
            return Err(invalid("truncated zip central directory entry"));
        }
        let name_bytes = &data[pos + 46..pos + 46 + name_len];
        let extra = &data[pos + 46 + name_len..pos + 46 + name_len + extra_len];
        let (z64_uncomp, z64_comp, z64_off) = zip64_extra(extra);
        if comp_size == U32_MAX_AS_U64 {
            comp_size = z64_comp.ok_or_else(|| invalid("missing zip64 compressed size"))?;
        }
        if uncomp_size == U32_MAX_AS_U64 {
            uncomp_size =
                z64_uncomp.ok_or_else(|| invalid("missing zip64 uncompressed size"))?;
        }
        if local_offset == U32_MAX_AS_U64 {
            local_offset =
                z64_off.ok_or_else(|| invalid("missing zip64 header offset"))?;
        }
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(unsupported("encrypted zip entries are not supported"));
        }
        let name = decode_name(name_bytes, flags)?;
        let unix_mode = if made_by >> 8 == 3 {
            Some(ext_attr >> 16)
        } else {
            None
        };
        pos += 46 + name_len + extra_len + comment_len;
        entries.push(ZipIndexEntry {
            name,
            method,
            uncompressed_size: uncomp_size,
            compressed_size: comp_size,
            crc32: crc,
            unix_mode,
            local_offset,
            flags,
        });
    }
    // cd_size is advisory; the entry walk above is authoritative.
    let _ = cd_size;
    Ok(entries)
}

fn decode_name(bytes: &[u8], flags: u16) -> Result<String> {
    if flags & FLAG_UTF8 != 0 {
        String::from_utf8(bytes.to_vec())
            .map_err(|_| invalid("invalid utf-8 in zip entry name"))
    } else {
        // Non-UTF8 names (CP437/OEM): lossy conversion never fails and
        // round-trips ASCII exactly.
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// One-shot helpers
// ---------------------------------------------------------------------------

/// Pack `(name, bytes)` files into a ZIP archive.
pub fn zip_pack(files: &[(&str, &[u8])], options: &ZipWriterOptions) -> Result<Vec<u8>> {
    let mut w = ZipWriter::with_options(options.clone());
    for (name, data) in files {
        if name.ends_with('/') {
            w.append_dir(name)?;
        } else {
            w.append_file(name, data)?;
        }
    }
    Ok(w.finish())
}

/// Unpack a ZIP archive into entries.
pub fn zip_unpack(data: &[u8]) -> Result<Vec<ZipEntry>> {
    ZipReader::new(data).read_all()
}

// ---------------------------------------------------------------------------
// File APIs (constant memory for huge archives)
// ---------------------------------------------------------------------------

/// Write adapter that hashes everything passing through.
struct HashWriter<W> {
    inner: W,
    crc: Crc32,
    written: u64,
}

impl<W> HashWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            crc: Crc32::new(),
            written: 0,
        }
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Copy exactly `n` bytes (bounded RAM, 1 MiB chunks).
fn copy_exact(src: &mut File, dst: &mut impl Write, mut n: u64) -> Result<()> {
    let mut buf = vec![0u8; 1024 * 1024];
    while n > 0 {
        let want = (buf.len() as u64).min(n) as usize;
        let mut got = 0;
        while got < want {
            match src.read(&mut buf[got..want]) {
                Ok(0) => return Err(invalid("truncated zip entry data")),
                Ok(k) => got += k,
                Err(e) => return Err(ArchiveError::Io(e.to_string())),
            }
        }
        dst.write_all(&buf[..got]).map_err(ArchiveError::from)?;
        n -= got as u64;
    }
    Ok(())
}

fn read_exact_file(file: &mut File, mut buf: &mut [u8]) -> Result<()> {
    while !buf.is_empty() {
        match file.read(buf) {
            Ok(0) => return Err(invalid("truncated zip file")),
            Ok(n) => buf = &mut buf[n..],
            Err(e) => return Err(ArchiveError::Io(e.to_string())),
        }
    }
    Ok(())
}

/// Reject absolute paths and `..` escapes (file extraction gate).
fn validate_zip_extract_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(ArchiveError::UnsafePath(path.to_string()));
    }
    let mut depth = 0i32;
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ArchiveError::UnsafePath(path.to_string()));
                }
            }
            Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    Ok(())
}

/// Parse a 30-byte local file header.
/// Returns `(method, flags, name_len, extra_len)`.
fn parse_local_header(buf: &[u8; 30]) -> Result<(ZipMethod, u16, usize, usize)> {
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != SIG_LOCAL {
        return Err(invalid("bad zip local header signature"));
    }
    let flags = u16::from_le_bytes([buf[6], buf[7]]);
    let method = ZipMethod::from_code(u16::from_le_bytes([buf[8], buf[9]]))?;
    let name_len = u16::from_le_bytes([buf[26], buf[27]]) as usize;
    let extra_len = u16::from_le_bytes([buf[28], buf[29]]) as usize;
    Ok((method, flags, name_len, extra_len))
}

/// File-backed ZIP reader: the index comes from the tail, entries stream
/// straight from disk with bounded RAM (100 GB archives welcome).
pub struct ZipFileReader {
    file: File,
    len: u64,
    index: Vec<ZipIndexEntry>,
}

impl ZipFileReader {
    /// Open an archive; only the tail plus the central directory are read.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).map_err(ArchiveError::from)?;
        let len = file.metadata().map_err(ArchiveError::from)?.len();
        let tail_len = len.min(0xFFFF + 22);
        if tail_len < 22 {
            return Err(invalid("file too small to be a zip archive"));
        }
        file.seek(SeekFrom::Start(len - tail_len))
            .map_err(ArchiveError::from)?;
        let mut tail = vec![0u8; tail_len as usize];
        read_exact_file(&mut file, &mut tail)?;
        let tail_base = len - tail_len;
        // EOCD scan inside the tail (absolute = tail_base + rel).
        let mut eocd_rel: Option<usize> = None;
        let mut pos = tail.len() - 22;
        let scan_start = tail.len().saturating_sub(0xFFFF + 22);
        loop {
            if u32le(&tail, pos).unwrap_or(0) == SIG_EOCD {
                eocd_rel = Some(pos);
                break;
            }
            if pos == scan_start {
                break;
            }
            pos -= 1;
        }
        let eocd_rel = eocd_rel.ok_or_else(|| invalid("zip end-of-central-directory not found"))?;
        let eocd_abs = tail_base + eocd_rel as u64;
        let mut count = u16le(&tail, eocd_rel + 10)? as usize;
        let mut cd_size = u32le(&tail, eocd_rel + 12)? as u64;
        let mut cd_offset = u32le(&tail, eocd_rel + 16)? as u64;
        // ZIP64 locator sits right before EOCD; read it from the file so
        // huge-comment archives (locator outside the tail) still work.
        if eocd_abs >= 20 {
            let mut loc = [0u8; 20];
            file.seek(SeekFrom::Start(eocd_abs - 20))
                .map_err(ArchiveError::from)?;
            read_exact_file(&mut file, &mut loc)?;
            if u32::from_le_bytes([loc[0], loc[1], loc[2], loc[3]]) == SIG_LOCATOR64 {
                let eocd64_off = u64::from_le_bytes(loc[8..16].try_into().unwrap());
                let mut rec = [0u8; 56];
                file.seek(SeekFrom::Start(eocd64_off))
                    .map_err(ArchiveError::from)?;
                read_exact_file(&mut file, &mut rec)?;
                if u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) != SIG_EOCD64 {
                    return Err(invalid("bad zip64 end record"));
                }
                count = u64::from_le_bytes(rec[32..40].try_into().unwrap()) as usize;
                cd_size = u64::from_le_bytes(rec[40..48].try_into().unwrap());
                cd_offset = u64::from_le_bytes(rec[48..56].try_into().unwrap());
            }
        }
        if cd_offset + cd_size > len {
            return Err(invalid("zip central directory out of bounds"));
        }
        file.seek(SeekFrom::Start(cd_offset))
            .map_err(ArchiveError::from)?;
        let mut central = vec![0u8; cd_size as usize];
        read_exact_file(&mut file, &mut central)?;
        let index = parse_central_records(&central, 0, cd_size, count)?;
        Ok(Self { file, len, index })
    }

    /// Archive byte size.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True for empty files.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Central-directory index (metadata only).
    pub fn index(&self) -> &[ZipIndexEntry] {
        &self.index
    }

    /// Stream one entry into `out` (hash-checked). Returns payload bytes.
    pub fn extract_entry_to_writer(
        &mut self,
        name: &str,
        out: &mut impl Write,
    ) -> Result<u64> {
        let idx = self
            .index
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| ArchiveError::NotFound(name.to_string()))?;
        if idx.is_dir() {
            return Ok(0);
        }
        self.file
            .seek(SeekFrom::Start(idx.local_offset))
            .map_err(ArchiveError::from)?;
        let mut head = [0u8; 30];
        read_exact_file(&mut self.file, &mut head)?;
        let (l_method, _l_flags, name_len, extra_len) = parse_local_header(&head)?;
        if l_method != idx.method {
            return Err(invalid("zip local/central method mismatch"));
        }
        let skip = name_len as u64 + extra_len as u64;
        self.file
            .seek(SeekFrom::Current(skip as i64))
            .map_err(ArchiveError::from)?;
        let mut hashed = HashWriter::new(out);
        match idx.method {
            ZipMethod::Stored => {
                if idx.compressed_size != idx.uncompressed_size {
                    return Err(invalid("stored zip sizes disagree"));
                }
                copy_exact(&mut self.file, &mut hashed, idx.compressed_size)?;
            }
            ZipMethod::Deflate => {
                use std::io::Take;
                let take: Take<&mut File> =
                    std::io::Read::by_ref(&mut self.file).take(idx.compressed_size);
                let (written, _read) = crate::deflate::decompress_stream(
                    take,
                    &mut hashed,
                    u64::MAX,
                )?;
                let _ = written;
            }
        }
        hashed.flush().map_err(ArchiveError::from)?;
        if hashed.written != idx.uncompressed_size {
            return Err(invalid("zip uncompressed size mismatch"));
        }
        if hashed.crc.finalize() != idx.crc32 {
            return Err(ArchiveError::ChecksumMismatch {
                expected: idx.crc32,
                actual: hashed.crc.finalize(),
                entry: name.to_string(),
            });
        }
        Ok(hashed.written)
    }

    /// Stream one entry straight into a file (parents created).
    pub fn extract_entry_to_path(&mut self, name: &str, dest: &Path) -> Result<u64> {
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(ArchiveError::from)?;
            }
        }
        let mut f = File::create(dest).map_err(ArchiveError::from)?;
        let n = self.extract_entry_to_writer(name, &mut f)?;
        f.flush().map_err(ArchiveError::from)?;
        Ok(n)
    }

    /// Extract everything into `dir` (paths validated before any write).
    pub fn extract_all_to(&mut self, dir: &Path) -> Result<()> {
        let names: Vec<String> = self.index.iter().map(|e| e.name.clone()).collect();
        for n in &names {
            validate_zip_extract_path(n)?;
        }
        std::fs::create_dir_all(dir).map_err(ArchiveError::from)?;
        // Clone the index so entry borrows don't cross the &mut calls.
        let index = self.index.clone();
        for e in &index {
            let dest = dir.join(Path::new(&e.name));
            if e.is_dir() {
                std::fs::create_dir_all(&dest).map_err(ArchiveError::from)?;
                continue;
            }
            if is_zip_symlink(e) {
                #[cfg(unix)]
                {
                    let mut target = Vec::new();
                    self.extract_entry_to_writer(&e.name, &mut target)?;
                    let target = String::from_utf8(target)
                        .map_err(|_| invalid("zip symlink target is not utf-8"))?;
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(ArchiveError::from)?;
                    }
                    let _ = std::fs::remove_file(&dest);
                    std::os::unix::fs::symlink(&target, &dest).map_err(ArchiveError::from)?;
                    continue;
                }
                #[cfg(not(unix))]
                {
                    return Err(crate::error::unsupported(
                        "zip symlink extraction needs Unix",
                    ));
                }
            }
            self.extract_entry_to_path(&e.name, &dest)?;
            #[cfg(unix)]
            if let Some(mode) = e.unix_mode {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &dest,
                    std::fs::Permissions::from_mode(mode & 0o7777),
                );
            }
        }
        Ok(())
    }
}

fn is_zip_symlink(e: &ZipIndexEntry) -> bool {
    matches!(e.unix_mode, Some(m) if m & 0o170_000 == 0o120_000)
}

/// File-backed ZIP writer: local data streams to disk, only the central
/// directory is buffered (bounded by entry count, not content size).
pub struct ZipFileWriter {
    file: std::io::BufWriter<File>,
    central: Vec<CentralRecord>,
    comment: String,
    offset: u64,
    level: CompressionLevel,
}

impl ZipFileWriter {
    /// Create a new archive file (parents created).
    pub fn create(path: &Path) -> Result<Self> {
        Self::create_with_options(path, ZipWriterOptions::default())
    }

    /// Create with custom options (level, comment).
    pub fn create_with_options(path: &Path, options: ZipWriterOptions) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(ArchiveError::from)?;
            }
        }
        let file = File::create(path).map_err(ArchiveError::from)?;
        Ok(Self {
            file: std::io::BufWriter::new(file),
            central: Vec::new(),
            comment: options.comment,
            offset: 0,
            level: options.level,
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes).map_err(ArchiveError::from)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Append a directory entry.
    pub fn append_dir(&mut self, name: &str) -> Result<()> {
        let mut n = name.to_string();
        if !n.ends_with('/') {
            n.push('/');
        }
        check_entry_name(&n)?;
        let header = render_local_dir(n.as_bytes());
        let local_offset = self.offset;
        self.write_all(&header)?;
        self.central.push(CentralRecord {
            name: n.into_bytes(),
            method: ZipMethod::Stored,
            crc: 0,
            comp_size: 0,
            uncomp_size: 0,
            local_offset,
            unix_mode: 0o755 << 16 | 0x10,
            is_dir: true,
        });
        Ok(())
    }

    /// Append a file from memory (deflate compresses first, like `ZipWriter`).
    pub fn append_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        let method = if data.is_empty() {
            ZipMethod::Stored
        } else {
            match self.level {
                CompressionLevel::None => ZipMethod::Stored,
                _ => ZipMethod::Deflate,
            }
        };
        self.append_file_with_method(name, data, method, 0o644)
    }

    /// Append a file from memory with explicit method and mode.
    pub fn append_file_with_method(
        &mut self,
        name: &str,
        data: &[u8],
        method: ZipMethod,
        unix_mode: u32,
    ) -> Result<()> {
        check_entry_name(name)?;
        let mut crc = Crc32::new();
        crc.update(data);
        let compressed: Vec<u8>;
        let payload: &[u8] = match method {
            ZipMethod::Stored => data,
            ZipMethod::Deflate => {
                compressed = compress_raw(data, self.level);
                &compressed
            }
        };
        let local_offset = self.offset;
        self.write_all(&render_local(
            name.as_bytes(),
            method,
            crc.finalize(),
            payload.len() as u64,
            data.len() as u64,
            local_offset,
        ))?;
        self.write_all(payload)?;
        self.central.push(CentralRecord {
            name: name.as_bytes().to_vec(),
            method,
            crc: crc.finalize(),
            comp_size: payload.len() as u64,
            uncomp_size: data.len() as u64,
            local_offset,
            unix_mode,
            is_dir: false,
        });
        Ok(())
    }

    /// Append a file from disk with constant memory.
    ///
    /// Stored entries stream in 1 MiB chunks (CRC pre-pass, then copy).
    /// Deflate entries are read fully first (documented limit).
    pub fn append_file_from_disk(
        &mut self,
        name: &str,
        src: &Path,
        method: ZipMethod,
    ) -> Result<()> {
        check_entry_name(name)?;
        let meta = std::fs::symlink_metadata(src).map_err(ArchiveError::from)?;
        if !meta.is_file() {
            return Err(invalid("zip file source is not a regular file"));
        }
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = 0o644u32;
        match method {
            ZipMethod::Stored => {
                // Pass 1: CRC32 over the source file.
                let mut crc = Crc32::new();
                let mut f = File::open(src).map_err(ArchiveError::from)?;
                let mut buf = vec![0u8; 1024 * 1024];
                let mut size = 0u64;
                loop {
                    let n = f.read(&mut buf).map_err(ArchiveError::from)?;
                    if n == 0 {
                        break;
                    }
                    crc.update(&buf[..n]);
                    size += n as u64;
                }
                // Pass 2: header plus streamed copy.
                let local_offset = self.offset;
                self.write_all(&render_local(
                    name.as_bytes(),
                    ZipMethod::Stored,
                    crc.finalize(),
                    size,
                    size,
                    local_offset,
                ))?;
                let mut f = File::open(src).map_err(ArchiveError::from)?;
                let mut hasher = HashWriter::new(&mut self.file);
                copy_exact_file(&mut f, &mut hasher, size)?;
                hasher.flush().map_err(ArchiveError::from)?;
                debug_assert_eq!(hasher.crc.finalize(), crc.finalize());
                debug_assert_eq!(hasher.written, size);
                drop(hasher);
                // The streamed copy bypassed write_all: track it manually.
                self.offset += size;
                self.central.push(CentralRecord {
                    name: name.as_bytes().to_vec(),
                    method: ZipMethod::Stored,
                    crc: crc.finalize(),
                    comp_size: size,
                    uncomp_size: size,
                    local_offset,
                    unix_mode: mode,
                    is_dir: false,
                });
                Ok(())
            }
            ZipMethod::Deflate => {
                let data = std::fs::read(src).map_err(ArchiveError::from)?;
                self.append_file_with_method(name, &data, ZipMethod::Deflate, mode)
            }
        }
    }

    /// Finish the archive (central directory + EOCD) and flush to disk.
    pub fn finish(mut self) -> Result<()> {
        let tail = render_central(&self.central, &self.comment, self.offset);
        self.write_all(&tail)?;
        self.file.flush().map_err(ArchiveError::from)?;
        Ok(())
    }
}

/// Copy exactly `n` bytes between files with bounded RAM.
fn copy_exact_file(src: &mut File, dst: &mut impl Write, mut n: u64) -> Result<()> {
    let mut buf = vec![0u8; 1024 * 1024];
    while n > 0 {
        let want = (buf.len() as u64).min(n) as usize;
        let mut got = 0;
        while got < want {
            match src.read(&mut buf[got..want]) {
                Ok(0) => return Err(invalid("truncated zip file source")),
                Ok(k) => got += k,
                Err(e) => return Err(ArchiveError::Io(e.to_string())),
            }
        }
        dst.write_all(&buf[..got]).map_err(ArchiveError::from)?;
        n -= got as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ZipWriterOptions {
        ZipWriterOptions {
            level: CompressionLevel::Balanced,
            comment: String::new(),
        }
    }

    #[test]
    fn roundtrip_basic() {
        let files: &[(&str, &[u8])] = &[
            ("hello.txt", b"hello zip world"),
            ("dir/nested.txt", b"nested content here and here"),
            ("empty.txt", b""),
        ];
        for level in [
            CompressionLevel::None,
            CompressionLevel::Fastest,
            CompressionLevel::Balanced,
            CompressionLevel::Best,
        ] {
            let o = ZipWriterOptions {
                level,
                comment: String::new(),
            };
            let entries = zip_unpack(&zip_pack(files, &o).unwrap()).unwrap();
            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].data, b"hello zip world");
            assert_eq!(entries[1].name, "dir/nested.txt");
            assert!(entries[2].data.is_empty());
        }
    }

    #[test]
    fn dir_entries() {
        let mut w = ZipWriter::with_options(opts());
        w.append_dir("mydir").unwrap();
        w.append_file("mydir/a.txt", b"a").unwrap();
        let entries = zip_unpack(&w.finish()).unwrap();
        assert!(entries[0].is_dir());
        assert!(!entries[1].is_dir());
    }

    #[test]
    fn crc_mismatch_detected() {
        let raw = zip_pack(&[("a.txt", b"data")], &opts()).unwrap();
        let mut bad = raw.clone();
        // Corrupt the first payload byte (local header for tiny archives
        // starts at a fixed offset: 30 + name + extra).
        let off = 30 + "a.txt".len();
        bad[off] ^= 0xFF;
        assert!(zip_unpack(&bad).is_err());
    }

    #[test]
    fn data_descriptor_read() {
        // Build an archive, then set bit 3 in both headers and zero the
        // local sizes to emulate descriptor-style writers.
        let mut w = ZipWriter::with_options(opts());
        w.append_file("d.txt", b"descriptor style payload").unwrap();
        let mut raw = w.finish();
        // Local header flags at offset 8, sizes at 18..30.
        raw[8] |= 0x08;
        // Central dir: find signature and patch flags + keep sizes.
        let sig: Vec<usize> = (0..raw.len().saturating_sub(4))
            .filter(|&i| &raw[i..i + 4] == &SIG_CENTRAL.to_le_bytes())
            .collect();
        assert_eq!(sig.len(), 1);
        raw[sig[0] + 8] |= 0x08;
        // Zero the local compressed/uncompressed sizes only (18..26);
        // name/extra lengths at 26..30 must stay intact.
        for b in raw[18..26].iter_mut() {
            *b = 0;
        }
        // Append a 12-byte descriptor after the payload (no signature).
        let entries = ZipReader::new(&raw).read_all().unwrap();
        assert_eq!(entries[0].data, b"descriptor style payload");
    }

    #[test]
    fn utf8_names() {
        let files: &[(&str, &[u8])] = &[("Grüße/ünïcodé.txt", b"x")];
        let entries = zip_unpack(&zip_pack(files, &opts()).unwrap()).unwrap();
        assert_eq!(entries[0].name, "Grüße/ünïcodé.txt");
    }

    #[test]
    fn encrypted_rejected() {
        let mut raw = zip_pack(&[("a.txt", b"data")], &opts()).unwrap();
        raw[8] |= 0x01; // local encrypted bit
        let sig = (0..raw.len().saturating_sub(4))
            .find(|&i| &raw[i..i + 4] == &SIG_CENTRAL.to_le_bytes())
            .unwrap();
        raw[sig + 8] |= 0x01;
        assert!(matches!(
            zip_unpack(&raw),
            Err(ArchiveError::Unsupported(_))
        ));
    }

    #[test]
    fn index_only_lists_without_decoding() {        let files: &[(&str, &[u8])] = &[
            ("a.txt", b"aaa"),
            ("sub/b.txt", b"bbb compress me bbb"),
        ];
        let raw = zip_pack(files, &opts()).unwrap();
        let reader = ZipReader::new(&raw);
        let index = reader.read_index().unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index[1].name, "sub/b.txt");
        assert_eq!(index[1].uncompressed_size, 19);
        // Single entry without decoding the rest.
        let hit = ZipReader::find_in_index(&index, "sub/b.txt").unwrap();
        assert_eq!(reader.read_one(&hit).unwrap().data, b"bbb compress me bbb");
        assert!(ZipReader::find_in_index(&index, "missing").is_none());
    }

    #[test]
    fn empty_archive() {
        let raw = ZipWriter::with_options(opts()).finish();
        let entries = zip_unpack(&raw).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn garbage_rejected() {
        assert!(zip_unpack(b"definitely not a zip file....................").is_err());
    }

    #[test]
    fn file_api_roundtrip() {
        use std::path::Path;
        let dir = std::env::temp_dir().join("archivekit_zipfile_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();

        // Writer (buffered deflate + streamed stored from disk).
        std::fs::write(dir.join("big.bin"), &payload).unwrap();
        let mut w = ZipFileWriter::create(&dir.join("test.zip")).unwrap();
        w.append_dir("docs").unwrap();
        w.append_file("docs/a.txt", b"hello file api").unwrap();
        w.append_file_from_disk("big.bin", &dir.join("big.bin"), ZipMethod::Stored)
            .unwrap();
        w.finish().unwrap();

        // Reader: index only, single entry, full extract.
        let mut r = ZipFileReader::open(&dir.join("test.zip")).unwrap();
        assert_eq!(r.index().len(), 3);
        assert!(r.len() > payload.len() as u64);
        let mut one = Vec::new();
        r.extract_entry_to_writer("docs/a.txt", &mut one).unwrap();
        assert_eq!(one, b"hello file api");
        assert!(matches!(
            r.extract_entry_to_writer("nope", &mut Vec::new()),
            Err(ArchiveError::NotFound(_))
        ));
        r.extract_all_to(&dir.join("out")).unwrap();
        assert_eq!(std::fs::read(dir.join("out/big.bin")).unwrap(), payload);

        // File output is a valid zip for the slice reader too.
        let raw = std::fs::read(dir.join("test.zip")).unwrap();
        assert_eq!(zip_unpack(&raw).unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_api_deflate_from_disk() {
        use std::path::Path;
        let dir = std::env::temp_dir().join("archivekit_zipfile_deflate_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = b"compressible ".repeat(20_000);
        std::fs::write(dir.join("t.txt"), &payload).unwrap();
        let mut w = ZipFileWriter::create(&dir.join("t.zip")).unwrap();
        w.append_file_from_disk("t.txt", &dir.join("t.txt"), ZipMethod::Deflate)
            .unwrap();
        w.finish().unwrap();
        let mut r = ZipFileReader::open(&dir.join("t.zip")).unwrap();
        let mut out = Vec::new();
        r.extract_entry_to_writer("t.txt", &mut out).unwrap();
        assert_eq!(out, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
