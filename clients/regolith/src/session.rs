//! Campaign consent, measured-link records, and AFK accounting.
//!
//! This module deliberately has no Bevy dependency beyond the client's normal
//! build: bots and the rendered client feed the same [`CampaignSession`].

use std::io::{self, Write};

use orrery_core::TICK_HZ;
use orrery_games::regolith::REGOLITH_RULESET;
use serde::Serialize;

/// Domain separating a client-owned campaign measurement from every other
/// signature made with the same transport identity.
pub const CAMPAIGN_MEASUREMENT_V1_DOMAIN: &[u8] = b"orrery/campaign-measurement/v1\0";

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

    /// Record one downlink arrival that closed a gap of `missing` broadcasts.
    ///
    /// The accounting this encodes: since the previous arrival, `missing + 1`
    /// broadcasts were due from this sender and exactly one landed, so the
    /// denominator grows by `missing + 1` and the dropped count by `missing`.
    /// Jitter is sampled once per *arrival* — the deviation of its inter-
    /// arrival interval — not once per expected packet; a lost packet produces
    /// no arrival and therefore no interval to deviate. The very first
    /// arrival from a sender has no baseline and carries `None`, contributing
    /// loss accounting only.
    pub fn observe_arrival(&mut self, missing: u64, deviation_ms: Option<u64>) {
        let arrivals = missing.saturating_add(1);
        self.sent = self.sent.saturating_add(arrivals);
        self.dropped = self.dropped.saturating_add(missing);
        if let Some(deviation_ms) = deviation_ms {
            self.jitter_ms.push(deviation_ms);
        }
    }

    /// Record one application-level uplink ack (#393's contract).
    ///
    /// Acks carry no timing information — they are queued when the router
    /// decides, at whatever cadence the downlink is carrying them — so they
    /// move loss only. Mixing a synthetic zero into the jitter samples here
    /// would drag the percentiles towards zero precisely when loss is high.
    pub fn observe_ack(&mut self, dropped: bool) {
        self.sent = self.sent.saturating_add(1);
        self.dropped = self.dropped.saturating_add(u64::from(dropped));
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
    /// Transport NodeId admitted by the host for this session.
    pub measurement_node: String,
    /// Exact lowercase-hex JSON bytes signed by [`Self::measurement_signature`].
    /// The payload contains every client-owned row field except
    /// `pipeline_digest`, which the assembler supplies from the host report.
    pub measurement_payload: String,
    /// Ed25519 signature over [`CAMPAIGN_MEASUREMENT_V1_DOMAIN`] followed by
    /// the decoded [`Self::measurement_payload`] bytes.
    pub measurement_signature: String,
}

impl SessionRecord {
    /// Bind every client-owned field in this row to the admitted transport key.
    pub fn sign(&mut self, key: &iroh_base::SecretKey) -> Result<(), serde_json::Error> {
        self.measurement_node = key.public().to_string();
        let mut value = serde_json::to_value(&*self)?;
        let object = value
            .as_object_mut()
            .expect("SessionRecord serializes as an object");
        object.remove("pipeline_digest");
        object.remove("measurement_payload");
        object.remove("measurement_signature");
        let payload = serde_json::to_vec(&value)?;
        let mut signed = Vec::with_capacity(CAMPAIGN_MEASUREMENT_V1_DOMAIN.len() + payload.len());
        signed.extend_from_slice(CAMPAIGN_MEASUREMENT_V1_DOMAIN);
        signed.extend_from_slice(&payload);
        self.measurement_payload = hex::encode(&payload);
        self.measurement_signature = hex::encode(key.sign(&signed).to_bytes());
        Ok(())
    }
}

