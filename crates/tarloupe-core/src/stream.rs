//! Codec-neutral random access into a compressed stream.
//!
//! Every codec supplies a [`Decoder`] that decodes forward and says when it
//! stands on a restart point (a gzip deflate block boundary, a zstd frame
//! boundary, an xz block start, a bzip2 stream start). The indexing pass
//! records some of those points as checkpoints, and [`RandomReader`] serves
//! reads by decoding from the nearest checkpoint before them, through a
//! chunk cache and a pool of decoders left where earlier reads stopped.
//!
//! A stream without restart points (one huge gzip block, a single-frame
//! zstd file, a single-block xz file) still works: reads then decode from
//! the start of the file, and the pool makes sequential reads cheap.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Decoded data is cached in aligned chunks of this size.
const CHUNK: u64 = 1024 * 1024;
/// Chunks kept in the cache (64 MiB).
const CACHE_CHUNKS: usize = 64;
/// Idle decoders kept for continuing sequential reads.
const POOL: usize = 8;

/// A decoder restart point, as stored in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Offset in the uncompressed stream.
    pub uncompressed_offset: u64,
    /// Position in the compressed file, in bits from its start.
    pub bit_offset: u64,
    /// Codec state needed to resume there (gzip: the zlib-compressed
    /// 32 KiB window; xz: the stream's check type; others: empty).
    pub window: Vec<u8>,
}

/// A checkpoint as kept in memory (the state is loaded on demand).
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub uncompressed_offset: u64,
    pub bit_offset: u64,
}

/// Result of one decoding step.
pub(crate) struct Step {
    pub produced: usize,
    /// The step ended on a restart point where a checkpoint may be taken.
    pub boundary: bool,
}

/// A forward decoder of one codec.
pub(crate) trait Decoder: Send {
    /// Uncompressed bytes produced so far (absolute stream offset).
    fn position(&self) -> u64;

    /// Bytes of the compressed file consumed so far (for progress).
    fn compressed_position(&self) -> u64;

    /// Decodes into `out` (which must not be empty). `produced == 0` with
    /// no boundary means the stream has ended.
    fn step(&mut self, out: &mut [u8]) -> io::Result<Step>;

    /// Where the decoder stands, as a checkpoint: only meaningful right
    /// after a step that reported a boundary. Gzip records its own
    /// checkpoints (they need the output window) and never calls this.
    fn restart_point(&self) -> (u64, Vec<u8>) {
        unreachable!("this codec records its own checkpoints")
    }

    /// Fills `buf` completely unless the stream ends; returns bytes written.
    fn read_full(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
    fn skip(&mut self, mut n: u64, scratch: &mut [u8]) -> io::Result<u64> {
        let start = self.position();
        while n > 0 {
            let want = (scratch.len() as u64).min(n) as usize;
            let got = self.read_full(&mut scratch[..want])?;
            if got == 0 {
                break;
            }
            n -= got as u64;
        }
        Ok(self.position() - start)
    }
}

/// Loads the state stored with the checkpoint at an uncompressed offset.
pub(crate) type StateLoader<'a> = &'a dyn Fn(u64) -> io::Result<Vec<u8>>;

/// Makes a decoder at a checkpoint, or at the start of the file for `None`.
pub(crate) type Opener =
    Box<dyn Fn(Option<Point>, StateLoader) -> io::Result<Box<dyn Decoder>> + Send + Sync>;

/// Reading adapter for the indexing pass of codecs whose restart points
/// need no output history: records a checkpoint at the first boundary
/// after every `spacing` bytes of output.
pub(crate) struct Indexer {
    dec: Box<dyn Decoder>,
    buf: Box<[u8]>,
    pending: std::ops::Range<usize>,
    spacing: u64,
    last: u64,
    cancel: Option<Arc<AtomicBool>>,
    pub checkpoints: Vec<Checkpoint>,
}

impl Indexer {
    pub fn new(dec: Box<dyn Decoder>, spacing: u64, cancel: Option<Arc<AtomicBool>>) -> Indexer {
        Indexer {
            dec,
            cancel,
            buf: vec![0u8; 256 * 1024].into_boxed_slice(),
            pending: 0..0,
            spacing: spacing.max(1),
            last: 0,
            checkpoints: Vec::new(),
        }
    }

    pub fn compressed_position(&self) -> u64 {
        self.dec.compressed_position()
    }

