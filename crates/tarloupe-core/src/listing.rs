//! `tar -tv` compatible listing, used to verify the index against GNU tar.

use crate::entry::Entry;
use crate::header;

/// Formats members exactly like GNU tar's verbose listing (with times in
/// UTC). The size column widens as longer user/group/size values appear,
/// as in GNU tar, so a formatter must see entries in archive order.
pub struct TarTvFormatter {
    ugswidth: usize,
}

impl Default for TarTvFormatter {
    fn default() -> Self {
        TarTvFormatter { ugswidth: 19 }
    }
}

impl TarTvFormatter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn format(&mut self, e: &Entry) -> Vec<u8> {
        let modes = mode_string(e.typeflag, e.mode, &e.raw_name);
        let user = if e.uname.is_empty() {
            e.uid.to_string().into_bytes()
        } else {
            e.uname.clone()
        };
        let group = if e.gname.is_empty() {
            e.gid.to_string().into_bytes()
        } else {
            e.gname.clone()
        };
        let size = match e.typeflag {
            header::CHRTYPE | header::BLKTYPE => format!("{},{}", e.dev_major, e.dev_minor),
            _ => e.size.to_string(),
        };
        let pad = user.len() + 1 + group.len() + 1 + size.len();
        if pad > self.ugswidth {
            self.ugswidth = pad;
        }
        let width = self.ugswidth - pad + size.len();

        let mut out = Vec::new();
        out.extend_from_slice(modes.as_bytes());
        out.push(b' ');
        out.extend_from_slice(&user);
        out.push(b'/');
        out.extend_from_slice(&group);
        out.extend_from_slice(format!(" {size:>width$} {} ", format_time(e.mtime)).as_bytes());
        out.extend_from_slice(&quote(&e.raw_name));
        match e.typeflag {
            header::SYMTYPE => {
                out.extend_from_slice(b" -> ");
                out.extend_from_slice(&quote(&e.link_target));
            }
            header::LNKTYPE => {
                out.extend_from_slice(b" link to ");
                out.extend_from_slice(&quote(&e.link_target));
            }
            _ => {}
        }
        out
    }
}

/// `drwxr-xr-x`-style mode string, using GNU tar's type characters.
pub fn mode_string(typeflag: u8, mode: u32, raw_name: &[u8]) -> String {
    let t = match typeflag {
        header::REGTYPE | header::AREGTYPE | header::GNU_SPARSE => {
            if raw_name.ends_with(b"/") {
                'd'
            } else {
                '-'
            }
        }
        header::LNKTYPE => 'h',
        header::GNU_DUMPDIR | header::DIRTYPE => 'd',
        header::SYMTYPE => 'l',
        header::BLKTYPE => 'b',
        header::CHRTYPE => 'c',
        header::FIFOTYPE => 'p',
        header::CONTTYPE => 'C',
        header::GNU_VOLHDR => 'V',
        header::GNU_MULTIVOL => 'M',
        _ => '?',
    };
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    let special = |x: u32, s: u32, on: char, off: char| match (mode & x != 0, mode & s != 0) {
        (true, true) => on,
        (false, true) => off,
        (true, false) => 'x',
        (false, false) => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(t);
    s.push(bit(0o400, 'r'));
    s.push(bit(0o200, 'w'));
    s.push(special(0o100, 0o4000, 's', 'S'));
    s.push(bit(0o040, 'r'));
    s.push(bit(0o020, 'w'));
    s.push(special(0o010, 0o2000, 's', 'S'));
    s.push(bit(0o004, 'r'));
    s.push(bit(0o002, 'w'));
    s.push(special(0o001, 0o1000, 't', 'T'));
    s
}

/// `YYYY-MM-DD HH:MM` in UTC.
pub fn format_time(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// GNU tar's default "escape" quoting: backslash and control characters are
/// escaped, invalid UTF-8 bytes become octal escapes, printable text is kept.
pub fn quote(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    let mut rest = name;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                escape_str(s, &mut out);
                break;
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                escape_str(std::str::from_utf8(good).unwrap(), &mut out);
                let n = e.error_len().unwrap_or(bad.len());
                for &b in &bad[..n] {
                    out.extend_from_slice(format!("\\{b:03o}").as_bytes());
                }
                rest = &bad[n..];
            }
        }
    }
    out
}

fn escape_str(s: &str, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match c {
            '\\' => out.extend_from_slice(b"\\\\"),
            '\x07' => out.extend_from_slice(b"\\a"),
            '\x08' => out.extend_from_slice(b"\\b"),
            '\x0c' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\x0b' => out.extend_from_slice(b"\\v"),
            c if c.is_control() => {
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.extend_from_slice(format!("\\{b:03o}").as_bytes());
                }
            }
            c => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(mode_string(b'0', 0o644, b"f"), "-rw-r--r--");
        assert_eq!(mode_string(b'5', 0o1777, b"d/"), "drwxrwxrwt");
        assert_eq!(mode_string(b'0', 0o4644, b"f"), "-rwSr--r--");
        assert_eq!(mode_string(b'2', 0o777, b"l"), "lrwxrwxrwx");
    }

    #[test]
    fn times() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_763_164_800), "2025-11-15 00:00");
        assert_eq!(format_time(-60), "1969-12-31 23:59");
        assert_eq!(format_time(4_102_444_800), "2100-01-01 00:00");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote(b"a\\b\nc"), b"a\\\\b\\nc");
        assert_eq!(quote(b"caf\xe9"), b"caf\\351");
        assert_eq!(quote("日本".as_bytes()), "日本".as_bytes());
    }
}
