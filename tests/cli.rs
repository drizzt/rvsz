// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    // tests match plain text: color settings of the caller must not leak in
    let mut c = Command::new(env!("CARGO_BIN_EXE_rvsz"));
    c.env_remove("NO_COLOR").env_remove("CLICOLOR_FORCE");
    c
}

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn ok(c: &mut Command) -> bool {
    c.output().unwrap().status.success()
}

/// Small corpus with every name/entry kind the format distinguishes.
fn corpus(root: &Path) {
    fs::create_dir_all(root.join("emptydir")).unwrap();
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::create_dir_all(root.join("n/m")).unwrap();
    fs::write(root.join("n/m/f"), b"hello").unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::write(root.join("\u{263A}smile"), b"x").unwrap();
    fs::write(root.join("\u{65E5}\u{672C}"), b"x").unwrap();
    fs::write(root.join("B"), b"x").unwrap();
    fs::write(root.join("_x"), b"x").unwrap();
    let big: Vec<u8> = (0..3_000_000u32)
        .flat_map(|i| (i % 251).to_le_bytes())
        .collect();
    fs::write(root.join("big"), big).unwrap();
}

fn files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    fn walk(root: &Path, d: &Path, v: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                v.push((rel + "/", Vec::new()));
                walk(root, &p, v);
            } else {
                v.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    walk(root, root, &mut v);
    v.sort();
    v
}

#[test]
fn pack_verify_extract() {
    let t = tmp("pack_verify_extract");
    let src = t.join("src");
    corpus(&src);
    let z = t.join("out.zip");
    assert!(ok(bin().args(["pack"]).arg(&src).arg("-o").arg(&z)));
    assert!(ok(bin().args(["verify", "--deep"]).arg(&z)));
    let x = t.join("x");
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(files(&src), files(&x));

    // rewriting a valid zip is a no-op; forced rewrite is byte-identical
    let first = fs::read(&z).unwrap();
    let out = bin().arg("pack").arg(&z).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("skipped"));
    assert!(ok(bin().args(["pack", "--force"]).arg(&z)));
    assert_eq!(first, fs::read(&z).unwrap());
    // a different worker count gives the same bytes
    let z1 = t.join("j1.zip");
    assert!(ok(bin()
        .args(["pack", "-j", "1"])
        .arg(&src)
        .arg("-o")
        .arg(&z1)));
    assert_eq!(first, fs::read(&z1).unwrap());
}

#[test]
fn pack_verbose_lists_entries() {
    let t = tmp("pack_verbose_lists_entries");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("one"), b"1").unwrap();
    fs::write(src.join("two"), b"22").unwrap();
    let z = t.join("v.zip");
    let out = bin()
        .args(["pack", "-v"])
        .arg(&src)
        .arg("-o")
        .arg(&z)
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("v.zip [1/2] one (1 bytes)"), "{err}");
    assert!(err.contains("v.zip [2/2] two (2 bytes)"), "{err}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("packed"));
    let q = t.join("q.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&q)));
    assert_eq!(fs::read(&z).unwrap(), fs::read(&q).unwrap());
    // a rerun checks the existing output instead of packing it again
    let out = bin()
        .args(["pack", "-v"])
        .arg(&src)
        .arg("-o")
        .arg(&z)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("checking"));
}

/// Packs `src` over the zip from an earlier run of `src` and, as a full
/// encode, to a fresh path: both must be the same bytes. Returns the
/// verbose output of the run over the old zip.
fn repack_matches_full(src: &Path, z: &Path, fmt: &str) -> String {
    let out = bin()
        .args(["pack", "-v", "--format", fmt])
        .arg(src)
        .arg("-o")
        .arg(z)
        .output()
        .unwrap();
    assert!(out.status.success());
    let full = z.with_extension("full.zip");
    assert!(ok(&mut pack_cmd(fmt, src, &full)));
    assert_eq!(fs::read(z).unwrap(), fs::read(&full).unwrap());
    fs::remove_file(&full).unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn repack_copies_unchanged_entries() {
    for fmt in ["rvzstd", "trrntzip"] {
        let t = tmp(&format!("repack_{fmt}"));
        let src = t.join("src");
        corpus(&src);
        let z = t.join("o.zip");
        assert!(ok(&mut pack_cmd(fmt, &src, &z)));
        // new files at the start, middle and end of the sort order
        fs::write(src.join("0first"), b"new").unwrap();
        fs::write(src.join("m"), b"new").unwrap();
        fs::write(src.join("zz"), b"new").unwrap();
        let err = repack_matches_full(&src, &z, fmt);
        assert!(
            err.contains(": 12 entries (9 copied), 9 bytes to encode"),
            "{err}"
        );
        assert!(err.contains("] big (11.4 MiB) copied"), "{err}");
        assert!(err.contains("] m (3 bytes)\n"), "{err}");
        // edited (same size and other size), deleted, renamed
        fs::write(src.join("m"), b"NEW").unwrap();
        fs::write(src.join("B"), b"longer").unwrap();
        fs::remove_file(src.join("zz")).unwrap();
        fs::rename(src.join("_x"), src.join("_y")).unwrap();
        let err = repack_matches_full(&src, &z, fmt);
        assert!(err.contains("] m (3 bytes)\n"), "{err}");
        assert!(err.contains("] big (11.4 MiB) copied"), "{err}");
        assert!(err.contains("] empty (0 bytes) copied"), "{err}");
        assert!(err.contains("] emptydir/ (0 bytes) copied"), "{err}");
        assert!(err.contains("] \u{65E5}\u{672C} (1 bytes) copied"), "{err}");
        // a file in the empty dir drops its dir entry; deleting all
        // but one file still writes a zip
        fs::write(src.join("emptydir/f"), b"x").unwrap();
        repack_matches_full(&src, &z, fmt);
        fs::remove_dir_all(&src).unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("empty"), b"").unwrap();
        repack_matches_full(&src, &z, fmt);
        assert!(ok(bin().args(["verify", "--deep"]).arg(&z)));
        // unchanged: skipped, nothing written
        let out = pack_cmd(fmt, &src, &z).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("skipped"));
    }
}

