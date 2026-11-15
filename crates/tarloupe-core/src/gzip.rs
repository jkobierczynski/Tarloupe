//! Random access into gzip streams through saved decoder restart points.
//!
//! The first pass decodes the whole stream once and, at deflate block
//! boundaries roughly every `spacing` bytes of output, records a
//! checkpoint: the bit position in the compressed file and the last 32 KiB
//! of output (the deflate window). Any later read starts at the nearest
//! checkpoint before it instead of at the start of the file (the approach
//! of zlib's `examples/zran.c`).
//!
//! Multi-member files (`cat a.gz b.gz`, pigz, BGZF) are one logical stream.
//! Bytes after the last member that do not start a new member are ignored,
//! as GNU gzip does.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::sync::{Arc, Mutex};

use libz_rs_sys as z;

/// Size of the deflate history window.
pub const WINDOW: usize = 32 * 1024;
/// Default distance between checkpoints, in uncompressed bytes.
pub const DEFAULT_SPACING: u64 = 8 * 1024 * 1024;
/// Decoded data is cached in aligned chunks of this size.
const CHUNK: u64 = 1024 * 1024;
/// Chunks kept in the cache (64 MiB).
const CACHE_CHUNKS: usize = 64;
/// Idle decoders kept for continuing sequential reads.
const POOL: usize = 8;
const INPUT_BUF: usize = 128 * 1024;

/// A decoder restart point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Offset in the uncompressed stream.
    pub uncompressed_offset: u64,
    /// Position in the compressed file, in bits from its start.
    pub bit_offset: u64,
    /// The preceding window (up to 32 KiB), zlib-compressed.
    pub window: Vec<u8>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Owns one zlib-rs inflate stream. Boxed because zlib keeps a pointer
/// back to the stream structure.
struct Inflater {
    strm: Box<z::z_stream>,
}

// The stream is exclusively owned and only touched through `&mut self`.
unsafe impl Send for Inflater {}

impl Inflater {
    /// `window_bits` is -15 for raw deflate or 31 for a gzip member.
    fn new(window_bits: i32) -> io::Result<Inflater> {
        let mut strm = Box::new(z::z_stream::default());
        let ret = unsafe {
            z::inflateInit2_(
                &mut *strm,
                window_bits,
                z::zlibVersion(),
                std::mem::size_of::<z::z_stream>() as i32,
            )
        };
        if ret != z::Z_OK {
            return Err(io::Error::other(format!("inflateInit2 failed ({ret})")));
        }
        Ok(Inflater { strm })
    }

    fn reset(&mut self, window_bits: i32) -> io::Result<()> {
        match unsafe { z::inflateReset2(&mut *self.strm, window_bits) } {
            z::Z_OK => Ok(()),
            ret => Err(io::Error::other(format!("inflateReset2 failed ({ret})"))),
        }
    }

    fn prime(&mut self, bits: i32, value: i32) -> io::Result<()> {
        match unsafe { z::inflatePrime(&mut *self.strm, bits, value) } {
            z::Z_OK => Ok(()),
            ret => Err(io::Error::other(format!("inflatePrime failed ({ret})"))),
        }
    }

    fn set_dictionary(&mut self, dict: &[u8]) -> io::Result<()> {
        match unsafe {
            z::inflateSetDictionary(&mut *self.strm, dict.as_ptr(), dict.len() as z::uInt)
        } {
            z::Z_OK => Ok(()),
            ret => Err(io::Error::other(format!(
                "inflateSetDictionary failed ({ret})"
            ))),
        }
    }

    /// One `inflate(Z_BLOCK)` call. Returns (consumed, produced, code).
    fn inflate(&mut self, input: &[u8], output: &mut [u8]) -> (usize, usize, i32) {
        let s = &mut *self.strm;
        s.next_in = input.as_ptr();
        s.avail_in = input.len().min(u32::MAX as usize) as z::uInt;
        s.next_out = output.as_mut_ptr();
        s.avail_out = output.len().min(u32::MAX as usize) as z::uInt;
        let (ai, ao) = (s.avail_in as usize, s.avail_out as usize);
        let ret = unsafe { z::inflate(s, z::Z_BLOCK) };
        (ai - s.avail_in as usize, ao - s.avail_out as usize, ret)
    }

