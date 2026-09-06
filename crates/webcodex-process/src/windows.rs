//! Windows Job Object backend for [`ManagedChild`].
//!
//! A [`ManagedChild`] owns a private Job Object configured with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The direct child is spawned
//! suspended, assigned to the job, and only then resumed — so it can never run
//! user code (or spawn descendants) outside the job. Terminating the tree is
//! `TerminateJobObject`; dropping the last job handle fires the kill-on-close
//! semantics as a fail-safe.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
};
use windows_sys::Win32::System::Threading::{
    GetProcessId, OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

use crate::{GracefulTermination, SpawnOptions};

/// `Thread32Next` reports this when enumeration reaches the end of the table.
const ERROR_NO_MORE_FILES: u32 = 18;
/// Poll interval used by `wait_tree_exit`.
const TREE_POLL: Duration = Duration::from_millis(20);

/// A private Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set.
struct JobObject(OwnedHandle);

impl JobObject {
    fn create() -> io::Result<Self> {
        // SAFETY: unnamed job with default security — both parameters are NULL.
        // The returned handle is checked and then owned by `OwnedHandle`.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(win_error("CreateJobObjectW"));
        }
        // SAFETY: the non-null handle was returned by CreateJobObjectW and is
        // transferred exactly once to the standard library RAII wrapper.
        let job = unsafe { OwnedHandle::from_raw_handle(job as RawHandle) };
        Ok(Self(job))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    /// Configure the job so the kernel terminates every contained process when
    /// the last job handle is closed.
    fn set_limits(&self, silent_child_breakaway: bool) -> io::Result<()> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if silent_child_breakaway {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        }
        // SAFETY: `info` is a valid, initialized
        // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` of exactly the size this info
        // class expects, and the job handle is valid for the call.
        let ok = unsafe {
            SetInformationJobObject(
                self.raw(),
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(win_error("SetInformationJobObject"));
        }
        Ok(())
    }

    /// Assign an already-created process to this job.
    fn assign(&self, process_handle: HANDLE) -> io::Result<()> {
        // SAFETY: both handles are valid open kernel handles for the duration
        // of the call.
        let ok = unsafe { AssignProcessToJobObject(self.raw(), process_handle) };
        if ok == 0 {
            return Err(win_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// Terminate every process currently in the job.
    fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the job handle is valid for the duration of the call.
        let ok = unsafe { TerminateJobObject(self.raw(), exit_code) };
        if ok == 0 {
            return Err(win_error("TerminateJobObject"));
        }
        Ok(())
    }

    /// Number of processes currently active in the job.
    fn active_processes(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is a valid, zero-initialized buffer of the size
        // `JobObjectBasicAccountingInformation` expects; the job handle is
        // valid; the return-length pointer may be NULL.
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut core::ffi::c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(win_error("QueryInformationJobObject"));
        }
        Ok(info.ActiveProcesses)
    }
}

/// A managed child process plus its entire descendant tree.
///
/// The explicit `Drop` implementation closes `job` before the remaining child
/// handles are released, firing `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` while the
/// direct child handle is still valid.
pub struct ManagedChild {
    job: Option<JobObject>,
    child: Child,
}

impl std::fmt::Debug for ManagedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `std::process::Child` has no `Debug`; print identity only.
        f.debug_struct("ManagedChild")
            .field("id", &self.child.id())
            .finish()
    }
}

