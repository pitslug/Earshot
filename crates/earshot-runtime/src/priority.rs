//! Below-normal OS priority for background work (T6.13d3): the live speaker thread and the
//! ONNX Runtime pool threads of its speaker models run only on CPU time that live
//! transcription, capture and other applications leave unused. The Windows call lives in
//! the WASAPI adapter crate (all `unsafe` there); other platforms have no implementation
//! yet, so background work is refused there rather than run at normal priority.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set when an ONNX Runtime pool thread could not lower its priority ([`lower_pool_thread`]
/// cannot report it to anyone; [`pool_failed`] reads it).
static POOL_FAILED: AtomicBool = AtomicBool::new(false);

/// [`POOL_FAILED`] for the previews' pool threads, apart: a preview never costs speaker work
/// (`live::preview` docs), nor the reverse.
static PREVIEW_POOL_FAILED: AtomicBool = AtomicBool::new(false);

/// [`lower_pool_thread`] for a preview session's pool threads.
pub(crate) fn lower_preview_pool_thread() {
    if lower_current_thread().is_err() {
        PREVIEW_POOL_FAILED.store(true, Ordering::SeqCst);
    }
}

/// Whether any preview pool thread failed to lower its priority (sticky for the process).
pub(crate) fn preview_pool_failed() -> bool {
    PREVIEW_POOL_FAILED.load(Ordering::SeqCst)
}

/// Lowers the calling thread to below-normal priority.
///
/// # Errors
/// Why it could not (the OS refused, or the platform has no implementation).
pub(crate) fn lower_current_thread() -> Result<(), String> {
    #[cfg(windows)]
    {
        earshot_capture_wasapi::wasapi::lower_current_thread_priority().map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("below-normal thread priority is implemented on Windows only".to_owned())
    }
}

/// The start function of background ONNX Runtime pool threads
/// ([`earshot_onnx::SessionConfig::background`]): lowers the thread's priority and
/// records a failure for [`pool_failed`].
pub(crate) fn lower_pool_thread() {
    if lower_current_thread().is_err() {
        POOL_FAILED.store(true, Ordering::SeqCst);
    }
}

/// Whether any background pool thread failed to lower its priority (sticky for the
/// process).
pub(crate) fn pool_failed() -> bool {
    POOL_FAILED.load(Ordering::SeqCst)
}

/// The calling thread's priority on Windows (0 normal, -1 below normal), for tests; panics
/// if Windows cannot report it.
#[cfg(all(test, windows))]
pub(crate) fn current() -> i32 {
    earshot_capture_wasapi::wasapi::current_thread_priority().expect("GetThreadPriority")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn priority_pool_thread_start_lowers_the_thread_it_runs_on() {
        // The production start function of background ONNX pool threads
        // (`backends::with_live_speakers`), run as ONNX Runtime runs it: first thing on a
        // fresh thread.
        let seen = std::thread::spawn(|| {
            lower_pool_thread();
            current()
        })
        .join()
        .unwrap();
        assert_eq!(seen, BELOW_NORMAL);
        assert!(!pool_failed());
        assert_eq!(current(), 0, "only the pool thread changed");
    }
}

/// Windows' below-normal priority as [`current`] reports it.
#[cfg(all(test, windows))]
pub(crate) const BELOW_NORMAL: i32 = earshot_capture_wasapi::wasapi::BELOW_NORMAL_PRIORITY;
