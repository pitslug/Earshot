//! Extraction of downloaded `.tar.gz` / `.tar.bz2` / `.zip` archives into the cache.
//!
//! Extraction goes to `<into>.tmp` and is renamed to `<into>` only when every entry was
//! written, so an interrupted run never leaves a half-extracted directory that looks done.
//! Every entry path goes through [`crate::download::jail_join`]; links and other special
//! entries are refused. The completion marker records the archive's SHA-256 and the size and
//! SHA-256 of every extracted file, so `--verify` can re-hash an extraction
//! ([`verify_extracted`]).

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Marker written inside a completed extraction directory. Line 1 is the archive's
/// SHA-256; every further line is `<sha256> <size> <path>` for one extracted file (path
/// relative to the extraction directory, `/`-separated). Markers written before T0.8 have
/// line 1 only.
pub const MARKER: &str = ".earshot-extracted";

/// Supported archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// gzip-compressed tar.
    TarGz,
    /// bzip2-compressed tar.
    TarBz2,
    /// zip with stored or deflated entries (zip64 extra fields of an entry read, a zip64
    /// end record refused; no encryption).
    Zip,
}

impl Format {
    /// Parses the manifest spelling (`tar.gz`, `tar.bz2`, `zip`).
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "tar.gz" => Ok(Self::TarGz),
            "tar.bz2" => Ok(Self::TarBz2),
            "zip" => Ok(Self::Zip),
            other => {
                bail!("unsupported archive format `{other}` (expected tar.gz, tar.bz2 or zip)")
            }
        }
    }
}

/// True if `into` holds a completed extraction of the archive with this SHA-256.
pub fn is_extracted(into: &Path, archive_sha256: &str) -> bool {
    fs::read_to_string(into.join(MARKER))
        .is_ok_and(|s| s.lines().next().map(str::trim) == Some(archive_sha256))
}

/// One extracted file recorded in the marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// Path relative to the extraction directory, `/`-separated.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
}

/// Parses a marker into the archive SHA-256 and the recorded files.
pub fn parse_marker(text: &str) -> Result<(String, Vec<Recorded>)> {
    let mut lines = text.lines();
    let archive = lines.next().unwrap_or("").trim().to_string();
    let mut files = Vec::new();
    for l in lines.filter(|l| !l.trim().is_empty()) {
        let mut parts = l.splitn(3, ' ');
        let (Some(sha), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("bad marker line `{l}`");
        };
        files.push(Recorded {
            path: path.to_string(),
            size: size
                .parse()
                .with_context(|| format!("bad size in marker line `{l}`"))?,
            sha256: sha.to_string(),
        });
    }
    Ok((archive, files))
}

/// Re-hashes every file the marker of `into` records. Fails when the marker is missing,
/// names another archive, records no files (a marker written before T0.8), any recorded
/// file is missing or differs, or the directory holds a file the marker does not record.
pub fn verify_extracted(into: &Path, archive_sha256: &str) -> Result<()> {
    let marker = into.join(MARKER);
    let text = fs::read_to_string(&marker).with_context(|| format!("read {}", marker.display()))?;
    let (archive, files) = parse_marker(&text)?;
    if archive != archive_sha256 {
        bail!("{} belongs to another archive", marker.display());
    }
    if files.is_empty() {
        bail!(
            "{} records no file hashes (written by an older xtask)",
            marker.display()
        );
    }
    let recorded: std::collections::BTreeSet<&str> =
        files.iter().map(|f| f.path.as_str()).collect();
    let extra: Vec<String> = list_files(into)?
        .into_iter()
        .map(|(rel, _)| rel)
        .filter(|rel| rel != MARKER && !recorded.contains(rel.as_str()))
        .collect();
    if !extra.is_empty() {
        bail!(
            "{} holds files the extraction did not produce: {}",
            into.display(),
            extra.join(", ")
        );
    }
    for f in &files {
        let p = crate::download::jail_join(into, &f.path)?;
        crate::download::verify_file(
            &p,
            &crate::download::Expected {
                sha256: &f.sha256,
                size: f.size,
            },
        )?;
    }
    Ok(())
}

