//! Windows sandbox backend: Job Objects. Unlike bwrap/seatbelt this is
//! RESOURCE CONTAINMENT, not a full security boundary: it guarantees
//! tree-kill on close/cancel and can cap process count and job memory, but
//! it does not restrict file-system access (Windows has no usable user-mode
//! equivalent without AppContainer/restricted tokens — deferred).
//!
//! The job handle is held for the lifetime of the command; dropping it with
//! JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE set reaps the whole process tree,
//! which also fixes orphaned grandchildren that `taskkill /T` can miss.

#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

use crate::sandbox::SandboxSpec;

/// A job object owning one command's process tree.
pub struct Job {
    // HANDLE is *mut c_void (not Send); store as isize, the job is only
    // ever manipulated through Win32 calls that are thread-safe per handle.
    handle: isize,
}

// SAFETY: the HANDLE is owned exclusively by this Job; Win32 job APIs are
// safe to call from any thread given a valid handle.
unsafe impl Send for Job {}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").finish_non_exhaustive()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: valid handle owned by self.
        unsafe { CloseHandle(self.handle as HANDLE) };
    }
}

impl Job {
    fn handle(&self) -> HANDLE {
        self.handle as HANDLE
    }

    /// Kill every process in the job (used on cancel/timeout).
    pub fn terminate(&self) {
        // SAFETY: valid handle owned by self.
        unsafe {
            TerminateJobObject(self.handle(), 1);
        }
    }
}

/// Create a job object with tree-kill + spec limits and assign `pid`.
/// Returns None when assignment fails (e.g. the process is already in an
/// incompatible job); the caller falls back to plain kill_process_tree.
pub fn assign(pid: u32, spec: &SandboxSpec) -> Option<Job> {
    // SAFETY: all Win32 calls use valid pointers/zeroed structs; handles
    // are checked for null and closed on every failure path.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut flags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if spec.max_processes.is_some() {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        }
        if spec.max_memory_mb.is_some() {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = flags;
        if let Some(max) = spec.max_processes {
            info.BasicLimitInformation.ActiveProcessLimit = max;
        }
        if let Some(mb) = spec.max_memory_mb {
            info.JobMemoryLimit = (mb as usize) * 1024 * 1024;
        }
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return None;
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return None;
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            CloseHandle(job);
            return None;
        }
        Some(Job {
            handle: job as isize,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn job_terminate_kills_process_tree() {
        // Spawn a long-lived process, assign it to a job, terminate the job.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-t", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let spec = SandboxSpec {
            writable: vec![],
            network: true,
            ..Default::default()
        };
        let job = assign(child.id(), &spec).expect("job assignment should work");
        job.terminate();
        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "terminated process should not exit 0: {status}"
        );
    }
}
