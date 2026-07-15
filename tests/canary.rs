//! The quirk canary: re-measure what vm is built on. `mise run quirk-canary`.
//!
//! vm's hardest code is not defensive against bugs of its own. It is defensive
//! against *Parallels*, and every one of those defences encodes a number or a
//! string that somebody measured on a particular version, on a particular day:
//!
//! - `prlctl exec` hangs forever past a ~3.9 KB command line — silently, and deaf
//!   to SIGTERM. vm caps its own at 3 KB and refuses to build a longer one.
//! - A refused Tools session says exactly `Unable to open new session`, and vm
//!   waits that out rather than failing. It matches on the *string*.
//! - A VM's status is one of eight words. An unrecognised one makes `wait_for_ip`
//!   wait forever, because nothing else can be assumed about it.
//! - `prlctl list`, `list -i` and `snapshot-list` answer in shapes vm parses.
//!
//! Every one of those is a fact about Parallels 26.4 (2026-07), and Parallels
//! updates itself. When one of them changes, nothing in vm breaks loudly: the cap
//! silently stops protecting anything, a wake silently stops being waited out, a
//! status word silently becomes an infinite wait. The failure surfaces weeks later
//! as "vm hung, sometimes", which is the exact shape of bug this whole exercise
//! exists to stop having.
//!
//! So the canary asks Parallels the questions again, and fails when an answer has
//! changed. It is not a test of vm — it is a test of the ground vm stands on. Run
//! it before and after a Parallels upgrade (`/vm-upgrade` does), and believe the
//! diff.
//!
//! ```console
//! $ mise run quirk-canary          # every guest the config names
//! ```
//!
//! Every probe here talks to Parallels *directly*, deliberately going around vm:
//! a probe that used vm's own guards could never see the guard's ground shift.

#![cfg(unix)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The guests to ask — `VM_REAL_TARGETS=linux,windows,macos`. Absent, nothing runs.
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

/// The Parallels name of a configured alias, and the guest it is.
fn guest(alias: &str) -> (String, vm::config::GuestOs) {
    let cfg = vm::config::Config::load().expect("a machine config");
    let vm = cfg.get(alias).unwrap_or_else(|e| panic!("{alias}: {e:#}"));
    (vm.parallels_name.clone(), vm.os)
}

/// Note a measurement — to the terminal, and to vm's journal.
///
/// The journal is the point. One canary run is a set of numbers; the *series* is
/// what tells you that the cliff moved in June, and moved because of the upgrade
/// you did in June. A measurement nobody kept is a measurement that has to be
/// taken again from scratch the next time something hangs.
fn note(line: &str) {
    static ARM: std::sync::Once = std::sync::Once::new();
    ARM.call_once(|| vm::journal::init(false));
    vm::notice!("vm ▸ canary ▸ {line}");
}

