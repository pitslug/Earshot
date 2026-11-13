//! A Windows job object that kills its processes when its last handle closes, so a child
//! and everything it starts end with the run, however the run ends. All `unsafe` of this
//! crate lives here, each call with its SAFETY comment. [`peak_memory`] (T8.1) shares the
//! file because it is the crate's other Windows call.
//!
//! The five job kernel32 functions and the three job structures are declared by hand, as are
//! the two calls and one structure of [`peak_memory`] (`Cargo.toml`: `windows-sys` would need
//! features nothing else in the workspace builds). A process
//! started between the spawn and [`KillOnClose::assign`] is not in the job (crate docs). Layouts follow
//! `winnt.h` (`JOBOBJECT_BASIC_LIMIT_INFORMATION`, `IO_COUNTERS`,
//! `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`); `SetInformationJobObject` refuses a wrong size
//! with `ERROR_BAD_LENGTH`, and the size is pinned by a test.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;

type Handle = *mut c_void;
type Bool = i32;

/// `JobObjectExtendedLimitInformation` (`JOBOBJECTINFOCLASS`).
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: u32) -> Bool;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> Bool;
    fn CloseHandle(handle: Handle) -> Bool;
    fn GetCurrentProcess() -> Handle;
    fn K32GetProcessMemoryInfo(process: Handle, counters: *mut MemoryCounters, size: u32) -> Bool;
}

/// `PROCESS_MEMORY_COUNTERS` (`psapi.h`); kernel32 exports the call as `K32...` since Windows 7.
#[repr(C)]
#[derive(Default)]
struct MemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

/// This process's peak memory so far, as Windows counts it; `None` if the call fails. Video
/// memory is not in it.
#[must_use]
pub fn peak_memory() -> Option<crate::PeakMemory> {
    let mut counters = MemoryCounters::default();
    let size = u32::try_from(size_of::<MemoryCounters>()).ok()?;
    counters.cb = size;
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing; `counters` is
    // a live, writable structure of the `size` passed.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, size) };
    (ok != 0).then_some(crate::PeakMemory {
        working_set: counters.peak_working_set_size as u64,
        committed: counters.peak_pagefile_usage as u64,
    })
}

/// An owned job object with kill-on-close; closing it (drop) kills every process in it.
#[derive(Debug)]
pub struct KillOnClose(Handle);

// SAFETY: a job object handle is a kernel handle usable from any thread; this type only
// passes it to thread-safe Win32 calls and closes it once, in `drop`.
unsafe impl Send for KillOnClose {}
// SAFETY: as for `Send`: the calls made through `&self` (`TerminateJobObject`,
// `AssignProcessToJobObject`) are safe to make concurrently on one handle.
unsafe impl Sync for KillOnClose {}

impl KillOnClose {
    /// A new, unnamed job object with kill-on-close.
    ///
    /// # Errors
    /// Windows refuses to create or configure it.
    pub fn new() -> io::Result<Self> {
        // SAFETY: no security attributes (null: the default) and no name (null); the call
        // returns an owned handle or null with the error in GetLastError.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        let mut info = ExtendedLimitInformation::default();
        info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size =
            u32::try_from(std::mem::size_of::<ExtendedLimitInformation>()).unwrap_or(u32::MAX);
        // SAFETY: `info` is a live, initialised JOBOBJECT_EXTENDED_LIMIT_INFORMATION owned by
        // this frame for the whole call and `size` is its exact size; the job handle is valid
        // (owned by `job`, which closes it on an early return).
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&raw const info).cast(),
                size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Puts `child` into the job.
    ///
    /// # Errors
    /// Windows refuses (the child already ended, or access is denied).
    pub fn assign(&self, child: &std::process::Child) -> io::Result<()> {
        let process: Handle = child.as_raw_handle();
        // SAFETY: both handles are valid for the call: the job is owned by `self`, and
        // `process` is borrowed from `child`, which outlives the call and closes it itself.
        let ok = unsafe { AssignProcessToJobObject(self.0, process) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Kills every process in the job now.
    pub fn terminate(&self) {
        // SAFETY: the job handle is valid (owned by `self`); terminating an empty job or
        // one whose processes already ended is harmless.
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for KillOnClose {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed exactly once, here; closing the
        // last handle kills the job's processes (kill-on-close).
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_memory_counters_have_the_psapi_size() {
        // psapi.h: 72 bytes on 64-bit Windows (x64 and ARM64), 40 on 32-bit.
        let expected = if cfg!(target_pointer_width = "64") {
            72
        } else {
            40
        };
        assert_eq!(size_of::<MemoryCounters>(), expected);
    }

    #[test]
    fn proc_peak_memory_grows_with_touched_memory() {
        const SIZE: usize = 256 * 1024 * 1024;
        let before = peak_memory().expect("peak memory");
        let mut block = vec![0u8; SIZE];
        for page in block.chunks_mut(4096) {
            page[0] = 1;
        }
        std::hint::black_box(&block);
        let after = peak_memory().expect("peak memory");
        assert!(after.working_set >= SIZE as u64, "{after:?}");
        assert!(after.committed >= SIZE as u64, "{after:?}");
        assert!(
            after.working_set >= before.working_set,
            "{before:?} {after:?}"
        );
    }

    #[test]
    fn proc_job_structure_has_the_winnt_size() {
        // winnt.h: 144 bytes on 64-bit Windows (x64 and ARM64), 112 on 32-bit.
        let expected = if cfg!(target_pointer_width = "64") {
            144
        } else {
            112
        };
        assert_eq!(std::mem::size_of::<ExtendedLimitInformation>(), expected);
    }

    #[test]
    fn proc_job_kills_its_process_when_closed() {
        // A child that would run for two minutes ends at once when the job closes.
        let started = std::time::Instant::now();
        let mut child = std::process::Command::new("ping")
            .args(["-n", "120", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let job = KillOnClose::new().unwrap();
        job.assign(&child).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "still running in the job"
        );
        drop(job);
        child.wait().unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "killed by the job, not finished: {:?}",
            started.elapsed()
        );
    }
}
