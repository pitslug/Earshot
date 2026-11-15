//! Suspend and resume (T11.3ca): when Windows is about to sleep, the sync thread tells the
//! other computers this one is gone (waiting at most [`SLEEP_GONE_WAIT`]); after a resume it
//! says online again and syncs. `PowerRegisterSuspendResumeNotification` with a callback, so no
//! window is needed; the callback runs on a system thread. Registered once for the app's life
//! and never unregistered (the process ends first), so the subscription is leaked on purpose.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
};

use crate::sync_settings::{SLEEP_GONE_WAIT, SyncHost};

/// What Windows said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerChange {
    /// About to sleep.
    Suspend,
    /// Woke up (automatic resume, or one the user started; both arrive on a user's wake, the
    /// second action is harmless).
    Resume,
}

/// The change a power broadcast means; `None` for the ones that do not matter here.
#[must_use]
pub fn change_of(kind: u32) -> Option<PowerChange> {
    match kind {
        PBT_APMSUSPEND => Some(PowerChange::Suspend),
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => Some(PowerChange::Resume),
        _ => None,
    }
}

static HOST: OnceLock<Arc<SyncHost>> = OnceLock::new();

/// Starts watching for suspend and resume for the app's life. Returns whether Windows took
/// the subscription; a refusal only means presence ends by the online window after a sleep.
pub fn watch(host: Arc<SyncHost>) -> bool {
    if HOST.set(host).is_err() {
        return false;
    }
    // Lives for the process: Windows reads it until unregistered, which never happens.
    let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
        Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_power),
            Context: std::ptr::null_mut(),
        }));
    let mut registration: *mut c_void = std::ptr::null_mut();
    // SAFETY: with DEVICE_NOTIFY_CALLBACK the recipient is a pointer to a
    // DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, here leaked so it outlives the subscription;
    // `on_power` has the PDEVICE_NOTIFY_CALLBACK_ROUTINE signature; `registration` is a valid
    // place for the handle, which is kept nowhere because it is never unregistered.
    let answer = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(std::ptr::from_mut(params).cast()),
            &raw mut registration,
        )
    };
    answer == ERROR_SUCCESS
}

/// The callback: a system thread, so a panic is caught here and never crosses into Windows.
unsafe extern "system" fn on_power(
    _context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let (Some(host), Some(change)) = (HOST.get(), change_of(kind)) {
            handle(host, change);
        }
    }));
    ERROR_SUCCESS.0
}

/// What the app does on a change: blocks a suspend at most [`SLEEP_GONE_WAIT`].
pub fn handle(host: &SyncHost, change: PowerChange) {
    match change {
        PowerChange::Suspend => host.say_gone_within(SLEEP_GONE_WAIT),
        PowerChange::Resume => host.back(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_power_broadcasts_map_to_suspend_and_resume_only() {
        assert_eq!(change_of(4), Some(PowerChange::Suspend), "PBT_APMSUSPEND");
        assert_eq!(
            change_of(18),
            Some(PowerChange::Resume),
            "PBT_APMRESUMEAUTOMATIC"
        );
        assert_eq!(
            change_of(7),
            Some(PowerChange::Resume),
            "PBT_APMRESUMESUSPEND"
        );
        // Power status and setting changes, battery low and the rest change nothing.
        for other in [10u32, 9, 0x8013] {
            assert_eq!(change_of(other), None, "{other}");
        }
    }
}