/// An earlier output in the other format, or `--force`, copies nothing:
/// zstd data must never end up in a TorrentZip.
#[test]
fn repack_no_copy_other_format_or_force() {
    let t = tmp("repack_no_copy");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a"), b"aaaa").unwrap();
    let z = t.join("o.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    fs::write(src.join("b"), b"b").unwrap();
    let err = repack_matches_full(&src, &z, "trrntzip");
    assert!(
        err.contains(": 2 entries (0 copied), 5 bytes to encode"),
        "{err}"
    );
    fs::write(src.join("c"), b"c").unwrap();
    let out = bin()
        .args(["pack", "-v", "--force", "--format", "trrntzip"])
        .arg(&src)
        .arg("-o")
        .arg(&z)
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(": 3 entries (0 copied), 6 bytes to encode"),
        "{err}"
    );
}

/// Flips one byte in the middle of the compressed data of entry `name`.
fn corrupt_entry(z: &Path, name: &str) {
    let mut b = fs::read(z).unwrap();
    let (off, _, _, _, comp, _) = local_headers(&b)
        .into_iter()
        .find(|h| h.5 == name.as_bytes())
        .unwrap();
    let extra = u16::from_le_bytes([b[off + 28], b[off + 29]]) as usize;
    b[off + 30 + name.len() + extra + comp as usize / 2] ^= 0x55;
    fs::write(z, b).unwrap();
}

/// Bad data in the existing output is not copied: the entry is encoded
/// again, unless `--trust-existing` is given.
#[test]
fn repack_reencodes_bad_old_entry() {
    for fmt in ["rvzstd", "trrntzip"] {
        let t = tmp(&format!("repack_bad_old_{fmt}"));
        let src = t.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a"), b"some text that is not too short").unwrap();
        let z = t.join("o.zip");
        assert!(ok(&mut pack_cmd(fmt, &src, &z)));
        corrupt_entry(&z, "a");
        fs::write(src.join("b"), b"b").unwrap();
        let err = repack_matches_full(&src, &z, fmt);
        assert!(
            err.contains("o.zip: a: bad data in the existing output, encoding it again"),
            "{err}"
        );
        assert!(err.contains("] a (31 bytes)\n"), "{err}");
        corrupt_entry(&z, "a");
        let bad = fs::read(&z).unwrap();
        fs::write(src.join("c"), b"c").unwrap();
        assert!(ok(pack_cmd(fmt, &src, &z).arg("--trust-existing")));
        // the corrupt entry is still first, copied as it was
        let new = fs::read(&z).unwrap();
        let (_, _, _, _, comp, _) = local_headers(&new)[0].clone();
        let n = 30 + 1 + comp as usize;
        assert_eq!(new[..n], bad[..n]);
        assert!(!ok(bin().args(["verify", "--deep"]).arg(&z)));
    }
}

/// A copied entry is checked in the source zip too: a corrupt one fails as
/// in a full repack, even if its recorded CRC matches the existing output.
#[test]
fn repack_checks_source_zip_entry() {
    let t = tmp("repack_bad_src");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a"), b"some text that is not too short").unwrap();
    let z = t.join("o.zip");
    assert!(ok(&mut pack_cmd("trrntzip", &src, &z)));
    fs::write(src.join("b"), b"b").unwrap();
    let input = t.join("in.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&input)));
    corrupt_entry(&input, "a");
    let out = pack_cmd("trrntzip", &input, &z).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("a: CRC error in source zip"), "{err}");
    assert!(ok(pack_cmd("trrntzip", &input, &z).arg("--trust-existing")));
}

/// `pack --format <fmt> <src> -o <z>`.
fn pack_cmd(fmt: &str, src: &Path, z: &Path) -> Command {
    let mut c = bin();
    c.args(["pack", "--format", fmt]).arg(src).arg("-o").arg(z);
    c
}

fn tz_pack(src: &Path, z: &Path) -> std::process::Output {
    pack_cmd("trrntzip", src, z).output().unwrap()
}

/// (offset, method, time, date, comp, name) of every local header.
fn local_headers(b: &[u8]) -> Vec<(usize, u16, u16, u16, u32, Vec<u8>)> {
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let mut v = Vec::new();
    let mut p = 0;
    while b[p..].starts_with(b"PK\x03\x04") {
        let comp = u32::from_le_bytes(b[p + 18..p + 22].try_into().unwrap());
        let (n, x) = (u16_at(p + 26) as usize, u16_at(p + 28) as usize);
        let name = b[p + 30..p + 30 + n].to_vec();
        v.push((p, u16_at(p + 8), u16_at(p + 10), u16_at(p + 12), comp, name));
        p += 30 + n + x + comp as usize;
    }
    v
}

#[test]
fn trrntzip_pack_verify_extract() {
    let t = tmp("trrntzip_pack");
    let src = t.join("src");
    corpus(&src);
    let z = t.join("tz.zip");
    assert!(tz_pack(&src, &z).status.success());
    let b = fs::read(&z).unwrap();
    let hs = local_headers(&b);
    assert_eq!(hs.len(), 9);
    for (p, method, time, date, comp, name) in &hs {
        assert_eq!((*method, *time, *date), (8, 0xBC00, 0x2198));
        if name == b"empty" || name.ends_with(b"/") {
            assert_eq!(*comp, 2);
            let d = p + 30 + name.len();
            assert_eq!(&b[d..d + 2], [3, 0]);
        }
    }
    // the non-CP437 name is UTF-8 with bit 11
    let (p, ..) = hs
        .iter()
        .find(|h| h.5 == "\u{65E5}\u{672C}".as_bytes())
        .unwrap();
    assert_eq!(&b[p + 6..p + 8], &0x0802u16.to_le_bytes());
    let eocd = find(&b, b"PK\x05\x06");
    assert!(b[eocd + 22..].starts_with(b"TORRENTZIPPED-"));
    assert!(ok(bin().args(["verify", "--deep"]).arg(&z)));
    let x = t.join("x");
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(files(&src), files(&x));

    // already valid TorrentZip: skipped
    let out = tz_pack(&z, &z);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("skipped"));
    // rewriting it as RVZSTD gives what packing the tree gives
    let rv = t.join("rv.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&rv)));
    let conv = t.join("conv.zip");
    assert!(ok(bin().arg("pack").arg(&z).arg("-o").arg(&conv)));
    assert_eq!(fs::read(&rv).unwrap(), fs::read(&conv).unwrap());
    // and back: an RVZSTD zip is rewritten in place when TorrentZip is asked
    let out = tz_pack(&conv, &conv);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("packed"));
    assert_eq!(b, fs::read(&conv).unwrap());
    // an existing RVZSTD output of the tree is not taken as up to date
    let out = tz_pack(&src, &rv);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("packed"));
    assert_eq!(b, fs::read(&rv).unwrap());
}

