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

/// The fraction of datagrams the host's impaired router holds back.
///
/// `Impairment::p4_profile_at_loss` in `gates/p1-swarm/src/router.rs` sets
/// `jitter_rate: 0.10` beside `jitter_ticks: 6`, and `Router::schedule` there
/// holds a packet for the *whole* six ticks or not at all — there is no draw
/// inside the interval. The host's added delay is therefore a two-point
/// distribution: zero with probability `1 - HOST_JITTER_SPIKE_RATE`, and one
/// full spike otherwise.
///
/// A campaign's single `jitter_ms` names the height of that spike. It is not a
/// median, and treating it as one is what made every honest session of
/// 2026-09-04 report `impairment_mismatch` (#1030).
pub const HOST_JITTER_SPIKE_RATE: f64 = 0.10;

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

impl ConfiguredImpairment {
    /// Derive the percentile expectations a campaign's single jitter figure
    /// actually implies, given the host's spike model.
    ///
    /// At the shipped `HOST_JITTER_SPIKE_RATE` of 0.10 this reads p50 = 0 and
    /// p99 = `spike_ms`: nine datagrams in ten are not held at all, so the
    /// median added delay is zero and the spike only appears above the ninetieth
    /// percentile. Sending `spike_ms` for *both*, as the coordinator did until
    /// #1030, asserts something no distribution can satisfy.
    #[must_use]
    pub fn from_spike(loss_pct: f64, spike_ms: u64) -> Self {
        Self {
            loss_pct,
            jitter_p50_ms: Self::spike_quantile(spike_ms, 0.50),
            jitter_p99_ms: Self::spike_quantile(spike_ms, 0.99),
        }
    }

    /// The `quantile`th quantile of the host's two-point added-delay
    /// distribution: zero below the spike's mass, the spike's full height above
    /// it.
    #[must_use]
    fn spike_quantile(spike_ms: u64, quantile: f64) -> u64 {
        if quantile > 1.0 - HOST_JITTER_SPIKE_RATE {
            spike_ms
        } else {
            0
        }
    }
}

/// Measurements taken from packets actually sent during this session.
#[derive(Debug, Default)]
pub struct ImpairmentMeasurement {
    sent: u64,
    dropped: u64,
    jitter_ms: Vec<u64>,
}

/// Packets a session must have accounted for before its measured impairment is
/// worth comparing to the configured profile.
///
/// One second of play at the 20 Hz send cadence, across a small audience. Below
/// this a single drop moves the rate by whole percentage points.
const MIN_IMPAIRMENT_SAMPLES: u64 = 200;

/// How far the measured loss may sit from the configured rate before it is a
/// disagreement rather than sampling noise, in percentage points.
///
/// The width of the criterion's own 3-5% band (`gates/p1-swarm`'s `--loss`).
const LOSS_TOLERANCE_PCT: f64 = 2.0;

/// How far a measured jitter percentile may fall *below* the configured one
/// before it is a disagreement rather than sampling noise.
///
/// One-sided, and that is the whole of #1030's fix. The figure this is compared
/// against is not a target the measurement should straddle: the client's jitter
/// is the deviation of downlink inter-arrival intervals over the *whole* path,
/// which is the host's injected spike composed with the player's own internet.
/// Delays add and never cancel, so an honest measurement can only sit at or
/// above what the host injected. 151 ms of p99 against a 100 ms spike is a
/// volunteer's link on top of the profile, not a claim that the profile was
/// wrong.
///
/// The direction that *is* evidence is the shortfall. A seat that never
/// received the profile reads its own link alone — 3% loss on a 20 Hz grid
/// carries a p99 near 50 ms and nothing above it — which is a 50 ms shortfall
/// against a 100 ms spike and still fires. Keeping the width at 40 ms is what
/// leaves that detection intact; widening it until the p50 case stopped firing
/// would have hidden the unapplied-profile case with it.
const JITTER_TOLERANCE_MS: u64 = 40;

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

