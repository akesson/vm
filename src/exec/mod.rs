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
