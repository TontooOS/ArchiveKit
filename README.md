# TontooArchiveKit

ZIP, GZIP, TAR and indexed `.app` / `.tico` single-file containers for TontooOS. Hand-written codecs with minimal dependencies: DEFLATE (RFC 1951), GZIP (RFC 1952), TAR (ustar/PAX/GNU) and ZIP (Stored + Deflate, ZIP64, UTF-8, data descriptors), plus indexed TAPP app containers with FishFile manifests and TICO icon containers with FishFile manifests and `.tlyr` layers.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["ArchiveKit"] }
```

Full documentation: [wiki/MAIN.md](wiki/MAIN.md)

## .app Containers

```rust
use archivekit::{AppBuilder, AppManifest, AppReader};
use std::path::Path;

let mut manifest = AppManifest::new("com.tontoo.demo", "1.0.0", "App/demo");
manifest.names.push(("en_us".to_string(), "Demo".to_string()));

let mut b = AppBuilder::new("Demo").unwrap();
b.set_manifest(manifest);
b.add_executable("App/demo", std::fs::read("target/release/demo").unwrap()).unwrap();
b.write_to_file(Path::new("Demo.app")).unwrap();

// Fast: only footer + central directory are read here.
let mut r = AppReader::open(Path::new("Demo.app")).unwrap();
let bin = r.read_file("Demo.app/App/demo").unwrap();
```

Layout mirrors TBuild (`App/`, `Resources/`, `Info.tontoo` manifest in fico syntax, `.tico` icons, no PNG). See [wiki/App.md](wiki/App.md).

## License

TCL v27.0