#[test]
fn verify_format_restricts() {
    let t = tmp("verify_format");
    let src = t.join("src");
    corpus(&src);
    let rv = t.join("rv.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&rv)));
    let tz = t.join("tz.zip");
    assert!(tz_pack(&src, &tz).status.success());
    let verify = |extra: &[&str], z: &Path| ok(bin().arg("verify").args(extra).arg(z));
    assert!(verify(&["--format", "rvzstd"], &rv));
    assert!(!verify(&["--format", "trrntzip"], &rv));
    assert!(verify(&["--format", "trrntzip"], &tz));
    assert!(!verify(&["--format", "rvzstd"], &tz));
    assert!(verify(&[], &rv));
    assert!(verify(&[], &tz));
    let out = bin().arg("verify").arg(&rv).arg(&tz).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}: RVZSTD\n{}: TorrentZip\n", rv.display(), tz.display())
    );
}

#[test]
fn trrntzip_batch() {
    let t = tmp("trrntzip_batch");
    let src = t.join("src");
    nested_tree(&src);
    let out = t.join("out");
    let run = || {
        bin()
            .args(["batch", "-j", "2", "--format", "trrntzip"])
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .output()
            .unwrap()
    };
    let o = run();
    assert!(o.status.success());
    assert_eq!(skipped(&o), 0);
    let o = run();
    assert!(o.status.success());
    assert_eq!(skipped(&o), 2);
}

#[test]
fn verify_rejects_trrntzip_mutations() {
    let t = tmp("verify_rejects_tz");
    let src = t.join("src");
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(src.join("a"), b"aaaa").unwrap();
    fs::write(src.join("e"), b"").unwrap();
    let z = t.join("good.zip");
    assert!(tz_pack(&src, &z).status.success());
    let good = fs::read(&z).unwrap();
    let hs = local_headers(&good);
    let e_data = hs[2].0 + 30 + 1;
    let cd = find(&good, b"PK\x01\x02");

    type Mutation = Box<dyn Fn(&mut Vec<u8>)>;
    let mutations: Vec<(&str, &str, Mutation)> = vec![
        ("local time", "TorrentZip", Box::new(|b| b[11] = 0)),
        (
            "central date",
            "TorrentZip",
            Box::new(move |b| b[cd + 14] = 0),
        ),
        (
            "empty data",
            "03 00",
            Box::new(move |b| {
                b[e_data] = 1;
            }),
        ),
        (
            "unknown comment",
            "no RVZSTD or TorrentZip comment",
            Box::new(|b| {
                let e = find(b, b"PK\x05\x06");
                b[e + 22] = b'X';
            }),
        ),
    ];
    for (what, msg, m) in mutations {
        let mut b = good.clone();
        m(&mut b);
        fix_comment(&mut b);
        let p = t.join("bad.zip");
        fs::write(&p, &b).unwrap();
        let out = bin().arg("verify").arg(&p).output().unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "verify accepted mutation: {what}");
        assert!(so.contains(msg), "{what}: {so}");
    }
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).position(|w| w == needle).unwrap()
}

#[test]
fn verify_rejects_mutations() {
    let t = tmp("verify_rejects");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a"), b"aaaa").unwrap();
    fs::write(src.join("b"), b"bbbb").unwrap();
    let z = t.join("good.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    let good = fs::read(&z).unwrap();
    let cd = find(&good, b"PK\x01\x02");

    type Mutation = Box<dyn Fn(&mut Vec<u8>)>;
    let mutations: Vec<(&str, Mutation)> = vec![
        (
            "comment crc",
            Box::new(|b: &mut Vec<u8>| *b.last_mut().unwrap() ^= 1),
        ),
        ("local time", Box::new(|b: &mut Vec<u8>| b[10] = 1)),
        (
            "central time",
            Box::new(move |b: &mut Vec<u8>| b[cd + 12] = 1),
        ),
        // swap names "a" and "b" in all headers: wrong order, comment fixed up
        (
            "order",
            Box::new(move |b: &mut Vec<u8>| {
                let c1 = cd + 46 + 1;
                let l1 = u32::from_le_bytes(b[c1 + 42..c1 + 46].try_into().unwrap()) as usize;
                for (i, v) in [
                    (30, b'b'),
                    (l1 + 30, b'a'),
                    (cd + 46, b'b'),
                    (c1 + 46, b'a'),
                ] {
                    b[i] = v;
                }
                fix_comment(b);
            }),
        ),
        (
            "extra field",
            Box::new(move |b: &mut Vec<u8>| {
                b[cd + 30] = 4; // central extra length 4, eats into next header
                fix_comment(b);
            }),
        ),
        ("trailing data", Box::new(|b: &mut Vec<u8>| b.push(0))),
        ("leading data", Box::new(|b: &mut Vec<u8>| b.insert(0, 0))),
    ];
    for (what, m) in mutations {
        let mut b = good.clone();
        m(&mut b);
        let p = t.join("bad.zip");
        fs::File::create(&p).unwrap().write_all(&b).unwrap();
        assert!(
            !ok(bin().arg("verify").arg(&p)),
            "verify accepted mutation: {what}"
        );
    }
}

/// Recomputes the RVZSTD comment so only the intended defect remains.
fn fix_comment(b: &mut [u8]) {
    let cd = find(b, b"PK\x01\x02");
    let eocd = find(b, b"PK\x05\x06");
    let crc = crc32fast::hash(&b[cd..eocd]);
    let s = format!("{crc:08X}");
    let n = b.len();
    b[n - 8..].copy_from_slice(s.as_bytes());
}

#[test]
fn case_collision_fails() {
    let t = tmp("case_collision");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("ABC"), b"1").unwrap();
    fs::write(src.join("abc"), b"2").unwrap();
    // On a case-insensitive file system (Windows, macOS by default) the
    // second write overwrote the first, so there is nothing to test
    if fs::read_dir(&src).unwrap().count() < 2 {
        return;
    }
    assert!(!ok(bin()
        .arg("pack")
        .arg(&src)
        .arg("-o")
        .arg(t.join("o.zip"))));
    assert!(!t.join("o.zip").exists());
}

