//! `vm run` — a command in a guest, with no repo and no sync behind it.
//!
//! [`super::host`] exists to run *this repo's code* in a guest: it finds the
//! repo, syncs it, and runs in the checkout. `vm run` is for the work that has
//! nothing to do with a repo — patching a guest, installing a tool, asking what
//! version of something it has — and it therefore needs neither, so it requires
//! neither. It runs in the guest user's home.
//!
//! Two things it has that exec deliberately does not:
//!
//! **`--elevated`** runs as root (linux/macos) or SYSTEM (windows) through
//! Parallels Tools ([`prl::exec_elevated`]). This is the only elevation there
//! is: sudo over ssh wants a password, and the Windows guest user is not an
//! administrator. It is what makes `apt-get upgrade`, `softwareupdate` and
//! Windows Update reachable from the host at all.
//!
//! **A stdin payload.** `vm exec` never forwards stdin — the host↔agent pipe is
//! its liveness channel, and the guest command reads the null device. `vm run`
//! reads what was piped or redirected into it and sends it *inside* the request,
//! so `vm run linux --elevated -- sh < step.sh` feeds the script to the guest's
//! shell. That is what a script over this transport must do: `prlctl exec` hangs
//! forever, silently, once its command line passes a few KB (see
//! [`prl::exec_elevated`]), so a script can never be argv — but on stdin there
//! is no such limit.

use crate::config::{Config, GuestOs};
use crate::exit::usage;
use crate::guest_env::GuestEnv;
use crate::proto::{ExecRequest, PROTO_VERSION};
use crate::{commands, prl};
use anyhow::{Context, Result};
use std::io::Read;
use std::time::Instant;

/// Lib-side mirror of the CLI run flags.
pub struct RunOptions {
    /// Run as root (linux/macos) / SYSTEM (windows) via Parallels Tools.
    pub elevated: bool,
    /// `NAME=value` / bare `NAME` specs from `-e`.
    pub env: Vec<String>,
    pub cmd: Vec<String>,
}

/// The most stdin vm will carry to a guest command. A script — the reason this
/// exists — is kilobytes; the cap is here so that `vm run … < ten-gigabyte.iso`
/// fails with a sentence instead of by exhausting memory on both sides.
const STDIN_CAP: usize = 8 * 1024 * 1024;

