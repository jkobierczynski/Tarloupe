# Tarloupe

A loupe for tarballs. Tarloupe makes huge `.tar`, `.tar.gz`, `.tar.xz`,
`.tar.bz2` and `.tar.zst` archives browsable on Windows without extracting
them. The first open builds a persistent seek index in one pass; every open
after that is instant, and any file inside can be read without decompressing
the archive from the start.

> **Status: milestone M0 (foundations).** Plain `.tar` only, through a
> command-line tool. Compressed archives are detected and reported; gzip
> arrives in M1 and the Windows Explorer integration (WinFsp mount) in M2.
> See the [roadmap](#roadmap).

## What works today

```console
$ tarloupe index big.tar            # one pass; later opens reuse the index
built: 6882 members, index C:\Users\me\AppData\Local\Tarloupe\index\9525….tli

$ tarloupe list -v big.tar          # same output as `tar -tvf` (times in UTC)
$ tarloupe ls -l big.tar some/dir   # browse one directory
$ tarloupe cat big.tar some/dir/file.txt > file.txt
```

* **Tar formats:** v7, POSIX ustar, GNU (long names and links, base-256
  numbers for files over 8 GiB and large ids), POSIX pax (local and global
  extended headers), and GNU sparse files in every variant (old GNU, pax
  0.0, 0.1 and 1.0).
* **Tar semantics:** a member that appears twice resolves to the last one,
  missing parent directories are synthesised, `./` prefixes are normalised,
  and hard links and symlinks (including chains and links to directories)
  resolve to their data.
* **Index:** SQLite, stored in `%LOCALAPPDATA%\Tarloupe\index` (Linux:
  `~/.local/share/Tarloupe/index`; override with `TARLOUPE_INDEX_DIR` or
  `--index-dir`), or as a sidecar with `--index archive.tar.tli`. It is keyed
  by size, modification time and a hash of the first and last MiB, so a
  renamed archive keeps its index and a modified one is re-indexed.
* **Random access:** reads go straight to the byte range of a member, so a
  file in the middle of a 100 GB tar costs the same as one at the start.

## Correctness

The M0 exit criterion is that `tarloupe list -v` matches `tar -tvf` byte for
byte. The test suite checks this against GNU tar, and also extracts each
archive with GNU tar and compares every file with what Tarloupe reads:

* a generated edge-case corpus, written as GNU, pax and ustar archives (long
  paths, a 120-character name, Unicode, Windows-hostile names such as `a:b`,
  `CON` and `trailing.`, case collisions, duplicates, implicit directories,
  symlink chains, hard links, set-uid and sticky modes, numeric owners,
  dates before 1970 and after 2038, FIFOs and devices);
* archives created by GNU tar itself in `gnu`, `oldgnu` and `pax` formats,
  with sparse files in each sparse format version.

## Test corpus generator

`tarloupe-corpus` writes deterministic archives for tests and benchmarks:

```console
$ cargo run --release -p tarloupe-corpus -- edge -f pax -o edge.tar
$ cargo run --release -p tarloupe-corpus -- bulk --size 100G -o big.tar
$ cargo run --release -p tarloupe-corpus -- bulk --size 1G -o - | gzip > big.tar.gz
```

`bulk` streams files of mixed size (about 160 KiB on average, so 100 GiB is
roughly 650,000 files) with content that is partly compressible text and
partly random, so compressors behave realistically.

## Layout

| Crate | Purpose |
|---|---|
| `tarloupe-core` | Platform-neutral library: codec detection, tar walker, SQLite index, random-access reads. No Windows code, so it builds and benchmarks on Linux. |
| `tarloupe-cli` | The `tarloupe` command: `index`, `list`, `ls`, `cat`. |
| `tarloupe-corpus` | Synthetic corpus generator and its standalone tar writer. |

Later milestones add `tarloupe-winfsp` (read-only file system), the Explorer
verb and a tray app.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** Foundations | Workspace, corpus generator, tar walker, `.tar` index, CLI. *(this release)* |
| M1 gzip engine | Checkpoint index with saved deflate windows, random reads, chunk cache, benchmarks. |
| M2 Windows MVP | WinFsp read-only mount, "Browse with Tarloupe" in Explorer, tray app, installer. |
| M3 More codecs | Seekable/multi-frame zstd, multi-block xz, sequential fallback, `convert`. |
| M4 Speed | Parallel gzip first pass, parallel bzip2. |
| M5 Native feel | Shell namespace extension, Windows 11 context menu. |
| M6 Reach | Linux FUSE, ratarmount index import, nested archives. |

## Building

```console
cargo build --release
cargo test
```

The comparison tests need GNU tar on `PATH` (`tar` on Linux, or `gtar`);
they are skipped when it is missing.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
