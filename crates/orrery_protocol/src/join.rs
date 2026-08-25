//! The versioned, portable campaign join-file format.
//!
//! This is deliberately a named-field JSON document rather than a positional
//! list: a volunteer never has to decide which opaque value belongs to which
//! command-line flag. It carries a token, so callers must treat its file like
//! any other short-lived credential.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The literal format marker required in a [`CampaignJoinFileV1`].
pub const CAMPAIGN_JOIN_FILE_V1_FORMAT: &str = "orrery-campaign-join-v1";

/// A versioned campaign launch document emitted by `orrery-invite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignJoinFileV1 {
    /// Literal format marker, which rejects a file from another format version.
    pub format: String,
    /// Hex-encoded `NodeId` of the host to dial.
    pub host_node: String,
    /// Slot whose deterministic transport key the token is bound to.
    pub slot: usize,
    /// Coordinator-issued campaign session identifier.
    pub session_id: String,
    /// Hex-encoded `SessionTokenV1` bound to the slot's transport key.
    pub session_token: String,
}

impl CampaignJoinFileV1 {
    /// Build a V1 document with its required format marker.
    #[must_use]
    pub fn new(host_node: String, slot: usize, session_id: String, session_token: String) -> Self {
        Self {
            format: CAMPAIGN_JOIN_FILE_V1_FORMAT.to_owned(),
            host_node,
            slot,
            session_id,
            session_token,
        }
    }

    /// Parse and validate one JSON join document.
    pub fn from_json(text: &str) -> Result<Self, CampaignJoinFileVersionError> {
        let document = serde_json::from_str::<Self>(text)
            .map_err(|error| CampaignJoinFileVersionError::Json(error.to_string()))?;
        if document.format != CAMPAIGN_JOIN_FILE_V1_FORMAT {
            return Err(CampaignJoinFileVersionError::UnsupportedFormat(
                document.format,
            ));
        }
        Ok(document)
    }

    /// Serialize the document as compact JSON followed by a newline.
    pub fn to_json(&self) -> Result<String, CampaignJoinFileVersionError> {
        serde_json::to_string(self)
            .map(|json| format!("{json}\n"))
            .map_err(|error| CampaignJoinFileVersionError::Json(error.to_string()))
    }
}

/// A join document was malformed or from an unsupported format version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CampaignJoinFileVersionError {
    /// JSON did not decode to the required named-field document.
    Json(String),
    /// The document identified a format this client does not support.
    UnsupportedFormat(String),
}

impl fmt::Display for CampaignJoinFileVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid campaign join file: {error}"),
            Self::UnsupportedFormat(format) => {
                write!(
                    formatter,
                    "unsupported campaign join-file format {format:?}"
                )
            }
        }
    }
}

impl std::error::Error for CampaignJoinFileVersionError {}

#[cfg(test)]
mod tests {
    use super::{CampaignJoinFileV1, CampaignJoinFileVersionError};

    #[test]
    fn v1_round_trips_with_explicit_field_names() {
        let document = CampaignJoinFileV1::new(
            "aabb".to_owned(),
            3,
            "session-7".to_owned(),
            "ccdd".to_owned(),
        );
        let json = document.to_json().expect("serialize");
        assert!(json.contains("\"host_node\""));
        assert_eq!(CampaignJoinFileV1::from_json(&json), Ok(document));
    }

    #[test]
    fn unsupported_format_is_named() {
        assert_eq!(
            CampaignJoinFileV1::from_json(
                r#"{"format":"orrery-campaign-join-v0","host_node":"a","slot":1,"session_id":"s","session_token":"t"}"#,
            ),
            Err(CampaignJoinFileVersionError::UnsupportedFormat(
                "orrery-campaign-join-v0".to_owned()
            ))
        );
    }
}
