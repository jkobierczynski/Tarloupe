//! High-level API: open an archive with its index, browse, and read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codec::Codec;
use crate::entry::{Entry, EntryKind};
use crate::error::{Error, Result};
use crate::gzip::{self, GzRandom, IndexingReader};
use crate::index::{build_tree, default_index_dir, Index};
use crate::key::ArchiveKey;
use crate::path::{normalize, resolve_link};
use crate::walker::{ReadSource, SeekSource, Walker};

/// Maximum symlink/hard-link hops when resolving an entry to its data.
const MAX_LINK_HOPS: usize = 40;

/// Called while an index is built with the bytes of the archive file read
/// so far and its total size.
pub type Progress = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone, Default)]
pub struct OpenOptions {
    /// Explicit index file (e.g. a sidecar `archive.tar.tli`).
    pub index_path: Option<PathBuf>,
    /// Directory for keyed index files; defaults to [`default_index_dir`].
    pub index_dir: Option<PathBuf>,
    /// Rebuild the index even if a valid one exists.
    pub rebuild: bool,
    /// Fail instead of building when no valid index exists.
    pub require_index: bool,
    /// Distance between gzip checkpoints when building an index, in
    /// uncompressed bytes (default 8 MiB). Smaller is faster for random
    /// reads and makes the index larger.
    pub checkpoint_spacing: Option<u64>,
    /// Told how far indexing has got, now and then (only when an index is
    /// built).
    pub progress: Option<Progress>,
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("index_path", &self.index_path)
            .field("index_dir", &self.index_dir)
            .field("rebuild", &self.rebuild)
            .field("require_index", &self.require_index)
            .field("checkpoint_spacing", &self.checkpoint_spacing)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

/// What [`Archive::open`] did about the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    Loaded,
    Built,
    Rebuilt,
}

pub struct Archive {
    path: PathBuf,
    file: Arc<File>,
    codec: Codec,
    index: Index,
    status: IndexStatus,
    gzip: Option<GzRandom>,
}

impl Archive {
    /// Opens an archive, loading its index or building it with one pass.
    pub fn open(path: impl AsRef<Path>, opts: &OpenOptions) -> Result<Archive> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let codec = detect(&mut file)?;
        if !matches!(codec, Codec::None | Codec::Gzip) {
            return Err(Error::UnsupportedCodec {
                codec: codec.name(),
                milestone: codec.milestone(),
            });
        }
        // Key and index come from the handle we keep for reads, so they
        // describe the same file even if the path is replaced meanwhile.
        let key = ArchiveKey::from_file(&file)?;
        let file = Arc::new(file);
        let spacing = opts
            .checkpoint_spacing
            .unwrap_or(gzip::DEFAULT_SPACING)
            .max(64 * 1024);
        let index_path = match &opts.index_path {
            Some(p) => p.clone(),
            None => opts
                .index_dir
                .clone()
                .unwrap_or_else(default_index_dir)
                .join(format!("{}.tli", key.id())),
        };
        // Building replaces the index file, so it must never be the archive.
        if same_file(&path, &index_path) {
            return Err(Error::Unsupported(format!(
                "the index path {} is the archive itself",
                index_path.display()
            )));
        }

        let (index, status) = if !opts.rebuild && index_path.exists() {
            match Index::open(&index_path, &key) {
                Ok(i) => (i, IndexStatus::Loaded),
                Err(e @ (Error::StaleIndex { .. } | Error::Sqlite(_))) if opts.require_index => {
                    return Err(e)
                }
                Err(Error::StaleIndex { .. }) | Err(Error::Sqlite(_)) => (
                    build_index(
                        &file,
                        &index_path,
                        &key,
                        codec,
                        spacing,
                        opts.progress.as_ref(),
                    )?,
                    IndexStatus::Rebuilt,
                ),
                Err(e) => return Err(e),
            }
        } else if opts.require_index && !opts.rebuild {
            return Err(Error::NotFound(format!("index {}", index_path.display())));
        } else {
            let st = if index_path.exists() {
                IndexStatus::Rebuilt
            } else {
                IndexStatus::Built
            };
            (
                build_index(
                    &file,
                    &index_path,
                    &key,
                    codec,
                    spacing,
                    opts.progress.as_ref(),
                )?,
                st,
            )
        };

