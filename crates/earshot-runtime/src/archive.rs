//! Extraction of the archives the app downloads (T9.1f). The rules of xtask's `archive`
//! module, with one difference: only the files the manifest pins (`[[artefact.extract.member]]`)
//! are written, so what lands on disk is what the manifest names and nothing else (ONNX
//! Runtime's zip is 83 MB holding 448 MB, nearly all debug symbols, of which a meeting needs
//! 17 MB).
//!
//! - Every entry of the archive is checked, wanted or not: a name that would leave the
//!   extraction directory ([`crate::catalogue::jail_join`]), a name seen before (compared
//!   without regard to case, as Windows does), a link or other special entry, an encrypted
//!   entry or a compression method other than stored or deflate refuses the whole archive.
//! - A member is written with `create_new` and never past its size (the manifest's; a zip's
//!   header must say the same); a zip member's CRC-32 and every member's size and SHA-256 are
//!   checked before anything is published.
//! - Extraction goes to `<into>.tmp`, renamed to `<into>` only when every pinned member is
//!   there and verified, with xtask's completion marker (`.earshot-extracted`), so an
//!   interrupted run never leaves a directory that looks done and xtask reads the result
//!   (`is_extracted`, `verify_extracted`) as its own.
//! - An extraction already in `<into>` is renamed aside to `<into>.old` first, whole or not at
//!   all, and only then deleted: a folder a meeting has mapped (`onnxruntime.dll` stays mapped
//!   after it) refuses the rename, so a re-download beside it is [`ExtractError::InUse`] with
//!   every file still there. Deleting in place would delete the files before the held one.
//!   `<into>.old` without `<into>` is a crash between the two renames and holds the whole good
//!   copy, because the delete only starts once `<into>` is back: [`restore_aside`] puts it back
//!   before anything else, and at start. `<into>.old` beside `<into>` is a crash during the
//!   delete and is only cleared.
//! - A cancel stops the extraction between reads ([`ExtractError::Cancelled`]); nothing is
//!   published and the caller keeps the archive.
//! - The opt-in CUDA files ([`extract_flat`], T8.4ae) share one folder between archives, so they
//!   have no marker and no folder swap: the same checks in a staging folder, then each file is
//!   renamed into place on its own. The folder's state is read from its files.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use earshot_core::asr::CancelToken;
use sha2::{Digest, Sha256};

use crate::catalogue::{CudaFile, Extract, Member, file_problem, jail_join};

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;

/// The completion marker xtask's `archive::MARKER` names and reads: line 1 the archive's
/// SHA-256, then `<sha256> <size> <path>` per extracted file.
pub(crate) const MARKER: &str = ".earshot-extracted";

/// Why an archive was not extracted. Nothing was published and the temp folder is gone.
#[derive(Debug)]
pub enum ExtractError {
    /// The archive is damaged or breaks a rule of the module docs (it matched its pinned
    /// SHA-256, so this is a wrong pin or a bug: extracting again gives the same answer).
    Refused(String),
    /// A local file operation failed (a full disk, a file another program holds).
    Io {
        /// What was being done, with the path.
        what: String,
        /// The error.
        error: io::Error,
    },
    /// A file to be replaced is held open or loaded, most often by this app until it restarts.
    InUse {
        /// The file.
        file: PathBuf,
        /// What the system said.
        error: io::Error,
    },
    /// The cancel token was set while extracting.
    Cancelled,
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "{why}"),
            Self::Io { what, error } => write!(f, "{what}: {error}"),
            Self::InUse { file, error } => write!(
                f,
                "{} is in use or not writable; restart Earshot and try again ({error})",
                file.display()
            ),
            Self::Cancelled => write!(f, "the extraction was cancelled"),
        }
    }
}

impl std::error::Error for ExtractError {}

fn refused(why: impl Into<String>) -> ExtractError {
    ExtractError::Refused(why.into())
}

fn io_at(what: impl FnOnce() -> String) -> impl FnOnce(io::Error) -> ExtractError {
    move |error| ExtractError::Io {
        what: what(),
        error,
    }
}

/// The archive formats the app extracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// bzip2-compressed tar.
    TarBz2,
    /// zip with stored or deflated entries (no zip64, no encryption).
    Zip,
}

impl Format {
    /// The manifest spelling; `None` for a format the app does not extract.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "tar.bz2" => Some(Self::TarBz2),
            "zip" => Some(Self::Zip),
            _ => None,
        }
    }
}

