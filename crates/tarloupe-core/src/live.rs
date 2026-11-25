//! The entry table of an archive that is still being indexed, kept in
//! memory and readable while it grows (see [`crate::Archive::open_live`]).
//!
//! It answers the same queries as an index file, with the same ids: entries
//! get their final ids as they arrive, so nothing changes when the finished
//! file takes over.

use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::entry::Entry;
use crate::error::{Error, Result};
use crate::gzip::{Checkpoint, Point};
use crate::index::TreeBuilder;
use crate::path::parent;

#[derive(Default)]
pub(crate) struct Live {
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    tree: TreeBuilder,
    /// For each entry, the earlier entry with the same path, if any.
    prev: Vec<Option<usize>>,
    /// Directory path -> entries directly in it, in archive order.
    children: HashMap<Vec<u8>, Vec<usize>>,
    /// Directory path -> `version` when an entry was last added to it.
    stamps: HashMap<Vec<u8>, u64>,
    version: u64,
    checkpoints: Vec<Checkpoint>,
}

impl State {
    /// Entry `i` as the index file will have it.
    fn entry(&self, i: usize) -> Entry {
        let mut e = self.tree.out[i].clone();
        e.parent_id = self.tree.parent_id(&e.path);
        e
    }

    /// The path of directory `dir_id` (empty: the root), or `None` when it
    /// has no children of its own: unknown, or replaced by a later member.
    fn dir_path(&self, dir_id: i64) -> Option<&[u8]> {
        if dir_id == 0 {
            return Some(b"");
        }
        let i = usize::try_from(dir_id - 1).ok()?;
        let d = self.tree.out.get(i)?;
        // Children belong to the current entry for a path, as in the file.
        (self.tree.latest.get(&d.path) == Some(&i)).then_some(&d.path[..])
    }
}

impl Live {
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds members (in archive order) and restart points.
    pub(crate) fn push(&self, members: Vec<Entry>, checkpoints: Vec<Checkpoint>) {
        let mut guard = self.write();
        let s = &mut *guard;
        s.version += 1;
        let mut added = Vec::new();
        for e in members {
            s.tree.push(e, |i, p| added.push((i, p)));
            for (i, p) in added.drain(..) {
                s.prev.push(p);
                let dir = parent(&s.tree.out[i].path).to_vec();
                s.stamps.insert(dir.clone(), s.version);
                s.children.entry(dir).or_default().push(i);
            }
        }
        s.checkpoints.extend(checkpoints);
    }

    /// Lets `f` see the table as the index file will hold it.
    pub(crate) fn finish<T>(&self, f: impl FnOnce(&[Entry], &[Checkpoint]) -> T) -> T {
        self.write().tree.link_parents();
        let s = self.read();
        f(&s.tree.out, &s.checkpoints)
    }

    pub(crate) fn points(&self) -> Vec<Point> {
        self.read()
            .checkpoints
            .iter()
            .map(|c| Point {
                uncompressed_offset: c.uncompressed_offset,
                bit_offset: c.bit_offset,
            })
            .collect()
    }

    pub(crate) fn window(&self, uncompressed_offset: u64) -> Result<Vec<u8>> {
        let s = self.read();
        let i = s
            .checkpoints
            .partition_point(|c| c.uncompressed_offset < uncompressed_offset);
        match s.checkpoints.get(i) {
            Some(c) if c.uncompressed_offset == uncompressed_offset => Ok(c.window.clone()),
            _ => Err(Error::NotFound(format!(
                "checkpoint at {uncompressed_offset}"
            ))),
        }
    }

    pub(crate) fn state_len_differs(&self, len: usize) -> bool {
        self.read()
            .checkpoints
            .iter()
            .any(|c| c.window.len() != len)
    }

    pub(crate) fn for_each_member(&self, mut f: impl FnMut(Entry) -> Result<()>) -> Result<()> {
        // A snapshot, so that `f` may query (and the table grow) meanwhile.
        let members: Vec<Entry> = {
            let s = self.read();
            (0..s.tree.out.len())
                .filter(|&i| !s.tree.out[i].implicit)
                .map(|i| s.entry(i))
                .collect()
        };
        members.into_iter().try_for_each(&mut f)
    }

    pub(crate) fn get(&self, id: i64) -> Option<Entry> {
        let s = self.read();
        let i = usize::try_from(id.checked_sub(1)?).ok()?;
        (i < s.tree.out.len()).then(|| s.entry(i))
    }

    pub(crate) fn lookup(&self, path: &[u8]) -> Option<Entry> {
        let s = self.read();
        s.tree.latest.get(path).map(|&i| s.entry(i))
    }

    pub(crate) fn lookup_before(&self, path: &[u8], before: i64) -> Option<Entry> {
        let s = self.read();
        let mut at = s.tree.latest.get(path).copied();
        while let Some(i) = at {
            let e = &s.tree.out[i];
            if e.id < before && !e.implicit {
                return Some(s.entry(i));
            }
            at = s.prev[i];
        }
        None
    }

    pub(crate) fn children(&self, dir_id: i64) -> Vec<Entry> {
        let s = self.read();
        let Some(dir) = s.dir_path(dir_id) else {
            return Vec::new();
        };
        let mut out: Vec<Entry> = s
            .children
            .get(dir)
            .into_iter()
            .flatten()
            .map(|&i| &s.tree.out[i])
            .filter(|e| !e.superseded && !e.path.is_empty())
            .map(|e| {
                let mut e = e.clone();
                e.parent_id = dir_id;
                e
            })
            .collect();
        out.sort_by(|a, b| a.name().cmp(b.name()));
        out
    }

    pub(crate) fn entries_after(&self, after: i64, max: usize) -> Vec<Entry> {
        let s = self.read();
        let from = usize::try_from(after.max(0)).unwrap_or(usize::MAX);
        (from..s.tree.out.len())
            .take(max)
            .map(|i| s.entry(i))
            .collect()
    }

    pub(crate) fn dir_stamp(&self, dir_id: i64) -> u64 {
        let s = self.read();
        s.dir_path(dir_id)
            .and_then(|d| s.stamps.get(d).copied())
            .unwrap_or(0)
    }
}
