//! Indexing in the background: members show up while the index is built,
//! and the finished index is exactly the one a normal build makes.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tarloupe_core::archive::IndexStatus;
use tarloupe_core::vfs::Vfs;
use tarloupe_core::{Archive, Entry, EntryKind, Error, Indexing, OpenOptions, Progress};
use tarloupe_corpus::gz::gzip;
use tarloupe_corpus::{write_bulk, write_edge, Format};

/// Bulk files, then the edge corpus (duplicates, links, odd names).
fn sample_tar(bulk: u64) -> Vec<u8> {
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Pax, bulk, 11).unwrap();
    let mut edge = Vec::new();
    write_edge(&mut edge, Format::Gnu).unwrap();
    while tar.len() >= 512 && tar[tar.len() - 512..].iter().all(|&b| b == 0) {
        tar.truncate(tar.len() - 512);
    }
    tar.extend(edge);
    tar
}

fn opts(idx: &Path) -> OpenOptions {
    OpenOptions {
        index_dir: Some(idx.to_path_buf()),
        checkpoint_spacing: Some(1 << 20),
        ..Default::default()
    }
}

fn all_entries(a: &Archive) -> Vec<Entry> {
    a.index().entries_after(0, usize::MAX).unwrap()
}

fn contents(a: &Archive, e: &Entry) -> Vec<u8> {
    let mut out = Vec::new();
    a.copy_to(e, &mut out).unwrap();
    out
}

#[test]
fn a_live_index_ends_like_a_normal_one() {
    let dir = tempfile::tempdir().unwrap();
    let tar = sample_tar(6 << 20);
    let plain = dir.path().join("t.tar");
    let gz = dir.path().join("t.tar.gz");
    fs::write(&plain, &tar).unwrap();
    fs::write(&gz, gzip(&tar, 6, None).unwrap()).unwrap();
    for (path, threads) in [(&plain, 1), (&gz, 1), (&gz, 3)] {
        let normal = Archive::open(path, &opts(&dir.path().join("normal"))).unwrap();
        let idx = dir.path().join(format!("live{threads}"));
        let live = Archive::open_live(
            path,
            &OpenOptions {
                threads: Some(threads),
                ..opts(&idx)
            },
        )
        .unwrap();
        assert_eq!(live.index_status(), IndexStatus::Built);
        live.wait_indexed().unwrap();
        assert_eq!(live.indexing(), Indexing::Done);
        assert!(!live.index().is_live());
        let want = all_entries(&normal);
        assert_eq!(all_entries(&live), want, "{}", path.display());
        for e in want.iter().filter(|e| e.kind == EntryKind::File).step_by(7) {
            assert_eq!(
                contents(&live, e),
                contents(&normal, e),
                "{}",
                e.display_path()
            );
        }
        // The file it wrote is a normal index.
        drop(live);
        let again = Archive::open_live(path, &opts(&idx)).unwrap();
        assert_eq!(again.index_status(), IndexStatus::Loaded);
        assert_eq!(again.indexing(), Indexing::Done);
        assert_eq!(all_entries(&again), want);
    }
}

/// Progress that pauses the build halfway until `resume` gets a message,
/// after telling `paused`. Every report waits a little first, so the
/// members found so far have been passed on when it pauses.
fn pausing(paused: mpsc::Sender<()>, resume: mpsc::Receiver<()>) -> Progress {
    let gate = Mutex::new(Some((paused, resume)));
    Arc::new(move |done, total| {
        std::thread::sleep(Duration::from_millis(120));
        if done >= total / 2 && done < total {
            if let Some((paused, resume)) = gate.lock().unwrap().take() {
                paused.send(()).unwrap();
                let _ = resume.recv_timeout(Duration::from_secs(60));
            }
        }
    })
}

