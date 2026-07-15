//! The Ubuntu guest's 90-second shutdown, and the systemd unit that cures it.
//!
//! Parallels' drag-and-drop agent (`prldnd`, forked by `prlcc`, which
//! gnome-session autostarts) does not exit when the GNOME session ends, and it
//! ignores the SIGTERM systemd sends it at shutdown. `gnome-session-binary`
//! therefore sits in its session scope until systemd's 90s
//! `DefaultTimeoutStopSec` runs out and SIGKILLs the lot. Every graceful stop
//! of the lin guest cost 92–99s (measured), against the 120s after which
//! Parallels stops asking and force-kills the VM — about twenty seconds
//! between a clean shutdown and a yanked power cord.
//!
//! `vm deploy` installs [`UNIT`], which SIGKILLs prldnd as the shutdown
//! transaction opens. Measured after: a 4.3s stop, `session-2.scope` gone in
//! the same second, and no timeouts in the journal. `vm doctor` checks it is
//! still there, because a guest that quietly loses it goes back to being twenty
//! seconds from a force-kill and nothing else would say so.
//!
//! Four details in that unit are load-bearing, and each cost a wrong attempt:
//!
//! - **SIGKILL, not SIGTERM.** An interactive `pkill prldnd` works, so a plain
//!   TERM looks right when tested by hand — and does nothing at an actual
//!   shutdown, which is the only time it matters.
//! - **`DefaultDependencies=no`.** It is what lets the stop job run with no
//!   ordering constraints, i.e. in the same second the shutdown starts. With
//!   the default dependencies the unit is ordered *after* the very session
//!   teardown it exists to unblock, and the kill lands 90s too late.
//! - **The `-` on `ExecStop`.** `pkill` exits 1 when nothing matched — prldnd
//!   already gone, or a future Parallels having fixed it — and that is a no-op,
//!   not a failed unit.
//! - **Mode 0644.** systemd logs `marked world-inaccessible` for a unit file it
//!   cannot read as non-root, on every reload. Harmless, but `cat >` under
//!   root's umask lands on 0600, so the mode has to be set explicitly.
//!
//! Why not `chmod -x /usr/bin/prldnd`: Parallels Tools is not dpkg-managed
//! (there is no `dpkg-divert` to reach for), and its updater `rm -f`s every
//! file it owns before reinstalling it — so any edit to the binary is reverted,
//! silently, by the next Tools update. Nothing under `/etc/systemd/system` is
//! on that list. Why not a `DefaultTimeoutStopSec=10s` drop-in: it shortens the
//! hang instead of removing it, and still SIGKILLs gnome-session every time.

use crate::config::VmConfig;
use crate::crumb;
use crate::prl;
use crate::ssh::{self, SshTarget};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::Stdio;

pub const UNIT_NAME: &str = "vm-prldnd-shutdown.service";
pub const UNIT_PATH: &str = "/etc/systemd/system/vm-prldnd-shutdown.service";

/// The unit, verbatim. `vm doctor` compares the guest's copy against this
/// text, so an older vm's version is reported as stale rather than passing.
pub const UNIT: &str = "\
[Unit]
Description=vm: kill prldnd at shutdown so gnome-session can exit promptly
Documentation=https://github.com/akesson/vm
DefaultDependencies=no
Conflicts=shutdown.target
Before=shutdown.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/true
ExecStop=-/usr/bin/pkill -9 -x prldnd

[Install]
WantedBy=multi-user.target
";

