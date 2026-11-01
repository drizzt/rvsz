// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! RVZSTD zip writer, following RomVault `ZipFileData.cs`, `ZipExtraFieldWrite.cs`,
//! `ZipOpenWrite.cs` and `ZipEndOfCentralDir.cs`. The header builders are
//! also used by the validator, which compares a file to what would be written.

use crate::cp437;
use crate::sys::{self, FileExt};
use std::fs::{self, File};
use std::io::{self, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_ZSTD: u16 = 93;
const MAX32: u64 = 0xFFFF_FFFF;
pub const COMMENT_ID: &str = "RVZSTD-";

pub struct Meta<'a> {
    pub name: &'a str,
    pub unc: u64,
    pub comp: u64,
    pub crc: u32,
}

fn method(unc: u64) -> u16 {
    if unc == 0 { METHOD_STORED } else { METHOD_ZSTD }
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
    let meth = method(m.unc);
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
    h.extend_from_slice(&[0, 0, 0, 0]); // time, date
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
    let meth = method(m.unc);
    let mut h = Vec::with_capacity(46 + name.len() + 28);
    h.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes()); // made by
    h.extend_from_slice(&version_needed(meth, zip64).to_le_bytes());
    h.extend_from_slice(&flags(utf8).to_le_bytes());
    h.extend_from_slice(&meth.to_le_bytes());
    h.extend_from_slice(&[0, 0, 0, 0]);
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

/// Zip64 records (if needed) and EOCD with the `RVZSTD-XXXXXXXX` comment.
pub fn end_records(count: u64, cd_start: u64, cd: &[u8]) -> Vec<u8> {
    let cd_size = cd.len() as u64;
    let comment = format!("{COMMENT_ID}{:08X}", crc32fast::hash(cd));
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

/// Temp files being written, removed by `remove_temp_files` on a signal.
static TEMP_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn track_temp(p: &Path) {
    TEMP_FILES.lock().unwrap().push(p.to_path_buf());
}

pub fn untrack_temp(p: &Path) {
    let mut v = TEMP_FILES.lock().unwrap();
    if let Some(i) = v.iter().position(|x| x == p) {
        v.swap_remove(i);
    }
}

/// Removes every tracked temp file. The lock is kept, so no writer can
/// register a new one before the process exits.
pub fn remove_temp_files() {
    for p in TEMP_FILES.lock().unwrap().iter() {
        let _ = fs::remove_file(p);
    }
}

/// Like `remove_temp_files`, but gives up if the list is locked, so a panic
/// hook running on a thread that holds the lock cannot deadlock.
pub fn try_remove_temp_files() {
    if let Ok(v) = TEMP_FILES.try_lock() {
        for p in v.iter() {
            let _ = fs::remove_file(p);
        }
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
        // tracked before it exists: a signal right after the open still finds it
        track_temp(&tmp);
        // read access is needed to move data for the zip64 fix-up
        let mut o = fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        sys::temp_mode(&mut o, old.is_some());
        let r = o.open(&tmp);
        let f = match r {
            Ok(f) => f,
            Err(e) => {
                untrack_temp(&tmp);
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
    pub fn create(dest: &Path) -> io::Result<Self> {
        let (f, tmp) = create_temp(dest)?;
        Ok(ZipWriter {
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
    /// `body` is not called.
    pub fn add(
        &mut self,
        name: &str,
        unc: u64,
        body: impl FnOnce(&mut dyn Write) -> io::Result<(u64, u32)>,
    ) -> io::Result<()> {
        let offset = self.pos;
        let pre = local_header(
            &Meta {
                name,
                unc,
                comp: 0,
                crc: 0,
            },
            false,
        );
        self.w.write_all(&pre)?;
        let (comp, crc) = if unc == 0 {
            (0, 0)
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

    pub fn finish(mut self) -> io::Result<()> {
        let end = end_records(self.count, self.pos, &self.cd);
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
