//! `vm doctor` — the tool's own diagnosis, diagnosed.
//!
//! Four hundred lines of checks with no tests, which is a particular kind of
//! risk: doctor is what a user is *sent to* when something is wrong, so a doctor
//! that reports the wrong thing does not merely fail — it sends someone after a
//! problem they do not have. (It has form. A stale reap plist and a guest whose
//! agent was too old to speak the protocol both went undetected until doctor was
//! taught to look, and the auth check exists because a claude login that is fine
//! on disk 401s on use.)
//!
//! Every check it makes rides one of the two names vm reaches the world by, so
//! with both pointed at the fake, a whole healthy guest can be scripted — and
//! then broken one check at a time.

mod fake;

use fake::{Fake, running};

/// A guest with nothing wrong with it, and a doctor that says so. The green path
/// is the one worth pinning hardest: a check that has quietly stopped being able
/// to pass is indistinguishable from a machine that is simply fine.
#[test]
fn a_healthy_guest_gets_a_clean_bill() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &fake.healthy_guest());

    let run = fake.vm(&["doctor", "windows"]);

    assert_eq!(
        run.code, 0,
        "a healthy guest must have no problems: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains('✗'),
        "and no failed check at all: {}",
        run.stderr
    );
    for check in [
        "prlctl",
        "config",
        "status",
        "ssh",
        "agent",
        "git",
        "work_root",
    ] {
        assert!(
            run.stderr.contains(check),
            "doctor stopped checking {check}: {}",
            run.stderr
        );
    }
}

/// The check that pays for itself. An agent left behind by an older vm speaks an
/// older protocol, and the failure that causes is nothing like its cause — the
/// version gate exists precisely because a v3 agent would otherwise take a v4
/// request, ignore the heartbeat it did not know about, and kill a perfectly good
/// command a minute in.
#[test]
fn an_agent_too_old_to_speak_the_protocol_is_caught_and_named() {
    let fake = Fake::new("windows");
    let mut rules = vec![fake.rule_ssh("_version", r#"{"binary":"0.1.0","proto":3}"#)];
    rules.extend(fake.healthy_guest());
    fake.scenario(&running(), &rules);

    let run = fake.vm(&["doctor", "windows"]);

    assert_ne!(
        run.code, 0,
        "an agent that cannot be spoken to is a problem"
    );
    assert!(
        run.stderr.contains("✗ agent") && run.stderr.contains("proto v3"),
        "the report has to name the version it found: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("vm deploy windows"),
        "and the one command that fixes it: {}",
        run.stderr
    );
}

/// A guest that is up but will not answer ssh. Every check past this one rides
/// ssh, so doctor stops here rather than reporting six failures with one cause.
#[test]
fn a_guest_that_will_not_answer_ssh_is_reported_once_not_six_times() {
    let fake = Fake::new("windows");
    fake.scenario(
        &running(),
        &[fake.rule_ssh_fails("true", "ssh: connect to host port 22: Connection refused")],
    );

    let run = fake.vm(&["doctor", "windows"]);

    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("✗ ssh"), "{}", run.stderr);
    assert!(
        !run.stderr.contains("✗ agent") && !run.stderr.contains("✗ git"),
        "the checks that ride the dead ssh must not each report it again: {}",
        run.stderr
    );
}

/// A VM in the config that Parallels has never heard of — a renamed VM, a config
/// copied between machines. Named, rather than left to fail later as something
/// stranger.
#[test]
fn a_vm_parallels_does_not_have_is_named_as_missing() {
    let fake = Fake::new("windows");
    // A guest list that does not contain the VM the config names.
    fake.guest(&[serde_json::json!({
        "uuid": "{other}",
        "status": "running",
        "ip_configured": "192.0.2.9",
        "name": "Some Other VM",
    })]);

    let run = fake.vm(&["doctor", "windows"]);

    assert_ne!(run.code, 0);
    assert!(
        run.stderr.contains("not registered in Parallels"),
        "{}",
        run.stderr
    );
}

/// The stale snapshots a killed `--with-snapshot` run leaves behind are disk
/// nobody is watching — a VM-sized copy per abandoned run. Doctor is the one
/// thing that looks.
#[test]
fn stale_snapshots_from_killed_runs_are_reported() {
    let fake = Fake::new("windows");
    let mut rules = vec![serde_json::json!({
        "match_prefix": ["snapshot-list"],
        "responses": [{ "stdout": r#"{"{abc}": {"name": "vm-with-snapshot-1752500000"}}"# }]
    })];
    rules.extend(fake.healthy_guest());
    fake.scenario(&running(), &rules);

    let run = fake.vm(&["doctor", "windows"]);

    assert!(
        run.stderr.contains("✗ snapshots") && run.stderr.contains("vm-with-snapshot-"),
        "a stale snapshot has to be named to be deleted: {}",
        run.stderr
    );
}

/// Doctor probes a *running* guest live — ssh, agent, git, the console user — so
/// it holds the use lock across those checks, exactly as an exec does: otherwise
/// reap, firing every five minutes, could shut the guest down mid-diagnosis and
/// turn a healthy VM into a page of spurious failures. That the lock is held is
/// visible where every use is — it resets the idle clock, so a guest doctored
/// just now is one reap leaves alone, even though nothing had touched it in an
/// hour beforehand.
///
/// Unix-only, like the reap/use-lock races: the coordination is an flock, and
/// vm's host half runs on macOS.
#[cfg(unix)]
#[test]
fn doctor_of_a_running_guest_counts_as_a_use_and_stays_reaps_hand() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &fake.healthy_guest());
    // Nothing has used this guest in an hour; reap's window here is 30 minutes,
    // so without the doctor counting as a use it would be shut down.
    fake.last_used_minutes_ago(60);

    let doctor = fake.vm(&["doctor", "windows"]);
    assert_eq!(doctor.code, 0, "a healthy guest: {}", doctor.stderr);

    let reap = fake.vm(&["reap", "--idle-minutes", "30"]);

    assert_eq!(reap.code, 0, "{}", reap.stderr);
    assert!(
        fake.calls_starting_with(&["stop"]).is_empty(),
        "reap shut down a guest a doctor had just held the use lock on: {:?}",
        fake.calls()
    );
    assert!(
        reap.stderr.contains("kept"),
        "and it must count the doctor as the guest's last use: {}",
        reap.stderr
    );
}
