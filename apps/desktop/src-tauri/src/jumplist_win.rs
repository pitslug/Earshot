//! The taskbar jump list through COM (T14.71): `ICustomDestinationList` with one user task per
//! [`crate::jumplist::entries`], each an `IShellLink` to this exe with its fixed argument and the
//! title in its property store. Every `unsafe` block holds one OS call or one COM method call and
//! has its SAFETY comment.
//!
//! COM is per thread: the list is set on a thread of its own in a single-threaded apartment
//! (thread `earshot-jumplist`), fed through a channel, so nothing on the start-up path, the main
//! thread or a meeting's thread waits for the shell, and no lock is held across a COM call.
//! Changes that arrive close together are set once, as the last of them. A list the shell
//! refuses is said nowhere: the taskbar keeps the list it had, and nothing else depends on it.
//!
//! The process's app id is set to the one the toasts use ([`crate::toast_win::APP_ID`], the
//! bundle identifier the installer's Start menu shortcut carries), first thing in the app's own
//! mode, so the window's taskbar button, the notifications and the jump list are one app however
//! Earshot was started (from the shortcut, from the `Run` value, or by hand). The uninstaller's
//! `DeleteAppUserModelId` of that id removes the list.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
    SetCurrentProcessExplicitAppUserModelID, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface};

use crate::jumplist::{JumpEntry, entries};
use crate::shell::Phase;

/// `PKEY_Title` (`propkey.h`): the text a jump list task shows. Written here so the crate needs
/// no `Win32_Storage_EnhancedStorage` feature for one constant.
const PKEY_TITLE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
    pid: 2,
};

/// How long the thread waits for the next change before it sets the list: changes closer
/// together than this are set once (a judgement; a phase lasts far longer).
const SETTLE: Duration = Duration::from_millis(200);

/// The name of the mutex tauri-plugin-single-instance 2.5.5 holds while an Earshot runs
/// (`platform_impl/windows.rs`: `{identifier}-sim`, its `semver` feature off).
#[must_use]
pub fn instance_mutex_name() -> String {
    format!("{}-sim", crate::toast_win::APP_ID)
}

/// Whether an Earshot runs in this session: the single-instance plugin's mutex exists. Asked
/// only by a jump list start, first in `main`.
#[must_use]
pub fn app_running() -> bool {
    let name = HSTRING::from(instance_mutex_name());
    // SAFETY: `name` is a NUL-terminated wide string that outlives the call; only the right to
    // wait is asked for, and the mutex is never waited on or released here.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) } {
        Ok(handle) => {
            // SAFETY: `handle` was just opened by this call and is closed exactly once.
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(_) => false,
    }
}

/// Gives this process the app id the toasts and the jump list use. Before any window exists; a
/// refusal leaves Windows's own id, and the jump list then only shows on a button started from
/// the Start menu shortcut.
pub fn use_app_id() {
    let id = HSTRING::from(crate::toast_win::APP_ID);
    // SAFETY: `id` is a NUL-terminated wide string that outlives the call (Windows copies it);
    // called on the main thread before any window is made.
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(&id) };
}

/// The jump list's thread: [`JumpList::show`] hands it the phase to show.
pub struct JumpList {
    tx: Sender<Phase>,
}

impl JumpList {
    /// Starts the thread and shows the list for `phase`. `None` when the thread cannot start
    /// (then there is no list, and nothing else changes).
    #[must_use]
    pub fn start(phase: Phase) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(phase).ok()?;
        std::thread::Builder::new()
            .name("earshot-jumplist".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&exe, crate::toast_win::APP_ID, &rx);
                }));
            })
            .ok()?;
        Some(Self { tx })
    }

    /// Shows the list for `phase` soon. Never blocks.
    pub fn show(&self, phase: Phase) {
        let _ = self.tx.send(phase);
    }
}

/// The last change of a burst: waits until none has come for [`SETTLE`]. `None` when every
/// sender is gone.
fn settled(rx: &Receiver<Phase>) -> Option<Phase> {
    let mut last = rx.recv().ok()?;
    loop {
        match rx.recv_timeout(SETTLE) {
            Ok(phase) => last = phase,
            Err(RecvTimeoutError::Timeout) => return Some(last),
            // Gone while settling: still set what was asked last.
            Err(RecvTimeoutError::Disconnected) => return Some(last),
        }
    }
}

/// Uninitialises COM on this thread when dropped (only after a successful initialise).
struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful `CoInitializeEx` on this same thread that made this
        // value; every COM object of the thread is released before it drops.
        unsafe { CoUninitialize() };
    }
}

fn run(exe: &std::path::Path, app_id: &str, rx: &Receiver<Phase>) {
    // SAFETY: called once on this new thread, before any other COM call on it.
    if unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_err() {
        return;
    }
    let _apartment = Apartment;
    let mut shown: Option<[JumpEntry; 2]> = None;
    while let Some(phase) = settled(rx) {
        let want = entries(phase);
        if shown == Some(want) {
            continue;
        }
        // A refusal keeps the list the taskbar had and is tried again at the next change.
        shown = set_list(exe, app_id, &want).ok().map(|()| want);
    }
}

