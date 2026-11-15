//! Gzip archives must read exactly like the plain tar they compress.

use std::fs::{self, File};
use std::path::Path;

use tarloupe_core::{Archive, Entry, EntryKind, OpenOptions};
use tarloupe_corpus::gz::gzip;
use tarloupe_corpus::{write_bulk, write_edge, Format, Rng};

fn open(path: &Path, idx: &Path, spacing: u64) -> Archive {
    Archive::open(
        path,
        &OpenOptions {
            index_dir: Some(idx.to_path_buf()),
            checkpoint_spacing: Some(spacing),
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn members(a: &Archive) -> Vec<Entry> {
    let mut v = Vec::new();
    a.for_each_member(|e| {
        v.push(e);
        Ok(())
    })
    .unwrap();
    v
}

/// A tar with every edge case followed by a few MiB of bulk files.
fn sample_tar() -> Vec<u8> {
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Pax, 6 << 20, 7).unwrap();
    // The bulk writer ends the archive; append the edge corpus after
    // dropping its two zero blocks so both parts are one archive.
    let mut edge = Vec::new();
    write_edge(&mut edge, Format::Gnu).unwrap();
    while tar.len() >= 512 && tar[tar.len() - 512..].iter().all(|&b| b == 0) {
        tar.truncate(tar.len() - 512);
    }
    tar.extend(edge);
    tar
}

/// Compares every member, its contents and many random reads.
fn assert_same(plain: &Archive, gz: &Archive, seed: u64) {
    let (pm, gm) = (members(plain), members(gz));
    assert_eq!(pm.len(), gm.len());
    for (p, g) in pm.iter().zip(&gm) {
        assert_eq!(p.path, g.path);
        assert_eq!(p.data_offset, g.data_offset);
        assert_eq!(p.size, g.size);
    }
    let files: Vec<Entry> = pm
        .iter()
        .filter(|e| e.kind == EntryKind::File && !e.superseded)
        .cloned()
        .collect();
    // Random reads first, while the cache is cold, in random order to
    // exercise checkpoint restarts and the decoder pool (including reads
    // that go backwards). Then every file in full.
    let mut rng = Rng::new(seed);
    let big: Vec<&Entry> = files.iter().filter(|e| e.size > 0).collect();
    for _ in 0..1500 {
        let e = big[rng.below(big.len() as u64) as usize];
        let off = rng.below(e.size);
        let len = 1 + rng.below(300_000) as usize;
        let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
        let na = plain.read_at(e, off, &mut a).unwrap();
        let nb = gz.read_at(e, off, &mut b).unwrap();
        assert_eq!(na, nb);
        assert!(a[..na] == b[..nb], "{} @ {off}+{len}", e.display_path());
    }
    for e in &files {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        plain.copy_to(e, &mut a).unwrap();
        gz.copy_to(e, &mut b).unwrap();
        assert!(a == b, "contents of {} differ", e.display_path());
    }
}

#[test]
fn gzip_variants_read_like_the_plain_tar() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let tar = sample_tar();
    let tar_path = dir.path().join("a.tar");
    fs::write(&tar_path, &tar).unwrap();
    let plain = open(&tar_path, &idx, 64 << 10);

    let mut padded = gzip(&tar, 6, None).unwrap();
    padded.extend([0u8; 1000]); // zero padding after the last member is ignored
    let variants: Vec<(&str, Vec<u8>)> = vec![
        ("level1", gzip(&tar, 1, None).unwrap()),
        ("level2", gzip(&tar, 2, None).unwrap()),
        ("level9", gzip(&tar, 9, None).unwrap()),
        // Level 0 writes stored blocks, which end on byte boundaries.
        ("stored", gzip(&tar, 0, None).unwrap()),
        // Many members, as pigz, `cat a.gz b.gz` or BGZF produce.
        ("bgzf-like", gzip(&tar, 6, Some(65_280)).unwrap()),
        ("odd-members", gzip(&tar, 6, Some(1_234_567)).unwrap()),
        ("padded", padded),
    ];
    for (i, (name, data)) in variants.iter().enumerate() {
        let path = dir.path().join(format!("{name}.tar.gz"));
        fs::write(&path, data).unwrap();
        let a = open(&path, &idx, 64 << 10);
        let points: u64 = a
            .index()
            .meta("checkpoint_count")
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        // zlib-rs level 1 (like zlib-ng) writes one huge deflate block, so
        // it has no boundaries to resume from: reads still work, but
        // decode from the start.
        if *name != "level1" {
            assert!(points > 50, "{name}: only {points} checkpoints");
        }
        assert_same(&plain, &a, i as u64);
        // Reopening loads the checkpoints from the index.
        drop(a);
        let a = open(&path, &idx, 64 << 10);
        assert_eq!(
            a.index_status(),
            tarloupe_core::archive::IndexStatus::Loaded
        );
        assert_same(&plain, &a, 100 + i as u64);
    }
}

#[test]
fn default_spacing_and_single_checkpoint() {
    // With the default 8 MiB spacing a small archive has one checkpoint
    // (at the start), so every read decodes from the beginning.
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let mut tar = Vec::new();
    write_edge(&mut tar, Format::Pax).unwrap();
    let tar_path = dir.path().join("e.tar");
    fs::write(&tar_path, &tar).unwrap();
    let gz_path = dir.path().join("e.tar.gz");
    fs::write(&gz_path, gzip(&tar, 6, None).unwrap()).unwrap();
    let plain = Archive::open(
        &tar_path,
        &OpenOptions {
            index_dir: Some(idx.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let gz = Archive::open(
        &gz_path,
        &OpenOptions {
            index_dir: Some(idx),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        gz.index().meta("checkpoint_count").unwrap().as_deref(),
        Some("1")
    );
    assert_same(&plain, &gz, 9);
}

#[test]
fn damaged_gzip_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Gnu, 2 << 20, 3).unwrap();
    let gz = gzip(&tar, 6, None).unwrap();
    let opts = OpenOptions {
        index_dir: Some(idx),
        ..Default::default()
    };

    let truncated = dir.path().join("t.tar.gz");
    fs::write(&truncated, &gz[..gz.len() / 2]).unwrap();
    let err = Archive::open(&truncated, &opts)
        .err()
        .expect("truncated gzip must fail");
    assert!(err.to_string().contains("gzip"), "{err}");

    let mut bad = gz.clone();
    let mid = bad.len() / 3;
    for b in &mut bad[mid..mid + 64] {
        *b ^= 0x5a;
    }
    let corrupt = dir.path().join("c.tar.gz");
    fs::write(&corrupt, &bad).unwrap();
    assert!(Archive::open(&corrupt, &opts).is_err());
}

#[test]
fn damage_after_the_tar_end_is_an_error() {
    // A small tar padded with zeros: the tar terminator comes long before
    // the end of the gzip data, so only decoding to the end finds damage.
    let dir = tempfile::tempdir().unwrap();
    let mut tar = Vec::new();
    write_edge(&mut tar, Format::Gnu).unwrap();
    tar.extend(vec![0u8; 128 << 10]);
    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    let good = gzip(&tar, 0, None).unwrap();
    let mut no_trailer = good.clone();
    no_trailer.truncate(good.len() - 8);
    let mut bad_crc = good.clone();
    let n = bad_crc.len();
    bad_crc[n - 8] ^= 1;
    // Two members; the second one's deflate data is damaged.
    let half = tar.len() / 2;
    let mut later = gzip(&tar[..half], 6, None).unwrap();
    let second_start = later.len();
    later.extend(gzip(&tar[half..], 6, None).unwrap());
    let k = second_start + 20;
    for b in &mut later[k..k + 16] {
        *b ^= 0xff;
    }
    fs::write(dir.path().join("good.tar.gz"), &good).unwrap();
    assert!(Archive::open(dir.path().join("good.tar.gz"), &opts).is_ok());
    for (name, data) in [
        ("no-trailer", no_trailer),
        ("bad-crc", bad_crc),
        ("later", later),
    ] {
        let path = dir.path().join(format!("{name}.tar.gz"));
        fs::write(&path, data).unwrap();
        assert!(Archive::open(&path, &opts).is_err(), "{name} must fail");
    }
}

#[test]
fn extract_writes_the_same_tree_as_cat() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Gnu, 3 << 20, 5).unwrap();
    let gz_path = dir.path().join("b.tar.gz");
    fs::write(&gz_path, gzip(&tar, 6, None).unwrap()).unwrap();
    let out = dir.path().join("out");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_tarloupe"))
        .env("TARLOUPE_INDEX_DIR", &idx)
        .arg("extract")
        .arg(&gz_path)
        .arg("-C")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let a = open(&gz_path, &idx, 8 << 20);
    let mut n = 0;
    for e in members(&a) {
        if e.kind != EntryKind::File || e.superseded {
            continue;
        }
        let mut want = Vec::new();
        a.copy_to(&e, &mut want).unwrap();
        let got = fs::read(out.join(String::from_utf8(e.path.clone()).unwrap())).unwrap();
        assert!(got == want, "{}", e.display_path());
        n += 1;
    }
    assert!(n > 10);
}

#[test]
fn extract_stays_inside_the_destination() {
    use tarloupe_corpus::{Kind, Member, TarWriter};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evil.tar");
    let mut w = TarWriter::new(File::create(&path).unwrap(), Format::Gnu);
    for name in ["../escape.txt", "ok/../../escape2.txt"] {
        let m = Member::new(name, Kind::File, 3, 0);
        w.append(&m, &mut &b"bad"[..]).unwrap();
    }
    w.append(
        &Member::new("ok/fine.txt", Kind::File, 2, 0),
        &mut &b"hi"[..],
    )
    .unwrap();
    w.finish().unwrap();
    let out = dir.path().join("out");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_tarloupe"))
        .env("TARLOUPE_INDEX_DIR", dir.path().join("idx"))
        .args([
            "extract".as_ref(),
            path.as_os_str(),
            "-C".as_ref(),
            out.as_os_str(),
        ])
        .output()
        .unwrap();
    assert!(!st.status.success(), "unsafe members must be reported");
    assert!(!dir.path().join("escape.txt").exists());
    assert!(!dir.path().join("escape2.txt").exists());
    assert_eq!(fs::read(out.join("ok/fine.txt")).unwrap(), b"hi");
}

#[test]
fn indexing_reports_progress_up_to_the_file_size() {
    let dir = tempfile::tempdir().unwrap();
    let tar = sample_tar();
    let plain = dir.path().join("p.tar");
    let gz = dir.path().join("p.tar.gz");
    fs::write(&plain, &tar).unwrap();
    fs::write(&gz, gzip(&tar, 6, None).unwrap()).unwrap();
    for path in [&plain, &gz] {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let opts = OpenOptions {
            index_dir: Some(dir.path().join("idx")),
            progress: Some(std::sync::Arc::new(move |done, total| {
                s.lock().unwrap().push((done, total))
            })),
            ..Default::default()
        };
        Archive::open(path, &opts).unwrap();
        let size = fs::metadata(path).unwrap().len();
        let v = std::mem::take(&mut *seen.lock().unwrap());
        assert!(v.len() >= 3, "{}: {v:?}", path.display());
        assert_eq!(v.first(), Some(&(0, size)));
        assert_eq!(v.last(), Some(&(size, size)));
        assert!(v.iter().all(|&(d, t)| t == size && d <= size));
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0), "went back: {v:?}");
        // Loading the finished index reports nothing.
        Archive::open(path, &opts).unwrap();
        assert!(seen.lock().unwrap().is_empty());
    }
}

#[test]
fn cancelling_stops_indexing_and_leaves_no_index() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let tar = sample_tar();
    let plain = dir.path().join("c.tar");
    let gz = dir.path().join("c.tar.gz");
    fs::write(&plain, &tar).unwrap();
    fs::write(&gz, gzip(&tar, 6, None).unwrap()).unwrap();
    for path in [&plain, &gz] {
        let idx = dir
            .path()
            .join(format!("idx-{}", path.display().to_string().len()));
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let opts = OpenOptions {
            index_dir: Some(idx.clone()),
            // Cancel as soon as indexing is under way.
            progress: Some(Arc::new(move |done, _| {
                if done > 0 {
                    c.store(true, Ordering::Relaxed)
                }
            })),
            cancel: Some(cancel),
            ..Default::default()
        };
        match Archive::open(path, &opts) {
            Err(tarloupe_core::Error::Cancelled) => {}
            Err(e) => panic!("{}: {e}", path.display()),
            Ok(_) => panic!("{}: not cancelled", path.display()),
        }
        let left = fs::read_dir(&idx).map_or(0, |d| d.count());
        assert_eq!(left, 0, "{}: an index was left behind", path.display());
        open(path, &idx, 1 << 20);
    }
}
