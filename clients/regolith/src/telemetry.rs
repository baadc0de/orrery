//! Overlay values and their unconditional JSONL stream.

use bevy::prelude::*;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::intent::OrderPacket;
use orrery_core::QVel;
use orrery_protocol::PersistId;

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
///
/// The writer is optional because an unwritable path is a **degradable**
/// condition: a game that cannot record telemetry can still be played, and
/// panicking during plugin registration turned a recoverable annoyance into a
/// process that died before its first frame, with nothing on screen for the
/// volunteer to read (#772). A sink without a writer accepts every append and
/// keeps nothing; what the player is owed is being *told*, which the scope
/// banner does.
#[derive(Resource)]
pub struct JsonlTelemetry {
    writer: Option<BufWriter<File>>,
    session_start: u64,
}

impl JsonlTelemetry {
    /// Open an append-only JSONL stream, creating its parent directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        // The stream is append-only across every session this binary ever
        // plays, so the file already holds earlier sessions' rows. The offset
        // the first append of *this* run lands at is the only boundary
        // separating them, and the upload is scoped to it (#735).
        let session_start = file.metadata()?.len();
        Ok(Self {
            writer: Some(BufWriter::new(file)),
            session_start,
        })
    }

    /// Open the stream, or carry on without one and say why.
    ///
    /// The second element is `None` while recording works and otherwise the
    /// human sentence describing what failed, for the player-visible notice.
    /// No launch may die of this: see the type's own note (#772).
    #[must_use]
    pub fn open_or_unavailable(path: &Path) -> (Self, Option<String>) {
        match Self::open(path) {
            Ok(sink) => (sink, None),
            Err(error) => (
                Self {
                    writer: None,
                    session_start: 0,
                },
                Some(format!("{}: {error}", path.display())),
            ),
        }
    }

    /// Whether this sink is keeping anything at all.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        self.writer.is_some()
    }

    /// Byte offset in the stream at which this session's rows begin.
    ///
    /// Rows before it belong to earlier sessions of the same binary. They stay
    /// on disk for the player; they are not this session's evidence.
    #[must_use]
    pub const fn session_start(&self) -> u64 {
        self.session_start
    }

    /// Append and flush one snapshot so tailing processes see it immediately.
    pub fn append(&mut self, metrics: &OverlayMetrics) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(
            &mut *writer,
            &serde_json::json!({
                "kind": "overlay",
                "session_scope": metrics.session_scope,
                "values": metrics,
            }),
        )?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// Append the exact core bytes emitted for one human-controlled tick.
    pub fn append_orders(
        &mut self,
        packet: &OrderPacket,
        session_scope: SessionScope,
    ) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(
            &mut *writer,
            &serde_json::json!({
                "kind": "orders",
                "session_scope": session_scope,
                "packet": packet,
            }),
        )?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// Append one human-readable canonical collision delivery.
    pub fn append_collision_resolved(
        &mut self,
        tick: u64,
        entity: PersistId,
        from: PersistId,
        velocity: QVel,
        session_scope: SessionScope,
    ) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(
            &mut *writer,
            &serde_json::json!({
                "kind": "CollisionResolved",
                "session_scope": session_scope,
                "tick": tick,
                "entity": entity.0,
                "payload": {
                    "from": from.0,
                    "velocity": { "x": velocity.x, "y": velocity.y, "z": velocity.z },
                },
            }),
        )?;
        writer.write_all(b"\n")?;
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_delivery_is_named_with_its_canonical_payload() {
        let path = std::env::temp_dir().join(format!(
            "orrery-regolith-collision-{}.jsonl",
            std::process::id()
        ));
        let mut sink = JsonlTelemetry::open(&path).expect("open telemetry");
        sink.append_collision_resolved(
            17,
            PersistId::new(33),
            PersistId::new(32),
            QVel { x: 4, y: 5, z: 6 },
            SessionScope::Campaign,
        )
        .expect("append collision");
        drop(sink);

        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&path)
                .expect("read telemetry")
                .trim(),
        )
        .expect("parse row");
        assert_eq!(row["kind"], "CollisionResolved");
        assert_eq!(row["tick"], 17);
        assert_eq!(row["entity"], 33);
        assert_eq!(row["payload"]["from"], 32);
        assert_eq!(row["payload"]["velocity"]["x"], 4);
        let _ = std::fs::remove_file(path);
    }

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
