# Deflate

Raw DEFLATE codec (RFC 1951). The decoder implements full inflate (stored, fixed-Huffman and dynamic-Huffman blocks); the encoder runs greedy LZ77 over a 32 KiB window and emits fixed-Huffman blocks with a stored-block fallback for incompressible data. Output is valid RFC 1951 and interoperable with system tools.

## Compression Levels

```rust
pub enum CompressionLevel {
    None,
    Fastest,
    Balanced,
    Best,
}
```

| Variant | Behavior |
|---|---|
| `None` | Stored blocks only, no match search |
| `Fastest` | Short hash-chain search (fast, larger output) |
| `Balanced` | Default; medium search depth |
| `Best` | Deepest search (slow, smallest output) |

## One-Shot API

### `compress_raw`

```rust
pub fn compress_raw(data: &[u8], level: CompressionLevel) -> Vec<u8>
```

Compresses raw bytes into a raw DEFLATE stream. Empty input yields a single empty fixed block so the stream always terminates.

```rust
use archivekit::deflate::{compress_raw, CompressionLevel};

let enc = compress_raw(b"hello hello hello", CompressionLevel::Balanced);
assert!(!enc.is_empty());
```

### `decompress_raw`

```rust
pub fn decompress_raw(data: &[u8]) -> Result<Vec<u8>>
```

Decompresses with the default 256 MiB output limit. Returns `Err` when the stream is truncated, uses a reserved block type, contains an invalid code or distance, or exceeds the limit.

### `decompress_raw_limited`

```rust
pub fn decompress_raw_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>>
```

Same as `decompress_raw` with an explicit output cap (zip-bomb protection).

```rust
use archivekit::deflate::{compress_raw, decompress_raw_limited, CompressionLevel};

let enc = compress_raw(b"abcabcabc", CompressionLevel::Best);
assert!(decompress_raw_limited(&enc, 4).is_err());
assert_eq!(decompress_raw_limited(&enc, 9).unwrap(), b"abcabcabc");
```

## Constants

| Item | Value | Description |
|---|---|---|
| `DEFAULT_MAX_OUTPUT` | `268_435_456` | Default 256 MiB output cap |

## Block Strategy

The encoder cuts the input into ~32 KiB blocks. Each block compares the fixed-Huffman bit cost against the stored cost and emits whichever is smaller, so incompressible data never expands by more than the stored-block framing (5 bytes per 64 KiB).

> **Note:** The encoder emits fixed-Huffman blocks only (plus stored). Dynamic-Huffman blocks are decoded but never written. This is fully spec-compliant.

## Usage / Example

```rust
use archivekit::deflate::{
    compress_raw, decompress_raw, CompressionLevel,
};

fn main() -> archivekit::Result<()> {
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    for level in [
        CompressionLevel::None,
        CompressionLevel::Fastest,
        CompressionLevel::Balanced,
        CompressionLevel::Best,
    ] {
        let enc = compress_raw(&data, level);
        assert_eq!(decompress_raw(&enc)?, data);
    }
    Ok(())
}
```

## Cross References

- [Gzip.md](Gzip.md) – wraps this engine with headers, CRC32 and members
- [Zip.md](Zip.md) – uses this engine for Deflate entries
- [Error.md](Error.md) – `InvalidData` cases for corrupt streams
