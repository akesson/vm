//! The unit vm's waits are measured in: one second, except under test.
//!
//! The waits that matter are state machines. [`crate::prl::ensure_up`] polls a
//! guest through a wake — off, transient, running-but-addressless, addressed —
//! giving Parallels a grace window to move a VM out of a settled off-state and
//! giving up after a timeout. [`crate::commands::bring_up`] waits for sshd to
//! open its door, and `bring_up_elevated` waits out the session lag Parallels
//! Tools reports after a resume. Every one of them has been the site of a bug,
//! and no wonder: they are the code whose *timing* is the logic.
//!
//! Which makes them exactly the code that has to be tested, and a test that
//! spent the real ninety seconds driving one of them through its transitions
//! would never be run. `VM_TEST_TICK_MS` shrinks the unit so a whole wake fits
//! inside a second: the timeouts, the grace windows and the polling intervals
//! all scale together, so what the test drives is the same state machine in the
//! same proportions, only faster.
//!
//! This is the seam the wire protocol already carries in
//! [`crate::proto::ExecRequest::heartbeat_timeout_ms`], and it works the same
//! way — the host never sets it. Only a test does.

use std::sync::OnceLock;
use std::time::Duration;

/// One tick — a second, in the world outside a test.
///
/// Read once and cached: a wait that changed unit halfway through would be a
/// stranger bug than any it could catch.
pub fn tick() -> Duration {
    static TICK: OnceLock<Duration> = OnceLock::new();
    *TICK.get_or_init(|| {
        std::env::var("VM_TEST_TICK_MS")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map_or(Duration::from_secs(1), Duration::from_millis)
    })
}

/// `n` ticks — `n` seconds, in the world outside a test.
pub fn ticks(n: u32) -> Duration {
    tick() * n
}
