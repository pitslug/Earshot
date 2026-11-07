//! Running llama.cpp from another folder of its DLLs (T8.4ada, docs/cuda-install.md).
//!
//! The x64 install holds llama.cpp on Vulkan beside the exe. Its CUDA build is a second set of
//! the same DLL names in a folder of its own, and which set loads is what decides the backend:
//! the exe's calls are the same either way. [`load_from_folder`] loads that folder's `llama.dll`
//! by full path with `LOAD_WITH_ALTERED_SEARCH_PATH`, so its imports come from that folder first,
//! before the exe's folder and `PATH`. The exe delay-loads `llama.dll`, and Windows answers a
//! later load of a name already loaded with that module, so every llama call binds to this one.
//!
//! Call it before the first llama call of the process: a loaded DLL cannot be replaced, so a
//! change of set takes a restart. A failure here is an error value, where a failed delay-load
//! would be an exception the app could not recover from, and leaves nothing loaded. The caller
//! then stays on the set beside the exe.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The file this loads.
const LLAMA_DLL: &str = "llama.dll";

/// The folder loaded, once one was.
static LOADED: OnceLock<PathBuf> = OnceLock::new();

/// Why a folder of DLLs could not be loaded.
#[derive(Debug, PartialEq, Eq)]
pub struct FolderRefused {
    /// The Windows error (126: a file or one of its imports is missing; 193: not a usable DLL).
    pub os_error: i32,
}

impl std::fmt::Display for FolderRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Windows error {}", self.os_error)
    }
}

impl std::error::Error for FolderRefused {}

/// Loads llama.cpp from `folder` for this process (module docs).
///
/// # Errors
/// [`FolderRefused`] when the folder lacks `llama.dll`, or the DLL or an import cannot load (for
/// example NVIDIA's cuBLAS, which the CUDA set needs and does not hold).
pub fn load_from_folder(folder: &Path) -> Result<(), FolderRefused> {
    imp::load(&folder.join(LLAMA_DLL))?;
    let _ = LOADED.set(folder.to_path_buf());
    Ok(())
}

/// The folder [`load_from_folder`] loaded in this process, `None` for the set beside the exe: a
/// child running the same model needs the same set.
#[must_use]
pub fn loaded_folder() -> Option<PathBuf> {
    LOADED.get().cloned()
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use super::FolderRefused;

    /// `LOAD_WITH_ALTERED_SEARCH_PATH`: imports are searched from the loaded file's folder first.
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    }

    pub(super) fn load(dll: &Path) -> Result<(), FolderRefused> {
        let wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path alive for the call, the reserved `file`
        // argument is null as documented, and the module stays loaded for the process's life on
        // purpose, so the returned handle needs no closing.
        let module = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
        };
        if module.is_null() {
            return Err(FolderRefused {
                os_error: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            });
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use super::FolderRefused;

    pub(super) fn load(_dll: &Path) -> Result<(), FolderRefused> {
        Err(FolderRefused { os_error: 0 })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("earshot-dll-folder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn dll_folder_without_the_dll_is_an_error_not_a_crash() {
        let d = temp("empty");
        assert_eq!(load_from_folder(&d), Err(FolderRefused { os_error: 126 }));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn dll_folder_with_a_damaged_dll_is_an_error_not_a_crash() {
        let d = temp("damaged");
        std::fs::write(d.join("llama.dll"), b"not a library").unwrap();
        let refused = load_from_folder(&d).unwrap_err();
        assert_eq!(refused.os_error, 193, "{refused}");
        assert_eq!(refused.to_string(), "Windows error 193");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
