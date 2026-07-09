//! Command execution: the guest agent (`vm _exec`) spawns argv natively from
//! a JSON request — no shell quoting layer anywhere between the host CLI and
//! the guest process. The child is wrapped in a kill-tree (job object on
//! Windows, process group on unix) so cancelling on the host never leaves
//! orphaned compilers in the guest.

pub mod guest;
pub mod host;

#[cfg(unix)]
#[path = "job_unix.rs"]
mod job;
#[cfg(windows)]
#[path = "job_windows.rs"]
mod job;
