<!--
SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# rvsz

Standalone writer and checker for RomVault "RVZSTD" and TorrentZip
structured zips. Output is byte-identical to what RomVault writes: same
entry order, headers, compressed data and comment. RVZSTD uses zstd frames
(libzstd 1.5.5, level 19, multithreaded framing) and the `RVZSTD-XXXXXXXX`
comment; TorrentZip uses raw deflate (zlib 1.3.2, level 9), the fixed
1996-12-24 23:32 timestamp and the `TORRENTZIPPED-XXXXXXXX` comment.

## Usage

```
rvsz pack <dir|zip>... [-o out]    dir -> <dir>.zip; zip is rewritten in place;
                                   with several inputs, -o is a directory
rvsz batch <folder>... [-o outdir] rewrite every *.zip under each folder, recursively;
                                   with -o, all folders merge into outdir
rvsz batch --pack-dirs <folder>... each subdir -> <name>.zip, top-level *.zip rewritten
rvsz verify [--deep] [--format rvzstd|trrntzip] <zip>...
                                   exit 1 if any zip is not valid RVZSTD or
                                   TorrentZip (picked by the zip comment), or
                                   not the --format given
rvsz extract <zip> [-o dir]

pack and batch options:
  --format rvzstd|trrntzip  output format (default: rvzstd)

global options:
  -j N           total zstd worker threads; with TorrentZip, zips packed
                 at once (default: CPU count)
  --max-mem MiB  memory budget for encoders (default: 3/4 of the free
                 memory; of all memory on macOS)
  --force        rewrite even if the target is already valid, and encode
                 every entry (no copy from the existing output)
  --trust-existing  copy entries from the existing output without
                 decoding them to check their CRCs
```

`batch` leaves out every name that starts with a dot, and the folders
Synology DSM keeps inside shared folders: `@eaDir`, `#recycle` and
`#snapshot`.

A zip that is already valid in the output format is skipped (an RVZSTD zip
is rewritten when `--format trrntzip` is asked, and the other way round).
Otherwise the existing output
(`<dir>.zip`, or the `-o` target of a zip) is skipped when it is valid and
holds the same names, sizes and CRCs. If only some entries differ (files
added, removed or changed), the unchanged ones are copied from it as they
are and only the rest is encoded; the result is the same bytes as a full
repack. Each copied entry is decoded on the way and its CRC checked (also
in the source zip, for a zip input): a bad one in the existing output is
encoded again, a bad one in the source zip is an error, as in a full
repack. `--trust-existing` skips these checks and copies the data as it is.
`--force` encodes every entry again. `verify` checks every byte outside the compressed data against the
layout this tool writes; `--deep` also decompresses, checks CRCs and
re-encodes each entry to compare the compressed bytes.

Input zips may use stored, deflate or zstd entries.

## Performance

Level 19 is required by the format, so packing is CPU bound. zstd splits
data into 32 MiB jobs, so one file uses at most size/32 MiB workers. `batch`
packs several zips at once to keep all threads busy, largest first. Each zip
gets workers in proportion to its share of the work not yet started, so a
large zip among small ones gets most of them; workers freed by a finished zip
go to the next ones. A zip keeps its worker count until it is done (zstd cannot
change it inside a frame). Peak memory is about
135 MiB plus 130 MiB per worker thread; `--max-mem` lowers concurrency to fit.

Deflate is single-threaded and much faster (about 70 MB/s per core for
incompressible data), so with `--format trrntzip` each zip uses one thread
and `-j N` packs N zips at once. A TorrentZip job needs only a few MiB, plus
140 MiB when it reads a zstd zip.

## Build

```
cargo build --release
# static binary (e.g. for Synology): needs the musl target and musl-gcc
cargo build --release --target x86_64-unknown-linux-musl
# Windows: needs the windows-gnu target and mingw-w64 gcc
cargo build --release --target x86_64-pc-windows-gnu
```

The Windows binary writes the same bytes. `extract` there works by path, not
relative to open directories, so it is weaker against links swapped in while
it runs, and it does not reject names Windows treats specially (`a:b`, `CON`,
a trailing dot or space).

zstd-sys is pinned to `=2.0.9` (libzstd 1.5.5): a newer libzstd can produce
different compressed bytes. RUSTSEC-2026-0410 affects only wasm32 builds.
libz-sys is pinned to `=1.1.31` and built from its bundled zlib 1.3.2, whose
level 9 output matches the managed zlib port RomVault uses for TorrentZip.

## Testing

`cargo test` runs unit and CLI tests. `tools/oracle` is a C# program that
writes RVZSTD or TorrentZip zips with RomVault's own `Compress` library, as a
reference for byte comparisons.

## License

rvsz is licensed under either of [MIT](LICENSES/MIT.txt) or
[Apache-2.0](LICENSES/Apache-2.0.txt) at your option. No code is copied
from RomVault: the format rules were reimplemented from its observable
behavior, the code page 437 table from the public standard mapping.

The repository is [REUSE](https://reuse.software/) compliant: every file
states its copyright and license.