    fn data_type(&self) -> i32 {
        self.strm.data_type
    }

    fn message(&self) -> String {
        if self.strm.msg.is_null() {
            "invalid compressed data".into()
        } else {
            unsafe { std::ffi::CStr::from_ptr(self.strm.msg) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

impl Drop for Inflater {
    fn drop(&mut self) {
        unsafe {
            z::inflateEnd(&mut *self.strm);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Inside a deflate stream (raw after a checkpoint, else a gzip member).
    Stream {
        raw: bool,
    },
    /// Skipping the 8-byte trailer of a member decoded in raw mode.
    Trailer(u8),
    /// Between members: another one starts here, or the data ends.
    NextMember,
    Done,
}

/// A gzip decoder that reads the compressed file positionally and reports
/// deflate block boundaries.
pub(crate) struct GzDecoder {
    file: Arc<File>,
    inbuf: Box<[u8]>,
    in_start: usize,
    in_end: usize,
    /// File offset of `inbuf[in_end]`.
    next_read: u64,
    inf: Inflater,
    phase: Phase,
    /// Uncompressed bytes produced so far (absolute stream offset).
    pos: u64,
}

/// Result of one decoding step.
pub(crate) struct Step {
    pub produced: usize,
    /// The step ended on a block boundary where a checkpoint may be taken.
    pub boundary: bool,
}

impl GzDecoder {
    /// A decoder at the start of the file.
    pub fn new(file: Arc<File>) -> io::Result<GzDecoder> {
        Ok(GzDecoder {
            file,
            inbuf: vec![0u8; INPUT_BUF].into_boxed_slice(),
            in_start: 0,
            in_end: 0,
            next_read: 0,
            inf: Inflater::new(31)?,
            phase: Phase::Stream { raw: false },
            pos: 0,
        })
    }

    /// A decoder resumed at a checkpoint, with its uncompressed window.
    pub fn at_checkpoint(
        file: Arc<File>,
        uncompressed_offset: u64,
        bit_offset: u64,
        window: &[u8],
    ) -> io::Result<GzDecoder> {
        let mut d = GzDecoder {
            file,
            inbuf: vec![0u8; INPUT_BUF].into_boxed_slice(),
            in_start: 0,
            in_end: 0,
            next_read: bit_offset / 8,
            inf: Inflater::new(-15)?,
            phase: Phase::Stream { raw: true },
            pos: uncompressed_offset,
        };
        let skip = (bit_offset % 8) as i32;
        if skip != 0 {
            let mut b = [0u8; 1];
            crate::archive::pread_exact(&d.file, &mut b, d.next_read)
                .map_err(|_| invalid("checkpoint lies beyond the end of the file"))?;
            d.next_read += 1;
            d.inf.prime(8 - skip, (b[0] >> skip) as i32)?;
        }
        if !window.is_empty() {
            d.inf.set_dictionary(window)?;
        }
        Ok(d)
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Bit position of the next unread compressed bit.
    fn bit_offset(&self) -> u64 {
        let byte = self.next_read - (self.in_end - self.in_start) as u64;
        byte * 8 - (self.inf.data_type() & 7) as u64
    }

    /// Refills the input buffer. Returns false at the end of the file.
    fn refill(&mut self) -> io::Result<bool> {
        if self.in_start < self.in_end {
            return Ok(true);
        }
        let n = read_at(&self.file, &mut self.inbuf, self.next_read)?;
        self.in_start = 0;
        self.in_end = n;
        self.next_read += n as u64;
        Ok(n > 0)
    }

    /// Decodes into `out` (which must not be empty). `produced == 0` with
    /// no boundary means the stream has ended.
    pub fn step(&mut self, out: &mut [u8]) -> io::Result<Step> {
        debug_assert!(!out.is_empty());
        loop {
            match self.phase {
                Phase::Done => {
                    return Ok(Step {
                        produced: 0,
                        boundary: false,
                    })
                }
                Phase::Trailer(left) => {
                    if !self.refill()? {
                        return Err(invalid("truncated gzip member trailer"));
                    }
                    let k = (left as usize).min(self.in_end - self.in_start);
                    self.in_start += k;
                    self.phase = match left as usize - k {
                        0 => Phase::NextMember,
                        rest => Phase::Trailer(rest as u8),
                    };
                }
                Phase::NextMember => {
                    if !self.refill()? {
                        self.phase = Phase::Done;
                        continue;
                    }
                    if self.inbuf[self.in_start] != 0x1f {
                        self.phase = Phase::Done; // trailing garbage or padding
                        continue;
                    }
                    if self.in_end - self.in_start < 2 {
                        // The magic straddles the buffer: move the byte to the front.
                        self.inbuf[0] = self.inbuf[self.in_start];
                        let n = read_at(&self.file, &mut self.inbuf[1..], self.next_read)?;
                        self.in_start = 0;
                        self.in_end = 1 + n;
                        self.next_read += n as u64;
                        if n == 0 {
                            self.phase = Phase::Done;
                            continue;
                        }
                    }
                    if self.inbuf[self.in_start + 1] != 0x8b {
                        self.phase = Phase::Done;
                        continue;
                    }
                    self.inf.reset(31)?;
                    self.phase = Phase::Stream { raw: false };
                }
                Phase::Stream { raw } => {
                    if !self.refill()? {
                        return Err(invalid("unexpected end of gzip data"));
                    }
                    let (used, produced, ret) = self
                        .inf
                        .inflate(&self.inbuf[self.in_start..self.in_end], out);
                    self.in_start += used;
                    self.pos += produced as u64;
                    match ret {
                        z::Z_OK | z::Z_BUF_ERROR => {}
                        z::Z_STREAM_END => {
                            self.phase = if raw {
                                Phase::Trailer(8)
                            } else {
                                Phase::NextMember
                            };
                            if produced > 0 {
                                return Ok(Step {
                                    produced,
                                    boundary: false,
                                });
                            }
                            continue;
                        }
                        z::Z_NEED_DICT => return Err(invalid("gzip member needs a dictionary")),
                        _ => return Err(invalid(format!("gzip: {}", self.inf.message()))),
                    }
                    let dt = self.inf.data_type();
                    let boundary = dt & 128 != 0 && dt & 64 == 0;
                    if produced > 0 || boundary {
                        return Ok(Step { produced, boundary });
                    }
                    if used == 0 && self.in_start < self.in_end {
                        return Err(invalid("gzip decoder made no progress"));
                    }
                }
            }
        }
    }

    /// Fills `buf` completely unless the stream ends; returns bytes written.
    pub fn read_full(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            let s = self.step(&mut buf[n..])?;
            if s.produced == 0 && !s.boundary {
                break;
            }
            n += s.produced;
        }
        Ok(n)
    }

    /// Decodes and discards up to `n` bytes; returns how many were skipped.
    pub fn skip(&mut self, mut n: u64, scratch: &mut [u8]) -> io::Result<u64> {
        let start = self.pos;
        while n > 0 {
            let want = (scratch.len() as u64).min(n) as usize;
            let got = self.read_full(&mut scratch[..want])?;
            if got == 0 {
                break;
            }
            n -= got as u64;
        }
        Ok(self.pos - start)
    }
}

fn read_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, off)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, off)
    }
}

/// Compresses a window for storage.
pub fn pack_window(window: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; z::compressBound(window.len() as _) as usize];
    let mut len = out.len() as z::uLongf;
    let ret = unsafe {
        z::compress2(
            out.as_mut_ptr(),
            &mut len,
            window.as_ptr(),
            window.len() as _,
            6,
        )
    };
    assert_eq!(
        ret,
        z::Z_OK,
        "compress2 cannot fail with a large enough buffer"
    );
    out.truncate(len as usize);
    out
}

/// Reverses [`pack_window`].
pub fn unpack_window(packed: &[u8]) -> io::Result<Vec<u8>> {
    if packed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; WINDOW];
    let mut len = out.len() as z::uLongf;
    let ret = unsafe {
        z::uncompress(
            out.as_mut_ptr(),
            &mut len,
            packed.as_ptr(),
            packed.len() as _,
        )
    };
    if ret != z::Z_OK {
        return Err(invalid("corrupt checkpoint window in the index"));
    }
    out.truncate(len as usize);
    Ok(out)
}

