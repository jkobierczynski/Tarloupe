//! Streaming tar walker.
//!
//! The walker reads headers in one forward pass and records, for every
//! member, where its data lives in the uncompressed stream. It never reads
//! member data except for metadata payloads (long names, PAX records, the
//! PAX 1.0 sparse map); the rest is skipped, which is a seek for plain tar
//! and a discard-decode for compressed streams.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};

use crate::entry::{Entry, EntryKind, SparseChunk};
use crate::error::{Error, Result};
use crate::header::{self, padded, Block, Header};
use crate::path::normalize;
use crate::pax;

/// Upper bound for metadata payloads (long names, PAX headers, sparse maps)
/// so a corrupt size field cannot exhaust memory.
const MAX_META: u64 = 64 * 1024 * 1024;

/// A forward-only byte source that knows its position in the tar stream.
pub trait TarSource {
    /// Reads one block. Returns `Ok(false)` on a clean end of input.
    fn read_block(&mut self, buf: &mut Block) -> io::Result<bool>;
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;
    fn skip(&mut self, n: u64) -> io::Result<()>;
    fn position(&self) -> u64;
}

fn read_block_from<R: Read>(r: &mut R, buf: &mut Block) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Source over any reader (e.g. a decompressor). Skips by reading.
pub struct ReadSource<R> {
    inner: R,
    pos: u64,
}

impl<R: Read> ReadSource<R> {
    pub fn new(inner: R) -> Self {
        ReadSource { inner, pos: 0 }
    }
}

