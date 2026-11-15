//! Deterministic synthetic tar corpora for tests and benchmarks.
//!
//! * [`edge_members`] — a small archive full of awkward cases: long names,
//!   PAX and GNU extensions, Windows-hostile names, case collisions, links,
//!   duplicates, implicit directories, odd modes and timestamps.
//! * [`write_bulk`] — many files totalling a target size (`1G`, `100G`, ...)
//!   with a realistic size spread and partly compressible content, streamed
//!   without touching the disk except for the output.

pub mod writer;

use std::io::{self, Read, Write};

pub use writer::{Format, Kind, Member, TarWriter};

/// xorshift64* — small, fast and deterministic across platforms.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Deterministic content: runs of pseudo-text (compresses well) mixed with
/// runs of random bytes (does not), so codecs see realistic data.
pub struct Content {
    rng: Rng,
    left: u64,
    run_left: usize,
    text: bool,
}

const WORDS: &[&str] = &[
    "the", "archive", "index", "block", "stream", "offset", "window", "header", "entry", "data",
    "seek", "fast", "tar", "file", "path", "name", "size", "mode", "time", "user", "group", "link",
    "dir",
];

impl Content {
    pub fn new(seed: u64, len: u64) -> Content {
        Content {
            rng: Rng::new(seed),
            left: len,
            run_left: 0,
            text: true,
        }
    }
}

impl Read for Content {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.left) as usize;
        let mut i = 0;
        while i < n {
            if self.run_left == 0 {
                self.text = self.rng.below(10) < 7;
                self.run_left = 256 + self.rng.below(16 * 1024) as usize;
            }
            let take = self.run_left.min(n - i);
            if self.text {
                let mut j = 0;
                while j < take {
                    let w = WORDS[self.rng.below(WORDS.len() as u64) as usize].as_bytes();
                    for &c in w.iter().chain(b" ".iter()) {
                        if j == take {
                            break;
                        }
                        buf[i + j] = c;
                        j += 1;
                    }
                }
            } else {
                for chunk in buf[i..i + take].chunks_mut(8) {
                    let r = self.rng.next_u64().to_le_bytes();
                    chunk.copy_from_slice(&r[..chunk.len()]);
                }
            }
            i += take;
            self.run_left -= take;
        }
        self.left -= n as u64;
        Ok(n)
    }
}

pub const T0: i64 = 1_700_000_000; // 2023-11-14

fn file(path: &str, size: u64) -> Member {
    Member::new(path, Kind::File, size, T0)
}

