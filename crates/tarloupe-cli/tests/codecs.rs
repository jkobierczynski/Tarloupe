//! zstd, xz and bzip2 archives must read exactly like the plain tar they
//! compress, whether or not they have restart points, and `convert` must
//! turn any of them into a fast one.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use tarloupe_core::convert::{convert, ConvertOptions, Target};
use tarloupe_core::{Archive, Codec, Entry, EntryKind, OpenOptions};
use tarloupe_corpus::{write_bulk, write_edge, Format, Rng};

const SPACING: u64 = 64 << 10;

fn open(path: &Path, idx: &Path) -> Archive {
    Archive::open(
        path,
        &OpenOptions {
            index_dir: Some(idx.to_path_buf()),
            checkpoint_spacing: Some(SPACING),
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

fn checkpoints(a: &Archive) -> u64 {
    a.index()
        .meta("checkpoint_count")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap()
}

/// A tar with every edge case followed by a few MiB of bulk files.
fn sample_tar() -> Vec<u8> {
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Pax, 6 << 20, 11).unwrap();
    let mut edge = Vec::new();
    write_edge(&mut edge, Format::Gnu).unwrap();
    while tar.len() >= 512 && tar[tar.len() - 512..].iter().all(|&b| b == 0) {
        tar.truncate(tar.len() - 512);
    }
    tar.extend(edge);
    tar
}

/// Compares every member, its contents and many random reads.
fn assert_same(plain: &Archive, other: &Archive, seed: u64) {
    let (pm, om) = (members(plain), members(other));
    assert_eq!(pm.len(), om.len());
    for (p, o) in pm.iter().zip(&om) {
        assert_eq!(p.path, o.path);
        assert_eq!(p.data_offset, o.data_offset);
        assert_eq!(p.size, o.size);
    }
    let files: Vec<Entry> = pm
        .iter()
        .filter(|e| e.kind == EntryKind::File && !e.superseded)
        .cloned()
        .collect();
    let mut rng = Rng::new(seed);
    let big: Vec<&Entry> = files.iter().filter(|e| e.size > 0).collect();
    for _ in 0..400 {
        let e = big[rng.below(big.len() as u64) as usize];
        let off = rng.below(e.size);
        let len = 1 + rng.below(300_000) as usize;
        let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
        let na = plain.read_at(e, off, &mut a).unwrap();
        let nb = other.read_at(e, off, &mut b).unwrap();
        assert_eq!(na, nb);
        assert!(a[..na] == b[..nb], "{} @ {off}+{len}", e.display_path());
    }
    for e in &files {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        plain.copy_to(e, &mut a).unwrap();
        other.copy_to(e, &mut b).unwrap();
        assert!(a == b, "contents of {} differ", e.display_path());
    }
}

/// zstd frames of `frame` bytes each, without a seek table (as `pzstd`
/// or multi-threaded writers produce).
fn zstd_frames(data: &[u8], frame: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for part in data.chunks(frame) {
        let mut buf = Vec::with_capacity(zstd_safe::compress_bound(part.len()));
        zstd_safe::compress(&mut buf, part, 3).unwrap();
        out.extend(buf);
    }
    out
}

fn bzip2_streams(data: &[u8], stream: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for part in data.chunks(stream) {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(6));
        e.write_all(part).unwrap();
        out.extend(e.finish().unwrap());
    }
    out
}

fn converted(src: &Path, target: Target, block: u64, threads: usize) -> Vec<u8> {
    let out = src.with_extension(format!("{target:?}-{block}-{threads}"));
    convert(
        src,
        &out,
        &ConvertOptions {
            target,
            level: Some(1),
            block_size: block,
            threads,
            ..Default::default()
        },
    )
    .unwrap();
    fs::read(out).unwrap()
}

/// Runs an external compressor, if it is installed.
fn external(cmd: &str, args: &[&str], input: &Path) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).arg(input).output().ok()?;
    assert!(out.status.success(), "{cmd} failed");
    Some(out.stdout)
}

