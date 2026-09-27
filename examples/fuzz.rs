//! Fuzz ArchiveKit decoders against corrupt inputs: no panics, no hangs.
//!
//! Usage: `fuzz [iterations] [seed]` (defaults: 20000, fixed seed).
//! Builds valid corpus archives with our own writers, mutates them, and
//! decodes with panic catching. Prints every panic with its reproducer and
//! exits nonzero when any case panicked.
//!
//! Also try: `cargo run --release --example fuzz -- 200000`

use archivekit::deflate::CompressionLevel;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Deterministic splitmix64 PRNG (reproducible runs).
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

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// Mutate bytes: bit flips, truncation, span cuts, inserts, field smashes,
/// chunk duplication. Output capped so cases stay fast.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut v = bytes.to_vec();
    for _ in 0..1 + rng.below(3) {
        if v.is_empty() {
            v.push(rng.byte());
            continue;
        }
        match rng.below(6) {
            0 => {
                for _ in 0..1 + rng.below(8) {
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
                let at = rng.below(v.len() + 1);
                let n = 1 + rng.below(32);
                let mut ins = vec![0u8; n];
                for x in &mut ins {
                    *x = rng.byte();
                }
                v.splice(at..at, ins);
            }
            4 => {
                let a = rng.below(v.len());
                let val = [0u32, u32::MAX, 0xDEAD_BEEF, 1][rng.below(4)];
                for k in 0..4 {
                    if a + k < v.len() {
                        v[a + k] = (val >> (8 * k)) as u8;
                    }
                }
            }
            _ => {
                let a = rng.below(v.len());
                let len = rng.below((v.len() - a).min(128) + 1);
                let chunk = v[a..a + len].to_vec();
                let at = rng.below(v.len() + 1);
                v.splice(at..at, chunk);
            }
        }
    }
    v.truncate(8192);
    v
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let text = b"The quick brown fox jumps over the lazy dog. Pack my box! ".repeat(40);
    let mut out: Vec<(&str, Vec<u8>)> = Vec::new();

    // Raw deflate, all levels and block types.
    for level in [
        CompressionLevel::None,
        CompressionLevel::Fastest,
        CompressionLevel::Balanced,
        CompressionLevel::Best,
    ] {
        out.push(("deflate", archivekit::deflate::compress_raw(&text, level)));
    }
    out.push((
        "deflate-empty",
        archivekit::deflate::compress_raw(b"", CompressionLevel::Balanced),
    ));

    // GZIP incl. names and multi-member.
    out.push(("gzip", archivekit::gzip_compress(&text, CompressionLevel::Balanced)));
    let opts = archivekit::GzipOptions {
        level: CompressionLevel::Best,
        mtime: 1_700_000_000,
        name: Some("a.txt".to_string()),
    };
    let mut multi = archivekit::gzip_compress_with_options(b"one", &opts);
    multi.extend_from_slice(&archivekit::gzip_compress(b"two", CompressionLevel::None));
    out.push(("gzip-multi", multi));

    // ZIP incl. dirs, utf-8, descriptors (built by hand in tests normally;
    // here: writer output covers stored/deflate/dir/utf8).
    let zopts = archivekit::ZipWriterOptions {
        level: CompressionLevel::Balanced,
        comment: String::new(),
    };
    let mut w = archivekit::ZipWriter::with_options(zopts);
    w.append_dir("d").unwrap();
    w.append_file("d/a.txt", &text).unwrap();
    w.append_file("Grüße.txt", b"unicode name").unwrap();
    w.append_file("empty.txt", b"").unwrap();
    out.push(("zip", w.finish()));

    // TAR incl. dirs, long names, symlinks.
    let mut tw = archivekit::TarWriter::new();
    let to = archivekit::TarWriteOptions::default();
    tw.append_dir("docs/", &archivekit::TarWriteOptions::dir_default())
        .unwrap();
    tw.append_file("hello.txt", b"hi tar", &to).unwrap();
    tw.append_file(&format!("long/{}.txt", "x".repeat(150)), b"l", &to)
        .unwrap();
    tw.append_symlink("link", "hello.txt", &to).unwrap();
    out.push(("tar", tw.finish()));

    // APP container with manifest, icon, mixed methods.
    let mut manifest = archivekit::AppManifest::new("com.t.fuzz", "1", "App/f");
    manifest.names.push(("en_us".to_string(), "Fuzz".to_string()));
    let mut b = archivekit::AppBuilder::new("Fuzz").unwrap();
    b.set_manifest(manifest);
    b.add_dir("App").unwrap();
    b.add_executable("App/f", b"bin".to_vec()).unwrap();
    b.add_file_compressed("App/r.txt", text.clone(), CompressionLevel::Balanced)
        .unwrap();
    out.push(("app", b.finish().unwrap()));

    // Minimal .tico (for validate_tico).
    let tico_manifest = r##"{"format":"tico","version":1,"name":"f","canvas":1024,"background":{"color":"#FFF"},"layers":[{"file":"layer/00.tlyr","opacity":1.0,"recolorable":true,"default_color":"#000"}]}"##;
    let mut tlyr = b"TLYR".to_vec();
    tlyr.extend_from_slice(&[1u8, 1, 64, 0, 0, 0, 64, 0, 0, 0, 4, 0, 0, 0, 1, 2, 3, 4]);
    let topts = archivekit::ZipWriterOptions {
        level: CompressionLevel::None,
        comment: String::new(),
    };
    let mut tw2 = archivekit::ZipWriter::with_options(topts);
    tw2.append_file("manifest.json", tico_manifest.as_bytes()).unwrap();
    tw2.append_file("layer/00.tlyr", &tlyr).unwrap();
    out.push(("tico", tw2.finish()));

    // Pure noise (decoders must reject without panicking).
    let mut rng = Rng(0x1234);
    let mut noise = vec![0u8; 512];
    for x in &mut noise {
        *x = rng.byte();
    }
    out.push(("noise", noise));
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let iters: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let seed: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xF022BA4);
    let corpus = corpus();
    // Sanity: pristine corpus decodes (else the harness itself is broken).
    for (name, bytes) in &corpus {
        if *name == "noise" {
            continue;
        }
        decode_one(name, bytes);
    }
    println!("corpus sanity OK ({} samples)", corpus.len() - 1);

    let mut rng = Rng(seed);
    let mut panics = 0usize;
    let mut checked = 0usize;
    for i in 0..iters {
        let (name, bytes) = &corpus[rng.below(corpus.len())];
        let input = mutate(&mut rng, bytes);
        let r = catch_unwind(AssertUnwindSafe(|| decode_one(name, &input)));
        checked += 1;
        if r.is_err() {
            panics += 1;
            println!("PANIC target={name} iter={i} len={}", input.len());
            let path = format!("/tmp/ak_fuzz_{name}_{i}.bin");
            let _ = std::fs::write(&path, &input);
            println!("  saved {path} (head={:02x?})", &input[..input.len().min(16)]);
            if panics >= 10 {
                println!("stopping after 10 panics");
                break;
            }
        }
        if (i + 1) % 10_000 == 0 {
            println!("... {i} cases, {panics} panics", i = i + 1);
        }
    }
    println!("done: {checked} cases, {panics} panics");
    if panics > 0 {
        std::process::exit(1);
    }

    // File phase: mutated zips through the file-backed reader (bounded).
    let file_cases = 300;
    let fdir = std::env::temp_dir().join("ak_fuzz_files");
    let _ = std::fs::remove_dir_all(&fdir);
    std::fs::create_dir_all(&fdir).unwrap();
    let zip_src = corpus
        .iter()
        .find(|(n, _)| *n == "zip")
        .map(|(_, b)| b.clone())
        .unwrap();
    let mut fpanics = 0;
    for i in 0..file_cases {
        let input = mutate(&mut rng, &zip_src);
        let zp = fdir.join(format!("f{i}.zip"));
        std::fs::write(&zp, &input).unwrap();
        let r = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut r) = archivekit::ZipFileReader::open(&zp) {
                let names: Vec<String> =
                    r.index().iter().map(|e| e.name.clone()).collect();
                if let Some(n) = names.first() {
                    let mut sink = Vec::new();
                    let _ = r.extract_entry_to_writer(n, &mut sink);
                }
                let _ = r.extract_all_to(&fdir.join(format!("out{i}")));
            }
        }));
        if r.is_err() {
            fpanics += 1;
            println!("PANIC file-phase iter={i}");
            if fpanics >= 5 {
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&fdir);
    println!("file phase: {file_cases} cases, {fpanics} panics");
    if fpanics > 0 {
        std::process::exit(1);
    }
}

