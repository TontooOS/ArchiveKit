//! Benchmark: ArchiveKit vs flate2 / zip / tar.
//!
//! Run with: `cargo run --release --example bench_cmp`
//! Compares wall time (median of 5), throughput and output size.
//! Dev-dependencies only – the library itself never uses these crates.

use archivekit::deflate::CompressionLevel;
use std::hint::black_box;
use std::io::{Cursor, Read, Write};
use std::time::Instant;

const ITERS: usize = 5;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Run `f` ITERS times, return (median_ms, last_output).
fn time_it<F, R>(mut f: F) -> (f64, R)
where
    F: FnMut() -> R,
{
    // Warmup (also serves as the correctness run site for callers).
    let mut last = f();
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        last = black_box(f());
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    black_box(&last);
    (median(samples), last)
}

fn mb_per_s(median_ms: f64, bytes: usize) -> f64 {
    (bytes as f64) / (median_ms / 1000.0) / (1024.0 * 1024.0)
}

// ---------------------------------------------------------------- payloads

fn lcg(seed: &mut u32) -> u8 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (*seed >> 24) as u8
}

fn make_text(size: usize) -> Vec<u8> {
    let para = b"The TontooOS octopus polishes every pixel until the glass shines. \
        Windows float above the blur, spring physics settle them softly. ";
    let mut out = Vec::with_capacity(size);
    let mut i = 0usize;
    while out.len() < size {
        out.extend_from_slice(para);
        out.extend_from_slice(format!("line {i:08} padding tail ~~~\n").as_bytes());
        i += 1;
    }
    out.truncate(size);
    out
}

fn make_random(size: usize) -> Vec<u8> {
    let mut seed = 0x1234_5678u32;
    (0..size).map(|_| lcg(&mut seed)).collect()
}

// ---------------------------------------------------------------- main

fn main() {
    let text = make_text(4 * 1024 * 1024);
    let rand = make_random(2 * 1024 * 1024);
    println!("# ArchiveKit vs flate2 / zip / tar");
    println!("release build, median of {ITERS} runs, payloads: TEXT 4 MiB, RAND 2 MiB");
    println!();

    bench_deflate(&text, &rand);
    bench_gzip(&text, &rand);
    bench_zip(&text, &rand);
    bench_tar(&text, &rand);
    bench_app_random_access(&text, &rand);
}

fn bench_deflate(text: &[u8], rand: &[u8]) {
    println!("## 1. Raw DEFLATE (4 MiB text)");
    println!("| impl | level | enc ms | enc MB/s | bytes | ratio | dec ms | dec MB/s |");
    println!("|---|---|---|---|---|---|---|---|");
    let levels = [
        ("none", CompressionLevel::None, flate2::Compression::none()),
        ("fast", CompressionLevel::Fastest, flate2::Compression::fast()),
        ("default/bal", CompressionLevel::Balanced, flate2::Compression::default()),
        ("best", CompressionLevel::Best, flate2::Compression::best()),
    ];
    for (name, ak_level, fl_level) in levels {
        // archivekit
        let (ms, enc) = time_it(|| archivekit::deflate::compress_raw(black_box(text), ak_level));
        assert_eq!(archivekit::deflate::decompress_raw(&enc).unwrap(), text);
        let (dms, _) = time_it(|| archivekit::deflate::decompress_raw(black_box(&enc)).unwrap());
        println!(
            "| ak | {name} | {ms:.1} | {:.1} | {} | {:.3} | {dms:.1} | {:.1} |",
            mb_per_s(ms, text.len()),
            enc.len(),
            enc.len() as f64 / text.len() as f64,
            mb_per_s(dms, text.len()),
        );
        // flate2
        let (ms, enc) = time_it(|| {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), fl_level);
            e.write_all(black_box(text)).unwrap();
            e.finish().unwrap()
        });
        {
            let mut d = flate2::read::DeflateDecoder::new(&enc[..]);
            let mut out = Vec::new();
            d.read_to_end(&mut out).unwrap();
            assert_eq!(out, text);
        }
        let enc2 = enc.clone();
        let (dms, _) = time_it(|| {
            let mut d = flate2::read::DeflateDecoder::new(black_box(&enc2[..]));
            let mut out = Vec::new();
            d.read_to_end(&mut out).unwrap();
            out
        });
        println!(
            "| flate2 | {name} | {ms:.1} | {:.1} | {} | {:.3} | {dms:.1} | {:.1} |",
            mb_per_s(ms, text.len()),
            enc.len(),
            enc.len() as f64 / text.len() as f64,
            mb_per_s(dms, text.len()),
        );
    }
    // incompressible sanity (encode only)
    println!();
    println!("2 MiB random (encode ms / bytes):");
    for (name, ak_level, fl_level) in [
        ("ak-fast", CompressionLevel::Fastest, None),
        ("ak-bal", CompressionLevel::Balanced, None),
        ("fl-fast", CompressionLevel::Fastest, Some(flate2::Compression::fast())),
        ("fl-default", CompressionLevel::Balanced, Some(flate2::Compression::default())),
    ] {
        if let Some(fl) = fl_level {
            let (ms, enc) = time_it(|| {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), fl);
                e.write_all(black_box(rand)).unwrap();
                e.finish().unwrap()
            });
            println!("| {name} | {ms:.1} | {} |", enc.len());
        } else {
            let (ms, enc) =
                time_it(|| archivekit::deflate::compress_raw(black_box(rand), ak_level));
            println!("| {name} | {ms:.1} | {} |", enc.len());
        }
    }
    println!();
}

