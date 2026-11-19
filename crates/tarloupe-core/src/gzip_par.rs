//! Parallel first pass over gzip files, after the idea of pugz and
//! rapidgzip.
//!
//! The compressed file is cut into chunks. A worker finds the first deflate
//! block that starts in its chunk (by checking every bit position for a
//! plausible block header) and decodes from there until the first block
//! boundary past the chunk's end, where the next chunk's block starts.
//!
//! A block in the middle of a stream may copy from the 32 KiB of output
//! before it, which the worker does not know yet. It therefore starts three
//! zlib decoders with made-up windows that differ at every position: one
//! window holds each position's low byte, the other two its high byte with
//! and without the top bit set. Where the last two decoders disagree, the
//! byte came from the unknown window, and the first two say from which
//! position. Once 32 KiB of output in a row came from the chunk itself,
//! the window no longer matters: the first decoder carries on alone.
//!
//! The caller's thread takes chunks in order, fills in the window bytes
//! from the previous chunk's output, checks that each chunk starts exactly
//! where the previous one stopped, checks member CRCs, records checkpoints
//! and passes the data on. Where a chunk's guess was wrong (a false block
//! start, or a fixed-Huffman block the search skips), the gap is decoded
//! there, sequentially, from the last known position.

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use libz_rs_sys as z;

use crate::gzip::{pack_window, GzDecoder, Inflater, WINDOW};
use crate::stream::{invalid, read_at, Checkpoint, Decoder, Input, Ordered};

/// Compressed bytes per chunk.
const CHUNK_BYTES: u64 = 4 << 20;
/// Decoded bytes all finished-but-unused chunks may hold together, roughly.
const MEMORY_BUDGET: usize = 512 << 20;

/// Where a chunk's decoding began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// The first member's first block (no unknown window).
    File,
    /// A dynamic-Huffman block header at this bit.
    Dynamic(u64),
    /// A stored block whose length field starts at this byte. Its header
    /// and padding lie somewhere in the 3 to 10 bits before.
    Stored(u64),
    /// The first block of a gzip member, at this bit (no unknown window).
    Member(u64),
}

impl Start {
    /// Bit to start decoding at.
    fn bit(self) -> u64 {
        match self {
            Start::File => 0,
            Start::Dynamic(b) | Start::Member(b) => b,
            Start::Stored(q) => q * 8 - 3,
        }
    }

    /// Whether decoding from the block boundary `b` reads the same block.
    fn matches(self, b: u64) -> bool {
        match self {
            Start::File => false,
            Start::Dynamic(x) | Start::Member(x) => b == x,
            Start::Stored(q) => b + 3 <= q * 8 && q * 8 <= b + 10,
        }
    }

    /// Whether the boundary `b` lies past every bit this start could match.
    fn passed_by(self, b: u64) -> bool {
        match self {
            Start::File => true,
            Start::Dynamic(x) | Start::Member(x) => b > x,
            Start::Stored(q) => b > q * 8 - 3,
        }
    }
}

/// What one worker made of its chunk.
struct ChunkOut {
    start: Start,
    /// Output that may refer to the unknown window: a value below 256 is a
    /// byte, `256 + p` is byte `p` of the 32 KiB before the start.
    prefix: Vec<u16>,
    /// Output after the prefix.
    data: Vec<u8>,
    /// Restart points: (offset in this chunk's output, bit).
    boundaries: Vec<(u64, u64)>,
    /// Members ending in this chunk: (offset in this chunk's output, the
    /// trailer's CRC-32 and size).
    trailers: Vec<(u64, u32, u32)>,
    /// Bit where decoding stopped (a block boundary or the first block of
    /// a member), or `None` where the gzip data ended.
    stop: Option<u64>,
}

// ---------------------------------------------------------------------------
// Finding block starts

/// Reads `n` (at most 56) bits at bit `bit` of `d`, least significant first
/// as deflate packs them; zeros past the end.
fn peek(d: &[u8], bit: u64, n: u32) -> u64 {
    let i = (bit / 8) as usize;
    let mut b = [0u8; 8];
    if i < d.len() {
        let k = (d.len() - i).min(8);
        b[..k].copy_from_slice(&d[i..i + k]);
    }
    (u64::from_le_bytes(b) >> (bit % 8)) & ((1u64 << n) - 1)
}

/// For the first 13 bits of a block, whether they can start a non-final
/// dynamic-Huffman block: BFINAL 0, BTYPE 2, HLIT and HDIST in range.
fn first_bits_ok() -> &'static [bool] {
    static T: OnceLock<Vec<bool>> = OnceLock::new();
    T.get_or_init(|| {
        (0..1u32 << 13)
            .map(|v| v & 1 == 0 && (v >> 1) & 3 == 2 && (v >> 3) & 31 <= 29 && (v >> 8) & 31 <= 29)
            .collect()
    })
}

const ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Whether code lengths form a code zlib accepts: never over-subscribed,
/// and complete, except that a literal or distance code may be a single
/// one-bit code (or, for distances, empty).
fn code_ok(lens: &[u8], max: usize, strict: bool) -> bool {
    let mut count = [0i64; 16];
    for &l in lens {
        count[l as usize] += 1;
    }
    let longest = (1..=max).rev().find(|&l| count[l] > 0).unwrap_or(0);
    if longest == 0 {
        return !strict;
    }
    let mut left = 1i64;
    for c in &count[1..=max] {
        left = (left << 1) - c;
        if left < 0 {
            return false;
        }
    }
    left == 0 || (!strict && longest == 1)
}

/// Whether a full dynamic block header parses at bit `h` of `d`.
fn dynamic_ok(d: &[u8], h: u64) -> bool {
    let nbits = d.len() as u64 * 8;
    let mut pos = h + 3;
    let hlit = peek(d, pos, 5) as usize + 257;
    let hdist = peek(d, pos + 5, 5) as usize + 1;
    let hclen = peek(d, pos + 10, 4) as usize + 4;
    pos += 14;
    if pos + hclen as u64 * 3 > nbits {
        return false;
    }
    let mut cl = [0u8; 19];
    for &o in &ORDER[..hclen] {
        cl[o] = peek(d, pos, 3) as u8;
        pos += 3;
    }
    if !code_ok(&cl, 7, true) {
        return false;
    }
    // Decoding table for the code-length code, indexed by 7 input bits.
    let mut table = [(0u8, 0u8); 128];
    let mut next = [0u32; 8];
    let mut code = 0u32;
    let mut count = [0u32; 8];
    for &l in &cl {
        count[l as usize] += 1;
    }
    count[0] = 0;
    for l in 1..8 {
        code = (code + count[l - 1]) << 1;
        next[l] = code;
    }
    for (sym, &l) in cl.iter().enumerate() {
        if l == 0 {
            continue;
        }
        let c = next[l as usize];
        next[l as usize] += 1;
        let rev = (c.reverse_bits() >> (32 - l)) as usize;
        let mut i = rev;
        while i < 128 {
            table[i] = (sym as u8, l);
            i += 1 << l;
        }
    }
    let n = hlit + hdist;
    let mut lens = [0u8; 320];
    let mut i = 0;
    while i < n {
        if pos + 14 > nbits {
            return false;
        }
        let (sym, l) = table[peek(d, pos, 7) as usize];
        pos += l as u64;
        let (val, rep) = match sym {
            0..=15 => (sym, 1),
            16 => {
                if i == 0 {
                    return false;
                }
                let r = 3 + peek(d, pos, 2) as usize;
                pos += 2;
                (lens[i - 1], r)
            }
            17 => {
                let r = 3 + peek(d, pos, 3) as usize;
                pos += 3;
                (0, r)
            }
            _ => {
                let r = 11 + peek(d, pos, 7) as usize;
                pos += 7;
                (0, r)
            }
        };
        if i + rep > n {
            return false;
        }
        lens[i..i + rep].fill(val);
        i += rep;
    }
    lens[256] != 0 && code_ok(&lens[..hlit], 15, false) && code_ok(&lens[hlit..n], 15, false)
}

/// Whether a stored block can have its length field at byte `q`: its 3
/// header bits and padding are zero, the length is followed by its
/// complement, and (when `follow`, and it lies in `d`) a plausible block
/// header comes after its data.
fn stored_ok(d: &[u8], q: usize, follow: bool) -> bool {
    if q < 1 || q + 4 > d.len() || d[q - 1] >> 5 != 0 {
        return false;
    }
    let len = u16::from_le_bytes([d[q], d[q + 1]]);
    if len != !u16::from_le_bytes([d[q + 2], d[q + 3]]) {
        return false;
    }
    let next = q + 4 + len as usize;
    if !follow || next + 64 > d.len() {
        return true;
    }
    let bit = next as u64 * 8;
    match (peek(d, bit, 3) >> 1) & 3 {
        // Another stored block: zero padding after its header bits.
        0 => d[next] >> 3 == 0 && stored_ok(d, next + 1, false),
        1 => true,
        2 => dynamic_ok(d, bit),
        _ => false,
    }
}

