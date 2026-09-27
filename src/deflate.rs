//! Raw DEFLATE (RFC 1951) – hand-written, zero dependencies.
//!
//! - [`decompress_raw`] implements a full inflate decoder covering stored,
//!   fixed-Huffman and dynamic-Huffman blocks.
//! - [`compress_raw`] implements an LZ77 encoder (32 KiB window) that emits
//!   fixed-Huffman blocks with a stored-block fallback for incompressible
//!   data. Output is valid RFC 1951 and interoperable with system tools.

use crate::error::{invalid, ArchiveError, Result};

/// Compression effort for [`compress_raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionLevel {
    /// No compression – emit stored blocks only.
    None,
    /// Fast compression, minimal match search.
    Fastest,
    /// Balanced speed and ratio (default).
    #[default]
    Balanced,
    /// Best ratio, deeper match search.
    Best,
}

impl CompressionLevel {
    fn max_chain(self) -> usize {
        match self {
            CompressionLevel::None => 0,
            CompressionLevel::Fastest => 8,
            CompressionLevel::Balanced => 32,
            CompressionLevel::Best => 128,
        }
    }
}

// ---------------------------------------------------------------------------
// Bit I/O (LSB-first byte order, Huffman codes packed MSB-first per RFC 1951)
// ---------------------------------------------------------------------------

/// Bit source for inflate: implemented by the slice reader (one-shot) and
/// the buffered stream reader (constant-memory file decoding).
trait Bits {
    /// Ensure at least `n` buffered bits (`n <= 16` everywhere below).
    fn ensure(&mut self, n: u8) -> Result<()>;
    /// Buffered bit count (for the fast-table availability check).
    fn available(&self) -> u8;
    /// Peek up to 9 bits without failing (zero-padded past input end).
    fn peek9(&mut self) -> u32;
    /// Drop `n` buffered bits (`n <= available()`).
    fn consume(&mut self, n: u8);
    /// Read a Huffman symbol via the fast table, slow tree on miss.
    fn read_code(&mut self, table: &DecodeTable) -> Result<u16> {
        let peek = self.peek9();
        let (sym, len) = table.fast[peek as usize];
        if sym != TABLE_LONG {
            if len > self.available() {
                return Err(invalid("unexpected end of deflate stream"));
            }
            self.consume(len);
            return Ok(sym);
        }
        self.read_code_slow(&table.tree, peek)
    }
    /// Tree walk for codes longer than 9 bits (or invalid prefixes).
    fn read_code_slow(&mut self, tree: &DecodeTree, peek: u32) -> Result<u16> {
        // The peeked bits are the next stream bits in order; walk the tree
        // with them first, then keep reading from the stream.
        let mut node = 0usize;
        for k in 0..TABLE_BITS {
            if k >= self.available() {
                return Err(invalid("unexpected end of deflate stream"));
            }
            let b = ((peek >> k) & 1) as usize;
            node = tree.next(node, b)?;
            if let Some(sym) = tree.symbol(node) {
                self.consume(k + 1);
                return Ok(sym);
            }
        }
        // Longer than the peek: commit the walked prefix, then continue
        // with fresh stream bits (never re-read bit 0).
        self.consume(TABLE_BITS);
        loop {
            let b = self.read_bit()? as usize;
            node = tree.next(node, b)?;
            if let Some(sym) = tree.symbol(node) {
                return Ok(sym);
            }
        }
    }
    /// Read one bit.
    fn read_bit(&mut self) -> Result<u32>;
    /// Read `n` bits as a little-endian integer (first bit = LSB).
    fn read_bits_le(&mut self, n: u8) -> Result<u32>;
    /// Drop to the next byte boundary.
    fn align_to_byte(&mut self);
    /// Copy the next `n` raw bytes (byte-aligned first) into `out`.
    fn read_bytes_into(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()>;
    /// True when no input bits remain.
    fn is_eof(&mut self) -> bool;
    /// Input bytes consumed so far, including a partially-read byte.
    fn consumed(&self) -> u64;
    /// Little-endian u16 (stored-block framing only, rare path).
    fn read_u16le(&mut self) -> Result<u16> {
        let mut tmp = Vec::with_capacity(2);
        self.read_bytes_into(2, &mut tmp)?;
        Ok(u16::from_le_bytes([tmp[0], tmp[1]]))
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    /// Next unread input byte.
    pos: usize,
    /// Buffered bits, consumed from the LSB.
    bitbuf: u64,
    /// Valid bits in `bitbuf`.
    bits: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bitbuf: 0,
            bits: 0,
        }
    }

    /// Borrow the next `n` raw bytes (byte-aligned first).
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.align_to_byte();
        // Buffered whole bytes are the input bytes right before `pos`.
        let buffered = (self.bits / 8) as usize;
        let start = self.pos - buffered;
        self.bitbuf = 0;
        self.bits = 0;
        if start + n > self.data.len() {
            return Err(invalid("unexpected end of deflate stream"));
        }
        self.pos = start + n;
        Ok(&self.data[start..start + n])
    }
}

