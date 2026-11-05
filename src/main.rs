// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

mod codec;
mod color;
mod cp437;
mod deflate;
mod entries;
mod progress;
mod rvzstd;
mod sort;
mod sys;
mod validate;
mod zipread;
mod zipwrite;

use clap::{Args, Parser, Subcommand};
use codec::Codec;
use color::{GREEN, GREY, RED, SKY};
use entries::{Entry, Source};
use progress::{Bar, Progress};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use zipread::{Archive, pump};
use zipwrite::{Format, ZipWriter};

#[derive(Parser)]
#[command(
    version,
    about = "Write and check RomVault RVZSTD or TorrentZip zip files"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Resources {
    /// Total zstd worker threads; with TorrentZip, zips packed at once
    /// (default: number of CPUs)
    #[arg(short = 'j', long)]
    jobs: Option<usize>,
    /// Memory budget in MiB for concurrent encoders and decoders (default: 3/4 of MemAvailable)
    #[arg(long)]
    max_mem: Option<u64>,
}

impl Resources {
    /// Total workers and memory budget in MiB.
    fn budget(&self) -> (usize, u64) {
        let total = self
            .jobs
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
        let mem = self.max_mem.unwrap_or_else(|| mem_available_mib() * 3 / 4);
        (total, mem)
    }
}

#[derive(Args)]
struct PackOpts {
    #[command(flatten)]
    res: Resources,
    /// Rewrite even if the target is already valid in the output format,
    /// and encode every entry (no copy from the existing output)
    #[arg(long)]
    force: bool,
    /// Copy entries from the existing output without decoding them to
    /// check their CRCs
    #[arg(long)]
    trust_existing: bool,
    /// Print each entry as it is packed (on stderr)
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pack a directory into a zip, or rewrite a zip (in place by default).
    /// With several inputs, -o is a directory for all the outputs
    Pack {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Output format
        #[arg(long, value_enum, default_value_t = Format::Rvzstd)]
        format: Format,
        #[command(flatten)]
        opts: PackOpts,
    },
    /// Rewrite every *.zip under each folder, recursively
    Batch {
        #[arg(required = true)]
        folders: Vec<PathBuf>,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Instead pack each subdirectory into <name>.zip and rewrite only
        /// the *.zip files directly in the folder
        #[arg(long)]
        pack_dirs: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = Format::Rvzstd)]
        format: Format,
        #[command(flatten)]
        opts: PackOpts,
    },
    /// Check zips; exit status is non-zero if any is not valid RVZSTD or
    /// TorrentZip (told apart by the zip comment), or not the --format given
    Verify {
        /// Also decompress, check CRCs and re-encode to compare the data
        #[arg(long)]
        deep: bool,
        /// Require this format (default: any, told apart by the zip comment)
        #[arg(long, value_enum)]
        format: Option<Format>,
        #[arg(required = true)]
        zips: Vec<PathBuf>,
        /// zstd worker threads for --deep re-encoding (default: number of CPUs)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        /// Memory budget in MiB for --deep (default: 3/4 of MemAvailable)
        #[arg(long)]
        max_mem: Option<u64>,
    },
    /// Extract a zip
    Extract {
        zip: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

/// Peak memory of one level 19 encoder with `workers` threads. Measured
/// peak RSS of `pack` on a 234 MB input: 264, 394, 623 MiB at 1, 2, 4 workers
/// (about 135 MiB shared plus 130 MiB per worker), plus some margin.
fn encoder_mem_mib(workers: usize) -> u64 {
    150 + 130 * workers as u64
}

/// Peak memory of one zstd decoder: a window of up to 128 MiB (windowLogMax
/// 27, set in rvzstd.rs) plus block and I/O buffers. A frame this tool
/// writes uses 8 MiB, but a hostile one may ask for the limit.
fn decoder_mem_mib() -> u64 {
    140
}

/// Peak memory of one TorrentZip job without a decoder: zlib at level 9
/// needs about 300 KiB, the rest is I/O buffers.
const DEFLATE_MEM_MIB: u64 = 16;

fn mem_available_mib() -> u64 {
    sys::mem_available_mib().unwrap_or(1024)
}

enum Outcome {
    Written,
    Skipped,
}

/// Where data for each entry comes from.
enum Input {
    Dir,
    Zip(Archive),
}

fn source_reader<'a>(input: &'a Input, e: &Entry) -> io::Result<Box<dyn Read + 'a>> {
    match (&e.src, input) {
        // `pump` reads in 1 MiB chunks, so a BufReader would add nothing
        (Source::File(p), _) => Ok(Box::new(open_source(p, e.size)?)),
        (Source::Zip(i, _), Input::Zip(a)) => a.open_entry(*i),
        _ => unreachable!(),
    }
}

