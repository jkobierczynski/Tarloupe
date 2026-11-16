//! `tarloupew ARCHIVE`: what "Browse with Tarloupe" in Explorer runs. Hands
//! the archive to this session's Tarloupe tray agent, or becomes that
//! agent, without a console window; errors are shown in a message box.
//!
//! For scripts and tests, `TARLOUPE_BROWSE_IDLE` sets the idle time in
//! seconds (default 120) and `TARLOUPE_BROWSE_NO_WINDOW=1` opens no
//! Explorer window.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "../browse.rs"]
#[allow(dead_code)] // the right-click setup lives in `tarloupe`
mod browse;

#[cfg(windows)]
#[path = "../agent.rs"]
mod agent;

#[cfg(windows)]
fn main() {
    let Some(archive) = std::env::args_os().nth(1) else {
        browse::message_box("Usage: tarloupew ARCHIVE");
        std::process::exit(2);
    };
    let idle = std::env::var("TARLOUPE_BROWSE_IDLE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let settings = agent::Settings {
        idle: std::time::Duration::from_secs(idle),
        open_window: std::env::var_os("TARLOUPE_BROWSE_NO_WINDOW").is_none_or(|v| v != "1"),
    };
    if let Err(e) = agent::run(std::path::Path::new(&archive), settings) {
        browse::message_box(&format!("{e:#}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("tarloupew is the Windows Explorer helper; use `tarloupe` instead");
    std::process::exit(1);
}
