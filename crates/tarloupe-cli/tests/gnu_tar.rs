//! M0 exit criterion: the index lists archives exactly like `tar -tv`, and
//! reads every file back byte for byte. Needs GNU tar on PATH; the tests
//! that use it are skipped (with a note) when it is missing.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use tarloupe_core::archive::IndexStatus;
use tarloupe_core::{Archive, EntryKind, OpenOptions};
use tarloupe_corpus::{write_edge, Format};

fn gnu_tar() -> Option<&'static str> {
    for exe in ["gtar", "tar"] {
        if let Ok(out) = Command::new(exe).arg("--version").output() {
            if String::from_utf8_lossy(&out.stdout).contains("GNU tar") {
                return Some(exe);
            }
        }
    }
    eprintln!("GNU tar not found; skipping comparison");
    None
}

fn tar_cmd(tar: &str) -> Command {
    let mut c = Command::new(tar);
    c.env("TZ", "UTC").env("LC_ALL", "C.UTF-8");
    c
}

fn tarloupe(index_dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tarloupe"));
    c.env("TARLOUPE_INDEX_DIR", index_dir);
    c
}

fn run(mut c: Command) -> Vec<u8> {
    let out = c.output().expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        c,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn assert_same_listing(tar: &str, archive: &Path, index_dir: &Path) {
    let mut t = tar_cmd(tar);
    t.arg("-tvf").arg(archive);
    let expected = run(t);
    let mut l = tarloupe(index_dir);
    l.arg("list").arg("-v").arg(archive);
    let actual = run(l);
    if expected != actual {
        let e = String::from_utf8_lossy(&expected);
        let a = String::from_utf8_lossy(&actual);
        for (i, (x, y)) in e.lines().zip(a.lines()).enumerate() {
            if x != y {
                panic!(
                    "{}: line {} differs\n tar:      {x}\n tarloupe: {y}",
                    archive.display(),
                    i + 1
                );
            }
        }
        panic!(
            "{}: line counts differ ({} vs {})",
            archive.display(),
            e.lines().count(),
            a.lines().count()
        );
    }
    // The plain listing matches `tar -t` too.
    let mut t = tar_cmd(tar);
    t.arg("-tf").arg(archive);
    let mut l = tarloupe(index_dir);
    l.arg("list").arg(archive);
    assert_eq!(
        String::from_utf8_lossy(&run(t)),
        String::from_utf8_lossy(&run(l))
    );
}

/// Extracts with GNU tar and compares every regular file with what the
/// index reads back.
fn assert_same_contents(tar: &str, archive: &Path, index_dir: &Path) {
    if cfg!(windows) {
        // The corpus has names Windows cannot create; M2 handles those.
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let mut t = tar_cmd(tar);
    t.arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(out.path())
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .arg("--exclude=*/special/*")
        .arg("--exclude=*/meta/no-perms");
    run(t);

    let opts = OpenOptions {
        index_dir: Some(index_dir.to_path_buf()),
        ..Default::default()
    };
    let a = Archive::open(archive, &opts).unwrap();
    let mut checked = 0;
    for path in walk(out.path()) {
        let rel = path.strip_prefix(out.path()).unwrap();
        let key = rel.to_string_lossy();
        #[cfg(windows)]
        let key = key.replace('\\', "/");
        let e = a
            .lookup(key.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("{key} missing from index"));
        assert!(
            matches!(e.kind, EntryKind::File | EntryKind::HardLink),
            "{key}: {:?}",
            e.kind
        );
        let mut got = Vec::new();
        a.copy_to(&e, &mut got).unwrap();
        let want = fs::read(&path).unwrap();
        assert!(
            got == want,
            "{key}: contents differ ({} vs {} bytes)",
            got.len(),
            want.len()
        );
        checked += 1;
    }
    assert!(checked > 0, "no files compared");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let ft = e.file_type().unwrap();
        if ft.is_dir() {
            v.extend(walk(&e.path()));
        } else if ft.is_file() {
            v.push(e.path());
        }
    }
    v
}

#[test]
fn edge_corpus_matches_gnu_tar() {
    let Some(tar) = gnu_tar() else { return };
    let dir = tempfile::tempdir().unwrap();
    for format in [Format::Gnu, Format::Pax, Format::Ustar] {
        let path = dir.path().join(format!("edge-{format:?}.tar"));
        write_edge(File::create(&path).unwrap(), format).unwrap();
        assert_same_listing(tar, &path, &dir.path().join("idx"));
        assert_same_contents(tar, &path, &dir.path().join("idx"));
    }
}

#[test]
fn gzip_archives_match_gnu_tar() {
    let Some(tar) = gnu_tar() else { return };
    if Command::new("gzip").arg("--version").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("edge.tar");
    write_edge(File::create(&plain).unwrap(), Format::Pax).unwrap();
    for level in ["-1", "-9"] {
        let out = Command::new("gzip")
            .args([level, "-c"])
            .arg(&plain)
            .output()
            .unwrap();
        assert!(out.status.success());
        let path = dir.path().join(format!("edge{level}.tar.gz"));
        fs::write(&path, out.stdout).unwrap();
        assert_same_listing(tar, &path, &dir.path().join("idx"));
        assert_same_contents(tar, &path, &dir.path().join("idx"));
    }
}

#[test]
#[cfg(unix)]
fn gnu_tar_created_archives_with_sparse_files() {
    use std::io::{Seek, SeekFrom};
    let Some(tar) = gnu_tar() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let long = src.join("l".repeat(110)).join("m".repeat(110));
    fs::create_dir_all(&long).unwrap();
    fs::write(long.join("inner.txt"), b"inside a long path\n").unwrap();
    fs::write(src.join("plain.txt"), b"hello\n").unwrap();
    std::os::unix::fs::symlink("plain.txt", src.join("sym")).unwrap();
    fs::hard_link(src.join("plain.txt"), src.join("hard")).unwrap();
    // Sparse file: 8 MiB with data at 0, 3 MiB and the very end.
    let mut f = File::create(src.join("sparse.img")).unwrap();
    f.set_len(8 << 20).unwrap();
    for (off, data) in [
        (0u64, &b"head"[..]),
        (3 << 20, b"middle"),
        ((8 << 20) - 4, b"tail"),
    ] {
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(data).unwrap();
    }
    drop(f);

    let variants: &[(&str, &[&str])] = &[
        ("gnu", &["--format=gnu", "-S"]),
        ("oldgnu", &["--format=oldgnu", "-S"]),
        ("pax-1.0", &["--format=pax", "-S", "--sparse-version=1.0"]),
        ("pax-0.1", &["--format=pax", "-S", "--sparse-version=0.1"]),
        ("pax-0.0", &["--format=pax", "-S", "--sparse-version=0.0"]),
        ("pax-plain", &["--format=pax"]),
    ];
    for (name, flags) in variants {
        let archive = dir.path().join(format!("{name}.tar"));
        let mut t = tar_cmd(tar);
        t.args(*flags)
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path())
            .arg("src");
        run(t);
        assert_same_listing(tar, &archive, &dir.path().join("idx"));
        assert_same_contents(tar, &archive, &dir.path().join("idx"));
    }
}

