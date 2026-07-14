//! Two `vm` processes on one guest — the normal case, not the exceptional one.
//!
//! A `mise` fan-out runs `vm exec <guest> …` twice at once and thinks nothing of
//! it, and meanwhile a launchd timer is firing every five minutes with an opinion
//! about whether that guest is in use. The locking that holds all of this together
//! is per-VM, shared for uses and exclusive for stop/reap, and it has been the
//! source of the two worst bugs this tool has had: a sync race that failed both
//! runs (#4), and a reap that could take a guest out from under one.
//!
//! These drive the real `vm` binary, twice, concurrently, with the guest command
//! run for real through the fake's passthrough — so what is asserted is what the
//! processes actually did to each other.

mod fake;

use fake::{Fake, running, sleeps_for};
use std::time::{Duration, Instant};

/// Two execs against one guest run at the same time. The use lock is *shared*
/// precisely so they can: serialising them would turn a two-minute fan-out into a
/// four-minute one, and the lock is there to keep reap out, not other work.
#[test]
fn two_execs_on_one_guest_run_at_the_same_time() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_passthrough()]);
    fake.with_repo();

    let cmd = sleeps_for(1);
    let mut args = vec!["exec", "windows", "--no-sync", "--"];
    args.extend(cmd.iter().map(String::as_str));

    let started = Instant::now();
    let runs: Vec<_> = (0..2)
        .map(|_| fake.command(&args).spawn().expect("vm runs"))
        .collect();
    for mut run in runs {
        let status = run.wait().expect("vm exits");
        assert!(status.success(), "a concurrent exec failed: {status:?}");
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1800),
        "two 1s commands took {elapsed:?} — they were serialised, not run together"
    );
}

/// Reap, firing in the middle of a run. The lock is what tells it to keep its
/// hands off — and the failure this prevents is the worst one available: a guest
/// shut down under a forty-minute `vm claude`, taking the work with it.
///
/// The exec here is a real one, holding its real lock, for as long as its guest
/// command runs.
#[test]
fn reap_cannot_take_a_guest_out_from_under_a_running_exec() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_passthrough()]);
    fake.with_repo();
    // Nobody has *started* a vm command in an hour, and nobody is at the console
    // either: by every measure reap has, this guest is idle. Only the lock held by
    // the run in flight says otherwise.
    fake.last_used_minutes_ago(60);

    let cmd = sleeps_for(2);
    let mut args = vec!["exec", "windows", "--no-sync", "--"];
    args.extend(cmd.iter().map(String::as_str));
    let mut exec = fake.command(&args).spawn().expect("vm runs");

    // Let it get past bring_up and take its lock.
    std::thread::sleep(Duration::from_millis(400));
    let reap = fake.vm(&["reap", "--idle-minutes", "30"]);

    assert_eq!(reap.code, 0, "{}", reap.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap shut down a guest with a command running in it: {:?}",
        fake.calls()
    );
    assert!(
        reap.stderr.contains("in use — skipped"),
        "and it has to say that is why: {}",
        reap.stderr
    );

    let status = exec.wait().expect("vm exits");
    assert!(
        status.success(),
        "the run reap spared did not survive anyway: {status:?}"
    );
}
