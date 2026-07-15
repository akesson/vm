//! `vm doctor [alias]` — diagnose host and guest setup, one check per line.
//!
//! Installs nothing. It brings a VM up in exactly one case: when the caller
//! *named* one (`vm doctor linux`), because the checks worth having — ssh,
//! agent, git, claude — are the live ones, and there is no `vm start` to run
//! first. A bare `vm doctor` surveys every configured VM, so it stays
//! read-only: booting a whole fleet to look at it would be a worse surprise
//! than skipping the guests that are down.

use crate::config::{Config, GuestOs, VmConfig};
use crate::proto::{PROTO_VERSION, VersionInfo};
use crate::{commands, crumb, notice, prl, prldnd, ssh};
use anyhow::Result;

struct Report {
    failures: u32,
}

impl Report {
    fn ok(&mut self, what: &str, detail: &str) {
        notice!("  ✓ {what}: {detail}");
    }
    fn fail(&mut self, what: &str, detail: &str) {
        self.failures += 1;
        notice!("  ✗ {what}: {detail}");
    }
    fn skip(&mut self, what: &str, detail: &str) {
        notice!("  - {what}: {detail}");
    }
}

pub fn doctor(alias: Option<&str>) -> Result<i32> {
    let mut r = Report { failures: 0 };

    notice!("host");
    let vms = match prl::list_all() {
        Ok(vms) => {
            r.ok("prlctl", &format!("{} VMs registered", vms.len()));
            vms
        }
        Err(err) => {
            r.fail("prlctl", &format!("{err:#}"));
            vec![]
        }
    };
    let cfg = match Config::load() {
        Ok(cfg) => {
            let aliases: Vec<&str> = cfg.vm.keys().map(String::as_str).collect();
            r.ok(
                "config",
                &format!("{} ({})", Config::path().display(), aliases.join(", ")),
            );
            cfg
        }
        Err(err) => {
            r.fail("config", &format!("{err:#}"));
            notice!("vm ▸ doctor ▸ {} problem(s)", r.failures);
            return Ok(1);
        }
    };
    // A typo'd alias must not silently check nothing and report success — the
    // per-VM loop below would simply match no VM.
    if let Some(alias) = alias {
        cfg.get(alias)?;
    }
    let key = crate::sync::expand_home("~/.ssh/id_ed25519")?;
    if key.exists() {
        r.ok("ssh key", &key.display().to_string());
    } else {
        r.fail(
            "ssh key",
            "~/.ssh/id_ed25519 missing — `ssh-keygen -t ed25519`",
        );
    }
    if !crate::reap::launchd_loaded() {
        r.skip(
            "reap timer",
            "not installed — `vm reap --install` shuts down idle VMs",
        );
    } else if crate::reap::plist_is_stale() {
        // Installed jobs keep running the plist they were installed with. A
        // pre-journal one still redirects every sweep into ~/Library/Logs, a
        // file nothing rotates and nothing timestamps — so it would go on
        // growing forever, silently, on exactly the machines that upgraded.
        r.fail(
            "reap timer",
            "installed from a pre-journal vm — sweeps still go to an unrotated log; \
             `vm reap --install` to move them to the journal",
        );
    } else {
        r.ok("reap timer", crate::reap::LAUNCHD_LABEL);
    }
    match crate::journal::status() {
        Some((path, bytes)) => r.ok(
            "journal",
            &format!("{} — {} KB", path.display(), bytes.div_ceil(1024)),
        ),
        None => r.skip(
            "journal",
            &crate::journal::path().map_or_else(
                || "no HOME — nowhere to keep one".to_string(),
                |p| format!("{} — written on the next run", p.display()),
            ),
        ),
    }

    for (name, vm) in &cfg.vm {
        if alias.is_some_and(|a| a != name) {
            continue;
        }
        notice!("{name} ({})", vm.parallels_name);
        let Some(prl_vm) = vms.iter().find(|p| p.name == vm.parallels_name) else {
            r.fail("vm", "not registered in Parallels (`prlctl list -a`)");
            continue;
        };
        match prl::snapshot_list(&vm.parallels_name) {
            Ok(snaps) => {
                let stale: Vec<&str> = snaps
                    .iter()
                    .filter(|(_, n)| n.starts_with("vm-with-snapshot-"))
                    .map(|(_, n)| n.as_str())
                    .collect();
                if !stale.is_empty() {
                    r.fail(
                        "snapshots",
                        &format!(
                            "stale from killed --with-snapshot runs, wasting disk: {} — \
                             the next `vm exec {name} --with-snapshot` sweeps them, or delete \
                             via `prlctl snapshot-list`/`snapshot-delete`",
                            stale.join(", ")
                        ),
                    );
                }
            }
            Err(err) => r.fail("snapshots", &format!("{err:#}")),
        }
        // The live checks below run against the guest, so hold the use lock
        // across them: without it reap can shut the VM down mid-probe and turn a
        // healthy guest into a string of spurious failures. Kept for the whole
        // check block (its Drop runs at the end of the iteration). A bare `vm
        // doctor` that only *surveys* a running guest still takes it — reap must
        // not race the survey either — and the down-branch takes it before
        // bringing the guest up.
        let _use;
        let target = if prl_vm.status == "running" {
            _use = crate::lock::shared(name)?;
            r.ok("status", "running");
            match commands::ssh_target(vm) {
                Ok(t) => t,
                Err(err) => {
                    r.fail("ip", &format!("{err:#}"));
                    continue;
                }
            }
        } else if alias.is_some() {
            _use = crate::lock::shared(name)?;
            match commands::bring_up(name, vm) {
                Ok(target) => {
                    r.ok("status", &format!("brought up (was {})", prl_vm.status));
                    target
                }
                Err(err) => {
                    r.fail(
                        "status",
                        &format!("{} — cannot bring it up: {err:#}", prl_vm.status),
                    );
                    continue;
                }
            }
        } else {
            r.skip(
                "status",
                &format!(
                    "{} — live checks skipped; `vm doctor {name}` brings it up and runs them",
                    prl_vm.status
                ),
            );
            continue;
        };
        if !ssh::reachable(&target) {
            r.fail(
                "ssh",
                &format!(
                    "{} not reachable (sshd? firewall? key?)",
                    target.destination()
                ),
            );
            continue;
        }
        r.ok("ssh", &target.destination());

        match commands::agent_call(vm, &target, &["_version"]) {
            Ok(json) => match serde_json::from_str::<VersionInfo>(&json) {
                Ok(v) if v.proto == PROTO_VERSION => {
                    r.ok("agent", &format!("v{} (proto v{})", v.binary, v.proto));
                }
                Ok(v) => r.fail(
                    "agent",
                    &format!(
                        "speaks proto v{}, host needs v{PROTO_VERSION} — `vm deploy {name}`",
                        v.proto
                    ),
                ),
                Err(err) => r.fail("agent", &format!("bad _version reply: {err:#}")),
            },
            Err(err) => r.fail("agent", &format!("{err:#} — `vm deploy {name}`")),
        }

        match ssh::run_capture(&target, &["git", "--version"]) {
            Ok(out) if out.status.success() => {
                r.ok("git", String::from_utf8_lossy(&out.stdout).trim());
            }
            _ => r.fail("git", "not on the guest's ssh PATH (sync needs it)"),
        }

        let root = ssh::shell_quote(&vm.work_root);
        let probe = format!("mkdir -p {root} && test -w {root}");
        match ssh::run_capture(&target, &["sh", "-c", &ssh::shell_quote(&probe)]) {
            Ok(out) if out.status.success() => r.ok("work_root", &vm.work_root),
            _ => r.fail("work_root", &format!("{} not writable", vm.work_root)),
        }

        claude_checks(&mut r, &target);
        idle_checks(&mut r, name, vm);

        match vm.os {
            GuestOs::Windows => console_checks(&mut r, vm),
            GuestOs::Linux => shutdown_checks(&mut r, name, &target),
            GuestOs::Macos => {}
        }
    }

    if r.failures == 0 {
        crumb!("vm ▸ doctor ▸ all checks passed");
        Ok(0)
    } else {
        // A verdict, not narration: `vm doctor -q` printing thirty check lines
        // and swallowing the count would be the wrong way round.
        notice!("vm ▸ doctor ▸ {} problem(s)", r.failures);
        Ok(1)
    }
}