fn bench_gzip(text: &[u8], rand: &[u8]) {
    println!("## 2. GZIP (4 MiB text)");
    println!("| impl | level | enc ms | enc MB/s | bytes | ratio | dec ms | dec MB/s |");
    println!("|---|---|---|---|---|---|---|---|");
    let levels = [
        ("none", CompressionLevel::None, flate2::Compression::none()),
        ("fast", CompressionLevel::Fastest, flate2::Compression::fast()),
        ("default/bal", CompressionLevel::Balanced, flate2::Compression::default()),
        ("best", CompressionLevel::Best, flate2::Compression::best()),
    ];
    for (name, ak_level, fl_level) in levels {
        let (ms, enc) = time_it(|| archivekit::gzip_compress(black_box(text), ak_level));
        assert_eq!(archivekit::gzip_decompress(&enc).unwrap(), text);
        let (dms, _) = time_it(|| archivekit::gzip_decompress(black_box(&enc)).unwrap());
        println!(
            "| ak | {name} | {ms:.1} | {:.1} | {} | {:.3} | {dms:.1} | {:.1} |",
            mb_per_s(ms, text.len()),
            enc.len(),
            enc.len() as f64 / text.len() as f64,
            mb_per_s(dms, text.len()),
        );
        let (ms, enc) = time_it(|| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), fl_level);
            e.write_all(black_box(text)).unwrap();
            e.finish().unwrap()
        });
        {
            let mut d = flate2::read::GzDecoder::new(&enc[..]);
            let mut out = Vec::new();
            d.read_to_end(&mut out).unwrap();
            assert_eq!(out, text);
        }
        let enc2 = enc.clone();
        let (dms, _) = time_it(|| {
            let mut d = flate2::read::GzDecoder::new(black_box(&enc2[..]));
            let mut out = Vec::new();
            d.read_to_end(&mut out).unwrap();
            out
        });
        println!(
            "| flate2 | {name} | {ms:.1} | {:.1} | {} | {:.3} | {dms:.1} | {:.1} |",
            mb_per_s(ms, text.len()),
            enc.len(),
            enc.len() as f64 / text.len() as f64,
            mb_per_s(dms, text.len()),
        );
    }
    // cross-compat spot check
    let ak_best = archivekit::gzip_compress(rand, CompressionLevel::Best);
    let mut d = flate2::read::GzDecoder::new(&ak_best[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    assert_eq!(out, rand);
    println!("cross-check: flate2 reads ak-gzip output: OK");
    println!();
}

fn file_set(text: &[u8], rand: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("docs/a.txt", text[..1_048_576].to_vec()),
        ("docs/b.txt", text[1_048_576..2_097_152].to_vec()),
        ("docs/c.txt", text[2_097_152..3_145_728].to_vec()),
        ("bin/blob1.bin", rand[..524_288].to_vec()),
        ("bin/blob2.bin", rand[524_288..1_048_576].to_vec()),
        ("root.txt", text[..65_536].to_vec()),
    ]
}

