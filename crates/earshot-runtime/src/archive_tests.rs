//! Tests of archive extraction with damaged and hostile archives. Names carry `download` so
//! `cargo test -p earshot-runtime download` selects them.

use std::io::Write;
use std::path::Path;

use earshot_core::asr::CancelToken;
use sha2::{Digest, Sha256};

use super::*;
use crate::catalogue::{Extract, Member};
use crate::testutil::TempDir;

fn sha(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn member(path: &str, bytes: &[u8]) -> Member {
    Member {
        path: path.to_owned(),
        sha256: sha(bytes),
        size: bytes.len() as u64,
    }
}

fn extract(format: &str, members: Vec<Member>) -> Extract {
    Extract {
        format: format.to_owned(),
        into: "out".to_owned(),
        members,
    }
}

/// One test zip entry: name, data, deflate, unix mode, general-purpose flags.
type ZipSpec<'a> = (&'a str, &'a [u8], bool, u32, u16);

const FILE: u32 = 0o100_644;
const LINK: u32 = 0o120_777;
const DIR: u32 = 0o040_755;

/// A minimal zip writer (APPNOTE local headers, central directory, end record), the one of
/// xtask's archive tests: the test must be able to write what a real writer refuses to.
fn zip_bytes(entries: &[ZipSpec<'_>]) -> Vec<u8> {
    let (mut out, mut cd) = (Vec::new(), Vec::new());
    for (name, data, deflate, mode, flags) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let body = if *deflate {
            let mut e =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = u32::try_from(out.len()).unwrap();
        let n = u16::try_from(name.len()).unwrap();
        let (body_len, data_len) = (
            u32::try_from(body.len()).unwrap(),
            u32::try_from(data.len()).unwrap(),
        );
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&body_len.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&body);
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&0x031eu16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&flags.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&[0; 4]);
        cd.extend_from_slice(&crc.sum().to_le_bytes());
        cd.extend_from_slice(&body_len.to_le_bytes());
        cd.extend_from_slice(&data_len.to_le_bytes());
        cd.extend_from_slice(&n.to_le_bytes());
        cd.extend_from_slice(&[0; 8]);
        cd.extend_from_slice(&(mode << 16).to_le_bytes());
        cd.extend_from_slice(&offset.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let (cd_offset, cd_len, count) = (
        u32::try_from(out.len()).unwrap(),
        u32::try_from(cd.len()).unwrap(),
        u16::try_from(entries.len()).unwrap(),
    );
    out.extend_from_slice(&cd);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn tar_header(path: &str, len: usize, kind: tar::EntryType) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    // The raw name, so the builder's own path checks cannot hide a bad entry.
    let name = &mut h.as_old_mut().name;
    name[..path.len()].copy_from_slice(path.as_bytes());
    h.set_size(len as u64);
    h.set_entry_type(kind);
    h.set_mode(0o644);
    h.set_cksum();
    h
}

fn tar_bz2(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data, kind) in entries {
        b.append(&tar_header(path, data.len(), *kind), *data)
            .unwrap();
    }
    let raw = b.into_inner().unwrap();
    let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    e.write_all(&raw).unwrap();
    e.finish().unwrap()
}

/// Extracts `archive` under a fresh directory; returns the result and the directory.
fn run(tag: &str, x: &Extract, archive: &[u8]) -> (Result<(), ExtractError>, TempDir) {
    let dir = TempDir::new(&format!("archive-{tag}"));
    let file = dir.0.join("archive.bin");
    std::fs::write(&file, archive).unwrap();
    let out = extract_verified(
        &file,
        x,
        &dir.0.join("out"),
        "archivesha",
        &CancelToken::new(),
    );
    (out, dir)
}

/// Refused, and nothing is published or left behind.
fn assert_refused(tag: &str, x: &Extract, archive: &[u8], words: &str) {
    let (out, dir) = run(tag, x, archive);
    let e = out.expect_err(tag);
    assert!(matches!(e, ExtractError::Refused(_)), "{tag}: {e:?}");
    assert!(e.to_string().contains(words), "{tag}: {e}");
    assert!(!dir.0.join("out").exists(), "{tag}: nothing is published");
    assert!(
        !dir.0.join("out.tmp").exists(),
        "{tag}: the temp is removed"
    );
    assert!(!dir.0.join("evil.txt").exists(), "{tag}: nothing escaped");
}

#[test]
fn download_archive_zip_extracts_only_the_pinned_members_with_xtasks_marker() {
    let big = b"onnxruntime ".repeat(5000);
    let z = zip_bytes(&[
        ("top/", b"", false, DIR, 0),
        ("top/LICENSE", b"mit", false, FILE, 0),
        ("top\\lib\\a.dll", &big, true, FILE, 0),
        ("top/lib/a.pdb", b"not wanted", true, FILE, 0),
    ]);
    let x = extract(
        "zip",
        vec![member("top/lib/a.dll", &big), member("top/LICENSE", b"mit")],
    );
    let (out, dir) = run("zip-ok", &x, &z);
    out.unwrap();
    let into = dir.0.join("out");
    assert_eq!(std::fs::read(into.join("top/lib/a.dll")).unwrap(), big);
    assert_eq!(std::fs::read(into.join("top/LICENSE")).unwrap(), b"mit");
    assert!(!into.join("top/lib/a.pdb").exists(), "not a member");
    assert!(!dir.0.join("out.tmp").exists());
    // xtask's `archive::parse_marker` reads this: line 1 the archive's SHA-256, then
    // `<sha256> <size> <path>` per file, sorted by path, `/`-separated.
    assert_eq!(
        std::fs::read_to_string(into.join(MARKER)).unwrap(),
        format!(
            "archivesha\n{} 3 top/LICENSE\n{} {} top/lib/a.dll\n",
            sha(b"mit"),
            sha(&big),
            big.len()
        )
    );
}

#[test]
fn download_archive_tar_bz2_extracts_only_the_pinned_members() {
    let t = tar_bz2(&[
        ("top/", b"", tar::EntryType::Directory),
        ("top/model.onnx", b"weights", tar::EntryType::Regular),
        ("top/README.md", b"not wanted", tar::EntryType::Regular),
    ]);
    let x = extract("tar.bz2", vec![member("top/model.onnx", b"weights")]);
    let (out, dir) = run("tar-ok", &x, &t);
    out.unwrap();
    let into = dir.0.join("out");
    assert_eq!(
        std::fs::read(into.join("top/model.onnx")).unwrap(),
        b"weights"
    );
    assert!(!into.join("top/README.md").exists());
    assert!(into.join(MARKER).is_file());
}

#[test]
fn download_archive_refuses_an_unknown_format_and_an_archive_with_no_members() {
    let z = zip_bytes(&[("a", b"x", false, FILE, 0)]);
    assert_refused("rar", &extract("rar", vec![member("a", b"x")]), &z, "rar");
    assert_refused(
        "targz",
        &extract("tar.gz", vec![member("a", b"x")]),
        &z,
        "tar.gz",
    );
    assert_refused("none", &extract("zip", Vec::new()), &z, "pins no file");
}

#[test]
fn download_archive_zip_refuses_escaping_names_anywhere_in_the_archive() {
    let wanted = member("top/a", b"x");
    for (i, bad) in [
        "../evil.txt",
        "top/../../evil.txt",
        "/abs.txt",
        "\\abs.txt",
        "C:\\evil.txt",
        "C:evil.txt",
        "top/a:stream",
    ]
    .iter()
    .enumerate()
    {
        // A hostile name refuses the archive even when it is not a file the app wants.
        let z = zip_bytes(&[("top/a", b"x", false, FILE, 0), (bad, b"x", false, FILE, 0)]);
        assert_refused(
            &format!("zip-escape-{i}"),
            &extract("zip", vec![wanted.clone()]),
            &z,
            "archive entry",
        );
    }
}

#[test]
fn download_archive_tar_refuses_escaping_names_and_links_anywhere_in_the_archive() {
    let wanted = member("top/a", b"x");
    for (i, bad) in ["../evil.txt", "top/../../evil.txt", "/abs.txt"]
        .iter()
        .enumerate()
    {
        let t = tar_bz2(&[
            ("top/a", b"x", tar::EntryType::Regular),
            (bad, b"x", tar::EntryType::Regular),
        ]);
        assert_refused(
            &format!("tar-escape-{i}"),
            &extract("tar.bz2", vec![wanted.clone()]),
            &t,
            "archive entry",
        );
    }
    for (i, kind) in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Fifo,
    ]
    .into_iter()
    .enumerate()
    {
        let t = tar_bz2(&[
            ("top/a", b"x", tar::EntryType::Regular),
            ("top/link", b"", kind),
        ]);
        assert_refused(
            &format!("tar-link-{i}"),
            &extract("tar.bz2", vec![wanted.clone()]),
            &t,
            "not a regular file",
        );
    }
}

