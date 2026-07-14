use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// One VM as reported by `prlctl list -a --json`.
#[derive(Debug, Deserialize)]
pub struct PrlVm {
    pub uuid: String,
    pub status: String,
    /// "-" when the VM has no IP (stopped, or tools not up yet)
    pub ip_configured: String,
    pub name: String,
}

impl PrlVm {
    /// The guest's usable IP, or None while it has none yet. A waking VM
    /// briefly reports only a link-local IPv6 (fe80::…) which isn't routable
    /// without a zone id — treat that the same as "no IP yet" and keep
    /// waiting for the DHCP address.
    pub fn ip(&self) -> Option<&str> {
        let ip = self.ip_configured.as_str();
        (ip != "-" && !ip.starts_with("fe80:")).then_some(ip)
    }
}

fn prlctl(args: &[&str]) -> Result<String> {
    let out = Command::new("prlctl")
        .args(args)
        .output()
        .context("failed to run prlctl (is Parallels installed?)")?;
    if !out.status.success() {
        bail!(
            "prlctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub fn list_all() -> Result<Vec<PrlVm>> {
    // -f (full) is what makes ip_configured carry the real IP.
    let json = prlctl(&["list", "-a", "-f", "--json"])?;
    serde_json::from_str(&json).context("unexpected `prlctl list --json` output")
}

pub fn find(name: &str) -> Result<PrlVm> {
    list_all()?
        .into_iter()
        .find(|vm| vm.name == name)
        .ok_or_else(|| anyhow::anyhow!("no Parallels VM named '{name}' (see `prlctl list -a`)"))
}

/// Start or resume as appropriate; no-op when already running. `true` if it
/// had to act.
///
/// There is no `vm start` — every command that needs a guest brings it up
/// itself — so this is the only place a wake is announced, and it always is:
/// the caller is about to sit through a boot or a resume, and silence there
/// reads as a hang. A VM that is already running is the common case and says
/// nothing.
pub fn ensure_running(alias: &str, name: &str) -> Result<bool> {
    let vm = find(name)?;
    match vm.status.as_str() {
        "running" => Ok(false),
        "stopped" => {
            eprintln!("vm ▸ {alias} ▸ '{name}' is stopped — starting it…");
            prlctl(&["start", name]).map(|_| true)
        }
        status @ ("suspended" | "paused") => {
            eprintln!("vm ▸ {alias} ▸ '{name}' is {status} — resuming it…");
            prlctl(&["resume", name]).map(|_| true)
        }
        other => bail!(
            "VM '{name}' is in unexpected state '{other}' — expected running, stopped, \
             suspended, or paused (see `prlctl list -a`)"
        ),
    }
}

/// A VM brought up: where to reach it, and whether vm had to wake it to get
/// there. `woke` is what tells a caller the guest's services may still be
/// coming up behind the IP (see `commands::bring_up`).
pub struct Up {
    pub ip: String,
    pub woke: bool,
}

/// Bring the VM up if it isn't already, and wait until it reports an IP.
///
/// Announcing the wait is this function's job as much as performing it: a wake
/// says what it is doing, and a long IP wait says where it has got to. A VM
/// that was already up returns on the first look and prints nothing —
/// `commands::bring_up` closes the loop with the readiness line.
pub fn ensure_up(alias: &str, name: &str) -> Result<Up> {
    let woke = ensure_running(alias, name)?;
    let ip = wait_for_ip(alias, name)?;
    Ok(Up { ip, woke })
}

/// How long a guest gets to report an IP once Parallels has it running.
const IP_TIMEOUT: Duration = Duration::from_secs(90);

/// How often the VM's state is re-read while waiting.
const POLL: Duration = Duration::from_secs(2);

/// A wait that passes this says so, and keeps saying so this often. Below it
/// the wait is not worth a line: the overwhelmingly common case is a VM that
/// is already up, which returns on the first look and prints nothing.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// How long Parallels gets to move a VM out of a settled off-state after
/// reporting a successful start or resume. Measured on Parallels 26.4: a
/// stopped VM reads `starting` the instant `prlctl start` returns, and a cold
/// resume of a 20 GB macOS VM reads `running` ~2.5s after `prlctl resume`
/// returns. 15s is a wide margin over that — so a VM still `suspended` here is
/// not a slow one, it is one that is not coming up.
const TRANSITION_GRACE: Duration = Duration::from_secs(15);

/// Statuses a VM never leaves on its own — it has to be started or resumed, so
/// one of these means no IP is ever coming. Everything else is left to the
/// timeout: `running`, and the transients Parallels reports while it works
/// (`starting`, `resuming`, `stopping`).
fn is_off(status: &str) -> bool {
    matches!(status, "stopped" | "suspended" | "paused")
}

/// What one observation of the VM means to a caller waiting on its IP.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Up(String),
    Wait,
    Fail(String),
}

fn assess(name: &str, status: &str, ip: Option<&str>, waited: Duration, timeout: Duration) -> Step {
    if let Some(ip) = ip {
        return Step::Up(ip.to_string());
    }
    // Checked before the timeout: it is the more specific diagnosis, and the
    // one whose advice is actionable.
    if is_off(status) && waited >= TRANSITION_GRACE {
        // The advice has to match the state it is advice about: a stopped VM
        // resumes to an error, and a suspended one starts to one.
        let verb = if status == "stopped" { "start" } else { "resume" };
        return Step::Fail(format!(
            "VM '{name}' is {status} {}s after vm asked Parallels to bring it up, so it \
             will never report an IP.\n  \
             Either the start/resume did not take effect, or something put the VM back \
             down while vm was waiting for it (`vm reap`, or a stop/suspend from the \
             Parallels GUI).\n  \
             `prlctl list -a` shows the current state; `prlctl {verb} \"{name}\"` brings \
             it up by hand.",
            waited.as_secs()
        ));
    }
    if waited >= timeout {
        return Step::Fail(format!(
            "VM '{name}' is {status} but did not report an IP within {}s — the guest may \
             still be booting, or Parallels Tools isn't running in it",
            timeout.as_secs()
        ));
    }
    Step::Wait
}

/// Wait until the guest reports an IP (Parallels Tools up / DHCP done),
/// narrating any wait long enough to wonder about. [`ensure_up`] announces the
/// readiness that ends it.
///
/// The VM's *status* is watched, not just its IP, because a VM that is not
/// running can never report one — so sitting out the full timeout on a
/// suspended VM buys nothing but a 90-second delay in front of a wrong answer
/// ("the guest may still be booting"). And a VM can be down here even though
/// `ensure_running` just brought it up: `vm reap` or a stop/suspend from the
/// Parallels GUI can put it back down while this loop is waiting on it.
fn wait_for_ip(alias: &str, name: &str) -> Result<String> {
    let start = Instant::now();
    let mut next_beat = HEARTBEAT;
    loop {
        let vm = find(name)?;
        let waited = start.elapsed();
        match assess(name, &vm.status, vm.ip(), waited, IP_TIMEOUT) {
            Step::Up(ip) => return Ok(ip),
            Step::Fail(msg) => bail!("{msg}"),
            Step::Wait => {
                if waited >= next_beat {
                    eprintln!(
                        "vm ▸ {alias} ▸ '{name}' {}, no IP yet — {}s of {}s",
                        vm.status,
                        waited.as_secs(),
                        IP_TIMEOUT.as_secs()
                    );
                    next_beat = waited + HEARTBEAT;
                }
                std::thread::sleep(POLL);
            }
        }
    }
}

/// The most guest-command argv `prlctl exec` is allowed to carry, in total
/// bytes. Over a threshold measured at ~3.9 KB on Parallels 26.4 (2026-07-12),
/// `prlctl exec` hangs forever — no output, no error, no guest-side process,
/// and it ignores SIGTERM. The limit is on the *combined* size of the command
/// line, not any single argument: ten 500-byte arguments hang as reliably as
/// one 5000-byte one. The cap sits well under the cliff to leave room for the
/// parts of the request not counted here (VM name, prlctl's own flags).
const EXEC_ARGV_LIMIT: usize = 3 * 1024;

/// Fail fast — with the real cause — where `prlctl exec` would hang forever.
fn check_exec_argv(name: &str, args: &[&str]) -> Result<()> {
    let total: usize = args.iter().map(|a| a.len() + 1).sum();
    if total > EXEC_ARGV_LIMIT {
        bail!(
            "refusing `prlctl exec {name} …`: the command line totals {total} bytes, \
             over vm's {EXEC_ARGV_LIMIT}-byte cap.\n  \
             prlctl exec hangs forever — silently, and immune to SIGTERM — once its \
             total command line passes ~3.9 KB (measured on Parallels 26.4; the limit \
             is combined size, not per argument).\n  \
             Send bulk data to the guest some other way: the agent protocol's stdin, \
             a synced file, or ssh."
        );
    }
    Ok(())
}

/// Run `args` in the guest's *console session* (the interactive desktop) via
/// Parallels Tools, as the console-logged-in user. This is how Windows exec
/// reaches session 1: ssh children land in session 0 on a non-interactive
/// window station, where UIA and every other GUI API see an empty desktop.
/// Caveats: argv is re-joined guest-side (no POSIX shell, so `~` never
/// expands), it requires a user logged in at the console, and the whole argv
/// must stay small (see [`check_exec_argv`]) — which is why this takes the
/// complete argv up front instead of returning a base `Command` to extend.
pub fn exec_console(name: &str, args: &[&str]) -> Result<Command> {
    check_exec_argv(name, args)?;
    let mut cmd = Command::new("prlctl");
    cmd.args(["exec", name, "--current-user"]);
    cmd.args(args);
    Ok(cmd)
}

/// Run a command in the console session, capturing output (for doctor).
pub fn exec_console_capture(name: &str, args: &[&str]) -> Result<Output> {
    exec_console(name, args)?
        .stdin(Stdio::null())
        .output()
        .context("failed to run prlctl exec")
}

/// Run `args` in the guest as the *superuser* — root on linux/macos, SYSTEM on
/// windows — via Parallels Tools. `vm run --elevated`'s transport, and the only
/// elevation available: sudo over ssh wants a password, and the Windows guest
/// user is not an administrator (UAC cannot be satisfied headless).
///
/// The same channel as [`exec_console`] minus `--current-user`, so it needs no
/// console login — but it runs as a user whose home is *not* the config user's:
/// `~` never expands here (argv is re-joined guest-side with no shell) and
/// `$HOME`/`%USERPROFILE%` belong to root/SYSTEM (on Windows,
/// `C:\WINDOWS\system32\config\systemprofile`). Everything passed in must
/// therefore be an absolute path — see [`crate::commands::agent_abs_path`].
///
/// Measured on Parallels 26.4, for anyone extending this: on linux/macos this
/// channel drops output written in the first fraction of a second, and reports a
/// nonzero exit only while the caller still holds the stdin pipe open. Both are
/// moot for the agent, whose protocol reads a request from that pipe (a
/// round-trip) before the child can print anything, and whose driver holds stdin
/// open across the wait. A simpler caller here would silently lose output.
pub fn exec_elevated(name: &str, args: &[&str]) -> Result<Command> {
    check_exec_argv(name, args)?;
    let mut cmd = Command::new("prlctl");
    cmd.args(["exec", name]);
    cmd.args(args);
    Ok(cmd)
}

/// Parallels Tools' exec session is not ready the instant a VM reports an IP: a
/// freshly resumed macOS guest refuses one for ~10s ("Unable to open new
/// session"), which is a wake to wait out, not a failure to report. Callers
/// retry a spawn that fails with this.
pub fn is_session_not_ready(stderr: &str) -> bool {
    stderr.contains("Unable to open new session")
}

/// Graceful shutdown via Parallels Tools ([`ensure_running`] boots it again).
/// Reap plumbing only — stopping is the *only* way vm ever puts a VM down, and
/// it does so on its own schedule, so there is deliberately no `vm stop` and no
/// `vm suspend`.
///
/// Stop rather than suspend: suspension proved unreliable on this stack — a
/// macOS guest's saved state can be one Parallels itself refuses to restore
/// (VZErrorDomain 12, and the "cannot restore" question is unanswerable
/// headless, leaving the VM stuck), and guests have re-suspended themselves
/// right after a resume. A saved state that will not restore is a VM that
/// cannot be used at all; a boot is seconds slower and always works.
pub fn stop(name: &str) -> Result<()> {
    prlctl(&["stop", name]).map(drop)
}

/// Existing snapshots as (id, name) pairs.
pub fn snapshot_list(name: &str) -> Result<Vec<(String, String)>> {
    let json = prlctl(&["snapshot-list", name, "--json"])?;
    parse_snapshot_list(&json)
        .with_context(|| format!("unexpected `prlctl snapshot-list {name} --json` output"))
}

fn parse_snapshot_list(json: &str) -> Result<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Snap {
        name: String,
    }
    if json.trim().is_empty() {
        return Ok(vec![]); // no snapshots → empty output, not `{}`
    }
    let map: std::collections::BTreeMap<String, Snap> = serde_json::from_str(json)?;
    Ok(map.into_iter().map(|(id, s)| (id, s.name)).collect())
}

