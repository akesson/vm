//! Per-VM advisory locks: a kernel-maintained "use count".
//!
//! Every command that uses a VM (exec, sync, deploy, clean, start) holds a
//! shared flock on `~/.config/vm/locks/<alias>` for its duration; commands
//! that take the VM away (stop, with-snapshot, reap) take an exclusive one.
//! An exclusive acquire succeeds exactly when no uses are in flight, and the
//! kernel drops a holder's lock on process death — clean exit, panic, Ctrl-C
//! or SIGKILL alike — so there are no stale counts to garbage-collect.
//!
//! The lock file's mtime doubles as the VM's last-use timestamp: shared
//! holders touch it on acquire and on release (idle time counts from the end
//! of a run, not the start). Exclusive holders never touch it, so a reap
//! sweep does not reset the idle clock it is measuring.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::PathBuf;
use std::time::SystemTime;

/// A held lock; the flock is released when this is dropped.
pub struct VmLock {
    file: File,
    touch_on_drop: bool,
}

impl Drop for VmLock {
    fn drop(&mut self) {
        if self.touch_on_drop {
            let _ = self.file.set_modified(SystemTime::now());
        }
    }
}

fn lock_path(alias: &str) -> Result<PathBuf> {
    let dir = crate::config::Config::path()
        .parent()
        .map(|p| p.join("locks"))
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create lock dir {}", dir.display()))?;
    Ok(dir.join(alias))
}

fn open(alias: &str) -> Result<File> {
    let path = lock_path(alias)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))
}

/// Register a use: blocks while an exclusive holder (stop/with-snapshot/reap)
/// finishes, then coexists with any number of other uses.
pub fn shared(alias: &str) -> Result<VmLock> {
    let file = open(alias)?;
    lock_shared(&file)?;
    let _ = file.set_modified(SystemTime::now());
    Ok(VmLock {
        file,
        touch_on_drop: true,
    })
}

/// Try to take the VM away. `None` means uses are in flight.
pub fn try_exclusive(alias: &str) -> Result<Option<VmLock>> {
    let file = open(alias)?;
    if !try_lock_exclusive(&file)? {
        return Ok(None);
    }
    Ok(Some(VmLock {
        file,
        touch_on_drop: false,
    }))
}

/// When the VM was last used via `vm` (lock-file mtime), if ever.
pub fn last_use(alias: &str) -> Option<SystemTime> {
    let path = lock_path(alias).ok()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(unix)]
fn lock_shared(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(std::io::Error::last_os_error()).context("flock failed");
    }
    Ok(())
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(err).context("flock failed")
}

// The host side only runs on macOS (Parallels); on Windows this code is only
// compiled, never executed — locks degrade to no-ops.
#[cfg(not(unix))]
fn lock_shared(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> Result<bool> {
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// flock treats separately-opened fds of one process as independent
    /// holders, so shared/exclusive interaction is testable in-process.
    /// VM_CONFIG is process-global — serialize the tests that set it.
    fn with_temp_config<T>(f: impl FnOnce() -> T) -> T {
        static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: the mutex keeps concurrent set_var/remove_var away.
        unsafe { std::env::set_var("VM_CONFIG", dir.path().join("config.toml")) };
        let out = f();
        unsafe { std::env::remove_var("VM_CONFIG") };
        out
    }

    #[test]
    fn uses_coexist_and_block_exclusive() {
        with_temp_config(|| {
            let a = shared("x").unwrap();
            let b = shared("x").unwrap();
            assert!(try_exclusive("x").unwrap().is_none(), "uses in flight");
            drop(a);
            assert!(try_exclusive("x").unwrap().is_none(), "one use left");
            drop(b);
            let ex = try_exclusive("x").unwrap();
            assert!(ex.is_some(), "idle VM is claimable");
            assert!(try_exclusive("x").unwrap().is_none(), "already claimed");
        })
    }

    #[test]
    fn shared_use_touches_last_use_but_exclusive_does_not() {
        with_temp_config(|| {
            drop(shared("y").unwrap());
            let t1 = last_use("y").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(try_exclusive("y").unwrap());
            assert_eq!(last_use("y").unwrap(), t1, "reap must not reset idle");
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(shared("y").unwrap());
            assert!(last_use("y").unwrap() > t1, "a use advances the clock");
        })
    }

    #[test]
    fn different_aliases_are_independent() {
        with_temp_config(|| {
            let _a = shared("a").unwrap();
            assert!(try_exclusive("b").unwrap().is_some());
        })
    }
}
