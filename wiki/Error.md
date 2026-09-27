# Error

`ArchiveError` is the single error enum for the crate. `Result<T>` is an alias for `Result<T, ArchiveError>`. The type is dependency-free (`std` only) and implements `Display` and `std::error::Error`.

## Enum

```rust
pub enum ArchiveError {
    InvalidData(String),
    Unsupported(String),
    ChecksumMismatch { expected: u32, actual: u32, entry: String },
    Io(String),
    NotFound(String),
    UnsafePath(String),
}
pub type Result<T> = std::result::Result<T, ArchiveError>;
```

### `InvalidData`

```rust
ArchiveError::InvalidData(String)
```

Corrupt or truncated input: bad magic, broken headers, invalid Huffman codes, size disagreements, output over the limit.

```rust
match archivekit::gzip_decompress(b"junk") {
    Err(archivekit::ArchiveError::InvalidData(m)) => eprintln!("bad input: {}", m),
    _ => {}
}
```

Display: `invalid data: ...`.

### `Unsupported`

```rust
ArchiveError::Unsupported(String)
```

Valid input using features ArchiveKit does not implement: encrypted ZIP entries, exotic ZIP methods (BZip2, LZMA), unknown TAR typeflags with payload, TAR files of 8 GiB and more, link recreation on extract.

Display: `unsupported: ...`.

### `ChecksumMismatch`

```rust
ArchiveError::ChecksumMismatch { expected: u32, actual: u32, entry: String }
```

CRC32 failure naming the entry. Thrown by GZIP member trailers and ZIP file entries.

Display: `checksum mismatch in '<entry>': expected <hex>, got <hex>`.

### `Io`

```rust
ArchiveError::Io(String)
```

Filesystem failures. Auto-converted via `From<std::io::Error>`, keeping the message as a string so the crate stays dependency-free.

Display: `i/o error: ...`.

### `NotFound`

```rust
ArchiveError::NotFound(String)
```

Reserved for entry lookups that miss. Currently informational; index-based APIs return `Option` instead.

Display: `not found: ...`.

### `UnsafePath`

```rust
ArchiveError::UnsafePath(String)
```

Absolute paths or `..` escapes inside TAR/ZIP names, rejected before any file is written.

Display: `unsafe path: ...`.

## Limits

Decompression caps output at 256 MiB by default (`DEFAULT_MAX_OUTPUT`, `ZipReader::with_max_output`, `gzip_decompress_limited`, `decompress_raw_limited`); over-limit input fails with `InvalidData`, not with an allocation failure.

## Cross References

- [Deflate.md](Deflate.md) – corrupt-stream cases
- [Gzip.md](Gzip.md) – `ChecksumMismatch` from trailers
- [Zip.md](Zip.md) – `Unsupported` and `ChecksumMismatch` cases
- [Tar.md](Tar.md) – `UnsafePath` cases