#[test]
fn unsafe_extract_refused() {
    let t = tmp("unsafe_extract");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("zzzzz"), b"x").unwrap();
    let z = t.join("o.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    let mut b = fs::read(&z).unwrap();
    // rename "zzzzz" to "../zz" in local and central headers
    let names: Vec<usize> = b
        .windows(5)
        .enumerate()
        .filter(|(_, w)| w == b"zzzzz")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(names.len(), 2);
    for i in names {
        b[i..i + 5].copy_from_slice(b"../zz");
    }
    fs::write(&z, &b).unwrap();
    assert!(!ok(bin().arg("extract").arg(&z).arg("-o").arg(t.join("x"))));
    assert!(!t.join("zz").exists());
}

#[test]
fn pack_current_dir_names_zip_after_it() {
    let t = tmp("pack_dot");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("f"), b"x").unwrap();
    assert!(ok(bin().arg("pack").arg(".").current_dir(&src)));
    assert!(t.join("src.zip").exists());
    assert_eq!(fs::read_dir(&src).unwrap().count(), 1);
}

/// Minimal zip with deflated "Hello.txt", a stored "Dir/" and an
/// unsorted, mixed-case order, as other tools write them.
fn deflate_zip(path: &Path, files: &[(&str, &[u8])]) {
    use flate2::{Compression, write::DeflateEncoder};
    let (mut out, mut cd) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        let comp = e.finish().unwrap();
        let (method, body): (u16, &[u8]) = if data.is_empty() {
            (0, &[])
        } else {
            (8, &comp)
        };
        let crc = crc32fast::hash(data);
        let off = out.len() as u32;
        let mut common = Vec::new();
        common.extend_from_slice(&20u16.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        common.extend_from_slice(&method.to_le_bytes());
        common.extend_from_slice(&[0x21, 0x43, 0x65, 0x57]); // some date/time
        common.extend_from_slice(&crc.to_le_bytes());
        common.extend_from_slice(&(body.len() as u32).to_le_bytes());
        common.extend_from_slice(&(data.len() as u32).to_le_bytes());
        common.extend_from_slice(&(name.len() as u16).to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&common);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(body);
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&common);
        cd.extend_from_slice(&[0; 6]); // comment len, disk, internal attrs
        cd.extend_from_slice(&0x20u32.to_le_bytes()); // attrs
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    fs::write(path, out).unwrap();
}

#[test]
fn recompress_deflate_zip_matches_dir_pack() {
    let t = tmp("recompress");
    let hello: Vec<u8> = b"Hello, world\n".repeat(1000);
    let z = t.join("in.zip");
    deflate_zip(
        &z,
        &[
            ("sub/Hello.txt", &hello),
            ("Dir/", b""),
            ("sub/", b""),
            ("a", b"x"),
            ("Empty", b""),
        ],
    );
    assert!(!ok(bin().arg("verify").arg(&z)));
    assert!(ok(bin().arg("pack").arg(&z)));
    assert!(ok(bin().args(["verify", "--deep"]).arg(&z)));

    // the same tree packed from a directory gives the same bytes
    let src = t.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::create_dir_all(src.join("Dir")).unwrap();
    fs::write(src.join("sub/Hello.txt"), &hello).unwrap();
    fs::write(src.join("a"), b"x").unwrap();
    fs::write(src.join("Empty"), b"").unwrap();
    let z2 = t.join("dir.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z2)));
    assert_eq!(fs::read(&z).unwrap(), fs::read(&z2).unwrap());
}

#[test]
fn batch_in_place_rerun() {
    let t = tmp("batch");
    fs::create_dir_all(t.join("set1")).unwrap();
    fs::write(t.join("set1/f"), b"one").unwrap();
    deflate_zip(&t.join("set2.zip"), &[("g", b"two")]);
    assert!(ok(bin().arg("batch").arg("--pack-dirs").arg(&t)));
    assert!(ok(bin()
        .arg("verify")
        .arg(t.join("set1.zip"))
        .arg(t.join("set2.zip"))));
    // second run: set1/ and its own set1.zip are not a conflict, nothing to do
    let out = bin()
        .arg("batch")
        .arg("--pack-dirs")
        .arg(&t)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .matches("skipped")
            .count(),
        2
    );
    // a changed directory is packed again
    fs::write(t.join("set1/h"), b"new").unwrap();
    let out = bin()
        .arg("batch")
        .arg("--pack-dirs")
        .arg(&t)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("packed"));
}

#[test]
fn batch_to_outdir_rerun() {
    let t = tmp("batch_outdir");
    let src = t.join("src");
    fs::create_dir_all(src.join("set1")).unwrap();
    fs::write(src.join("set1/f"), b"one").unwrap();
    deflate_zip(&src.join("set2.zip"), &[("g", b"two")]);
    let out = t.join("out");
    assert!(ok(bin()
        .arg("batch")
        .arg("--pack-dirs")
        .arg(&src)
        .arg("-o")
        .arg(&out)));
    assert!(ok(bin()
        .arg("verify")
        .arg(out.join("set1.zip"))
        .arg(out.join("set2.zip"))));
    // second run: both outputs are up to date, the deflate zip is not re-encoded
    let r = bin()
        .arg("batch")
        .arg("--pack-dirs")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(r.status.success());
    assert_eq!(
        String::from_utf8_lossy(&r.stdout)
            .matches("skipped")
            .count(),
        2
    );
}