pub fn run(alias: &str, opts: &RunOptions) -> Result<i32> {
    // Before anything costs a VM resume: a typo'd `-e` spec is the caller's own
    // invocation and retrying it will never help (see `host::exec`).
    let env = super::host::resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    reject_exec_flags(&opts.cmd)?;
    // Read the payload before the guest is even woken — it is the caller's, and
    // an oversized or binary one must fail without having touched a VM. Where
    // `vm exec` *warns* about input on its stdin, run consumes it: that is the
    // whole difference between the two, so it is read at the same point exec
    // would have complained.
    let payload = match super::host::stdin_source() {
        Some(_) => Some(read_payload(std::io::stdin().lock())?),
        None => None,
    };

    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;

    // Registers this run as a use of the VM, exactly as exec does, so reap
    // cannot suspend the guest out from under a long `apt-get upgrade`.
    let _use = crate::lock::shared(alias)?;

    let transport = if opts.elevated {
        // No ssh anywhere on this path — Parallels Tools is the transport, so
        // Tools is what we wait for.
        commands::bring_up_elevated(alias, vm)?;
        prl::exec_elevated(
            &vm.parallels_name,
            &[&commands::agent_abs_path(vm), "_exec"],
        )?
    } else {
        let target = commands::bring_up(alias, vm)?;
        super::host::agent_exec_command(vm, &target)?
    };

    // The guest user's home, spelled out rather than left as `~`: under
    // `--elevated` the agent runs as root/SYSTEM, whose `$HOME` is not the one
    // the caller means. Same cwd either way — one rule, no mode to remember.
    let cwd = commands::guest_home(vm);
    let req = ExecRequest {
        version: PROTO_VERSION,
        argv: super::host::build_argv(
            &opts.cmd,
            // No repo, so nothing to detect a guest env *from* — and no checkout
            // whose tools would need wrapping. mise can only reach a guest
            // command through this wrap, so it is excluded by construction.
            &crate::guest_env::resolve(Some(GuestEnv::None), std::path::Path::new(".")),
            vm.os,
        ),
        env,
        cwd: cwd.clone(),
        stdin: payload,
    };

    // Who and where, before what: an elevated run is the one case where the
    // command's identity is not the one the config implies, and a breadcrumb
    // that left that out would be the wrong kind of quiet.
    let who = match (opts.elevated, vm.os) {
        (false, _) => String::new(),
        (true, GuestOs::Windows) => " ▸ elevated (SYSTEM)".to_string(),
        (true, _) => " ▸ elevated (root)".to_string(),
    };
    eprintln!(
        "vm ▸ {alias} ({}) ▸ {cwd}{who} ▸ $ {}",
        vm.parallels_name,
        super::host::render_argv(&req.argv)
    );
    // Said out loud because it contradicts what `vm exec` taught: there, input on
    // vm's stdin is dropped and noted. Here it travels, and the reader has to be
    // able to tell the two apart at a glance.
    if let Some(payload) = &req.stdin {
        eprintln!(
            "vm ▸ {alias} ▸ stdin ▸ {} bytes → the guest command",
            payload.len()
        );
    }
    let started = Instant::now();

    let code = super::host::drive_agent(alias, transport, &req)?;
    eprintln!(
        "vm ▸ {alias} ▸ exit {code} ▸ {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(code)
}

/// `vm exec`'s flags, and why each one cannot exist here. All of them are about
/// a repo, which run does not have.
const EXEC_ONLY_FLAGS: &[(&str, &str)] = &[
    ("--no-sync", "run never syncs — there is nothing to skip"),
    (
        "--writeback",
        "run has no synced repo to write changes back to",
    ),
    ("--with-file", "run syncs no files at all"),
    ("--with-snapshot", "not available on run"),
    ("--or-native", "not available on run"),
    (
        "--guest-env",
        "run has no repo to detect a guest env from, and wraps nothing",
    ),
];

/// Refuse an exec-only flag that clap swallowed into the command.
///
/// `cmd` is `trailing_var_arg`, so `vm run lin --no-sync -- true` does not fail
/// to parse: the flag lands in the *command*, and the guest goes looking for a
/// binary called `--no-sync` and comes back with a 127 — having resumed a VM to
/// get there. A caller reaching for these has the wrong verb in mind, and the
/// one thing that must not happen is that vm quietly runs something else (cf.
/// `host::reject_removed_flags`, which exists for exactly this reason).
fn reject_exec_flags(cmd: &[String]) -> Result<()> {
    let Some(first) = cmd.first() else {
        return Ok(());
    };
    // Only in the leading position: past the command name these are the guest
    // command's own flags, and none of vm's business (`vm run lin -- git log
    // --no-sync` is a fine thing to want).
    let name = first.split('=').next().unwrap_or(first);
    let Some((flag, why)) = EXEC_ONLY_FLAGS.iter().find(|(f, _)| *f == name) else {
        return Ok(());
    };
    Err(usage(format!(
        "`{flag}` is not a `vm run` flag — {why}.\n  \
         run takes no repo and does no sync; it runs in the guest user's home.\n  \
         To run against this repo's checkout in the guest, that is `vm exec` \
         (which has {flag})."
    )))
}

/// Read what the caller wired into vm's stdin, for the guest command to read.
///
/// Both refusals are usage errors (exit 2), not infra: they are about the input
/// the caller chose, and no retry changes either. Text only — the payload rides
/// inside the request's JSON, which has no way to spell a byte that is not
/// valid UTF-8, so binary is refused rather than mangled into U+FFFD.
fn read_payload(reader: impl Read) -> Result<String> {
    let mut buf = Vec::new();
    // One byte over the cap is enough to know it was exceeded, and stops a huge
    // input from being read into memory just to be rejected.
    reader
        .take(STDIN_CAP as u64 + 1)
        .read_to_end(&mut buf)
        .context("reading the stdin payload")?;
    if buf.len() > STDIN_CAP {
        return Err(usage(format!(
            "the input on vm's stdin is larger than the {} MiB vm will carry to a guest \
             command.\n  \
             stdin is for a script or a modest amount of text; for anything bigger, fetch \
             it in the guest, or put it in a repo and use `vm exec`.",
            STDIN_CAP / 1024 / 1024
        )));
    }
    String::from_utf8(buf).map_err(|_| {
        usage(
            "the input on vm's stdin is not valid UTF-8 text.\n  \
             The guest command's stdin travels inside the request, which carries text — \
             binary input would have to be mangled to fit, so vm refuses it instead.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::UsageError;

    fn payload(bytes: &[u8]) -> Result<String> {
        read_payload(bytes)
    }

    #[test]
    fn a_script_sized_payload_arrives_byte_for_byte() {
        let script = "set -e\napt-get update\nexit 0\n";
        assert_eq!(payload(script.as_bytes()).unwrap(), script);
    }

    #[test]
    fn an_empty_payload_is_still_a_payload() {
        // `vm run lin -- sh < /dev/null` runs sh on an empty script — an empty
        // payload and no payload at all are different requests, and only the
        // caller's fd 0 decides which this is.
        assert_eq!(payload(b"").unwrap(), "");
    }

    #[test]
    fn a_payload_at_the_cap_is_accepted_and_one_over_it_is_not() {
        let at_cap = vec![b'x'; STDIN_CAP];
        assert_eq!(payload(&at_cap).unwrap().len(), STDIN_CAP);

        let over = vec![b'x'; STDIN_CAP + 1];
        let err = payload(&over).unwrap_err();
        assert!(
            err.downcast_ref::<UsageError>().is_some(),
            "exit 2, not 125"
        );
        assert!(err.to_string().contains("8 MiB"), "{err:#}");
    }

    #[test]
    fn an_exec_only_flag_is_refused_with_the_verb_that_has_it() {
        // clap swallows these into the command (see cli.rs), so without this the
        // guest would 127 on a binary called `--no-sync` after a VM resume.
        for flag in ["--no-sync", "--writeback", "--with-file", "--guest-env"] {
            let cmd = vec![flag.to_string(), "true".to_string()];
            let err = reject_exec_flags(&cmd).unwrap_err();
            assert!(err.downcast_ref::<UsageError>().is_some(), "{flag}: exit 2");
            let msg = err.to_string();
            assert!(msg.contains(flag), "{msg}");
            assert!(msg.contains("vm exec"), "names the verb that has it: {msg}");
        }
        // `--with-file=.env` is the same flag, spelled with an `=`.
        assert!(reject_exec_flags(&["--with-file=.env".to_string()]).is_err());
    }

    /// Past the command name these are the *guest command's* flags. `vm run lin
    /// -- git log --no-sync` is a fine thing to want, and vm has no business
    /// second-guessing it.
    #[test]
    fn the_same_flag_inside_the_command_is_left_alone() {
        let cmd = ["git", "log", "--no-sync"].map(String::from).to_vec();
        assert!(reject_exec_flags(&cmd).is_ok());
        assert!(reject_exec_flags(&[]).is_ok());
    }

    #[test]
    fn binary_input_is_refused_rather_than_mangled() {
        // A lossy conversion here would hand the guest a script full of U+FFFD
        // and let it fail somewhere far away from the cause.
        let err = payload(&[0x1f, 0x8b, 0x08, 0xff, 0xfe]).unwrap_err();
        assert!(
            err.downcast_ref::<UsageError>().is_some(),
            "exit 2, not 125"
        );
        assert!(err.to_string().contains("UTF-8"), "{err:#}");
    }
}
