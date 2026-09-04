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
    /// Local practice: this row belongs to a session that banks nothing.
    Local,
    /// This row belongs to a campaign session, and its minutes are bankable.
    ///
    /// The scope of the *session*, not of the instant: a campaign session
    /// whose host link has dropped keeps this scope, because the minutes it
    /// already flew were flown against a host. Reporting `Local` there put
    /// `banked_minutes: 12.93` on a row claiming to be local practice (#942).
    Campaign,
}

/// Values shown in the always-on strip and F3 pane.
///
/// **Every field here is assigned from a measurement on the live path, and
/// `scripts/telemetry-liveness-gate.py` fails the build if one stops being**
/// (`session_record_path` is the single declared exemption, and the gate
/// carries the reason). That gate exists because six of these fields were set
/// once in [`OverlayMetrics::new`] and never again: `rollbacks_per_minute`,
/// `live_discrepancies`, `adjudications_completed`,
/// `adjudication_latency_p50_ms`, `adjudication_latency_p99_ms` and
/// `prediction_set_size`. A twelve-minute human session on 2026-09-04
/// recorded `rollbacks_per_minute: 0` and `prediction_set_size: 2` — the
/// constructor's literals, indistinguishable in the JSONL from a
/// measurement, and read back as if they were one. A missing field is an
/// obvious gap; a defaulted field is quoted as evidence (#1029).
///
/// Five of the six are gone rather than wired, because this client cannot
/// produce them:
///
/// - **`rollbacks_per_minute`.** This client never rolls back. It is the sole
///   authority over its own craft and steps it straight-line; replicated
///   bodies are installed verbatim by `CampaignRuntime::advance` and no peer
///   ever broadcasts state for an entity this client authors, so there is no
///   correction to reconcile against and nothing to resimulate. Late
///   deliveries are applied at their arrival tick and never back-dated (D46
///   clause (d), docs/05 §2 case 3). `orrery_predict`'s rollback machinery is
///   registered by `OrreryPredictPlugin` and, in this client, unfed:
///   `ReconciliationMonitor` and `AuthorityCorrectionInbox` have no producer
///   here. A field named for rollbacks must count rollbacks, and zero is the
///   true count for a reason no counter would have shown.
/// - **`live_discrepancies`, `adjudications_completed`,
///   `adjudication_latency_p50_ms`, `adjudication_latency_p99_ms`.** This
///   client authors its own witness stream and watches no other subject;
///   discrepancy episodes are opened and adjudicated off this machine, and
///   nothing in the wire format carries a verdict back to it. Surfacing them
///   is a protocol addition, not a wiring job, and the campaign is pinned.
#[derive(Debug, Clone, Resource, Serialize)]
pub struct OverlayMetrics {
    /// Whether these values measure a live campaign or local-only play.
    pub session_scope: SessionScope,
    /// Input orders emitted during the last one-second window.
    pub intents_per_second: u64,
    /// Entities this client advanced by simulation on its most recent tick.
    ///
    /// D8's predicted set (docs/05 §2), counted rather than assumed: how many
    /// `Executor::step_entity` calls the last driven tick actually made. A
    /// joined campaign steps exactly one — this craft — because every other
    /// body in the executor is a replica installed verbatim from the wire and
    /// nothing advances it locally. Local practice steps two, the player and
    /// the bot.
    ///
    /// Zero is the informative reading: it means the client stepped nothing,
    /// which is a session that is not joined, or one whose own order packet
    /// failed to decode and skipped its step (the path that also increments
    /// `own_orders_undecodable`). Before #1029 this said `2` in a joined
    /// campaign, which was the constructor's literal and wrong in both
    /// modes.
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
    /// The host's attempt generation, once a `StartV1` manifest has been
    /// adopted.
    ///
    /// This replaces `island_id` (#942), which was declared, defaulted to
    /// `None`, emitted on every row and printed on the HUD without ever being
    /// assigned anywhere in the client — a field that is always null while
    /// being presented as state is worse than absent, and it read identically
    /// in a campaign and in local practice. The attempt id is the join between
    /// a client's rows and the host's attempt report, which is exactly what
    /// had to be reconstructed by hand when the 2026-09-02 sessions were
    /// triaged.
    pub attempt_id: Option<String>,
    /// Current cell identifier, if known.
    pub cell_id: Option<u64>,
    /// Session record path displayed to the operator.
    pub session_record_path: PathBuf,
    /// Minutes accepted by the campaign ledger.
    pub banked_minutes: f64,
    /// Minutes without player input.
    pub idle_minutes: f64,
    /// Whether the idle banking allowance has been exhausted.
    ///
    /// Reachable for the first time in #947: while the campaign accumulator
    /// was fed an order count that could never be zero, this could only ever
    /// be `false`.
    pub afk_capped: bool,
    /// Uplink frames the bounded channel refused (visible backpressure).
    ///
    /// This and the three counters below are incremented on real failure
    /// paths and, before #947, surfaced only in the F3 pane — which is closed
    /// by default. None of them reached the JSONL a volunteer ships back, so
    /// a lost session could not be diagnosed from the evidence sent with it.
    /// They are additions to the schema; no existing key changed, because
    /// `scripts/p4-attempt-accounting.py` reads `banked_minutes` off these
    /// rows by name.
    pub uplink_shed: u64,
    /// This client's own order packet failing to decode (#1034).
    ///
    /// Split from `downlink_undecodable`, which both sides used to feed and
    /// which made the 2026-09-04 session's 28 740 uninterpretable. This
    /// counter is the own side alone: a non-zero value means ticks whose
    /// order packet did not decode, each a skipped step — a literal
    /// one-tick freeze of this craft — with no downlink implication at all.
    pub own_orders_undecodable: u64,
    /// Received downlink traffic that decoded to nothing this client
    /// recognises (#1034).
    ///
    /// The received side alone, since the split; the client's own packet has
    /// [`Self::own_orders_undecodable`]. Read with care before calling it a
    /// network failure: witness frames from the bot cohort ride the same
    /// datagram lane as replication (`orrery_net`'s `send_peer_packets` puts
    /// every `Channel::State` send on it) and land in the neither-replication
    /// arm, so a healthy island's steady witness cadence is counted here
    /// too. The bot cohort's own receive path exempts those frames.
    pub downlink_undecodable: u64,
    /// Ruleset deliveries for an entity this client could not route to.
    pub delivered_unroutable: u64,
    /// Delivered inputs addressed to another authority and refused here.
    pub delivered_foreign: u64,
}

impl OverlayMetrics {
    /// Values for a new local session.
    #[must_use]
    pub fn new(session_record_path: PathBuf) -> Self {
        Self {
            session_scope: SessionScope::Local,
            intents_per_second: 0,
            // Nothing has been stepped yet, and this is the only reading of
            // it a frame ever sees before `stream_metrics` overwrites it from
            // the tick loop's own count.
            prediction_set_size: 0,
            observed_loss_pct: 0.0,
            configured_loss_pct: 0.0,
            observed_jitter_p50_ms: 0,
            observed_jitter_p99_ms: 0,
            configured_jitter_ms: 0,
            attempt_id: None,
            cell_id: None,
            session_record_path,
            banked_minutes: 0.0,
            idle_minutes: 0.0,
            afk_capped: false,
            uplink_shed: 0,
            own_orders_undecodable: 0,
            downlink_undecodable: 0,
            delivered_unroutable: 0,
            delivered_foreign: 0,
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
            "prediction_set_size",
            "observed_loss_pct",
            "configured_loss_pct",
            "observed_jitter_p50_ms",
            "observed_jitter_p99_ms",
            "configured_jitter_ms",
            "attempt_id",
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
