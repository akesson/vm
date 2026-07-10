//! Process exit codes vm reserves for its *own* failures, kept distinct from a
//! guest command's own exit status (which passes through untouched). A caller
//! — a shell, a `mise` fan-out — can then tell "vm itself failed" apart from
//! "the command I asked for failed," and retry only the former.
//!
//! The split mirrors ssh/docker: one reserved code for operational failure,
//! plus the shell's usual usage code. Everything vm does that returns an error
//! is one of these two; a guest command's nonzero exit never becomes an `Err`.

/// Operational/infra failure: sync, agent RPC, ssh/prlctl transport, VM
/// lifecycle, lock IO. "vm itself failed; the command may not have run." Often
/// transient, so a caller may retry. Matches docker's "error with the tool
/// itself" convention.
pub const INFRA: i32 = 125;

/// Usage/config error: the invocation or the machine's config is wrong (unknown
/// alias/target/OS, unreadable or invalid config, run outside a git repo).
/// "Fix your setup; retrying won't help." Shares clap's own arg-error code on
/// purpose — both mean the same thing to a caller.
pub const USAGE: i32 = 2;

/// Tags an error as usage/config ([`USAGE`], exit 2) rather than the default
/// operational failure ([`INFRA`], exit 125). `main` downcasts to this at the
/// top-level boundary to pick the exit code; because `anyhow::Error::downcast_ref`
/// walks the cause chain, the tag survives intermediate `.context(..)` layers.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Build a usage/config error as an `anyhow::Error`, for `?` / `return Err(..)`
/// / `ok_or_else(..)` at the sites where the user's setup is at fault.
pub fn usage(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(msg.into()))
}
