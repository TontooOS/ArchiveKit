//! Combined formats and high-level helpers.
//!
//! - `.tar.gz` / `.tgz` pipelines (TAR through GZIP)
//! - [`Format`] detection by magic bytes
//! - One-shot bytes APIs and file/directory APIs used by apps and the C FFI

use crate::deflate::CompressionLevel;
use crate::error::{invalid, Result};
use crate::gzip::{gzip_compress, gzip_decompress, GzipOptions};
use crate::tar::{tar_pack, tar_pack_dir, tar_unpack, tar_unpack_to_dir, TarEntry};
use crate::zip::{zip_pack, zip_unpack, ZipWriterOptions};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::ArchiveError;

/// Archive format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// ZIP archive (`.zip`).
    #[default]
    Zip,
    /// GZIP stream (`.gz`, single file or stream).
    Gzip,
    /// Uncompressed TAR (`.tar`).
    Tar,
    /// GZIP-compressed TAR (`.tar.gz`, `.tgz`).
    TarGzip,
    /// TontooOS app container (`.app`, TAPP with central directory).
    App,
}

impl Format {
    /// Detect from a file extension (`.zip`, `.gz`, `.tar`, `.tar.gz`, `.tgz`, `.app`).
    pub fn from_extension(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Some(Format::TarGzip)
        } else if name.ends_with(".app") {
            Some(Format::App)
        } else if name.ends_with(".zip") {
            Some(Format::Zip)
        } else if name.ends_with(".gz") {
            Some(Format::Gzip)
        } else if name.ends_with(".tar") {
            Some(Format::Tar)
        } else {
            None
        }
    }

    /// Canonical extension for the format.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Gzip => "gz",
            Format::Tar => "tar",
            Format::TarGzip => "tar.gz",
            Format::App => "app",
        }
    }
}

/// Detect the format of in-memory archive bytes by magic.
///
/// Returns `None` when the bytes match no known format. GZIP-compressed
/// TAR is reported as [`Format::Gzip`]; use the file extension (or a trial
/// TAR parse of the payload) to distinguish plain GZIP from TAR+GZIP.
pub fn detect_format(data: &[u8]) -> Option<Format> {
    if data.len() >= 8
        && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == crate::app::APP_MAGIC
    {
        return Some(Format::App);
    }
    if data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B {
        return Some(Format::Gzip);
    }
    if data.len() >= 4 && data[0] == b'P' && data[1] == b'K' {
        // Local header, EOCD (empty archive) or spanning marker.
        let sig = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if sig == 0x0403_4B50 || sig == 0x0605_4B50 || sig == 0x0807_4B50 || sig == 0x0201_4B50 {
            return Some(Format::Zip);
        }
    }
    if data.len() >= 512 && is_tar_block(&data[..512]) {
        return Some(Format::Tar);
    }
    None
}

fn is_tar_block(block: &[u8]) -> bool {
    if block.len() < 512 || block.iter().all(|&b| b == 0) {
        return false;
    }
    let stored = match parse_tar_chksum(&block[148..156]) {
        Some(v) => v,
        None => return false,
    };
    let computed: u32 = block[..148].iter().map(|b| *b as u32).sum::<u32>()
        + 8 * (b' ' as u32)
        + block[156..512].iter().map(|b| *b as u32).sum::<u32>();
    stored == computed
}

