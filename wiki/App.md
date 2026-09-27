# App

TontooOS `.app` single-file containers (TAPP format). Unlike macOS folder bundles – and unlike the ZIP-based `.app` files TBuild produces today – a TAPP container carries a central directory in its footer, so listing names or reading one file only touches the footer plus the central directory (or that single entry). Hundred-megabyte apps open in milliseconds instead of being scanned end to end.

## Layout

The tree inside mirrors what TBuild stages on disk:

```text
Foo.app/
  App/
    foo              main binary (0o755)
    icon.tico        app icon, Tontoo tico format (no PNG)
  Resources/
    ...              project resources
    icon.tico
  Info.tontoo        app manifest in Fish Config (.fico) syntax
```

Binary layout: `TAPP` magic + version, entry records, central directory, 28-byte footer (`TAPF` magic, central offset/length/count/CRC32). Methods are Stored (0) and Deflate (8), with per-entry CRC32 and Unix modes.

## Manifest

### `AppManifest`

```rust
pub struct AppManifest {
    pub bundle_id: String,
    pub version: String,
    pub executable: String,
    pub icon: Option<String>,
    pub names: Vec<(String, String)>,
}
```

Written and parsed with FishFile (`.fico`):

```text
app {
  bundle_id: com.tontoo.foo
  version: 26.1.0
  executable: App/foo
  icon: App/icon.tico
  name {
    en_us: Foo
    de_de: Foo
  }
}
```

Constructors and I/O:

```rust
pub fn new(bundle_id: ..., version: ..., executable: ...) -> Self
pub fn name(&self, locale: &str) -> Option<&str>
pub fn display_name(&self) -> Option<&str>   // en_us, else first
pub fn to_fico(&self) -> String              // via FishFile
pub fn from_fico(text: &str) -> Result<Self> // via FishFile
```

`from_fico` returns `Err` on missing/empty required fields (`app.bundle_id`, `app.version`, `app.executable`), on absolute or `..`-escaping executable paths, and on icons that are missing, empty or not `*.tico`.

```rust
use archivekit::AppManifest;

let m = AppManifest {
    bundle_id: "com.tontoo.demo".to_string(),
    version: "1.0.0".to_string(),
    executable: "App/demo".to_string(),
    icon: Some("App/icon.tico".to_string()),
    names: vec![("en_us".to_string(), "Demo".to_string())],
};
let text = m.to_fico();
assert_eq!(AppManifest::from_fico(&text)?.bundle_id, "com.tontoo.demo");
```

## Icons

### `validate_tico` / `TicoInfo`

```rust
pub fn validate_tico(bytes: &[u8]) -> Result<TicoInfo>
pub struct TicoInfo {
    pub layers: Vec<String>,
}
```

Structural `.tico` validation with ArchiveKit's own ZIP reader, mirroring the rules in `CoreIcon/src/tico.rs`: the file is a ZIP holding `manifest.json` plus `layer/*.tlyr`; the manifest declares `"format": "tico"`; every `.tlyr` has `TLYR` magic, version byte `1` and a fitting payload length; every layer is referenced by the manifest and every `"file"` reference exists; no `.png` files. Returns `Err` otherwise. Decoding and rendering stay in CoreIcon.

## Builder

### `AppBuilder`

```rust
pub struct AppBuilder { /* ... */ }
impl AppBuilder {
    pub fn new(app_name: &str) -> Result<Self>;
    pub fn top(&self) -> &str;
    pub fn set_manifest(&mut self, manifest: AppManifest);
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<()>;
    pub fn add_file_compressed(&mut self, path: &str, data: Vec<u8>, level: CompressionLevel) -> Result<()>;
    pub fn add_file_with_mode(&mut self, path: &str, data: Vec<u8>, mode: u32, method: AppMethod, level: CompressionLevel) -> Result<()>;
    pub fn add_dir(&mut self, path: &str) -> Result<()>;
    pub fn add_executable(&mut self, path: &str, data: Vec<u8>) -> Result<()>;
    pub fn add_icon_tico(&mut self, path: &str, data: Vec<u8>) -> Result<TicoInfo>;
    pub fn pack_tree(&mut self, staging: &Path) -> Result<()>;
    pub fn pack_tree_compressed(&mut self, staging: &Path, level: CompressionLevel) -> Result<()>;
    pub fn pack_tree_adopt_manifest(&mut self, staging: &Path) -> Result<AppManifest>;
    pub fn finish(self) -> Result<Vec<u8>>;
    pub fn write_to_file(self, path: &Path) -> Result<()>;
}
```

