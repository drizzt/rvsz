// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! RVZSTD and TorrentZip writer, following RomVault `ZipFileData.cs`, `ZipExtraFieldWrite.cs`,
//! `ZipOpenWrite.cs` and `ZipEndOfCentralDir.cs`. The header builders are
//! also used by the validator, which compares a file to what would be written.

use crate::cp437;
use crate::sys::{self, FileExt};
use std::fs::{self, File};
use std::io::{self, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATE: u16 = 8;
pub const METHOD_ZSTD: u16 = 93;
const MAX32: u64 = 0xFFFF_FFFF;

/// Output structure: RomVault `ZipStructure.ZipZSTD` or `ZipTrrnt`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Format {
    Rvzstd,
    Trrntzip,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Rvzstd => "RVZSTD",
            Format::Trrntzip => "TorrentZip",
        }
    }

    /// Every TorrentZip entry is deflated, also empty ones (data `03 00`).
    pub fn method(self, unc: u64) -> u16 {
        match self {
            Format::Rvzstd if unc == 0 => METHOD_STORED,
            Format::Rvzstd => METHOD_ZSTD,
            Format::Trrntzip => METHOD_DEFLATE,
        }
    }

    /// Data of an empty entry: none for RVZSTD (stored), the deflate
    /// stream `03 00` for TorrentZip (RomVault `LocalFileAddZeroLengthFile`).
    pub fn empty_data(self) -> &'static [u8] {
        match self {
            Format::Rvzstd => &[],
            Format::Trrntzip => &[3, 0],
        }
    }

    /// DOS time and date as written: 0 for RVZSTD, 1996-12-24 23:32 for
    /// TorrentZip (time 0xBC00, date 0x2198).
    fn time_date(self) -> [u8; 4] {
        match self {
            Format::Rvzstd => [0; 4],
            Format::Trrntzip => [0x00, 0xBC, 0x98, 0x21],
        }
    }

    pub fn comment_id(self) -> &'static str {
        match self {
            Format::Rvzstd => "RVZSTD-",
            Format::Trrntzip => "TORRENTZIPPED-",
        }
    }

    /// Format named by the zip comment prefix, if any.
    pub fn detect(comment: &[u8]) -> Option<Format> {
        [Format::Rvzstd, Format::Trrntzip]
            .into_iter()
            .find(|f| comment.starts_with(f.comment_id().as_bytes()))
    }
}

pub struct Meta<'a> {
    pub fmt: Format,
    pub name: &'a str,
    pub unc: u64,
    pub comp: u64,
    pub crc: u32,
}

fn flags(utf8: bool) -> u16 {
    if utf8 { 0x0802 } else { 0x0002 }
}

fn version_needed(method: u16, zip64: bool) -> u16 {
    if method == METHOD_ZSTD {
        63
    } else if zip64 {
        45
    } else {
        20
    }
}

/// Local header. `final_` false gives the placeholder written before the data
/// (sizes unknown, extra chosen from `unc` only); true gives the patched one.
pub fn local_header(m: &Meta, final_: bool) -> Vec<u8> {
    let (name, utf8) = cp437::name_bytes(m.name);
    let pre64 = m.unc >= MAX32;
    let zip64 = if final_ {
        pre64 || m.comp >= MAX32
    } else {
        pre64
    };
    let meth = m.fmt.method(m.unc);
    // When only the compressed size overflows, RomVault fixes the header up
    // afterwards and forces version 45 (FixFileForZip64).
    let ver = if zip64 && !pre64 {
        45
    } else {
        version_needed(meth, zip64)
    };
    let mut h = Vec::with_capacity(30 + name.len() + 20);
    h.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
    h.extend_from_slice(&ver.to_le_bytes());
    h.extend_from_slice(&flags(utf8).to_le_bytes());
    h.extend_from_slice(&meth.to_le_bytes());
    h.extend_from_slice(&m.fmt.time_date());
    if final_ {
        h.extend_from_slice(&m.crc.to_le_bytes());
        let (c, u) = if zip64 {
            (MAX32, MAX32)
        } else {
            (m.comp, m.unc)
        };
        h.extend_from_slice(&(c as u32).to_le_bytes());
        h.extend_from_slice(&(u as u32).to_le_bytes());
    } else {
        h.extend_from_slice(&[0xFF; 12]);
    }
    h.extend_from_slice(&(name.len() as u16).to_le_bytes());
    h.extend_from_slice(&(if zip64 { 20u16 } else { 0 }).to_le_bytes());
    h.extend_from_slice(&name);
    if zip64 {
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        // placeholder header holds comp 0, as RomVault writes it before the data
        h.extend_from_slice(&m.unc.to_le_bytes());
        h.extend_from_slice(&(if final_ { m.comp } else { 0 }).to_le_bytes());
    }
    h
}