fn parse_tar_chksum(field: &[u8]) -> Option<u32> {
    let mut end = field.len();
    while end > 0 && (field[end - 1] == 0 || field[end - 1] == b' ') {
        end -= 1;
    }
    let mut start = 0;
    while start < end && (field[start] == b' ' || field[start] == 0) {
        start += 1;
    }
    if start == end {
        return None;
    }
    let mut v: u32 = 0;
    for &b in &field[start..end] {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        v = v.checked_mul(8)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

/// Compress bytes into the given format.
///
/// For [`Format::Gzip`] the input is treated as one stream; for
/// [`Format::Tar`] and [`Format::TarGzip`] the input must already be a TAR
/// archive (see [`tar_gzip_compress`] for the entry-based pipeline).
pub fn compress_bytes(data: &[u8], format: Format, level: CompressionLevel) -> Result<Vec<u8>> {
    match format {
        Format::Zip => zip_pack(
            &[("data.bin", data)],
            &ZipWriterOptions {
                level,
                comment: String::new(),
            },
        ),
        Format::Gzip => Ok(gzip_compress(data, level)),
        Format::Tar => Err(invalid("tar needs entries: use tar_pack() instead")),
        Format::TarGzip => Err(invalid(
            "tar.gz needs entries: use tar_gzip_compress() instead",
        )),
        Format::App => Err(invalid(
            "app needs a manifest and tree: use AppBuilder or app_pack_dir() instead",
        )),
    }
}

/// Decompress bytes of the given format.
///
/// For [`Format::Tar`] and [`Format::TarGzip`] the TAR payload is returned
/// raw; parse it with [`tar_unpack`].
pub fn decompress_bytes(data: &[u8], format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Zip => {
            let entries = zip_unpack(data)?;
            match entries.into_iter().next() {
                Some(e) => Ok(e.data),
                None => Err(invalid("empty zip archive")),
            }
        }
        Format::Gzip => gzip_decompress(data),
        Format::Tar => Ok(data.to_vec()),
        Format::TarGzip => tar_gzip_decompress_raw(data),
        Format::App => Err(invalid(
            "app needs indexed access: use AppReader::read_file() instead",
        )),
    }
}

/// Pack TAR entries straight into `.tar.gz` bytes.
pub fn tar_gzip_compress(entries: &[TarEntry], level: CompressionLevel) -> Vec<u8> {
    let tar = tar_pack(entries).expect("tar packing of in-memory entries cannot fail");
    gzip_compress(&tar, level)
}

/// Pack `(name, bytes)` files straight into `.tar.gz` bytes.
pub fn tar_gzip_compress_files(files: &[(&str, &[u8])], level: CompressionLevel) -> Result<Vec<u8>> {
    let entries: Vec<TarEntry> = files
        .iter()
        .map(|(name, data)| TarEntry::file(*name, data.to_vec()))
        .collect();
    Ok(tar_gzip_compress(&entries, level))
}

/// Decompress `.tar.gz` bytes into TAR entries.
pub fn tar_gzip_decompress(data: &[u8]) -> Result<Vec<TarEntry>> {
    let tar = tar_gzip_decompress_raw(data)?;
    tar_unpack(&tar)
}

fn tar_gzip_decompress_raw(data: &[u8]) -> Result<Vec<u8>> {
    let tar = gzip_decompress(data).map_err(|e| match e {
        ArchiveError::InvalidData(m) => invalid(format!("tar.gz gzip layer: {m}")),
        ArchiveError::ChecksumMismatch { .. } => e,
        other => other,
    })?;
    Ok(tar)
}

/// Pack a directory (recursively) into any format.
pub fn pack_dir_to_archive(dir: &Path, format: Format, level: CompressionLevel) -> Result<Vec<u8>> {
    match format {
        Format::Tar => tar_pack_dir(dir),
        Format::TarGzip => {
            let tar = tar_pack_dir(dir)?;
            Ok(gzip_compress(&tar, level))
        }
        Format::Zip => zip_dir(dir, level),
        Format::Gzip => Err(invalid("gzip packs a single stream, not a directory")),
        Format::App => {
            // Adopt the fico manifest; the top prefix comes from its name.
            let text = fs::read_to_string(dir.join(crate::app::APP_MANIFEST_NAME))
                .map_err(|_| invalid("app dir has no Info.tontoo manifest"))?;
            let manifest = crate::app::AppManifest::from_fico(&text)?;
            let top_name = manifest
                .display_name()
                .ok_or_else(|| invalid("app manifest has no name"))?
                .to_string();
            let mut builder = crate::app::AppBuilder::new(&top_name)?;
            builder.set_manifest(manifest);
            if level == CompressionLevel::None {
                builder.pack_tree(dir)?;
            } else {
                builder.pack_tree_compressed(dir, level)?;
            }
            builder.finish()
        }
    }
}

fn zip_dir(dir: &Path, level: CompressionLevel) -> Result<Vec<u8>> {
    let mut w = crate::zip::ZipWriter::with_options(ZipWriterOptions {
        level,
        comment: String::new(),
    });
    zip_dir_walk(dir, Path::new(""), &mut w)?;
    Ok(w.finish())
}

fn zip_dir_walk(
    base: &Path,
    rel: &Path,
    w: &mut crate::zip::ZipWriter,
) -> Result<()> {
    let full = base.join(rel);
    let mut names: Vec<PathBuf> = fs::read_dir(&full)
        .map_err(ArchiveError::from)?
        .map(|e| e.map_err(ArchiveError::from).map(|x| x.path()))
        .collect::<Result<_>>()?;
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("non-utf8 file name"))?;
        let child_rel = rel.join(name);
        let arc = child_rel.to_string_lossy().replace('\\', "/");
        let meta = fs::symlink_metadata(&path).map_err(ArchiveError::from)?;
        if meta.is_dir() {
            w.append_dir(&arc)?;
            zip_dir_walk(base, &child_rel, w)?;
        } else if meta.is_file() {
            let data = fs::read(&path).map_err(ArchiveError::from)?;
            w.append_file(&arc, &data)?;
        }
        // Symlinks are skipped in ZIP directory packing (no portable
        // encoding); they are preserved by the TAR pipeline instead.
    }
    Ok(())
}

