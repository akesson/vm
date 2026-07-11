use crate::config::{Config, GuestOs, VmConfig};
use crate::exit::usage;
use crate::guest_env::{ActiveEnv, GuestEnv};
use crate::proto::{ExecRequest, PROTO_VERSION};
use crate::{commands, mapping, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Lib-side mirror of the CLI exec flags.
pub struct ExecOptions {
    pub no_sync: bool,
    pub writeback: bool,
    pub shell: bool,
    /// Snapshot the VM, run, then roll it back. Takes the VM exclusively.
    pub with_snapshot: bool,
    /// Run natively when the host OS already matches the target's os, instead
    /// of routing to the VM. See [`exec`].
    pub or_native: bool,
    /// Explicit `--guest-env` choice; `None` auto-detects from the repo root.
    pub guest_env: Option<GuestEnv>,
    /// `NAME=value` / bare `NAME` specs from `-e`; resolved against the host
    /// environment when the request is built.
    pub env: Vec<String>,
    pub cmd: Vec<String>,
}

/// `vm exec <alias> -- cmd…`: sync, run in the guest checkout, propagate exit.
/// The target is a VM alias from the machine config, and by default the command
/// always runs in that VM — even when its os is the host's own — so a `vm`
/// invocation never silently runs on the host.
///
/// `--or-native` opts into the one exception: when the host OS already matches
/// the target's os, the command runs natively (no VM, no sync) with a loud
/// banner so the location is never ambiguous. This is what lets one task drive
/// a guest on a dev host and run in place on a CI runner that is already the
/// target OS — where there is neither Parallels nor a machine config. To keep
/// that CI path working without config, a target *literally named* for an OS
/// (`windows`/`linux`/`macos`) is matched against the host before the config is
/// even loaded; any other alias needs the config to learn its os (so it only
/// takes the native path on a configured host).
pub fn exec(target: &str, opts: &ExecOptions) -> Result<i32> {
    // Validate `-e` before touching anything: a typo'd spec must not first cost
    // a VM resume, a sync, and — under --with-snapshot — a snapshot and its
    // rollback. Cheap and pure, so the real resolution below just redoes it.
    resolve_env(&opts.env, |name| std::env::var(name).ok())?;

    // Config-free native fast path: an os-literal target we can match against
    // the host without the machine config (CI runners have neither config nor
    // Parallels, so loading it would fail before we could decide to go native).
    if opts.or_native
        && let Some(os) = GuestOs::parse(target)
        && os == GuestOs::current()
    {
        return run_native(opts);
    }
    let cfg = Config::load()?;
    let vm = cfg.get(target)?;
    let alias = target;
    // Alias on a matching host: honor --or-native now that config has told us
    // the alias's os.
    if opts.or_native && vm.os == GuestOs::current() {
        return run_native(opts);
    }
    if opts.with_snapshot {
        // Takes the VM exclusively and rolls it back around the run.
        return commands::with_snapshot(alias, vm, opts);
    }
    // Registers this run as a use of the VM: stop/--with-snapshot/reap keep
    // their hands off until it finishes. Blocks briefly if one of those is
    // mid-flight right now.
    let _use = crate::lock::shared(alias)?;
    exec_in(alias, vm, opts)
}

/// The body of an exec against a resolved VM, without taking the VM's use lock.
/// `exec` wraps it in a shared lock; `commands::with_snapshot` calls it while
/// already holding the exclusive one (where a shared lock would deadlock).
pub fn exec_in(alias: &str, vm: &VmConfig, opts: &ExecOptions) -> Result<i32> {
    prl::ensure_running(&vm.parallels_name)?;
    prl::wait_for_ip(&vm.parallels_name, Duration::from_secs(90))?;
    let target = commands::ssh_target(vm)?;
    let repo = mapping::RepoLocation::discover()?;

    // The guest env (mise, …) that sets up and wraps this run: an explicit
    // `--guest-env`, else detected from the repo root. Announced before it does
    // anything, so a detected env is never a silent behavior change.
    let genv = crate::guest_env::resolve(opts.guest_env, &repo.root);
    genv.announce(alias);

    let base = if opts.no_sync {
        None
    } else {
        Some(commands::sync_repo(alias, vm, &target)?)
    };

    // The guest env's one-time setup (mise: `mise trust`) the first time this
    // checkout exists — and after `vm clean`. No-op otherwise; also covers a
    // `--no-sync` run against a checkout that never ran it.
    commands::first_sync_hook(alias, vm, &target, &repo, &genv)?;

    let cwd = mapping::guest_cwd(&vm.work_root, &repo.name, &repo.rel)?;
    let env = resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    let (argv, shell) = build_argv(&opts.cmd, &genv, vm.os, opts.shell);
    let req = ExecRequest {
        version: PROTO_VERSION,
        argv,
        env,
        cwd: cwd.clone(),
        shell,
    };

    eprintln!(
        "vm ▸ {alias} ({}) ▸ {cwd} ▸ $ {}",
        vm.parallels_name,
        render_argv(&req.argv)
    );
    let started = Instant::now();

    let mut child = agent_exec_command(vm, &target)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn the exec transport")?;
    let mut request_line = serde_json::to_string(&req)?;
    request_line.push('\n');
    // Take stdin OUT of the Child: Child::wait() closes child.stdin before
    // blocking, and this pipe is the liveness channel — it must stay open for
    // the whole run. If this process dies (Ctrl-C, kill), the pipe closes,
    // the agent's watcher sees EOF, and the guest kills the child tree.
    let mut agent_stdin = child.stdin.take().expect("piped stdin");
    agent_stdin.write_all(request_line.as_bytes())?;
    let status = child.wait().context("waiting on the exec transport")?;
    drop(agent_stdin);
    let code = match status.code() {
        Some(code) => code,
        // No exit code means the transport itself was killed by a signal while
        // this process survived (the connection dropped, or ssh/prlctl was
        // signalled) — a vm infra failure, not a result from the guest command.
        None => bail!(
            "the exec transport to {alias} was killed before the guest reported an \
             exit status — the connection dropped, or ssh/prlctl was signalled"
        ),
    };

    // 127 now doubles as the guest reporting command-not-found (see
    // exec/guest.rs), so keep the hint suggestive rather than assertive.
    if code == 127 {
        eprintln!(
            "vm ▸ {alias} ▸ exit 127 — command not found in the guest \
             (or the agent is missing — try `vm deploy {alias}`)"
        );
    }

    if opts.writeback
        && let Some(base) = &base
    {
        // 255 is ambiguous — an ssh transport failure, or a guest command that
        // itself exited 255 — so the guest's tree can't be trusted to be the
        // result of a completed run. Skip the writeback, but say so: a silently
        // missing diff would look like the command simply changed nothing.
        if code == 255 {
            eprintln!(
                "vm ▸ {alias} ▸ writeback skipped — exit 255 may be a dropped connection \
                 rather than the command's own status, so the guest tree is not trusted"
            );
        } else {
            writeback(alias, vm, &target, &repo, base)?;
        }
    }

    eprintln!(
        "vm ▸ {alias} ▸ exit {code} ▸ {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(code)
}

/// The argv (and shell mode) sent to the guest: the user's command with the
/// guest env's wrap prefix (mise: `mise exec --`) in front, so the checkout's
/// tools resolve. Prepending in argv space — not a shell string — is
/// quoting-safe: the elements ride to the guest as JSON.
///
/// `--shell` needs the wrap to go *around the shell*, not in front of its first
/// word. The guest runs a `--shell` request by joining argv into one script, so
/// a wrap merely prepended there would produce `sh -c "mise exec -- cd src &&
/// pwd"`, where mise cannot exec a shell builtin, everything past the first pipe
/// segment escapes the environment, and `exit 42` comes back as mise's own exit
/// code of one. So when a wrap is active the host composes the guest's shell
/// invocation itself — it knows the guest's OS from the config — and sends a
/// plain argv, `mise exec -- sh -c "<script>"`, putting the script inside the env.
fn build_argv(
    cmd: &[String],
    genv: &ActiveEnv,
    guest_os: GuestOs,
    shell: bool,
) -> (Vec<String>, bool) {
    let wrap = genv.wrap();
    if wrap.is_empty() {
        // No env to wrap: leave `--shell` to the guest, exactly as before.
        return (cmd.to_vec(), shell);
    }
    let mut argv: Vec<String> = wrap.iter().map(|s| s.to_string()).collect();
    if shell {
        let (bin, flag) = match guest_os {
            GuestOs::Windows => ("cmd", "/C"),
            GuestOs::Linux | GuestOs::Macos => ("sh", "-c"),
        };
        argv.extend([bin.to_string(), flag.to_string(), cmd.join(" ")]);
        return (argv, false);
    }
    argv.extend(cmd.iter().cloned());
    (argv, false)
}

/// Render an argv for the `$ …` breadcrumb. Elements are separate strings all
/// the way to the guest, so an element holding shell syntax (`--shell`'s script:
/// `cd src && pwd`) must be shown quoted — joined bare it would read as though
/// the `&&` split the command, which is exactly what it does *not* do.
fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty()
                || a.contains(|c: char| c.is_whitespace() || "\"'`$&|;<>()*?".contains(c))
            {
                format!("'{}'", a.replace('\'', r"'\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--or-native` on a host that already is the target OS: run the command in
/// place — current dir, inherited stdio, host environment plus any `-e` vars —
/// with no VM, sync, or guest-env wrap (natively the environment is already
/// whatever launched the command). A loud banner records that this run stayed
/// on the host, so `--or-native` never hides where a command executed.
fn run_native(opts: &ExecOptions) -> Result<i32> {
    let env = resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    eprintln!(
        "vm ▸ native ({}) ▸ $ {}",
        GuestOs::current().as_str(),
        render_argv(&opts.cmd)
    );
    let mut cmd = if opts.shell {
        let joined = opts.cmd.join(" ");
        if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", &joined]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &joined]);
            c
        }
    } else {
        let mut c = std::process::Command::new(&opts.cmd[0]);
        c.args(&opts.cmd[1..]);
        c
    };
    cmd.envs(&env);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {:?} natively", opts.cmd[0]))?;
    Ok(status.code().unwrap_or(1))
}

/// Resolve `-e` specs into an explicit NAME→value map for the guest process.
/// `NAME=value` sets the variable directly (the value may be empty or itself
/// contain `=`). Bare `NAME` forwards the host's current value and errors if
/// it is unset — an explicit request gets explicit feedback. On a duplicate
/// name the last spec wins.
///
/// A bad spec is the caller's own invocation (a typo, a variable they forgot to
/// export), so it is a usage error: retrying it will never help.
fn resolve_env(
    specs: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for spec in specs {
        match spec.split_once('=') {
            Some(("", _)) => return Err(usage(format!("-e {spec}: empty variable name"))),
            Some((name, value)) => {
                env.insert(name.to_string(), value.to_string());
            }
            None => {
                let value = lookup(spec).ok_or_else(|| {
                    usage(format!(
                        "-e {spec}: not set on host (use -e {spec}=value to set it explicitly)"
                    ))
                })?;
                env.insert(spec.clone(), value);
            }
        }
    }
    Ok(env)
}

/// The host process that carries an ExecRequest to the guest agent. Unix
/// guests go over ssh. Windows goes through `prlctl exec --current-user`
/// instead: sshd puts children in session 0 on a non-interactive window
/// station, where UIA (and any GUI automation) sees an empty desktop, while
/// Parallels Tools injects into the console session. Same agent, same
/// protocol — stdout/stderr stream and stdin stays the liveness channel
/// either way.
fn agent_exec_command(vm: &VmConfig, target: &ssh::SshTarget) -> std::process::Command {
    match vm.os {
        GuestOs::Windows => {
            let mut cmd = prl::exec_console(&vm.parallels_name);
            // Through cmd.exe so %USERPROFILE% in the agent path expands.
            cmd.args([
                "cmd",
                "/c",
                &format!("{} _exec", commands::agent_console_path(vm)),
            ]);
            cmd
        }
        GuestOs::Linux | GuestOs::Macos => {
            let mut cmd = ssh::ssh_command(target);
            cmd.arg(commands::agent_path(vm)).arg("_exec");
            cmd
        }
    }
}

fn writeback(
    alias: &str,
    vm: &VmConfig,
    target: &ssh::SshTarget,
    repo: &mapping::RepoLocation,
    base: &sync::Snapshot,
) -> Result<()> {
    // Same critical section as the forward sync: the guest's writeback
    // snapshot index and refs/sync/writeback, plus the patch applied back onto
    // the host tree. Not held across the guest command run in between (parallel
    // execs on one VM must stay parallel) — only around sync and writeback.
    let _sync_guard = sync::host::lock_sync(&repo.root, alias)?;
    let guest_repo = mapping::guest_repo_path(&vm.work_root, &repo.name);
    let json = commands::agent_call(vm, target, &["_tree", "--repo", &guest_repo])?;
    let wb: sync::Snapshot = serde_json::from_str(&json).context("parsing _tree reply")?;
    let url = mapping::ssh_remote_url(&target.user, &target.host, &guest_repo);
    let applied =
        sync::host::apply_writeback(&repo.root, &url, base, &wb, Some(&ssh::git_ssh_command()))?;
    if applied {
        eprintln!("vm ▸ {alias} ▸ writeback applied to host tree");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host environment stub, so the tests never touch the real process env
    /// (mutating it is `unsafe` on edition 2024 and races other tests).
    fn host<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_assignment_sets_the_value() {
        let env = resolve_env(&s(&["FOO=bar"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn value_may_contain_equals_signs() {
        let env = resolve_env(&s(&["FOO=a=b"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("a=b"));
    }

    #[test]
    fn empty_value_is_allowed() {
        let env = resolve_env(&s(&["FOO="]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some(""));
    }

    #[test]
    fn bare_name_forwards_the_host_value() {
        let env = resolve_env(&s(&["FOO"]), host(&[("FOO", "from-host")])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("from-host"));
    }

    #[test]
    fn bare_name_unset_on_host_is_an_error() {
        let err = resolve_env(&s(&["FOO"]), host(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("FOO"), "{err}");
        assert!(err.contains("not set on host"), "{err}");
        assert!(err.contains("FOO=value"), "{err}");
    }

    #[test]
    fn empty_name_is_an_error() {
        let err = resolve_env(&s(&["=value"]), host(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty variable name"), "{err}");
    }

    #[test]
    fn a_bad_env_spec_is_a_usage_error_not_an_infra_one() {
        // Exit 2 ("fix your invocation"), never 125 ("transient, retry me") — a
        // typo'd spec is not going to fix itself on a second attempt.
        for spec in [&["=value"][..], &["NOPE_NOT_SET_ANYWHERE"][..]] {
            let err = resolve_env(&s(spec), host(&[])).unwrap_err();
            assert!(
                err.downcast_ref::<crate::exit::UsageError>().is_some(),
                "{spec:?} should be a usage error, got: {err:#}"
            );
        }
    }

    #[test]
    fn duplicate_name_takes_the_last_spec() {
        let env = resolve_env(&s(&["FOO=1", "FOO=2"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("2"));
    }

    /// Minimal ExecOptions for the native tests.
    fn opts(cmd: &[&str]) -> ExecOptions {
        ExecOptions {
            no_sync: false,
            writeback: false,
            shell: false,
            with_snapshot: false,
            or_native: false,
            guest_env: None,
            env: Vec::new(),
            cmd: s(cmd),
        }
    }

    /// The ActiveEnv a repo root yields, without a `--guest-env` override.
    fn detected(root: &std::path::Path) -> ActiveEnv {
        crate::guest_env::resolve(None, root)
    }

    /// A repo root with a mise marker, so detection activates the wrap.
    fn mise_root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mise.toml"), "[tools]\n").unwrap();
        tmp
    }

    #[test]
    fn build_argv_prepends_the_guest_envs_wrap() {
        let tmp = mise_root();
        let (argv, shell) = build_argv(
            &s(&["cargo", "test"]),
            &detected(tmp.path()),
            GuestOs::Linux,
            false,
        );
        assert_eq!(argv, s(&["mise", "exec", "--", "cargo", "test"]));
        assert!(!shell);
    }

    #[test]
    fn build_argv_without_a_guest_env_is_the_bare_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (argv, _) = build_argv(
            &s(&["cargo", "test"]),
            &detected(tmp.path()),
            GuestOs::Linux,
            false,
        );
        assert_eq!(argv, s(&["cargo", "test"]));
    }

    #[test]
    fn build_argv_honors_guest_env_none_over_detection() {
        let tmp = mise_root();
        let genv = crate::guest_env::resolve(Some(GuestEnv::None), tmp.path());
        let (argv, _) = build_argv(&s(&["cargo", "test"]), &genv, GuestOs::Linux, false);
        assert_eq!(argv, s(&["cargo", "test"]));
    }

    #[test]
    fn shell_mode_puts_the_wrap_around_the_shell_not_its_first_word() {
        // The bug this guards: prepending the wrap in argv space and letting the
        // guest join it into a script yields `sh -c "mise exec -- cd src && pwd"`
        // — mise cannot exec the `cd` builtin, and `exit 42` would come back as
        // mise's exit 1. The whole script must run *inside* the env instead.
        let tmp = mise_root();
        let (argv, shell) = build_argv(
            &s(&["cd src && pwd"]),
            &detected(tmp.path()),
            GuestOs::Linux,
            true,
        );
        assert_eq!(
            argv,
            s(&["mise", "exec", "--", "sh", "-c", "cd src && pwd"])
        );
        // Composed here, so the guest execs this argv directly rather than
        // re-joining it into a second shell.
        assert!(!shell);
    }

    #[test]
    fn shell_mode_uses_the_guests_shell_not_the_hosts() {
        // The host may be macOS while the guest is Windows — the shell is picked
        // from the target's os, never from cfg!(windows) here.
        let tmp = mise_root();
        let (argv, _) = build_argv(
            &s(&["echo hi"]),
            &detected(tmp.path()),
            GuestOs::Windows,
            true,
        );
        assert_eq!(argv, s(&["mise", "exec", "--", "cmd", "/C", "echo hi"]));
    }

    #[test]
    fn render_argv_quotes_elements_that_hold_shell_syntax() {
        // `$ mise exec -- sh -c cd src && pwd` would read as if the && ran
        // outside the wrap; it is one argument, and must look like one.
        assert_eq!(
            render_argv(&s(&["mise", "exec", "--", "sh", "-c", "cd src && pwd"])),
            "mise exec -- sh -c 'cd src && pwd'"
        );
        assert_eq!(render_argv(&s(&["cargo", "test"])), "cargo test");
    }

    #[test]
    fn shell_mode_without_a_guest_env_is_left_to_the_guest() {
        let tmp = tempfile::tempdir().unwrap();
        let (argv, shell) = build_argv(
            &s(&["exit 42"]),
            &detected(tmp.path()),
            GuestOs::Linux,
            true,
        );
        assert_eq!(argv, s(&["exit 42"]));
        assert!(shell, "the guest still runs it through its own shell");
    }

    #[cfg(unix)]
    #[test]
    fn run_native_propagates_the_exit_code() {
        let mut o = opts(&["sh", "-c", "exit 3"]);
        o.shell = false;
        assert_eq!(run_native(&o).unwrap(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn run_native_shell_mode_joins_argv() {
        let mut o = opts(&["exit", "7"]);
        o.shell = true;
        assert_eq!(run_native(&o).unwrap(), 7);
    }
}
