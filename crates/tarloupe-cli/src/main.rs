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
}

fn open(archive: &PathBuf, idx: &IndexOpts, rebuild: bool) -> Result<Archive> {
    let opts = OpenOptions {
        index_path: idx.index.clone(),
        index_dir: idx.index_dir.clone(),
        rebuild,
        require_index: false,
        checkpoint_spacing: idx.checkpoint_spacing,
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
        let points: u64 = a
            .index()
            .meta("checkpoint_count")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let size = fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
        if a.codec() == tarloupe_core::Codec::Gzip && points <= 1 && size > 32 << 20 {
            eprintln!(
                "tarloupe: warning: this gzip file is one huge deflate block (as some \
                 fast compressors write), so every read decodes from the start. \
                 Recompressing it with gzip or pigz makes reads fast."
            );
        }
    }
    Ok(a)
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
    }
    out.flush()?;
    Ok(())
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