fn nested_tree(src: &Path) {
    fs::create_dir_all(src.join("a/b")).unwrap();
    fs::create_dir_all(src.join("set")).unwrap();
    fs::write(src.join("set/f"), b"loose").unwrap();
    deflate_zip(&src.join("a/b/x.zip"), &[("g", b"two")]);
    deflate_zip(&src.join("top.zip"), &[("h", b"three")]);
}

fn skipped(out: &std::process::Output) -> usize {
    String::from_utf8_lossy(&out.stdout)
        .matches("skipped")
        .count()
}

#[test]
fn batch_recursive() {
    let t = tmp("batch_rec");
    nested_tree(&t);
    assert!(ok(bin().arg("batch").arg(&t)));
    assert!(ok(bin()
        .arg("verify")
        .arg(t.join("a/b/x.zip"))
        .arg(t.join("top.zip"))));
    // directories are never packed without --pack-dirs
    assert!(!t.join("set.zip").exists());
    assert!(!t.join("a.zip").exists());
    let out = bin().arg("batch").arg(&t).output().unwrap();
    assert!(out.status.success());
    assert_eq!(skipped(&out), 2);
}

#[test]
fn batch_recursive_outdir() {
    let t = tmp("batch_rec_outdir");
    let src = t.join("src");
    nested_tree(&src);
    // output inside the input tree is not walked as input on a rerun
    let out = src.join("out");
    let run = || {
        bin()
            .arg("batch")
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    assert!(ok(bin()
        .arg("verify")
        .arg(out.join("a/b/x.zip"))
        .arg(out.join("top.zip"))));
    let r = run();
    assert!(r.status.success());
    assert_eq!(skipped(&r), 2);
    assert!(!out.join("out").exists());
}

#[test]
fn batch_multiple_folders() {
    let t = tmp("batch_multi");
    let (a, b) = (t.join("a"), t.join("b"));
    nested_tree(&a);
    nested_tree(&b);
    assert!(ok(bin().arg("batch").arg(&a).arg(&b)));
    for d in [&a, &b] {
        assert!(ok(bin()
            .arg("verify")
            .arg(d.join("a/b/x.zip"))
            .arg(d.join("top.zip"))));
    }
    let out = bin().arg("batch").arg(&a).arg(&b).output().unwrap();
    assert!(out.status.success());
    assert_eq!(skipped(&out), 4);
}

#[test]
fn batch_multiple_folders_outdir() {
    let t = tmp("batch_multi_outdir");
    let (a, b, out) = (t.join("a"), t.join("b"), t.join("out"));
    fs::create_dir_all(a.join("set1")).unwrap();
    fs::write(a.join("set1/f"), b"one").unwrap();
    fs::create_dir_all(&b).unwrap();
    deflate_zip(&b.join("set2.zip"), &[("g", b"two")]);
    assert!(ok(bin()
        .arg("batch")
        .arg("--pack-dirs")
        .arg(&a)
        .arg(&b)
        .arg("-o")
        .arg(&out)));
    assert!(ok(bin()
        .arg("verify")
        .arg(out.join("set1.zip"))
        .arg(out.join("set2.zip"))));

    // the same name in two folders merged into one outdir
    let (c, d, out2) = (t.join("c"), t.join("d"), t.join("out2"));
    fs::create_dir_all(&c).unwrap();
    fs::create_dir_all(&d).unwrap();
    deflate_zip(&c.join("x.zip"), &[("g", b"two")]);
    deflate_zip(&d.join("x.zip"), &[("h", b"three")]);
    let r = bin()
        .arg("batch")
        .arg(&c)
        .arg(&d)
        .arg("-o")
        .arg(&out2)
        .output()
        .unwrap();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("both map to"));
    assert!(!out2.join("x.zip").exists());

    // the same folder twice
    let r = bin().arg("batch").arg(&c).arg(&c).output().unwrap();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("both map to"));
}

#[test]
fn batch_missing_folder_fails() {
    let t = tmp("batch_missing");
    let typo = t.join("typo");
    let fails = |cmd: &mut Command| {
        let r = cmd.output().unwrap();
        assert!(!r.status.success());
        assert!(String::from_utf8_lossy(&r.stderr).contains("not a directory"));
    };
    fails(bin().arg("batch").arg(&typo));
    fails(bin().args(["batch", "--pack-dirs"]).arg(&typo));
    fails(bin().arg("batch").arg(&typo).arg("-o").arg(t.join("out")));
    // neither the folder nor the output folder was created
    assert_eq!(fs::read_dir(&t).unwrap().count(), 0);

    // an empty folder is not an error, but the run says why it did nothing
    let r = bin().arg("batch").arg(&t).output().unwrap();
    assert!(r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("no zip found"));
}

/// Incompressible file just under 4 GiB: compressed size crosses 4 GiB, so
/// the data is moved down for a zip64 extra and the zip64 EOCD is used.
/// Slow and needs ~13 GB of disk: `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn zip64_compressed_size_only() {
    let t = tmp("zip64");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    let mut f = std::io::BufWriter::new(fs::File::create(src.join("big.bin")).unwrap());
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut left = 0xFFFF_FFFEu64;
    while left > 0 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = left.min(8) as usize;
        f.write_all(&x.to_le_bytes()[..n]).unwrap();
        left -= n as u64;
    }
    drop(f);
    let z = t.join("o.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    assert!(ok(bin().arg("verify").arg(&z)));
    let x = t.join("x");
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(fs::metadata(x.join("big.bin")).unwrap().len(), 0xFFFF_FFFE);
    fs::remove_dir_all(&x).unwrap();
    // a new small file: the zip64 entry is copied, its data moved up 20
    // bytes as written. "copied" is printed only once the data decoded to
    // the right CRC, so a plain verify of the layout is enough
    fs::write(src.join("a"), b"small").unwrap();
    let out = bin()
        .args(["pack", "-v"])
        .arg(&src)
        .arg("-o")
        .arg(&z)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("big.bin (4096.0 MiB) copied"));
    assert!(ok(bin().arg("verify").arg(&z)));
    fs::remove_file(src.join("a")).unwrap();
    fs::remove_file(&z).unwrap();
    // deflate stored blocks also grow random data past 4 GiB
    let z = t.join("tz.zip");
    assert!(tz_pack(&src, &z).status.success());
    assert!(ok(bin().arg("verify").arg(&z)));
    let mut h = [0u8; 30];
    std::io::Read::read_exact(&mut fs::File::open(&z).unwrap(), &mut h).unwrap();
    assert_eq!(&h[4..6], &45u16.to_le_bytes());
    assert_eq!(&h[8..10], &8u16.to_le_bytes());
    assert_eq!(&h[28..30], &20u16.to_le_bytes());
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(fs::metadata(x.join("big.bin")).unwrap().len(), 0xFFFF_FFFE);
    fs::remove_dir_all(&t).unwrap();
}

