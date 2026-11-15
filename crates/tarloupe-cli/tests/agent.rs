//! `tarloupew` (what "Browse with Tarloupe" runs) keeps one tray agent per
//! session: a second `tarloupew` hands its archive to the running agent and
//! exits, and the agent leaves once everything it mounted has gone idle.
//!
//! Needs WinFsp, like the mount test: skipped without it unless
//! `TARLOUPE_REQUIRE_WINFSP=1`.
#![cfg(windows)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tarloupe_corpus::{Format, Kind, Member, TarWriter};

fn sample(dir: &Path, name: &str, text: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut w = TarWriter::new(File::create(&path).unwrap(), Format::Gnu);
    let m = Member::new("a.txt", Kind::File, text.len() as u64, 1_700_000_000);
    w.append(&m, &mut &text[..]).unwrap();
    w.finish().unwrap();
    path
}

fn tarloupew(idx: &Path, archive: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tarloupew"));
    c.env("TARLOUPE_INDEX_DIR", idx)
        .env("TARLOUPE_BROWSE_IDLE", "4")
        .env("TARLOUPE_BROWSE_NO_WINDOW", "1")
        .arg(archive)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

fn winfsp_installed() -> bool {
    [
        "HKLM\\SOFTWARE\\WOW6432Node\\WinFsp",
        "HKLM\\SOFTWARE\\WinFsp",
    ]
    .iter()
    .any(|k| {
        Command::new("reg")
            .args(["query", k, "/v", "InstallDir"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Kills the agent if the test fails half way.
struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The mount points the running agent has recorded, once there are `n`
/// that can be read.
fn wait_for_mounts(state: &Path, n: usize, agent: &mut Child) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let points: Vec<String> = fs::read_dir(state)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "mnt"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter(|p| !p.is_empty() && fs::read_dir(format!("{p}\\")).is_ok())
            .collect();
        if points.len() == n {
            return points;
        }
        if let Some(s) = agent.try_wait().unwrap() {
            panic!("the agent exited ({s}) with {} of {n} mounts", points.len());
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {n} mounts",
            points.len()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn second_browse_goes_to_the_running_agent() {
    if !winfsp_installed() {
        if std::env::var_os("TARLOUPE_REQUIRE_WINFSP").is_some_and(|v| v == "1") {
            panic!("WinFsp is not installed");
        }
        eprintln!("skipping: WinFsp is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let state = dir.path().join("mounts");
    let first = sample(dir.path(), "first.tar", b"one");
    let second = sample(dir.path(), "second.tar", b"two");

    let mut guard = Guard(Some(tarloupew(&idx, &first).spawn().unwrap()));
    let agent = guard.0.as_mut().unwrap();
    let points = wait_for_mounts(&state, 1, agent);
    let one = PathBuf::from(format!("{}\\", points[0]));
    assert_eq!(fs::read(one.join("A.TXT")).unwrap(), b"one");

    // A second tarloupew hands its archive over and exits at once.
    let mut c = tarloupew(&idx, &second).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = c.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = c.kill();
            panic!("the second tarloupew did not exit");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success());
    let points = wait_for_mounts(&state, 2, agent);
    let two = points
        .iter()
        .map(|p| PathBuf::from(format!("{p}\\")))
        .find(|p| *p != one)
        .unwrap();
    assert_eq!(fs::read(two.join("a.txt")).unwrap(), b"two");

    // An open file keeps its archive mounted; the other goes idle.
    let held = File::open(one.join("a.txt")).unwrap();
    std::thread::sleep(Duration::from_secs(10));
    assert!(agent.try_wait().unwrap().is_none(), "the agent left early");
    assert!(
        fs::read_dir(&two).is_err(),
        "the idle archive is still mounted"
    );
    assert!(
        fs::read_dir(&one).is_ok(),
        "unmounted while a file was open"
    );

    // Once nothing is open, the agent unmounts and leaves.
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = agent.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the agent did not leave");
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(status.success());
    guard.0 = None;
    assert!(fs::read_dir(&one).is_err(), "still mounted");
}