/// Every regular file under `dir` (recursively) as `(relative path, absolute path)`, sorted
/// by relative path (`/`-separated). Anything that is not a file or directory is an error.
fn list_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                let rel = path
                    .strip_prefix(root)?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if rel.contains(['\n', '\r']) {
                    bail!("extracted file name {rel:?} contains a line break; refusing");
                }
                out.push((rel, path));
            } else {
                bail!("{} is not a regular file; refusing", path.display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Every regular file under `dir` (recursively) with its size and SHA-256, sorted by path.
fn record_files(dir: &Path) -> Result<Vec<Recorded>> {
    list_files(dir)?
        .into_iter()
        .map(|(rel, path)| {
            Ok(Recorded {
                size: fs::metadata(&path)?.len(),
                sha256: crate::download::sha256_file(&path)?,
                path: rel,
            })
        })
        .collect()
}

fn tmp_dir(into: &Path) -> PathBuf {
    let mut name = into.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    into.with_file_name(name)
}

/// Extracts `archive` into `into`, replacing any previous (incomplete or stale) content.
pub fn extract(archive: &Path, format: Format, into: &Path, archive_sha256: &str) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let reader: Box<dyn Read> = match format {
        Format::TarGz => Box::new(flate2::read::GzDecoder::new(BufReader::new(file))),
        Format::TarBz2 => Box::new(bzip2::read::BzDecoder::new(BufReader::new(file))),
        Format::Zip => {
            return extract_zip(BufReader::new(file), into, archive_sha256)
                .with_context(|| format!("extract {}", archive.display()));
        }
    };
    extract_tar(reader, into, archive_sha256)
        .with_context(|| format!("extract {}", archive.display()))
}

/// Creates an empty `<into>.tmp`.
fn fresh_tmp(into: &Path) -> Result<PathBuf> {
    let tmp = tmp_dir(into);
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("remove {}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp)?;
    Ok(tmp)
}

/// Writes the marker (archive SHA-256 plus every extracted file's size and SHA-256) into
/// `tmp` and renames it to `into`.
fn publish(tmp: &Path, into: &Path, files: usize, archive_sha256: &str) -> Result<()> {
    if files == 0 {
        bail!("archive contains no files");
    }
    let mut marker = format!("{archive_sha256}\n");
    for f in record_files(tmp)? {
        marker.push_str(&format!("{} {} {}\n", f.sha256, f.size, f.path));
    }
    fs::write(tmp.join(MARKER), marker)?;
    if into.exists() {
        fs::remove_dir_all(into).with_context(|| format!("remove {}", into.display()))?;
    }
    fs::rename(tmp, into)
        .with_context(|| format!("rename {} -> {}", tmp.display(), into.display()))?;
    Ok(())
}

fn extract_tar(reader: impl Read, into: &Path, archive_sha256: &str) -> Result<()> {
    let tmp = fresh_tmp(into)?;
    let mut ar = tar::Archive::new(reader);
    let mut files = 0usize;
    for entry in ar.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_string_lossy().replace('\\', "/");
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(crate::download::jail_join(&tmp, &raw)?)?;
            continue;
        }
        // GNU/PAX metadata headers are consumed by the tar crate; anything else that is not
        // a regular file (symlinks, hard links, devices) is refused.
        if !(kind.is_file() || kind.is_contiguous()) {
            bail!("archive entry `{raw}` is not a regular file ({kind:?}); refusing");
        }
        let dest = crate::download::jail_join(&tmp, &raw)
            .with_context(|| format!("archive entry `{raw}`"))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)?;
        files += 1;
    }
    publish(&tmp, into, files, archive_sha256)
}

fn le16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// One central-directory entry of a zip archive.
struct ZipEntry {
    name: String,
    flags: usize,
    method: usize,
    crc: u32,
    compressed: u64,
    size: u64,
    unix_mode: u32,
    local_offset: u64,
}