/// Run `prlctl` and give back (code, stdout, stderr). The real one — the whole
/// point is to ask Parallels, not vm's idea of it.
fn prlctl(args: &[&str]) -> (i32, String, String) {
    let out = Command::new("prlctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("prlctl runs (is Parallels installed?)");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The version everything here is measured against. In the report so that a drift
/// can be pinned to the upgrade that caused it.
#[test]
#[ignore = "asks a real Parallels"]
fn record_the_version_everything_here_is_measured_against() {
    let (_, out, _) = prlctl(&["--version"]);
    note(&format!("prlctl {}", out.trim()));
}

/// **The cap.** `prlctl exec` hangs forever past roughly 3.9 KB of command line —
/// no output, no error, no guest-side process, and it ignores SIGTERM, so only a
/// `kill -9` ends it. vm caps its own command lines at 3 KB on the strength of
/// that measurement.
///
/// This walks up to the cliff with a watchdog. What it protects is the *margin*:
/// if the cliff has moved down towards vm's 3 KB cap, the cap has quietly stopped
/// protecting anything, and a long `vm run --elevated` will hang instead of
/// failing. If it has moved up, or gone, that is worth knowing too — but it is not
/// an emergency, so it is only noted.
#[test]
#[ignore = "asks a real Parallels, and leaves a killed prlctl behind by design"]
fn the_command_line_that_hangs_prlctl_is_still_far_above_vms_cap() {
    let cap = 3 * 1024;
    for alias in targets() {
        let (name, os) = guest(&alias);
        if os == vm::config::GuestOs::Macos {
            // A hung session on the macOS guest poisons the *next* elevated run
            // for a minute or two (#27, a Parallels bug vm cannot fix). Not worth
            // it: the cliff is a property of prlctl, and the other guests measure
            // it just as well.
            continue;
        }

        let mut largest_ok = 0;
        let mut smallest_hang = usize::MAX;
        for size in [3_200, 3_600, 3_900, 4_400] {
            match echo_of_size(&name, size) {
                Some(_) => largest_ok = largest_ok.max(size),
                None => {
                    smallest_hang = smallest_hang.min(size);
                    break; // past the cliff; no need to go further out
                }
            }
        }
        note(&format!(
            "{alias}: prlctl exec answers at {largest_ok}B, hangs at {}B (vm caps at {cap}B)",
            if smallest_hang == usize::MAX {
                "no size tried".to_string()
            } else {
                smallest_hang.to_string()
            }
        ));

        assert!(
            largest_ok > cap + 256,
            "{alias}: prlctl exec now hangs at {smallest_hang}B, which is at or below vm's \
             {cap}B cap — the cap has stopped protecting anything and `vm run` will hang \
             instead of failing. Lower EXEC_ARGV_LIMIT in src/prl.rs."
        );
    }
}

/// Run an `echo` whose command line totals about `bytes`, under a watchdog. `Some`
/// if it answered, `None` if it hung — the hang being the quirk, and SIGKILL the
/// only thing that ends it.
fn echo_of_size(name: &str, bytes: usize) -> Option<String> {
    let filler = "x".repeat(bytes.saturating_sub(32));
    let mut child = Command::new("prlctl")
        .args(["exec", name, &format!("echo {filler}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("prlctl runs");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait") {
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut out);
            }
            return Some(out);
        }
        if Instant::now() >= deadline {
            // SIGTERM is ignored by the hang — this is the quirk, not a courtesy.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// **The string.** Parallels Tools refuses an exec session for about ten seconds
/// after a guest wakes, and vm waits that out rather than failing the run — by
/// matching the message on the substring `Unable to open new session`. Change the
/// wording and vm stops recognising a wake it should wait out, and starts failing
/// the first `vm run --elevated` after every resume.
#[test]
#[ignore = "asks a real Parallels (stops and starts a guest)"]
fn a_refused_tools_session_still_says_what_vm_listens_for() {
    for alias in targets() {
        let (name, _) = guest(&alias);
        prlctl(&["stop", &name]);
        prlctl(&["start", &name]);

        // Hammer the session the moment the VM is up: the refusal is what we came
        // for, and it only exists in the first seconds of a wake.
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut refusals = Vec::new();
        let mut answered = false;
        while Instant::now() < deadline {
            let (code, _, stderr) = prlctl(&["exec", &name, "true"]);
            if code == 0 {
                answered = true;
                break;
            }
            if !stderr.trim().is_empty() {
                refusals.push(stderr.trim().to_string());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(
            answered,
            "{alias}: the guest never accepted an exec session"
        );

        // Every refusal seen on the way up has to be one vm knows to wait out.
        // A new wording is the whole point of this probe.
        for refusal in &refusals {
            assert!(
                vm::prl::is_session_not_ready(refusal),
                "{alias}: Parallels refused a session with a message vm does not \
                 recognise, so vm would fail the run instead of waiting the wake out:\n  \
                 {refusal}\n  \
                 Teach `prl::is_session_not_ready` the new wording."
            );
        }
        note(&format!(
            "{alias}: {} session refusal(s) on the way up, all recognised",
            refusals.len()
        ));
    }
}

/// **The vocabulary.** vm's wait treats a VM's status as one of eight words: three
/// it will never leave on its own (stopped, suspended, paused) and the transients
/// Parallels reports while it works. Anything else is left to the timeout — which
/// is to say, an unrecognised status makes every wake a 90-second wait ending in
/// the wrong error.
#[test]
#[ignore = "asks a real Parallels (stops and starts a guest)"]
fn parallels_still_reports_a_status_vm_knows_the_meaning_of() {
    const KNOWN: &[&str] = &[
        "running",
        "stopped",
        "suspended",
        "paused",
        "starting",
        "resuming",
        "stopping",
        "suspending",
    ];
    for alias in targets() {
        let (name, _) = guest(&alias);
        let mut seen: Vec<String> = Vec::new();

        // A full cycle, sampled fast enough to catch the transients.
        prlctl(&["stop", &name]);
        let sample = |seen: &mut Vec<String>| {
            let (_, out, _) = prlctl(&["list", "-a", "-f", "--json"]);
            if let Ok(vms) = serde_json::from_str::<Vec<serde_json::Value>>(&out)
                && let Some(status) = vms
                    .iter()
                    .find(|v| v["name"] == name.as_str())
                    .and_then(|v| v["status"].as_str())
                && !seen.iter().any(|s| s == status)
            {
                seen.push(status.to_string());
            }
        };
        for _ in 0..10 {
            sample(&mut seen);
            std::thread::sleep(Duration::from_millis(300));
        }
        prlctl(&["start", &name]);
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            sample(&mut seen);
            std::thread::sleep(Duration::from_millis(300));
        }

        note(&format!("{alias}: statuses seen — {}", seen.join(", ")));
        for status in &seen {
            assert!(
                KNOWN.contains(&status.as_str()),
                "{alias}: Parallels reports a status vm has never heard of: '{status}'. \
                 An unknown status is not an error to vm — it is a wait that never ends. \
                 Teach `prl::is_off` and its comment what this word means."
            );
        }
    }
}

/// **The shapes.** Everything vm knows about a VM it learns by parsing prlctl's
/// JSON. A field renamed in an update is not a parse error somewhere quiet — it is
/// every command failing at once, at the first thing any of them do.
#[test]
#[ignore = "asks a real Parallels"]
fn the_json_parallels_answers_in_is_still_the_json_vm_parses() {
    let (_, out, _) = prlctl(&["list", "-a", "-f", "--json"]);
    let vms: Vec<vm::prl::PrlVm> = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("`prlctl list --json` no longer parses: {e}\n{out}"));
    assert!(!vms.is_empty(), "no VMs registered at all?");
    note(&format!("prlctl list --json: {} VMs parsed", vms.len()));

    for alias in targets() {
        let (name, _) = guest(&alias);
        // `list -i` (memory, home) and the snapshot listing, both parsed by vm.
        vm::prl::details(&name)
            .unwrap_or_else(|e| panic!("{alias}: `prlctl list -i --json` no longer parses: {e:#}"));
        vm::prl::snapshot_list(&name).unwrap_or_else(|e| {
            panic!("{alias}: `prlctl snapshot-list --json` no longer parses: {e:#}")
        });
    }
}

/// **The staged wake.** A guest hands out addresses it will not keep before it
/// hands out the one it will — a link-local IPv6, the Parallels ULA, an APIPA
/// IPv4 — and vm has taken the wrong one twice. This records the ladder as it
/// stands today, and fails if the address a guest *settles* on is one vm would
/// refuse to use.
#[test]
#[ignore = "asks a real Parallels (stops and starts a guest)"]
fn the_address_a_guest_settles_on_is_still_one_vm_will_use() {
    for alias in targets() {
        let (name, _) = guest(&alias);
        prlctl(&["stop", &name]);
        prlctl(&["start", &name]);

        let mut ladder: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut settled = None;
        while Instant::now() < deadline {
            let (_, out, _) = prlctl(&["list", "-a", "-f", "--json"]);
            if let Ok(vms) = serde_json::from_str::<Vec<vm::prl::PrlVm>>(&out)
                && let Some(prl_vm) = vms.iter().find(|v| v.name == name)
            {
                let reported = prl_vm.ip_configured.clone();
                if !ladder.contains(&reported) {
                    ladder.push(reported);
                }
                if let Some(ip) = prl_vm.ip() {
                    settled = Some(ip.to_string());
                    break;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }

        note(&format!("{alias}: address ladder — {}", ladder.join(" → ")));
        assert!(
            settled.is_some(),
            "{alias}: the guest never reported an address vm would use. It reported: {}",
            ladder.join(", ")
        );
    }
}
