use std::ffi::{c_void, OsStr, OsString};
use std::fmt;
use std::sync::Arc;

use tarloupe_core::vfs::{Node, Vfs};
use tarloupe_core::{Archive, Error};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, HANDLE, HLOCAL, NTSTATUS, STATUS_END_OF_FILE,
    STATUS_FILE_CORRUPT_ERROR, STATUS_FILE_IS_A_DIRECTORY, STATUS_IO_DEVICE_ERROR,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    GetLogicalDrives, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::{FspError, FspInit, U16CStr};

/// Read and execute (`FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`). The
/// volume is read-only, so nobody gets more.
const READ_EXECUTE: &str = "0x1200a9";
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: i64 = 11_644_473_600;

/// Why mounting failed.
#[derive(Debug)]
pub enum MountError {
    /// WinFsp is not installed (or its DLL could not be loaded).
    WinFspMissing,
    /// WinFsp reported an error.
    WinFsp(String),
    Archive(Error),
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MountError::WinFspMissing => write!(
                f,
                "WinFsp is not installed; get it from https://winfsp.dev/rel/ (or `winget install WinFsp.WinFsp`)"
            ),
            MountError::WinFsp(e) => write!(f, "WinFsp: {e}"),
            MountError::Archive(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MountError {}

fn fsp(e: windows::core::Error) -> MountError {
    MountError::WinFsp(e.to_string())
}

/// A mounted archive. Dropping it unmounts.
pub struct Mount {
    host: FileSystemHost<TarFs>,
    point: OsString,
    _init: FspInit,
}

impl Mount {
    /// Where the archive is mounted (a drive like `X:` or a folder).
    pub fn mount_point(&self) -> &OsStr {
        &self.point
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
    }
}

/// Mounts `archive` read-only at `point`: a drive letter (`X:`), or a
/// folder that does not exist yet. `None` picks the next free drive letter.
pub fn mount(archive: Arc<Archive>, point: Option<&OsStr>) -> Result<Mount, MountError> {
    let init = load_winfsp()?;
    let label = archive
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Tarloupe".into());
    let total = std::fs::metadata(archive.path())
        .map(|m| m.len())
        .unwrap_or(0);
    let fs = TarFs {
        vfs: Vfs::new(archive).map_err(MountError::Archive)?,
        sd: security_descriptor()?,
        label,
        total,
    };

    let mut params = VolumeParams::new();
    params
        .filesystem_name("Tarloupe")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_serial_number(0x7a71_0cbe)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .read_only_volume(true)
        .post_cleanup_when_modified_only(true)
        // Nothing ever changes, so the kernel may cache metadata for long.
        .file_info_timeout(60_000)
        .dir_info_timeout(60_000)
        .volume_info_timeout(60_000)
        .security_timeout(60_000);
    let point = match point {
        Some(p) => p.to_os_string(),
        None => free_drive().ok_or_else(|| MountError::WinFsp("no free drive letter".into()))?,
    };
    let mut host = FileSystemHost::<TarFs>::new(params, fs).map_err(fsp)?;
    host.mount(&point).map_err(fsp)?;
    host.start().map_err(fsp)?;
    Ok(Mount {
        host,
        point,
        _init: init,
    })
}

/// The last unused drive letter, counting down from `Z:`.
fn free_drive() -> Option<OsString> {
    let used = unsafe { GetLogicalDrives() };
    (b'D'..=b'Z')
        .rev()
        .find(|&l| used & (1 << (l - b'A')) == 0)
        .map(|l| OsString::from(format!("{}:", l as char)))
}

/// Loads WinFsp's DLL from its install folder (it is not on `PATH`), then
/// lets the winfsp crate finish initialising.
fn load_winfsp() -> Result<FspInit, MountError> {
    if let Some(dir) = install_dir() {
        let dll = if cfg!(target_arch = "aarch64") {
            "winfsp-a64.dll"
        } else if cfg!(target_arch = "x86") {
            "winfsp-x86.dll"
        } else {
            "winfsp-x64.dll"
        };
        let mut path: Vec<u16> = dir.encode_wide_lossy();
        path.extend("\\bin\\".encode_utf16());
        path.extend(dll.encode_utf16());
        path.push(0);
        // Once loaded, the delay-load stubs find the module by name.
        let _ = unsafe { LoadLibraryW(PCWSTR(path.as_ptr())) };
    }
    winfsp::winfsp_init().map_err(|_| MountError::WinFspMissing)
}

trait EncodeWide {
    fn encode_wide_lossy(&self) -> Vec<u16>;
}

impl EncodeWide for OsString {
    fn encode_wide_lossy(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        let mut v: Vec<u16> = self.encode_wide().collect();
        while v.last() == Some(&0) {
            v.pop();
        }
        while v.last() == Some(&(b'\\' as u16)) {
            v.pop();
        }
        v
    }
}

fn install_dir() -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    for key in [w!("SOFTWARE\\WOW6432Node\\WinFsp"), w!("SOFTWARE\\WinFsp")] {
        let mut buf = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size as *mut u32),
            )
        };
        if status.is_ok() {
            let len = (size as usize / 2).min(buf.len());
            return Some(OsString::from_wide(&buf[..len]));
        }
    }
    None
}

/// The string SID of the user running this process.
fn current_user_sid() -> Result<String, MountError> {
    let err = |what: &str, e: windows::core::Error| MountError::WinFsp(format!("{what}: {e}"));
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| err("cannot open the process token", e))?;
        let mut len = 0u32;
        // The first call only reports the size needed.
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let res = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        res.map_err(|e| err("cannot read the process user", e))?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)
            .map_err(|e| err("cannot format the user SID", e))?;
        let sid = s.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(sid)
    }
}