pub fn central_header(m: &Meta, offset: u64) -> Vec<u8> {
    let (name, utf8) = cp437::name_bytes(m.name);
    let mut extra = Vec::new();
    let mut field = |v: u64| -> u32 {
        if v >= MAX32 {
            extra.extend_from_slice(&v.to_le_bytes());
            MAX32 as u32
        } else {
            v as u32
        }
    };
    let u = field(m.unc);
    let c = field(m.comp);
    let o = field(offset);
    let zip64 = !extra.is_empty();
    let meth = m.fmt.method(m.unc);
    let mut h = Vec::with_capacity(46 + name.len() + 28);
    h.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes()); // made by
    h.extend_from_slice(&version_needed(meth, zip64).to_le_bytes());
    h.extend_from_slice(&flags(utf8).to_le_bytes());
    h.extend_from_slice(&meth.to_le_bytes());
    h.extend_from_slice(&m.fmt.time_date());
    h.extend_from_slice(&m.crc.to_le_bytes());
    h.extend_from_slice(&c.to_le_bytes());
    h.extend_from_slice(&u.to_le_bytes());
    h.extend_from_slice(&(name.len() as u16).to_le_bytes());
    let xlen = if zip64 { extra.len() + 4 } else { 0 };
    h.extend_from_slice(&(xlen as u16).to_le_bytes());
    h.extend_from_slice(&[0; 10]); // comment len, disk, internal, external attrs
    h.extend_from_slice(&o.to_le_bytes());
    h.extend_from_slice(&name);
    if zip64 {
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&extra);
    }
    h
}

/// Zip64 records (if needed) and EOCD with the `RVZSTD-XXXXXXXX` or
/// `TORRENTZIPPED-XXXXXXXX` comment.
pub fn end_records(fmt: Format, count: u64, cd_start: u64, cd: &[u8]) -> Vec<u8> {
    let cd_size = cd.len() as u64;
    let comment = format!("{}{:08X}", fmt.comment_id(), crc32fast::hash(cd));
    let mut h = Vec::new();
    if cd_start >= MAX32 || cd_size >= MAX32 || count >= 0xFFFF {
        let eocd64 = cd_start + cd_size;
        h.extend_from_slice(&0x0606_4B50u32.to_le_bytes());
        h.extend_from_slice(&44u64.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&[0; 8]);
        h.extend_from_slice(&count.to_le_bytes());
        h.extend_from_slice(&count.to_le_bytes());
        h.extend_from_slice(&cd_size.to_le_bytes());
        h.extend_from_slice(&cd_start.to_le_bytes());
        h.extend_from_slice(&0x0706_4B50u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&eocd64.to_le_bytes());
        h.extend_from_slice(&1u32.to_le_bytes());
    }
    let n = count.min(0xFFFF) as u16;
    h.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&n.to_le_bytes());
    h.extend_from_slice(&n.to_le_bytes());
    h.extend_from_slice(&(cd_size.min(MAX32) as u32).to_le_bytes());
    h.extend_from_slice(&(cd_start.min(MAX32) as u32).to_le_bytes());
    h.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    h.extend_from_slice(comment.as_bytes());
    h
}

