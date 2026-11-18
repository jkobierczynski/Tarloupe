//! Recompresses an archive into a form that reads fast: seekable zstd
//! (independent frames plus a seek table) or multi-block xz. The tar
//! stream inside is copied byte for byte.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::archive::{detect, sequential_decoder, Progress};
use crate::codec::Codec;
use crate::error::{Error, Result};
use crate::stream::Decoder;

/// Default amount of uncompressed data per zstd frame or xz block. Every
/// read decodes at most this much before the data it wants.
pub const DEFAULT_BLOCK_SIZE: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Seekable zstd: fast to decode, the best choice for browsing.
    Zstd,
    /// Multi-block xz: smaller, slower to decode.
    Xz,
}

impl Target {
    /// The usual file extension, with its dot.
    pub fn extension(self) -> &'static str {
        match self {
            Target::Zstd => ".tar.zst",
            Target::Xz => ".tar.xz",
        }
    }

    pub fn default_level(self) -> i32 {
        match self {
            Target::Zstd => 9,
            Target::Xz => 6,
        }
    }
}

#[derive(Clone)]
pub struct ConvertOptions {
    pub target: Target,
    /// zstd level (1-22) or xz preset (0-9); `None` for the default.
    pub level: Option<i32>,
    /// Uncompressed bytes per frame or block.
    pub block_size: u64,
    /// Compression threads (0: one per CPU).
    pub threads: usize,
    /// Replace an existing output file.
    pub overwrite: bool,
    /// Told how much of the input file has been read, now and then.
    pub progress: Option<Progress>,
    /// When set, converting stops with [`Error::Cancelled`].
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            target: Target::Zstd,
            level: None,
            block_size: DEFAULT_BLOCK_SIZE,
            threads: 0,
            overwrite: false,
            progress: None,
            cancel: None,
        }
    }
}

/// What a conversion did.
#[derive(Debug, Clone, Copy)]
pub struct ConvertStats {
    pub input_codec: Codec,
    pub input_size: u64,
    /// Size of the uncompressed tar stream.
    pub stream_size: u64,
    pub output_size: u64,
}

/// The name an archive gets when converted next to itself:
/// `big.tar.gz` or `big.tgz` becomes `big.tar.zst`.
pub fn default_output(input: &Path, target: Target) -> PathBuf {
    let name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let stem_len = [
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.zstd",
        ".tgz",
        ".tbz2",
        ".tbz",
        ".txz",
        ".tzst",
        ".tar",
    ]
    .iter()
    .find(|ext| lower.ends_with(*ext))
    .map_or(name.len(), |ext| name.len() - ext.len());
    let mut out = name[..stem_len].to_string();
    out.push_str(target.extension());
    if out.eq_ignore_ascii_case(&name) {
        // Same format already: do not collide with the input.
        out = format!("{}.seekable{}", &name[..stem_len], target.extension());
    }
    input.with_file_name(out)
}

