//! xz streams. An xz file is one or more streams, each a sequence of
//! independently compressed blocks followed by an index. Every block start
//! is a restart point; resuming there only needs the stream's check type.
//! Multi-threaded `xz -T`, `pixz` and `tarloupe convert` write many blocks
//! and read fast. A single-block file (plain `xz` before 5.6, `xz -T1`)
//! has no restart point inside and reads from the start.
//!
//! This parses the container itself and decodes each block with
//! liblzma's block decoder, which also verifies the block's check.

use std::fs::File;
use std::io;
use std::os::raw::c_void;
use std::sync::Arc;

use liblzma_sys as lz;

use crate::stream::{invalid, Decoder, Input, Opener, Step};

const HEADER_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
const FOOTER_MAGIC: [u8; 2] = *b"YZ";

/// `lzma_block` from `lzma/block.h`; `liblzma-sys` only exposes it with
/// its `bindgen` feature, which needs libclang.
#[repr(C)]
struct LzmaBlock {
    version: u32,
    header_size: u32,
    check: lz::lzma_check,
    compressed_size: lz::lzma_vli,
    uncompressed_size: lz::lzma_vli,
    filters: *mut lz::lzma_filter,
    raw_check: [u8; 64],
    reserved_ptr: [*mut c_void; 3],
    reserved_int32: [u32; 2],
    reserved_int64: [lz::lzma_vli; 6],
    reserved_enum: [u32; 4],
    ignore_check: u8,
    reserved_bool: [u8; 7],
}

const _: () =
    assert!(std::mem::size_of::<LzmaBlock>() == 208 || cfg!(not(target_pointer_width = "64")));

extern "C" {
    fn lzma_block_header_decode(
        block: *mut LzmaBlock,
        allocator: *const lz::lzma_allocator,
        input: *const u8,
    ) -> lz::lzma_ret;
    fn lzma_block_decoder(strm: *mut lz::lzma_stream, block: *mut LzmaBlock) -> lz::lzma_ret;
}

/// Owns one liblzma stream. Boxed because liblzma keeps a pointer to it.
struct Stream(Box<lz::lzma_stream>);

// The stream is exclusively owned and only touched through `&mut self`.
unsafe impl Send for Stream {}
// Only the decoder that owns it (through its stream) uses it.
unsafe impl Send for LzmaBlock {}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { lz::lzma_end(&mut *self.0) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// A stream header is expected.
    StreamHeader {
        first: bool,
    },
    /// A block header or the index follows; `announced` once the restart
    /// point here has been reported.
    BlockStart {
        announced: bool,
    },
    Block,
    /// Stream padding, then another stream or the end of the file.
    Padding,
    Done,
}

pub(crate) struct XzDecoder {
    input: Input,
    strm: Stream,
    /// The block decoder keeps a pointer to this until the block ends.
    block: Box<LzmaBlock>,
    phase: Phase,
    check: u32,
    pos: u64,
}

impl XzDecoder {
    /// A decoder at the start of the file.
    pub fn new(file: Arc<File>) -> XzDecoder {
        XzDecoder {
            input: Input::new(file, 0),
            strm: Stream(Box::new(unsafe { std::mem::zeroed() })),
            block: Box::new(unsafe { std::mem::zeroed() }),
            phase: Phase::StreamHeader { first: true },
            check: 0,
            pos: 0,
        }
    }

    /// A decoder at the start of a block, in a stream with check type `check`.
    pub fn at_block(file: Arc<File>, offset: u64, pos: u64, check: u32) -> XzDecoder {
        XzDecoder {
            input: Input::new(file, offset),
            strm: Stream(Box::new(unsafe { std::mem::zeroed() })),
            block: Box::new(unsafe { std::mem::zeroed() }),
            phase: Phase::BlockStart { announced: true },
            check,
            pos,
        }
    }

    fn stream_header(&mut self, first: bool) -> io::Result<()> {
        let n = self.input.fill(12)?;
        if n == 0 && !first {
            self.phase = Phase::Done;
            return Ok(());
        }
        let h = self.input.data();
        if n < 12 || h[..6] != HEADER_MAGIC {
            return Err(invalid(if first {
                "not an xz stream"
            } else {
                "unexpected data after an xz stream"
            }));
        }
        let crc = unsafe { lz::lzma_crc32(h[6..8].as_ptr(), 2, 0) };
        if h[6] != 0 || h[7] & 0xf0 != 0 || crc.to_le_bytes() != h[8..12] {
            return Err(invalid("unsupported or corrupt xz stream header"));
        }
        self.check = (h[7] & 0x0f) as u32;
        self.input.consume(12);
        self.phase = Phase::BlockStart { announced: false };
        Ok(())
    }

