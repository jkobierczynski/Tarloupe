//! `tarloupe mount` must show the archive through Windows itself: every
//! folder lists the same names as the view, every file reads back the same
//! bytes, lookups ignore case and nothing can be written.
//!
//! Needs WinFsp. Without it the test is skipped, unless
//! `TARLOUPE_REQUIRE_WINFSP=1` (as in CI), when it fails instead.
#![cfg(windows)]

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use tarloupe_core::vfs::Vfs;
use tarloupe_core::{Archive, OpenOptions};
use tarloupe_corpus::gz::gzip;
use tarloupe_corpus::{write_bulk, write_edge, Format, Kind, Member, TarWriter};

/// A gzip archive with the edge corpus, some bulk data and the names that
/// need mapping on Windows.
fn sample(dir: &Path) -> PathBuf {
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Pax, 3 << 20, 11).unwrap();
    while tar.len() >= 512 && tar[tar.len() - 512..].iter().all(|&b| b == 0) {
        tar.truncate(tar.len() - 512);
    }
    write_edge(&mut tar, Format::Gnu).unwrap();
    while tar.len() >= 512 && tar[tar.len() - 512..].iter().all(|&b| b == 0) {
        tar.truncate(tar.len() - 512);
    }
    let mut w = TarWriter::new(&mut tar, Format::Gnu);
    for (name, kind, data) in [
        ("win/Makefile", Kind::File, &b"upper"[..]),
        ("win/makefile", Kind::File, b"lower"),
        ("win/a:b", Kind::File, b"colon"),
        ("win/CON", Kind::File, b"device name"),
        ("win/trailing.", Kind::File, b"dot"),
        ("win/link", Kind::Symlink(b"Makefile".to_vec()), b""),
    ] {
        let m = Member::new(name, kind, data.len() as u64, 1_700_000_000);
        w.append(&m, &mut &data[..]).unwrap();
    }
    w.finish().unwrap();
    let path = dir.join("sample.tar.gz");
    fs::write(&path, gzip(&tar, 6, None).unwrap()).unwrap();
    path
}

/// Kills the mount process if the test fails half way.
struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn mounted_archive_reads_like_the_view() {
    let required = std::env::var_os("TARLOUPE_REQUIRE_WINFSP").is_some_and(|v| v == "1");
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let archive = sample(dir.path());
    let a = Archive::open(
        &archive,
        &OpenOptions {
            index_dir: Some(idx.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let vfs = Vfs::new(Arc::new(a)).unwrap();

    // A folder mount point must not exist yet; its parent must.
    let mnt = dir.path().join("mnt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tarloupe"))
        .env("TARLOUPE_INDEX_DIR", &idx)
        .arg("mount")
        .arg(&archive)
        .arg(&mnt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut guard = Guard(Some(child));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_default();
    if line.trim().is_empty() {
        let c = guard.0.as_mut().unwrap();
        let _ = c.kill();
        let _ = c.wait();
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        if !required && err.contains("WinFsp is not installed") {
            eprintln!("skipping: WinFsp is not installed");
            return;
        }
        panic!("mount did not start: {err}");
    }
    assert_eq!(Path::new(line.trim()), mnt);

    // Walk the view and compare each folder and file with what Windows shows.
    let mut seen = HashSet::new();
    let mut todo = vec![(String::new(), vfs.root().clone())];
    let mut files = 0;
    while let Some((rel, node)) = todo.pop() {
        if !seen.insert(node.id()) {
            continue; // a symlink back up the tree
        }
        let here = mnt.join(rel.trim_start_matches('\\'));
        let listing = vfs.list(&node).unwrap();
        let want: BTreeSet<String> = listing.entries.iter().map(|(n, _)| n.clone()).collect();
        let got: BTreeSet<String> = fs::read_dir(&here)
            .unwrap_or_else(|e| panic!("{}: {e}", here.display()))
            .map(|d| d.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(got, want, "{}", here.display());
        for (name, child) in &listing.entries {
            let path = format!("{rel}\\{name}");
            let p = here.join(name);
            let md = fs::metadata(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(md.is_dir(), child.is_dir, "{}", p.display());
            if child.is_dir {
                todo.push((path, child.clone()));
                continue;
            }
            assert_eq!(md.len(), child.size(), "{}", p.display());
            assert!(md.permissions().readonly(), "{}", p.display());
            let mut want = vec![0u8; child.size() as usize];
            let mut n = 0;
            while n < want.len() {
                let k = vfs.read(child, n as u64, &mut want[n..]).unwrap();
                assert!(k > 0);
                n += k;
            }
            let got = fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert!(got == want, "contents of {}", p.display());
            files += 1;
        }
    }
    assert!(files > 20, "only {files} files");

    // Names are looked up without regard to case, and mapped names work.
    assert_eq!(fs::read(mnt.join("WIN").join("LINK")).unwrap(), b"upper");
    assert_eq!(
        fs::read(mnt.join("win").join("\u{f043}ON")).unwrap(),
        b"device name"
    );
    assert_eq!(
        fs::read(mnt.join("win").join("a\u{f03a}b")).unwrap(),
        b"colon"
    );
    assert_eq!(
        fs::read(mnt.join("win").join("trailing\u{f02e}")).unwrap(),
        b"dot"
    );

    // Nothing can be changed.
    assert!(fs::write(mnt.join("new.txt"), b"x").is_err());
    assert!(fs::OpenOptions::new()
        .append(true)
        .open(mnt.join("win").join("Makefile"))
        .is_err());
    assert!(fs::create_dir(mnt.join("newdir")).is_err());
    assert!(fs::remove_file(mnt.join("win").join("Makefile")).is_err());
    assert!(fs::rename(mnt.join("win").join("Makefile"), mnt.join("win").join("x")).is_err());

    // Ending the process removes the mount.
    let mut c = guard.0.take().unwrap();
    c.kill().unwrap();
    c.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while fs::read_dir(&mnt).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(fs::read_dir(&mnt).is_err(), "still mounted after exit");
}
