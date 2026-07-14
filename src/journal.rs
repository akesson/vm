//! The journal: every line vm prints, kept with the time it printed it.
//!
//! vm's terminal channel is unchanged — breadcrumbs, notes and errors go to
//! stderr, stdout stays the command's own (see the README's "Stdio"). This
//! module *tees* those same lines into `~/.config/vm/log/vm.log`, stamped with
//! a local-time timestamp and the pid. It is a transcript, not an event
//! stream: the line in the file is the line you saw, so nothing can drift out
//! of sync with what vm actually said.
//!
//! It exists because the unattended half of vm had no memory. `vm reap` runs
//! from launchd every 5 minutes and its decisions went to a file launchd
//! redirected for it — with no timestamps, so the one file you would open to
//! ask "why did my VM go down at 3pm" could not say when anything happened.
//! And issue #27's debugging procedure asks you to correlate vm's death
//! against Parallels' own log "within milliseconds", which vm gave you no way
//! to do. Now it does: the `exit 130` line carries the millisecond.
//!
//! Four things are load-bearing, and each one is a way this could have gone
//! wrong:
//!
//! - **Default-off; `main` arms it for host verbs only.** The guest verbs
//!   (`_exec`, `_sync-*`, `_tree`, `_first-sync`, `_version`, `_idle`) never
//!   call [`init`], so inside a guest the macros are plain `eprintln!`. The
//!   host *parses* the guest's stdout as JSON and matches its stderr for
//!   substrings ([`crate::commands`]), and a journal writing into a VM nobody
//!   shells into would be pointless anyway.
//! - **Panic-free by construction.** No unwrap, no indexing, no arithmetic
//!   that can trap on the emit path. Any io error moves the state to `Dead`
//!   and stays there. A journal that fails must never fail a run.
//! - **The panic hook takes the lock with `try_lock`.** `std::sync::Mutex` is
//!   not reentrant and a panic hook runs *before* unwinding releases the lock.
//!   A blocking lock here would deadlock a process that panicked inside the
//!   emit critical section — and a hung vm keeps its [`crate::lock`] flock, so
//!   reap would skip that VM forever. Losing a panic line is the cheaper
//!   failure.
//! - **Rotation happens on open, not on write.** Every vm process is
//!   short-lived — including each reap sweep — so no fd outlives a rotation
//!   and there is no SIGHUP dance to get wrong.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The journal file, and the single generation of history kept behind it.
const FILE: &str = "vm.log";
const ROTATED: &str = "vm.log.1";

/// The gutter launchd redirects the reap job's stdio into. Expected to stay
/// empty: the job runs `-q` and the panic hook journals first, so only output
/// from a catastrophe vm never saw lands here. See [`crate::reap`].
const GUTTER: &str = "reap-launchd.log";

/// Rotate at 8 MiB, keeping one generation — a 16 MiB ceiling on a file that
/// grows by a few lines per `vm` invocation and per 5-minute reap sweep.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Where a journal is in its short life. `Armed` is the interesting one: it
/// means "enabled but not yet touched", so a run that prints nothing never
/// creates a file, and the cost of opening one is paid on the first line.
enum State {
    /// Not enabled — a guest verb, or `VM_JOURNAL=off`. Emits are no-ops.
    Disabled,
    /// Enabled; the file is opened lazily on the first line.
    Armed,
    Open(File),
    /// Something failed. Silently no-op forever rather than fail the run.
    Dead,
}

static JOURNAL: Mutex<State> = Mutex::new(State::Disabled);
static QUIET: AtomicBool = AtomicBool::new(false);

/// Arm the journal for this process, and record whether `--quiet` was passed.
///
/// Called from `main` for host verbs only. `VM_JOURNAL=off` opts out — the
/// escape hatch for a command line you would rather not have persisted, since
/// the journal keeps argv (which is also why the file is 0600).
pub fn init(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
    if std::env::var("VM_JOURNAL").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return; // stays Disabled
    }
    if let Ok(mut state) = JOURNAL.lock() {
        *state = State::Armed;
    }
}

