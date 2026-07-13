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