/// Ctrl+C during an encode removes the unfinished temp file.
#[cfg(unix)]
#[test]
fn sigint_removes_temp_file() {
    let t = tmp("sigint");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    // incompressible, so level 19 takes a few seconds
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let data: Vec<u8> = (0..64 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    fs::write(src.join("big.bin"), data).unwrap();
    let out = t.join("out");
    fs::create_dir_all(&out).unwrap();
    let mut child = bin()
        .arg("pack")
        .arg("-j1")
        .arg(&src)
        .arg("-o")
        .arg(out.join("o.zip"))
        .spawn()
        .unwrap();
    let list = || fs::read_dir(&out).unwrap().count();
    let start = std::time::Instant::now();
    while list() == 0 {
        assert!(start.elapsed().as_secs() < 30, "temp file never appeared");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        Command::new("kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let st = child.wait().unwrap();
    assert_eq!(st.code(), Some(130));
    assert_eq!(list(), 0);
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn chmod(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
}

#[cfg(unix)]
#[test]
fn rewrite_keeps_mode() {
    let t = tmp("keep_mode");
    // encode path: in-place rewrite of a deflate zip
    let z = t.join("d.zip");
    deflate_zip(&z, &[("f", b"data")]);
    chmod(&z, 0o640);
    assert!(ok(bin().arg("pack").arg(&z)));
    assert!(ok(bin().arg("verify").arg(&z)));
    assert_eq!(mode(&z), 0o640);
    // copy path: valid zip over an existing file
    let other = t.join("other.zip");
    fs::write(&other, b"old").unwrap();
    chmod(&other, 0o600);
    assert!(ok(bin().arg("pack").arg(&z).arg("-o").arg(&other)));
    assert_eq!(fs::read(&other).unwrap(), fs::read(&z).unwrap());
    assert_eq!(mode(&other), 0o600);
}

#[cfg(unix)]
#[test]
fn pack_rejects_symlinks() {
    use std::os::unix::fs::symlink;
    let t = tmp("pack_symlinks");
    fs::write(t.join("secret"), b"secret").unwrap();
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("f"), b"x").unwrap();
    symlink(t.join("secret"), src.join("link")).unwrap();
    let z = t.join("o.zip");
    assert!(!ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    assert!(!z.exists());
    // a link loop fails cleanly instead of overflowing the stack
    fs::remove_file(src.join("link")).unwrap();
    symlink("..", src.join("loop")).unwrap();
    let out = bin()
        .arg("pack")
        .arg(&src)
        .arg("-o")
        .arg(&z)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!z.exists());
}

#[cfg(unix)]
#[test]
fn batch_rejects_symlinks() {
    use std::os::unix::fs::symlink;
    let t = tmp("batch_symlinks");
    deflate_zip(&t.join("outside.zip"), &[("g", b"two")]);
    let before = fs::read(t.join("outside.zip")).unwrap();
    let folder = t.join("folder");
    fs::create_dir_all(&folder).unwrap();
    deflate_zip(&folder.join("a.zip"), &[("f", b"one")]);
    // a link to something that is not a zip or a directory is ignored
    symlink(t.join("outside.zip"), folder.join("readme.txt")).unwrap();
    assert!(ok(bin().arg("batch").arg(&folder)));
    symlink(t.join("outside.zip"), folder.join("l.zip")).unwrap();
    assert!(!ok(bin().arg("batch").arg("--force").arg(&folder)));
    assert!(!ok(bin().arg("batch").arg("--pack-dirs").arg(&folder)));
    fs::remove_file(folder.join("l.zip")).unwrap();
    symlink(&t, folder.join("d")).unwrap();
    assert!(!ok(bin().arg("batch").arg(&folder)));
    assert!(!ok(bin().arg("batch").arg("--pack-dirs").arg(&folder)));
    assert!(!folder.join("d.zip").exists());
    assert_eq!(fs::read(t.join("outside.zip")).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn extract_refuses_symlinked_parent() {
    use std::os::unix::fs::symlink;
    let t = tmp("extract_link_parent");
    let src = t.join("src");
    fs::create_dir_all(src.join("a")).unwrap();
    fs::write(src.join("a/f"), b"x").unwrap();
    let z = t.join("o.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    let victim = t.join("victim_dir");
    fs::create_dir_all(&victim).unwrap();
    let out = t.join("out");
    fs::create_dir_all(&out).unwrap();
    symlink(&victim, out.join("a")).unwrap();
    assert!(!ok(bin().arg("extract").arg(&z).arg("-o").arg(&out)));
    assert_eq!(fs::read_dir(&victim).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn extract_replaces_symlinked_file() {
    use std::os::unix::fs::symlink;
    let t = tmp("extract_link_file");
    let src = t.join("src");
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(src.join("f"), b"new").unwrap();
    fs::write(src.join("d/g"), b"deep").unwrap();
    let z = t.join("o.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    let victim = t.join("victim");
    fs::write(&victim, b"SECRET").unwrap();
    let out = t.join("out");
    fs::create_dir_all(&out).unwrap();
    symlink(&victim, out.join("f")).unwrap();
    // a hard link to someone else's file is replaced too, not written into
    fs::create_dir_all(out.join("d")).unwrap();
    fs::hard_link(&victim, out.join("d/g")).unwrap();
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&out)));
    assert!(fs::symlink_metadata(out.join("f")).unwrap().is_file());
    assert_eq!(fs::read(out.join("f")).unwrap(), b"new");
    assert_eq!(fs::read(out.join("d/g")).unwrap(), b"deep");
    assert_eq!(fs::read(&victim).unwrap(), b"SECRET");
}

/// Offset of the first central directory header.
fn central(b: &[u8]) -> usize {
    find(b, b"PK\x01\x02")
}

#[test]
fn bomb_is_bounded() {
    let t = tmp("bomb");
    let src = t.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("f"), vec![0u8; 10 << 20]).unwrap();
    let z = t.join("bomb.zip");
    assert!(ok(bin().arg("pack").arg(&src).arg("-o").arg(&z)));
    // the central directory now claims 100 bytes for 10 MiB of zstd data
    let mut b = fs::read(&z).unwrap();
    let c = central(&b);
    b[c + 24..c + 28].copy_from_slice(&100u32.to_le_bytes());
    fs::write(&z, &b).unwrap();
    let x = t.join("x");
    let out = bin()
        .arg("extract")
        .arg(&z)
        .arg("-o")
        .arg(&x)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("more data than its recorded size"));
    assert_eq!(fs::read_dir(&x).unwrap().count(), 0);
    let o = t.join("o.zip");
    let out = bin()
        .arg("pack")
        .arg(&z)
        .arg("-o")
        .arg(&o)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("more data than its recorded size"));
    assert!(!o.exists());
}

#[test]
fn extract_keeps_old_file_on_error() {
    let t = tmp("extract_keeps_old");
    let z = t.join("f.zip");
    // stored entry whose CRC does not match its data
    raw_zip(&z, "f", 0, 3, 0, b"new");
    let x = t.join("x");
    fs::create_dir_all(&x).unwrap();
    fs::write(x.join("f"), b"old").unwrap();
    assert!(!ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    // the copy that was there is intact, and no temp file is left
    assert_eq!(fs::read(x.join("f")).unwrap(), b"old");
    assert_eq!(fs::read_dir(&x).unwrap().count(), 1);
    // a good entry replaces it
    raw_zip(&z, "f", 0, 3, crc32fast::hash(b"new"), b"new");
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(fs::read(x.join("f")).unwrap(), b"new");
    assert_eq!(fs::read_dir(&x).unwrap().count(), 1);
}

#[test]
fn extract_refuses_duplicate_names() {
    let t = tmp("extract_dup");
    let z = t.join("dup.zip");
    deflate_zip(&z, &[("same", b"first"), ("same", b"second")]);
    let x = t.join("x");
    let out = bin()
        .arg("extract")
        .arg(&z)
        .arg("-o")
        .arg(&x)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate entry"));
    assert!(!x.exists());
}

/// Where the file system ignores case, `a.txt` and `A.TXT` are one file:
/// the second entry must not replace the first.
#[test]
fn extract_refuses_names_of_one_file() {
    let t = tmp("extract_one_file");
    let z = t.join("in.zip");
    deflate_zip(&z, &[("a.txt", b"lower"), ("A.TXT", b"upper")]);
    fs::write(t.join("probe"), b"").unwrap();
    let ignores_case = t.join("PROBE").exists();
    let x = t.join("x");
    let out = bin()
        .arg("extract")
        .arg(&z)
        .arg("-o")
        .arg(&x)
        .output()
        .unwrap();
    assert_eq!(fs::read(x.join("a.txt")).unwrap(), b"lower");
    if ignores_case {
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("same file as \"a.txt\""));
        assert_eq!(fs::read_dir(&x).unwrap().count(), 1);
    } else {
        assert!(out.status.success());
        assert_eq!(fs::read(x.join("A.TXT")).unwrap(), b"upper");
    }
    // again over the files of the first run: these are replaced
    let again = bin()
        .arg("extract")
        .arg(&z)
        .arg("-o")
        .arg(&x)
        .output()
        .unwrap();
    assert_eq!(again.status.success(), !ignores_case);
    assert_eq!(fs::read(x.join("a.txt")).unwrap(), b"lower");
}

#[test]
fn entry_count_mismatch_refused() {
    let t = tmp("count_mismatch");
    let z = t.join("in.zip");
    deflate_zip(&z, &[("a", b"1"), ("b", b"2"), ("c", b"3")]);
    // the end record now counts 2 of the 3 central directory headers
    let mut b = fs::read(&z).unwrap();
    let e = b.len() - 22;
    b[e + 8..e + 12].copy_from_slice(&[2, 0, 2, 0]);
    fs::write(&z, &b).unwrap();
    for cmd in ["pack", "verify", "extract"] {
        let out = bin().arg(cmd).arg(&z).current_dir(&t).output().unwrap();
        assert!(!out.status.success(), "{cmd}");
        let text = [out.stdout, out.stderr].concat();
        assert!(
            String::from_utf8_lossy(&text).contains("entry count does not match"),
            "{cmd}"
        );
    }
    // the in-place rewrite did not touch the zip, extract wrote nothing
    assert_eq!(fs::read(&z).unwrap(), b);
    assert_eq!(fs::read_dir(&t).unwrap().count(), 1);
}

/// Zip with one entry whose data is `body` as is, under `method`.
fn raw_zip(path: &Path, name: &str, method: u16, unc: u32, crc: u32, body: &[u8]) {
    let mut common = Vec::new();
    common.extend_from_slice(&20u16.to_le_bytes());
    common.extend_from_slice(&0u16.to_le_bytes());
    common.extend_from_slice(&method.to_le_bytes());
    common.extend_from_slice(&[0; 4]);
    common.extend_from_slice(&crc.to_le_bytes());
    common.extend_from_slice(&(body.len() as u32).to_le_bytes());
    common.extend_from_slice(&unc.to_le_bytes());
    common.extend_from_slice(&(name.len() as u16).to_le_bytes());
    common.extend_from_slice(&0u16.to_le_bytes());
    let mut out = b"PK\x03\x04".to_vec();
    out.extend_from_slice(&common);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(body);
    let start = out.len() as u32;
    let mut cd = b"PK\x01\x02".to_vec();
    cd.extend_from_slice(&20u16.to_le_bytes());
    cd.extend_from_slice(&common);
    cd.extend_from_slice(&[0; 14]); // comment len, disk, attrs, offset 0
    cd.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&cd);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    fs::write(path, out).unwrap();
}

#[test]
fn huge_zstd_window_refused() {
    let t = tmp("huge_window");
    // frame header with windowLog 30 (1 GiB), then one raw last block "x"
    let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 20 << 3, 0x09, 0, 0, b'x'];
    let z = t.join("w.zip");
    raw_zip(&z, "f", 93, 1, crc32fast::hash(b"x"), &frame);
    let x = t.join("x");
    let out = bin()
        .arg("extract")
        .arg(&z)
        .arg("-o")
        .arg(&x)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("zstd:"));
    assert_eq!(fs::read_dir(&x).unwrap().count(), 0);
    // the same frame with windowLog 23 is fine
    let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 13 << 3, 0x09, 0, 0, b'x'];
    raw_zip(&z, "f", 93, 1, crc32fast::hash(b"x"), &frame);
    assert!(ok(bin().arg("extract").arg(&z).arg("-o").arg(&x)));
    assert_eq!(fs::read(x.join("f")).unwrap(), b"x");
}