/// List entry names of any supported archive (auto-detected).
pub fn list_names(data: &[u8]) -> Result<Vec<String>> {
    if let Some(Format::Gzip) = detect_format(data) {
        // Could be plain gzip or tar.gz – peek inside.
        if let Ok(tar) = gzip_decompress(data) {
            if tar.len() >= 512 && is_tar_block(&tar[..512.min(tar.len())]) {
                return Ok(tar_unpack(&tar)?.iter().map(|e| e.path.clone()).collect());
            }
        }
        let members = crate::gzip::gzip_members(data)?;
        return Ok(members
            .iter()
            .enumerate()
            .map(|(i, m)| {
                m.name.clone().unwrap_or_else(|| format!("<gzip member {i}>"))
            })
            .collect());
    }
    match detect_format(data) {
        Some(Format::Zip) => Ok(zip_unpack(data)?
            .iter()
            .map(|e| e.name.clone())
            .collect()),
        Some(Format::Tar) => Ok(tar_unpack(data)?.iter().map(|e| e.path.clone()).collect()),
        Some(Format::App) => Ok(crate::app::AppReader::from_bytes(data)?.list_names()),
        _ => Err(invalid("unknown archive format")),
    }
}

/// Compress a file on disk into `dst`.
///
/// The format is taken from `dst`'s extension when `format` is `None`.
pub fn compress_file(src: &Path, dst: &Path, format: Option<Format>) -> Result<()> {
    let format = format
        .or_else(|| Format::from_extension(dst))
        .ok_or_else(|| invalid("cannot infer format from destination extension"))?;
    let level = CompressionLevel::Balanced;
    let bytes = match format {
        Format::Gzip => {
            let data = fs::read(src).map_err(ArchiveError::from)?;
            let name = src
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
            crate::gzip::gzip_compress_with_options(
                &data,
                &GzipOptions {
                    level,
                    mtime: 0,
                    name,
                },
            )
        }
        Format::Zip => {
            // Single file -> one-entry zip; directories use pack_dir().
            let meta = fs::symlink_metadata(src).map_err(ArchiveError::from)?;
            if meta.is_dir() {
                return pack_dir_to_file(src, dst, Format::Zip, level);
            }
            let data = fs::read(src).map_err(ArchiveError::from)?;
            let name = src
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("non-utf8 file name"))?;
            zip_pack(
                &[(name, data.as_slice())],
                &ZipWriterOptions {
                    level,
                    comment: String::new(),
                },
            )?
        }
        Format::Tar => {
            let meta = fs::symlink_metadata(src).map_err(ArchiveError::from)?;
            if meta.is_dir() {
                return pack_dir_to_file(src, dst, Format::Tar, level);
            }
            let data = fs::read(src).map_err(ArchiveError::from)?;
            let name = src
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("non-utf8 file name"))?;
            tar_pack(&[TarEntry::file(name, data)])?
        }
        Format::TarGzip => {
            let meta = fs::symlink_metadata(src).map_err(ArchiveError::from)?;
            if meta.is_dir() {
                return pack_dir_to_file(src, dst, Format::TarGzip, level);
            }
            let data = fs::read(src).map_err(ArchiveError::from)?;
            let name = src
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("non-utf8 file name"))?;
            tar_gzip_compress(&[TarEntry::file(name, data)], level)
        }
        Format::App => {
            let meta = fs::symlink_metadata(src).map_err(ArchiveError::from)?;
            if !meta.is_dir() {
                return Err(invalid("app packs a directory tree, not a single file"));
            }
            let app_name = dst
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("bad destination name"))?;
            crate::app::app_pack_dir(src, app_name, Some(level))?
        }
    };
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(ArchiveError::from)?;
        }
    }
    fs::write(dst, bytes).map_err(ArchiveError::from)?;
    Ok(())
}

