use super::job;
use crate::proto::{self, ExecRequest};
use crate::sync::expand_home;
use anyhow::{Context, Result};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `vm _exec`: read an ExecRequest from stdin, run it, propagate the exit
/// code. Stdout/stderr stream straight through the ssh channel.
pub fn exec() -> Result<i32> {
    let mut req = ExecRequest::read_from(std::io::stdin().lock())?;
    let cwd = expand_home(&req.cwd)?;
    if !cwd.is_dir() {
        anyhow::bail!(
            "working directory {} does not exist (sync first? or is `user` in the \
             machine config wrong for this guest?)",
            cwd.display()
        );
    }
    // `vm run` may have sent input for the child (a script on `sh`'s stdin); an
    // exec never does, and its child keeps the null device.
    let payload = req.stdin.take();
    // How long the host may go quiet before it counts as dead. The host itself
    // never sets this — only a test does, to watch the timeout fire in half a
    // second instead of a minute.
    let silence_budget = req
        .heartbeat_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(proto::HEARTBEAT_TIMEOUT);

    // Always a plain argv: the host composed any shell invocation before sending
    // it (it knows this guest's OS from the config), so there is nothing here to
    // interpret — just spawn what was asked for, byte for byte.
    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..]);
    cmd.current_dir(&cwd)
        .env("PATH", augmented_path())
        .envs(&req.env)
        .stdin(if payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = match job::spawn_and_wait(cmd, |child| {
        if let Some(payload) = payload {
            feed_stdin(child, payload);
        }
        start_liveness_watcher(silence_budget);
    }) {
        Ok(status) => status,
        Err(err) => {
            // A command that isn't found or isn't executable is the *command's*
            // own result, not a vm failure — report it with the shell's own
            // codes (127 / 126) on the Ok path, so it never collides with the
            // infra exit code the host would otherwise read back. (A script
            // already yields these from the sh/cmd the host wrapped it in.)
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                match io.kind() {
                    std::io::ErrorKind::NotFound => {
                        eprintln!("vm: command not found: {}", req.argv[0]);
                        return Ok(127);
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        eprintln!("vm: command not executable: {}", req.argv[0]);
                        return Ok(126);
                    }
                    _ => {}
                }
            }
            return Err(err)
                .with_context(|| format!("running {:?} in {}", req.argv.join(" "), cwd.display()));
        }
    };
    Ok(exit_code(&status))
}

/// `vm _first-sync`: run the repo's `on_first_sync` hook in the checkout, once
/// per checkout creation. A marker inside the checkout's `.git/` records a
/// successful run; it survives ordinary re-syncs (`reset --hard` / `clean -fd`
/// never touch `.git/` internals) but is gone whenever the checkout is
/// recreated, so the hook re-runs after `vm clean`, a rebuilt guest, or a
/// manual delete. Absent marker ⇒ run the hook; a nonzero hook exit leaves no
/// marker, so it retries on the next exec/sync.
pub fn first_sync(repo: &str, cmd: &str) -> Result<i32> {
    let path = expand_home(repo)?;
    let git_dir = path.join(".git");
    if !git_dir.is_dir() {
        // No checkout yet (e.g. `--no-sync` before any sync). Nothing to set up
        // — the exec that follows reports the canonical "sync first?" itself.
        return Ok(0);
    }
    let marker = git_dir.join("vm-first-sync-done");
    if marker.exists() {
        return Ok(0); // already set up
    }

    eprintln!("vm ▸ first-sync ▸ $ {cmd}");
    // Same environment as a normal `vm exec` command, so per-user tool dirs
    // (`mise`, `cargo`, …) resolve. Plain wait, not the stdin liveness watcher
    // exec() uses: the host holds our stdin at /dev/null, so a watcher would see
    // instant EOF and kill the hook. First-sync hooks are short setup steps.
    let mut command = shell_command(cmd);
    command
        .current_dir(&path)
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .with_context(|| format!("running first-sync hook {cmd:?} in {}", path.display()))?;

    let code = exit_code(&status);
    if code == 0 {
        std::fs::write(&marker, format!("{cmd}\n"))
            .with_context(|| format!("writing first-sync marker {}", marker.display()))?;
    }
    Ok(code)
}