/// Reads the central directory (PKWARE APPNOTE.TXT: end of central directory record 4.3.16,
/// central file header 4.3.12), with an entry's zip64 extra field ([`zip64_fields`]). An
/// archive whose end record needs zip64 (over 65,535 entries or a directory past 4 GiB) is
/// refused.
fn zip_directory<R: Read + Seek>(r: &mut R) -> Result<Vec<ZipEntry>> {
    let len = r.seek(SeekFrom::End(0))?;
    let tail_len = len.min(22 + 65_535);
    r.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; usize::try_from(tail_len)?];
    r.read_exact(&mut tail)?;
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| le32(&tail, i) == 0x0605_4b50)
        .context("no zip end-of-central-directory record")?;
    let count = le16(&tail, eocd + 10);
    let cd_size = le32(&tail, eocd + 12);
    let cd_offset = le32(&tail, eocd + 16);
    if count == 0xFFFF || cd_size == u32::MAX || cd_offset == u32::MAX {
        bail!("zip64 archives are not supported");
    }
    r.seek(SeekFrom::Start(u64::from(cd_offset)))?;
    let mut cd = vec![0u8; usize::try_from(cd_size)?];
    r.read_exact(&mut cd)?;
    let mut entries = Vec::with_capacity(count);
    let mut at = 0usize;
    for _ in 0..count {
        if at + 46 > cd.len() || le32(&cd, at) != 0x0201_4b50 {
            bail!("corrupt zip central directory");
        }
        let (n, e, c) = (le16(&cd, at + 28), le16(&cd, at + 30), le16(&cd, at + 32));
        let name_bytes = cd
            .get(at + 46..at + 46 + n)
            .context("corrupt zip central directory")?;
        let extra = cd
            .get(at + 46 + n..at + 46 + n + e)
            .context("corrupt zip central directory")?;
        let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");
        let [size, compressed, local_offset] = zip64_fields(
            [le32(&cd, at + 24), le32(&cd, at + 20), le32(&cd, at + 42)],
            extra,
        )
        .with_context(|| format!("zip entry `{name}`"))?;
        entries.push(ZipEntry {
            name,
            flags: le16(&cd, at + 8),
            method: le16(&cd, at + 10),
            crc: le32(&cd, at + 16),
            compressed,
            size,
            unix_mode: le32(&cd, at + 38) >> 16,
            local_offset,
        });
        at += 46 + n + e + c;
    }
    Ok(entries)
}

/// The uncompressed size, compressed size and local header offset of a central-directory
/// entry: each field of `fields` (in that order) that reads `0xFFFFFFFF` is replaced by the
/// next 8 bytes of the zip64 extended information extra field (header id 1) of `extra`, in
/// that order (APPNOTE 4.5.3). Python's `zipfile` writes such offsets for every entry past
/// 2 GiB, even in an archive smaller than 4 GiB.
///
/// # Errors
/// A field marked for zip64 with no zip64 extra field, or one too short to hold it.
fn zip64_fields(fields: [u32; 3], extra: &[u8]) -> Result<[u64; 3]> {
    let mut out = fields.map(u64::from);
    if !fields.contains(&u32::MAX) {
        return Ok(out);
    }
    let mut p = 0usize;
    let data = loop {
        let header = extra
            .get(p..p + 4)
            .context("a zip64 field without its zip64 extra field")?;
        let (id, len) = (le16(header, 0), le16(header, 2));
        let body = extra
            .get(p + 4..p + 4 + len)
            .context("a zip extra field runs past its entry")?;
        if id == 1 {
            break body;
        }
        p += 4 + len;
    };
    let mut q = 0usize;
    for (value, raw) in out.iter_mut().zip(fields) {
        if raw == u32::MAX {
            let bytes = data
                .get(q..q + 8)
                .context("a zip64 extra field too short for its fields")?;
            *value = u64::from_le_bytes(bytes.try_into()?);
            q += 8;
        }
    }
    Ok(out)
}

