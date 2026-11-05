// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! zstd encoder driven exactly like RomVault's `RVZstdSharp.CompressionStream`
//! (a port of zstd 1.5.5): level 19, nbWorkers >= 1, no pledged size.
//! Input goes in with `ZSTD_e_continue`, then `ZSTD_e_flush` until done, then
//! `ZSTD_e_end` until done. The flush before end is what makes the frame end
//! with an empty last block (`01 00 00`).

use std::ffi::CStr;
use std::io::{self, Read, Write};
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zstd_sys as z;

/// `ZSTD_frameProgression` from zstd.h (static API, not in the default
/// zstd-sys bindings; the "experimental" feature would add them).
#[repr(C)]
struct FrameProgression {
    ingested: u64,
    consumed: u64,
    produced: u64,
    flushed: u64,
    current_job_id: u32,
    nb_active_workers: u32,
}

unsafe extern "C" {
    fn ZSTD_getFrameProgression(cctx: *const z::ZSTD_CCtx) -> FrameProgression;
}

fn check(code: usize) -> io::Result<usize> {
    unsafe {
        if z::ZSTD_isError(code) != 0 {
            let msg = CStr::from_ptr(z::ZSTD_getErrorName(code));
            return Err(io::Error::other(format!("zstd: {}", msg.to_string_lossy())));
        }
    }
    Ok(code)
}

pub struct Encoder {
    cctx: *mut z::ZSTD_CCtx,
    out: Vec<u8>,
    meter: Option<Arc<AtomicU64>>,
    /// Input of the frames ended since `set_meter`.
    base: u64,
}