/// `<into><suffix>`.
fn beside(into: &Path, suffix: &str) -> PathBuf {
    let mut name = into.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    into.with_file_name(name)
}

/// `<into>.tmp`.
pub(crate) fn tmp_dir(into: &Path) -> PathBuf {
    beside(into, ".tmp")
}

/// `<into>.old`.
pub(crate) fn aside_dir(into: &Path) -> PathBuf {
    beside(into, ".old")
}

/// Puts back the extraction a crash left aside (module docs); whether it did.
///
/// # Errors
/// The rename's error; `<into>.old` then stays where it was.
pub(crate) fn restore_aside(into: &Path) -> io::Result<bool> {
    let aside = aside_dir(into);
    if into.exists() || !aside.exists() {
        return Ok(false);
    }
    fs::rename(&aside, into)?;
    Ok(true)
}

/// Extracts the members `x` pins from `archive` into `into` (module docs), replacing what
/// `into` held only once they are all verified. `archive_sha256` goes into the marker; the
/// archive itself was verified by the download.
///
/// # Errors
/// [`ExtractError`]; nothing is published and `<into>.tmp` is removed.
pub(crate) fn extract_verified(
    archive: &Path,
    x: &Extract,
    into: &Path,
    archive_sha256: &str,
    cancel: &CancelToken,
) -> Result<(), ExtractError> {
    restore_aside(into).map_err(io_at(|| format!("restore {}", aside_dir(into).display())))?;
    let format = Format::parse(&x.format).ok_or_else(|| {
        refused(format!(
            "this version cannot extract `{}` archives",
            x.format
        ))
    })?;
    if x.members.is_empty() {
        return Err(refused("the manifest pins no file of this archive"));
    }
    let tmp = tmp_dir(into);
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(io_at(|| format!("remove {}", tmp.display())))?;
    }
    fs::create_dir_all(&tmp).map_err(io_at(|| format!("create {}", tmp.display())))?;
    let result = write_members(archive, format, x, &tmp, cancel)
        .and_then(|()| publish(&tmp, x, into, archive_sha256));
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

/// Extracts `files` from `archive` into `folder` under their own names (module docs), through
/// the staging folder `<folder>/<staging>`, which never outlives the call. A file that cannot be
/// replaced (a loaded DLL) is an [`ExtractError::InUse`] unless it already holds the right bytes;
/// files renamed before it stay, they are verified, and running it again finishes the job.
///
/// # Errors
/// [`ExtractError`].
pub(crate) fn extract_flat(
    archive: &Path,
    format: &str,
    files: &[CudaFile],
    folder: &Path,
    staging: &str,
    cancel: &CancelToken,
) -> Result<(), ExtractError> {
    let parsed = Format::parse(format)
        .ok_or_else(|| refused(format!("this version cannot extract `{format}` archives")))?;
    if files.is_empty() {
        return Err(refused("the manifest pins no file of this archive"));
    }
    let x = Extract {
        format: format.to_owned(),
        into: String::new(),
        members: files
            .iter()
            .map(|f| Member {
                path: f.member.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
            })
            .collect(),
    };
    let tmp = folder.join(staging);
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(io_at(|| format!("remove {}", tmp.display())))?;
    }
    fs::create_dir_all(&tmp).map_err(io_at(|| format!("create {}", tmp.display())))?;
    let result = write_members(archive, parsed, &x, &tmp, cancel)
        .and_then(|()| publish_flat(&tmp, files, folder));
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Whether `e`, from opening or replacing an existing file, means something holds that file: a
/// sharing or lock violation, or the access denial Windows gives for a mapped image. The caller
/// has already ruled out the read-only attribute, which gives the same denial. Any other access
/// denial (a folder this user cannot write to) reads the same, so the copy says "in use or not
/// writable".
pub(crate) fn held_by_another(e: &io::Error) -> bool {
    /// ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
    const HELD: [i32; 2] = [32, 33];
    e.kind() == io::ErrorKind::PermissionDenied
        || e.raw_os_error().is_some_and(|c| HELD.contains(&c))
}

/// Clears the read-only attribute of `path`, on Windows only what it says.
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "these are Windows files, where this clears one attribute; no Unix mode bits exist"
)]
pub(crate) fn clear_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

