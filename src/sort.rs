// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Entry name ordering of RomVault structured zips (TorrentZip style):
//! compare UTF-16 code units, folding only ASCII `A-Z` to lower case; a
//! shorter prefix sorts first.

use std::cmp::Ordering;

fn fold(u: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&u) {
        u + 0x20
    } else {
        u
    }
}

/// Case-insensitive order (ASCII letters only).
pub fn tz_cmp(a: &[u16], b: &[u16]) -> Ordering {
    a.iter().map(|&u| fold(u)).cmp(b.iter().map(|&u| fold(u)))
}

/// Case-insensitive first, ordinal on ties.
pub fn tz_cmp_case(a: &[u16], b: &[u16]) -> Ordering {
    tz_cmp(a, b).then_with(|| a.cmp(b))
}

/// True if `dir` is a directory name (ends in `/`) and `name` is inside it,
/// ignoring ASCII case. Such a directory entry is redundant.
pub fn dir_contains(dir: &[u16], name: &[u16]) -> bool {
    dir.last() == Some(&(b'/' as u16))
        && name.len() > dir.len()
        && tz_cmp(dir, &name[..dir.len()]) == Ordering::Equal
}

pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    fn c(a: &str, b: &str) -> Ordering {
        tz_cmp_case(&utf16(a), &utf16(b))
    }

    #[test]
    fn order() {
        assert_eq!(c("a", "B"), Less);
        assert_eq!(c("B", "a"), Greater);
        assert_eq!(c("a", "ab"), Less);
        assert_eq!(c("A", "a"), Less);
        assert_eq!(tz_cmp(&utf16("ABC"), &utf16("abc")), Equal);
        // '_' (0x5F) sorts before folded letters (0x61+)
        assert_eq!(c("_", "a"), Less);
        assert_eq!(c("_", "A"), Less); // 'A' folds to 0x61 > 0x5F
        assert_eq!(c("a/b", "a.b"), Greater); // '/' 0x2F > '.' 0x2E
        assert_eq!(c("a b", "a/"), Less);
        // non-ASCII letters are not folded, compared as UTF-16 units
        assert_eq!(c("\u{00C9}", "\u{00E9}"), Less);
        assert_eq!(c("\u{FFFF}", "\u{1F600}"), Greater); // surrogate 0xD83D < 0xFFFF
    }
}
