//! Archive path normalisation.

/// Normalises a member name: strips leading `/` and `./`, collapses `//` and
/// `/./`, and drops a trailing `/`. `..` components are kept verbatim (the
/// index never resolves them against the host file system).
pub fn normalize(raw: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for comp in raw.split(|&c| c == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(comp);
    }
    out
}

/// Parent of a normalised path (empty for top-level names).
pub fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&c| c == b'/') {
        Some(i) => &path[..i],
        None => &[],
    }
}

/// Resolves a symlink target relative to the directory containing `link`.
/// Absolute targets are treated as archive-root relative. Returns `None`
/// when `..` escapes the archive root.
pub fn resolve_link(link: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<&[u8]> = Vec::new();
    if !target.starts_with(b"/") {
        parts.extend(parent(link).split(|&c| c == b'/').filter(|c| !c.is_empty()));
    }
    for comp in target.split(|&c| c == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm() {
        assert_eq!(normalize(b"./a//b/./c/"), b"a/b/c");
        assert_eq!(normalize(b"/abs/x"), b"abs/x");
        assert_eq!(normalize(b"./"), b"");
    }

    #[test]
    fn links() {
        assert_eq!(resolve_link(b"a/b/l", b"../c").unwrap(), b"a/c");
        assert_eq!(resolve_link(b"a/l", b"/x/y").unwrap(), b"x/y");
        assert_eq!(resolve_link(b"l", b"../x"), None);
    }
}