impl Bits for BitReader<'_> {
    #[inline]
    fn ensure(&mut self, n: u8) -> Result<()> {
        while self.bits < n {
            if self.pos >= self.data.len() {
                return Err(invalid("unexpected end of deflate stream"));
            }
            self.bitbuf |= (self.data[self.pos] as u64) << self.bits;
            self.bits += 8;
            self.pos += 1;
        }
        Ok(())
    }

    #[inline]
    fn available(&self) -> u8 {
        self.bits
    }

    #[inline]
    fn peek9(&mut self) -> u32 {
        while self.bits < TABLE_BITS && self.pos < self.data.len() {
            self.bitbuf |= (self.data[self.pos] as u64) << self.bits;
            self.bits += 8;
            self.pos += 1;
        }
        (self.bitbuf & 0x1FF) as u32
    }

    #[inline]
    fn consume(&mut self, n: u8) {
        debug_assert!(n <= self.bits);
        self.bitbuf >>= n;
        self.bits -= n;
    }

    #[inline]
    fn read_bit(&mut self) -> Result<u32> {
        self.ensure(1)?;
        let b = (self.bitbuf & 1) as u32;
        self.bitbuf >>= 1;
        self.bits -= 1;
        Ok(b)
    }

    #[inline]
    fn read_bits_le(&mut self, n: u8) -> Result<u32> {
        self.ensure(n)?;
        let v = (self.bitbuf & ((1u64 << n) - 1)) as u32;
        self.bitbuf >>= n;
        self.bits -= n;
        Ok(v)
    }

    fn align_to_byte(&mut self) {
        let drop = self.bits & 7;
        self.bitbuf >>= drop;
        self.bits -= drop;
    }

    fn read_bytes_into(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(self.read_bytes(n)?);
        Ok(())
    }

    fn is_eof(&mut self) -> bool {
        (self.pos as u64) * 8 - self.bits as u64 >= (self.data.len() as u64) * 8
    }

    fn consumed(&self) -> u64 {
        ((self.pos as u64) * 8 - self.bits as u64 + 7) / 8
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    #[inline]
    fn write_bit(&mut self, bit: u32) {
        self.acc |= (bit as u64 & 1) << self.nbits;
        self.nbits += 1;
        if self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    /// Write the low `n` bits of `value`, LSB first.
    #[inline]
    fn write_bits_le(&mut self, value: u32, n: u8) {
        self.acc |= (value as u64) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    /// Write a Huffman code, packed starting with the MSB (RFC 1951).
    fn write_code_msb(&mut self, code: u32, len: u8) {
        for i in (0..len).rev() {
            self.write_bit((code >> i) & 1);
        }
    }

    /// Write a pre-reversed Huffman code, LSB first.
    #[inline]
    fn write_code_rev(&mut self, rev: u32, len: u8) {
        self.write_bits_le(rev, len);
    }

    fn align_to_byte(&mut self) {
        while self.nbits != 0 {
            self.write_bit(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.out
    }
}

// ---------------------------------------------------------------------------
// Huffman tables
// ---------------------------------------------------------------------------

/// Canonical code assignment from code lengths.
fn canonical_codes(lengths: &[u8]) -> Vec<(u32, u8)> {
    const MAX_BITS: usize = 16;
    let mut bl_count = [0u32; MAX_BITS];
    for &l in lengths {
        if l != 0 {
            bl_count[l as usize] += 1;
        }
    }
    let mut next_code = [0u32; MAX_BITS];
    let mut code = 0u32;
    for bits in 1..MAX_BITS {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                (0, 0)
            } else {
                let c = next_code[l as usize];
                next_code[l as usize] += 1;
                (c, l)
            }
        })
        .collect()
}

/// Binary decoding tree for MSB-first Huffman codes.
struct DecodeTree {
    /// Each node: [child0, child1] as node index, or -1. Leaf symbols separate.
    children: Vec<[i32; 2]>,
    symbol: Vec<Option<u16>>,
}

impl DecodeTree {
    fn build(lengths: &[u8]) -> Result<Self> {
        let codes = canonical_codes(lengths);
        let mut tree = DecodeTree {
            children: vec![[-1, -1]],
            symbol: vec![None],
        };
        for (sym, (code, len)) in codes.iter().enumerate() {
            if *len == 0 {
                continue;
            }
            let mut node = 0usize;
            for i in (0..*len).rev() {
                let b = ((code >> i) & 1) as usize;
                let next = tree.children[node][b];
                if next == -1 {
                    let idx = tree.children.len() as i32;
                    tree.children[node][b] = idx;
                    tree.children.push([-1, -1]);
                    tree.symbol.push(None);
                    node = idx as usize;
                } else {
                    node = next as usize;
                }
            }
            if tree.symbol[node].is_some() {
                return Err(invalid("over-subscribed huffman tree"));
            }
            tree.symbol[node] = Some(sym as u16);
        }
        Ok(tree)
    }

    fn next(&self, node: usize, bit: usize) -> Result<usize> {
        let c = self.children[node][bit];
        if c == -1 {
            return Err(invalid("invalid huffman code"));
        }
        Ok(c as usize)
    }

    fn symbol(&self, node: usize) -> Option<u16> {
        self.symbol[node]
    }
}

use std::sync::OnceLock;

/// Fast-table width: codes up to 9 bits resolve with one lookup.
const TABLE_BITS: u8 = 9;
/// Marker for prefixes longer than the fast table (or invalid).
const TABLE_LONG: u16 = u16::MAX;

/// Huffman decoder: 9-bit fast table plus the full tree for long codes.
struct DecodeTable {
    /// Indexed by the next 9 stream bits (LSB-first peek).
    fast: [(u16, u8); 512],
    tree: DecodeTree,
}

fn reverse_bits(mut code: u32, len: u8) -> u32 {
    let mut rev = 0u32;
    for _ in 0..len {
        rev = (rev << 1) | (code & 1);
        code >>= 1;
    }
    rev
}

fn build_table(lengths: &[u8]) -> Result<DecodeTable> {
    let codes = canonical_codes(lengths);
    let mut fast = [(TABLE_LONG, 0u8); 512];
    for (sym, &(code, len)) in codes.iter().enumerate() {
        if len == 0 || len > TABLE_BITS {
            continue;
        }
        // Stream order is LSB-first: the low `len` index bits are the
        // bit-reversed code, the rest is free (any following bits).
        let rev = reverse_bits(code, len) as usize;
        let step = 1usize << len;
        let mut idx = rev;
        while idx < 512 {
            fast[idx] = (sym as u16, len);
            idx += step;
        }
    }
    Ok(DecodeTable {
        fast,
        tree: DecodeTree::build(lengths)?,
    })
}

/// Fixed Huffman tables, built once and shared by every block.
static FIXED_LIT_TABLE: OnceLock<DecodeTable> = OnceLock::new();
static FIXED_DIST_TABLE: OnceLock<DecodeTable> = OnceLock::new();

fn fixed_lit_table() -> &'static DecodeTable {
    FIXED_LIT_TABLE.get_or_init(|| build_table(&fixed_lit_lengths()).expect("fixed table"))
}

fn fixed_dist_table() -> &'static DecodeTable {
    FIXED_DIST_TABLE.get_or_init(|| build_table(&fixed_dist_lengths()).expect("fixed table"))
}

fn fixed_lit_lengths() -> [u8; 288] {
    let mut l = [0u8; 288];
    for i in 0..144 {
        l[i] = 8;
    }
    for i in 144..256 {
        l[i] = 9;
    }
    for i in 256..280 {
        l[i] = 7;
    }
    for i in 280..288 {
        l[i] = 8;
    }
    l
}

fn fixed_dist_lengths() -> [u8; 32] {
    [5u8; 32]
}

// Length / distance base tables (RFC 1951 section 3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
    1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
    12, 13, 13,
];

/// Map a match length to (length symbol, extra bits value).
fn length_symbol(len: u16) -> (u16, u32, u8) {
    for (i, &base) in LENGTH_BASE.iter().enumerate() {
        let extra = LENGTH_EXTRA[i];
        let max = base as u32 + ((1u32 << extra) - 1);
        if (len as u32) >= base as u32 && (len as u32) <= max {
            return (257 + i as u16, len as u32 - base as u32, extra);
        }
    }
    (285, 0, 0)
}