fn extract_zip<R: Read + Seek>(mut r: R, into: &Path, archive_sha256: &str) -> Result<()> {
    let entries = zip_directory(&mut r)?;
    let tmp = fresh_tmp(into)?;
    let result = extract_zip_entries(&mut r, &entries, &tmp);
    match result {
        Ok(files) => publish(&tmp, into, files, archive_sha256),
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

fn extract_zip_entries<R: Read + Seek>(
    r: &mut R,
    entries: &[ZipEntry],
    tmp: &Path,
) -> Result<usize> {
    let mut files = 0usize;
    let mut names = std::collections::BTreeSet::new();
    for z in entries {
        let raw = &z.name;
        if !names.insert(raw.trim_end_matches('/').to_owned()) {
            bail!("zip entry `{raw}` appears twice; refusing");
        }
        if z.flags & 1 != 0 {
            bail!("zip entry `{raw}` is encrypted; refusing");
        }
        if z.unix_mode & 0o170_000 == 0o120_000 {
            bail!("zip entry `{raw}` is a symbolic link; refusing");
        }
        if raw.ends_with('/') {
            fs::create_dir_all(crate::download::jail_join(tmp, raw)?)?;
            continue;
        }
        let dest = crate::download::jail_join(tmp, raw)
            .with_context(|| format!("archive entry `{raw}`"))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        // Local file header (APPNOTE 4.3.7): 30 fixed bytes, then name and extra field.
        r.seek(SeekFrom::Start(z.local_offset))?;
        let mut lh = [0u8; 30];
        r.read_exact(&mut lh)?;
        if le32(&lh, 0) != 0x0403_4b50 {
            bail!("zip entry `{raw}`: bad local header");
        }
        let skip = le16(&lh, 26) + le16(&lh, 28);
        r.seek(SeekFrom::Current(i64::try_from(skip)?))?;
        let data = r.by_ref().take(z.compressed);
        let src: Box<dyn Read + '_> = match z.method {
            0 => Box::new(data),
            8 => Box::new(flate2::read::DeflateDecoder::new(data)),
            m => bail!("zip entry `{raw}` uses unsupported compression method {m}"),
        };
        // Never write more than the declared size (+1 byte to detect an overrun).
        let limit = z
            .size
            .checked_add(1)
            .with_context(|| format!("zip entry `{raw}`: declared size {} is too large", z.size))?;
        let mut src = src.take(limit);
        let mut out = File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        let mut crc = flate2::Crc::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut written = 0u64;
        loop {
            let k = src.read(&mut buf)?;
            if k == 0 {
                break;
            }
            crc.update(&buf[..k]);
            written += k as u64;
            if written > z.size {
                bail!(
                    "zip entry `{raw}` expands past its declared size of {} bytes",
                    z.size
                );
            }
            out.write_all(&buf[..k])?;
        }
        if written != z.size || crc.sum() != z.crc {
            bail!("zip entry `{raw}`: size or CRC-32 mismatch");
        }
        files += 1;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("earshot-xtask-ar-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn header(path: &str, len: usize, kind: tar::EntryType) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        // Write the raw name so the builder's own path validation cannot hide a bad entry.
        let name = &mut h.as_old_mut().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_size(len as u64);
        h.set_entry_type(kind);
        h.set_mode(0o644);
        h.set_cksum();
        h
    }

    fn tar_bytes(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data, kind) in entries {
            let h = header(path, data.len(), *kind);
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn extracts_files_and_writes_marker() {
        let d = tmp("ok");
        let into = d.join("out");
        let t = tar_bytes(&[
            ("top/", b"", tar::EntryType::Directory),
            ("top/a.txt", b"hello", tar::EntryType::Regular),
            ("top/sub/b.txt", b"world", tar::EntryType::Regular),
        ]);
        extract_tar(&t[..], &into, "abc").unwrap();
        assert_eq!(fs::read(into.join("top/a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(into.join("top/sub/b.txt")).unwrap(), b"world");
        assert!(is_extracted(&into, "abc"));
        assert!(!is_extracted(&into, "abd"));
        assert!(!tmp_dir(&into).exists());
    }

    #[test]
    fn marker_records_every_file_and_verify_rehashes_them() {
        let dir = tmp("rehash");
        let t = tar_bytes(&[
            ("top/a.txt", b"alpha", tar::EntryType::Regular),
            ("top/sub/b.txt", b"bravo!", tar::EntryType::Regular),
        ]);
        let into = dir.join("out");
        extract_tar(&t[..], &into, "abc").unwrap();
        let (sha, files) = parse_marker(&fs::read_to_string(into.join(MARKER)).unwrap()).unwrap();
        assert_eq!(sha, "abc");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["top/a.txt", "top/sub/b.txt"]);
        assert_eq!(files[1].size, 6);
        assert!(is_extracted(&into, "abc"));
        verify_extracted(&into, "abc").expect("fresh extraction verifies");
        assert!(verify_extracted(&into, "other").is_err());

        // Same size, different bytes: only a re-hash notices.
        fs::write(into.join("top").join("sub").join("b.txt"), b"BRAVO!").unwrap();
        assert!(is_extracted(&into, "abc"), "the cheap check still passes");
        let err = format!("{:#}", verify_extracted(&into, "abc").unwrap_err());
        assert!(err.contains("sha256"), "{err}");

        fs::write(into.join("top").join("sub").join("b.txt"), b"bravo!").unwrap();
        verify_extracted(&into, "abc").expect("restored");
        // A file the archive did not contain.
        fs::write(into.join("top").join("sub").join("extra.txt"), b"x").unwrap();
        let err = format!("{:#}", verify_extracted(&into, "abc").unwrap_err());
        assert!(err.contains("top/sub/extra.txt"), "{err}");
        fs::remove_file(into.join("top").join("sub").join("extra.txt")).unwrap();

        fs::remove_file(into.join("top").join("a.txt")).unwrap();
        assert!(verify_extracted(&into, "abc").is_err());
    }

    #[test]
    fn marker_without_file_list_is_extracted_but_does_not_verify() {
        let dir = tmp("oldmarker");
        fs::write(dir.join(MARKER), "abc").unwrap();
        assert!(is_extracted(&dir, "abc"));
        let err = format!("{:#}", verify_extracted(&dir, "abc").unwrap_err());
        assert!(err.contains("no file hashes"), "{err}");
    }

    #[test]
    fn rejects_entries_that_escape() {
        for bad in ["../evil.txt", "top/../../evil.txt", "/abs.txt"] {
            let d = tmp("escape");
            let into = d.join("out");
            let t = tar_bytes(&[(bad, b"x", tar::EntryType::Regular)]);
            assert!(extract_tar(&t[..], &into, "abc").is_err(), "{bad}");
            assert!(!into.exists(), "{bad}: nothing may be published");
            assert!(!d.join("evil.txt").exists());
        }
    }

    #[test]
    fn rejects_symlinks() {
        let d = tmp("link");
        let into = d.join("out");
        let t = tar_bytes(&[("top/link", b"", tar::EntryType::Symlink)]);
        assert!(extract_tar(&t[..], &into, "abc").is_err());
        assert!(!into.exists());
    }

    #[test]
    fn format_parse() {
        assert_eq!(Format::parse("tar.gz").unwrap(), Format::TarGz);
        assert_eq!(Format::parse("tar.bz2").unwrap(), Format::TarBz2);
        assert_eq!(Format::parse("zip").unwrap(), Format::Zip);
        assert!(Format::parse("rar").is_err());
    }

    /// One test zip entry: name, data, deflate, unix mode, general-purpose flags.
    type ZipSpec<'a> = (&'a str, &'a [u8], bool, u32, u16);

    /// A minimal zip writer for tests (APPNOTE local headers, central directory, EOCD).
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
            let offset = out.len() as u32;
            let n = name.len() as u16;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc.sum().to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
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
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&n.to_le_bytes());
            cd.extend_from_slice(&[0; 8]);
            cd.extend_from_slice(&(mode << 16).to_le_bytes());
            cd.extend_from_slice(&offset.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let (cd_offset, cd_len, count) = (out.len() as u32, cd.len() as u32, entries.len() as u16);
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

    /// `z` (from [`zip_bytes`]) with every central-directory entry moved to zip64 fields as
    /// Python's `zipfile` writes an entry past 2 GiB: the chosen 32-bit fields read 0xFFFFFFFF
    /// and their values follow in a zip64 extra field (id 1), after an unrelated extra field.
    fn with_zip64_fields(z: &[u8], sizes: bool, offsets: bool) -> Vec<u8> {
        let eocd = z.len() - 22;
        let cd_offset = le32(z, eocd + 16) as usize;
        let count = le16(z, eocd + 10);
        let mut out = z[..cd_offset].to_vec();
        let mut cd = Vec::new();
        let mut at = cd_offset;
        for _ in 0..count {
            let (n, e, c) = (le16(z, at + 28), le16(z, at + 30), le16(z, at + 32));
            let mut head = z[at..at + 46].to_vec();
            let mut values = Vec::new();
            // APPNOTE 4.5.3 order: uncompressed size, compressed size, local header offset.
            for (field, on) in [(24, sizes), (20, sizes), (42, offsets)] {
                if on {
                    values.extend_from_slice(&u64::from(le32(z, at + field)).to_le_bytes());
                    head[field..field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
                }
            }
            let mut extra = z[at + 46 + n..at + 46 + n + e].to_vec();
            // An unrelated field first (an NTFS-style timestamp id), so the reader must walk.
            extra.extend_from_slice(&0x000au16.to_le_bytes());
            extra.extend_from_slice(&4u16.to_le_bytes());
            extra.extend_from_slice(&[0; 4]);
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&(values.len() as u16).to_le_bytes());
            extra.extend_from_slice(&values);
            head[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
            cd.extend_from_slice(&head);
            cd.extend_from_slice(&z[at + 46..at + 46 + n]);
            cd.extend_from_slice(&extra);
            cd.extend_from_slice(&z[at + 46 + n + e..at + 46 + n + e + c]);
            at += 46 + n + e + c;
        }
        out.extend_from_slice(&cd);
        let mut end = z[eocd..].to_vec();
        end[12..16].copy_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&end);
        out
    }

    /// TODO 85: Qualcomm AI Hub's Genie zip (2.5 GB, written by Python's `zipfile`) gives the
    /// local header offset of its last entries only in zip64 extra fields; reading 0xFFFFFFFF
    /// as the offset failed with "failed to fill whole buffer".
    #[test]
    fn zip_reads_sizes_and_offsets_from_zip64_extra_fields() {
        let big = b"genie ".repeat(4000);
        let entries: [ZipSpec<'_>; 3] = [
            ("top/", b"", false, 0o040_755, 0),
            ("top/a.json", b"{}", false, 0o100_644, 0),
            ("top/b.bin", &big, true, 0o100_644, 0),
        ];
        let plain = zip_bytes(&entries);
        for (sizes, offsets) in [(false, true), (true, false), (true, true)] {
            let z = with_zip64_fields(&plain, sizes, offsets);
            let d = tmp("zip64-fields");
            let into = d.join("out");
            zip_extract(z, &into).unwrap_or_else(|e| panic!("{sizes} {offsets}: {e:#}"));
            assert_eq!(fs::read(into.join("top/a.json")).unwrap(), b"{}");
            assert_eq!(fs::read(into.join("top/b.bin")).unwrap(), big);
        }
        // A zip64 marker with no zip64 field to read it from is refused, never guessed.
        assert!(zip64_fields([1, 2, u32::MAX], &[]).is_err());
        assert!(zip64_fields([1, 2, u32::MAX], &[1, 0, 4, 0, 0, 0, 0, 0]).is_err());
        assert_eq!(zip64_fields([1, 2, 3], &[]).unwrap(), [1, 2, 3]);
    }

    /// A zip64 size of `u64::MAX` (a hostile archive) is an error, never an overflow panic.
    #[test]
    fn zip_refuses_a_declared_size_of_u64_max() {
        let data = b"some bytes";
        let plain = zip_bytes(&[("top/a.bin", data, false, 0o100_644, 0)]);
        let mut z = with_zip64_fields(&plain, true, false);
        // The zip64 field holds the uncompressed size first (APPNOTE 4.5.3).
        let size = (data.len() as u64).to_le_bytes();
        let hits: Vec<usize> = (0..z.len() - 7).filter(|&i| z[i..i + 8] == size).collect();
        assert_eq!(
            hits.len(),
            2,
            "the zip64 sizes, uncompressed then compressed"
        );
        let at = hits[0];
        z[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let d = tmp("zip-max");
        let err = zip_extract(z, &d.join("out")).unwrap_err();
        assert!(format!("{err:#}").contains("too large"), "{err:#}");
        assert!(!d.join("out").exists());
    }

    fn zip_extract(z: Vec<u8>, into: &Path) -> Result<()> {
        extract_zip(std::io::Cursor::new(z), into, "abc")
    }

    #[test]
    fn zip_extracts_stored_and_deflated_files() {
        let d = tmp("zip-ok");
        let into = d.join("out");
        let big = b"onnxruntime ".repeat(5000);
        let z = zip_bytes(&[
            ("top/", b"", false, 0o040_755, 0),
            ("top/a.txt", b"hello", false, 0o100_644, 0),
            ("top\\lib\\b.dll", &big, true, 0o100_644, 0),
        ]);
        zip_extract(z, &into).unwrap();
        assert_eq!(fs::read(into.join("top/a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(into.join("top/lib/b.dll")).unwrap(), big);
        assert!(is_extracted(&into, "abc"));
        assert!(!tmp_dir(&into).exists());
    }

    #[test]
    fn zip_rejects_escapes_links_encryption_and_bad_crc() {
        let bad: Vec<(&str, Vec<u8>)> = vec![
            (
                "escape",
                zip_bytes(&[("../evil.txt", b"x", false, 0o100_644, 0)]),
            ),
            ("abs", zip_bytes(&[("/abs.txt", b"x", false, 0o100_644, 0)])),
            (
                "link",
                zip_bytes(&[("top/link", b"x", false, 0o120_777, 0)]),
            ),
            (
                "encrypted",
                zip_bytes(&[("top/a", b"x", false, 0o100_644, 1)]),
            ),
            ("empty", zip_bytes(&[("top/", b"", false, 0o040_755, 0)])),
        ];
        for (what, z) in bad {
            let d = tmp("zip-bad");
            let into = d.join("out");
            assert!(zip_extract(z, &into).is_err(), "{what}");
            assert!(!into.exists(), "{what}: nothing may be published");
            assert!(!d.join("evil.txt").exists());
        }
        let dup = zip_bytes(&[
            ("top/a.txt", b"one", false, 0o100_644, 0),
            ("top/a.txt", b"two", false, 0o100_644, 0),
        ]);
        let d = tmp("zip-dup");
        assert!(zip_extract(dup, &d.join("out")).is_err(), "duplicate names");
        assert!(!d.join("out").exists());
        let mut z = zip_bytes(&[("top/a.txt", b"hello", false, 0o100_644, 0)]);
        let pos = z.windows(5).position(|w| w == b"hello").unwrap();
        z[pos] = b'j';
        let d = tmp("zip-crc");
        assert!(zip_extract(z, &d.join("out")).is_err(), "corrupted data");
        assert!(!d.join("out").exists());
    }

    #[test]
    fn zip_never_writes_past_the_declared_size() {
        // A deflated entry of 1 MB whose header declares 10 bytes (a zip bomb pattern).
        let big = vec![b'x'; 1 << 20];
        let mut z = zip_bytes(&[("top/bomb.bin", &big, true, 0o100_644, 0)]);
        let declared = 10u32.to_le_bytes();
        let size = (big.len() as u32).to_le_bytes();
        // Patch the uncompressed size in the local header and the central directory.
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
        let d = tmp("zip-bomb");
        let err = zip_extract(z, &d.join("out")).unwrap_err();
        assert!(format!("{err:#}").contains("declared size"), "{err:#}");
        assert!(!d.join("out").exists());
        assert!(
            !tmp_dir(&d.join("out")).exists(),
            "the partial output is removed"
        );
    }
}