/// The members of the edge-case archive, in archive order.
pub fn edge_members() -> Vec<Member> {
    let mut v: Vec<Member> = Vec::new();
    let dir = |p: &str| Member::new(p, Kind::Dir, 0, T0);
    v.push(dir("corpus"));
    v.push(dir("corpus/files"));
    for (name, size) in [
        ("empty.txt", 0),
        ("one-byte.txt", 1),
        ("block-minus-one.bin", 511),
        ("one-block.bin", 512),
        ("block-plus-one.bin", 513),
        ("one-mib-plus-one.bin", 1024 * 1024 + 1),
    ] {
        v.push(file(&format!("corpus/files/{name}"), size));
    }

    // Long paths: > 100 bytes total, a > 100 byte single component, > 255.
    let long_dir = format!("corpus/long/{}", "directory-name-segment/".repeat(4));
    v.push(file(&format!("{long_dir}file-with-a-long-path.txt"), 10));
    v.push(file(&format!("corpus/long/{}.txt", "c".repeat(120)), 11));
    v.push(file(
        &format!(
            "corpus/long/{}/end.txt",
            "deep/".repeat(60).trim_end_matches('/')
        ),
        12,
    ));

    // Non-ASCII names.
    v.push(dir("corpus/unicodé"));
    v.push(file("corpus/unicodé/日本語のファイル.txt", 13));
    v.push(file("corpus/unicodé/emoji 🎉 party.txt", 14));
    v.push(file("corpus/unicodé/Ελληνικά.txt", 15));

    // Names Windows cannot represent directly.
    v.push(dir("corpus/windows"));
    for name in [
        "colon:name.txt",
        "question?.txt",
        "star*.txt",
        "quote\".txt",
        "angle<brackets>.txt",
        "pipe|.txt",
        "trailing-dot.",
        "trailing-space ",
        "CON",
        "nul.txt",
        "com1.log",
        "back\\slash.txt",
        " leading-space.txt",
        "-leading-dash.txt",
    ] {
        v.push(file(&format!("corpus/windows/{name}"), 16));
    }

    // Case-insensitive collisions.
    v.push(file("corpus/case/Makefile", 17));
    v.push(file("corpus/case/makefile", 18));
    v.push(file("corpus/case/MAKEFILE", 19));
    v.push(dir("corpus/case/Dir"));
    v.push(file("corpus/case/dir", 20));

    // Links.
    v.push(dir("corpus/links"));
    v.push(Member::new(
        "corpus/links/to-file",
        Kind::Symlink(b"../files/one-block.bin".to_vec()),
        0,
        T0,
    ));
    v.push(Member::new(
        "corpus/links/to-dir",
        Kind::Symlink(b"../files".to_vec()),
        0,
        T0,
    ));
    v.push(Member::new(
        "corpus/links/dangling",
        Kind::Symlink(b"does/not/exist".to_vec()),
        0,
        T0,
    ));
    v.push(Member::new(
        "corpus/links/absolute",
        Kind::Symlink(b"/etc/hostname".to_vec()),
        0,
        T0,
    ));
    let long_target = format!("../{}target", "x/".repeat(60));
    v.push(Member::new(
        "corpus/links/long-target",
        Kind::Symlink(long_target.into_bytes()),
        0,
        T0,
    ));
    v.push(Member::new(
        "corpus/links/hard",
        Kind::HardLink(b"corpus/files/one-mib-plus-one.bin".to_vec()),
        0,
        T0,
    ));
    v.push(Member::new(
        "corpus/links/chain",
        Kind::Symlink(b"to-file".to_vec()),
        0,
        T0,
    ));

    // The same path twice: the second one wins.
    v.push(file("corpus/dup/file.txt", 21));
    v.push(file("corpus/dup/file.txt", 22));

    // Parents that are never listed.
    v.push(file("corpus/implicit/a/b/c/file.txt", 23));
    v.push(file("./corpus/dot-slash/file.txt", 24));

    // Deep nesting.
    let deep: String = (0..40).map(|i| format!("d{i}/")).collect();
    v.push(file(&format!("corpus/{deep}bottom.txt"), 25));

    v.push(dir("corpus/empty-dir"));

    // Modes, ids and timestamps.
    let mut m = file("corpus/meta/setuid", 26);
    m.mode = 0o4755;
    v.push(m);
    let mut m = file("corpus/meta/setgid-noexec", 27);
    m.mode = 0o2644;
    v.push(m);
    let mut m = dir("corpus/meta/sticky");
    m.mode = 0o1777;
    v.push(m);
    let mut m = file("corpus/meta/no-perms", 28);
    m.mode = 0;
    v.push(m);
    let mut m = file("corpus/meta/numeric-owner", 29);
    m.uname.clear();
    m.gname.clear();
    m.uid = 4242;
    m.gid = 4343;
    v.push(m);
    let mut m = file("corpus/meta/big-ids", 30);
    m.uid = 3_000_000;
    m.gid = 3_000_001;
    m.uname.clear();
    m.gname.clear();
    v.push(m);
    let mut m = file("corpus/meta/epoch", 31);
    m.mtime = 0;
    v.push(m);
    let mut m = file("corpus/meta/year-2100", 32);
    m.mtime = 4_102_444_800;
    v.push(m);
    let mut m = file("corpus/meta/before-1970", 33);
    m.mtime = -86_400 * 365;
    v.push(m);
    let mut m = file("corpus/meta/long-user", 34);
    m.uname = "a-rather-long-user-name".into();
    m.gname = "and-a-long-group-name".into();
    v.push(m);

    // Special files.
    v.push(Member::new("corpus/special/fifo", Kind::Fifo, 0, T0));
    v.push(Member::new(
        "corpus/special/null",
        Kind::CharDev(1, 3),
        0,
        T0,
    ));

    v
}

/// Writes the edge-case archive. Returns the members skipped because the
/// format cannot represent them (only plain ustar skips any).
pub fn write_edge<W: Write>(w: W, format: Format) -> io::Result<Vec<String>> {
    let mut w = TarWriter::new(w, format);
    for (i, m) in edge_members().iter().enumerate() {
        let mut data = Content::new(i as u64 + 1, m.size);
        w.append(m, &mut data)?;
    }
    let skipped = std::mem::take(&mut w.skipped);
    w.finish()?;
    Ok(skipped)
}

/// Writes a bulk archive of roughly `size` bytes. Returns the file count.
pub fn write_bulk<W: Write>(w: W, format: Format, size: u64, seed: u64) -> io::Result<u64> {
    let mut w = TarWriter::new(w, format);
    let mut rng = Rng::new(seed);
    let mut n: u64 = 0;
    let mut last_dir = String::new();
    while w.written < size {
        // About 200 files per leaf directory, 100 leaves per branch.
        let dir = format!("bulk/b{:03}/l{:03}", n / 20_000, (n / 200) % 100);
        if dir != last_dir {
            if n % 20_000 == 0 {
                w.append(
                    &Member::new(format!("bulk/b{:03}", n / 20_000), Kind::Dir, 0, T0),
                    &mut io::empty(),
                )?;
            }
            w.append(
                &Member::new(dir.clone(), Kind::Dir, 0, T0),
                &mut io::empty(),
            )?;
            last_dir = dir.clone();
        }
        // 85% small (< 16 KiB), 14% medium (< 1 MiB), 1% large (< 16 MiB):
        // about 160 KiB on average, so 100 GiB is roughly 650k files.
        let class = rng.below(100);
        let max = if class < 85 {
            16 << 10
        } else if class < 99 {
            1 << 20
        } else {
            16 << 20
        };
        let fsize = rng.below(max).min(size.saturating_sub(w.written));
        let ext = ["txt", "log", "bin", "json", "dat"][rng.below(5) as usize];
        let m = Member::new(
            format!("{dir}/file{n:07}.{ext}"),
            Kind::File,
            fsize,
            T0 + n as i64,
        );
        w.append(&m, &mut Content::new(seed ^ n.wrapping_mul(0x9E37), fsize))?;
        n += 1;
    }
    w.finish()?;
    Ok(n)
}
