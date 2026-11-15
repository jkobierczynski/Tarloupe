//! `tarloupe`: index, list and read huge tar archives without extracting them.

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
        path: String,
        /// Long listing.
        #[arg(short, long)]
        long: bool,
        #[command(flatten)]
        idx: IndexOpts,
    },
    /// Write the contents of one or more files to standard output.
    Cat {
        archive: PathBuf,
        #[arg(required = true)]
        paths: Vec<String>,
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
    }
    Ok(a)
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
            writeln!(
                out,
                "{status}: {members} members, index {}",
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
            for e in a.list_dir(path.as_bytes())? {
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
                        String::from_utf8_lossy(&e.uname).into_owned()
                    };
                    let group = if e.gname.is_empty() {
                        e.gid.to_string()
                    } else {
                        String::from_utf8_lossy(&e.gname).into_owned()
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
        Cmd::Cat {
            archive,
            paths,
            idx,
        } => {
            let a = open(&archive, &idx, false)?;
            for p in paths {
                let e = a
                    .lookup(p.as_bytes())?
                    .with_context(|| format!("{p}: not found in archive"))?;
                a.copy_to(&e, &mut out)
                    .with_context(|| format!("{p}: cannot read"))?;
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
            if let Some(io) = e.downcast_ref::<io::Error>() {
                if io.kind() == io::ErrorKind::BrokenPipe {
                    return ExitCode::SUCCESS;
                }
            }
            eprintln!("tarloupe: {e:#}");
            ExitCode::FAILURE
        }
    }
}
