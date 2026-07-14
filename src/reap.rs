//! `vm reap` — shut down VMs that nobody is using.
//!
//! For each configured (or the named) VM: skip it while any `vm` process
//! holds a use on it, skip it inside the idle window, skip it while someone
//! is at its console (guest input idle below the window — manual GUI use
//! leaves no trace in the lock files), otherwise shut it down. The console
//! probe fails open: reclaiming RAM stays guaranteed, and a wrongly
//! stopped VM costs one boot (seconds).
//!
//! Idleness is measured from the per-VM lock file, which only `vm` touches —
//! so a VM started by hand (`prlctl start`) still looks idle here and gets
//! shut down at the next sweep. That is working as intended, but it is worth
//! knowing when a hand-started VM goes back down on its own.
//! Stop, not suspend: reap used to suspend, until suspension proved the
//! unreliable half of the pair — see [`prl::stop`] for what went wrong. A
//! boot costs seconds more than a resume, and that is the whole price.
//!
//! Meant to run from a launchd interval job (`vm reap --install`); no
//! long-running daemon of our own.

use crate::config::Config;
use crate::{crumb, lock, notice, prl};
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
            crumb!("vm ▸ reap ▸ {name} in use — skipped");
            continue;
        };
        // Idle time counts from the end of the last use; a lock file that
        // does not exist yet gets created by try_exclusive with mtime = now,
        // so a never-used VM becomes reapable one idle window from now.
        let idle = lock::last_use(name)
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::ZERO);
        if idle < idle_limit {
            crumb!(
                "vm ▸ reap ▸ {name} idle {}m of {}m — kept",
                idle.as_secs() / 60,
                idle_minutes
            );
            continue;
        }
        match crate::idle::input_idle(vm) {
            Ok(input) if input < idle_limit => {
                crumb!(
                    "vm ▸ reap ▸ {name} console input {}m ago — kept",
                    input.as_secs() / 60
                );
                continue;
            }
            Ok(_) => {}
            Err(err) => notice!(
                "vm ▸ reap ▸ {name} input-idle probe failed ({err:#}) — shutting down anyway"
            ),
        }
        prl::stop(&vm.parallels_name)?;
        crumb!(
            "vm ▸ reap ▸ {name} shut down after {}m idle (any `vm exec` boots it)",
            idle.as_secs() / 60
        );
    }
    Ok(0)
}

pub const LAUNCHD_LABEL: &str = "com.akesson.vm.reap";
/// Sweep every 5 minutes; with the default 30m idle window a VM is shut down
/// at most ~35m after its last use.
const LAUNCHD_INTERVAL_SECS: u32 = 300;

/// The log launchd used to keep for us, until v0.3. It had no timestamps —
/// launchd's `StandardOutPath` is a raw fd redirect and adds nothing — and
/// nothing rotated it, so the one file you would open to ask why a VM went down
/// at 3pm could not tell you when anything happened. [`install`] deletes it; the
/// journal ([`crate::journal`]) replaces it.
const LEGACY_LOG: &str = "~/Library/Logs/vm-reap.log";

pub fn plist_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(std::path::PathBuf::from(home).join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

/// The plist, as a pure function of the three things that vary in it — so the
/// details that make it work can be asserted without going near launchd. (The
/// systemd unit in [`crate::prldnd`] is kept honest the same way.)
fn plist(exe: &str, idle_minutes: u64, gutter: &std::path::Path) -> String {
    // `--quiet` because there is no terminal here: the sweep's narration belongs
    // in the journal, and stderr under launchd only leads to the gutter below.
    // Warnings are `notice!` and print anyway — quiet suppresses narration, not
    // news.
    //
    // launchd jobs get a bare PATH without /usr/local/bin, where prlctl lives.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--quiet</string>
        <string>reap</string>
        <string>--idle-minutes</string>
        <string>{idle_minutes}</string>
    </array>
    <key>StartInterval</key><integer>{LAUNCHD_INTERVAL_SECS}</integer>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key><string>/usr/local/bin:/usr/bin:/bin</string>
    </dict>
    <key>StandardOutPath</key><string>{gutter}</string>
    <key>StandardErrorPath</key><string>{gutter}</string>
</dict>
</plist>
"#,
        gutter = gutter.display(),
    )
}

/// Whether the installed plist predates the journal — it still points launchd at
/// the retired `~/Library/Logs` file, or it never learned `--quiet`. Such a job
/// goes on growing a log nobody rotates, so [`crate::doctor`] tells you to
/// reinstall. Pure so it can be tested against the shape vm actually shipped.
fn is_stale(plist: &str) -> bool {
    plist.contains("Library/Logs") || !plist.contains("<string>--quiet</string>")
}

/// Is the plist on disk one of the pre-journal ones? `false` when no job is
/// installed at all — that is [`launchd_loaded`]'s news to report, not ours.
pub fn plist_is_stale() -> bool {
    plist_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|text| is_stale(&text))
}