/// Converts `input` (any supported archive) into `output`.
pub fn convert(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<ConvertStats> {
    let mut file = File::open(input)?;
    let codec = detect(&mut file)?;
    let input_size = file.metadata()?.len();
    if output.exists() {
        if !opts.overwrite {
            return Err(Error::Unsupported(format!(
                "{} already exists",
                output.display()
            )));
        }
        if fs::canonicalize(output).ok() == fs::canonicalize(input).ok() {
            return Err(Error::Unsupported(
                "the output would replace the input".into(),
            ));
        }
    }
    let file = Arc::new(file);
    let source: Box<dyn Source> = match sequential_decoder(codec, &file)? {
        Some(dec) => Box::new(DecoderSource(dec)),
        None => {
            let mut plain = file.try_clone()?;
            plain.seek(SeekFrom::Start(0))?; // detecting read the first block
            Box::new(PlainSource {
                file: plain,
                pos: 0,
            })
        }
    };
    let mut src = Watched {
        inner: source,
        progress: opts.progress.as_ref(),
        cancel: opts.cancel.as_deref(),
        total: input_size,
        last: 0,
        read: 0,
    };
    if let Some(p) = &opts.progress {
        p(0, input_size);
    }

    // Written next to the output, then renamed into place.
    let part = {
        let mut name = output.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        output.with_file_name(name)
    };
    let result = (|| -> Result<u64> {
        let out = File::create(&part)?;
        let mut w = io::BufWriter::with_capacity(1 << 20, out);
        let threads = match opts.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        let block = opts.block_size.clamp(64 << 10, 1 << 30);
        let level = opts.level.unwrap_or(opts.target.default_level());
        match opts.target {
            Target::Zstd => write_seekable_zstd(&mut src, &mut w, level, block as usize, threads)?,
            Target::Xz => crate::xz::compress(
                &mut src,
                &mut w,
                level.clamp(0, 9) as u32,
                block,
                threads as u32,
            )
            .map_err(|e| {
                if src.cancelled() {
                    Error::Cancelled
                } else {
                    e.into()
                }
            })?,
        }
        let out = w.into_inner().map_err(|e| e.into_error())?;
        out.sync_all()?;
        Ok(out.metadata()?.len())
    })();
    let output_size = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(if src.cancelled() { Error::Cancelled } else { e });
        }
    };
    if opts.overwrite {
        let _ = fs::remove_file(output);
    }
    if let Err(e) = fs::rename(&part, output) {
        let _ = fs::remove_file(&part);
        return Err(e.into());
    }
    if let Some(p) = &opts.progress {
        p(input_size, input_size);
    }
    Ok(ConvertStats {
        input_codec: codec,
        input_size,
        stream_size: src.read,
        output_size,
    })
}

/// Compresses frames of `block` bytes on `threads` threads, in order,
/// then appends the seek table.
fn write_seekable_zstd(
    r: &mut dyn Read,
    w: &mut dyn Write,
    level: i32,
    block: usize,
    threads: usize,
) -> Result<()> {
    let mut table = Vec::new();
    loop {
        let mut batch = Vec::with_capacity(threads);
        let mut eof = false;
        while batch.len() < threads && !eof {
            let mut buf = vec![0u8; block];
            let n = read_full(r, &mut buf)?;
            buf.truncate(n);
            eof = n < block;
            if n > 0 {
                batch.push(buf);
            }
        }
        let frames: Vec<io::Result<Vec<u8>>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|data| s.spawn(move || crate::zstd::compress_frame(data, level)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("compression thread panicked"))
                .collect()
        });
        for (data, frame) in batch.iter().zip(frames) {
            let frame = frame?;
            w.write_all(&frame)?;
            table.push((frame.len() as u32, data.len() as u32));
        }
        if eof {
            break;
        }
    }
    w.write_all(&crate::zstd::seek_table(&table))?;
    Ok(())
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// The uncompressed tar stream of the input.
trait Source: Read {
    /// Bytes of the input file consumed so far.
    fn consumed(&self) -> u64;
}

struct DecoderSource(Box<dyn Decoder>);

impl Read for DecoderSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let s = self.0.step(buf)?;
            if s.produced > 0 || !s.boundary {
                return Ok(s.produced);
            }
        }
    }
}

impl Source for DecoderSource {
    fn consumed(&self) -> u64 {
        self.0.compressed_position()
    }
}

struct PlainSource {
    file: File,
    pos: u64,
}

impl Read for PlainSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Source for PlainSource {
    fn consumed(&self) -> u64 {
        self.pos
    }
}

/// Reports progress and stops reading once cancelled.
struct Watched<'a> {
    inner: Box<dyn Source>,
    progress: Option<&'a Progress>,
    cancel: Option<&'a AtomicBool>,
    total: u64,
    last: u64,
    /// Uncompressed bytes read.
    read: u64,
}

impl Watched<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

impl Read for Watched<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancelled() {
            return Err(io::Error::other("conversion was cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if let Some(p) = self.progress {
            let done = self.inner.consumed().min(self.total);
            if done.abs_diff(self.last) >= (self.total / 100).max(1 << 20) {
                self.last = done;
                p(done, self.total);
            }
        }
        Ok(n)
    }
}
