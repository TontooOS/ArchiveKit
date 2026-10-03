# Zlib

ZLIB (RFC 1950) for TontooOS: a two-byte header, a raw DEFLATE payload and a
four-byte Adler-32 trailer. The DEFLATE engine comes from
[Deflate.md](Deflate.md) and the checksum from `crc.rs`, so the module has no
dependencies of its own.

PNG `IDAT` chunks, PDF `/FlateDecode` streams and zlib-wrapped `.app`
bundles all use this framing.

- Repository: https://github.com/TontooOS/ArchiveKit
- License: TCL v27.0
- Version: 27.0.0

## API

```rust
pub fn zlib_compress(data: &[u8], level: CompressionLevel) -> Vec<u8>
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>>
pub fn zlib_decompress_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>>
pub fn zlib_decompress_unverified(data: &[u8]) -> Result<Vec<u8>>
pub fn zlib_decompress_unverified_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>>
pub fn zlib_stream(data: &[u8]) -> Result<ZlibStream>
pub fn zlib_stream_limited(data: &[u8], max_output: usize) -> Result<ZlibStream>
```

| Function | Verifies Adler-32 | Output limit |
|---|---|---|
| `zlib_decompress` | yes | `DEFAULT_MAX_OUTPUT` |
| `zlib_decompress_limited` | yes | caller supplied |
| `zlib_decompress_unverified` | no | `DEFAULT_MAX_OUTPUT` |
| `zlib_decompress_unverified_limited` | no | caller supplied |
| `zlib_stream` / `zlib_stream_limited` | yes | `DEFAULT_MAX_OUTPUT` / caller supplied |

## Stream Format

| Bytes | Content |
|---|---|
| 0 | `CMF`: `CM = 8` (DEFLATE) in the low nibble, `CINFO = 7` (32 KiB window) in the high nibble, so `0x78` |
| 1 | `FLG`: `FLEVEL` from the compression level, plus `FCHECK` so the pair read as a big-endian `u16` is divisible by 31 |
| 2..len-4 | Raw DEFLATE payload |
| len-4..len | Adler-32 of the uncompressed data, big-endian |

`zlib_compress` writes the common `0x78` header byte; the `FLG` byte varies
with the level (`0x9C` for `Balanced`, `0xDA` for `Best`, `0x01` for
`Fastest`).

```rust
use archivekit::{zlib_compress, zlib_decompress, CompressionLevel};

let packed = zlib_compress(b"payload", CompressionLevel::Balanced);
assert_eq!(packed[0], 0x78);
assert_eq!(zlib_decompress(&packed).unwrap(), b"payload");
```

## Header Validation

Every decompress function rejects:

- input shorter than 2 bytes, or shorter than 6 bytes overall,
- a `CM` other than 8,
- a `CINFO` above 7,
- a header pair that is not divisible by 31,
- `FDICT` set (preset dictionaries return
  `Err(ArchiveError::Unsupported)`),
- output larger than the limit, which returns
  `Err(ArchiveError::InvalidData)`.

## Checksum Policy

`zlib_decompress` verifies the Adler-32 trailer and returns
`Err(ArchiveError::ChecksumMismatch { expected, actual, entry })` on a
mismatch.

Real-world producers are often sloppy: PDF writers leave the trailer zeroed
or stale. Formats that must keep reading those files use the `unverified`
variants, which inflate the payload exactly the same way and only skip the
integrity check. PDFKit's `/FlateDecode` path uses
`zlib_decompress_unverified_limited`.

## `ZlibStream`

```rust
pub struct ZlibStream {
    pub data: Vec<u8>,
    pub method: u8,
    pub preset_dictionary: bool,
    pub window_bits: u8,
}
```

`zlib_stream` reports the header fields alongside the payload.
`is_default_header` is true for the `0x78 0x9C` pair this crate writes.
`preset_dictionary` is always `false` because a set `FDICT` is rejected
before inflation.

## Adler-32

`crc::Adler32` is a streaming hasher (`new`, `update`, `finalize`,
`checksum`) plus the `crc::adler32` one-shot. Chunks are reduced every 5552
bytes, the largest bound that cannot overflow the 32-bit accumulators.
The RFC 1950 check value for `123456789` is `0x091E01DE`.

## Cross References

- [Deflate.md](Deflate.md) – the raw DEFLATE engine used for the payload
- [Gzip.md](Gzip.md) – the sibling container format, which uses CRC32
- [Error.md](Error.md) – `InvalidData`, `Unsupported`, `ChecksumMismatch`
- [MAIN.md](MAIN.md) – feature index