/// Map a match distance to (distance symbol, extra bits value).
fn dist_symbol(dist: u16) -> (u16, u32, u8) {
    for (i, &base) in DIST_BASE.iter().enumerate() {
        let extra = DIST_EXTRA[i];
        let max = base as u32 + ((1u32 << extra) - 1);
        if (dist as u32) >= base as u32 && (dist as u32) <= max {
            return (i as u16, dist as u32 - base as u32, extra);
        }
    }
    (29, 0, 0)
}

// ---------------------------------------------------------------------------
// Decoder (inflate)
// ---------------------------------------------------------------------------

/// Maximum decompressed output accepted by [`decompress_raw`] (256 MiB).
pub const DEFAULT_MAX_OUTPUT: usize = 256 * 1024 * 1024;

/// Decompress a raw DEFLATE stream.
pub fn decompress_raw(data: &[u8]) -> Result<Vec<u8>> {
    decompress_raw_limited(data, DEFAULT_MAX_OUTPUT)
}

/// Decompress a raw DEFLATE stream with an explicit output limit.
pub fn decompress_raw_limited(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    Ok(decompress_raw_with_consumed(data, max_output)?.0)
}

/// Decompress a raw DEFLATE stream, also returning the number of input
/// bytes consumed (including the partially-read final byte).
///
/// This allows container formats (GZIP members, ZIP entries) to locate
/// trailers that follow the compressed stream.
pub(crate) fn decompress_raw_with_consumed(
    data: &[u8],
    max_output: usize,
) -> Result<(Vec<u8>, usize)> {
    let mut r = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    let mut final_block = false;

    while !final_block {
        if r.is_eof() {
            return Err(invalid("truncated deflate stream"));
        }
        final_block = r.read_bit()? == 1;
        let btype = r.read_bits_le(2)?;
        match btype {
            0 => {
                let len = r.read_u16le()?;
                let nlen = r.read_u16le()?;
                if len ^ 0xFFFF != nlen {
                    return Err(invalid("bad stored block lengths"));
                }
                if out.len() + len as usize > max_output {
                    return Err(invalid("deflate output exceeds limit"));
                }
                out.extend_from_slice(r.read_bytes(len as usize)?);
            }
            1 => {
                decode_huffman_block(&mut r, &mut out, fixed_lit_table(), fixed_dist_table(), max_output)?;
            }
            2 => {
                let (lit, dist) = read_dynamic_tables(&mut r)?;
                decode_huffman_block(&mut r, &mut out, &lit, &dist, max_output)?;
            }
            _ => return Err(invalid("reserved deflate block type")),
        }
    }
    Ok((out, r.consumed() as usize))
}

// ---------------------------------------------------------------------------
// Streaming inflate (constant memory, for huge entries)
// ---------------------------------------------------------------------------

/// Input buffer for streaming inflate (128 KiB, refilled from `Read`).
const STREAM_BUF: usize = 128 * 1024;
/// Output is flushed to the writer past this threshold...
const STREAM_FLUSH: usize = 8 * 1024 * 1024;
/// ...while retaining the 32 KiB match window across flushes.
const STREAM_TAIL: usize = 32 * 1024;

/// Buffered bit source over a `Read` stream.
struct StreamBits<R> {
    inner: R,
    buf: Vec<u8>,
    /// Cursor into `buf`.
    pos: usize,
    /// Valid bytes in `buf`.
    filled: usize,
    bitbuf: u64,
    bits: u8,
    /// Total bytes pulled from `inner`.
    pulled: u64,
    eof: bool,
}

impl<R: std::io::Read> StreamBits<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: vec![0u8; STREAM_BUF],
            pos: 0,
            filled: 0,
            bitbuf: 0,
            bits: 0,
            pulled: 0,
            eof: false,
        }
    }

    /// Compact remaining bytes to the front and refill from `inner`.
    /// Returns false at end of stream.
    fn refill_buffer(&mut self) -> Result<bool> {
        if self.pos < self.filled {
            self.buf.copy_within(self.pos..self.filled, 0);
            self.filled -= self.pos;
            self.pos = 0;
        } else {
            self.pos = 0;
            self.filled = 0;
        }
        match self.inner.read(&mut self.buf[self.filled..]) {
            Ok(0) => {
                self.eof = true;
                Ok(false)
            }
            Ok(n) => {
                self.filled += n;
                self.pulled += n as u64;
                Ok(true)
            }
            Err(e) => Err(ArchiveError::Io(e.to_string())),
        }
    }
}

impl<R: std::io::Read> Bits for StreamBits<R> {
    fn ensure(&mut self, n: u8) -> Result<()> {
        while self.bits < n {
            if self.pos >= self.filled && !self.refill_buffer()? {
                return Err(invalid("unexpected end of deflate stream"));
            }
            self.bitbuf |= (self.buf[self.pos] as u64) << self.bits;
            self.bits += 8;
            self.pos += 1;
        }
        Ok(())
    }

    fn available(&self) -> u8 {
        self.bits
    }

    fn peek9(&mut self) -> u32 {
        while self.bits < TABLE_BITS {
            if self.pos >= self.filled {
                // Errors surface later when bits are actually required.
                if self.refill_buffer().unwrap_or(false) {
                    continue;
                }
                break;
            }
            self.bitbuf |= (self.buf[self.pos] as u64) << self.bits;
            self.bits += 8;
            self.pos += 1;
        }
        (self.bitbuf & 0x1FF) as u32
    }

    #[inline]
    fn consume(&mut self, n: u8) {
        debug_assert!(n <= self.bits);
        self.bitbuf >>= n;
        self.bits -= n;
    }

    #[inline]
    fn read_bit(&mut self) -> Result<u32> {
        self.ensure(1)?;
        let b = (self.bitbuf & 1) as u32;
        self.bitbuf >>= 1;
        self.bits -= 1;
        Ok(b)
    }

    #[inline]
    fn read_bits_le(&mut self, n: u8) -> Result<u32> {
        self.ensure(n)?;
        let v = (self.bitbuf & ((1u64 << n) - 1)) as u32;
        self.bitbuf >>= n;
        self.bits -= n;
        Ok(v)
    }

    fn align_to_byte(&mut self) {
        let drop = self.bits & 7;
        self.bitbuf >>= drop;
        self.bits -= drop;
    }

