//! Overlay values and their unconditional JSONL stream.

use bevy::prelude::*;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::intent::OrderPacket;

/// Whether a telemetry row came from a live campaign or a local-only world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    /// No live campaign link backed this row.
    Local,
    /// A joined campaign link backed this row.
    Campaign,
}

/// Values shown in the always-on strip and F3 pane.
#[derive(Debug, Clone, Resource, Serialize)]
pub struct OverlayMetrics {
    /// Whether these values measure a live campaign or local-only play.
    pub session_scope: SessionScope,
    /// Input orders emitted during the last one-second window.
    pub intents_per_second: u64,
    /// Rollbacks observed during the last minute.
    pub rollbacks_per_minute: u64,
    /// Currently live discrepancy episodes.
    pub live_discrepancies: u64,
    /// Completed adjudications.
    pub adjudications_completed: u64,
    /// Adjudication latency p50.
    pub adjudication_latency_p50_ms: u64,
    /// Adjudication latency p99.
    pub adjudication_latency_p99_ms: u64,
    /// Current prediction set size.
    pub prediction_set_size: u64,
    /// Observed packet loss percentage.
    pub observed_loss_pct: f64,
    /// Configured packet loss percentage.
    pub configured_loss_pct: f64,
    /// Observed jitter p50.
    pub observed_jitter_p50_ms: u64,
    /// Observed jitter p99.
    pub observed_jitter_p99_ms: u64,
    /// Configured jitter.
    pub configured_jitter_ms: u64,
    /// Current island identifier, if joined.
    pub island_id: Option<u64>,
    /// Current cell identifier, if known.
    pub cell_id: Option<u64>,
    /// Session record path displayed to the operator.
    pub session_record_path: PathBuf,
    /// Minutes accepted by the campaign ledger.
    pub banked_minutes: f64,
    /// Minutes without player input.
    pub idle_minutes: f64,
}

impl OverlayMetrics {
    /// Values for a new local session.
    #[must_use]
    pub fn new(session_record_path: PathBuf) -> Self {
        Self {
            session_scope: SessionScope::Local,
            intents_per_second: 0,
            rollbacks_per_minute: 0,
            live_discrepancies: 0,
            adjudications_completed: 0,
            adjudication_latency_p50_ms: 0,
            adjudication_latency_p99_ms: 0,
            prediction_set_size: 2,
            observed_loss_pct: 0.0,
            configured_loss_pct: 0.0,
            observed_jitter_p50_ms: 0,
            observed_jitter_p99_ms: 0,
            configured_jitter_ms: 0,
            island_id: None,
            cell_id: None,
            session_record_path,
            banked_minutes: 0.0,
            idle_minutes: 0.0,
        }
    }
}

/// Append-only telemetry output. It runs whether or not the F3 pane is open.
#[derive(Resource)]
pub struct JsonlTelemetry {
    writer: BufWriter<File>,
}

impl JsonlTelemetry {
    /// Open an append-only JSONL stream, creating its parent directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    /// Append and flush one snapshot so tailing processes see it immediately.
    pub fn append(&mut self, metrics: &OverlayMetrics) -> io::Result<()> {
        serde_json::to_writer(
            &mut self.writer,
            &serde_json::json!({
                "kind": "overlay",
                "session_scope": metrics.session_scope,
                "values": metrics,
            }),
        )?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    /// Append the exact core bytes emitted for one human-controlled tick.
    pub fn append_orders(
        &mut self,
        packet: &OrderPacket,
        session_scope: SessionScope,
    ) -> io::Result<()> {
        serde_json::to_writer(
            &mut self.writer,
            &serde_json::json!({
                "kind": "orders",
                "session_scope": session_scope,
                "packet": packet,
            }),
        )?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_overlay_value_is_present_in_jsonl_when_f3_is_closed() {
        let path = std::env::temp_dir().join(format!(
            "orrery-regolith-telemetry-{}.jsonl",
            std::process::id()
        ));
        let mut sink = JsonlTelemetry::open(&path).expect("open telemetry");
        sink.append(&OverlayMetrics::new(path.clone()))
            .expect("append telemetry");
        drop(sink);
        let line = std::fs::read_to_string(&path).expect("read telemetry");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid jsonl");
        assert_eq!(value["session_scope"], "local");
        let values = value.get("values").expect("overlay envelope");
        for field in [
            "session_scope",
            "intents_per_second",
            "rollbacks_per_minute",
            "live_discrepancies",
            "adjudications_completed",
            "adjudication_latency_p50_ms",
            "adjudication_latency_p99_ms",
            "prediction_set_size",
            "observed_loss_pct",
            "configured_loss_pct",
            "observed_jitter_p50_ms",
            "observed_jitter_p99_ms",
            "configured_jitter_ms",
            "island_id",
            "cell_id",
            "session_record_path",
            "banked_minutes",
            "idle_minutes",
        ] {
            assert!(values.get(field).is_some(), "missing {field}");
        }
        std::fs::remove_file(path).expect("remove test output");
    }

    #[test]
    fn every_telemetry_envelope_declares_its_session_scope() {
        let path = std::env::temp_dir().join(format!(
            "orrery-regolith-scoped-telemetry-{}.jsonl",
            std::process::id()
        ));
        let mut sink = JsonlTelemetry::open(&path).expect("open telemetry");
        let packet = OrderPacket {
            tick: 1_000_000,
            entity: 1,
            orders: Vec::new(),
        };
        sink.append_orders(&packet, SessionScope::Local)
            .expect("append local orders");
        sink.append_orders(&packet, SessionScope::Campaign)
            .expect("append campaign orders");
        drop(sink);

        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .expect("read telemetry")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
            .collect();
        assert_eq!(rows[0]["session_scope"], "local");
        assert_eq!(rows[1]["session_scope"], "campaign");
        std::fs::remove_file(path).expect("remove test output");
    }
}