fn bench_zip(text: &[u8], rand: &[u8]) {
    let files = file_set(text, rand);
    let total: usize = files.iter().map(|(_, d)| d.len()).sum();
    println!("## 3. ZIP ({} files, {:.1} MiB)", files.len(), total as f64 / 1048576.0);
    println!("| impl | method | pack ms | MB/s | bytes | list ms | read-one ms | full-unpack ms |");
    println!("|---|---|---|---|---|---|---|---|");

    // archivekit stored
    let opts_none = archivekit::ZipWriterOptions {
        level: CompressionLevel::None,
        comment: String::new(),
    };
    let (ms, ak_stored) = time_it(|| {
        let mut w = archivekit::ZipWriter::with_options(opts_none.clone());
        for (n, d) in &files {
            w.append_file(black_box(n), black_box(d)).unwrap();
        }
        w.finish()
    });
    let ak_stored2 = ak_stored.clone();
    let (lms, _) = time_it(|| archivekit::zip_unpack(black_box(&ak_stored2)).unwrap().len());
    let (rms, _) = time_it(|| {
        archivekit::zip_unpack(black_box(&ak_stored2))
            .unwrap()
            .into_iter()
            .find(|e| e.name == "root.txt")
            .unwrap()
            .data
    });
    let (ums, _) = time_it(|| archivekit::zip_unpack(black_box(&ak_stored2)).unwrap());
    println!(
        "| ak | stored | {ms:.1} | {:.1} | {} | {lms:.2} | {rms:.2} | {ums:.1} |",
        mb_per_s(ms, total),
        ak_stored.len()
    );

    // archivekit deflate balanced
    let opts_bal = archivekit::ZipWriterOptions {
        level: CompressionLevel::Balanced,
        comment: String::new(),
    };
    let (ms, ak_def) = time_it(|| {
        let mut w = archivekit::ZipWriter::with_options(opts_bal.clone());
        for (n, d) in &files {
            w.append_file(black_box(n), black_box(d)).unwrap();
        }
        w.finish()
    });
    {
        let back = archivekit::zip_unpack(&ak_def).unwrap();
        assert_eq!(back.iter().find(|e| e.name == "docs/a.txt").unwrap().data, files[0].1);
    }
    let ak_def2 = ak_def.clone();
    let (lms, _) = time_it(|| archivekit::zip_unpack(black_box(&ak_def2)).unwrap().len());
    let (rms, _) = time_it(|| {
        archivekit::zip_unpack(black_box(&ak_def2))
            .unwrap()
            .into_iter()
            .find(|e| e.name == "root.txt")
            .unwrap()
            .data
    });
    let (ums, _) = time_it(|| archivekit::zip_unpack(black_box(&ak_def2)).unwrap());
    println!(
        "| ak | deflate-bal | {ms:.1} | {:.1} | {} | {lms:.2} | {rms:.2} | {ums:.1} |",
        mb_per_s(ms, total),
        ak_def.len()
    );

    // zip crate stored
    let (ms, z_stored) = time_it(|| {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (n, d) in &files {
            w.start_file(black_box(*n), o).unwrap();
            w.write_all(black_box(d)).unwrap();
        }
        w.finish().unwrap().into_inner()
    });
    let z_stored2 = z_stored.clone();
    let (lms, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_stored2))).unwrap();
        black_box(z.len())
    });
    let (rms, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_stored2))).unwrap();
        let mut f = z.by_name("root.txt").unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        out
    });
    let (ums, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_stored2))).unwrap();
        let mut n = 0usize;
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let mut out = Vec::new();
            f.read_to_end(&mut out).unwrap();
            n += out.len();
        }
        n
    });
    println!(
        "| zip-crate | stored | {ms:.1} | {:.1} | {} | {lms:.2} | {rms:.2} | {ums:.1} |",
        mb_per_s(ms, total),
        z_stored.len()
    );

    // zip crate deflated
    let (ms, z_def) = time_it(|| {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (n, d) in &files {
            w.start_file(black_box(*n), o).unwrap();
            w.write_all(black_box(d)).unwrap();
        }
        w.finish().unwrap().into_inner()
    });
    {
        let mut z = zip::ZipArchive::new(Cursor::new(&z_def[..])).unwrap();
        let mut f = z.by_name("docs/a.txt").unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        assert_eq!(out, files[0].1);
    }
    let z_def2 = z_def.clone();
    let (lms, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_def2))).unwrap();
        black_box(z.len())
    });
    let (rms, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_def2))).unwrap();
        let mut f = z.by_name("root.txt").unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        out
    });
    let (ums, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&z_def2))).unwrap();
        let mut n = 0usize;
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let mut out = Vec::new();
            f.read_to_end(&mut out).unwrap();
            n += out.len();
        }
        n
    });
    println!(
        "| zip-crate | deflated | {ms:.1} | {:.1} | {} | {lms:.2} | {rms:.2} | {ums:.1} |",
        mb_per_s(ms, total),
        z_def.len()
    );
    println!();
}

