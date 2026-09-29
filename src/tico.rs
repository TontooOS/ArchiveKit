//! TontooOS `.tico` icon containers (TICO format).
//!
//! A `.tico` file uses the same indexed single-file engine as `.app`
//! (TAPP) containers: header magic, entry records, a central directory and
//! a CRC-checked footer. Listing names or reading one layer only touches
//! the footer plus the central directory (or the single entry), so icons
//! open in milliseconds without scanning the whole file.
//!
//! Layout inside the container (flat, no top prefix unlike `.app`):
//!
//! ```text
//! icon.tico
//!   manifest.fico        icon manifest in Fish Config (.fico) syntax
//!   layer/00.tlyr        one custom layer file per entry
//!   layer/01.tlyr        ...
//!   layer/background.tlyr  raster background (only for photo backgrounds)
//! ```
//!
//! A `.tlyr` file is a tiny custom format: magic `TLYR`, version byte `1`,
//! dimensions and PNG-coded RGBA bytes. Layers are stored at full 1024px.
//! Full decoding and rendering of layers stays in CoreIcon
//! (`CoreIcon/src/tico.rs`); ArchiveKit only packs, indexes and
//! structurally validates the container.
//!
//! The manifest is written and parsed with FishFile:
//!
//! ```text
//! tico {
//!   format: tico
//!   version: 1
//!   name: demo
//!   canvas: 1024
//!   background {
//!     kind: color
//!     color: "#1D1D1D"
//!   }
//!   files: [layer/00.tlyr]
//!   opacity: [1.0]
//!   recolorable: [true]
//!   default_color: ["#FFFFFF"]
//! }
//! ```
//!
//! Layers are parallel scalar arrays (`files`, `opacity`, `recolorable`,
//! `default_color`): same length, index-aligned. This uses only plain
//! `.fico` constructs (nested tables with identifier keys, scalar arrays),
//! so any FishFile parser reads what this crate writes.

use crate::crc::Crc32;
use crate::deflate::{compress_raw, decompress_raw_limited, CompressionLevel, DEFAULT_MAX_OUTPUT};
use crate::error::{invalid, ArchiveError, Result};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Canonical extension for icon containers.
pub const TICO_EXTENSION: &str = "tico";
/// Header magic `"TICO"` (u32 LE).
pub const TICO_MAGIC: u32 = 0x4F43_4954;
/// Container format version written by this crate.
pub const TICO_VERSION: u16 = 1;
/// Footer magic `"TICF"` (u32 LE).
pub const TICO_FOOTER_MAGIC: u32 = 0x4643_4954;
/// Manifest file name inside the container (fico syntax).
pub const TICO_MANIFEST_NAME: &str = "manifest.fico";
/// Size of the footer in bytes.
pub const TICO_FOOTER_LEN: u64 = 28;
/// Magic bytes at the start of every `.tlyr` file.
pub const TLYR_MAGIC: &[u8; 4] = b"TLYR";
/// `.tlyr` format version accepted by this crate.
pub const TLYR_VERSION: u8 = 1;

/// Compression method of a tico entry (codes match ZIP and TAPP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TicoMethod {
    /// Stored raw (default: fastest reads).
    #[default]
    Stored,
    /// DEFLATE compression.
    Deflate,
}

