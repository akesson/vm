//! The lane that runs against real Parallels guests. `mise run test-real`.
//!
//! Every other test in this tree replaces Parallels with something that behaves
//! the way vm *believes* Parallels behaves. That is the only way to test vm's own
//! logic, and it is worth a great deal — but it can never catch the thing that
//! has caused more bugs here than everything else combined: Parallels not
//! behaving the way vm believes.
//!
//! The guards in the source are all *measurements*. A command line over ~3.9 KB
//! hangs `prlctl exec` forever, silently, deaf to SIGTERM. A killed host does not
//! close the guest's end of stdin on macOS or Windows, so the guest command runs
//! on happily without it. A nonzero exit only comes back while that pipe is still
//! open. Output written in the first fraction of a second is dropped. Each of
//! those is a fact about Parallels 26.4 that vm is *built around*, and not one of
//! them can be verified without a real guest.
//!
//! So these tests exist, and they are `#[ignore]`d, and they never run in CI:
//!
//! ```console
//! $ mise run test-real            # every guest the config names
//! $ mise run test-real linux      # or just one
//! ```
//!
//! They drive real VMs — stopping them, cold-booting them, killing runs mid-flight
//! — which is why they are opt-in. `VM_REAL_TARGETS` names the aliases to use;
//! nothing runs without it.
//!
//! The companion to this file is `tests/canary.rs`, which re-measures the
//! Parallels behaviours listed above, so that a Parallels update that changes one
//! of them is discovered by a test rather than by a user.