#[test]
fn every_codec_reads_like_the_plain_tar() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let tar = sample_tar();
    let tar_path = dir.path().join("a.tar");
    fs::write(&tar_path, &tar).unwrap();
    let plain = open(&tar_path, &idx);

    // (name, data, codec, has restart points)
    let mut variants: Vec<(&str, Vec<u8>, Codec, bool)> = vec![
        (
            "zstd-one-frame",
            zstd_frames(&tar, usize::MAX),
            Codec::Zstd,
            false,
        ),
        ("zstd-frames", zstd_frames(&tar, 100_000), Codec::Zstd, true),
        (
            "zstd-seekable",
            converted(&tar_path, Target::Zstd, 128 << 10, 3),
            Codec::Zstd,
            true,
        ),
        (
            "zstd-seekable-one-frame",
            converted(&tar_path, Target::Zstd, 32 << 20, 1),
            Codec::Zstd,
            false,
        ),
        (
            "xz-blocks",
            converted(&tar_path, Target::Xz, 128 << 10, 4),
            Codec::Xz,
            true,
        ),
        (
            "xz-one-block",
            converted(&tar_path, Target::Xz, 32 << 20, 1),
            Codec::Xz,
            false,
        ),
        (
            // Every block of a bzip2 stream is a restart point.
            "bzip2-one-stream",
            bzip2_streams(&tar, usize::MAX),
            Codec::Bzip2,
            true,
        ),
        (
            "bzip2-streams",
            bzip2_streams(&tar, 300_000),
            Codec::Bzip2,
            true,
        ),
    ];
    // Concatenated xz files are one stream of data too.
    let half = tar.len() / 2;
    let (a, b) = (dir.path().join("h1"), dir.path().join("h2"));
    fs::write(&a, &tar[..half]).unwrap();
    fs::write(&b, &tar[half..]).unwrap();
    if let (Some(x), Some(y)) = (
        external("xz", &["-c", "-1", "--block-size=200KiB"], &a),
        external("xz", &["-c", "-1", "--check=sha256"], &b),
    ) {
        let mut cat = x;
        cat.extend([0u8; 8]); // stream padding
        cat.extend(y);
        variants.push(("xz-cli-two-streams", cat, Codec::Xz, true));
    }
    if let Some(x) = external(
        "xz",
        &["-c", "-0", "--check=none", "-T2", "--block-size=150KiB"],
        &tar_path,
    ) {
        variants.push(("xz-cli-blocks", x, Codec::Xz, true));
    }
    if let Some(x) = external("bzip2", &["-c", "-9"], &tar_path) {
        variants.push(("bzip2-cli", x, Codec::Bzip2, true));
    }

    for (i, (name, data, codec, seekable)) in variants.iter().enumerate() {
        let path = dir.path().join(format!("{name}.tar.x"));
        fs::write(&path, data).unwrap();
        let a = open(&path, &idx);
        assert_eq!(a.codec(), *codec, "{name}");
        let points = checkpoints(&a);
        if *seekable {
            // bzip2 blocks hold up to 900 kB, so a few MiB has only a few.
            let least = if *codec == Codec::Bzip2 { 5 } else { 10 };
            assert!(points > least, "{name}: only {points} checkpoints");
        } else {
            assert_eq!(points, 0, "{name}: {points} checkpoints");
        }
        assert_eq!(a.stream_size(), Some(tar.len() as u64), "{name}");
        assert_same(&plain, &a, i as u64);
        // Reopening loads the checkpoints from the index.
        drop(a);
        let a = open(&path, &idx);
        assert_eq!(
            a.index_status(),
            tarloupe_core::archive::IndexStatus::Loaded
        );
        assert_same(&plain, &a, 100 + i as u64);
    }
}

#[test]
fn slow_archives_say_why() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let mut tar = Vec::new();
    // Highly compressible, so the test stays small and fast.
    write_bulk(&mut tar, Format::Gnu, 70 << 20, 1).unwrap();
    let path = dir.path().join("big.tar");
    fs::write(&path, &tar).unwrap();
    let zst = dir.path().join("big.tar.zst");
    fs::write(&zst, zstd_frames(&tar, usize::MAX)).unwrap();
    let slow = open(&zst, &idx);
    assert!(slow.slow_access().unwrap().contains("single frame"));
    assert!(slow.max_decode_distance() >= tar.len() as u64);
    let fast = dir.path().join("fast.tar.zst");
    fs::write(&fast, converted(&path, Target::Zstd, 8 << 20, 0)).unwrap();
    let fast = open(&fast, &idx);
    assert_eq!(fast.slow_access(), None);
    assert!(fast.max_decode_distance() <= 16 << 20);
    assert_eq!(open(&path, &idx).slow_access(), None);
}

