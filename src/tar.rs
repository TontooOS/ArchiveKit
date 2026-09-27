//! TAR (ustar, PAX and GNU long names) – hand-written, zero dependencies.
//!
//! Implements a POSIX-compatible subset: regular files, directories,
//! symlinks and hardlinks, with `prefix` splitting, GNU `./@LongLink`
//! entries and PAX extended headers on read. Long paths are written as
//! GNU long-name entries for maximum compatibility.

use crate::error::{invalid, ArchiveError, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;

/// Entry type inside a TAR archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarKind {
    /// Regular file (typeflags `0` / `\0`).
    File,
    /// Directory (typeflag `5`).
    Directory,
    /// Symbolic link with target path.
    Symlink(String),
    /// Hard link with target path.
    Hardlink(String),
}

/// A single TAR entry.
#[derive(Debug, Clone)]
pub struct TarEntry {
    /// Path inside the archive (forward slashes).
    pub path: String,
    /// Entry kind.
    pub kind: TarKind,
    /// Unix permission bits (e.g. `0o644`).
    pub mode: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Modification time (Unix timestamp).
    pub mtime: u64,
    /// File payload (empty for non-files).
    pub data: Vec<u8>,
}

impl TarEntry {
    /// Create a regular file entry with default metadata.
    pub fn file(path: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            kind: TarKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            data,
        }
    }

    /// Create a directory entry with default metadata.
    pub fn dir(path: impl Into<String>) -> Self {
        let mut p = path.into();
        if !p.ends_with('/') {
            p.push('/');
        }
        Self {
            path: p,
            kind: TarKind::Directory,
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
            data: Vec::new(),
        }
    }

    /// File size in bytes.
    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }

    /// True for regular files.
    pub fn is_file(&self) -> bool {
        self.kind == TarKind::File
    }

    /// True for directories.
    pub fn is_dir(&self) -> bool {
        self.kind == TarKind::Directory
    }
}

/// Metadata for entries created by [`TarWriter`].
#[derive(Debug, Clone)]
pub struct TarWriteOptions {
    /// Unix permission bits.
    pub mode: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Modification time (Unix timestamp).
    pub mtime: u64,
}

impl Default for TarWriteOptions {
    fn default() -> Self {
        Self {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
        }
    }
}