/// Install and enable the unit in a linux guest.
///
/// Over `prlctl exec`, not ssh: the unit is root-owned, and sudo in the guest
/// wants a password on a tty that a non-interactive ssh does not have. The unit
/// text goes over **stdin**, never argv — `prlctl exec` re-joins its argv into
/// one string and hands it to a guest shell, so every quote and newline in it
/// would be the shell's to mangle (and a long argv wedges the channel outright,
/// see [`prl::exec_elevated`]).
pub fn install(alias: &str, vm: &VmConfig) -> Result<()> {
    let script = format!(
        "cat > {UNIT_PATH} && chmod 0644 {UNIT_PATH} && \
         systemctl daemon-reload && systemctl enable --now {UNIT_NAME}"
    );
    let mut child = prl::exec_elevated(&vm.parallels_name, &[&script])?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn prlctl exec")?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin
        .write_all(UNIT.as_bytes())
        .context("writing the unit file to the guest")?;
    drop(stdin); // the EOF that ends the guest's `cat`
    let out = child
        .wait_with_output()
        .context("waiting for prlctl exec to finish")?;
    if !out.status.success() {
        bail!(
            "could not install {UNIT_NAME} in '{}' (exit {:?}): {}\n  \
             Without it the guest takes ~95s to shut down and Parallels force-kills \
             it at 120s.",
            vm.parallels_name,
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    crumb!("vm ▸ {alias} ▸ {UNIT_NAME} installed");
    Ok(())
}

/// What the guest's copy of the unit is worth at the next shutdown.
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    /// Enabled, active, and the text this vm would install.
    Good,
    /// No unit file at all — the guest is back to the 90s hang.
    Missing,
    /// The file is there but systemd will not pull it in at boot.
    NotEnabled(String),
    /// Enabled but not active *now*: a oneshot only runs `ExecStop` while it is
    /// active, so an inactive unit is a unit that will not fire.
    Inactive(String),
    /// An older vm's text. It may predate one of the details above, each of
    /// which is the difference between working and silently doing nothing.
    Stale,
}

/// Separates the systemd properties from the file body in [`probe_command`]'s
/// output. A line no unit file can contain.
const MARKER: &str = "\n---\n";

/// One guest command that answers every question [`assess`] asks. Unprivileged:
/// `systemctl show` needs no root and the unit file is 0644, so this rides the
/// same ssh doctor already has open.
///
/// It ends in `|| true` because the missing-unit case is the one that matters
/// most and the one a bare `cat` would ruin: with no file to read, `cat` exits
/// 1, the whole probe looks like a *broken* probe, and the guest that most
/// needs "run `vm deploy`" gets "probe failed" and an empty stderr instead.
/// A real transport failure still surfaces — ssh's own exit code is separate.
pub fn probe_command() -> String {
    format!(
        "systemctl show --property=ActiveState --property=UnitFileState {UNIT_NAME}; \
         echo ---; cat {UNIT_PATH} 2>/dev/null || true"
    )
}

/// Read [`probe_command`]'s output.
///
/// The properties are matched by name, not position: `systemctl show` prints
/// them in its own order, not the order they were asked for (verified — it
/// answers `ActiveState` first whichever way round the flags go).
pub fn assess(stdout: &str) -> State {
    let (props, body) = stdout.split_once(MARKER).unwrap_or((stdout, ""));
    let file_state = property(props, "UnitFileState");
    let active = property(props, "ActiveState");
    if file_state.is_empty() {
        return State::Missing; // systemd knows no such unit
    }
    if file_state != "enabled" {
        return State::NotEnabled(file_state.to_string());
    }
    if active != "active" {
        return State::Inactive(active.to_string());
    }
    if body.trim_end() != UNIT.trim_end() {
        return State::Stale;
    }
    State::Good
}

fn property<'a>(props: &'a str, key: &str) -> &'a str {
    props
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim())
        .unwrap_or("")
}

