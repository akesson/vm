use crate::config::{Config, GuestOs, VmConfig};
use crate::exit::usage;
use crate::ssh::SshTarget;
use crate::{lock, mapping, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

impl std::fmt::Display for GuestOs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // f.pad (not write_str) so `{:<8}` column widths apply.
        f.pad(match self {
            GuestOs::Windows => "windows",
            GuestOs::Linux => "linux",
            GuestOs::Macos => "macos",
        })
    }
}

/// Resolve the address to ssh to *without* touching the VM: config override, or
/// the IP Parallels already reports. For the read-only probes (`vm doctor`'s
/// live checks, reap's console-idle probe), which only ever ask about a VM that
/// is already running. Everything that intends to *use* a guest calls
/// [`bring_up`] instead.
pub fn ssh_target(vm: &VmConfig) -> Result<SshTarget> {
    let host = match &vm.host {
        Some(host) => host.clone(),
        None => {
            let prl = prl::find(&vm.parallels_name)?;
            prl.ip()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "VM '{}' has no IP (status: {})",
                        vm.parallels_name,
                        prl.status
                    )
                })?
                .to_string()
        }
    };
    Ok(SshTarget {
        user: vm.user.clone(),
        host,
    })
}

/// Bring the VM up — starting or resuming it as needed — and return the address
/// to reach the guest at, ready to use. Every command that runs something in a
/// guest goes through here.
///
/// This is why there is no `vm start`: a VM that is down is not an error state a
/// caller has to resolve first, it is a VM one second away from being up, and
/// the command that wants it says so and waits (see [`prl::ensure_up`]). The
/// mirror image is `vm reap`, which shuts down VMs nobody is using — together
/// they mean VM lifecycle is never something a caller has to think about.
///
/// The caller is responsible for holding the VM's use lock (`lock::shared`)
/// across the work that follows, so reap cannot shut the VM down out from
/// under it.
pub fn bring_up(alias: &str, vm: &VmConfig) -> Result<SshTarget> {
    let started = Instant::now();
    let up = prl::ensure_up(alias, &vm.parallels_name)?;
    let target = SshTarget {
        user: vm.user.clone(),
        host: vm.host.clone().unwrap_or(up.ip),
    };
    // An IP is not the same as a guest that will talk to us: a VM reports one as
    // soon as Parallels Tools is up, which on a cold boot is well before sshd
    // accepts connections — and every guest is cold-booted after a host reboot.
    // Everything a command does next rides ssh (the sync, the agent, and even a
    // Windows exec's sync), so wait for the door to actually open. Only after a
    // wake: a VM that was already running has long since opened it, and probing
    // a warm guest would put an ssh round-trip in front of every exec.
    if up.woke {
        wait_for_ssh(alias, &target);
    }
    if up.woke || started.elapsed() >= HEARTBEAT {
        eprintln!(
            "vm ▸ {alias} ▸ ready at {} ▸ {:.1}s",
            target.host,
            started.elapsed().as_secs_f32()
        );
    }
    Ok(target)
}

/// A wait worth narrating, and the point past which a guest that has an IP but
/// no ssh is more likely broken than slow.
const HEARTBEAT: Duration = Duration::from_secs(10);
const SSH_TIMEOUT: Duration = Duration::from_secs(60);