#[test]
fn damaged_archives_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut tar = Vec::new();
    write_bulk(&mut tar, Format::Gnu, 2 << 20, 3).unwrap();
    let tar_path = dir.path().join("d.tar");
    fs::write(&tar_path, &tar).unwrap();
    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    for (name, data) in [
        ("zst", converted(&tar_path, Target::Zstd, 256 << 10, 2)),
        ("xz", converted(&tar_path, Target::Xz, 256 << 10, 2)),
        ("bz2", bzip2_streams(&tar, usize::MAX)),
    ] {
        let truncated = dir.path().join(format!("t.tar.{name}"));
        fs::write(&truncated, &data[..data.len() / 2]).unwrap();
        assert!(
            Archive::open(&truncated, &opts).is_err(),
            "truncated {name} must fail"
        );
        let mut bad = data.clone();
        let mid = bad.len() / 3;
        for b in &mut bad[mid..mid + 64] {
            *b ^= 0x5a;
        }
        let corrupt = dir.path().join(format!("c.tar.{name}"));
        fs::write(&corrupt, &bad).unwrap();
        assert!(
            Archive::open(&corrupt, &opts).is_err(),
            "corrupt {name} must fail"
        );
    }
}

/// CRC-32 (IEEE), as xz uses it.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

#[test]
fn damaged_trailers_are_errors() {
    // The tar ends long before the compressed data, so only checking the
    // trailers finds this damage.
    let dir = tempfile::tempdir().unwrap();
    let mut tar = Vec::new();
    write_edge(&mut tar, Format::Gnu).unwrap();
    let tar_path = dir.path().join("e.tar");
    fs::write(&tar_path, &tar).unwrap();
    let opts = OpenOptions {
        index_dir: Some(dir.path().join("idx")),
        ..Default::default()
    };
    let xz = converted(&tar_path, Target::Xz, 1 << 20, 1);
    let n = xz.len();
    let mut cases = Vec::new();
    for (name, at) in [
        ("footer-crc", n - 12),
        ("backward-size", n - 7),
        ("index-crc", n - 13),
    ] {
        let mut bad = xz.clone();
        bad[at] ^= 1;
        if name == "backward-size" {
            // Keep the footer CRC valid, so only the size check catches it.
            let crc = crc32(&bad[n - 8..n - 2]);
            bad[n - 12..n - 8].copy_from_slice(&crc.to_le_bytes());
        }
        cases.push((name, bad));
    }
    // Another bzip2 stream starts, but is cut off after its magic.
    let mut bz = bzip2_streams(&tar, usize::MAX);
    bz.extend(b"BZh");
    cases.push(("bzip2-cut", bz));
    for (name, data) in cases {
        let path = dir.path().join(format!("{name}.tar.x"));
        fs::write(&path, data).unwrap();
        assert!(Archive::open(&path, &opts).is_err(), "{name} must fail");
    }
    // Unrelated bytes after the last bzip2 stream are ignored, like bzip2 does.
    let mut bz = bzip2_streams(&tar, usize::MAX);
    bz.extend(b"garbage!");
    let path = dir.path().join("bz-garbage.tar.bz2");
    fs::write(&path, bz).unwrap();
    Archive::open(&path, &opts).unwrap();
}

