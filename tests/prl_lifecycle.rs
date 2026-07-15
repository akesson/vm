//! The VM lifecycle, driven through a fake `prlctl` (`tests/bin/fake_prlctl.rs`).
//!
//! None of this was testable until vm grew a seam at the one name it calls
//! Parallels by. `wait_for_ip` is a state machine — it polls a guest through a
//! wake, holds out for an address the guest will actually keep, gives Parallels
//! a grace window to move a VM out of a settled off-state, and gives up after a
//! timeout — and every one of those rules is there because it once cost a bug.
//! Not one of them could be exercised without a Parallels install and a VM to
//! break, so not one of them was.
//!
//! The scenarios are the wakes vm has actually met: the staged address
//! progression of a cold Windows guest, a resume and the stale status that
//! follows it, a VM that never comes up, a guest whose lease never arrives.
//! `VM_TEST_TICK_MS` shrinks vm's clock, so the 90-second timeout takes under
//! two seconds and the state machine is the same one, only faster.

mod fake;

use fake::{Fake, LEASE, cold_boot, phase, running};

/// A cold boot in full. Every stage but the last is an address the guest will
/// not keep — the ULA broke the sync push (#35), the APIPA address left ssh
/// waiting on a host that was never there — so what this pins is that vm waits
/// for the one it settles on, and takes that.
#[test]
fn a_cold_boot_holds_out_through_every_stopgap_address() {
    let fake = Fake::new("windows");
    fake.guest(&cold_boot());

    let run = fake.vm(&["doctor", "windows"]);

    assert!(
        run.stderr.contains(&format!("ready at {LEASE}")),
        "vm must come up on the address the guest settled on: {}",
        run.stderr
    );
    for stopgap in ["fe80:", "fdb2:", "169.254."] {
        assert!(
            !run.stderr.contains(&format!("ready at {stopgap}")),
            "vm came up on a stopgap address ({stopgap}): {}",
            run.stderr
        );
    }
    assert_eq!(
        fake.calls_starting_with(&["start"]).len(),
        1,
        "a stopped VM is started once: {:?}",
        fake.calls()
    );
}

/// A suspended VM is resumed — once. Parallels reports the *old* status for a
/// beat after `prlctl resume` returns, and a loop that read that as "still needs
/// resuming" would resume a VM already on its way up, every poll, for as long as
/// the wake took.
#[test]
fn a_suspended_vm_is_resumed_exactly_once() {
    let fake = Fake::new("windows");
    fake.guest(&[
        phase("suspended", "-"),
        phase("suspended", "-"), // the stale reading, after resume took effect
        phase("resuming", "-"),
        phase("running", LEASE),
    ]);

    let run = fake.vm(&["doctor", "windows"]);

    assert!(
        run.stderr.contains("is suspended — resuming it"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&format!("ready at {LEASE}")),
        "{}",
        run.stderr
    );
    assert_eq!(
        fake.calls_starting_with(&["resume"]).len(),
        1,
        "the stale status must not draw a second resume: {:?}",
        fake.calls()
    );
    assert!(
        fake.calls_starting_with(&["start"]).is_empty(),
        "a suspended VM is resumed, not started — `prlctl start` on one is an error"
    );
}

/// The bug the grace window exists for (#17): vm asked Parallels to bring a VM
/// up, nothing happened, and vm sat out the full IP timeout before blaming a
/// guest that was never running. A VM still settled-off after the grace window
/// is not a slow one — it is one that is not coming up.
#[test]
fn a_vm_that_never_leaves_its_off_state_fails_fast_and_names_it() {
    let fake = Fake::new("windows");
    // Suspended, and suspended it stays: the resume never took.
    fake.guest(&[phase("suspended", "-")]);

    let run = fake.vm(&["doctor", "windows"]);

    assert!(
        run.stderr.contains("will never report an IP"),
        "the diagnosis must be the specific one: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("vm reap") && run.stderr.contains(r#"prlctl resume "Windows 11""#),
        "and must name both the cause and the way out: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("Parallels Tools isn't running"),
        "a suspended VM must not be blamed on Parallels Tools: {}",
        run.stderr
    );
}

/// The other half of that rule: a VM that really is running and simply never
/// gets an address does get the booting-guest story — and only it does.
///
/// Driven through `vm exec`, not `vm doctor`: doctor brings a guest up only when
/// it is down, and reports a running one's missing address itself. Every command
/// that means to *use* a guest goes through `bring_up`, and that is the path the
/// timeout lives on.
#[test]
fn a_running_guest_with_no_address_times_out_on_the_booting_story() {
    let fake = Fake::new("windows");
    fake.guest(&[phase("running", "-")]);

    let run = fake.vm(&["exec", "windows", "--", "echo hi"]);

    assert_eq!(
        run.code, 125,
        "a guest that never comes up is infra: {run:?}"
    );
    assert!(
        run.stderr.contains("did not report an IP") && run.stderr.contains("Parallels Tools"),
        "stderr: {}",
        run.stderr
    );
}

/// A guest stuck on an address vm cannot reach it on says which one, instead of
/// blaming Parallels Tools for a guest whose Tools are plainly answering.
#[test]
fn a_guest_stuck_on_a_stopgap_address_is_told_which_one() {
    let fake = Fake::new("windows");
    fake.guest(&[phase("running", "169.254.96.137")]);

    let run = fake.vm(&["exec", "windows", "--", "echo hi"]);

    assert!(
        run.stderr.contains("169.254.96.137") && run.stderr.contains("IPv4"),
        "the address it is stuck on has to be in the message: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("Parallels Tools isn't running"),
        "stderr: {}",
        run.stderr
    );
}

/// A VM that is already up is not woken, not narrated and not waited on — the
/// overwhelmingly common case, and the one that has to cost nothing.
#[test]
fn a_running_vm_is_used_as_it_is() {
    let fake = Fake::new("windows");
    fake.guest(&running());

    let run = fake.vm(&["doctor", "windows"]);

    assert!(fake.calls_starting_with(&["start"]).is_empty());
    assert!(fake.calls_starting_with(&["resume"]).is_empty());
    assert!(
        !run.stderr.contains("ready at"),
        "a warm guest is not an event: {}",
        run.stderr
    );
}

/// Nothing vm builds may come near the command line that hangs `prlctl exec`
/// forever — silently, and deaf to SIGTERM, past ~3.9 KB. The guard on the way
/// in is a unit test; this is the other half, asserting on what the real code
/// paths actually put on the wire.
#[test]
fn no_prlctl_command_line_vm_builds_comes_near_the_size_that_hangs_it() {
    let fake = Fake::new("windows");
    fake.scenario(&running(), &[fake.rule_exec_ok()]);

    fake.vm(&["doctor", "windows"]);
    fake.vm(&["ls"]);
    fake.vm(&["reap", "--idle-minutes", "0"]);

    let calls = fake.calls();
    assert!(!calls.is_empty(), "the fake saw no calls at all");
    for call in &calls {
        let total: usize = call.iter().map(|a| a.len() + 1).sum();
        assert!(
            total < 3 * 1024,
            "a {total}-byte prlctl command line: {call:?}"
        );
    }
}
