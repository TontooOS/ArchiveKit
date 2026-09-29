# Tico

TontooOS `.tico` icon containers (TICO format). A `.tico` file uses the same indexed single-file engine as `.app` (TAPP) containers – header magic, entry records, a central directory and a CRC-checked footer – with its own `TICO`/`TICF` magic and a FishFile manifest. Listing names or reading one layer only touches the footer plus the central directory (or the single entry), so icons open in milliseconds without scanning the whole file.

## Layout

Entries are stored flat (no top prefix, unlike `.app`):

```text
icon.tico
├── manifest.fico       # icon manifest in Fish Config (.fico) syntax
├── layer/00.tlyr       # custom layer files (never .png)
└── layer/01.tlyr
```

Binary layout: `TICO` magic + version, entry records, central directory, 28-byte footer (`TICF` magic, central offset/length/count/CRC32). Methods are Stored (0) and Deflate (8), with per-entry CRC32 and Unix modes.

A `.tlyr` file is a tiny custom format: magic `TLYR`, version byte `1`, dimensions and PNG-coded RGBA bytes. Decoding and rendering of layers stays in CoreIcon.

## Manifest

### `TicoManifest`

```rust
pub struct TicoManifest {
    pub name: String,
    pub canvas: u32,
    pub background: TicoBackground,
    pub layers: Vec<TicoLayerMeta>,
}
```

```rust
pub enum TicoBackground {
    Color { color: String },
    Gradient { colors: Vec<String>, positions: Vec<f32>, direction: String },
    Raster { file: String },
}
```

```rust
pub struct TicoLayerMeta {
    pub file: String,
    pub opacity: f32,
    pub recolorable: bool,
    pub default_color: String,
}
```

Written and parsed with FishFile (`.fico`). Layers are parallel scalar arrays (`files`, `opacity`, `recolorable`, `default_color`): same length, index-aligned. Only plain `.fico` constructs are used (nested tables with identifier keys, scalar arrays), so any FishFile parser reads what this crate writes:

```text
tico {
  format: tico
  version: 1
  name: demo
  canvas: 1024
  background {
    kind: color
    color: "#FFFFFF"
  }
  files: [layer/00.tlyr]
  opacity: [1.0]
  recolorable: [true]
  default_color: ["#000000"]
}
```

Gradient backgrounds store `direction` (one of `TopToBottom`, `BottomToTop`, `LeftToRight`, `RightToLeft`, `TopLeadingToBottomTrailing`, `TopTrailingToBottomLeading`, `CenterRadial`), `colors` and `positions` arrays inside `background`. Raster backgrounds store `file` pointing at a `layer/*.tlyr` entry.

Constructors and I/O:

```rust
pub fn new(name: ..., canvas: u32, background: TicoBackground) -> Self
pub fn to_fico(&self) -> String              // via FishFile
pub fn from_fico(text: &str) -> Result<Self> // via FishFile
```

`from_fico` returns `Err` on a missing/non-`tico` format, a version other than `1`, an empty name, a canvas outside `16..=4096`, an unknown background kind or direction, non-hex colors, layer paths that are not `*.tlyr`, opacities outside `0.0..=1.0`, or layer arrays with mismatched lengths. An icon with no layers is rejected.

```rust
use archivekit::{TicoBackground, TicoLayerMeta, TicoManifest};

let mut m = TicoManifest::new("demo", 1024, TicoBackground::Color {
    color: "#FFFFFF".to_string(),
});
m.layers.push(TicoLayerMeta {
    file: "layer/00.tlyr".to_string(),
    opacity: 1.0,
    recolorable: true,
    default_color: "#000000".to_string(),
});
assert_eq!(TicoManifest::from_fico(&m.to_fico())?, m);
```

## Icons

### `validate_tico` / `TicoInfo`

```rust
pub fn validate_tico(bytes: &[u8]) -> Result<TicoInfo>
pub struct TicoInfo {
    pub layers: Vec<String>,
}
```

Structural `.tico` validation (no rendering): the file is a TICO container holding `manifest.fico` plus `layer/*.tlyr`; the manifest declares `format: tico` with a supported version; every `.tlyr` has `TLYR` magic, version byte `1` and a fitting payload length; every layer file is referenced by the manifest and every `file` reference exists; no `.png` files and no unexpected files. Returns `Err` otherwise. Also re-exported from `archivekit::app` so existing import paths keep working.

## Builder

### `TicoBuilder`

