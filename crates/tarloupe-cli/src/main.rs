//! `tarloupe`: index, list and read huge tar archives without extracting them.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tarloupe_core::archive::IndexStatus;
use tarloupe_core::listing::{format_time, mode_string, quote, TarTvFormatter};
use tarloupe_core::{Archive, EntryKind, OpenOptions};

#[derive(Parser)]
#[command(
    name = "tarloupe",
    version,
    about = "Fast indexed browsing of huge tar archives"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct IndexOpts {
    /// Use this index file instead of the shared index cache (e.g. a sidecar file).
    #[arg(long, global = true, value_name = "FILE")]
    index: Option<PathBuf>,
    /// Directory for cached index files (default: the per-user Tarloupe index folder).
    #[arg(long, global = true, value_name = "DIR")]
    index_dir: Option<PathBuf>,
    /// Distance between gzip restart points when building an index, e.g. 8M
    /// (default). Smaller makes random reads faster and the index larger.
    #[arg(long, global = true, value_name = "SIZE", value_parser = parse_size)]
    checkpoint_spacing: Option<u64>,
    /// Threads that decode while an index is built (default: one per CPU).
    /// Used for bzip2 and gzip.
    #[arg(long, global = true, value_name = "N")]
    index_threads: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build (or rebuild) the index for an archive.
    Index {
        archive: PathBuf,
        /// Rebuild even if a valid index exists.
        #[arg(short, long)]
        force: bool,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// List every member in archive order, like `tar -t`.
    #[command(visible_alias = "t")]
    List {
        archive: PathBuf,
        /// Long listing in the same format as `tar -tv` (times in UTC).
        #[arg(short, long)]
        verbose: bool,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// List the contents of a directory inside the archive.
    Ls {
        archive: PathBuf,
        /// Directory inside the archive (default: the root).
        #[arg(default_value = "")]
        path: OsString,
        /// Long listing.
        #[arg(short, long)]
        long: bool,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// Extract files from the archive into a directory.
    #[command(visible_alias = "x")]
    Extract {
        archive: PathBuf,
        /// Members to extract (default: everything). Directories are
        /// extracted with their contents.
        paths: Vec<OsString>,
        /// Destination directory.
        #[arg(short = 'C', long, default_value = ".")]
        directory: PathBuf,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// Write the contents of one or more files to standard output.
    Cat {
        archive: PathBuf,
        #[arg(required = true)]
        paths: Vec<OsString>,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// Recompress an archive so that any file in it reads fast: seekable
    /// zstd (default) or multi-block xz. The tar inside stays the same.
    Convert {
        archive: PathBuf,
        /// Output file (default: next to the archive, e.g. big.tar.gz ->
        /// big.tar.zst).
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value = "zstd")]
        to: ConvertTo,
        /// zstd level (1-22, default 9) or xz preset (0-9, default 6).
        #[arg(short, long)]
        level: Option<i32>,
        /// Uncompressed size of each zstd frame or xz block, e.g. 8M
        /// (default). Smaller makes reads faster and the file a bit larger.
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        block_size: Option<u64>,
        /// Compression threads (default: one per CPU).
        #[arg(short = 'T', long, default_value_t = 0)]
        threads: usize,
        /// Replace the output file if it exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Open the archive in an Explorer window (Windows, needs WinFsp). It is
    /// mounted on a free drive letter and unmounted once every window and
    /// file on it has been closed for the idle time. Running it again for
    /// the same archive opens another window on the same mount.
    Browse {
        archive: PathBuf,
        /// Seconds of idleness before unmounting.
        #[arg(long, default_value_t = 120, value_name = "SECONDS")]
        idle_timeout: u64,
        /// Mount and print the drive letter, but open no window.
        #[arg(long)]
        no_window: bool,
    },
    /// Add or remove "Browse with Tarloupe" in Explorer's right-click menu
    /// for .tar, .tar.gz, .tar.xz, .tar.zst and .tar.bz2 files (and .tgz,
    /// .txz, .tzst, .tbz2, .tbz) (Windows, current user only).
    Explorer {
        #[command(subcommand)]
        action: ExplorerAction,
    },
    /// Show the archive as a read-only folder or drive (Windows, needs WinFsp).
    /// Runs until Ctrl+C, then unmounts.
    Mount {
        archive: PathBuf,
        /// Where to mount: a free drive letter such as `T:`, or a folder path
        /// that does not exist yet (default: the last free drive letter).
        mount_point: Option<OsString>,
        #[command(flatten)]
        idx: IndexOpts,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ConvertTo {
    Zstd,
    Xz,
}

#[derive(Subcommand)]
enum ExplorerAction {
    /// Add the right-click item. It runs `tarloupew.exe`, which must be
    /// next to this program.
    Install,
    /// Remove the right-click item.
    Uninstall,
}

fn open(archive: &PathBuf, idx: &IndexOpts, rebuild: bool) -> Result<Archive> {
    let opts = OpenOptions {
        index_path: idx.index.clone(),
        index_dir: idx.index_dir.clone(),
        rebuild,
        require_index: false,
        checkpoint_spacing: idx.checkpoint_spacing,
        progress: None,
        cancel: None,
        threads: idx.index_threads.filter(|&n| n > 0),
    };
    let start = Instant::now();
    let a = Archive::open(archive, &opts)
        .with_context(|| format!("cannot open {}", archive.display()))?;
    if a.index_status() != IndexStatus::Loaded {
        eprintln!(
            "tarloupe: indexed {} in {:.2?} -> {}",
            archive.display(),
            start.elapsed(),
            a.index_path().display()
        );
        warn_if_slow(&a);
    }
    Ok(a)
}

/// Explains why reads will be slow, and how to fix it.
fn warn_if_slow(a: &Archive) {
    if let Some(why) = a.slow_access() {
        // The path is not pasted into a command line: it may come from
        // someone else and contain shell syntax.
        // Without the stream size (indexes from 0.1) only the known gaps
        // are measured, so the figure is a lower bound.
        let bound = if a.stream_size().is_some() {
            "up to"
        } else {
            "at least"
        };
        eprintln!(
            "tarloupe: warning: {why}; a read can decode {bound} {} first. \
             `tarloupe convert` on this archive makes a copy that reads fast.",
            human_size(a.max_decode_distance()),
        );
    }
}

fn human_size(n: u64) -> String {
    let units = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < units.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} bytes")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

mod extract;

/// Parses a size such as `512K`, `8M` or `1G` (binary units).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (
            &s[..i],
            match c.to_ascii_uppercase() {
                'K' => 1u64 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                _ => return Err(format!("unknown size suffix in {s:?}")),
            },
        ),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("invalid size {s:?}"))
}

fn run(cli: Cli) -> Result<()> {
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    match cli.cmd {
        Cmd::Index {
            archive,
            force,
            idx,
        } => {
            let a = open(&archive, &idx, force)?;
            let members = a.index().meta("member_count")?.unwrap_or_default();
            let status = match a.index_status() {
                IndexStatus::Loaded => "up to date",
                IndexStatus::Built => "built",
                IndexStatus::Rebuilt => "rebuilt",
            };
            let points = a
                .index()
                .meta("checkpoint_count")?
                .unwrap_or_else(|| "0".into());
            writeln!(
                out,
                "{status}: {members} members, {points} checkpoints, index {}",
                a.index_path().display()
            )?;
        }
        Cmd::List {
            archive,
            verbose,
            idx,
        } => {
            let a = open(&archive, &idx, false)?;
            let mut fmt = TarTvFormatter::new();
            a.for_each_member(|e| {
                if verbose {
                    out.write_all(&fmt.format(&e))?;
                } else {
                    out.write_all(&quote(&e.raw_name))?;
                }
                out.write_all(b"\n")?;
                Ok(())
            })?;
        }
        Cmd::Ls {
            archive,
            path,
            long,
            idx,
        } => {
            let a = open(&archive, &idx, false)?;
            for e in a.list_dir(path.as_encoded_bytes())? {
                if long {
                    // Hard links show the size of the data they share.
                    let size = match e.kind {
                        EntryKind::HardLink => {
                            a.resolve(e.clone()).map(|t| t.size).unwrap_or(e.size)
                        }
                        _ => e.size,
                    };
                    let user = if e.uname.is_empty() {
                        e.uid.to_string()
                    } else {
                        String::from_utf8_lossy(&quote(&e.uname)).into_owned()
                    };
                    let group = if e.gname.is_empty() {
                        e.gid.to_string()
                    } else {
                        String::from_utf8_lossy(&quote(&e.gname)).into_owned()
                    };
                    write!(
                        out,
                        "{} {:>8} {:>8} {:>12} {} ",
                        mode_string(
                            e.typeflag,
                            e.mode,
                            if e.kind == EntryKind::Dir { b"/" } else { b"" }
                        ),
                        user,
                        group,
                        size,
                        format_time(e.mtime)
                    )?;
                }
                out.write_all(&quote(e.name()))?;
                if e.kind == EntryKind::Dir {
                    out.write_all(b"/")?;
                }
                if long && e.kind == EntryKind::Symlink {
                    out.write_all(b" -> ")?;
                    out.write_all(&quote(&e.link_target))?;
                }
                out.write_all(b"\n")?;
            }
        }
        Cmd::Extract {
            archive,
            paths,
            directory,
            idx,
        } => {
            let a = open(&archive, &idx, false)?;
            let mut todo = Vec::new();
            if paths.is_empty() {
                todo.extend(a.list_dir(b"")?);
            }
            for p in &paths {
                let name = p.to_string_lossy();
                todo.push(
                    a.lookup(p.as_encoded_bytes())?
                        .with_context(|| format!("{name}: not found in archive"))?,
                );
            }
            fs::create_dir_all(&directory)
                .with_context(|| format!("cannot create {}", directory.display()))?;
            let mut problems = 0usize;
            while let Some(e) = todo.pop() {
                match extract::one(&a, &e, &directory) {
                    Ok(Some(children)) => todo.extend(children),
                    Ok(None) => {}
                    Err(err) => {
                        problems += 1;
                        eprintln!(
                            "tarloupe: {}: {}",
                            String::from_utf8_lossy(&quote(&e.path)),
                            escape_controls(&format!("{err:#}"))
                        );
                    }
                }
            }
            if problems > 0 {
                anyhow::bail!("{problems} member(s) were not extracted");
            }
        }
        Cmd::Cat {
            archive,
            paths,
            idx,
        } => {
            let a = open(&archive, &idx, false)?;
            for p in paths {
                // Operands keep their raw bytes, so names that are not
                // valid UTF-8 can still be selected on Unix.
                let name = p.to_string_lossy();
                let e = a
                    .lookup(p.as_encoded_bytes())?
                    .with_context(|| format!("{name}: not found in archive"))?;
                a.copy_to(&e, &mut out)
                    .with_context(|| format!("{name}: cannot read"))?;
            }
        }
        Cmd::Convert {
            archive,
            output,
            to,
            level,
            block_size,
            threads,
            force,
        } => {
            use tarloupe_core::convert::{self, ConvertOptions, Target};
            let target = match to {
                ConvertTo::Zstd => Target::Zstd,
                ConvertTo::Xz => Target::Xz,
            };
            let output = output.unwrap_or_else(|| convert::default_output(&archive, target));
            let start = Instant::now();
            let progress: tarloupe_core::Progress = if io::IsTerminal::is_terminal(&io::stderr()) {
                std::sync::Arc::new(|done, total| {
                    eprint!("\rtarloupe: converting... {}%", done * 100 / total.max(1));
                })
            } else {
                std::sync::Arc::new(|_, _| {})
            };
            let stats = convert::convert(
                &archive,
                &output,
                &ConvertOptions {
                    target,
                    level,
                    block_size: block_size.unwrap_or(convert::DEFAULT_BLOCK_SIZE),
                    threads,
                    overwrite: force,
                    progress: Some(progress),
                    cancel: None,
                },
            );
            if io::IsTerminal::is_terminal(&io::stderr()) {
                eprintln!();
            }
            let stats = stats.with_context(|| {
                format!(
                    "cannot convert {} to {}",
                    archive.display(),
                    output.display()
                )
            })?;
            eprintln!(
                "tarloupe: wrote {} ({} -> {}, tar {}) in {:.1?}",
                output.display(),
                human_size(stats.input_size),
                human_size(stats.output_size),
                human_size(stats.stream_size),
                start.elapsed()
            );
        }
        Cmd::Browse {
            archive,
            idle_timeout,
            no_window,
        } => browse(&archive, idle_timeout, no_window)?,
        Cmd::Explorer { action } => explorer(action)?,
        Cmd::Mount {
            archive,
            mount_point,
            idx,
        } => {
            // Say so before spending time on indexing.
            if !cfg!(windows) {
                anyhow::bail!(
                    "mount is only available on Windows (with WinFsp); use ls, cat or extract"
                );
            }
            let a = open(&archive, &idx, false)?;
            mount(a, mount_point)?;
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(windows)]
mod browse;

#[cfg(windows)]
fn browse(archive: &std::path::Path, idle: u64, no_window: bool) -> Result<()> {
    let opts = browse::Options {
        idle: std::time::Duration::from_secs(idle),
        open_window: !no_window,
        ..Default::default()
    };
    let stop = opts.stop.clone();
    ctrlc::set_handler(move || stop.store(true, std::sync::atomic::Ordering::Relaxed))?;
    browse::browse(archive, &opts, |point| {
        // Scripts (and the tests) wait for this line.
        println!("{}", point.to_string_lossy());
        let _ = io::stdout().flush();
    })
}

#[cfg(windows)]
fn explorer(action: ExplorerAction) -> Result<()> {
    match action {
        ExplorerAction::Install => {
            let exe = browse::windowless_exe()?;
            browse::install_verb(&exe)?;
            eprintln!(
                "tarloupe: added \"Browse with Tarloupe\" (runs {})",
                exe.display()
            );
            eprintln!("tarloupe: on Windows 11 it is under \"Show more options\" for now");
        }
        ExplorerAction::Uninstall => {
            browse::uninstall_verb()?;
            eprintln!("tarloupe: removed \"Browse with Tarloupe\"");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn browse(_archive: &std::path::Path, _idle: u64, _no_window: bool) -> Result<()> {
    anyhow::bail!("browse is only available on Windows (with WinFsp); use ls, cat or extract")
}

#[cfg(not(windows))]
fn explorer(_action: ExplorerAction) -> Result<()> {
    anyhow::bail!("the Explorer integration is only available on Windows")
}

#[cfg(windows)]
fn mount(a: Archive, point: Option<OsString>) -> Result<()> {
    use std::sync::{mpsc, Arc};
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("cannot install the Ctrl+C handler")?;
    let m = tarloupe_winfsp::mount(Arc::new(a), point.as_deref())?;
    // Scripts (and the tests) wait for this line before using the mount.
    println!("{}", m.mount_point().to_string_lossy());
    io::stdout().flush()?;
    eprintln!("tarloupe: mounted read-only; press Ctrl+C to unmount");
    let _ = rx.recv();
    drop(m);
    Ok(())
}

#[cfg(not(windows))]
fn mount(_a: Archive, _point: Option<OsString>) -> Result<()> {
    unreachable!("checked before opening the archive")
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if is_broken_pipe(&e) {
                return ExitCode::SUCCESS;
            }
            // Messages can contain names from the archive; never let them
            // send control sequences to the terminal.
            eprintln!("tarloupe: {}", escape_controls(&format!("{e:#}")));
            ExitCode::FAILURE
        }
    }
}

/// True when the error (or anything it wraps) is a closed output pipe,
/// e.g. `tarloupe cat ... | head`.
fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let io = cause.downcast_ref::<io::Error>().or_else(|| {
            match cause.downcast_ref::<tarloupe_core::Error>() {
                Some(tarloupe_core::Error::Io(io)) => Some(io),
                _ => None,
            }
        });
        io.is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
    })
}

/// Replaces control characters with visible escapes.
fn escape_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}