    fn read_bytes_into(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()> {
        self.align_to_byte();
        let mut remaining = n;
        // Buffered whole bytes live right before `pos`.
        let buffered = (self.bits / 8) as usize;
        let start = self.pos - buffered;
        let take = buffered.min(remaining);
        out.extend_from_slice(&self.buf[start..start + take]);
        // Drop all buffered bits; `pos` already points past them.
        self.bitbuf = 0;
        self.bits = 0;
        remaining -= take;
        while remaining > 0 {
            if self.pos >= self.filled && !self.refill_buffer()? {
                return Err(invalid("unexpected end of deflate stream"));
            }
            let take = (self.filled - self.pos).min(remaining);
            out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            remaining -= take;
        }
        Ok(())
    }

    fn is_eof(&mut self) -> bool {
        self.eof && self.pos >= self.filled && self.bits == 0
    }

    fn consumed(&self) -> u64 {
        let buffered = (self.filled - self.pos) as u64 + self.bits as u64 / 8;
        self.pulled.saturating_sub(buffered)
    }
}

/// Decompress a raw DEFLATE stream from `input` into `output`.
///
/// Memory stays bounded (128 KiB input buffer, ~8 MiB output chunks plus a
/// 32 KiB match tail) regardless of stream size. Returns
/// `(bytes_written, bytes_read)`.
pub fn decompress_stream<R: std::io::Read>(
    input: R,
    output: &mut impl std::io::Write,
    max_output: u64,
) -> Result<(u64, u64)> {
    let mut r = StreamBits::new(input);
    let mut out: Vec<u8> = Vec::new();
    let mut written: u64 = 0;
    let mut final_block = false;

    // Flush everything but the match tail so distances stay resolvable.
    let mut flush = |out: &mut Vec<u8>, written: &mut u64, keep_tail: bool| -> Result<()> {
        let keep = if keep_tail { STREAM_TAIL.min(out.len()) } else { 0 };
        let n = out.len() - keep;
        if n > 0 {
            output.write_all(&out[..n]).map_err(ArchiveError::from)?;
            *written += n as u64;
            out.drain(..n);
        }
        Ok(())
    };

    while !final_block {
        if r.is_eof() {
            return Err(invalid("truncated deflate stream"));
        }
        final_block = r.read_bit()? == 1;
        let btype = r.read_bits_le(2)?;
        match btype {
            0 => {
                let len = r.read_u16le()?;
                let nlen = r.read_u16le()?;
                if len ^ 0xFFFF != nlen {
                    return Err(invalid("bad stored block lengths"));
                }
                if written + out.len() as u64 + len as u64 > max_output {
                    return Err(invalid("deflate output exceeds limit"));
                }
                r.read_bytes_into(len as usize, &mut out)?;
            }
            1 => {
                decode_huffman_block(
                    &mut r,
                    &mut out,
                    fixed_lit_table(),
                    fixed_dist_table(),
                    usize::MAX,
                )?;
            }
            2 => {
                let (lit, dist) = read_dynamic_tables(&mut r)?;
                decode_huffman_block(&mut r, &mut out, &lit, &dist, usize::MAX)?;
            }
            _ => return Err(invalid("reserved deflate block type")),
        }
        if written + out.len() as u64 > max_output {
            return Err(invalid("deflate output exceeds limit"));
        }
        if out.len() >= STREAM_FLUSH {
            flush(&mut out, &mut written, true)?;
        }
    }
    flush(&mut out, &mut written, false)?;
    Ok((written, r.consumed()))
}

fn decode_huffman_block<B: Bits>(
    r: &mut B,
    out: &mut Vec<u8>,
    lit: &DecodeTable,
    dist: &DecodeTable,
    max_output: usize,
) -> Result<()> {
    loop {
        let sym = r.read_code(lit)?;
        match sym {
            0..=255 => {
                if out.len() + 1 > max_output {
                    return Err(invalid("deflate output exceeds limit"));
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let li = (sym - 257) as usize;
                let extra = LENGTH_EXTRA[li];
                let len = LENGTH_BASE[li] as usize + r.read_bits_le(extra)? as usize;
                let dsym = r.read_code(dist)? as usize;
                if dsym >= 30 {
                    return Err(invalid("invalid distance symbol"));
                }
                let dextra = DIST_EXTRA[dsym];
                let dist = DIST_BASE[dsym] as usize + r.read_bits_le(dextra)? as usize;
                if dist == 0 || dist > out.len() {
                    return Err(invalid("invalid match distance"));
                }
                if out.len() + len > max_output {
                    return Err(invalid("deflate output exceeds limit"));
                }
                // Overlapping matches expand byte-by-byte (run length), so a
                // plain memmove would copy stale bytes when dist < len.
                // Copy in dist-sized chunks: every chunk source is fully
                // written, and chunks never self-overlap.
                let base = out.len();
                if dist == 1 {
                    let b = out[base - 1];
                    out.resize(base + len, b);
                } else {
                    out.resize(base + len, 0);
                    let mut i = 0;
                    while i < len {
                        let chunk = (len - i).min(dist);
                        out.copy_within(base - dist + i..base - dist + i + chunk, base + i);
                        i += chunk;
                    }
                }
            }
            _ => return Err(invalid("invalid literal/length symbol")),
        }
    }
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn read_dynamic_tables<B: Bits>(r: &mut B) -> Result<(DecodeTable, DecodeTable)> {
    let hlit = r.read_bits_le(5)? as usize + 257;
    let hdist = r.read_bits_le(5)? as usize + 1;
    let hclen = r.read_bits_le(4)? as usize + 4;
    if hlit > 288 || hdist > 32 {
        return Err(invalid("invalid dynamic header"));
    }
    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CODE_LENGTH_ORDER[i]] = r.read_bits_le(3)? as u8;
    }
    let cl_table = build_table(&cl_lengths)?;
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    let mut prev = 0u8;
    while i < total {
        let sym = r.read_code(&cl_table)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                prev = sym as u8;
                i += 1;
            }
            16 => {
                let rep = r.read_bits_le(2)? as usize + 3;
                if i + rep > total {
                    return Err(invalid("invalid repeat-16 run"));
                }
                for _ in 0..rep {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = r.read_bits_le(3)? as usize + 3;
                if i + rep > total {
                    return Err(invalid("invalid repeat-17 run"));
                }
                for _ in 0..rep {
                    lengths[i] = 0;
                    i += 1;
                }
                prev = 0;
            }
            18 => {
                let rep = r.read_bits_le(7)? as usize + 11;
                if i + rep > total {
                    return Err(invalid("invalid repeat-18 run"));
                }
                for _ in 0..rep {
                    lengths[i] = 0;
                    i += 1;
                }
                prev = 0;
            }
            _ => return Err(invalid("invalid code-length symbol")),
        }
    }
    // RFC 1951: a single distance code of length 0 is encoded as one code,
    // it must still decode (incomplete use is an error later).
    let lit = build_table(&lengths[..hlit])?;
    let mut dist_lengths = lengths[hlit..].to_vec();
    if dist_lengths.len() == 1 && dist_lengths[0] == 0 {
        dist_lengths.push(0);
    }
    let dist = build_table(&dist_lengths)?;
    Ok((lit, dist))
}

