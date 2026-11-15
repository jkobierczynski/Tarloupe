//! A read-only, Windows-shaped view of an archive for file system front
//! ends (WinFsp now, FUSE later).
//!
//! Tar names are bytes and may hold anything Windows refuses, and two names
//! can differ only in case. This layer turns every directory into a list of
//! unique, legal Windows names and resolves paths case-insensitively:
//!
//! * Characters Windows forbids (`< > : " / \ | ? *`, control characters)
//!   and a trailing dot or space become look-alike characters from the
//!   Unicode private use area (U+F000 + the ASCII code, as Cygwin and WSL
//!   do). Bytes that are not UTF-8 become U+F100 + the byte.
//! * Reserved device names (`CON`, `nul.txt`, `COM1`, ...) get their first
//!   character mapped the same way.
//! * A name that is still taken in its directory, ignoring case, gets a
//!   suffix: `Makefile`, then `makefile (2)`.
//! * Hard links show the data they share; symlinks are followed, and
//!   symlinks that dangle or leave the archive are hidden, as are devices
//!   and FIFOs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::archive::Archive;
use crate::entry::{Entry, EntryKind};
use crate::error::{Error, Result};

/// Longest name Windows accepts, in UTF-16 units.
const MAX_NAME: usize = 255;
/// Directory listings kept in memory.
const DIR_CACHE: usize = 4096;

/// A file or directory as the file system shows it.
#[derive(Debug, Clone)]
pub struct Node {
    /// The entry whose data and metadata are shown (links resolved).
    pub entry: Entry,
    pub is_dir: bool,
}

impl Node {
    pub fn size(&self) -> u64 {
        if self.is_dir {
            0
        } else {
            self.entry.size
        }
    }
    /// A stable number for the file (the index row of its data).
    pub fn id(&self) -> u64 {
        self.entry.id as u64
    }
    /// Seconds since 1970.
    pub fn mtime(&self) -> i64 {
        self.entry.mtime
    }
    /// Directory key: entries whose parent is this id are its children.
    fn dir_id(&self) -> i64 {
        if self.entry.path.is_empty() {
            0
        } else {
            self.entry.id
        }
    }
}

/// One directory's children under their Windows names, sorted by name.
#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<(String, Node)>,
    by_fold: HashMap<String, usize>,
}

impl Listing {
    /// Finds a child by name, ignoring case like Windows does.
    pub fn get(&self, name: &str) -> Option<&(String, Node)> {
        self.by_fold.get(&fold(name)).map(|&i| &self.entries[i])
    }
}

pub struct Vfs {
    archive: Arc<Archive>,
    root: Node,
    cache: Mutex<DirCache>,
}

#[derive(Default)]
struct DirCache {
    map: HashMap<i64, (Arc<Listing>, u64)>,
    tick: u64,
}

impl Vfs {
    pub fn new(archive: Arc<Archive>) -> Result<Vfs> {
        let root = match archive.lookup(b"")? {
            Some(e) if e.kind == EntryKind::Dir => e,
            _ => root_entry(),
        };
        Ok(Vfs {
            archive,
            root: Node {
                entry: root,
                is_dir: true,
            },
            cache: Mutex::new(DirCache::default()),
        })
    }

