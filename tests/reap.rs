//! `vm reap` — the decision matrix, driven through a fake `prlctl`.
//!
//! Reap is the one part of vm that runs unattended, every five minutes, with
//! nobody watching: launchd starts it, it decides whether to shut a guest down,
//! and the only thing it leaves behind is a line in the journal. Which is exactly
//! why it had no tests — every one of its decisions needed a real VM to make, and
//! the cost of getting one wrong is somebody's guest disappearing out from under
//! their work.
//!
//! Five things stay its hand, and each is asserted here on the only evidence that
//! matters: whether `prlctl stop` was called at all.
//!
//! The last row is the uncomfortable one. The console-input probe *fails open* —
//! if vm cannot tell whether someone is at the keyboard, it shuts the guest down
//! anyway, because reclaiming the RAM is the job and a wrongly stopped VM costs
//! one boot. That is a deliberate choice and not a safe one, so the least it can
//! do is say so out loud, and the test pins that it does.

mod fake;

use fake::{Fake, phase, running};

/// A VM nobody has touched in longer than the window, with nobody at its
/// console: the case reap exists for.
#[test]
fn an_idle_vm_is_shut_down_and_the_journal_says_why() {
    let fake = Fake::new("windows");
    // The idle probe rides `prlctl exec` on a windows guest; the reply is
    // milliseconds since the last console input — an hour, here.
    fake.scenario(&running(), &[fake.rule_exec_says("3600000")]);
    fake.last_used_minutes_ago(60);

    let run = fake.vm(&["reap", "--idle-minutes", "30"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        fake.calls_starting_with(&["stop"]).len(),
        1,
        "an idle VM is the one thing reap is for: {:?}",
        fake.calls()
    );
    assert!(
        fake.journal().contains("shut down"),
        "a sweep nobody watched has to be readable afterwards: {}",
        fake.journal()
    );
}

/// A VM that is not running is not reap's business — and `prlctl stop` on one is
/// an error, not a no-op.
#[test]
fn a_vm_that_is_already_down_is_left_alone() {
    let fake = Fake::new("windows");
    fake.guest(&[phase("stopped", "-")]);

    let run = fake.vm(&["reap", "--idle-minutes", "0"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap stopped a VM that was already stopped: {:?}",
        fake.calls()
    );
}

/// Inside the idle window, a VM is kept — the window being the whole of reap's
/// politeness.
#[test]
fn a_recently_used_vm_is_kept() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_says("3600000")]);

    // 30 minutes: the lock file was just created, so its idle time is ~0.
    let run = fake.vm(&["reap", "--idle-minutes", "30"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap took a VM that had just been used: {:?}",
        fake.calls()
    );
    assert!(run.stderr.contains("kept"), "{}", run.stderr);
}

/// Somebody is at the console. The lock files know nothing about that — a guest
/// used through the Parallels GUI leaves no trace in them — so the guest itself
/// is asked how long ago its keyboard was last touched. This is the check that
/// keeps reap from shutting a VM down while its owner is looking at it.
#[test]
fn a_vm_with_someone_at_its_console_is_kept() {
    let fake = Fake::new("windows");
    // Nobody has run a `vm` command against this guest in an hour…
    fake.scenario(&running(), &[fake.rule_exec_says("5000")]);
    fake.last_used_minutes_ago(60);

    // …but its console was touched five seconds ago. Somebody is right there.
    let run = fake.vm(&["reap", "--idle-minutes", "30"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap shut down a VM somebody was using at its console: {:?}",
        fake.calls()
    );
    assert!(
        run.stderr.contains("console input"),
        "and it has to say why it kept it: {}",
        run.stderr
    );
}

/// The fail-open rule, stated out loud. When the console probe cannot answer,
/// reap shuts the guest down anyway — reclaiming the RAM is the job, and a
/// wrongly stopped VM costs one boot. It is not a safe default, and the one
/// thing that makes it defensible is that it is never silent: the notice reaches
/// the journal even under `-q`, which is how the launchd job runs.
#[test]
fn a_console_probe_that_fails_shuts_the_vm_down_but_never_quietly() {
    let fake = Fake::new("windows");
    fake.scenario(
        &running(),
        &[fake.rule_fails("exec", "Unable to open new session")],
    );
    fake.last_used_minutes_ago(60);

    let run = fake.vm(&["-q", "reap", "--idle-minutes", "30"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        fake.calls_starting_with(&["stop"]).len(),
        1,
        "fail-open means the VM goes down: {:?}",
        fake.calls()
    );
    assert!(
        run.stderr.contains("probe failed") && run.stderr.contains("shutting down anyway"),
        "a guess this consequential must not be made silently, -q or no -q: {}",
        run.stderr
    );
    assert!(
        fake.journal().contains("probe failed"),
        "and it has to be there tomorrow, when somebody asks why: {}",
        fake.journal()
    );
}

/// A VM in use is untouchable, however idle it looks. Reap asks the kernel, not
/// a heuristic: it tries for the VM's lock, and a lock it cannot have means
/// somebody else is here. This is the race that would be worst to get wrong — a
/// sweep firing in the middle of a 40-minute `vm claude` run and taking the guest
/// out from under it.
///
/// The lock is held here on the file itself, which is what a `vm exec` holds for
/// the whole of its life (shared, where this is exclusive — reap's attempt is
/// non-blocking and fails against either, and it is reap's attempt under test).
#[test]
fn a_vm_in_use_is_never_reaped() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_says("3600000")]);

    let lock_file = fake.path().join("locks").join(fake.alias());
    std::fs::create_dir_all(lock_file.parent().unwrap()).unwrap();
    let held = vm::lock::exclusive_path(&lock_file, || {}).expect("the use lock");

    let run = fake.vm(&["reap", "--idle-minutes", "0"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap took a VM that was in use: {:?}",
        fake.calls()
    );
    assert!(
        run.stderr.contains("in use — skipped"),
        "stderr: {}",
        run.stderr
    );
    drop(held);
}
