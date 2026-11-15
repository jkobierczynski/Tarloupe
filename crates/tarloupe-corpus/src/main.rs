//! `tarloupe-corpus`: writes deterministic synthetic tar archives.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tarloupe_corpus::{write_bulk, write_edge, Format};

#[derive(Parser)]
#[command(
    name = "tarloupe-corpus",
    version,
    about = "Generate synthetic tar test corpora"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Small archive exercising tar and Windows edge cases.
    Edge {
        #[arg(short, long, value_enum, default_value = "gnu")]
        format: Format,
        /// Output file, or `-` for standard output.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Large archive of many files, e.g. `--size 1G` or `--size 100G`.
    Bulk {
        #[arg(short, long, value_enum, default_value = "gnu")]
        format: Format,
        #[arg(short, long, value_parser = parse_size)]
        size: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mult)
        .map_err(|e| format!("invalid size {s:?}: {e}"))
}

fn open_output(path: &PathBuf) -> Result<Box<dyn Write>> {
    Ok(if path.as_os_str() == "-" {
        Box::new(BufWriter::with_capacity(1 << 20, io::stdout().lock()))
    } else {
        let f = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        Box::new(BufWriter::with_capacity(1 << 20, f))
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Edge { format, output } => {
            for s in write_edge(open_output(&output)?, format)? {
                eprintln!("skipped (not representable in {format:?}): {s}");
            }
        }
        Cmd::Bulk {
            format,
            size,
            seed,
            output,
        } => {
            if size == 0 {
                bail!("--size must be positive");
            }
            let n = write_bulk(open_output(&output)?, format, size, seed)?;
            eprintln!("wrote {n} files");
        }
    }
    Ok(())
}