/// Sets `app_id`'s jump list to the user tasks `tasks`, each starting `exe` with its argument.
///
/// # Errors
/// COM's, when the shell refuses any step; the list then stays as it was.
pub fn set_list(
    exe: &std::path::Path,
    app_id: &str,
    tasks: &[JumpEntry],
) -> windows::core::Result<()> {
    // SAFETY: COM is initialised on this thread (the caller's apartment); the CLSID and
    // interface are the shell's documented destination list pair.
    let list: ICustomDestinationList =
        unsafe { CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER) }?;
    let id = HSTRING::from(app_id);
    // SAFETY: `id` is a NUL-terminated wide string that outlives the call; the list copies it.
    unsafe { list.SetAppID(&id) }?;
    let mut slots = 0u32;
    // SAFETY: `slots` is a live u32 the call writes; the returned array of items the user
    // removed is dropped unread (a task list has none to honour).
    let _removed: IObjectArray = unsafe { list.BeginList(&mut slots) }?;
    // SAFETY: COM is initialised on this thread; the shell's documented object collection.
    let collection: IObjectCollection =
        unsafe { CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER) }?;
    for task in tasks {
        let link = task_link(exe, task)?;
        // SAFETY: `link` is a live shell link of this apartment; the collection adds a reference.
        unsafe { collection.AddObject(&link) }?;
    }
    let array: IObjectArray = collection.cast()?;
    // SAFETY: `array` is a live object array of this apartment, read during the call.
    unsafe { list.AddUserTasks(&array) }?;
    // SAFETY: `BeginList` succeeded on `list`, so a list is open to commit.
    unsafe { list.CommitList() }
}

/// A shell link that starts `exe` with `task`'s argument and shows `task`'s title.
fn task_link(exe: &std::path::Path, task: &JumpEntry) -> windows::core::Result<IShellLinkW> {
    // SAFETY: COM is initialised on this thread; the shell's documented shell link object.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?;
    let path = HSTRING::from(exe.as_os_str());
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call; the link copies it.
    unsafe { link.SetPath(&path) }?;
    let arg = HSTRING::from(task.action.arg());
    // SAFETY: as above, for the fixed argument.
    unsafe { link.SetArguments(&arg) }?;
    // SAFETY: as above; the exe's first icon, the one the taskbar button shows.
    unsafe { link.SetIconLocation(&path, 0) }?;
    let store: IPropertyStore = link.cast()?;
    // A BSTR value: the shell reads the title through the property system, which takes it.
    let title = PROPVARIANT::from(task.title);
    // SAFETY: `PKEY_TITLE` and `title` are live for the call; the store copies the value, and
    // `title` is cleared once by its own drop.
    unsafe { store.SetValue(&PKEY_TITLE, &title) }?;
    // SAFETY: `store` is the link's live property store.
    unsafe { store.Commit() }?;
    Ok(link)
}

/// Removes `app_id`'s jump list (the real test's clean-up).
///
/// # Errors
/// COM's.
#[cfg(test)]
pub fn delete_list(app_id: &str) -> windows::core::Result<()> {
    // SAFETY: COM is initialised on this thread; the shell's documented destination list.
    let list: ICustomDestinationList =
        unsafe { CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER) }?;
    let id = HSTRING::from(app_id);
    // SAFETY: `id` is a NUL-terminated wide string that outlives the call.
    unsafe { list.DeleteList(&id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumplist_win_mutex_is_the_single_instance_plugins_for_this_identifier() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        assert_eq!(
            instance_mutex_name(),
            format!("{}-sim", conf["identifier"].as_str().expect("identifier"))
        );
        // The plugin's own name, read from its source when it was pinned: a plugin upgrade
        // re-reads `platform_impl/windows.rs`.
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains("tauri-plugin-single-instance = \"=2.5.5\""),
            "the single-instance plugin moved: re-read its mutex name"
        );
    }

    #[test]
    fn jumplist_win_changes_close_together_are_set_once_as_the_last() {
        let (tx, rx) = std::sync::mpsc::channel();
        for phase in [Phase::Idle, Phase::Starting, Phase::Recording] {
            tx.send(phase).expect("send");
        }
        assert_eq!(settled(&rx), Some(Phase::Recording));
        // Control: a change on its own is that change.
        tx.send(Phase::Finishing).expect("send");
        assert_eq!(settled(&rx), Some(Phase::Finishing));
        drop(tx);
        assert_eq!(settled(&rx), None);
    }

    /// Every `.customDestinations-ms` file of this user, with its bytes.
    fn custom_destinations() -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let dir = std::path::PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA"))
            .join(r"Microsoft\Windows\Recent\CustomDestinations");
        std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    #[ignore = "writes this user's taskbar jump list for an invented app id, then deletes it: \
                cargo test -p earshot-desktop --bin earshot jumplist_win_real -- --ignored"]
    fn jumplist_win_real_list_is_set_read_back_and_deleted() {
        let app_id = format!("earshot.jumplist-test.{}", std::process::id());
        let exe = std::env::current_exe().expect("exe");
        let tasks = entries(Phase::Recording);
        // SAFETY: once on this test's thread before any other COM call; uninitialised by the
        // guard after every COM object below is released.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .expect("COM");
        let _apartment = Apartment;
        let before = custom_destinations();
        set_list(&exe, &app_id, &tasks).expect("set the list");
        // Read back from the file the shell wrote (new or changed, so a real Earshot's own list
        // is never taken for it): it names each title and argument.
        let files = custom_destinations();
        let ours: Vec<_> = files
            .iter()
            .filter(|f| !before.contains(f))
            .filter(|(_, bytes)| holds(bytes, &utf16(crate::jumplist::QUIT_ARG)))
            .filter(|(_, bytes)| holds(bytes, &utf16("Stop meeting")))
            .filter(|(_, bytes)| holds(bytes, &utf16(crate::jumplist::MEETING_ARG)))
            .filter(|(_, bytes)| holds(bytes, &utf16(crate::shell::QUIT_LABEL)))
            .map(|(p, _)| p.clone())
            .collect();
        assert!(!ours.is_empty(), "no jump list file names the tasks");
        delete_list(&app_id).expect("delete the list");
        let left: Vec<_> = ours.iter().filter(|p| p.exists()).collect();
        assert!(left.is_empty(), "the list was not deleted: {left:?}");
    }
}
