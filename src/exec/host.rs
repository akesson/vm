use crate::config::{Config, GuestOs, VmConfig};
use crate::exit::usage;
use crate::guest_env::{ActiveEnv, GuestEnv};
use crate::proto::{ExecRequest, HEARTBEAT_INTERVAL, PROTO_VERSION};
use crate::{commands, crumb, mapping, notice, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

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
    /// `--with-file` paths: gitignored files to force into the sync anyway.
    /// Resolved against the repo root (see [`commands::resolve_with_files`]).
    pub with_file: Vec<String>,
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
    // Validate `-e` and `--with-file` before touching anything: a typo'd spec
    // must not first cost a VM resume, a sync, and — under --with-snapshot — a
    // snapshot and its rollback. Cheap and pure, so the real resolution below
    // just redoes it.
    resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    with_files(opts)?;
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
    // From here the command runs in a guest, where vm's stdin does not follow
    // it — input piped into vm would be discarded without a word (and the run
    // may well *succeed*, so this cannot wait for a failure). Placed after the
    // --or-native returns: a native run inherits stdin and uses it normally.
    if let Some(note) = super::advise::stdin_note(stdin_source()) {
        notice!("vm ▸ note: {note}");
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
        let extra =
            commands::resolve_with_files(&opts.with_file, &std::env::current_dir()?, &repo.root)?;
        Some(commands::sync_repo(alias, vm, &target, &extra)?)
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
        // An exec's guest command gets the null device: vm's own stdin is the
        // liveness channel — request, then heartbeats — and never travels.
        // `vm run` is the one that sends a payload (see `super::run`).
        stdin: None,
        // The guest keeps the real budget. Only a test ever overrides it.
        heartbeat_timeout_ms: None,
    };

    crumb!(
        "vm ▸ {alias} ({}) ▸ {cwd} ▸ $ {}",
        vm.parallels_name,
        render_argv(&req.argv)
    );
    let started = Instant::now();

    let code = drive_agent(alias, agent_exec_command(vm, &target)?, &req)?;

    // A failed command in a repo whose `.env` never left the host: the likeliest
    // cause of the failure the caller is now staring at, and the one thing the
    // green sync breadcrumb above actively argues against. Said once, after the
    // fact, only when a sync ran — never on a healthy run.
    if code != 0
        && base.is_some()
        && let Some(note) =
            super::advise::unsynced_env_note(&unsynced_env_files(&repo.root, &opts.with_file))
    {
        notice!("vm ▸ note: {note}");
    }

    if opts.writeback
        && let Some(base) = &base
    {
        // 255 is ambiguous — an ssh transport failure, or a guest command that
        // itself exited 255 — so the guest's tree can't be trusted to be the
        // result of a completed run. Skip the writeback, but say so: a silently
        // missing diff would look like the command simply changed nothing.
        if code == 255 {
            notice!(
                "vm ▸ {alias} ▸ writeback skipped — exit 255 may be a dropped connection \
                 rather than the command's own status, so the guest tree is not trusted"
            );
        } else {
            writeback(alias, vm, &target, &repo, base)?;
        }
    }

    crumb!(
        "vm ▸ {alias} ▸ exit {code} ▸ {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(code)
}

/// Carry one request to the guest agent over an already-built transport
/// (ssh, or one of the two `prlctl exec` channels), stream its output, and
/// return the guest command's exit code. The single place vm talks to an agent
/// — [`exec_in`] and [`super::run::run`] differ in everything *around* this and
/// in nothing about it.
///
/// The open stdin pipe is the contract. It carries the request, and then a
/// keepalive byte every [`HEARTBEAT_INTERVAL`] for as long as the command runs.
/// The agent tears the guest's process tree down when that pipe closes — this
/// process died, Ctrl-C or kill — *or* when it falls silent for
/// [`crate::proto::HEARTBEAT_TIMEOUT`]. The silence half is the one that holds
/// over prlctl, where a dead host may close nothing the guest can ever see:
/// Parallels Tools can keep the guest's end of stdin open, which is how a killed
/// `vm run --elevated macos` used to leave its command running for as long as
/// anyone cared to watch (#21).
///
/// The pipe must therefore stay open across the wait — which is why the handle
/// is taken *out* of the `Child`, whose own `wait()` would close it, and given
/// to the heartbeat thread to hold. (Measured on Parallels 26.4: over the
/// prlctl channels a unix guest's nonzero exit code only comes back while that
/// pipe is still open, so this is load-bearing for the exit-code contract too,
/// not only for teardown.)
///
/// Exposed for `tests/host_agent.rs`, which drives it with a `vm _exec` transport
/// of its own: this is the one function both `vm exec` and `vm run` funnel
/// through, and the half of the liveness contract the agent's own tests cannot
/// see, because they *are* the agent.
#[doc(hidden)]
pub fn drive_agent(
    alias: &str,
    mut transport: std::process::Command,
    req: &ExecRequest,
) -> Result<i32> {
    let mut child = transport
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn the exec transport")?;
    let mut request_line = serde_json::to_string(req)?;
    request_line.push('\n');
    let mut agent_stdin = child.stdin.take().expect("piped stdin");
    agent_stdin.write_all(request_line.as_bytes())?;

    // The pulse. Detached, and the owner of the pipe from here on: it has to
    // outlive the `wait()` below — a pipe closed early would read as a dead
    // host — and nothing after the wait has any use for the handle. It ends by
    // failing, not by being told: when the transport exits, the read end goes
    // with it and the next write returns `BrokenPipe`. Leaning on a failed
    // write is only safe because Rust ignores SIGPIPE at startup (the same
    // reasoning [`super::guest::feed_stdin`] runs on). So the thread outlives
    // the command by at most one interval, asleep, holding a dead pipe.
    let interval = beat_interval(req);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            if agent_stdin.write_all(b"\n").is_err() {
                break;
            }
        }
    });

    let status = child.wait().context("waiting on the exec transport")?;
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
        notice!(
            "vm ▸ {alias} ▸ exit 127 — command not found in the guest \
             (or the agent is missing — try `vm deploy {alias}`)"
        );
    }
    Ok(code)
}

