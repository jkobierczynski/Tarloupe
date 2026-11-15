//! `tarloupe extract`: writes members below a destination directory.
//!
//! Member paths never leave the destination: `..` components are refused,
//! and an existing symlink in the destination is never followed. Symlinks,
//! devices and FIFOs in the archive are skipped with a note.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use tarloupe_core::{Archive, Entry, EntryKind};

/// Extracts one entry. Returns the children to visit for a directory.
pub fn one(a: &Archive, e: &Entry, dest: &Path) -> Result<Option<Vec<Entry>>> {
    let target = host_path(dest, &e.path)?;
    if target.as_path() == dest {
        // The archive root (`./`): just visit its contents.
        return Ok(Some(a.list_dir(b"")?));
    }
    let parent = target.parent().unwrap_or(dest);
    make_dirs(dest, parent)?;
    match e.kind {
        EntryKind::Dir => {
            make_dirs(dest, &target)?;
            Ok(Some(a.list_dir(&e.path)?))
        }
        EntryKind::File | EntryKind::HardLink => {
            // Replace whatever is there (like tar), without following it.
            match fs::symlink_metadata(&target) {
                Ok(m) if m.is_dir() => bail!("a directory is in the way"),
                Ok(_) => fs::remove_file(&target)?,
                Err(_) => {}
            }
            let f = File::options()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("cannot create {}", target.display()))?;
            let mut w = BufWriter::with_capacity(1024 * 1024, f);
            a.copy_to(e, &mut w)?;
            w.flush()?;
            let f = w.into_inner().map_err(|e| e.into_error())?;
            let mtime = if e.mtime >= 0 {
                SystemTime::UNIX_EPOCH + Duration::from_secs(e.mtime as u64)
            } else {
                SystemTime::UNIX_EPOCH - Duration::from_secs(e.mtime.unsigned_abs())
            };
            let _ = f.set_modified(mtime);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = match e.kind {
                    EntryKind::HardLink => a.resolve(e.clone()).map_or(e.mode, |t| t.mode),
                    _ => e.mode,
                };
                f.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            }
            Ok(None)
        }
        EntryKind::Symlink => {
            eprintln!(
                "tarloupe: {}: skipped symbolic link",
                String::from_utf8_lossy(&tarloupe_core::listing::quote(&e.path))
            );
            Ok(None)
        }
        _ => {
            eprintln!(
                "tarloupe: {}: skipped special file",
                String::from_utf8_lossy(&tarloupe_core::listing::quote(&e.path))
            );
            Ok(None)
        }
    }
}

/// Maps a normalised archive path to a path below `dest`.
fn host_path(dest: &Path, path: &[u8]) -> Result<PathBuf> {
    let mut out = dest.to_path_buf();
    for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        if comp == b".." {
            bail!("refusing a path with `..`");
        }
        #[cfg(unix)]
        let part = {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(comp).to_os_string()
        };
        #[cfg(not(unix))]
        let part = {
            let s = std::str::from_utf8(comp).context("name is not valid UTF-8")?;
            if s.contains(['\\', ':']) {
                bail!("name is not valid on this system");
            }
            std::ffi::OsString::from(s)
        };
        out.push(part);
    }
    Ok(out)
}

/// Creates `dir` and its ancestors below `dest`, refusing to pass through
/// anything that is not a real directory (such as a symlink).
fn make_dirs(dest: &Path, dir: &Path) -> Result<()> {
    let rel = dir.strip_prefix(dest).unwrap_or(Path::new(""));
    let mut cur = dest.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is in the way (not a directory)", cur.display()),
            Err(_) => match fs::create_dir(&cur) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(&cur)?.is_dir() {
                        bail!("{} is in the way (not a directory)", cur.display());
                    }
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot create {}", cur.display()))
                }
            },
        }
    }
    Ok(())
}
