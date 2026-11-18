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

use std::fs::File;
use std::io;
use std::sync::Arc;

use libz_rs_sys as z;

use crate::stream::{invalid, read_at, Decoder, Opener, Step};
pub use crate::stream::{Checkpoint, Point};

/// Size of the deflate history window.
pub const WINDOW: usize = 32 * 1024;
/// Default distance between checkpoints, in uncompressed bytes.
pub const DEFAULT_SPACING: u64 = 8 * 1024 * 1024;
const INPUT_BUF: usize = 128 * 1024;

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
}

impl Decoder for GzDecoder {
    fn position(&self) -> u64 {
        self.pos
    }

    fn compressed_position(&self) -> u64 {
        self.next_read
    }

    /// Decodes into `out` (which must not be empty). `produced == 0` with
    /// no boundary means the stream has ended.
    fn step(&mut self, out: &mut [u8]) -> io::Result<Step> {
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

    /// Uncompressed bytes decoded so far.
    pub fn position(&self) -> u64 {
        self.dec.position()
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

/// Makes gzip decoders for a [`crate::stream::RandomReader`]; the state
/// of a checkpoint is its packed window.
pub(crate) fn opener(file: Arc<File>) -> Opener {
    Box::new(move |cp, state| {
        Ok(Box::new(match cp {
            Some(p) => {
                let w = unpack_window(&state(p.uncompressed_offset)?)?;
                GzDecoder::at_checkpoint(file.clone(), p.uncompressed_offset, p.bit_offset, &w)?
            }
            None => GzDecoder::new(file.clone())?,
        }))
    })
}
