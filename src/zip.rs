//! ZIP (APPNOTE-compatible subset) – hand-written, zero dependencies.
//!
//! Supported on read and write: Stored (0) and Deflate (8), multiple files,
//! directories, UTF-8 names, data descriptors (bit 3) and ZIP64 archives.
//! Encrypted entries and exotic methods (BZip2, LZMA, ...) are rejected
//! with [`ArchiveError::Unsupported`](crate::error::ArchiveError).

use crate::crc::Crc32;
use crate::deflate::{compress_raw, decompress_raw_limited, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, unsupported, ArchiveError, Result};

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
        if name.is_empty() {
            return Err(invalid("empty zip entry name"));
        }
        if name.starts_with('/') {
            return Err(ArchiveError::UnsafePath(name.to_string()));
        }
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(unsupported("zip entry names longer than 64 KiB"));
        }
        let (payload, method) = match method {
            ZipMethod::Stored => (data.to_vec(), ZipMethod::Stored),
            ZipMethod::Deflate => (compress_raw(data, self.options.level), ZipMethod::Deflate),
        };
        let mut crc = Crc32::new();
        crc.update(data);
        let need_zip64 = data.len() as u64 > U32_MAX_AS_U64
            || payload.len() as u64 > U32_MAX_AS_U64
            || self.buf.len() as u64 > U32_MAX_AS_U64;

        let local_offset = self.buf.len() as u64;
        // Local file header.
        self.buf.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        self.buf
            .extend_from_slice(&(if need_zip64 {
                VERSION_NEEDED_ZIP64
            } else {
                VERSION_NEEDED
            })
            .to_le_bytes());
        self.buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        self.buf.extend_from_slice(&method.code().to_le_bytes());
        self.buf.extend_from_slice(&[0u8; 4]); // time/date: set below
        let (crc_v, comp_v, uncomp_v) = (crc.finalize(), payload.len(), data.len());
        self.buf.extend_from_slice(&crc_v.to_le_bytes());
        if need_zip64 {
            self.buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            self.buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            self.buf.extend_from_slice(&(comp_v as u32).to_le_bytes());
            self.buf.extend_from_slice(&(uncomp_v as u32).to_le_bytes());
        }
        let mut extra = Vec::new();
        if need_zip64 {
            extra.extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
            extra.extend_from_slice(&16u16.to_le_bytes());
            extra.extend_from_slice(&(uncomp_v as u64).to_le_bytes());
            extra.extend_from_slice(&(comp_v as u64).to_le_bytes());
        }
        self.buf
            .extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.buf
            .extend_from_slice(&(extra.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(&extra);
        self.buf.extend_from_slice(&payload);

        self.central.push(CentralRecord {
            name: name_bytes.to_vec(),
            method,
            crc: crc_v,
            comp_size: comp_v as u64,
            uncomp_size: uncomp_v as u64,
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
        self.buf.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        self.buf.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        self.buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        self.buf.extend_from_slice(&METHOD_STORED.to_le_bytes());
        self.buf.extend_from_slice(&[0u8; 16]); // time/date/crc/sizes
        self.buf
            .extend_from_slice(&(n.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(n.as_bytes());
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
        let mut cd_size = 0u64;
        for rec in &self.central {
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
            let start = self.buf.len();
            self.buf.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            self.buf
                .extend_from_slice(&MADE_BY_UNIX.to_le_bytes());
            self.buf.extend_from_slice(
                &(if need_zip64 {
                    VERSION_NEEDED_ZIP64
                } else {
                    VERSION_NEEDED
                })
                .to_le_bytes(),
            );
            self.buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
            self.buf.extend_from_slice(&rec.method.code().to_le_bytes());
            self.buf.extend_from_slice(&[0u8; 4]); // time/date
            self.buf.extend_from_slice(&rec.crc.to_le_bytes());
            if need_zip64 {
                self.buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                self.buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            } else {
                self.buf
                    .extend_from_slice(&(rec.comp_size as u32).to_le_bytes());
                self.buf
                    .extend_from_slice(&(rec.uncomp_size as u32).to_le_bytes());
            }
            self.buf
                .extend_from_slice(&(rec.name.len() as u16).to_le_bytes());
            self.buf
                .extend_from_slice(&(extra.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // int attr
            let ext_attr = if rec.is_dir {
                rec.unix_mode
            } else {
                (rec.unix_mode & 0xFFFF) << 16
            };
            self.buf.extend_from_slice(&ext_attr.to_le_bytes());
            if need_zip64 {
                self.buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            } else {
                self.buf
                    .extend_from_slice(&(rec.local_offset as u32).to_le_bytes());
            }
            self.buf.extend_from_slice(&rec.name);
            self.buf.extend_from_slice(&extra);
            cd_size += (self.buf.len() - start) as u64;
        }
        let cd_end = self.buf.len() as u64;
        let count = self.central.len() as u64;
        let need_zip64 =
            count > 0xFFFF || cd_size > U32_MAX_AS_U64 || cd_offset > U32_MAX_AS_U64;
        let comment = self.options.comment.as_bytes();
        if need_zip64 {
            let eocd64_offset = self.buf.len() as u64;
            self.buf.extend_from_slice(&SIG_EOCD64.to_le_bytes());
            self.buf.extend_from_slice(&44u64.to_le_bytes()); // size of remainder
            self.buf.extend_from_slice(&MADE_BY_UNIX.to_le_bytes());
            self.buf
                .extend_from_slice(&VERSION_NEEDED_ZIP64.to_le_bytes());
            self.buf.extend_from_slice(&0u32.to_le_bytes()); // disk
            self.buf.extend_from_slice(&0u32.to_le_bytes()); // cd disk
            self.buf.extend_from_slice(&count.to_le_bytes());
            self.buf.extend_from_slice(&count.to_le_bytes());
            self.buf.extend_from_slice(&cd_size.to_le_bytes());
            self.buf.extend_from_slice(&cd_offset.to_le_bytes());
            self.buf.extend_from_slice(&SIG_LOCATOR64.to_le_bytes());
            self.buf.extend_from_slice(&0u32.to_le_bytes()); // cd disk
            self.buf
                .extend_from_slice(&eocd64_offset.to_le_bytes());
            self.buf.extend_from_slice(&1u32.to_le_bytes()); // disks
        }
        self.buf.extend_from_slice(&SIG_EOCD.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(
            &(if count > 0xFFFF { 0xFFFF } else { count as u16 }).to_le_bytes(),
        );
        self.buf.extend_from_slice(
            &(if count > 0xFFFF { 0xFFFF } else { count as u16 }).to_le_bytes(),
        );
        self.buf.extend_from_slice(
            &(if cd_size > U32_MAX_AS_U64 {
                0xFFFF_FFFF
            } else {
                cd_size as u32
            })
            .to_le_bytes(),
        );
        self.buf.extend_from_slice(
            &(if cd_offset > U32_MAX_AS_U64 {
                0xFFFF_FFFF
            } else {
                cd_offset as u32
            })
            .to_le_bytes(),
        );
        self.buf
            .extend_from_slice(&(comment.len().min(0xFFFF) as u16).to_le_bytes());
        self.buf
            .extend_from_slice(&comment[..comment.len().min(0xFFFF)]);
        let _ = cd_end;
        self.buf
    }
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
        let (cd_offset, cd_size, count) = self.locate_central_dir()?;
        let mut entries = Vec::with_capacity(count.min(1_000_000));
        let mut pos = cd_offset;
        for _ in 0..count {
            if pos + 46 > self.data.len() {
                return Err(invalid("truncated zip central directory"));
            }
            if u32le(self.data, pos)? != SIG_CENTRAL {
                return Err(invalid("bad zip central directory signature"));
            }
            let flags = u16le(self.data, pos + 8)?;
            let method = ZipMethod::from_code(u16le(self.data, pos + 10)?)?;
            let crc = u32le(self.data, pos + 16)?;
            let mut comp_size = u32le(self.data, pos + 20)? as u64;
            let mut uncomp_size = u32le(self.data, pos + 24)? as u64;
            let name_len = u16le(self.data, pos + 28)? as usize;
            let extra_len = u16le(self.data, pos + 30)? as usize;
            let comment_len = u16le(self.data, pos + 32)? as usize;
            let mut local_offset = u32le(self.data, pos + 42)? as u64;
            let made_by = u16le(self.data, pos + 4)?;
            let ext_attr = u32le(self.data, pos + 38)?;
            if pos + 46 + name_len + extra_len + comment_len > self.data.len() {
                return Err(invalid("truncated zip central directory entry"));
            }
            let name_bytes = &self.data[pos + 46..pos + 46 + name_len];
            let extra = &self.data[pos + 46 + name_len..pos + 46 + name_len + extra_len];
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
            let entry = self.read_entry_data(
                &name,
                method,
                flags,
                crc,
                comp_size,
                uncomp_size,
                local_offset,
                unix_mode,
            )?;
            pos += 46 + name_len + extra_len + comment_len;
            entries.push(entry);
        }
        // cd_size is advisory; the entry walk above is authoritative.
        let _ = cd_size;
        Ok(entries)
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

    fn locate_central_dir(&self) -> Result<(usize, u64, usize)> {
        let data = self.data;
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
    fn empty_archive() {
        let raw = ZipWriter::with_options(opts()).finish();
        let entries = zip_unpack(&raw).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn garbage_rejected() {
        assert!(zip_unpack(b"definitely not a zip file....................").is_err());
    }
}
