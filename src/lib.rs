//! ArchiveKit – ZIP, GZIP, TAR and indexed `.app` / `.tico` containers.
//!
//! 100% hand-written compression: DEFLATE (RFC 1951), GZIP (RFC 1952),
//! TAR (ustar/PAX/GNU) and ZIP (Stored + Deflate, ZIP64, UTF-8, data
//! descriptors). The `.app` container (TAPP) adds indexed single-file apps
//! with FishFile manifests and `.tico` icons; `.tico` icons use the same
//! indexed engine with their own `TICO` magic and a FishFile manifest.
//! Codecs are dependency-free (`std` only); only the manifests use
//! FishFile.
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
//!
pub mod app;
pub mod combined;
pub mod crc;
pub mod deflate;
pub mod error;
pub mod ffi;
pub mod gzip;
pub mod tar;
pub mod tico;
pub mod zip;
pub mod zlib;

/// Library version: (major, minor, patch).
pub const ARCHIVEKIT_VERSION: (u32, u32, u32) = (26, 1, 0);

/// Library version string.
pub const ARCHIVEKIT_VERSION_STR: &str = "27.0.0";

pub use combined::{
    compress_bytes, compress_file, decompress_bytes, detect_format, extract_archive, extract_bytes,
    list_names, pack_dir_to_archive, tar_gzip_compress, tar_gzip_compress_files,
    tar_gzip_decompress, Format,
};
pub use app::{
    app_extract_to_file, app_pack_dir, AppBuilder, AppEntryMeta, AppManifest, AppMethod,
    AppReader, APP_EXTENSION, APP_FOOTER_MAGIC, APP_MAGIC, APP_MANIFEST_NAME, APP_VERSION,
};
pub use tico::{
    tico_extract_to_file, tico_pack_bytes, tico_pack_dir, validate_tico, TicoBackground,
    TicoBuilder, TicoEntryMeta, TicoInfo, TicoLayerMeta, TicoManifest, TicoMethod, TicoReader,
    TICO_EXTENSION, TICO_FOOTER_LEN, TICO_FOOTER_MAGIC, TICO_MAGIC, TICO_MANIFEST_NAME,
    TICO_VERSION, TLYR_MAGIC, TLYR_VERSION,
};
pub use deflate::CompressionLevel;
pub use error::{ArchiveError, Result};
pub use gzip::{
    gzip_compress, gzip_compress_with_options, gzip_decompress, gzip_decompress_limited,
    gzip_members, GzipDecoder, GzipEncoder, GzipMember, GzipOptions,
};
pub use tar::{tar_pack, tar_pack_dir, tar_unpack, tar_unpack_to_dir, TarEntry, TarKind, TarReader,
    TarWriteOptions, TarWriter};
pub use zip::{zip_pack, zip_unpack, ZipEntry, ZipFileReader, ZipFileWriter, ZipIndexEntry, ZipMethod, ZipReader, ZipWriter, ZipWriterOptions};
pub use zlib::{
    zlib_compress, zlib_decompress, zlib_decompress_limited, zlib_decompress_unverified,
    zlib_decompress_unverified_limited, zlib_stream, zlib_stream_limited, ZlibStream,
};

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

#[cfg(test)]
mod fuzz_regression {
    //! Deterministic no-panic regression test over mutated corrupt inputs.
    //! The heavy campaign lives in `examples/fuzz.rs`; this is the fast
    //! gate that runs with every `cargo test`.
    use std::panic::{catch_unwind, AssertUnwindSafe};

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                return 0;
            }
            (self.next() % n as u64) as usize
        }
    }

    fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
        let mut v = bytes.to_vec();
        for _ in 0..1 + rng.below(2) {
            if v.is_empty() {
                v.push(rng.next() as u8);
                continue;
            }
            match rng.below(5) {
                0 => {
                    for _ in 0..1 + rng.below(6) {
                        let i = rng.below(v.len());
                        v[i] ^= 1 << rng.below(8);
                    }
                }
                1 => v.truncate(rng.below(v.len() + 1)),
                2 => {
                    let a = rng.below(v.len());
                    let len = rng.below(v.len() - a + 1);
                    v.drain(a..a + len);
                }
                3 => {
                    let a = rng.below(v.len());
                    for k in 0..4 {
                        if a + k < v.len() {
                            v[a + k] = 0xFF;
                        }
                    }
                }
                _ => {
                    let at = rng.below(v.len() + 1);
                    let n = 1 + rng.below(16);
                    let ins = vec![rng.next() as u8; n];
                    v.splice(at..at, ins);
                }
            }
        }
        v.truncate(2048);
        v
    }

    #[test]
    fn corrupt_inputs_never_panic() {
        use crate::deflate::CompressionLevel;
        let text = b"The quick brown fox jumps over the lazy dog. ".repeat(30);
        let gz = crate::gzip_compress(&text, CompressionLevel::Balanced);
        let zp = crate::zip_pack(
            &[("a.txt", text.as_slice())],
            &crate::ZipWriterOptions {
                level: CompressionLevel::Balanced,
                comment: String::new(),
            },
        )
        .unwrap();
        let tp = crate::tar_pack(&[crate::TarEntry::file("a", text.clone())]).unwrap();
        let mut manifest = crate::AppManifest::new("com.t.fuzz", "1", "App/f");
        manifest.names.push(("en_us".to_string(), "F".to_string()));
        let mut builder = crate::AppBuilder::new("F").unwrap();
        builder.set_manifest(manifest);
        builder.add_file("App/f", text.clone()).unwrap();
        let app = builder.finish().unwrap();
        let corpus: &[&[u8]] = &[&gz, &zp, &tp, &app, &text, b""];
        let mut rng = Rng(0xC0FFEE);
        for i in 0..1500 {
            let input = mutate(&mut rng, corpus[i % corpus.len()]);
            let r = catch_unwind(AssertUnwindSafe(|| {
                let _ = crate::deflate::decompress_raw(&input);
                let _ = crate::gzip_decompress(&input);
                let _ = crate::zip_unpack(&input);
                let _ = crate::tar_unpack(&input);
                if let Ok(mut r) = crate::AppReader::from_bytes(&input) {
                    for n in r.list_names() {
                        let _ = r.read_file(&n);
                    }
                    let _ = r.read_manifest();
                }
                let _ = crate::validate_tico(&input);
                let _ = crate::detect_format(&input);
                let _ = crate::list_names(&input);
            }));
            assert!(r.is_ok(), "panic on fuzz case {i}");
        }
    }
}