    fn block_header(&mut self) -> io::Result<()> {
        let size = (self.input.data()[0] as usize + 1) * 4;
        if self.input.fill(size)? < size {
            return Err(invalid("truncated xz block header"));
        }
        let mut filters: [lz::lzma_filter; 5] = unsafe { std::mem::zeroed() };
        let block = &mut *self.block;
        *block = unsafe { std::mem::zeroed() };
        block.version = 1;
        block.header_size = size as u32;
        block.check = self.check as lz::lzma_check;
        block.filters = filters.as_mut_ptr();
        let ret = unsafe {
            lzma_block_header_decode(block, std::ptr::null(), self.input.data().as_ptr())
        };
        if ret != lz::LZMA_OK {
            return Err(invalid(format!(
                "corrupt or unsupported xz block header ({ret})"
            )));
        }
        let ret = unsafe { lzma_block_decoder(&mut *self.strm.0, block) };
        // The decoder copied what it needs from the filter options.
        unsafe { lz::lzma_filters_free(filters.as_mut_ptr(), std::ptr::null()) };
        block.filters = std::ptr::null_mut();
        if ret != lz::LZMA_OK {
            return Err(invalid(format!("cannot decode this xz block ({ret})")));
        }
        self.input.consume(size);
        self.phase = Phase::Block;
        Ok(())
    }

    /// Reads one index byte, keeping it for the index CRC.
    fn byte(&mut self, seen: &mut Vec<u8>) -> io::Result<u8> {
        if !self.input.refill()? {
            return Err(invalid("truncated xz index"));
        }
        let b = self.input.data()[0];
        self.input.consume(1);
        seen.push(b);
        Ok(b)
    }

    fn varint(&mut self, seen: &mut Vec<u8>) -> io::Result<u64> {
        let mut v = 0u64;
        for i in 0..9 {
            let b = self.byte(seen)?;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(invalid("corrupt xz index"))
    }

    /// Checks the index (its indicator byte is next) and the stream footer.
    /// The block records themselves are not needed: every block was (or,
    /// after a checkpoint, will be) verified by its own check.
    fn index(&mut self) -> io::Result<()> {
        let mut seen = Vec::new();
        self.byte(&mut seen)?;
        let records = self.varint(&mut seen)?;
        for _ in 0..records {
            self.varint(&mut seen)?;
            self.varint(&mut seen)?;
        }
        let pad = (4 - seen.len() % 4) % 4;
        // Padding, the index CRC32, then the 12-byte footer.
        let rest = pad + 4 + 12;
        if self.input.fill(rest)? < rest {
            return Err(invalid("truncated xz stream footer"));
        }
        let d = &self.input.data()[..rest];
        let crc32 = |b: &[u8], init: u32| unsafe { lz::lzma_crc32(b.as_ptr(), b.len(), init) };
        let (padding, crc, footer) = (&d[..pad], &d[pad..pad + 4], &d[pad + 4..rest]);
        let index_size = (seen.len() + pad + 4) as u64;
        let backward = (u32::from_le_bytes(footer[4..8].try_into().unwrap()) as u64 + 1) * 4;
        if padding.iter().any(|&b| b != 0) || crc32(padding, crc32(&seen, 0)).to_le_bytes() != crc {
            return Err(invalid("corrupt xz index"));
        }
        if footer[10..12] != FOOTER_MAGIC
            || crc32(&footer[4..10], 0).to_le_bytes() != footer[..4]
            || backward != index_size
            || footer[8] != 0
            || footer[9] as u32 != self.check
        {
            return Err(invalid("corrupt xz stream footer"));
        }
        self.input.consume(rest);
        self.phase = Phase::Padding;
        Ok(())
    }

    fn padding(&mut self) -> io::Result<()> {
        let mut zeros = 0u64;
        loop {
            if !self.input.refill()? {
                self.phase = Phase::Done;
                break;
            }
            let d = self.input.data();
            let k = d.iter().take_while(|&&b| b == 0).count();
            let all = k == d.len();
            zeros += k as u64;
            self.input.consume(k);
            if !all {
                self.phase = Phase::StreamHeader { first: false };
                break;
            }
        }
        if !zeros.is_multiple_of(4) {
            return Err(invalid("corrupt xz stream padding"));
        }
        Ok(())
    }
}

impl Decoder for XzDecoder {
    fn position(&self) -> u64 {
        self.pos
    }

    fn compressed_position(&self) -> u64 {
        self.input.next_read
    }

