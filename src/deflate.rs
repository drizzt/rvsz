// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw deflate encoder driven like RomVault's TorrentZip writer (a managed
//! port of zlib): level 9, windowBits -15 (no zlib header), memLevel 8,
//! default strategy, `Z_NO_FLUSH` for the data and `Z_FINISH` at the end.
//! zlib output at `Z_NO_FLUSH` does not depend on how the input is split.

use libz_sys as z;
use std::alloc::{Layout, alloc, dealloc};
use std::ffi::c_int;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Room before each allocation for its size, keeping 16-byte alignment.
const HDR: usize = 16;

unsafe extern "C" fn zalloc(_: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    let Some(n) = (items as usize)
        .checked_mul(size as usize)
        .and_then(|n| n.checked_add(HDR))
    else {
        return std::ptr::null_mut();
    };
    let Ok(l) = Layout::from_size_align(n, HDR) else {
        return std::ptr::null_mut();
    };
    // SAFETY: n >= HDR > 0; the size is kept in front for zfree
    unsafe {
        let p = alloc(l);
        if p.is_null() {
            return p.cast();
        }
        (p as *mut usize).write(n);
        p.add(HDR).cast()
    }
}

unsafe extern "C" fn zfree(_: z::voidpf, p: z::voidpf) {
    if p.is_null() {
        return;
    }
    // SAFETY: p came from zalloc, which stored the allocation size before it
    unsafe {
        let p = (p as *mut u8).sub(HDR);
        let n = (p as *const usize).read();
        dealloc(p, Layout::from_size_align_unchecked(n, HDR));
    }
}

fn zerr(what: &str, r: c_int) -> io::Error {
    io::Error::other(format!("zlib: {what} failed ({r})"))
}

pub struct Deflater {
    /// Boxed: zlib keeps a pointer back to the stream and checks it.
    strm: Box<z::z_stream>,
    out: Vec<u8>,
    meter: Option<Arc<AtomicU64>>,
    /// Input of the streams ended since `set_meter`.
    base: u64,
    /// Input taken by the current stream (`total_in` is 32 bits on Windows).
    taken: u64,
}

// The stream is only used from one thread at a time.
unsafe impl Send for Deflater {}

impl Deflater {
    pub fn new() -> io::Result<Self> {
        let mut strm = Box::new(z::z_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        });
        // SAFETY: strm is a valid, boxed stream with allocators set
        let r = unsafe {
            z::deflateInit2_(
                &mut *strm,
                9,
                z::Z_DEFLATED,
                -15,
                8,
                z::Z_DEFAULT_STRATEGY,
                z::zlibVersion(),
                size_of::<z::z_stream>() as c_int,
            )
        };
        if r != z::Z_OK {
            return Err(zerr("deflateInit2", r));
        }
        Ok(Deflater {
            strm,
            out: vec![0; 1 << 16],
            meter: None,
            base: 0,
            taken: 0,
        })
    }

    pub fn write(&mut self, data: &[u8], sink: &mut dyn Write) -> io::Result<()> {
        // avail_in is 32 bits: feed huge buffers in parts
        for part in data.chunks(1 << 30) {
            let start = self.taken;
            self.strm.next_in = part.as_ptr() as *mut u8;
            self.strm.avail_in = part.len() as z::uInt;
            loop {
                self.strm.next_out = self.out.as_mut_ptr();
                self.strm.avail_out = self.out.len() as z::uInt;
                // SAFETY: both buffers are valid for the lengths given
                let r = unsafe { z::deflate(&mut *self.strm, z::Z_NO_FLUSH) };
                // Z_BUF_ERROR: no progress possible, all input taken
                if r != z::Z_OK && r != z::Z_BUF_ERROR {
                    return Err(zerr("deflate", r));
                }
                let n = self.out.len() - self.strm.avail_out as usize;
                self.taken = start + (part.len() - self.strm.avail_in as usize) as u64;
                if let Some(m) = &self.meter {
                    m.store(self.base + self.taken, Ordering::Relaxed);
                }
                if n > 0 {
                    sink.write_all(&self.out[..n])?;
                }
                if self.strm.avail_in == 0 && self.strm.avail_out != 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    /// From now on, keeps `m` set to the input compressed so far, over
    /// all streams.
    pub fn set_meter(&mut self, m: Option<Arc<AtomicU64>>) {
        self.meter = m;
        self.base = 0;
    }

    /// Counts `n` input bytes as done without encoding them (an entry
    /// copied from an older zip).
    pub fn skip(&mut self, n: u64) {
        self.base += n;
        if let Some(m) = &self.meter {
            m.store(self.base, Ordering::Relaxed);
        }
    }

    /// Ends the stream and resets it for the next entry.
    pub fn finish(&mut self, sink: &mut dyn Write) -> io::Result<()> {
        self.strm.next_in = std::ptr::null_mut();
        self.strm.avail_in = 0;
        loop {
            self.strm.next_out = self.out.as_mut_ptr();
            self.strm.avail_out = self.out.len() as z::uInt;
            // SAFETY: the output buffer is valid for the length given
            let r = unsafe { z::deflate(&mut *self.strm, z::Z_FINISH) };
            if r != z::Z_OK && r != z::Z_STREAM_END {
                return Err(zerr("deflate", r));
            }
            let n = self.out.len() - self.strm.avail_out as usize;
            if n > 0 {
                sink.write_all(&self.out[..n])?;
            }
            if r == z::Z_STREAM_END {
                break;
            }
        }
        self.base += self.taken;
        if let Some(m) = &self.meter {
            m.store(self.base, Ordering::Relaxed);
        }
        self.reset();
        Ok(())
    }

    /// Drops any partial stream (after an error).
    pub fn reset(&mut self) {
        self.taken = 0;
        // SAFETY: the stream was initialised by deflateInit2
        unsafe { z::deflateReset(&mut *self.strm) };
    }
}

impl Drop for Deflater {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by deflateInit2
        unsafe { z::deflateEnd(&mut *self.strm) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::io::Read;

    #[test]
    fn zlib_is_1_3_2() {
        // SAFETY: zlibVersion returns a static NUL-terminated string
        let v = unsafe { CStr::from_ptr(z::zlibVersion()) };
        assert_eq!(v.to_str().unwrap(), "1.3.2");
    }

    #[test]
    fn stream_shape() {
        let data: Vec<u8> = (0..200_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        let mut d = Deflater::new().unwrap();
        let mut out = Vec::new();
        d.write(&data, &mut out).unwrap();
        d.finish(&mut out).unwrap();
        // the split of the input does not change the bytes, nor does reuse
        let mut out2 = Vec::new();
        for c in data.chunks(777) {
            d.write(c, &mut out2).unwrap();
        }
        d.finish(&mut out2).unwrap();
        assert_eq!(out, out2);
        let mut back = Vec::new();
        flate2::read::DeflateDecoder::new(&out[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
        // empty input is the 2-byte final empty block
        let mut e = Vec::new();
        d.finish(&mut e).unwrap();
        assert_eq!(e, [3, 0]);
    }
}