#[test]
fn index_is_reused_and_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.tar");
    write_edge(File::create(&path).unwrap(), Format::Gnu).unwrap();
    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    assert_eq!(
        Archive::open(&path, &opts).unwrap().index_status(),
        IndexStatus::Built
    );
    assert_eq!(
        Archive::open(&path, &opts).unwrap().index_status(),
        IndexStatus::Loaded
    );

    // Renaming keeps the index.
    let renamed = dir.path().join("renamed.tar");
    fs::rename(&path, &renamed).unwrap();
    assert_eq!(
        Archive::open(&renamed, &opts).unwrap().index_status(),
        IndexStatus::Loaded
    );

    // A sidecar index is honoured and validated against the archive.
    let sidecar = OpenOptions {
        index_path: Some(dir.path().join("renamed.tar.tli")),
        ..Default::default()
    };
    assert_eq!(
        Archive::open(&renamed, &sidecar).unwrap().index_status(),
        IndexStatus::Built
    );
    let mut f = fs::OpenOptions::new().append(true).open(&renamed).unwrap();
    f.write_all(&[0u8; 10240]).unwrap();
    drop(f);
    assert_eq!(
        Archive::open(&renamed, &sidecar).unwrap().index_status(),
        IndexStatus::Rebuilt
    );
}

#[test]
fn browsing_follows_tar_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.tar");
    write_edge(File::create(&path).unwrap(), Format::Pax).unwrap();
    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    let a = Archive::open(&path, &opts).unwrap();

    let names = |p: &str| -> Vec<String> {
        a.list_dir(p.as_bytes())
            .unwrap()
            .iter()
            .map(|e| String::from_utf8_lossy(e.name()).into_owned())
            .collect()
    };
    assert_eq!(names(""), ["corpus"]);
    // Duplicates collapse to the last member.
    assert_eq!(names("corpus/dup"), ["file.txt"]);
    assert_eq!(a.lookup(b"corpus/dup/file.txt").unwrap().unwrap().size, 22);
    // Implicit parents and `./` prefixes.
    assert_eq!(names("corpus/implicit/a/b"), ["c"]);
    assert_eq!(names("corpus/dot-slash"), ["file.txt"]);
    // Case-distinct names are all present.
    assert_eq!(
        names("corpus/case"),
        ["Dir", "MAKEFILE", "Makefile", "dir", "makefile"]
    );
    // A symlinked directory can be browsed.
    assert_eq!(names("corpus/links/to-dir").len(), 6);
    // Symlink chains and hard links resolve to data.
    let chain = a.lookup(b"corpus/links/chain").unwrap().unwrap();
    assert_eq!(
        a.resolve(chain).unwrap().path,
        b"corpus/files/one-block.bin"
    );
    let mut buf = Vec::new();
    a.copy_to(&a.lookup(b"corpus/links/hard").unwrap().unwrap(), &mut buf)
        .unwrap();
    assert_eq!(buf.len(), 1024 * 1024 + 1);
    // Dangling and escaping links fail cleanly.
    assert!(a
        .resolve(a.lookup(b"corpus/links/dangling").unwrap().unwrap())
        .is_err());
    // Random access in the middle of a file.
    let big = a
        .lookup(b"corpus/files/one-mib-plus-one.bin")
        .unwrap()
        .unwrap();
    let mut part = [0u8; 100];
    assert_eq!(a.read_at(&big, 1024 * 1024 - 50, &mut part).unwrap(), 51);
    assert_eq!(&part[..51], &buf[1024 * 1024 - 50..]);
}

