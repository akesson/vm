//! Linux tier-0 checks: session-bus discovery (with the systemd
//! `/run/user/<uid>/bus` fallback a bare ssh env needs), the AT-SPI
//! accessibility bus, and whether the registry root has any accessible
//! applications. Tier-1: spawn a zenity fixture window and find it through
//! the registry by title.

use atspi::connection::AccessibilityConnection;
use atspi::zbus;

use crate::Report;

pub fn run() -> Report {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("failed to build tokio runtime");
    rt.block_on(run_inner())
}

async fn run_inner() -> Report {
    let mut r = Report::new();

    // What a desktop session would have exported; a bare ssh env has none.
    for key in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        match std::env::var(key) {
            Ok(v) => r.info(format!("env {key}"), v),
            Err(_) => r.info(format!("env {key}"), "(unset)"),
        }
    }

    // zbus falls back to $XDG_RUNTIME_DIR/bus when DBUS_SESSION_BUS_ADDRESS
    // is unset; if both are missing, point it at the systemd user bus
    // ourselves. No socket at all means no user session to probe.
    let uid = unsafe { libc::getuid() };
    let default_sock = format!("/run/user/{uid}/bus");
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        if !std::path::Path::new(&default_sock).exists() {
            r.fail(
                "bus-socket",
                format!(
                    "DBUS_SESSION_BUS_ADDRESS unset and {default_sock} does not exist — \
                     no user session bus (is anyone logged in?)"
                ),
            );
            return r;
        }
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            // Safety: single-threaded at this point (current_thread runtime).
            unsafe {
                std::env::set_var(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={default_sock}"),
                )
            };
            r.info(
                "bus-socket",
                format!("{default_sock} exists — injected DBUS_SESSION_BUS_ADDRESS"),
            );
        } else {
            r.info(
                "bus-socket",
                format!("{default_sock} exists (zbus XDG_RUNTIME_DIR fallback applies)"),
            );
        }
    }

    match zbus::Connection::session().await {
        Ok(_) => r.pass("session-bus", "connected to the D-Bus session bus"),
        Err(e) => {
            r.fail(
                "session-bus",
                format!("cannot connect to the session bus: {e}"),
            );
            return r;
        }
    }

    // org.a11y.Status IsEnabled — informational: toolkits check this flag
    // before exporting their trees, but the bus itself works either way.
    match atspi::connection::read_session_accessibility().await {
        Ok(enabled) => r.info(
            "a11y-enabled",
            format!("org.a11y.Status IsEnabled = {enabled}"),
        ),
        Err(e) => r.info(
            "a11y-enabled",
            format!("could not read org.a11y.Status: {e}"),
        ),
    }

    // The tier-0 gate: reach the accessibility bus (org.a11y.Bus GetAddress
    // may D-Bus-activate at-spi-bus-launcher) and read the registry root.
    let conn = match AccessibilityConnection::new().await {
        Ok(c) => {
            r.pass("a11y-bus", "connected to the AT-SPI accessibility bus");
            c
        }
        Err(e) => {
            r.fail(
                "a11y-bus",
                format!("cannot reach the accessibility bus (at-spi2 installed?): {e}"),
            );
            return r;
        }
    };

    let root = match conn.root_accessible_on_registry().await {
        Ok(root) => root,
        Err(e) => {
            r.fail(
                "registry-root",
                format!("cannot get the registry root: {e}"),
            );
            return r;
        }
    };
    let name = root
        .name()
        .await
        .unwrap_or_else(|e| format!("<error: {e}>"));
    let role = root
        .get_role_name()
        .await
        .unwrap_or_else(|e| format!("<error: {e}>"));
    r.pass("registry-root", format!("name={name:?} role={role:?}"));

    match root.child_count().await {
        Ok(n) if n > 0 => r.pass(
            "registry-children",
            format!("{n} accessible application(s) registered"),
        ),
        Ok(_) => r.fail(
            "registry-children",
            "registry is empty — no a11y-enabled GUI applications running in the session",
        ),
        Err(e) => r.fail("registry-children", format!("cannot read child count: {e}")),
    }
    if !r.ok() {
        return r;
    }

    fixture_check(&mut r, &conn).await;
    r
}

/// Tier-1: spawn a zenity dialog with a unique title and find its frame
/// through the AT-SPI registry — proves cross-process window discovery, not
/// just that the bus answers.
async fn fixture_check(r: &mut Report, conn: &AccessibilityConnection) {
    let title = format!("a11y-fixture-{}", std::process::id());
    let mut cmd = std::process::Command::new("zenity");
    cmd.args(["--info", "--title", &title, "--text", "a11y-probe fixture"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A bare ssh env has no display; the compositor's Wayland socket lives
    // next to the session bus socket we already discovered.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        let runtime_dir = format!("/run/user/{}", unsafe { libc::getuid() });
        if std::path::Path::new(&runtime_dir)
            .join("wayland-0")
            .exists()
        {
            cmd.env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("WAYLAND_DISPLAY", "wayland-0");
        } else {
            cmd.env("DISPLAY", ":0");
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            r.fail("fixture-spawn", format!("cannot spawn zenity: {e}"));
            return;
        }
    };
    r.pass("fixture-spawn", format!("zenity pid {}", child.id()));

    let found = find_frame(conn, &title).await;
    let _ = child.kill();
    let _ = child.wait();
    match found {
        Ok(Some(app)) => r.pass(
            "fixture-window",
            format!("found frame {title:?} under application {app:?}"),
        ),
        Ok(None) => r.fail(
            "fixture-window",
            format!("no frame named {title:?} appeared in the registry within 10s"),
        ),
        Err(e) => r.fail("fixture-window", format!("registry walk failed: {e}")),
    }
}

/// Poll the registry (applications → their top-level frames) for a frame
/// with the given name; returns the owning application's name.
async fn find_frame(
    conn: &AccessibilityConnection,
    title: &str,
) -> Result<Option<String>, atspi::AtspiError> {
    use atspi::proxy::accessible::ObjectRefExt;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let root = conn.root_accessible_on_registry().await?;
        for app_ref in root.get_children().await? {
            let Ok(app) = app_ref.as_accessible_proxy(conn.connection()).await else {
                continue;
            };
            for frame_ref in app.get_children().await.unwrap_or_default() {
                let Ok(frame) = frame_ref.into_accessible_proxy(conn.connection()).await else {
                    continue;
                };
                if frame.name().await.unwrap_or_default() == title {
                    let app_name = app.name().await.unwrap_or_default();
                    return Ok(Some(app_name));
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
