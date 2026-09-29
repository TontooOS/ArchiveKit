//! TontooOS `.app` single-file containers (TAPP format).
//!
//! Unlike macOS folder bundles (and unlike the ZIP-based `.app` files TBuild
//! produces today), a TAPP container carries a central directory in its
//! footer. Listing names or reading one file only touches the footer plus
//! the central directory (or the single entry), so even hundred-megabyte
//! apps open in milliseconds instead of being scanned end to end.
//!
//! Layout inside the container mirrors what TBuild stages on disk:
//!
//! ```text
//! Foo.app/
//!   App/
//!     foo              main binary (0o755)
//!     icon.tico        app icon, Tontoo tico format (no PNG)
//!   Resources/
//!     ...              copied project resources
//!     icon.tico
//!   Info.tontoo        app manifest in Fish Config (.fico) syntax
//! ```
//!
//! The manifest is written and parsed with FishFile. Icons are `.tico`
//! files as defined by [`crate::tico`]: a TICO container (same indexed
//! engine as TAPP, own `TICO`/`TICF` magic) holding `manifest.fico` plus
//! `layer/*.tlyr` layers. ArchiveKit validates that structure natively
//! (magic, version, layer references); decoding and rendering stay in
//! CoreIcon.
//!
//! [`validate_tico`] and [`TicoInfo`] are re-exported here from
//! [`crate::tico`] so existing `archivekit::app::` import paths keep
//! working.

use crate::crc::Crc32;
use crate::deflate::{compress_raw, decompress_raw_limited, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, ArchiveError, Result};
pub use crate::tico::{validate_tico, TicoInfo};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Canonical extension for app containers.
pub const APP_EXTENSION: &str = "app";
/// Header magic `"TAPP"` (u32 LE).
pub const APP_MAGIC: u32 = 0x5050_4154;
/// Container format version written by this crate.
pub const APP_VERSION: u16 = 1;
/// Footer magic `"TAPF"` (u32 LE).
pub const APP_FOOTER_MAGIC: u32 = 0x4650_4154;
/// Manifest file name inside the container (fico syntax).
pub const APP_MANIFEST_NAME: &str = "Info.tontoo";
/// Size of the footer in bytes.
pub const APP_FOOTER_LEN: u64 = 28;

/// Compression method of an app entry (codes match ZIP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppMethod {
    /// Stored raw (default: fastest reads).
    #[default]
    Stored,
    /// DEFLATE compression.
    Deflate,
}