    /// Uncompressed bytes decoded so far.
    pub fn position(&self) -> u64 {
        self.dec.position()
    }
}

impl io::Read for Indexer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            // Steps that only pass frames without data do not return to
            // the caller, so cancelling is checked here too.
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                return Err(io::Error::other("indexing was cancelled"));
            }
            let s = self.dec.step(&mut self.buf)?;
            self.pending = 0..s.produced;
            if s.boundary {
                // Output of this step comes before the boundary.
                let at = self.dec.position();
                if at > 0 && at - self.last >= self.spacing {
                    let (bit_offset, window) = self.dec.restart_point();
                    self.last = at;
                    self.checkpoints.push(Checkpoint {
                        uncompressed_offset: at,
                        bit_offset,
                        window,
                    });
                }
            }
            if s.produced == 0 && !s.boundary {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.buf[self.pending.start..self.pending.start + n]);
        self.pending.start += n;
        Ok(n)
    }
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

/// Random-access reader over a compressed file with a loaded checkpoint list.
pub(crate) struct RandomReader {
    points: Vec<Point>,
    open: Opener,
    cache: Mutex<ChunkCache>,
    /// Idle decoders, each positioned at a chunk boundary.
    pool: Mutex<Vec<Box<dyn Decoder>>>,
}

impl RandomReader {
    pub fn new(points: Vec<Point>, open: Opener) -> RandomReader {
        RandomReader {
            points,
            open,
            cache: Mutex::new(ChunkCache {
                map: HashMap::new(),
                tick: 0,
            }),
            pool: Mutex::new(Vec::new()),
        }
    }

    /// Fills `buf` from uncompressed offset `off`; fails at end of data.
    pub fn read_exact_at(&self, buf: &mut [u8], off: u64, state: StateLoader) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = off + done as u64;
            let idx = at / CHUNK;
            let chunk = self.chunk(idx, state)?;
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

    fn chunk(&self, idx: u64, state: StateLoader) -> io::Result<Arc<[u8]>> {
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
        let pooled = {
            let mut pool = self.pool.lock().unwrap();
            let best = pool
                .iter()
                .enumerate()
                .filter(|(_, d)| d.position() <= start && d.position() >= cp_off)
                .max_by_key(|(_, d)| d.position())
                .map(|(i, _)| i);
            best.map(|i| pool.swap_remove(i))
        };
        let mut dec = match pooled {
            Some(d) => d,
            None => (self.open)(cp, state)?,
        };
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

/// Buffered positional reads of the compressed file, shared by the
/// decoders that parse their container themselves.
pub(crate) struct Input {
    pub file: Arc<std::fs::File>,
    pub buf: Box<[u8]>,
    pub start: usize,
    pub end: usize,
    /// File offset of `buf[end]`.
    pub next_read: u64,
}

pub(crate) const INPUT_BUF: usize = 128 * 1024;

impl Input {
    pub fn new(file: Arc<std::fs::File>, at: u64) -> Input {
        Input {
            file,
            buf: vec![0u8; INPUT_BUF].into_boxed_slice(),
            start: 0,
            end: 0,
            next_read: at,
        }
    }

    /// File offset of the next unconsumed byte.
    pub fn offset(&self) -> u64 {
        self.next_read - (self.end - self.start) as u64
    }

    pub fn data(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    pub fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.end - self.start);
        self.start += n;
    }

    /// Refills the buffer when it is empty. Returns false at the end of the file.
    pub fn refill(&mut self) -> io::Result<bool> {
        if self.start < self.end {
            return Ok(true);
        }
        let n = read_at(&self.file, &mut self.buf, self.next_read)?;
        self.start = 0;
        self.end = n;
        self.next_read += n as u64;
        Ok(n > 0)
    }

    /// Makes at least `n` bytes (n <= INPUT_BUF) available, unless the
    /// file ends first. Returns how many are available.
    pub fn fill(&mut self, n: usize) -> io::Result<usize> {
        debug_assert!(n <= INPUT_BUF);
        if self.end - self.start >= n {
            return Ok(self.end - self.start);
        }
        self.buf.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < n {
            let k = read_at(&self.file, &mut self.buf[self.end..], self.next_read)?;
            if k == 0 {
                break;
            }
            self.end += k;
            self.next_read += k as u64;
        }
        Ok(self.end)
    }
}

pub(crate) fn read_at(file: &std::fs::File, buf: &mut [u8], off: u64) -> io::Result<usize> {
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

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}