/// Wait for a freshly woken guest to accept ssh.
///
/// Advisory, not a gate: if ssh never comes up we let the caller's real work
/// proceed and fail on its own terms. It will do so in a second, with the actual
/// ssh error — a better answer than one this function could invent, and it keeps
/// the odd guest that is reachable some other way (a Windows console exec with
/// `--no-sync`) working instead of being blocked here on a door it never needed.
fn wait_for_ssh(alias: &str, target: &SshTarget) {
    let started = Instant::now();
    let mut next_beat = HEARTBEAT;
    while !ssh::reachable(target) {
        if started.elapsed() >= SSH_TIMEOUT {
            eprintln!(
                "vm ▸ {alias} ▸ WARNING: no ssh at {} {}s after the guest came up — \
                 continuing anyway (guest sshd not running? see `vm doctor {alias}`)",
                target.destination(),
                started.elapsed().as_secs()
            );
            return;
        }
        if started.elapsed() >= next_beat {
            eprintln!(
                "vm ▸ {alias} ▸ waiting for ssh at {} — {}s of {}s",
                target.destination(),
                started.elapsed().as_secs(),
                SSH_TIMEOUT.as_secs()
            );
            next_beat = started.elapsed() + HEARTBEAT;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Bring the VM up for a transport that does not use ssh at all: `vm run
/// --elevated` rides Parallels Tools, so the guest needs Tools up, not sshd.
///
/// Tools' exec session lags the IP after a wake — a resumed macOS guest refuses
/// one for about ten seconds ("Unable to open new session") while already
/// reporting an address. Waiting that out here is the difference between a
/// `vm run` that works on the first try after a resume and one that fails with
/// a Parallels error the caller can do nothing about.
///
/// Advisory like [`wait_for_ssh`], and for the same reason: if the session never
/// opens, the real run fails on its own terms with the actual error, which is a
/// better answer than one invented here. Only after a wake — a warm guest has
/// long since opened one, and probing it would put a prlctl round-trip in front
/// of every elevated run.
pub fn bring_up_elevated(alias: &str, vm: &VmConfig) -> Result<()> {
    let started = Instant::now();
    if !prl::ensure_up(alias, &vm.parallels_name)?.woke {
        return Ok(());
    }
    let agent = agent_abs_path(vm);
    let mut next_beat = HEARTBEAT;
    loop {
        match prl::exec_elevated(&vm.parallels_name, &[&agent, "_version"])?
            .stdin(std::process::Stdio::null())
            .output()
        {
            // Anything but a closed session is the guest answering — including
            // an error, which the run itself will report far better than we can.
            Ok(out)
                if out.status.success()
                    || !prl::is_session_not_ready(&String::from_utf8_lossy(&out.stderr)) =>
            {
                break;
            }
            Ok(_) => {}
            Err(err) => return Err(err).context("probing the guest's elevated session"),
        }
        if started.elapsed() >= SESSION_TIMEOUT {
            eprintln!(
                "vm ▸ {alias} ▸ WARNING: Parallels Tools accepted no elevated session {}s \
                 after the guest came up — continuing anyway (see `vm doctor {alias}`)",
                started.elapsed().as_secs()
            );
            break;
        }
        if started.elapsed() >= next_beat {
            eprintln!(
                "vm ▸ {alias} ▸ waiting for the guest's elevated session — {}s of {}s",
                started.elapsed().as_secs(),
                SESSION_TIMEOUT.as_secs()
            );
            next_beat = started.elapsed() + HEARTBEAT;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    eprintln!(
        "vm ▸ {alias} ▸ ready ▸ {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

/// How long Parallels Tools gets to accept an exec session after a wake.
/// Measured on Parallels 26.4: a resumed macOS guest takes ~10s; the rest are
/// ready at once.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

pub fn ls() -> Result<i32> {
    let cfg = Config::load()?;
    let vms = prl::list_all()?;
    let repo = mapping::RepoLocation::discover().ok();

    // Rows first, so every column is as wide as its widest cell: a long alias
    // or Parallels name shifts the table instead of ragging it.
    let mut rows = vec![[
        "ALIAS".to_string(),
        "OS".to_string(),
        "VM".to_string(),
        "STATUS".to_string(),
        "IP".to_string(),
        "CHECKOUT (this repo)".to_string(),
    ]];
    for (alias, vm) in &cfg.vm {
        let prl_vm = vms.iter().find(|p| p.name == vm.parallels_name);
        rows.push([
            alias.clone(),
            vm.os.to_string(),
            vm.parallels_name.clone(),
            prl_vm.map_or("missing!", |p| p.status.as_str()).to_string(),
            prl_vm.and_then(|p| p.ip()).unwrap_or("-").to_string(),
            match &repo {
                Some(repo) => mapping::guest_repo_path(&vm.work_root, &repo.name),
                None => "- (not in a git repo)".to_string(),
            },
        ]);
    }
    let widths = column_widths(&rows);
    for row in &rows {
        println!("{}", format_row(row, &widths));
    }
    Ok(0)
}

/// Widest cell per column. The last column is never padded (nothing follows
/// it), so it does not need a width.
fn column_widths<const N: usize>(rows: &[[String; N]]) -> Vec<usize> {
    (0..N - 1)
        .map(|col| {
            rows.iter()
                .map(|r| r[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect()
}

fn format_row<const N: usize>(row: &[String; N], widths: &[usize]) -> String {
    let mut line = String::new();
    for (col, cell) in row.iter().enumerate() {
        match widths.get(col) {
            Some(width) => line.push_str(&format!("{cell:<width$}  ")),
            None => line.push_str(cell), // last column: no padding
        }
    }
    line
}

/// The vm agent binary inside the guest. `~`/`%USERPROFILE%` are expanded by
/// the remote shell (sh on unix, cmd.exe on Windows).
pub fn agent_path(vm: &VmConfig) -> String {
    match &vm.agent_path {
        Some(path) => path.clone(),
        None => match vm.os {
            GuestOs::Windows => "~/.vm/bin/vm.exe".to_string(),
            GuestOs::Linux | GuestOs::Macos => "~/.vm/bin/vm".to_string(),
        },
    }
}

/// The agent path as invoked through the console-session transport
/// (`prlctl exec --current-user`), which runs through cmd.exe rather than a
/// POSIX shell: `~` never expands there, but `%USERPROFILE%` does, and path
/// separators must be backslashes.
///
/// Console-session *only*. `%USERPROFILE%` is whatever the running account's
/// profile is, so under `--elevated` (SYSTEM) this resolves to
/// `C:\WINDOWS\system32\config\systemprofile` and finds no agent at all — that
/// path uses [`agent_abs_path`] instead.
pub fn agent_console_path(vm: &VmConfig) -> String {
    let path = agent_path(vm);
    match path.strip_prefix("~/") {
        Some(rest) => format!(r"%USERPROFILE%\{}", rest.replace('/', r"\")),
        None => path.replace('/', r"\"),
    }
}

/// The configured user's home directory in the guest, spelled out. Every other
/// path helper leans on a shell (or the running process's `$HOME`) to expand
/// `~`; the elevated transport has neither — no shell to expand it, and a
/// `$HOME` belonging to root/SYSTEM rather than to `vm.user`.
pub fn guest_home(vm: &VmConfig) -> String {
    let user = &vm.user;
    match vm.os {
        GuestOs::Windows => format!(r"C:\Users\{user}"),
        GuestOs::Linux => format!("/home/{user}"),
        GuestOs::Macos => format!("/Users/{user}"),
    }
}

/// The vm agent binary in the guest as an *absolute* path, for a transport with
/// no shell and no useful `$HOME` behind it (`prlctl exec` without
/// `--current-user` — see [`crate::prl::exec_elevated`]).
///
/// An `agent_path` config override is honored, so a guest whose home is not
/// where [`guest_home`] guesses (a renamed Windows profile directory, an
/// unusual layout) has a way to say so: give it an absolute path.
pub fn agent_abs_path(vm: &VmConfig) -> String {
    let path = agent_path(vm);
    let win = vm.os == GuestOs::Windows;
    let joined = match path.strip_prefix("~/") {
        Some(rest) => {
            let sep = if win { '\\' } else { '/' };
            format!("{}{sep}{rest}", guest_home(vm))
        }
        None => path,
    };
    if win {
        joined.replace('/', r"\")
    } else {
        joined
    }
}

/// Invoke a hidden agent verb in the guest, capturing stdout. Fails with the
/// agent's stderr, or a deploy hint when the binary is missing.
pub fn agent_call(vm: &VmConfig, target: &SshTarget, args: &[&str]) -> Result<String> {
    let agent = agent_path(vm);
    // POSIX-quote values (e.g. 'C:\work\syncfs' would lose its backslashes
    // in bash otherwise); the agent path stays bare so `~` expands.
    let quoted: Vec<String> = args.iter().map(|a| ssh::shell_quote(a)).collect();
    let mut remote: Vec<&str> = vec![&agent];
    remote.extend(quoted.iter().map(String::as_str));
    let out = ssh::run_capture(target, &remote)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such file")
            || stderr.contains("not recognized")
            || stderr.contains("not found")
        {
            bail!("vm agent not installed in guest (run `vm deploy` first)");
        }
        bail!(
            "guest agent {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

/// Full sync of the current repo to a guest. Returns the verified snapshot.
///
/// `extra` holds already-validated, repo-root-relative `--with-file` paths (see
/// [`resolve_with_files`]) to carry even though they are gitignored.
pub fn sync_repo(
    alias: &str,
    vm: &VmConfig,
    target: &SshTarget,
    extra: &[String],
) -> Result<sync::Snapshot> {
    let repo = mapping::RepoLocation::discover()?;
    let guest_repo = mapping::guest_repo_path(&vm.work_root, &repo.name);
    let started = Instant::now();

    // Serialize the whole init→push→apply→verify section against any other
    // sync of this repo to this guest (e.g. a parallel `vm exec` fan-out);
    // released when this function returns.
    let _sync_guard = sync::host::lock_sync(&repo.root, alias)?;

    // 1. Make sure the guest repository exists (idempotent, cheap).
    agent_call(vm, target, &["_sync-init", "--repo", &guest_repo])?;

    // 2. Snapshot + push objects.
    let url = mapping::ssh_remote_url(&target.user, &target.host, &guest_repo);
    let snap = sync::host::sync_to(
        &repo.root,
        alias,
        &url,
        Some(&ssh::git_ssh_command()),
        extra,
    )?;

    // 3. Apply in the guest and verify the tree hash round-trips.
    let guest_tree = agent_call(
        vm,
        target,
        &["_sync-apply", "--repo", &guest_repo, "--sha", &snap.commit],
    )?;
    if guest_tree != snap.tree {
        bail!(
            "sync verification failed: host tree {} but guest reports {guest_tree}",
            snap.tree
        );
    }
    // Forced files are named in the breadcrumb, never smuggled: `--with-file`
    // ships something the repo's own .gitignore says to leave behind (often a
    // secret), and a run that does that must say so on its way past.
    let forced = match extra {
        [] => String::new(),
        extra => format!(" ▸ forced {}", extra.join(", ")),
    };
    eprintln!(
        "vm ▸ {alias} ▸ synced {} ▸ tree {}{forced} ▸ {:.1}s",
        repo.name,
        &snap.tree[..10],
        started.elapsed().as_secs_f32()
    );
    Ok(snap)
}

/// Resolve `--with-file` specs into repo-root-relative paths for the snapshot.
/// Each names an existing file inside the repo, taken relative to `cwd` like any
/// other path the caller types.
///
/// A bad spec is the caller's own invocation — a typo, a path from the wrong
/// directory — so it is a usage error (exit 2, "fix it and retry"), raised
/// before vm resumes a VM or syncs anything, exactly like a typo'd `-e` (see
/// `exec::host::resolve_env`).
///
/// Two things are refused rather than half-worked:
///
/// - **Directories.** `--with-file target` would quietly push a multi-gigabyte
///   build tree over ssh. The flag exists for the `.env`-shaped case; naming the
///   files is cheap, and discovering why a sync took ten minutes is not.
/// - **Symlinks.** git syncs the *link*, not what it points at, so a
///   `.env -> ~/secrets/app.env` would land in the guest dangling — a file that
///   exists and cannot be read, which is a worse failure than not syncing it.
pub fn resolve_with_files(specs: &[String], cwd: &Path, repo_root: &Path) -> Result<Vec<String>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let root = std::fs::canonicalize(repo_root)
        .with_context(|| format!("resolving repo root {}", repo_root.display()))?;
    let mut out = Vec::new();
    for spec in specs {
        let path = cwd.join(spec);
        // Checked before canonicalize, which would follow the link and hide it —
        // and whose non-following of it is what makes the containment test below
        // mean what it says.
        if path.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) {
            return Err(usage(format!(
                "--with-file {spec}: is a symlink, and git syncs the link rather than the \
                 file it points at — the guest would get a dangling one.\n  \
                 Point the flag at a real file, or pass the values with -e NAME=value."
            )));
        }
        let abs = std::fs::canonicalize(&path).map_err(|_| {
            usage(format!(
                "--with-file {spec}: no such file (paths are relative to the current \
                 directory; to pass a value rather than a file, use -e NAME=value)"
            ))
        })?;
        if !abs.is_file() {
            return Err(usage(format!(
                "--with-file {spec}: not a file — a directory is not synced this way. \
                 Name the files you need."
            )));
        }
        let rel = abs.strip_prefix(&root).map_err(|_| {
            usage(format!(
                "--with-file {spec}: sits outside the repo ({}), and the sync only ever \
                 carries the repo tree",
                root.display()
            ))
        })?;
        // git takes `/` on every platform; a Windows host would hand us `\`.
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(out)
}

/// Run the guest env's one-time setup (mise: `mise trust`) in the guest
/// checkout, once per checkout creation; a no-op when no guest env is active or
/// the checkout already ran it (the guest verb keeps the marker). Serialized per
/// (repo, guest) via the same lock the sync path uses, so a parallel exec
/// fan-out on a fresh checkout runs it exactly once. A nonzero exit is an infra
/// failure (exit 125): vm couldn't ready the checkout, so the caller's command
/// must not run.
pub fn first_sync_hook(
    alias: &str,
    vm: &VmConfig,
    target: &SshTarget,
    repo: &mapping::RepoLocation,
    genv: &crate::guest_env::ActiveEnv,
) -> Result<()> {
    let Some(hook) = genv.first_sync_cmd() else {
        return Ok(());
    };
    let guest_repo = mapping::guest_repo_path(&vm.work_root, &repo.name);
    let _sync_guard = sync::host::lock_sync(&repo.root, alias)?;
    let agent = agent_path(vm);
    // Bare agent path so `~` expands in the remote shell; POSIX-quote the values
    // (a Windows checkout path, or a hook with spaces) exactly like agent_call.
    let (repo_q, cmd_q) = (ssh::shell_quote(&guest_repo), ssh::shell_quote(hook));
    let remote = [
        agent.as_str(),
        "_first-sync",
        "--repo",
        &repo_q,
        "--cmd",
        &cmd_q,
    ];
    let status = ssh::run_streamed(target, &remote)?;
    if !status.success() {
        bail!(
            "guest-env setup failed (exit {}): {hook}\n  \
             it ran in the guest checkout {guest_repo}; fix it there, or re-run with \
             `--guest-env none` to skip the guest env entirely",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// `vm sync <alias> [--guest-env …] [--with-file …]`: bring the VM up if needed,
/// then sync (and run the guest env's first-sync setup, exactly as an exec would).
pub fn sync_cmd(
    alias: &str,
    guest_env: Option<crate::guest_env::GuestEnv>,
    with_file: &[String],
) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    let repo = mapping::RepoLocation::discover()?;
    // Before the VM is touched: a typo'd path must cost a line of stderr, not a
    // resume and a sync.
    let extra = resolve_with_files(with_file, &std::env::current_dir()?, &repo.root)?;
    let _use = lock::shared(alias)?;
    let target = bring_up(alias, vm)?;
    let genv = crate::guest_env::resolve(guest_env, &repo.root);
    genv.announce(alias);
    sync_repo(alias, vm, &target, &extra)?;
    first_sync_hook(alias, vm, &target, &repo, &genv)?;
    Ok(0)
}

/// `vm shot <alias> [file]`: screenshot the VM display. Useful for seeing GUI
/// state ssh can't (TCC dialogs, login screens, stuck installers).
///
/// Brings the VM up first — `prlctl capture` fails outright on a suspended VM —
/// but does *not* wait for an IP the way [`bring_up`] does: a guest whose
/// network or Parallels Tools never came up is precisely the guest whose screen
/// is worth looking at, and it must not be the one command that refuses to.
pub fn shot(alias: &str, file: Option<std::path::PathBuf>) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    let _use = lock::shared(alias)?;
    prl::ensure_running(alias, &vm.parallels_name)?;
    let file = file.unwrap_or_else(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::path::PathBuf::from(format!("{alias}-{secs}.png"))
    });
    let path = file.to_string_lossy().into_owned();
    prl::capture(&vm.parallels_name, &path)?;
    eprintln!("vm ▸ {alias} ▸ screenshot ▸ {path}");
    Ok(0)
}

/// `vm clean <alias>`: delete the guest checkout of the current repo. Only
/// replica state is lost — the next exec/sync recreates it (cold build).
pub fn clean(alias: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    let repo = mapping::RepoLocation::discover()?;
    let guest_repo = mapping::guest_repo_path(&vm.work_root, &repo.name);
    let _use = lock::shared(alias)?;
    let target = bring_up(alias, vm)?;
    // All guests speak POSIX (Windows sshd shell is Git Bash).
    let quoted = ssh::shell_quote(&guest_repo);
    let out = ssh::run_capture(&target, &["rm", "-rf", &quoted])?;
    if !out.status.success() {
        bail!(
            "removing {guest_repo} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    eprintln!("vm ▸ {alias} ▸ removed {guest_repo}");
    Ok(0)
}

/// `vm exec <alias> --with-snapshot -- cmd…`: snapshot, run, roll back, delete
/// the snapshot. The guest ends up exactly as it started — for destructive or
/// state-polluting experiments (installers, registry edits, config trials).
/// Called from `exec::host::exec` once the target has resolved; it takes the VM
/// exclusively and calls back into `exec_in` (which does not lock).
pub fn with_snapshot(
    alias: &str,
    vm: &VmConfig,
    opts: &crate::exec::host::ExecOptions,
) -> Result<i32> {
    // Exclusive: rollback rewinds the whole VM (disk and memory), which would
    // obliterate any concurrent run and silently undo its guest-side effects.
    let Some(_claim) = lock::try_exclusive(alias)? else {
        bail!(
            "'{alias}' is in use by another vm process — --with-snapshot needs the \
             VM to itself (rollback would destroy the other run)"
        );
    };
    // Fully up before the snapshot, not merely powered on: the memory image
    // captures whatever state the guest is in, so snapshotting one that is still
    // finding its feet is a state the rollback would faithfully restore.
    bring_up(alias, vm)?;
    sweep_stale_snapshots(alias, &vm.parallels_name)?;
    ensure_snapshot_headroom(&vm.parallels_name)?;

    let id = prl::snapshot_create(&vm.parallels_name, &format!("vm-with-snapshot-{alias}"))?;
    eprintln!("vm ▸ {alias} ▸ snapshot {id} taken");
    let run = crate::exec::host::exec_in(alias, vm, opts);
    // Roll back even when the command failed — that's the whole point.
    let restore = prl::snapshot_switch(&vm.parallels_name, &id)
        .and_then(|()| prl::snapshot_delete(&vm.parallels_name, &id));
    match restore {
        Ok(()) => eprintln!("vm ▸ {alias} ▸ rolled back to pre-run state"),
        Err(err) => eprintln!(
            "vm ▸ {alias} ▸ WARNING: rollback failed ({err:#}); snapshot {id} kept — \
             restore manually with `prlctl snapshot-switch '{}' --id '{id}'`",
            vm.parallels_name
        ),
    }
    run
}

/// Delete leftovers of with-snapshot runs that were killed before rollback
/// (each is ~VM-RAM on disk and grows). Safe under the exclusive lock: no
/// live run can own one. The dead run's guest-side effects were kept, so warn.
fn sweep_stale_snapshots(alias: &str, name: &str) -> Result<()> {
    for (id, snap) in prl::snapshot_list(name)? {
        if snap.starts_with("vm-with-snapshot-") {
            eprintln!(
                "vm ▸ {alias} ▸ WARNING: deleting stale snapshot {snap} — a previous \
                 with-snapshot run died before rolling back; its changes are still in the VM"
            );
            prl::snapshot_delete(name, &id)?;
        }
    }
    Ok(())
}

/// Refuse to snapshot when the volume holding the VM is short on space: a
/// running-VM snapshot writes a memory image of roughly the VM's RAM, then
/// grows a delta disk for as long as it exists — require 2× RAM free.
fn ensure_snapshot_headroom(name: &str) -> Result<()> {
    let details = prl::details(name)?;
    match free_disk_bytes(&details.home) {
        Some(free) => check_headroom(name, &details, free),
        None => Ok(()),
    }
}

fn check_headroom(name: &str, details: &prl::VmDetails, free: u64) -> Result<()> {
    let need = details.memory_mb * 1024 * 1024 * 2;
    if free < need {
        bail!(
            "not enough disk space to snapshot '{name}': {:.1} GiB free on the volume \
             holding {}, but a snapshot wants ~{:.1} GiB (2× the VM's {:.1} GiB RAM)",
            free as f64 / GIB,
            details.home,
            need as f64 / GIB,
            details.memory_mb as f64 / 1024.0,
        );
    }
    Ok(())
}

const GIB: f64 = (1u64 << 30) as f64;

/// Free bytes available to this user on the volume containing `path`.
/// None where the probe isn't available — the snapshot then proceeds
/// unchecked rather than failing a healthy run.
#[cfg(unix)]
fn free_disk_bytes(path: &str) -> Option<u64> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } == 0)
        .then(|| vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_disk_bytes(_path: &str) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(memory_mb: u64) -> prl::VmDetails {
        prl::VmDetails {
            home: "/Users/x/Parallels/macOS.macvm/".into(),
            memory_mb,
        }
    }

    #[test]
    fn headroom_needs_twice_the_vm_ram() {
        let d = details(20480); // 20 GiB RAM → wants 40 GiB free
        let gib = 1u64 << 30;
        assert!(check_headroom("mac", &d, 41 * gib).is_ok());
        let err = check_headroom("mac", &d, 39 * gib).unwrap_err().to_string();
        assert!(err.contains("39.0 GiB free"), "{err}");
        assert!(err.contains("~40.0 GiB"), "{err}");
        assert!(err.contains("20.0 GiB RAM"), "{err}");
    }

    fn win_vm(agent_path: Option<&str>) -> VmConfig {
        let toml = format!(
            "parallels_name = \"W\"\nos = \"windows\"\nuser = \"u\"\nwork_root = 'C:\\work'\n{}",
            agent_path.map_or(String::new(), |p| format!("agent_path = '{p}'\n"))
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn console_path_rewrites_tilde_for_cmd() {
        // prlctl exec has no POSIX shell: `~` stays literal, %USERPROFILE%
        // expands (via cmd), and cmd needs backslash separators.
        assert_eq!(
            agent_console_path(&win_vm(None)),
            r"%USERPROFILE%\.vm\bin\vm.exe"
        );
    }

    #[test]
    fn console_path_keeps_absolute_paths_with_backslashes() {
        assert_eq!(
            agent_console_path(&win_vm(Some(r"C:\tools/vm.exe"))),
            r"C:\tools\vm.exe"
        );
    }

    // ── absolute guest paths (the elevated transport) ─────────────────────────

    fn vm_of(os: &str, user: &str, agent_path: Option<&str>) -> VmConfig {
        let toml = format!(
            "parallels_name = \"P\"\nos = \"{os}\"\nuser = \"{user}\"\nwork_root = '~/work'\n{}",
            agent_path.map_or(String::new(), |p| format!("agent_path = '{p}'\n"))
        );
        toml::from_str(&toml).unwrap()
    }

    /// `vm run --elevated` runs as root/SYSTEM, whose `$HOME` is not the config
    /// user's — and its transport has no shell to expand `~` anyway. Both the
    /// agent and the cwd therefore have to be spelled out, per OS.
    #[test]
    fn the_guest_home_is_spelled_out_per_os() {
        assert_eq!(
            guest_home(&vm_of("linux", "parallels", None)),
            "/home/parallels"
        );
        assert_eq!(
            guest_home(&vm_of("macos", "claudedev", None)),
            "/Users/claudedev"
        );
        assert_eq!(
            guest_home(&vm_of("windows", "hakesson", None)),
            r"C:\Users\hakesson"
        );
    }

    #[test]
    fn the_absolute_agent_path_expands_against_that_home() {
        assert_eq!(
            agent_abs_path(&vm_of("linux", "parallels", None)),
            "/home/parallels/.vm/bin/vm"
        );
        assert_eq!(
            agent_abs_path(&vm_of("macos", "claudedev", None)),
            "/Users/claudedev/.vm/bin/vm"
        );
        // Windows: home-joined *and* separator-normalized — prlctl hands this
        // straight to the guest with no shell to fix it up.
        assert_eq!(
            agent_abs_path(&vm_of("windows", "hakesson", None)),
            r"C:\Users\hakesson\.vm\bin\vm.exe"
        );
    }

    /// The escape hatch for a guest whose home is not where `guest_home` guesses
    /// (a renamed Windows profile dir, an unusual layout): an absolute
    /// `agent_path` in the config passes through untouched.
    #[test]
    fn an_absolute_agent_path_override_wins_over_the_derived_home() {
        assert_eq!(
            agent_abs_path(&vm_of("linux", "parallels", Some("/opt/vm/bin/vm"))),
            "/opt/vm/bin/vm"
        );
        assert_eq!(
            agent_abs_path(&vm_of("windows", "hakesson", Some(r"D:\tools/vm.exe"))),
            r"D:\tools\vm.exe"
        );
        // A `~/`-relative override still expands against the *config user's* home.
        assert_eq!(
            agent_abs_path(&vm_of("linux", "parallels", Some("~/tools/vm"))),
            "/home/parallels/tools/vm"
        );
    }

    // ── --with-file path resolution ───────────────────────────────────────────

    /// A repo root holding a `.env` and a `config/` dir.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "API_KEY=x\n").unwrap();
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/local.toml"), "a = 1\n").unwrap();
        tmp
    }

    fn resolve(specs: &[&str], cwd: &Path, root: &Path) -> Result<Vec<String>> {
        let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        resolve_with_files(&specs, cwd, root)
    }

    /// The message of the usage error `specs` must produce.
    fn refused(specs: &[&str], cwd: &Path, root: &Path) -> String {
        let err = resolve(specs, cwd, root).unwrap_err();
        assert!(
            err.downcast_ref::<crate::exit::UsageError>().is_some(),
            "a bad --with-file path is the caller's invocation, not a transient fault: {err:#}"
        );
        err.to_string()
    }

    #[test]
    fn a_file_in_the_repo_root_resolves_to_its_name() {
        let tmp = repo();
        assert_eq!(
            resolve(&[".env"], tmp.path(), tmp.path()).unwrap(),
            [".env"]
        );
    }

    #[test]
    fn paths_are_relative_to_the_cwd_but_reported_from_the_repo_root() {
        // Typed from a subdirectory, as any other path would be — the snapshot
        // needs it repo-relative.
        let tmp = repo();
        let cwd = tmp.path().join("config");
        assert_eq!(
            resolve(&["local.toml"], &cwd, tmp.path()).unwrap(),
            ["config/local.toml"]
        );
        // …and an absolute path works from anywhere.
        let abs = tmp.path().join(".env");
        assert_eq!(
            resolve(&[abs.to_str().unwrap()], &cwd, tmp.path()).unwrap(),
            [".env"]
        );
    }

    #[test]
    fn no_specs_resolves_to_nothing() {
        // The overwhelmingly common path: it must not even look at the disk.
        assert_eq!(
            resolve_with_files(&[], Path::new("/nonexistent"), Path::new("/nonexistent")).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_missing_file_is_refused_and_points_at_the_alternative() {
        let tmp = repo();
        let msg = refused(&[".env.nope"], tmp.path(), tmp.path());
        assert!(msg.contains("no such file"), "{msg}");
        assert!(msg.contains("-e NAME=value"), "{msg}");
    }

    #[test]
    fn a_directory_is_refused() {
        // `--with-file target` would push a build tree over ssh.
        let tmp = repo();
        let msg = refused(&["config"], tmp.path(), tmp.path());
        assert!(msg.contains("not a file"), "{msg}");
    }

    #[test]
    fn a_path_outside_the_repo_is_refused() {
        let tmp = repo();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secrets.env"), "K=v\n").unwrap();
        let spec = outside.path().join("secrets.env");
        let msg = refused(&[spec.to_str().unwrap()], tmp.path(), tmp.path());
        assert!(msg.contains("outside the repo"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_because_git_would_sync_the_link() {
        // The trap: `.env -> ~/secrets/app.env` is the tidy way people keep a
        // secret out of a repo, and git would ship the *link*, landing a
        // dangling file in the guest — a file that exists and cannot be read.
        let tmp = repo();
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("app.env");
        std::fs::write(&real, "K=v\n").unwrap();
        let link = tmp.path().join(".env.link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let msg = refused(&[".env.link"], tmp.path(), tmp.path());
        assert!(msg.contains("symlink"), "{msg}");
        assert!(msg.contains("dangling"), "{msg}");
    }
}