#[test]
fn members_show_up_while_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let tar = sample_tar(16 << 20);
    let gz = dir.path().join("t.tar.gz");
    fs::write(&gz, gzip(&tar, 6, None).unwrap()).unwrap();
    let normal = Archive::open(&gz, &opts(&dir.path().join("normal"))).unwrap();
    let want = all_entries(&normal);

    let (paused_tx, paused) = mpsc::channel();
    let (resume, resume_rx) = mpsc::channel();
    let live = Arc::new(
        Archive::open_live(
            &gz,
            &OpenOptions {
                progress: Some(pausing(paused_tx, resume_rx)),
                // The parallel reader reports progress a whole chunk at a
                // time; the sequential one pauses closer to halfway.
                threads: Some(1),
                ..opts(&dir.path().join("live"))
            },
        )
        .unwrap(),
    );
    paused.recv_timeout(Duration::from_secs(60)).unwrap();

    assert_eq!(live.indexing(), Indexing::Running);
    assert!(live.index().is_live());
    let seen = all_entries(&live);
    assert!(!seen.is_empty(), "nothing found halfway");
    assert!(seen.len() < want.len(), "everything found halfway");
    // What is there has its final id, and later members are not there yet.
    for e in &seen {
        let w = &want[e.id as usize - 1];
        assert_eq!((&e.path, e.data_offset), (&w.path, w.data_offset));
    }
    let last = want.iter().rev().find(|e| !e.implicit).unwrap();
    assert!(live.lookup(&last.path).unwrap().is_none());
    // Members found so far can be read, listed and looked up.
    let files: Vec<&Entry> = seen
        .iter()
        .filter(|e| e.kind == EntryKind::File && !e.superseded)
        .collect();
    assert!(!files.is_empty());
    for e in files.iter().step_by(11) {
        assert_eq!(
            contents(&live, e),
            contents(&normal, e),
            "{}",
            e.display_path()
        );
        assert_eq!(live.lookup(&e.path).unwrap().map(|x| x.id), Some(e.id));
    }
    let vfs = Vfs::new(live.clone()).unwrap();
    let root = vfs.list(vfs.root()).unwrap();
    assert!(root.get("bulk").is_some());
    let bulk = root.get("bulk").unwrap().1.clone();
    let before = vfs.list(&bulk).unwrap().entries.len();

    resume.send(()).unwrap();
    live.wait_indexed().unwrap();
    assert_eq!(all_entries(&live), want);
    // Listings made while indexing are not kept once they are out of date.
    let after = vfs.list(&bulk).unwrap().entries.len();
    let full = Vfs::new(Arc::new(normal)).unwrap();
    let bulk_full = full
        .list(full.root())
        .unwrap()
        .get("bulk")
        .unwrap()
        .1
        .clone();
    assert_eq!(after, full.list(&bulk_full).unwrap().entries.len());
    assert!(before <= after);
}

#[test]
fn dropping_the_archive_stops_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let gz = dir.path().join("t.tar.gz");
    fs::write(&gz, gzip(&sample_tar(16 << 20), 6, None).unwrap()).unwrap();
    let idx = dir.path().join("idx");
    let (started_tx, started) = mpsc::channel();
    let started_tx = Mutex::new(started_tx);
    let progress: Progress = Arc::new(move |_, _| {
        let _ = started_tx.lock().unwrap().send(());
        std::thread::sleep(Duration::from_millis(50));
    });
    let live = Archive::open_live(
        &gz,
        &OpenOptions {
            progress: Some(progress),
            ..opts(&idx)
        },
    )
    .unwrap();
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    let t = Instant::now();
    drop(live);
    assert!(t.elapsed() < Duration::from_secs(10));
    // No index was written.
    let written = fs::read_dir(&idx)
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(written, 0);
}

#[test]
fn a_cancelled_build_fails_but_keeps_what_it_found() {
    let dir = tempfile::tempdir().unwrap();
    let gz = dir.path().join("t.tar.gz");
    fs::write(&gz, gzip(&sample_tar(16 << 20), 6, None).unwrap()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let progress: Progress = Arc::new(move |done, total| {
        std::thread::sleep(Duration::from_millis(120));
        if done >= total / 2 {
            stop.store(true, Ordering::Relaxed);
        }
    });
    let live = Archive::open_live(
        &gz,
        &OpenOptions {
            progress: Some(progress),
            cancel: Some(cancel),
            ..opts(&dir.path().join("idx"))
        },
    )
    .unwrap();
    match live.wait_indexed() {
        Err(Error::IndexingFailed(why)) => assert_eq!(why, "cancelled"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(live.indexing(), Indexing::Failed(_)));
    assert!(live.index().is_live());
    assert!(!all_entries(&live).is_empty());
}
