//! `vm ▸ note:` advisories, end to end through the real binary.
//!
//! The rules themselves are exhaustively unit-tested in `exec::advise`; what is
//! proved here is the *wiring* — that a note reaches stderr at all, that it is
//! printed before vm touches a VM (so the reader sees it even when the run then
//! fails), that it never fires on healthy commands, and that `vm claude`'s
//! self-built argv is not run past the rules.
//!
//! Every case leans on 'lin' pointing at a Parallels VM that does not exist (as
//! in tests/exit_codes.rs): reaching the VM always fails with 125, so the note —
//! or its absence — on the way there is what is under test. No VM required.

use std::path::Path;
use std::process::Command;

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

const CONFIG: &str = r#"
[vm.lin]
parallels_name = "vm-test-does-not-exist-42"
os = "linux"
user = "nobody"
work_root = "~/work"
"#;

/// Run `vm …` from `cwd` with $VM_CONFIG set; returns (exit code, stderr).
fn run_vm(config: &Path, cwd: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(VM_BIN)
        .args(args)
        .current_dir(cwd)
        .env("VM_CONFIG", config)
        .output()
        .expect("vm runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A temp dir holding the config, used as the working directory too — so the
/// filesystem probe behind the shell-form advisory sees only what a test puts
/// there.
fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, CONFIG).unwrap();
    (dir, config)
}

fn notes(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.contains("vm ▸ note:"))
        .collect()
}

