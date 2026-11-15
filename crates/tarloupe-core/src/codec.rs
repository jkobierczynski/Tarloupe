//! Compression detection. M0 only reads plain tar; everything else is
//! recognised so the user gets a clear message instead of "not a tar".

use crate::header;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::Gzip => "gzip",
            Codec::Bzip2 => "bzip2",
            Codec::Xz => "xz",
            Codec::Zstd => "zstd",
        }
    }

    pub(crate) fn milestone(self) -> &'static str {
        match self {
            Codec::None => "M0",
            Codec::Gzip => "M1",
            Codec::Zstd | Codec::Xz => "M3",
            Codec::Bzip2 => "M4",
        }
    }

    /// Detects the codec from the first bytes of a file (at least 512 bytes
    /// should be passed when available). Returns `None` when the data is
    /// neither a known compressed format nor a tar header.
    pub fn detect(head: &[u8]) -> Option<Codec> {
        // A valid tar header wins over magic prefixes: a plain tar whose
        // first member is named e.g. `BZh-report.txt` starts with `BZh`.
        if head.len() >= 512 {
            let block: &[u8; 512] = head[..512].try_into().unwrap();
            if !header::is_zero_block(block) && header::checksum_ok(block) {
                return Some(Codec::None);
            }
        }
        if head.starts_with(&[0x1f, 0x8b]) {
            Some(Codec::Gzip)
        } else if head.starts_with(b"BZh") {
            Some(Codec::Bzip2)
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Some(Codec::Xz)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
            || (head.len() >= 4 && head[0] & 0xf0 == 0x50 && head[1..4] == [0x2a, 0x4d, 0x18])
        {
            // zstd frame, or a skippable frame (used by the seekable format).
            Some(Codec::Zstd)
        } else if head.len() >= 512 {
            let block: &[u8; 512] = head[..512].try_into().unwrap();
            if header::is_zero_block(block) || header::checksum_ok(block) {
                Some(Codec::None)
            } else {
                None
            }
        } else if head.is_empty() {
            // An empty file is a valid (empty) tar for GNU tar.
            Some(Codec::None)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_member_named_like_a_magic_number() {
        let mut h = [0u8; 512];
        h[..14].copy_from_slice(b"BZh-report.txt");
        h[100..108].copy_from_slice(b"0000644\0");
        h[124..136].copy_from_slice(b"00000000000\0");
        h[156] = b'0';
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| b as u64).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        assert_eq!(Codec::detect(&h), Some(Codec::None));
        assert_eq!(Codec::detect(b"BZh91AY&SY"), Some(Codec::Bzip2));
    }
}
