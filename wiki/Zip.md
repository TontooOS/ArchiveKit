# Zip

ZIP codec (APPNOTE-compatible subset). Stored and Deflate methods on read and write, multi-file archives, directory entries, UTF-8 names, data descriptors (bit 3) and ZIP64 on read and write. Encrypted entries and exotic methods are rejected.

## Types

### `ZipMethod`

```rust
pub enum ZipMethod {
    Stored,
    Deflate,
}
```

`ZipMethod::default()` is `Deflate`.

### `ZipEntry`

```rust
pub struct ZipEntry {
    pub name: String,
    pub method: ZipMethod,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub crc32: u32,
    pub unix_mode: Option<u32>,
    pub data: Vec<u8>,
}
```
| Field | Type | Description |
|---|---|---|
| `name` | `String` | Forward-slash path inside the archive |
| `method` | `ZipMethod` | Compression method |
| `uncompressed_size` | `u64` | Payload size |
| `compressed_size` | `u64` | Stored size |
| `crc32` | `u32` | Integrity checksum |
| `unix_mode` | `Option<u32>` | Permission bits when made by Unix, else `None` |
| `data` | `Vec<u8>` | Decompressed payload (empty for directories) |

Predicates: `is_dir()` (trailing `/`), `is_symlink()` (Unix mode check).

### `ZipIndexEntry`

```rust
pub struct ZipIndexEntry {
    pub name: String,
    pub method: ZipMethod,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub crc32: u32,
    pub unix_mode: Option<u32>,
    pub local_offset: u64,
    pub flags: u16,
}
```

Central-directory metadata without payload, returned by `read_index` and consumed by `read_one`. Predicate: `is_dir()`.

### `ZipWriterOptions`

```rust
pub struct ZipWriterOptions {
    pub level: CompressionLevel,
    pub comment: String,
}
```

## Writer

### `ZipWriter`

```rust
pub struct ZipWriter { /* ... */ }
impl ZipWriter {
    pub fn new() -> Self;
    pub fn with_options(options: ZipWriterOptions) -> Self;
    pub fn append_file(&mut self, name: &str, data: &[u8]) -> Result<()>;
    pub fn append_file_with_method(
        &mut self,
        name: &str,
        data: &[u8],
        method: ZipMethod,
        unix_mode: u32,
    ) -> Result<()>;
    pub fn append_dir(&mut self, name: &str) -> Result<()>;
    pub fn finish(self) -> Vec<u8>;
}
```

`append_file` picks Stored for empty input (or when the level is `None`) and Deflate otherwise. Names are always written with the UTF-8 flag. Entries larger than 4 GiB, offsets beyond 4 GiB, or more than 65535 entries switch the archive to ZIP64 automatically (extra field `0x0001`, EOCD64 + locator). Returns `Err` for empty or absolute names and for names over 64 KiB.

## Reader

### `ZipReader`

```rust
pub struct ZipReader<'a> { /* ... */ }
impl<'a> ZipReader<'a> {
    pub fn new(data: &'a [u8]) -> Self;
    pub fn with_max_output(self, max_output: usize) -> Self;
    pub fn read_all(&self) -> Result<Vec<ZipEntry>>;
    pub fn read_index(&self) -> Result<Vec<ZipIndexEntry>>;
    pub fn find_in_index(index: &[ZipIndexEntry], name: &str) -> Option<ZipIndexEntry>;
    pub fn read_one(&self, index: &ZipIndexEntry) -> Result<ZipEntry>;
}
```

The reader locates the end-of-central-directory (scanning the last 64 KiB + 22 bytes), follows the ZIP64 locator when present, then resolves each entry through its local header. `read_index` parses only the central directory (no payload touched); `read_one` decodes a single entry; `read_all` is index plus per-entry decode. CRC32 is verified per file entry. Returns `Err` on missing EOCD, out-of-bounds directories, encrypted entries (`Unsupported`), unknown methods (`Unsupported`), size mismatches, checksum failures (`ChecksumMismatch`), or output over the limit (default 256 MiB).

