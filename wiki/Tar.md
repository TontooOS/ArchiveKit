# Tar

TAR codec (ustar with PAX and GNU extensions). Reads regular files, directories, symlinks and hardlinks; understands `prefix` splitting, GNU `./@LongLink` entries and PAX extended headers. Writes ustar with GNU long-name entries when paths exceed the name/prefix fields.

## Types

### `TarKind`

```rust
pub enum TarKind {
    File,
    Directory,
    Symlink(String),
    Hardlink(String),
}
```

### `TarEntry`

```rust
pub struct TarEntry {
    pub path: String,
    pub kind: TarKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub data: Vec<u8>,
}
```

| Field | Type | Description |
|---|---|---|
| `path` | `String` | Archive path with forward slashes |
| `kind` | `TarKind` | Entry type |
| `mode` | `u32` | Permission bits, e.g. `0o644` |
| `uid` | `u32` | Owner user id |
| `gid` | `u32` | Owner group id |
| `mtime` | `u64` | Unix timestamp |
| `data` | `Vec<u8>` | Payload (empty for non-files) |

Constructors: `TarEntry::file(path, data)`, `TarEntry::dir(path)`. Predicates: `is_file()`, `is_dir()`, `size()`.

### `TarWriteOptions`

```rust
pub struct TarWriteOptions {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
}
```

`TarWriteOptions::dir_default()` uses mode `0o755`.

## Reader

### `TarReader`

```rust
pub struct TarReader<'a> { /* ... */ }
impl<'a> TarReader<'a> {
    pub fn new(data: &'a [u8]) -> Self;
    pub fn next_entry(&mut self) -> Result<Option<TarEntry>>;
    pub fn read_all(&mut self) -> Result<Vec<TarEntry>>;
}
```

Sequential reader. Returns `Ok(None)` at the end marker. Returns `Err` on bad checksums, truncated data, unsupported typeflags with payload, PAX size conflicts, or unsafe paths (`UnsafePath` for absolute or `..`-escaping names).

> **Note:** GNU base-256 numeric fields are accepted on read. Unknown typeflags with zero size are skipped.

## Writer

### `TarWriter`

```rust
pub struct TarWriter { /* ... */ }
impl TarWriter {
    pub fn new() -> Self;
    pub fn append_file(&mut self, path: &str, data: &[u8], options: &TarWriteOptions) -> Result<()>;
    pub fn append_dir(&mut self, path: &str, options: &TarWriteOptions) -> Result<()>;
    pub fn append_symlink(&mut self, path: &str, target: &str, options: &TarWriteOptions) -> Result<()>;
    pub fn finish(self) -> Vec<u8>;
}
```

Appends entries sequentially; `finish` writes the two zero blocks. Returns `Err` for unsafe paths, symlink targets over 100 bytes, or files of 8 GiB and more (PAX writing for huge files is not implemented).

## One-Shot and Filesystem API

### `tar_pack` / `tar_unpack`

```rust
pub fn tar_pack(entries: &[TarEntry]) -> Result<Vec<u8>>
pub fn tar_unpack(data: &[u8]) -> Result<Vec<TarEntry>>
```

### `tar_pack_dir` / `tar_unpack_to_dir`

```rust
pub fn tar_pack_dir(dir: &Path) -> Result<Vec<u8>>
pub fn tar_unpack_to_dir(data: &[u8], dir: &Path) -> Result<()>
```

`tar_pack_dir` walks a directory recursively (sorted, symlinks preserved as link entries); archive paths are relative with forward slashes. `tar_unpack_to_dir` validates all paths before writing anything, creates parent directories, and refuses to recreate links (`Unsupported` for symlink/hardlink entries – inspect them via `tar_unpack` instead).

## Usage / Example

```rust
use archivekit::{TarEntry, TarWriter, tar_unpack, TarWriteOptions};

fn main() -> archivekit::Result<()> {
    let mut w = TarWriter::new();
    w.append_dir("docs/", &TarWriteOptions::dir_default())?;
    w.append_file("docs/hi.txt", b"hi", &TarWriteOptions::default())?;
    let entries = tar_unpack(&w.finish())?;
    assert_eq!(entries[1].data, b"hi");
    Ok(())
}
```

## Cross References

- [Combined.md](Combined.md) – tar.gz pipeline and directory packing for all formats
- [Error.md](Error.md) – `UnsafePath` and `Unsupported` cases
