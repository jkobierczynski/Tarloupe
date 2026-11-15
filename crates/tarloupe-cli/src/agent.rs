//! The Tarloupe tray agent: one per Windows session. "Browse with
//! Tarloupe" starts `tarloupew.exe`, which hands the archive to the agent
//! that is already running (and exits), or becomes the agent itself. The
//! agent mounts each archive on its own thread, shows a tray icon with
//! indexing progress and what is mounted, and exits once nothing is.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW,
    GetWindowThreadProcessId, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SendMessageTimeoutW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HMENU, IDI_APPLICATION, MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, SMTO_ABORTIFHUNG, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP,
    WM_COMMAND, WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::browse;

const CLASS: &str = "Tarloupe.Agent";
/// Serialises finding, starting and leaving the agent.
const LOCK: &str = "Local\\Tarloupe-agent";
/// Marks a WM_COPYDATA that carries an archive path.
const COPY_PATH: usize = 0x544c_5550;
/// The tray icon's mouse messages.
const WM_TRAY: u32 = WM_APP + 1;
/// Something changed: redraw the tip, and leave if nothing is left.
const WM_REFRESH: u32 = WM_APP + 2;
const TIMER: usize = 1;
const CMD_QUIT: usize = 1;
/// Menu ids from here on: `CMD_JOB + 2 * job id + (0 open, 1 unmount)`.
const CMD_JOB: usize = 0x100;

/// Settings for every archive the agent browses.
pub struct Settings {
    pub idle: Duration,
    pub open_window: bool,
}

/// Runs "Browse with Tarloupe" for `archive`: by handing it to the agent
/// of this session, or by becoming that agent. In the agent, returns once
/// every archive is unmounted.
pub fn run(archive: &Path, settings: Settings) -> Result<()> {
    let path = std::fs::canonicalize(archive)
        .with_context(|| format!("cannot open {}", archive.display()))?;
    let lock = Lock::new()?;
    lock.acquire()?;
    let class = wide(CLASS);
    if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) } {
        let r = forward(hwnd, &path);
        lock.release();
        return r;
    }
    let agent = AGENT.get_or_init(|| Agent {
        settings,
        jobs: Mutex::default(),
        next_id: Mutex::new(0),
        quitting: AtomicBool::new(false),
        threads: AtomicUsize::new(0),
        hwnd: AtomicIsize::new(0),
        lock,
        taskbar_created: unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) },
    });
    let made = create_window(agent);
    agent.lock.release();
    let hwnd = made?;
    agent.add_icon(hwnd);
    agent.start(path);
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Sends the path to the running agent.
fn forward(hwnd: HWND, path: &Path) -> Result<()> {
    let data: Vec<u16> = path.as_os_str().encode_wide().collect();
    let cds = COPYDATASTRUCT {
        dwData: COPY_PATH,
        cbData: (data.len() * 2) as u32,
        lpData: data.as_ptr() as *mut _,
    };
    // Let the agent bring its Explorer window to the front.
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != 0 {
        let _ = unsafe { AllowSetForegroundWindow(pid) };
    }
    let mut answer = 0usize;
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            30_000,
            Some(&mut answer),
        )
    };
    if sent.0 == 0 || answer != 1 {
        bail!("Tarloupe is running but did not answer; try again in a moment");
    }
    Ok(())
}

/// What the agent knows about one archive.
struct Job {
    id: usize,
    path: PathBuf,
    state: State,
    stop: Arc<AtomicBool>,
}

#[derive(Clone)]
enum State {
    Indexing(u8),
    Mounted(OsString),
}

struct Agent {
    settings: Settings,
    jobs: Mutex<Vec<Job>>,
    next_id: Mutex<usize>,
    quitting: AtomicBool,
    /// Worker threads still running (one may be showing an error).
    threads: AtomicUsize,
    hwnd: AtomicIsize,
    lock: Lock,
    taskbar_created: u32,
}

static AGENT: OnceLock<Agent> = OnceLock::new();

