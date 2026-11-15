//! Persistent SQLite index (`.tli`).
//!
//! Schema version 2:
//! - `meta`: format version, codec, archive key, counts.
//! - `entries`: one row per member in archive order, plus synthesised
//!   parent directories. Lookups ignore superseded rows (last one wins).
//! - `checkpoints`: decoder restart points for compressed streams (gzip:
//!   bit offset plus the zlib-compressed 32 KiB window before it).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};

use crate::codec::Codec;
use crate::entry::{decode_sparse, encode_sparse, Entry, EntryKind};
use crate::error::{Error, Result};
use crate::gzip::{Checkpoint, Point};
use crate::key::ArchiveKey;
use crate::path::parent;

pub const FORMAT_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE entries (
    id            INTEGER PRIMARY KEY,
    parent_id     INTEGER NOT NULL,
    path          BLOB NOT NULL,
    name          BLOB NOT NULL,
    raw_name      BLOB NOT NULL,
    typeflag      INTEGER NOT NULL,
    kind          INTEGER NOT NULL,
    mode          INTEGER NOT NULL,
    uid           INTEGER NOT NULL,
    gid           INTEGER NOT NULL,
    uname         BLOB NOT NULL,
    gname         BLOB NOT NULL,
    mtime         INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    link_target   BLOB NOT NULL,
    dev_major     INTEGER NOT NULL,
    dev_minor     INTEGER NOT NULL,
    header_offset INTEGER NOT NULL,
    data_offset   INTEGER NOT NULL,
    stored_size   INTEGER NOT NULL,
    sparse        BLOB,
    implicit      INTEGER NOT NULL,
    superseded    INTEGER NOT NULL
);
CREATE TABLE checkpoints (
    uncompressed_offset   INTEGER PRIMARY KEY,
    compressed_bit_offset INTEGER NOT NULL,
    state                 BLOB
);
";

const INDEXES: &str = "
CREATE INDEX entries_by_parent ON entries (parent_id, name) WHERE superseded = 0;
CREATE INDEX entries_by_path ON entries (path) WHERE superseded = 0;
CREATE INDEX entries_by_path_history ON entries (path, id) WHERE implicit = 0;
";

const COLUMNS: &str = "id, parent_id, path, raw_name, typeflag, kind, mode, uid, gid, uname, gname, mtime, \
     size, link_target, dev_major, dev_minor, header_offset, data_offset, stored_size, sparse, implicit, superseded";

/// Default directory for index files: `%LOCALAPPDATA%\Tarloupe\index` on
/// Windows, `~/.local/share/Tarloupe/index` on Linux. `TARLOUPE_INDEX_DIR`
/// overrides it.
pub fn default_index_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TARLOUPE_INDEX_DIR") {
        return PathBuf::from(d);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Tarloupe")
        .join("index")
}

/// Assigns ids, synthesises missing parent directories, links parents and
/// marks members replaced by a later member with the same path.
pub fn build_tree(members: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(members.len());
    let mut latest: HashMap<Vec<u8>, usize> = HashMap::with_capacity(members.len());
    for e in members {
        // Make sure every ancestor exists.
        let mut missing: Vec<Vec<u8>> = Vec::new();
        let mut p = parent(&e.path);
        while !p.is_empty() && !latest.contains_key(p) {
            missing.push(p.to_vec());
            p = parent(p);
        }
        for dir in missing.into_iter().rev() {
            latest.insert(dir.clone(), out.len());
            out.push(Entry {
                id: 0,
                parent_id: 0,
                path: dir,
                raw_name: Vec::new(),
                typeflag: crate::header::DIRTYPE,
                kind: EntryKind::Dir,
                mode: 0o755,
                uid: 0,
                gid: 0,
                uname: Vec::new(),
                gname: Vec::new(),
                mtime: e.mtime,
                size: 0,
                link_target: Vec::new(),
                dev_major: 0,
                dev_minor: 0,
                header_offset: e.header_offset,
                data_offset: e.header_offset,
                stored_size: 0,
                sparse: None,
                implicit: true,
                superseded: false,
            });
        }
        if let Some(prev) = latest.insert(e.path.clone(), out.len()) {
            out[prev].superseded = true;
        }
        out.push(e);
    }
    for (i, e) in out.iter_mut().enumerate() {
        e.id = i as i64 + 1;
        let p = parent(&e.path);
        e.parent_id = if e.path.is_empty() || p.is_empty() {
            0
        } else {
            latest.get(p).map(|&j| j as i64 + 1).unwrap_or(0)
        };
    }
    out
}