/// Whether `--quiet` suppressed breadcrumbs on stderr. The journal ignores it.
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// `~/.config/vm/log`, derived from the config path — so `$VM_CONFIG`
/// redirects the journal exactly as it already redirects the lock dir
/// ([`crate::lock`]), and the tests get that redirection for free.
pub fn log_dir() -> Option<PathBuf> {
    let config = crate::config::Config::path();
    Some(config.parent()?.join("log"))
}

/// The journal file's path, whether or not it exists yet.
pub fn path() -> Option<PathBuf> {
    Some(log_dir()?.join(FILE))
}

/// The file launchd points the reap job's stdout and stderr at.
pub fn gutter_path() -> Option<PathBuf> {
    Some(log_dir()?.join(GUTTER))
}

/// The journal and its size, if it exists. `vm doctor` renders this.
pub fn status() -> Option<(PathBuf, u64)> {
    let path = path()?;
    let len = std::fs::metadata(&path).ok()?.len();
    Some((path, len))
}

/// Append `text` to the journal, one stamped record per line of it.
///
/// The entry point for [`crumb!`] and [`notice!`]; a no-op unless [`init`]
/// armed this process. Never fails, never panics, never blocks on anything but
/// this process's own short critical section.
pub fn emit(text: &str) {
    let Ok(mut state) = JOURNAL.lock() else {
        return; // poisoned by a panic elsewhere; the hook still gets a shot
    };
    // Cheap exit before touching the environment or the clock.
    if matches!(*state, State::Disabled | State::Dead) {
        return;
    }
    let Some(dir) = log_dir() else {
        *state = State::Dead;
        return;
    };
    emit_into(&mut state, &dir, text);
}

/// The whole emit path, with the global and the real directory factored out —
/// which is what makes it testable without arming a process-wide static.
fn emit_into(state: &mut State, dir: &Path, text: &str) {
    if matches!(state, State::Disabled | State::Dead) {
        return;
    }
    if matches!(state, State::Armed) {
        match open_in(dir) {
            Ok(file) => *state = State::Open(file),
            Err(_) => {
                *state = State::Dead;
                return;
            }
        }
    }
    let State::Open(file) = state else {
        return;
    };
    let record = render(&stamp_now(), std::process::id(), text);
    // One write_all per emit: on an O_APPEND fd that is atomic enough to keep
    // concurrent vm processes from interleaving mid-line (`lock::shared` means
    // concurrent runs on one VM are the normal case, not the exception).
    if file.write_all(record.as_bytes()).is_err() {
        *state = State::Dead;
    }
}

/// Open (creating) the journal, rotating it first if it has grown past
/// [`MAX_BYTES`].
fn open_in(dir: &Path) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE);
    rotate(&path, &dir.join(ROTATED));
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        // The journal keeps the command lines you ran, which may carry secrets
        // that used to be ephemeral on a terminal. Only applies at creation;
        // an existing file keeps the mode it has.
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)
}

/// Best-effort rotation. A failure here just means we keep appending to a file
/// that is over the limit — worth nobody's run.
///
/// Two processes starting at once can both decide to rotate; the second rename
/// clobbers the first's `.1`, costing one generation of history. And a process
/// that already holds an fd on the renamed inode keeps appending to it, so its
/// remaining lines land in `vm.log.1` rather than nowhere. Both are acceptable
/// for a file whose readers are `grep` and a human.
fn rotate(path: &Path, rotated: &Path) {
    let oversized = std::fs::metadata(path).is_ok_and(|m| m.len() >= MAX_BYTES);
    if oversized {
        let _ = std::fs::rename(path, rotated);
    }
}