/// Sets the read-only attribute of `path` again, after a removal or a replacement did not happen.
/// Best effort: the attribute was someone's choice, and failing to restore it changes no outcome.
pub(crate) fn restore_read_only(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        let _ = fs::set_permissions(path, permissions);
    }
}

fn publish_flat(tmp: &Path, files: &[CudaFile], folder: &Path) -> Result<(), ExtractError> {
    for f in files {
        let from =
            jail_join(tmp, &f.member).map_err(|e| refused(format!("`{}`: {e}", f.member)))?;
        let to = folder.join(&f.name);
        let mut renamed = fs::rename(&from, &to);
        let read_only = fs::metadata(&to).is_ok_and(|m| m.permissions().readonly());
        if renamed.is_err() && read_only {
            // A wrong file someone marked read-only: the download replaces it like any other.
            clear_read_only(&to)
                .map_err(io_at(|| format!("clear read-only on {}", to.display())))?;
            renamed = fs::rename(&from, &to);
            if renamed.is_err() {
                restore_read_only(&to);
            }
        }
        if let Err(error) = renamed {
            if matches!(file_problem(&to, &f.sha256, f.size), Ok(None)) {
                continue;
            }
            if to.exists() && held_by_another(&error) {
                return Err(ExtractError::InUse { file: to, error });
            }
            return Err(ExtractError::Io {
                what: format!("replace {}", to.display()),
                error,
            });
        }
    }
    Ok(())
}

fn write_members(
    archive: &Path,
    format: Format,
    x: &Extract,
    tmp: &Path,
    cancel: &CancelToken,
) -> Result<(), ExtractError> {
    let file = File::open(archive).map_err(io_at(|| format!("open {}", archive.display())))?;
    let reader = BufReader::new(file);
    let mut entries = Entries {
        wanted: &x.members,
        tmp,
        seen: BTreeSet::new(),
        written: BTreeSet::new(),
        cancel,
    };
    match format {
        Format::Zip => extract_zip(reader, &mut entries)?,
        Format::TarBz2 => extract_tar(bzip2::read::BzDecoder::new(reader), &mut entries)?,
    }
    match x
        .members
        .iter()
        .find(|m| !entries.written.contains(m.path.as_str()))
    {
        Some(m) => Err(refused(format!(
            "the archive does not hold the pinned file `{}`",
            m.path
        ))),
        None => Ok(()),
    }
}

/// Writes the marker and swaps `tmp` in for `into` (module docs).
fn publish(tmp: &Path, x: &Extract, into: &Path, archive_sha256: &str) -> Result<(), ExtractError> {
    let mut sorted: Vec<&Member> = x.members.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut marker = format!("{archive_sha256}\n");
    for m in sorted {
        marker.push_str(&format!("{} {} {}\n", m.sha256, m.size, m.path));
    }
    let path = tmp.join(MARKER);
    fs::write(&path, marker).map_err(io_at(|| format!("write {}", path.display())))?;
    let aside = aside_dir(into);
    // A crash during an earlier delete, beside a good `into` ([`restore_aside`] has run). It was
    // renamed once, so nothing holds it; if it cannot go, `into` stays.
    if aside.exists() {
        fs::remove_dir_all(&aside).map_err(io_at(|| format!("remove {}", aside.display())))?;
    }
    let replacing = into.exists();
    if replacing && let Err(error) = fs::rename(into, &aside) {
        return Err(if held_by_another(&error) {
            ExtractError::InUse {
                file: into.to_path_buf(),
                error,
            }
        } else {
            ExtractError::Io {
                what: format!("rename {} -> {}", into.display(), aside.display()),
                error,
            }
        });
    }
    if let Err(error) = fs::rename(tmp, into) {
        if replacing {
            let _ = fs::rename(&aside, into);
        }
        return Err(ExtractError::Io {
            what: format!("rename {} -> {}", tmp.display(), into.display()),
            error,
        });
    }
    // Only costs disk if it stays, and the next run removes it.
    if replacing {
        let _ = fs::remove_dir_all(&aside);
    }
    Ok(())
}

/// The state of one extraction: what is wanted and what the archive held so far.
struct Entries<'a> {
    wanted: &'a [Member],
    tmp: &'a Path,
    /// Every name seen, case-folded and without `.` segments.
    seen: BTreeSet<String>,
    /// The pinned paths written.
    written: BTreeSet<&'a str>,
    /// Stops the extraction.
    cancel: &'a CancelToken,
}