/// Finds the first plausible block start at bits `from..to` of `d`, whose
/// first byte is file byte `base`.
fn find(d: &[u8], base: u64, from: u64, to: u64) -> Option<Start> {
    let lut = first_bits_ok();
    for h in from..to {
        if h % 8 == 0 {
            if let Some(end) = header_in(d, (h / 8) as usize) {
                return Some(Start::Member((base + end as u64) * 8));
            }
        }
        if (h + 3) % 8 == 0 && stored_ok(d, ((h + 3) / 8) as usize, true) {
            return Some(Start::Stored(base + (h + 3) / 8));
        }
        if lut[peek(d, h, 13) as usize] && dynamic_ok(d, h) {
            return Some(Start::Dynamic(base * 8 + h));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Decoding a chunk

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// A block boundary at this bit.
    Boundary(u64),
    /// The member's deflate data ended; its trailer starts at this byte.
    End(u64),
}

/// One raw inflate stream reading the file.
struct Stream {
    inf: Inflater,
    input: Input,
}

impl Stream {
    fn new(file: &Arc<File>, bit: u64, dict: Option<&[u8]>) -> io::Result<Stream> {
        let mut inf = Inflater::new(-15)?;
        let mut input = Input::new(file.clone(), bit / 8);
        let skip = (bit % 8) as i32;
        if skip != 0 {
            if input.fill(1)? == 0 {
                return Err(invalid("unexpected end of gzip data"));
            }
            let b = input.data()[0];
            input.consume(1);
            inf.prime(8 - skip, (b >> skip) as i32)?;
        }
        if let Some(d) = dict {
            inf.set_dictionary(d)?;
        }
        Ok(Stream { inf, input })
    }

    /// Decodes up to the next block boundary or the end of the member;
    /// fails once `out` holds more than `limit` bytes.
    fn run(&mut self, out: &mut Vec<u8>, limit: usize) -> io::Result<Event> {
        loop {
            if out.len() > limit {
                return Err(invalid("deflate block too large to decode ahead"));
            }
            if !self.input.refill()? {
                return Err(invalid("unexpected end of gzip data"));
            }
            if out.capacity() - out.len() < 64 << 10 {
                out.reserve(1 << 20);
            }
            let (used, produced, ret) = self
                .inf
                .inflate_uninit(self.input.data(), out.spare_capacity_mut());
            self.input.consume(used);
            // SAFETY: zlib initialized the `produced` bytes it wrote.
            unsafe { out.set_len(out.len() + produced) };
            match ret {
                z::Z_OK | z::Z_BUF_ERROR => {}
                z::Z_STREAM_END => return Ok(Event::End(self.input.offset())),
                _ => return Err(invalid(format!("gzip: {}", self.inf.message()))),
            }
            let dt = self.inf.data_type();
            if dt & 128 != 0 && dt & 64 == 0 {
                return Ok(Event::Boundary(self.input.offset() * 8 - (dt & 7) as u64));
            }
            if used == 0 && produced == 0 {
                return Err(invalid("gzip decoder made no progress"));
            }
        }
    }
}

/// Reads exactly `buf.len()` bytes at `off`.
fn read_exact(file: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    let mut n = 0;
    while n < buf.len() {
        match read_at(file, &mut buf[n..], off + n as u64)? {
            0 => return Err(invalid("unexpected end of gzip data")),
            k => n += k,
        }
    }
    Ok(())
}

/// Parses a gzip member header at byte `at`; returns the byte after it.
fn parse_header(file: &File, at: u64) -> io::Result<u64> {
    let mut h = [0u8; 10];
    read_exact(file, &mut h, at)?;
    if h[0] != 0x1f || h[1] != 0x8b || h[2] != 8 {
        return Err(invalid("gzip: incorrect header check"));
    }
    let flags = h[3];
    if flags & 0xe0 != 0 {
        return Err(invalid("gzip: unknown header flags set"));
    }
    let mut p = at + 10;
    if flags & 4 != 0 {
        let mut x = [0u8; 2];
        read_exact(file, &mut x, p)?;
        p += 2 + u16::from_le_bytes(x) as u64;
    }
    for flag in [8u8, 16] {
        if flags & flag != 0 {
            // A zero-terminated name or comment.
            let mut buf = [0u8; 256];
            loop {
                let n = read_at(file, &mut buf, p)?;
                if n == 0 {
                    return Err(invalid("unexpected end of gzip data"));
                }
                match buf[..n].iter().position(|&b| b == 0) {
                    Some(i) => {
                        p += i as u64 + 1;
                        break;
                    }
                    None => p += n as u64,
                }
            }
        }
    }
    if flags & 2 != 0 {
        // The header CRC: the low 16 bits of the CRC-32 of the header.
        let mut hdr = vec![0u8; (p - at + 2) as usize];
        read_exact(file, &mut hdr, at)?;
        let n = hdr.len() - 2;
        if crc32(0, &hdr[..n]) as u16 != u16::from_le_bytes([hdr[n], hdr[n + 1]]) {
            return Err(invalid("gzip: header crc mismatch"));
        }
        p += 2;
    }
    Ok(p)
}

/// If a gzip member header starts at byte `q` of `d` (and ends in it),
/// the byte after it.
fn header_in(d: &[u8], q: usize) -> Option<usize> {
    let h = d.get(q..q + 10)?;
    if h[..3] != [0x1f, 0x8b, 8] || h[3] & 0xe0 != 0 {
        return None;
    }
    let flags = h[3];
    let mut p = q + 10;
    if flags & 4 != 0 {
        p += 2 + u16::from_le_bytes(d.get(p..p + 2)?.try_into().unwrap()) as usize;
    }
    for flag in [8u8, 16] {
        if flags & flag != 0 {
            p += d.get(p..)?.iter().position(|&b| b == 0)? + 1;
        }
    }
    if flags & 2 != 0 {
        let c = d.get(p..p + 2)?;
        if crc32(0, &d[q..p]) as u16 != u16::from_le_bytes([c[0], c[1]]) {
            return None;
        }
        p += 2;
    }
    (p < d.len()).then_some(p)
}

/// Whether another gzip member starts at byte `at` (as `gzip -d` decides).
fn member_follows(file: &File, at: u64) -> io::Result<bool> {
    let mut m = [0u8; 2];
    let mut n = 0;
    while n < 2 {
        match read_at(file, &mut m[n..], at + n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n == 2 && m == [0x1f, 0x8b])
}

/// The made-up windows: low byte of each position, and its high byte
/// with and without the top bit.
fn fake_windows() -> &'static [Vec<u8>; 3] {
    static W: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
    W.get_or_init(|| {
        let lo = (0..WINDOW).map(|p| p as u8).collect();
        let hi1 = (0..WINDOW).map(|p| (p >> 8) as u8 | 0x80).collect();
        let hi0 = (0..WINDOW).map(|p| (p >> 8) as u8).collect();
        [lo, hi1, hi0]
    })
}

/// Decodes from `start` until the first restart point at or past
/// `end_bit` (or until `cap` bytes), as described in the module docs.
fn decode(
    file: &Arc<File>,
    start: Start,
    bit: u64,
    end_bit: u64,
    cap: usize,
) -> io::Result<ChunkOut> {
    let known = matches!(start, Start::File | Start::Member(_));
    let fw = fake_windows();
    let mut lo = Stream::new(file, bit, (!known).then_some(&fw[0][..]))?;
    // The two high-byte decoders and their output, while needed.
    let mut extra = if known {
        None
    } else {
        Some((
            Stream::new(file, bit, Some(&fw[1]))?,
            Stream::new(file, bit, Some(&fw[2]))?,
            Vec::new(),
            Vec::new(),
        ))
    };
    let mut out = ChunkOut {
        start,
        prefix: Vec::new(),
        data: Vec::new(),
        boundaries: Vec::new(),
        trailers: Vec::new(),
        stop: None,
    };
    if known {
        out.boundaries.push((0, bit));
    }
    let mut buf = Vec::new();
    let mut checked = 0;
    // One past the last byte that came from the unknown window.
    let mut last_marker = 0;
    loop {
        let ev = lo.run(&mut buf, 2 * cap)?;
        if let Some((h1, h0, b1, b0)) = &mut extra {
            let (e1, e0) = (h1.run(b1, 2 * cap)?, h0.run(b0, 2 * cap)?);
            if e1 != ev || e0 != ev || b1.len() != buf.len() || b0.len() != buf.len() {
                return Err(invalid("speculative gzip decoders disagree"));
            }
            if let Some(i) = (checked..buf.len()).rev().find(|&i| b1[i] != b0[i]) {
                last_marker = i + 1;
            }

            checked = buf.len();
            let clean = buf.len() >= WINDOW && buf.len() - last_marker >= WINDOW;
            if clean || matches!(ev, Event::End(_)) {
                out.prefix = markers(&buf, b1, b0, last_marker);
                buf.drain(..last_marker);
                extra = None;
            }
        }
        let total = (out.prefix.len() + buf.len()) as u64;
        let at = match ev {
            Event::Boundary(b) => b,
            Event::End(byte) => {
                let mut t = [0u8; 8];
                read_exact(file, &mut t, byte)?;
                out.trailers.push((
                    total,
                    u32::from_le_bytes(t[..4].try_into().unwrap()),
                    u32::from_le_bytes(t[4..].try_into().unwrap()),
                ));
                if !member_follows(file, byte + 8)? {
                    break;
                }
                let b = parse_header(file, byte + 8)? * 8;
                lo = Stream::new(file, b, None)?;
                b
            }
        };
        out.boundaries.push((total, at));
        if at >= end_bit || total as usize >= cap {
            out.stop = Some(at);
            break;
        }
    }
    if let Some((_, _, b1, b0)) = &extra {
        // Stopped before the window stopped mattering.
        out.prefix = markers(&buf, b1, b0, last_marker);
        buf.drain(..last_marker);
    }
    out.data = buf;
    Ok(out)
}

/// The first `n` bytes of output as bytes and window references.
fn markers(lo: &[u8], hi1: &[u8], hi0: &[u8], n: usize) -> Vec<u16> {
    (0..n)
        .map(|i| {
            if hi1[i] != hi0[i] {
                256 + ((hi0[i] as u16) << 8 | lo[i] as u16)
            } else {
                lo[i] as u16
            }
        })
        .collect()
}

/// Decodes chunk `k`.
fn work(file: &Arc<File>, size: u64, chunk: u64, k: u64, cap: usize) -> io::Result<ChunkOut> {
    let begin = k * chunk;
    let end_bit = (begin + chunk).min(size) * 8;
    if k == 0 {
        let at = parse_header(file, 0)?;
        return decode(file, Start::File, at * 8, end_bit, cap);
    }
    // The chunk, plus room to read a header that starts near its end.
    let mut d = vec![0u8; (chunk + 1024).min(size - begin) as usize];
    read_exact(file, &mut d, begin)?;
    let to = (chunk.min(size - begin)) * 8;
    let mut from = 0;
    while let Some(start) = find(&d, begin, from, to) {
        if let Ok(out) = decode(file, start, start.bit(), end_bit, cap) {
            return Ok(out);
        }
        from = start.bit() - begin * 8 + 1;
    }
    Err(invalid("no deflate block found"))
}

// ---------------------------------------------------------------------------
// Putting chunks in order

/// The indexing reader: decodes chunks on worker threads and returns the
/// stream in order, recording a checkpoint at the first restart point
/// after every `spacing` bytes.
pub(crate) struct ParallelIndexer {
    file: Arc<File>,
    chunks: Ordered<io::Result<ChunkOut>>,
    /// A decoded chunk that starts ahead of where decoding stands.
    target: Option<ChunkOut>,
    /// Bit where the stream continues (`None` once it ended).
    cur: Option<u64>,
    /// Decodes gaps that no chunk covers.
    fallback: Option<GzDecoder>,
    /// Output being handed out, and how much of it was.
    piece: Vec<u8>,
    at: usize,
    /// The 32 KiB of output before `piece`.
    hist: Vec<u8>,
    /// Stream offset of `piece[0]`.
    pos: u64,
    /// CRC-32 of the current member so far, and where it started.
    crc: u32,
    member_start: u64,
    spacing: u64,
    last: Option<u64>,
    pub checkpoints: Vec<Checkpoint>,
    /// Chunks used, and bytes decoded here because no chunk covered them.
    pub stats: (u64, u64),
    chunk: u64,
    cancel: Option<Arc<AtomicBool>>,
}

impl ParallelIndexer {
    pub fn new(
        file: Arc<File>,
        spacing: u64,
        threads: usize,
        cancel: Option<Arc<AtomicBool>>,
    ) -> io::Result<ParallelIndexer> {
        let mut p = Self::with_chunk(file, spacing, threads, CHUNK_BYTES)?;
        p.cancel = cancel;
        Ok(p)
    }

    fn with_chunk(
        file: Arc<File>,
        spacing: u64,
        threads: usize,
        chunk: u64,
    ) -> io::Result<ParallelIndexer> {
        let size = file.metadata()?.len();
        let threads = threads.clamp(1, 1024);
        let window = threads as u64 * 2;
        let cap = (MEMORY_BUDGET / window as usize).max(16 << 20);
        let f = file.clone();
        let chunks = Ordered::new(size.div_ceil(chunk).max(1), threads, window, move |k| {
            work(&f, size, chunk, k, cap)
        })?;
        Ok(ParallelIndexer {
            file,
            chunks,
            target: None,
            cur: Some(0),
            fallback: None,
            piece: Vec::new(),
            at: 0,
            hist: Vec::new(),
            pos: 0,
            crc: 0,
            member_start: 0,
            spacing: spacing.max(1),
            last: None,
            checkpoints: Vec::new(),
            stats: (0, 0),
            chunk,
            cancel: None,
        })
    }

    /// Uncompressed bytes handed out so far.
    pub fn position(&self) -> u64 {
        self.pos + self.at as u64
    }

    pub fn compressed_position(&self) -> u64 {
        match (&self.fallback, self.cur) {
            (Some(d), _) => d.compressed_position(),
            (None, Some(b)) => b / 8,
            (None, None) => self.chunks.position() * self.chunk,
        }
    }

    /// The last 32 KiB of output before offset `r` of the new piece `p`.
    fn window(&self, p: &[u8], r: usize) -> Vec<u8> {
        if r >= WINDOW {
            return p[r - WINDOW..r].to_vec();
        }
        let from_hist = (WINDOW - r).min(self.hist.len());
        let mut w = self.hist[self.hist.len() - from_hist..].to_vec();
        w.extend_from_slice(&p[..r]);
        w
    }

    /// Records a checkpoint at offset `r` of the new piece `p`, if due.
    fn restart_point(&mut self, p: &[u8], r: usize, bit: u64) {
        let at = self.pos + self.at as u64 + r as u64;
        if self.last.is_some_and(|l| at - l < self.spacing) {
            return;
        }
        self.last = Some(at);
        let window = pack_window(&self.window(p, r));
        self.checkpoints.push(Checkpoint {
            uncompressed_offset: at,
            bit_offset: bit,
            window,
        });
    }

    /// Hashes the new piece `p` into the member CRCs; `ends` are member
    /// ends (offset in `p`, and the trailer when it was not checked yet).
    fn account(&mut self, p: &[u8], ends: &[(usize, Option<(u32, u32)>)]) -> io::Result<()> {
        let base = self.pos + self.at as u64;
        let mut done = 0;
        for &(r, trailer) in ends {
            self.crc = crc32(self.crc, &p[done..r]);
            done = r;
            let end = base + r as u64;
            if let Some((crc, size)) = trailer {
                if crc != self.crc {
                    return Err(invalid("gzip: incorrect data check"));
                }
                if size != (end - self.member_start) as u32 {
                    return Err(invalid("gzip: incorrect length check"));
                }
            }
            self.crc = 0;
            self.member_start = end;
        }
        self.crc = crc32(self.crc, &p[done..]);
        Ok(())
    }

    /// Moves the handed-out piece behind: into `hist` (its last 32 KiB)
    /// and `pos`.
    fn advance(&mut self) {
        let old = std::mem::take(&mut self.piece);
        self.pos += old.len() as u64;
        self.at = 0;
        if old.len() >= WINDOW {
            self.hist.clear();
            self.hist.extend_from_slice(&old[old.len() - WINDOW..]);
        } else {
            self.hist.extend_from_slice(&old);
            let cut = self.hist.len().saturating_sub(WINDOW);
            self.hist.drain(..cut);
        }
    }

    /// Uses a chunk that starts where decoding stands.
    fn accept(&mut self, c: ChunkOut) -> io::Result<()> {
        // Previous output is all handed out: `hist` now ends at the chunk's
        // start.
        self.advance();
        let before = &self.hist;
        let pad = WINDOW - before.len();
        let p = if c.prefix.is_empty() {
            c.data
        } else {
            let mut p = Vec::with_capacity(c.prefix.len() + c.data.len());
            for &v in &c.prefix {
                p.push(if v < 256 {
                    v as u8
                } else {
                    let i = (v - 256) as usize;
                    if i < pad {
                        return Err(invalid("gzip: invalid distance too far back"));
                    }
                    before[i - pad]
                });
            }
            p.extend_from_slice(&c.data);
            p
        };
        let ends: Vec<_> = c
            .trailers
            .iter()
            .map(|&(r, crc, size)| (r as usize, Some((crc, size))))
            .collect();
        self.account(&p, &ends)?;
        for &(r, bit) in &c.boundaries {
            self.restart_point(&p, r as usize, bit);
        }
        self.cur = c.stop;
        self.piece = p;
        self.stats.0 += 1;
        Ok(())
    }

    /// The next decoded chunk, skipping failed ones; `None` when all are used.
    fn next_chunk(&mut self) -> Option<ChunkOut> {
        loop {
            // A long run of failed chunks must not hold up cancelling.
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                return None;
            }
            match self.chunks.next()? {
                Ok(c) => return Some(c),
                Err(_) => continue,
            }
        }
    }

    /// Produces the next piece of output. Returns false at the end.
    fn fill(&mut self) -> io::Result<bool> {
        loop {
            let Some(cur) = self.cur else {
                return Ok(false);
            };
            if self.fallback.is_some() {
                return self.fill_gap();
            }
            if cur == 0 {
                // The first chunk starts at the start of the file. If it
                // failed (damaged data, or one huge block that would not
                // fit in memory), decode from the start here.
                match self.chunks.next().expect("at least one chunk") {
                    Ok(c) => {
                        self.accept(c)?;
                        return Ok(true);
                    }
                    Err(_) => {
                        let mut dec = GzDecoder::new(self.file.clone())?;
                        dec.members = Some(Vec::new());
                        self.fallback = Some(dec);
                        continue;
                    }
                }
            }
            let c = match self.target.take() {
                Some(c) => Some(c),
                None => self.next_chunk(),
            };
            match c {
                Some(c) if c.start.matches(cur) => {
                    self.accept(c)?;
                    return Ok(true);
                }
                Some(c) if c.start.passed_by(cur) => continue,
                c => {
                    // Decode the gap up to that chunk (or to the end) here.
                    self.target = c;
                    let mut dec = GzDecoder::at_checkpoint(
                        self.file.clone(),
                        self.pos + self.piece.len() as u64,
                        cur,
                        &self.window(&self.piece, self.piece.len()),
                    )?;
                    dec.members = Some(Vec::new());
                    self.fallback = Some(dec);
                }
            }
        }
    }

    /// Decodes one step of a gap.
    fn fill_gap(&mut self) -> io::Result<bool> {
        let mut dec = self.fallback.take().unwrap();
        let mut p = vec![0u8; 1 << 20];
        let s = dec.step(&mut p)?;
        p.truncate(s.produced);
        self.stats.1 += s.produced as u64;
        // The step's output comes before any boundary it reports.
        let start = self.pos + self.piece.len() as u64;
        let ends: Vec<_> = dec
            .members
            .as_mut()
            .unwrap()
            .drain(..)
            .map(|(at, t)| ((at - start) as usize, t))
            .collect();
        self.advance();
        self.account(&p, &ends)?;
        if s.produced == 0 && !s.boundary {
            self.cur = None;
            self.piece = p;
            return Ok(true);
        }
        if s.boundary {
            let b = dec.bit_offset();
            self.restart_point(&p, p.len(), b);
            self.cur = Some(b);
            loop {
                if self.target.is_none() {
                    self.target = self.next_chunk();
                }
                match &self.target {
                    Some(t) if t.start.matches(b) => {
                        // Back on track: the chunk takes over.
                        self.piece = p;
                        return Ok(true);
                    }
                    Some(t) if t.start.passed_by(b) => self.target = None,
                    _ => break,
                }
            }
        }
        self.fallback = Some(dec);
        self.piece = p;
        Ok(true)
    }
}

fn crc32(crc: u32, data: &[u8]) -> u32 {
    if data.is_empty() {
        return crc;
    }
    let mut c = crc as z::uLong;
    for part in data.chunks(1 << 30) {
        c = unsafe { z::crc32(c, part.as_ptr(), part.len() as z::uInt) };
    }
    c as u32
}

impl io::Read for ParallelIndexer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.at == self.piece.len() {
            if !self.fill()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.piece.len() - self.at);
        buf[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        let words: [&[u8]; 6] = [b"tar ", b"loupe ", b"gzip ", b"block ", b"window\n", b"42 "];
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if x >> 61 == 0 {
                // Some incompressible noise too.
                out.extend((0..64).map(|i| (x >> (i % 56)) as u8));
            } else {
                out.extend_from_slice(words[(x >> 40) as usize % 6]);
            }
        }
        out.truncate(n);
        out
    }

    fn gzip(data: &[u8], level: i32) -> Vec<u8> {
        let mut strm = z::z_stream::default();
        unsafe {
            assert_eq!(
                z::deflateInit2_(
                    &mut strm,
                    level,
                    z::Z_DEFLATED,
                    31,
                    8,
                    z::Z_DEFAULT_STRATEGY,
                    z::zlibVersion(),
                    std::mem::size_of::<z::z_stream>() as i32,
                ),
                z::Z_OK
            );
            let mut out = vec![0u8; z::deflateBound(&mut strm, data.len() as _) as usize];
            strm.next_in = data.as_ptr();
            strm.avail_in = data.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            assert_eq!(z::deflate(&mut strm, z::Z_FINISH), z::Z_STREAM_END);
            out.truncate(strm.total_out as usize);
            z::deflateEnd(&mut strm);
            out
        }
    }

    fn temp(bytes: &[u8]) -> (tempfile::TempDir, Arc<File>) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.gz");
        std::fs::write(&p, bytes).unwrap();
        (dir, Arc::new(File::open(&p).unwrap()))
    }

    fn sequential(file: &Arc<File>) -> io::Result<Vec<u8>> {
        let mut d = GzDecoder::new(file.clone())?;
        let mut all = Vec::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let s = d.step(&mut buf)?;
            all.extend_from_slice(&buf[..s.produced]);
            if s.produced == 0 && !s.boundary {
                return Ok(all);
            }
        }
    }

    type Decoded = (Vec<u8>, Vec<Checkpoint>, (u64, u64));

    /// Decodes with small chunks, so that many chunks start mid-stream.
    fn parallel(file: &Arc<File>, threads: usize) -> io::Result<Decoded> {
        let mut p = ParallelIndexer::with_chunk(file.clone(), 1 << 20, threads, 256 << 10)?;
        let mut got = Vec::new();
        p.read_to_end(&mut got)?;
        Ok((got, std::mem::take(&mut p.checkpoints), p.stats))
    }

    #[test]
    fn finds_real_block_headers() {
        let data = sample(3 << 20, 1);
        let gz = gzip(&data, 6);
        let (_d, file) = temp(&gz);
        // Every boundary zlib reports, as bits.
        let mut d = GzDecoder::new(file.clone()).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut bounds = Vec::new();
        loop {
            let s = d.step(&mut buf).unwrap();
            if s.boundary {
                bounds.push(d.bit_offset());
            }
            if s.produced == 0 && !s.boundary {
                break;
            }
        }
        assert!(bounds.len() > 10);
        // Searching from just after each boundary finds the next one first.
        let mut hits = 0;
        for w in bounds.windows(2).skip(1) {
            let found = find(&gz, 0, w[0] + 1, w[1] + 1);
            if found == Some(Start::Dynamic(w[1])) {
                hits += 1;
            } else {
                // Only final blocks are skipped (by design).
                assert_eq!(peek(&gz, w[1], 1), 1, "boundary {}: found {found:?}", w[1]);
            }
        }
        assert!(
            hits * 10 >= (bounds.len() - 2) * 9,
            "{hits} of {}",
            bounds.len()
        );
    }

    #[test]
    fn parallel_matches_sequential() {
        let data = sample(20 << 20, 7);
        for (name, gz) in [
            ("level 6", gzip(&data, 6)),
            ("level 1", gzip(&data, 1)),
            ("stored", gzip(&data, 0)),
            ("members", {
                let mut v = Vec::new();
                for part in data.chunks(3 << 20) {
                    v.extend(gzip(part, 6));
                }
                v.extend_from_slice(b"\0\0trailing junk");
                v
            }),
            ("tiny members", {
                let mut v = Vec::new();
                for part in data[..4 << 20].chunks(60_000) {
                    v.extend(gzip(part, 6));
                }
                v
            }),
        ] {
            let (_d, file) = temp(&gz);
            let want = sequential(&file).unwrap();
            for threads in [1, 4] {
                let (got, points, (used, gaps)) = parallel(&file, threads).unwrap();
                assert!(got == want, "{name}, {threads} threads");
                // Nearly all of the stream comes from the chunks.
                let chunks = (gz.len() as u64).div_ceil(256 << 10);
                eprintln!("{name}: {used} of {chunks} chunks, {gaps} bytes decoded in gaps");
                if name != "level 1" {
                    assert!(
                        used * 10 >= chunks * 8,
                        "{name}: {used} of {chunks} chunks used"
                    );
                }
                // Every checkpoint resumes correctly.
                for c in &points {
                    let w = crate::gzip::unpack_window(&c.window).unwrap();
                    let mut d = GzDecoder::at_checkpoint(
                        file.clone(),
                        c.uncompressed_offset,
                        c.bit_offset,
                        &w,
                    )
                    .unwrap();
                    let at = c.uncompressed_offset as usize;
                    let mut got = vec![0u8; 4096.min(want.len() - at)];
                    d.read_full(&mut got).unwrap();
                    assert!(
                        got[..] == want[at..at + got.len()],
                        "{name}: checkpoint {at}"
                    );
                }
                // zlib-rs level 1 writes one huge block: no restart points.
                if name != "level 1" {
                    assert!(
                        points.len() >= want.len() / (2 << 20),
                        "{name}: {} points",
                        points.len()
                    );
                }
            }
        }
    }

    #[test]
    fn header_crc_is_checked() {
        let mut h = vec![0x1f, 0x8b, 8, 2, 0, 0, 0, 0, 0, 3];
        let c = crc32(0, &h) as u16;
        h.extend(c.to_le_bytes());
        h.extend([0u8; 8]);
        assert_eq!(header_in(&h, 0), Some(12));
        let (_d, file) = temp(&h);
        assert_eq!(parse_header(&file, 0).unwrap(), 12);
        h[10] ^= 1;
        assert_eq!(header_in(&h, 0), None);
        let (_d, file) = temp(&h);
        assert!(parse_header(&file, 0).is_err());
    }

    #[test]
    fn damage_is_an_error() {
        let data = sample(12 << 20, 3);
        let gz = gzip(&data, 6);
        let mut bad_crc = gz.clone();
        let n = bad_crc.len();
        bad_crc[n - 6] ^= 1;
        let mut bad_data = gz.clone();
        bad_data[gz.len() * 2 / 3] ^= 0x40;
        for (name, bytes) in [
            ("crc", bad_crc),
            ("data", bad_data),
            ("truncated", gz[..gz.len() * 3 / 4].to_vec()),
        ] {
            let (_d, file) = temp(&bytes);
            assert!(parallel(&file, 4).is_err(), "{name}");
        }
    }
}