#[test]
fn convert_command_makes_a_fast_copy() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let tar = sample_tar();
    let src = dir.path().join("slow.tar.xz");
    fs::write(&src, converted_bytes_xz_single(&tar, dir.path())).unwrap();
    let run = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_tarloupe"))
            .env("TARLOUPE_INDEX_DIR", &idx)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&[
        "convert".as_ref(),
        src.as_os_str(),
        "--block-size".as_ref(),
        "256K".as_ref(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dst = dir.path().join("slow.tar.zst");
    assert!(dst.exists());
    // Again: the output exists, so it is refused without --force.
    let again = run(&["convert".as_ref(), src.as_os_str()]);
    assert!(!again.status.success());
    let forced = run(&[
        "convert".as_ref(),
        src.as_os_str(),
        "--force".as_ref(),
        "--block-size".as_ref(),
        "128K".as_ref(),
    ]);
    assert!(forced.status.success());
    let xz = dir.path().join("fast.tar.xz");
    let out = run(&[
        "convert".as_ref(),
        dst.as_os_str(),
        "-o".as_ref(),
        xz.as_os_str(),
        "--to".as_ref(),
        "xz".as_ref(),
        "--block-size".as_ref(),
        "256K".as_ref(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let tar_path = dir.path().join("p.tar");
    fs::write(&tar_path, &tar).unwrap();
    let plain = open(&tar_path, &idx);
    for p in [&dst, &xz] {
        let a = open(p, &idx);
        assert!(checkpoints(&a) > 5, "{}: {}", p.display(), checkpoints(&a));
        assert_same(&plain, &a, 7);
    }
}

fn converted_bytes_xz_single(tar: &[u8], dir: &Path) -> Vec<u8> {
    let p = dir.join("single-src.tar");
    fs::write(&p, tar).unwrap();
    converted(&p, Target::Xz, 32 << 20, 1)
}

#[test]
fn default_output_names() {
    use tarloupe_core::convert::default_output;
    let cases = [
        ("a/big.tar.gz", Target::Zstd, "a/big.tar.zst"),
        ("big.tgz", Target::Zstd, "big.tar.zst"),
        ("big.TAR.BZ2", Target::Xz, "big.tar.xz"),
        ("big.tar", Target::Zstd, "big.tar.zst"),
        ("big.tar.zst", Target::Zstd, "big.seekable.tar.zst"),
        ("weird", Target::Xz, "weird.tar.xz"),
    ];
    for (i, t, o) in cases {
        assert_eq!(default_output(Path::new(i), t), Path::new(o), "{i}");
    }
}

#[test]
fn misaligned_checkpoints_rebuild_the_index() {
    // zstd can only start at a byte, so a checkpoint mid-byte means the
    // index is damaged: it is rebuilt, or refused when it must be used.
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let path = dir.path().join("f.tar.zst");
    fs::write(&path, zstd_frames(&sample_tar(), 256 << 10)).unwrap();
    let a = open(&path, &idx);
    assert!(checkpoints(&a) > 0);
    let index = a.index_path().to_path_buf();
    drop(a);
    let db = rusqlite::Connection::open(&index).unwrap();
    db.execute(
        "UPDATE checkpoints SET compressed_bit_offset = compressed_bit_offset + 3 \
         WHERE uncompressed_offset = (SELECT MAX(uncompressed_offset) FROM checkpoints)",
        [],
    )
    .unwrap();
    drop(db);
    let strict = OpenOptions {
        index_dir: Some(idx.clone()),
        require_index: true,
        ..Default::default()
    };
    assert!(Archive::open(&path, &strict).is_err());
    let a = open(&path, &idx);
    assert_eq!(
        a.index_status(),
        tarloupe_core::archive::IndexStatus::Rebuilt
    );
}

#[test]
fn bzip2_indexes_without_blocks_are_rebuilt() {
    // Indexes of 0.1 only recorded where bzip2 streams start; they are
    // rebuilt so that single-stream files get their block restart points.
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let tar = sample_tar();
    let path = dir.path().join("f.tar.bz2");
    fs::write(&path, bzip2_streams(&tar, usize::MAX)).unwrap();
    let a = open(&path, &idx);
    assert!(checkpoints(&a) > 10);
    let index = a.index_path().to_path_buf();
    drop(a);
    let db = rusqlite::Connection::open(&index).unwrap();
    db.execute("DELETE FROM meta WHERE key = 'bzip2_seek'", [])
        .unwrap();
    drop(db);
    let a = open(&path, &idx);
    assert_eq!(
        a.index_status(),
        tarloupe_core::archive::IndexStatus::Rebuilt
    );
}

#[test]
fn bzip2_indexes_the_same_on_any_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let tar = sample_tar();
    let path = dir.path().join("f.tar.bz2");
    fs::write(&path, bzip2_streams(&tar, 700_000)).unwrap();
    let plain_path = dir.path().join("f.tar");
    fs::write(&plain_path, &tar).unwrap();
    let plain = open(&plain_path, &dir.path().join("p"));
    let mut seen = None;
    for threads in [1, 2, 7] {
        let a = Archive::open(
            &path,
            &OpenOptions {
                index_dir: Some(dir.path().join(format!("t{threads}"))),
                threads: Some(threads),
                checkpoint_spacing: Some(SPACING),
                ..Default::default()
            },
        )
        .unwrap();
        let points: Vec<_> = a
            .index()
            .checkpoints()
            .unwrap()
            .iter()
            .map(|p| (p.uncompressed_offset, p.bit_offset))
            .collect();
        assert!(points.len() > 5);
        assert!(points.iter().any(|p| p.1 % 8 != 0), "block starts mid-byte");
        match &seen {
            None => seen = Some(points),
            Some(s) => assert_eq!(s, &points, "threads {threads}"),
        }
        assert_same(&plain, &a, threads as u64);
    }
}