impl<'a> Entries<'a> {
    /// Checks an entry's name (any entry, wanted or not) and returns the pinned member it is.
    fn entry(&mut self, raw: &str) -> Result<Option<&'a Member>, ExtractError> {
        if self.cancel.is_cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let name = raw.replace('\\', "/");
        let trimmed = name.trim_end_matches('/');
        jail_join(self.tmp, trimmed).map_err(|e| refused(format!("archive entry `{raw}`: {e}")))?;
        let parts: Vec<&str> = trimmed
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if !self.seen.insert(parts.join("/").to_lowercase()) {
            return Err(refused(format!("archive entry `{raw}` appears twice")));
        }
        let plain = parts.join("/");
        Ok(self.wanted.iter().find(|m| m.path == plain))
    }

    /// Writes member `m` from `src` under the temp folder, never past `m.size` bytes, and
    /// checks its size and SHA-256.
    fn write(&mut self, m: &'a Member, src: &mut dyn Read) -> Result<(), ExtractError> {
        let dest = jail_join(self.tmp, &m.path)
            .map_err(|e| refused(format!("archive entry `{}`: {e}", m.path)))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io_at(|| format!("create {}", parent.display())))?;
        }
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(io_at(|| format!("create {}", dest.display())))?;
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut src = src.take(m.size + 1);
        let mut written = 0u64;
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExtractError::Cancelled);
            }
            let n = src
                .read(&mut buf)
                .map_err(|e| refused(format!("archive entry `{}` cannot be read: {e}", m.path)))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > m.size {
                return Err(refused(format!(
                    "archive entry `{}` expands past its size of {} bytes",
                    m.path, m.size
                )));
            }
            hash.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(io_at(|| format!("write {}", dest.display())))?;
        }
        if written != m.size {
            return Err(refused(format!(
                "archive entry `{}` is {written} bytes, the manifest says {}",
                m.path, m.size
            )));
        }
        out.sync_all()
            .map_err(io_at(|| format!("write {}", dest.display())))?;
        let got = hex(&hash.finalize());
        if !got.eq_ignore_ascii_case(&m.sha256) {
            return Err(refused(format!(
                "archive entry `{}` has sha256 {got}, the manifest says {}",
                m.path, m.sha256
            )));
        }
        self.written.insert(&m.path);
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn extract_tar(reader: impl Read, entries: &mut Entries<'_>) -> Result<(), ExtractError> {
    let damaged = |e: io::Error| refused(format!("the tar.bz2 archive is damaged: {e}"));
    let mut ar = tar::Archive::new(reader);
    let mut files = 0usize;
    for entry in ar.entries().map_err(damaged)? {
        let mut entry = entry.map_err(damaged)?;
        let raw = entry
            .path()
            .map_err(damaged)?
            .to_string_lossy()
            .into_owned();
        let kind = entry.header().entry_type();
        // GNU and PAX metadata headers are consumed by the tar crate; anything else that is
        // not a regular file or directory (symlinks, hard links, devices) is refused.
        let dir = kind.is_dir();
        if !(dir || kind.is_file() || kind.is_contiguous()) {
            return Err(refused(format!(
                "archive entry `{raw}` is not a regular file ({kind:?})"
            )));
        }
        let member = entries.entry(&raw)?;
        if dir {
            continue;
        }
        files += 1;
        if let Some(m) = member {
            entries.write(m, &mut entry)?;
        }
    }
    if files == 0 {
        return Err(refused("the tar.bz2 archive holds no files"));
    }
    Ok(())
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
/// central file header 4.3.12). zip64 archives are refused.
fn zip_directory<R: Read + Seek>(r: &mut R) -> Result<Vec<ZipEntry>, ExtractError> {
    let read_err = |e: io::Error| refused(format!("the zip archive is damaged: {e}"));
    let corrupt = || refused("the zip archive has a corrupt central directory");
    let len = r.seek(SeekFrom::End(0)).map_err(read_err)?;
    let tail_len = len.min(22 + 65_535);
    r.seek(SeekFrom::Start(len - tail_len)).map_err(read_err)?;
    let mut tail = vec![0u8; usize::try_from(tail_len).map_err(|_| corrupt())?];
    r.read_exact(&mut tail).map_err(read_err)?;
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| le32(&tail, i) == 0x0605_4b50)
        .ok_or_else(|| refused("the zip archive has no end-of-central-directory record"))?;
    let count = le16(&tail, eocd + 10);
    let cd_size = le32(&tail, eocd + 12);
    let cd_offset = le32(&tail, eocd + 16);
    if count == 0xFFFF || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(refused("zip64 archives are not supported"));
    }
    // Checked before allocating: the sizes come from the file.
    if u64::from(cd_offset) + u64::from(cd_size) > len {
        return Err(corrupt());
    }
    r.seek(SeekFrom::Start(u64::from(cd_offset)))
        .map_err(read_err)?;
    let mut cd = vec![0u8; usize::try_from(cd_size).map_err(|_| corrupt())?];
    r.read_exact(&mut cd).map_err(read_err)?;
    let mut out = Vec::with_capacity(count);
    let mut at = 0usize;
    for _ in 0..count {
        if at + 46 > cd.len() || le32(&cd, at) != 0x0201_4b50 {
            return Err(corrupt());
        }
        let (n, e, c) = (le16(&cd, at + 28), le16(&cd, at + 30), le16(&cd, at + 32));
        let name = cd.get(at + 46..at + 46 + n).ok_or_else(corrupt)?;
        out.push(ZipEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            flags: le16(&cd, at + 8),
            method: le16(&cd, at + 10),
            crc: le32(&cd, at + 16),
            compressed: u64::from(le32(&cd, at + 20)),
            size: u64::from(le32(&cd, at + 24)),
            unix_mode: le32(&cd, at + 38) >> 16,
            local_offset: u64::from(le32(&cd, at + 42)),
        });
        at += 46 + n + e + c;
    }
    Ok(out)
}