/// Install (or update) the launchd interval job running `vm reap`.
pub fn install(idle_minutes: u64) -> Result<i32> {
    let exe = std::env::current_exe().context("cannot resolve own binary path")?;
    let exe = exe
        .to_str()
        .context("binary path is not valid UTF-8")?
        .to_string();

    let gutter = crate::journal::gutter_path().context("cannot resolve the log directory")?;
    // launchd creates no parent directories for StandardErrorPath: with the dir
    // missing, the gutter's output is dropped on the floor — and the gutter only
    // ever holds output from a catastrophe vm never saw, which is precisely the
    // output worth not dropping. The journal makes this dir on its own first
    // write, but that lands *after* the bootstrap below, and never at all under
    // `VM_JOURNAL=off`. So make it here, while it is still ours to make.
    let log_dir = gutter.parent().context("gutter path has no parent")?;
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("cannot create log dir {}", log_dir.display()))?;

    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
    std::fs::write(&path, plist(&exe, idle_minutes, &gutter))
        .with_context(|| format!("writing {}", path.display()))?;

    // Re-bootstrap so an updated plist takes effect; ignore "not loaded".
    let _ = launchctl(&["bootout", &gui_domain_target()]);
    launchctl(&["bootstrap", &gui_domain(), &path.display().to_string()])?;
    crumb!(
        "vm ▸ reap ▸ launchd job {LAUNCHD_LABEL} installed: every {}m, shut down VMs idle ≥{idle_minutes}m\n\
         vm ▸ reap ▸ runs {exe} — reinstall after moving the binary (`vm reap --install`)",
        LAUNCHD_INTERVAL_SECS / 60,
    );

    // Nothing else will ever tidy it: it is not launchd's to trim and it was
    // never vm's to write. Upgrading is the one moment we know to do it.
    if let Ok(legacy) = crate::sync::expand_home(LEGACY_LOG)
        && legacy.exists()
        && std::fs::remove_file(&legacy).is_ok()
    {
        crumb!(
            "vm ▸ reap ▸ removed the old {} — sweeps are recorded in the journal now, with the time they happened",
            legacy.display()
        );
    }
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    let _ = launchctl(&["bootout", &gui_domain_target()]);
    let path = plist_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    crumb!("vm ▸ reap ▸ launchd job removed");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample() -> String {
        plist(
            "/usr/local/bin/vm",
            30,
            Path::new("/home/x/.config/vm/log/reap-launchd.log"),
        )
    }

    /// Each of these is a detail whose absence makes the job install, load, run,
    /// reap VMs correctly — and quietly go back to keeping the unbounded,
    /// untimestamped log this release exists to retire. They are asserted so a
    /// later tidy-up cannot drop one.
    #[test]
    fn the_plist_keeps_the_details_that_put_the_sweep_in_the_journal() {
        let text = sample();

        // Without --quiet, every "idle 12m of 30m — kept" line the sweep prints
        // lands in the gutter instead of only the journal — the old unbounded
        // log under a new name.
        assert!(text.contains("<string>--quiet</string>"), "{text}");
        // launchd's Std*Path is a raw fd redirect: it stamps nothing. Both point
        // at the gutter, a file vm owns, beside a journal vm rotates.
        assert!(
            text.contains(
                "<key>StandardOutPath</key><string>/home/x/.config/vm/log/reap-launchd.log</string>"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "<key>StandardErrorPath</key><string>/home/x/.config/vm/log/reap-launchd.log</string>"
            ),
            "{text}"
        );
        assert!(
            !text.contains("Library/Logs"),
            "the retired log is gone: {text}"
        );
        // launchd hands the job a PATH without /usr/local/bin, where prlctl lives.
        assert!(
            text.contains("<key>PATH</key><string>/usr/local/bin:/usr/bin:/bin</string>"),
            "{text}"
        );
        assert!(
            text.contains("<key>StartInterval</key><integer>300</integer>"),
            "{text}"
        );
        assert!(text.contains("<string>--idle-minutes</string>"), "{text}");
        assert!(text.contains("<string>30</string>"), "{text}");
        assert!(
            text.contains("<string>/usr/local/bin/vm</string>"),
            "{text}"
        );
    }

    /// An existing install keeps running its old plist until someone reinstalls
    /// — and would go on feeding a log nobody rotates. doctor has to be able to
    /// see that, so this is the shape it must recognize.
    #[test]
    fn a_plist_from_before_the_journal_reads_as_stale() {
        let shipped_through_v0_2 = r#"    <array>
        <string>/usr/local/bin/vm</string>
        <string>reap</string>
    </array>
    <key>StandardOutPath</key><string>/home/x/Library/Logs/vm-reap.log</string>
"#;
        assert!(is_stale(shipped_through_v0_2));
        assert!(!is_stale(&sample()), "the one we write now is not stale");
    }

    /// The two failure modes are independent: a plist could be pointed at the
    /// gutter but still be missing `--quiet`.
    #[test]
    fn a_plist_without_quiet_reads_as_stale_even_with_a_good_gutter() {
        let no_quiet = sample().replace("        <string>--quiet</string>\n", "");
        assert!(!no_quiet.contains("--quiet"));
        assert!(is_stale(&no_quiet));
    }
}
