use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

/// Version of the host↔guest request format. The guest agent rejects
/// requests from a different version; the host responds by redeploying
/// the agent (`vm deploy`).
///
/// v2 dropped the `shell` flag: the host now composes any shell invocation
/// itself (it knows the guest's OS from the config), so a request is always a
/// plain argv and the guest has no interpreting left to do.
///
/// v3 added `stdin`: `vm run` feeds the guest command an input payload
/// (`vm run lin -- sh < step.sh`), which `vm exec` never does.
///
/// v4 gave the pipe a pulse. After the request line the host now sends a
/// keepalive byte every [`HEARTBEAT_INTERVAL`], and the agent kills the process
/// tree after [`HEARTBEAT_TIMEOUT`] of silence as well as on EOF — because the
/// EOF may never come. Over `prlctl exec` Parallels Tools can leave the guest's
/// end of stdin open after the host-side `prlctl` is gone, and the agent then
/// sits blocked on a pipe nobody will ever write to again: measured on the
/// macOS guest, a killed `vm run --elevated` left both the agent and its command
/// running indefinitely (#21). The two sides must not mix in either direction —
/// a v3 host is silent, and a v4 agent would read that silence as a dead host
/// and kill a perfectly good command a minute in — which is precisely what the
/// version gate below prevents.
pub const PROTO_VERSION: u32 = 4;

/// How often the host writes a keepalive byte on the request pipe, for as long
/// as the guest command runs.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long the agent lets that pipe stay silent before it calls the host dead
/// and tears the guest's process tree down. Four missed beats — the budget ssh's
/// own keepalives already run on (`ServerAliveInterval=15`,
/// `ServerAliveCountMax=4`, see [`crate::ssh`]), so a killed host and a frozen
/// VM are noticed on the same timescale no matter which transport is underneath.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// A command to run in the guest, sent as JSON on the agent's stdin. argv is
/// always spawned natively — never through a shell — so arguments survive
/// byte-for-byte: spaces, quotes, unicode, newlines. A command the caller meant
/// as a *script* arrives already wrapped in the guest's shell by the host (see
/// `exec::host::build_argv`), which is just more argv by the time it lands here.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecRequest {
    pub version: u32,
    pub argv: Vec<String>,
    /// Extra environment (merged over the agent's own environment)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory; a leading `~/` is expanded against the guest home
    pub cwd: String,
    /// Input for the guest command's stdin, fed to it and then closed (EOF).
    /// `None` — every `vm exec` — leaves the child on the null device.
    ///
    /// It rides *inside* this JSON line rather than after it, so the bytes the
    /// payload is made of can never be mistaken for the heartbeats the same
    /// pipe carries once the request is read (see [`ExecRequest::read_from`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Override for the agent's silence budget, in milliseconds
    /// ([`HEARTBEAT_TIMEOUT`] when absent). The host always sends `None`; it
    /// exists so a test can watch the timeout fire in half a second rather than
    /// in a minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout_ms: Option<u64>,
}

impl ExecRequest {
    /// Read one newline-terminated JSON request. The stream is deliberately
    /// NOT read to EOF: the host keeps it open for the lifetime of the command
    /// and heartbeats on it, and the agent watches it. EOF *or* silence past
    /// [`HEARTBEAT_TIMEOUT`] means the host or the connection died, and the
    /// whole process tree must be torn down. Both halves are load-bearing: ssh
    /// delivers the EOF but no signal (no-PTY sessions get none on disconnect),
    /// and the prlctl channels deliver no EOF at all — there, silence is the
    /// only news of a dead host that ever arrives.
    pub fn read_from(input: impl Read) -> Result<ExecRequest> {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(input)
            .read_line(&mut line)
            .context("reading ExecRequest from stdin")?;
        let req: ExecRequest = serde_json::from_str(&line).context("parsing ExecRequest JSON")?;
        if req.version != PROTO_VERSION {
            bail!(
                "protocol version mismatch: host sent v{}, agent speaks v{PROTO_VERSION} \
                 (run `vm deploy` to update the guest agent)",
                req.version
            );
        }
        if req.argv.is_empty() {
            bail!("ExecRequest.argv is empty");
        }
        Ok(req)
    }
}

/// Reply printed by `vm _version`, one line of JSON on stdout.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionInfo {
    pub proto: u32,
    pub binary: String,
}