impl TicoMethod {
    fn code(self) -> u8 {
        match self {
            TicoMethod::Stored => 0,
            TicoMethod::Deflate => 8,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(TicoMethod::Stored),
            8 => Ok(TicoMethod::Deflate),
            _ => Err(crate::error::unsupported(format!(
                "tico compression method {code} (only Stored and Deflate are supported)"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Manifest (Fish Config via FishFile)
// ---------------------------------------------------------------------------

/// Background of a `.tico` icon (no raster needed for flat icons).
#[derive(Debug, Clone, PartialEq)]
pub enum TicoBackground {
    /// Flat color, e.g. `#1D1D1D`.
    Color { color: String },
    /// Gradient with per-stop colors, positions and direction name
    /// (e.g. `TopToBottom`).
    Gradient {
        colors: Vec<String>,
        positions: Vec<f32>,
        direction: String,
    },
    /// Photo background embedded as its own raster layer file.
    Raster { file: String },
}

/// One entry of the manifest layer table.
#[derive(Debug, Clone, PartialEq)]
pub struct TicoLayerMeta {
    /// Container-relative layer path, e.g. `layer/00.tlyr`.
    pub file: String,
    /// Layer opacity (`0.0`–`1.0`).
    pub opacity: f32,
    /// Whether CoreIcon may recolor the layer with a tint.
    pub recolorable: bool,
    /// Fill color used when no tint is applied, e.g. `#FFFFFF`.
    pub default_color: String,
}

/// Icon manifest, stored as `manifest.fico` in `.fico` syntax.
#[derive(Debug, Clone, PartialEq)]
pub struct TicoManifest {
    /// Icon display name.
    pub name: String,
    /// Canvas size in px (layers are stored at this size, usually 1024).
    pub canvas: u32,
    /// Background description.
    pub background: TicoBackground,
    /// Layer table in container order.
    pub layers: Vec<TicoLayerMeta>,
}

impl TicoManifest {
    /// Create a manifest with no layers.
    pub fn new(name: impl Into<String>, canvas: u32, background: TicoBackground) -> Self {
        Self {
            name: name.into(),
            canvas,
            background,
            layers: Vec::new(),
        }
    }

    /// Serialize to `.fico` text via FishFile.
    pub fn to_fico(&self) -> String {
        let mut doc = fishfile::FishDocument::new();
        doc.set("tico.format", "tico");
        doc.set("tico.version", 1);
        doc.set("tico.name", self.name.as_str());
        doc.set("tico.canvas", self.canvas as i64);
        match &self.background {
            TicoBackground::Color { color } => {
                doc.set("tico.background.kind", "color");
                doc.set("tico.background.color", color.as_str());
            }
            TicoBackground::Gradient {
                colors,
                positions,
                direction,
            } => {
                doc.set("tico.background.kind", "gradient");
                doc.set("tico.background.direction", direction.as_str());
                doc.set(
                    "tico.background.colors",
                    fishfile::FishValue::Array(
                        colors.iter().map(|c| fishfile::FishValue::String(c.clone())).collect(),
                    ),
                );
                doc.set(
                    "tico.background.positions",
                    fishfile::FishValue::Array(
                        positions.iter().map(|p| fishfile::FishValue::Float(*p as f64)).collect(),
                    ),
                );
            }
            TicoBackground::Raster { file } => {
                doc.set("tico.background.kind", "raster");
                doc.set("tico.background.file", file.as_str());
            }
        }
        doc.set(
            "tico.files",
            fishfile::FishValue::Array(
                self.layers
                    .iter()
                    .map(|l| fishfile::FishValue::String(l.file.clone()))
                    .collect(),
            ),
        );
        doc.set(
            "tico.opacity",
            fishfile::FishValue::Array(
                self.layers
                    .iter()
                    .map(|l| fishfile::FishValue::Float(l.opacity as f64))
                    .collect(),
            ),
        );
        doc.set(
            "tico.recolorable",
            fishfile::FishValue::Array(
                self.layers
                    .iter()
                    .map(|l| fishfile::FishValue::Bool(l.recolorable))
                    .collect(),
            ),
        );
        doc.set(
            "tico.default_color",
            fishfile::FishValue::Array(
                self.layers
                    .iter()
                    .map(|l| fishfile::FishValue::String(l.default_color.clone()))
                    .collect(),
            ),
        );
        doc.to_string()
    }

    /// Parse `.fico` text via FishFile.
    pub fn from_fico(text: &str) -> Result<Self> {
        // FishFile is an external crate: never let it panic across our API.
        let doc = std::panic::catch_unwind(|| fishfile::FishDocument::parse(text))
            .map_err(|_| invalid("tico manifest crashed the fico parser"))?
            .map_err(|e| invalid(format!("tico manifest: {e}")))?;
        let format = doc
            .get("tico.format")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid("tico manifest missing 'tico.format'"))?;
        if format != "tico" {
            return Err(invalid("tico manifest is not format tico"));
        }
        let version = doc
            .get("tico.version")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| invalid("tico manifest missing 'tico.version'"))?;
        if version != 1 {
            return Err(invalid(format!("unsupported tico v{version}")));
        }
        let name = doc
            .get("tico.name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid("tico manifest missing 'tico.name'"))?;
        if name.is_empty() {
            return Err(invalid("tico manifest has empty name"));
        }
        let canvas = doc
            .get("tico.canvas")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| invalid("tico manifest missing 'tico.canvas'"))?;
        if !(16..=4096).contains(&canvas) {
            return Err(invalid("tico manifest has invalid canvas size"));
        }
        let kind = doc
            .get("tico.background.kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid("tico manifest missing background kind"))?;
        let background = match kind {
            "color" => {
                let color = doc
                    .get("tico.background.color")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("tico color background missing color"))?;
                validate_hex_color(color)?;
                TicoBackground::Color {
                    color: color.to_string(),
                }
            }
            "gradient" => {
                let direction = doc
                    .get("tico.background.direction")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("tico gradient background missing direction"))?;
                validate_direction(direction)?;
                let colors = doc
                    .get("tico.background.colors")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| invalid("tico gradient background missing colors"))?;
                let mut out_colors = Vec::with_capacity(colors.len());
                for c in colors {
                    let s = c
                        .as_str()
                        .ok_or_else(|| invalid("tico gradient color is not a string"))?;
                    validate_hex_color(s)?;
                    out_colors.push(s.to_string());
                }
                let positions = doc
                    .get("tico.background.positions")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| invalid("tico gradient background missing positions"))?;
                let mut out_positions = Vec::with_capacity(positions.len());
                for p in positions {
                    let f = p
                        .as_f64()
                        .ok_or_else(|| invalid("tico gradient position is not a number"))?;
                    if !(0.0..=1.0).contains(&f) {
                        return Err(invalid("tico gradient position out of range"));
                    }
                    out_positions.push(f as f32);
                }
                if out_colors.len() < 2 || out_colors.len() != out_positions.len() {
                    return Err(invalid("tico gradient needs matching colors and positions"));
                }
                TicoBackground::Gradient {
                    colors: out_colors,
                    positions: out_positions,
                    direction: direction.to_string(),
                }
            }
            "raster" => {
                let file = doc
                    .get("tico.background.file")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("tico raster background missing file"))?;
                validate_layer_path(file)?;
                TicoBackground::Raster {
                    file: file.to_string(),
                }
            }
            _ => return Err(invalid(format!("tico has unknown background kind '{kind}'"))),
        };
        // Layers are parallel scalar arrays: same length, index-aligned.
        let files = doc
            .get("tico.files")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("tico manifest missing 'tico.files'"))?;
        let opacity = doc
            .get("tico.opacity")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("tico manifest missing 'tico.opacity'"))?;
        let recolorable = doc
            .get("tico.recolorable")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("tico manifest missing 'tico.recolorable'"))?;
        let default_color = doc
            .get("tico.default_color")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("tico manifest missing 'tico.default_color'"))?;
        if files.is_empty() {
            return Err(invalid("tico has no layers"));
        }
        if opacity.len() != files.len()
            || recolorable.len() != files.len()
            || default_color.len() != files.len()
        {
            return Err(invalid("tico layer arrays have mismatched lengths"));
        }
        let mut layers = Vec::with_capacity(files.len());
        for i in 0..files.len() {
            let file = files[i]
                .as_str()
                .ok_or_else(|| invalid(format!("tico layer {i} file is not a string")))?;
            validate_layer_path(file)?;
            let op = opacity[i]
                .as_f64()
                .ok_or_else(|| invalid(format!("tico layer {i} opacity is not a number")))?;
            if !(0.0..=1.0).contains(&op) {
                return Err(invalid(format!("tico layer {i} opacity out of range")));
            }
            let rec = recolorable[i]
                .as_bool()
                .ok_or_else(|| invalid(format!("tico layer {i} recolorable is not a bool")))?;
            let dc = default_color[i]
                .as_str()
                .ok_or_else(|| invalid(format!("tico layer {i} default_color is not a string")))?;
            validate_hex_color(dc)?;
            layers.push(TicoLayerMeta {
                file: file.to_string(),
                opacity: op as f32,
                recolorable: rec,
                default_color: dc.to_string(),
            });
        }
        Ok(Self {
            name: name.to_string(),
            canvas: canvas as u32,
            background,
            layers,
        })
    }
}