fn extract_zip<R: Read + Seek>(mut r: R, entries: &mut Entries<'_>) -> Result<(), ExtractError> {
    let directory = zip_directory(&mut r)?;
    let mut files = 0usize;
    for z in &directory {
        let raw = &z.name;
        if z.flags & 1 != 0 {
            return Err(refused(format!("archive entry `{raw}` is encrypted")));
        }
        if z.unix_mode & 0o170_000 == 0o120_000 {
            return Err(refused(format!("archive entry `{raw}` is a symbolic link")));
        }
        if !matches!(z.method, 0 | 8) {
            return Err(refused(format!(
                "archive entry `{raw}` uses unsupported compression method {}",
                z.method
            )));
        }
        let member = entries.entry(raw)?;
        if raw.ends_with('/') || raw.ends_with('\\') {
            continue;
        }
        files += 1;
        let Some(m) = member else { continue };
        if z.size != m.size {
            return Err(refused(format!(
                "archive entry `{raw}` declares {} bytes, the manifest says {}",
                z.size, m.size
            )));
        }
        zip_member(&mut r, z, m, entries)?;
    }
    if files == 0 {
        return Err(refused("the zip archive holds no files"));
    }
    Ok(())
}

fn zip_member<'a, R: Read + Seek>(
    r: &mut R,
    z: &ZipEntry,
    m: &'a Member,
    entries: &mut Entries<'a>,
) -> Result<(), ExtractError> {
    let raw = &z.name;
    let read_err = |e: io::Error| refused(format!("archive entry `{raw}` cannot be read: {e}"));
    // Local file header (APPNOTE 4.3.7): 30 fixed bytes, then name and extra field.
    r.seek(SeekFrom::Start(z.local_offset)).map_err(read_err)?;
    let mut lh = [0u8; 30];
    r.read_exact(&mut lh).map_err(read_err)?;
    if le32(&lh, 0) != 0x0403_4b50 {
        return Err(refused(format!(
            "archive entry `{raw}` has a bad local header"
        )));
    }
    let skip = le16(&lh, 26) + le16(&lh, 28);
    r.seek(SeekFrom::Current(i64::try_from(skip).unwrap_or(i64::MAX)))
        .map_err(read_err)?;
    let data = r.by_ref().take(z.compressed);
    let src: Box<dyn Read + '_> = if z.method == 8 {
        Box::new(flate2::read::DeflateDecoder::new(data))
    } else {
        Box::new(data)
    };
    let mut src = flate2::CrcReader::new(src);
    entries.write(m, &mut src)?;
    if src.crc().sum() != z.crc {
        return Err(refused(format!("archive entry `{raw}` fails its CRC-32")));
    }
    Ok(())
}