/// Every file and folder gets the same descriptor: owned by the user who
/// mounted the archive, readable by that user, SYSTEM and Administrators
/// only. Other local accounts get nothing, even on a folder mount they
/// can reach, since they may not be allowed to read the archive itself.
fn security_descriptor() -> Result<Vec<u8>, MountError> {
    let sid = current_user_sid()?;
    let sddl = format!(
        "O:{sid}G:{sid}D:P(A;;{READ_EXECUTE};;;{sid})(A;;{READ_EXECUTE};;;SY)(A;;{READ_EXECUTE};;;BA)"
    );
    let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let mut len = 0u32;
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            Some(&mut len as *mut u32),
        )
        .map_err(|e| MountError::WinFsp(format!("security descriptor: {e}")))?;
        let sd = std::slice::from_raw_parts(psd.0 as *const u8, len as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(psd.0)));
        Ok(sd)
    }
}

struct TarFs {
    vfs: Vfs,
    /// Self-relative security descriptor shared by every file.
    sd: Vec<u8>,
    label: String,
    total: u64,
}

pub struct Handle {
    node: Node,
    dir: DirBuffer,
}

fn status(s: NTSTATUS) -> FspError {
    s.into()
}

fn map_err(e: Error) -> FspError {
    status(match e {
        Error::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        Error::Corrupt { .. } => STATUS_FILE_CORRUPT_ERROR,
        Error::Io(_) | Error::Sqlite(_) => STATUS_IO_DEVICE_ERROR,
        _ => STATUS_UNSUCCESSFUL,
    })
}

fn filetime(unix: i64) -> u64 {
    unix.saturating_add(EPOCH_DIFF)
        .max(0)
        .saturating_mul(10_000_000) as u64
}

fn attributes(node: &Node) -> u32 {
    if node.is_dir {
        FILE_ATTRIBUTE_DIRECTORY.0
    } else {
        FILE_ATTRIBUTE_READONLY.0
    }
}

fn fill(info: &mut FileInfo, node: &Node) {
    let t = filetime(node.mtime());
    info.file_attributes = attributes(node);
    info.reparse_tag = 0;
    info.file_size = node.size();
    info.allocation_size = node.size().div_ceil(4096) * 4096;
    info.creation_time = t;
    info.last_access_time = t;
    info.last_write_time = t;
    info.change_time = t;
    info.index_number = node.id();
    info.hard_links = 0;
    info.ea_size = 0;
}

impl TarFs {
    fn lookup(&self, name: &U16CStr) -> winfsp::Result<Node> {
        let path = name.to_string_lossy();
        self.vfs
            .lookup(&path)
            .map_err(map_err)?
            .ok_or_else(|| status(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    fn copy_sd(&self, buf: Option<&mut [c_void]>) {
        if let Some(buf) = buf {
            if buf.len() >= self.sd.len() {
                // SAFETY: both regions are valid for `sd.len()` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.sd.as_ptr(),
                        buf.as_mut_ptr().cast::<u8>(),
                        self.sd.len(),
                    );
                }
            }
        }
    }
}

impl FileSystemContext for TarFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let node = self.lookup(file_name)?;
        self.copy_sd(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: attributes(&node),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        let node = self.lookup(file_name)?;
        if create_options & FILE_DIRECTORY_FILE != 0 && !node.is_dir {
            return Err(status(STATUS_NOT_A_DIRECTORY));
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && node.is_dir {
            return Err(status(STATUS_FILE_IS_A_DIRECTORY));
        }
        fill(file_info.as_mut(), &node);
        Ok(Handle {
            node,
            dir: DirBuffer::new(),
        })
    }

    fn close(&self, _context: Handle) {}

    fn get_file_info(&self, context: &Handle, file_info: &mut FileInfo) -> winfsp::Result<()> {
        fill(file_info, &context.node);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Handle,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_sd(security_descriptor);
        Ok(self.sd.len() as u64)
    }

    fn read(&self, context: &Handle, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if offset >= context.node.size() {
            return Err(status(STATUS_END_OF_FILE));
        }
        let mut done = 0;
        while done < buffer.len() {
            let n = self
                .vfs
                .read(&context.node, offset + done as u64, &mut buffer[done..])
                .map_err(map_err)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done as u32)
    }

    fn read_directory(
        &self,
        context: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.node.is_dir {
            return Err(status(STATUS_NOT_A_DIRECTORY));
        }
        // Fill the buffer on the first read; later reads continue from the
        // marker. WinFsp sorts the entries and applies the pattern.
        if let Ok(lock) = context.dir.acquire(marker.is_none(), None) {
            let listing = self.vfs.list(&context.node).map_err(map_err)?;
            let is_root = context.node.entry.path.is_empty();
            if !is_root {
                for dots in [".", ".."] {
                    let mut di: DirInfo = DirInfo::new();
                    fill(di.file_info_mut(), &context.node);
                    di.set_name(dots)?;
                    lock.write(&mut di)?;
                }
            }
            for (name, node) in &listing.entries {
                let mut di: DirInfo = DirInfo::new();
                fill(di.file_info_mut(), node);
                di.set_name(name)?;
                lock.write(&mut di)?;
            }
        }
        Ok(context.dir.read(marker, buffer))
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        out.total_size = self.total;
        out.free_size = 0;
        out.set_volume_label(&self.label);
        Ok(())
    }
}