/// Counts bytes passing through to the file.
struct Counter<'a> {
    w: &'a mut BufWriter<File>,
    n: u64,
}

impl Write for Counter<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.w.write(b)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Temp files being written, removed by `remove_temp_files_and_exit` on a
/// signal. A temp file is created and pushed under one lock, so it exists
/// only while it is in the list.
static TEMP_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn untrack_temp(p: &Path) {
    let mut v = TEMP_FILES.lock().unwrap();
    if let Some(i) = v.iter().position(|x| x == p) {
        v.swap_remove(i);
    }
}

/// Removes every tracked temp file, then exits with `code`. The lock is
/// held until the process is gone, so no writer can create a new temp file
/// during or after the cleanup. A poisoned lock is still used: the list is
/// valid, and giving up would leave every temp file behind.
pub fn remove_temp_files_and_exit(code: i32) -> ! {
    remove_all(&TEMP_FILES.lock().unwrap_or_else(PoisonError::into_inner));
    std::process::exit(code)
}

/// Removes every tracked temp file unless the list is locked, so a panic
/// hook running on a thread that holds the lock cannot deadlock.
pub fn try_remove_temp_files() {
    if let Ok(v) = TEMP_FILES.try_lock() {
        remove_all(&v);
    }
}

fn remove_all(v: &[PathBuf]) {
    for p in v {
        let _ = fs::remove_file(p);
    }
}

/// Creates and tracks a new temp file next to `dest`. `create_new` never
/// opens an existing name, so a symlink planted there is not followed. If
/// `dest` is a regular file, its owner (best effort) and mode carry over.
pub fn create_temp(dest: &Path) -> io::Result<(File, PathBuf)> {
    let dir = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let fname = dest
        .file_name()
        .ok_or_else(|| io::Error::other("bad output path"))?;
    let old = fs::symlink_metadata(dest).ok().filter(|m| m.is_file());
    for n in 0..100 {
        // not lossy: two non-UTF-8 names must not share one temp file
        let mut t = std::ffi::OsString::from(".");
        t.push(fname);
        t.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = dir.join(t);
        // read access is needed to move data for the zip64 fix-up
        let mut o = fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        sys::temp_mode(&mut o, old.is_some());
        // open and track under one lock: the signal cleanup either runs
        // before (and exits with the lock held, so this never runs) or
        // after, and then finds the file in the list
        let r = {
            let mut v = TEMP_FILES.lock().unwrap();
            o.open(&tmp).inspect(|_| v.push(tmp.clone()))
        };
        let f = match r {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e);
            }
        };
        if let Some(m) = &old
            && let Err(e) = sys::copy_owner_mode(&f, m)
        {
            let _ = fs::remove_file(&tmp);
            untrack_temp(&tmp);
            return Err(e);
        }
        return Ok((f, tmp));
    }
    Err(io::Error::other(format!(
        "no free temp file name in {}",
        dir.display()
    )))
}

pub struct ZipWriter {
    fmt: Format,
    w: BufWriter<File>,
    tmp: PathBuf,
    dest: PathBuf,
    pos: u64,
    cd: Vec<u8>,
    count: u64,
    done: bool,
}

impl ZipWriter {
    /// Writes to a temp file next to `dest`; `finish` renames it into place.
    pub fn create(dest: &Path, fmt: Format) -> io::Result<Self> {
        let (f, tmp) = create_temp(dest)?;
        Ok(ZipWriter {
            fmt,
            w: BufWriter::with_capacity(1 << 20, f),
            tmp,
            dest: dest.to_path_buf(),
            pos: 0,
            cd: Vec::new(),
            count: 0,
            done: false,
        })
    }