#[test]
fn a_lone_operator_in_exec_form_draws_a_note_before_any_vm_work() {
    let (dir, config) = workspace();
    // `--` then five arguments: the host shell already split them, so `&&` is a
    // literal word bound for echo, not shell syntax.
    let (code, stderr) = run_vm(
        &config,
        dir.path(),
        &["exec", "lin", "--", "echo", "a", "&&", "echo", "b"],
    );
    let found = notes(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(found[0].contains("`&&`"), "{}", found[0]);
    assert!(found[0].contains("`echo`"), "{}", found[0]);
    // 125 = it went on to try the (nonexistent) VM, so the note is not an error
    // and did not abort the run — it is advice, printed on the way past.
    assert_eq!(code, 125, "the note must not change the outcome");
    // …and printed *before* vm reached for the VM: the reader gets the advice
    // even though the run then died on infrastructure.
    let note_at = stderr.find("note:").expect("note on stderr");
    let error_at = stderr.find("vm: error:").expect("infra error on stderr");
    assert!(note_at < error_at, "note came after the failure: {stderr}");
}

#[test]
fn healthy_commands_say_nothing() {
    // The advisory channel is only worth having if it stays quiet: a note that
    // fires on ordinary commands teaches its reader to ignore every note.
    let (dir, config) = workspace();
    for cmd in [
        &["exec", "lin", "--", "cargo", "test", "--workspace"][..],
        // A quoted operator in a flag's value position: `|` is awk's field
        // separator, exactly as the caller meant it.
        &["exec", "lin", "--", "awk", "-F", "|", "file"][..],
        // Shell form: the pipe is a real pipe, so there is nothing to warn about.
        &["exec", "lin", "--", "echo hi | tr a-z A-Z"][..],
        // Shell form naming no file at all — the common case.
        &["exec", "lin", "--", "cd src && cargo test"][..],
    ] {
        let (_, stderr) = run_vm(&config, dir.path(), cmd);
        assert!(
            notes(&stderr).is_empty(),
            "{cmd:?} should be silent, got: {stderr}"
        );
    }
}

#[test]
fn a_script_beginning_with_a_spaced_filename_draws_a_note() {
    let (dir, config) = workspace();
    std::fs::write(dir.path().join("my script.sh"), "#!/bin/sh\n").unwrap();

    // One argument is a script, so the guest shell splits `my script.sh` in two
    // and looks for a command called `my`.
    let (_, stderr) = run_vm(
        &config,
        dir.path(),
        &["exec", "lin", "--", "my script.sh --flag"],
    );
    let found = notes(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(found[0].contains("`my script.sh`"), "{}", found[0]);

    // The same file as a plain argument is exec form — byte-identical, no shell,
    // nothing to split, nothing to say.
    let (_, stderr) = run_vm(
        &config,
        dir.path(),
        &["exec", "lin", "--", "cat", "my script.sh"],
    );
    assert!(notes(&stderr).is_empty(), "stderr: {stderr}");
}

#[test]
fn the_advisory_probe_looks_at_the_real_filesystem() {
    // Without the file on disk the identical command is silent — the note is
    // earned by a file that exists, never guessed from the shape of the string.
    let (dir, config) = workspace();
    let (_, stderr) = run_vm(
        &config,
        dir.path(),
        &["exec", "lin", "--", "my script.sh --flag"],
    );
    assert!(notes(&stderr).is_empty(), "stderr: {stderr}");
}

/// Run `vm …` like [`run_vm`], but with the given `Stdio` as vm's own stdin —
/// the stdin-note tests are *about* fd 0, which `Command::output()` pins to the
/// null device (the silent case every other test in this file exercises).
#[cfg(unix)]
fn run_vm_with_stdin(
    config: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: std::process::Stdio,
) -> (i32, String) {
    let out = Command::new(VM_BIN)
        .args(args)
        .current_dir(cwd)
        .env("VM_CONFIG", config)
        .stdin(stdin)
        .output()
        .expect("vm runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[cfg(unix)]
#[test]
fn piped_stdin_draws_a_note_before_any_vm_work() {
    // `echo hi | vm exec lin -- 'cat > f'` would exit 0 having written an empty
    // file — the input is discarded, nothing fails, nothing says so. The note
    // therefore fires up front, not on failure. An open-but-unwritten pipe is
    // the same fd type, so spawning with a piped stdin and dropping the handle
    // is exactly the caller-piped shape.
    let (dir, config) = workspace();
    let (code, stderr) = run_vm_with_stdin(
        &config,
        dir.path(),
        &["exec", "lin", "--", "cat > f"],
        std::process::Stdio::piped(),
    );
    let found = notes(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(found[0].contains("piped into vm"), "{}", found[0]);
    assert_eq!(code, 125, "the note must not change the outcome");
    let note_at = stderr.find("note:").expect("note on stderr");
    let error_at = stderr.find("vm: error:").expect("infra error on stderr");
    assert!(note_at < error_at, "note came after the failure: {stderr}");
}

#[cfg(unix)]
#[test]
fn stdin_redirected_from_a_file_draws_the_note_too() {
    // `vm exec lin -- 'wc -l' < data.txt` — same discard, different wiring.
    let (dir, config) = workspace();
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "hi\n").unwrap();
    let (_, stderr) = run_vm_with_stdin(
        &config,
        dir.path(),
        &["exec", "lin", "--", "wc -l"],
        std::fs::File::open(&data).unwrap().into(),
    );
    let found = notes(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(found[0].contains("redirected into vm"), "{}", found[0]);
}

#[cfg(unix)]
#[test]
fn vm_claude_with_piped_stdin_is_told_too() {
    // `git diff | vm claude lin "review this"` looks like it hands claude the
    // diff; it does not — claude's stdin is the null device like any other
    // guest command's. Same path (claude drives exec::host::exec), same note.
    let (dir, config) = workspace();
    let (_, stderr) = run_vm_with_stdin(
        &config,
        dir.path(),
        &["claude", "lin", "review this"],
        std::process::Stdio::piped(),
    );
    let found = notes(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(found[0].contains("piped into vm"), "{}", found[0]);
}

// ── The PATH behind a command-not-found (#25) ────────────────────────────────

/// The host's own OS, spelled as an `--or-native` target literal — the one path
/// that runs a command without a VM, a config or a sync, so these cases drive the
/// real spawn and the real report from `cargo test` on any machine.
fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// A name nothing will ever resolve, in exec form (several arguments), which is
/// the form that hands argv[0] to the OS instead of to a shell.
const MISSING: &str = "vm-definitely-not-a-real-binary";

#[test]
fn a_missing_native_command_reports_the_path_it_searched() {
    let (dir, config) = workspace();
    // `-e PATH=…` so the expected report is knowable: it also proves vm reports
    // the PATH the *child* was given, not the one vm itself inherited — the two
    // differ on every run that sets one, and only the child's decides a not-found.
    let path = if cfg!(windows) {
        r"C:\vm-test\bin;C:\vm-test\other"
    } else {
        "/vm-test/bin:/vm-test/other"
    };
    let env = format!("PATH={path}");
    let (code, stderr) = run_vm(
        &config,
        dir.path(),
        &[
            "exec",
            host_os(),
            "--or-native",
            "-e",
            &env,
            "--",
            MISSING,
            "--flag",
        ],
    );
    assert_eq!(
        code, 127,
        "a missing command is 127, not vm's 125: {stderr}"
    );
    assert!(
        stderr.contains(&format!("vm: command not found: {MISSING}")),
        "{stderr}"
    );
    // The search path, entry by entry: the state the reader cannot see and vm can.
    assert!(
        stderr.contains("the PATH it searched (2 entries)"),
        "{stderr}"
    );
    assert!(stderr.contains("vm-test"), "{stderr}");
    // A healthy PATH earns no warning and no advice — the report is the answer.
    assert!(!stderr.contains('⚠'), "{stderr}");
    assert!(notes(&stderr).is_empty(), "{stderr}");
}

#[test]
fn a_half_posix_path_is_flagged_on_windows_and_nowhere_else() {
    // The #25 failure itself: a `mise` task whose shell is bash hands a native
    // grandchild a PATH whose head was converted to Windows form and whose tail
    // was left POSIX and colon-joined. `cargo` lives in the tail; Win32 searches
    // only the head; "program not found" was all vm used to say about it.
    let (dir, config) = workspace();
    let half = r"C:\Program Files\Git\usr\bin;/c/Users/runneradmin/.cargo/bin:/c/Program Files/Git/mingw64/bin";
    let env = format!("PATH={half}");
    let (code, stderr) = run_vm(
        &config,
        dir.path(),
        &[
            "exec",
            host_os(),
            "--or-native",
            "-e",
            &env,
            "--",
            MISSING,
            "--flag",
        ],
    );
    assert_eq!(code, 127, "{stderr}");

    // Deliberately branched, not `#[cfg(windows)]`-gated: the rule is Windows-only
    // and the Windows guest is where this test earns its keep (`vm exec windows --
    // cargo test`), but the *silence* on unix is just as much a property worth
    // holding — those same bytes are a perfectly ordinary PATH there.
    if cfg!(windows) {
        assert!(stderr.contains('⚠'), "the POSIX tail is unmarked: {stderr}");
        assert!(stderr.contains("colon-joined: 2 paths"), "{stderr}");
        let found = notes(&stderr);
        assert_eq!(found.len(), 1, "stderr: {stderr}");
        assert!(
            found[0].contains("POSIX PATH reached a native Windows process"),
            "{}",
            found[0]
        );
    } else {
        assert!(!stderr.contains('⚠'), "{stderr}");
        assert!(notes(&stderr).is_empty(), "{stderr}");
    }
}

#[test]
fn vm_claude_is_not_run_past_the_exec_advisories() {
    // claude's argv is built by vm, not typed by the caller, and its prompt is
    // one element — a prompt that happened to be `&&` must not draw a note about
    // shell syntax the user never wrote.
    let (dir, config) = workspace();
    let (_, stderr) = run_vm(&config, dir.path(), &["claude", "lin", "&&"]);
    assert!(notes(&stderr).is_empty(), "stderr: {stderr}");
}
