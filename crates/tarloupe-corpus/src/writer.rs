//! Minimal streaming tar writer that can emit GNU, PAX and plain ustar
//! headers. It is deliberately independent of the reader in
//! `tarloupe-core`, and its output is checked against GNU tar in tests.

use std::io::{self, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// GNU tar format (long names via `././@LongLink`, base-256 numbers).
    Gnu,
    /// POSIX.1-2001 pax (extended headers).
    Pax,
    /// POSIX.1-1988 ustar (no extensions; some members are skipped).
    Ustar,
}

#[derive(Debug, Clone)]
pub enum Kind {
    File,
    Dir,
    Symlink(Vec<u8>),
    HardLink(Vec<u8>),
    Fifo,
    CharDev(u64, u64),
}

#[derive(Debug, Clone)]
pub struct Member {
    pub path: Vec<u8>,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub uname: String,
    pub gname: String,
    pub mtime: i64,
    pub size: u64,
}

impl Member {
    pub fn new(path: impl Into<Vec<u8>>, kind: Kind, size: u64, mtime: i64) -> Member {
        let mode = match kind {
            Kind::Dir => 0o755,
            Kind::Symlink(_) => 0o777,
            _ => 0o644,
        };
        Member {
            path: path.into(),
            kind,
            mode,
            uid: 1000,
            gid: 1000,
            uname: "tarloupe".into(),
            gname: "users".into(),
            mtime,
            size,
        }
    }
}

pub struct TarWriter<W: Write> {
    w: W,
    format: Format,
    pub written: u64,
    pub skipped: Vec<String>,
}

const OCTAL_SIZE_MAX: u64 = 0o77777777777; // 11 octal digits
const OCTAL_ID_MAX: u64 = 0o7777777; // 7 octal digits

impl<W: Write> TarWriter<W> {
    pub fn new(w: W, format: Format) -> Self {
        TarWriter {
            w,
            format,
            written: 0,
            skipped: Vec::new(),
        }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.written += b.len() as u64;
        Ok(())
    }

    fn pad(&mut self, n: u64) -> io::Result<()> {
        let rem = (512 - n % 512) % 512;
        self.put(&vec![0u8; rem as usize])
    }