/// Opens a file found by the directory walk. If it was swapped for a
/// symlink, a FIFO or another file since, the open or the checks fail.
/// A parent directory swapped for a symlink is still
/// followed; covering that would need an openat walk like `extract` uses.
fn open_source(p: &Path, size: u64) -> io::Result<File> {
    let f =
        sys::open_source_file(p).map_err(|e| io::Error::other(format!("{}: {e}", p.display())))?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() != size {
        return Err(io::Error::other(format!(
            "{}: changed while packing",
            p.display()
        )));
    }
    Ok(f)
}

/// An earlier output and, for each entry of the new list, the index of
/// the old entry it can copy unchanged.
type Reuse = (Archive, Vec<Option<usize>>);

#[allow(clippy::too_many_arguments)]
fn write_zip(
    input: Input,
    old: Option<Reuse>,
    list: &[Entry],
    dest: &Path,
    fmt: Format,
    enc: &mut Codec,
    opts: &PackOpts,
    bar: &Bar,
) -> io::Result<()> {
    let verbose = opts.verbose;
    let mut w = ZipWriter::create(dest, fmt)?;
    // batch runs several zips at once: the prefix tells their lines apart
    let tag = dest.file_name().unwrap_or_default().to_string_lossy();
    let reused = |i: usize| old.as_ref().and_then(|(a, m)| m[i].map(|j| (a, j)));
    // copied entries are decoded to check them, much faster than an
    // encode: the bar counts them too, the summary only the encode work
    bar.set_total(list.iter().map(|e| e.size).sum());
    enc.set_meter(bar.counter());
    if verbose {
        let (copied, total) = list.iter().enumerate().fold((0, 0), |(c, t), (i, e)| {
            if reused(i).is_some() {
                (c + 1, t)
            } else {
                (c, t + e.size)
            }
        });
        bar.line(
            true,
            &format!(
                "packing {}: {} entries ({copied} copied), {} to encode",
                dest.display(),
                list.len(),
                human(total)
            ),
        );
    }
    for (i, e) in list.iter().enumerate() {
        let entry_line = |copied: bool| {
            if verbose {
                bar.line(
                    true,
                    &format!(
                        "{tag} [{}/{}] {} ({}){}",
                        i + 1,
                        list.len(),
                        e.name,
                        human(e.size),
                        if copied { " copied" } else { "" }
                    ),
                );
            }
        };
        // a copied entry is printed once its check passed
        if let Some((a, j)) = reused(i) {
            if copy_entry(&mut w, &input, e, a, j, !opts.trust_existing)? {
                enc.skip(e.size);
                entry_line(true);
                continue;
            }
            bar.line(
                true,
                &format!(
                    "{tag}: {}: bad data in the existing output, encoding it again",
                    e.name
                ),
            );
        }
        entry_line(false);
        let r = w.add(&e.name, e.size, |sink| {
            let src = source_reader(&input, e)?;
            let (n, crc) = pump(src, e.size, |b| enc.write(b, sink))
                .map_err(|err| io::Error::other(format!("{}: {err}", e.name)))?;
            if let Source::Zip(_, want) = e.src
                && crc != want
            {
                return Err(io::Error::other(format!(
                    "{}: CRC error in source zip",
                    e.name
                )));
            }
            if n == e.size {
                enc.finish(sink)?;
            }
            Ok((n, crc))
        });
        if let Err(err) = r {
            enc.reset();
            return Err(err);
        }
    }
    // an in-place rewrite replaces the source zip, any rewrite the old
    // output: Windows refuses to rename over a file that is still open
    drop(input);
    drop(old);
    w.finish()
}

/// Copies entry `j` of the earlier output `a` as entry `e`. With `check`,
/// the source zip entry (for a zip input) and the copied data are decoded
/// first: false if the copied data is bad, and nothing written.
fn copy_entry(
    w: &mut ZipWriter,
    input: &Input,
    e: &Entry,
    a: &Archive,
    j: usize,
    check: bool,
) -> io::Result<bool> {
    let z = &a.entries[j];
    if check
        && let Source::Zip(_, want) = e.src
        && pump(source_reader(input, e)?, e.size, |_| Ok(()))
            .map_err(|err| io::Error::other(format!("{}: {err}", e.name)))?
            .1
            != want
    {
        return Err(io::Error::other(format!(
            "{}: CRC error in source zip",
            e.name
        )));
    }
    w.add_raw(&e.name, z.unc, z.crc, z.comp, |sink| {
        let mut raw = a.raw(j)?;
        if !check {
            io::copy(&mut raw, sink)?;
            return Ok(true);
        }
        copy_checked(&e.name, z, &mut raw, sink)
    })
}

