//! Low-level parsing of 512-byte tar header blocks (v7, POSIX ustar, GNU).

pub const BLOCK_SIZE: u64 = 512;
pub type Block = [u8; 512];

// Type flags.
pub const REGTYPE: u8 = b'0';
pub const AREGTYPE: u8 = b'\0';
pub const LNKTYPE: u8 = b'1';
pub const SYMTYPE: u8 = b'2';
pub const CHRTYPE: u8 = b'3';
pub const BLKTYPE: u8 = b'4';
pub const DIRTYPE: u8 = b'5';
pub const FIFOTYPE: u8 = b'6';
pub const CONTTYPE: u8 = b'7';
pub const PAX_LOCAL: u8 = b'x';
pub const PAX_GLOBAL: u8 = b'g';
pub const GNU_LONGNAME: u8 = b'L';
pub const GNU_LONGLINK: u8 = b'K';
pub const GNU_SPARSE: u8 = b'S';
pub const GNU_DUMPDIR: u8 = b'D';
pub const GNU_VOLHDR: u8 = b'V';
pub const GNU_MULTIVOL: u8 = b'M';
/// Solaris extended header, same record format as PAX.
pub const SOLARIS_X: u8 = b'X';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Pre-POSIX header, no magic.
    V7,
    /// POSIX.1-1988 ustar (also used by pax): `ustar\0` + `00`.
    Ustar,
    /// GNU tar: `ustar ` + ` \0`.
    Gnu,
}

/// Rounds `n` up to a whole number of 512-byte blocks.
pub fn padded(n: u64) -> u64 {
    n.div_ceil(BLOCK_SIZE) * BLOCK_SIZE
}

pub fn is_zero_block(b: &Block) -> bool {
    b.iter().all(|&x| x == 0)
}

/// Verifies the header checksum. Accepts both the unsigned sum (POSIX) and
/// the signed sum written by some historic implementations.
pub fn checksum_ok(b: &Block) -> bool {
    let Some(stored) = parse_octal(&b[148..156]) else {
        return false;
    };
    let mut unsigned: u64 = 0;
    let mut signed: i64 = 0;
    for (i, &x) in b.iter().enumerate() {
        let x = if (148..156).contains(&i) { b' ' } else { x };
        unsigned += x as u64;
        signed += x as i8 as i64;
    }
    stored == unsigned || stored as i64 == signed
}

/// Bytes of a NUL-terminated field.
pub fn cstr(field: &[u8]) -> &[u8] {
    match field.iter().position(|&c| c == 0) {
        Some(n) => &field[..n],
        None => field,
    }
}

/// Parses an octal numeric field (leading spaces/NULs allowed, terminated by
/// space or NUL). Returns `None` for an invalid field; an all-blank field is 0.
pub fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    let mut i = 0;
    while i < field.len() && (field[i] == b' ' || field[i] == 0) {
        i += 1;
    }
    while i < field.len() {
        let c = field[i];
        match c {
            b'0'..=b'7' => {
                v = v.checked_mul(8)?.checked_add((c - b'0') as u64)?;
            }
            b' ' | 0 => break,
            _ => return None,
        }
        i += 1;
    }
    Some(v)
}

/// Parses a numeric field that may use GNU base-256 encoding (high bit of the
/// first byte set; `0xff` marks a negative two's-complement number).
pub fn parse_numeric(field: &[u8]) -> Option<i128> {
    match field.first() {
        Some(&b) if b & 0x80 != 0 => {
            if field.len() > 15 {
                // Only the low 15 bytes can be represented; the rest must be sign fill.
                let fill = if b == 0xff { 0xff } else { 0 };
                let (hi, _) = field.split_at(field.len() - 15);
                if hi[1..].iter().any(|&x| x != fill) {
                    return None;
                }
            }
            let tail = &field[field.len().saturating_sub(15)..];
            if b == 0xff {
                let mut v: i128 = -1;
                for &x in tail {
                    v = (v << 8) | x as i128;
                }
                Some(v)
            } else {
                let mut v: i128 = 0;
                for (i, &x) in tail.iter().enumerate() {
                    let x = if i == 0 && tail.len() == field.len() {
                        x & 0x7f
                    } else {
                        x
                    };
                    v = (v << 8) | x as i128;
                }
                Some(v)
            }
        }
        _ => parse_octal(field).map(|v| v as i128),
    }
}

pub fn parse_u64(field: &[u8]) -> Option<u64> {
    parse_numeric(field).and_then(|v| u64::try_from(v).ok())
}

/// A view on one header block with typed field accessors.
pub struct Header<'a>(pub &'a Block);

