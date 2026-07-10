//! Regression test for GH issue #4: concurrent `vm exec <same-guest>` syncs
//! raced on shared git state (the host alternate index's `.lock` and the
//! `refs/sync/head` force-push CAS) and the loser hard-failed. The fix
//! serializes the per-(repo, peer) sync critical section with a blocking flock
//! (`sync::host::lock_sync`). This drives that exact critical section — lock,
//! snapshot+push, guest apply + verify — from many threads at once, composed
//! the same way `commands::sync_repo` composes it (which itself needs a VM).
//!
//! Unix-only: `lock_sync` is a real lock only on unix, and the host side runs
//! only on macOS. On a no-op-lock platform the mutual-exclusion assertion below
//! would (correctly) fail, so the whole file compiles away off-unix.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use vm::sync::{guest, host};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Decrements the "threads currently in the critical section" counter on drop,
/// so it stays balanced even when a sync step fails and returns early.
struct CsGuard<'a>(&'a AtomicUsize);
impl Drop for CsGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One turn through the critical section, exactly as `commands::sync_repo`
/// sequences it. Returns the error as a string so the test needs no anyhow.
fn sync_once(
    repo: &Path,
    peer: &str,
    guest_path: &str,
    in_cs: &AtomicUsize,
    max_cs: &AtomicUsize,
) -> Result<(), String> {
    let _guard = host::lock_sync(repo, peer).map_err(|e| format!("{e:#}"))?;

    // Count concurrent occupants; the lock must keep this at exactly 1.
    let live = in_cs.fetch_add(1, Ordering::SeqCst) + 1;
    let _cs = CsGuard(in_cs); // decremented before _guard releases the lock
    max_cs.fetch_max(live, Ordering::SeqCst);
    // A small window so a serialization bug would let another thread observe
    // the overlap via its own fetch_max before this one leaves the section.
    std::thread::sleep(Duration::from_millis(2));

    let snap = host::sync_to(repo, peer, guest_path, None).map_err(|e| format!("{e:#}"))?;
    let tree = guest::apply(guest_path, &snap.commit).map_err(|e| format!("{e:#}"))?;
    if tree != snap.tree {
        return Err(format!(
            "tree {} did not round-trip (guest reported {tree})",
            snap.tree
        ));
    }
    Ok(())
}

#[test]
fn concurrent_syncs_to_one_guest_serialize_and_all_succeed() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("host-repo");
    let guest_dir = tmp.path().join("guest-checkout");
    fs::create_dir_all(&host_dir).unwrap();

    git(&host_dir, &["init", "--quiet"]);
    git(&host_dir, &["config", "user.name", "test"]);
    git(&host_dir, &["config", "user.email", "test@local"]);
    git(&host_dir, &["config", "core.autocrlf", "false"]);
    fs::create_dir_all(host_dir.join("src")).unwrap();
    fs::write(host_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(host_dir.join(".gitignore"), "target/\n").unwrap();
    git(&host_dir, &["add", "-A"]);
    git(&host_dir, &["commit", "--quiet", "-m", "initial"]);
    // Dirty working tree — the reason the tool exists.
    fs::write(host_dir.join("src/main.rs"), "fn main() { /* dirty */ }\n").unwrap();

    let guest_path = guest_dir.to_str().unwrap().to_string();
    guest::ensure_init(&guest_path).unwrap();

    // Every thread targets the SAME peer — that is the production contract two
    // parallel `vm exec win …` calls hit. Distinct peers wouldn't collide.
    const THREADS: usize = 8;
    const ROUNDS: usize = 5;
    const PEER: &str = "win";

    let barrier = Arc::new(Barrier::new(THREADS));
    let in_cs = Arc::new(AtomicUsize::new(0));
    let max_cs = Arc::new(AtomicUsize::new(0));
    let host_dir = Arc::new(host_dir);
    let guest_path = Arc::new(guest_path);

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let barrier = Arc::clone(&barrier);
        let in_cs = Arc::clone(&in_cs);
        let max_cs = Arc::clone(&max_cs);
        let host_dir = Arc::clone(&host_dir);
        let guest_path = Arc::clone(&guest_path);
        handles.push(std::thread::spawn(move || {
            let mut errs = Vec::new();
            for round in 0..ROUNDS {
                // Diverge the tree between rounds so pushes actually MOVE
                // refs/sync/head (the CAS-with-different-old-value path, not
                // just ref creation). Only thread 0 writes, and only while
                // every thread is parked at the barrier, so no thread's
                // `git add -A` ever races the edit.
                if tid == 0 {
                    fs::write(host_dir.join("round.txt"), format!("round {round}\n")).unwrap();
                }
                barrier.wait();

                if let Err(e) = sync_once(&host_dir, PEER, &guest_path, &in_cs, &max_cs) {
                    errs.push(format!("thread {tid} round {round}: {e}"));
                }

                // Hold thread 0 back from editing round N+1 until every thread
                // has finished round N (keeps the working tree stable per round).
                barrier.wait();
            }
            errs
        }));
    }

    let mut all_errs = Vec::new();
    for h in handles {
        all_errs.extend(h.join().expect("thread panicked"));
    }

    assert!(
        all_errs.is_empty(),
        "{} of {} concurrent syncs failed:\n{}",
        all_errs.len(),
        THREADS * ROUNDS,
        all_errs.join("\n")
    );
    let peak = max_cs.load(Ordering::SeqCst);
    assert_eq!(
        peak, 1,
        "sync lock did not serialize: {peak} threads were in the critical section at once"
    );
}