/// An open index. Safe to share between threads: each query borrows a
/// read-only connection from a small pool (opening another one when all are
/// busy), so callbacks may query the index again.
pub struct Index {
    pool: Mutex<Vec<Connection>>,
    path: PathBuf,
    /// The `meta` table as first opened. A connection opened later must see
    /// the same, so that every connection reads the same index file even if
    /// the path is replaced (a rebuild renames a new file over it).
    meta: Vec<(String, String)>,
}

/// A pooled connection, returned to the pool on drop.
struct Conn<'a> {
    conn: Option<Connection>,
    index: &'a Index,
}

impl std::ops::Deref for Conn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        if let (Some(c), Ok(mut pool)) = (self.conn.take(), self.index.pool.lock()) {
            if pool.len() < 16 {
                pool.push(c);
            }
        }
    }
}

impl Index {
    /// Writes a new index atomically (temp file + rename).
    pub fn create(
        path: &Path,
        key: &ArchiveKey,
        codec: Codec,
        entries: &[Entry],
        checkpoints: &[Checkpoint],
    ) -> Result<Index> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = unique_temp_path(path)?;
        let result = Index::write(&tmp, key, codec, entries, checkpoints)
            .and_then(|()| Ok(std::fs::rename(&tmp, path)?));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        // Validate: a concurrent build may have replaced a shared index path.
        Index::open(path, key)
    }

    fn write(
        tmp: &Path,
        key: &ArchiveKey,
        codec: Codec,
        entries: &[Entry],
        checkpoints: &[Checkpoint],
    ) -> Result<()> {
        {
            let mut conn = Connection::open(tmp)?;
            conn.execute_batch(
                "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA page_size = 8192;",
            )?;
            conn.execute_batch(SCHEMA)?;
            let tx = conn.transaction()?;
            {
                let mut meta = tx.prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")?;
                let members = entries.iter().filter(|e| !e.implicit).count();
                for (k, v) in [
                    ("format_version", FORMAT_VERSION.to_string()),
                    ("tool_version", env!("CARGO_PKG_VERSION").to_string()),
                    ("codec", codec.name().to_string()),
                    ("archive_size", key.size.to_string()),
                    ("archive_mtime_ns", key.mtime_ns.to_string()),
                    ("archive_sample_hash", format!("{:016x}", key.sample_hash)),
                    ("archive_key", key.id()),
                    ("entry_count", entries.len().to_string()),
                    ("member_count", members.to_string()),
                    ("checkpoint_count", checkpoints.len().to_string()),
                    // Unique per build (the temp file was created
                    // exclusively), so two builds never look the same.
                    (
                        "generation",
                        tmp.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    ),
                ] {
                    meta.execute(params![k, v])?;
                }
                let mut ins = tx.prepare(&format!(
                    "INSERT INTO entries (id, parent_id, path, name, raw_name, typeflag, kind, mode, uid, gid, \
                     uname, gname, mtime, size, link_target, dev_major, dev_minor, header_offset, data_offset, \
                     stored_size, sparse, implicit, superseded) VALUES ({})",
                    (1..=23).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",")
                ))?;
                for e in entries {
                    ins.execute(params![
                        e.id,
                        e.parent_id,
                        e.path,
                        e.name(),
                        e.raw_name,
                        e.typeflag as i64,
                        e.kind.to_code(),
                        e.mode as i64,
                        e.uid as i64,
                        e.gid as i64,
                        e.uname,
                        e.gname,
                        e.mtime,
                        e.size as i64,
                        e.link_target,
                        e.dev_major as i64,
                        e.dev_minor as i64,
                        e.header_offset as i64,
                        e.data_offset as i64,
                        e.stored_size as i64,
                        e.sparse.as_deref().map(encode_sparse),
                        e.implicit,
                        e.superseded,
                    ])?;
                }
                let mut cp = tx.prepare(
                    "INSERT INTO checkpoints (uncompressed_offset, compressed_bit_offset, state) \
                     VALUES (?1, ?2, ?3)",
                )?;
                for c in checkpoints {
                    cp.execute(params![
                        c.uncompressed_offset as i64,
                        c.bit_offset as i64,
                        c.window
                    ])?;
                }
            }
            tx.commit()?;
            conn.execute_batch(INDEXES)?;
            conn.execute_batch("ANALYZE;")?;
        }
        Ok(())
    }

    fn open_unchecked(path: &Path) -> Result<Index> {
        let conn = open_read_only(path)?;
        let meta = read_meta(&conn)?;
        Ok(Index {
            pool: Mutex::new(vec![conn]),
            path: path.to_path_buf(),
            meta,
        })
    }

    fn conn(&self) -> Result<Conn<'_>> {
        let pooled = self.pool.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let conn = match pooled {
            Some(c) => c,
            None => {
                let c = open_read_only(&self.path)?;
                if read_meta(&c)? != self.meta {
                    return Err(Error::StaleIndex {
                        path: self.path.display().to_string(),
                        reason: "the index file was replaced while in use".into(),
                    });
                }
                c
            }
        };
        Ok(Conn {
            conn: Some(conn),
            index: self,
        })
    }

    /// Opens an index and checks that it belongs to the archive described by `key`.
    pub fn open(path: &Path, key: &ArchiveKey) -> Result<Index> {
        let idx = Index::open_unchecked(path)?;
        let stale = |reason: String| Error::StaleIndex {
            path: path.display().to_string(),
            reason,
        };
        let version: i64 = idx
            .meta("format_version")?
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| stale("missing format version".into()))?;
        if version != FORMAT_VERSION {
            return Err(stale(format!(
                "format version {version}, expected {FORMAT_VERSION}"
            )));
        }
        let size = idx.meta("archive_size")?.unwrap_or_default();
        let mtime = idx.meta("archive_mtime_ns")?.unwrap_or_default();
        let hash = idx.meta("archive_sample_hash")?.unwrap_or_default();
        if size != key.size.to_string()
            || mtime != key.mtime_ns.to_string()
            || hash != format!("{:016x}", key.sample_hash)
        {
            return Err(stale("the archive has changed since it was indexed".into()));
        }
        Ok(idx)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All decoder restart points, in stream order (windows not loaded).
    pub fn checkpoints(&self) -> Result<Vec<Point>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT uncompressed_offset, compressed_bit_offset FROM checkpoints \
             ORDER BY uncompressed_offset",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Point {
                uncompressed_offset: r.get::<_, i64>(0)? as u64,
                bit_offset: r.get::<_, i64>(1)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The packed window stored with the checkpoint at `uncompressed_offset`.
    pub fn checkpoint_window(&self, uncompressed_offset: u64) -> Result<Vec<u8>> {
        Ok(self.conn()?.query_row(
            "SELECT state FROM checkpoints WHERE uncompressed_offset = ?1",
            [uncompressed_offset as i64],
            |r| Ok(r.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default()),
        )?)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Visits every archive member (not synthesised directories) in archive order.
    pub fn for_each_member(&self, mut f: impl FnMut(Entry) -> Result<()>) -> Result<()> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM entries WHERE implicit = 0 ORDER BY id"
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            f(row_to_entry(r)?)?;
        }
        Ok(())
    }

    pub fn get(&self, id: i64) -> Result<Option<Entry>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM entries WHERE id = ?1"),
                [id],
                row_to_entry,
            )
            .optional()?)
    }

    /// Current (non-superseded) entry for a normalised path.
    pub fn lookup(&self, path: &[u8]) -> Result<Option<Entry>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM entries WHERE path = ?1 AND superseded = 0 ORDER BY id DESC LIMIT 1"),
                [path],
                row_to_entry,
            )
            .optional()?)
    }

    /// The latest member with this path that precedes member `before`,
    /// including superseded ones. Hard links refer to data that was
    /// archived before them, even when the link reuses the same path.
    pub fn lookup_before(&self, path: &[u8], before: i64) -> Result<Option<Entry>> {
        Ok(self
            .conn()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM entries WHERE path = ?1 AND id < ?2 AND implicit = 0 ORDER BY id DESC LIMIT 1"
                ),
                params![path, before],
                row_to_entry,
            )
            .optional()?)
    }

    /// Current children of a directory (0 = archive root), sorted by name.
    pub fn children(&self, dir_id: i64) -> Result<Vec<Entry>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM entries WHERE parent_id = ?1 AND superseded = 0 AND path <> x'' ORDER BY name"
        ))?;
        let rows = stmt.query_map([dir_id], row_to_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn read_meta(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare("SELECT key, value FROM meta ORDER BY key")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

/// Creates (exclusively) a temporary file next to `path` that no other
/// builder, in this or another process, can be using.
fn unique_temp_path(path: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tli.tmp-{}-{n}-{nanos:x}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(_) => return Ok(tmp),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

fn row_to_entry(r: &Row<'_>) -> rusqlite::Result<Entry> {
    let sparse: Option<Vec<u8>> = r.get(19)?;
    Ok(Entry {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        path: r.get(2)?,
        raw_name: r.get(3)?,
        typeflag: r.get::<_, i64>(4)? as u8,
        kind: EntryKind::from_code(r.get(5)?),
        mode: r.get::<_, i64>(6)? as u32,
        uid: r.get::<_, i64>(7)? as u64,
        gid: r.get::<_, i64>(8)? as u64,
        uname: r.get(9)?,
        gname: r.get(10)?,
        mtime: r.get(11)?,
        size: r.get::<_, i64>(12)? as u64,
        link_target: r.get(13)?,
        dev_major: r.get::<_, i64>(14)? as u64,
        dev_minor: r.get::<_, i64>(15)? as u64,
        header_offset: r.get::<_, i64>(16)? as u64,
        data_offset: r.get::<_, i64>(17)? as u64,
        stored_size: r.get::<_, i64>(18)? as u64,
        sparse: sparse.map(|b| decode_sparse(&b)),
        implicit: r.get(20)?,
        superseded: r.get(21)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooled_connections_never_mix_index_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tli");
        let key = |size| ArchiveKey {
            size,
            mtime_ns: 1,
            sample_hash: 2,
        };
        let idx = Index::create(&path, &key(10), Codec::None, &[], &[]).unwrap();
        // Keep the only pooled connection busy, so the next query opens a
        // new one, then replace the file as a rebuild for another archive
        // would (where the platform allows replacing an open file).
        let generation = idx.meta("generation").unwrap().unwrap();
        // Another archive, then the same archive again (equal metadata, but
        // its checkpoints could differ): neither may be mixed in.
        for size in [20, 10] {
            let busy = idx.conn().unwrap();
            drop(Index::create(&path, &key(size), Codec::None, &[], &[]));
            match idx.meta("generation") {
                Ok(v) => assert_eq!(v.as_ref(), Some(&generation)),
                Err(Error::StaleIndex { .. }) => {}
                Err(e) => panic!("{e}"),
            }
            drop(busy);
            assert_eq!(idx.meta("generation").unwrap(), Some(generation.clone()));
        }
    }
}
