//! `vm reap` — suspend VMs that nobody is using.
//!
//! For each configured (or the named) VM: skip it while any `vm` process
//! holds a use on it, skip it inside the idle window, skip it while someone
//! is at its console (guest input idle below the window — manual GUI use
//! leaves no trace in the lock files), otherwise suspend it. The console
//! probe fails open: reclaiming RAM stays guaranteed, and a wrongly
//! suspended VM costs one ~1s resume.
//! Suspend, not stop: it frees the VM's host RAM, and the next `vm exec`
//! resumes in about a second instead of paying a full boot.
//!
//! Meant to run from a launchd interval job (`vm reap --install`); no
//! long-running daemon of our own.

use crate::config::Config;
use crate::{lock, prl};
use anyhow::{Context, Result, bail};
use std::time::Duration;

pub fn reap(alias: Option<&str>, idle_minutes: u64) -> Result<i32> {
    let cfg = Config::load()?;
    if let Some(a) = alias {
        cfg.get(a)?; // fail loudly on typos instead of silently reaping nothing
    }
    let idle_limit = Duration::from_secs(idle_minutes * 60);

    for (name, vm) in &cfg.vm {
        if alias.is_some_and(|a| a != name) {
            continue;
        }
        if prl::find(&vm.parallels_name)?.status != "running" {
            continue;
        }
        let Some(_claim) = lock::try_exclusive(name)? else {
            eprintln!("vm ▸ reap ▸ {name} in use — skipped");
            continue;
        };
        // Idle time counts from the end of the last use; a lock file that
        // does not exist yet gets created by try_exclusive with mtime = now,
        // so a never-used VM becomes reapable one idle window from now.
        let idle = lock::last_use(name)
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::ZERO);
        if idle < idle_limit {
            eprintln!(
                "vm ▸ reap ▸ {name} idle {}m of {}m — kept",
                idle.as_secs() / 60,
                idle_minutes
            );
            continue;
        }
        match crate::idle::input_idle(vm) {
            Ok(input) if input < idle_limit => {
                eprintln!(
                    "vm ▸ reap ▸ {name} console input {}m ago — kept",
                    input.as_secs() / 60
                );
                continue;
            }
            Ok(_) => {}
            Err(err) => eprintln!(
                "vm ▸ reap ▸ {name} input-idle probe failed ({err:#}) — suspending anyway"
            ),
        }
        prl::suspend(&vm.parallels_name)?;
        eprintln!(
            "vm ▸ reap ▸ {name} suspended after {}m idle (any `vm exec` resumes it)",
            idle.as_secs() / 60
        );
    }
    Ok(0)
}

pub const LAUNCHD_LABEL: &str = "com.akesson.vm.reap";
/// Sweep every 5 minutes; with the default 30m idle window a VM is suspended
/// at most ~35m after its last use.
const LAUNCHD_INTERVAL_SECS: u32 = 300;

pub fn plist_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(std::path::PathBuf::from(home).join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

/// Install (or update) the launchd interval job running `vm reap`.
pub fn install(idle_minutes: u64) -> Result<i32> {
    let exe = std::env::current_exe().context("cannot resolve own binary path")?;
    let exe = exe
        .to_str()
        .context("binary path is not valid UTF-8")?
        .to_string();
    let log = crate::sync::expand_home("~/Library/Logs/vm-reap.log")?;
    // launchd jobs get a bare PATH without /usr/local/bin, where prlctl lives.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>reap</string>
        <string>--idle-minutes</string>
        <string>{idle_minutes}</string>
    </array>
    <key>StartInterval</key><integer>{LAUNCHD_INTERVAL_SECS}</integer>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key><string>/usr/local/bin:/usr/bin:/bin</string>
    </dict>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        log = log.display(),
    );
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
    std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

    // Re-bootstrap so an updated plist takes effect; ignore "not loaded".
    let _ = launchctl(&["bootout", &gui_domain_target()]);
    launchctl(&["bootstrap", &gui_domain(), &path.display().to_string()])?;
    eprintln!(
        "vm ▸ reap ▸ launchd job {LAUNCHD_LABEL} installed: every {}m, suspend VMs idle ≥{idle_minutes}m\n\
         vm ▸ reap ▸ runs {exe} — reinstall after moving the binary (`vm reap --install`)",
        LAUNCHD_INTERVAL_SECS / 60,
    );
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    let _ = launchctl(&["bootout", &gui_domain_target()]);
    let path = plist_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    eprintln!("vm ▸ reap ▸ launchd job removed");
    Ok(0)
}

/// Is the launchd job currently loaded?
pub fn launchd_loaded() -> bool {
    launchctl(&["print", &gui_domain_target()]).is_ok()
}

fn gui_domain() -> String {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    format!("gui/{uid}")
}

fn gui_domain_target() -> String {
    format!("{}/{LAUNCHD_LABEL}", gui_domain())
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
