// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds the sorted entry list of an RVZSTD zip from a directory or a zip.

use crate::cp437;
use crate::sort::{dir_contains, tz_cmp, tz_cmp_case, utf16};
use crate::zipread::Archive;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub enum Source {
    File(PathBuf),
    /// Index in the source zip and the CRC its central directory records.
    Zip(usize, u32),
    Dir,
}

pub struct Entry {
    pub name: String,
    pub size: u64,
    pub src: Source,
    key: Vec<u16>,
}

impl Entry {
    fn new(name: String, size: u64, src: Source) -> Self {
        let name = name.replace('\\', "/");
        Entry {
            key: utf16(&name),
            name,
            size,
            src,
        }
    }
}

pub fn symlink_error(p: &Path) -> io::Error {
    io::Error::other(format!("{}: symbolic link, not packed", p.display()))
}

fn walk(root: &Path, rel: &str, out: &mut Vec<Entry>) -> io::Result<()> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let mut empty = true;
    for d in fs::read_dir(&dir)? {
        let d = d?;
        empty = false;
        let fname = d.file_name();
        let Some(fname) = fname.to_str() else {
            return Err(io::Error::other(format!(
                "{}: name is not valid UTF-8",
                d.path().display()
            )));
        };
        let name = if rel.is_empty() {
            fname.to_string()
        } else {
            format!("{rel}/{fname}")
        };
        // not followed: a link could pull in files from outside the tree
        let ft = d.file_type()?;
        if ft.is_symlink() {
            return Err(symlink_error(&d.path()));
        } else if ft.is_dir() {
            walk(root, &name, out)?;
        } else if ft.is_file() {
            out.push(Entry::new(
                name,
                d.metadata()?.len(),
                Source::File(d.path()),
            ));
        }
    }
    if empty && !rel.is_empty() {
        out.push(Entry::new(format!("{rel}/"), 0, Source::Dir));
    }
    Ok(())
}

pub fn from_dir(root: &Path) -> io::Result<Vec<Entry>> {
    let mut v = Vec::new();
    walk(root, "", &mut v)?;
    finalize(v)
}

pub fn from_zip(a: &Archive) -> io::Result<Vec<Entry>> {
    let mut v = Vec::with_capacity(a.entries.len());
    for (i, e) in a.entries.iter().enumerate() {
        if e.is_dir() {
            if e.unc != 0 {
                return Err(io::Error::other(format!(
                    "{}: directory entry with data",
                    e.name
                )));
            }
            v.push(Entry::new(e.name.clone(), 0, Source::Dir));
        } else {
            v.push(Entry::new(e.name.clone(), e.unc, Source::Zip(i, e.crc)));
        }
    }
    finalize(v)
}

/// Sorts, rejects names equal ignoring ASCII case or too long for a zip
/// header, and drops directory entries that have something inside them.
fn finalize(mut v: Vec<Entry>) -> io::Result<Vec<Entry>> {
    // a bit-11 name from a zip can grow: each invalid byte became U+FFFD,
    // which takes 3 bytes in UTF-8
    if let Some(e) = v
        .iter()
        .find(|e| cp437::name_bytes(&e.name).0.len() > 0xFFFF)
    {
        return Err(io::Error::other(format!(
            "{}...: name longer than 65535 bytes",
            e.name.chars().take(40).collect::<String>()
        )));
    }
    v.sort_by(|a, b| tz_cmp_case(&a.key, &b.key));
    for w in v.windows(2) {
        if tz_cmp(&w[0].key, &w[1].key) == Ordering::Equal {
            return Err(io::Error::other(format!(
                "names collide (equal ignoring case): {:?} and {:?}",
                w[0].name, w[1].name
            )));
        }
    }
    let keep: Vec<bool> = v
        .windows(2)
        .map(|w| !dir_contains(&w[0].key, &w[1].key))
        .chain([true])
        .collect();
    let mut keep = keep.into_iter();
    v.retain(|_| keep.next() == Some(true));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: Vec<(&str, u64)>) -> io::Result<Vec<String>> {
        let v = v
            .into_iter()
            .map(|(n, s)| Entry::new(n.into(), s, Source::Dir))
            .collect();
        Ok(finalize(v)?.into_iter().map(|e| e.name).collect())
    }

    #[test]
    fn dirs_and_order() {
        // directory match is case-insensitive, like RomVault's check
        let r = names(vec![
            ("b/c/", 0),
            ("b/", 0),
            ("B/c/d", 1),
            ("a/", 0),
            ("z\\y", 1),
            ("Za", 1),
        ])
        .unwrap();
        assert_eq!(r, ["a/", "B/c/d", "z/y", "Za"]);
        let r = names(vec![("e/", 0), ("e", 1), ("e.x", 1)]).unwrap();
        assert_eq!(r, ["e", "e.x", "e/"]);
    }

    #[test]
    fn name_too_long() {
        assert!(names(vec![(&"x".repeat(0xFFFF), 1)]).is_ok());
        assert!(names(vec![(&"x".repeat(0x10000), 1)]).is_err());
        // 21846 U+FFFD take 65538 bytes in UTF-8
        assert!(names(vec![(&"\u{FFFD}".repeat(21846), 1)]).is_err());
    }

    #[test]
    fn case_collision() {
        assert!(names(vec![("ABC", 1), ("abc", 1)]).is_err());
    }
}