impl AppMethod {
    fn code(self) -> u8 {
        match self {
            AppMethod::Stored => 0,
            AppMethod::Deflate => 8,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(AppMethod::Stored),
            8 => Ok(AppMethod::Deflate),
            _ => Err(crate::error::unsupported(format!(
                "app compression method {code} (only Stored and Deflate are supported)"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Manifest (Fish Config via FishFile)
// ---------------------------------------------------------------------------

/// App manifest, stored as `Info.tontoo` in `.fico` syntax.
///
/// ```text
/// app {
///   bundle_id: com.tontoo.foo
///   version: 26.1.0
///   executable: App/foo
///   icon: App/icon.tico
///   name {
///     en_us: Foo
///     de_de: Foo
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AppManifest {
    /// Reverse-DNS id, e.g. `com.tontoo.foo`.
    pub bundle_id: String,
    /// Human version string.
    pub version: String,
    /// Container-relative path of the main binary, e.g. `App/foo`.
    pub executable: String,
    /// Container-relative path of the `.tico` icon, if any.
    pub icon: Option<String>,
    /// `(locale, display name)` pairs, e.g. `("en_us", "Foo")`.
    pub names: Vec<(String, String)>,
}

impl AppManifest {
    /// Create a manifest with no icon and no names.
    pub fn new(
        bundle_id: impl Into<String>,
        version: impl Into<String>,
        executable: impl Into<String>,
    ) -> Self {
        Self {
            bundle_id: bundle_id.into(),
            version: version.into(),
            executable: executable.into(),
            icon: None,
            names: Vec::new(),
        }
    }

    /// Display name for `locale`, or `None` when missing.
    pub fn name(&self, locale: &str) -> Option<&str> {
        self.names
            .iter()
            .find(|(l, _)| l == locale)
            .map(|(_, n)| n.as_str())
    }

    /// `en_us` name, falling back to the first available name.
    pub fn display_name(&self) -> Option<&str> {
        self.name("en_us")
            .or_else(|| self.names.first().map(|(_, n)| n.as_str()))
    }

    /// Serialize to `.fico` text via FishFile.
    pub fn to_fico(&self) -> String {
        let mut doc = fishfile::FishDocument::new();
        doc.set("app.bundle_id", self.bundle_id.as_str());
        doc.set("app.version", self.version.as_str());
        doc.set("app.executable", self.executable.as_str());
        if let Some(icon) = &self.icon {
            doc.set("app.icon", icon.as_str());
        }
        for (locale, name) in &self.names {
            doc.set(&format!("app.name.{locale}"), name.as_str());
        }
        doc.to_string()
    }

    /// Parse `.fico` text via FishFile.
    pub fn from_fico(text: &str) -> Result<Self> {
        // FishFile is an external crate: never let it panic across our API.
        let doc = std::panic::catch_unwind(|| fishfile::FishDocument::parse(text))
            .map_err(|_| invalid("app manifest crashed the fico parser"))?
            .map_err(|e| invalid(format!("app manifest: {e}")))?;
        let req = |key: &str| {
            doc.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| invalid(format!("app manifest missing '{key}'")))
        };
        let bundle_id = req("app.bundle_id")?;
        let version = req("app.version")?;
        let executable = req("app.executable")?;
        if bundle_id.is_empty() || version.is_empty() || executable.is_empty() {
            return Err(invalid("app manifest has empty required field"));
        }
        validate_relative_path(&executable)
            .map_err(|_| invalid("app manifest has invalid executable path"))?;
        let icon = doc
            .get("app.icon")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref icon) = icon {
            if icon.is_empty() {
                return Err(invalid("app manifest has empty icon path"));
            }
            validate_relative_path(icon)
                .map_err(|_| invalid("app manifest has invalid icon path"))?;
            if !icon.ends_with(".tico") {
                return Err(invalid("app icon must be a .tico file"));
            }
        }
        let mut names = Vec::new();
        if let Some(table) = doc.get("app.name").and_then(|v| v.as_table()) {
            for (locale, value) in table.iter() {
                if let Some(name) = value.as_str() {
                    names.push((locale.clone(), name.to_string()));
                }
            }
        }
        // Fallback for locales with characters FishFile tables cannot hold
        // is unnecessary: locales are identifiers, iteration is complete.
        Ok(Self {
            bundle_id,
            version,
            executable,
            icon,
            names,
        })
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Reject absolute paths and `..` escapes; normalize separators.
fn validate_relative_path(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(ArchiveError::UnsafePath(path.to_string()));
    }
    let normalized = path.replace('\\', "/");
    let p = Path::new(&normalized);
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
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(ArchiveError::UnsafePath(path.to_string()))
            }
        }
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PendingEntry {
    name: String,
    method: AppMethod,
    level: CompressionLevel,
    mode: u32,
    mtime: u64,
    data: Vec<u8>,
}

/// Builds a `.app` container with a `<Name>.app/` top prefix.
#[derive(Debug, Default)]
pub struct AppBuilder {
    top: String,
    entries: Vec<PendingEntry>,
    manifest: Option<AppManifest>,
}

impl AppBuilder {
    /// Create a builder; entries live under `<app_name>.app/`.
    pub fn new(app_name: &str) -> Result<Self> {
        if app_name.is_empty() || app_name.contains('/') || app_name.contains('\\') {
            return Err(invalid("app name must be a plain file stem"));
        }
        Ok(Self {
            top: format!("{app_name}.app/"),
            entries: Vec::new(),
            manifest: None,
        })
    }

    /// Top prefix, e.g. `Foo.app/`.
    pub fn top(&self) -> &str {
        &self.top
    }

    /// Set the manifest (written as `<top>Info.tontoo` in fico syntax).
    pub fn set_manifest(&mut self, manifest: AppManifest) {
        self.manifest = Some(manifest);
    }

    fn full_path(&self, path: &str) -> Result<String> {
        let mut rel = validate_relative_path(path)?;
        while rel.starts_with("./") {
            rel = rel[2..].to_string();
        }
        if let Some(stripped) = rel.strip_prefix(&self.top) {
            rel = stripped.to_string();
        }
        if rel.is_empty() {
            return Err(invalid("empty entry path"));
        }
        Ok(format!("{}{}", self.top, rel))
    }

    /// Add a stored (uncompressed) file.
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        self.add_file_with_mode(path, data, 0o644, AppMethod::Stored, CompressionLevel::Balanced)
    }

    /// Add a DEFLATE-compressed file.
    pub fn add_file_compressed(
        &mut self,
        path: &str,
        data: Vec<u8>,
        level: CompressionLevel,
    ) -> Result<()> {
        self.add_file_with_mode(path, data, 0o644, AppMethod::Deflate, level)
    }

    /// Add a file with explicit mode and method.
    pub fn add_file_with_mode(
        &mut self,
        path: &str,
        data: Vec<u8>,
        mode: u32,
        method: AppMethod,
        level: CompressionLevel,
    ) -> Result<()> {
        let name = self.full_path(path)?;
        self.entries.push(PendingEntry {
            name,
            method,
            level,
            mode,
            mtime: 0,
            data,
        });
        Ok(())
    }

    /// Add a directory (trailing slash added when missing).
    pub fn add_dir(&mut self, path: &str) -> Result<()> {
        let mut name = self.full_path(path)?;
        if !name.ends_with('/') {
            name.push('/');
        }
        self.entries.push(PendingEntry {
            name,
            method: AppMethod::Stored,
            level: CompressionLevel::None,
            mode: 0o755,
            mtime: 0,
            data: Vec::new(),
        });
        Ok(())
    }

    /// Add an executable with `0o755` mode.
    pub fn add_executable(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        self.add_file_with_mode(
            path,
            data,
            0o755,
            AppMethod::Stored,
            CompressionLevel::Balanced,
        )
    }

    /// Add an icon after structural `.tico` validation (no PNG accepted).
    pub fn add_icon_tico(&mut self, path: &str, data: Vec<u8>) -> Result<TicoInfo> {
        if !path.ends_with(".tico") {
            return Err(invalid("app icon must be a .tico file"));
        }
        let info = validate_tico(&data)?;
        let name = self.full_path(path)?;
        self.entries.push(PendingEntry {
            name,
            method: AppMethod::Stored,
            level: CompressionLevel::None,
            mode: 0o644,
            mtime: 0,
            data,
        });
        Ok(info)
    }

    /// Pack a staging tree (TBuild layout: `App/`, `Resources/`, ...).
    ///
    /// `Info.tontoo` at the staging root is reserved: it is skipped here and
    /// must come from [`AppBuilder::set_manifest`] (or be adopted – see
    /// [`AppBuilder::pack_tree_adopt_manifest`]). `*.tico` files are
    /// validated; everything else is stored raw.
    pub fn pack_tree(&mut self, staging: &Path) -> Result<()> {
        self.pack_tree_inner(staging, AppMethod::Stored, CompressionLevel::None)
    }

    /// Pack a staging tree, DEFLATE-compressing every file except `*.tico`
    /// (already a compressed ZIP).
    pub fn pack_tree_compressed(
        &mut self,
        staging: &Path,
        level: CompressionLevel,
    ) -> Result<()> {
        self.pack_tree_inner(staging, AppMethod::Deflate, level)
    }

    fn pack_tree_inner(
        &mut self,
        staging: &Path,
        method: AppMethod,
        level: CompressionLevel,
    ) -> Result<()> {
        let mut names: Vec<PathBuf> = fs::read_dir(staging)
            .map_err(ArchiveError::from)?
            .map(|e| e.map_err(ArchiveError::from).map(|x| x.path()))
            .collect::<Result<_>>()?;
        names.sort();
        // Depth-first with sorted siblings for reproducible output.
        let mut stack: Vec<PathBuf> = names.into_iter().rev().collect();
        while let Some(path) = stack.pop() {
            let rel = path
                .strip_prefix(staging)
                .map_err(|_| invalid("staging path escapes root"))?;
            let arc = rel.to_string_lossy().replace('\\', "/");
            if arc == APP_MANIFEST_NAME {
                continue; // reserved for the generated/adopted manifest
            }
            let meta = fs::symlink_metadata(&path).map_err(ArchiveError::from)?;
            if meta.is_dir() {
                self.add_dir(&arc)?;
                let mut kids: Vec<PathBuf> = fs::read_dir(&path)
                    .map_err(ArchiveError::from)?
                    .map(|e| e.map_err(ArchiveError::from).map(|x| x.path()))
                    .collect::<Result<_>>()?;
                kids.sort();
                for k in kids.into_iter().rev() {
                    stack.push(k);
                }
            } else if meta.is_file() {
                let data = fs::read(&path).map_err(ArchiveError::from)?;
                if arc.ends_with(".tico") {
                    self.add_icon_tico(&arc, data)?;
                } else {
                    let mode = unix_mode(&meta);
                    self.add_file_with_mode(&arc, data, mode, method, level)?;
                }
            } else if meta.is_symlink() {
                return Err(crate::error::unsupported(
                    "symlinks are not packed into .app containers",
                ));
            }
        }
        Ok(())
    }

    /// Adopt `Info.tontoo` (fico) from the staging root as the manifest.
    pub fn pack_tree_adopt_manifest(&mut self, staging: &Path) -> Result<AppManifest> {
        let text = fs::read_to_string(staging.join(APP_MANIFEST_NAME))
            .map_err(|_| invalid("staging has no Info.tontoo manifest"))?;
        let manifest = AppManifest::from_fico(&text)?;
        self.manifest = Some(manifest.clone());
        Ok(manifest)
    }

    /// Finish the container and return its bytes.
    pub fn finish(self) -> Result<Vec<u8>> {
        let manifest = self
            .manifest
            .ok_or_else(|| invalid("app manifest missing: call set_manifest()"))?;
        let mut out = Vec::new();
        out.extend_from_slice(&APP_MAGIC.to_le_bytes());
        out.extend_from_slice(&APP_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // flags

        // Manifest entry first so readers find it without the index.
        let manifest_text = manifest.to_fico();
        let manifest_name = format!("{}{}", self.top, APP_MANIFEST_NAME);

        struct Written {
            name: String,
            method: AppMethod,
            mode: u32,
            mtime: u64,
            crc: u32,
            comp_len: u64,
            raw_len: u64,
            offset: u64,
        }
        let mut written: Vec<Written> = Vec::new();

        let mut entries = self.entries;
        entries.insert(
            0,
            PendingEntry {
                name: manifest_name,
                method: AppMethod::Stored,
                level: CompressionLevel::None,
                mode: 0o644,
                mtime: 0,
                data: manifest_text.into_bytes(),
            },
        );

        for e in &entries {
            let payload = match e.method {
                AppMethod::Stored => e.data.clone(),
                AppMethod::Deflate => compress_raw(&e.data, e.level),
            };
            let mut crc = Crc32::new();
            crc.update(&e.data);
            let offset = out.len() as u64;
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(e.name.as_bytes());
            out.push(e.method.code());
            out.extend_from_slice(&e.mode.to_le_bytes());
            out.extend_from_slice(&e.mtime.to_le_bytes());
            out.extend_from_slice(&crc.finalize().to_le_bytes());
            out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            out.extend_from_slice(&(e.data.len() as u64).to_le_bytes());
            out.extend_from_slice(&payload);
            written.push(Written {
                name: e.name.clone(),
                method: e.method,
                mode: e.mode,
                mtime: e.mtime,
                crc: crc.finalize(),
                comp_len: payload.len() as u64,
                raw_len: e.data.len() as u64,
                offset,
            });
        }

        // Central directory.
        let central_offset = out.len() as u64;
        let mut central: Vec<u8> = Vec::new();
        central.extend_from_slice(&(written.len() as u32).to_le_bytes());
        for w in &written {
            central.extend_from_slice(&(w.name.len() as u16).to_le_bytes());
            central.extend_from_slice(w.name.as_bytes());
            central.push(w.method.code());
            central.extend_from_slice(&w.mode.to_le_bytes());
            central.extend_from_slice(&w.mtime.to_le_bytes());
            central.extend_from_slice(&w.crc.to_le_bytes());
            central.extend_from_slice(&w.comp_len.to_le_bytes());
            central.extend_from_slice(&w.raw_len.to_le_bytes());
            central.extend_from_slice(&w.offset.to_le_bytes());
        }
        let mut central_crc = Crc32::new();
        central_crc.update(&central);
        out.extend_from_slice(&central);
        let central_len = central.len() as u64;

        // Footer.
        out.extend_from_slice(&APP_FOOTER_MAGIC.to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&central_len.to_le_bytes());
        out.extend_from_slice(&(written.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_crc.finalize().to_le_bytes());
        Ok(out)
    }

    /// Finish and write the container to `path` (parents created).
    pub fn write_to_file(self, path: &Path) -> Result<()> {
        let bytes = self.finish()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(ArchiveError::from)?;
            }
        }
        fs::write(path, bytes).map_err(ArchiveError::from)?;
        Ok(())
    }
}

#[cfg(unix)]
fn unix_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

// ---------------------------------------------------------------------------
// Reader (random access over any Read + Seek)
// ---------------------------------------------------------------------------

/// Central-directory metadata of one entry.
#[derive(Debug, Clone)]
pub struct AppEntryMeta {
    /// Full container path, e.g. `Foo.app/App/foo`.
    pub name: String,
    /// Storage method.
    pub method: AppMethod,
    /// Unix permission bits.
    pub mode: u32,
    /// Modification time (Unix timestamp).
    pub mtime: u64,
    /// CRC32 of the raw payload.
    pub crc: u32,
    /// Stored payload length.
    pub comp_len: u64,
    /// Raw payload length.
    pub raw_len: u64,
    /// Absolute offset of the entry record.
    pub offset: u64,
}

impl AppEntryMeta {
    /// True for directory entries (trailing `/`).
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

/// Random-access `.app` reader.
///
/// Listing touches only the footer plus the central directory; `read_file`
/// seeks to a single entry and decodes only its bytes.
pub struct AppReader<R> {
    inner: R,
    entries: Vec<AppEntryMeta>,
}

impl<R: Read + Seek> AppReader<R> {
    /// Load the central directory from an open stream.
    pub fn load(mut inner: R) -> Result<Self> {
        let file_len = inner.seek(SeekFrom::End(0)).map_err(ArchiveError::from)?;
        if file_len < 8 + APP_FOOTER_LEN {
            return Err(invalid("file too small to be an app container"));
        }
        let mut magic = [0u8; 8];
        inner
            .seek(SeekFrom::Start(0))
            .map_err(ArchiveError::from)?;
        inner.read_exact(&mut magic).map_err(ArchiveError::from)?;
        if u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]) != APP_MAGIC {
            return Err(invalid("bad app magic"));
        }
        if u16::from_le_bytes([magic[4], magic[5]]) != APP_VERSION {
            return Err(invalid("unsupported app version"));
        }
        let mut footer = [0u8; APP_FOOTER_LEN as usize];
        inner
            .seek(SeekFrom::End(-(APP_FOOTER_LEN as i64)))
            .map_err(ArchiveError::from)?;
        inner
            .read_exact(&mut footer)
            .map_err(ArchiveError::from)?;
        if u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]) != APP_FOOTER_MAGIC {
            return Err(invalid("bad app footer"));
        }
        let central_offset = u64::from_le_bytes(footer[4..12].try_into().unwrap());
        let central_len = u64::from_le_bytes(footer[12..20].try_into().unwrap());
        let count = u32::from_le_bytes(footer[20..24].try_into().unwrap()) as usize;
        let central_crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());
        let central_end = central_offset
            .checked_add(central_len)
            .ok_or_else(|| invalid("app central directory out of bounds"))?;
        if central_end > file_len {
            return Err(invalid("app central directory out of bounds"));
        }
        let mut central = vec![0u8; central_len as usize];
        inner
            .seek(SeekFrom::Start(central_offset))
            .map_err(ArchiveError::from)?;
        inner
            .read_exact(&mut central)
            .map_err(ArchiveError::from)?;
        let mut check = Crc32::new();
        check.update(&central);
        if check.finalize() != central_crc {
            return Err(invalid("app central directory checksum mismatch"));
        }
        let entries = parse_central(&central, count)?;
        Ok(Self { inner, entries })
    }

    /// Number of entries (files + directories + manifest).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Metadata of entry `index`, or `None` when out of range.
    pub fn entry(&self, index: usize) -> Option<&AppEntryMeta> {
        self.entries.get(index)
    }

    /// Metadata by full container path.
    pub fn find(&self, name: &str) -> Option<&AppEntryMeta> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// All entry names in container order.
    pub fn list_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// Full container path of the manifest (`*/Info.tontoo`).
    pub fn manifest_name(&self) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.name.ends_with(APP_MANIFEST_NAME))
            .map(|e| e.name.as_str())
    }

    /// Read and parse the manifest.
    pub fn read_manifest(&mut self) -> Result<AppManifest> {
        let name = self
            .manifest_name()
            .ok_or_else(|| invalid("app has no manifest"))?
            .to_string();
        let bytes = self.read_file(&name)?;
        let text =
            String::from_utf8(bytes).map_err(|_| invalid("app manifest is not utf-8"))?;
        AppManifest::from_fico(&text)
    }

    /// Read one entry by full container path (dirs yield empty bytes).
    ///
    /// Only this entry's bytes are read from the stream and (for Deflate)
    /// decoded; everything else is skipped by seeking.
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>> {
        let meta = self
            .find(name)
            .cloned()
            .ok_or_else(|| ArchiveError::NotFound(name.to_string()))?;
        if meta.is_dir() {
            return Ok(Vec::new());
        }
        if meta.raw_len > DEFAULT_MAX_OUTPUT as u64 {
            return Err(invalid("app entry exceeds output limit"));
        }
        // Bound the upcoming allocation against the actual stream size.
        let stream_end = self.inner.seek(SeekFrom::End(0)).map_err(ArchiveError::from)?;
        self.inner
            .seek(SeekFrom::Start(meta.offset))
            .map_err(ArchiveError::from)?;
        if meta.offset > stream_end || meta.comp_len > stream_end - meta.offset {
            return Err(invalid("app entry out of bounds"));
        }
        let header = read_record_header(&mut self.inner)?;
        if header.name != meta.name
            || header.method != meta.method
            || header.comp_len != meta.comp_len
            || header.raw_len != meta.raw_len
        {
            return Err(invalid("app entry disagrees with central directory"));
        }
        let mut comp = vec![0u8; meta.comp_len as usize];
        self.inner
            .read_exact(&mut comp)
            .map_err(ArchiveError::from)?;
        let data = match meta.method {
            AppMethod::Stored => {
                if meta.comp_len != meta.raw_len {
                    return Err(invalid("stored app sizes disagree"));
                }
                comp
            }
            AppMethod::Deflate => decompress_raw_limited(&comp, DEFAULT_MAX_OUTPUT).map_err(|e| {
                match e {
                    ArchiveError::InvalidData(m) => {
                        invalid(format!("app deflate error in '{name}': {m}"))
                    }
                    other => other,
                }
            })?,
        };
        if data.len() as u64 != meta.raw_len {
            return Err(invalid("app raw size mismatch"));
        }
        let mut check = Crc32::new();
        check.update(&data);
        if check.finalize() != meta.crc {
            return Err(ArchiveError::ChecksumMismatch {
                expected: meta.crc,
                actual: check.finalize(),
                entry: name.to_string(),
            });
        }
        Ok(data)
    }

    /// Extract the whole container into `dir` (unsafe paths rejected first).
    pub fn extract_to(&mut self, dir: &Path) -> Result<()> {
        let names: Vec<String> = self.list_names();
        for n in &names {
            validate_relative_path(n)?;
        }
        fs::create_dir_all(dir).map_err(ArchiveError::from)?;
        for meta in self.entries.clone() {
            let dest = dir.join(Path::new(&meta.name));
            if meta.is_dir() {
                fs::create_dir_all(&dest).map_err(ArchiveError::from)?;
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(ArchiveError::from)?;
                }
                let data = self.read_file(&meta.name)?;
                fs::write(&dest, data).map_err(ArchiveError::from)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(
                        &dest,
                        fs::Permissions::from_mode(meta.mode),
                    );
                }
            }
        }
        Ok(())
    }
}