/// Whether the player supplied input on one campaign tick.
///
/// This exists because counting *orders* could not answer the question. The
/// campaign path fed [`CampaignSession::observe_tick`] the length of the
/// authored order vector, and `pilot::honest_orders` pushes `Thrust`, `Lock`
/// and `Fire` on **every** tick — the skin's idle gate only zeroes
/// `accel_mmss` and the yaw, it does not drop the order. So the count was
/// never zero, `idle_ticks` was permanently zero, `banked_ticks` always
/// equalled `connected_ticks`, and [`SessionRecord::afk_capped`] was
/// unreachable: a tester who idled twenty minutes banked all twenty and the
/// row asserted `afk_seconds: 0` (#947).
///
/// A count of orders is a fact about the codec. Naming the question as its
/// own type makes the accumulator take a fact about the *player*, which is
/// what it was always documented to measure, and makes both values something
/// production actually produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerActivity {
    /// The player's controls differed from rest on this tick.
    Active,
    /// No control was held: the tick is idle for banking purposes.
    Idle,
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

    /// Account for one simulation tick and whether the player drove it.
    ///
    /// See [`PlayerActivity`] for why this is not an order count.
    pub fn observe_tick(&mut self, activity: PlayerActivity) {
        self.connected_ticks = self.connected_ticks.saturating_add(1);
        if activity == PlayerActivity::Active {
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
        // A measured rate never lands exactly on its configured one, so an
        // exact comparison flagged every session ever recorded -- including a
        // seventeen-millisecond one whose 15.6% "loss" was four dropped
        // packets. A flag that is always true is not a signal, and the first
        // playtest records would have taught their reader to ignore it.
        //
        // Flag a real disagreement instead: a sample large enough to mean
        // anything, and a gap wider than sampling noise at these rates. The
        // loss tolerance is the width of the criterion's own 3-5% band, so a
        // profile inside the band the gate accepts is not reported as a
        // mismatch against the band's floor.
        //
        // Loss is two-sided: a link cannot invent drops the host did not cause,
        // and a rate far above the configured one is as much a disagreement as
        // one far below. Jitter is one-sided, for the reason recorded at
        // `JITTER_TOLERANCE_MS` — the measurement composes the injected spike
        // with the player's own path, and only a *shortfall* says the profile
        // did not arrive.
        let mismatch = self.measurement.sent >= MIN_IMPAIRMENT_SAMPLES
            && ((observed_loss_pct - self.configured.loss_pct).abs() > LOSS_TOLERANCE_PCT
                || self
                    .configured
                    .jitter_p50_ms
                    .saturating_sub(observed_jitter_p50_ms)
                    > JITTER_TOLERANCE_MS
                || self
                    .configured
                    .jitter_p99_ms
                    .saturating_sub(observed_jitter_p99_ms)
                    > JITTER_TOLERANCE_MS);
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

    /// A session under the campaign profile, replaying one real volunteer row.
    ///
    /// `01a06b05-52e9` (macOS, 12.4 minutes, 2026-09-04): observed loss 3.17%,
    /// jitter p50 17 ms, p99 151 ms, against a campaign configured at 3% loss
    /// and a 100 ms spike.
    fn session_52e9() -> CampaignSession {
        let mut session = CampaignSession::new(
            "01a06b05-52e9-7cc8-afef-c3b87b00428c".into(),
            "2026-09-04T12:00:00Z".into(),
            Actor::Human,
            ConfiguredImpairment::from_spike(3.0, 100),
        );
        // 400 samples: the sorted p50 index is 199 and the p99 index 395, so
        // the first 200 fix the median at 17 ms and index 395 fixes the p99 at
        // 151 ms. Thirteen drops in 400 is 3.25%, inside the loss band.
        for index in 0..400 {
            let jitter_ms = if index < 200 {
                17
            } else if index < 396 {
                151
            } else {
                400
            };
            session.observe_transport(index < 13, jitter_ms);
        }
        session
    }

    fn finish(session: &CampaignSession) -> SessionRecord {
        session.finish(
            "2026-09-04T12:12:21Z".into(),
            "aarch64-apple-darwin".into(),
            "deadbeef".into(),
            "pipeline".into(),
        )
    }

    /// #1030. The coordinator sent one scalar as both percentiles and the
    /// client compared both against it, so every honest session of 2026-09-04
    /// banked with `impairment_mismatch: true` — the exact false positive P4's
    /// exit condition (#240) counts.
    ///
    /// The host holds a tenth of datagrams for a full six ticks and the rest
    /// not at all, so the median added delay is zero and the spike shows only
    /// above the ninetieth percentile. A p50 of 17 ms and a p99 of 151 ms is
    /// that model plus a real internet path, which is the only direction a
    /// path can move it.
    #[test]
    fn a_jitter_percentile_above_the_configured_spike_is_the_players_link_not_a_mismatch() {
        let configured = ConfiguredImpairment::from_spike(3.0, 100);
        assert_eq!(
            (configured.jitter_p50_ms, configured.jitter_p99_ms),
            (0, 100),
            "a 100 ms spike on a tenth of datagrams has a zero median"
        );

        let record = finish(&session_52e9());
        assert_eq!(record.observed_jitter_p50_ms, 17);
        assert_eq!(record.observed_jitter_p99_ms, 151);
        assert!(
            !record.impairment_mismatch,
            "an honest volunteer session must not raise a discrepancy flag"
        );

        // The malformed comparison this replaces: one scalar as both
        // percentiles, straddled rather than floored. A median of 17 ms cannot
        // sit within 40 ms of 100, so it fired on every row ever recorded.
        let malformed = ConfiguredImpairment {
            loss_pct: 3.0,
            jitter_p50_ms: 100,
            jitter_p99_ms: 100,
        };
        assert!(
            record
                .observed_jitter_p50_ms
                .abs_diff(malformed.jitter_p50_ms)
                > JITTER_TOLERANCE_MS,
            "the fixture must be one the old comparison flagged"
        );
    }

    /// The flag still has to catch the case it exists for: a seat whose
    /// profile never arrived. Its link alone carries no spike, so its p99 falls
    /// the spike's full height short of the configured figure.
    #[test]
    fn a_link_that_never_received_the_spike_still_reports_a_mismatch() {
        let mut session = CampaignSession::new(
            "01a06b05-0000-7000-8000-000000000001".into(),
            "2026-09-04T12:00:00Z".into(),
            Actor::Human,
            ConfiguredImpairment::from_spike(3.0, 100),
        );
        // Loss applied, jitter not: an unspiked path on a 20 Hz grid.
        for index in 0..400 {
            session.observe_transport(index < 13, 12);
        }
        let record = finish(&session);
        assert_eq!(record.observed_jitter_p99_ms, 12);
        assert!(
            record.impairment_mismatch,
            "an 88 ms shortfall against a 100 ms spike is the profile missing"
        );
    }

    #[test]
    fn measured_impairment_mismatch_is_flagged_in_the_completed_row() {
        let mut session = session();
        // Far outside the configured 3%, over a sample worth believing.
        for index in 0..400 {
            session.observe_transport(index < 100, 100);
        }
        let record = session.finish(
            "2026-08-23T12:01:00Z".into(),
            "x86_64-unknown-linux-gnu".into(),
            "deadbeef".into(),
            "pipeline".into(),
        );
        assert!(
            record.impairment_mismatch,
            "25% observed loss must not echo configured 3%"
        );
        assert_eq!(record.observed_loss_pct, 25.0);
    }

    /// The row records what was measured whether or not it is a disagreement:
    /// the flag is a judgement about the gap, never a substitute for the
    /// numbers, and a reader must be able to see 2% against a configured 3%.
    #[test]
    fn a_measurement_close_to_the_configured_profile_is_recorded_but_not_flagged() {
        let mut session = session();
        for index in 0..400 {
            session.observe_transport(index < 8, 100);
        }
        let record = session.finish(
            "2026-08-23T12:01:00Z".into(),
            "x86_64-unknown-linux-gnu".into(),
            "deadbeef".into(),
            "pipeline".into(),
        );
        assert_eq!(
            record.observed_loss_pct, 2.0,
            "the measurement is still recorded"
        );
        assert!(
            !record.impairment_mismatch,
            "2% against a configured 3% is sampling noise, not a disagreement"
        );
    }

    /// #711 found this in the first real uploads: every record ever written
    /// carried `impairment_mismatch: true`, including a seventeen-millisecond
    /// session whose 15.6% "loss" was four dropped packets. A flag that is
    /// always true teaches its reader to ignore it, which is worse than not
    /// having it at all.
    #[test]
    fn a_sample_too_small_to_mean_anything_is_not_a_mismatch() {
        let mut session = session();
        for index in 0..26 {
            session.observe_transport(index < 4, 100);
        }
        let record = session.finish(
            "2026-08-23T12:01:00Z".into(),
            "x86_64-unknown-linux-gnu".into(),
            "deadbeef".into(),
            "pipeline".into(),
        );
        assert!(
            record.observed_loss_pct > 15.0,
            "the measured rate is wild on a tiny sample: {}",
            record.observed_loss_pct
        );
        assert!(
            !record.impairment_mismatch,
            "a handful of packets cannot disagree with a configured profile"
        );
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
        // Whether this sample is big enough to *judge* against the configured
        // profile is a different question, and the tests named for it own it.
        // This one is about both entry points reaching the accumulator.
    }

    /// A zero-loss link measures zero — the accumulator cannot be fed by
    /// configuration, so an unimpaired hour reads as one and is flagged as
    /// outside the criterion band rather than silently banking.
    #[test]
    fn a_clean_link_measures_clean() {
        let mut session = session();
        // A session's worth of acks, not a handful: the flag now asks whether
        // the sample is large enough to disagree with the profile at all, and
        // an unimpaired *hour* is the case this protects against.
        for _ in 0..400 {
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
            active.observe_tick(PlayerActivity::Active);
        }
        let progress = active.progress();
        assert_eq!(progress.banked_minutes, 1.0);
        assert_eq!(progress.idle_minutes, 0.0);
        assert!(!progress.afk_capped);
        assert!(progress.joined_session_ran);

        let mut idle_run = session();
        for _ in 0..idle * 11 {
            idle_run.observe_tick(PlayerActivity::Idle);
        }
        let capped = idle_run.progress();
        assert!(capped.afk_capped, "600 s cap then overflow trips the flag");
        assert_eq!(capped.banked_minutes, 10.0);
    }

    #[test]
    fn idle_only_session_banks_at_most_six_hundred_seconds_and_sets_cap() {
        let mut session = session();
        for _ in 0..((CampaignSession::IDLE_BANK_CAP_SECONDS + 20) * u64::from(TICK_HZ)) {
            session.observe_tick(PlayerActivity::Idle);
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
            session.observe_tick(PlayerActivity::Idle);
        }
        session.observe_tick(PlayerActivity::Active);
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