```rust
pub struct TicoBuilder { /* ... */ }
impl TicoBuilder {
    pub fn new() -> Self;
    pub fn set_manifest(&mut self, manifest: TicoManifest);
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<()>;
    pub fn add_file_compressed(&mut self, path: &str, data: Vec<u8>, level: CompressionLevel) -> Result<()>;
    pub fn add_file_with_mode(&mut self, path: &str, data: Vec<u8>, mode: u32, method: TicoMethod, level: CompressionLevel) -> Result<()>;
    pub fn add_layer(&mut self, path: &str, data: Vec<u8>) -> Result<()>;
    pub fn finish(self) -> Result<Vec<u8>>;
    pub fn write_to_file(self, path: &Path) -> Result<()>;
}
```

Entries are flat container paths (`layer/00.tlyr`); `manifest.fico` is reserved for the manifest and always written first. `add_layer` accepts only `*.tlyr` paths and validates the `TLYR` structure up front. Paths must be relative (absolute and `..`-escaping paths fail with `UnsafePath`). `finish` returns `Err` without a manifest or with no layers.

### `tico_pack_bytes` / `tico_pack_dir`

```rust
pub fn tico_pack_bytes(manifest: TicoManifest, files: &[(&str, Vec<u8>)]) -> Result<Vec<u8>>
pub fn tico_pack_dir(dir: &Path) -> Result<Vec<u8>>
pub fn tico_extract_to_file(data_path: &Path, dir: &Path) -> Result<()>
```

`tico_pack_bytes` routes `*.tlyr` files through layer validation. `tico_pack_dir` packs an unpacked directory (`manifest.fico` + `layer/*.tlyr`) and rejects stray files so directories round-trip exactly.

## Reader

### `TicoReader`

```rust
pub struct TicoReader<R> { /* ... */ }
impl<R: Read + Seek> TicoReader<R> {
    pub fn load(inner: R) -> Result<Self>;
    pub fn entry_count(&self) -> usize;
    pub fn entry(&self, index: usize) -> Option<&TicoEntryMeta>;
    pub fn find(&self, name: &str) -> Option<&TicoEntryMeta>;
    pub fn list_names(&self) -> Vec<String>;
    pub fn read_manifest(&mut self) -> Result<TicoManifest>;
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>>;
    pub fn extract_to(&mut self, dir: &Path) -> Result<()>;
}
impl TicoReader<std::fs::File> {
    pub fn open(path: &Path) -> Result<Self>;
}
impl<'a> TicoReader<std::io::Cursor<&'a [u8]>> {
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self>;
}
```

`load`/`open`/`from_bytes` only read the 8-byte header, the 28-byte footer and the central directory (CRC-checked). `read_file` seeks to one entry, verifies it against the central directory, decodes only its bytes and checks CRC32 (`ChecksumMismatch` on corruption, `NotFound` for missing names). `extract_to` validates every path before writing.

### `TicoEntryMeta` / `TicoMethod`

```rust
pub struct TicoEntryMeta {
    pub name: String,
    pub method: TicoMethod,
    pub mode: u32,
    pub mtime: u64,
    pub crc: u32,
    pub comp_len: u64,
    pub raw_len: u64,
    pub offset: u64,
}
pub enum TicoMethod {
    Stored,
    Deflate,
}
```

`TicoEntryMeta::is_dir()` is true for trailing-`/` names.

## Usage / Example

```rust
use archivekit::{TicoBackground, TicoBuilder, TicoLayerMeta, TicoManifest, TicoReader};

fn main() -> archivekit::Result<()> {
    let mut manifest = TicoManifest::new("demo", 1024, TicoBackground::Color {
        color: "#FFFFFF".to_string(),
    });
    manifest.layers.push(TicoLayerMeta {
        file: "layer/00.tlyr".to_string(),
        opacity: 1.0,
        recolorable: true,
        default_color: "#000000".to_string(),
    });

    let mut b = TicoBuilder::new();
    b.set_manifest(manifest);
    b.add_layer("layer/00.tlyr", std::fs::read("layer-00.tlyr")?)?;
    let bytes = b.finish()?;

    // Fast path: only footer + central directory are read here.
    let mut r = TicoReader::from_bytes(&bytes)?;
    assert_eq!(r.read_manifest()?.name, "demo");
    Ok(())
}
```

## Cross References

- [App.md](App.md) – sibling TAPP containers sharing the indexed engine
- [Combined.md](Combined.md) – `Format::Tico`, detection, file and dir APIs
- [Deflate.md](Deflate.md) – engine behind Deflate entries
- [Error.md](Error.md) – `NotFound`, `UnsafePath`, `ChecksumMismatch`
- [Ffi.md](Ffi.md) – C API format code for tico detection
