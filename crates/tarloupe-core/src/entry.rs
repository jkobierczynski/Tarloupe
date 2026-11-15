//! One member of a tar archive, as recorded in the index.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    HardLink,
    CharDev,
    BlockDev,
    Fifo,
    Other,
}

impl EntryKind {
    pub fn from_typeflag(t: u8, raw_name: &[u8]) -> EntryKind {
        use crate::header::*;
        match t {
            REGTYPE | AREGTYPE | CONTTYPE | GNU_SPARSE => {
                // v7 archives mark directories with a trailing slash only.
                if raw_name.ends_with(b"/") {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                }
            }
            LNKTYPE => EntryKind::HardLink,
            SYMTYPE => EntryKind::Symlink,
            CHRTYPE => EntryKind::CharDev,
            BLKTYPE => EntryKind::BlockDev,
            DIRTYPE | GNU_DUMPDIR => EntryKind::Dir,
            FIFOTYPE => EntryKind::Fifo,
            _ => EntryKind::Other,
        }
    }

    pub fn to_code(self) -> i64 {
        self as i64
    }

    pub fn from_code(c: i64) -> EntryKind {
        use EntryKind::*;
        [File, Dir, Symlink, HardLink, CharDev, BlockDev, Fifo, Other]
            .get(c as usize)
            .copied()
            .unwrap_or(Other)
    }
}

/// A run of real data in a sparse file. Data runs are stored back to back in
/// the archive; everything between runs reads as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseChunk {
    /// Offset in the logical (expanded) file.
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Row id in the index (archive order, 1-based). 0 until indexed.
    pub id: i64,
    /// Parent directory id; 0 is the archive root.
    pub parent_id: i64,
    /// Normalised path: no leading `./` or `/`, no trailing `/`.
    pub path: Vec<u8>,
    /// Name exactly as stored in the archive (after long-name/PAX resolution).
    /// Empty for directories the archive never listed explicitly.
    pub raw_name: Vec<u8>,
    pub typeflag: u8,
    pub kind: EntryKind,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub uname: Vec<u8>,
    pub gname: Vec<u8>,
    pub mtime: i64,
    /// Logical size (the expanded size for sparse files).
    pub size: u64,
    pub link_target: Vec<u8>,
    pub dev_major: u64,
    pub dev_minor: u64,
    /// Offset of the first header block of this member (including any
    /// GNU long-name or PAX headers that precede it).
    pub header_offset: u64,
    /// Offset of the member's data in the uncompressed tar stream.
    pub data_offset: u64,
    /// Bytes of data stored at `data_offset`.
    pub stored_size: u64,
    pub sparse: Option<Vec<SparseChunk>>,
    /// Directory synthesised because members exist below it.
    pub implicit: bool,
    /// A later member with the same path replaces this one (tar semantics).
    pub superseded: bool,
}

impl Entry {
    /// Last path component.
    pub fn name(&self) -> &[u8] {
        match self.path.iter().rposition(|&c| c == b'/') {
            Some(i) => &self.path[i + 1..],
            None => &self.path,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    pub fn display_path(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }
}

pub(crate) fn encode_sparse(chunks: &[SparseChunk]) -> Vec<u8> {
    let mut v = Vec::with_capacity(chunks.len() * 16);
    for c in chunks {
        v.extend_from_slice(&c.offset.to_le_bytes());
        v.extend_from_slice(&c.len.to_le_bytes());
    }
    v
}

pub(crate) fn decode_sparse(b: &[u8]) -> Vec<SparseChunk> {
    b.chunks_exact(16)
        .map(|c| SparseChunk {
            offset: u64::from_le_bytes(c[..8].try_into().unwrap()),
            len: u64::from_le_bytes(c[8..].try_into().unwrap()),
        })
        .collect()
}