/// How often to beat, for a given silence budget.
///
/// Four beats to a budget. That ratio is the contract — enough that a stalled
/// scheduler or a loaded host can lose a beat, or two, without the agent
/// concluding vm is dead and killing a healthy build — and the fifteen seconds is
/// only what it works out to against the real minute.
///
/// So a request that shortens the budget shortens the pulse with it. The host
/// never shortens it; only a test does, to watch a minute's worth of contract
/// play out in a second ([`crate::proto::ExecRequest::heartbeat_timeout_ms`]) —
/// and a test whose agent gave up after half a second while the host beat every
/// fifteen would be testing nothing but its own impatience.
fn beat_interval(req: &ExecRequest) -> Duration {
    match req.heartbeat_timeout_ms {
        Some(ms) => Duration::from_millis(ms) / 4,
        None => HEARTBEAT_INTERVAL,
    }
}

/// Validate the `--with-file` paths up front, discarding the result — the real
/// resolution happens in [`exec_in`], against the same repo root, once the run
/// is actually going to sync.
///
/// The repo is only discovered when the flag was passed at all: `--or-native` on
/// a CI runner may not be inside a git repo (and does not need to be), so an
/// unused flag must not go looking for one.
fn with_files(opts: &ExecOptions) -> Result<()> {
    if opts.with_file.is_empty() {
        return Ok(());
    }
    let repo = mapping::RepoLocation::discover()?;
    commands::resolve_with_files(&opts.with_file, &std::env::current_dir()?, &repo.root)?;
    Ok(())
}

/// After a *failed* guest command, the gitignored env files sitting in the repo
/// root that the sync left behind — the ones that would explain a "VAR not set"
/// the caller is about to go hunting for in the wrong place.
///
/// Deliberately narrow. It runs only on a nonzero exit (a healthy run says
/// nothing and pays nothing — see [`super::advise`] on the silence budget), and
/// only `.env*` names qualify: they are the ones whose absence fails a build
/// while every breadcrumb reads green. Tracked files are excluded because they
/// *did* sync (the HEAD seed carries tracked-but-ignored files), as are files
/// the caller already passed to `--with-file` — a note about a file that
/// travelled would be a lie.
fn unsynced_env_files(repo_root: &std::path::Path, with_file: &[String]) -> Vec<String> {
    let git = sync::Git::in_dir(repo_root);
    let mut found: Vec<String> = std::fs::read_dir(repo_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".env"))
        .filter(|name| !with_file.iter().any(|w| w == name))
        .filter(|name| {
            // Ignored (so `add -A` skipped it) *and* untracked (so the HEAD seed
            // did not carry it either): only then is it truly not in the guest.
            // `check-ignore` exits 1 for "not ignored", which `out` reports as a
            // failure — the empty/error case is the one we want to drop anyway.
            let ignored = git
                .out(&["check-ignore", "--quiet", "--", name])
                .map(|_| true)
                .unwrap_or(false);
            let untracked = git
                .out(&["ls-files", "--error-unmatch", "--", name])
                .is_err();
            ignored && untracked
        })
        .collect();
    // read_dir order is filesystem order; the note must read the same every run.
    found.sort();
    found
}