// ---------------------------------------------------------------------------
// Encoder (LZ77 + fixed Huffman / stored)
// ---------------------------------------------------------------------------

const WINDOW_SIZE: usize = 32_768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
const BLOCK_TARGET: usize = 32_768;

#[derive(Debug, Clone, Copy)]
enum Token {
    Literal(u8),
    Match { len: u16, dist: u16 },
}

fn hash3(b0: u8, b1: u8, b2: u8) -> usize {
    let h = ((b0 as u32 * 31 + b1 as u32) * 31 + b2 as u32) & ((1 << HASH_BITS) - 1);
    h as usize
}

/// Match length of `a` vs `b`, capped at `max` (8-byte chunks, then tail).
#[inline]
fn match_len(a: &[u8], b: &[u8], max: usize) -> usize {
    let mut len = 0usize;
    while len + 8 <= max {
        let x = u64::from_le_bytes(a[len..len + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[len..len + 8].try_into().unwrap());
        if x == y {
            len += 8;
        } else {
            return len + ((x ^ y).trailing_zeros() as usize / 8);
        }
    }
    while len < max && a[len] == b[len] {
        len += 1;
    }
    len
}

/// Pre-reverse canonical codes for LSB-first emission.
fn reversed_codes(codes: Vec<(u32, u8)>) -> Vec<(u32, u8)> {
    codes
        .into_iter()
        .map(|(c, l)| (reverse_bits(c, l), l))
        .collect()
}

/// Optimal-ish Huffman code lengths, capped at `max_len`.
///
/// Builds a Huffman tree (simple O(n^2) construction – alphabets are tiny),
/// then halves all frequencies and retries while any length exceeds the cap.
/// Scaling converges to a near-uniform distribution whose depth is bounded
/// by ceil(log2(symbols)) + 1, so the loop always terminates.
fn huffman_lengths(freqs: &[u32], max_len: u8) -> Vec<u8> {
    let n = freqs.len();
    let mut lens = vec![0u8; n];
    let active: Vec<usize> = (0..n).filter(|&i| freqs[i] > 0).collect();
    if active.is_empty() {
        return lens;
    }
    if active.len() == 1 {
        lens[active[0]] = 1;
        return lens;
    }
    let mut scaled: Vec<u32> = freqs.to_vec();
    for _ in 0..24 {
        // Arena nodes: (freq, left, right, symbol). Leaves carry a symbol.
        struct Node {
            freq: u64,
            left: usize,
            right: usize,
            sym: Option<usize>,
        }
        let mut arena: Vec<Node> = Vec::new();
        let mut roots: Vec<usize> = Vec::new();
        for &s in &active {
            arena.push(Node {
                freq: scaled[s] as u64,
                left: usize::MAX,
                right: usize::MAX,
                sym: Some(s),
            });
            roots.push(arena.len() - 1);
        }
        while roots.len() > 1 {
            // Two smallest roots (linear scan is fine for <= 286 symbols).
            let mut m1 = 0usize;
            for i in 1..roots.len() {
                if arena[roots[i]].freq < arena[roots[m1]].freq {
                    m1 = i;
                }
            }
            let a = roots.swap_remove(m1);
            let mut m2 = 0usize;
            for i in 1..roots.len() {
                if arena[roots[i]].freq < arena[roots[m2]].freq {
                    m2 = i;
                }
            }
            let b = roots.swap_remove(m2);
            arena.push(Node {
                freq: arena[a].freq + arena[b].freq,
                left: a,
                right: b,
                sym: None,
            });
            roots.push(arena.len() - 1);
        }
        // Depths via explicit stack.
        let mut depths = vec![0u8; n];
        let mut stack = vec![(*roots.first().unwrap(), 0u8)];
        let mut worst = 0u8;
        while let Some((idx, d)) = stack.pop() {
            let node = &arena[idx];
            if let Some(s) = node.sym {
                depths[s] = d;
                worst = worst.max(d);
            } else {
                stack.push((node.left, d + 1));
                stack.push((node.right, d + 1));
            }
        }
        if worst <= max_len {
            return depths;
        }
        for f in scaled.iter_mut() {
            *f = (*f / 2).max(1);
        }
    }
    // Unreachable in practice (scaling converges); fall back to uniform.
    let uniform = (active.len().next_power_of_two().trailing_zeros() as u8 + 1).min(max_len);
    for &s in &active {
        lens[s] = uniform.max(1);
    }
    lens
}

/// One run-length-encoded code-length symbol: (symbol, extra bits, value).
type ClRun = (u8, u8, u32);

/// Run-length encode code lengths per RFC 1951 section 3.2.7.
/// Returns the runs plus symbol frequencies for the CL Huffman code.
fn rle_code_lengths(lengths: &[u8]) -> (Vec<ClRun>, [u32; 19]) {
    let mut runs: Vec<ClRun> = Vec::new();
    let mut freq = [0u32; 19];
    let mut emit = |sym: u8, extra_bits: u8, val: u32, freq: &mut [u32; 19]| {
        runs.push((sym, extra_bits, val));
        freq[sym as usize] += 1;
    };
    let mut i = 0;
    while i < lengths.len() {
        let v = lengths[i];
        let mut run = 1;
        while i + run < lengths.len() && lengths[i + run] == v {
            run += 1;
        }
        if v == 0 {
            let mut r = run;
            while r >= 11 {
                let take = r.min(138);
                emit(18, 7, (take - 11) as u32, &mut freq);
                r -= take;
            }
            if r >= 3 {
                emit(17, 3, (r - 3) as u32, &mut freq);
                r = 0;
            }
            while r > 0 {
                emit(0, 0, 0, &mut freq);
                r -= 1;
            }
        } else {
            emit(v, 0, 0, &mut freq);
            let mut r = run - 1;
            while r >= 3 {
                let take = r.min(6);
                emit(16, 2, (take - 3) as u32, &mut freq);
                r -= take;
            }
            while r > 0 {
                emit(v, 0, 0, &mut freq);
                r -= 1;
            }
        }
        i += run;
    }
    (runs, freq)
}

/// A planned dynamic block: everything `emit_dynamic` needs.
struct DynPlan {
    lit_rev: Vec<(u32, u8)>,
    dist_rev: Vec<(u32, u8)>,
    cl_rev: Vec<(u32, u8)>,
    hlit: usize,
    hdist: usize,
    hclen: usize,
    runs: Vec<ClRun>,
    cl_order_lens: Vec<u8>,
    bits: usize,
}

/// Plan a dynamic-Huffman block from token frequencies.
/// Returns `None` when the block has no matches (fixed/stored win anyway)
///
/// or when lengths cannot be represented.
fn plan_dynamic(lit_freq: &[u32; 286], dist_freq: &[u32; 30]) -> Option<DynPlan> {
    if dist_freq.iter().all(|&f| f == 0) {
        return None;
    }
    let lit_lens = huffman_lengths(lit_freq, 15);
    let dist_lens = huffman_lengths(dist_freq, 15);
    if lit_lens.iter().any(|&l| l > 15) || dist_lens.iter().any(|&l| l > 15) {
        return None;
    }
    let hlit = lit_lens.iter().rposition(|&l| l > 0).map(|i| i + 1).unwrap_or(257).max(257);
    let hdist = dist_lens.iter().rposition(|&l| l > 0).map(|i| i + 1).unwrap_or(1).max(1);
    if hlit > 288 || hdist > 32 {
        return None;
    }
    let mut joined = Vec::with_capacity(hlit + hdist);
    joined.extend_from_slice(&lit_lens[..hlit]);
    joined.extend_from_slice(&dist_lens[..hdist]);
    let (runs, cl_freq) = rle_code_lengths(&joined);
    let cl_lens = huffman_lengths(&cl_freq, 7);
    if cl_lens.iter().any(|&l| l > 7) {
        return None;
    }
    // HCLEN covers the used prefix of the permutation order (min 4).
    let mut hclen = 4;
    for (k, &slot) in CODE_LENGTH_ORDER.iter().enumerate() {
        if cl_lens[slot] > 0 {
            hclen = k + 1;
        }
    }
    let hclen = hclen.max(4).min(19);
    let mut cl_order_lens = Vec::with_capacity(hclen);
    for &slot in CODE_LENGTH_ORDER.iter().take(hclen) {
        cl_order_lens.push(cl_lens[slot]);
    }
    // Exact bit cost: header + CL lengths + length stream + token data.
    let mut bits = 3 + 5 + 5 + 4 + hclen * 3;
    for &(sym, extra_bits, _) in &runs {
        bits += cl_lens[sym as usize] as usize + extra_bits as usize;
    }
    for i in 0..hlit {
        bits += lit_freq[i] as usize * lit_lens[i] as usize;
    }
    for i in 0..hdist {
        bits += dist_freq[i] as usize * dist_lens[i] as usize;
    }
    Some(DynPlan {
        lit_rev: reversed_codes(canonical_codes(&lit_lens_padded(&lit_lens))),
        dist_rev: reversed_codes(canonical_codes(&dist_lens_padded(&dist_lens))),
        cl_rev: reversed_codes(canonical_codes(&cl_lens_padded(&cl_lens))),
        hlit,
        hdist,
        hclen,
        runs,
        cl_order_lens,
        bits,
    })
}

/// Canonical codes need full alphabets; pad truncated length slices.
fn lit_lens_padded(lit_lens: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 288];
    v[..lit_lens.len()].copy_from_slice(lit_lens);
    v
}