fn bench_tar(text: &[u8], rand: &[u8]) {
    let files = file_set(text, rand);
    let total: usize = files.iter().map(|(_, d)| d.len()).sum();
    println!("## 4. TAR ({} files, {:.1} MiB, uncompressed)", files.len(), total as f64 / 1048576.0);
    println!("| impl | pack ms | MB/s | bytes | full-scan ms | find-one ms |");
    println!("|---|---|---|---|---|---|");

    let (ms, ak_tar) = time_it(|| {
        let mut w = archivekit::TarWriter::new();
        let o = archivekit::TarWriteOptions::default();
        for (n, d) in &files {
            w.append_file(black_box(n), black_box(d), &o).unwrap();
        }
        w.finish()
    });
    {
        let back = archivekit::tar_unpack(&ak_tar).unwrap();
        assert_eq!(back[0].data, files[0].1);
    }
    let ak_tar2 = ak_tar.clone();
    let (sms, _) = time_it(|| archivekit::tar_unpack(black_box(&ak_tar2)).unwrap());
    // find-one: scan until root.txt (last entry) and read it
    let (fms, _) = time_it(|| {
        let mut r = archivekit::TarReader::new(black_box(&ak_tar2));
        let mut out = Vec::new();
        while let Some(e) = r.next_entry().unwrap() {
            if e.path == "root.txt" {
                out = e.data;
                break;
            }
        }
        out
    });
    println!(
        "| ak | {ms:.1} | {:.1} | {} | {sms:.1} | {fms:.1} |",
        mb_per_s(ms, total),
        ak_tar.len()
    );

    let (ms, tc_tar) = time_it(|| {
        let mut b = tar::Builder::new(Vec::new());
        for (n, d) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, black_box(*n), black_box(&d[..])).unwrap();
        }
        b.into_inner().unwrap()
    });
    let tc_tar2 = tc_tar.clone();
    let (sms, _) = time_it(|| {
        let mut a = tar::Archive::new(black_box(&tc_tar2[..]));
        let mut n = 0usize;
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let mut out = Vec::new();
            e.read_to_end(&mut out).unwrap();
            n += out.len();
        }
        n
    });
    let (fms, _) = time_it(|| {
        let mut a = tar::Archive::new(black_box(&tc_tar2[..]));
        let mut out = Vec::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let p = e.path().unwrap().to_string_lossy().into_owned();
            if p == "root.txt" {
                e.read_to_end(&mut out).unwrap();
                break;
            }
        }
        out
    });
    println!(
        "| tar-crate | {ms:.1} | {:.1} | {} | {sms:.1} | {fms:.1} |",
        mb_per_s(ms, total),
        tc_tar.len()
    );
    println!();
}

