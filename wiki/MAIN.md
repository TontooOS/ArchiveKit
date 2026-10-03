# TontooArchiveKit – Wiki

ZIP, GZIP, TAR and `.app` single-file containers for TontooOS. Hand-written DEFLATE, CRC32, TAR and ZIP codecs (dependency-free) plus indexed TAPP app containers with FishFile manifests and `.tico` icons.

- Repository: https://github.com/TontooOS/ArchiveKit
- License: TCL v27.0
- Version: 27.0.0

## Feature Index

| Feature | File | Description |
|---|---|---|
| Main index | [MAIN.md](MAIN.md) | This page |
| Rules | [RULE.md](RULE.md) | Wiki design system |
| Deflate | [Deflate.md](Deflate.md) | Raw DEFLATE engine (RFC 1951) and compression levels |
| Zlib | [Zlib.md](Zlib.md) | ZLIB streams (RFC 1950), Adler-32, checksum-verified and lenient |
| Gzip | [Gzip.md](Gzip.md) | GZIP members, multi-member streams, streaming API |
| Tar | [Tar.md](Tar.md) | TAR reader/writer, PAX, GNU long names, directory I/O |
| Zip | [Zip.md](Zip.md) | ZIP reader/writer, ZIP64, UTF-8, data descriptors |
| App | [App.md](App.md) | `.app` containers, manifest, tico icons, random access |
| Tico | [Tico.md](Tico.md) | `.tico` icon containers, fico manifest, layers, random access |
| Combined | [Combined.md](Combined.md) | tar.gz pipeline, format detection, file and dir APIs |
| Error | [Error.md](Error.md) | Error types and handling |
| FFI | [Ffi.md](Ffi.md) | C header and interop |

## Quick Start

```rust
use archivekit::{tar_gzip_compress_files, tar_gzip_decompress};
use archivekit::deflate::CompressionLevel;

fn main() -> archivekit::Result<()> {
    let tgz = tar_gzip_compress_files(
        &[("hello.txt", b"hello tontoo" as &[u8])],
        CompressionLevel::Balanced,
    )?;
    let entries = tar_gzip_decompress(&tgz)?;
    assert_eq!(entries[0].data, b"hello tontoo");
    Ok(())
}
```

```c
#include "archivekit.h"

int main(void) {
    size_t out_len = 0;
    uint8_t *gz = archivekit_gzip_compress(
        (const uint8_t *)"hello", 5, 2, &out_len);
    archivekit_free_buffer(gz, out_len);
    return 0;
}
```

See [Combined.md](Combined.md), [App.md](App.md), [Tico.md](Tico.md), [Zip.md](Zip.md),
[Gzip.md](Gzip.md) and [Tar.md](Tar.md) for details.

## Changelog

- 2026-10-02: New `zlib` module (RFC 1950): `zlib_compress` / `zlib_decompress` plus `zlib_stream` for the header fields, `*_limited` variants for decompression-bomb protection, and `*_unverified` variants that skip the Adler-32 check for sloppy producers such as PDF. `crc::Adler32` and `crc::adler32` added next to CRC32. See [Zlib.md](Zlib.md).
- 2026-09-29: `.tico` icons moved from ZIP to the indexed TICO container (own `TICO`/`TICF` magic, `manifest.fico` FishFile manifest, `layer/*.tlyr` entries, `TicoBuilder`/`TicoReader`); `validate_tico` enforces the new format; `Format::Tico` added to detection, dir packing, extraction and the C FFI (code 6)
- 2026-09-27: GZIP single-pass decode, ZIP capacity hints, fuzz harness (120k cases clean) + overflow hardening
- 2026-09-27: Added `.app` containers (TAPP) with FishFile manifest, tico icons and random access
- 2026-09-27: Initial wiki for ArchiveKit 27.0.0
