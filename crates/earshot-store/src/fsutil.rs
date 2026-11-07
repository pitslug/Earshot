//! File-system primitives the export writer needs and std does not offer: a rename that
//! never replaces an existing file, and a directory sync; plus the machine's total memory
//! for the first-run model check (T6.10c2). All `unsafe` code of this crate is in this file,
//! in minimal blocks with `SAFETY` comments.

use std::io;
use std::path::Path;

/// Moves `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] if `to` exists. The
/// check and the move are one atomic file-system operation, so a file created at `to` by
/// someone else (Obsidian, a sync client) at any moment is never overwritten.
///
/// Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` (with
/// `MOVEFILE_WRITE_THROUGH`); paths are made absolute and, when the absolute path is 248
/// UTF-16 units or more, given the `\\?\` (or `\\?\UNC\`) prefix as std's `maybe_verbatim`
/// does, but deciding on the absolute length, so vault paths beyond `MAX_PATH` work.
/// Elsewhere: `link(2)`, which fails on an existing target, then unlinking `from`. (v2
/// Android, M10: replace with `renameat2(RENAME_NOREPLACE)` falling back to an `O_EXCL`
/// create, since some Android storage has no hard links.)
///
/// # Errors
/// [`io::ErrorKind::AlreadyExists`] when `to` exists; other I/O errors.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_noreplace(from, to)
}

/// Makes a rename inside `dir` durable where the platform needs it (POSIX: `fsync` on the
/// directory). On Windows NTFS journals the rename itself, and directories cannot be
/// opened for sync through std, so this does nothing.
///
/// # Errors
/// I/O errors.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    imp::sync_dir(dir)
}