/// Copies the compressed data of `z` from `raw` to `sink` and decodes it on
/// the way. False if it does not decode to `z.unc` bytes with CRC `z.crc`.
fn copy_checked(
    name: &str,
    z: &zipread::Entry,
    raw: &mut impl Read,
    sink: &mut dyn io::Write,
) -> io::Result<bool> {
    /// Passes on to `sink` every byte read from `src`. Keeps the first I/O
    /// error, which the decoder would report as bad data.
    struct Tee<'a, R> {
        src: &'a mut R,
        sink: &'a mut dyn io::Write,
        err: Option<io::Error>,
    }
    impl<R: Read> Read for Tee<'_, R> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let r = self
                .src
                .read(b)
                .and_then(|n| self.sink.write_all(&b[..n]).map(|()| n));
            if let Err(e) = &r
                && self.err.is_none()
            {
                self.err = Some(io::Error::new(e.kind(), e.to_string()));
            }
            r
        }
    }
    let mut tee = Tee {
        src: raw,
        sink,
        err: None,
    };
    let got = zipread::decoder(
        name,
        z.method,
        io::BufReader::with_capacity(1 << 20, &mut tee),
    )
    .and_then(|d| pump(d, z.unc, |_| Ok(())));
    if let Some(e) = tee.err.take() {
        return Err(e);
    }
    // a decoder may stop before the end of bad data: copy the rest too
    io::copy(&mut tee, &mut io::sink())?;
    Ok(matches!(got, Ok((n, crc)) if n == z.unc && crc == z.crc))
}

/// Entries of `zip` (an earlier output) that `list` can copy unchanged:
/// for each entry of `list`, the index of the entry of `zip` with the same
/// name, size and CRC (read from the files, or taken from the source zip).
/// None if `zip` is not valid in format `fmt`, as its compressed data may
/// then differ from what this tool writes.
fn reusable(zip: &Path, fmt: Format, input: &Input, list: &[Entry]) -> io::Result<Option<Reuse>> {
    let Ok(a) = Archive::open(zip) else {
        return Ok(None);
    };
    if validate::check(&a, fmt, None).is_err() {
        return Ok(None);
    }
    let by_name: HashMap<&str, usize> = a
        .entries
        .iter()
        .enumerate()
        .map(|(i, z)| (z.name.as_str(), i))
        .collect();
    let mut map = Vec::with_capacity(list.len());
    for e in list {
        let hit = match by_name.get(e.name.as_str()) {
            Some(&j) if a.entries[j].unc == e.size => {
                let crc = a.entries[j].crc;
                let same = match e.src {
                    Source::File(_) => {
                        pump(source_reader(input, e)?, e.size, |_| Ok(()))? == (e.size, crc)
                    }
                    Source::Zip(_, c) => c == crc,
                    Source::Dir => true,
                };
                same.then_some(j)
            }
            _ => None,
        };
        map.push(hit);
    }
    Ok(Some((a, map)))
}

