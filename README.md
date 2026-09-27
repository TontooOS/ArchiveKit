# TontooArchiveKit

ZIP, GZIP and TAR compression for TontooOS. 100% hand-written codecs with zero dependencies (only `std`): DEFLATE (RFC 1951), GZIP (RFC 1952), TAR (ustar/PAX/GNU) and ZIP (Stored + Deflate, ZIP64, UTF-8, data descriptors), plus a `.tar.gz` pipeline, format detection and file/directory helpers.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

Full documentation: [wiki/MAIN.md](wiki/MAIN.md)

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["ArchiveKit"] }
```

Then at the crate root:

```rust
sdk::preinclude!();
use ArchiveKit::{tar_gzip_compress_files, tar_gzip_decompress};
use ArchiveKit::deflate::CompressionLevel;
```

Or depend directly:

```toml
[dependencies]
archivekit = { path = "../ArchiveKit" }
```

```rust
use archivekit::{zip_pack, zip_unpack, ZipWriterOptions};

let opts = ZipWriterOptions::default();
let raw = zip_pack(&[("a.txt", b"data" as &[u8])], &opts).unwrap();
let entries = zip_unpack(&raw).unwrap();
```

Bundle ID: `com.tontoo.archivekit`

## C Interop

```c
#include "archivekit.h"
size_t len = 0;
uint8_t *gz = archivekit_gzip_compress(input, input_len, 2, &len);
archivekit_free_buffer(gz, len);
```

## License

TCL v26.1