/// The subset of `prlctl list -i --json` a snapshot pre-check needs: where
/// the VM lives on the host disk, and its RAM size (a running-VM snapshot
/// writes a memory image of about that size, then grows a delta disk).
#[derive(Debug)]
pub struct VmDetails {
    pub home: String,
    pub memory_mb: u64,
}

pub fn details(name: &str) -> Result<VmDetails> {
    let json = prlctl(&["list", "-i", name, "--json"])?;
    parse_details(&json)
        .with_context(|| format!("unexpected `prlctl list -i {name} --json` output"))
}

fn parse_details(json: &str) -> Result<VmDetails> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(rename = "Home")]
        home: String,
        #[serde(rename = "Hardware")]
        hardware: Hardware,
    }
    #[derive(Deserialize)]
    struct Hardware {
        memory: Memory,
    }
    #[derive(Deserialize)]
    struct Memory {
        /// e.g. "20480Mb"
        size: String,
    }
    let mut infos: Vec<Info> = serde_json::from_str(json)?;
    let info = infos
        .pop()
        .ok_or_else(|| anyhow::anyhow!("empty VM info list"))?;
    let mb = info
        .hardware
        .memory
        .size
        .trim_end_matches("Mb")
        .parse::<u64>()
        .with_context(|| format!("cannot parse memory size '{}'", info.hardware.memory.size))?;
    Ok(VmDetails {
        home: info.home,
        memory_mb: mb,
    })
}