fn human(n: u64) -> String {
    if n < 1024 * 1024 {
        format!("{n} bytes")
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

#[allow(clippy::too_many_arguments)]
fn pack(
    input: &Path,
    dest: &Path,
    fmt: Format,
    opts: &PackOpts,
    enc: &mut Codec,
    bar: &Bar,
    started: &dyn Fn(),
) -> io::Result<Outcome> {
    let (force, verbose) = (opts.force, opts.verbose);
    let (src, list) = if input.is_dir() {
        // the output would be packed into itself by the next run
        let parent = dest.parent().filter(|p| !p.as_os_str().is_empty());
        if let (Ok(d), Ok(i)) = (
            fs::canonicalize(parent.unwrap_or(Path::new("."))),
            fs::canonicalize(input),
        ) && d.starts_with(&i)
        {
            return Err(io::Error::other("output is inside the packed directory"));
        }
        (Input::Dir, entries::from_dir(input)?)
    } else {
        let a = Archive::open(input)?;
        if !force && validate::check(&a, fmt, None).is_ok() {
            if !same_file(input, dest) {
                copy_atomic(input, dest)?;
                return Ok(Outcome::Written);
            }
            return Ok(Outcome::Skipped);
        }
        let list = entries::from_zip(&a)?;
        (Input::Zip(a), list)
    };
    // an output from an earlier run (e.g. `batch -o`) needs no new encode,
    // or only for the entries that changed
    let mut old = None;
    if !force && dest.exists() {
        if verbose {
            // reads each source file whose name and size match an old
            // entry, to compare CRCs
            bar.line(
                true,
                &format!(
                    "{}{}",
                    color::word(true, GREY, "checking", 9),
                    dest.display()
                ),
            );
        }
        if let Some((a, map)) = reusable(dest, fmt, &src, &list)? {
            if a.entries.len() == list.len() && map.iter().all(Option::is_some) {
                return Ok(Outcome::Skipped);
            }
            old = Some((a, map));
        }
    }
    started();
    write_zip(src, old, &list, dest, fmt, enc, opts, bar)?;
    Ok(Outcome::Written)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn copy_atomic(src: &Path, dest: &Path) -> io::Result<()> {
    let (mut f, tmp) = zipwrite::create_temp(dest)?;
    let r = File::open(src)
        .and_then(|mut s| io::copy(&mut s, &mut f))
        .and_then(|_| f.sync_all())
        .and_then(|_| fs::rename(&tmp, dest));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    zipwrite::untrack_temp(&tmp);
    r
}

fn default_pack_output(input: &Path) -> PathBuf {
    if input.is_dir() {
        // "." and ".." have no name of their own: use the real directory's,
        // else "." would give "..zip" inside the directory being packed
        let dir = match input.file_name() {
            Some(_) => input.to_path_buf(),
            None => fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf()),
        };
        // collecting components drops a trailing '/'
        let mut s = dir.components().collect::<PathBuf>().into_os_string();
        s.push(".zip");
        PathBuf::from(s)
    } else {
        input.to_path_buf()
    }
}

fn batch_jobs(folder: &Path, outdir: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut dirs = std::collections::BTreeMap::new();
    let mut zips = Vec::new();
    for d in fs::read_dir(folder)? {
        let d = d?;
        let p = d.path();
        // kept as OsString: a lossy name would point the job at another file
        let fname = d.file_name();
        let b = fname.as_encoded_bytes();
        if b.starts_with(b".") {
            continue;
        }
        let ft = d.file_type()?;
        // a link here would be packed or rewritten, reaching outside the folder
        if ft.is_symlink() && (p.is_dir() || is_zip_name(b)) {
            return Err(entries::symlink_error(&p));
        }
        if ft.is_dir() {
            let mut target = fname;
            target.push(".zip");
            dirs.insert(target, p);
        } else if is_zip_name(b) && ft.is_file() {
            zips.push((fname, p));
        }
    }
    let mut jobs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (target, p) in zips {
        if dirs.contains_key(&target) {
            // X.zip next to X/ is X's own output from an earlier run: the
            // directory job rewrites it (or skips it if it is up to date).
            if same_file(&p, &outdir.join(&target)) {
                continue;
            }
            return Err(io::Error::other(format!(
                "both a directory and a zip map to {}",
                target.to_string_lossy()
            )));
        }
        jobs.push((p, outdir.join(target)));
    }
    jobs.extend(dirs.into_iter().map(|(target, p)| (p, outdir.join(target))));
    jobs.sort();
    Ok(jobs)
}

/// Error if two jobs write the same output: several folders merged into one
/// `-o` directory, the same folder given twice, or one folder inside another.
/// Every output parent exists by now (created while collecting the jobs).
fn check_dest_collisions(jobs: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    let mut seen = std::collections::HashMap::new();
    for (src, dest) in jobs {
        // a bare file name has an empty parent, which canonicalize rejects
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let key = (
            fs::canonicalize(parent)?,
            dest.file_name().map(OsStr::to_owned),
        );
        if let Some(prev) = seen.insert(key, src) {
            return Err(io::Error::other(format!(
                "{} and {} both map to {}",
                prev.display(),
                src.display(),
                dest.display()
            )));
        }
    }
    Ok(())
}

fn is_zip_name(b: &[u8]) -> bool {
    b.len() >= 4 && b[b.len() - 4..].eq_ignore_ascii_case(b".zip")
}

/// Every *.zip under `dir`, at any depth, with its output path under `outdir`
/// (the same relative path). `root_out` is the top output directory, skipped
/// if found inside the input tree. A symlinked directory or zip is an error:
/// following it would read or rewrite files outside the tree.
fn zip_jobs(
    dir: &Path,
    outdir: &Path,
    root_out: &Path,
    jobs: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    for d in fs::read_dir(dir)? {
        let d = d?;
        let p = d.path();
        let fname = d.file_name();
        let b = fname.as_encoded_bytes();
        if b.starts_with(b".") {
            continue;
        }
        let ft = d.file_type()?;
        if ft.is_symlink() && (p.is_dir() || is_zip_name(b)) {
            return Err(entries::symlink_error(&p));
        } else if ft.is_dir() {
            // `batch src -o src/out`: do not take earlier outputs as inputs
            if !same_file(&p, root_out) {
                zip_jobs(&p, &outdir.join(&fname), root_out, jobs)?;
            }
        } else if is_zip_name(b) && ft.is_file() {
            fs::create_dir_all(outdir)?;
            jobs.push((p, outdir.join(fname)));
        }
    }
    Ok(())
}

/// Most workers (at least 1, at most `total`) whose encoder fits in `mem_mib`.
fn fit_workers(total: usize, mem_mib: u64) -> usize {
    let fit =
        mem_mib.saturating_sub(encoder_mem_mib(0)) / (encoder_mem_mib(1) - encoder_mem_mib(0));
    fit.clamp(1, total.max(1) as u64) as usize
}

/// One batch job and its size, the estimate of its encode time: the zip's
/// file size, or the total size of the files of a packed directory.
struct Job {
    src: PathBuf,
    dest: PathBuf,
    size: u64,
    /// Needs a zstd decoder: reads a source zip, or checks entries copied
    /// from the existing output. Set by `run_jobs`.
    decode: bool,
}

fn tree_size(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    fs::read_dir(p).map_or(0, |rd| rd.flatten().map(|d| tree_size(&d.path())).sum())
}

/// Hands out jobs, largest first, with their worker count. zstd cannot
/// change the worker count inside a frame, so the count is fixed when a job
/// starts: its share of the free workers is its share of the work not yet
/// started (a large file next to small ones gets many, equal files get one
/// each, and the last jobs take the workers that earlier ones gave back).
/// Running encoders and decoders stay within `budget` MiB. TorrentZip
/// deflate is single-threaded: there each job takes exactly one worker.
struct Sched {
    fmt: Format,
    jobs: Vec<Job>,
    free: usize,
    left: u64,
    used: u64,
    running: usize,
    budget: u64,
}

impl Sched {
    fn job_mem(&self, w: usize, decode: bool) -> u64 {
        let enc = match self.fmt {
            Format::Rvzstd => encoder_mem_mib(w),
            Format::Trrntzip => DEFLATE_MEM_MIB,
        };
        enc + if decode { decoder_mem_mib() } else { 0 }
    }

    /// Next job and its workers, or None if the queue is empty. Some(None):
    /// no memory or worker free yet, wait for a running job to end.
    fn take(&mut self) -> Option<Option<(Job, usize)>> {
        let job = self.jobs.last()?;
        let avail = match self.fmt {
            Format::Trrntzip => {
                // alone: run even if the budget is too small, as `pack` does
                let fits =
                    self.running == 0 || self.used + self.job_mem(1, job.decode) <= self.budget;
                if fits { self.free.min(1) } else { 0 }
            }
            Format::Rvzstd => {
                let dec = self.job_mem(0, job.decode) - encoder_mem_mib(0);
                if self.running == 0 {
                    // alone: run even if the budget is too small, as `pack` does
                    fit_workers(self.free, self.budget.saturating_sub(dec))
                } else {
                    let mem = self
                        .budget
                        .saturating_sub(self.used + encoder_mem_mib(0) + dec);
                    self.free
                        .min((mem / (encoder_mem_mib(1) - encoder_mem_mib(0))) as usize)
                }
            }
        };
        if avail == 0 {
            return Some(None);
        }
        let share = (job.size as u128 * self.free as u128).div_ceil(self.left.max(1) as u128);
        let w = (share.min(avail as u128) as usize).max(1);
        let job = self.jobs.pop()?;
        self.free -= w;
        self.left -= job.size;
        self.used += self.job_mem(w, job.decode);
        self.running += 1;
        Some(Some((job, w)))
    }

    fn release(&mut self, w: usize, decode: bool) {
        self.free += w;
        self.used -= self.job_mem(w, decode);
        self.running -= 1;
    }
}

fn run_jobs(
    mut jobs: Vec<Job>,
    fmt: Format,
    total: usize,
    budget: u64,
    opts: &PackOpts,
    batch: bool,
) -> bool {
    for j in &mut jobs {
        // the dest check is made once here: the job creates dest, and the
        // scheduler must give back the same memory it took
        j.decode = !j.src.is_dir() || (!opts.trust_existing && !opts.force && j.dest.exists());
    }
    jobs.sort_by_key(|j| j.size);
    let threads = total.clamp(1, jobs.len().max(1));
    let left = jobs.iter().map(|j| j.size).sum();
    let count = jobs.len();
    let sched = Mutex::new(Sched {
        fmt,
        left,
        jobs,
        free: total.max(1),
        used: 0,
        running: 0,
        budget,
    });
    let freed = Condvar::new();
    let failed = AtomicBool::new(false);
    Progress::run(count, left, |p| {
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let mut st = sched.lock().unwrap();
                        let (job, w) = loop {
                            match st.take() {
                                None => return,
                                Some(Some(t)) => break t,
                                Some(None) => st = freed.wait(st).unwrap(),
                            }
                        };
                        drop(st);
                        // gives back the workers even if `pack` panics, else the
                        // other threads would wait on `freed` forever
                        struct Release<'a>(&'a Mutex<Sched>, &'a Condvar, usize, bool);
                        impl Drop for Release<'_> {
                            fn drop(&mut self) {
                                let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
                                st.release(self.2, self.3);
                                self.1.notify_all();
                            }
                        }
                        let _release = Release(&sched, &freed, w, job.decode);
                        let name = job.dest.file_name().unwrap_or_default().to_string_lossy();
                        let bar = p.start(&name, job.size);
                        let started = || {
                            if batch && !p.on() {
                                // a big zip at level 19 can take an hour: show it started
                                eprintln!(
                                    "{}{} ({w} worker{})",
                                    color::word(true, SKY, "packing", 8),
                                    job.dest.display(),
                                    if w == 1 { "" } else { "s" }
                                );
                            }
                        };
                        let r = Codec::new(fmt, w as u32).and_then(|mut enc| {
                            pack(&job.src, &job.dest, fmt, opts, &mut enc, &bar, &started)
                        });
                        drop(bar);
                        match r {
                            Ok(Outcome::Written) => p.line(
                                false,
                                &format!(
                                    "{}{}",
                                    color::word(false, GREEN, "packed", 8),
                                    job.dest.display()
                                ),
                            ),
                            Ok(Outcome::Skipped) => p.line(
                                false,
                                &format!(
                                    "{}{} (already valid)",
                                    color::word(false, GREY, "skipped", 8),
                                    job.dest.display()
                                ),
                            ),
                            Err(e) => {
                                p.line(true, &format!("{}{}: {e}", err_word(), job.src.display()));
                                failed.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        })
    });
    !failed.load(Ordering::Relaxed)
}

fn safe_path(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Opens or creates every directory of `rel` below `root` and, for a file
/// entry, creates the file. Each step is relative to the directory opened
/// before it, so a symlink swapped in meanwhile is never followed (on Unix;
/// see `sys::Dir` for Windows). For a
/// file, also returns its parent (None for `root`) and its name in there.
fn create_at<'a>(
    root: &sys::Dir,
    rel: &'a Path,
    is_dir: bool,
) -> io::Result<Option<(Option<sys::Dir>, &'a OsStr, File)>> {
    let parts: Vec<&OsStr> = rel.iter().collect();
    let (last, parents) = parts.split_last().expect("safe_path is not empty");
    let mut dir: Option<sys::Dir> = None;
    for p in parents {
        dir = Some(sys::open_dir_at(dir.as_ref().unwrap_or(root), p)?);
    }
    let parent = dir.as_ref().unwrap_or(root);
    if is_dir {
        sys::open_dir_at(parent, last)?;
        return Ok(None);
    }
    let f = sys::create_file_at(parent, last)?;
    Ok(Some((dir, last, f)))
}

/// Output path of every entry, checked before anything is written: each
/// must be safe, and no file may sit where another entry needs a directory
/// (`e` next to `e/` or `e/x`), which would fail halfway through.
fn extract_paths(a: &Archive) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(a.entries.len());
    let mut files = HashSet::new();
    let mut dirs = HashSet::new();
    for e in &a.entries {
        let name = e.name.replace('\\', "/");
        let Some(rel) = safe_path(&name) else {
            return Err(io::Error::other(format!(
                "unsafe path in zip: {:?}",
                e.name
            )));
        };
        if e.is_dir() {
            dirs.insert(rel.clone());
        } else {
            files.insert(rel.clone());
        }
        dirs.extend(rel.ancestors().skip(1).map(Path::to_path_buf));
        paths.push(rel);
    }
    if let Some(p) = files.iter().find(|p| dirs.contains(*p)) {
        return Err(io::Error::other(format!(
            "{}: both a file and a directory in zip",
            p.display()
        )));
    }
    Ok(paths)
}