impl Agent {
    fn hwnd(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::Relaxed) as *mut _)
    }

    fn refresh(&self) {
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_REFRESH, WPARAM(0), LPARAM(0)) };
    }

    /// Browses `path` on a new thread, unless it is already being browsed.
    /// False if the agent is on its way out.
    fn start(&'static self, path: PathBuf) -> bool {
        if self.quitting.load(Ordering::Relaxed) {
            return false;
        }
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.iter().find(|j| j.path == path) {
            // Clicked again: open another window once it is mounted. While
            // it is still indexing, the window it opens then is enough.
            if let (State::Mounted(p), true) = (&j.state, self.settings.open_window) {
                let _ = browse::open_window(p);
            }
            return true;
        }
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let stop = Arc::new(AtomicBool::new(false));
        jobs.push(Job {
            id,
            path: path.clone(),
            state: State::Indexing(0),
            stop: stop.clone(),
        });
        drop(jobs);
        self.refresh();
        self.threads.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let progress: tarloupe_core::Progress = Arc::new(move |done, total| {
                let pct = (done.saturating_mul(100) / total.max(1)).min(100) as u8;
                self.set_state(id, State::Indexing(pct));
            });
            let opts = browse::Options {
                idle: self.settings.idle,
                open_window: self.settings.open_window,
                stop,
                progress: Some(progress),
            };
            let r = browse::browse(&path, &opts, |point| {
                self.set_state(id, State::Mounted(point.to_os_string()))
            });
            self.jobs.lock().unwrap().retain(|j| j.id != id);
            self.refresh();
            if let Err(e) = r {
                browse::message_box(&format!("{e:#}"));
            }
            self.threads.fetch_sub(1, Ordering::SeqCst);
            self.refresh();
        });
        true
    }

    fn set_state(&self, id: usize, state: State) {
        if let Some(j) = self.jobs.lock().unwrap().iter_mut().find(|j| j.id == id) {
            j.state = state;
        }
        self.refresh();
    }

    fn stop(&self, id: Option<usize>) {
        for j in self.jobs.lock().unwrap().iter() {
            if id.is_none_or(|id| id == j.id) {
                j.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn icon_data(&self, hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
            ..Default::default()
        };
        let tip: Vec<u16> = OsStr::new(&self.tip()).encode_wide().collect();
        let n = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        nid
    }

    /// The tray icon is a convenience: without a taskbar the agent still
    /// works, so a failure here is ignored.
    fn add_icon(&self, hwnd: HWND) {
        let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &self.icon_data(hwnd)) };
    }

    fn tip(&self) -> String {
        let jobs = self.jobs.lock().unwrap();
        let mut t = String::from("Tarloupe");
        for j in jobs.iter() {
            t.push('\n');
            t.push_str(&describe(j));
        }
        t
    }

    fn idle(&self) -> bool {
        self.threads.load(Ordering::SeqCst) == 0 && self.jobs.lock().unwrap().is_empty()
    }

    /// Leaves when nothing is mounted or being indexed. Another
    /// `tarloupew` may be about to hand over an archive: only leave while
    /// holding the lock, so it either reaches this agent before or starts
    /// a new one after.
    fn maybe_leave(&self, hwnd: HWND) {
        if !self.idle() {
            return;
        }
        if !self.lock.try_acquire() {
            return; // the timer tries again
        }
        if self.idle() {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        self.lock.release();
    }

    fn menu(&self, hwnd: HWND) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let jobs: Vec<(usize, String, State)> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|j| (j.id, describe(j), j.state.clone()))
            .collect();
        unsafe {
            if jobs.is_empty() {
                append(menu, MF_STRING | MF_GRAYED, 0, "Nothing is mounted");
            }
            for (id, text, state) in &jobs {
                let Ok(sub) = CreatePopupMenu() else { continue };
                let open = if matches!(state, State::Mounted(_)) {
                    MF_STRING
                } else {
                    MF_STRING | MF_GRAYED
                };
                append(sub, open, CMD_JOB + 2 * id, "Open");
                append(sub, MF_STRING, CMD_JOB + 2 * id + 1, "Unmount");
                append(menu, MF_POPUP, sub.0 as usize, text);
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            append(menu, MF_STRING, CMD_QUIT, "Unmount all and quit");
            let mut at = POINT::default();
            let _ = GetCursorPos(&mut at);
            // Without this the menu does not close when clicking elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                at.x,
                at.y,
                None,
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }

    fn command(&self, cmd: usize) {
        if cmd == CMD_QUIT {
            self.quitting.store(true, Ordering::Relaxed);
            self.stop(None);
            return;
        }
        if cmd < CMD_JOB {
            return;
        }
        let (id, unmount) = ((cmd - CMD_JOB) / 2, (cmd - CMD_JOB) % 2 == 1);
        if unmount {
            self.stop(Some(id));
        } else if let Some(State::Mounted(p)) = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.state.clone())
        {
            let _ = browse::open_window(&p);
        }
    }
}