impl<'a> Header<'a> {
    pub fn flavor(&self) -> Flavor {
        let magic = &self.0[257..263];
        let version = &self.0[263..265];
        if magic == b"ustar " && version == b" \0" {
            Flavor::Gnu
        } else if magic == b"ustar\0" || magic == b"ustar " {
            Flavor::Ustar
        } else {
            Flavor::V7
        }
    }

    pub fn typeflag(&self) -> u8 {
        self.0[156]
    }

    /// Path from the header, joining the ustar prefix when present.
    pub fn path(&self) -> Vec<u8> {
        let name = cstr(&self.0[0..100]);
        if self.flavor() == Flavor::Ustar {
            let prefix = cstr(&self.0[345..500]);
            if !prefix.is_empty() {
                let mut p = prefix.to_vec();
                p.push(b'/');
                p.extend_from_slice(name);
                return p;
            }
        }
        name.to_vec()
    }

    pub fn link_name(&self) -> Vec<u8> {
        cstr(&self.0[157..257]).to_vec()
    }

    pub fn mode(&self) -> Option<u32> {
        parse_numeric(&self.0[100..108]).map(|v| (v as u64 & 0o7777_7777) as u32)
    }
    pub fn uid(&self) -> Option<u64> {
        parse_u64(&self.0[108..116])
    }
    pub fn gid(&self) -> Option<u64> {
        parse_u64(&self.0[116..124])
    }
    pub fn size(&self) -> Option<u64> {
        parse_u64(&self.0[124..136])
    }
    pub fn mtime(&self) -> Option<i64> {
        parse_numeric(&self.0[136..148]).and_then(|v| i64::try_from(v).ok())
    }
    pub fn uname(&self) -> Vec<u8> {
        if self.flavor() == Flavor::V7 {
            Vec::new()
        } else {
            cstr(&self.0[265..297]).to_vec()
        }
    }
    pub fn gname(&self) -> Vec<u8> {
        if self.flavor() == Flavor::V7 {
            Vec::new()
        } else {
            cstr(&self.0[297..329]).to_vec()
        }
    }
    pub fn dev_major(&self) -> Option<u64> {
        parse_u64(&self.0[329..337])
    }
    pub fn dev_minor(&self) -> Option<u64> {
        parse_u64(&self.0[337..345])
    }

    /// GNU old-style sparse header: up to 4 (offset, numbytes) pairs,
    /// the "is extended" flag, and the real (logical) file size.
    pub fn gnu_sparse(&self) -> (Vec<(u64, u64)>, bool, Option<u64>) {
        let mut chunks = Vec::new();
        for i in 0..4 {
            let base = 386 + i * 24;
            let off = parse_u64(&self.0[base..base + 12]);
            let len = parse_u64(&self.0[base + 12..base + 24]);
            if self.0[base] == 0 {
                break;
            }
            if let (Some(o), Some(l)) = (off, len) {
                chunks.push((o, l));
            }
        }
        let extended = self.0[482] != 0;
        let realsize = parse_u64(&self.0[483..495]);
        (chunks, extended, realsize)
    }
}

/// Parses a GNU sparse extension block: up to 21 pairs plus the "is extended" flag.
pub fn gnu_sparse_ext(b: &Block) -> (Vec<(u64, u64)>, bool) {
    let mut chunks = Vec::new();
    for i in 0..21 {
        let base = i * 24;
        if b[base] == 0 {
            break;
        }
        if let (Some(o), Some(l)) = (
            parse_u64(&b[base..base + 12]),
            parse_u64(&b[base + 12..base + 24]),
        ) {
            chunks.push((o, l));
        }
    }
    (chunks, b[504] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal() {
        assert_eq!(parse_octal(b"0000644\0"), Some(0o644));
        assert_eq!(parse_octal(b"   644 \0"), Some(0o644));
        assert_eq!(parse_octal(b"\0\0\0\0"), Some(0));
        assert_eq!(parse_octal(b"12x"), None);
    }

    #[test]
    fn base256_positive_over_8gib() {
        let mut f = [0u8; 12];
        f[0] = 0x80;
        let v: u64 = 10 * 1024 * 1024 * 1024 + 7;
        f[4..].copy_from_slice(&v.to_be_bytes());
        assert_eq!(parse_u64(&f), Some(v));
    }

    #[test]
    fn base256_negative() {
        let mut f = [0xffu8; 12];
        f[11] = 0xfe; // -2
        assert_eq!(parse_numeric(&f), Some(-2));
    }

    #[test]
    fn padding() {
        assert_eq!(padded(0), 0);
        assert_eq!(padded(1), 512);
        assert_eq!(padded(512), 512);
        assert_eq!(padded(513), 1024);
    }
}
