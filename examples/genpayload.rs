//! Deterministic payload generator for benchmarks.
//!
//! Usage: genpayload <dir>
//! Writes text/ (8 x 32 MiB compressible) + random/ (8 x 32 MiB noise).

use std::path::Path;

fn lcg(seed: &mut u32) -> u8 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (*seed >> 24) as u8
}

fn main() {
    let arg = std::env::args().nth(1).expect("usage: genpayload <dir>");
    let dir = Path::new(&arg);
    std::fs::create_dir_all(dir.join("text")).unwrap();
    std::fs::create_dir_all(dir.join("random")).unwrap();
    let para = b"The TontooOS octopus polishes every pixel until the glass shines. \
        Windows float above the blur, spring physics settle them softly. ";
    const FILE: usize = 32 * 1024 * 1024;
    for i in 0..8 {
        // compressible text with per-file variation
        let mut v = Vec::with_capacity(FILE);
        let mut n = 0usize;
        while v.len() < FILE {
            v.extend_from_slice(para);
            v.extend_from_slice(format!("file {i} line {n:08} padding tail ~~~\n").as_bytes());
            n += 1;
        }
        v.truncate(FILE);
        std::fs::write(dir.join(format!("text/t{i}.txt")), &v).unwrap();
        // incompressible noise
        let mut seed = 0x1234_5678u32 ^ (i as u32 * 0x9E37_79B9);
        let r: Vec<u8> = (0..FILE).map(|_| lcg(&mut seed)).collect();
        std::fs::write(dir.join(format!("random/r{i}.bin")), &r).unwrap();
        eprintln!("wrote pair {i}");
    }
    eprintln!("done");
}