#![cfg(unix)] // the host half of vm runs on macOS; this is the host half

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// The guests to run against — `VM_REAL_TARGETS=linux,windows,macos`. Absent, the
/// suite skips itself: these tests take real VMs up and down, and doing that to
/// somebody who only typed `cargo test` would be inexcusable.
fn targets() -> Vec<String> {
    std::env::var("VM_REAL_TARGETS")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// The repo root — every `vm exec` has to run from inside a git repo, and this
/// one is the obvious repo to hand it.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `vm …`, for real: the real prlctl, the real guest, the real network.
fn vm(args: &[&str]) -> Run {
    let out = Command::new(VM_BIN)
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("vm runs");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The guest's OS, which decides how a command has to be written for it. Read
/// from the config the same way vm reads it, so a test cannot disagree with the
/// tool about what a guest is.
fn guest_os(alias: &str) -> vm::config::GuestOs {
    vm::config::Config::load()
        .expect("a machine config")
        .get(alias)
        .unwrap_or_else(|e| panic!("{alias}: {e:#}"))
        .os
}

/// A shell script, written for whichever shell the guest has.
fn script(alias: &str, unix: &str, windows: &str) -> String {
    match guest_os(alias) {
        vm::config::GuestOs::Windows => windows.to_string(),
        _ => unix.to_string(),
    }
}

/// Run a script in a guest, as one argument — the arity rule hands a single
/// argument to the guest's own shell.
fn exec(alias: &str, unix: &str, windows: &str) -> Run {
    vm(&["exec", alias, "--", &script(alias, unix, windows)])
}

// ── the exit-code contract ───────────────────────────────────────────────────

/// A guest command's exit code is the run's exit code. Every task runner in front
/// of vm depends on it, and vm reserves 125 and 2 for its own failures precisely
/// so that a caller can tell "your command failed" from "vm hiccuped, retry".
///
/// Over a real transport, this is not free: measured on Parallels 26.4, a unix
/// guest reached over `prlctl exec` reports a nonzero exit *only while the host
/// still holds the stdin pipe open*. Close it early — as `Child::wait()` would —
/// and every failure comes back as a success.
#[test]
#[ignore = "drives a real Parallels guest"]
fn a_guest_commands_exit_code_comes_back_as_it_is() {
    for alias in targets() {
        for expected in [0, 1, 7, 42] {
            let run = exec(
                &alias,
                &format!("exit {expected}"),
                &format!("exit {expected}"),
            );
            assert_eq!(
                run.code, expected,
                "{alias}: exit {expected} came back as {}: {}",
                run.code, run.stderr
            );
        }
    }
}

/// A command the guest cannot find answers with a *not-found* code — the
/// command's own result, never vm's. It must never be 125: a task runner reads
/// that as "vm hiccuped" and would retry a typo until it gave up.
///
/// Which code depends on who answers. The script form rides the guest's own
/// shell, and each shell has its convention: `sh` says 127, `cmd.exe` says 1
/// (its convention, not a vm bug — the `--or-native` tests in exec/host.rs
/// document the same fact). The exec form, unwrapped, is the agent's own spawn,
/// and there the answer is vm's contract: 127 on every OS.
#[test]
#[ignore = "drives a real Parallels guest"]
fn a_command_the_guest_cannot_find_is_not_found_and_never_an_infra_code() {
    for alias in targets() {
        // Script form: the shell's own code.
        let expected = match guest_os(&alias) {
            vm::config::GuestOs::Windows => 1,
            _ => 127,
        };
        let run = vm(&["exec", &alias, "--", "vm-test-definitely-not-an-executable"]);
        assert_eq!(
            run.code, expected,
            "{alias}: the shell answers a missing command with {expected}, not {}: {}",
            run.code, run.stderr
        );

        // Exec form, unwrapped: the agent's spawn, 127 everywhere. Unwrapped
        // because mise answers a missing binary with its own 1, which would
        // test mise's convention rather than the agent's contract.
        let run = vm(&[
            "exec",
            &alias,
            "--guest-env",
            "none",
            "--",
            "vm-test-definitely-not-an-executable",
            "with-an-argument",
        ]);
        assert_eq!(
            run.code, 127,
            "{alias}: the agent answers a missing command with 127, not {}: {}",
            run.code, run.stderr
        );
    }
}

// ── the liveness contract (#21) ──────────────────────────────────────────────

/// The bug that the heartbeat exists for, in the guest it actually happened in.
///
/// Kill the host `vm` mid-run and the guest command must die with it. Over ssh
/// that is free — the pipe closes, the agent sees EOF. Over `prlctl exec` it is
/// not: Parallels Tools can hold the guest's end of stdin open after the host is
/// gone, so no EOF ever arrives, and a killed `vm run --elevated macos` used to
/// leave its command running for as long as anyone cared to watch. The silence
/// timeout is the only thing that ends it, and only a real guest can prove it does.
#[test]
#[ignore = "drives a real Parallels guest"]
fn killing_the_host_kills_the_guest_command_it_left_behind() {
    for alias in targets() {
        let marker = format!("vm-orphan-{}", std::process::id());
        // A command that will outlive its host unless something stops it, and
        // that can be found again from outside by the name it runs under. The
        // marker must be a *command* (`: marker`), not a comment: a comment
        // would be discarded when sh exec-replaces itself with the sleep
        // (measured on the macOS guest), and no process would carry the marker
        // — which is how this test once passed vacuously there, its probe
        // seeing nothing before the kill as after. The compound form keeps sh
        // resident with the whole script — marker included — in its argv.
        let unix = format!("sleep 600; : {marker}");
        let windows = format!("ping -n 600 127.0.0.1 >NUL & rem {marker}");

        let mut child = Command::new(VM_BIN)
            .args(["exec", &alias, "--", &script(&alias, &unix, &windows)])
            .current_dir(repo_root())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("vm runs");

        // Long enough to be past the sync and actually running in the guest.
        std::thread::sleep(Duration::from_secs(20));
        // The probe must see the command *before* the kill, or "gone" below
        // would be vacuous — this probe once spent its life broken (its quotes
        // were mangled on the way to cmd.exe) and reported "still running"
        // forever; a positive control is what catches the next breakage.
        assert!(
            command_is_running(&alias, &marker),
            "{alias}: the probe cannot see the command it is about to orphan — \
             the probe is broken, not the contract"
        );
        child.kill().expect("the host dies");
        child.wait().ok();

        // The agent gets one silence budget to notice, plus room for a loaded
        // guest. Ask the guest itself whether the command is still there.
        let deadline = Instant::now() + vm::proto::HEARTBEAT_TIMEOUT + Duration::from_secs(30);
        let gone = loop {
            if !command_is_running(&alias, &marker) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        assert!(
            gone,
            "{alias}: the guest command outlived the host that started it — \
             an orphan, exactly as in #21"
        );
    }
}

/// Whether the orphan-test command is still running in the guest. Asked with
/// a *fresh* `vm exec`, which is the only honest way: the guest is the authority
/// on what the guest is doing.
///
/// On unix the probe greps for `marker` and excludes itself. On Windows it
/// looks for the ping the command runs as, by image name: this test is the only
/// thing that pings in the guest, the probe (findstr) never matches itself, and
/// nothing here needs a quote — the previous probe (`find /c "marker"`) lost
/// its quotes on the way into cmd.exe and answered "still running" forever.
/// Case-insensitive because tasklist prints `PING.EXE`; findstr exits 1 on no
/// match, so the verdict is read from stdout, not the exit code.
fn command_is_running(alias: &str, marker: &str) -> bool {
    match guest_os(alias) {
        vm::config::GuestOs::Windows => {
            let run = exec(alias, "", "tasklist | findstr /i ping");
            !run.stdout.trim().is_empty()
        }
        _ => {
            let run = exec(
                alias,
                &format!("ps ax | grep -v grep | grep -c '{marker}' || true"),
                "",
            );
            run.stdout
                .trim()
                .lines()
                .next()
                .and_then(|n| n.trim().parse::<u32>().ok())
                != Some(0)
        }
    }
}

// ── the wake ─────────────────────────────────────────────────────────────────

/// A cold guest, from stopped to running a command — the path that broke twice in
/// one week (#35 and the APIPA address it hid behind). What is asserted is not
/// just that it works, but *which address* it worked on: a guest advertises
/// several before it settles, and vm has taken the wrong one more than once.
#[test]
#[ignore = "drives a real Parallels guest (stops it first)"]
fn a_cold_guest_comes_up_on_the_address_it_settles_on() {
    for alias in targets() {
        // Take it down first: a cold boot is the case that has the bugs in it.
        let down = vm(&["reap", &alias, "--idle-minutes", "0"]);
        assert_eq!(
            down.code, 0,
            "{alias}: could not shut the guest down: {}",
            down.stderr
        );

        let run = exec(alias.as_str(), "echo cold-boot-ok", "echo cold-boot-ok");

        assert_eq!(
            run.code, 0,
            "{alias}: a cold guest must just work: {}",
            run.stderr
        );
        assert!(
            run.stdout.contains("cold-boot-ok"),
            "{alias}: stdout: {}",
            run.stdout
        );
        // The stopgaps, each of which vm has at some point mistaken for an
        // address: a link-local IPv6, the Parallels ULA, an APIPA IPv4.
        for stopgap in ["ready at fe80:", "ready at fd", "ready at 169.254."] {
            assert!(
                !run.stderr.contains(stopgap),
                "{alias}: came up on a stopgap address ({stopgap}): {}",
                run.stderr
            );
        }
    }
}

// ── stdio ────────────────────────────────────────────────────────────────────

/// Output written the instant a command starts, twenty times over. Measured on
/// Parallels 26.4, the elevated channel *drops* what a guest writes in its first
/// fraction of a second — which the agent protocol happens to shield vm from,
/// because the request round-trip takes longer than the window. That shielding is
/// accidental, so it is worth knowing if it ever stops working.
#[test]
#[ignore = "drives a real Parallels guest"]
fn output_written_the_instant_a_command_starts_is_never_dropped() {
    for alias in targets() {
        for i in 0..20 {
            let run = exec(&alias, "echo instant", "echo instant");
            assert!(
                run.stdout.contains("instant"),
                "{alias}: run {i} lost the command's output entirely: {:?}",
                run.stdout
            );
        }
    }
}

/// A megabyte of stdin into a command that never reads a byte of it. The payload
/// is written on a thread for exactly this reason: inline, it would block on a
/// full pipe buffer forever, and the run would hang rather than fail.
#[test]
#[ignore = "drives a real Parallels guest"]
fn a_big_stdin_payload_into_a_command_that_ignores_it_does_not_hang() {
    for alias in targets() {
        let payload = "x".repeat(1024 * 1024);
        let mut child = Command::new(VM_BIN)
            .args(["run", &alias, "--", &script(&alias, "exit 7", "exit 7")])
            .current_dir(repo_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("vm runs");
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
        let status = child.wait().expect("vm exits");
        assert_eq!(
            status.code(),
            Some(7),
            "{alias}: the command's own exit code, not a hang"
        );
    }
}

/// Hostile bytes, through the guest's shell and back, unchanged. `prlctl exec` on
/// a linux guest re-joins its argv and lets a shell re-split it, so quoting *is*
/// lost on that channel — which is why the agent never uses it for argv, and why
/// this has to be checked against the real thing.
#[test]
#[ignore = "drives a real Parallels guest"]
fn hostile_arguments_reach_the_guest_byte_for_byte() {
    let hostile = "a b|c&&d$(echo pwned)e'f\"g";
    for alias in targets() {
        // Exec form (several arguments): spawned as given, never through a shell.
        let run = vm(&["exec", &alias, "--", "printf", "%s", hostile]);
        if guest_os(&alias) == vm::config::GuestOs::Windows {
            continue; // no printf; the console channel gets its own coverage
        }
        assert_eq!(
            run.stdout, hostile,
            "{alias}: an argument was mangled on the way to the guest: {}",
            run.stderr
        );
    }
}

/// A double quote, through the guest's shell and back. Spawning
/// `["cmd", "/C", script]` with std's `.args()` backslash-escapes embedded `"`
/// — CRT quoting, which cmd.exe does not parse — so before the agent stepped
/// around it (`raw_arg`, see `exec::command_for`), every quoted Windows script
/// arrived mangled: `echo "QQ"` printed `\"QQ\"`, and this is the test that
/// proves it stays fixed against the real guest.
///
/// `--guest-env none` is load-bearing: mise (2026.7.5) spawns cmd with the
/// same std quoting, so through the wrap the quotes stay lost until mise fixes
/// it upstream — that failure would be mise's, not the agent's, and this test
/// pins the agent.
#[test]
#[ignore = "drives a real Parallels guest"]
fn a_double_quote_in_a_script_survives_the_trip_to_the_guest_shell() {
    for alias in targets() {
        let run = vm(&["exec", &alias, "--guest-env", "none", "--", r#"echo "QQ""#]);
        assert_eq!(run.code, 0, "{alias}: {}", run.stderr);
        // sh consumes the quotes (`QQ`); cmd echoes them back (`"QQ"`). Either
        // way no backslash was ever sent — one in the output *is* the mangling.
        assert!(
            run.stdout.contains("QQ") && !run.stdout.contains('\\'),
            "{alias}: the quote was mangled on the way to the shell: {:?}",
            run.stdout
        );
    }
}

// ── the guest's own health ───────────────────────────────────────────────────

/// `vm doctor` is what a user is sent to when something is wrong. On a guest with
/// nothing wrong with it, every guest check has to pass — a check that has quietly
/// stopped being able to pass is worse than no check at all.
///
/// Scoped to the guest's own section, not doctor's exit code. The host section
/// above it reports on *this machine* — whether an ssh key exists, whether the
/// reap timer is the current one — and those are true findings about the machine
/// rather than statements about the guest. A test that demanded them green would
/// be a test that failed on a developer's laptop for being a developer's laptop.
#[test]
#[ignore = "drives a real Parallels guest"]
fn doctor_is_green_on_a_healthy_guest() {
    for alias in targets() {
        let run = vm(&["doctor", &alias]);

        // The guest's section runs from its header (`linux (Ubuntu 24.04)`) to
        // the end of the report.
        let header = run
            .stderr
            .find(&format!("{alias} ("))
            .unwrap_or_else(|| panic!("{alias}: doctor never reached the guest:\n{}", run.stderr));
        let guest_section = &run.stderr[header..];

        assert!(
            !guest_section.contains('✗'),
            "{alias}: doctor found problems on a guest that should be healthy:\n{guest_section}"
        );
        for check in ["status", "ssh", "agent", "git", "work_root"] {
            assert!(
                guest_section.contains(check),
                "{alias}: doctor stopped checking {check}:\n{guest_section}"
            );
        }
    }
}

/// The shutdown stall (#32): a graceful stop of the linux guest used to take
/// 92–99 seconds, because `prldnd` — the Parallels drag-and-drop agent — ignores
/// SIGTERM and systemd waited out its full 90-second timeout before killing it.
/// Parallels then force-kills at 120s, so every reap was seconds away from being
/// a yanked power cord. The unit `vm deploy` installs kills prldnd as shutdown
/// opens; this is the budget it bought.
#[test]
#[ignore = "drives a real Parallels guest (stops it)"]
fn a_linux_guest_shuts_down_in_seconds_not_a_minute_and_a_half() {
    for alias in targets() {
        if guest_os(&alias) != vm::config::GuestOs::Linux {
            continue;
        }
        // Make sure it is up, so the stop has something to do.
        let up = exec(&alias, "true", "true");
        assert_eq!(up.code, 0, "{alias}: {}", up.stderr);

        let started = Instant::now();
        let run = vm(&["reap", &alias, "--idle-minutes", "0"]);
        let took = started.elapsed();

        assert_eq!(run.code, 0, "{alias}: {}", run.stderr);
        assert!(
            took < Duration::from_secs(30),
            "{alias}: the guest took {took:?} to shut down — the prldnd unit is not \
             doing its job, and Parallels force-kills at 120s (`vm doctor {alias}`)"
        );
    }
}
