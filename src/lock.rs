//! Per-VM advisory locks: a kernel-maintained "use count".
//!
//! Every command that uses a VM (exec, run, sync, deploy, clean, shot) holds a
//! shared flock on `~/.config/vm/locks/<alias>` for its duration; commands
//! that take the VM away (exec --with-snapshot, reap) take an exclusive one.
//! An exclusive acquire succeeds exactly when no uses are in flight, and the
//! kernel drops a holder's lock on process death — clean exit, panic, Ctrl-C
//! or SIGKILL alike — so there are no stale counts to garbage-collect.
//!
//! The lock file's mtime doubles as the VM's last-use timestamp: shared
//! holders touch it on acquire and on release (idle time counts from the end
//! of a run, not the start). Exclusive holders never touch it, so a reap
//! sweep does not reset the idle clock it is measuring.
//!
//! Two more locks live here, both built on a blocking exclusive flock over an
//! arbitrary file ([`exclusive_path`]) rather than the use count above:
//!
//! - [`bringup`] serialises the *start/resume* of one VM — held only across the
//!   bring-up decision, so concurrent uses (which all hold the shared lock, and
//!   so run in parallel) cannot all read a cold guest as `stopped` and all issue
//!   `prlctl start`, every loser but one getting Parallels' "is not stopped"
//!   (#40).
//! - [`crate::sync::host::lock_sync`] serialises the per-(repo, guest) sync
//!   critical section — which the shared use lock deliberately does *not* do
//!   (parallel execs on one VM must stay parallel; only their sync phase needs a
//!   single writer).
//!
//! A command that takes all three takes them in the order use → bringup → sync,
//! and the bringup lock is released before the sync lock is taken, so there is
//! no cycle.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
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

fn locks_dir() -> Result<PathBuf> {
    let dir = crate::config::Config::path()
        .parent()
        .map(|p| p.join("locks"))
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create lock dir {}", dir.display()))?;
    Ok(dir)
}

fn lock_path(alias: &str) -> Result<PathBuf> {
    Ok(locks_dir()?.join(alias))
}

fn open_path(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("cannot open lock file {}", path.display()))
}

fn open(alias: &str) -> Result<File> {
    open_path(&lock_path(alias)?)
}

/// Register a use: blocks while an exclusive holder (stop/--with-snapshot/reap)
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

/// A held exclusive flock on an arbitrary path; released when dropped
/// (closing the file descriptor drops the flock). Unlike [`VmLock`] it never
/// touches the file's mtime.
///
/// The lock file is intentionally *never* deleted: flock is keyed on the open
/// file's inode, so unlinking a held lock file and recreating it would let a
/// second holder acquire a lock on the new inode while the first still holds
/// the old one. Leave the file on disk forever, like `locks/<alias>`.
#[must_use = "the lock is released as soon as the PathLock is dropped"]
pub struct PathLock {
    _file: File,
}

/// Take a blocking exclusive flock on `path` (created if absent). Returns once
/// this process holds it exclusively. If another holder is in the way,
/// `on_wait` fires once — before blocking — so the caller can explain the
/// pause; it does not fire when the lock is free.
pub fn exclusive_path(path: &Path, on_wait: impl FnOnce()) -> Result<PathLock> {
    let file = open_path(path)?;
    if !try_lock_exclusive(&file)? {
        on_wait();
        lock_exclusive_blocking(&file)?;
    }
    Ok(PathLock { _file: file })
}

/// Serialise the bring-up of one VM: at most one `vm` process asks Parallels to
/// start or resume a given alias at a time (#40).
///
/// The use lock ([`shared`]) is deliberately shared, so concurrent execs run in
/// parallel — which also means nothing stops them all from reading a cold guest
/// as `stopped` and all issuing `prlctl start`, every loser but one getting
/// Parallels' "is not stopped". This is what does. It is held only across the
/// bring-up *decision* — the status read and the one start/resume — never across
/// the IP wait, so a loser blocks for a moment, not a whole boot.
///
/// Taken after the per-VM use lock and released before the sync lock, keeping
/// the order use → bringup → sync with no cycle. A separate file from
/// `locks/<alias>` so it does not contend with the shared use lock, and a
/// formatted name rather than `Path::with_extension` so a dotted alias is not
/// mangled (a VM literally named `foo.bringup` would then collide with `foo`'s —
/// contrived enough to leave).
pub fn bringup(alias: &str, on_wait: impl FnOnce()) -> Result<PathLock> {
    exclusive_path(&locks_dir()?.join(format!("{alias}.bringup")), on_wait)
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

#[cfg(unix)]
fn lock_exclusive_blocking(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // A blocking flock can be interrupted by a signal (EINTR) on macOS — retry
    // rather than surfacing a spurious failure.
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err).context("flock failed");
    }
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

#[cfg(not(unix))]
fn lock_exclusive_blocking(_file: &File) -> Result<()> {
    Ok(())
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

    /// A second `exclusive_path` on the same file must block until the first is
    /// dropped. Made deterministic (no timing sleeps) by having the waiter's
    /// `on_wait` signal a channel: the holder does not release until it has
    /// seen that signal, which only fires when the waiter observed contention.
    #[test]
    fn exclusive_path_serializes_holders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.flock");

        let held = exclusive_path(&path, || panic!("should not wait: lock is free")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let path2 = path.clone();
        let waiter = std::thread::spawn(move || {
            // Blocks inside until `held` is dropped; on_wait fires first.
            let _w = exclusive_path(&path2, || tx.send(()).unwrap()).unwrap();
        });

        // The waiter reached contention (its on_wait fired) — so it is now
        // blocked. Only then release, and it must be able to proceed.
        rx.recv().expect("waiter reported contention");
        drop(held);
        waiter.join().expect("waiter acquired after release");
    }
}
