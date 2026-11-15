//! The Windows-shaped view must show every regular file under a legal,
//! unique name, with the same contents.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::sync::Arc;

use tarloupe_core::vfs::{fold, Node, Vfs};
use tarloupe_core::{Archive, EntryKind, OpenOptions};
use tarloupe_corpus::{write_edge, Format, Kind, Member, TarWriter};

fn legal(name: &str) -> bool {
    !name.is_empty()
        && name.encode_utf16().count() <= 255
        && !name
            .chars()
            .any(|c| (c as u32) < 0x20 || "<>:\"/\\|?*".contains(c))
        && !name.ends_with(['.', ' '])
}

/// Walks the view; returns display path -> node for every file.
fn walk(vfs: &Vfs) -> HashMap<String, Node> {
    let mut files = HashMap::new();
    let mut seen = HashSet::new();
    let mut todo = vec![(String::new(), vfs.root().clone())];
    while let Some((path, dir)) = todo.pop() {
        if !seen.insert(dir.id()) {
            continue; // a symlink back up the tree
        }
        let listing = vfs.list(&dir).unwrap();
        let mut folded = HashSet::new();
        for (name, node) in &listing.entries {
            assert!(legal(name), "illegal name {name:?} in {path:?}");
            assert!(folded.insert(fold(name)), "duplicate {name:?} in {path:?}");
            let child = format!("{path}\\{name}");
            if node.is_dir {
                todo.push((child, node.clone()));
            } else {
                files.insert(child, node.clone());
            }
        }
    }
    files
}

#[test]
fn edge_corpus_through_the_windows_view() {
    let dir = tempfile::tempdir().unwrap();
    for format in [Format::Gnu, Format::Pax] {
        let path = dir.path().join(format!("edge-{format:?}.tar"));
        write_edge(File::create(&path).unwrap(), format).unwrap();
        let a = Arc::new(
            Archive::open(
                &path,
                &OpenOptions {
                    index_dir: Some(dir.path().join("idx")),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let vfs = Vfs::new(a.clone()).unwrap();
        let files = walk(&vfs);

        // Every current regular file is shown somewhere, with its data.
        let shown: HashSet<i64> = files.values().map(|n| n.entry.id).collect();
        let mut members = Vec::new();
        a.for_each_member(|e| {
            members.push(e);
            Ok(())
        })
        .unwrap();
        for e in members
            .iter()
            .filter(|e| e.kind == EntryKind::File && !e.superseded)
        {
            assert!(shown.contains(&e.id), "{} is not shown", e.display_path());
        }
        for (p, node) in &files {
            let mut want = Vec::new();
            a.copy_to(&node.entry, &mut want).unwrap();
            let mut got = vec![0u8; node.size() as usize + 10];
            let mut n = 0;
            loop {
                let k = vfs.read(node, n as u64, &mut got[n..]).unwrap();
                if k == 0 {
                    break;
                }
                n += k;
            }
            assert_eq!(&got[..n], &want[..], "{p}");
            // Paths resolve back, ignoring case.
            let back = vfs.lookup(&p.to_uppercase()).unwrap();
            assert!(
                back.is_some_and(|b| b.entry.id == node.entry.id),
                "{p} (upper case)"
            );
            assert_eq!(
                vfs.lookup(p).unwrap().unwrap().entry.id,
                node.entry.id,
                "{p}"
            );
        }
    }
}

#[test]
fn collisions_reserved_names_and_links() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.tar");
    let mut w = TarWriter::new(File::create(&path).unwrap(), Format::Gnu);
    let mut add = |name: &str, kind: Kind, data: &[u8]| {
        let m = Member::new(name, kind, data.len() as u64, 1_700_000_000);
        w.append(&m, &mut &data[..]).unwrap();
    };
    add("d/Makefile", Kind::File, b"upper");
    add("d/makefile", Kind::File, b"lower");
    add("d/MAKEFILE", Kind::File, b"caps");
    add("d/a:b", Kind::File, b"colon");
    add("d/a\u{f03a}b", Kind::File, b"lookalike");
    add("d/CON", Kind::File, b"device name");
    add("d/link", Kind::Symlink(b"Makefile".to_vec()), b"");
    add("d/dangling", Kind::Symlink(b"nowhere".to_vec()), b"");
    add("d/hard", Kind::HardLink(b"d/makefile".to_vec()), b"");
    add("dirlink", Kind::Symlink(b"d".to_vec()), b"");
    add("fifo", Kind::Fifo, b"");
    drop(add);
    w.finish().unwrap();

    let a = Archive::open(
        &path,
        &OpenOptions {
            index_dir: Some(dir.path().join("idx")),
            ..Default::default()
        },
    )
    .unwrap();
    let vfs = Vfs::new(Arc::new(a)).unwrap();
    let read = |p: &str| {
        let n = vfs
            .lookup(p)
            .unwrap()
            .unwrap_or_else(|| panic!("{p} missing"));
        let mut b = vec![0u8; 64];
        let k = vfs.read(&n, 0, &mut b).unwrap();
        String::from_utf8(b[..k].to_vec()).unwrap()
    };
    let d = vfs.lookup("\\d").unwrap().unwrap();
    let names: Vec<String> = vfs
        .list(&d)
        .unwrap()
        .entries
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    assert_eq!(names.len(), 8, "{names:?}"); // the dangling link is hidden
                                             // The three makefiles get distinct names; which one keeps the plain
                                             // name follows the index order (sorted by raw name).
    let mut makes = Vec::new();
    for n in &names {
        if fold(n).starts_with("MAKEFILE") {
            makes.push(read(&format!("\\d\\{n}")));
        }
    }
    makes.sort();
    assert_eq!(makes, ["caps", "lower", "upper"]);
    assert!(names.iter().any(|n| n == "a\u{f03a}b"));
    assert!(names.iter().any(|n| n == "a\u{f03a}b (2)"));
    assert_eq!(read("\\d\\\u{f043}ON"), "device name");
    assert_eq!(read("\\d\\link"), "upper");
    assert_eq!(read("\\d\\hard"), "lower");
    assert_eq!(read("\\DIRLINK\\link"), "upper");
    assert!(vfs.lookup("\\fifo").unwrap().is_none());
    assert!(vfs.lookup("\\d\\dangling").unwrap().is_none());
    assert!(vfs.lookup("\\d\\link\\x").unwrap().is_none());
    let _ = fs::remove_dir_all(dir.path().join("idx"));
}
