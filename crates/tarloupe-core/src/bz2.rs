//! bzip2 streams, with random access to every block.
//!
//! A bzip2 stream is a 4-byte header (`BZh1`..`BZh9`) and a run of blocks
//! packed bit by bit, closed by an end-of-stream marker and the CRC of the
//! whole stream. Every block starts with the 48-bit magic `0x314159265359`
//! at any bit position, carries its own CRC, and decodes on its own. Files
//! written by parallel compressors (`pbzip2`, `lbzip2`) are many such
//! streams back to back.
//!
//! To decode one block, the block's bits are copied (shifted to a byte
//! boundary) into a small stream of their own: a header, the block, and an
//! end marker whose stream CRC is the block's CRC. The standard decoder
//! then checks the block CRC for us. The end of a block is the next magic
//! (block or end of stream). A magic can also occur by chance inside
//! compressed data; a block cut there fails its CRC, and the next magic is
//! tried instead.
//!
//! The indexing pass ([`ParallelIndexer`]) splits the file into chunks,
//! finds the magics in each and decodes their blocks on all cores, while
//! the caller's thread puts the blocks in order, checks the stream CRCs
//! and feeds the tar walker. Every block start is a restart point.
//!
//! Data after the last stream that does not start a new one is ignored,
//! as `bzip2 -d` does.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::sync::{Arc, OnceLock};

use bzip2::{Decompress, Status};

use crate::stream::{invalid, read_at, Checkpoint, Decoder, Opener, Ordered, Step};

/// The magic that starts every block.
const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
/// The magic that ends every stream.
const END_MAGIC: u64 = 0x1772_4538_5090;
/// No block is longer than this, compressed (the largest real one is
/// about 1 MB): a block that has not ended by then is damaged.
const MAX_BLOCK_BYTES: u64 = 2 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Magic {
    Block,
    End,
}

/// Where a magic was found, in bits from the start of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    bit: u64,
    kind: Magic,
}

/// For bytes 1 and 2 of a 7-byte window, which bit shifts of the two
/// magics they fit: bits 0-7 for a block magic starting at bit `s` of
/// byte 0, bits 8-15 for an end magic.
fn table() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![0u16; 1 << 16];
        for (k, magic) in [BLOCK_MAGIC, END_MAGIC].into_iter().enumerate() {
            for s in 0..8 {
                let w = (magic << 16) >> s;
                let b1 = (w >> 48) & 0xff;
                let b2 = (w >> 40) & 0xff;
                t[((b1 << 8) | b2) as usize] |= 1 << (s + 8 * k);
            }
        }
        t
    })
}

/// Finds the magics that start in bytes `from..to` of `data`, whose first
/// byte is at file offset `base`. Matches must fit in `data`.
fn scan(data: &[u8], from: usize, to: usize, base: u64, out: &mut Vec<Candidate>) {
    let t = table();
    let to = to.min(data.len().saturating_sub(2));
    for i in from..to {
        let mask = t[((data[i + 1] as usize) << 8) | data[i + 2] as usize];
        if mask == 0 {
            continue;
        }
        let mut w = 0u64;
        for k in 0..8 {
            w = (w << 8) | *data.get(i + k).unwrap_or(&0) as u64;
        }
        let avail_bits = ((data.len() - i) * 8) as u64;
        for bit in 0..16 {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let (s, magic, kind) = if bit < 8 {
                (bit, BLOCK_MAGIC, Magic::Block)
            } else {
                (bit - 8, END_MAGIC, Magic::End)
            };
            if s + 48 > avail_bits {
                continue;
            }
            let m = (0xffff_ffff_ffffu64 << 16) >> s;
            if w & m == (magic << 16) >> s {
                out.push(Candidate {
                    bit: (base + i as u64) * 8 + s,
                    kind,
                });
            }
        }
    }
}

/// Reads `n` (at most 57) bits at bit `bit` of the file, most significant first.
fn read_bits(file: &File, bit: u64, n: u32) -> io::Result<u64> {
    debug_assert!(n <= 57);
    let mut b = [0u8; 8];
    let need = ((bit % 8) as usize + n as usize).div_ceil(8);
    let mut got = 0;
    while got < need {
        match read_at(file, &mut b[got..need], bit / 8 + got as u64)? {
            0 => return Err(invalid("unexpected end of bzip2 data")),
            k => got += k,
        }
    }
    let w = u64::from_be_bytes(b);
    Ok((w << (bit % 8)) >> (64 - n))
}