/// Decompressing reader used for the indexing pass. Records a checkpoint
/// at the first block boundary after every `spacing` bytes of output.
pub(crate) struct IndexingReader {
    dec: GzDecoder,
    /// Circular buffer holding the last 32 KiB of output; output is
    /// decoded straight into it.
    win: Box<[u8]>,
    wpos: usize,
    wrapped: bool,
    pending: std::ops::Range<usize>,
    spacing: u64,
    last: Option<u64>,
    pub checkpoints: Vec<Checkpoint>,
}

impl IndexingReader {
    pub fn new(file: Arc<File>, spacing: u64) -> io::Result<IndexingReader> {
        Ok(IndexingReader {
            dec: GzDecoder::new(file)?,
            win: vec![0u8; WINDOW].into_boxed_slice(),
            wpos: 0,
            wrapped: false,
            pending: 0..0,
            spacing: spacing.max(1),
            last: None,
            checkpoints: Vec::new(),
        })
    }

    /// Bytes of the gzip file read so far.
    pub fn compressed_position(&self) -> u64 {
        self.dec.next_read
    }

    fn window(&self) -> Vec<u8> {
        if self.wrapped {
            let mut w = self.win[self.wpos..].to_vec();
            w.extend_from_slice(&self.win[..self.wpos]);
            w
        } else {
            self.win[..self.wpos].to_vec()
        }
    }