> **Note:** Data-descriptor entries (bit 3, local sizes zeroed) are resolved via the central directory. Non-UTF8 names decode lossily and round-trip ASCII exactly.

## One-Shot API

### `zip_pack` / `zip_unpack`

```rust
pub fn zip_pack(files: &[(&str, &[u8])], options: &ZipWriterOptions) -> Result<Vec<u8>>
pub fn zip_unpack(data: &[u8]) -> Result<Vec<ZipEntry>>
```

Names ending in `/` become directory entries.

## Usage / Example

```rust
use archivekit::{zip_pack, zip_unpack, ZipWriterOptions};
use archivekit::deflate::CompressionLevel;

fn main() -> archivekit::Result<()> {
    let opts = ZipWriterOptions {
        level: CompressionLevel::Balanced,
        comment: String::new(),
    };
    let raw = zip_pack(&[("a.txt", b"aaa" as &[u8]), ("d/", b"")], &opts)?;
    let entries = zip_unpack(&raw)?;
    assert_eq!(entries[0].data, b"aaa");
    assert!(entries[1].is_dir());
    Ok(())
}
```

## File APIs (constant memory)

`ZipFileReader` opens an archive from disk reading only the tail plus the central directory; entries stream straight to disk with bounded RAM (100 GB archives welcome). `ZipFileWriter` streams local data to disk and buffers only the central directory.

```rust
use archivekit::{ZipFileReader, ZipFileWriter, ZipMethod};
use std::path::Path;

fn main() -> archivekit::Result<()> {
    let mut w = ZipFileWriter::create(Path::new("/tmp/big.zip"))?;
    w.append_file_from_disk("big.bin", Path::new("/data/big.bin"), ZipMethod::Stored)?;
    w.finish()?;

    let mut r = ZipFileReader::open(Path::new("/tmp/big.zip"))?;
    assert_eq!(r.index().len(), 1);
    r.extract_all_to(Path::new("/tmp/out"))?;
    Ok(())
}
```

### `ZipFileReader`

```rust
impl ZipFileReader {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn len(&self) -> u64;
    pub fn is_empty(&self) -> bool;
    pub fn index(&self) -> &[ZipIndexEntry];
    pub fn extract_entry_to_writer(&mut self, name: &str, out: &mut impl Write) -> Result<u64>;
    pub fn extract_entry_to_path(&mut self, name: &str, dest: &Path) -> Result<u64>;
    pub fn extract_all_to(&mut self, dir: &Path) -> Result<()>;
}
```

`extract_entry_to_writer` returns payload bytes after CRC verification (`ChecksumMismatch` on corruption, `NotFound` for missing names, `Ok(0)` for directories). `extract_all_to` validates every path before writing, recreates symlinks on Unix (`Unsupported` elsewhere) and restores Unix modes on Unix.

### `ZipFileWriter`

```rust
impl ZipFileWriter {
    pub fn create(path: &Path) -> Result<Self>;
    pub fn create_with_options(path: &Path, options: ZipWriterOptions) -> Result<Self>;
    pub fn append_dir(&mut self, name: &str) -> Result<()>;
    pub fn append_file(&mut self, name: &str, data: &[u8]) -> Result<()>;
    pub fn append_file_with_method(&mut self, name: &str, data: &[u8], method: ZipMethod, unix_mode: u32) -> Result<()>;
    pub fn append_file_from_disk(&mut self, name: &str, src: &Path, method: ZipMethod) -> Result<()>;
    pub fn finish(self) -> Result<()>;
}
```

`append_file_from_disk` streams Stored entries in 1 MiB chunks (CRC pre-pass plus copy); Deflate entries are read fully first (documented limit).

## Cross References

- [Deflate.md](Deflate.md) – engine behind Deflate entries
- [Combined.md](Combined.md) – auto-detection, listing and extraction
- [Error.md](Error.md) – `ChecksumMismatch`, `Unsupported`, `InvalidData`
