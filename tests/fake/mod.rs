//! The harness for tests that drive `vm` against a fake `prlctl` (and a fake
//! `ssh`) — see `tests/bin/fake_prlctl.rs` for what it can be told to do.
//!
//! Each [`Fake`] is one temp dir standing in for everything outside vm: the
//! config it reads, the locks it takes, the journal it writes, the home
//! directory it looks in, the guest it drives, and the network it reaches over.
//! Nothing a test here does can touch the real machine — which matters more than
//! it sounds, because the machine vm is developed on has a real Parallels guest
//! at the address these scenarios hand out.
//!
//! vm runs as a real subprocess. That is the only way to give it an environment
//! of its own, and the only way to test what lives in a process — a journal, a
//! lock held across a run, a wake that takes four polls.

#![allow(dead_code)] // each test file uses a different part of this

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");
const FAKE_BIN: &str = env!("CARGO_BIN_EXE_fake-prlctl");

/// vm's clock, shrunk: a 90-second IP timeout becomes 1.8s, a 2-second poll
/// 40ms. Slow enough that a loaded CI box does not skip a state, fast enough
/// that a whole wake — timeouts and all — fits inside a test ([`vm::clock`]).
const TICK_MS: &str = "20";

/// The address the guests here settle on. Deliberately TEST-NET-1 (RFC 5737):
/// it is a perfectly ordinary routable-looking IPv4 as far as vm is concerned,
/// and it belongs to nobody, so a test that somehow escapes the fake ssh reaches
/// nothing rather than reaching the developer's own Windows guest.
pub const LEASE: &str = "192.0.2.7";

/// The Parallels name of the one VM in these scenarios.
pub const VM_NAME: &str = "Windows 11";

/// One `prlctl list -a -f --json` entry: what a guest reports at one moment.
pub fn phase(status: &str, ip: &str) -> Value {
    json!({
        "uuid": "{deadbeef-0000-0000-0000-000000000000}",
        "status": status,
        "ip_configured": ip,
        "name": VM_NAME,
    })
}

/// A cold boot, exactly as a Windows 11 guest reports one (measured on Parallels
/// 26.4): stopped, then running with no address, then a link-local IPv6, then the
/// Parallels ULA, then a link-local IPv4, and only then the DHCP lease. Every
/// stage but the last has been taken for the address at some point.
pub fn cold_boot() -> Vec<Value> {
    vec![
        phase("stopped", "-"),
        phase("running", "-"),
        phase("running", "fe80::bcca:2118:95a7:5e25"),
        phase("running", "fdb2:2c26:f4e4:0:357:80a2:89e0:6574"),
        phase("running", "169.254.96.137"),
        phase("running", LEASE),
    ]
}

/// A guest that is already up, and stays up.
pub fn running() -> Vec<Value> {
    vec![phase("running", LEASE)]
}

/// A guest command that runs for about `secs` seconds and then exits 0.
///
/// It runs on whatever OS the *test* is running on: the guests in these scenarios
/// are Windows in the sense that matters — the transport is `prlctl exec`, which
/// is what reaches the fake — but the agent answering is this machine's own.
/// Several arguments, so the arity rule spawns them as given rather than handing
/// a script to a shell that is not there.
pub fn sleeps_for(secs: u32) -> Vec<String> {
    let argv: Vec<&str> = if cfg!(windows) {
        // No `sleep` on Windows, and `timeout` refuses a redirected stdin.
        vec!["cmd", "/C", "ping"]
    } else {
        vec!["sh", "-c", "sleep"]
    };
    let tail = if cfg!(windows) {
        format!("-n {} 127.0.0.1 >NUL", secs + 1)
    } else {
        secs.to_string()
    };
    let mut out: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let last = out.pop().expect("a command");
    out.push(format!("{last} {tail}"));
    out
}

pub struct Fake {
    dir: tempfile::TempDir,
    alias: String,
}