impl AppReader<std::fs::File> {
    /// Open a container file (only footer + central directory are read).
    pub fn open(path: &Path) -> Result<Self> {
        let file = fs::File::open(path).map_err(ArchiveError::from)?;
        Self::load(file)
    }
}

impl<'a> AppReader<std::io::Cursor<&'a [u8]>> {
    /// Read a container from memory (footer + central directory only).
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self> {
        Self::load(std::io::Cursor::new(bytes))
    }
}

struct RecordHeader {
    name: String,
    method: AppMethod,
    comp_len: u64,
    raw_len: u64,
}

fn read_record_header<R: Read>(r: &mut R) -> Result<RecordHeader> {
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).map_err(ArchiveError::from)?;
    let name_len = u16::from_le_bytes(len_buf) as usize;
    if name_len == 0 || name_len > 4096 {
        return Err(invalid("bad app entry name length"));
    }
    let mut name_buf = vec![0u8; name_len];
    r.read_exact(&mut name_buf).map_err(ArchiveError::from)?;
    let name =
        String::from_utf8(name_buf).map_err(|_| invalid("app entry name is not utf-8"))?;
    let mut fixed = [0u8; 33];
    r.read_exact(&mut fixed).map_err(ArchiveError::from)?;
    let method = AppMethod::from_code(fixed[0])?;
    let comp_len = u64::from_le_bytes(fixed[17..25].try_into().unwrap());
    let raw_len = u64::from_le_bytes(fixed[25..33].try_into().unwrap());
    Ok(RecordHeader {
        name,
        method,
        comp_len,
        raw_len,
    })
}