    /// Writes one entry. `body` gets a sink for compressed bytes and returns
    /// (uncompressed size, crc). For `unc == 0` (empty file or directory)
    /// `body` is not called: RVZSTD stores no data, TorrentZip the 2 bytes
    /// of an empty deflate stream.
    pub fn add(
        &mut self,
        name: &str,
        unc: u64,
        body: impl FnOnce(&mut dyn Write) -> io::Result<(u64, u32)>,
    ) -> io::Result<()> {
        let offset = self.pos;
        let pre = local_header(
            &Meta {
                fmt: self.fmt,
                name,
                unc,
                comp: 0,
                crc: 0,
            },
            false,
        );
        self.w.write_all(&pre)?;
        let (comp, crc) = if unc == 0 {
            let d = self.fmt.empty_data();
            self.w.write_all(d)?;
            (d.len() as u64, 0)
        } else {
            let mut c = Counter {
                w: &mut self.w,
                n: 0,
            };
            let (got, crc) = body(&mut c)?;
            if got != unc {
                return Err(io::Error::other(format!(
                    "{name}: size changed while reading ({unc} -> {got})"
                )));
            }
            (c.n, crc)
        };
        let m = Meta {
            fmt: self.fmt,
            name,
            unc,
            comp,
            crc,
        };
        let fin = local_header(&m, true);
        self.w.flush()?;
        self.pos = offset + fin.len() as u64 + comp;
        if fin.len() != pre.len() {
            // compressed size crossed 4 GiB: move the data down for the zip64 extra
            let start = offset + pre.len() as u64;
            shift(
                self.w.get_ref(),
                start,
                comp,
                (fin.len() - pre.len()) as u64,
            )?;
        }
        self.w.get_ref().write_all_at(&fin, offset)?;
        // positional writes move the cursor on Windows, and the shift above
        // leaves it wrong everywhere: put it back at the end of the entry
        self.w.seek(io::SeekFrom::Start(self.pos))?;
        self.cd.extend_from_slice(&central_header(&m, offset));
        self.count += 1;
        Ok(())
    }

    /// Writes one entry whose compressed data is already in this format,
    /// e.g. copied from an older zip of the same layout. `body` writes the
    /// `comp` bytes and returns false if they turn out bad; the entry is
    /// then taken back and false returned, so that `add` can write it.
    /// The result is what `add` writes for the same data.
    pub fn add_raw(
        &mut self,
        name: &str,
        unc: u64,
        crc: u32,
        comp: u64,
        body: impl FnOnce(&mut dyn Write) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let offset = self.pos;
        let m = Meta {
            fmt: self.fmt,
            name,
            unc,
            comp,
            crc,
        };
        let h = local_header(&m, true);
        self.w.write_all(&h)?;
        let mut c = Counter {
            w: &mut self.w,
            n: 0,
        };
        let good = body(&mut c).map_err(|e| io::Error::other(format!("{name}: {e}")))?;
        if c.n != comp {
            return Err(io::Error::other(format!("{name}: short read from old zip")));
        }
        if !good {
            self.w.flush()?;
            self.w.get_ref().set_len(offset)?;
            self.w.seek(io::SeekFrom::Start(offset))?;
            return Ok(false);
        }
        self.pos = offset + h.len() as u64 + comp;
        self.cd.extend_from_slice(&central_header(&m, offset));
        self.count += 1;
        Ok(true)
    }

    pub fn finish(mut self) -> io::Result<()> {
        let end = end_records(self.fmt, self.count, self.pos, &self.cd);
        self.w.write_all(&std::mem::take(&mut self.cd))?;
        self.w.write_all(&end)?;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        fs::rename(&self.tmp, &self.dest)?;
        untrack_temp(&self.tmp);
        self.done = true;
        Ok(())
    }
}

/// Moves `len` bytes at `start` forward by `by`, copying from the end.
fn shift(f: &File, start: u64, len: u64, by: u64) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut end = start + len;
    while end > start {
        let n = (end - start).min(buf.len() as u64) as usize;
        end -= n as u64;
        f.read_exact_at(&mut buf[..n], end)?;
        f.write_all_at(&buf[..n], end + by)?;
    }
    Ok(())
}

