//! Windows tier-0 checks: which session / window station / desktop this
//! process landed in (OpenSSH puts children in a non-interactive service
//! session, and UIA cannot cross session boundaries), then whether UIA can
//! reach a populated desktop from there.

use uiautomation::UIAutomation;
use uiautomation::types::TreeScope;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetProcessWindowStation, GetThreadDesktop,
    GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

use crate::Report;

pub fn run() -> Report {
    let mut r = Report::new();

    // Session placement: UIA only sees the session the process runs in.
    let mut session = 0u32;
    match unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } {
        Ok(()) => r.info("session-id", format!("process runs in session {session}")),
        Err(e) => r.info("session-id", format!("ProcessIdToSessionId failed: {e}")),
    }
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console == 0xFFFF_FFFF {
        r.info(
            "console-session-id",
            "no session is attached to the console",
        );
    } else if console == session {
        r.info(
            "console-session-id",
            format!("{console} — process IS in the console session"),
        );
    } else {
        r.info(
            "console-session-id",
            format!(
                "{console} — process is NOT in the console session; UIA sees the wrong desktop"
            ),
        );
    }

    // Window station + desktop names: `WinSta0` is the interactive one;
    // OpenSSH children get `Service-0x...-...$`.
    match unsafe { GetProcessWindowStation() } {
        Ok(winsta) => {
            let name =
                user_object_name(HANDLE(winsta.0)).unwrap_or_else(|e| format!("<error: {e}>"));
            let suffix = if name == "WinSta0" {
                " (interactive)"
            } else {
                " (non-interactive)"
            };
            r.info("window-station", format!("{name}{suffix}"));
        }
        Err(e) => r.info(
            "window-station",
            format!("GetProcessWindowStation failed: {e}"),
        ),
    }
    match unsafe { GetThreadDesktop(GetCurrentThreadId()) } {
        Ok(desktop) => {
            let name =
                user_object_name(HANDLE(desktop.0)).unwrap_or_else(|e| format!("<error: {e}>"));
            r.info("thread-desktop", name);
        }
        Err(e) => r.info("thread-desktop", format!("GetThreadDesktop failed: {e}")),
    }
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS.into()) } {
        Ok(_) => r.info("input-desktop", "input desktop is reachable"),
        Err(e) => r.info(
            "input-desktop",
            format!("cannot open the input desktop ({e}) — no interactive desktop in this window station"),
        ),
    }

    // The tier-0 gate: create a UIA client and read the desktop root.
    let automation = match UIAutomation::new() {
        Ok(a) => {
            r.pass("uia-init", "UIAutomation COM client created");
            a
        }
        Err(e) => {
            r.fail(
                "uia-init",
                format!("cannot create the UIAutomation client: {e}"),
            );
            return r;
        }
    };
    let root = match automation.get_root_element() {
        Ok(root) => {
            let name = root.get_name().unwrap_or_else(|e| format!("<error: {e}>"));
            let class = root
                .get_classname()
                .unwrap_or_else(|e| format!("<error: {e}>"));
            r.pass("uia-root", format!("name={name:?} class={class:?}"));
            root
        }
        Err(e) => {
            r.fail("uia-root", format!("cannot get the UIA root element: {e}"));
            return r;
        }
    };
    let children = automation
        .create_true_condition()
        .and_then(|cond| root.find_all(TreeScope::Children, &cond));
    match children {
        Ok(children) if !children.is_empty() => {
            let names: Vec<String> = children
                .iter()
                .take(5)
                .map(|c| c.get_name().unwrap_or_default())
                .collect();
            r.pass(
                "uia-root-children",
                format!("{} top-level elements, first: {names:?}", children.len()),
            );
        }
        Ok(_) => r.fail(
            "uia-root-children",
            "desktop root has no children — this session's desktop is empty (see session checks above)",
        ),
        Err(e) => r.fail("uia-root-children", format!("cannot enumerate desktop children: {e}")),
    }

    r
}

fn user_object_name(handle: HANDLE) -> Result<String, windows::core::Error> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}
