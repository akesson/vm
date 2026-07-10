//! Tier-0 accessibility probe: reports whether this process can reach the
//! OS accessibility API (UIA / AT-SPI / AX) and read a populated element
//! tree, and if not, which precondition is missing. Exit 0 iff every gate
//! check passes. Run inside a guest with
//! `vm exec <os> -- cargo run -p a11y-probe --release`.

use std::process::ExitCode;

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pass,
    Fail,
    Info,
}

/// Ordered check results. `Fail` is reserved for gate checks (the a11y API
/// connection and a populated tree) and hard prerequisites; degraded
/// environment/session observations stay `Info` — they explain a failure,
/// they are not the failure.
pub struct Report {
    checks: Vec<(String, Status, String)>,
}

impl Report {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn pass(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.checks.push((name.into(), Status::Pass, detail.into()));
    }

    fn fail(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.checks.push((name.into(), Status::Fail, detail.into()));
    }

    fn info(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.checks.push((name.into(), Status::Info, detail.into()));
    }

    fn ok(&self) -> bool {
        self.checks.iter().all(|(_, s, _)| *s != Status::Fail)
    }

    fn print(&self) {
        for (name, status, detail) in &self.checks {
            let tag = match status {
                Status::Pass => "PASS",
                Status::Fail => "FAIL",
                Status::Info => "info",
            };
            println!("{tag}  {name}: {detail}");
        }
        println!();
        if self.ok() {
            println!("a11y-probe ▸ OK — accessibility API reachable and populated");
        } else {
            println!("a11y-probe ▸ NOT READY — see FAIL lines above");
        }
    }
}

#[cfg(windows)]
#[path = "win.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "mac.rs"]
mod platform;

fn main() -> ExitCode {
    let report = platform::run();
    report.print();
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