fn pack_dir_to_file(
    src: &Path,
    dst: &Path,
    format: Format,
    level: CompressionLevel,
) -> Result<()> {
    let bytes = pack_dir_to_archive(src, format, level)?;
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(ArchiveError::from)?;
        }
    }
    fs::write(dst, bytes).map_err(ArchiveError::from)?;
    Ok(())
}

/// Extract an archive file into a directory (format auto-detected;
/// `.tar.gz`/`.tgz` handled via the extension).
pub fn extract_archive(src: &Path, dst_dir: &Path) -> Result<()> {
    let data = fs::read(src).map_err(ArchiveError::from)?;
    extract_bytes(&data, Some(src), dst_dir)
}

/// Extract in-memory archive bytes into a directory.
pub fn extract_bytes(data: &[u8], src_hint: Option<&Path>, dst_dir: &Path) -> Result<()> {
    // tar.gz first: gzip magic + tar-ish extension or tar payload.
    let looks_tgz = src_hint
        .and_then(|p| p.file_name())
        .map(|n| {
            let l = n.to_string_lossy().to_lowercase();
            l.ends_with(".tar.gz") || l.ends_with(".tgz")
        })
        .unwrap_or(false);
    if looks_tgz {
        let tar = tar_gzip_decompress_raw(data)?;
        return tar_unpack_to_dir(&tar, dst_dir);
    }
    match detect_format(data) {
        Some(Format::Zip) => extract_zip(data, dst_dir),
        Some(Format::Tar) => tar_unpack_to_dir(data, dst_dir),
        Some(Format::App) => {
            let mut reader = crate::app::AppReader::from_bytes(data)?;
            reader.extract_to(dst_dir)
        }
        Some(Format::Gzip) => {
            // Plain gzip: maybe a tar payload without the extension.
            let raw = gzip_decompress(data)?;
            if raw.len() >= 512 && is_tar_block(&raw[..512]) {
                return tar_unpack_to_dir(&raw, dst_dir);
            }
            // Otherwise write a single file: member name or "data".
            let members = crate::gzip::gzip_members(data)?;
            fs::create_dir_all(dst_dir).map_err(ArchiveError::from)?;
            if members.len() == 1 {
                let name = members[0].name.clone().unwrap_or_else(|| "data".to_string());
                let dest = dst_dir.join(safe_file_name(&name));
                fs::write(dest, &members[0].data).map_err(ArchiveError::from)?;
            } else {
                for (i, m) in members.iter().enumerate() {
                    let name = m
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("data.{i}"));
                    fs::write(dst_dir.join(safe_file_name(&name)), &m.data)
                        .map_err(ArchiveError::from)?;
                }
            }
            Ok(())
        }
        _ => Err(invalid("unknown archive format")),
    }
}

