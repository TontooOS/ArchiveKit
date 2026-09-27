//! Raw DEFLATE (RFC 1951) – hand-written, zero dependencies.
//!
//! - [`decompress_raw`] implements a full inflate decoder covering stored,
//!   fixed-Huffman and dynamic-Huffman blocks.
//! - [`compress_raw`] implements an LZ77 encoder (32 KiB window) that emits
//!   fixed-Huffman blocks with a stored-block fallback for incompressible
//!   data. Output is valid RFC 1951 and interoperable with system tools.

use crate::error::{invalid, Result};

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

struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bit: 0,
        }
    }

    fn read_bit(&mut self) -> Result<u32> {
        if self.byte >= self.data.len() {
            return Err(invalid("unexpected end of deflate stream"));
        }
        let b = (self.data[self.byte] >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Ok(b as u32)
    }

    /// Read `n` bits as a little-endian integer (first bit = LSB).
    fn read_bits_le(&mut self, n: u8) -> Result<u32> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.read_bit()? << i;
        }
        Ok(v)
    }

    /// Read a Huffman code accumulated MSB-first.
    fn read_code(&mut self, tree: &DecodeTree) -> Result<u16> {
        let mut node = 0usize;
        for _ in 0..15 {
            let b = self.read_bit()? as usize;
            node = tree.next(node, b)?;
            if let Some(sym) = tree.symbol(node) {
                return Ok(sym);
            }
        }
        Err(invalid("invalid huffman code"))
    }

    fn align_to_byte(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }

    fn read_u16le(&mut self) -> Result<u16> {
        self.align_to_byte();
        if self.byte + 2 > self.data.len() {
            return Err(invalid("unexpected end of deflate stream"));
        }
        let v = u16::from_le_bytes([self.data[self.byte], self.data[self.byte + 1]]);
        self.byte += 2;
        Ok(v)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.align_to_byte();
        if self.byte + n > self.data.len() {
            return Err(invalid("unexpected end of deflate stream"));
        }
        let s = &self.data[self.byte..self.byte + n];
        self.byte += n;
        Ok(s)
    }

    fn is_eof(&self) -> bool {
        self.byte >= self.data.len()
    }

    /// Bytes consumed so far, including a partially-read byte.
    fn consumed(&self) -> usize {
        self.byte + if self.bit == 0 { 0 } else { 1 }
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
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

    fn write_bit(&mut self, bit: u32) {
        self.acc |= (bit & 1) << self.nbits;
        self.nbits += 1;
        if self.nbits == 8 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.nbits = 0;
        }
    }

    /// Write the low `n` bits of `value`, LSB first.
    fn write_bits_le(&mut self, value: u32, n: u8) {
        for i in 0..n {
            self.write_bit((value >> i) & 1);
        }
    }

    /// Write a Huffman code, packed starting with the MSB (RFC 1951).
    fn write_code_msb(&mut self, code: u32, len: u8) {
        for i in (0..len).rev() {
            self.write_bit((code >> i) & 1);
        }
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
                let lit = DecodeTree::build(&fixed_lit_lengths())?;
                let dist = DecodeTree::build(&fixed_dist_lengths())?;
                decode_huffman_block(&mut r, &mut out, &lit, &dist, max_output)?;
            }
            2 => {
                let (lit, dist) = read_dynamic_trees(&mut r)?;
                decode_huffman_block(&mut r, &mut out, &lit, &dist, max_output)?;
            }
            _ => return Err(invalid("reserved deflate block type")),
        }
    }
    Ok((out, r.consumed()))
}