fn bench_app_random_access(text: &[u8], rand: &[u8]) {
    // One 16 MiB blob FIRST, then the small files: scanners pay full price.
    let big = rand.iter().cycle().take(16 * 1024 * 1024).cloned().collect::<Vec<u8>>();
    let files = file_set(text, rand);
    println!("## 5. Random access (16 MiB blob + {} small files)", files.len());
    println!("| impl | pack ms | bytes | list ms | read-one-small ms |");
    println!("|---|---|---|---|---|");

    // app, stored
    let (ms, ak_app) = time_it(|| {
        let mut b = archivekit::AppBuilder::new("Bench").unwrap();
        b.set_manifest(archivekit::AppManifest::new("com.t.bench", "1", "App/x"));
        b.add_file(black_box("App/big.bin"), black_box(big.clone())).unwrap();
        for (n, d) in &files {
            let p = format!("App/{n}");
            b.add_file(black_box(&p), black_box(d.clone())).unwrap();
        }
        b.finish().unwrap()
    });
    let ak_app2 = ak_app.clone();
    let (lms, _) = time_it(|| archivekit::AppReader::from_bytes(black_box(&ak_app2)).unwrap().list_names());
    let (rms, _) = time_it(|| {
        let mut r = archivekit::AppReader::from_bytes(black_box(&ak_app2)).unwrap();
        r.read_file("Bench.app/App/root.txt").unwrap()
    });
    println!("| ak-app stored | {ms:.1} | {} | {lms:.2} | {rms:.2} |", ak_app.len());

    // zip crate deflated (central dir, the fair indexed competitor)
    let (ms, zc) = time_it(|| {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("App/big.bin", o).unwrap();
        w.write_all(black_box(&big)).unwrap();
        for (n, d) in &files {
            let p = format!("App/{n}");
            w.start_file(black_box(&p), o).unwrap();
            w.write_all(black_box(d)).unwrap();
        }
        w.finish().unwrap().into_inner()
    });
    let zc2 = zc.clone();
    let (lms, _) = time_it(|| {
        let z = zip::ZipArchive::new(Cursor::new(black_box(&zc2))).unwrap();
        z.len()
    });
    let (rms, _) = time_it(|| {
        let mut z = zip::ZipArchive::new(Cursor::new(black_box(&zc2))).unwrap();
        let mut f = z.by_name("App/root.txt").unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        out
    });
    println!("| zip-crate stored | {ms:.1} | {} | {lms:.2} | {rms:.2} |", zc.len());

    // tar crate: must stream past the 16 MiB blob to list/find
    let (ms, tc) = time_it(|| {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(big.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "App/big.bin", black_box(&big[..])).unwrap();
        for (n, d) in &files {
            let p = format!("App/{n}");
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, black_box(&p), black_box(&d[..])).unwrap();
        }
        b.into_inner().unwrap()
    });
    let tc2 = tc.clone();
    let (lms, _) = time_it(|| {
        let mut a = tar::Archive::new(black_box(&tc2[..]));
        let mut names = Vec::new();
        for e in a.entries().unwrap() {
            let e = e.unwrap();
            names.push(e.path().unwrap().to_string_lossy().into_owned());
            let mut out = Vec::new();
            let mut e2 = e;
            e2.read_to_end(&mut out).unwrap();
        }
        names
    });
    let (rms, _) = time_it(|| {
        let mut a = tar::Archive::new(black_box(&tc2[..]));
        let mut out = Vec::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().to_string_lossy() == "App/root.txt" {
                e.read_to_end(&mut out).unwrap();
                break;
            }
        }
        out
    });
    println!("| tar-crate | {ms:.1} | {} | {lms:.1} | {rms:.1} |", tc.len());
    println!();
    println!("done.");
}