`new("Foo")` scopes every entry under `Foo.app/`. Paths must be relative (absolute and `..`-escaping paths fail with `UnsafePath`); an already-prefixed top is stripped, not doubled. The manifest entry (`<top>Info.tontoo`) is always written first. `finish` returns `Err` without a manifest. `pack_tree` skips a staging-root `Info.tontoo` (reserved for the manifest); `pack_tree_compressed` deflates everything except `*.tico`. Symlinks are rejected as `Unsupported`.

### `app_pack_dir`

```rust
pub fn app_pack_dir(staging: &Path, app_name: &str, level: Option<CompressionLevel>) -> Result<Vec<u8>>
```

Packs a staging tree (`App/`, `Resources/`, `Info.tontoo` in fico syntax) with manifest adoption. `None` stores everything; `Some(level)` deflates except `*.tico`.

## Reader

### `AppReader`

```rust
pub struct AppReader<R> { /* ... */ }
impl<R: Read + Seek> AppReader<R> {
    pub fn load(inner: R) -> Result<Self>;
    pub fn entry_count(&self) -> usize;
    pub fn entry(&self, index: usize) -> Option<&AppEntryMeta>;
    pub fn find(&self, name: &str) -> Option<&AppEntryMeta>;
    pub fn list_names(&self) -> Vec<String>;
    pub fn manifest_name(&self) -> Option<&str>;
    pub fn read_manifest(&mut self) -> Result<AppManifest>;
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>>;
    pub fn extract_to(&mut self, dir: &Path) -> Result<()>;
}
impl AppReader<std::fs::File> {
    pub fn open(path: &Path) -> Result<Self>;
}
impl<'a> AppReader<std::io::Cursor<&'a [u8]>> {
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self>;
}
```

`load`/`open`/`from_bytes` only read the 8-byte header, the 28-byte footer and the central directory (CRC-checked). `read_file` seeks to one entry, verifies it against the central directory, decodes only its bytes and checks CRC32 (`ChecksumMismatch` on corruption, `NotFound` for missing names, empty bytes for directories). `extract_to` validates every path before writing and restores Unix modes on Unix.

### `AppEntryMeta` / `AppMethod`

```rust
pub struct AppEntryMeta {
    pub name: String,
    pub method: AppMethod,
    pub mode: u32,
    pub mtime: u64,
    pub crc: u32,
    pub comp_len: u64,
    pub raw_len: u64,
    pub offset: u64,
}
pub enum AppMethod {
    Stored,
    Deflate,
}
```

`AppEntryMeta::is_dir()` is true for trailing-`/` names.

## Usage / Example

```rust
use archivekit::{AppBuilder, AppManifest, AppReader};
use std::path::Path;

fn main() -> archivekit::Result<()> {
    let mut manifest = AppManifest::new("com.tontoo.demo", "1.0.0", "App/demo");
    manifest.names.push(("en_us".to_string(), "Demo".to_string()));

    let mut b = AppBuilder::new("Demo")?;
    b.set_manifest(manifest);
    b.add_dir("App")?;
    b.add_executable("App/demo", b"binary".to_vec())?;
    b.write_to_file(Path::new("/tmp/Demo.app"))?;

    // Fast path: only footer + central directory are read here.
    let mut r = AppReader::open(Path::new("/tmp/Demo.app"))?;
    assert_eq!(r.read_file("Demo.app/App/demo")?, b"binary");
    assert_eq!(r.read_manifest()?.bundle_id, "com.tontoo.demo");
    Ok(())
}
```

## Cross References

- [Combined.md](Combined.md) – `Format::App`, detection, file and dir APIs
- [Zip.md](Zip.md) – engine used for `.tico` validation
- [Deflate.md](Deflate.md) – engine behind Deflate entries
- [Error.md](Error.md) – `NotFound`, `UnsafePath`, `ChecksumMismatch`
- [Ffi.md](Ffi.md) – C API for open/list/read/manifest/extract/pack
