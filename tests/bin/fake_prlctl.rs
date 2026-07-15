//! A scriptable stand-in for the commands vm shells out to — `prlctl` above
//! all, and `ssh` where a test wants the network under its control too. So vm's
//! VM lifecycle can be tested without Parallels, and without a VM to break.
//!
//! vm reaches Parallels through exactly one name (`prl::prlctl_bin`) and ssh
//! through one more (`ssh::ssh_bin`); `VM_PRLCTL` and `VM_SSH` point them here.
//! What that makes testable is everything wrapped around them: `wait_for_ip`'s
//! status machine, reap's decision matrix, doctor's checks, the argv each one
//! builds. All of it pure logic that had only ever been exercised by running it
//! against a real guest and watching.
//!
//! # The guest
//!
//! `$FAKE_PRLCTL_DIR/scenario.json`:
//!
//! ```json
//! {
//!   "phases": [ {"status": "stopped",  "ip_configured": "-", …},
//!               {"status": "running",  "ip_configured": "-", …},
//!               {"status": "running",  "ip_configured": "10.211.55.3", …} ],
//!   "rules":  [ {"match_prefix": ["exec"], "responses": [{"exec_passthrough": true}]} ]
//! }
//! ```
//!
//! `phases` is a guest that **wakes up**. `prlctl list` reports the phase the
//! guest is in; `start` and `resume` are what set it moving, and each subsequent
//! `list` advances it one phase, the last repeating forever. Before a start, every
//! `list` answers with phase 0 — so it does not matter how many times vm looks at
//! a stopped VM before it decides to start it, which is the whole point: a test
//! says what the *guest* does, not what vm's nth call happens to get back.
//!
//! That is how a cold boot is replayed — stopped, running-with-no-address, a
//! link-local IPv6, the Parallels ULA, an APIPA address, and finally the DHCP
//! lease — and how a resume replays the stale status Parallels reports for a beat
//! after `prlctl resume` has already taken effect. The phase lives in a file,
//! because every prlctl call is a new process.
//!
//! # The rules
//!
//! `rules` are matched first, in order, so a test can override any of it — a
//! `resume` that fails, an `exec` that hangs. Each rule's responses are consumed
//! in sequence, one per call, the last repeating. A response can:
//!
//! - `stdout` / `stdout_file` — what the command prints
//! - `stderr`, `exit` — what it says when it fails, and how
//! - `sleep_ms` — how slowly it answers
//! - `hang` — never answer at all, the way an oversized `prlctl exec` hangs:
//!   silently, and deaf to SIGTERM
//! - `exec_passthrough` — run the *real* vm agent locally with the tail of the
//!   argv, stdio wired straight through, so a `prlctl exec … vm _exec` becomes a
//!   genuine host↔agent round trip with only Parallels replaced
//!
//! Every invocation is appended to `calls.log`, one JSON line per call, so a test
//! can assert on what vm *did*: that it resumed a VM exactly once, that it never
//! stopped one that was in use, that no command line it built came near the size
//! that hangs the real thing.
//!
//! An argv nothing matches exits 66 and says so, rather than inventing an answer:
//! a test that has fallen off its scenario should fail loudly, not quietly pass.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Exit code for "your scenario does not cover this". Not one prlctl uses.
const UNMATCHED: i32 = 66;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(
        std::env::var("FAKE_PRLCTL_DIR").expect("fake-prlctl: FAKE_PRLCTL_DIR is not set"),
    );

    record(&dir, &args);

    let body = std::fs::read_to_string(dir.join("scenario.json"))
        .expect("fake-prlctl: no scenario.json in FAKE_PRLCTL_DIR");
    let scenario: Scenario =
        serde_json::from_str(&body).expect("fake-prlctl: scenario.json does not parse");

    // An explicit rule wins over the guest, so a test can break anything.
    if let Some((index, rule)) = scenario
        .rules
        .iter()
        .enumerate()
        .find(|(_, r)| r.matches(&args))
    {
        let n = bump(&dir, &format!("rule-{index}"));
        let response = rule
            .responses
            .get(n)
            .or_else(|| rule.responses.last())
            .expect("fake-prlctl: a rule needs at least one response");
        response.perform(&dir, &args);
    }

    if !scenario.phases.is_empty() {
        guest(&dir, &scenario, &args);
    }

    eprintln!("fake-prlctl: nothing in scenario.json matches {args:?}");
    std::process::exit(UNMATCHED);
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Scenario {
    /// The guest, phase by phase — see the module docs.
    phases: Vec<serde_json::Value>,
    rules: Vec<Rule>,
}