fn safe_file_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("data");
    if base.is_empty() {
        "data".to_string()
    } else {
        base.to_string()
    }
}

fn extract_zip(data: &[u8], dst_dir: &Path) -> Result<()> {
    let entries = zip_unpack(data)?;
    for e in &entries {
        if e.name.starts_with('/') || e.name.contains("..") {
            // Reuse the strict check: absolute or parent escapes rejected.
            validate_zip_path(&e.name)?;
        }
    }
    fs::create_dir_all(dst_dir).map_err(ArchiveError::from)?;
    for e in entries {
        let dest = dst_dir.join(Path::new(&e.name));
        if e.is_dir() {
            fs::create_dir_all(&dest).map_err(ArchiveError::from)?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(ArchiveError::from)?;
            }
            fs::write(&dest, &e.data).map_err(ArchiveError::from)?;
        }
    }
    Ok(())
}

fn validate_zip_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(ArchiveError::UnsafePath(path.to_string()));
    }
    let mut depth = 0i32;
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ArchiveError::UnsafePath(path.to_string()));
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_basics() {
        let gz = gzip_compress(b"hi", CompressionLevel::Balanced);
        assert_eq!(detect_format(&gz), Some(Format::Gzip));
        let z = zip_pack(
            &[("a", b"b")],
            &ZipWriterOptions {
                level: CompressionLevel::None,
                comment: String::new(),
            },
        )
        .unwrap();
        assert_eq!(detect_format(&z), Some(Format::Zip));
        let t = tar_pack(&[TarEntry::file("a", b"b".to_vec())]).unwrap();
        assert_eq!(detect_format(&t), Some(Format::Tar));
        assert_eq!(detect_format(b"junk"), None);
    }

    #[test]
    fn tgz_pipeline() {
        let files: &[(&str, &[u8])] = &[("a.txt", b"aaa"), ("d/b.txt", b"bbb")];
        let tgz = tar_gzip_compress_files(files, CompressionLevel::Balanced).unwrap();
        assert_eq!(&tgz[0..2], &[0x1F, 0x8B]);
        let entries = tar_gzip_decompress(&tgz).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].data, b"aaa");
    }

    #[test]
    fn list_names_all_formats() {
        let z = zip_pack(
            &[("a", b"b")],
            &ZipWriterOptions {
                level: CompressionLevel::None,
                comment: String::new(),
            },
        )
        .unwrap();
        assert_eq!(list_names(&z).unwrap(), vec!["a".to_string()]);
        let t = tar_pack(&[TarEntry::file("a", b"b".to_vec())]).unwrap();
        assert_eq!(list_names(&t).unwrap(), vec!["a".to_string()]);
        let gz = gzip_compress(b"hi", CompressionLevel::None);
        assert_eq!(list_names(&gz).unwrap(), vec!["<gzip member 0>".to_string()]);
    }

    #[test]
    fn file_roundtrip_tmp() {
        let dir = std::env::temp_dir().join("archivekit_test_tgz");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        fs::write(dir.join("src/a.txt"), b"alpha").unwrap();
        fs::write(dir.join("src/sub/b.txt"), b"beta").unwrap();

        let dst = dir.join("out.tar.gz");
        compress_file(&dir.join("src"), &dst, None).unwrap();
        let out = dir.join("restored");
        extract_archive(&dst, &out).unwrap();
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"beta");

        let zip_dst = dir.join("out.zip");
        compress_file(&dir.join("src"), &zip_dst, None).unwrap();
        let out2 = dir.join("restored_zip");
        extract_archive(&zip_dst, &out2).unwrap();
        assert_eq!(fs::read(out2.join("a.txt")).unwrap(), b"alpha");
        let _ = fs::remove_dir_all(&dir);
    }
}