/// `vm claude` needs the claude CLI installed and authenticated in the guest.
/// Not installed → skip (the feature is optional); installed but without
/// credentials → fail (a half-configured guest would only surface later as a
/// confusing `vm claude` runtime error).
fn claude_checks(r: &mut Report, target: &ssh::SshTarget) {
    // Prepend the same per-user dirs the exec agent does (see
    // exec::guest::augmented_path): non-interactive ssh gets a bare PATH
    // that misses ~/.local/bin, where claude usually lives.
    const PATH: &str = r#"PATH="$HOME/bin:$HOME/.cargo/bin:$HOME/.local/bin:$HOME/.vm/bin:$PATH""#;
    let version = format!("{PATH} claude --version");
    match ssh::run_capture(target, &["sh", "-c", &ssh::shell_quote(&version)]) {
        Ok(out) if out.status.success() => {
            r.ok("claude", String::from_utf8_lossy(&out.stdout).trim());
        }
        _ => {
            r.skip("claude", "not installed — needed only for `vm claude`");
            return;
        }
    }
    // A real probe call rather than a credentials-presence check: a stale
    // OAuth login looks authenticated on disk and only 401s on use. Costs
    // one haiku call and a few seconds — doctor's slowest check.
    let auth = format!(r#"{PATH} claude -p --model haiku "say hi""#);
    match ssh::run_capture(target, &["sh", "-c", &ssh::shell_quote(&auth)]) {
        Ok(out) if out.status.success() => {
            r.ok(
                "claude auth",
                &format!("probe replied: {}", first_line(&out.stdout)),
            );
        }
        Ok(out) => {
            let detail = if out.stderr.is_empty() {
                first_line(&out.stdout)
            } else {
                first_line(&out.stderr)
            };
            r.fail(
                "claude auth",
                &format!("probe failed: {detail} — log in inside the guest (run `claude`)"),
            );
        }
        Err(err) => r.fail("claude auth", &format!("probe failed: {err:#}")),
    }
}

/// Reap asks the guest agent for input idle before shutting down, so manual
/// GUI use is protected; verify that probe works. On Windows this exercises
/// the console transport (GetLastInputInfo is per-session).
fn idle_checks(r: &mut Report, name: &str, vm: &VmConfig) {
    match crate::idle::input_idle(vm) {
        Ok(idle) => r.ok(
            "input idle",
            &format!(
                "{}m — reap won't shut down while you're at the console",
                idle.as_secs() / 60
            ),
        ),
        Err(err) => r.fail(
            "input idle",
            &format!(
                "{err:#} — reap will shut down regardless of console use (`vm deploy {name}`?)"
            ),
        ),
    }
}

/// A linux guest without `vm deploy`'s shutdown unit takes ~95s to stop, and
/// Parallels force-kills a guest that takes 120s — so the margin between a
/// clean shutdown and a yanked power cord is about twenty seconds. Nothing else
/// in vm would notice it had gone missing: reap's stop would just get slower,
/// and then one day be a force-kill. See [`crate::prldnd`].
fn shutdown_checks(r: &mut Report, name: &str, target: &ssh::SshTarget) {
    let advice = format!("`vm deploy {name}` installs it");
    match prldnd::check(target) {
        Ok(prldnd::State::Good) => r.ok(
            "shutdown unit",
            &format!("{} active — stops take seconds", prldnd::UNIT_NAME),
        ),
        Ok(prldnd::State::Missing) => r.fail(
            "shutdown unit",
            &format!(
                "{} not installed — this guest takes ~95s to shut down (prldnd jams \
                 gnome-session's logout) and Parallels force-kills it at 120s. {advice}",
                prldnd::UNIT_NAME
            ),
        ),
        Ok(prldnd::State::NotEnabled(state)) => r.fail(
            "shutdown unit",
            &format!(
                "{} is {state}, so nothing pulls it in at boot and it cannot run at \
                 the next shutdown. {advice}",
                prldnd::UNIT_NAME
            ),
        ),
        Ok(prldnd::State::Inactive(state)) => r.fail(
            "shutdown unit",
            &format!(
                "{} is enabled but {state} — a oneshot runs its ExecStop only while \
                 active, so this one will not fire. {advice}",
                prldnd::UNIT_NAME
            ),
        ),
        Ok(prldnd::State::Stale) => r.fail(
            "shutdown unit",
            &format!(
                "{} differs from the one this vm installs — an older copy may be \
                 missing a detail it needs to work at all. {advice}",
                prldnd::UNIT_NAME
            ),
        ),
        Err(err) => r.fail("shutdown unit", &format!("{err:#}")),
    }
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim().lines().next().unwrap_or("(no output)").into()
}

/// Windows exec rides `prlctl exec --current-user` into the console session
/// (ssh lands in session 0, where GUI APIs see an empty desktop). Verify that
/// path end to end: a user is logged in at the console, it is the configured
/// user, and the agent answers through the transport.
fn console_checks(r: &mut Report, vm: &VmConfig) {
    let who = match prl::exec_console_capture(&vm.parallels_name, &["cmd", "/c", "whoami"]) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            r.fail(
                "console user",
                "prlctl exec failed — is a user logged in at the VM console?",
            );
            return;
        }
    };
    // whoami prints `machine\user`; exec runs as whoever owns the console.
    let user = who.rsplit('\\').next().unwrap_or(&who);
    if user.eq_ignore_ascii_case(&vm.user) {
        r.ok("console user", &who);
    } else {
        r.fail(
            "console user",
            &format!(
                "console session belongs to '{who}' but config user is '{}' — \
                 exec would run as (and see the checkout of) the wrong user",
                vm.user
            ),
        );
        return;
    }
    let agent = format!("{} _version", commands::agent_console_path(vm));
    match prl::exec_console_capture(&vm.parallels_name, &["cmd", "/c", &agent]) {
        Ok(out) if out.status.success() => {
            match serde_json::from_str::<VersionInfo>(String::from_utf8_lossy(&out.stdout).trim()) {
                Ok(v) if v.proto == PROTO_VERSION => {
                    r.ok("console agent", &format!("v{} via prlctl exec", v.binary));
                }
                Ok(v) => r.fail(
                    "console agent",
                    &format!("speaks proto v{}, host needs v{PROTO_VERSION}", v.proto),
                ),
                Err(err) => r.fail("console agent", &format!("bad _version reply: {err:#}")),
            }
        }
        _ => r.fail(
            "console agent",
            "agent did not answer via prlctl exec — `vm deploy` it?",
        ),
    }
}
