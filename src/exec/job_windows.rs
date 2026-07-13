use anyhow::{Context, Result};
use std::process::{Child, Command, ExitStatus};

/// Exit immediately. The job handle (KILL_ON_JOB_CLOSE) is still open, so
/// process exit closes it and Windows kills the entire child tree. Called
/// from the stdin-EOF watcher (host/connection died).
pub fn emergency_stop() -> ! {
    std::process::exit(130)
}

/// Spawn the child inside a job object with KILL_ON_JOB_CLOSE. If the agent
/// dies (ssh disconnect, host Ctrl-C), the job handle closes and Windows
/// kills the entire process tree — cargo's rustc grandchildren included,
/// which plain session teardown does not do. `after_registered` runs once the
/// child is inside the job, with the live child in hand: the agent starts its
/// liveness watcher there, and hands the child its stdin payload.
pub fn spawn_and_wait(
    mut cmd: Command,
    after_registered: impl FnOnce(&mut Child),
) -> Result<ExitStatus> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let job = unsafe { CreateJobObjectW(None, None) }.context("CreateJobObject failed")?;
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("SetInformationJobObject failed")?;

    let mut child = cmd.spawn().context("failed to spawn command")?;
    // Tiny window between spawn and assignment; acceptable for build tools.
    unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }
        .context("AssignProcessToJobObject failed")?;
    after_registered(&mut child);

    let status = child.wait().context("failed to wait for command")?;
    // HANDLE is Copy, so dropping it would NOT close it: close explicitly.
    // (If this process dies before reaching here, the OS closes the handle
    // and KILL_ON_JOB_CLOSE takes the tree down — the point of the job.)
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(job);
    }
    Ok(status)
}
