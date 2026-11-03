// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! 256-color escapes, used only when the stream is a color terminal.
//! `NO_COLOR` turns them off, `CLICOLOR_FORCE` turns them on.

use crate::sys;
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

pub const GREEN: u8 = 2;
pub const GREY: u8 = 8;
pub const RED: u8 = 9;
pub const SKY: u8 = 39;

fn set(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0")
}

fn detect(stderr: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if set("CLICOLOR_FORCE") {
        return true;
    }
    let tty = if stderr {
        io::stderr().is_terminal()
    } else {
        io::stdout().is_terminal()
    };
    tty && std::env::var_os("TERM").is_none_or(|t| t != "dumb") && sys::enable_vt(stderr)
}

/// Whether to color stderr (`true`) or stdout (`false`).
pub fn enabled(stderr: bool) -> bool {
    static ERR: OnceLock<bool> = OnceLock::new();
    static OUT: OnceLock<bool> = OnceLock::new();
    if stderr {
        *ERR.get_or_init(|| detect(true))
    } else {
        *OUT.get_or_init(|| detect(false))
    }
}

/// `s` in color `code` if `on`, else as it is.
pub fn paint(on: bool, code: u8, s: &str) -> String {
    if on {
        format!("\x1b[38;5;{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// `word` padded to `w` columns, colored for the stream.
pub fn word(stderr: bool, code: u8, word: &str, w: usize) -> String {
    let pad = w.saturating_sub(word.chars().count());
    format!("{}{}", paint(enabled(stderr), code, word), " ".repeat(pad))
}