    fn step(&mut self, out: &mut [u8]) -> io::Result<Step> {
        loop {
            match self.phase {
                Phase::Done => {
                    return Ok(Step {
                        produced: 0,
                        boundary: false,
                    })
                }
                Phase::StreamHeader { first } => self.stream_header(first)?,
                Phase::Padding => self.padding()?,
                Phase::BlockStart { announced } => {
                    if !self.input.refill()? {
                        return Err(invalid("unexpected end of xz data"));
                    }
                    if self.input.data()[0] == 0 {
                        self.index()?;
                    } else if !announced {
                        self.phase = Phase::BlockStart { announced: true };
                        return Ok(Step {
                            produced: 0,
                            boundary: true,
                        });
                    } else {
                        self.block_header()?;
                    }
                }
                Phase::Block => {
                    let more = self.input.refill()?;
                    let s = &mut *self.strm.0;
                    let data = self.input.data();
                    s.next_in = data.as_ptr();
                    s.avail_in = data.len() as _;
                    s.next_out = out.as_mut_ptr();
                    s.avail_out = out.len() as _;
                    let ret = unsafe { lz::lzma_code(s, lz::LZMA_RUN) };
                    let used = data.len() - s.avail_in;
                    let produced = out.len() - s.avail_out;
                    self.input.consume(used);
                    self.pos += produced as u64;
                    match ret {
                        lz::LZMA_OK => {}
                        lz::LZMA_STREAM_END => {
                            self.phase = Phase::BlockStart { announced: false };
                        }
                        lz::LZMA_BUF_ERROR if !more => {
                            return Err(invalid("unexpected end of xz data"))
                        }
                        lz::LZMA_BUF_ERROR => {}
                        lz::LZMA_MEM_ERROR => return Err(io::Error::other("xz: out of memory")),
                        lz::LZMA_DATA_ERROR => return Err(invalid("xz: corrupt data")),
                        _ => return Err(invalid(format!("xz: decoding failed ({ret})"))),
                    }
                    if produced > 0 {
                        return Ok(Step {
                            produced,
                            boundary: false,
                        });
                    }
                    if used == 0 && ret == lz::LZMA_OK && !more {
                        return Err(invalid("unexpected end of xz data"));
                    }
                }
            }
        }
    }

    fn restart_point(&self) -> (u64, Vec<u8>) {
        (self.input.offset() * 8, vec![self.check as u8])
    }
}

pub(crate) fn opener(file: Arc<File>) -> Opener {
    Box::new(move |cp, state| {
        Ok(Box::new(match cp {
            Some(p) => {
                let check = state(p.uncompressed_offset)?;
                let &[check] = check.as_slice() else {
                    return Err(invalid("corrupt xz checkpoint in the index"));
                };
                XzDecoder::at_block(
                    file.clone(),
                    p.bit_offset / 8,
                    p.uncompressed_offset,
                    check as u32,
                )
            }
            None => XzDecoder::new(file.clone()),
        }))
    })
}

/// Compresses a stream as xz with blocks of `block_size` uncompressed
/// bytes, on `threads` threads, writing everything to `w`.
pub fn compress(
    r: &mut dyn io::Read,
    w: &mut dyn io::Write,
    preset: u32,
    block_size: u64,
    threads: u32,
) -> io::Result<()> {
    let mut strm = Stream(Box::new(unsafe { std::mem::zeroed() }));
    let mut mt: lz::lzma_mt = unsafe { std::mem::zeroed() };
    mt.threads = threads.max(1);
    mt.block_size = block_size;
    mt.preset = preset;
    mt.check = lz::LZMA_CHECK_CRC64;
    let ret = unsafe { lz::lzma_stream_encoder_mt(&mut *strm.0, &mt) };
    if ret != lz::LZMA_OK {
        return Err(io::Error::other(format!(
            "cannot start the xz encoder ({ret})"
        )));
    }
    let mut inbuf = vec![0u8; 1 << 20];
    let mut outbuf = vec![0u8; 1 << 20];
    let (mut in_len, mut eof) = (0usize, false);
    let mut in_pos = 0usize;
    loop {
        if in_pos == in_len && !eof {
            in_len = r.read(&mut inbuf)?;
            in_pos = 0;
            eof = in_len == 0;
        }
        let s = &mut *strm.0;
        s.next_in = inbuf[in_pos..in_len].as_ptr();
        s.avail_in = (in_len - in_pos) as _;
        s.next_out = outbuf.as_mut_ptr();
        s.avail_out = outbuf.len() as _;
        let action = if eof { lz::LZMA_FINISH } else { lz::LZMA_RUN };
        let ret = unsafe { lz::lzma_code(s, action) };
        in_pos = in_len - s.avail_in as usize;
        let produced = outbuf.len() - s.avail_out as usize;
        w.write_all(&outbuf[..produced])?;
        match ret {
            lz::LZMA_OK => {}
            lz::LZMA_STREAM_END => return Ok(()),
            _ => return Err(io::Error::other(format!("xz encoding failed ({ret})"))),
        }
    }
}