/// Total physical memory of the machine (bytes), `None` where it cannot be read.
///
/// Windows: `GlobalMemoryStatusEx`. Elsewhere: not read yet (v2 Android, M10).
#[must_use]
pub fn total_memory() -> Option<u64> {
    imp::total_memory()
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    /// Paths at least this long (UTF-16 units) get the verbatim prefix (std's rule:
    /// `MAX_PATH` minus room for an 8.3 file name).
    pub(super) const VERBATIM_FROM: usize = 248;

    /// NUL-terminated UTF-16 path for a Win32 call: made absolute (GetFullPathNameW via
    /// `std::path::absolute`), and verbatim-prefixed when the absolute path is long.
    pub(super) fn wide(p: &Path) -> io::Result<Vec<u16>> {
        wide_with(p, |p| std::path::absolute(p))
    }

    /// [`wide`] with the absolutising step injected (tests). The length decision is taken
    /// on the absolute path: a short relative path below a deep current directory is long
    /// in Win32's eyes. (std's `maybe_verbatim` decides on the length as given, which
    /// misses that case.)
    pub(super) fn wide_with(
        p: &Path,
        absolutize: impl Fn(&Path) -> io::Result<std::path::PathBuf>,
    ) -> io::Result<Vec<u16>> {
        // Normalised: absolute, backslashes, no `.`/`..`.
        let abs: Vec<u16> = absolutize(p)?.as_os_str().encode_wide().collect();
        let mut out: Vec<u16> = if abs.len() < VERBATIM_FROM {
            abs
        } else {
            let bs = u16::from(b'\\');
            let q = u16::from(b'?');
            let dot = u16::from(b'.');
            if abs.starts_with(&[bs, bs, q, bs]) || abs.starts_with(&[bs, bs, dot, bs]) {
                abs
            } else if abs.starts_with(&[bs, bs]) {
                // \\server\share\... -> \\?\UNC\server\share\...
                let mut v: Vec<u16> = "\\\\?\\UNC".encode_utf16().collect();
                v.extend_from_slice(&abs[1..]);
                v
            } else {
                let mut v: Vec<u16> = "\\\\?\\".encode_utf16().collect();
                v.extend(abs);
                v
            }
        };
        out.push(0);
        Ok(out)
    }

    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (wide(from)?, wide(to)?);
        // SAFETY: both pointers are to NUL-terminated UTF-16 buffers that stay alive for the
        // duration of the call; MoveFileExW does not retain them.
        let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if ok == 0 {
            // ERROR_ALREADY_EXISTS / ERROR_FILE_EXISTS map to ErrorKind::AlreadyExists.
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn total_memory() -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: `status` is a valid, writable MEMORYSTATUSEX with `dwLength` set to its
        // size, as GlobalMemoryStatusEx requires; it is only read after a nonzero return.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0 && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)?;
        std::fs::remove_file(from)
    }

    pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    pub(super) fn total_memory() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("earshot-fsutil-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "total memory is read on Windows only")]
    fn system_total_memory_is_read() {
        let m = total_memory().expect("total memory");
        // More than 1 GB and less than 64 TB on any machine this runs on.
        assert!(m > 1 << 30 && m < 1 << 46, "{m}");
    }

    #[test]
    fn export_rename_noreplace_moves_to_a_free_name() {
        let d = tmp("free");
        std::fs::write(d.join("a"), "A").unwrap();
        rename_noreplace(&d.join("a"), &d.join("b")).unwrap();
        assert!(!d.join("a").exists());
        assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "A");
        sync_dir(&d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn export_rename_noreplace_handles_paths_longer_than_max_path() {
        let mut d = tmp("long");
        for i in 0..6 {
            d = d.join(format!(
                "{i:02}-a-rather-long-folder-name-for-a-vault-path-test"
            ));
        }
        std::fs::create_dir_all(&d).unwrap();
        let from = d.join("note being moved into place.md.earshot-tmp");
        let to = d.join("2026-10-18 09-00 A long meeting title for the path test.md");
        assert!(to.as_os_str().len() > 300, "{}", to.as_os_str().len());
        std::fs::write(&from, "A").unwrap();
        rename_noreplace(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "A");
        std::fs::write(&from, "B").unwrap();
        let e = rename_noreplace(&from, &to).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "A");
        let _ = std::fs::remove_dir_all(tmp("long"));
    }

    #[cfg(windows)]
    #[test]
    fn export_long_windows_paths_get_the_verbatim_prefix() {
        use std::path::PathBuf;
        let short = imp::wide(Path::new("C:\\v\\a.md")).unwrap();
        assert_eq!(
            String::from_utf16_lossy(&short[..short.len() - 1]),
            "C:\\v\\a.md"
        );
        let long = PathBuf::from(format!("C:/v/{}/a.md", "x".repeat(300)));
        let w = imp::wide(&long).unwrap();
        let s = String::from_utf16_lossy(&w[..w.len() - 1]);
        assert!(s.starts_with("\\\\?\\C:\\v\\"), "{s}");
        assert!(!s.contains('/'), "normalised to backslashes");
        let unc = PathBuf::from(format!("\\\\server\\share\\{}\\a.md", "y".repeat(300)));
        let w = imp::wide(&unc).unwrap();
        let s = String::from_utf16_lossy(&w[..w.len() - 1]);
        assert!(s.starts_with("\\\\?\\UNC\\server\\share\\"), "{s}");
    }

    #[cfg(windows)]
    #[test]
    fn export_verbatim_decision_uses_the_absolute_length() {
        use std::path::PathBuf;
        // A short relative path below a deep current directory (injected, so the test does
        // not change the process's current directory).
        let deep = PathBuf::from(format!("C:\\{}", "d".repeat(260)));
        let rel = Path::new("a.md");
        let w = imp::wide_with(rel, |p| Ok(deep.join(p))).unwrap();
        let s = String::from_utf16_lossy(&w[..w.len() - 1]);
        assert!(s.starts_with("\\\\?\\C:\\ddd"), "{s}");
        assert!(s.ends_with("\\a.md"));
        // And a short absolute result stays unprefixed.
        let w = imp::wide_with(rel, |p| Ok(PathBuf::from("C:\\v").join(p))).unwrap();
        assert_eq!(String::from_utf16_lossy(&w[..w.len() - 1]), "C:\\v\\a.md");
    }

    #[test]
    fn export_rename_noreplace_never_overwrites() {
        let d = tmp("taken");
        std::fs::write(d.join("a"), "A").unwrap();
        std::fs::write(d.join("b"), "B").unwrap();
        let e = rename_noreplace(&d.join("a"), &d.join("b")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "B");
        assert_eq!(
            std::fs::read_to_string(d.join("a")).unwrap(),
            "A",
            "source kept"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