fn validate_hex_color(color: &str) -> Result<()> {
    let hex = color.strip_prefix('#').unwrap_or(color);
    if !(hex.len() == 6 || hex.len() == 8)
        || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        || !color.starts_with('#')
    {
        return Err(invalid(format!("tico has invalid color '{color}'")));
    }
    Ok(())
}

fn validate_direction(direction: &str) -> Result<()> {
    match direction {
        "TopToBottom" | "BottomToTop" | "LeftToRight" | "RightToLeft" | "TopLeadingToBottomTrailing"
        | "TopTrailingToBottomLeading" | "CenterRadial" => Ok(()),
        _ => Err(invalid(format!(
            "tico has unknown gradient direction '{direction}'"
        ))),
    }
}

fn validate_layer_path(path: &str) -> Result<()> {
    validate_relative_path(path)?;
    if !path.ends_with(".tlyr") {
        return Err(invalid(format!("tico layer '{path}' must be a .tlyr file")));
    }
    if path.contains(".png") {
        return Err(invalid("tico must not contain .png files"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// .tico structural validation (no rendering)
// ---------------------------------------------------------------------------

/// Validated `.tico` contents: layer files referenced by the manifest.
#[derive(Debug, Clone)]
pub struct TicoInfo {
    /// Layer paths inside the `.tico` (e.g. `layer/00.tlyr`).
    pub layers: Vec<String>,
}

/// Validate `.tico` bytes structurally (no rendering).
///
/// Rules:
/// - the file is a TICO container (magic, version, footer, central dir)
/// - it holds `manifest.fico` plus `layer/*.tlyr` files
/// - the manifest declares `format: tico` with a supported version
/// - every `.tlyr` starts with `TLYR` magic and version byte `1`, and its
///   declared payload length fits
/// - every layer file is referenced by the manifest, and every `file`
///   reference exists
/// - no `.png` files (tico never embeds PNG)
///
/// Full decoding/rendering stays in CoreIcon.
pub fn validate_tico(bytes: &[u8]) -> Result<TicoInfo> {
    let mut reader = TicoReader::from_bytes(bytes)
        .map_err(|e| invalid(format!("tico is not a valid container: {e}")))?;
    let names = reader.list_names();
    if !names.iter().any(|n| n == TICO_MANIFEST_NAME) {
        return Err(invalid("tico missing manifest.fico"));
    }
    for n in &names {
        if n.ends_with(".png") {
            return Err(invalid("tico must not contain .png files"));
        }
    }
    let manifest = reader.read_manifest()?;
    let mut refs: Vec<String> = manifest.layers.iter().map(|l| l.file.clone()).collect();
    if let TicoBackground::Raster { file } = &manifest.background {
        refs.push(file.clone());
    }
    for r in &refs {
        if !names.iter().any(|n| n == r) {
            return Err(invalid(format!("tico references missing file '{r}'")));
        }
    }
    for n in &names {
        if n == TICO_MANIFEST_NAME {
            continue;
        }
        if n.ends_with(".tlyr") {
            if !refs.iter().any(|r| r == n) {
                return Err(invalid(format!("tico layer '{n}' is not referenced")));
            }
            let data = reader.read_file(n)?;
            validate_tlyr(&data)?;
        } else if !n.ends_with('/') {
            return Err(invalid(format!("tico has unexpected file '{n}'")));
        }
    }
    Ok(TicoInfo {
        layers: manifest.layers.iter().map(|l| l.file.clone()).collect(),
    })
}

/// Check one `.tlyr` blob: `TLYR` magic, version 1, fitting payload length.
fn validate_tlyr(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 18 {
        return Err(invalid("tlyr too short"));
    }
    if &bytes[0..4] != TLYR_MAGIC {
        return Err(invalid("bad tlyr magic"));
    }
    if bytes[4] != TLYR_VERSION {
        return Err(invalid(format!("unsupported tlyr v{}", bytes[4])));
    }
    let len = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]) as usize;
    if bytes.len() < 18 + len {
        return Err(invalid("tlyr truncated"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Reject absolute paths and `..` escapes; normalize separators.
pub(crate) fn validate_relative_path(path: &str) -> Result<String> {
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
    method: TicoMethod,
    level: CompressionLevel,
    mode: u32,
    mtime: u64,
    data: Vec<u8>,
}

/// Builds a `.tico` icon container (flat entries, no top prefix).
#[derive(Debug, Default)]
pub struct TicoBuilder {
    entries: Vec<PendingEntry>,
    manifest: Option<TicoManifest>,
}

impl TicoBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            manifest: None,
        }
    }

    /// Set the manifest (written as `manifest.fico` in fico syntax).
    pub fn set_manifest(&mut self, manifest: TicoManifest) {
        self.manifest = Some(manifest);
    }

    /// Add a stored (uncompressed) file.
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        self.add_file_with_mode(path, data, 0o644, TicoMethod::Stored, CompressionLevel::None)
    }

    /// Add a DEFLATE-compressed file.
    pub fn add_file_compressed(
        &mut self,
        path: &str,
        data: Vec<u8>,
        level: CompressionLevel,
    ) -> Result<()> {
        self.add_file_with_mode(path, data, 0o644, TicoMethod::Deflate, level)
    }

    /// Add a file with explicit mode and method.
    pub fn add_file_with_mode(
        &mut self,
        path: &str,
        data: Vec<u8>,
        mode: u32,
        method: TicoMethod,
        level: CompressionLevel,
    ) -> Result<()> {
        let name = validate_relative_path(path)?;
        if name == TICO_MANIFEST_NAME {
            return Err(invalid("manifest.fico is reserved for the manifest"));
        }
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

    /// Add a `.tlyr` layer after structural validation.
    pub fn add_layer(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        validate_layer_path(path)?;
        validate_tlyr(&data)?;
        let name = validate_relative_path(path)?;
        self.entries.push(PendingEntry {
            name,
            method: TicoMethod::Stored,
            level: CompressionLevel::None,
            mode: 0o644,
            mtime: 0,
            data,
        });
        Ok(())
    }

    /// Finish the container and return its bytes.
    pub fn finish(self) -> Result<Vec<u8>> {
        let manifest = self
            .manifest
            .ok_or_else(|| invalid("tico manifest missing: call set_manifest()"))?;
        if manifest.layers.is_empty() {
            return Err(invalid("tico has no layers"));
        }
        let mut out = Vec::new();
        out.extend_from_slice(&TICO_MAGIC.to_le_bytes());
        out.extend_from_slice(&TICO_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // flags

        // Manifest entry first so readers find it without the index.
        struct Written {
            name: String,
            method: TicoMethod,
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
                name: TICO_MANIFEST_NAME.to_string(),
                method: TicoMethod::Stored,
                level: CompressionLevel::None,
                mode: 0o644,
                mtime: 0,
                data: manifest.to_fico().into_bytes(),
            },
        );

        for e in &entries {
            let payload = match e.method {
                TicoMethod::Stored => e.data.clone(),
                TicoMethod::Deflate => compress_raw(&e.data, e.level),
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
        out.extend_from_slice(&TICO_FOOTER_MAGIC.to_le_bytes());
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

// ---------------------------------------------------------------------------
// Reader (random access over any Read + Seek)
// ---------------------------------------------------------------------------

/// Central-directory metadata of one entry.
#[derive(Debug, Clone)]
pub struct TicoEntryMeta {
    /// Container path, e.g. `layer/00.tlyr`.
    pub name: String,
    /// Storage method.
    pub method: TicoMethod,
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

impl TicoEntryMeta {
    /// True for directory entries (trailing `/`).
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

/// Random-access `.tico` reader.
///
/// Listing touches only the footer plus the central directory; `read_file`
/// seeks to a single entry and decodes only its bytes.
pub struct TicoReader<R> {
    inner: R,
    entries: Vec<TicoEntryMeta>,
}

impl<R: Read + Seek> TicoReader<R> {
    /// Load the central directory from an open stream.
    pub fn load(mut inner: R) -> Result<Self> {
        let file_len = inner.seek(SeekFrom::End(0)).map_err(ArchiveError::from)?;
        if file_len < 8 + TICO_FOOTER_LEN {
            return Err(invalid("file too small to be a tico container"));
        }
        let mut magic = [0u8; 8];
        inner
            .seek(SeekFrom::Start(0))
            .map_err(ArchiveError::from)?;
        inner.read_exact(&mut magic).map_err(ArchiveError::from)?;
        if u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]) != TICO_MAGIC {
            return Err(invalid("bad tico magic"));
        }
        if u16::from_le_bytes([magic[4], magic[5]]) != TICO_VERSION {
            return Err(invalid("unsupported tico version"));
        }
        let mut footer = [0u8; TICO_FOOTER_LEN as usize];
        inner
            .seek(SeekFrom::End(-(TICO_FOOTER_LEN as i64)))
            .map_err(ArchiveError::from)?;
        inner
            .read_exact(&mut footer)
            .map_err(ArchiveError::from)?;
        if u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]) != TICO_FOOTER_MAGIC {
            return Err(invalid("bad tico footer"));
        }
        let central_offset = u64::from_le_bytes(footer[4..12].try_into().unwrap());
        let central_len = u64::from_le_bytes(footer[12..20].try_into().unwrap());
        let count = u32::from_le_bytes(footer[20..24].try_into().unwrap()) as usize;
        let central_crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());
        let central_end = central_offset
            .checked_add(central_len)
            .ok_or_else(|| invalid("tico central directory out of bounds"))?;
        if central_end > file_len {
            return Err(invalid("tico central directory out of bounds"));
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
            return Err(invalid("tico central directory checksum mismatch"));
        }
        let entries = parse_central(&central, count)?;
        Ok(Self { inner, entries })
    }

    /// Number of entries (files + manifest).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Metadata of entry `index`, or `None` when out of range.
    pub fn entry(&self, index: usize) -> Option<&TicoEntryMeta> {
        self.entries.get(index)
    }

    /// Metadata by container path.
    pub fn find(&self, name: &str) -> Option<&TicoEntryMeta> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// All entry names in container order.
    pub fn list_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// Read and parse the manifest.
    pub fn read_manifest(&mut self) -> Result<TicoManifest> {
        let bytes = self.read_file(TICO_MANIFEST_NAME)?;
        let text =
            String::from_utf8(bytes).map_err(|_| invalid("tico manifest is not utf-8"))?;
        TicoManifest::from_fico(&text)
    }

    /// Read one entry by container path.
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
            return Err(invalid("tico entry exceeds output limit"));
        }
        // Bound the upcoming allocation against the actual stream size.
        let stream_end = self.inner.seek(SeekFrom::End(0)).map_err(ArchiveError::from)?;
        self.inner
            .seek(SeekFrom::Start(meta.offset))
            .map_err(ArchiveError::from)?;
        if meta.offset > stream_end || meta.comp_len > stream_end - meta.offset {
            return Err(invalid("tico entry out of bounds"));
        }
        let header = read_record_header(&mut self.inner)?;
        if header.name != meta.name
            || header.method != meta.method
            || header.comp_len != meta.comp_len
            || header.raw_len != meta.raw_len
        {
            return Err(invalid("tico entry disagrees with central directory"));
        }
        let mut comp = vec![0u8; meta.comp_len as usize];
        self.inner
            .read_exact(&mut comp)
            .map_err(ArchiveError::from)?;
        let data = match meta.method {
            TicoMethod::Stored => {
                if meta.comp_len != meta.raw_len {
                    return Err(invalid("stored tico sizes disagree"));
                }
                comp
            }
            TicoMethod::Deflate => decompress_raw_limited(&comp, DEFAULT_MAX_OUTPUT).map_err(|e| {
                match e {
                    ArchiveError::InvalidData(m) => {
                        invalid(format!("tico deflate error in '{name}': {m}"))
                    }
                    other => other,
                }
            })?,
        };
        if data.len() as u64 != meta.raw_len {
            return Err(invalid("tico raw size mismatch"));
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
            }
        }
        Ok(())
    }
}

impl TicoReader<std::fs::File> {
    /// Open a container file (only footer + central directory are read).
    pub fn open(path: &Path) -> Result<Self> {
        let file = fs::File::open(path).map_err(ArchiveError::from)?;
        Self::load(file)
    }
}

impl<'a> TicoReader<std::io::Cursor<&'a [u8]>> {
    /// Read a container from memory (footer + central directory only).
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self> {
        Self::load(std::io::Cursor::new(bytes))
    }
}

struct RecordHeader {
    name: String,
    method: TicoMethod,
    comp_len: u64,
    raw_len: u64,
}

fn read_record_header<R: Read>(r: &mut R) -> Result<RecordHeader> {
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).map_err(ArchiveError::from)?;
    let name_len = u16::from_le_bytes(len_buf) as usize;
    if name_len == 0 || name_len > 4096 {
        return Err(invalid("bad tico entry name length"));
    }
    let mut name_buf = vec![0u8; name_len];
    r.read_exact(&mut name_buf).map_err(ArchiveError::from)?;
    let name =
        String::from_utf8(name_buf).map_err(|_| invalid("tico entry name is not utf-8"))?;
    let mut fixed = [0u8; 33];
    r.read_exact(&mut fixed).map_err(ArchiveError::from)?;
    let method = TicoMethod::from_code(fixed[0])?;
    let comp_len = u64::from_le_bytes(fixed[17..25].try_into().unwrap());
    let raw_len = u64::from_le_bytes(fixed[25..33].try_into().unwrap());
    Ok(RecordHeader {
        name,
        method,
        comp_len,
        raw_len,
    })
}