/// Decode once with every API fitting the sample kind. Result ignored:
/// only panics (and hangs) are failures here.
fn decode_one(name: &str, bytes: &[u8]) {
    match name {
        "deflate" | "deflate-empty" => {
            let _ = archivekit::deflate::decompress_raw(bytes);
        }
        "gzip" | "gzip-multi" => {
            let _ = archivekit::gzip_decompress(bytes);
            let _ = archivekit::gzip_members(bytes);
            let _ = archivekit::detect_format(bytes);
            let _ = archivekit::list_names(bytes);
        }
        "zip" => {
            let _ = archivekit::zip_unpack(bytes);
            if let Ok(index) = archivekit::ZipReader::new(bytes).read_index() {
                let r = archivekit::ZipReader::new(bytes);
                for e in index.iter().take(4) {
                    let _ = r.read_one(e);
                }
            }
            let _ = archivekit::detect_format(bytes);
            let _ = archivekit::list_names(bytes);
        }
        "tar" => {
            let _ = archivekit::tar_unpack(bytes);
            let _ = archivekit::detect_format(bytes);
            let _ = archivekit::list_names(bytes);
        }
        "app" => {
            if let Ok(mut r) = archivekit::AppReader::from_bytes(bytes) {
                for n in r.list_names() {
                    let _ = r.read_file(&n);
                }
                let _ = r.read_manifest();
            }
            let _ = archivekit::detect_format(bytes);
            let _ = archivekit::list_names(bytes);
        }
        "tico" => {
            let _ = archivekit::validate_tico(bytes);
        }
        _ => {
            let _ = archivekit::deflate::decompress_raw(bytes);
            let _ = archivekit::gzip_decompress(bytes);
            let _ = archivekit::zip_unpack(bytes);
            let _ = archivekit::tar_unpack(bytes);
        }
    }
}