impl<R: Read> TarSource for ReadSource<R> {
    fn read_block(&mut self, buf: &mut Block) -> io::Result<bool> {
        let n = read_block_from(&mut self.inner, buf)?;
        self.pos += n as u64;
        match n {
            0 => Ok(false),
            512 => Ok(true),
            _ => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
    fn skip(&mut self, n: u64) -> io::Result<()> {
        let copied = io::copy(&mut (&mut self.inner).take(n), &mut io::sink())?;
        self.pos += copied;
        if copied < n {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
    fn position(&self) -> u64 {
        self.pos
    }
}

/// Source over a seekable reader (a plain `.tar` file). Skips by seeking.
pub struct SeekSource<R> {
    inner: R,
    pos: u64,
    len: u64,
}

impl<R: Read + Seek> SeekSource<R> {
    pub fn new(mut inner: R) -> io::Result<Self> {
        let pos = inner.stream_position()?;
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(pos))?;
        Ok(SeekSource { inner, pos, len })
    }
}

impl<R: Read + Seek> TarSource for SeekSource<R> {
    fn read_block(&mut self, buf: &mut Block) -> io::Result<bool> {
        let n = read_block_from(&mut self.inner, buf)?;
        self.pos += n as u64;
        match n {
            0 => Ok(false),
            512 => Ok(true),
            _ => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
    fn skip(&mut self, n: u64) -> io::Result<()> {
        if self.pos.saturating_add(n) > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.pos = self.inner.seek(SeekFrom::Current(n as i64))?;
        Ok(())
    }
    fn position(&self) -> u64 {
        self.pos
    }
}

/// Iterates over the members of a tar stream.
pub struct Walker<S> {
    src: S,
    global: HashMap<Vec<u8>, Vec<u8>>,
    done: bool,
}

#[derive(Default)]
struct Pending {
    start: Option<u64>,
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    pax: Vec<(Vec<u8>, Vec<u8>)>,
}

impl<S: TarSource> Walker<S> {
    pub fn new(src: S) -> Self {
        Walker {
            src,
            global: HashMap::new(),
            done: false,
        }
    }

    pub fn position(&self) -> u64 {
        self.src.position()
    }

    fn read_payload(&mut self, size: u64, at: u64) -> Result<Vec<u8>> {
        if size > MAX_META {
            return Err(Error::corrupt(
                at,
                format!("metadata payload of {size} bytes is too large"),
            ));
        }
        let mut buf = vec![0u8; padded(size) as usize];
        self.src.read_exact(&mut buf).map_err(|e| eof(e, at))?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// Returns the next member, or `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Result<Option<Entry>> {
        if self.done {
            return Ok(None);
        }
        let mut block: Block = [0; 512];
        let mut p = Pending::default();
        loop {
            let at = self.src.position();
            if !self.src.read_block(&mut block).map_err(|e| eof(e, at))? {
                // Missing end-of-archive marker; GNU tar accepts this too.
                self.done = true;
                return Ok(None);
            }
            if header::is_zero_block(&block) {
                self.done = true;
                return Ok(None);
            }
            if !header::checksum_ok(&block) {
                return Err(Error::corrupt(at, "header checksum mismatch"));
            }
            p.start.get_or_insert(at);
            let h = Header(&block);
            let size = h
                .size()
                .ok_or_else(|| Error::corrupt(at, "invalid size field"))?;
            match h.typeflag() {
                header::GNU_LONGNAME => {
                    let mut v = self.read_payload(size, at)?;
                    v.truncate(header::cstr(&v).len());
                    p.long_name = Some(v);
                }
                header::GNU_LONGLINK => {
                    let mut v = self.read_payload(size, at)?;
                    v.truncate(header::cstr(&v).len());
                    p.long_link = Some(v);
                }
                header::PAX_LOCAL | header::SOLARIS_X => {
                    let v = self.read_payload(size, at)?;
                    p.pax.extend(pax::parse_records(&v));
                }
                header::PAX_GLOBAL => {
                    let v = self.read_payload(size, at)?;
                    for (k, val) in pax::parse_records(&v) {
                        if val.is_empty() {
                            self.global.remove(&k);
                        } else {
                            self.global.insert(k, val);
                        }
                    }
                    // A global header is not a member; start over.
                    p = Pending::default();
                }
                header::GNU_VOLHDR => {
                    self.src.skip(padded(size)).map_err(|e| eof(e, at))?;
                    p = Pending::default();
                }
                header::GNU_MULTIVOL => {
                    return Err(Error::Unsupported(
                        "multi-volume tar archives are not supported".into(),
                    ));
                }
                _ => return self.finish_member(&block, size, p, at).map(Some),
            }
        }
    }

    fn finish_member(
        &mut self,
        block: &Block,
        header_size: u64,
        p: Pending,
        at: u64,
    ) -> Result<Entry> {
        let h = Header(block);
        let typeflag = h.typeflag();

        // Local PAX records override global ones.
        let mut attrs: HashMap<Vec<u8>, Vec<u8>> = self.global.clone();
        for (k, v) in &p.pax {
            attrs.insert(k.clone(), v.clone());
        }
        let get = |k: &str| attrs.get(k.as_bytes()).map(Vec::as_slice);

        let mut raw_name = get("path")
            .map(<[u8]>::to_vec)
            .or(p.long_name)
            .unwrap_or_else(|| h.path());
        let link_target = get("linkpath")
            .map(<[u8]>::to_vec)
            .or(p.long_link)
            .unwrap_or_else(|| h.link_name());
        let stored_size = get("size").and_then(pax::parse_u64).unwrap_or(header_size);
        let mode = h
            .mode()
            .ok_or_else(|| Error::corrupt(at, "invalid mode field"))?;
        let uid = get("uid").and_then(pax::parse_u64).or(h.uid()).unwrap_or(0);
        let gid = get("gid").and_then(pax::parse_u64).or(h.gid()).unwrap_or(0);
        let uname = get("uname")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| h.uname());
        let gname = get("gname")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| h.gname());
        let mtime = get("mtime")
            .and_then(pax::parse_time)
            .or(h.mtime())
            .ok_or_else(|| Error::corrupt(at, "invalid mtime field"))?;
        let (dev_major, dev_minor) = match typeflag {
            header::CHRTYPE | header::BLKTYPE => {
                (h.dev_major().unwrap_or(0), h.dev_minor().unwrap_or(0))
            }
            _ => (0, 0),
        };

        let mut size = stored_size;
        let mut sparse: Option<Vec<SparseChunk>> = None;
        let mut data_offset;

        if typeflag == header::GNU_SPARSE {
            // Old GNU sparse: map in the header plus extension blocks.
            let (mut chunks, mut extended, realsize) = h.gnu_sparse();
            let mut ext: Block = [0; 512];
            while extended {
                let eat = self.src.position();
                if !self.src.read_block(&mut ext).map_err(|e| eof(e, eat))? {
                    return Err(Error::corrupt(eat, "truncated sparse header"));
                }
                let (more, ext_flag) = header::gnu_sparse_ext(&ext);
                chunks.extend(more);
                extended = ext_flag;
            }
            size = realsize.unwrap_or(stored_size);
            sparse = Some(to_chunks(&chunks));
            data_offset = self.src.position();
        } else {
            data_offset = self.src.position();
            let major = get("GNU.sparse.major").and_then(pax::parse_u64);
            let minor = get("GNU.sparse.minor").and_then(pax::parse_u64);
            if let Some(name) = get("GNU.sparse.name") {
                raw_name = name.to_vec();
            }
            if major == Some(1) && minor == Some(0) {
                // PAX sparse 1.0: the map is at the start of the data.
                let (chunks, map_len) = self.read_sparse_1_0(stored_size, at)?;
                data_offset = self.src.position();
                size = get("GNU.sparse.realsize")
                    .and_then(pax::parse_u64)
                    .unwrap_or(stored_size);
                sparse = Some(chunks);
                let remaining = stored_size.saturating_sub(map_len);
                self.src.skip(padded(remaining)).map_err(|e| eof(e, at))?;
                return Ok(self.build(
                    p.start.unwrap_or(at),
                    raw_name,
                    typeflag,
                    link_target,
                    mode,
                    uid,
                    gid,
                    uname,
                    gname,
                    mtime,
                    size,
                    remaining,
                    dev_major,
                    dev_minor,
                    data_offset,
                    sparse,
                ));
            } else if let Some(map) = get("GNU.sparse.map") {
                // PAX sparse 0.1: comma-separated offset,size list.
                let nums: Vec<u64> = map
                    .split(|&c| c == b',')
                    .filter_map(pax::parse_u64)
                    .collect();
                let pairs: Vec<(u64, u64)> = nums.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                sparse = Some(to_chunks(&pairs));
                size = get("GNU.sparse.size")
                    .and_then(pax::parse_u64)
                    .unwrap_or(stored_size);
            } else if get("GNU.sparse.offset").is_some() {
                // PAX sparse 0.0: repeated offset/numbytes records, in order.
                let mut pairs = Vec::new();
                let mut off = None;
                for (k, v) in &p.pax {
                    match k.as_slice() {
                        b"GNU.sparse.offset" => off = pax::parse_u64(v),
                        b"GNU.sparse.numbytes" => {
                            if let (Some(o), Some(n)) = (off.take(), pax::parse_u64(v)) {
                                pairs.push((o, n));
                            }
                        }
                        _ => {}
                    }
                }
                sparse = Some(to_chunks(&pairs));
                size = get("GNU.sparse.size")
                    .and_then(pax::parse_u64)
                    .unwrap_or(stored_size);
            }
        }

        self.src.skip(padded(stored_size)).map_err(|e| eof(e, at))?;
        Ok(self.build(
            p.start.unwrap_or(at),
            raw_name,
            typeflag,
            link_target,
            mode,
            uid,
            gid,
            uname,
            gname,
            mtime,
            size,
            stored_size,
            dev_major,
            dev_minor,
            data_offset,
            sparse,
        ))
    }

    /// Reads the PAX 1.0 sparse map. Returns the chunks and the number of
    /// bytes (whole blocks) the map occupies.
    fn read_sparse_1_0(&mut self, stored_size: u64, at: u64) -> Result<(Vec<SparseChunk>, u64)> {
        let mut buf: Vec<u8> = Vec::new();
        let mut block: Block = [0; 512];
        let mut numbers: Vec<u64> = Vec::new();
        loop {
            if buf.len() as u64 >= stored_size || buf.len() as u64 > MAX_META {
                return Err(Error::corrupt(at, "unterminated PAX 1.0 sparse map"));
            }
            let bat = self.src.position();
            if !self.src.read_block(&mut block).map_err(|e| eof(e, bat))? {
                return Err(Error::corrupt(bat, "truncated PAX 1.0 sparse map"));
            }
            buf.extend_from_slice(&block);
            // Parse complete lines from the accumulated buffer.
            numbers.clear();
            let mut needed: Option<u64> = None;
            let mut complete = false;
            for line in buf.split_inclusive(|&c| c == b'\n') {
                if !line.ends_with(b"\n") {
                    break;
                }
                let n = pax::parse_u64(&line[..line.len() - 1])
                    .ok_or_else(|| Error::corrupt(bat, "invalid PAX 1.0 sparse map"))?;
                if needed.is_none() {
                    needed = Some(
                        n.checked_mul(2)
                            .ok_or_else(|| Error::corrupt(bat, "sparse map too large"))?,
                    );
                } else {
                    numbers.push(n);
                }
                if needed == Some(numbers.len() as u64) {
                    complete = true;
                    break;
                }
            }
            if complete {
                break;
            }
        }
        let pairs: Vec<(u64, u64)> = numbers.chunks_exact(2).map(|c| (c[0], c[1])).collect();
        Ok((to_chunks(&pairs), buf.len() as u64))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        header_offset: u64,
        raw_name: Vec<u8>,
        typeflag: u8,
        link_target: Vec<u8>,
        mode: u32,
        uid: u64,
        gid: u64,
        uname: Vec<u8>,
        gname: Vec<u8>,
        mtime: i64,
        size: u64,
        stored_size: u64,
        dev_major: u64,
        dev_minor: u64,
        data_offset: u64,
        sparse: Option<Vec<SparseChunk>>,
    ) -> Entry {
        let kind = EntryKind::from_typeflag(typeflag, &raw_name);
        Entry {
            id: 0,
            parent_id: 0,
            path: normalize(&raw_name),
            raw_name,
            typeflag,
            kind,
            mode,
            uid,
            gid,
            uname,
            gname,
            mtime,
            size,
            link_target,
            dev_major,
            dev_minor,
            header_offset,
            data_offset,
            stored_size,
            sparse,
            implicit: false,
            superseded: false,
        }
    }
}

fn to_chunks(pairs: &[(u64, u64)]) -> Vec<SparseChunk> {
    pairs
        .iter()
        .map(|&(offset, len)| SparseChunk { offset, len })
        .collect()
}

fn eof(e: io::Error, at: u64) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        Error::corrupt(at, "unexpected end of archive")
    } else {
        Error::Io(e)
    }
}

impl<S: TarSource> Iterator for Walker<S> {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_entry() {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
