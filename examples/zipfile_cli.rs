//! File-backed ZIP CLI for benchmarks and scripting.
//!
//! Usage:
//!   zipfile_cli pack <src> <dst.zip> [--stored]
//!   zipfile_cli unpack <src.zip> <dst_dir>
//!   zipfile_cli list <src.zip>

use archivekit::{ZipFileReader, ZipFileWriter, ZipMethod};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: pack|unpack|list ...");
        std::process::exit(2);
    }
    let res = match args[1].as_str() {
        "pack" => {
            if args.len() < 4 {
                eprintln!("usage: pack <src> <dst.zip> [--stored]");
                std::process::exit(2);
            }
            let stored = args.get(4).map(|s| s == "--stored").unwrap_or(false);
            cmd_pack(Path::new(&args[2]), Path::new(&args[3]), stored)
        }
        "unpack" => {
            if args.len() < 4 {
                eprintln!("usage: unpack <src.zip> <dst_dir>");
                std::process::exit(2);
            }
            cmd_unpack(Path::new(&args[2]), Path::new(&args[3]))
        }
        "list" => {
            if args.len() < 3 {
                eprintln!("usage: list <src.zip>");
                std::process::exit(2);
            }
            cmd_list(Path::new(&args[2]))
        }
        other => {
            eprintln!("unknown command {other}");
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn walk_sorted(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut kids: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|e| e.map(|x| x.path()))
        .collect::<Result<_, _>>()?;
    kids.sort();
    for k in kids {
        out.push(k.clone());
        if k.is_dir() {
            walk_sorted(&k, out)?;
        }
    }
    Ok(())
}

fn cmd_pack(src: &Path, dst: &Path, stored: bool) -> archivekit::Result<()> {
    let mut w = ZipFileWriter::create(dst)?;
    let meta = std::fs::symlink_metadata(src)
        .map_err(|e| archivekit::ArchiveError::Io(e.to_string()))?;
    if meta.is_dir() {
        let mut paths = Vec::new();
        walk_sorted(src, &mut paths).map_err(|e| archivekit::ArchiveError::Io(e.to_string()))?;
        for p in paths {
            let rel = p
                .strip_prefix(src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                w.append_dir(&rel)?;
            } else if p.is_file() {
                if stored {
                    w.append_file_from_disk(&rel, &p, ZipMethod::Stored)?;
                } else {
                    let data = std::fs::read(&p)
                        .map_err(|e| archivekit::ArchiveError::Io(e.to_string()))?;
                    w.append_file(&rel, &data)?;
                }
            }
        }
    } else {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("data.bin");
        if stored {
            w.append_file_from_disk(name, src, ZipMethod::Stored)?;
        } else {
            let data =
                std::fs::read(src).map_err(|e| archivekit::ArchiveError::Io(e.to_string()))?;
            w.append_file(name, &data)?;
        }
    }
    w.finish()
}

fn cmd_unpack(src: &Path, dst: &Path) -> archivekit::Result<()> {
    ZipFileReader::open(src)?.extract_all_to(dst)
}

fn cmd_list(src: &Path) -> archivekit::Result<()> {
    let r = ZipFileReader::open(src)?;
    for e in r.index() {
        println!("{}\t{}\t{}", e.uncompressed_size, e.compressed_size, e.name);
    }
    Ok(())
}
