# TontooArchiveKit – Wiki

ZIP, GZIP and TAR compression for TontooOS. 100% hand-written DEFLATE, CRC32, TAR and ZIP codecs with zero dependencies (only `std`).

- Repository: https://github.com/TontooOS/ArchiveKit
- License: TCL v26.1
- Version: 26.1.0

## Feature Index

| Feature | File | Description |
|---|---|---|
| Main index | [MAIN.md](MAIN.md) | This page |
| Rules | [RULE.md](RULE.md) | Wiki design system |
| Deflate | [Deflate.md](Deflate.md) | Raw DEFLATE engine (RFC 1951) and compression levels |
| Gzip | [Gzip.md](Gzip.md) | GZIP members, multi-member streams, streaming API |
| Tar | [Tar.md](Tar.md) | TAR reader/writer, PAX, GNU long names, directory I/O |
| Zip | [Zip.md](Zip.md) | ZIP reader/writer, ZIP64, UTF-8, data descriptors |
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

See [Combined.md](Combined.md), [Zip.md](Zip.md), [Gzip.md](Gzip.md) and
[Tar.md](Tar.md) for details.

## Changelog

- 2026-09-27: Initial wiki for ArchiveKit 26.1.0