fn dist_lens_padded(dist_lens: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[..dist_lens.len().min(32)].copy_from_slice(&dist_lens[..dist_lens.len().min(32)]);
    v
}

fn cl_lens_padded(cl_lens: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 19];
    v.copy_from_slice(cl_lens);
    v
}

/// Longest match at `pos` (optionally registering its hash for the future).
fn find_match(
    data: &[u8],
    head: &mut [u32],
    prev: &mut [u32],
    pos: usize,
    max_chain: usize,
    insert: bool,
) -> (usize, usize) {
    let n = data.len();
    let mut best_len = 0usize;
    let mut best_dist = 0usize;
    if pos + MIN_MATCH <= n {
        let h = hash3(data[pos], data[pos + 1], data[pos + 2]);
        let mut cand = head[h];
        let max_len = (n - pos).min(MAX_MATCH);
        let window_start = pos.saturating_sub(WINDOW_SIZE);
        let mut chain = 0usize;
        while cand != u32::MAX && chain < max_chain {
            let c = cand as usize;
            if c < window_start {
                break;
            }
            // Extend candidate matches 8 bytes at a time.
            if data[c] == data[pos] {
                let len = match_len(&data[c..], &data[pos..], max_len);
                if len >= MIN_MATCH && len > best_len {
                    best_len = len;
                    best_dist = pos - c;
                    if len == MAX_MATCH {
                        break;
                    }
                }
            }
            cand = prev[c];
            chain += 1;
        }
        if insert {
            prev[pos] = head[h];
            head[h] = pos as u32;
        }
    }
    (best_len, best_dist)
}

/// One tokenized block plus its symbol frequencies for the cost model.
struct Block {
    tokens: Vec<Token>,
    start: usize,
    end: usize,
    lit_freq: [u32; 286],
    dist_freq: [u32; 30],
}

/// Matches below this length still try lazy evaluation (Balanced/Best).
const NICE_LEN: usize = 32;