impl ManagedChild {
    /// Spawn `command` inside a private Job Object tree.
    ///
    /// # Creation flags
    ///
    /// [`ManagedChild::spawn`] owns the setting of the Windows process
    /// creation flags. `std::os::windows::process::CommandExt::creation_flags`
    /// has no stable read-back API, so any flags the caller may have configured
    /// on `command` are replaced by `CREATE_SUSPENDED` plus the flags in
    /// [`SpawnOptions::windows_creation_flags`]. Pass extra flags through
    /// [`SpawnOptions`] instead of setting them on the [`Command`].
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        Self::spawn_with_options(command, SpawnOptions::default())
    }

    /// Like [`ManagedChild::spawn`], but with extra [`SpawnOptions`].
    pub fn spawn_with_options(command: &mut Command, options: SpawnOptions) -> io::Result<Self> {
        let job = JobObject::create()?;
        job.set_limits(options.windows_silent_child_breakaway)?;

        command.creation_flags(CREATE_SUSPENDED | options.windows_creation_flags);
        let spawn_result = command.spawn();
        // Do not leave CREATE_SUSPENDED on the reusable Command.
        command.creation_flags(options.windows_creation_flags);

        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                // `job` drops here, closing the (empty) job handle.
                return Err(error);
            }
        };

        // From here on a (suspended) child exists. Every failure path must
        // terminate it, reap it, and return an error rather than leak it.
        let process_handle = child.as_raw_handle() as HANDLE;
        if let Err(error) = job.assign(process_handle) {
            cleanup_failed_spawn(&mut child, &job);
            return Err(error);
        }
        if let Err(error) = resume_process_threads(process_handle) {
            cleanup_failed_spawn(&mut child, &job);
            return Err(error);
        }

        Ok(Self {
            job: Some(job),
            child,
        })
    }

    /// PID of the direct child.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Borrow the underlying [`Child`].
    pub fn child(&self) -> &Child {
        &self.child
    }

    /// Mutably borrow the underlying [`Child`].
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Wait only for the direct child.
    ///
    /// This must not be read as "the tree has exited": grandchildren owned by
    /// the job may still be running. To wait for the whole tree, use
    /// [`ManagedChild::wait_tree_exit`].
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Non-blocking check for the direct child.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Forcefully terminate the entire owned process tree.
    ///
    /// `TerminateJobObject` kills every process in the job, including the
    /// direct child and all descendants. An already-empty tree is treated as
    /// idempotent success.
    pub fn terminate_tree(&mut self) -> io::Result<()> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        match job.terminate(1) {
            Ok(()) => Ok(()),
            Err(error) => {
                // The job may already be empty (e.g. everything exited on its
                // own); treat that as success.
                match job.active_processes() {
                    Ok(0) => Ok(()),
                    Ok(_) | Err(_) => Err(error),
                }
            }
        }
    }

    /// Graceful tree termination is not available through Job Objects.
    ///
    /// Returns `Unsupported` without killing anything; callers may escalate
    /// explicitly with `terminate_tree`.
    pub fn request_terminate_tree(&mut self) -> io::Result<GracefulTermination> {
        Ok(GracefulTermination::Unsupported)
    }

    /// Wait until the job contains no live processes.
    ///
    /// Returns `Ok(true)` once `ActiveProcesses` reaches zero, `Ok(false)` if
    /// `timeout` elapses first. Polls with a short bounded interval; never
    /// blocks indefinitely on its own.
    pub fn wait_tree_exit(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tree wait timeout is too large",
            )
        })?;
        loop {
            match self.tree_is_empty() {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(TREE_POLL);
        }
    }

    /// Non-blocking check whether the owned process tree has fully exited.
    ///
    /// This is the non-blocking counterpart of [`ManagedChild::wait_tree_exit`]:
    /// `Ok(true)` means the job currently contains zero active processes,
    /// `Ok(false)` means at least one member is still running.
    pub fn try_tree_exit(&self) -> io::Result<bool> {
        self.tree_is_empty()
    }

    fn tree_is_empty(&self) -> io::Result<bool> {
        match &self.job {
            Some(job) => Ok(job.active_processes()? == 0),
            None => Ok(true),
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Close the job handle first: with KILL_ON_JOB_CLOSE set, the kernel
        // terminates every process in the job. Dropping `Child` then releases
        // the remaining process and pipe handles; Windows has no Unix-style
        // zombie reaping requirement. This path never waits or runs a command.
        self.job.take();
    }
}

/// Terminate a suspended child after a failed spawn and make a bounded effort
/// to observe its exit; closing the job catches anything already assigned.
fn cleanup_failed_spawn(child: &mut Child, job: &JobObject) {
    const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
    let _ = job.terminate(1);
    let _ = child.kill();
    let Some(deadline) = Instant::now().checked_add(CLEANUP_TIMEOUT) else {
        return;
    };
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TREE_POLL),
            Ok(None) => return,
        }
    }
}

/// Resume every thread of a process created with `CREATE_SUSPENDED`.
///
/// `std::process::Child` does not expose the primary thread handle, so threads
/// are located via a Toolhelp snapshot and each matching one is resumed.
fn resume_process_threads(process_handle: HANDLE) -> io::Result<()> {
    // SAFETY: `process_handle` is a valid open process handle owned by the
    // caller's Child, alive for the duration of the call.
    let pid = unsafe { GetProcessId(process_handle) };
    if pid == 0 {
        return Err(win_error("GetProcessId"));
    }

    // SAFETY: a thread snapshot is a read-only enumeration of the system
    // thread table; TH32CS_SNAPTHREAD with a zero target PID snapshots all
    // threads, and we filter by `pid` below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(win_error("CreateToolhelp32Snapshot"));
    }
    // SAFETY: the valid snapshot handle is transferred exactly once to the
    // standard library RAII wrapper and is closed on every return path.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };

    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry` is a valid, initialized THREADENTRY32 with dwSize set,
    // and the snapshot handle is valid for the call.
    if unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) } == 0 {
        let error = io::Error::last_os_error();
        return Err(with_context("Thread32First", error));
    }

    loop {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: OpenThread returns a valid thread handle or NULL; the
            // result is checked before use. THREAD_SUSPEND_RESUME grants the
            // right to resume the thread.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(with_context("OpenThread", io::Error::last_os_error()));
            }
            // SAFETY: the non-null thread handle is transferred exactly once
            // to the standard library RAII wrapper.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread as RawHandle) };
            // SAFETY: `thread` is a valid open thread handle. ResumeThread
            // returns the previous suspend count, or u32::MAX on failure.
            let previous = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
            if previous == u32::MAX {
                return Err(with_context("ResumeThread", io::Error::last_os_error()));
            }
            if previous != 1 {
                return Err(io::Error::other(format!(
                    "ResumeThread: expected suspend count 1, got {previous}",
                )));
            }
            // CREATE_SUSPENDED creates exactly one initial thread. User code
            // has not run yet, so after resuming that matching thread there is
            // no reason to enumerate the rest of the system-wide snapshot.
            return Ok(());
        }
        // SAFETY: continues the enumeration, updating `entry` in place.
        if unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("resume_process_threads: no thread found for pid {pid}"),
                ));
            }
            return Err(with_context("Thread32Next", error));
        }
    }
}

/// Wrap the current OS error with call-site context while preserving its
/// broad [`io::ErrorKind`]. A contextual `io::Error` cannot also expose the
/// original `raw_os_error()` through its top-level API.
fn win_error(context: &'static str) -> io::Error {
    with_context(context, io::Error::last_os_error())
}

fn with_context(context: &'static str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}