    /// Appends a member; `data` must yield exactly `m.size` bytes for files.
    /// Returns `false` if the member cannot be represented in this format.
    pub fn append(&mut self, m: &Member, data: &mut dyn Read) -> io::Result<bool> {
        let (typeflag, link, size) = match &m.kind {
            Kind::File => (b'0', &b""[..], m.size),
            Kind::Dir => (b'5', &b""[..], 0),
            Kind::Symlink(t) => (b'2', t.as_slice(), 0),
            Kind::HardLink(t) => (b'1', t.as_slice(), 0),
            Kind::Fifo => (b'6', &b""[..], 0),
            Kind::CharDev(..) => (b'3', &b""[..], 0),
        };
        let mut name: Vec<u8> = m.path.clone();
        if matches!(m.kind, Kind::Dir) && !name.ends_with(b"/") {
            name.push(b'/');
        }
        let (mut prefix, mut short): (Vec<u8>, Vec<u8>) = (Vec::new(), name.clone());

        match self.format {
            Format::Gnu => {
                if name.len() > 100 {
                    self.long_entry(b'L', &name)?;
                    short.truncate(100);
                }
                if link.len() > 100 {
                    self.long_entry(b'K', link)?;
                }
            }
            Format::Pax => {
                let mut recs: Vec<(&str, Vec<u8>)> = Vec::new();
                if name.len() > 100 || !name.is_ascii() {
                    recs.push(("path", name.clone()));
                    short = name
                        .iter()
                        .copied()
                        .filter(|c| c.is_ascii())
                        .take(100)
                        .collect();
                }
                if link.len() > 100 || !link.is_ascii() {
                    recs.push(("linkpath", link.to_vec()));
                }
                if size > OCTAL_SIZE_MAX {
                    recs.push(("size", size.to_string().into_bytes()));
                }
                if m.uid > OCTAL_ID_MAX {
                    recs.push(("uid", m.uid.to_string().into_bytes()));
                }
                if m.gid > OCTAL_ID_MAX {
                    recs.push(("gid", m.gid.to_string().into_bytes()));
                }
                if m.mtime < 0 || m.mtime as u64 > OCTAL_SIZE_MAX {
                    recs.push(("mtime", m.mtime.to_string().into_bytes()));
                }
                if !recs.is_empty() {
                    let mut body = Vec::new();
                    for (k, v) in recs {
                        body.extend(pax_record(k, &v));
                    }
                    let base: Vec<u8> = short
                        .rsplit(|&c| c == b'/')
                        .find(|c| !c.is_empty())
                        .unwrap_or(b"x")
                        .to_vec();
                    let mut hname = b"PaxHeaders/".to_vec();
                    hname.extend(base.iter().take(80));
                    let h = self.header(
                        &hname,
                        &[],
                        b'x',
                        body.len() as u64,
                        0o644,
                        0,
                        0,
                        "",
                        "",
                        0,
                        (0, 0),
                        &[],
                    )?;
                    self.put(&h)?;
                    self.put(&body)?;
                    self.pad(body.len() as u64)?;
                }
            }
            Format::Ustar => {
                let (dev_major, dev_minor) = match m.kind {
                    Kind::CharDev(a, b) => (a, b),
                    _ => (0, 0),
                };
                if link.len() > 100
                    || size > OCTAL_SIZE_MAX
                    || m.uid > OCTAL_ID_MAX
                    || m.gid > OCTAL_ID_MAX
                    || m.mtime < 0
                    || m.mtime as u64 > OCTAL_SIZE_MAX
                    || dev_major > OCTAL_ID_MAX
                    || dev_minor > OCTAL_ID_MAX
                    || m.mode as u64 > OCTAL_ID_MAX
                {
                    self.skipped
                        .push(String::from_utf8_lossy(&m.path).into_owned());
                    return Ok(false);
                }
                if name.len() > 100 {
                    // Split at a '/' so that prefix <= 155 and name <= 100.
                    let split = (0..name.len()).rev().find(|&i| {
                        name[i] == b'/'
                            && i <= 155
                            && name.len() - i - 1 <= 100
                            && i + 1 < name.len()
                    });
                    match split {
                        Some(i) => {
                            prefix = name[..i].to_vec();
                            short = name[i + 1..].to_vec();
                        }
                        None => {
                            self.skipped
                                .push(String::from_utf8_lossy(&m.path).into_owned());
                            return Ok(false);
                        }
                    }
                }
            }
        }

        let dev = if let Kind::CharDev(a, b) = m.kind {
            (a, b)
        } else {
            (0, 0)
        };
        let h = self.header(
            &short, link, typeflag, size, m.mode, m.uid, m.gid, &m.uname, &m.gname, m.mtime, dev,
            &prefix,
        )?;
        self.put(&h)?;
        if size > 0 {
            let n = io::copy(&mut data.take(size), &mut CountingWriter(self))?;
            if n != size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "member data shorter than its size",
                ));
            }
            self.pad(size)?;
        }
        Ok(true)
    }

    fn long_entry(&mut self, typeflag: u8, value: &[u8]) -> io::Result<()> {
        let mut body = value.to_vec();
        body.push(0);
        let h = self.header(
            b"././@LongLink",
            &[],
            typeflag,
            body.len() as u64,
            0o644,
            0,
            0,
            "root",
            "root",
            0,
            (0, 0),
            &[],
        )?;
        self.put(&h)?;
        self.put(&body)?;
        self.pad(body.len() as u64)
    }

    #[allow(clippy::too_many_arguments)]
    fn header(
        &self,
        name: &[u8],
        link: &[u8],
        typeflag: u8,
        size: u64,
        mode: u32,
        uid: u64,
        gid: u64,
        uname: &str,
        gname: &str,
        mtime: i64,
        dev: (u64, u64),
        prefix: &[u8],
    ) -> io::Result<[u8; 512]> {
        let mut h = [0u8; 512];
        copy(&mut h[0..100], name);
        num(&mut h[100..108], mode as i128, self.format);
        num(&mut h[108..116], uid as i128, self.format);
        num(&mut h[116..124], gid as i128, self.format);
        num(&mut h[124..136], size as i128, self.format);
        num(&mut h[136..148], mtime as i128, self.format);
        h[156] = typeflag;
        copy(&mut h[157..257], &link[..link.len().min(100)]);
        if self.format == Format::Gnu {
            h[257..265].copy_from_slice(b"ustar  \0");
        } else {
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
        }
        copy(&mut h[265..297], uname.as_bytes());
        copy(&mut h[297..329], gname.as_bytes());
        if typeflag == b'3' || typeflag == b'4' {
            num(&mut h[329..337], dev.0 as i128, self.format);
            num(&mut h[337..345], dev.1 as i128, self.format);
        }
        copy(&mut h[345..500], prefix);
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| b as u64).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        Ok(h)
    }

    /// Writes the end-of-archive marker and pads to a 10 KiB record like GNU tar.
    pub fn finish(mut self) -> io::Result<W> {
        self.put(&[0u8; 1024])?;
        let rem = (10240 - self.written % 10240) % 10240;
        self.put(&vec![0u8; rem as usize])?;
        self.w.flush()?;
        Ok(self.w)
    }
}

