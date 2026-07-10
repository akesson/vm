//! macOS tier-0 checks: window-server session, TCC Accessibility trust, and
//! reading a known app's AX tree. The Dock is the target app — it is always
//! running when a user is logged in at the console. Tier-1: spawn an
//! osascript dialog and find its window through the AX API by title.

use std::ptr::NonNull;

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGSessionCopyCurrentDictionary, CGWindowListCopyWindowInfo, CGWindowListOption,
};

use crate::Report;

pub fn run() -> Report {
    let mut r = Report::new();

    // A window-server (Aqua) session is what a plain ssh process usually
    // lacks; the AX API needs one to reach on-screen apps.
    match CGSessionCopyCurrentDictionary() {
        Some(dict) => r.info(
            "gui-session",
            format!("window-server session present ({} keys)", dict.count()),
        ),
        None => r.info(
            "gui-session",
            "no window-server session for this process (typical for plain ssh)",
        ),
    }

    // The gate for all AX API use: TCC Accessibility trust. Cannot be
    // granted programmatically with SIP enabled.
    if unsafe { AXIsProcessTrusted() } {
        r.pass("ax-trusted", "process is a trusted accessibility client");
    } else {
        r.fail(
            "ax-trusted",
            "not TCC-trusted — grant Accessibility to this process's responsible binary \
             (System Settings ▸ Privacy & Security ▸ Accessibility)",
        );
    }

    // Window list works without AX trust but needs window-server access —
    // separates "no session" failures from "no TCC" failures.
    // 1 | 16 = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements
    let opts = CGWindowListOption::from_bits_retain(1 | 16);
    match CGWindowListCopyWindowInfo(opts, 0) {
        Some(list) => r.info("window-list", format!("{} on-screen windows", list.count())),
        None => r.info(
            "window-list",
            "CGWindowListCopyWindowInfo returned NULL (no window-server access)",
        ),
    }

    // Find the Dock: always present in a logged-in console session, so a
    // miss here means nobody is logged in at the console at all.
    let pid = match dock_pid() {
        Some(pid) => {
            r.pass("dock-pid", format!("Dock is running (pid {pid})"));
            pid
        }
        None => {
            r.fail(
                "dock-pid",
                "Dock is not running — no user logged in at the console?",
            );
            return r;
        }
    };

    // The actual tier-0 gate: read the Dock's AX tree.
    let dock = unsafe { AXUIElement::new_application(pid) };
    match copy_attribute(&dock, "AXRole") {
        Ok(value) => {
            let role = value
                .downcast::<CFString>()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "<non-string>".into());
            r.pass("ax-role", format!("Dock AXRole = {role:?}"));
        }
        Err(err) => {
            r.fail(
                "ax-role",
                format!("cannot read Dock AXRole: {}", ax_err(err)),
            );
            return r;
        }
    }
    match copy_attribute(&dock, "AXChildren") {
        Ok(value) => match value.downcast::<CFArray>() {
            Ok(children) if children.count() > 0 => r.pass(
                "ax-children",
                format!("Dock has {} accessible children", children.count()),
            ),
            Ok(_) => r.fail("ax-children", "Dock AXChildren is empty"),
            Err(_) => r.fail("ax-children", "Dock AXChildren is not an array"),
        },
        Err(err) => r.fail(
            "ax-children",
            format!("cannot read Dock AXChildren: {}", ax_err(err)),
        ),
    }
    if !r.ok() {
        return r;
    }

    fixture_check(&mut r);
    r
}

/// Tier-1: spawn an osascript dialog with a unique title and find its window
/// through the AX API — proves cross-process window discovery against a
/// process we started, not just reading the long-running Dock.
fn fixture_check(r: &mut Report) {
    let title = format!("a11y-fixture-{}", std::process::id());
    let script = format!(
        "display dialog \"a11y-probe fixture\" with title \"{title}\" \
         buttons {{\"OK\"}} giving up after 30"
    );
    let mut child = match std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            r.fail("fixture-spawn", format!("cannot spawn osascript: {e}"));
            return;
        }
    };
    r.pass("fixture-spawn", format!("osascript pid {}", child.id()));

    let found = find_window(child.id() as i32, &title);
    let _ = child.kill();
    let _ = child.wait();
    if found {
        r.pass("fixture-window", format!("found window {title:?} via AX"));
    } else {
        r.fail(
            "fixture-window",
            format!("no window titled {title:?} appeared in the AX tree within 10s"),
        );
    }
}

/// Poll the fixture process's AX element for a window with the given title.
/// AX errors while polling are expected (the app element isn't ready until
/// the dialog is up), so they just mean "keep waiting".
fn find_window(pid: i32, title: &str) -> bool {
    let app = unsafe { AXUIElement::new_application(pid) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(value) = copy_attribute(&app, "AXWindows")
            && let Ok(windows) = value.downcast::<CFArray>()
        {
            let windows = unsafe { windows.cast_unchecked::<AXUIElement>() };
            for i in 0..windows.len() {
                let Some(window) = windows.get(i) else {
                    continue;
                };
                let window_title = copy_attribute(&window, "AXTitle")
                    .ok()
                    .and_then(|t| t.downcast::<CFString>().ok())
                    .map(|s| s.to_string());
                if window_title.as_deref() == Some(title) {
                    return true;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn dock_pid() -> Option<i32> {
    let out = std::process::Command::new("pgrep")
        .args(["-x", "Dock"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn copy_attribute(
    element: &AXUIElement,
    attribute: &'static str,
) -> Result<CFRetained<CFType>, AXError> {
    let attr = CFString::from_static_str(attribute);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { element.copy_attribute_value(&attr, NonNull::new_unchecked(&mut value)) };
    match NonNull::new(value.cast_mut()) {
        Some(ptr) if err.0 == 0 => Ok(unsafe { CFRetained::from_raw(ptr) }),
        _ => Err(err),
    }
}

/// Human-readable names for the AXError codes tier-0 actually encounters.
fn ax_err(err: AXError) -> String {
    let name = match err.0 {
        -25200 => "kAXErrorFailure",
        -25201 => "kAXErrorIllegalArgument",
        -25202 => "kAXErrorInvalidUIElement",
        -25204 => "kAXErrorCannotComplete (no window-server connection?)",
        -25205 => "kAXErrorAttributeUnsupported",
        -25208 => "kAXErrorNotImplemented",
        -25211 => "kAXErrorAPIDisabled (process is not TCC-trusted)",
        _ => "unknown AXError",
    };
    format!("{name} ({})", err.0)
}
