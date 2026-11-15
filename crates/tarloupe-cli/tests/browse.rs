//! `tarloupe browse` keeps one mount per archive, stays mounted while
//! something on it is open, and unmounts once it has been idle. The
//! Explorer right-click item is added and removed again.
//!
//! Needs WinFsp, like the mount test: skipped without it unless
//! `TARLOUPE_REQUIRE_WINFSP=1`. The registry test changes the current
//! user's Explorer settings, so it only runs with `TARLOUPE_TEST_REGISTRY=1`
//! (as in CI).
#![cfg(windows)]

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tarloupe_corpus::{Format, Kind, Member, TarWriter};

fn sample(dir: &Path) -> PathBuf {
    let path = dir.join("browse sample.tar");
    let mut w = TarWriter::new(File::create(&path).unwrap(), Format::Gnu);
    for (name, data) in [("a.txt", &b"hello"[..]), ("dir/b.txt", b"world")] {
        let m = Member::new(name, Kind::File, data.len() as u64, 1_700_000_000);
        w.append(&m, &mut &data[..]).unwrap();
    }
    w.finish().unwrap();
    path
}

fn tarloupe(idx: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tarloupe"));
    c.env("TARLOUPE_INDEX_DIR", idx);
    c
}

/// Kills the browse process if the test fails half way.
struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Runs a command, failing the test (after killing it) if it takes longer
/// than `limit`.
fn run_within(mut cmd: Command, limit: Duration) -> Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reader = std::thread::spawn(move || {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let t = std::thread::spawn(move || {
            let _ = err.read_to_end(&mut e);
            e
        });
        let _ = out.read_to_end(&mut o);
        (o, t.join().unwrap())
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command did not finish in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (stdout, stderr) = reader.join().unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn browse_shares_one_mount_and_unmounts_when_idle() {
    let required = std::env::var_os("TARLOUPE_REQUIRE_WINFSP").is_some_and(|v| v == "1");
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let archive = sample(dir.path());

    let mut child = tarloupe(&idx)
        .args(["browse", "--no-window", "--idle-timeout", "4"])
        .arg(&archive)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut guard = Guard(Some(child));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_default();
    let point = line.trim().to_string();
    if point.is_empty() {
        let c = guard.0.as_mut().unwrap();
        let _ = c.kill();
        let _ = c.wait();
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        if !required && err.contains("WinFsp is not installed") {
            eprintln!("skipping: WinFsp is not installed");
            return;
        }
        panic!("browse did not start: {err}");
    }
    let root = PathBuf::from(format!("{point}\\"));
    assert_eq!(fs::read(root.join("A.TXT")).unwrap(), b"hello");

    // A second browse of the same archive reuses the mount and exits.
    let mut again = tarloupe(&idx);
    again.args(["browse", "--no-window"]).arg(&archive);
    let out = run_within(again, Duration::from_secs(60));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), point);

    // An open file keeps it mounted well past the idle time.
    let held = File::open(root.join("dir").join("b.txt")).unwrap();
    std::thread::sleep(Duration::from_secs(9));
    let c = guard.0.as_mut().unwrap();
    assert!(
        c.try_wait().unwrap().is_none(),
        "unmounted while a file was open"
    );

    // Once it is closed, the mount goes away after the idle time.
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = c.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "still mounted long after going idle"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(status.success());
    guard.0 = None;
    assert!(fs::read_dir(&root).is_err(), "{point} is still mounted");
}

#[test]
fn explorer_item_installs_and_uninstalls() {
    if std::env::var_os("TARLOUPE_TEST_REGISTRY").is_none_or(|v| v != "1") {
        eprintln!("skipping: set TARLOUPE_TEST_REGISTRY=1 to let this change your Explorer menu");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let key = r"HKCU\Software\Classes\SystemFileAssociations\.gz\shell\Tarloupe.Browse";
    let query = |k: &str| {
        Command::new("reg")
            .args(["query", k, "/s"])
            .output()
            .unwrap()
    };

    let out = run_within(
        {
            let mut c = tarloupe(dir.path());
            c.args(["explorer", "install"]);
            c
        },
        Duration::from_secs(30),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for ext in [".tar", ".tgz", ".gz"] {
        let k =
            format!(r"HKCU\Software\Classes\SystemFileAssociations\{ext}\shell\Tarloupe.Browse");
        let q = query(&k);
        let text = String::from_utf8_lossy(&q.stdout).to_string();
        assert!(q.status.success(), "{k} missing");
        assert!(text.contains("Browse with Tarloupe"), "{text}");
        assert!(text.contains("tarloupew.exe"), "{text}");
    }
    let gz = String::from_utf8_lossy(&query(key).stdout).to_string();
    assert!(gz.contains("*.tar.gz"), "{gz}");

    let out = run_within(
        {
            let mut c = tarloupe(dir.path());
            c.args(["explorer", "uninstall"]);
            c
        },
        Duration::from_secs(30),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!query(key).status.success(), "still installed");
    // Uninstalling twice is fine.
    let mut c = tarloupe(dir.path());
    c.args(["explorer", "uninstall"]);
    assert!(run_within(c, Duration::from_secs(30)).status.success());
}