/// Screenshot the VM display to a PNG file.
pub fn capture(name: &str, file: &str) -> Result<()> {
    prlctl(&["capture", name, "--file", file]).map(drop)
}

/// Create a snapshot and return its id (a `{uuid}` string).
pub fn snapshot_create(name: &str, snap_name: &str) -> Result<String> {
    let out = prlctl(&["snapshot", name, "--name", snap_name])?;
    parse_snapshot_id(&out)
        .ok_or_else(|| anyhow::anyhow!("could not find a snapshot id in prlctl output: {out}"))
}

/// Roll the VM back to a snapshot (restores disk AND run state).
pub fn snapshot_switch(name: &str, id: &str) -> Result<()> {
    prlctl(&["snapshot-switch", name, "--id", id]).map(drop)
}

pub fn snapshot_delete(name: &str, id: &str) -> Result<()> {
    prlctl(&["snapshot-delete", name, "--id", id]).map(drop)
}

/// prlctl prints e.g. `The snapshot with id {8b171e2f-…} has been successfully
/// created.` — pull out the braced id.
fn parse_snapshot_id(out: &str) -> Option<String> {
    let start = out.find('{')?;
    let end = out[start..].find('}')? + start;
    Some(out[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prlctl_list_json() {
        let json = r#"[
            {"uuid": "{db670d16}", "status": "suspended", "ip_configured": "-", "name": "Ubuntu 24.04"},
            {"uuid": "{d2b7786c}", "status": "running", "ip_configured": "10.211.55.4", "name": "Windows 11"}
        ]"#;
        let vms: Vec<PrlVm> = serde_json::from_str(json).unwrap();
        assert_eq!(vms.len(), 2);
        assert_eq!(vms[0].ip(), None);
        assert_eq!(vms[1].ip(), Some("10.211.55.4"));
    }

    /// Everything vm itself sends through `prlctl exec` — agent invocations,
    /// doctor probes — is a few dozen bytes and must pass untouched.
    #[test]
    fn exec_argv_guard_passes_real_vm_traffic() {
        check_exec_argv(
            "Windows 11",
            &["cmd", "/c", r"%USERPROFILE%\.vm\bin\vm.exe _exec"],
        )
        .unwrap();
        check_exec_argv("Windows 11", &["cmd", "/c", "whoami"]).unwrap();
    }

    /// Over the cap the transport would hang forever, silently; the guard's
    /// whole job is to turn that into an error naming the real cause.
    #[test]
    fn exec_argv_guard_rejects_oversized_argv_with_the_cause() {
        let big = "A".repeat(EXEC_ARGV_LIMIT + 1);
        let err = check_exec_argv("Ubuntu 24.04", &["/bin/echo", &big])
            .unwrap_err()
            .to_string();
        assert!(err.contains("hangs forever"), "{err}");
        assert!(err.contains("stdin"), "{err}");
    }

    /// The limit is on the *total* command line: many small arguments hang
    /// prlctl exactly as reliably as one large one (measured — five 1000-byte
    /// args wedge it just like a single 5000-byte arg), so a per-argument
    /// check would wave through commands that never return.
    #[test]
    fn exec_argv_guard_sums_across_arguments() {
        let arg = "A".repeat(500);
        let args: Vec<&str> = std::iter::repeat_n(arg.as_str(), 10).collect();
        assert!(check_exec_argv("Ubuntu 24.04", &args).is_err());
    }

    /// Both prlctl channels ride the same hanging argv, so both are guarded.
    /// The elevated one carries only `<abs agent path> _exec` — bytes, not
    /// kilobytes, because the command itself goes over the agent's stdin.
    #[test]
    fn both_prlctl_channels_check_their_argv() {
        exec_elevated("Ubuntu 24.04", &["/home/parallels/.vm/bin/vm", "_exec"]).unwrap();
        exec_elevated(
            "Windows 11",
            &[r"C:\Users\hakesson\.vm\bin\vm.exe", "_exec"],
        )
        .unwrap();

        let big = "A".repeat(EXEC_ARGV_LIMIT + 1);
        let err = exec_elevated("Ubuntu 24.04", &["sh", "-c", &big])
            .unwrap_err()
            .to_string();
        assert!(err.contains("hangs forever"), "{err}");
    }

    /// The one prlctl error a caller must not treat as failure: Tools' exec
    /// session lags a resumed guest's IP by ~10s (macOS), and that is a wake to
    /// wait out, not a broken VM.
    #[test]
    fn a_lagging_tools_session_is_recognized() {
        assert!(is_session_not_ready(
            "Failed to execute the command: Unable to open new session in this virtual machine."
        ));
        assert!(!is_session_not_ready(
            "Unable to perform the operation because \"macOS\" is not started."
        ));
    }

    #[test]
    fn extracts_snapshot_id_from_prlctl_output() {
        let out = "Creating the snapshot...\nThe snapshot with id {8b171e2f-4b7f-4e01-a689-a2d360d63e49} has been successfully created.\n";
        assert_eq!(
            parse_snapshot_id(out).as_deref(),
            Some("{8b171e2f-4b7f-4e01-a689-a2d360d63e49}")
        );
        assert_eq!(parse_snapshot_id("no id here"), None);
    }

    #[test]
    fn parses_snapshot_list_including_empty() {
        assert_eq!(parse_snapshot_list("").unwrap(), vec![]);
        assert_eq!(parse_snapshot_list("\n").unwrap(), vec![]);
        // Real shape from Parallels 26.4.
        let json = r#"{
            "{351b744b-3b1b-422c-957f-cfeae36b472d}": {
            "name": "vm-with-snapshot-lin",
            "date": "2026-07-10 11:41:38",
            "state": "poweron",
            "current": true,
            "parent": ""
        }
        }"#;
        assert_eq!(
            parse_snapshot_list(json).unwrap(),
            vec![(
                "{351b744b-3b1b-422c-957f-cfeae36b472d}".to_string(),
                "vm-with-snapshot-lin".to_string()
            )]
        );
    }

    #[test]
    fn parses_vm_details() {
        // Trimmed from real `prlctl list -i --json` output (Parallels 26.4).
        let json = r#"[{
            "Name": "macOS",
            "Home": "/Users/hakesson/Parallels/macOS.macvm/",
            "Hardware": {
                "cpu": {"cpus": 10},
                "memory": {"size": "20480Mb", "auto": "off", "hotplug": false}
            }
        }]"#;
        let d = parse_details(json).unwrap();
        assert_eq!(d.home, "/Users/hakesson/Parallels/macOS.macvm/");
        assert_eq!(d.memory_mb, 20480);
    }

    /// The statuses in `is_off` are the ones a VM stays in until something
    /// starts or resumes it; the transients Parallels reports while it works
    /// are on their way somewhere and must not be mistaken for them.
    #[test]
    fn only_settled_off_states_count_as_off() {
        for status in ["stopped", "suspended", "paused"] {
            assert!(is_off(status), "{status}");
        }
        // Observed live on Parallels 26.4 during start/resume/stop.
        for status in ["running", "starting", "resuming", "stopping"] {
            assert!(!is_off(status), "{status}");
        }
    }

    /// An IP settles it, whatever the status says or how long it took.
    #[test]
    fn an_ip_ends_the_wait() {
        let up = Step::Up("10.211.55.4".into());
        assert_eq!(
            assess("v", "running", Some("10.211.55.4"), secs(0), secs(90)),
            up
        );
        // Defensive: a stale status alongside a real IP must not lose to the
        // off-state check below — the address is the thing the caller needs.
        assert_eq!(
            assess("v", "suspended", Some("10.211.55.4"), secs(60), secs(90)),
            up
        );
    }

    #[test]
    fn a_booting_guest_is_given_its_full_timeout() {
        for waited in [0, 5, 30, 89] {
            assert_eq!(
                assess("v", "running", None, secs(waited), secs(90)),
                Step::Wait,
                "{waited}s"
            );
        }
    }

    /// The grace window exists because `prlctl` reports the old status for a
    /// beat after it returns — bailing on that would break every resume.
    #[test]
    fn an_off_vm_is_given_the_grace_window_before_being_judged() {
        for waited in [0, 5, 14] {
            assert_eq!(
                assess("v", "suspended", None, secs(waited), secs(90)),
                Step::Wait,
                "{waited}s"
            );
        }
        // Transients get no early exit at all: they are Parallels working.
        assert_eq!(
            assess("v", "starting", None, secs(60), secs(90)),
            Step::Wait
        );
        assert_eq!(
            assess("v", "resuming", None, secs(60), secs(90)),
            Step::Wait
        );
    }

    #[test]
    fn an_off_vm_past_the_grace_window_fails_early_and_says_why() {
        for status in ["stopped", "suspended", "paused"] {
            let Step::Fail(msg) = assess("macOS", status, None, secs(15), secs(90)) else {
                panic!("{status} past grace must fail");
            };
            assert!(msg.contains(status), "{msg}");
            // The two causes seen in the wild, and the way out of both.
            assert!(msg.contains("did not take effect"), "{msg}");
            assert!(msg.contains("vm reap"), "{msg}");
            // The way out must fit the state: `prlctl resume` on a stopped VM
            // is itself an error.
            let verb = if status == "stopped" { "start" } else { "resume" };
            assert!(msg.contains(&format!(r#"prlctl {verb} "macOS""#)), "{msg}");
        }
    }

    /// The old error blamed a booting guest / missing Parallels Tools for
    /// *every* IP-less wait, including a VM that was plainly suspended. It now
    /// only fires where that story is actually the plausible one.
    #[test]
    fn the_timeout_message_names_the_status_it_timed_out_on() {
        let Step::Fail(msg) = assess("macOS", "running", None, secs(90), secs(90)) else {
            panic!("a running VM with no IP must time out");
        };
        assert!(msg.contains("is running"), "{msg}");
        assert!(msg.contains("Parallels Tools"), "{msg}");

        // A suspended VM that also ran out the clock gets the *specific*
        // diagnosis, not the booting-guest guess.
        let Step::Fail(msg) = assess("macOS", "suspended", None, secs(90), secs(90)) else {
            panic!("suspended must fail");
        };
        assert!(msg.contains("will never report an IP"), "{msg}");
        assert!(!msg.contains("Parallels Tools"), "{msg}");
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn link_local_ipv6_is_not_an_ip_yet() {
        // Seen live: a resuming Windows guest reports its link-local IPv6
        // before DHCP completes; ssh to it fails with "No route to host".
        let vm = PrlVm {
            uuid: "{x}".into(),
            status: "running".into(),
            ip_configured: "fe80::bcca:2118:95a7:5e25".into(),
            name: "Windows 11".into(),
        };
        assert_eq!(vm.ip(), None);
    }
}
