// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! RVZSTD validation. Covers RomVault's `ValidateStructure` checks (comment
//! CRC, method rule, no `\`, CP437 vs bit 11, time 0, sort order, no redundant
//! directories, data ends with `01 00 00`) and is stricter: every byte outside
//! the compressed data must equal what this tool writes. `deep` also
//! decompresses, checks CRC and size, and re-encodes to compare the data.

use crate::progress::Bar;
use crate::rvzstd::{Decoder, Encoder};
use crate::sort::{dir_contains, tz_cmp, utf16};
use crate::sys::FileExt;
use crate::zipread::{Archive, pump};
use crate::zipwrite::{Meta, central_header, end_records, local_header};
use std::cmp::Ordering;
use std::io::{self, Read, Write};

fn fail<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

/// Ok(()) if valid, Err(reason) if not. I/O errors are reasons too.
pub fn check(a: &Archive, deep: Option<(&mut Encoder, &Bar)>) -> Result<(), String> {
    let ents = &a.entries;
    let mut pos = 0u64;
    let mut cd = Vec::new();
    for (i, e) in ents.iter().enumerate() {
        let n = &e.name;
        if n.contains('\\') {
            return fail(format!("{n}: name contains '\\'"));
        }
        if e.offset != pos {
            return fail(format!("{n}: unexpected local header offset"));
        }
        if e.unc == 0 && (e.comp != 0 || e.crc != 0) {
            return fail(format!("{n}: empty entry with data"));
        }
        if e.unc != 0 && e.comp < 3 {
            return fail(format!("{n}: compressed data too short"));
        }
        let m = Meta {
            name: n,
            unc: e.unc,
            comp: e.comp,
            crc: e.crc,
        };
        let want = local_header(&m, true);
        let got = a.local_header(i).map_err(|e| e.to_string())?;
        if got != want {
            return fail(format!("{n}: local header differs from RVZSTD layout"));
        }
        pos += want.len() as u64;
        let Some(data_end) = pos.checked_add(e.comp).filter(|&x| x <= a.len) else {
            return fail(format!("{n}: data out of range"));
        };
        if e.unc != 0 {
            let mut tail = [0u8; 3];
            a.file
                .read_exact_at(&mut tail, data_end - 3)
                .map_err(|e| e.to_string())?;
            if tail != [1, 0, 0] {
                return fail(format!("{n}: zstd data does not end with an empty block"));
            }
        }
        pos = data_end;
        cd.extend_from_slice(&central_header(&m, e.offset));
    }
    for w in ents.windows(2) {
        let (x, y) = (utf16(&w[0].name), utf16(&w[1].name));
        if tz_cmp(&x, &y) != Ordering::Less {
            return fail(format!("bad order: {:?} before {:?}", w[0].name, w[1].name));
        }
        if dir_contains(&x, &y) {
            return fail(format!("redundant directory entry {:?}", w[0].name));
        }
    }
    if pos != a.cd_start || cd.len() as u64 != a.cd_size {
        return fail("central directory position or size differs");
    }
    let mut got = vec![0u8; cd.len()];
    a.file
        .read_exact_at(&mut got, pos)
        .map_err(|e| e.to_string())?;
    if got != cd {
        return fail("central directory differs from RVZSTD layout");
    }
    let end = end_records(ents.len() as u64, pos, &cd);
    let mut got = vec![0u8; end.len()];
    pos += cd.len() as u64;
    if pos + end.len() as u64 != a.len {
        return fail("bad end of central directory or comment (file length)");
    }
    a.file
        .read_exact_at(&mut got, pos)
        .map_err(|e| e.to_string())?;
    if got != end {
        return fail("bad end of central directory or RVZSTD comment");
    }
    if let Some((enc, bar)) = deep {
        bar.set_total(ents.iter().map(|e| e.unc).sum());
        enc.set_meter(bar.counter());
        for (i, e) in ents.iter().enumerate() {
            deep_entry(a, i, enc).map_err(|err| format!("{}: {err}", e.name))?;
        }
    }
    Ok(())
}

/// Compares written bytes to the stored compressed data.
struct Cmp<R: Read> {
    src: R,
    buf: Vec<u8>,
}

impl<R: Read> Write for Cmp<R> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.resize(b.len(), 0);
        self.src.read_exact(&mut self.buf)?;
        if self.buf != b {
            return Err(io::Error::other("re-encoded data differs"));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn deep_entry(a: &Archive, i: usize, enc: &mut Encoder) -> io::Result<()> {
    let e = &a.entries[i];
    if e.unc == 0 {
        return Ok(());
    }
    let raw = a.raw(i)?;
    let dec = Decoder::new(io::BufReader::with_capacity(1 << 20, raw.clone()))?;
    let mut cmp = Cmp {
        src: raw,
        buf: Vec::new(),
    };
    let r = (|| {
        let (n, crc) = pump(dec, e.unc, |b| enc.write(b, &mut cmp))?;
        if n != e.unc || crc != e.crc {
            return Err(io::Error::other("size or CRC mismatch"));
        }
        enc.finish(&mut cmp)?;
        if cmp.src.read(&mut [0u8; 1])? != 0 {
            return Err(io::Error::other("re-encoded data is shorter"));
        }
        Ok(())
    })();
    if r.is_err() {
        enc.reset();
    }
    r
}
