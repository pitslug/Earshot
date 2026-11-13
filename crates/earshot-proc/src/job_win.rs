//! A Windows job object that kills its processes when its last handle closes, so a child
//! and everything it starts end with the run, however the run ends. All `unsafe` of this
//! crate lives here, each call with its SAFETY comment. [`peak_memory`] (T8.1) shares the
//! file because it is the crate's other Windows call.
//!
//! The five job kernel32 functions and the three job structures are declared by hand, as are
//! the two calls and one structure of [`peak_memory`] and the five calls and one structure that
//! resume a suspended child (`Cargo.toml`: `windows-sys` would need
//! features nothing else in the workspace builds). [`KillOnClose::spawn`] starts a child
//! suspended and resumes it only once it is in the job (std cannot hand out the primary
//! thread's handle, so the thread is found through a toolhelp snapshot); a child spawned
//! elsewhere and then given to [`KillOnClose::assign`] runs outside the job until then. Layouts follow
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
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> Bool;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> Bool;
    fn OpenThread(access: u32, inherit: Bool, thread_id: u32) -> Handle;
    fn ResumeThread(thread: Handle) -> u32;
}

/// `CREATE_SUSPENDED` (winbase.h): the primary thread waits for `ResumeThread`.
const CREATE_SUSPENDED: u32 = 0x0000_0004;
/// `TH32CS_SNAPTHREAD` (tlhelp32.h): every thread of the system.
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
/// `THREAD_SUSPEND_RESUME` (winnt.h).
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
/// `INVALID_HANDLE_VALUE`: what `CreateToolhelp32Snapshot` returns on failure.
const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

/// `THREADENTRY32` (tlhelp32.h).
#[repr(C)]
#[derive(Default)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

/// Why [`KillOnClose::spawn`] gave no child.
#[derive(Debug)]
pub enum SpawnError {
    /// The program could not be started.
    Start(io::Error),
    /// It started but could not be put into the job or resumed (it was killed unrun).
    Job(io::Error),
}

/// Resumes every thread of process `pid` (a suspended new process has one: its primary
/// thread, whose handle std does not expose). Errors when none was found or one refused.
fn resume_threads_of(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    // SAFETY: no pointers; returns an owned snapshot handle or INVALID_HANDLE_VALUE.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a valid handle just returned to us and owned by nothing else; the
    // OwnedHandle closes it once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = ThreadEntry {
        size: u32::try_from(size_of::<ThreadEntry>()).unwrap_or(u32::MAX),
        ..ThreadEntry::default()
    };
    let mut resumed = 0usize;
    // SAFETY: the snapshot handle is valid (owned above); `entry` is a live, writable
    // THREADENTRY32 whose `size` is set as the call requires.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        if entry.owner_process_id == pid {
            // SAFETY: no pointers; returns an owned thread handle or null.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `thread` is a valid handle just returned and owned by nothing else.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: the thread handle is valid (owned above) and opened with
            // THREAD_SUSPEND_RESUME.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: as for Thread32First.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("the new process has no thread to resume"));
    }
    Ok(())
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

    /// Starts `cmd` inside the job: suspended (`CREATE_SUSPENDED` added to `flags`, which
    /// replace any creation flags set on `cmd`), put into the job, then resumed, so no code
    /// of the child, and nothing it starts, ever runs outside the job.
    ///
    /// # Errors
    /// [`SpawnError::Start`] when it cannot start; [`SpawnError::Job`] when it cannot be put
    /// into the job or resumed (the child is killed before it ran).
    pub fn spawn(
        &self,
        cmd: &mut std::process::Command,
        flags: u32,
    ) -> Result<std::process::Child, SpawnError> {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(flags | CREATE_SUSPENDED);
        let mut child = cmd.spawn().map_err(SpawnError::Start)?;
        if let Err(e) = self
            .assign(&child)
            .and_then(|()| resume_threads_of(child.id()))
        {
            // Only the child: the job may hold other processes of the caller's.
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnError::Job(e));
        }
        Ok(child)
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

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn IsProcessInJob(process: Handle, job: Handle, result: *mut Bool) -> Bool;
    }

    fn in_job(job: &KillOnClose, child: &std::process::Child) -> bool {
        let mut result: Bool = 0;
        // SAFETY: both handles are valid for the call (owned by `job` and `child`, which
        // outlive it); `result` is a live, writable Bool.
        let ok = unsafe { IsProcessInJob(child.as_raw_handle(), job.0, &raw mut result) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        result != 0
    }

    #[test]
    fn proc_job_spawn_runs_the_child_only_inside_the_job() {
        // Started suspended, put into the job, then resumed: it is in the job when `spawn`
        // returns and runs to its normal end.
        let job = KillOnClose::new().unwrap();
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "echo resumed"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        let child = job.spawn(&mut cmd, 0).unwrap();
        assert!(in_job(&job, &child));
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "resumed");
    }

    #[test]
    fn proc_job_spawn_keeps_the_callers_creation_flags() {
        // BELOW_NORMAL_PRIORITY_CLASS passed through: the child reports it (WMIC is gone, so
        // PowerShell reads its own priority class).
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let job = KillOnClose::new().unwrap();
        let mut cmd = std::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Id $PID).PriorityClass",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
        let child = job.spawn(&mut cmd, BELOW_NORMAL_PRIORITY_CLASS).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "BelowNormal");
    }

    #[test]
    fn proc_job_spawn_reports_a_program_that_cannot_start() {
        let job = KillOnClose::new().unwrap();
        let e = job
            .spawn(
                &mut std::process::Command::new("no-such-program-earshot-proc"),
                0,
            )
            .unwrap_err();
        assert!(matches!(e, SpawnError::Start(_)), "{e:?}");
    }

    #[test]
    fn proc_job_spawned_child_dies_when_the_job_closes() {
        let started = std::time::Instant::now();
        let job = KillOnClose::new().unwrap();
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-n", "120", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = job.spawn(&mut cmd, 0).unwrap();
        assert!(child.try_wait().unwrap().is_none(), "running in the job");
        drop(job);
        child.wait().unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "killed by the job, not finished: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn proc_thread_entry_has_the_tlhelp32_size() {
        // tlhelp32.h THREADENTRY32: seven 32-bit fields on every Windows.
        assert_eq!(size_of::<ThreadEntry>(), 28);
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
