// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The entry encoder of each output format.

use crate::deflate::Deflater;
use crate::rvzstd::Encoder;
use crate::zipwrite::Format;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

pub enum Codec {
    Zstd(Encoder),
    Deflate(Deflater),
}

impl Codec {
    /// `workers` is used by zstd only; deflate is single-threaded.
    pub fn new(fmt: Format, workers: u32) -> io::Result<Self> {
        Ok(match fmt {
            Format::Rvzstd => Codec::Zstd(Encoder::new(workers)?),
            Format::Trrntzip => Codec::Deflate(Deflater::new()?),
        })
    }

    pub fn set_meter(&mut self, m: Option<Arc<AtomicU64>>) {
        match self {
            Codec::Zstd(e) => e.set_meter(m),
            Codec::Deflate(d) => d.set_meter(m),
        }
    }

    pub fn skip(&mut self, n: u64) {
        match self {
            Codec::Zstd(e) => e.skip(n),
            Codec::Deflate(d) => d.skip(n),
        }
    }

    pub fn write(&mut self, data: &[u8], sink: &mut dyn Write) -> io::Result<()> {
        match self {
            Codec::Zstd(e) => e.write(data, sink),
            Codec::Deflate(d) => d.write(data, sink),
        }
    }

    pub fn finish(&mut self, sink: &mut dyn Write) -> io::Result<()> {
        match self {
            Codec::Zstd(e) => e.finish(sink),
            Codec::Deflate(d) => d.finish(sink),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Codec::Zstd(e) => e.reset(),
            Codec::Deflate(d) => d.reset(),
        }
    }
}