        let gzip = match codec {
            Codec::Gzip => Some(GzRandom::new(file.clone(), index.checkpoints()?)),
            _ => None,
        };
        Ok(Archive {
            path,
            file,
            codec,
            index,
            status,
            gzip,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn codec(&self) -> Codec {
        self.codec
    }
    pub fn index(&self) -> &Index {
        &self.index
    }
    pub fn index_path(&self) -> &Path {
        self.index.path()
    }
    pub fn index_status(&self) -> IndexStatus {
        self.status
    }

    /// Members in archive order, as `tar -t` lists them.
    pub fn for_each_member(&self, f: impl FnMut(Entry) -> Result<()>) -> Result<()> {
        self.index.for_each_member(f)
    }

    /// Looks up the current entry for a path (`""` or `"/"` is the root,
    /// which returns `None` unless the archive has an explicit `./` member).
    pub fn lookup(&self, path: &[u8]) -> Result<Option<Entry>> {
        self.index.lookup(&normalize(path))
    }

    /// Lists a directory. `path` may be empty for the archive root.
    pub fn list_dir(&self, path: &[u8]) -> Result<Vec<Entry>> {
        let norm = normalize(path);
        if norm.is_empty() {
            return self.index.children(0);
        }
        let e = self.resolve(
            self.index
                .lookup(&norm)?
                .ok_or_else(|| Error::NotFound(String::from_utf8_lossy(path).into_owned()))?,
        )?;
        if e.kind != EntryKind::Dir {
            return Err(Error::Unsupported(format!(
                "{} is not a directory",
                e.display_path()
            )));
        }
        // The root (an explicit `./` member, or a link to `/`) lists the
        // top-level entries, whose parent is the virtual root 0.
        self.index
            .children(if e.path.is_empty() { 0 } else { e.id })
    }

    /// Follows hard links and symlinks until reaching an entry that has
    /// its own data (or is a directory, device, ...).
    pub fn resolve(&self, mut e: Entry) -> Result<Entry> {
        for _ in 0..MAX_LINK_HOPS {
            let target = match e.kind {
                EntryKind::HardLink => {
                    // A hard link names data archived before it, which may
                    // since have been superseded (even by the link itself).
                    let target = normalize(&e.link_target);
                    e = self.index.lookup_before(&target, e.id)?.ok_or_else(|| {
                        Error::NotFound(format!(
                            "{} => {} (hard link target not found)",
                            e.display_path(),
                            String::from_utf8_lossy(&target)
                        ))
                    })?;
                    continue;
                }
                EntryKind::Symlink => resolve_link(&e.path, &e.link_target).ok_or_else(|| {
                    Error::NotFound(format!(
                        "{} -> {} (escapes the archive)",
                        e.display_path(),
                        String::from_utf8_lossy(&e.link_target)
                    ))
                })?,
                _ => return Ok(e),
            };
            if target.is_empty() {
                e = self.index.lookup(b"")?.unwrap_or_else(root_entry);
                continue;
            }
            e = self.index.lookup(&target)?.ok_or_else(|| {
                Error::NotFound(format!(
                    "{} -> {} (dangling link)",
                    e.display_path(),
                    String::from_utf8_lossy(&target)
                ))
            })?;
        }
        Err(Error::Unsupported(format!(
            "too many levels of links at {}",
            e.display_path()
        )))
    }

    /// Reads up to `buf.len()` bytes of an entry's contents at `offset`.
    /// Links must be resolved first with [`Archive::resolve`].
    pub fn read_at(&self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if e.kind != EntryKind::File {
            return Err(Error::Unsupported(format!(
                "{} is not a regular file",
                e.display_path()
            )));
        }
        if offset >= e.size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(e.size - offset) as usize;
        let buf = &mut buf[..want];
        match &e.sparse {
            None => {
                let avail = e.stored_size.saturating_sub(offset).min(want as u64) as usize;
                self.read_stream(&mut buf[..avail], e.data_offset + offset)?;
                buf[avail..].fill(0);
            }
            Some(chunks) => {
                buf.fill(0);
                let end = offset + want as u64;
                let mut stored = 0u64;
                for c in chunks {
                    let (cs, ce) = (c.offset, c.offset + c.len);
                    if ce > offset && cs < end {
                        let s = cs.max(offset);
                        let t = ce.min(end);
                        let src = e.data_offset + stored + (s - cs);
                        self.read_stream(
                            &mut buf[(s - offset) as usize..(t - offset) as usize],
                            src,
                        )?;
                    }
                    stored += c.len;
                }
            }
        }
        Ok(want)
    }

    /// Reads `buf.len()` bytes of the (uncompressed) tar stream at `off`.
    fn read_stream(&self, buf: &mut [u8], off: u64) -> Result<()> {
        match &self.gzip {
            None => pread_exact(&self.file, buf, off),
            Some(gz) => {
                let window = |at: u64| {
                    self.index
                        .checkpoint_window(at)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                };
                gz.read_exact_at(buf, off, &window).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        Error::corrupt(off, "the archive ends before this member's data")
                    } else {
                        e.into()
                    }
                })
            }
        }
    }

    /// Streams an entry's full contents (following links) into `w`.
    pub fn copy_to(&self, e: &Entry, w: &mut dyn Write) -> Result<u64> {
        let e = self.resolve(e.clone())?;
        let mut buf = vec![0u8; 1024 * 1024];
        let mut off = 0u64;
        loop {
            let n = self.read_at(&e, off, &mut buf)?;
            if n == 0 {
                return Ok(off);
            }
            w.write_all(&buf[..n])?;
            off += n as u64;
        }
    }
}