impl VersionInfo {
    pub fn current() -> VersionInfo {
        VersionInfo {
            proto: PROTO_VERSION,
            binary: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(argv: &[&str]) -> ExecRequest {
        ExecRequest {
            version: PROTO_VERSION,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            cwd: "~/work/repo".into(),
            stdin: None,
            heartbeat_timeout_ms: None,
        }
    }

    #[test]
    fn roundtrips_hostile_arguments() {
        let mut req = request(&[
            "echo",
            "with space",
            "quote\"inside",
            "single'quote",
            "new\nline",
            "uni-→-code",
            "%PATH%",
            "$HOME",
            "back\\slash",
        ]);
        req.env.insert("KEY".into(), "va l=ue".into());
        let json = serde_json::to_string(&req).unwrap();
        let back = ExecRequest::read_from(json.as_bytes()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn rejects_version_mismatch_with_hint() {
        let mut req = request(&["true"]);
        req.version = PROTO_VERSION + 1;
        let json = serde_json::to_string(&req).unwrap();
        let err = ExecRequest::read_from(json.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(err.contains("vm deploy"), "{err}");
    }

    #[test]
    fn rejects_empty_argv() {
        let json = serde_json::to_string(&request(&[])).unwrap();
        assert!(ExecRequest::read_from(json.as_bytes()).is_err());
    }

    #[test]
    fn env_defaults_when_absent() {
        let json = format!(r#"{{"version":{PROTO_VERSION},"argv":["ls"],"cwd":"/tmp"}}"#);
        let req = ExecRequest::read_from(json.as_bytes()).unwrap();
        assert!(req.env.is_empty());
    }

    /// The payload is a *script* as often as not, so the bytes that would end a
    /// JSON line — newlines above all — have to survive it intact.
    #[test]
    fn a_stdin_payload_roundtrips_through_the_one_json_line() {
        let mut req = request(&["sh"]);
        req.stdin = Some("set -e\necho 'quoted \"thing\"'\nexit 7\n".into());
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains('\n'),
            "the request must stay one line: {json}"
        );
        assert_eq!(ExecRequest::read_from(json.as_bytes()).unwrap(), req);
    }

    /// Every `vm exec` sends no payload, and the guest must read that as "null
    /// device", not as an empty one — an absent field and `""` are different.
    #[test]
    fn stdin_defaults_to_none_when_absent() {
        let json = format!(r#"{{"version":{PROTO_VERSION},"argv":["ls"],"cwd":"/tmp"}}"#);
        assert_eq!(ExecRequest::read_from(json.as_bytes()).unwrap().stdin, None);
    }

    #[test]
    fn a_v2_host_is_rejected_with_the_redeploy_hint() {
        // v2 is the version before `stdin`; an un-redeployed guest is exactly
        // what a v2 host talking to a v3 agent (and the reverse) hits, and both
        // must land on the actionable hint rather than a serde complaint.
        let v2 = r#"{"version":2,"argv":["ls"],"cwd":"/tmp"}"#;
        let err = ExecRequest::read_from(v2.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(err.contains("vm deploy"), "{err}");
        assert!(err.contains("host sent v2"), "{err}");
    }

    #[test]
    fn a_v1_host_is_rejected_with_the_redeploy_hint() {
        // v1 carried a `shell` field this struct no longer has. An old host must
        // still fail on the *version*, not on a JSON parse error: the reader gets
        // the actionable "run `vm deploy`" rather than a serde complaint.
        let v1 = r#"{"version":1,"argv":["ls"],"cwd":"/tmp","shell":true}"#;
        let err = ExecRequest::read_from(v1.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(err.contains("vm deploy"), "{err}");
        assert!(err.contains("host sent v1"), "{err}");
    }

    /// The pairing this version gate exists for. A v3 request parses cleanly
    /// into a v4 struct — every field it has, v4 still has — so nothing but the
    /// version number stands between a v3 host and a v4 agent that would read
    /// its silence as a dead host and kill the command a minute in.
    #[test]
    fn a_v3_host_is_rejected_rather_than_left_silent() {
        let v3 = r#"{"version":3,"argv":["cargo","build"],"cwd":"/tmp"}"#;
        let err = ExecRequest::read_from(v3.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(err.contains("vm deploy"), "{err}");
        assert!(err.contains("host sent v3"), "{err}");
    }

    /// The host never sends the override, so the agent has to read its absence
    /// as "the real budget" — a 0, or a serde error, would be a disaster in
    /// opposite directions.
    #[test]
    fn heartbeat_timeout_defaults_to_none_when_absent() {
        let json = format!(r#"{{"version":{PROTO_VERSION},"argv":["ls"],"cwd":"/tmp"}}"#);
        let req = ExecRequest::read_from(json.as_bytes()).unwrap();
        assert_eq!(req.heartbeat_timeout_ms, None);
    }

    #[test]
    fn a_heartbeat_timeout_override_roundtrips() {
        let mut req = request(&["sleep", "300"]);
        req.heartbeat_timeout_ms = Some(400);
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(ExecRequest::read_from(json.as_bytes()).unwrap(), req);
    }

    /// The two constants are one contract, not two numbers: the agent must sit
    /// through several missed beats before it calls the host dead, or a single
    /// slow scheduler tick on either side becomes a killed build.
    #[test]
    fn the_silence_budget_outlasts_several_missed_beats() {
        assert!(HEARTBEAT_TIMEOUT >= 3 * HEARTBEAT_INTERVAL);
    }
}
