//! Campaign consent, measured-link records, and AFK accounting.
//!
//! This module deliberately has no Bevy dependency beyond the client's normal
//! build: bots and the rendered client feed the same [`CampaignSession`].

use std::io::{self, Write};

use orrery_core::TICK_HZ;
use orrery_games::regolith::REGOLITH_RULESET;
use serde::Serialize;

/// The notice displayed before a campaign client is allowed to join.
pub const CONSENT_NOTICE: &str = "**This build records your play.** Inputs, world-state hashes and network\nmeasurements are logged for replay-adjudication research. Recording is\nshadow-mode: reports never affect your account or your access. Playing a\ncampaign build means you consent to recording; if you do not consent, do not\nplay campaign builds.";

/// The source of the canonical input stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// A deterministic pilot supplies inputs.
    Bot,
    /// A person supplies inputs through the skin.
    Human,
}

/// The impairment profile requested for a session, expressed for operators.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ConfiguredImpairment {
    /// Expected dropped-packet percentage.
    pub loss_pct: f64,
    /// Expected median jitter in milliseconds.
    pub jitter_p50_ms: u64,
    /// Expected p99 jitter in milliseconds.
    pub jitter_p99_ms: u64,
}

/// Measurements taken from packets actually sent during this session.
#[derive(Debug, Default)]
pub struct ImpairmentMeasurement {
    sent: u64,
    dropped: u64,
    jitter_ms: Vec<u64>,
}

impl ImpairmentMeasurement {
    /// Record one observed transport outcome; this is not configuration echo.
    pub fn observe(&mut self, dropped: bool, jitter_ms: u64) {
        self.sent = self.sent.saturating_add(1);
        self.dropped = self.dropped.saturating_add(u64::from(dropped));
        self.jitter_ms.push(jitter_ms);
    }

    fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            self.dropped as f64 * 100.0 / self.sent as f64
        }
    }

    fn percentile(&self, numerator: usize) -> u64 {
        if self.jitter_ms.is_empty() {
            return 0;
        }
        let mut values = self.jitter_ms.clone();
        values.sort_unstable();
        let index = (values.len() * numerator).div_ceil(100).saturating_sub(1);
        values[index]
    }
}

/// A finished session row, carried inside the P4 report and ledger line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRecord {
    /// Coordinator-issued session identity; human records also use it as the
    /// ledger's `identity.human_session_id`.
    pub session_id: String,
    /// UTC start time supplied by the coordinator.
    pub wall_start: String,
    /// UTC end time supplied by the coordinator.
    pub wall_end: String,
    /// Connected minutes before the AFK banking cap is applied.
    pub distinct_play_minutes: f64,
    /// Minutes eligible to bank after the AFK rule is applied.
    pub banked_minutes: f64,
    /// Rust target triple for this client build.
    pub platform_triple: String,
    /// Client revision supplied by the campaign launcher.
    pub client_rev: String,
    /// Ruleset identity pinned into the replay.
    pub ruleset_id: String,
    /// Ruleset version pinned into the replay.
    pub ruleset_version: u32,
    /// P4 pipeline digest supplied by the coordinator after it hashes its four trees.
    pub pipeline_digest: String,
    /// Input source; bots and humans share this record/code path.
    pub actor: Actor,
    /// Requested impairment, retained separately from observations.
    pub configured_impairment_profile: ConfiguredImpairment,
    /// Packet loss measured from observed transport outcomes.
    pub observed_loss_pct: f64,
    /// Measured jitter median.
    pub observed_jitter_p50_ms: u64,
    /// Measured jitter p99.
    pub observed_jitter_p99_ms: u64,
    /// Total seconds with no local intents.
    pub afk_seconds: u64,
    /// Whether the session exhausted its 600-second idle banking allowance.
    pub afk_capped: bool,
    /// Observations disagree with the requested profile; retained in-row and
    /// therefore cannot be silently treated as verified impairment.
    pub impairment_mismatch: bool,
}

/// Pure session accumulator used by both bot and human front ends.
#[derive(Debug)]
pub struct CampaignSession {
    session_id: String,
    wall_start: String,
    actor: Actor,
    configured: ConfiguredImpairment,
    measurement: ImpairmentMeasurement,
    connected_ticks: u64,
    banked_ticks: u64,
    idle_ticks: u64,
    idle_banked_ticks: u64,
    afk_capped: bool,
}

impl CampaignSession {
    /// No local intent for this long is considered idle.
    pub const IDLE_AFTER_SECONDS: u64 = 300;
    /// A session can bank at most this much idle time.
    pub const IDLE_BANK_CAP_SECONDS: u64 = 600;

    /// Start a consented session. `session_id` is coordinator-issued for humans.
    #[must_use]
    pub fn new(
        session_id: String,
        wall_start: String,
        actor: Actor,
        configured: ConfiguredImpairment,
    ) -> Self {
        Self {
            session_id,
            wall_start,
            actor,
            configured,
            measurement: ImpairmentMeasurement::default(),
            connected_ticks: 0,
            banked_ticks: 0,
            idle_ticks: 0,
            idle_banked_ticks: 0,
            afk_capped: false,
        }
    }

    /// Account for one simulation tick and its local input count.
    pub fn observe_tick(&mut self, local_intents: usize) {
        self.connected_ticks = self.connected_ticks.saturating_add(1);
        if local_intents != 0 {
            self.idle_ticks = 0;
            self.banked_ticks = self.banked_ticks.saturating_add(1);
            return;
        }
        self.idle_ticks = self.idle_ticks.saturating_add(1);
        let cap = Self::IDLE_BANK_CAP_SECONDS * u64::from(TICK_HZ);
        if self.idle_banked_ticks < cap {
            self.idle_banked_ticks = self.idle_banked_ticks.saturating_add(1);
            self.banked_ticks = self.banked_ticks.saturating_add(1);
        } else {
            self.afk_capped = true;
        }
    }

