//! PAX extended header records: `"<len> <key>=<value>\n"`.

/// Parses all records; stops at the first malformed one.
pub fn parse_records(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if rest[0] == 0 {
            break; // trailing padding
        }
        let Some(sp) = rest.iter().position(|&c| c == b' ') else {
            break;
        };
        let Some(len) = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            break;
        };
        if len <= sp + 1 || len > rest.len() || rest[len - 1] != b'\n' {
            break;
        }
        let record = &rest[sp + 1..len - 1];
        if let Some(eq) = record.iter().position(|&c| c == b'=') {
            out.push((record[..eq].to_vec(), record[eq + 1..].to_vec()));
        }
        rest = &rest[len..];
    }
    out
}

/// Parses a PAX time value (`[-]seconds[.fraction]`) to whole seconds,
/// rounding toward negative infinity like GNU tar.
pub fn parse_time(v: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(v).ok()?;
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let secs: i64 = int.parse().ok()?;
    let has_frac = frac.bytes().any(|c| c != b'0');
    if !frac.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if secs < 0 || int.starts_with('-') {
        Some(if has_frac { secs - 1 } else { secs })
    } else {
        Some(secs)
    }
}

pub fn parse_u64(v: &[u8]) -> Option<u64> {
    std::str::from_utf8(v).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let data = b"30 mtime=1700000000.123456789\n20 path=a/long/name\n";
        let r = parse_records(data);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, b"mtime");
        assert_eq!(r[1].1, b"a/long/name");
    }

    #[test]
    fn time() {
        assert_eq!(parse_time(b"1700000000.5"), Some(1700000000));
        assert_eq!(parse_time(b"-1.5"), Some(-2));
        assert_eq!(parse_time(b"42"), Some(42));
    }
}