#[test]
fn extract_refuses_file_dir_clash() {
    let t = tmp("file_dir_clash");
    let cases: [&[(&str, &[u8])]; 2] = [&[("e", b"x"), ("e/", b"")], &[("e", b"x"), ("e/x", b"y")]];
    for (i, files) in cases.iter().enumerate() {
        let z = t.join(format!("{i}.zip"));
        deflate_zip(&z, files);
        let x = t.join(format!("x{i}"));
        let out = bin()
            .arg("extract")
            .arg(&z)
            .arg("-o")
            .arg(&x)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("both a file and a directory"));
        assert!(!x.exists());
    }
}

#[test]
fn pack_refuses_output_inside_input() {
    let t = tmp("pack_self");
    let src = t.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("f"), b"x").unwrap();
    for o in ["self.zip", "sub/self.zip"] {
        let out = bin()
            .arg("pack")
            .arg(&src)
            .arg("-o")
            .arg(src.join(o))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("inside the packed directory"));
    }
    assert_eq!(files(&src).len(), 2); // f and sub/, nothing new
    // batch --pack-dirs with the output folder among the packed ones
    let folder = t.join("folder");
    fs::create_dir_all(folder.join("X")).unwrap();
    fs::write(folder.join("X/f"), b"x").unwrap();
    assert!(!ok(bin()
        .args(["batch", "--pack-dirs"])
        .arg(&folder)
        .arg("-o")
        .arg(folder.join("X"))));
    assert!(!folder.join("X/X.zip").exists());
}

