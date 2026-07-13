use anyhow::{Context, Result};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};

/// Child process-group id, for the signal handler.
static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_kill(_sig: libc::c_int) {
    emergency_stop()
}

/// Kill the whole child process group and exit. Called from the signal
/// handler and from the stdin-EOF watcher (host/connection died).
pub fn emergency_stop() -> ! {
    let pgid = CHILD_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    unsafe { libc::_exit(130) }
}

/// Spawn the child in its own process group and forward fatal signals to the
/// whole group — without this, grandchildren (rustc under cargo) would
/// survive as orphans. `after_registered` runs once the group is recorded, with
/// the live child in hand: the guest agent starts its stdin liveness watcher
/// there, and hands the child its stdin payload (`vm run … < script`).
pub fn spawn_and_wait(
    mut cmd: Command,
    after_registered: impl FnOnce(&mut Child),
) -> Result<ExitStatus> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let mut child = cmd.spawn().context("failed to spawn command")?;
    CHILD_PGID.store(child.id() as i32, Ordering::SeqCst);

    unsafe {
        for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            libc::signal(sig, forward_kill as *const () as libc::sighandler_t);
        }
    }
    after_registered(&mut child);

    let status = child.wait().context("failed to wait for command")?;
    CHILD_PGID.store(0, Ordering::SeqCst);
    Ok(status)
}