impl Drop for ZipWriter {
    fn drop(&mut self) {
        if !self.done {
            let _ = fs::remove_file(&self.tmp);
            untrack_temp(&self.tmp);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn temp_file_skips_planted_symlink() {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp/unit_temp_symlink");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let victim = d.join("victim");
        fs::write(&victim, b"SECRET").unwrap();
        let link = d.join(format!(".o.zip.{}.0.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let (mut f, tmp) = create_temp(&d.join("o.zip")).unwrap();
        f.write_all(b"zip").unwrap();
        assert_eq!(tmp, d.join(format!(".o.zip.{}.1.tmp", std::process::id())));
        assert_eq!(fs::read(&victim).unwrap(), b"SECRET");
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        untrack_temp(&tmp);
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;

    fn tz(unc: u64, comp: u64) -> Meta<'static> {
        Meta {
            fmt: Format::Trrntzip,
            name: "a",
            unc,
            comp,
            crc: 0x1234_5678,
        }
    }

    #[test]
    fn trrntzip_local_header() {
        let h = local_header(&tz(5, 7), true);
        let want: &[u8] = &[
            0x50, 0x4B, 0x03, 0x04, 20, 0, 2, 0, 8, 0, 0x00, 0xBC, 0x98, 0x21, 0x78, 0x56, 0x34,
            0x12, 7, 0, 0, 0, 5, 0, 0, 0, 1, 0, 0, 0, b'a',
        ];
        assert_eq!(h, want);
        // empty entries are deflated too
        assert_eq!(&local_header(&tz(0, 2), true)[8..10], [8, 0]);
        // unc needs zip64 from the start: version 45, extra with both sizes
        let h = local_header(&tz(MAX32, MAX32 + 9), true);
        assert_eq!(&h[4..6], [45, 0]);
        assert_eq!(&h[18..26], [0xFF; 8]);
        assert_eq!(&h[28..30], [20, 0]);
        assert_eq!(&h[31..35], [1, 0, 16, 0]);
        assert_eq!(&h[35..43], MAX32.to_le_bytes());
        assert_eq!(&h[43..51], (MAX32 + 9).to_le_bytes());
        // only comp overflows: placeholder has no extra, final one does
        let m = tz(MAX32 - 1, MAX32);
        assert_eq!(local_header(&m, false).len(), 31);
        let h = local_header(&m, true);
        assert_eq!((&h[4..6], h.len()), (&[45, 0][..], 51));
    }

    #[test]
    fn trrntzip_central_and_end() {
        let h = central_header(&tz(5, 7), 0);
        assert_eq!(&h[4..8], [0, 0, 20, 0]);
        assert_eq!(&h[10..16], [8, 0, 0x00, 0xBC, 0x98, 0x21]);
        // only the offset needs zip64: version 45, extra holds the offset
        let h = central_header(&tz(5, 7), MAX32);
        assert_eq!(&h[6..8], [45, 0]);
        assert_eq!(&h[30..32], [12, 0]);
        assert_eq!(&h[47..59], [1, 0, 8, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        let e = end_records(Format::Trrntzip, 1, 0, b"x");
        let c = format!("TORRENTZIPPED-{:08X}", crc32fast::hash(b"x"));
        assert_eq!(&e[20..22], (c.len() as u16).to_le_bytes());
        assert_eq!(&e[22..], c.as_bytes());
    }

    #[test]
    fn detect_by_comment() {
        assert_eq!(Format::detect(b"RVZSTD-00000000"), Some(Format::Rvzstd));
        assert_eq!(Format::detect(b"TORRENTZIPPED-0"), Some(Format::Trrntzip));
        assert_eq!(Format::detect(b"TORRENT"), None);
    }
}