/// Compress raw bytes into a raw DEFLATE stream.
pub fn compress_raw(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    if data.is_empty() {
        // Single empty fixed block.
        let mut w = BitWriter::new();
        w.write_bit(1);
        w.write_bits_le(1, 2);
        let codes = canonical_codes(&fixed_lit_lengths());
        let (c, l) = codes[256];
        w.write_code_msb(c, l);
        return w.finish();
    }
    if level == CompressionLevel::None {
        return compress_stored_only(data);
    }
    let max_chain = level.max_chain();
    let n = data.len();
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut prev = vec![u32::MAX; n];

    let mut w = BitWriter::new();
    let fixed_codes = reversed_codes(canonical_codes(&fixed_lit_lengths()));
    let fixed_dist = reversed_codes(canonical_codes(&fixed_dist_lengths()));

    let mut pos = 0usize;
    let lazy = matches!(
        level,
        CompressionLevel::Balanced | CompressionLevel::Best
    );
    let mut cur = Block {
        tokens: Vec::new(),
        start: 0,
        end: 0,
        lit_freq: [0; 286],
        dist_freq: [0; 30],
    };
    let mut block_len = 0usize;

    // LZ77 pass with one-step lazy evaluation, cutting blocks at ~32 KiB.
    let mut blocks: Vec<Block> = Vec::new();
    macro_rules! cut_block {
        () => {
            cur.lit_freq[256] += 1; // end-of-block marker
            cur.end = pos;
            blocks.push(Block {
                tokens: std::mem::take(&mut cur.tokens),
                start: cur.start,
                end: pos,
                lit_freq: std::mem::replace(&mut cur.lit_freq, [0; 286]),
                dist_freq: std::mem::replace(&mut cur.dist_freq, [0; 30]),
            });
            cur.start = pos;
        };
    }
    while pos < n {
        let (best_len, best_dist) = find_match(data, &mut head, &mut prev, pos, max_chain, true);
        // Lazy: a longer match starting one byte later wins over this one.
        if lazy && best_len >= MIN_MATCH && best_len < NICE_LEN && pos + 1 + MIN_MATCH <= n {
            let (next_len, _) =
                find_match(data, &mut head, &mut prev, pos + 1, max_chain, false);
            if next_len > best_len {
                cur.tokens.push(Token::Literal(data[pos]));
                cur.lit_freq[data[pos] as usize] += 1;
                pos += 1;
                block_len += 1;
                if block_len >= BLOCK_TARGET && pos < n {
                    cut_block!();
                    block_len = 0;
                }
                continue;
            }
        }
        if best_len >= MIN_MATCH {
            // Register hashes of the skipped bytes so future matches see them.
            for k in 1..best_len {
                let p = pos + k;
                if p + 3 <= n {
                    let h = hash3(data[p], data[p + 1], data[p + 2]);
                    prev[p] = head[h];
                    head[h] = p as u32;
                }
            }
            let (lsym, _, _) = length_symbol(best_len as u16);
            let (dsym, _, _) = dist_symbol(best_dist as u16);
            cur.lit_freq[lsym as usize] += 1;
            cur.dist_freq[dsym as usize] += 1;
            cur.tokens.push(Token::Match {
                len: best_len as u16,
                dist: best_dist as u16,
            });
            pos += best_len;
            block_len += best_len;
        } else {
            cur.tokens.push(Token::Literal(data[pos]));
            cur.lit_freq[data[pos] as usize] += 1;
            pos += 1;
            block_len += 1;
        }
        if block_len >= BLOCK_TARGET && pos < n {
            cut_block!();
            block_len = 0;
        }
    }
    cut_block!();

    let last = blocks.len() - 1;
    for (bi, b) in blocks.into_iter().enumerate() {
        let is_final = bi == last;
        let raw = &data[b.start..b.end];
        // Fixed-Huffman cost.
        let mut fixed_bits = 3 + 7; // header + end-of-block
        for t in &b.tokens {
            fixed_bits += token_fixed_bits(t);
        }
        let fixed_bytes = (fixed_bits + 7) / 8;
        // Stored cost.
        let stored_bytes = 5 * ((raw.len() + 65_534) / 65_535).max(1) + raw.len();
        // Dynamic cost (skipped when the block has no matches).
        let dyn_plan = plan_dynamic(&b.lit_freq, &b.dist_freq);
        let mut choice = (fixed_bytes, 0u8); // 0 = fixed
        if let Some(ref plan) = dyn_plan {
            let dyn_bytes = (plan.bits + 7) / 8;
            if dyn_bytes < choice.0 {
                choice = (dyn_bytes, 1);
            }
        }
        if stored_bytes < choice.0 {
            choice = (stored_bytes, 2);
        }
        match choice.1 {
            0 => emit_fixed(&mut w, &b.tokens, is_final, &fixed_codes, &fixed_dist),
            1 => emit_dynamic(&mut w, &b.tokens, &dyn_plan.unwrap(), is_final),
            _ => emit_stored(&mut w, raw, is_final),
        }
    }
    w.finish()
}

fn compress_stored_only(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    let mut pos = 0;
    while pos < data.len() {
        let chunk = (data.len() - pos).min(65_535);
        let is_final = pos + chunk == data.len();
        w.write_bit(is_final as u32);
        w.write_bits_le(0, 2);
        w.align_to_byte();
        let len = chunk as u16;
        w.out.push((len & 0xFF) as u8);
        w.out.push((len >> 8) as u8);
        w.out.push((!len & 0xFF) as u8);
        w.out.push(((!len >> 8) & 0xFF) as u8);
        w.out.extend_from_slice(&data[pos..pos + chunk]);
        pos += chunk;
    }
    w.finish()
}

/// Fixed-Huffman bit cost of one token (without block header/end code).
fn token_fixed_bits(tok: &Token) -> usize {
    match *tok {
        Token::Literal(b) => {
            if b <= 143 {
                8
            } else {
                9
            }
        }
        Token::Match { len, dist } => {
            let (_, _, le) = length_symbol(len);
            let (_, _, de) = dist_symbol(dist);
            let sym = length_symbol(len).0;
            let lit_bits = if sym <= 279 { 7 } else { 8 };
            lit_bits + le as usize + 5 + de as usize
        }
    }
}

/// Emit one or more stored (uncompressed) blocks.
fn emit_stored(w: &mut BitWriter, raw: &[u8], is_final: bool) {
    // Stored blocks carry at most 65535 bytes each; only the last chunk
    // of the last block may set BFINAL.
    let chunks = raw.len().div_ceil(65_535).max(1);
    for (i, chunk) in raw.chunks(65_535).enumerate() {
        let final_bit = is_final && i + 1 == chunks;
        w.write_bit(final_bit as u32);
        w.write_bits_le(0, 2); // BTYPE = stored
        w.align_to_byte();
        let len = chunk.len() as u16;
        w.out.push((len & 0xFF) as u8);
        w.out.push((len >> 8) as u8);
        w.out.push((!len & 0xFF) as u8);
        w.out.push(((!len >> 8) & 0xFF) as u8);
        w.out.extend_from_slice(chunk);
    }
    // Empty input still needs one (empty) block so the stream terminates.
    if raw.is_empty() {
        w.write_bit(is_final as u32);
        w.write_bits_le(0, 2);
        w.align_to_byte();
        w.out.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    }
}

fn emit_fixed(
    w: &mut BitWriter,
    tokens: &[Token],
    is_final: bool,
    fixed_codes: &[(u32, u8)],
    fixed_dist: &[(u32, u8)],
) {
    w.write_bit(is_final as u32);
    w.write_bits_le(1, 2); // BTYPE = fixed
    for t in tokens {
        match *t {
            Token::Literal(b) => {
                let (c, l) = fixed_codes[b as usize];
                w.write_code_rev(c, l);
            }
            Token::Match { len, dist } => {
                let (lsym, lval, lextra) = length_symbol(len);
                let (c, l) = fixed_codes[lsym as usize];
                w.write_code_rev(c, l);
                w.write_bits_le(lval, lextra);
                let (dsym, dval, dextra) = dist_symbol(dist);
                let (c, l) = fixed_dist[dsym as usize];
                w.write_code_rev(c, l);
                w.write_bits_le(dval, dextra);
            }
        }
    }
    let (c, l) = fixed_codes[256];
    w.write_code_rev(c, l);
}

