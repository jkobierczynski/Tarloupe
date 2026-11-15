# Tarloupe

A loupe for tarballs. Tarloupe makes huge `.tar`, `.tar.gz`, `.tar.xz`,
`.tar.bz2` and `.tar.zst` archives browsable on Windows without extracting
them. The first open builds a persistent seek index in one pass; every open
after that is instant, and any file inside can be read without decompressing
the archive from the start.

> **Status: milestone M1 (gzip engine).** Plain `.tar` and `.tar.gz`,
> through a command-line tool. Other compressed archives are detected and
> reported; the Windows Explorer integration (WinFsp mount) arrives in M2
> and xz/zstd/bzip2 in M3. See the [roadmap](#roadmap).

## What works today

```console
$ tarloupe index big.tar.gz         # one pass; later opens reuse the index
built: 6882 members, 129 checkpoints, index C:\Users\me\AppData\Local\Tarloupe\index\9525….tli

$ tarloupe list -v big.tar.gz       # same output as `tar -tvf` (times in UTC)
$ tarloupe ls -l big.tar.gz some/dir   # browse one directory
$ tarloupe cat big.tar.gz some/dir/file.txt > file.txt
$ tarloupe extract big.tar.gz some/dir -C out   # extract a file or directory
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
* **Random access:** for a plain `.tar`, reads go straight to the byte range
  of a member, so a file in the middle of a 100 GB tar costs the same as one
  at the start.
* **gzip:** the indexing pass also saves a decoder restart point (the bit
  position plus the 32 KiB deflate window, compressed) at the first block
  boundary after every 8 MiB of output. A read decodes from the nearest
  restart point before it, so it costs at most about 8 MiB of decoding plus
  the file itself, wherever the file is. Decoded 1 MiB chunks are cached
  (64 MiB) and decoders are kept to continue sequential reads.
  Multi-member files (`cat a.gz b.gz`, pigz, BGZF) are supported; data after
  the last member that is not another member is ignored, as `gzip` does.
  `--checkpoint-spacing 1M` trades a larger index for faster small reads.
  A file written as one single deflate block (zlib-ng/zlib-rs level 1 do
  this) has no restart points; it still works, but every read decodes from
  the start, and `tarloupe index` warns about it.

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
* gzip archives: GNU gzip at levels 1 and 9, zlib levels 0 (stored
  blocks), 1, 2 and 9, many-member files (BGZF-like and uneven member
  sizes) and zero padding after the end. Every member and 1,500 random
  reads per variant are compared with the same archive uncompressed, with
  restart points every 64 KiB so that reads cross many of them. Truncated
  and corrupted files must fail.

`tarloupe extract` writes regular files and directories only (symlinks,
devices and FIFOs are skipped with a note), refuses member paths with `..`,
and never writes through an existing symlink in the destination.

## Benchmark: one file out of a 4 GiB `.tar.gz`

`scripts/bench-gzip.sh 4G /tmp/bench` generates a 4 GiB bulk corpus
(26,281 files), compresses it with `gzip -6` (1.8 GB), and times reading
single files. Linux, 4 vCPUs, warm page cache, 7-Zip 23.01, GNU tar 1.35:

| Step | Time |
|---|---|
| `gzip -dc` of the whole archive (baseline) | 18.6 s |
| `tarloupe index` (first open, one pass) | 6.4 s |
| `tarloupe ls` (reopen with the index) | 0.005 s |
| Last file in the archive (0.6 MB): `tarloupe cat` | **0.024 s** |
| ... GNU `tar -xzO --occurrence` | 20.5 s |
| ... 7-Zip (`7z x` the gzip, then the tar) | 18.2 s |
| An 11 MB file in the middle: `tarloupe cat` | **0.031 s** |
| ... GNU `tar -xzO --occurrence` | 10.1 s |
| ... 7-Zip | 18.1 s |
| Index size | 14 MB (0.8% of the archive) |

The plan's target for a random 10 MB read is under 300 ms; it takes 31 ms
here, and the cost does not grow with the archive size. The first index
decodes the archive once (zlib-rs, single-threaded); a parallel first pass
is M4.

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
| `tarloupe-cli` | The `tarloupe` command: `index`, `list`, `ls`, `cat`, `extract`. |
| `tarloupe-corpus` | Synthetic corpus generator and its standalone tar writer. |

Later milestones add `tarloupe-winfsp` (read-only file system), the Explorer
verb and a tray app.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** Foundations | Workspace, corpus generator, tar walker, `.tar` index, CLI. *(done)* |
| **M1** gzip engine | Checkpoint index with saved deflate windows, random reads, chunk cache, benchmarks. *(this release)* |
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