// The context is only used from one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(workers: u32) -> io::Result<Self> {
        let cctx = unsafe { z::ZSTD_createCCtx() };
        if cctx.is_null() {
            return Err(io::Error::other("zstd: cannot create context"));
        }
        let e = Encoder {
            cctx,
            out: vec![0; unsafe { z::ZSTD_CStreamOutSize() }],
            meter: None,
            base: 0,
        };
        unsafe {
            check(z::ZSTD_CCtx_setParameter(
                cctx,
                z::ZSTD_cParameter::ZSTD_c_compressionLevel,
                19,
            ))?;
            // zstd clamps out-of-range values instead of failing (a negative
            // one becomes 0) and returns the value it set, so check that too.
            let n = workers.clamp(1, i32::MAX as u32) as i32;
            let r = z::ZSTD_CCtx_setParameter(cctx, z::ZSTD_cParameter::ZSTD_c_nbWorkers, n);
            if z::ZSTD_isError(r) != 0 || r == 0 {
                // Without multithreading the frame layout would differ.
                return Err(io::Error::other("zstd: built without multithread support"));
            }
        }
        Ok(e)
    }

    fn run(
        &mut self,
        data: &[u8],
        op: z::ZSTD_EndDirective,
        sink: &mut dyn Write,
    ) -> io::Result<()> {
        let mut input = z::ZSTD_inBuffer {
            src: data.as_ptr() as *const c_void,
            size: data.len(),
            pos: 0,
        };
        loop {
            let mut output = z::ZSTD_outBuffer {
                dst: self.out.as_mut_ptr() as *mut c_void,
                size: self.out.len(),
                pos: 0,
            };
            let remaining =
                check(unsafe { z::ZSTD_compressStream2(self.cctx, &mut output, &mut input, op) })?;
            if let Some(m) = &self.meter {
                // input the workers have compressed, not only taken in:
                // with 32 MiB jobs that can be far behind what was written
                let p = unsafe { ZSTD_getFrameProgression(self.cctx) };
                m.store(self.base + p.consumed, Ordering::Relaxed);
            }
            if output.pos > 0 {
                sink.write_all(&self.out[..output.pos])?;
            }
            let done = match op {
                z::ZSTD_EndDirective::ZSTD_e_continue => input.pos == input.size,
                _ => remaining == 0,
            };
            if done {
                return Ok(());
            }
        }
    }

    /// From now on, keeps `m` set to the input compressed so far, over
    /// all frames.
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

    pub fn write(&mut self, data: &[u8], sink: &mut dyn Write) -> io::Result<()> {
        self.run(data, z::ZSTD_EndDirective::ZSTD_e_continue, sink)
    }

    /// Ends the frame and resets the context for the next entry.
    pub fn finish(&mut self, sink: &mut dyn Write) -> io::Result<()> {
        self.run(&[], z::ZSTD_EndDirective::ZSTD_e_flush, sink)?;
        // the context resets when the frame ends, and with it the progression
        let p = unsafe { ZSTD_getFrameProgression(self.cctx) };
        self.run(&[], z::ZSTD_EndDirective::ZSTD_e_end, sink)?;
        self.base += p.ingested;
        if let Some(m) = &self.meter {
            m.store(self.base, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Drops any partial frame (after an error).
    pub fn reset(&mut self) {
        unsafe { z::ZSTD_CCtx_reset(self.cctx, z::ZSTD_ResetDirective::ZSTD_reset_session_only) };
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { z::ZSTD_freeCCtx(self.cctx) };
    }
}

/// Streaming decoder of one or more zstd frames.
pub struct Decoder<R: Read> {
    dctx: *mut z::ZSTD_DCtx,
    src: R,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    frame_done: bool,
}

unsafe impl<R: Read + Send> Send for Decoder<R> {}

impl<R: Read> Decoder<R> {
    pub fn new(src: R) -> io::Result<Self> {
        let dctx = unsafe { z::ZSTD_createDCtx() };
        if dctx.is_null() {
            return Err(io::Error::other("zstd: cannot create context"));
        }
        let n = unsafe { z::ZSTD_DStreamInSize() };
        // built first so that Drop frees the context if setup fails
        let d = Decoder {
            dctx,
            src,
            buf: vec![0; n],
            pos: 0,
            len: 0,
            eof: false,
            frame_done: true,
        };
        // RomVault frames use windowLog 23 (8 MiB). 27 is libzstd's default
        // limit; a larger window in a hostile frame would allocate up to 2 GiB.
        // `decoder_mem_mib` in main.rs assumes this limit.
        unsafe {
            check(z::ZSTD_DCtx_setParameter(
                dctx,
                z::ZSTD_dParameter::ZSTD_d_windowLogMax,
                27,
            ))?;
        }
        Ok(d)
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos == self.len && !self.eof {
                self.len = self.src.read(&mut self.buf)?;
                self.pos = 0;
                self.eof = self.len == 0;
            }
            if self.pos == self.len && self.eof && self.frame_done {
                return Ok(0);
            }
            let mut input = z::ZSTD_inBuffer {
                src: self.buf[self.pos..].as_ptr() as *const c_void,
                size: self.len - self.pos,
                pos: 0,
            };
            let mut output = z::ZSTD_outBuffer {
                dst: dst.as_mut_ptr() as *mut c_void,
                size: dst.len(),
                pos: 0,
            };
            let r = check(unsafe { z::ZSTD_decompressStream(self.dctx, &mut output, &mut input) })?;
            self.pos += input.pos;
            self.frame_done = r == 0;
            if output.pos > 0 {
                return Ok(output.pos);
            }
            if self.eof && self.pos == self.len {
                // no input left and no progress: frame never finished
                return Err(io::Error::other("zstd: truncated frame"));
            }
        }
    }
}

impl<R: Read> Drop for Decoder<R> {
    fn drop(&mut self) {
        unsafe { z::ZSTD_freeDCtx(self.dctx) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libzstd_is_1_5_5() {
        assert_eq!(unsafe { z::ZSTD_versionNumber() }, 10505);
    }

    #[test]
    fn frame_shape() {
        let mut e = Encoder::new(1).unwrap();
        let mut out = Vec::new();
        e.write(b"hello hello hello", &mut out).unwrap();
        e.finish(&mut out).unwrap();
        assert_eq!(&out[..6], [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x68]);
        assert_eq!(&out[out.len() - 3..], [1, 0, 0]);
        // reuse after finish gives the same bytes
        let mut out2 = Vec::new();
        e.write(b"hello hello hello", &mut out2).unwrap();
        e.finish(&mut out2).unwrap();
        assert_eq!(out, out2);
        let mut d = Decoder::new(&out[..]).unwrap();
        let mut back = Vec::new();
        d.read_to_end(&mut back).unwrap();
        assert_eq!(back, b"hello hello hello");
    }
}