fn parse_central(central: &[u8], count: usize) -> Result<Vec<AppEntryMeta>> {
    if central.len() < 4 {
        return Err(invalid("truncated app central directory"));
    }
    let stored = u32::from_le_bytes([central[0], central[1], central[2], central[3]]) as usize;
    if stored != count {
        return Err(invalid("app central count mismatch"));
    }
    let mut entries = Vec::with_capacity(count.min(1_000_000));
    let mut pos = 4;
    for _ in 0..count {
        if pos + 2 > central.len() {
            return Err(invalid("truncated app central entry"));
        }
        let name_len = u16::from_le_bytes([central[pos], central[pos + 1]]) as usize;
        pos += 2;
        if pos + name_len + 33 > central.len() {
            return Err(invalid("truncated app central entry"));
        }
        let name = String::from_utf8(central[pos..pos + name_len].to_vec())
            .map_err(|_| invalid("app entry name is not utf-8"))?;
        pos += name_len;
        let method = AppMethod::from_code(central[pos])?;
        let mode = u32::from_le_bytes(central[pos + 1..pos + 5].try_into().unwrap());
        let mtime = u64::from_le_bytes(central[pos + 5..pos + 13].try_into().unwrap());
        let crc = u32::from_le_bytes(central[pos + 13..pos + 17].try_into().unwrap());
        let comp_len = u64::from_le_bytes(central[pos + 17..pos + 25].try_into().unwrap());
        let raw_len = u64::from_le_bytes(central[pos + 25..pos + 33].try_into().unwrap());
        let offset = u64::from_le_bytes(central[pos + 33..pos + 41].try_into().unwrap());
        pos += 41;
        entries.push(AppEntryMeta {
            name,
            method,
            mode,
            mtime,
            crc,
            comp_len,
            raw_len,
            offset,
        });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Whole-container helpers
// ---------------------------------------------------------------------------

/// Pack a staging tree plus manifest into `.app` bytes.
///
/// The staging dir holds `App/`, `Resources/`, ... (TBuild layout without the
/// top prefix); `Info.tontoo` (fico) is adopted as the manifest. `app_name`
/// becomes the `<app_name>.app/` top prefix.
pub fn app_pack_dir(
    staging: &Path,
    app_name: &str,
    level: Option<CompressionLevel>,
) -> Result<Vec<u8>> {
    let mut builder = AppBuilder::new(app_name)?;
    builder.pack_tree_adopt_manifest(staging)?;
    match level {
        Some(l) => builder.pack_tree_compressed(staging, l)?,
        None => builder.pack_tree(staging)?,
    }
    builder.finish()
}

/// Extract a `.app` container file into a directory.
pub fn app_extract_to_file(data_path: &Path, dir: &Path) -> Result<()> {
    AppReader::open(data_path)?.extract_to(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Counts bytes pulled through `read` (seeks are free to count).
    struct Counting<R> {
        inner: R,
        read_bytes: u64,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read_bytes += n as u64;
            Ok(n)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn demo_manifest() -> AppManifest {
        AppManifest {
            bundle_id: "com.tontoo.demo".to_string(),
            version: "1.0.0".to_string(),
            executable: "App/demo".to_string(),
            icon: Some("App/icon.tico".to_string()),
            names: vec![
                ("en_us".to_string(), "Demo".to_string()),
                ("de_de".to_string(), "Demo".to_string()),
            ],
        }
    }

    fn minimal_tico() -> Vec<u8> {
        // manifest.fico + one layer, packed with the TICO container engine.
        let mut tlyr = Vec::new();
        tlyr.extend_from_slice(b"TLYR");
        tlyr.push(1);
        tlyr.push(1); // kind
        tlyr.extend_from_slice(&64u32.to_le_bytes());
        tlyr.extend_from_slice(&64u32.to_le_bytes());
        tlyr.extend_from_slice(&4u32.to_le_bytes());
        tlyr.extend_from_slice(&[1, 2, 3, 4]);
        let manifest = crate::tico::TicoManifest {
            name: "demo".to_string(),
            canvas: 1024,
            background: crate::tico::TicoBackground::Color {
                color: "#FFFFFF".to_string(),
            },
            layers: vec![crate::tico::TicoLayerMeta {
                file: "layer/00.tlyr".to_string(),
                opacity: 1.0,
                recolorable: true,
                default_color: "#000000".to_string(),
            }],
        };
        let mut b = crate::tico::TicoBuilder::new();
        b.set_manifest(manifest);
        b.add_layer("layer/00.tlyr", tlyr).unwrap();
        b.finish().unwrap()
    }

    #[test]
    fn manifest_fico_roundtrip() {
        let m = demo_manifest();
        let text = m.to_fico();
        assert!(text.contains("bundle_id"));
        assert!(text.contains("com.tontoo.demo"));
        // Real FishFile parses what we emit.
        let doc = fishfile::FishDocument::parse(&text).unwrap();
        assert_eq!(
            doc.get("app.bundle_id").unwrap().as_str(),
            Some("com.tontoo.demo")
        );
        let back = AppManifest::from_fico(&text).unwrap();
        assert_eq!(back.bundle_id, "com.tontoo.demo");
        assert_eq!(back.name("de_de"), Some("Demo"));
        assert_eq!(back.display_name(), Some("Demo"));
        assert_eq!(back.icon.as_deref(), Some("App/icon.tico"));
    }

    #[test]
    fn manifest_rejects_bad() {
        assert!(AppManifest::from_fico("app { version: 1 }").is_err());
        assert!(AppManifest::from_fico("app { bundle_id: x version: 1 executable: App/x icon: App/i.png }").is_err());
    }

    #[test]
    fn tico_validation() {
        let info = validate_tico(&minimal_tico()).unwrap();
        assert_eq!(info.layers, vec!["layer/00.tlyr".to_string()]);
        assert!(validate_tico(b"not a tico").is_err());
        // ZIP bytes are not valid tico containers anymore.
        let zip = crate::zip::zip_pack(
            &[("manifest.fico", b"tico { format: tico }" as &[u8])],
            &crate::zip::ZipWriterOptions {
                level: CompressionLevel::None,
                comment: String::new(),
            },
        )
        .unwrap();
        assert!(validate_tico(&zip).is_err());
        // Craft invalid: png inside.
        let manifest = crate::tico::TicoManifest {
            name: "bad".to_string(),
            canvas: 1024,
            background: crate::tico::TicoBackground::Color {
                color: "#FFFFFF".to_string(),
            },
            layers: vec![crate::tico::TicoLayerMeta {
                file: "layer/00.tlyr".to_string(),
                opacity: 1.0,
                recolorable: true,
                default_color: "#000000".to_string(),
            }],
        };
        let mut tlyr = Vec::new();
        tlyr.extend_from_slice(b"TLYR");
        tlyr.push(1);
        tlyr.push(1);
        tlyr.extend_from_slice(&64u32.to_le_bytes());
        tlyr.extend_from_slice(&64u32.to_le_bytes());
        tlyr.extend_from_slice(&4u32.to_le_bytes());
        tlyr.extend_from_slice(&[1, 2, 3, 4]);
        let mut b = crate::tico::TicoBuilder::new();
        b.set_manifest(manifest);
        b.add_layer("layer/00.tlyr", tlyr).unwrap();
        b.add_file("a.png", b"png".to_vec()).unwrap();
        assert!(validate_tico(&b.finish().unwrap()).is_err());
    }

    #[test]
    fn builder_reader_roundtrip() {
        let mut b = AppBuilder::new("Demo").unwrap();
        b.set_manifest(demo_manifest());
        b.add_dir("App").unwrap();
        b.add_executable("App/demo", b"fake-binary-bytes".to_vec())
            .unwrap();
        b.add_icon_tico("App/icon.tico", minimal_tico()).unwrap();
        b.add_dir("Resources").unwrap();
        b.add_file_compressed(
            "Resources/data.txt",
            b"hello compressed".to_vec(),
            CompressionLevel::Balanced,
        )
        .unwrap();
        let bytes = b.finish().unwrap();
        assert_eq!(&bytes[0..4], b"TAPP");

        let mut r = AppReader::from_bytes(&bytes).unwrap();
        assert_eq!(r.entry_count(), 6); // manifest + 5
        assert!(r.manifest_name().unwrap().ends_with("Info.tontoo"));
        let m = r.read_manifest().unwrap();
        assert_eq!(m.bundle_id, "com.tontoo.demo");
        assert_eq!(
            r.read_file("Demo.app/App/demo").unwrap(),
            b"fake-binary-bytes"
        );
        assert_eq!(
            r.read_file("Demo.app/Resources/data.txt").unwrap(),
            b"hello compressed"
        );
        assert!(r.read_file("Demo.app/Resources/").unwrap().is_empty());
        assert!(matches!(
            r.read_file("Demo.app/nope"),
            Err(ArchiveError::NotFound(_))
        ));
    }

    #[test]
    fn listing_and_single_read_are_partial() {
        // 16 MiB stored blob: listing must not stream it.
        let big = vec![0xABu8; 16 * 1024 * 1024];
        let mut b = AppBuilder::new("Big").unwrap();
        b.set_manifest(AppManifest::new("com.tontoo.big", "1", "App/big"));
        b.add_file("App/big.bin", big).unwrap();
        b.add_file("App/small.txt", b"small".to_vec()).unwrap();
        let bytes = b.finish().unwrap();

        let counting = Counting {
            inner: Cursor::new(bytes.as_slice()),
            read_bytes: 0,
        };
        let mut r = AppReader::load(counting).unwrap();
        let listed = r.list_names();
        assert_eq!(listed.len(), 3);
        assert!(r.inner.read_bytes < 64 * 1024, "listing read {} bytes", r.inner.read_bytes);

        let small = r.read_file("Big.app/App/small.txt").unwrap();
        assert_eq!(small, b"small");
        assert!(
            r.inner.read_bytes < 1024 * 1024,
            "single read pulled {} bytes",
            r.inner.read_bytes
        );
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut b = AppBuilder::new("C").unwrap();
        b.set_manifest(AppManifest::new("com.tontoo.c", "1", "App/c"));
        b.add_file("App/c", b"data".to_vec()).unwrap();
        let mut bytes = b.finish().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // corrupt footer/central
        assert!(AppReader::from_bytes(&bytes).is_err());
    }

    #[test]
    fn unsafe_paths_rejected() {
        let mut b = AppBuilder::new("U").unwrap();
        assert!(b.add_file("/abs", b"x".to_vec()).is_err());
        assert!(b.add_file("../evil", b"x".to_vec()).is_err());
    }

    #[test]
    fn manifest_required() {
        let mut b = AppBuilder::new("M").unwrap();
        b.add_file("App/m", b"x".to_vec()).unwrap();
        assert!(b.finish().is_err());
    }

    #[test]
    fn file_backed_open_and_extract() {
        let dir = std::env::temp_dir().join("archivekit_app_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("staging/App")).unwrap();
        fs::write(dir.join("staging/App/x"), b"binary!").unwrap();
        fs::write(dir.join("staging/App/icon.tico"), minimal_tico()).unwrap();
        fs::write(dir.join("staging/Info.tontoo"), demo_manifest().to_fico()).unwrap();

        let bytes = app_pack_dir(&dir.join("staging"), "Demo", None).unwrap();
        fs::write(dir.join("Demo.app"), &bytes).unwrap();

        // open() reads footer + central only (fast path smoke test).
        let mut r = AppReader::open(&dir.join("Demo.app")).unwrap();
        assert_eq!(r.read_manifest().unwrap().version, "1.0.0");

        r.extract_to(&dir.join("out")).unwrap();
        assert_eq!(
            fs::read(dir.join("out/Demo.app/App/x")).unwrap(),
            b"binary!"
        );
        let _ = fs::remove_dir_all(&dir);
    }

}