/// Ask a linux guest about its unit (`vm doctor`).
pub fn check(target: &SshTarget) -> Result<State> {
    let probe = probe_command();
    let out = ssh::run_capture(target, &["sh", "-c", &ssh::shell_quote(&probe)])?;
    if !out.status.success() {
        bail!(
            "probe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(assess(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of these is a detail whose absence makes the unit compile, enable,
    /// activate, report healthy — and do nothing at the shutdown it exists for.
    /// They are asserted so a later tidy-up cannot quietly drop one.
    #[test]
    fn the_unit_keeps_the_four_details_that_make_it_work() {
        // SIGKILL: prldnd ignores the SIGTERM systemd sends at shutdown, even
        // though an interactive TERM kills it.
        assert!(
            UNIT.contains("ExecStop=-/usr/bin/pkill -9 -x prldnd"),
            "{UNIT}"
        );
        // Leading `-`: pkill exits 1 when nothing matched, which is a no-op.
        assert!(UNIT.contains("ExecStop=-"), "{UNIT}");
        // No default deps: what lets the stop run before the session teardown
        // it unblocks, rather than after it.
        assert!(UNIT.contains("DefaultDependencies=no"), "{UNIT}");
        // Conflicts+Before: what gets a stop job queued at all when the system
        // goes down.
        assert!(UNIT.contains("Conflicts=shutdown.target"), "{UNIT}");
        assert!(UNIT.contains("Before=shutdown.target"), "{UNIT}");
        // RemainAfterExit: a oneshot that is not active gets no ExecStop.
        assert!(UNIT.contains("RemainAfterExit=yes"), "{UNIT}");
        assert!(UNIT.contains("WantedBy=multi-user.target"), "{UNIT}");
    }

    /// Real output from the lin guest, unit installed and healthy.
    #[test]
    fn a_healthy_guest_reads_good() {
        let out = format!("ActiveState=active\nUnitFileState=enabled\n---\n{UNIT}");
        assert_eq!(assess(&out), State::Good);
    }

    /// Real shape from a guest with no such unit: systemctl still answers, with
    /// an empty file state.
    #[test]
    fn an_unknown_unit_reads_missing() {
        let out = "ActiveState=inactive\nUnitFileState=\n---\n";
        assert_eq!(assess(out), State::Missing);
    }

    /// The probe must not report *failure* on the guest it exists to catch.
    /// Without the trailing `|| true`, `cat` of the absent unit file exits 1,
    /// the probe reads as broken, and the un-deployed guest — the one case this
    /// check is for — is told "probe failed" instead of "run `vm deploy`".
    /// Regression: it shipped that way, and only a removed unit found it.
    #[test]
    fn the_probe_survives_the_unit_being_absent() {
        assert!(probe_command().ends_with("|| true"), "{}", probe_command());
    }

    #[test]
    fn a_disabled_or_inactive_unit_is_not_good() {
        let disabled = format!("ActiveState=active\nUnitFileState=disabled\n---\n{UNIT}");
        assert_eq!(assess(&disabled), State::NotEnabled("disabled".to_string()));
        // Enabled but not started: `systemctl stop` it, or a boot where it
        // failed, and the ExecStop never runs — the hang is back, silently.
        let inactive = format!("ActiveState=inactive\nUnitFileState=enabled\n---\n{UNIT}");
        assert_eq!(assess(&inactive), State::Inactive("inactive".to_string()));
    }

    /// A guest deployed by an older vm keeps whatever unit that vm wrote —
    /// including, possibly, one missing a detail above. Say so instead of
    /// passing it.
    #[test]
    fn an_older_units_text_reads_stale() {
        let old = "ActiveState=active\nUnitFileState=enabled\n---\n\
                   [Unit]\nDescription=old\n[Service]\nExecStop=/usr/bin/pkill prldnd\n";
        assert_eq!(assess(old), State::Stale);
    }

    /// systemd is free to print the properties in either order, and does not
    /// use the one they were requested in — so they are read by name.
    #[test]
    fn properties_are_read_by_name_not_position() {
        let swapped = format!("UnitFileState=enabled\nActiveState=active\n---\n{UNIT}");
        assert_eq!(assess(&swapped), State::Good);
    }

    /// A trailing newline more or less on the guest's copy is not a difference
    /// worth telling anyone to redeploy over.
    #[test]
    fn trailing_whitespace_is_not_staleness() {
        let out = format!("ActiveState=active\nUnitFileState=enabled\n---\n{UNIT}\n\n");
        assert_eq!(assess(&out), State::Good);
    }
}
