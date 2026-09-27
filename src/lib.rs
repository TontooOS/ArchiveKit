//! ArchiveKit – ZIP, GZIP, TAR and `.app` containers for TontooOS.
//!
//! 100% hand-written compression: DEFLATE (RFC 1951), GZIP (RFC 1952),
//! TAR (ustar/PAX/GNU) and ZIP (Stored + Deflate, ZIP64, UTF-8, data
//! descriptors). The `.app` container (TAPP) adds indexed single-file apps
//! with FishFile manifests and `.tico` icons. Codecs are dependency-free
//! (`std` only); only the app manifest uses FishFile.
//!
//! # Quick Start
//!
//! ```rust
//! use archivekit::{Format, tar_gzip_compress_files, tar_gzip_decompress};
//! use archivekit::deflate::CompressionLevel;
//!
//! let tgz = tar_gzip_compress_files(
//!     &[("hello.txt", b"hello tgz" as &[u8])],
//!     CompressionLevel::Balanced,
//! )
//! .unwrap();
//! let entries = tar_gzip_decompress(&tgz).unwrap();
//! assert_eq!(entries[0].data, b"hello tgz");
//! ```

pub mod app;
pub mod combined;
pub mod crc;
pub mod deflate;
pub mod error;
pub mod ffi;
pub mod gzip;
pub mod tar;
pub mod zip;

/// Library version: (major, minor, patch).
pub const ARCHIVEKIT_VERSION: (u32, u32, u32) = (26, 1, 0);

/// Library version string.
pub const ARCHIVEKIT_VERSION_STR: &str = "26.1.0";

pub use combined::{
    compress_bytes, compress_file, decompress_bytes, detect_format, extract_archive, extract_bytes,
    list_names, pack_dir_to_archive, tar_gzip_compress, tar_gzip_compress_files,
    tar_gzip_decompress, Format,
};
pub use app::{
    app_extract_to_file, app_pack_dir, validate_tico, AppBuilder, AppEntryMeta, AppManifest,
    AppMethod, AppReader, TicoInfo, APP_EXTENSION, APP_FOOTER_MAGIC, APP_MAGIC, APP_MANIFEST_NAME,
    APP_VERSION,
};
pub use deflate::CompressionLevel;
pub use error::{ArchiveError, Result};
pub use gzip::{
    gzip_compress, gzip_compress_with_options, gzip_decompress, gzip_decompress_limited,
    gzip_members, GzipDecoder, GzipEncoder, GzipMember, GzipOptions,
};
pub use tar::{tar_pack, tar_pack_dir, tar_unpack, tar_unpack_to_dir, TarEntry, TarKind, TarReader,
    TarWriteOptions, TarWriter};
pub use zip::{zip_pack, zip_unpack, ZipEntry, ZipMethod, ZipReader, ZipWriter, ZipWriterOptions};

/// Convenience prelude.
pub mod prelude {
    pub use crate::app::{AppManifest, AppMethod};
    pub use crate::combined::Format;
    pub use crate::deflate::CompressionLevel;
    pub use crate::error::{ArchiveError, Result};
    pub use crate::gzip::{GzipMember, GzipOptions};
    pub use crate::tar::{TarEntry, TarKind};
    pub use crate::zip::{ZipEntry, ZipMethod};
    pub use crate::{ARCHIVEKIT_VERSION, ARCHIVEKIT_VERSION_STR};
}
