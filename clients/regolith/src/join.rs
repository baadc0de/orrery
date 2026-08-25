//! Campaign launch-input resolution for the Regolith command line.
//!
//! Precedence is deliberate: `--join` wins over every individual source;
//! without it, `--session-token` wins over `ORRERY_SESSION_TOKEN`. A token
//! argument beginning with `@` reads its payload from that file.

use orrery_protocol::CampaignJoinFileV1;
use std::ffi::OsString;
use std::path::Path;

/// Campaign fields resolved before the Bevy app is assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignJoinInput {
    /// Host NodeId as hexadecimal text.
    pub host_node: String,
    /// The joining process's deterministic transport-key slot.
    pub slot: usize,
    /// Coordinator-issued session ID.
    pub session_id: String,
    /// Optional encoded session token.
    pub session_token: Option<String>,
}

/// Resolve the campaign arguments using the supplied token environment value.
///
/// Supplying the environment explicitly keeps precedence tests independent of
/// the process environment.
pub fn resolve_campaign_input(
    args: &[OsString],
    env_session_token: Option<String>,
) -> Result<Option<CampaignJoinInput>, String> {
    if let Some(path) = flag_value(args, "--join") {
        let text = std::fs::read_to_string(&path).map_err(|error| {
            format!("cannot read --join {}: {error}", Path::new(&path).display())
        })?;
        let join = CampaignJoinFileV1::from_json(&text).map_err(|error| {
            format!(
                "cannot parse --join {}: {error}",
                Path::new(&path).display()
            )
        })?;
        return Ok(Some(CampaignJoinInput {
            host_node: join.host_node,
            slot: join.slot,
            session_id: join.session_id,
            session_token: Some(join.session_token),
        }));
    }

    let Some(host_node) = flag_value(args, "--host-node") else {
        return Ok(None);
    };
    let slot = flag_value(args, "--slot")
        .ok_or_else(|| {
            "--host-node needs --slot <n>: the slot derives your transport identity".to_owned()
        })?
        .parse::<usize>()
        .map_err(|_| "--slot needs a slot number".to_owned())?;
    let session_id =
        flag_value(args, "--session-id").unwrap_or_else(|| format!("local-{}", crate::BUILD_REV));
    let session_token = match flag_value(args, "--session-token") {
        Some(value) => Some(read_token_argument(&value)?),
        None => env_session_token,
    };
    Ok(Some(CampaignJoinInput {
        host_node,
        slot,
        session_id,
        session_token,
    }))
}

/// Resolve campaign arguments from this process's environment.
pub fn resolve_process_campaign_input(
    args: &[OsString],
) -> Result<Option<CampaignJoinInput>, String> {
    resolve_campaign_input(args, std::env::var("ORRERY_SESSION_TOKEN").ok())
}

fn flag_value(args: &[OsString], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].to_string_lossy().into_owned())
}

fn read_token_argument(argument: &str) -> Result<String, String> {
    let Some(path) = argument.strip_prefix('@') else {
        return Ok(argument.to_owned());
    };
    if path.is_empty() {
        return Err("--session-token @path needs a path after @".to_owned());
    }
    std::fs::read_to_string(path)
        .map(|token| token.trim().to_owned())
        .map_err(|error| format!("cannot read --session-token @{path}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::resolve_campaign_input;
    use orrery_protocol::CampaignJoinFileV1;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn join_file_wins_over_flags_and_environment() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("volunteer.join.json");
        std::fs::write(
            &path,
            CampaignJoinFileV1::new(
                "file-host".to_owned(),
                7,
                "file-session".to_owned(),
                "file-token".to_owned(),
            )
            .to_json()
            .expect("serialize join"),
        )
        .expect("write join");

        let resolved = resolve_campaign_input(
            &args(&[
                "regolith",
                "--join",
                path.to_str().expect("utf-8 path"),
                "--host-node",
                "flag-host",
                "--slot",
                "2",
                "--session-id",
                "flag-session",
                "--session-token",
                "flag-token",
            ]),
            Some("environment-token".to_owned()),
        )
        .expect("join resolves")
        .expect("campaign input");
        assert_eq!(resolved.host_node, "file-host");
        assert_eq!(resolved.slot, 7);
        assert_eq!(resolved.session_id, "file-session");
        assert_eq!(resolved.session_token.as_deref(), Some("file-token"));
    }

    #[test]
    fn session_token_flag_wins_over_environment() {
        let resolved = resolve_campaign_input(
            &args(&[
                "regolith",
                "--host-node",
                "host",
                "--slot",
                "2",
                "--session-token",
                "flag-token",
            ]),
            Some("environment-token".to_owned()),
        )
        .expect("flags resolve")
        .expect("campaign input");
        assert_eq!(resolved.session_token.as_deref(), Some("flag-token"));
    }

    #[test]
    fn session_token_at_path_reads_trimmed_payload() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("token");
        std::fs::write(&path, " token-from-file\n").expect("write token");
        let resolved = resolve_campaign_input(
            &args(&[
                "regolith",
                "--host-node",
                "host",
                "--slot",
                "2",
                "--session-token",
                &format!("@{}", path.display()),
            ]),
            None,
        )
        .expect("token file resolves")
        .expect("campaign input");
        assert_eq!(resolved.session_token.as_deref(), Some("token-from-file"));
    }
}