    /// Record a packet outcome observed by the session transport.
    pub fn observe_transport(&mut self, dropped: bool, jitter_ms: u64) {
        self.measurement.observe(dropped, jitter_ms);
    }

    /// Finish a row. The caller supplies coordinator-pinned build provenance.
    #[must_use]
    pub fn finish(
        &self,
        wall_end: String,
        platform_triple: String,
        client_rev: String,
        pipeline_digest: String,
    ) -> SessionRecord {
        let observed_loss_pct = self.measurement.loss_pct();
        let observed_jitter_p50_ms = self.measurement.percentile(50);
        let observed_jitter_p99_ms = self.measurement.percentile(99);
        let mismatch = (observed_loss_pct - self.configured.loss_pct).abs() > f64::EPSILON
            || observed_jitter_p50_ms != self.configured.jitter_p50_ms
            || observed_jitter_p99_ms != self.configured.jitter_p99_ms;
        SessionRecord {
            session_id: self.session_id.clone(),
            wall_start: self.wall_start.clone(),
            wall_end,
            distinct_play_minutes: self.connected_ticks as f64 / (TICK_HZ as f64 * 60.0),
            banked_minutes: self.banked_ticks as f64 / (TICK_HZ as f64 * 60.0),
            platform_triple,
            client_rev,
            ruleset_id: hex::encode(REGOLITH_RULESET.digest),
            ruleset_version: REGOLITH_RULESET.version,
            pipeline_digest,
            actor: self.actor,
            configured_impairment_profile: self.configured,
            observed_loss_pct,
            observed_jitter_p50_ms,
            observed_jitter_p99_ms,
            afk_seconds: self.idle_ticks / u64::from(TICK_HZ),
            afk_capped: self.afk_capped,
            impairment_mismatch: mismatch,
        }
    }

    /// Append one completed row to the campaign JSONL stream.
    pub fn write_record(mut writer: impl Write, record: &SessionRecord) -> io::Result<()> {
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")
    }
}

/// Reject a campaign join until the exact notice has been acknowledged.
pub fn require_campaign_consent(acknowledged: bool) -> Result<(), &'static str> {
    acknowledged
        .then_some(())
        .ok_or("campaign join refused: consent was not acknowledged")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> CampaignSession {
        CampaignSession::new(
            "018f0f8a-0000-7000-8000-000000000001".into(),
            "2026-08-23T12:00:00Z".into(),
            Actor::Human,
            ConfiguredImpairment {
                loss_pct: 3.0,
                jitter_p50_ms: 100,
                jitter_p99_ms: 100,
            },
        )
    }

    #[test]
    fn measured_impairment_mismatch_is_flagged_in_the_completed_row() {
        let mut session = session();
        for index in 0..100 {
            session.observe_transport(index < 2, 100);
        }
        let record = session.finish(
            "2026-08-23T12:01:00Z".into(),
            "x86_64-unknown-linux-gnu".into(),
            "deadbeef".into(),
            "pipeline".into(),
        );
        assert!(
            record.impairment_mismatch,
            "2% observed loss must not echo configured 3%"
        );
        assert_eq!(record.observed_loss_pct, 2.0);
    }

    #[test]
    fn idle_only_session_banks_at_most_six_hundred_seconds_and_sets_cap() {
        let mut session = session();
        for _ in 0..((CampaignSession::IDLE_BANK_CAP_SECONDS + 20) * u64::from(TICK_HZ)) {
            session.observe_tick(0);
        }
        let record = session.finish(
            "2026-08-23T12:11:00Z".into(),
            "x86_64-unknown-linux-gnu".into(),
            "deadbeef".into(),
            "pipeline".into(),
        );
        assert_eq!(record.banked_minutes, 10.0);
        assert!(record.afk_capped);
        assert_eq!(record.afk_seconds, 620);
    }

    #[test]
    fn input_resumes_accrual_after_an_afk_cap() {
        let mut session = session();
        for _ in 0..(CampaignSession::IDLE_BANK_CAP_SECONDS * u64::from(TICK_HZ)) {
            session.observe_tick(0);
        }
        session.observe_tick(1);
        let record = session.finish(
            "end".into(),
            "linux".into(),
            "rev".into(),
            "pipeline".into(),
        );
        assert!(record.banked_minutes > 10.0);
    }

    #[test]
    fn consent_refusal_blocks_campaign_join() {
        assert!(require_campaign_consent(false).is_err());
        assert!(require_campaign_consent(true).is_ok());
    }

    #[test]
    fn completed_row_contains_every_campaign_field() {
        let record = session().finish(
            "end".into(),
            "linux".into(),
            "rev".into(),
            "pipeline".into(),
        );
        let value = serde_json::to_value(record).expect("serialize session row");
        for field in [
            "session_id",
            "wall_start",
            "wall_end",
            "distinct_play_minutes",
            "banked_minutes",
            "platform_triple",
            "client_rev",
            "ruleset_id",
            "ruleset_version",
            "pipeline_digest",
            "actor",
            "configured_impairment_profile",
            "observed_loss_pct",
            "observed_jitter_p50_ms",
            "observed_jitter_p99_ms",
            "afk_seconds",
            "afk_capped",
            "impairment_mismatch",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
    }
}