fn detect(file: &mut File) -> Result<Codec> {
    let mut head = [0u8; 512];
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Codec::detect(&head[..n]).ok_or(Error::NotTar)
}

/// Synthetic entry for the archive root when it has no `./` member.
fn root_entry() -> Entry {
    Entry {
        id: 0,
        parent_id: 0,
        path: Vec::new(),
        raw_name: Vec::new(),
        typeflag: crate::header::DIRTYPE,
        kind: EntryKind::Dir,
        mode: 0o755,
        uid: 0,
        gid: 0,
        uname: Vec::new(),
        gname: Vec::new(),
        mtime: 0,
        size: 0,
        link_target: Vec::new(),
        dev_major: 0,
        dev_minor: 0,
        header_offset: 0,
        data_offset: 0,
        stored_size: 0,
        sparse: None,
        implicit: true,
        superseded: false,
    }
}

/// True when both paths name the same file (through any alias). A path
/// that does not exist yet is compared through its canonical parent.
fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| -> Option<PathBuf> {
        if let Ok(c) = p.canonicalize() {
            return Some(c);
        }
        let parent = match p.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.canonicalize().ok()?,
            _ => std::env::current_dir().ok()?,
        };
        Some(parent.join(p.file_name()?))
    };
    matches!((canon(a), canon(b)), (Some(x), Some(y)) if x == y)
}

fn build_index(
    file: &Arc<File>,
    index_path: &Path,
    key: &ArchiveKey,
    codec: Codec,
    spacing: u64,
    progress: Option<&Progress>,
) -> Result<Index> {
    let total = key.size;
    let mut report = Reporter {
        progress,
        total,
        last: 0,
    };
    report.start();
    let (members, checkpoints) = match codec {
        Codec::Gzip => {
            let mut reader = IndexingReader::new(file.clone(), spacing)?;
            let members = {
                let mut r = GzProgress {
                    inner: &mut reader,
                    report: &mut report,
                };
                let members = Walker::new(ReadSource::new(&mut r)).collect::<Result<Vec<_>>>()?;
                // The walker stops at the tar terminator; decode the rest so
                // a damaged tail or a bad trailer CRC fails here, not later.
                std::io::copy(&mut r, &mut std::io::sink())?;
                members
            };
            (members, std::mem::take(&mut reader.checkpoints))
        }
        _ => {
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            let counted = Counted {
                inner: reader,
                pos: 0,
                report: &mut report,
            };
            let src = SeekSource::new(std::io::BufReader::with_capacity(64 * 1024, counted))?;
            (Walker::new(src).collect::<Result<Vec<_>>>()?, Vec::new())
        }
    };
    report.done();
    // Refuse to publish an index if the file changed while it was read.
    if ArchiveKey::from_file(file)? != *key {
        return Err(Error::Unsupported(
            "the archive changed while it was being indexed; try again".into(),
        ));
    }
    let entries = build_tree(members);
    Index::create(index_path, key, codec, &entries, &checkpoints)
}

/// Passes indexing progress on, at most once per 1 MiB or 1% read.
struct Reporter<'a> {
    progress: Option<&'a Progress>,
    total: u64,
    last: u64,
}

impl Reporter<'_> {
    fn at(&mut self, done: u64) {
        let Some(p) = self.progress else { return };
        let done = done.min(self.total);
        let step = (self.total / 100).max(1 << 20);
        if done.abs_diff(self.last) >= step {
            self.last = done;
            p(done, self.total);
        }
    }

    fn start(&mut self) {
        if let Some(p) = self.progress {
            p(0, self.total);
        }
    }

    fn done(&mut self) {
        if let Some(p) = self.progress {
            self.last = self.total;
            p(self.total, self.total);
        }
    }
}

/// The gzip indexing reader, reporting how much compressed input it used.
struct GzProgress<'a, 'b> {
    inner: &'a mut IndexingReader,
    report: &'a mut Reporter<'b>,
}

impl Read for GzProgress<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.report.at(self.inner.compressed_position());
        Ok(n)
    }
}

/// A plain file, reporting where it is.
struct Counted<'a, 'b> {
    inner: File,
    pos: u64,
    report: &'a mut Reporter<'b>,
}

impl Read for Counted<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        self.report.at(self.pos);
        Ok(n)
    }
}

impl Seek for Counted<'_, '_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        // Not reported: setting up the walker seeks to the end to learn the
        // length and back, and the next read reports the real position.
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

#[cfg(unix)]
pub(crate) fn pread_exact(f: &File, buf: &mut [u8], off: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn pread_exact(f: &File, mut buf: &mut [u8], mut off: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off)? {
            0 => return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()),
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

// File systems call into one archive from many threads.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Archive>();
};