    pub fn archive(&self) -> &Archive {
        &self.archive
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Resolves a Windows path (`\dir\file`, `/` also accepted) to a node.
    pub fn lookup(&self, path: &str) -> Result<Option<Node>> {
        let mut node = self.root.clone();
        for comp in path.split(['\\', '/']).filter(|c| !c.is_empty()) {
            if !node.is_dir {
                return Ok(None);
            }
            let listing = self.list(&node)?;
            match listing.get(comp) {
                Some((_, child)) => node = child.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(node))
    }

    /// The children of a directory node.
    pub fn list(&self, dir: &Node) -> Result<Arc<Listing>> {
        let key = dir.dir_id();
        {
            let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            c.tick += 1;
            let t = c.tick;
            if let Some((l, used)) = c.map.get_mut(&key) {
                *used = t;
                return Ok(l.clone());
            }
        }
        let listing = Arc::new(self.build_listing(key)?);
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.map.len() >= DIR_CACHE {
            if let Some(&old) = c.map.iter().min_by_key(|(_, (_, u))| *u).map(|(k, _)| k) {
                c.map.remove(&old);
            }
        }
        c.tick += 1;
        let t = c.tick;
        c.map.insert(key, (listing.clone(), t));
        Ok(listing)
    }

    fn build_listing(&self, dir_id: i64) -> Result<Listing> {
        let mut out = Listing::default();
        for e in self.archive.index().children(dir_id)? {
            let Some(node) = self.node_for(e.clone())? else {
                continue;
            };
            let base = windows_name(e.name());
            let mut name = base.clone();
            let mut n = 2;
            while out.by_fold.contains_key(&fold(&name)) {
                name = with_suffix(&base, n);
                n += 1;
            }
            out.by_fold.insert(fold(&name), out.entries.len());
            out.entries.push((name, node));
        }
        Ok(out)
    }

    /// What an index entry shows as, or `None` when it is hidden.
    fn node_for(&self, e: Entry) -> Result<Option<Node>> {
        let target = match e.kind {
            EntryKind::HardLink | EntryKind::Symlink => match self.archive.resolve(e) {
                Ok(t) => t,
                Err(Error::NotFound(_)) | Err(Error::Unsupported(_)) => return Ok(None),
                Err(err) => return Err(err),
            },
            _ => e,
        };
        Ok(match target.kind {
            EntryKind::Dir => Some(Node {
                is_dir: true,
                entry: if target.path.is_empty() && target.implicit {
                    self.root.entry.clone()
                } else {
                    target
                },
            }),
            EntryKind::File => Some(Node {
                entry: target,
                is_dir: false,
            }),
            _ => None,
        })
    }

    /// Reads file data; returns the number of bytes read (0 at the end).
    pub fn read(&self, node: &Node, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if node.is_dir {
            return Err(Error::Unsupported("is a directory".into()));
        }
        self.archive.read_at(&node.entry, offset, buf)
    }
}

/// The archive root when the archive has no `./` member.
fn root_entry() -> Entry {
    Entry {
        id: 0,
        parent_id: 0,
        path: Vec::new(),
        raw_name: Vec::new(),
        typeflag: b'5',
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

/// Case folding used for lookups (close to NTFS's upcase table).
pub fn fold(name: &str) -> String {
    name.chars().flat_map(char::to_uppercase).collect()
}

fn private(c: u32) -> char {
    char::from_u32(0xF000 + c).unwrap()
}

/// Maps a tar name (bytes) to a legal Windows name.
pub fn windows_name(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            let forbidden = (c as u32) < 0x20
                || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
            out.push(if forbidden { private(c as u32) } else { c });
        }
        for &b in chunk.invalid() {
            out.push(private(0x100 + b as u32));
        }
    }
    // Windows drops a trailing dot or space; keep them visible.
    if let Some(last) = out.chars().last() {
        if last == '.' || last == ' ' {
            out.pop();
            out.push(private(last as u32));
        }
    }
    if is_reserved(&out) {
        let first = out.chars().next().unwrap();
        out.replace_range(..first.len_utf8(), &private(first as u32).to_string());
    }
    if out.is_empty() {
        out.push(private(0));
    }
    truncate_utf16(out, MAX_NAME)
}

/// True for device names Windows reserves, with or without an extension.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    if matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let rest = up.strip_prefix("COM").or_else(|| up.strip_prefix("LPT"));
    matches!(
        rest,
        Some(
            "0" | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
                | "9"
                | "\u{b9}"
                | "\u{b2}"
                | "\u{b3}"
        )
    )
}

/// Shortens a name to `max` UTF-16 units, keeping the extension and adding
/// a hash so that different long names stay different.
fn truncate_utf16(name: String, max: usize) -> String {
    if name.encode_utf16().count() <= max {
        return name;
    }
    let hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes());
    let tag = format!("~{:08x}", hash as u32);
    let ext = match name.rfind('.') {
        Some(i) if name.len() - i <= 16 => &name[i..],
        _ => "",
    };
    let budget = max - tag.len() - ext.encode_utf16().count();
    let mut stem = String::new();
    let mut used = 0;
    for c in name.chars() {
        if used + c.len_utf16() > budget {
            break;
        }
        used += c.len_utf16();
        stem.push(c);
    }
    format!("{stem}{tag}{ext}")
}

/// `name (n)`, before the extension.
fn with_suffix(name: &str, n: usize) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    truncate_utf16(format!("{stem} ({n}){ext}"), MAX_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(windows_name(b"plain.txt"), "plain.txt");
        assert_eq!(windows_name(b"a:b"), "a\u{f03a}b");
        assert_eq!(
            windows_name(b"q?*\"<>|\\"),
            "q\u{f03f}\u{f02a}\u{f022}\u{f03c}\u{f03e}\u{f07c}\u{f05c}"
        );
        assert_eq!(windows_name(b"trailing."), "trailing\u{f02e}");
        assert_eq!(windows_name(b"space "), "space\u{f020}");
        assert_eq!(windows_name(b"tab\there"), "tab\u{f009}here");
        assert_eq!(windows_name(b"bad\xffutf8"), "bad\u{f1ff}utf8");
        assert_eq!(windows_name(b"CON"), "\u{f043}ON");
        assert_eq!(windows_name(b"nul.txt"), "\u{f06e}ul.txt");
        assert_eq!(windows_name(b"com1"), "\u{f063}om1");
        assert_eq!(windows_name(b"CONSOLE"), "CONSOLE");
        assert_eq!(windows_name(b"com10"), "com10");
        assert_eq!(windows_name("Grüße 日本".as_bytes()), "Grüße 日本");
    }

    #[test]
    fn long_names_stay_distinct() {
        let a = windows_name(&[b'a'; 300]);
        let mut bn = vec![b'a'; 299];
        bn.push(b'b');
        let b = windows_name(&bn);
        assert_eq!(a.encode_utf16().count(), 255);
        assert_ne!(a, b);
        let long_ext = windows_name(format!("{}.txt", "x".repeat(400)).as_bytes());
        assert!(long_ext.ends_with(".txt") && long_ext.encode_utf16().count() <= 255);
    }

    #[test]
    fn suffixes() {
        assert_eq!(with_suffix("makefile", 2), "makefile (2)");
        assert_eq!(with_suffix("Read.Me.txt", 3), "Read.Me (3).txt");
        assert_eq!(with_suffix(".bashrc", 2), ".bashrc (2)");
        assert_eq!(fold("Straße"), fold("STRASSE"));
    }
}
