//! "Browse with Tarloupe": mount an archive, open it in Explorer, and
//! unmount once nothing has used it for a while. Also adds and removes the
//! Explorer right-click item. Windows only.
//!
//! Shared by `tarloupe browse` and the windowless `tarloupew.exe` that the
//! right-click item runs.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tarloupe_core::{Archive, OpenOptions, Progress};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

/// How `browse` behaves.
pub struct Options {
    /// Unmount after this long with nothing open and no requests.
    pub idle: Duration,
    /// Open an Explorer window on the mount.
    pub open_window: bool,
    /// Set to unmount now, even if something is still open.
    pub stop: Arc<AtomicBool>,
    /// Told how far indexing has got, if the archive has to be indexed.
    pub progress: Option<Progress>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            idle: Duration::from_secs(120),
            open_window: true,
            stop: Arc::default(),
            progress: None,
        }
    }
}

/// Wide, NUL-terminated.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

/// A stable 64-bit FNV-1a hash (the same in every build of both programs).
pub fn fnv(bytes: impl IntoIterator<Item = u16>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        for x in b.to_le_bytes() {
            h ^= x as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Where running browse processes record their mount point.
fn state_dir() -> PathBuf {
    let idx = tarloupe_core::index::default_index_dir();
    idx.parent().unwrap_or(&idx).join("mounts")
}

/// Mounts `archive` (or finds the process that already did), opens an
/// Explorer window on it, and returns once it is unmounted. `mounted` is
/// called with the mount point as soon as it is usable.
pub fn browse(archive: &Path, opts: &Options, mut mounted: impl FnMut(&OsStr)) -> Result<()> {
    let path = std::fs::canonicalize(archive)
        .with_context(|| format!("cannot open {}", archive.display()))?;
    // One mount per archive: a named mutex held for as long as it is mounted.
    // canonicalize returns the name as stored on disk, so every spelling of
    // the same file gives the same key, while files that differ only in
    // case (in a case-sensitive folder) stay apart.
    let key = format!("{:016x}", fnv(path.as_os_str().encode_wide()));
    let state = state_dir().join(format!("{key}.mnt"));
    let name = wide(format!("Local\\Tarloupe-browse-{key}"));
    let mutex = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
        .context("cannot create the browse lock")?;
    let mut lock = MutexGuard(mutex, true);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // Another process has it (or is still indexing it). Wait until it
        // is mounted and just open a window, or take over if it exits.
        lock.1 = false;
        loop {
            if let Some(point) = std::fs::read_to_string(&state)
                .ok()
                .filter(|p| !p.is_empty() && std::fs::read_dir(format!("{p}\\")).is_ok())
            {
                let point = OsString::from(point);
                mounted(&point);
                if opts.open_window {
                    open_window(&point)?;
                }
                return Ok(());
            }
            if opts.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let w = unsafe { WaitForSingleObject(mutex, 250) };
            if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
                lock.1 = true;
                break; // the other process is gone; mount it here
            }
        }
    }
    // A file left by a process that crashed may name a drive letter that
    // is now something else; nobody may trust it until it is rewritten.
    let _ = std::fs::remove_file(&state);
    let result = serve(&path, &state, opts, &mut mounted);
    let _ = std::fs::remove_file(&state);
    drop(lock);
    result
}

/// A mutex handle, released (if this thread owns it) and closed on drop.
struct MutexGuard(HANDLE, bool);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            if self.1 {
                let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            }
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn serve(
    path: &Path,
    state: &Path,
    opts: &Options,
    mounted: &mut impl FnMut(&OsStr),
) -> Result<()> {
    let stop = &opts.stop;
    let a = Archive::open(
        path,
        &OpenOptions {
            progress: opts.progress.clone(),
            ..Default::default()
        },
    )
    .with_context(|| format!("cannot open {}", path.display()))?;
    if stop.load(Ordering::Relaxed) {
        return Ok(()); // asked to stop while it was being indexed
    }
    let m = tarloupe_winfsp::mount(Arc::new(a), None)?;
    let point = m.mount_point().to_os_string();
    if let Some(dir) = state.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(state, point.to_string_lossy().as_bytes())?;
    mounted(&point);
    if opts.open_window {
        open_window(&point)?;
    }
    // An Explorer window keeps its folder open, so the idle clock only
    // runs once every window and file on the mount is closed.
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        if m.open_handles() == 0 && m.idle_for() >= opts.idle && started.elapsed() >= opts.idle {
            break;
        }
    }
    drop(m);
    Ok(())
}

pub fn open_window(point: &OsStr) -> Result<()> {
    let mut dir = point.to_os_string();
    dir.push("\\");
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .context("cannot start Explorer")?;
    Ok(())
}

/// File types that get the right-click item, and for `.gz` which names.
const TYPES: [(&str, Option<&str>); 3] = [
    (".tar", None),
    (".tgz", None),
    // Only `.tar.gz`, not every gzip file.
    (".gz", Some("System.FileName:~\"*.tar.gz\"")),
];
const VERB: &str = "Tarloupe.Browse";

fn verb_key(ext: &str) -> String {
    format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\{VERB}")
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn create_key(path: &str) -> Result<Key> {
    let mut key = HKEY::default();
    let p = wide(path);
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(p.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .with_context(|| format!("cannot create HKCU\\{path}"))?;
    Ok(Key(key))
}

fn set_string(key: &Key, name: Option<&str>, value: &OsStr) -> Result<()> {
    let n = name.map(wide);
    let v = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2) };
    unsafe {
        RegSetValueExW(
            key.0,
            n.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            None,
            REG_SZ,
            Some(bytes),
        )
    }
    .ok()
    .context("cannot write to the registry")
}

/// Adds "Browse with Tarloupe" to the right-click menu of tar archives,
/// for the current user only (no administrator rights needed). `exe` is the
/// program the item runs with the archive path.
pub fn install_verb(exe: &Path) -> Result<()> {
    if !exe.is_file() {
        bail!("{} not found", exe.display());
    }
    let mut command = OsString::from("\"");
    command.push(exe);
    command.push("\" \"%1\"");
    let mut icon = exe.as_os_str().to_os_string();
    icon.push(",0");
    for (ext, applies_to) in TYPES {
        let key = create_key(&verb_key(ext))?;
        set_string(&key, Some("MUIVerb"), "Browse with Tarloupe".as_ref())?;
        set_string(&key, Some("Icon"), &icon)?;
        if let Some(a) = applies_to {
            set_string(&key, Some("AppliesTo"), a.as_ref())?;
        }
        let cmd = create_key(&format!("{}\\command", verb_key(ext)))?;
        set_string(&cmd, None, &command)?;
    }
    notify_shell();
    Ok(())
}

/// Removes what `install_verb` added.
pub fn uninstall_verb() -> Result<()> {
    for (ext, _) in TYPES {
        let p = wide(verb_key(ext));
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr())) };
        if r.is_err() && r != ERROR_FILE_NOT_FOUND {
            bail!("cannot remove HKCU\\{}: {r:?}", verb_key(ext));
        }
    }
    notify_shell();
    Ok(())
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// The windowless program next to this one, which the right-click item runs.
pub fn windowless_exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("cannot find this program")?;
    Ok(me.with_file_name("tarloupew.exe"))
}

#[allow(dead_code)] // only the windowless program shows errors this way
pub fn message_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let t = wide(text);
    let c = wide("Tarloupe");
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(t.as_ptr()),
            PCWSTR(c.as_ptr()),
            MB_OK | MB_ICONERROR,
        )
    };
}
