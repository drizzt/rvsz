// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal zip reader: EOCD (with zip64), central directory, and decoders
//! for stored, deflate and zstd entries.

use crate::sys::FileExt;
use crate::{cp437, rvzstd};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

fn bad<T>(msg: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, msg.into()))
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub struct Entry {
    pub name: String,
    pub flags: u16,
    pub method: u16,
    pub crc: u32,
    pub comp: u64,
    pub unc: u64,
    pub offset: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/') || self.name.ends_with('\\')
    }
}

pub struct Archive {
    pub file: File,
    pub len: u64,
    pub entries: Vec<Entry>,
    pub cd_start: u64,
    pub cd_size: u64,
    /// EOCD comment (cut short if the file ends inside it).
    pub comment: Vec<u8>,
}

/// Reads `len` bytes from `pos` with positional reads.
#[derive(Clone)]
pub struct Range<'a> {
    f: &'a File,
    pos: u64,
    end: u64,
}

impl Read for Range<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.end - self.pos).min(buf.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        let n = self.f.read_at(&mut buf[..n], self.pos)?;
        if n == 0 {
            return bad("unexpected end of file");
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Archive {
    pub fn open(path: &Path) -> io::Result<Self> {
        // opening a FIFO with no writer would block forever; non-blocking
        // does nothing for a regular file, the only kind accepted
        let file = crate::sys::open_nonblock(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return bad("not a regular file");
        }
        let len = meta.len();
        let tail_len = len.min(22 + 0xFFFF + 20) as usize;
        let mut tail = vec![0; tail_len];
        file.read_exact_at(&mut tail, len - tail_len as u64)?;
        let eocd = (0..tail_len.saturating_sub(21))
            .rev()
            .find(|&i| {
                u32_at(&tail, i) == 0x0605_4B50
                    && i + 22 + u16_at(&tail, i + 20) as usize == tail_len
            })
            .or_else(|| {
                (0..tail_len.saturating_sub(21))
                    .rev()
                    .find(|&i| u32_at(&tail, i) == 0x0605_4B50)
            });
        let Some(e) = eocd else {
            return bad("not a zip file (no end of central directory)");
        };
        let clen = u16_at(&tail, e + 20) as usize;
        let comment = tail[(e + 22).min(tail_len)..(e + 22 + clen).min(tail_len)].to_vec();
        let mut count = u16_at(&tail, e + 10) as u64;
        let mut cd_size = u32_at(&tail, e + 12) as u64;
        let mut cd_start = u32_at(&tail, e + 16) as u64;
        if e >= 20 && u32_at(&tail, e - 20) == 0x0706_4B50 {
            let off = u64_at(&tail, e - 12);
            let mut z = [0u8; 56];
            if off.checked_add(56).is_none_or(|x| x > len) {
                return bad("bad zip64 locator");
            }
            file.read_exact_at(&mut z, off)?;
            if u32_at(&z, 0) != 0x0606_4B50 {
                return bad("bad zip64 end of central directory");
            }
            count = u64_at(&z, 32);
            cd_size = u64_at(&z, 40);
            cd_start = u64_at(&z, 48);
        }
        if cd_start.checked_add(cd_size).is_none_or(|x| x > len) {
            return bad("central directory out of range");
        }
        let mut cd = vec![0; cd_size as usize];
        file.read_exact_at(&mut cd, cd_start)?;
        let mut entries = Vec::with_capacity(count.min(1 << 20) as usize);
        let mut p = 0usize;
        for _ in 0..count {
            if p + 46 > cd.len() || u32_at(&cd, p) != 0x0201_4B50 {
                return bad("bad central directory header");
            }
            let flags = u16_at(&cd, p + 8);
            let method = u16_at(&cd, p + 10);
            let crc = u32_at(&cd, p + 16);
            let mut comp = u32_at(&cd, p + 20) as u64;
            let mut unc = u32_at(&cd, p + 24) as u64;
            let nlen = u16_at(&cd, p + 28) as usize;
            let xlen = u16_at(&cd, p + 30) as usize;
            let clen = u16_at(&cd, p + 32) as usize;
            let mut offset = u32_at(&cd, p + 42) as u64;
            let end = p + 46 + nlen + xlen + clen;
            if end > cd.len() {
                return bad("bad central directory header");
            }
            let raw = &cd[p + 46..p + 46 + nlen];
            let name = if flags & 0x800 != 0 {
                String::from_utf8_lossy(raw).into_owned()
            } else {
                cp437::decode(raw)
            };
            let mut x = &cd[p + 46 + nlen..p + 46 + nlen + xlen];
            while x.len() >= 4 {
                let (tag, n) = (u16_at(x, 0), u16_at(x, 2) as usize);
                if 4 + n > x.len() {
                    break;
                }
                if tag == 1 {
                    let mut d = &x[4..4 + n];
                    for v in [&mut unc, &mut comp, &mut offset] {
                        if *v == 0xFFFF_FFFF && d.len() >= 8 {
                            *v = u64_at(d, 0);
                            d = &d[8..];
                        }
                    }
                }
                x = &x[4 + n..];
            }
            entries.push(Entry {
                name,
                flags,
                method,
                crc,
                comp,
                unc,
                offset,
            });
            p = end;
        }
        Ok(Archive {
            file,
            len,
            entries,
            cd_start,
            cd_size,
            comment,
        })
    }

    /// Raw local header bytes (fixed part, name and extra) of entry `i`.
    pub fn local_header(&self, i: usize) -> io::Result<Vec<u8>> {
        let off = self.entries[i].offset;
        let mut h = vec![0; 30];
        if off.checked_add(30).is_none_or(|x| x > self.len) {
            return bad("local header out of range");
        }
        self.file.read_exact_at(&mut h, off)?;
        if u32_at(&h, 0) != 0x0403_4B50 {
            return bad(format!("{}: bad local header", self.entries[i].name));
        }
        let n = u16_at(&h, 26) as usize + u16_at(&h, 28) as usize;
        h.resize(30 + n, 0);
        if off + 30 + n as u64 > self.len {
            return bad("local header out of range");
        }
        self.file.read_exact_at(&mut h[30..], off + 30)?;
        Ok(h)
    }

    pub fn raw(&self, i: usize) -> io::Result<Range<'_>> {
        let e = &self.entries[i];
        let start = e.offset + self.local_header(i)?.len() as u64;
        if start.checked_add(e.comp).is_none_or(|x| x > self.len) {
            return bad(format!("{}: data out of range", e.name));
        }
        Ok(Range {
            f: &self.file,
            pos: start,
            end: start + e.comp,
        })
    }

    /// Decompressed data of entry `i`.
    pub fn open_entry(&self, i: usize) -> io::Result<Box<dyn Read + '_>> {
        let e = &self.entries[i];
        if e.flags & 1 != 0 {
            return bad(format!("{}: encrypted entries are not supported", e.name));
        }
        let raw = BufReader::with_capacity(1 << 20, self.raw(i)?);
        Ok(match e.method {
            0 => Box::new(raw),
            8 => Box::new(flate2::bufread::DeflateDecoder::new(raw)),
            93 | 20 => Box::new(rvzstd::Decoder::new(raw)?),
            m => return bad(format!("{}: unsupported compression method {m}", e.name)),
        })
    }
}

/// Reads `r` to the end, passing chunks to `f`; returns (size, crc32).
/// Fails before passing on any byte past `limit`, so data longer than its
/// header says (a zip bomb) costs neither disk nor CPU beyond `limit`.
pub fn pump(
    mut r: impl Read,
    limit: u64,
    mut f: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<(u64, u32)> {
    let mut buf = vec![0u8; 1 << 20];
    let mut h = crc32fast::Hasher::new();
    let mut n = 0u64;
    loop {
        let k = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n + k as u64 > limit {
            return bad("more data than its recorded size");
        }
        h.update(&buf[..k]);
        n += k as u64;
        f(&buf[..k])?;
    }
    Ok((n, h.finalize()))
}