/// What vm's own stdin is connected to, when it plainly carries input: a pipe
/// (`echo hi | vm …`) or a redirected regular file (`vm … < data.txt`). A
/// terminal and the null device are character devices and classify as `None` —
/// and fd 0 is one of those in every environment that runs vm routinely (a
/// shell, an agent harness, CI, cron), which is what keeps the resulting note
/// off healthy runs. See [`super::advise::stdin_note`].
///
/// Unix-only by construction — the vm host runs on macOS; in the Windows build
/// (the guest agent) this is always `None`.
pub(super) fn stdin_source() -> Option<super::advise::StdinSource> {
    #[cfg(not(unix))]
    return None;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        // fstat fd 0 via a duplicate: `File::from` takes ownership of the fd it
        // is given and would close the real stdin when dropped.
        let fd = std::os::fd::AsFd::as_fd(&std::io::stdin())
            .try_clone_to_owned()
            .ok()?;
        let ft = std::fs::File::from(fd).metadata().ok()?.file_type();
        if ft.is_fifo() {
            Some(super::advise::StdinSource::Piped)
        } else if ft.is_file() {
            Some(super::advise::StdinSource::Redirected)
        } else {
            None
        }
    }
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
pub(super) fn build_argv(cmd: &[String], genv: &ActiveEnv, guest_os: GuestOs) -> Vec<String> {
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
pub(super) fn render_argv(argv: &[String]) -> String {
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
///
/// An unspawnable argv[0] is classified exactly as [`super::guest`] classifies
/// it — 127 not-found, 126 not-executable — because `--or-native` is meant to be
/// a transparent swap, and a swap that changes what "command not found" means is
/// not one (#24). Only the exec form reaches that check: a script is resolved by
/// the shell [`build_argv`] wrapped it in, which reports its own codes and never
/// fails the spawn. That shell is the same one the guest path would have picked,
/// so both paths still answer alike — `sh`'s 127/126 on unix, and on Windows
/// cmd.exe's 1 for an unrecognized command.
fn run_native(opts: &ExecOptions) -> Result<i32> {
    let env = resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    let no_wrap = crate::guest_env::resolve(Some(GuestEnv::None), std::path::Path::new("."));
    let argv = build_argv(&opts.cmd, &no_wrap, GuestOs::current());
    // Composed first, printed second: the breadcrumb owes the reader the command
    // that actually runs, here as much as in a guest.
    crumb!(
        "vm ▸ native ({}) ▸ $ {}",
        GuestOs::current().as_str(),
        render_argv(&argv)
    );
    // Through [`super::command_for`], not a bare `.args()` spawn: on a Windows
    // host the script form composed `cmd /C …`, and cmd must be handed its
    // line verbatim or every `"` in the script arrives backslash-mangled.
    let mut cmd = super::command_for(&argv);
    cmd.envs(&env);
    let status = match cmd.status() {
        Ok(status) => status,
        // A command that isn't found or isn't executable is the *command's* own
        // result, not a vm failure — reported with the shell's own codes on the
        // Ok path, so it never lands on 125 ("vm itself failed, often transient,
        // retry"), which would send the reader hunting through vm's plumbing for
        // what is really a broken PATH. Any other spawn failure is infra.
        Err(err) => match super::spawn_failure(&err, &argv[0], native_path(&env).as_deref()) {
            Some(code) => return Ok(code),
            None => {
                return Err(err).with_context(|| format!("failed to run {:?} natively", argv[0]));
            }
        },
    };
    Ok(status.code().unwrap_or(1))
}

/// The PATH a native command was about to be searched on: an explicit `-e PATH=…`
/// if the caller passed one, else the one vm inherited and is about to hand down.
///
/// Both platforms search the *child's* PATH first — on unix std even gives up its
/// `posix_spawn` fast path when a modified PATH would otherwise be ignored — so an
/// override is what a not-found is a statement about, and what there is to report.
/// Windows then *also* falls back to vm's own PATH and the system directories
/// (measured: an `-e PATH=…` holding no `cargo` still spawns the one on vm's PATH),
/// which is why this is the PATH the command was handed rather than the last word
/// on where Win32 looked. It cannot mislead: the report only ever prints when the
/// command was found in none of them.
fn native_path(env: &BTreeMap<String, String>) -> Option<String> {
    super::path_override(env)
        .map(str::to_string)
        .or_else(|| std::env::var("PATH").ok())
}

/// Resolve `-e` specs into an explicit NAME→value map for the guest process.
/// `NAME=value` sets the variable directly (the value may be empty or itself
/// contain `=`). Bare `NAME` forwards the host's current value and errors if
/// it is unset — an explicit request gets explicit feedback. On a duplicate
/// name the last spec wins.
///
/// A bad spec is the caller's own invocation (a typo, a variable they forgot to
/// export), so it is a usage error: retrying it will never help.
pub(super) fn resolve_env(
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
pub(super) fn agent_exec_command(
    vm: &VmConfig,
    target: &ssh::SshTarget,
) -> Result<std::process::Command> {
    match vm.os {
        GuestOs::Windows => {
            // Through cmd.exe so %USERPROFILE% in the agent path expands.
            prl::exec_console(
                &vm.parallels_name,
                &[
                    "cmd",
                    "/c",
                    &format!("{} _exec", commands::agent_console_path(vm)),
                ],
            )
        }
        GuestOs::Linux | GuestOs::Macos => {
            let mut cmd = ssh::ssh_command(target);
            cmd.arg(commands::agent_path(vm)).arg("_exec");
            Ok(cmd)
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
        crumb!("vm ▸ {alias} ▸ writeback applied to host tree");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four beats to a budget, whatever the budget — including the real one.
    /// A pulse slower than the budget it is keeping alive would be a host that
    /// reports itself dead on a schedule.
    #[test]
    fn the_pulse_always_fits_four_beats_into_the_silence_budget() {
        let real = beat_interval(&request_with_budget(None));
        assert_eq!(real, HEARTBEAT_INTERVAL);
        assert_eq!(real * 4, crate::proto::HEARTBEAT_TIMEOUT);

        // And a test's shortened budget shortens the pulse with it, or the agent
        // would call a beating host dead.
        for budget_ms in [200, 1000, 60_000] {
            let beat = beat_interval(&request_with_budget(Some(budget_ms)));
            assert_eq!(beat * 4, Duration::from_millis(budget_ms), "{budget_ms}ms");
        }
    }

    fn request_with_budget(heartbeat_timeout_ms: Option<u64>) -> ExecRequest {
        ExecRequest {
            version: PROTO_VERSION,
            argv: vec!["true".into()],
            env: BTreeMap::new(),
            cwd: ".".into(),
            stdin: None,
            heartbeat_timeout_ms,
        }
    }

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

    /// Minimal ExecOptions for the native tests. Most of them run `sh` and are
    /// unix-only, but the not-found case (#24) needs no shell and runs on both
    /// platforms — which is the point, since Windows is where that bug bit.
    fn opts(cmd: &[&str]) -> ExecOptions {
        ExecOptions {
            no_sync: false,
            writeback: false,
            with_snapshot: false,
            or_native: false,
            guest_env: None,
            env: Vec::new(),
            with_file: Vec::new(),
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

    // ── The probe behind the unsynced-env note ────────────────────────────────

    /// A repo whose .gitignore hides `.env*`, with a real commit so HEAD exists.
    fn env_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(tmp.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@local"]);
        std::fs::write(tmp.path().join(".gitignore"), ".env*\n").unwrap();
        std::fs::write(tmp.path().join("src.rs"), "fn main() {}\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "init"]);
        tmp
    }

    #[test]
    fn the_probe_finds_ignored_untracked_env_files() {
        let tmp = env_repo();
        std::fs::write(tmp.path().join(".env"), "K=v\n").unwrap();
        std::fs::write(tmp.path().join(".env.local"), "D=1\n").unwrap();
        assert_eq!(
            unsynced_env_files(tmp.path(), &[]),
            [".env".to_string(), ".env.local".to_string()],
            "sorted, so the note reads the same on every run"
        );
    }

    #[test]
    fn the_probe_is_silent_when_nothing_was_left_behind() {
        // No env file at all — the ordinary repo, and the ordinary failing
        // command in one. Nothing to say.
        let tmp = env_repo();
        assert!(unsynced_env_files(tmp.path(), &[]).is_empty());
    }

    #[test]
    fn a_file_that_did_sync_is_never_reported() {
        let tmp = env_repo();
        std::fs::write(tmp.path().join(".env"), "K=v\n").unwrap();

        // Already forced by this very run: it is in the guest, so a note about it
        // would be a lie.
        assert!(unsynced_env_files(tmp.path(), &[".env".to_string()]).is_empty());

        // Tracked despite matching .gitignore: the HEAD seed carried it, so it is
        // in the guest too (see sync::snapshot).
        let out = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["add", "-f", ".env"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            unsynced_env_files(tmp.path(), &[]).is_empty(),
            "a tracked-but-ignored file reaches the guest and must not draw a note"
        );
    }

    #[test]
    fn an_unignored_env_file_is_never_reported() {
        // `.env` committed as ordinary tracked content (no ignore rule): it syncs
        // like anything else.
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.name", "test"][..],
            &["config", "user.email", "test@local"][..],
        ] {
            std::process::Command::new("git")
                .current_dir(tmp.path())
                .args(args)
                .output()
                .unwrap();
        }
        std::fs::write(tmp.path().join(".env"), "K=v\n").unwrap();
        assert!(unsynced_env_files(tmp.path(), &[]).is_empty());
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

    // ── A native command that cannot be spawned (#24) ─────────────────────────

    /// Deliberately *not* `#[cfg(unix)]`: the report came off a `windows-latest`
    /// runner, and a spawn error is one of the few things whose `ErrorKind` the
    /// two platforms have to agree on for this fix to hold. A unix-only test
    /// here would re-create the exact blind spot that let 125 ship — so this one
    /// runs in the Windows guest too (`vm exec windows -- cargo test`).
    #[test]
    fn run_native_reports_a_missing_command_as_127_not_infra() {
        // The exec form hands argv[0] straight to the OS, so this is the path
        // that used to raise the spawn error as an anyhow chain and exit 125 —
        // "vm infra error, often transient, retry" — for a command that was
        // simply not on PATH, and never would be on a retry. The guest agent
        // answers 127 here (exec/guest.rs); --or-native is a transparent swap
        // only if the host does too.
        let code = run_native(&opts(&["definitely-not-a-real-binary", "--flag"])).unwrap();
        assert_eq!(
            code, 127,
            "a missing command is the shell's 127, not vm's 125"
        );
    }

    /// Unix-only, and not an oversight: Windows has no "exists but not
    /// executable" spawn error for an ordinary file. CreateProcess rejects a
    /// non-image file with ERROR_BAD_EXE_FORMAT, which is not `PermissionDenied`
    /// and correctly stays an infra error — there is no 126 to assert.
    #[cfg(unix)]
    #[test]
    fn run_native_reports_a_non_executable_command_as_126_not_infra() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("not-executable.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        // Readable, but no +x: the spawn fails with PermissionDenied.
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();

        let code = run_native(&opts(&[script.to_str().unwrap(), "--flag"])).unwrap();
        assert_eq!(code, 126, "a non-executable command is 126, not vm's 125");
    }

    /// Unix-only, because the code being asserted is `sh`'s and not vm's. The
    /// same run on Windows goes through `cmd /C`, which answers 1 for an
    /// unrecognized command rather than 127 — so the two *forms* genuinely
    /// disagree there. That is cmd.exe's convention, not a vm bug, and it is not
    /// what #24 is about: a script reaches a Windows *guest* through the very
    /// same `cmd /C` (see [`build_argv`]) and yields the same 1, so native and
    /// guest still answer alike, which is the property `--or-native` sells.
    #[cfg(unix)]
    #[test]
    fn the_script_form_gets_its_codes_from_the_shell_itself() {
        // The other half of the arity rule, and the reason the bug hid for so
        // long: a single argument goes to `sh -c`, which resolves argv[0] on its
        // own and has always reported the shell's codes. Only the exec form ever
        // reached the spawn error that used to become a 125.
        assert_eq!(
            run_native(&opts(&["definitely-not-a-real-binary"])).unwrap(),
            127
        );
    }
}