fn parse_central(central: &[u8], count: usize) -> Result<Vec<TicoEntryMeta>> {
    if central.len() < 4 {
        return Err(invalid("truncated tico central directory"));
    }
    let stored = u32::from_le_bytes([central[0], central[1], central[2], central[3]]) as usize;
    if stored != count {
        return Err(invalid("tico central count mismatch"));
    }
    let mut entries = Vec::with_capacity(count.min(1_000_000));
    let mut pos = 4;
    for _ in 0..count {
        if pos + 2 > central.len() {
            return Err(invalid("truncated tico central entry"));
        }
        let name_len = u16::from_le_bytes([central[pos], central[pos + 1]]) as usize;
        pos += 2;
        if pos + name_len + 33 > central.len() {
            // 33 fixed bytes after the name, plus 8 offset bytes.
            return Err(invalid("truncated tico central entry"));
        }
        if pos + name_len + 41 > central.len() {
            return Err(invalid("truncated tico central entry"));
        }
        let name = String::from_utf8(central[pos..pos + name_len].to_vec())
            .map_err(|_| invalid("tico entry name is not utf-8"))?;
        pos += name_len;
        let method = TicoMethod::from_code(central[pos])?;
        let mode = u32::from_le_bytes(central[pos + 1..pos + 5].try_into().unwrap());
        let mtime = u64::from_le_bytes(central[pos + 5..pos + 13].try_into().unwrap());
        let crc = u32::from_le_bytes(central[pos + 13..pos + 17].try_into().unwrap());
        let comp_len = u64::from_le_bytes(central[pos + 17..pos + 25].try_into().unwrap());
        let raw_len = u64::from_le_bytes(central[pos + 25..pos + 33].try_into().unwrap());
        let offset = u64::from_le_bytes(central[pos + 33..pos + 41].try_into().unwrap());
        pos += 41;
        entries.push(TicoEntryMeta {
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

/// Pack a manifest plus `(path, bytes)` files into `.tico` bytes.
///
/// Layer files (`*.tlyr`) are validated; every other path must be relative.
/// The manifest is always stored as the first entry (`manifest.fico`).
pub fn tico_pack_bytes(manifest: TicoManifest, files: &[(&str, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut builder = TicoBuilder::new();
    builder.set_manifest(manifest);
    for (path, data) in files {
        if path.ends_with(".tlyr") {
            builder.add_layer(path, data.clone())?;
        } else {
            builder.add_file(path, data.clone())?;
        }
    }
    builder.finish()
}

/// Extract a `.tico` container file into a directory.
pub fn tico_extract_to_file(data_path: &Path, dir: &Path) -> Result<()> {
    TicoReader::open(data_path)?.extract_to(dir)
}

/// Pack an unpacked `.tico` directory (`manifest.fico` + `layer/*.tlyr`)
/// into `.tico` bytes.
pub fn tico_pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let text = fs::read_to_string(dir.join(TICO_MANIFEST_NAME))
        .map_err(|_| invalid("tico dir has no manifest.fico"))?;
    let manifest = TicoManifest::from_fico(&text)?;
    let mut builder = TicoBuilder::new();
    builder.set_manifest(manifest.clone());
    let mut refs: Vec<String> = manifest.layers.iter().map(|l| l.file.clone()).collect();
    if let TicoBackground::Raster { file } = &manifest.background {
        refs.push(file.clone());
    }
    for r in &refs {
        let data = fs::read(dir.join(r))
            .map_err(|_| invalid(format!("tico dir missing '{r}'")))?;
        builder.add_layer(r, data)?;
    }
    // Include the manifest-declared files only; stray files are rejected so
    // unpacked directories round-trip exactly.
    let mut names: Vec<PathBuf> = Vec::new();
    collect_files(dir, dir, &mut names).map_err(ArchiveError::from)?;
    for full in names {
        let rel = full
            .strip_prefix(dir)
            .map_err(|_| invalid("tico path escapes root"))?;
        let arc = rel.to_string_lossy().replace('\\', "/");
        if arc == TICO_MANIFEST_NAME || refs.iter().any(|r| r == &arc) {
            continue;
        }
        return Err(invalid(format!("tico dir has unexpected file '{arc}'")));
    }
    builder.finish()
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let _ = base;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::CompressionLevel;

    fn tlyr(payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"TLYR");
        v.push(1);
        v.push(1); // kind
        v.extend_from_slice(&64u32.to_le_bytes());
        v.extend_from_slice(&64u32.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn demo_manifest() -> TicoManifest {
        TicoManifest {
            name: "demo".to_string(),
            canvas: 1024,
            background: TicoBackground::Color {
                color: "#FFFFFF".to_string(),
            },
            layers: vec![TicoLayerMeta {
                file: "layer/00.tlyr".to_string(),
                opacity: 1.0,
                recolorable: true,
                default_color: "#000000".to_string(),
            }],
        }
    }

    fn demo_bytes() -> Vec<u8> {
        let mut b = TicoBuilder::new();
        b.set_manifest(demo_manifest());
        b.add_layer("layer/00.tlyr", tlyr(&[1, 2, 3, 4])).unwrap();
        b.finish().unwrap()
    }

    #[test]
    fn magic_values() {
        assert_eq!(&TICO_MAGIC.to_le_bytes(), b"TICO");
        assert_eq!(&TICO_FOOTER_MAGIC.to_le_bytes(), b"TICF");
    }

    #[test]
    fn manifest_fico_roundtrip_color() {
        let m = demo_manifest();
        let text = m.to_fico();
        assert!(text.contains("manifest") || text.contains("tico"));
        let back = TicoManifest::from_fico(&text).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn manifest_fico_roundtrip_gradient_and_raster() {
        let mut m = demo_manifest();
        m.background = TicoBackground::Gradient {
            colors: vec!["#FF0000".to_string(), "#0000FF".to_string()],
            positions: vec![0.0, 1.0],
            direction: "TopToBottom".to_string(),
        };
        m.layers.push(TicoLayerMeta {
            file: "layer/01.tlyr".to_string(),
            opacity: 0.5,
            recolorable: false,
            default_color: "#11223344".to_string(),
        });
        let back = TicoManifest::from_fico(&m.to_fico()).unwrap();
        assert_eq!(back, m);

        m.background = TicoBackground::Raster {
            file: "layer/background.tlyr".to_string(),
        };
        let back = TicoManifest::from_fico(&m.to_fico()).unwrap();
        assert_eq!(back, m);
        // Real FishFile parses what we emit.
        let doc = fishfile::FishDocument::parse(&m.to_fico()).unwrap();
        assert_eq!(
            doc.get("tico.background.kind").unwrap().as_str(),
            Some("raster")
        );
    }

    #[test]
    fn manifest_rejects_bad() {
        assert!(TicoManifest::from_fico("tico { version: 1 }").is_err());
        assert!(TicoManifest::from_fico(
            "tico { format: zip version: 1 name: x canvas: 1024 background { kind: color color: #FFFFFF } layer { 0 { file: layer/00.tlyr opacity: 1.0 recolorable: true default_color: #000000 } } }"
        )
        .is_err());
        assert!(TicoManifest::from_fico(
            "tico { format: tico version: 2 name: x canvas: 1024 background { kind: color color: #FFFFFF } layer { 0 { file: layer/00.tlyr opacity: 1.0 recolorable: true default_color: #000000 } } }"
        )
        .is_err());
        // Bad color.
        let mut m = demo_manifest();
        m.background = TicoBackground::Color {
            color: "red".to_string(),
        };
        assert!(TicoManifest::from_fico(&m.to_fico()).is_err());
        // PNG layer path.
        m.background = TicoBackground::Color {
            color: "#FFFFFF".to_string(),
        };
        m.layers[0].file = "layer/00.png".to_string();
        assert!(TicoManifest::from_fico(&m.to_fico()).is_err());
        // Unknown direction.
        m.layers[0].file = "layer/00.tlyr".to_string();
        m.background = TicoBackground::Gradient {
            colors: vec!["#FF0000".to_string(), "#0000FF".to_string()],
            positions: vec![0.0, 1.0],
            direction: "Diagonal".to_string(),
        };
        assert!(TicoManifest::from_fico(&m.to_fico()).is_err());
        // Opacity out of range.
        m.background = TicoBackground::Color {
            color: "#FFFFFF".to_string(),
        };
        m.layers[0].opacity = 2.0;
        assert!(TicoManifest::from_fico(&m.to_fico()).is_err());
    }

    #[test]
    fn builder_reader_roundtrip() {
        let bytes = demo_bytes();
        assert_eq!(&bytes[0..4], b"TICO");
        let mut r = TicoReader::from_bytes(&bytes).unwrap();
        assert_eq!(r.entry_count(), 2); // manifest + 1 layer
        let m = r.read_manifest().unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.canvas, 1024);
        assert_eq!(r.read_file("layer/00.tlyr").unwrap(), tlyr(&[1, 2, 3, 4]));
        assert!(matches!(
            r.read_file("layer/missing.tlyr"),
            Err(ArchiveError::NotFound(_))
        ));
    }

    #[test]
    fn compressed_extra_file_roundtrip() {
        let mut b = TicoBuilder::new();
        b.set_manifest(demo_manifest());
        b.add_layer("layer/00.tlyr", tlyr(&[9; 64])).unwrap();
        b.add_file_compressed("layer/extra.bin", vec![7u8; 1024], CompressionLevel::Balanced)
            .unwrap();
        // Extra file is stored but unreferenced: validation rejects it.
        let bytes = b.finish().unwrap();
        assert!(validate_tico(&bytes).is_err());
        // Direct reads still work.
        let mut r = TicoReader::from_bytes(&bytes).unwrap();
        assert_eq!(r.read_file("layer/extra.bin").unwrap(), vec![7u8; 1024]);
    }

    #[test]
    fn tico_validation() {
        let info = validate_tico(&demo_bytes()).unwrap();
        assert_eq!(info.layers, vec!["layer/00.tlyr".to_string()]);
        assert!(validate_tico(b"not a tico").is_err());
        // ZIP bytes are rejected: only the TICO container is accepted.
        let zip = crate::zip::zip_pack(
            &[("manifest.fico", b"tico { format: tico }" as &[u8])],
            &crate::zip::ZipWriterOptions {
                level: CompressionLevel::None,
                comment: String::new(),
            },
        )
        .unwrap();
        assert!(validate_tico(&zip).is_err());
        // Bad tlyr magic.
        let mut b = TicoBuilder::new();
        b.set_manifest(demo_manifest());
        assert!(b.add_layer("layer/00.tlyr", b"NOPE".to_vec()).is_err());
        // PNG inside.
        let mut b = TicoBuilder::new();
        b.set_manifest(demo_manifest());
        b.add_layer("layer/00.tlyr", tlyr(&[1])).unwrap();
        b.add_file("evil.png", b"png".to_vec()).unwrap();
        assert!(validate_tico(&b.finish().unwrap()).is_err());
    }

    #[test]
    fn raster_background_roundtrip() {
        let mut m = demo_manifest();
        m.background = TicoBackground::Raster {
            file: "layer/background.tlyr".to_string(),
        };
        let mut b = TicoBuilder::new();
        b.set_manifest(m);
        b.add_layer("layer/background.tlyr", tlyr(&[5, 6])).unwrap();
        b.add_layer("layer/00.tlyr", tlyr(&[1])).unwrap();
        let bytes = b.finish().unwrap();
        let info = validate_tico(&bytes).unwrap();
        assert_eq!(info.layers, vec!["layer/00.tlyr".to_string()]);
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut bytes = demo_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // corrupt footer/central
        assert!(TicoReader::from_bytes(&bytes).is_err());
    }

    #[test]
    fn unsafe_paths_rejected() {
        let mut b = TicoBuilder::new();
        assert!(b.add_file("/abs", b"x".to_vec()).is_err());
        assert!(b.add_file("../evil", b"x".to_vec()).is_err());
        assert!(b.add_file("manifest.fico", b"x".to_vec()).is_err());
    }

    #[test]
    fn manifest_required() {
        let mut b = TicoBuilder::new();
        b.add_layer("layer/00.tlyr", tlyr(&[1])).unwrap();
        assert!(b.finish().is_err());
    }

    #[test]
    fn empty_layers_rejected() {
        let mut b = TicoBuilder::new();
        b.set_manifest(TicoManifest::new(
            "empty",
            1024,
            TicoBackground::Color {
                color: "#FFFFFF".to_string(),
            },
        ));
        assert!(b.finish().is_err());
    }

    #[test]
    fn file_backed_open_and_extract() {
        let dir = std::env::temp_dir().join("archivekit_tico_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bytes = demo_bytes();
        fs::write(dir.join("demo.tico"), &bytes).unwrap();

        let mut r = TicoReader::open(&dir.join("demo.tico")).unwrap();
        assert_eq!(r.read_manifest().unwrap().name, "demo");

        r.extract_to(&dir.join("out")).unwrap();
        assert_eq!(
            fs::read(dir.join("out/layer/00.tlyr")).unwrap(),
            tlyr(&[1, 2, 3, 4])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_dir_roundtrip() {
        let dir = std::env::temp_dir().join("archivekit_tico_packdir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("staging/layer")).unwrap();
        fs::write(dir.join("staging/manifest.fico"), demo_manifest().to_fico()).unwrap();
        fs::write(dir.join("staging/layer/00.tlyr"), tlyr(&[1, 2, 3, 4])).unwrap();
        let bytes = tico_pack_dir(&dir.join("staging")).unwrap();
        assert!(validate_tico(&bytes).is_ok());
        // Stray files are rejected.
        fs::write(dir.join("staging/stray.txt"), b"stray").unwrap();
        assert!(tico_pack_dir(&dir.join("staging")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
