//! bzip2 streams. Files written by parallel compressors such as `pbzip2`
//! are many concatenated bzip2 streams, and each stream start is a restart
//! point that needs no state. Inside one stream, blocks start at arbitrary
//! bit positions; jumping to them is planned for a later version (M4), so
//! a single-stream file (plain `bzip2`, `lbzip2`) reads from the start.
//!
//! Data after the last stream that does not start a new one is ignored,
//! as `bzip2 -d` does.

use std::fs::File;
use std::io;
use std::sync::Arc;

use bzip2::{Decompress, Status};

use crate::stream::{invalid, Decoder, Input, Opener, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Another stream may start here; `announced` once the restart point
    /// has been reported.
    Between {
        first: bool,
        announced: bool,
    },
    Stream,
    Done,
}

pub(crate) struct Bz2Decoder {
    input: Input,
    dec: Decompress,
    phase: Phase,
    pos: u64,
}

impl Bz2Decoder {
    pub fn new(file: Arc<File>, compressed_offset: u64, pos: u64) -> Bz2Decoder {
        Bz2Decoder {
            input: Input::new(file, compressed_offset),
            dec: Decompress::new(false),
            phase: Phase::Between {
                first: true,
                announced: true,
            },
            pos,
        }
    }
}

fn is_stream_start(d: &[u8]) -> bool {
    d.len() >= 4 && &d[..3] == b"BZh" && (b'1'..=b'9').contains(&d[3])
}

impl Decoder for Bz2Decoder {
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
                Phase::Between { first, announced } => {
                    self.input.fill(4)?;
                    if !is_stream_start(self.input.data()) {
                        if first {
                            return Err(invalid("not a bzip2 stream"));
                        }
                        self.phase = Phase::Done;
                        continue;
                    }
                    if !announced {
                        self.phase = Phase::Between {
                            first,
                            announced: true,
                        };
                        return Ok(Step {
                            produced: 0,
                            boundary: true,
                        });
                    }
                    if !first {
                        self.dec = Decompress::new(false);
                    }
                    self.phase = Phase::Stream;
                }
                Phase::Stream => {
                    if !self.input.refill()? {
                        return Err(invalid("unexpected end of bzip2 data"));
                    }
                    let (in0, out0) = (self.dec.total_in(), self.dec.total_out());
                    let status = self
                        .dec
                        .decompress(self.input.data(), out)
                        .map_err(|e| invalid(format!("bzip2: {e}")))?;
                    let used = (self.dec.total_in() - in0) as usize;
                    let produced = (self.dec.total_out() - out0) as usize;
                    self.input.consume(used);
                    self.pos += produced as u64;
                    if status == Status::StreamEnd {
                        self.phase = Phase::Between {
                            first: false,
                            announced: false,
                        };
                    } else if used == 0 && produced == 0 {
                        return Err(invalid("bzip2 decoder made no progress"));
                    }
                    if produced > 0 {
                        return Ok(Step {
                            produced,
                            boundary: false,
                        });
                    }
                }
            }
        }
    }

    fn restart_point(&self) -> (u64, Vec<u8>) {
        (self.input.offset() * 8, Vec::new())
    }
}

pub(crate) fn opener(file: Arc<File>) -> Opener {
    Box::new(move |cp, _state| {
        let (off, pos) = cp.map_or((0, 0), |p| (p.bit_offset / 8, p.uncompressed_offset));
        Ok(Box::new(Bz2Decoder::new(file.clone(), off, pos)))
    })
}
