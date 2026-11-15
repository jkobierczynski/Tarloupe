//! `tarloupew ARCHIVE`: what "Browse with Tarloupe" in Explorer runs. The
//! same as `tarloupe browse ARCHIVE`, but without a console window; errors
//! are shown in a message box.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "../browse.rs"]
#[allow(dead_code)] // the right-click setup lives in `tarloupe`
mod browse;

#[cfg(windows)]
fn main() {
    let Some(archive) = std::env::args_os().nth(1) else {
        browse::message_box("Usage: tarloupew ARCHIVE");
        std::process::exit(2);
    };
    let archive = std::path::PathBuf::from(archive);
    if let Err(e) = browse::browse(&archive, &browse::Options::default(), |_| {}) {
        browse::message_box(&format!("{e:#}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("tarloupew is the Windows Explorer helper; use `tarloupe` instead");
    std::process::exit(1);
}
