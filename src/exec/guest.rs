use super::job;
use crate::proto::ExecRequest;
use crate::sync::expand_home;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// `vm _exec`: read an ExecRequest from stdin, run it, propagate the exit
/// code. Stdout/stderr stream straight through the ssh channel.
pub fn exec() -> Result<i32> {
    let req = ExecRequest::read_from(std::io::stdin().lock())?;
    let cwd = expand_home(&req.cwd)?;
    if !cwd.is_dir() {
        anyhow::bail!(
            "working directory {} does not exist (sync first?)",
            cwd.display()
        );
    }

    let mut cmd = build_command(&req);
    cmd.current_dir(&cwd)
        .env("PATH", augmented_path())
        .envs(&req.env)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = job::spawn_and_wait(cmd, start_liveness_watcher)
        .with_context(|| format!("running {:?} in {}", req.argv.join(" "), cwd.display()))?;
    Ok(exit_code(&status))
}

/// The host holds our stdin open for the whole run. EOF (or error) means the
/// host or the ssh connection died — sshd sends no signal for no-PTY
/// sessions, so this is the only disconnect notification we get. Tear the
/// child tree down instead of leaving orphaned compilers. Started only after
/// the kill-tree (process group / job object) is registered, so the stop can
/// never miss the child.
fn start_liveness_watcher() {
    std::thread::spawn(|| {
        use std::io::Read;
        let mut sink = [0u8; 64];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        super::job::emergency_stop();
    });
}

fn build_command(req: &ExecRequest) -> Command {
    if req.shell {
        let joined = req.argv.join(" ");
        if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", &joined]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", &joined]);
            cmd
        }
    } else {
        let mut cmd = Command::new(&req.argv[0]);
        cmd.args(&req.argv[1..]);
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
    path
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
