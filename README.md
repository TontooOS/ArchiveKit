# TontooArchiveKit

ZIP, GZIP and TAR compression for TontooOS. 100% hand-written codecs with zero dependencies (only `std`): DEFLATE (RFC 1951), GZIP (RFC 1952), TAR (ustar/PAX/GNU) and ZIP (Stored + Deflate, ZIP64, UTF-8, data descriptors), plus a `.tar.gz` pipeline, format detection and file/directory helpers.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["ArchiveKit"] }
```

## License

TCL v26.1