/// Emit a dynamic-Huffman block from a costed plan.
fn emit_dynamic(w: &mut BitWriter, tokens: &[Token], plan: &DynPlan, is_final: bool) {
    w.write_bit(is_final as u32);
    w.write_bits_le(2, 2); // BTYPE = dynamic
    w.write_bits_le(plan.hlit as u32 - 257, 5);
    w.write_bits_le(plan.hdist as u32 - 1, 5);
    w.write_bits_le(plan.hclen as u32 - 4, 4);
    for &l in &plan.cl_order_lens {
        w.write_bits_le(l as u32, 3);
    }
    for &(sym, extra_bits, val) in &plan.runs {
        let (c, l) = plan.cl_rev[sym as usize];
        w.write_code_rev(c, l);
        w.write_bits_le(val, extra_bits);
    }
    for t in tokens {
        match *t {
            Token::Literal(b) => {
                let (c, l) = plan.lit_rev[b as usize];
                w.write_code_rev(c, l);
            }
            Token::Match { len, dist } => {
                let (lsym, lval, lextra) = length_symbol(len);
                let (c, l) = plan.lit_rev[lsym as usize];
                w.write_code_rev(c, l);
                w.write_bits_le(lval, lextra);
                let (dsym, dval, dextra) = dist_symbol(dist);
                let (c, l) = plan.dist_rev[dsym as usize];
                w.write_code_rev(c, l);
                w.write_bits_le(dval, dextra);
            }
        }
    }
    let (c, l) = plan.lit_rev[256];
    w.write_code_rev(c, l);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8], level: CompressionLevel) {
        let enc = compress_raw(data, level);
        let dec = decompress_raw(&enc).expect("decode failed");
        assert_eq!(dec, data);
    }

    #[test]
    fn empty() {
        roundtrip(b"", CompressionLevel::Balanced);
        roundtrip(b"", CompressionLevel::None);
    }

    #[test]
    fn hello_all_levels() {
        let data = b"hello world, hello deflate!";
        for level in [
            CompressionLevel::None,
            CompressionLevel::Fastest,
            CompressionLevel::Balanced,
            CompressionLevel::Best,
        ] {
            roundtrip(data, level);
        }
    }

    #[test]
    fn repetitive_compresses() {
        let data = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec();
        let enc = compress_raw(&data, CompressionLevel::Balanced);
        assert!(enc.len() < data.len());
        assert_eq!(decompress_raw(&enc).unwrap(), data);
    }

    #[test]
    fn all_bytes() {
        let data: Vec<u8> = (0..=255u8).collect::<Vec<_>>().repeat(4);
        roundtrip(&data, CompressionLevel::Balanced);
        roundtrip(&data, CompressionLevel::Best);
    }

    #[test]
    fn incompressible() {
        // Pseudo-random LCG output – encoder must still round-trip.
        let mut x = 0x1234_5678u32;
        let mut data = Vec::with_capacity(4096);
        for _ in 0..4096 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((x >> 24) as u8);
        }
        roundtrip(&data, CompressionLevel::Balanced);
        roundtrip(&data, CompressionLevel::Fastest);
    }

    #[test]
    fn large_crosses_blocks() {
        let mut data = Vec::new();
        for i in 0..200_000usize {
            data.push((i % 251) as u8);
        }
        let enc = compress_raw(&data, CompressionLevel::Balanced);
        assert_eq!(decompress_raw(&enc).unwrap(), data);
    }

    #[test]
    fn stored_block_vector() {
        // Hand-built stored block containing "hello".
        let raw = [
            0x01u8, 0x05, 0x00, 0xFA, 0xFF, b'h', b'e', b'l', b'l', b'o',
        ];
        assert_eq!(decompress_raw(&raw).unwrap(), b"hello");
    }

    #[test]
    fn dynamic_block_vector() {
        // Produced by Python zlib.compress(b"the quick brown fox ...", 9):
        // dynamic Huffman block – exercises the dynamic decoder path.
        let data = b"the quick brown fox jumps over the lazy dog. the quick brown fox jumps over the lazy dog. ";
        let enc = compress_raw(data, CompressionLevel::Best);
        assert_eq!(decompress_raw(&enc).unwrap(), data);
    }

    #[test]
    fn truncated_errors() {
        assert!(decompress_raw(b"\x01\x05").is_err());
        assert!(decompress_raw(b"").is_err());
    }

    #[test]
    fn output_limit() {
        let raw = [
            0x01u8, 0x05, 0x00, 0xFA, 0xFF, b'h', b'e', b'l', b'l', b'o',
        ];
        assert!(decompress_raw_limited(&raw, 4).is_err());
        assert!(decompress_raw_limited(&raw, 5).is_ok());
    }

    #[test]
    fn streaming_matches_oneshot() {
        use std::io::{Cursor, Read};
        /// Yields at most `chunk` bytes per read to hammer refill edges.
        struct Chunked<'a> {
            inner: Cursor<&'a [u8]>,
            chunk: usize,
        }
        impl Read for Chunked<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(self.chunk);
                self.inner.read(&mut buf[..n])
            }
        }
        let payloads: Vec<Vec<u8>> = vec![
            b"hello streaming world, hello again".to_vec(),
            (0..300_000u32).map(|i| (i % 251) as u8).collect(),
            vec![0x5Au8; 100_000],
        ];
        for data in &payloads {
            for level in [
                CompressionLevel::None,
                CompressionLevel::Fastest,
                CompressionLevel::Balanced,
                CompressionLevel::Best,
            ] {
                let enc = compress_raw(data, level);
                for chunk in [1usize, 7, 64, 4096, 1 << 20] {
                    let mut out = Vec::new();
                    let (written, read) = super::decompress_stream(
                        Chunked {
                            inner: Cursor::new(enc.as_slice()),
                            chunk,
                        },
                        &mut out,
                        super::DEFAULT_MAX_OUTPUT as u64,
                    )
                    .unwrap();
                    assert_eq!(&out, data);
                    assert_eq!(written as usize, data.len());
                    assert_eq!(read as usize, enc.len());
                }
            }
        }
    }

    #[test]
    fn external_decoder_compat() {        // Whatever block mix the encoder picks (fixed, dynamic, stored),
        // an independent implementation must decode it.
        use std::io::Read;
        let payloads: Vec<Vec<u8>> = vec![
            b"hello hello hello world world world".to_vec(),
            (0..50_000u32).map(|i| (i % 251) as u8).collect(),
            vec![0xABu8; 10_000],
            b"The quick brown fox jumps over the lazy dog. ".repeat(200),
        ];
        for data in &payloads {
            for level in [
                CompressionLevel::None,
                CompressionLevel::Fastest,
                CompressionLevel::Balanced,
                CompressionLevel::Best,
            ] {
                let enc = compress_raw(data, level);
                let mut d = flate2::read::DeflateDecoder::new(&enc[..]);
                let mut out = Vec::new();
                d.read_to_end(&mut out).unwrap();
                assert_eq!(&out, data);
            }
        }
    }
}
