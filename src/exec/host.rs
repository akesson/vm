use crate::config::{Config, GuestOs, VmConfig};
use crate::exit::usage;
use crate::guest_env::{ActiveEnv, GuestEnv};
use crate::proto::{ExecRequest, PROTO_VERSION};
use crate::{commands, mapping, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::time::Instant;

/// Lib-side mirror of the CLI exec flags.
pub struct ExecOptions {
    pub no_sync: bool,
    pub writeback: bool,
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
    reject_removed_flags(&opts.cmd)?;

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
    // Starts or resumes the VM if it is down, and says so — an exec never asks
    // the caller to bring a VM up first.
    let target = commands::bring_up(alias, vm)?;
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
    let req = ExecRequest {
        version: PROTO_VERSION,
        argv: build_argv(&opts.cmd, &genv, vm.os),
        env,
        cwd: cwd.clone(),
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

/// `--shell` was replaced by the arity rule (see [`build_argv`]). It is not a
/// flag any more, and `cmd` is `trailing_var_arg`, so a leftover one in an old
/// script is *swallowed into the command* — the guest would hunt for a binary
/// called `--shell` and come back with a 127, having already resumed a VM and
/// run a sync to get there. Nothing can legitimately start with it, so refuse up
/// front and say what replaced it: a dropped flag that quietly changes what runs
/// is precisely the failure this codebase does not accept (cf. `vm claude`'s
/// misplaced-flag check).
fn reject_removed_flags(cmd: &[String]) -> Result<()> {
    if cmd.first().is_some_and(|first| first == "--shell") {
        // clap stopped parsing at `--shell`, so the `--` that followed it is
        // sitting in the command too — drop it, or the suggested fix would read
        // back `-- '-- echo hi'`.
        let rest = cmd[1..].iter().skip_while(|a| *a == "--");
        let script = rest.cloned().collect::<Vec<_>>().join(" ");
        return Err(usage(format!(
            "`--shell` no longer exists — a command's *form* now says whether it is a script.\n  \
             Pass the script as ONE argument and the guest's shell runs it: \
             vm exec <alias> -- '{script}'\n  \
             Several arguments still run exactly as given, with no shell involved."
        )));
    }
    Ok(())
}

/// The exact argv the guest will exec, composed here on the host — the guest
/// never interprets a request, it only spawns it. Two things go into it:
///
/// **The arity rule** decides what the command *is*, the way a Dockerfile's
/// `RUN` does. Several arguments are an argv, exec'd as given: they reach the
/// guest byte-identical (JSON, no quoting layer anywhere), so `grep 'a|b' f`
/// keeps its regex. **Exactly one** argument is a script, handed to the guest's
/// own shell — `sh -c`, or `cmd /C` on Windows, taken from the target's config
/// and never from the host's `cfg!` — so `'cd src && cargo test'` gets pipes,
/// builtins and `&&`.
///
/// The rule counts arguments; it never reads them. Content cannot decide this:
/// the `|` in `grep 'a|b' f` and the one in `echo hi | wc` are the same byte
/// with opposite meanings, so sniffing would fix one case by breaking the other.
/// [`super::advise`] carries that guesswork instead, where being wrong costs a
/// line of stderr rather than a wrong command.
///
/// **The guest env's wrap** (mise: `mise exec --`) then goes in front, so the
/// checkout's tools resolve — in front of the *shell*, not of the script's first
/// word. `mise exec -- sh -c '<script>'` runs the whole script inside the
/// environment; prepended into the script instead, mise would try to exec `cd`
/// as a binary, everything past the first pipe would escape the environment, and
/// `exit 42` would come back as mise's own exit code of one.
fn build_argv(cmd: &[String], genv: &ActiveEnv, guest_os: GuestOs) -> Vec<String> {
    let mut argv: Vec<String> = genv.wrap().iter().map(|s| s.to_string()).collect();
    match cmd {
        [script] => {
            let (bin, flag) = match guest_os {
                GuestOs::Windows => ("cmd", "/C"),
                GuestOs::Linux | GuestOs::Macos => ("sh", "-c"),
            };
            argv.extend([bin.to_string(), flag.to_string(), script.clone()]);
        }
        exec_form => argv.extend(exec_form.iter().cloned()),
    }
    argv
}

/// Render an argv for the `$ …` breadcrumb, which is a contract: it always shows
/// the literal command the guest runs, never a paraphrase of what was typed —
/// that is how a caller (or an agent driving vm) sees which form the arity rule
/// picked and what the wrap did to it. Elements stay separate strings all the way
/// to the guest, so one holding shell syntax (a script: `cd src && pwd`) must be
/// shown quoted — joined bare it would read as though the `&&` split the command,
/// which is exactly what it does *not* do.
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
///
/// The arity rule applies unchanged, so one task line means the same thing on
/// both paths. Its shell comes out the same too: native runs only when the host
/// already *is* the target os, so the host's shell is the very one the guest
/// path would have picked.
fn run_native(opts: &ExecOptions) -> Result<i32> {
    let env = resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    let no_wrap = crate::guest_env::resolve(Some(GuestEnv::None), std::path::Path::new("."));
    let argv = build_argv(&opts.cmd, &no_wrap, GuestOs::current());
    // Composed first, printed second: the breadcrumb owes the reader the command
    // that actually runs, here as much as in a guest.
    eprintln!(
        "vm ▸ native ({}) ▸ $ {}",
        GuestOs::current().as_str(),
        render_argv(&argv)
    );
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.envs(&env);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {:?} natively", argv[0]))?;
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

    /// Minimal ExecOptions for the native tests. Those run `sh`, so they are
    /// unix-only; the helper carries the same gate or it is dead code — and
    /// under `-D warnings`, a build failure — on the Windows runner.
    #[cfg(unix)]
    fn opts(cmd: &[&str]) -> ExecOptions {
        ExecOptions {
            no_sync: false,
            writeback: false,
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

    /// A repo root with no marker at all — no wrap.
    fn plain_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ── The arity rule: several arguments are an argv ─────────────────────────

    #[test]
    fn several_arguments_are_execd_as_given() {
        let tmp = plain_root();
        let argv = build_argv(
            &s(&["cargo", "test"]),
            &detected(tmp.path()),
            GuestOs::Linux,
        );
        assert_eq!(argv, s(&["cargo", "test"]));
    }

    #[test]
    fn exec_form_keeps_shell_syntax_literal() {
        // The reason the rule counts arguments instead of reading them: this `|`
        // is a regex alternation and must reach grep untouched. No shell, ever.
        let tmp = plain_root();
        let argv = build_argv(
            &s(&["grep", "a|b", "src/lib.rs"]),
            &detected(tmp.path()),
            GuestOs::Linux,
        );
        assert_eq!(argv, s(&["grep", "a|b", "src/lib.rs"]));
    }

    #[test]
    fn exec_form_gets_the_guest_envs_wrap_in_front() {
        let tmp = mise_root();
        let argv = build_argv(
            &s(&["cargo", "test"]),
            &detected(tmp.path()),
            GuestOs::Linux,
        );
        assert_eq!(argv, s(&["mise", "exec", "--", "cargo", "test"]));
    }

    // ── The arity rule: a single argument is a script ─────────────────────────

    #[test]
    fn a_single_argument_is_run_by_the_guests_shell() {
        // No guest env in play: the host still composes the shell, so the guest
        // has nothing to interpret — it only ever execs an argv.
        let tmp = plain_root();
        let argv = build_argv(
            &s(&["cd src && pwd"]),
            &detected(tmp.path()),
            GuestOs::Linux,
        );
        assert_eq!(argv, s(&["sh", "-c", "cd src && pwd"]));
    }

    #[test]
    fn the_wrap_goes_around_the_shell_not_its_first_word() {
        // The bug this guards: with the wrap prepended *inside* the script, mise
        // would try to exec the `cd` builtin as a binary, anything past a pipe
        // would escape the environment, and `exit 42` would come back as mise's
        // own exit 1. The whole script must run inside the env.
        let tmp = mise_root();
        let argv = build_argv(
            &s(&["cd src && pwd"]),
            &detected(tmp.path()),
            GuestOs::Linux,
        );
        assert_eq!(
            argv,
            s(&["mise", "exec", "--", "sh", "-c", "cd src && pwd"])
        );
    }

    #[test]
    fn the_script_shell_is_the_guests_not_the_hosts() {
        // The host may be macOS while the guest is Windows: the shell comes from
        // the target's configured os, never from a cfg!() on the host.
        let tmp = mise_root();
        let argv = build_argv(&s(&["echo hi"]), &detected(tmp.path()), GuestOs::Windows);
        assert_eq!(argv, s(&["mise", "exec", "--", "cmd", "/C", "echo hi"]));

        let plain = plain_root();
        let argv = build_argv(&s(&["echo hi"]), &detected(plain.path()), GuestOs::Windows);
        assert_eq!(argv, s(&["cmd", "/C", "echo hi"]));
    }

    #[test]
    fn a_single_argument_with_no_shell_syntax_is_still_a_script() {
        // One rule, no carve-outs: `sh -c ls` and exec'ing `ls` are the same run,
        // so there is nothing to gain from a metacharacter exception — and a rule
        // with no conditions is one nobody has to remember the conditions of.
        let tmp = plain_root();
        let argv = build_argv(&s(&["ls"]), &detected(tmp.path()), GuestOs::Linux);
        assert_eq!(argv, s(&["sh", "-c", "ls"]));
    }

    #[test]
    fn guest_env_none_beats_detection_in_both_forms() {
        let tmp = mise_root();
        let genv = crate::guest_env::resolve(Some(GuestEnv::None), tmp.path());
        assert_eq!(
            build_argv(&s(&["cargo", "test"]), &genv, GuestOs::Linux),
            s(&["cargo", "test"])
        );
        assert_eq!(
            build_argv(&s(&["exit 42"]), &genv, GuestOs::Linux),
            s(&["sh", "-c", "exit 42"])
        );
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

    // ── The removed --shell flag ──────────────────────────────────────────────

    #[test]
    fn a_leftover_shell_flag_is_a_usage_error_that_teaches_the_new_form() {
        let err = reject_removed_flags(&s(&["--shell", "cd src && cargo test"])).unwrap_err();
        assert!(
            err.downcast_ref::<crate::exit::UsageError>().is_some(),
            "an obsolete flag is the caller's invocation, not a transient fault"
        );
        let msg = err.to_string();
        // The error hands back the exact command that replaces the old one.
        assert!(
            msg.contains("vm exec <alias> -- 'cd src && cargo test'"),
            "{msg}"
        );
    }

    #[test]
    fn the_suggested_fix_drops_the_separator_clap_left_behind() {
        // `vm exec lin --shell -- 'echo hi'` reaches us as ["--shell", "--",
        // "echo hi"]: clap stops parsing at the unknown flag, so its own `--`
        // rides along. Suggesting `-- '-- echo hi'` back would be worse than
        // useless — it is the very command that just failed.
        let err = reject_removed_flags(&s(&["--shell", "--", "echo hi"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("vm exec <alias> -- 'echo hi'"), "{msg}");
        assert!(!msg.contains("'-- echo hi'"), "{msg}");
    }

    #[test]
    fn shell_as_an_ordinary_argument_is_untouched() {
        // Only the *first* element can be the stale flag; `--shell` further along
        // is an argument to the command, and none of vm's business.
        assert!(reject_removed_flags(&s(&["echo", "--shell"])).is_ok());
        assert!(reject_removed_flags(&s(&["cargo", "test", "--", "--shell"])).is_ok());
    }

    // ── The native path obeys the same rule ───────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn run_native_execs_an_exec_form_command() {
        assert_eq!(run_native(&opts(&["sh", "-c", "exit 3"])).unwrap(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn run_native_runs_a_single_argument_as_a_script() {
        // Same arity rule as the guest path, so one task line means one thing on
        // both — including the exit code coming back from the script, not the
        // shell that hosted it.
        assert_eq!(run_native(&opts(&["exit 7"])).unwrap(), 7);
        assert_eq!(run_native(&opts(&["true && exit 5"])).unwrap(), 5);
    }
}
