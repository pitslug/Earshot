//! The Windows folder dialog of the Obsidian page (T7.4l): the shell's common item dialog
//! (`IFileOpenDialog` with `FOS_PICKFOLDERS`), owned by the app window so it is modal to it.
//! COM is per thread: [`pick_folder`] runs the dialog on a thread of its own in a
//! single-threaded apartment and returns when the user has chosen or cancelled. Every
//! `unsafe` call has its SAFETY comment.

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::HSTRING;

/// Shows the folder dialog owned by window `owner` (a raw `HWND` value; 0 for none), starting
/// in `start` when that folder exists, with `title`. `Ok(None)` when the user cancelled.
///
/// # Errors
/// COM or the dialog failed (not a cancel).
pub fn pick_folder(
    owner: isize,
    start: &str,
    title: &str,
) -> windows::core::Result<Option<String>> {
    let start = start.to_owned();
    let title = title.to_owned();
    std::thread::Builder::new()
        .name("folder-picker".into())
        .spawn(move || on_sta_thread(owner, &start, &title))
        .map_err(|_| windows::core::Error::empty())?
        .join()
        .unwrap_or_else(|_| Err(windows::core::Error::empty()))
}

/// Uninitialises COM on this thread when dropped (only after a successful initialise).
struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful `CoInitializeEx` on this same thread that made
        // this value; every COM object of the thread is released before (they are locals
        // declared after it, so they drop first).
        unsafe { CoUninitialize() };
    }
}

fn on_sta_thread(owner: isize, start: &str, title: &str) -> windows::core::Result<Option<String>> {
    // SAFETY: called once on this new thread, before any other COM call on it.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok()?;
    let _apartment = Apartment;
    // SAFETY: COM is initialised on this thread; the CLSID and interface are the shell's
    // documented file open dialog pair.
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: `dialog` is a live interface of this thread's apartment.
    let options = unsafe { dialog.GetOptions() }?;
    // SAFETY: as above; the flags are documented `FILEOPENDIALOGOPTIONS` values.
    unsafe {
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
    }?;
    let title = HSTRING::from(title);
    // SAFETY: `title` is a NUL-terminated wide string that outlives the call (the dialog
    // copies it).
    unsafe { dialog.SetTitle(&title) }?;
    if !start.is_empty() && std::path::Path::new(start).is_dir() {
        let start = HSTRING::from(start);
        // SAFETY: `start` is a NUL-terminated wide string that outlives the call; no bind
        // context.
        if let Ok(folder) = unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(&start, None) }
        {
            // SAFETY: `folder` is a live shell item of this apartment. A refusal only means
            // the dialog opens in its usual place.
            let _ = unsafe { dialog.SetFolder(&folder) };
        }
    }
    let owner = (owner != 0).then_some(HWND(owner as *mut core::ffi::c_void));
    // SAFETY: `owner` is the app window's handle (or none); the call blocks this thread until
    // the dialog closes, pumping its own messages.
    if let Err(e) = unsafe { dialog.Show(owner) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(None);
        }
        return Err(e);
    }
    // SAFETY: `Show` succeeded, so the dialog holds a result.
    let item = unsafe { dialog.GetResult() }?;
    // SAFETY: `item` is a live shell item; the returned string is allocated with the COM
    // task allocator and freed below exactly once, after it is copied.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }?;
    // SAFETY: `name` is a valid NUL-terminated wide string from `GetDisplayName`.
    let path = unsafe { name.to_string() };
    // SAFETY: `name` came from the COM task allocator and is not used after this.
    unsafe { CoTaskMemFree(Some(name.0 as *const core::ffi::c_void)) };
    Ok(Some(path.map_err(|_| windows::core::Error::empty())?))
}