/// Where the accumulator stands mid-session, for the overlay and the strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveProgress {
    /// Connected minutes before any AFK cap is applied.
    pub connected_minutes: f64,
    /// Minutes currently eligible to bank.
    pub banked_minutes: f64,
    /// Minutes with no local input.
    pub idle_minutes: f64,
    /// Whether the 600-second idle banking allowance is exhausted.
    pub afk_capped: bool,
    /// Always true on a live [`CampaignSession`]: this struct only exists for
    /// sessions whose joined state machine ran. `LocalSession` never produces
    /// one — an offline hour is not a campaign path and banks nothing.
    pub joined_session_ran: bool,
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

    /// Account for one downlink arrival that closed a gap of `missing`
    /// broadcasts (see [`ImpairmentMeasurement::observe_arrival`] for the
    /// accounting). A first-arrival has no baseline and carries `None`.
    pub fn observe_arrival(&mut self, missing: u64, jitter_ms: Option<u64>) {
        self.measurement.observe_arrival(missing, jitter_ms);
    }

    /// Account for one application-level uplink ack (#393's contract).
    pub fn observe_uplink_ack(&mut self, dropped: bool) {
        self.measurement.observe_ack(dropped);
    }

    /// Loss as measured so far, from this client's own packet outcomes.
    ///
    /// Reading a live value is deliberate: the F3 pane and the JSONL stream
    /// must show what the link has done *so far*, not only at `finish`.
    #[must_use]
    pub fn observed_loss_pct(&self) -> f64 {
        self.measurement.loss_pct()
    }

    /// Measured jitter median so far.
    #[must_use]
    pub fn observed_jitter_p50_ms(&self) -> u64 {
        self.measurement.percentile(50)
    }

    /// Measured jitter p99 so far.
    #[must_use]
    pub fn observed_jitter_p99_ms(&self) -> u64 {
        self.measurement.percentile(99)
    }

    /// Where the accumulator stands right now, for the overlay.
    ///
    /// A campaign hour is banked by *this* machine having run; the overlay
    /// shows that accumulation live rather than only in the finished row.
    #[must_use]
    pub fn progress(&self) -> LiveProgress {
        LiveProgress {
            connected_minutes: self.connected_ticks as f64 / (TICK_HZ as f64 * 60.0),
            banked_minutes: self.banked_ticks as f64 / (TICK_HZ as f64 * 60.0),
            idle_minutes: self.idle_ticks as f64 / u64::from(TICK_HZ) as f64 / 60.0,
            afk_capped: self.afk_capped,
            joined_session_ran: true,
        }
    }

    /// The coordinator-issued session identity.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
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
            measurement_node: String::new(),
            measurement_payload: String::new(),
            measurement_signature: String::new(),
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

    /// The two live entry points must land in the same accumulator the row is
    /// built from: uplink loss from Dropped acks (#393), downlink loss from
    /// gap-closing arrivals, jitter only from arrival intervals.
    #[test]
    fn live_ack_and_arrival_outcomes_reach_the_banking_row() {
        let mut session = session();
        // Uplink: 10 datagrams sequenced, the router settles 3 as Dropped.
        for outcome in [
            false, true, false, true, false, false, true, false, false, false,
        ] {
            session.observe_uplink_ack(outcome);
        }
        // Downlink: three arrivals closing gaps of 1, 0 and 4 broadcasts.
        session.observe_arrival(1, Some(40));
        session.observe_arrival(0, Some(5));
        session.observe_arrival(4, Some(90));
        // 10 acked + (2 + 1 + 5) expected arrivals = 18 outcomes;
        // dropped = 3 Dropped acks + 5 gap-closed broadcasts = 8.
        let expected_loss = 8.0 * 100.0 / 18.0;
        assert_eq!(session.observed_loss_pct(), expected_loss);
        assert_eq!(
            (
                session.observed_jitter_p50_ms(),
                session.observed_jitter_p99_ms()
            ),
            (40, 90),
            "jitter percentiles come from arrival intervals alone"
        );
        let record = session.finish(
            "end".into(),
            "linux".into(),
            "rev".into(),
            "pipeline".into(),
        );
        assert_eq!(record.observed_loss_pct, expected_loss);
        assert!(
            record.impairment_mismatch,
            "{expected_loss}% is not the configured 3%"
        );
    }

    /// A zero-loss link measures zero — the accumulator cannot be fed by
    /// configuration, so an unimpaired hour reads as one and is flagged as
    /// outside the criterion band rather than silently banking.
    #[test]
    fn a_clean_link_measures_clean() {
        let mut session = session();
        for _ in 0..20 {
            session.observe_uplink_ack(false);
        }
        session.observe_arrival(0, Some(0));
        assert_eq!(session.observed_loss_pct(), 0.0);
        let record = session.finish(
            "end".into(),
            "linux".into(),
            "rev".into(),
            "pipeline".into(),
        );
        assert_eq!(record.observed_loss_pct, 0.0);
        assert!(record.impairment_mismatch);
    }

    /// The overlay reads live progress off the same accumulator that banks.
    #[test]
    fn progress_reports_the_accumulator_live() {
        let mut active = session();
        let idle = u64::from(orrery_core::TICK_HZ) * 60;
        for _ in 0..idle {
            active.observe_tick(1);
        }
        let progress = active.progress();
        assert_eq!(progress.banked_minutes, 1.0);
        assert_eq!(progress.idle_minutes, 0.0);
        assert!(!progress.afk_capped);
        assert!(progress.joined_session_ran);

        let mut idle_run = session();
        for _ in 0..idle * 11 {
            idle_run.observe_tick(0);
        }
        let capped = idle_run.progress();
        assert!(capped.afk_capped, "600 s cap then overflow trips the flag");
        assert_eq!(capped.banked_minutes, 10.0);
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
            "measurement_node",
            "measurement_payload",
            "measurement_signature",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn signature_binds_the_observed_fields_and_excludes_only_the_host_digest() {
        let key = iroh_base::SecretKey::from_bytes(&[0x49; 32]);
        let mut record = session().finish(
            "end".into(),
            "linux".into(),
            "rev".into(),
            "unavailable-client-side".into(),
        );
        record.sign(&key).expect("sign row");
        let payload = hex::decode(&record.measurement_payload).expect("payload hex");
        let payload: serde_json::Value = serde_json::from_slice(&payload).expect("payload JSON");
        assert_eq!(payload["observed_loss_pct"], record.observed_loss_pct);
        assert_eq!(payload["measurement_node"], key.public().to_string());
        assert!(payload.get("pipeline_digest").is_none());
        assert!(payload.get("measurement_signature").is_none());
        let signature: [u8; 64] = hex::decode(&record.measurement_signature)
            .expect("signature hex")
            .try_into()
            .expect("64-byte signature");
        let mut signed = CAMPAIGN_MEASUREMENT_V1_DOMAIN.to_vec();
        signed.extend_from_slice(
            &hex::decode(&record.measurement_payload).expect("measurement payload hex"),
        );
        key.public()
            .verify(&signed, &iroh_base::Signature::from_bytes(&signature))
            .expect("client signature verifies");
    }
}