    fn checkpoint(&mut self) {
        let at = self.dec.position();
        if self.last.is_some_and(|l| at - l < self.spacing) {
            return;
        }
        self.last = Some(at);
        self.checkpoints.push(Checkpoint {
            uncompressed_offset: at,
            bit_offset: self.dec.bit_offset(),
            window: pack_window(&self.window()),
        });
    }
}

impl io::Read for IndexingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            if self.wpos == WINDOW {
                self.wpos = 0;
                self.wrapped = true;
            }
            let s = self.dec.step(&mut self.win[self.wpos..])?;
            self.pending = self.wpos..self.wpos + s.produced;
            self.wpos += s.produced;
            if s.boundary {
                self.checkpoint();
            }
            if s.produced == 0 && !s.boundary {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.win[self.pending.start..self.pending.start + n]);
        self.pending.start += n;
        Ok(n)
    }
}

/// A checkpoint as kept in memory (the window is loaded on demand).
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub uncompressed_offset: u64,
    pub bit_offset: u64,
}

/// Small LRU of decoded chunks.
struct ChunkCache {
    map: HashMap<u64, (Arc<[u8]>, u64)>,
    tick: u64,
}

impl ChunkCache {
    fn get(&mut self, k: u64) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(&k).map(|(v, used)| {
            *used = t;
            v.clone()
        })
    }
    fn put(&mut self, k: u64, v: Arc<[u8]>) {
        self.tick += 1;
        if self.map.len() >= CACHE_CHUNKS && !self.map.contains_key(&k) {
            if let Some(&old) = self.map.iter().min_by_key(|(_, (_, u))| *u).map(|(k, _)| k) {
                self.map.remove(&old);
            }
        }
        self.map.insert(k, (v, self.tick));
    }
}