impl TarWriteOptions {
    /// Defaults for directories (`0o755`).
    pub fn dir_default() -> Self {
        Self {
            mode: 0o755,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Low-level header codec
// ---------------------------------------------------------------------------

fn parse_octal(field: &[u8]) -> Result<u64> {
    // GNU base-256 encoding for values that overflow octal.
    if !field.is_empty() && field[0] == 0x80 {
        let mut v: u64 = 0;
        for &b in &field[1..] {
            v = (v << 8) | b as u64;
        }
        return Ok(v);
    }
    if !field.is_empty() && field[0] == 0xFF {
        // Negative base-256 (rare); treat magnitude as unsigned.
        let mut v: u64 = 0;
        for &b in &field[1..] {
            v = (v << 8) | (b ^ 0xFF) as u64;
        }
        return Ok(v.wrapping_neg());
    }
    let mut end = field.len();
    while end > 0 && (field[end - 1] == 0 || field[end - 1] == b' ') {
        end -= 1;
    }
    let mut start = 0;
    while start < end && (field[start] == b' ' || field[start] == 0) {
        start += 1;
    }
    if start == end {
        return Ok(0);
    }
    let mut v: u64 = 0;
    for &b in &field[start..end] {
        if !(b'0'..=b'7').contains(&b) {
            return Err(invalid("invalid octal field in tar header"));
        }
        v = v
            .checked_mul(8)
            .and_then(|x| x.checked_add((b - b'0') as u64))
            .ok_or_else(|| invalid("tar numeric field overflow"))?;
    }
    Ok(v)
}

fn write_octal(buf: &mut [u8], value: u64) {
    // buf includes the trailing NUL; digits are right-aligned, zero-padded.
    let width = buf.len();
    let s = format!("{:o}", value);
    let digits = s.as_bytes();
    // Truncate from the left on overflow (ustar limitation); callers that
    // need large values should use PAX – sizes here always fit in practice
    // because writer paths cap lengths below the octal limit.
    let take = digits.len().min(width.saturating_sub(1));
    let skip = digits.len() - take;
    for b in buf.iter_mut() {
        *b = b'0';
    }
    buf[width - take - 1..width - 1].copy_from_slice(&digits[skip..]);
    buf[width - 1] = 0;
}

fn read_cstr(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn checksum(block: &[u8; BLOCK]) -> u32 {
    let mut sum: u32 = 0;
    for b in &block[0..148] {
        sum += *b as u32;
    }
    sum += 8 * (b' ' as u32); // checksum field counts as spaces
    for b in &block[156..] {
        sum += *b as u32;
    }
    sum
}

struct RawHeader<'a> {
    name: &'a [u8; 100],
    mode: &'a [u8; 8],
    uid: &'a [u8; 8],
    gid: &'a [u8; 8],
    size: &'a [u8; 12],
    mtime: &'a [u8; 12],
    chksum: &'a [u8; 8],
    typeflag: u8,
    linkname: &'a [u8; 100],
    magic: &'a [u8; 6],
    uname: &'a [u8; 32],
    gname: &'a [u8; 32],
    prefix: &'a [u8; 155],
}

fn split_header(block: &[u8; BLOCK]) -> RawHeader<'_> {
    macro_rules! arr {
        ($off:expr, $len:expr) => {
            block[$off..$off + $len].try_into().unwrap()
        };
    }
    RawHeader {
        name: arr!(0, 100),
        mode: arr!(100, 8),
        uid: arr!(108, 8),
        gid: arr!(116, 8),
        size: arr!(124, 12),
        mtime: arr!(136, 12),
        chksum: arr!(148, 8),
        typeflag: block[156],
        linkname: arr!(157, 100),
        magic: arr!(257, 6),
        uname: arr!(265, 32),
        gname: arr!(297, 32),
        prefix: arr!(345, 155),
    }
}

fn parse_pax(data: &[u8]) -> Vec<(String, String)> {
    // Records: "<len> <key>=<value>\n".
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let eol = match data[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => pos + i,
            None => break,
        };
        let line = &data[pos..eol];
        if let Some(sp) = line.iter().position(|&b| b == b' ') {
            if let Some(eq) = line.iter().position(|&b| b == b'=') {
                if eq > sp {
                    let key = String::from_utf8_lossy(&line[sp + 1..eq]).into_owned();
                    let val = String::from_utf8_lossy(&line[eq + 1..]).into_owned();
                    out.push((key, val));
                }
            }
        }
        pos = eol + 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Sequential TAR reader over an in-memory archive.
pub struct TarReader<'a> {
    data: &'a [u8],
    pos: usize,
    long_name: Option<String>,
    long_link: Option<String>,
    pax: Vec<(String, String)>,
}

impl<'a> TarReader<'a> {
    /// Create a reader over the raw archive bytes.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            long_name: None,
            long_link: None,
            pax: Vec::new(),
        }
    }