fn describe(j: &Job) -> String {
    let name = j.path.file_name().map_or_else(
        || j.path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    match &j.state {
        State::Indexing(pct) => format!("{name}: indexing {pct}%"),
        State::Mounted(p) => format!("{name}: {}", p.to_string_lossy()),
    }
}

fn create_window(agent: &'static Agent) -> Result<HWND> {
    let class = wide(CLASS);
    let title = wide("Tarloupe");
    let instance = unsafe { GetModuleHandleW(None) }.context("cannot start the tray agent")?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
    // A hidden top-level window rather than a message-only one: the tray
    // needs it for its menu, and it hears that Explorer restarted.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .context("cannot start the tray agent")?;
    agent.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    // Checks now and then whether it is time to leave.
    unsafe { SetTimer(Some(hwnd), TIMER, 1000, None) };
    Ok(hwnd)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(agent) = AGENT.get() else {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    };
    match msg {
        WM_COPYDATA => {
            // SAFETY: the sender keeps the data alive until we return.
            let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            if cds.dwData != COPY_PATH || cds.lpData.is_null() || cds.cbData % 2 != 0 {
                return LRESULT(0);
            }
            let units = unsafe {
                std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2)
            };
            LRESULT(agent.start(PathBuf::from(OsString::from_wide(units))) as isize)
        }
        WM_TRAY => {
            let m = (lp.0 & 0xffff) as u32;
            if m == WM_RBUTTONUP || m == WM_LBUTTONUP || m == WM_CONTEXTMENU {
                agent.menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            agent.command(wp.0 & 0xffff);
            LRESULT(0)
        }
        WM_REFRESH => {
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &agent.icon_data(hwnd)) };
            agent.maybe_leave(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            agent.maybe_leave(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &agent.icon_data(hwnd)) };
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        m if m == agent.taskbar_created && m != 0 => {
            agent.add_icon(hwnd);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// The named mutex that `LOCK` names.
struct Lock(HANDLE);

// The handle is only a kernel object reference; any thread may use it.
unsafe impl Send for Lock {}
unsafe impl Sync for Lock {}

impl Lock {
    fn new() -> Result<Lock> {
        let name = wide(LOCK);
        let h = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .context("cannot create the agent lock")?;
        Ok(Lock(h))
    }

    fn acquire(&self) -> Result<()> {
        let w = unsafe { WaitForSingleObject(self.0, 60_000) };
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Ok(())
        } else {
            bail!("Tarloupe is busy; try again in a moment")
        }
    }

    fn try_acquire(&self) -> bool {
        let w = unsafe { WaitForSingleObject(self.0, 0) };
        w == WAIT_OBJECT_0 || w == WAIT_ABANDONED
    }

    fn release(&self) {
        let _ = unsafe { ReleaseMutex(self.0) };
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

/// Adds one item to a menu.
unsafe fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let t = wide(text);
    let _ = unsafe { AppendMenuW(menu, flags, id, PCWSTR(t.as_ptr())) };
}
