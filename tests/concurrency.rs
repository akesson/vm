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

    // One exec alone, timed on this machine right now, is the yardstick: a fixed
    // cap flaked on a loaded CI runner where a single exec took 2.1s by itself.
    let started = Instant::now();
    let status = fake
        .command(&args)
        .spawn()
        .expect("vm runs")
        .wait()
        .expect("vm exits");
    assert!(status.success(), "the yardstick exec failed: {status:?}");
    let single = started.elapsed();

    let started = Instant::now();
    let runs: Vec<_> = (0..2)
        .map(|_| fake.command(&args).spawn().expect("vm runs"))
        .collect();
    for mut run in runs {
        let status = run.wait().expect("vm exits");
        assert!(status.success(), "a concurrent exec failed: {status:?}");
    }
    let elapsed = started.elapsed();

    // Together ≈ one exec's time; serialised = two of them. Halfway splits the
    // outcomes however slow the machine is, and never tighter than the old cap.
    let cap = (single * 3 / 2).max(Duration::from_millis(1800));
    assert!(
        elapsed < cap,
        "two 1s commands took {elapsed:?} (one alone took {single:?}) — \
         they were serialised, not run together"
    );
}

/// Five execs land on a cold guest at once — the `mise` fan-out of #40. One wins
/// the bring-up and boots the guest; the rest block on the bring-up lock for a
/// moment, then read a guest already on its way up and join the wake, rather than
/// each issuing its own `prlctl start` — which Parallels rejects, all but the
/// first, with "is not stopped" (exit 125). All five run, and the guest is
/// started exactly once.
///
/// The `start`-issued-once assertion is what guards the fix specifically: the
/// fake rejects a second start the way Parallels does, so a regression that drops
/// the bring-up lock would leave a second `start` in `calls.log` even though the
/// tolerance would still let every process exit 0.
///
/// Unix-only, like `tests/sync_race.rs`: the serialisation is an flock, and vm's
/// host half runs on macOS. In the Windows build `crate::lock` is a no-op, so
/// there would be nothing here holding the five apart.
#[cfg(unix)]
#[test]
fn concurrent_execs_against_a_cold_guest_all_run_and_start_it_once() {
    let fake = Fake::new("windows");
    fake.scenario(
        &fake::cold_boot(),
        &[fake.rule_exec_passthrough(), fake.rule_ssh("true", "")],
    );
    fake.with_repo();

    let cmd = sleeps_for(1);
    let mut args = vec!["exec", "windows", "--no-sync", "--"];
    args.extend(cmd.iter().map(String::as_str));

    let started = Instant::now();
    let runs: Vec<_> = (0..5)
        .map(|_| fake.command(&args).spawn().expect("vm runs"))
        .collect();
    for mut run in runs {
        let status = run.wait().expect("vm exits");
        assert!(
            status.success(),
            "a concurrent cold-boot exec failed: {status:?}"
        );
    }
    let elapsed = started.elapsed();

    assert_eq!(
        fake.calls_starting_with(&["start"]).len(),
        1,
        "the cold guest was started once, not once per racer: {:?}",
        fake.calls_starting_with(&["start"])
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "five 1s commands took {elapsed:?} — the bring-up lock serialised the runs \
         themselves, not just the starts"
    );
}

/// Reap, firing in the middle of a run. The lock is what tells it to keep its
/// hands off — and the failure this prevents is the worst one available: a guest
/// shut down under a forty-minute `vm claude`, taking the work with it.
///
/// The exec here is a real one, holding its real lock, for as long as its guest
/// command runs.
///
/// Unix-only, like `tests/sync_race.rs`: the lock is an flock, and vm's host half
/// runs on macOS. In the Windows build — the guest agent — `crate::lock` is a
/// no-op by construction, so there would be nothing here to test.
#[cfg(unix)]
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

/// `vm clean` deletes the guest checkout, so it takes the VM *exclusively* — a
/// shared lock (which every exec holds) would let it `rm -rf` the directory a
/// concurrent run is executing in. Held here by a real exec, clean refuses
/// rather than delete out from under it, and never issues the removal.
///
/// Unix-only, like the reap race above: the lock is an flock, and vm's host half
/// runs on macOS.
#[cfg(unix)]
#[test]
fn clean_refuses_while_an_exec_is_using_the_guest() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_passthrough()]);
    fake.with_repo();

    let cmd = sleeps_for(2);
    let mut args = vec!["exec", "windows", "--no-sync", "--"];
    args.extend(cmd.iter().map(String::as_str));
    let mut exec = fake.command(&args).spawn().expect("vm runs");

    // Let the exec get past bring_up and take its shared lock.
    std::thread::sleep(Duration::from_millis(400));
    let clean = fake.vm(&["clean", "windows"]);

    assert_ne!(
        clean.code, 0,
        "clean ran while the guest was in use: {clean:?}"
    );
    assert!(
        clean.stderr.contains("in use by another vm process"),
        "and it has to say why: {}",
        clean.stderr
    );
    assert!(
        !fake.calls().iter().any(|c| c.join(" ").contains("rm -rf")),
        "clean deleted a checkout out from under a running exec: {:?}",
        fake.calls()
    );

    let status = exec.wait().expect("vm exits");
    assert!(
        status.success(),
        "the run clean stepped around did not survive: {status:?}"
    );
}
