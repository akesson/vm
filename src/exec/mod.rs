//! Command execution: the guest agent (`vm _exec`) spawns argv natively from
//! a JSON request — no shell quoting layer anywhere between the host CLI and
//! the guest process. The child is wrapped in a kill-tree (job object on
//! Windows, process group on unix) so cancelling on the host never leaves
//! orphaned compilers in the guest.
//!
//! Whether a command *is* an argv or a shell script is decided by its arity on
//! the host (see [`host::build_argv`]); the guest only ever execs an argv, and
//! [`advise`] warns when that rule may not have matched the caller's intent.
//!
//! Two verbs drive that agent. [`host`] is `vm exec`: the repo's code, in the
//! guest checkout the sync keeps current. [`run`] is `vm run`: no repo, no sync,
//! optionally elevated — the guest itself as the subject rather than the stage.

use crate::notice;

pub mod advise;
pub mod guest;
pub mod host;
pub mod run;

#[cfg(unix)]
#[path = "job_unix.rs"]
mod job;
#[cfg(windows)]
#[path = "job_windows.rs"]
mod job;

/// A [`std::process::Command`] for `argv` — with the one exception to "just
/// `.args()` it" that Windows forces.
///
/// `cmd.exe` has no argv. It parses a raw command line by its own rules, and
/// those rules do not include the CRT's backslash escapes — the convention
/// `std` quotes with. So spawning `["cmd", "/C", script]` through `.args()`
/// turns every `"` inside the script into `\"`, which cmd passes along as
/// literal bytes: `echo "QQ"` prints `\"QQ\"`, and a `find /c "x"` gets a stray
/// `\` argument and dies (measured on the real Windows guest, 2026-07-15).
/// `raw_arg` hands cmd the line verbatim, which is the only correct way to
/// invoke it. Everything after `/C` *is* one command line to cmd, so verbatim
/// args joined by spaces are exactly what it expects.
///
/// Every other program gets `.args()` unchanged: native executables parse
/// their command line with the CRT rules std quotes for, and on unix there is
/// a real argv and no quoting layer at all.
pub(crate) fn command_for(argv: &[String]) -> std::process::Command {
    let mut cmd = std::process::Command::new(&argv[0]);
    #[cfg(windows)]
    if is_cmd_exe(&argv[0]) {
        use std::os::windows::process::CommandExt;
        for arg in &argv[1..] {
            cmd.raw_arg(arg);
        }
        return cmd;
    }
    cmd.args(&argv[1..]);
    cmd
}

/// Whether `program` will resolve to cmd.exe — the caller may say `cmd`,
/// `cmd.exe`, or a full path to it.
#[cfg(windows)]
fn is_cmd_exe(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("cmd") || n.eq_ignore_ascii_case("cmd.exe"))
}

/// A spawn that never started: report it the way a shell would, and answer with
/// the shell's code — 127 not-found, 126 not-executable. `None` when the failure
/// is none of the command's doing, which leaves it an infra error for the caller
/// to raise (and 125).
///
/// One implementation, two callers, deliberately: [`guest`] classifies the guest's
/// spawn and [`host::exec`]'s native path classifies its own, and `--or-native`
/// only sells what it sells — the same task line meaning the same thing in a guest
/// and on a CI runner that already is the target OS — if the two cannot drift
/// (#24). They were separate copies of this match once; this is where that stops.
///
/// `path` is the PATH the child was actually going to be searched on, which is the
/// caller's to know: the agent augments the guest's, `-e PATH=…` overrides either,
/// and neither is the PATH of the shell the reader is standing in. It is printed
/// on a not-found because a not-found is a statement *about* it (#25).
pub(crate) fn spawn_failure(
    err: &std::io::Error,
    program: &str,
    path: Option<&str>,
) -> Option<i32> {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            notice!("vm: command not found: {program}");
            notice!("{}", advise::path_searched(path, cfg!(windows)));
            if let Some(note) = advise::half_posix_path_note(path, cfg!(windows)) {
                notice!("vm ▸ note: {note}");
            }
            Some(127)
        }
        std::io::ErrorKind::PermissionDenied => {
            notice!("vm: command not executable: {program}");
            Some(126)
        }
        _ => None,
    }
}

/// The `PATH` a caller set with `-e`, if they set one at all. Env names are
/// case-insensitive on Windows (where the usual spelling is `Path`) and
/// case-sensitive on unix — matched the same way [`std::process::Command`] itself
/// matches them, or the report would name a PATH the child never got.
pub(crate) fn path_override(env: &std::collections::BTreeMap<String, String>) -> Option<&str> {
    env.iter()
        .find(|(name, _)| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case("PATH")
            } else {
                name.as_str() == "PATH"
            }
        })
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// The regression [`super::command_for`] exists for: through `.args()`,
    /// cmd would receive `echo \"QQ\"` and print the backslashes.
    #[cfg(windows)]
    #[test]
    fn a_quoted_script_reaches_cmd_verbatim() {
        let out = super::command_for(&argv(&["cmd", "/C", r#"echo "QQ""#]))
            .output()
            .expect("cmd runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#""QQ""#);
    }

    /// The unix path is plain `.args()` — the quotes are `sh`'s to consume.
    #[cfg(unix)]
    #[test]
    fn a_quoted_script_reaches_sh_verbatim() {
        let out = super::command_for(&argv(&["sh", "-c", r#"echo "QQ""#]))
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "QQ");
    }
}
