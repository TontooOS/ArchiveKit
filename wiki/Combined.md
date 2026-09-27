# Combined

Combined formats and high-level helpers: the `.tar.gz` pipeline, magic-based format detection, one-shot bytes APIs, and file/directory APIs shared by apps and the C FFI.

## Format

```rust
pub enum Format {
    Zip,
    Gzip,
    Tar,
    TarGzip,
}
```

`Format::default()` is `Zip`. Helpers: `Format::from_extension(path)` understands `.zip`, `.gz`, `.tar`, `.tar.gz`, `.tgz`; `Format::extension()` returns the canonical extension.

### `detect_format`

```rust
pub fn detect_format(data: &[u8]) -> Option<Format>
```

Detects by magic: GZIP (`1F 8B`), ZIP (`PK` signatures), TAR (checksum-validated ustar block). Returns `None` for unknown input. GZIP-compressed TAR reports as `Gzip`; use the file extension or trial-parse the payload to tell plain GZIP from TAR+GZIP.

```rust
use archivekit::{detect_format, gzip_compress, Format};
use archivekit::deflate::CompressionLevel;

let gz = gzip_compress(b"hi", CompressionLevel::None);
assert_eq!(detect_format(&gz), Some(Format::Gzip));
assert_eq!(detect_format(b"junk"), None);
```

## tar.gz Pipeline

### `tar_gzip_compress` / `tar_gzip_decompress`

```rust
pub fn tar_gzip_compress(entries: &[TarEntry], level: CompressionLevel) -> Vec<u8>
pub fn tar_gzip_decompress(data: &[u8]) -> Result<Vec<TarEntry>>
```

### `tar_gzip_compress_files`

```rust
pub fn tar_gzip_compress_files(files: &[(&str, &[u8])], level: CompressionLevel) -> Result<Vec<u8>>
```

Packs `(name, bytes)` files straight into `.tar.gz` bytes.

```rust
use archivekit::{tar_gzip_compress_files, tar_gzip_decompress};
use archivekit::deflate::CompressionLevel;

let tgz = tar_gzip_compress_files(
    &[("a.txt", b"aaa" as &[u8])],
    CompressionLevel::Balanced,
)?;
assert_eq!(tar_gzip_decompress(&tgz)?[0].data, b"aaa");
```

## Bytes API

### `compress_bytes` / `decompress_bytes`

```rust
pub fn compress_bytes(data: &[u8], format: Format, level: CompressionLevel) -> Result<Vec<u8>>
pub fn decompress_bytes(data: &[u8], format: Format) -> Result<Vec<u8>>
```

`compress_bytes` with `Zip` wraps the input as a single `data.bin` entry; with `Tar`/`TarGzip` it returns `Err` (those need entries – use `tar_pack` / `tar_gzip_compress`). `decompress_bytes` with `Zip` returns the first entry's payload and errors on empty archives; with `Tar` it returns the input unchanged; with `TarGzip` it returns the raw TAR payload (parse with `tar_unpack`).

### `list_names`

```rust
pub fn list_names(data: &[u8]) -> Result<Vec<String>>
```

Lists entry names of any supported archive (auto-detected). GZIP members without a stored name appear as `<gzip member N>`; TAR inside GZIP is detected and listed. Returns `Err` for unknown formats.

## File and Directory API

### `compress_file`

```rust
pub fn compress_file(src: &Path, dst: &Path, format: Option<Format>) -> Result<()>
```

Compresses a file or directory into `dst`. With `format = None` the format comes from the destination extension. GZIP stores the source file name in the header. Parent directories of `dst` are created. Returns `Err` when the format cannot be inferred, or for GZIP-of-directory (a stream cannot hold a tree).

### `pack_dir_to_archive`

```rust
pub fn pack_dir_to_archive(dir: &Path, format: Format, level: CompressionLevel) -> Result<Vec<u8>>
```

Packs a directory recursively into `Zip`, `Tar` or `TarGzip` bytes. ZIP packing skips symlinks (no portable encoding); the TAR pipeline preserves them as link entries.

### `extract_archive` / `extract_bytes`

```rust
pub fn extract_archive(src: &Path, dst_dir: &Path) -> Result<()>
pub fn extract_bytes(data: &[u8], src_hint: Option<&Path>, dst_dir: &Path) -> Result<()>
```

Extracts into a directory (created when missing). `.tar.gz`/`.tgz` extensions take the TAR+GZIP path; otherwise the format is auto-detected, and a GZIP payload that turns out to be TAR is unpacked as TAR. Plain multi-member GZIP yields one file per member. Unsafe ZIP paths are rejected before anything is written. TAR link entries are not recreated (`Unsupported`).

## Usage / Example

```rust
use archivekit::{compress_file, extract_archive, Format};
use std::path::Path;

fn main() -> archivekit::Result<()> {
    compress_file(Path::new("notes"), Path::new("/tmp/notes.tar.gz"), None)?;
    extract_archive(Path::new("/tmp/notes.tar.gz"), Path::new("/tmp/restored"))?;
    assert!(Path::new("/tmp/restored").exists());
    let _ = Format::TarGzip;
    Ok(())
}
```

## Cross References

- [Gzip.md](Gzip.md) – member codec used by the pipeline
- [Tar.md](Tar.md) – TAR side of the pipeline and directory walking
- [Zip.md](Zip.md) – ZIP side of detection and extraction
- [Ffi.md](Ffi.md) – C wrappers around these helpers
