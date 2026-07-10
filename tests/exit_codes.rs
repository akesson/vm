//! The exit-code contract, end to end: vm reserves 125 for its own
//! infrastructure failures and 2 for usage/config errors, so a caller can tell
//! "the command failed" from "vm failed". A guest command's own code passes
//! through untouched (covered in tests/exec_agent.rs). Each case drives the
//! real `vm` binary with $VM_CONFIG pointing at a temp config — no VM required,
//! so it runs in CI on all three OSes.

use std::path::Path;
use std::process::Command;

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// A config whose one alias points at a Parallels VM that does not exist — so
/// resolving succeeds but every VM operation fails (as it also does in CI,
/// where `prlctl` isn't installed at all): a vm infra error either way.
const CONFIG: &str = r#"
[vm.lin]
parallels_name = "vm-test-does-not-exist-42"
os = "linux"
user = "nobody"
work_root = "~/work"
"#;

/// Run `vm …` with $VM_CONFIG set; `None` points it at a path that does not
/// exist. Returns the process exit code.
fn run_vm(config: Option<&Path>, args: &[&str]) -> i32 {
    let mut cmd = Command::new(VM_BIN);
    cmd.args(args);
    cmd.env(
        "VM_CONFIG",
        config.map_or_else(
            || Path::new("/vm-test/definitely/not/a/config.toml").to_path_buf(),
            Path::to_path_buf,
        ),
    );
    cmd.output().expect("vm runs").status.code().unwrap_or(-1)
}

fn temp_config() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    (dir, path)
}

#[test]
fn unreadable_config_is_a_usage_error() {
    // No config file at $VM_CONFIG → the user's setup is wrong → exit 2.
    assert_eq!(run_vm(None, &["exec", "lin", "--", "true"]), 2);
}

#[test]
fn unknown_target_is_a_usage_error() {
    let (_dir, path) = temp_config();
    // 'bogus' is neither a configured alias nor an OS name → exit 2.
    assert_eq!(run_vm(Some(&path), &["exec", "bogus", "--", "true"]), 2);
}

#[test]
fn vm_lifecycle_failure_is_an_infra_error() {
    let (_dir, path) = temp_config();
    // 'lin' resolves, but its Parallels VM does not exist (or prlctl is absent
    // in CI) → vm can't do its job → exit 125, distinct from any guest code.
    assert_eq!(run_vm(Some(&path), &["exec", "lin", "--", "true"]), 125);
}