    /// Read the next entry, or `None` at end of archive.
    pub fn next_entry(&mut self) -> Result<Option<TarEntry>> {
        loop {
            if self.pos + BLOCK > self.data.len() {
                return Ok(None);
            }
            let block: &[u8; BLOCK] = self.data[self.pos..self.pos + BLOCK]
                .try_into()
                .map_err(|_| invalid("tar block split"))?;
            if block.iter().all(|&b| b == 0) {
                // End marker: one zero block is enough; consume the rest.
                return Ok(None);
            }
            self.pos += BLOCK;
            let h = split_header(block);
            let stored = parse_octal(h.chksum)? as u32;
            if stored != checksum(block) {
                return Err(invalid("tar header checksum mismatch"));
            }
            let size = parse_octal(h.size)?;
            // Checked arithmetic throughout: sizes are untrusted (base-256
            // can encode up to u64::MAX).
            let len = self.data.len() as u64;
            let data_end = (self.pos as u64)
                .checked_add(size)
                .filter(|&e| e <= len)
                .ok_or_else(|| invalid("truncated tar entry data"))?;
            let padded = size
                .div_ceil(512)
                .checked_mul(512)
                .ok_or_else(|| invalid("tar entry size overflow"))?;
            let next_pos = (self.pos as u64)
                .checked_add(padded)
                .filter(|&e| e <= len)
                .ok_or_else(|| invalid("truncated tar entry data"))?;
            let payload = &self.data[self.pos..data_end as usize];
            self.pos = next_pos as usize;

            match h.typeflag {
                b'L' => {
                    // GNU long name: payload overrides the next name.
                    self.long_name = Some(read_cstr_lossy(payload));
                    continue;
                }
                b'K' => {
                    self.long_link = Some(read_cstr_lossy(payload));
                    continue;
                }
                b'x' | b'X' => {
                    self.pax = parse_pax(payload);
                    continue;
                }
                b'g' => continue, // global extended header: ignore
                _ => {}
            }

            let mut name = if let Some(n) = self.long_name.take() {
                n
            } else {
                let mut full = read_cstr(h.prefix);
                let base = read_cstr(h.name);
                if !full.is_empty() {
                    full.push('/');
                    full.push_str(&base);
                    full
                } else {
                    base
                }
            };
            let mut link = self.long_link.take().unwrap_or_else(|| read_cstr(h.linkname));
            let _ = &mut link;
            let mode = parse_octal(h.mode)? as u32;
            let uid = parse_octal(h.uid)? as u32;
            let gid = parse_octal(h.gid)? as u32;
            let mut mtime = parse_octal(h.mtime)?;
            let mut size_override: Option<u64> = None;

            // PAX overrides (path, size, mtime).
            for (k, v) in std::mem::take(&mut self.pax) {
                match k.as_str() {
                    "path" => name = v,
                    "size" => {
                        size_override = v.parse::<u64>().ok();
                    }
                    "mtime" => {
                        if let Ok(f) = v.parse::<f64>() {
                            mtime = f as u64;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(s) = size_override {
                if s != size {
                    return Err(invalid("pax size disagrees with header size"));
                }
            }
            let _ = h.magic;
            let _ = (h.uname, h.gname);

            let kind = match h.typeflag {
                b'0' | 0 => TarKind::File,
                b'5' => TarKind::Directory,
                b'2' => TarKind::Symlink(link.clone()),
                b'1' => TarKind::Hardlink(link.clone()),
                other => {
                    // Unknown typeflags with no payload are skipped; with
                    // payload they are exposed as files would be, but marked
                    // unsupported to avoid silent data corruption.
                    if size == 0 {
                        continue;
                    }
                    return Err(crate::error::unsupported(format!(
                        "tar typeflag '{other}' ({other:#04x})"
                    )));
                }
            };

            // Reject unsafe paths at decode time.
            validate_tar_path(&name)?;

            return Ok(Some(TarEntry {
                path: name,
                kind,
                mode,
                uid,
                gid,
                mtime,
                data: if size == 0 {
                    Vec::new()
                } else {
                    payload.to_vec()
                },
            }));
        }
    }

    /// Read all remaining entries.
    pub fn read_all(&mut self) -> Result<Vec<TarEntry>> {
        let mut out = Vec::new();
        while let Some(e) = self.next_entry()? {
            out.push(e);
        }
        Ok(out)
    }
}

fn read_cstr_lossy(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Reject absolute paths and `..` escapes.
fn validate_tar_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(invalid("empty tar entry name"));
    }
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
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Sequential TAR writer accumulating an in-memory archive.
#[derive(Debug, Default)]
pub struct TarWriter {
    buf: Vec<u8>,
}

impl TarWriter {
    /// Create an empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a regular file.
    pub fn append_file(
        &mut self,
        path: &str,
        data: &[u8],
        options: &TarWriteOptions,
    ) -> Result<()> {
        self.append_entry(path, b'0', "", data, options, data.len() as u64)
    }

    /// Append a directory (trailing slash is added when missing).
    pub fn append_dir(&mut self, path: &str, options: &TarWriteOptions) -> Result<()> {
        let mut dir_opts = options.clone();
        if dir_opts.mode == 0o644 {
            dir_opts.mode = 0o755;
        }
        self.append_entry(path, b'5', "", &[], &dir_opts, 0)
    }

    /// Append a symlink.
    pub fn append_symlink(
        &mut self,
        path: &str,
        target: &str,
        options: &TarWriteOptions,
    ) -> Result<()> {
        self.append_entry(path, b'2', target, &[], options, 0)
    }

    fn append_entry(
        &mut self,
        path: &str,
        typeflag: u8,
        linkname: &str,
        data: &[u8],
        options: &TarWriteOptions,
        size: u64,
    ) -> Result<()> {
        validate_tar_path(path)?;
        let link_bytes = linkname.as_bytes();
        if link_bytes.len() > 100 {
            return Err(crate::error::unsupported(
                "symlink targets longer than 100 bytes",
            ));
        }
        // GNU long name when the path does not fit ustar name+prefix.
        if !fits_ustar(path) {
            let mut name_data = path.as_bytes().to_vec();
            name_data.push(0);
            self.write_raw_header(
                "././@LongLink",
                b'L',
                &[],
                name_data.len() as u64,
                0,
                0,
                0,
            )?;
            self.buf.extend_from_slice(&name_data);
            self.pad_to_block(name_data.len());
        }
        self.write_raw_header(path, typeflag, link_bytes, size, options.mode, options.mtime, 0)?;
        // uid/gid are written as zero in write_raw_header; patch them here
        // is unnecessary – keep zero for reproducibility. (Options uid/gid
        // are accepted for API compatibility and used when PAX is added.)
        let _ = (options.uid, options.gid);
        self.buf.extend_from_slice(data);
        self.pad_to_block(data.len());
        Ok(())
    }

    fn write_raw_header(
        &mut self,
        path: &str,
        typeflag: u8,
        linkname: &[u8],
        size: u64,
        mode: u32,
        mtime: u64,
        _reserved: u64,
    ) -> Result<()> {
        let (name, prefix) = split_ustar(path);
        let mut block = [0u8; BLOCK];
        write_bytes(&mut block[0..100], name.as_bytes());
        write_octal(&mut block[100..108], mode as u64);
        write_octal(&mut block[108..116], 0);
        write_octal(&mut block[116..124], 0);
        // 11 octal digits + NUL fit sizes up to 8 GiB.
        if size >= 8 << 30 {
            return Err(crate::error::unsupported(
                "tar files larger than 8 GiB need PAX (not yet written)",
            ));
        }
        write_octal_12(&mut block[124..136], size);
        write_octal(&mut block[136..148], mtime);
        block[156] = typeflag;
        write_bytes(&mut block[157..257], linkname);
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        write_bytes(&mut block[265..297], b"tontoo");
        write_bytes(&mut block[297..329], b"tontoo");
        write_bytes(&mut block[345..500], prefix.as_bytes());
        // Checksum with the field treated as spaces.
        let sum: u32 = block[..].iter().map(|b| *b as u32).sum::<u32>() + 8 * (b' ' as u32);
        let s = format!("{:06o}\0 ", sum);
        block[148..156].copy_from_slice(s.as_bytes());
        self.buf.extend_from_slice(&block);
        Ok(())
    }

    fn pad_to_block(&mut self, len: usize) {
        let rem = len % BLOCK;
        if rem != 0 {
            self.buf.extend(std::iter::repeat(0).take(BLOCK - rem));
        }
    }

    /// Finish the archive (two zero blocks) and return its bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.extend_from_slice(&[0u8; BLOCK * 2]);
        self.buf
    }
}

fn write_bytes(buf: &mut [u8], data: &[u8]) {
    let n = data.len().min(buf.len());
    buf[..n].copy_from_slice(&data[..n]);
}

fn write_octal_12(buf: &mut [u8], value: u64) {
    debug_assert_eq!(buf.len(), 12);
    let s = format!("{:011o}\0", value);
    buf.copy_from_slice(s.as_bytes());
}

/// True when `path` fits into ustar name/prefix fields.
fn fits_ustar(path: &str) -> bool {
    if path.len() <= 100 {
        return true;
    }
    split_ustar(path).0.len() <= 100
}

/// Split into (name, prefix) following the ustar rule: prefix + '/' + name.
fn split_ustar(path: &str) -> (String, String) {
    if path.len() <= 100 {
        return (path.to_string(), String::new());
    }
    // Find a '/' such that name <= 100 and prefix <= 155.
    let bytes = path.as_bytes();
    let mut best: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'/' && i > 0 {
            let name_len = bytes.len() - i - 1;
            let prefix_len = i;
            if name_len <= 100 && prefix_len <= 155 {
                best = Some(i);
            }
        }
    }
    match best {
        Some(i) => (path[i + 1..].to_string(), path[..i].to_string()),
        None => (path.to_string(), String::new()), // caller emits GNU long name
    }
}

// ---------------------------------------------------------------------------
// One-shot + filesystem helpers
// ---------------------------------------------------------------------------

/// Pack entries into a TAR archive.
pub fn tar_pack(entries: &[TarEntry]) -> Result<Vec<u8>> {
    let mut w = TarWriter::new();
    for e in entries {
        let opts = TarWriteOptions {
            mode: e.mode,
            uid: e.uid,
            gid: e.gid,
            mtime: e.mtime,
        };
        match &e.kind {
            TarKind::File => w.append_file(&e.path, &e.data, &opts)?,
            TarKind::Directory => w.append_dir(&e.path, &opts)?,
            TarKind::Symlink(t) => w.append_symlink(&e.path, t, &opts)?,
            TarKind::Hardlink(t) => {
                w.append_entry(&e.path, b'1', t, &[], &opts, 0)?;
            }
        }
    }
    Ok(w.finish())
}

/// Unpack a TAR archive into entries.
pub fn tar_unpack(data: &[u8]) -> Result<Vec<TarEntry>> {
    TarReader::new(data).read_all()
}

/// Pack a directory (recursively) into a TAR archive.
///
/// Paths inside the archive are relative to `dir` with forward slashes.
pub fn tar_pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let mut entries = Vec::new();
    collect_dir(dir, Path::new(""), &mut entries)?;
    tar_pack(&entries)
}

fn collect_dir(base: &Path, rel: &Path, out: &mut Vec<TarEntry>) -> Result<()> {
    let full = base.join(rel);
    let rd = fs::read_dir(&full).map_err(ArchiveError::from)?;
    let mut names: Vec<PathBuf> = rd
        .map(|e| e.map_err(ArchiveError::from).map(|x| x.path()))
        .collect::<Result<_>>()?;
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("non-utf8 file name"))?;
        let child_rel = rel.join(name);
        let arc_path = child_rel.to_string_lossy().replace('\\', "/");
        let meta = fs::symlink_metadata(&path).map_err(ArchiveError::from)?;
        if meta.is_dir() {
            out.push(TarEntry {
                path: format!("{arc_path}/"),
                kind: TarKind::Directory,
                mode: 0o755,
                uid: 0,
                gid: 0,
                mtime: unix_mtime(&meta),
                data: Vec::new(),
            });
            collect_dir(base, &child_rel, out)?;
        } else if meta.is_file() {
            let data = fs::read(&path).map_err(ArchiveError::from)?;
            out.push(TarEntry {
                path: arc_path,
                kind: TarKind::File,
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: unix_mtime(&meta),
                data,
            });
        } else if meta.is_symlink() {
            let target = fs::read_link(&path).map_err(ArchiveError::from)?;
            out.push(TarEntry {
                path: arc_path,
                kind: TarKind::Symlink(target.to_string_lossy().replace('\\', "/")),
                mode: 0o777,
                uid: 0,
                gid: 0,
                mtime: unix_mtime(&meta),
                data: Vec::new(),
            });
        }
    }
    Ok(())
}