/// Write the request's stdin payload to the child and close the pipe, so a
/// command reading stdin (`sh` running a script, `findstr`) sees the input and
/// then a clean EOF.
///
/// On a thread, and never inline, for two independent reasons. The payload can
/// be megabytes while a pipe buffer is tens of kilobytes, so a synchronous
/// `write_all` would block until the child drained it — and a child that reads
/// something else first (or nothing at all) would never get that far: deadlock.
/// And `Child::wait()` closes `child.stdin` before it blocks, which would
/// truncate a payload still being written. Taking the handle out of the `Child`
/// gives this thread sole ownership of both problems.
///
/// A child that never reads its stdin is the ordinary case, not an error: the
/// write blocks until the child exits, the read end closes, and the write fails
/// `BrokenPipe` — which is why the result is dropped. That is only safe because
/// Rust ignores SIGPIPE at startup; an agent that ever opted out would die here
/// instead.
fn feed_stdin(child: &mut Child, payload: String) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(payload.as_bytes());
        // Dropping `stdin` closes the pipe: that EOF is what ends `sh`'s read of
        // the script and lets the command run at all.
    });
}

/// The host holds our stdin open for the whole run and beats on it every
/// [`proto::HEARTBEAT_INTERVAL`]. Either way it goes quiet — the pipe closes,
/// or the beats stop for `timeout` — the host or the connection died, and the
/// child tree comes down instead of turning into orphaned compilers.
///
/// Two ways, because no single one covers every transport. Over ssh the close
/// arrives (and is the only notification there is: sshd sends no signal for
/// no-PTY sessions), so EOF is acted on the instant it lands. Over `prlctl exec`
/// it may never arrive at all — Parallels Tools can leave the guest's end of
/// stdin open after the host-side `prlctl` is gone, and the reader below then
/// blocks forever on a pipe nobody will ever write to again (measured on the
/// macOS guest: a killed `vm run --elevated` orphaned agent and command alike,
/// #21) — so silence is the same verdict, reached the slow way.
///
/// Started only after the kill-tree (process group / job object) is registered,
/// so the stop can never miss the child. Reads the *agent's* own stdin — a
/// different pipe from the child's, which [`feed_stdin`] owns — so a payload
/// and the liveness channel never contend.
fn start_liveness_watcher(timeout: Duration) {
    // When the host was last heard from. Seeded now rather than at the first
    // beat: one is a whole interval away, and until then the host is presumed
    // alive — it just spoke, in the request this run is made of.
    let last_heard = Arc::new(Mutex::new(Instant::now()));

    let heard = Arc::clone(&last_heard);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut sink = [0u8; 64];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut sink) {
                Ok(0) | Err(_) => break, // EOF: the host is gone, and we know it now.
                Ok(_) => *heard.lock().unwrap() = Instant::now(), // a beat; nothing else is ever sent here
            }
        }
        super::job::emergency_stop();
    });

    // Checked on its own thread because the read above blocks with no timeout —
    // std has no portable way to ask for one, and the whole point is to notice
    // the read that will never return.
    let tick = (timeout / 4).max(Duration::from_millis(50));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(tick);
            if last_heard.lock().unwrap().elapsed() > timeout {
                super::job::emergency_stop();
            }
        }
    });
}

/// A guest shell running `script`: `sh -c` on unix, `cmd /C` on Windows. The
/// exec path composes its own shell on the *host* (which knows this guest's OS
/// without having to be running on it); this is for the first-sync hook, whose
/// command is a string the guest env hands us.
fn shell_command(script: &str) -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", script]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }
}

/// Non-interactive ssh sessions get a bare PATH; put the usual per-user tool
/// directories in front so `cargo`, `mise` etc. resolve like they do in a
/// login shell.
fn augmented_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        return current;
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    let dirsep = if cfg!(windows) { '\\' } else { '/' };
    let mut path = String::new();
    for dir in ["bin", ".cargo/bin", ".local/bin", ".vm/bin"] {
        let dir = dir.replace('/', &dirsep.to_string());
        path.push_str(&format!("{home}{dirsep}{dir}{sep}"));
    }
    path.push_str(&current);
    #[cfg(windows)]
    if let Some(usr_bin) = git_usr_bin(&current) {
        path.push(sep);
        path.push_str(&usr_bin);
    }
    path
}

/// Git for Windows' POSIX userland (`sh`, `bash`, `printf`, …). An ssh
/// session has it implicitly because sshd's DefaultShell is Git Bash, but the
/// console-session transport (prlctl exec) starts from the plain user
/// environment. Append it — at the tail, so system32's `find`/`sort` keep
/// winning — to make commands behave the same over both transports.
#[cfg(windows)]
fn git_usr_bin(path: &str) -> Option<String> {
    // git.exe on PATH lives at <install>\cmd\git.exe; the userland is
    // <install>\usr\bin.
    std::env::split_paths(path)
        .filter(|dir| dir.join("git.exe").is_file())
        .filter_map(|dir| {
            let usr_bin = dir.parent()?.join("usr").join("bin");
            usr_bin.join("sh.exe").is_file().then_some(usr_bin)
        })
        .next()
        .map(|p| p.to_string_lossy().into_owned())
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}
