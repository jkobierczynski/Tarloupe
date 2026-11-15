//! Identity of an archive file, used to name and validate its index.
//!
//! The key combines the file size, its modification time, and an xxh3 hash
//! of the first and last MiB. Renaming or moving the archive keeps its
//! index; modifying it invalidates the index.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use xxhash_rust::xxh3::{xxh3_128, Xxh3};

use crate::error::Result;

const SAMPLE: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveKey {
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch (0 if unknown).
    pub mtime_ns: i128,
    /// xxh3-64 of the first and last MiB.
    pub sample_hash: u64,
}

impl ArchiveKey {
    pub fn compute(path: &Path) -> Result<ArchiveKey> {
        ArchiveKey::from_file(&File::open(path)?)
    }

    /// Computes the key from an open handle, so the key describes exactly
    /// the file that will be read (not whatever the path names later).
    pub fn from_file(file: &File) -> Result<ArchiveKey> {
        let mut f = file;
        let md = f.metadata()?;
        let size = md.len();
        let mtime_ns = md
            .modified()
            .ok()
            .map(|t| match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            })
            .unwrap_or(0);

        let mut h = Xxh3::new();
        let mut buf = vec![0u8; SAMPLE.min(size) as usize];
        f.seek(SeekFrom::Start(0))?;
        f.read_exact(&mut buf)?;
        h.update(&buf);
        if size > SAMPLE {
            let tail_start = (size - SAMPLE).max(SAMPLE);
            let mut tail = vec![0u8; (size - tail_start) as usize];
            f.seek(SeekFrom::Start(tail_start))?;
            f.read_exact(&mut tail)?;
            h.update(&tail);
        }
        Ok(ArchiveKey {
            size,
            mtime_ns,
            sample_hash: h.digest(),
        })
    }

    /// Stable identifier used as the index file name.
    pub fn id(&self) -> String {
        let mut b = Vec::with_capacity(40);
        b.extend_from_slice(&self.size.to_le_bytes());
        b.extend_from_slice(&self.mtime_ns.to_le_bytes());
        b.extend_from_slice(&self.sample_hash.to_le_bytes());
        format!("{:032x}", xxh3_128(&b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn key_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.tar");
        std::fs::write(&p, vec![1u8; 3 * 1024 * 1024]).unwrap();
        let k1 = ArchiveKey::compute(&p).unwrap();
        assert_eq!(k1, ArchiveKey::compute(&p).unwrap());
        let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.seek(SeekFrom::End(-1)).unwrap();
        f.write_all(&[2]).unwrap();
        drop(f);
        let k2 = ArchiveKey::compute(&p).unwrap();
        assert_ne!(k1.sample_hash, k2.sample_hash);
    }
}
