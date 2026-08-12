//! JSON telemetry output for the punch-rate dashboard.
//!
//! When `--json` is set, each session event is emitted as one JSON object per
//! line on stdout (tracing logs go to stderr so stdout stays machine-parseable).
//! Every record carries a timestamp, the local node id, the role, and the peer
//! index so a collector can correlate host and peer sides of the same pair.

use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use serde::Serialize;

use crate::session::{PathState, SessionEvent};

/// Context shared by every telemetry record.
#[derive(Debug, Clone)]
pub struct TelemetryContext {
    pub node: EndpointId,
    pub role: &'static str,
}

/// One JSON telemetry record.
#[derive(Debug, Serialize)]
struct Record<'a> {
    ts: u64,
    node: String,
    role: &'a str,
    peer: usize,
    #[serde(flatten)]
    event: Event<'a>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    Connected {
        remote: String,
    },
    Path {
        path: &'a str,
        ttd_ms: Option<u64>,
    },
    Stats {
        sent: u64,
        received: u64,
        dropped: u64,
    },
    Error {
        error: &'a str,
    },
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn path_name(p: &PathState) -> &'static str {
    match p {
        PathState::Relay => "relay",
        PathState::Direct => "direct",
        PathState::Mixed => "mixed",
    }
}

/// Emit one JSON line for a session event. `ttd_ms` is the time-to-direct-path
/// for the peer, computed by the caller from the connect timestamp.
pub fn emit(ctx: &TelemetryContext, peer: usize, event: &SessionEvent, ttd_ms: Option<u64>) {
    let event = match event {
        SessionEvent::Connected { remote, .. } => Event::Connected {
            remote: remote.to_string(),
        },
        SessionEvent::Path { path, .. } => Event::Path {
            path: path_name(path),
            ttd_ms,
        },
        SessionEvent::Stats {
            sent,
            received,
            dropped,
            ..
        } => Event::Stats {
            sent: *sent,
            received: *received,
            dropped: *dropped,
        },
        SessionEvent::Error { error, .. } => Event::Error { error },
    };

    let record = Record {
        ts: now_ms(),
        node: ctx.node.to_string(),
        role: ctx.role,
        peer,
        event,
    };

    // Serialize to a single line. serde_json never emits newlines in values, so
    // one record == one line.
    let line = serde_json::to_string(&record).unwrap_or_else(|e| {
        format!(
            "{{\"ts\":{},\"node\":\"{}\",\"role\":\"{}\",\"peer\":{},\"type\":\"error\",\"error\":\"serialize failed: {}\"}}",
            now_ms(),
            ctx.node,
            ctx.role,
            peer,
            e
        )
    });
    println!("{line}");
}