#[test]
fn damaged_compressed_archives_name_their_codec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.tar.xz");
    fs::write(&path, [0xfd, b'7', b'z', b'X', b'Z', 0, 0, 4]).unwrap();
    let out = tarloupe(&dir.path().join("idx"))
        .arg("list")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an xz stream"), "{err}");
}

#[test]
fn self_named_hard_links_and_links_to_the_root() {
    use tarloupe_corpus::{Content, Kind, Member, TarWriter};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("links.tar");
    let mut w = TarWriter::new(File::create(&path).unwrap(), Format::Gnu);
    let f = Member::new("top/f", Kind::File, 100, 0);
    w.append(&f, &mut Content::new(1, 100)).unwrap();
    // `tar --hard-dereference`-less archives can repeat a path as a hard
    // link to itself; the data is in the earlier member.
    let l = Member::new("top/f", Kind::HardLink(b"top/f".to_vec()), 0, 0);
    w.append(&l, &mut std::io::empty()).unwrap();
    let r = Member::new("rootlink", Kind::Symlink(b"/".to_vec()), 0, 0);
    w.append(&r, &mut std::io::empty()).unwrap();
    w.finish().unwrap();

    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    let a = Archive::open(&path, &opts).unwrap();
    let mut buf = Vec::new();
    a.copy_to(&a.lookup(b"top/f").unwrap().unwrap(), &mut buf)
        .unwrap();
    let mut want = Vec::new();
    std::io::Read::read_to_end(&mut Content::new(1, 100), &mut want).unwrap();
    assert_eq!(buf, want);

    let names = |p: &[u8]| -> Vec<Vec<u8>> {
        a.list_dir(p)
            .unwrap()
            .iter()
            .map(|e| e.name().to_vec())
            .collect()
    };
    assert_eq!(names(b"rootlink"), names(b""));
}

#[test]
fn index_safety() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.tar");
    write_edge(File::create(&path).unwrap(), Format::Gnu).unwrap();
    let before = fs::read(&path).unwrap();

    // The archive itself is never used as its index destination.
    let alias = dir.path().join(".").join("a.tar");
    let opts = OpenOptions {
        index_path: Some(alias),
        ..Default::default()
    };
    assert!(Archive::open(&path, &opts).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);

    // ... nor when the cache name derived from its key is the archive.
    let id = tarloupe_core::key::ArchiveKey::compute(&path).unwrap().id();
    let keyed = dir.path().join(format!("{id}.tli"));
    fs::rename(&path, &keyed).unwrap();
    let opts = OpenOptions {
        index_dir: Some(dir.path().to_path_buf()),
        rebuild: true,
        ..Default::default()
    };
    assert!(Archive::open(&keyed, &opts).is_err());
    assert_eq!(fs::read(&keyed).unwrap(), before);
    fs::rename(&keyed, &path).unwrap();

    // A corrupt index is not silently rebuilt when one is required.
    let sidecar = dir.path().join("a.tar.tli");
    fs::write(&sidecar, b"not a database").unwrap();
    let opts = OpenOptions {
        index_path: Some(sidecar.clone()),
        require_index: true,
        ..Default::default()
    };
    assert!(Archive::open(&path, &opts).is_err());

    // A sidecar is rejected when only the archive's mtime changed.
    let opts = OpenOptions {
        index_path: Some(sidecar),
        ..Default::default()
    };
    assert_eq!(
        Archive::open(&path, &opts).unwrap().index_status(),
        IndexStatus::Rebuilt
    );
    let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();
    drop(f);
    assert_eq!(
        Archive::open(&path, &opts).unwrap().index_status(),
        IndexStatus::Rebuilt
    );
    assert_eq!(
        Archive::open(&path, &opts).unwrap().index_status(),
        IndexStatus::Loaded
    );
}