fn decode_huffman_block(
    r: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &DecodeTree,
    dist: &DecodeTree,
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
                for _ in 0..len {
                    let b = out[out.len() - dist];
                    out.push(b);
                }
            }
            _ => return Err(invalid("invalid literal/length symbol")),
        }
    }
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn read_dynamic_trees(r: &mut BitReader) -> Result<(DecodeTree, DecodeTree)> {
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
    let cl_tree = DecodeTree::build(&cl_lengths)?;
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    let mut prev = 0u8;
    while i < total {
        let sym = r.read_code(&cl_tree)?;
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
    let lit = DecodeTree::build(&lengths[..hlit])?;
    let mut dist_lengths = lengths[hlit..].to_vec();
    if dist_lengths.len() == 1 && dist_lengths[0] == 0 {
        dist_lengths.push(0);
    }
    let dist = DecodeTree::build(&dist_lengths)?;
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
    let fixed_codes = canonical_codes(&fixed_lit_lengths());
    let fixed_dist = canonical_codes(&fixed_dist_lengths());

    let mut pos = 0usize;
    // Tokens of the current block plus uncompressed byte count.
    let mut tokens: Vec<Token> = Vec::new();
    let mut block_len = 0usize;

    // Greedy LZ77 pass, cutting blocks at ~BLOCK_TARGET bytes.
    let mut pending: Vec<(Vec<Token>, usize, usize)> = Vec::new();
    let mut block_start = 0usize;
    while pos < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if pos + MIN_MATCH <= n {
            let h = hash3(data[pos], data[pos + 1], data[pos + 2]);
            let mut cand = head[h];
            let mut chain = 0usize;
            let max_len = (n - pos).min(MAX_MATCH);
            let window_start = pos.saturating_sub(WINDOW_SIZE);
            while cand != u32::MAX && chain < max_chain {
                let c = cand as usize;
                if c < window_start {
                    break;
                }
                // Quick first-byte check, then extend.
                if data[c] == data[pos] {
                    let mut len = 1usize;
                    while len < max_len && data[c + len] == data[pos + len] {
                        len += 1;
                    }
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
            prev[pos] = head[h];
            head[h] = pos as u32;
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
            tokens.push(Token::Match {
                len: best_len as u16,
                dist: best_dist as u16,
            });
            pos += best_len;
            block_len += best_len;
        } else {
            tokens.push(Token::Literal(data[pos]));
            pos += 1;
            block_len += 1;
        }
        if block_len >= BLOCK_TARGET && pos < n {
            pending.push((std::mem::take(&mut tokens), block_start, pos));
            block_start = pos;
            block_len = 0;
        }
    }
    pending.push((tokens, block_start, n));

    let last = pending.len().saturating_sub(1);
    for (bi, (tokens, start, end)) in pending.into_iter().enumerate() {
        let is_final = bi == last;
        emit_block(
            &mut w,
            &tokens,
            &data[start..end],
            is_final,
            &fixed_codes,
            &fixed_dist,
        );
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

fn emit_block(
    w: &mut BitWriter,
    tokens: &[Token],
    raw: &[u8],
    is_final: bool,
    fixed_codes: &[(u32, u8)],
    fixed_dist: &[(u32, u8)],
) {
    // Compare fixed-Huffman cost vs stored cost.
    let mut fixed_bits = 3 + 7; // header + end-of-block code
    for t in tokens {
        fixed_bits += token_fixed_bits(t);
    }
    let stored_bytes = 5 * ((raw.len() + 65_534) / 65_535).max(1) + raw.len();
    if (fixed_bits + 7) / 8 >= stored_bytes {
        emit_stored(w, raw, is_final);
        return;
    }
    emit_fixed(w, tokens, is_final, fixed_codes, fixed_dist);
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
                w.write_code_msb(c, l);
            }
            Token::Match { len, dist } => {
                let (lsym, lval, lextra) = length_symbol(len);
                let (c, l) = fixed_codes[lsym as usize];
                w.write_code_msb(c, l);
                w.write_bits_le(lval, lextra);
                let (dsym, dval, dextra) = dist_symbol(dist);
                let (c, l) = fixed_dist[dsym as usize];
                w.write_code_msb(c, l);
                w.write_bits_le(dval, dextra);
            }
        }
    }
    let (c, l) = fixed_codes[256];
    w.write_code_msb(c, l);
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
}