/// One stamped record per line of `text` — vm prints the odd two-line message,
/// and a journal that is not line-oriented is a journal `grep` cannot read.
fn render(stamp: &str, pid: u32, text: &str) -> String {
    let mut out = String::new();
    for line in text.trim_end_matches('\n').split('\n') {
        out.push_str(stamp);
        out.push_str(" [");
        out.push_str(&pid.to_string());
        out.push_str("] ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn stamp_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    rfc3339(millis, local_offset(millis))
}

/// Seconds east of UTC, as the C library sees it right now (so DST is whatever
/// was in force at `epoch_millis`, not what is in force in January).
///
/// `libc` is already a dependency on unix, which is where the journal actually
/// runs — the host is always macOS. Windows gets UTC: it is reachable (CI
/// drives host verbs on a Windows runner) but it is not a real vm host.
#[cfg(unix)]
fn local_offset(epoch_millis: i64) -> i32 {
    let secs = epoch_millis.div_euclid(1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` fills `tm` from `secs`; both pointers are valid for
    // the call and the `_r` form is the thread-safe one (no shared static).
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if ok { tm.tm_gmtoff as i32 } else { 0 }
}

#[cfg(not(unix))]
fn local_offset(_epoch_millis: i64) -> i32 {
    0
}

/// `2026-07-14T16:19:48.412+02:00` — RFC 3339, local time, millisecond
/// precision (the precision issue #27's correlation against Parallels' own log
/// actually needs).
///
/// Pure arithmetic on purpose: a date crate would be the first new runtime
/// dependency in this tree, and this is 20 lines that never panic.
fn rfc3339(epoch_millis: i64, offset_secs: i32) -> String {
    let local = epoch_millis + i64::from(offset_secs) * 1000;
    let days = local.div_euclid(86_400_000);
    let ms_of_day = local.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);

    let ms = ms_of_day % 1000;
    let secs_of_day = ms_of_day / 1000;
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    let (sign, offset) = if offset_secs < 0 {
        ('-', -i64::from(offset_secs))
    } else {
        ('+', i64::from(offset_secs))
    };
    let (oh, om) = (offset / 3600, (offset % 3600) / 60);

    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}{sign}{oh:02}:{om:02}")
}

/// Days since the epoch → (year, month, day). Howard Hinnant's `civil_from_days`,
/// which is branch-free, exact for any date we will ever stamp, and — the part
/// that matters here — free of any operation that can panic.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Install the panic hook that gets a panic into the journal before the
/// process dies. See this module's docs for why it must never block.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut state) = JOURNAL.try_lock()
            && let Some(dir) = log_dir()
        {
            emit_into(&mut state, &dir, &format!("vm: panic: {info}"));
        }
        previous(info);
    }));
}

/// Print a line to the terminal, tolerating a reader that has gone away.
///
/// A closed pipe downstream is not vm failing — it is the reader saying
/// "enough". `vm doctor | grep -q prlctl` closes the pipe the moment grep
/// matches, while vm is still writing. Rust ignores SIGPIPE, so that arrives as
/// an `EPIPE` *write error*, and `println!`/`eprintln!` panic on write errors:
/// vm died with exit 101 and printed nothing at all to say why, the panic
/// message going to the very pipe that had just closed (#36).
///
/// The SIGPIPE disposition is deliberately left as Rust sets it. Two places
/// depend on a broken pipe arriving as an error rather than as a signal:
/// [`crate::exec::host`]'s heartbeat thread, which learns the transport is gone
/// by writing to it, and [`crate::exec::guest`]'s `feed_stdin`. Restoring
/// `SIG_DFL` would fix the panic by killing the process instead — and take those
/// with it.
///
/// Nothing else that goes wrong writing here is reportable either: the channel
/// vm would report it on is the one that just failed.
fn print_line(mut out: impl std::io::Write, line: &str) {
    let _ = writeln!(out, "{line}");
}

/// Print to stderr — vm's own channel: breadcrumbs, notes, errors.
pub fn to_stderr(line: &str) {
    print_line(std::io::stderr().lock(), line);
}

/// Print to stdout, which stays the command's own everywhere except the handful
/// of verbs whose output *is* the answer (`vm ls`'s table, the guest verbs'
/// protocol replies). `vm ls | head -1` closes it on the second row.
pub fn to_stdout(line: &str) {
    print_line(std::io::stdout().lock(), line);
}

/// Print a progress breadcrumb: to stderr unless `--quiet`, and to the journal
/// always. For narration — what vm is doing and how long it took.
#[macro_export]
macro_rules! crumb {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        if !$crate::journal::quiet() {
            $crate::journal::to_stderr(&line);
        }
        $crate::journal::emit(&line);
    }};
}

/// Print something the user has to act on — a `vm ▸ note:`, a `WARNING:`, a
/// command that was not found, a fatal error, a `vm doctor` verdict. Always
/// reaches stderr, `--quiet` or not: quiet suppresses narration, never news.
#[macro_export]
macro_rules! notice {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        $crate::journal::to_stderr(&line);
        $crate::journal::emit(&line);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The formatter ────────────────────────────────────────────────────────

    #[test]
    fn rfc3339_renders_the_epoch_in_utc() {
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00.000+00:00");
    }

    #[test]
    fn rfc3339_keeps_milliseconds_zero_padded() {
        // 7ms past the epoch: the field is three digits or a log is unsortable.
        assert_eq!(rfc3339(7, 0), "1970-01-01T00:00:00.007+00:00");
        assert_eq!(rfc3339(70, 0), "1970-01-01T00:00:00.070+00:00");
        assert_eq!(rfc3339(700, 0), "1970-01-01T00:00:00.700+00:00");
    }

    /// The stamp this whole module exists to produce, at the offset the host
    /// that produces it actually runs at (CEST, +2).
    #[test]
    fn rfc3339_renders_a_positive_offset_in_local_time() {
        // 2026-07-14T14:19:48.412Z == 16:19:48.412+02:00
        assert_eq!(
            rfc3339(1_784_038_788_412, 2 * 3600),
            "2026-07-14T16:19:48.412+02:00"
        );
    }

    /// A negative offset must not just flip the sign — the local date can roll
    /// back a day, which is where a naive implementation gets it wrong.
    #[test]
    fn rfc3339_rolls_the_date_back_across_a_negative_offset() {
        // 2026-01-01T04:30:00Z in UTC-08:00 is still 2025-12-31.
        assert_eq!(
            rfc3339(1_767_241_800_000, -8 * 3600),
            "2025-12-31T20:30:00.000-08:00"
        );
    }

    /// Offsets are not all whole hours (India is +05:30). The minutes field is
    /// not decorative.
    #[test]
    fn rfc3339_renders_a_half_hour_offset() {
        assert_eq!(rfc3339(0, 5 * 3600 + 1800), "1970-01-01T05:30:00.000+05:30");
    }

    #[test]
    fn civil_from_days_handles_leap_years_and_century_rules() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000 was a leap year (divisible by 400); 1900 was not.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2024-02-29, the most recent leap day, and the day after it.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        // A non-leap year's end-of-February.
        assert_eq!(civil_from_days(20_148), (2025, 3, 1));
    }

    /// Pre-epoch days go negative, and `div_euclid`/`rem_euclid` are what keep
    /// that from producing a garbage date (plain `/` and `%` would).
    #[test]
    fn civil_from_days_survives_a_negative_day_count() {
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    // ── The record shape ─────────────────────────────────────────────────────

    #[test]
    fn render_stamps_one_record_per_line() {
        // reap's install breadcrumb is genuinely two lines; a journal that is
        // not line-oriented is one `grep` cannot read.
        let out = render("STAMP", 42, "first line\nsecond line");
        assert_eq!(out, "STAMP [42] first line\nSTAMP [42] second line\n");
    }

    #[test]
    fn render_does_not_emit_an_empty_record_for_a_trailing_newline() {
        assert_eq!(render("STAMP", 42, "only\n"), "STAMP [42] only\n");
    }

    // ── The state machine ────────────────────────────────────────────────────

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(FILE)).unwrap_or_default()
    }

    #[test]
    fn a_disabled_journal_writes_nothing_and_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::Disabled;
        emit_into(&mut state, dir.path(), "vm ▸ linux ▸ exit 0");
        assert!(!dir.path().join(FILE).exists(), "no file for a guest verb");
    }

    #[test]
    fn an_armed_journal_opens_on_first_write_and_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::Armed;
        emit_into(&mut state, dir.path(), "first");
        assert!(
            matches!(state, State::Open(_)),
            "armed → open on first line"
        );
        emit_into(&mut state, dir.path(), "second");

        let body = read(dir.path());
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 2, "{body}");
        assert!(lines[0].ends_with("] first"), "{body}");
        assert!(lines[1].ends_with("] second"), "{body}");
        // Every record carries a stamp and this process's pid.
        let pid = std::process::id();
        assert!(lines[0].contains(&format!("[{pid}]")), "{body}");
        assert!(lines[0].starts_with("20"), "stamped with a year: {body}");
    }

    #[test]
    fn a_dead_journal_stays_dead() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::Dead;
        emit_into(&mut state, dir.path(), "ignored");
        assert!(!dir.path().join(FILE).exists());
        assert!(matches!(state, State::Dead));
    }

    /// An unusable log dir must cost the run nothing — not an error, not a
    /// panic, just a journal that quietly is not there.
    #[test]
    fn an_unopenable_journal_goes_dead_instead_of_failing_the_run() {
        let dir = tempfile::tempdir().unwrap();
        // A *file* where the log directory should be: create_dir_all fails.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let mut state = State::Armed;
        emit_into(&mut state, &blocked, "vm ▸ linux ▸ exit 0");
        assert!(matches!(state, State::Dead), "armed → dead on an io error");
    }

    // ── Rotation ─────────────────────────────────────────────────────────────

    #[test]
    fn rotation_moves_an_oversized_journal_aside_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();

        let mut state = State::Armed;
        emit_into(&mut state, dir.path(), "after rotation");

        let rotated = std::fs::read(dir.path().join(ROTATED)).unwrap();
        assert_eq!(rotated.len(), MAX_BYTES as usize, "history kept, intact");
        let fresh = read(dir.path());
        assert!(fresh.ends_with("] after rotation\n"), "{fresh}");
        assert!(fresh.len() < 200, "the new journal starts empty: {fresh}");
    }

    #[test]
    fn rotation_keeps_exactly_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ROTATED), b"the previous generation").unwrap();
        std::fs::write(dir.path().join(FILE), vec![b'x'; MAX_BYTES as usize]).unwrap();

        let mut state = State::Armed;
        emit_into(&mut state, dir.path(), "line");

        let rotated = std::fs::read(dir.path().join(ROTATED)).unwrap();
        assert_eq!(
            rotated.len(),
            MAX_BYTES as usize,
            "the older .1 is clobbered"
        );
    }

    #[test]
    fn a_journal_under_the_limit_is_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), b"existing history\n").unwrap();

        let mut state = State::Armed;
        emit_into(&mut state, dir.path(), "appended");

        assert!(!dir.path().join(ROTATED).exists(), "nothing to rotate");
        let body = read(dir.path());
        assert!(body.starts_with("existing history\n"), "{body}");
        assert!(
            body.ends_with("] appended\n"),
            "appends, never truncates: {body}"
        );
    }

    /// The mode matters: the journal keeps the command lines you ran.
    #[cfg(unix)]
    #[test]
    fn a_new_journal_is_created_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::Armed;
        emit_into(&mut state, dir.path(), "secret command line");

        let mode = std::fs::metadata(dir.path().join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "0600, not the umask's whim");
    }
}
