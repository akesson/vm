//! Guest input-idle probe: how long since the last keyboard/mouse input at
//! the guest's console. Reap consults it so a VM being used manually through
//! the Parallels GUI (invisible to the lock files) is not suspended mid-use.
//!
//! Guest side: the hidden `vm _idle` verb prints milliseconds since the last
//! input event. Host side: `input_idle` invokes it over the same transports
//! exec uses — ssh for linux/macos, the prlctl console session for windows,
//! where GetLastInputInfo is per-session and ssh would land in input-less
//! session 0.

use crate::config::{GuestOs, VmConfig};
use anyhow::{Context, Result};
use std::time::Duration;

/// Host side: ask the guest agent for the time since the last console input.
pub fn input_idle(vm: &VmConfig) -> Result<Duration> {
    let reply = match vm.os {
        GuestOs::Windows => {
            let cmd = format!("{} _idle", crate::commands::agent_console_path(vm));
            let out = crate::prl::exec_console_capture(&vm.parallels_name, &["cmd", "/c", &cmd])?;
            if !out.status.success() {
                anyhow::bail!(
                    "agent _idle failed via prlctl exec: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        GuestOs::Linux | GuestOs::Macos => {
            let target = crate::commands::ssh_target(vm)?;
            crate::commands::agent_call(vm, &target, &["_idle"])?
        }
    };
    let ms: u64 = reply
        .trim()
        .parse()
        .with_context(|| format!("agent _idle replied '{}'", reply.trim()))?;
    Ok(Duration::from_millis(ms))
}

/// Guest side (`vm _idle`): milliseconds since the last local input event.
#[cfg(windows)]
pub fn guest_idle_ms() -> Result<u64> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        GetLastInputInfo(&mut info)
            .ok()
            .context("GetLastInputInfo failed")?;
        // Both are ms since boot as u32; wrapping_sub stays correct across
        // the ~49.7-day tick rollover.
        Ok(GetTickCount().wrapping_sub(info.dwTime) as u64)
    }
}

/// Guest side (`vm _idle`): milliseconds since the last local input event.
/// Asks the compositor's idle monitor on the user's session bus — sshd gives
/// us no DBUS_SESSION_BUS_ADDRESS, but the bus socket path is deterministic.
#[cfg(target_os = "linux")]
pub fn guest_idle_ms() -> Result<u64> {
    let uid = unsafe { libc::getuid() };
    let out = std::process::Command::new("dbus-send")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        )
        .args([
            "--print-reply",
            "--dest=org.gnome.Mutter.IdleMonitor",
            "/org/gnome/Mutter/IdleMonitor/Core",
            "org.gnome.Mutter.IdleMonitor.GetIdletime",
        ])
        .output()
        .context("failed to run dbus-send")?;
    if !out.status.success() {
        anyhow::bail!(
            "dbus-send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_dbus_idletime(&String::from_utf8_lossy(&out.stdout))
}

/// Guest side (`vm _idle`): milliseconds since the last local input event,
/// from the HID system's system-wide idle counter.
#[cfg(target_os = "macos")]
pub fn guest_idle_ms() -> Result<u64> {
    let out = std::process::Command::new("ioreg")
        .args(["-c", "IOHIDSystem"])
        .output()
        .context("failed to run ioreg")?;
    if !out.status.success() {
        anyhow::bail!(
            "ioreg failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_hid_idle_ms(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the `uint64 <ms>` value out of a dbus-send `--print-reply` reply.
#[cfg(any(target_os = "linux", test))]
fn parse_dbus_idletime(reply: &str) -> Result<u64> {
    reply
        .split_whitespace()
        .skip_while(|w| *w != "uint64")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("no 'uint64 <ms>' in dbus reply: {}", reply.trim()))
}

/// Pull `"HIDIdleTime" = <ns>` out of ioreg output and convert to ms.
#[cfg(any(target_os = "macos", test))]
fn parse_hid_idle_ms(ioreg: &str) -> Result<u64> {
    ioreg
        .lines()
        .find(|l| l.contains("\"HIDIdleTime\""))
        .and_then(|l| l.rsplit('=').next())
        .and_then(|n| n.trim().parse::<u64>().ok())
        .map(|ns| ns / 1_000_000)
        .context("no HIDIdleTime in ioreg output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dbus_idletime_reply() {
        // Real reply shape from the lin guest (GNOME on Wayland).
        let reply = "method return time=1752131234.567890 sender=:1.34 -> \
                     destination=:1.99 serial=42 reply_serial=2\n   uint64 1928634\n";
        assert_eq!(parse_dbus_idletime(reply).unwrap(), 1_928_634);
        assert!(parse_dbus_idletime("method return time=1 sender=:1.34").is_err());
        assert!(parse_dbus_idletime("uint64 notanumber").is_err());
    }

    #[test]
    fn parses_hid_idle_time() {
        // Trimmed from real `ioreg -c IOHIDSystem` output.
        let ioreg = r#"
+-o IOHIDSystem  <class IOHIDSystem, id 0x100000456, registered, matched, active, busy 0 (2 ms), retain 8>
    | {
    |   "HIDIdleTime" = 466140625
    |   "HIDParameters" = {"key"=1}
    | }
"#;
        assert_eq!(parse_hid_idle_ms(ioreg).unwrap(), 466);
        assert!(parse_hid_idle_ms("no such key").is_err());
    }
}