fn extract(zip: &Path, outdir: &Path, bar: &Bar) -> io::Result<()> {
    let a = Archive::open(zip)?;
    let paths = extract_paths(&a)?;
    bar.set_total(a.entries.iter().map(|e| e.unc).sum());
    fs::create_dir_all(outdir)?;
    let root = sys::open_root(outdir)?;
    for (i, (e, rel)) in a.entries.iter().zip(&paths).enumerate() {
        let out = create_at(&root, rel, e.is_dir())
            .map_err(|err| io::Error::other(format!("{}: {err}", e.name)))?;
        let Some((dir, last, out)) = out else {
            continue;
        };
        let r = (|| {
            let mut f = io::BufWriter::with_capacity(1 << 20, out);
            let (n, crc) = pump(a.open_entry(i)?, e.unc, |b| {
                bar.add(b.len());
                io::Write::write_all(&mut f, b)
            })?;
            io::Write::flush(&mut f)?;
            if n != e.unc || crc != e.crc {
                return Err(io::Error::other("size or CRC mismatch"));
            }
            Ok(())
        })();
        if let Err(err) = r {
            // by name in the directory opened before, never through a path
            let _ = sys::unlink_at(dir.as_ref().unwrap_or(&root), last);
            return Err(io::Error::other(format!("{}: {err}", e.name)));
        }
    }
    Ok(())
}