/// Appends bits to a byte vector, most significant first.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 = byte aligned).
    used: u32,
}

impl BitWriter {
    fn push(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            let b = ((value >> i) & 1) as u8;
            if self.used == 0 {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= b << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }

    /// Appends bits `start..end` of `src`, whose first byte is file byte
    /// `base`. The writer must be byte aligned.
    fn push_slice(&mut self, src: &[u8], base: u64, start: u64, end: u64) {
        debug_assert_eq!(self.used, 0);
        let first = (start / 8 - base) as usize;
        let s = (start % 8) as u32;
        let nbits = end - start;
        let whole = (nbits / 8) as usize;
        if s == 0 {
            self.bytes.extend_from_slice(&src[first..first + whole]);
        } else {
            self.bytes.reserve(whole);
            for i in first..first + whole {
                self.bytes.push((src[i] << s) | (src[i + 1] >> (8 - s)));
            }
        }
        let rest = (nbits % 8) as u32;
        if rest > 0 {
            let i = first + whole;
            let hi = (src[i] as u16) << 8 | *src.get(i + 1).unwrap_or(&0) as u16;
            let v = (hi << s) >> (16 - rest);
            self.push(v as u64, rest);
        }
    }
}

/// Decodes the block whose magic starts at bit `start` and that ends where
/// bit `end` starts the next magic. `src` holds the file from byte `base`
/// and must cover the block. Appends the data to `out` and returns the
/// block's CRC, or fails if the bits are not exactly one valid block.
fn decode_segment(
    src: &[u8],
    base: u64,
    start: u64,
    end: u64,
    out: &mut Vec<u8>,
) -> io::Result<u32> {
    if end < start + 80 {
        return Err(invalid("bzip2 block too short"));
    }
    let mut w = BitWriter {
        bytes: Vec::with_capacity(((end - start) / 8) as usize + 16),
        used: 0,
    };
    w.bytes.extend_from_slice(b"BZh9");
    w.push_slice(src, base, start, end);
    // The stored block CRC follows the 48-bit block magic.
    let crc_at = ((start + 48 - base * 8) / 8) as usize;
    let s = (start + 48) % 8;
    let mut word = 0u64;
    for k in 0..5 {
        word = (word << 8) | src[crc_at + k] as u64;
    }
    let crc = ((word << (24 + s)) >> 32) as u32;
    w.push(END_MAGIC, 48);
    w.push(crc as u64, 32);

    let mut dec = Decompress::new(false);
    let input = &w.bytes;
    let start_len = out.len();
    loop {
        if out.capacity() - out.len() < 256 * 1024 {
            out.reserve(1 << 20);
        }
        let used = dec.total_in() as usize;
        let before = out.len();
        let status = dec.decompress_vec(&input[used..], out).map_err(|e| {
            out.truncate(start_len);
            invalid(format!("bzip2: {e}"))
        })?;
        if status == Status::StreamEnd {
            return Ok(crc);
        }
        if dec.total_in() as usize == input.len() && out.len() == before {
            out.truncate(start_len);
            return Err(invalid("bzip2 block ends early"));
        }
    }
}

/// Decodes the block at bit `start`, finding its end by trying each later
/// magic in turn. Appends its data to `out`; returns its CRC and the bit
/// where the next magic starts.
fn decode_block(file: &File, start: u64, out: &mut Vec<u8>) -> io::Result<(u32, u64)> {
    let base = start / 8;
    let mut data: Vec<u8> = Vec::new();
    let mut scanned = 0usize;
    let mut cands = Vec::new();
    let mut tried = 0usize;
    let mut eof = false;
    loop {
        while tried < cands.len() {
            let c: Candidate = cands[tried];
            tried += 1;
            if c.bit < start + 80 {
                continue;
            }
            if let Ok(crc) = decode_segment(&data, base, start, c.bit, out) {
                return Ok((crc, c.bit));
            }
        }
        if eof || data.len() as u64 >= MAX_BLOCK_BYTES + 8 {
            return Err(if eof {
                invalid("unexpected end of bzip2 data")
            } else {
                invalid(format!("damaged bzip2 block at byte {base}"))
            });
        }
        let old = data.len();
        let want = if old == 0 { 384 << 10 } else { old };
        data.resize(old + want, 0);
        let mut got = 0;
        while got < want {
            match read_at(file, &mut data[old + got..], base + (old + got) as u64)? {
                0 => break,
                k => got += k,
            }
        }
        data.truncate(old + got);
        eof = got < want;
        // A magic needs 7 bytes; scan up to where the next read resumes.
        let upto = if eof {
            data.len()
        } else {
            data.len().saturating_sub(7)
        };
        scan(&data, scanned, upto, base, &mut cands);
        scanned = upto.max(scanned);
    }
}

/// Parses a stream header at byte `at`. Returns the bit of its first magic,
/// or `None` when no further stream starts there.
fn stream_header(file: &File, at: u64, first: bool) -> io::Result<Option<u64>> {
    let mut d = [0u8; 4];
    let mut n = 0;
    while n < 4 {
        match read_at(file, &mut d[n..], at + n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    let d = &d[..n];
    if n == 4 && &d[..3] == b"BZh" && (b'1'..=b'9').contains(&d[3]) {
        return Ok(Some(at * 8 + 32));
    }
    if first {
        return Err(invalid("not a bzip2 stream"));
    }
    // The start of another stream, cut off.
    if !d.is_empty() && n < 4 && b"BZh"[..n.min(3)] == d[..n.min(3)] {
        return Err(invalid("unexpected end of bzip2 data"));
    }
    Ok(None)
}

/// Combines a block CRC into the CRC of its stream.
fn fold(stream: u32, block: u32) -> u32 {
    stream.rotate_left(1) ^ block
}

/// What a decoder meets next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    Header { byte: u64, first: bool },
    Magic { bit: u64 },
    Done,
}

/// Moves `next` over stream ends and headers until it stands on a block
/// (returns true) or the data ends. `crc` is the running stream CRC, when
/// decoding began at the stream's start.
fn settle(file: &File, next: &mut Next, crc: &mut Option<u32>) -> io::Result<bool> {
    loop {
        match *next {
            Next::Done => return Ok(false),
            Next::Header { byte, first } => match stream_header(file, byte, first)? {
                Some(bit) => {
                    *crc = Some(0);
                    *next = Next::Magic { bit };
                }
                None => *next = Next::Done,
            },
            Next::Magic { bit } => {
                let m = read_bits(file, bit, 48)?;
                if m == BLOCK_MAGIC {
                    return Ok(true);
                }
                if m != END_MAGIC {
                    return Err(invalid(format!("damaged bzip2 data at byte {}", bit / 8)));
                }
                let stored = read_bits(file, bit + 48, 32)? as u32;
                if crc.is_some_and(|c| c != stored) {
                    return Err(invalid("bzip2 stream CRC mismatch"));
                }
                *next = Next::Header {
                    byte: (bit + 80).div_ceil(8),
                    first: false,
                };
            }
        }
    }
}

/// A forward decoder, one block at a time.
pub(crate) struct Bz2Decoder {
    file: Arc<File>,
    next: Next,
    crc: Option<u32>,
    block: Vec<u8>,
    at: usize,
    pos: u64,
}

impl Bz2Decoder {
    /// A decoder at the start of the file.
    pub fn new(file: Arc<File>) -> Bz2Decoder {
        Bz2Decoder {
            file,
            next: Next::Header {
                byte: 0,
                first: true,
            },
            crc: None,
            block: Vec::new(),
            at: 0,
            pos: 0,
        }
    }

    /// A decoder at a restart point: a block magic, or (in indexes of
    /// Tarloupe 0.1) the start of a stream.
    pub fn at(file: Arc<File>, bit: u64, pos: u64) -> io::Result<Bz2Decoder> {
        let next = if bit.is_multiple_of(8) && stream_header(&file, bit / 8, false)?.is_some() {
            Next::Header {
                byte: bit / 8,
                first: false,
            }
        } else if read_bits(&file, bit, 48)? == BLOCK_MAGIC {
            Next::Magic { bit }
        } else {
            return Err(invalid("checkpoint is not at a bzip2 block"));
        };
        Ok(Bz2Decoder {
            file,
            next,
            crc: None,
            block: Vec::new(),
            at: 0,
            pos,
        })
    }
}

impl Decoder for Bz2Decoder {
    fn position(&self) -> u64 {
        self.pos
    }

    fn compressed_position(&self) -> u64 {
        match self.next {
            Next::Header { byte, .. } => byte,
            Next::Magic { bit } => bit / 8,
            Next::Done => self.file.metadata().map_or(0, |m| m.len()),
        }
    }

    fn step(&mut self, out: &mut [u8]) -> io::Result<Step> {
        while self.at == self.block.len() {
            if !settle(&self.file, &mut self.next, &mut self.crc)? {
                return Ok(Step {
                    produced: 0,
                    boundary: false,
                });
            }
            let Next::Magic { bit } = self.next else {
                unreachable!()
            };
            self.block.clear();
            self.at = 0;
            let (crc, end) = decode_block(&self.file, bit, &mut self.block)?;
            self.crc = self.crc.map(|c| fold(c, crc));
            self.next = Next::Magic { bit: end };
        }
        let n = out.len().min(self.block.len() - self.at);
        out[..n].copy_from_slice(&self.block[self.at..self.at + n]);
        self.at += n;
        self.pos += n as u64;
        let boundary =
            self.at == self.block.len() && settle(&self.file, &mut self.next, &mut self.crc)?;
        Ok(Step {
            produced: n,
            boundary,
        })
    }

    fn restart_point(&self) -> (u64, Vec<u8>) {
        match self.next {
            Next::Magic { bit } => (bit, Vec::new()),
            _ => unreachable!("only called at a block"),
        }
    }
}

pub(crate) fn opener(file: Arc<File>) -> Opener {
    Box::new(move |cp, _state| {
        Ok(match cp {
            None => Box::new(Bz2Decoder::new(file.clone())),
            Some(p) => Box::new(Bz2Decoder::at(
                file.clone(),
                p.bit_offset,
                p.uncompressed_offset,
            )?),
        })
    })
}

// ---------------------------------------------------------------------------
// Parallel indexing

/// Compressed bytes per work item.
const CHUNK_BYTES: u64 = 2 << 20;
/// Decoded bytes one work item keeps at most (plus one block).
const CHUNK_DECODED_BUDGET: usize = 64 << 20;

/// A magic found in a chunk, with its block decoded on the guess that the
/// next magic ends it.
struct Found {
    cand: Candidate,
    /// For a block: its data and CRC, and the bit where it ended.
    block: Option<io::Result<(Vec<u8>, u32, u64)>>,
}

/// Finds the magics starting in one chunk and decodes their blocks.
fn work(file: &File, size: u64, chunk: u64) -> io::Result<Vec<Found>> {
    let start = chunk * CHUNK_BYTES;
    let end = (start + CHUNK_BYTES).min(size);
    // Read the chunk plus enough to finish its last block.
    let mut data = vec![0u8; (end - start + 8).min(size - start) as usize];
    crate::archive::pread_exact(file, &mut data, start).map_err(io::Error::other)?;
    let mut cands = Vec::new();
    scan(&data, 0, (end - start) as usize, start, &mut cands);
    // The first magic after the chunk ends the chunk's last block.
    let mut scanned = (end - start) as usize;
    let mut after = Vec::new();
    while after.is_empty() && (start + data.len() as u64) < size {
        if data.len() as u64 > CHUNK_BYTES + MAX_BLOCK_BYTES {
            break;
        }
        let old = data.len();
        let more = (size - start - old as u64).min(256 << 10) as usize;
        data.resize(old + more, 0);
        crate::archive::pread_exact(file, &mut data[old..], start + old as u64)
            .map_err(io::Error::other)?;
        let upto = if start + data.len() as u64 == size {
            data.len()
        } else {
            data.len() - 7
        };
        scan(&data, scanned, upto, start, &mut after);
        scanned = upto;
    }
    if after.is_empty() && start + data.len() as u64 == size {
        scan(&data, scanned, data.len(), start, &mut after);
    }
    let mut found = Vec::with_capacity(cands.len());
    // Runs of repeated bytes make blocks expand up to ~46 MB each: past the
    // budget, blocks are left for the reader to decode when it gets there.
    let mut decoded = 0usize;
    for (i, &c) in cands.iter().enumerate() {
        if decoded >= CHUNK_DECODED_BUDGET {
            found.push(Found {
                cand: c,
                block: None,
            });
            continue;
        }
        let block = (c.kind == Magic::Block).then(|| {
            let next = cands.get(i + 1).or(after.first()).copied();
            match next {
                Some(n) => {
                    let mut out = Vec::new();
                    decode_segment(&data, start, c.bit, n.bit, &mut out)
                        .map(|crc| (out, crc, n.bit))
                }
                None => Err(invalid("no end found")),
            }
        });
        if let Some(Ok((data, _, _))) = &block {
            decoded += data.len();
        }
        found.push(Found { cand: c, block });
    }
    Ok(found)
}

/// The indexing reader: decodes blocks on worker threads and returns the
/// data in order, recording a checkpoint at the first block start after
/// every `spacing` bytes.
pub(crate) struct ParallelIndexer {
    file: Arc<File>,
    chunks: Ordered<io::Result<Vec<Found>>>,
    found: VecDeque<Found>,
    next: Next,
    crc: Option<u32>,
    block: Vec<u8>,
    at: usize,
    pos: u64,
    spacing: u64,
    last: u64,
    pub checkpoints: Vec<Checkpoint>,
}

impl ParallelIndexer {
    pub fn new(file: Arc<File>, spacing: u64, threads: usize) -> io::Result<ParallelIndexer> {
        let size = file.metadata()?.len();
        let threads = threads.clamp(1, 1024);
        let f = file.clone();
        let chunks = Ordered::new(
            size.div_ceil(CHUNK_BYTES),
            threads,
            threads as u64 * 2,
            move |k| work(&f, size, k),
        )?;
        Ok(ParallelIndexer {
            file,
            chunks,
            found: VecDeque::new(),
            next: Next::Header {
                byte: 0,
                first: true,
            },
            crc: None,
            block: Vec::new(),
            at: 0,
            pos: 0,
            spacing: spacing.max(1),
            last: 0,
            checkpoints: Vec::new(),
        })
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn compressed_position(&self) -> u64 {
        match self.next {
            Next::Header { byte, .. } => byte,
            Next::Magic { bit } => bit / 8,
            Next::Done => self.chunks.position() * CHUNK_BYTES,
        }
    }

    /// Waits for the next chunk's magics. Returns false when all are taken.
    fn pull(&mut self) -> io::Result<bool> {
        match self.chunks.next() {
            Some(found) => {
                self.found.extend(found?);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Decodes the next block into `self.block`. Returns false at the end.
    fn next_block(&mut self) -> io::Result<bool> {
        if !settle(&self.file, &mut self.next, &mut self.crc)? {
            return Ok(false);
        }
        let Next::Magic { bit } = self.next else {
            unreachable!()
        };
        // Drop magics before this block (found by chance in earlier
        // blocks or in data between streams) and find this one.
        loop {
            while self.found.front().is_some_and(|f| f.cand.bit < bit) {
                self.found.pop_front();
            }
            if !self.found.is_empty() || !self.pull()? {
                break;
            }
        }
        self.block.clear();
        self.at = 0;
        let guess = match self.found.front_mut() {
            Some(f) if f.cand.bit == bit => f.block.take(),
            _ => None,
        };
        let (crc, end) = match guess {
            Some(Ok((data, crc, end))) => {
                self.block = data;
                (crc, end)
            }
            // The guessed end was a chance magic, or the block was
            // not where the chunk's scan expected it: decode it here.
            _ => decode_block(&self.file, bit, &mut self.block)?,
        };
        self.crc = self.crc.map(|c| fold(c, crc));
        self.next = Next::Magic { bit: end };
        if self.pos > 0 && self.pos - self.last >= self.spacing {
            self.last = self.pos;
            self.checkpoints.push(Checkpoint {
                uncompressed_offset: self.pos,
                bit_offset: bit,
                window: Vec::new(),
            });
        }
        Ok(true)
    }
}

impl io::Read for ParallelIndexer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.at == self.block.len() {
            if !self.next_block()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.block.len() - self.at);
        buf[..n].copy_from_slice(&self.block[self.at..self.at + n]);
        self.at += n;
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn compress(data: &[u8], level: u32) -> Vec<u8> {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(level));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn sample(n: usize) -> Vec<u8> {
        let mut x = 1u64;
        (0..n)
            .map(|i| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                // Compressible but not trivially so.
                if i % 3 == 0 {
                    (x >> 60) as u8 + b'a'
                } else {
                    b"tarloupe "[i % 9]
                }
            })
            .collect()
    }

    fn temp(bytes: &[u8]) -> (tempfile::TempDir, Arc<File>) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bz2");
        std::fs::write(&p, bytes).unwrap();
        (dir, Arc::new(File::open(&p).unwrap()))
    }

    #[test]
    fn scan_finds_magics_at_every_shift() {
        for s in 0..8u64 {
            for (magic, kind) in [(BLOCK_MAGIC, Magic::Block), (END_MAGIC, Magic::End)] {
                let mut w = BitWriter {
                    bytes: vec![0xaa],
                    used: 0,
                };
                w.push(0, s as u32);
                w.push(magic, 48);
                w.push(0x5, 3);
                let mut out = Vec::new();
                scan(&w.bytes, 0, w.bytes.len(), 10, &mut out);
                assert_eq!(out, vec![Candidate { bit: 88 + s, kind }], "shift {s}");
            }
        }
    }

    #[test]
    fn blocks_decode_alone_and_in_parallel() {
        let data = sample(3_000_000);
        let mut bz = compress(&data, 1); // 100k blocks
        bz.extend(compress(&data[..500_000], 9)); // a second stream
        bz.extend_from_slice(b"garbage after the end");
        let (_d, file) = temp(&bz);

        let mut all = Vec::new();
        let mut seq = Bz2Decoder::new(file.clone());
        let mut buf = vec![0u8; 100_000];
        let mut points = Vec::new();
        loop {
            let s = seq.step(&mut buf).unwrap();
            all.extend_from_slice(&buf[..s.produced]);
            if s.boundary {
                points.push((seq.position(), seq.restart_point().0));
            }
            if s.produced == 0 && !s.boundary {
                break;
            }
        }
        let mut want = data.clone();
        want.extend_from_slice(&data[..500_000]);
        assert!(all == want);
        assert!(points.len() > 20);
        for &(pos, bit) in &points {
            let mut d = Bz2Decoder::at(file.clone(), bit, pos).unwrap();
            let mut got = vec![0u8; 1000.min(want.len() - pos as usize)];
            d.read_full(&mut got).unwrap();
            assert!(got[..] == want[pos as usize..pos as usize + got.len()]);
        }

        for threads in [1, 3] {
            let mut p = ParallelIndexer::new(file.clone(), 200_000, threads).unwrap();
            let mut got = Vec::new();
            p.read_to_end(&mut got).unwrap();
            assert!(got == want, "threads {threads}");
            assert!(!p.checkpoints.is_empty());
            for c in &p.checkpoints {
                assert!(points.contains(&(c.uncompressed_offset, c.bit_offset)));
            }
        }
    }

    #[test]
    fn damaged_blocks_are_errors() {
        let data = sample(400_000);
        let mut bz = compress(&data, 1);
        let mid = bz.len() / 2;
        bz[mid] ^= 0x10;
        let (_d, file) = temp(&bz);
        let mut got = Vec::new();
        assert!(ParallelIndexer::new(file.clone(), 1 << 20, 2)
            .unwrap()
            .read_to_end(&mut got)
            .is_err());
        let mut seq = Bz2Decoder::new(file);
        let mut buf = vec![0u8; 1 << 16];
        let r = (|| loop {
            let s = seq.step(&mut buf)?;
            if s.produced == 0 && !s.boundary {
                return Ok::<_, io::Error>(());
            }
        })();
        assert!(r.is_err());
    }
}