/// What a `vm` run did.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Fake {
    /// A workspace whose config names one VM, under `alias`.
    ///
    /// Always a *windows* guest: its console probes ride `prlctl exec`, and so
    /// reach the fake, where a linux guest's would ride ssh. The guest OS under
    /// test is the transport, not the operating system.
    pub fn new(alias: &str) -> Fake {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                r#"
[vm.{alias}]
parallels_name = "{VM_NAME}"
os = "windows"
user = "tester"
work_root = "~/work"
"#
            ),
        )
        .expect("a config");

        // The ssh key doctor looks for. It is never used to authenticate
        // anything — the fake ssh answers whatever is asked of it — but a
        // *missing* key is a real finding, and a test's temp home has no key in
        // it unless one is put there.
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).expect("an .ssh dir");
        std::fs::write(ssh.join("id_ed25519"), "not a key\n").expect("an ssh key");

        Fake {
            dir,
            alias: alias.to_string(),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// The guest these runs will meet, and nothing else: ssh refuses, and the
    /// odd incidental prlctl call a command makes on its way past is answered
    /// blandly. A test that wants any of that different passes its own rules.
    pub fn guest(&self, phases: &[Value]) {
        self.scenario(phases, &[]);
    }

    /// The guest, plus rules that override it — an `exec` that answers, a
    /// `resume` that fails. Rules are matched before the guest, in order.
    pub fn scenario(&self, phases: &[Value], rules: &[Value]) {
        let mut all: Vec<Value> = rules.to_vec();
        all.extend(self.defaults());
        let body = json!({ "phases": phases, "rules": all });
        std::fs::write(
            self.dir.path().join("scenario.json"),
            serde_json::to_string_pretty(&body).expect("a scenario"),
        )
        .expect("scenario.json");
    }

    /// The calls that are never the point of a test, answered so they cannot be:
    /// the snapshot listing doctor takes on its way past, and ssh, which must
    /// fail *instantly* and locally rather than spending five seconds finding
    /// out that an address belongs to nobody.
    fn defaults(&self) -> Vec<Value> {
        vec![
            json!({ "match_prefix": ["snapshot-list"], "responses": [{ "stdout": "" }] }),
            json!({
                "match_prefix": ["-o", "BatchMode=yes"],
                "responses": [{ "exit": 255, "stderr": "ssh: connect to host port 22: Connection refused" }]
            }),
        ]
    }

    /// A `prlctl <verb> …` that fails, the way Parallels refuses one.
    pub fn rule_fails(&self, verb: &str, stderr: &str) -> Value {
        json!({ "match_prefix": [verb], "responses": [{ "exit": 1, "stderr": stderr }] })
    }

    /// An ssh command recognised by what it asks the guest to *run* — every one
    /// of them starts with the same dozen `-o` options, so the tail is the only
    /// thing that tells them apart.
    pub fn rule_ssh(&self, running: &str, stdout: &str) -> Value {
        json!({ "match_contains": [running], "responses": [{ "stdout": stdout }] })
    }

    pub fn rule_ssh_fails(&self, running: &str, stderr: &str) -> Value {
        json!({ "match_contains": [running], "responses": [{ "exit": 1, "stderr": stderr }] })
    }

    /// Every check `vm doctor <alias>` makes of a *healthy* guest, answered the
    /// way a healthy guest answers it. A test that wants one thing broken passes
    /// its own rule for that one thing — rules are matched in order, so the first
    /// match wins and this stays the backdrop.
    pub fn healthy_guest(&self) -> Vec<Value> {
        let version = serde_json::json!({
            "binary": env!("CARGO_PKG_VERSION"),
            "proto": vm::proto::PROTO_VERSION,
        })
        .to_string();
        vec![
            // `_version` is asked over both channels — ssh, and the Windows
            // console — and one answer serves both.
            self.rule_ssh("_version", &version),
            self.rule_ssh("git --version", "git version 2.51.0"),
            self.rule_ssh("mkdir -p", ""),
            self.rule_ssh("claude", "ok"),
            self.rule_ssh("_idle", "3600000"),
            // The console session: `prlctl exec … whoami` answers with the user
            // who owns the desktop, and doctor checks it is the config's user —
            // an exec that lands as somebody else sees somebody else's checkout.
            self.rule_ssh("whoami", "WINBOX\\tester"),
            // The bare reachability probe (`ssh … true`) — last, so the more
            // specific commands above claim their own calls first.
            self.rule_ssh("true", ""),
        ]
    }

    /// Make the workspace a git repo, and create the checkout the guest command
    /// will run in. What a `--no-sync` exec needs and no more: the repo is how vm
    /// knows where it is, and the checkout is where the command lands.
    pub fn with_repo(&self) -> std::path::PathBuf {
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(self.dir.path())
                .args(args)
                .output()
                .expect("git runs")
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@local"]);

        // `~/work/<repo>` in the guest — and the guest's home is this temp dir.
        let checkout = self
            .dir
            .path()
            .join("work")
            .join(self.dir.path().file_name().expect("a repo name"));
        std::fs::create_dir_all(&checkout).expect("the guest checkout");
        checkout
    }

    /// `prlctl exec` answered by the *real* vm agent, run locally: the request,
    /// the heartbeat and the exit code all genuine, with only Parallels replaced.
    /// This is what makes the Windows console transport testable.
    pub fn rule_exec_passthrough(&self) -> Value {
        json!({ "match_prefix": ["exec"], "responses": [{ "exec_passthrough": true }] })
    }

    /// `prlctl exec` that succeeds, saying nothing.
    pub fn rule_exec_ok(&self) -> Value {
        json!({ "match_prefix": ["exec"], "responses": [{ "exit": 0 }] })
    }

    /// `prlctl exec` that answers with `stdout` — the shape of an idle probe.
    pub fn rule_exec_says(&self, stdout: &str) -> Value {
        json!({ "match_prefix": ["exec"], "responses": [{ "stdout": stdout }] })
    }

    /// Run `vm …` against this fake, and wait for it.
    pub fn vm(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().expect("vm runs");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// A `vm` invocation wired to this fake but not yet run — for the tests that
    /// need to hold one open (concurrency, locks).
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(VM_BIN);
        cmd.args(args)
            .current_dir(self.dir.path())
            .env("VM_CONFIG", self.dir.path().join("config.toml"))
            .env("VM_PRLCTL", FAKE_BIN)
            .env("VM_SSH", FAKE_BIN)
            .env("VM_TEST_TICK_MS", TICK_MS)
            .env("FAKE_PRLCTL_DIR", self.dir.path())
            .env("FAKE_PRLCTL_VM", VM_BIN)
            // Whatever vm looks for in a home directory, it finds this one — so
            // a check reads the same on a CI runner as on the machine that has a
            // reap job installed and an ssh key on disk.
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path());
        cmd
    }

    /// The VM's use-lock file, as if the last `vm` to touch this guest let go of
    /// it `minutes` ago. Idle time is measured from that file's mtime and from
    /// nothing else, so this is what "nobody has used this VM in an hour" *is* —
    /// and reap's `try_exclusive` deliberately does not touch the mtime, so
    /// asking cannot reset the clock being asked about.
    pub fn last_used_minutes_ago(&self, minutes: u64) {
        let locks = self.dir.path().join("locks");
        std::fs::create_dir_all(&locks).expect("a lock dir");
        let path = locks.join(&self.alias);
        let file = std::fs::File::create(&path).expect("a lock file");
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(minutes * 60);
        file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(when)
                .set_modified(when),
        )
        .expect("backdating the lock");
    }

    /// Every prlctl (and ssh) invocation the runs made, in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The calls whose argv starts with `prefix` — `["stop"]`, `["exec", VM_NAME]`.
    pub fn calls_starting_with(&self, prefix: &[&str]) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| call.len() >= prefix.len() && call[..prefix.len()] == *prefix)
            .collect()
    }

    /// The journal vm kept — which, for a reap sweep, is the only thing it leaves
    /// behind at all.
    pub fn journal(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log").join("vm.log")).unwrap_or_default()
    }
}