struct CountingWriter<'a, W: Write>(&'a mut TarWriter<W>);

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.w.write(buf)?;
        self.0.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.w.flush()
    }
}

fn copy(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
}

/// Writes an octal field, or base-256 when it does not fit (GNU and pax;
/// pax readers normally use the extended header instead, but GNU tar
/// accepts both).
fn num(dst: &mut [u8], v: i128, format: Format) {
    let digits = dst.len() - 1;
    if v >= 0 && (v as u128) < 8u128.pow(digits as u32) {
        let s = format!("{:0width$o}", v, width = digits);
        dst[..digits].copy_from_slice(s.as_bytes());
        dst[digits] = 0;
    } else if format != Format::Ustar {
        let bytes = v.to_be_bytes();
        let n = dst.len();
        dst.copy_from_slice(&bytes[16 - n..]);
        if v >= 0 {
            dst[0] = 0x80;
        } else {
            dst[0] = 0xff;
        }
    }
}

fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let base = 1 + key.len() + 1 + value.len() + 1;
    let mut len = base + 1;
    while len != base + len.to_string().len() {
        len = base + len.to_string().len();
    }
    let mut r = format!("{len} {key}=").into_bytes();
    r.extend_from_slice(value);
    r.push(b'\n');
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ustar_skips_unencodable_numbers() {
        let mut ok = Member::new("ok", Kind::File, 0, 0);
        ok.mode = OCTAL_ID_MAX as u32;
        let mut big_mode = Member::new("big-mode", Kind::File, 0, 0);
        big_mode.mode = OCTAL_ID_MAX as u32 + 1;
        let big_mtime = Member::new("big-mtime", Kind::File, 0, OCTAL_SIZE_MAX as i64 + 1);
        let big_dev = Member::new("big-dev", Kind::CharDev(OCTAL_ID_MAX + 1, 0), 0, 0);
        let mut w = TarWriter::new(Vec::new(), Format::Ustar);
        assert!(w.append(&ok, &mut io::empty()).unwrap());
        for m in [&big_mode, &big_mtime, &big_dev] {
            assert!(!w.append(m, &mut io::empty()).unwrap());
        }
        assert_eq!(w.skipped, ["big-mode", "big-mtime", "big-dev"]);
    }

    #[test]
    fn pax_len() {
        let r = pax_record("path", b"abc");
        assert_eq!(
            r.len().to_string(),
            String::from_utf8_lossy(&r).split(' ').next().unwrap()
        );
        let r = pax_record("path", &[b'a'; 95]);
        assert_eq!(
            r.len().to_string(),
            String::from_utf8_lossy(&r).split(' ').next().unwrap()
        );
    }
}
