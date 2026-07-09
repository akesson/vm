use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;

/// Version of the host↔guest request format. The guest agent rejects
/// requests from a different version; the host responds by redeploying
/// the agent (`vm deploy`).
pub const PROTO_VERSION: u32 = 1;

/// A command to run in the guest, sent as JSON on the agent's stdin.
/// argv is spawned natively (no shell) unless `shell` is set, so arguments
/// survive byte-for-byte: spaces, quotes, unicode, newlines.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecRequest {
    pub version: u32,
    pub argv: Vec<String>,
    /// Extra environment (merged over the agent's own environment)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory; a leading `~/` is expanded against the guest home
    pub cwd: String,
    /// Join argv and run through the guest shell (cmd.exe /C or sh -c)
    #[serde(default)]
    pub shell: bool,
}

impl ExecRequest {
    pub fn read_from(mut input: impl Read) -> Result<ExecRequest> {
        let mut buf = String::new();
        input
            .read_to_string(&mut buf)
            .context("reading ExecRequest from stdin")?;
        let req: ExecRequest = serde_json::from_str(&buf).context("parsing ExecRequest JSON")?;
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
            shell: false,
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
    fn env_and_shell_default_when_absent() {
        let json = format!(r#"{{"version":{PROTO_VERSION},"argv":["ls"],"cwd":"/tmp"}}"#);
        let req = ExecRequest::read_from(json.as_bytes()).unwrap();
        assert!(req.env.is_empty());
        assert!(!req.shell);
    }
}
