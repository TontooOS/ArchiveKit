# Gzip

GZIP codec (RFC 1952). Single- and multi-member streams with header parsing (`FEXTRA`, `FNAME`, `FCOMMENT`, `FHCRC`), CRC32 + ISIZE trailer verification per member, and exact member-boundary scanning via consumed-byte tracking in inflate.

## Types

### `GzipMember`

```rust
pub struct GzipMember {
    pub mtime: u32,
    pub name: Option<String>,
    pub os: u8,
    pub data: Vec<u8>,
}
```

| Field | Type | Description |
|---|---|---|
| `mtime` | `u32` | Modification time, 0 when unknown |
| `name` | `Option<String>` | Original file name from `FNAME`, if present |
| `os` | `u8` | OS byte from the header (`3` when written by ArchiveKit) |
| `data` | `Vec<u8>` | Decompressed payload |

### `GzipOptions`

```rust
pub struct GzipOptions {
    pub level: CompressionLevel,
    pub mtime: u32,
    pub name: Option<String>,
}
```

## Functions

### `gzip_compress`

```rust
pub fn gzip_compress(data: &[u8], level: CompressionLevel) -> Vec<u8>
```

Compresses into a single-member stream with `mtime = 0` and no file name.

### `gzip_compress_with_options`

```rust
pub fn gzip_compress_with_options(data: &[u8], options: &GzipOptions) -> Vec<u8>
```

Full header control (`XFL` is derived from the level, OS is always Unix).

```rust
use archivekit::{gzip_compress_with_options, GzipOptions};
use archivekit::deflate::CompressionLevel;

let enc = gzip_compress_with_options(b"hi", &GzipOptions {
    level: CompressionLevel::Balanced,
    mtime: 1_700_000_000,
    name: Some("hello.txt".to_string()),
});
assert_eq!(&enc[0..2], &[0x1F, 0x8B]);
```

### `gzip_decompress`

```rust
pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>>
```

Decompresses all members and returns the concatenated payloads. Returns `Err` on bad magic, unsupported method, reserved flags, truncated input, CRC mismatch (`ChecksumMismatch`) or size mismatch. Empty input is an error.

### `gzip_decompress_limited`

```rust
pub fn gzip_decompress_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>>
```

Same as `gzip_decompress` with an explicit total output cap. Single-pass: members inflate straight into the output buffer while the CRC is folded in (no per-member buffers, no concat copy).

### `gzip_members` / `gzip_members_limited`

```rust
pub fn gzip_members(data: &[u8]) -> Result<Vec<GzipMember>>
pub fn gzip_members_limited(data: &[u8], max_output: usize) -> Result<Vec<GzipMember>>
```

Decodes every member with metadata instead of concatenating.

## Streaming Types

### `GzipEncoder`

```rust
pub struct GzipEncoder { /* buffered */ }
impl GzipEncoder {
    pub fn new(options: GzipOptions) -> Self;
    pub fn write_bytes(&mut self, bytes: &[u8]);
    pub fn finish(self) -> Vec<u8>;
}
```

Buffered streaming encoder: collects input via repeated `write_bytes` calls and emits one complete member on `finish`.

### `GzipDecoder`

```rust
pub struct GzipDecoder { /* decoded members */ }
impl GzipDecoder {
    pub fn new(data: &[u8]) -> Result<Self>;
    pub fn next_member(&mut self) -> Option<&GzipMember>;
    pub fn decode_all(data: &[u8]) -> Result<Vec<u8>>;
}
```

Iterates pre-decoded members; `decode_all` equals `gzip_decompress`.

## Usage / Example

```rust
use archivekit::{gzip_compress, gzip_decompress, gzip_members};
use archivekit::deflate::CompressionLevel;

fn main() -> archivekit::Result<()> {
    let mut enc = gzip_compress(b"first", CompressionLevel::Balanced);
    enc.extend_from_slice(&gzip_compress(b"second", CompressionLevel::Balanced));
    assert_eq!(gzip_decompress(&enc)?, b"firstsecond");
    assert_eq!(gzip_members(&enc)?.len(), 2);
    Ok(())
}
```

## Cross References

- [Deflate.md](Deflate.md) – the compression engine underneath
- [Combined.md](Combined.md) – tar.gz pipeline built on this module
- [Error.md](Error.md) – `ChecksumMismatch` and `InvalidData` cases