#[test]
fn download_archive_zip_refuses_links_encryption_and_other_compression() {
    let wanted = || extract("zip", vec![member("top/a", b"x")]);
    let ok = ("top/a", &b"x"[..], false, FILE, 0u16);
    assert_refused(
        "zip-link",
        &wanted(),
        &zip_bytes(&[ok, ("top/link", b"x", false, LINK, 0)]),
        "symbolic link",
    );
    assert_refused(
        "zip-encrypted",
        &wanted(),
        &zip_bytes(&[("top/a", b"x", false, FILE, 1)]),
        "encrypted",
    );
    // Method 12 is bzip2: rewrite the method field of the central directory only, which is
    // the one the reader believes.
    let mut z = zip_bytes(&[ok]);
    let cd = z
        .windows(4)
        .position(|w| w == 0x0201_4b50u32.to_le_bytes())
        .unwrap();
    z[cd + 10] = 12;
    assert_refused("zip-method", &wanted(), &z, "compression method 12");
}

#[test]
fn download_archive_refuses_duplicate_names_also_by_case() {
    let wanted = || member("top/a.txt", b"one");
    let zip = |entries: &[ZipSpec<'_>]| zip_bytes(entries);
    assert_refused(
        "zip-dup",
        &extract("zip", vec![wanted()]),
        &zip(&[
            ("top/a.txt", b"one", false, FILE, 0),
            ("top/a.txt", b"two", false, FILE, 0),
        ]),
        "twice",
    );
    // One file on Windows: the second must not replace the first unseen.
    assert_refused(
        "zip-dup-case",
        &extract("zip", vec![wanted()]),
        &zip(&[
            ("top/a.txt", b"one", false, FILE, 0),
            ("TOP/A.TXT", b"two", false, FILE, 0),
        ]),
        "twice",
    );
    assert_refused(
        "zip-dup-spelling",
        &extract("zip", vec![wanted()]),
        &zip(&[
            ("top/a.txt", b"one", false, FILE, 0),
            ("top\\.\\a.txt", b"two", false, FILE, 0),
        ]),
        "twice",
    );
    assert_refused(
        "tar-dup",
        &extract("tar.bz2", vec![wanted()]),
        &tar_bz2(&[
            ("top/a.txt", b"one", tar::EntryType::Regular),
            ("top/a.txt", b"two", tar::EntryType::Regular),
        ]),
        "twice",
    );
}

#[test]
fn download_archive_never_writes_past_the_declared_size() {
    // 1 MB of deflated data whose headers declare 10 bytes (a zip bomb's shape).
    let big = vec![b'x'; 1 << 20];
    let mut z = zip_bytes(&[("top/bomb.bin", &big, true, FILE, 0)]);
    let declared = 10u32.to_le_bytes();
    let size = u32::try_from(big.len()).unwrap().to_le_bytes();
    let mut patched = 0;
    let mut i = 0;
    while i + 4 <= z.len() {
        if z[i..i + 4] == size {
            z[i..i + 4].copy_from_slice(&declared);
            patched += 1;
        }
        i += 1;
    }
    assert_eq!(patched, 2);
    let x = extract("zip", vec![member("top/bomb.bin", &big[..10])]);
    assert_refused("zip-bomb", &x, &z, "past its size");

    // A tar member larger than the manifest says stops at the manifest's size.
    let t = tar_bz2(&[("top/big.bin", &big, tar::EntryType::Regular)]);
    let x = extract("tar.bz2", vec![member("top/big.bin", &big[..10])]);
    assert_refused("tar-bomb", &x, &t, "past its size");
}

#[test]
fn download_archive_refuses_a_zip_whose_size_disagrees_with_the_manifest() {
    let z = zip_bytes(&[("top/a", b"hello", false, FILE, 0)]);
    let x = extract("zip", vec![member("top/a", b"hell")]);
    assert_refused("zip-size", &x, &z, "manifest says");
}

#[test]
fn download_archive_refuses_a_member_the_manifest_does_not_recognise() {
    // Same size, other bytes: only the digest notices.
    let z = zip_bytes(&[("top/a", b"hello", false, FILE, 0)]);
    let x = extract("zip", vec![member("top/a", b"jello")]);
    assert_refused("zip-sha", &x, &z, "sha256");
    let t = tar_bz2(&[("top/a", b"hello", tar::EntryType::Regular)]);
    let x = extract("tar.bz2", vec![member("top/a", b"jello")]);
    assert_refused("tar-sha", &x, &t, "sha256");
}

#[test]
fn download_archive_refuses_a_missing_member_and_damaged_archives() {
    let z = zip_bytes(&[("top/a", b"hello", false, FILE, 0)]);
    let want = |p: &str| extract("zip", vec![member(p, b"hello")]);
    assert_refused("zip-missing", &want("top/b"), &z, "does not hold");
    // A name that differs by case is not the pinned file.
    assert_refused("zip-case", &want("TOP/a"), &z, "does not hold");

    let mut crc = z.clone();
    let pos = crc.windows(5).position(|w| w == b"hello").unwrap();
    crc[pos] = b'j';
    // The manifest pins the damaged bytes here, so only the CRC-32 can notice.
    let pinned_damage = extract("zip", vec![member("top/a", b"jello")]);
    assert_refused("zip-crc", &pinned_damage, &crc, "CRC-32");
    assert_refused(
        "zip-garbage",
        &want("top/a"),
        b"this is not an archive",
        "zip",
    );
    assert_refused("zip-empty", &want("top/a"), b"", "zip");
    // Cut inside the central directory.
    assert_refused("zip-cut", &want("top/a"), &z[..z.len() - 30], "zip");

    let mut far = z.clone();
    let end = far.len() - 22;
    // A central directory said to reach past the end of the file, without allocating it.
    far[end + 12..end + 16].copy_from_slice(&u32::MAX.wrapping_sub(1).to_le_bytes());
    assert_refused("zip-far", &want("top/a"), &far, "central directory");
    let mut zip64 = z.clone();
    zip64[end + 16..end + 20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_refused("zip-64", &want("top/a"), &zip64, "zip64");

    let t = tar_bz2(&[("top/a", b"hello", tar::EntryType::Regular)]);
    let x = extract("tar.bz2", vec![member("top/a", b"hello")]);
    assert_refused(
        "tar-missing",
        &extract("tar.bz2", vec![member("b", b"x")]),
        &t,
        "does not hold",
    );
    assert_refused("tar-garbage", &x, b"this is not an archive", "tar.bz2");
    assert_refused("tar-cut", &x, &t[..t.len() / 2], "tar.bz2");
}

#[test]
fn download_archive_failure_keeps_a_previous_extraction_and_success_replaces_it() {
    let dir = TempDir::new("archive-replace");
    let into = dir.0.join("out");
    std::fs::create_dir_all(&into).unwrap();
    std::fs::write(into.join("old.txt"), b"old").unwrap();
    // A crash's leftovers are not trusted.
    std::fs::create_dir_all(dir.0.join("out.tmp")).unwrap();
    std::fs::write(dir.0.join("out.tmp/stale"), b"stale").unwrap();

    let bad = dir.0.join("bad.zip");
    std::fs::write(&bad, b"not an archive").unwrap();
    let x = extract("zip", vec![member("top/a", b"new")]);
    assert!(extract_verified(&bad, &x, &into, "sha", &CancelToken::new()).is_err());
    assert!(
        into.join("old.txt").is_file(),
        "a failure publishes nothing"
    );
    assert!(!dir.0.join("out.tmp").exists());

    let good = dir.0.join("good.zip");
    std::fs::write(&good, zip_bytes(&[("top/a", b"new", false, FILE, 0)])).unwrap();
    extract_verified(&good, &x, &into, "sha", &CancelToken::new()).unwrap();
    assert!(!into.join("old.txt").exists());
    assert!(!dir.0.join("out.tmp").exists());
    assert!(!dir.0.join("out.old").exists());
    assert_eq!(std::fs::read(into.join("top/a")).unwrap(), b"new");
}

#[test]
fn download_archive_an_unreadable_archive_is_a_retryable_io_error() {
    let dir = TempDir::new("archive-io");
    let x = extract("zip", vec![member("a", b"x")]);
    let e = extract_verified(
        Path::new(&dir.0.join("no-such.zip")),
        &x,
        &dir.0.join("out"),
        "s",
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(matches!(e, ExtractError::Io { .. }), "{e:?}");
    assert!(!dir.0.join("out").exists());
}

/// A folder a meeting has mapped: every file stays and the download says it is in use. Deleting in
/// place removed the files before the held one (`a.dll` here).
#[cfg(windows)]
#[test]
fn download_archive_a_held_folder_is_in_use_with_every_file_still_there() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let dir = TempDir::new("archive-held");
    let into = dir.0.join("out");
    std::fs::create_dir_all(&into).unwrap();
    std::fs::write(into.join("a.dll"), b"first").unwrap();
    std::fs::write(into.join("z.dll"), b"held").unwrap();
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(into.join("z.dll"))
        .unwrap();
    let good = dir.0.join("good.zip");
    std::fs::write(&good, zip_bytes(&[("top/a", b"new", false, FILE, 0)])).unwrap();
    let x = extract("zip", vec![member("top/a", b"new")]);

    let e = extract_verified(&good, &x, &into, "sha", &CancelToken::new()).unwrap_err();
    assert!(matches!(e, ExtractError::InUse { .. }), "{e:?}");
    assert!(e.to_string().contains("in use or not writable"), "{e}");
    assert_eq!(std::fs::read(into.join("a.dll")).unwrap(), b"first");
    assert!(into.join("z.dll").is_file());
    assert!(!into.join("top/a").exists());
    assert!(!dir.0.join("out.tmp").exists());
    assert!(!dir.0.join("out.old").exists());

    // Let go, the same archive replaces it.
    drop(handle);
    extract_verified(&good, &x, &into, "sha", &CancelToken::new()).unwrap();
    assert!(!into.join("a.dll").exists());
    assert_eq!(std::fs::read(into.join("top/a")).unwrap(), b"new");
    assert!(!dir.0.join("out.old").exists());
}

/// A crash between the two renames: `<into>` is gone and `<into>.old` holds the good copy. Even an
/// extraction that fails puts it back.
#[test]
fn download_archive_a_crash_between_the_renames_gets_the_good_copy_back() {
    let dir = TempDir::new("archive-crash");
    let into = dir.0.join("out");
    std::fs::create_dir_all(dir.0.join("out.old")).unwrap();
    std::fs::write(dir.0.join("out.old/good"), b"good").unwrap();
    let bad = dir.0.join("bad.zip");
    std::fs::write(&bad, b"not an archive").unwrap();
    let x = extract("zip", vec![member("top/a", b"new")]);
    assert!(extract_verified(&bad, &x, &into, "sha", &CancelToken::new()).is_err());
    assert_eq!(std::fs::read(into.join("good")).unwrap(), b"good");
    assert!(!dir.0.join("out.old").exists());
    // With nothing aside, or with `into` there, nothing moves.
    assert!(!restore_aside(&into).unwrap());
    std::fs::create_dir_all(dir.0.join("out.old")).unwrap();
    assert!(!restore_aside(&into).unwrap());
    assert!(dir.0.join("out.old").exists());
}

/// A crash during the delete: the new `<into>` is the good one, and what is left aside goes.
#[test]
fn download_archive_a_leftover_old_folder_beside_the_extraction_is_cleared() {
    let dir = TempDir::new("archive-old");
    let into = dir.0.join("out");
    std::fs::create_dir_all(&into).unwrap();
    std::fs::write(into.join("kept-until-replaced"), b"x").unwrap();
    std::fs::create_dir_all(dir.0.join("out.old")).unwrap();
    std::fs::write(dir.0.join("out.old/stale"), b"stale").unwrap();
    let good = dir.0.join("good.zip");
    std::fs::write(&good, zip_bytes(&[("top/a", b"new", false, FILE, 0)])).unwrap();
    let x = extract("zip", vec![member("top/a", b"new")]);
    extract_verified(&good, &x, &into, "sha", &CancelToken::new()).unwrap();
    assert_eq!(std::fs::read(into.join("top/a")).unwrap(), b"new");
    assert!(!into.join("kept-until-replaced").exists());
    assert!(!dir.0.join("out.old").exists());
}

#[test]
fn download_archive_a_cancel_before_extracting_publishes_nothing_and_keeps_what_was_there() {
    let dir = TempDir::new("archive-cancel");
    let into = dir.0.join("out");
    std::fs::create_dir_all(&into).unwrap();
    std::fs::write(into.join("old.txt"), b"old").unwrap();
    let good = dir.0.join("good.zip");
    std::fs::write(&good, zip_bytes(&[("top/a", b"new", false, FILE, 0)])).unwrap();
    let x = extract("zip", vec![member("top/a", b"new")]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let e = extract_verified(&good, &x, &into, "sha", &cancel).unwrap_err();
    assert!(matches!(e, ExtractError::Cancelled), "{e:?}");
    assert!(into.join("old.txt").is_file());
    assert!(!dir.0.join("out.tmp").exists());
    assert!(good.is_file(), "the archive is the caller's to keep");
}

/// Pause while a member unpacks, the case the ONNX zip takes tens of seconds over: the next read
/// stops it, with the member unfinished.
#[test]
fn download_archive_a_cancel_stops_a_member_part_way() {
    struct CancelsOnRead<'a>(&'a CancelToken, usize);
    impl Read for CancelsOnRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            self.0.cancel();
            buf.fill(7);
            Ok(buf.len())
        }
    }
    let dir = TempDir::new("archive-cancel-member");
    let wanted = [Member {
        path: "big".to_owned(),
        sha256: "unused".to_owned(),
        size: 1 << 30,
    }];
    let cancel = CancelToken::new();
    let mut entries = Entries {
        wanted: &wanted,
        tmp: &dir.0,
        seen: BTreeSet::new(),
        written: BTreeSet::new(),
        cancel: &cancel,
    };
    let mut src = CancelsOnRead(&cancel, 0);
    let e = entries.write(&wanted[0], &mut src).unwrap_err();
    assert!(matches!(e, ExtractError::Cancelled), "{e:?}");
    assert_eq!(src.1, 1, "one read and no more");
}
