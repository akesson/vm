use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;

/// Version of the host↔guest request format. The guest agent rejects
/// requests from a different version; the host responds by redeploying
/// the agent (`vm deploy`).
///
/// v2 dropped the `shell` flag: the host now composes any shell invocation
/// itself (it knows the guest's OS from the config), so a request is always a
/// plain argv and the guest has no interpreting left to do.
pub const PROTO_VERSION: u32 = 2;

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
}

impl ExecRequest {
    /// Read one newline-terminated JSON request. The stream is deliberately
    /// NOT read to EOF: the host keeps it open for the lifetime of the
    /// command, and the agent watches it — EOF means the host or connection
    /// died, and the whole process tree must be torn down (sshd sends no
    /// signal on disconnect for no-PTY sessions).
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
}
