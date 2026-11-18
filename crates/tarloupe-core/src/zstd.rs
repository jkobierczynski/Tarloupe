//! zstd streams. Every frame is independent, so each frame boundary is a
//! restart point that needs no state: files written as many frames (the
//! seekable format, `pzstd`, `tarloupe convert`) read fast. A single-frame
//! file (what `zstd` writes, even with `-T`) has no restart point and reads
//! from the start.
//!
//! Skippable frames, such as the seek table of the seekable format, are
//! passed over by the decoder.

use std::fs::File;
use std::io;
use std::sync::Arc;

use zstd_safe::{get_error_name, DCtx, DParameter, InBuffer, OutBuffer};

use crate::stream::{invalid, Decoder, Input, Opener, Step};

/// Largest window accepted (`zstd --long=31` writes up to 2 GiB windows).
const WINDOW_LOG_MAX: u32 = 31;

pub(crate) struct ZstdDecoder {
    input: Input,
    dctx: DCtx<'static>,
    pos: u64,
    /// Part of a frame has been consumed.
    in_frame: bool,
    done: bool,
}

impl ZstdDecoder {
    pub fn new(file: Arc<File>, compressed_offset: u64, pos: u64) -> io::Result<ZstdDecoder> {
        let mut dctx = DCtx::create();
        dctx.set_parameter(DParameter::WindowLogMax(WINDOW_LOG_MAX))
            .map_err(|c| io::Error::other(format!("zstd: {}", get_error_name(c))))?;
        Ok(ZstdDecoder {
            input: Input::new(file, compressed_offset),
            dctx,
            pos,
            in_frame: false,
            done: false,
        })
    }
}

impl Decoder for ZstdDecoder {
    fn position(&self) -> u64 {
        self.pos
    }

    fn compressed_position(&self) -> u64 {
        self.input.next_read
    }

    fn step(&mut self, out: &mut [u8]) -> io::Result<Step> {
        loop {
            if self.done {
                return Ok(Step {
                    produced: 0,
                    boundary: false,
                });
            }
            if !self.input.refill()? {
                if self.in_frame {
                    return Err(invalid("unexpected end of zstd data"));
                }
                self.done = true;
                continue;
            }
            let (used, produced, hint) = {
                let mut inb = InBuffer::around(self.input.data());
                let mut outb = OutBuffer::around(&mut *out);
                let hint = self
                    .dctx
                    .decompress_stream(&mut outb, &mut inb)
                    .map_err(|c| invalid(format!("zstd: {}", get_error_name(c))))?;
                (inb.pos, outb.pos(), hint)
            };
            self.input.consume(used);
            self.pos += produced as u64;
            if hint == 0 {
                // A frame (or a skippable frame) ended and is fully flushed.
                self.in_frame = false;
                return Ok(Step {
                    produced,
                    boundary: true,
                });
            }
            if used > 0 || produced > 0 {
                self.in_frame = true;
            }
            if produced > 0 {
                return Ok(Step {
                    produced,
                    boundary: false,
                });
            }
            if used == 0 && !self.input.data().is_empty() {
                return Err(invalid("zstd decoder made no progress"));
            }
        }
    }

    fn restart_point(&self) -> (u64, Vec<u8>) {
        (self.input.offset() * 8, Vec::new())
    }
}

pub(crate) fn opener(file: Arc<File>) -> Opener {
    Box::new(move |cp, _state| {
        Ok(Box::new(match cp {
            Some(p) => ZstdDecoder::new(file.clone(), p.bit_offset / 8, p.uncompressed_offset)?,
            None => ZstdDecoder::new(file.clone(), 0, 0)?,
        }))
    })
}

/// Magic number of a skippable frame holding a seek table.
const SEEK_TABLE_SKIPPABLE: u32 = 0x184D_2A5E;
/// Magic number at the very end of a seek table.
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;

/// Builds the seek table frame of the zstd seekable format for frames of
/// the given (compressed, uncompressed) sizes, without checksums.
pub fn seek_table(frames: &[(u32, u32)]) -> Vec<u8> {
    let payload = frames.len() * 8 + 9;
    let mut t = Vec::with_capacity(payload + 8);
    t.extend_from_slice(&SEEK_TABLE_SKIPPABLE.to_le_bytes());
    t.extend_from_slice(&(payload as u32).to_le_bytes());
    for &(c, d) in frames {
        t.extend_from_slice(&c.to_le_bytes());
        t.extend_from_slice(&d.to_le_bytes());
    }
    t.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    t.push(0); // descriptor: no checksums
    t.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
    t
}

/// Compresses `data` as one frame with a content checksum.
pub fn compress_frame(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    let mut cctx = zstd_safe::CCtx::create();
    let err = |c| io::Error::other(format!("zstd: {}", get_error_name(c)));
    cctx.set_parameter(zstd_safe::CParameter::CompressionLevel(level))
        .map_err(err)?;
    cctx.set_parameter(zstd_safe::CParameter::ChecksumFlag(true))
        .map_err(err)?;
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(err)?;
    Ok(out)
}
