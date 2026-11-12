//! Tells Windows why Earshot holds up a restart, shut down or sign-out while it saves a
//! recording (T9.19): `ShutdownBlockReasonCreate` on the app's window with fixed copy, removed
//! again once the recording is saved. The text is fixed: never meeting content.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::core::HSTRING;

/// What Windows shows beside Earshot on its "apps are preventing restart" screen.
pub const REASON: &str = "Earshot is saving a recording.";

/// The reason, set on `hwnd` until dropped. Best effort: a refusal changes nothing else.
pub struct BlockReason {
    hwnd: HWND,
    set: bool,
}

impl BlockReason {
    /// Sets [`REASON`] on `hwnd`, which must be a top-level window of the calling thread (the
    /// app's main window, on the main thread).
    #[must_use]
    pub fn set(hwnd: HWND) -> Self {
        let reason = HSTRING::from(REASON);
        // SAFETY: `reason` is a valid NUL-terminated wide string that outlives the call, which
        // copies it; `hwnd` is the caller's window, valid for the call (a stale handle makes the
        // call fail, which is reported, not undefined).
        let set = unsafe { ShutdownBlockReasonCreate(hwnd, &reason) }.is_ok();
        Self { hwnd, set }
    }
}

impl Drop for BlockReason {
    fn drop(&mut self) {
        if self.set {
            // SAFETY: the same window the reason was set on; the call takes only the handle.
            let _ = unsafe { ShutdownBlockReasonDestroy(self.hwnd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_shutdown_reason_on_no_window_is_refused_and_dropped_quietly() {
        // A window that does not exist: Windows refuses, and nothing is destroyed at the drop.
        let reason = BlockReason::set(HWND(std::ptr::null_mut()));
        assert!(!reason.set);
        drop(reason);
    }

    #[test]
    fn meeting_shutdown_reason_is_fixed_copy() {
        assert_eq!(REASON, "Earshot is saving a recording.");
    }
}