fn unix_mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unpack a TAR archive into `dir`, creating directories as needed.
///
/// Unsafe paths are rejected before any file is written.
pub fn tar_unpack_to_dir(data: &[u8], dir: &Path) -> Result<()> {
    let entries = tar_unpack(data)?;
    for e in &entries {
        validate_tar_path(&e.path)?;
    }
    fs::create_dir_all(dir).map_err(ArchiveError::from)?;
    for e in entries {
        let dest = dir.join(Path::new(&e.path));
        match e.kind {
            TarKind::Directory => {
                fs::create_dir_all(&dest).map_err(ArchiveError::from)?;
            }
            TarKind::File => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(ArchiveError::from)?;
                }
                fs::write(&dest, &e.data).map_err(ArchiveError::from)?;
            }
            TarKind::Symlink(_) | TarKind::Hardlink(_) => {
                // Links are not recreated on extract (platform-dependent
                // privileges); they are reported via tar_unpack() instead.
                return Err(crate::error::unsupported(
                    "link extraction is not performed by tar_unpack_to_dir",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TarWriteOptions {
        TarWriteOptions {
            mtime: 1_700_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_files_and_dirs() {
        let mut w = TarWriter::new();
        w.append_dir("docs/", &TarWriteOptions::dir_default())
            .unwrap();
        w.append_file("docs/hello.txt", b"hello tar", &opts())
            .unwrap();
        w.append_file("root.bin", &[1, 2, 3, 4], &opts()).unwrap();
        let entries = tar_unpack(&w.finish()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].path, "docs/hello.txt");
        assert_eq!(entries[1].data, b"hello tar");
        assert_eq!(entries[1].mtime, 1_700_000_000);
    }

    #[test]
    fn long_name_roundtrip() {
        let long = format!("a/{}.txt", "x".repeat(150));
        let mut w = TarWriter::new();
        w.append_file(&long, b"long", &opts()).unwrap();
        let entries = tar_unpack(&w.finish()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, long);
    }

    #[test]
    fn ustar_prefix_split() {
        let path = format!("{}/file.txt", "d".repeat(120));
        let mut w = TarWriter::new();
        w.append_file(&path, b"x", &opts()).unwrap();
        // Must use prefix splitting, not GNU long name.
        let raw = w.finish();
        assert_eq!(&raw[257..259], b"us");
        let entries = tar_unpack(&raw).unwrap();
        assert_eq!(entries[0].path, path);
    }

    #[test]
    fn unsafe_paths_rejected_on_read() {
        let mut w = TarWriter::new();
        // Bypass the writer check by crafting a raw header manually.
        w.buf.extend_from_slice(&[0u8; 0]);
        let mut block = [0u8; BLOCK];
        write_bytes(&mut block[0..100], b"../evil.txt");
        write_octal(&mut block[100..108], 0o644);
        write_octal(&mut block[108..116], 0);
        write_octal(&mut block[116..124], 0);
        write_octal_12(&mut block[124..136], 3);
        write_octal(&mut block[136..148], 0);
        block[156] = b'0';
        block[257..263].copy_from_slice(b"ustar\0");
        let sum: u32 = block.iter().map(|b| *b as u32).sum::<u32>() + 8 * 32;
        block[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        w.buf.extend_from_slice(&block);
        w.buf.extend_from_slice(b"bad");
        w.buf.extend_from_slice(&vec![0u8; 512 - 3]);
        w.buf.extend_from_slice(&[0u8; 1024]);
        assert!(matches!(
            tar_unpack(&w.buf),
            Err(ArchiveError::UnsafePath(_))
        ));
    }

    #[test]
    fn bad_checksum_errors() {
        let mut raw = TarWriter::new().finish();
        raw[0] = b'X';
        assert!(tar_unpack(&raw).is_err());
    }

    #[test]
    fn symlink_roundtrip() {
        let mut w = TarWriter::new();
        w.append_symlink("link", "target.txt", &opts()).unwrap();
        let entries = tar_unpack(&w.finish()).unwrap();
        assert_eq!(
            entries[0].kind,
            TarKind::Symlink("target.txt".to_string())
        );
    }

    #[test]
    fn gnu_tar_compat_vector() {
        // Minimal ustar header for "hi.txt" with "hi\n", as GNU tar writes it.
        let mut block = [0u8; BLOCK];
        write_bytes(&mut block[0..100], b"hi.txt");
        write_octal(&mut block[100..108], 0o644);
        write_octal(&mut block[108..116], 0);
        write_octal(&mut block[116..124], 0);
        write_octal_12(&mut block[124..136], 3);
        write_octal(&mut block[136..148], 1_700_000_000);
        block[156] = b'0';
        block[257..263].copy_from_slice(b"ustar ");
        block[263..265].copy_from_slice(b" \0");
        let sum: u32 = block.iter().map(|b| *b as u32).sum::<u32>() + 8 * 32;
        block[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = Vec::new();
        raw.extend_from_slice(&block);
        raw.extend_from_slice(b"hi\n");
        raw.extend_from_slice(&vec![0u8; 509]);
        raw.extend_from_slice(&[0u8; 1024]);
        let entries = tar_unpack(&raw).unwrap();
        assert_eq!(entries[0].path, "hi.txt");
        assert_eq!(entries[0].data, b"hi\n");
    }
}