/// `error: ` prefix for stderr lines.
fn err_word() -> String {
    color::word(true, RED, "error:", 7)
}

fn main() {
    let cli = Cli::parse();
    sys::cleanup_on_signal();
    // panic = "abort" skips Drop, which would remove the temp file; the hook
    // still runs before the abort
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        zipwrite::try_remove_temp_files();
    }));
    let ok = match cli.cmd {
        Cmd::Pack {
            mut inputs,
            output,
            format,
            opts,
        } if inputs.len() == 1 => {
            let (total, mem) = opts.res.budget();
            let input = inputs.remove(0);
            let dest = output.unwrap_or_else(|| default_pack_output(&input));
            // alone, the job gets every worker that fits whatever its size
            let job = Job {
                size: 1,
                src: input,
                dest,
                decode: false,
            };
            run_jobs(vec![job], format, total, mem, &opts, false)
        }
        Cmd::Pack {
            inputs,
            output,
            format,
            opts,
        } => {
            let (total, mem) = opts.res.budget();
            let r = (|| {
                if let Some(out) = &output {
                    fs::create_dir_all(out)?;
                }
                let jobs: Vec<_> = inputs
                    .into_iter()
                    .map(|src| {
                        let def = default_pack_output(&src);
                        let dest = match &output {
                            Some(out) => out.join(def.file_name().unwrap_or_default()),
                            None => def,
                        };
                        (src, dest)
                    })
                    .collect();
                check_dest_collisions(&jobs)?;
                Ok::<_, io::Error>(jobs)
            })();
            match r {
                Ok(jobs) => {
                    let jobs = jobs
                        .into_iter()
                        .map(|(src, dest)| Job {
                            size: tree_size(&src),
                            src,
                            dest,
                            decode: false,
                        })
                        .collect();
                    run_jobs(jobs, format, total, mem, &opts, true)
                }
                Err(e) => {
                    eprintln!("{}{e}", err_word());
                    false
                }
            }
        }
        Cmd::Batch {
            folders,
            output,
            pack_dirs,
            format,
            opts,
        } => {
            let (total, mem) = opts.res.budget();
            let r = (|| {
                let mut jobs = Vec::new();
                for folder in &folders {
                    let outdir = output.clone().unwrap_or_else(|| folder.clone());
                    fs::create_dir_all(&outdir)?;
                    if pack_dirs {
                        jobs.extend(batch_jobs(folder, &outdir)?);
                    } else {
                        zip_jobs(folder, &outdir, &outdir, &mut jobs)?;
                    }
                }
                check_dest_collisions(&jobs)?;
                jobs.sort();
                Ok::<_, io::Error>(jobs)
            })();
            match r {
                Ok(jobs) => {
                    let jobs = jobs
                        .into_iter()
                        .map(|(src, dest)| Job {
                            size: tree_size(&src),
                            src,
                            dest,
                            decode: false,
                        })
                        .collect();
                    run_jobs(jobs, format, total, mem, &opts, true)
                }
                Err(e) => {
                    eprintln!("{}{e}", err_word());
                    false
                }
            }
        }
        Cmd::Verify {
            deep,
            format,
            zips,
            jobs,
            max_mem,
        } => {
            let (total, mem) = Resources { jobs, max_mem }.budget();
            let workers = fit_workers(total, mem.saturating_sub(decoder_mem_mib())) as u32;
            // made on first use, one per format
            let mut encs: [Option<Codec>; 2] = [None, None];
            let sizes: Vec<u64> = zips
                .iter()
                .map(|z| fs::metadata(z).map_or(0, |m| m.len()))
                .collect();
            Progress::run(zips.len(), sizes.iter().sum(), |p| {
                let mut ok = true;
                let on = color::enabled(false);
                for (z, size) in zips.iter().zip(sizes) {
                    let bar = p.start(&z.file_name().unwrap_or_default().to_string_lossy(), size);
                    let r = Archive::open(z).map_err(|e| e.to_string()).and_then(|a| {
                        let Some(fmt) = Format::detect(&a.comment) else {
                            return Err("no RVZSTD or TorrentZip comment".into());
                        };
                        if let Some(want) = format.filter(|&w| w != fmt) {
                            return Err(format!("{}, expected {}", fmt.name(), want.name()));
                        }
                        let enc = if deep {
                            let slot = &mut encs[fmt as usize];
                            if slot.is_none() {
                                *slot = Some(Codec::new(fmt, workers).map_err(|e| e.to_string())?);
                            }
                            slot.as_mut().map(|c| (c, &bar))
                        } else {
                            None
                        };
                        validate::check(&a, fmt, enc)
                            .map(|()| fmt)
                            .map_err(|e| format!("{}: {e}", fmt.name()))
                    });
                    drop(bar);
                    match r {
                        Ok(fmt) => p.line(
                            false,
                            &format!("{}: {}", z.display(), color::paint(on, GREEN, fmt.name())),
                        ),
                        Err(e) => {
                            p.line(
                                false,
                                &format!(
                                    "{}: {}: {e}",
                                    z.display(),
                                    color::paint(on, RED, "invalid")
                                ),
                            );
                            ok = false;
                        }
                    }
                }
                ok
            })
        }
        Cmd::Extract { zip, output } => {
            let out = output.unwrap_or_else(|| PathBuf::from("."));
            let size = fs::metadata(&zip).map_or(0, |m| m.len());
            Progress::run(1, size, |p| {
                let bar = p.start(&zip.file_name().unwrap_or_default().to_string_lossy(), size);
                let r = extract(&zip, &out, &bar);
                drop(bar);
                match r {
                    Ok(()) => true,
                    Err(e) => {
                        p.line(true, &format!("{}{}: {e}", err_word(), zip.display()));
                        false
                    }
                }
            })
        }
    };
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(sizes: &[u64], free: usize, budget: u64) -> Sched {
        let mut jobs: Vec<Job> = sizes
            .iter()
            .map(|&size| Job {
                src: PathBuf::from("a.zip"),
                dest: PathBuf::from("b.zip"),
                size,
                decode: true,
            })
            .collect();
        jobs.sort_by_key(|j| j.size);
        Sched {
            fmt: Format::Rvzstd,
            left: sizes.iter().sum(),
            jobs,
            free,
            used: 0,
            running: 0,
            budget,
        }
    }

    fn workers(s: &mut Sched) -> Option<usize> {
        s.take().unwrap().map(|(_, w)| w)
    }

    #[test]
    fn sched_shares_workers_by_size() {
        // equal jobs: one worker each
        let mut s = sched(&[100; 30], 24, u64::MAX);
        for _ in 0..24 {
            assert_eq!(workers(&mut s), Some(1));
        }
        assert_eq!(workers(&mut s), None);
        // a large job next to small ones gets its share first
        let mut s = sched(&[4000, 50, 50, 50, 50, 50], 24, u64::MAX);
        assert_eq!(workers(&mut s), Some(23));
        assert_eq!(workers(&mut s), Some(1));
        assert_eq!(workers(&mut s), None);
        // workers given back go to the jobs still queued
        s.release(23, true);
        assert_eq!(workers(&mut s), Some(6));
        // memory: a job costs 150 + 130 per worker + 140, the first takes the
        // 4 workers that fit, the next waits until it ends
        let mut s = sched(&[100; 4], 24, 2 * 420);
        assert_eq!(workers(&mut s), Some(4));
        assert_eq!(workers(&mut s), None);
        let mut s = sched(&[100; 30], 24, 2 * 420);
        s.free = 1;
        s.running = 1;
        s.used = 420;
        assert_eq!(workers(&mut s), Some(1));
        assert_eq!(workers(&mut s), None);
        // alone, a job runs even if the budget is too small
        let mut s = sched(&[100], 24, 0);
        assert_eq!(workers(&mut s), Some(1));
    }

    #[test]
    fn sched_trrntzip_one_worker_per_job() {
        let mut s = sched(&[4000, 50, 50, 50], 3, u64::MAX);
        s.fmt = Format::Trrntzip;
        for _ in 0..3 {
            assert_eq!(workers(&mut s), Some(1));
        }
        assert_eq!(workers(&mut s), None);
        s.release(1, true);
        assert_eq!(workers(&mut s), Some(1));
        // memory: 16 + 140 per job reading a zip
        let mut s = sched(&[100; 4], 24, 2 * 156);
        s.fmt = Format::Trrntzip;
        assert_eq!(workers(&mut s), Some(1));
        assert_eq!(workers(&mut s), Some(1));
        assert_eq!(workers(&mut s), None);
        // alone, a job runs even if the budget is too small
        let mut s = sched(&[100], 24, 0);
        s.fmt = Format::Trrntzip;
        assert_eq!(workers(&mut s), Some(1));
    }

    #[test]
    fn open_source_refuses_swapped_files() {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/unit_open_source");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let f = d.join("f");
        fs::write(&f, b"abc").unwrap();
        assert!(open_source(&f, 3).is_ok());
        assert!(open_source(&f, 2).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let link = d.join("link");
            std::os::unix::fs::symlink(&f, &link).unwrap();
            assert!(open_source(&link, 3).is_err());
            // a FIFO with no writer would block a plain open forever
            let fifo = d.join("fifo");
            let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: NUL-terminated path
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            assert!(open_source(&fifo, 0).is_err());
        }
    }
}
