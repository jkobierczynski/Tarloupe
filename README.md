# Tarloupe

A loupe for tarballs. Tarloupe makes huge `.tar`, `.tar.gz`, `.tar.xz`,
`.tar.bz2` and `.tar.zst` archives browsable on Windows without extracting
them. The first open builds a persistent seek index in one pass; every open
after that is instant, and any file inside can be read without decompressing
the archive from the start.

> **Status: milestone M3 done (all four codecs).** `.tar`, `.tar.gz`,
> `.tar.xz`, `.tar.zst` and `.tar.bz2`, through a command-line tool, and on
> Windows as a read-only drive from Explorer's "Browse with Tarloupe", with
> a tray icon and an installer that brings WinFsp. See the
> [roadmap](#roadmap) and the [releases](https://github.com/jkobierczynski/Tarloupe/releases).

## What works today

```console
$ tarloupe index big.tar.gz         # one pass; later opens reuse the index
built: 6882 members, 129 checkpoints, index C:\Users\me\AppData\Local\Tarloupe\index\9525….tli

$ tarloupe list -v big.tar.gz       # same output as `tar -tvf` (times in UTC)
$ tarloupe ls -l big.tar.gz some/dir   # browse one directory
$ tarloupe cat big.tar.gz some/dir/file.txt > file.txt
$ tarloupe extract big.tar.gz some/dir -C out   # extract a file or directory
$ tarloupe convert slow.tar.xz      # make big.tar.zst, which reads fast (see below)

PS> tarloupe mount big.tar.gz               # Windows: next free drive, e.g. Z:
PS> tarloupe mount big.tar.gz C:\mnt\big     # or a folder that does not exist yet
PS> tarloupe browse big.tar.gz              # open it in Explorer; unmounts when idle
PS> tarloupe explorer install               # add "Browse with Tarloupe" to the right-click menu
```

**Browse with Tarloupe.** The installer adds a right-click item for every
user; without it, `tarloupe explorer install` adds the same
item for `.tar`, `.tar.gz`, `.tar.xz`, `.tar.zst` and `.tar.bz2` files
(and `.tgz`, `.txz`, `.tzst`, `.tbz2`, `.tbz`), for the current user (no
administrator rights; `tarloupe explorer uninstall` removes it). On
Windows 11 it is under "Show more options" until the native menu entry
arrives in M5. It runs `tarloupew.exe`, which must sit next to
`tarloupe.exe`: it mounts the archive on a free drive letter, opens an
Explorer window there, and unmounts once every window and file on it has
been closed for two minutes (`tarloupe browse --idle-timeout` changes
that). Browsing the same archive again opens another window on the same
mount.

`tarloupew.exe` runs as one tray agent per Windows session: browsing
another archive hands it to the agent that is already running. Its tray
icon shows how far a big archive's first indexing has got and what is
mounted where; its menu opens or unmounts each archive, or unmounts all
and quits. The agent leaves once nothing is mounted.

`tarloupe mount` needs [WinFsp](https://winfsp.dev/rel/)
(`winget install WinFsp.WinFsp`). It prints the mount point, keeps the
archive mounted until Ctrl+C, and works with Explorer, `dir`, editors and
any other program. The volume is read-only. Windows needs every name in a
folder to be legal and unique ignoring case, so names are shown the way
Cygwin and WSL show them: `<>:"/\|?*`, control characters and a trailing
dot or space become look-alike characters (U+F000 + the ASCII code), bytes
that are not UTF-8 become U+F100 + the byte, `CON`, `NUL`, `COM1` and the
other device names get their first letter mapped, names over 255
characters are shortened with a hash, and names that differ only in case
get ` (2)`, ` (3)`. Hard links and symlinks show the file or folder they
point to; dangling links, devices and FIFOs are hidden.

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
* **zstd:** every frame starts fresh, so each frame boundary is a restart
  point that costs nothing to store. Files written as many frames read
  fast: the [seekable format](https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md)
  (its seek table is skipped like any skippable frame), `pzstd`, and
  `tarloupe convert`. Plain `zstd` writes one single frame, which works
  but reads from the start. Windows of up to 2 GiB (`--long=31`) are
  accepted.
* **xz:** every block starts fresh too. Multi-threaded `xz -T0` (24 MiB blocks at
  the default preset), `pixz` and `tarloupe convert` write many blocks and
  read fast; single-threaded `xz -T1` writes one block, which reads from
  the start.
  Concatenated streams, stream padding and every check type are supported,
  and each block's check is verified.
* **bzip2:** files made of many streams (`pbzip2`, `cat a.bz2 b.bz2`) restart
  at each stream. Inside one stream (plain `bzip2`, `lbzip2`), jumping to a
  block is M4, so for now those read from the start.
* **Slow archives:** when some data can only be reached by decoding more
  than 64 MiB, `tarloupe` says why after indexing and suggests `convert`.
* **`tarloupe convert`** recompresses any archive Tarloupe reads into
  seekable zstd (default, level 9, 8 MiB frames, compressed on every core)
  or multi-block xz (`--to xz`, preset 6, 8 MiB blocks); the tar inside
  stays byte for byte the same. The output goes next to the input
  (`big.tar.gz` -> `big.tar.zst`) unless `-o` says otherwise; `--level`,
  `--block-size` and `-T` tune it. Both formats are standard: `zstd -d`,
  `xz -d`, `tar` and 7-Zip read them as usual.

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
* zstd, xz and bzip2 archives, the same way: one frame and many frames,
  with and without a seek table; one block and many blocks; xz from
  `xz -T` and two concatenated `xz` streams with padding and different
  check types; one and many bzip2 streams, and `bzip2 -9` itself. Each must
  have restart points exactly when its format allows them, and `convert`
  must produce files that read the same.

`tarloupe extract` writes regular files and directories only (symlinks,
devices and FIFOs are skipped with a note), refuses member paths with `..`,
and never writes through an existing symlink in the destination.

On Windows, CI also checks that `browse` shares one mount per archive,
stays mounted while a file is open and unmounts once idle, that a second
`tarloupew` hands its archive to the running tray agent, which leaves once
everything has gone idle, and that the right-click item is added and
removed.

On Windows, CI installs WinFsp, mounts a gzip archive with the edge corpus
and checks through the real file system that every folder lists the same
names as the view, every file has the same bytes, lookups ignore case,
mapped names open, and writes, deletes and renames fail.

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
| `tarloupe-core` | Platform-neutral library: codecs (gzip, zstd, xz, bzip2), tar walker, SQLite index, random-access reads, conversion. No Windows code, so it builds and benchmarks on Linux. |
| `tarloupe-cli` | The `tarloupe` command: `index`, `list`, `ls`, `cat`, `extract`, `convert`, `mount`, `browse`, `explorer`; and `tarloupew`, the console-less helper Explorer runs. |
| `tarloupe-winfsp` | Read-only WinFsp file system over the core's Windows-shaped view (`tarloupe_core::vfs`). Empty on other platforms. |
| `tarloupe-corpus` | Synthetic corpus generator and its standalone tar writer. |

`installer/` builds the Windows installers with WiX (see Building).

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** Foundations | Workspace, corpus generator, tar walker, `.tar` index, CLI. *(done)* |
| **M1** gzip engine | Checkpoint index with saved deflate windows, random reads, chunk cache, benchmarks. *(done)* |
| **M2** Windows MVP | WinFsp read-only mount, "Browse with Tarloupe" in Explorer, tray app, installer. *(done, 0.1.0)* |
| **M3** More codecs | Seekable/multi-frame zstd, multi-block xz, bzip2 (sequential, multi-stream), sequential fallback, `convert`. *(done)* |
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

On Windows, building needs LLVM (`winget install LLVM.LLVM`, for the WinFsp
bindings) and the MSVC toolchain (it also compiles the bundled zstd and
liblzma C sources). The mount test is skipped when WinFsp is
not installed, unless `TARLOUPE_REQUIRE_WINFSP=1`.

### Windows installer

```powershell
./installer/build.ps1
```

builds two files into `dist\`: `tarloupe-setup-<version>-x64.exe`, which
installs WinFsp (unless some version is already installed) and then
Tarloupe, and `tarloupe-<version>-x64.msi`, Tarloupe alone. Both install for
all users into `Program Files\Tarloupe`, put it on `PATH` and add "Browse
with Tarloupe" to Explorer. Uninstalling removes Tarloupe and leaves WinFsp,
which other programs may use. The script needs the .NET SDK (8 or later) to
fetch the WiX toolset, and checks the WinFsp download's hash and signature.
CI builds both, installs the setup program on a clean runner, mounts an
archive with the bundled WinFsp and uninstalls again.

### Releasing

Bump `version` in `Cargo.toml`, add `release-notes/v<version>.md`, and push
a tag `v<version>`. The release workflow builds and tests the installers
from a clean tree and attaches them, with `SHA256SUMS.txt`, to a draft
release, which goes public when someone publishes it on GitHub.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