/// Random-access reader over a gzip file with a loaded checkpoint list.
pub(crate) struct GzRandom {
    file: Arc<File>,
    points: Vec<Point>,
    cache: Mutex<ChunkCache>,
    /// Idle decoders, each positioned at a chunk boundary.
    pool: Mutex<Vec<GzDecoder>>,
}

impl GzRandom {
    pub fn new(file: Arc<File>, points: Vec<Point>) -> GzRandom {
        GzRandom {
            file,
            points,
            cache: Mutex::new(ChunkCache {
                map: HashMap::new(),
                tick: 0,
            }),
            pool: Mutex::new(Vec::new()),
        }
    }

    /// Fills `buf` from uncompressed offset `off`; fails at end of data.
    /// `window` loads the packed window of a checkpoint.
    pub fn read_exact_at(
        &self,
        buf: &mut [u8],
        off: u64,
        window: &dyn Fn(u64) -> io::Result<Vec<u8>>,
    ) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = off + done as u64;
            let idx = at / CHUNK;
            let chunk = self.chunk(idx, window)?;
            let from = (at - idx * CHUNK) as usize;
            if from >= chunk.len() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let n = (chunk.len() - from).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&chunk[from..from + n]);
            done += n;
        }
        Ok(())
    }

    fn chunk(
        &self,
        idx: u64,
        window: &dyn Fn(u64) -> io::Result<Vec<u8>>,
    ) -> io::Result<Arc<[u8]>> {
        if let Some(c) = self.cache.lock().unwrap().get(idx) {
            return Ok(c);
        }
        let start = idx * CHUNK;
        let cp = match self
            .points
            .partition_point(|p| p.uncompressed_offset <= start)
        {
            0 => None,
            i => Some(self.points[i - 1]),
        };
        let cp_off = cp.map_or(0, |p| p.uncompressed_offset);
        // Reuse an idle decoder between the checkpoint and the chunk.
        let mut dec = {
            let mut pool = self.pool.lock().unwrap();
            let best = pool
                .iter()
                .enumerate()
                .filter(|(_, d)| d.position() <= start && d.position() >= cp_off)
                .max_by_key(|(_, d)| d.position())
                .map(|(i, _)| i);
            best.map(|i| pool.swap_remove(i))
        };
        if dec.is_none() {
            dec = Some(match cp {
                Some(p) => {
                    let w = unpack_window(&window(p.uncompressed_offset)?)?;
                    GzDecoder::at_checkpoint(
                        self.file.clone(),
                        p.uncompressed_offset,
                        p.bit_offset,
                        &w,
                    )?
                }
                None => GzDecoder::new(self.file.clone())?,
            });
        }
        let mut dec = dec.unwrap();
        // Get to a chunk boundary, then decode whole chunks up to `idx`,
        // caching each one on the way.
        let mut result = None;
        loop {
            let pos = dec.position();
            let cur = pos.div_ceil(CHUNK);
            let boundary = cur * CHUNK;
            if pos < boundary {
                let mut scratch = vec![0u8; (boundary - pos) as usize];
                if dec.skip(boundary - pos, &mut scratch)? < boundary - pos {
                    break; // the stream ended before this chunk
                }
                continue;
            }
            let mut data = vec![0u8; CHUNK as usize];
            let n = dec.read_full(&mut data)?;
            data.truncate(n);
            let data: Arc<[u8]> = data.into();
            self.cache.lock().unwrap().put(cur, data.clone());
            if cur == idx {
                result = Some(data);
                break;
            }
            if n < CHUNK as usize {
                break;
            }
        }
        if dec.position() % CHUNK == 0 {
            let mut pool = self.pool.lock().unwrap();
            if pool.len() >= POOL {
                pool.remove(0);
            }
            pool.push(dec);
        }
        Ok(result.unwrap_or_else(|| Arc::from(Vec::new())))
    }
}
