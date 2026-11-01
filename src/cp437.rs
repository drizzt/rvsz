// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Code page 437 file names, matching what RomVault accepts. Bytes 0x20-0x7F
//! are ASCII (0x7F stays U+007F), 0x80-0xFF follow the standard Unicode
//! mapping of code page 437, and 0x01-0x1F are the IBM PC display glyphs
//! (smiley, card suits, arrows, ...), not control characters. A name is
//! stored as CP437 only if every UTF-16 unit maps; otherwise it is stored
//! as UTF-8 with GP flag bit 11.

/// Glyphs for bytes 0x00-0x1F. 0x00 is kept as U+0000.
#[rustfmt::skip]
const LOW: [char; 32] = [
    '\u{0000}', '\u{263A}', '\u{263B}', '\u{2665}', '\u{2666}', '\u{2663}', '\u{2660}', '\u{2022}',
    '\u{25D8}', '\u{25CB}', '\u{25D9}', '\u{2642}', '\u{2640}', '\u{266A}', '\u{266B}', '\u{263C}',
    '\u{25BA}', '\u{25C4}', '\u{2195}', '\u{203C}', '\u{00B6}', '\u{00A7}', '\u{25AC}', '\u{21A8}',
    '\u{2191}', '\u{2193}', '\u{2192}', '\u{2190}', '\u{221F}', '\u{2194}', '\u{25B2}', '\u{25BC}',
];

/// Characters for bytes 0x80-0xFF.
#[rustfmt::skip]
const HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

fn byte_to_char(b: u8) -> char {
    match b {
        0x00..=0x1F => LOW[b as usize],
        0x20..=0x7F => b as char,
        _ => HIGH[b as usize - 0x80],
    }
}

fn unit_to_byte(u: u16) -> Option<u8> {
    if (0x20..=0x7F).contains(&u) {
        return Some(u as u8);
    }
    let c = char::from_u32(u as u32)?;
    if let Some(i) = LOW.iter().position(|&g| g == c) {
        return Some(i as u8);
    }
    HIGH.iter().position(|&g| g == c).map(|i| (i + 0x80) as u8)
}

/// Encode as CP437, or `None` if any UTF-16 unit has no CP437 byte.
pub fn encode(s: &str) -> Option<Vec<u8>> {
    s.encode_utf16().map(unit_to_byte).collect()
}

pub fn decode(b: &[u8]) -> String {
    b.iter().map(|&c| byte_to_char(c)).collect()
}

/// Bytes stored in the zip for `name`, and whether bit 11 (UTF-8) is set.
pub fn name_bytes(name: &str) -> (Vec<u8>, bool) {
    match encode(name) {
        Some(b) => (b, false),
        None => (name.as_bytes().to_vec(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_glyphs() {
        assert_eq!(encode("abc ~\u{7f}").unwrap(), b"abc ~\x7f");
        assert_eq!(
            encode("\u{263A}\u{00C7}\u{00A0}\0").unwrap(),
            [1, 0x80, 0xFF, 0]
        );
        assert_eq!(decode(&[1, 0x80, 0xFF, b'A']), "\u{263A}\u{00C7}\u{00A0}A");
    }

    #[test]
    fn non_cp437() {
        // control characters other than NUL force UTF-8
        assert!(encode("a\u{01}").is_none());
        assert!(encode("a\n").is_none());
        assert!(encode("\u{65E5}").is_none());
        assert!(encode("\u{1F600}").is_none());
        assert_eq!(
            name_bytes("\u{65E5}"),
            ("\u{65E5}".as_bytes().to_vec(), true)
        );
    }

    #[test]
    fn roundtrip_all_bytes() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(encode(&decode(&all)).unwrap(), all);
    }
}