/// The built-in guest: `list` reports where it is, `start`/`resume` set it going,
/// `stop` puts it back down.
fn guest(dir: &Path, scenario: &Scenario, args: &[String]) -> ! {
    let verb = args.first().map(String::as_str).unwrap_or("");
    match verb {
        "list" => {
            let phase = read(dir, "phase").unwrap_or(0);
            let waking = read(dir, "waking").unwrap_or(0) == 1;
            let at = phase.min(scenario.phases.len() - 1);
            // Looking at a guest is what lets time pass for it — but only once
            // it has been told to wake. A stopped VM stays stopped however many
            // times vm asks, so a test does not have to count vm's calls.
            if waking {
                write(dir, "phase", phase + 1);
            }
            let listing = serde_json::to_string(&[&scenario.phases[at]]).unwrap_or_default();
            println!("{listing}");
            std::process::exit(0);
        }
        "start" | "resume" => {
            write(dir, "waking", 1);
            // The phase Parallels reports for a beat *after* the call returns is
            // still the old one; phase 1 is that stale reading.
            write(dir, "phase", 1);
            std::process::exit(0);
        }
        "stop" => {
            write(dir, "waking", 0);
            write(dir, "phase", 0);
            std::process::exit(0);
        }
        _ => {
            eprintln!("fake-prlctl: the guest handles list/start/resume/stop, not {args:?}");
            std::process::exit(UNMATCHED);
        }
    }
}

#[derive(serde::Deserialize)]
struct Rule {
    /// The whole argv, exactly.
    #[serde(default)]
    r#match: Option<Vec<String>>,
    /// The argv starts with these — `["exec", "Windows 11"]`, never mind what follows.
    #[serde(default)]
    match_prefix: Option<Vec<String>>,
    /// Every one of these appears somewhere in the argv. How an ssh command is
    /// told from another: they all begin with the same dozen `-o` options, and
    /// differ only in what they ask the guest to *run* (`_version`, `git
    /// --version`, `claude -p`).
    #[serde(default)]
    match_contains: Option<Vec<String>>,
    responses: Vec<Response>,
}

impl Rule {
    fn matches(&self, args: &[String]) -> bool {
        if let Some(exact) = &self.r#match {
            return args == exact.as_slice();
        }
        if let Some(prefix) = &self.match_prefix {
            return args.len() >= prefix.len() && args[..prefix.len()] == prefix[..];
        }
        if let Some(needles) = &self.match_contains {
            let joined = args.join(" ");
            return needles.iter().all(|needle| joined.contains(needle));
        }
        false
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Response {
    stdout: Option<String>,
    stdout_file: Option<String>,
    stderr: Option<String>,
    exit: i32,
    sleep_ms: u64,
    hang: bool,
    exec_passthrough: bool,
}

impl Response {
    fn perform(&self, dir: &Path, args: &[String]) -> ! {
        if self.sleep_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.sleep_ms));
        }
        if self.hang {
            hang();
        }
        if self.exec_passthrough {
            passthrough(args);
        }
        if let Some(name) = &self.stdout_file {
            let body = std::fs::read_to_string(dir.join(name))
                .unwrap_or_else(|e| panic!("fake-prlctl: stdout_file {name}: {e}"));
            print!("{body}");
        }
        if let Some(text) = &self.stdout {
            println!("{text}");
        }
        if let Some(text) = &self.stderr {
            eprintln!("{text}");
        }
        let _ = std::io::stdout().flush();
        std::process::exit(self.exit);
    }
}

/// The way a real `prlctl exec` dies once its command line runs past ~3.9 KB: no
/// output, no error, no guest-side process — and deaf to SIGTERM, so only a
/// `kill -9` ends it. vm's argv guard exists so this is never reached.
fn hang() -> ! {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Run the real vm agent in place of the guest's, and become its exit code.
///
/// What arrives is the whole console invocation —
/// `exec "Windows 11" --current-user cmd /c "%USERPROFILE%\.vm\bin\vm.exe _exec"`
/// — of which only the *verb* is meaningful here: the agent it names is the same
/// binary the host is running, and it is already on this machine. So the wrapping
/// is dropped and the agent is run directly, with stdio wired straight through.
/// The request, the heartbeat, the exit code and the teardown are all genuine;
/// only Parallels is gone.
fn passthrough(args: &[String]) -> ! {
    let vm = std::env::var("FAKE_PRLCTL_VM").expect("fake-prlctl: FAKE_PRLCTL_VM is not set");
    let joined = args.join(" ");
    let verb_at = joined
        .find(" _")
        .unwrap_or_else(|| panic!("fake-prlctl: no guest verb in {args:?}"));
    let agent_args: Vec<&str> = joined[verb_at + 1..].split_whitespace().collect();

    let status = Command::new(vm)
        .args(agent_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("fake-prlctl: the passthrough agent runs");
    std::process::exit(status.code().unwrap_or(1));
}

/// Append this invocation to `calls.log`: one JSON array per line.
fn record(dir: &Path, args: &[String]) {
    let line = serde_json::to_string(args).unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("calls.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// State that has to outlive the process reading it — every call is a new one.
fn state_path(dir: &Path, key: &str) -> PathBuf {
    let state = dir.join("state");
    let _ = std::fs::create_dir_all(&state);
    state.join(key)
}

fn read(dir: &Path, key: &str) -> Option<usize> {
    std::fs::read_to_string(state_path(dir, key))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write(dir: &Path, key: &str, value: usize) {
    let _ = std::fs::write(state_path(dir, key), value.to_string());
}

/// How many times this key has been hit, before this one. One byte appended per
/// call: an `O_APPEND` write is atomic enough for the concurrent-`vm` tests to
/// share it.
fn bump(dir: &Path, key: &str) -> usize {
    let path = state_path(dir, key);
    let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(b".");
    }
    before as usize
}