#[cfg(unix)]
#[test]
fn fifo_named_zip_does_not_hang() {
    let t = tmp("fifo_zip");
    let folder = t.join("folder");
    fs::create_dir_all(&folder).unwrap();
    let fifo = folder.join("evil.zip");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // each would block forever in open(2) if the FIFO were opened plainly
    assert!(ok(bin().arg("batch").arg(&folder)));
    assert!(ok(bin().args(["batch", "--pack-dirs"]).arg(&folder)));
    assert!(!ok(bin().arg("verify").arg(&fifo)));
    assert!(!ok(bin().arg("pack").arg(&fifo)));
    assert!(!ok(bin()
        .arg("extract")
        .arg(&fifo)
        .arg("-o")
        .arg(t.join("x"))));
}

/// Two dirs and a deflate zip for the multi-input pack tests.
fn pack_inputs(t: &Path) -> [PathBuf; 3] {
    let (a, b, z) = (t.join("a"), t.join("b"), t.join("z.zip"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("f"), b"one").unwrap();
    fs::write(b.join("g"), b"two".repeat(1000)).unwrap();
    deflate_zip(&z, &[("h", b"three")]);
    [a, b, z]
}

#[test]
fn pack_multiple() {
    let t = tmp("pack_multiple");
    let [a, b, z] = pack_inputs(&t);
    assert!(ok(bin().arg("pack").arg(&a).arg(&b).arg(&z)));
    for out in [t.join("a.zip"), t.join("b.zip"), z] {
        assert!(ok(bin().args(["verify", "--deep"]).arg(&out)), "{out:?}");
    }
}

#[test]
fn pack_multiple_outdir() {
    let t = tmp("pack_multiple_outdir");
    let [a, b, z] = pack_inputs(&t);
    let before = fs::read(&z).unwrap();
    let out = t.join("out");
    assert!(ok(bin()
        .arg("pack")
        .arg(&a)
        .arg(&b)
        .arg(&z)
        .arg("-o")
        .arg(&out)));
    for name in ["a.zip", "b.zip", "z.zip"] {
        assert!(
            ok(bin().args(["verify", "--deep"]).arg(out.join(name))),
            "{name}"
        );
    }
    assert_eq!(fs::read(&z).unwrap(), before);
    assert!(!t.join("a.zip").exists() && !t.join("b.zip").exists());
}

#[test]
fn pack_multiple_collisions() {
    let t = tmp("pack_multiple_collisions");
    let c = t.join("c");
    fs::create_dir_all(c.join("x")).unwrap();
    fs::create_dir_all(t.join("d/x")).unwrap();
    fs::write(c.join("x/f"), b"one").unwrap();
    fs::write(t.join("d/x/g"), b"two").unwrap();
    let fails = |cmd: &mut Command| {
        let r = cmd.output().unwrap();
        assert!(!r.status.success());
        assert!(String::from_utf8_lossy(&r.stderr).contains("both map to"));
    };

    // the same dir twice
    fails(bin().arg("pack").arg(&c).arg(&c));
    assert!(!t.join("c.zip").exists());

    // a dir and its default output, from the current directory
    deflate_zip(&t.join("c.zip"), &[("h", b"three")]);
    fails(bin().args(["pack", "c", "c.zip"]).current_dir(&t));
    fs::remove_file(t.join("c.zip")).unwrap();

    // two dirs with the same name into one outdir
    let out = t.join("out");
    fails(
        bin()
            .arg("pack")
            .arg(c.join("x"))
            .arg(t.join("d/x"))
            .arg("-o")
            .arg(&out),
    );
    assert!(!out.join("x.zip").exists());
}
