//! The waiting room: what the host has said about this attempt's seats.
//!
//! Before this module a friend's first minute was a black screen that
//! eventually became a game. There was no way to tell "the host has not
//! started yet" from "this is broken". The lobby is that minute, drawn.
//!
//! ## It shows only what the host said
//!
//! A12 §5.6 and ADR-0050 bind the skin: it may interpolate, but it may not
//! assert anything the ruleset or the host has not. Every line drawn here has
//! a named source.
//!
//! * The **seat map** is admission's roster answer, which #573 turned into a
//!   complete seat map — every configured seat, its state, and who holds it.
//!   An empty seat is drawn empty because a row said `"state": "empty"`, not
//!   because the client noticed a gap in the slots it received.
//! * The **phase** is admission's `phase` field. When the service does not
//!   send one — every service older than #573 — no phase line is drawn at
//!   all, and no panel either. The client does not guess that "no phase"
//!   means "lobby".
//! * The **countdown** is drawn only when the service sends `starts_in_s`.
//!   There is deliberately no client-side clock extrapolating from a start
//!   stamp: a countdown that keeps ticking after the host stopped talking is
//!   the skin asserting a fact it does not have. Nothing that reaches this
//!   module today sends `starts_in_s`, so in practice no countdown is drawn,
//!   and that is the correct output rather than a missing feature.
//! * The **occupancy count** is arithmetic over rows the host sent, which is
//!   reading rather than inventing.
//!
//! ## A seat with nobody in it has no name
//!
//! [`crate::roster`] settled this for craft labels in #484: a missing label is
//! absence, never `"UNKNOWN"` or `"PLAYER 3"`, because a placeholder can be
//! mistaken for a name somebody chose. An empty seat is the same case one
//! level up, so an empty seat row draws its state word and no name column.
//!
//! ## ASCII only
//!
//! No font asset is loaded, so Bevy's built-in ASCII face draws these lines
//! and anything outside it renders as an empty box (#526). Every constant here
//! is ASCII, and every string that arrives from the network passes through
//! [`plain_ascii`] before it can reach Bevy text.

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};

/// The panel heading.
pub const HEADING: &str = "WAITING ROOM";

/// What a line of service-supplied prose is bounded at, in characters.
///
/// Long enough for admission's refusal sentences, short enough that one row
/// cannot push the panel past the 720-line window #552 already calls tight.
pub const DETAIL_MAX_CHARS: usize = 160;

/// A network string reduced to something the built-in face can draw.
///
/// Same rule as [`crate::roster::sanitise_nickname`] and for the same reason,
/// with a longer bound because this is a sentence rather than a name. `None`
/// means *there is nothing to draw*, and the caller must draw nothing rather
/// than substitute a placeholder.
#[must_use]
pub fn plain_ascii(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(char::is_ascii)
        .filter(|glyph| !glyph.is_ascii_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(DETAIL_MAX_CHARS).collect())
}

/// Who a configured seat is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatKind {
    /// A headless peer the host runs.
    Bot,
    /// A seat a person can reserve.
    Human,
    /// A kind this build does not know, or none at all. Kept as its own text
    /// rather than folded into `Bot` or `Human`.
    Other(String),
}

impl SeatKind {
    /// Parse admission's `kind`. `None` is a service older than #573.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("bot") => Self::Bot,
            Some("human") => Self::Human,
            None => Self::Other(String::new()),
            Some(other) => Self::Other(plain_ascii(other).unwrap_or_default()),
        }
    }
}

/// What the host says is happening in a seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatState {
    /// Occupied and playing.
    Active,
    /// Allocated to a person who has not dialled yet.
    Reserved,
    /// Dialled and waiting in the lobby.
    Connected,
    /// Nobody has taken it.
    Empty,
    /// Someone took it and left; it is not reused this attempt.
    Vacant,
    /// A state this build does not know, or none at all.
    Other(String),
}

impl SeatState {
    /// Parse admission's `state`. `None` is a service older than #573.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("active") => Self::Active,
            Some("reserved") => Self::Reserved,
            Some("connected") => Self::Connected,
            Some("empty") => Self::Empty,
            Some("vacant") => Self::Vacant,
            None => Self::Other(String::new()),
            Some(other) => Self::Other(plain_ascii(other).unwrap_or_default()),
        }
    }

    /// Whether a person is counted as holding this seat.
    ///
    /// `vacant` deliberately is not held: the seat is spent for this attempt,
    /// but nobody is in it, and saying "3 of 4 taken" about a seat whose
    /// player left would misdescribe the room.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        matches!(self, Self::Active | Self::Reserved | Self::Connected)
    }
}

/// One configured seat, as the host described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seat {
    /// The swarm slot.
    pub slot: usize,
    /// Bot, human, or a kind this build does not know.
    pub kind: SeatKind,
    /// What the host says is in it.
    pub state: SeatState,
    /// The label, when the host sent one. `None` draws no name at all.
    pub nickname: Option<String>,
}

/// Where the campaign is in its attempt cycle, per admission's `phase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyPhase {
    /// Seats are reservable and no cohort is forming yet.
    Open,
    /// A cohort is forming; membership freezes at Start.
    Lobby,
    /// The attempt has started; membership is frozen.
    Running,
    /// Every seat is taken.
    Full,
    /// Between attempts.
    Restarting,
    /// The operator closed the campaign.
    Closed,
    /// The operator paused admissions.
    Paused,
    /// A phase this build does not know, kept as its own text.
    Other(String),
}

impl LobbyPhase {
    /// Parse admission's `phase`.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "open" => Self::Open,
            "lobby" => Self::Lobby,
            "running" => Self::Running,
            "full" => Self::Full,
            "restarting" => Self::Restarting,
            "closed" => Self::Closed,
            "paused" => Self::Paused,
            other => Self::Other(plain_ascii(other).unwrap_or_default()),
        }
    }

    /// The sentence drawn under the heading.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::Open => "seats are open".to_owned(),
            Self::Lobby => "lobby is open - waiting for players".to_owned(),
            Self::Running => "this attempt has started".to_owned(),
            Self::Full => "every player seat is taken".to_owned(),
            Self::Restarting => "between attempts - the next lobby opens shortly".to_owned(),
            Self::Closed => "this campaign is closed".to_owned(),
            Self::Paused => "the operator has paused admissions".to_owned(),
            Self::Other(text) if text.is_empty() => "the host did not name a phase".to_owned(),
            Self::Other(text) => format!("phase: {text}"),
        }
    }

    /// Whether a player is still waiting for the attempt to begin.
    #[must_use]
    pub const fn is_waiting(&self) -> bool {
        matches!(
            self,
            Self::Open | Self::Lobby | Self::Full | Self::Restarting
        )
    }
}

/// The waiting room this client currently believes in.
///
/// Presentation only, exactly like [`crate::roster::ShipRoster`]. Empty is the
/// ordinary state before the first roster answer lands and after one fails,
/// and both must draw the same as a service that says nothing: no panel.
#[derive(Debug, Default, Clone, Resource)]
pub struct LobbyView {
    /// Every configured seat the host described, in the order it sent them.
    pub seats: Vec<Seat>,
    /// The phase the host named, or `None` when it named none.
    pub phase: Option<LobbyPhase>,
    /// Seconds to Start, only when the host sent them.
    pub starts_in_s: Option<u64>,
    /// This client's own seat, when it has one.
    pub own_slot: Option<usize>,
    /// A refusal or manifest disagreement worth showing in the room.
    pub notice: Option<String>,
}

impl LobbyView {
    /// How many human seats the host described.
    #[must_use]
    pub fn human_seats(&self) -> usize {
        self.seats
            .iter()
            .filter(|seat| seat.kind == SeatKind::Human)
            .count()
    }

    /// How many of them somebody holds.
    #[must_use]
    pub fn human_seats_held(&self) -> usize {
        self.seats
            .iter()
            .filter(|seat| seat.kind == SeatKind::Human && seat.state.is_held())
            .count()
    }

    /// Whether there is anything host-said to draw.
    ///
    /// A roster answer from a service older than #573 carries labelled craft
    /// and no phase or seat states. That is not a waiting room and must not be
    /// drawn as one — the alternative, showing a panel built from whatever
    /// rows happened to arrive, would put a seat map on screen that no host
    /// ever asserted.
    #[must_use]
    pub fn is_describable(&self) -> bool {
        self.phase.is_some() && self.seats.iter().any(|seat| seat.kind == SeatKind::Human)
    }

    /// One seat's row, `seat 4  you      ada`.
    ///
    /// The name column is *absent* for a seat with no label rather than
    /// filled with a placeholder.
    #[must_use]
    pub fn seat_line(&self, seat: &Seat) -> String {
        let word = match (&seat.kind, &seat.state) {
            (SeatKind::Bot, _) => "crowd",
            (SeatKind::Human, _) if Some(seat.slot) == self.own_slot => "you",
            (SeatKind::Human, SeatState::Connected) => "here",
            (SeatKind::Human, SeatState::Active) => "flying",
            (SeatKind::Human, SeatState::Reserved) => "joining",
            (SeatKind::Human, SeatState::Empty) => "empty",
            (SeatKind::Human, SeatState::Vacant) => "left",
            _ => "seat",
        };
        let row = format!("seat {:<2} {word:<8}", seat.slot);
        match seat.nickname.as_deref().and_then(plain_ascii) {
            Some(name) => format!("{row} {name}"),
            None => row.trim_end().to_owned(),
        }
    }

    /// The whole panel, one string per line, ASCII only.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![HEADING.to_owned()];
        if let Some(phase) = &self.phase {
            lines.push(phase.sentence());
        }
        let human = self.human_seats();
        if human > 0 {
            lines.push(format!(
                "{} of {human} player seats taken",
                self.human_seats_held()
            ));
        }
        // Only what the host sent. No local clock extrapolates this.
        if let Some(seconds) = self.starts_in_s {
            lines.push(format!("starts in {seconds} s"));
        }
        for seat in &self.seats {
            lines.push(self.seat_line(seat));
        }
        if let Some(notice) = self.notice.as_deref().and_then(plain_ascii) {
            lines.push(notice);
        }
        lines
    }

    /// The panel as one Bevy text block.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }
}

/// A wait, in words a person reads rather than seconds they divide.
#[must_use]
pub fn about_duration(seconds: u64) -> String {
    match seconds {
        0 => "a moment".to_owned(),
        1 => "1 second".to_owned(),
        2..=90 => format!("{seconds} seconds"),
        _ => {
            let minutes = (seconds + 30) / 60;
            if minutes == 1 {
                "about 1 minute".to_owned()
            } else {
                format!("about {minutes} minutes")
            }
        }
    }
}

/// A join refusal, said in plain language.
///
/// The *detail* is the service's own sentence and is used as written: #573
/// authors these ("All 3 player seats are full; try the next lobby.") and
/// rewriting them client-side would mean two places describe one refusal and
/// can disagree. What this adds is the wait, which the service sends as
/// `retry_after_s` and no prose of its own explains, plus a readable fallback
/// for a code that arrives with no drawable detail at all.
#[must_use]
pub fn refusal_sentence(
    error: Option<&str>,
    detail: Option<&str>,
    retry_after_s: Option<u64>,
) -> String {
    let said = detail.and_then(plain_ascii);
    let mut sentence = match (said, error) {
        (Some(text), _) => text,
        (None, Some("campaign_full")) => "Every player seat is taken.".to_owned(),
        (None, Some("session_started")) => "This attempt has already started.".to_owned(),
        (None, Some("host_failed")) => "The campaign host is not ready.".to_owned(),
        (None, Some(code)) => match plain_ascii(code) {
            Some(code) => format!("The campaign service refused the join: {code}."),
            None => "The campaign service refused the join.".to_owned(),
        },
        (None, None) => "The campaign service refused the join.".to_owned(),
    };
    if let Some(seconds) = retry_after_s {
        if !sentence.ends_with('.') {
            sentence.push('.');
        }
        sentence.push_str(&format!(" Next lobby in {}.", about_duration(seconds)));
    }
    sentence
}

/// One seat named active by [`StartManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActiveSeat {
    /// The swarm slot.
    pub slot: usize,
    /// Hex transport identity holding it.
    pub node: String,
    /// The persistent entity id it flies.
    pub entity: u64,
}

/// `StartV1`, the frozen active membership the host sends at lobby close.
///
/// Field-for-field the manifest specified in
/// `docs/plans/multi-human-campaign.md` §3.2. #574 owns the host half and is
/// not landed at the time of writing, so what is fixed here is that
/// specification's *shape*, decoded from JSON carried on the handshake stream
/// — the same encoding the tick-zero claim beside it already uses, chosen so
/// this does not become a second spelling of a wire #574 has yet to write.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StartManifest {
    /// The attempt generation this membership belongs to.
    pub attempt_id: String,
    /// The host's swarm seed.
    ///
    /// Carried, not compared: the client's universe seed is its own constant
    /// and is not the same number, so an equality here would be theatre.
    pub seed: u64,
    /// The tick membership froze at. Always zero in this cut.
    pub tick: u64,
    /// The configured island size every spawn pose is computed against.
    pub island_seats: u16,
    /// Every seat with somebody in it.
    pub active: Vec<ActiveSeat>,
    /// This subject's frozen witness ring.
    pub witness_recipients: Vec<usize>,
    /// How long the attempt runs.
    pub duration_ticks: u64,
}

impl StartManifest {
    /// Decode a manifest from the handshake's JSON payload.
    ///
    /// # Errors
    /// Any payload that is not a `StartV1` object.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| format!("StartV1 did not decode: {error}"))
    }

    /// Encode a manifest, for tests and local host fixtures.
    ///
    /// # Panics
    /// Never: the type has no key that can fail to serialize.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("StartV1 serializes")
    }
}

/// What this client committed to before the manifest arrived.
///
/// Every field is something already spent: the entity was inserted into the
/// executor, the pose was computed from `island_seats`, and the tick-zero
/// claim was signed over both. That is why disagreement is fatal rather than
/// adjustable — the anchor cannot be re-signed once it has been authored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartExpectation {
    /// The seat admission reserved for this client.
    pub slot: usize,
    /// The entity the executor spawned for that seat.
    pub entity: u64,
    /// This client's hex transport identity.
    pub node_hex: String,
    /// The island size the spawn pose was computed against.
    pub island_seats: u16,
}

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestMismatch {
    /// Membership froze at a tick this client cannot enter at.
    NotTickZero {
        /// The tick the manifest named.
        tick: u64,
    },
    /// The island is a different size than the spawn pose assumed.
    SeatCount {
        /// What the manifest named.
        manifest: u16,
        /// What this client spawned against.
        expected: u16,
    },
    /// This client's seat is not in the active set.
    SeatMissing {
        /// The seat admission reserved.
        slot: usize,
    },
    /// This client's seat is held by another transport identity.
    NodeDisagrees {
        /// The seat in question.
        slot: usize,
        /// The identity the manifest put in it.
        manifest_node: String,
    },
    /// This client's seat flies a different entity than it spawned.
    EntityDisagrees {
        /// The seat in question.
        slot: usize,
        /// The entity the manifest named.
        manifest: u64,
        /// The entity this client anchored.
        expected: u64,
    },
    /// One slot appears twice in the active set.
    DuplicateSeat {
        /// The repeated slot.
        slot: usize,
    },
    /// An active seat lies outside the island the manifest declared.
    SeatOutsideIsland {
        /// The offending slot.
        slot: usize,
        /// The declared island size.
        island_seats: u16,
    },
    /// A witness recipient is not an active member.
    WitnessRecipientNotActive {
        /// The recipient named.
        slot: usize,
    },
    /// The witness ring names this client itself.
    WitnessRecipientIsSelf {
        /// This client's seat.
        slot: usize,
    },
}

impl ManifestMismatch {
    /// The sentence logged, and shown after the refusal preamble.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotTickZero { tick } => format!(
                "the host froze membership at tick {tick}; this client can only start at tick 0"
            ),
            Self::SeatCount { manifest, expected } => format!(
                "the host says the island has {manifest} seats; this client spawned for {expected}"
            ),
            Self::SeatMissing { slot } => {
                format!("the host started without seat {slot}, which is this client's seat")
            }
            Self::NodeDisagrees {
                slot,
                manifest_node,
            } => {
                format!("the host says seat {slot} is held by {manifest_node}, not by this client")
            }
            Self::EntityDisagrees {
                slot,
                manifest,
                expected,
            } => format!(
                "the host flies entity {manifest} in seat {slot}; this client anchored entity {expected}"
            ),
            Self::DuplicateSeat { slot } => {
                format!("the host named seat {slot} twice in one manifest")
            }
            Self::SeatOutsideIsland { slot, island_seats } => {
                format!("the host named active seat {slot} on an island of {island_seats} seats")
            }
            Self::WitnessRecipientNotActive { slot } => {
                format!("the host named witness recipient {slot}, which is not an active seat")
            }
            Self::WitnessRecipientIsSelf { slot } => {
                format!("the host named this client's own seat {slot} as its witness recipient")
            }
        }
    }

    /// What the player is told, as opposed to what the log records.
    #[must_use]
    pub fn player_sentence(&self) -> String {
        format!(
            "Refusing to play: this client and the host disagree about the attempt - {}.",
            self.message()
        )
    }
}

/// The manifest, once it has been checked against what was already spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedStart {
    /// The attempt generation.
    pub attempt_id: String,
    /// The island size, confirmed against the spawn pose.
    pub island_seats: u16,
    /// Every active seat, in the order the manifest named them.
    pub active_slots: Vec<usize>,
    /// The frozen witness ring for this subject.
    pub witness_recipients: Vec<usize>,
    /// How long the attempt runs.
    pub duration_ticks: u64,
}

/// The guarded stage: adopt the host's manifest, or refuse the attempt.
///
/// This is the one place a mismatched manifest can become a running game, so
/// it is the one place that must fail closed. A client that plays on against a
/// manifest it does not match signs witness frames for an entity the host does
/// not believe it flies, from a pose the host did not compute, into a ring
/// that may not be listening — evidence nobody can later reconcile, which is
/// worse than no evidence at all.
///
/// # Errors
/// Every disagreement in [`ManifestMismatch`]. There is no partial acceptance
/// and no repair: the tick-zero claim is already signed.
pub fn accept_start(
    manifest: &StartManifest,
    expect: &StartExpectation,
) -> Result<AcceptedStart, ManifestMismatch> {
    if manifest.tick != 0 {
        return Err(ManifestMismatch::NotTickZero {
            tick: manifest.tick,
        });
    }
    if manifest.island_seats != expect.island_seats {
        return Err(ManifestMismatch::SeatCount {
            manifest: manifest.island_seats,
            expected: expect.island_seats,
        });
    }
    let mut seen: Vec<usize> = Vec::with_capacity(manifest.active.len());
    for seat in &manifest.active {
        if seen.contains(&seat.slot) {
            return Err(ManifestMismatch::DuplicateSeat { slot: seat.slot });
        }
        if seat.slot >= usize::from(manifest.island_seats) {
            return Err(ManifestMismatch::SeatOutsideIsland {
                slot: seat.slot,
                island_seats: manifest.island_seats,
            });
        }
        seen.push(seat.slot);
    }
    let Some(own) = manifest.active.iter().find(|seat| seat.slot == expect.slot) else {
        return Err(ManifestMismatch::SeatMissing { slot: expect.slot });
    };
    if !own.node.eq_ignore_ascii_case(&expect.node_hex) {
        return Err(ManifestMismatch::NodeDisagrees {
            slot: expect.slot,
            manifest_node: plain_ascii(&own.node).unwrap_or_default(),
        });
    }
    if own.entity != expect.entity {
        return Err(ManifestMismatch::EntityDisagrees {
            slot: expect.slot,
            manifest: own.entity,
            expected: expect.entity,
        });
    }
    for recipient in &manifest.witness_recipients {
        if *recipient == expect.slot {
            return Err(ManifestMismatch::WitnessRecipientIsSelf { slot: *recipient });
        }
        if !seen.contains(recipient) {
            return Err(ManifestMismatch::WitnessRecipientNotActive { slot: *recipient });
        }
    }
    Ok(AcceptedStart {
        attempt_id: plain_ascii(&manifest.attempt_id).unwrap_or_default(),
        island_seats: manifest.island_seats,
        active_slots: seen,
        witness_recipients: manifest.witness_recipients.clone(),
        duration_ticks: manifest.duration_ticks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN_NODE: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";
    const OTHER_NODE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn expectation() -> StartExpectation {
        StartExpectation {
            slot: 4,
            entity: 5,
            node_hex: OWN_NODE.to_owned(),
            island_seats: 8,
        }
    }

    fn manifest() -> StartManifest {
        StartManifest {
            attempt_id: "0192f0a0-0000-7000-8000-000000000001".to_owned(),
            seed: 7,
            tick: 0,
            island_seats: 8,
            active: vec![
                ActiveSeat {
                    slot: 0,
                    node: OTHER_NODE.to_owned(),
                    entity: 1,
                },
                ActiveSeat {
                    slot: 4,
                    node: OWN_NODE.to_owned(),
                    entity: 5,
                },
                ActiveSeat {
                    slot: 5,
                    node: OTHER_NODE.to_owned(),
                    entity: 6,
                },
            ],
            witness_recipients: vec![0, 5],
            duration_ticks: 216_000,
        }
    }

    /// A manifest that agrees is adopted whole, including the witness ring the
    /// client must send its frames to rather than deriving one.
    #[test]
    fn a_matching_manifest_is_adopted() {
        let accepted = accept_start(&manifest(), &expectation()).expect("manifest agrees");
        assert_eq!(accepted.island_seats, 8);
        assert_eq!(accepted.active_slots, vec![0, 4, 5]);
        assert_eq!(accepted.witness_recipients, vec![0, 5]);
        assert_eq!(accepted.duration_ticks, 216_000);
    }

    /// **The fail-closed property.** Every way a manifest can disagree with
    /// what this client already spent on its tick-zero anchor must refuse the
    /// attempt. Proceeding would sign witness frames the host cannot
    /// reconcile, which is worse than not playing.
    #[test]
    fn a_mismatched_manifest_refuses_the_attempt() {
        let expect = expectation();

        let mut late = manifest();
        late.tick = 900;
        assert_eq!(
            accept_start(&late, &expect),
            Err(ManifestMismatch::NotTickZero { tick: 900 }),
            "a manifest that froze after tick zero cannot be entered"
        );

        let mut resized = manifest();
        resized.island_seats = 6;
        assert_eq!(
            accept_start(&resized, &expect),
            Err(ManifestMismatch::SeatCount {
                manifest: 6,
                expected: 8
            }),
            "a different island size means a different spawn pose than the one anchored"
        );

        let mut without_us = manifest();
        without_us.active.retain(|seat| seat.slot != 4);
        without_us.witness_recipients = vec![0, 5];
        assert_eq!(
            accept_start(&without_us, &expect),
            Err(ManifestMismatch::SeatMissing { slot: 4 }),
            "the host started without this client's seat"
        );

        let mut stolen = manifest();
        stolen.active[1].node = OTHER_NODE.to_owned();
        assert_eq!(
            accept_start(&stolen, &expect),
            Err(ManifestMismatch::NodeDisagrees {
                slot: 4,
                manifest_node: OTHER_NODE.to_owned()
            }),
            "another transport identity holds the seat this client reserved"
        );

        let mut wrong_entity = manifest();
        wrong_entity.active[1].entity = 9;
        assert_eq!(
            accept_start(&wrong_entity, &expect),
            Err(ManifestMismatch::EntityDisagrees {
                slot: 4,
                manifest: 9,
                expected: 5
            }),
            "the anchored entity is already signed and cannot be moved"
        );

        let mut duplicated = manifest();
        duplicated.active.push(ActiveSeat {
            slot: 0,
            node: OTHER_NODE.to_owned(),
            entity: 1,
        });
        assert_eq!(
            accept_start(&duplicated, &expect),
            Err(ManifestMismatch::DuplicateSeat { slot: 0 }),
            "one slot cannot be held twice"
        );

        let mut overflowing = manifest();
        overflowing.active.push(ActiveSeat {
            slot: 9,
            node: OTHER_NODE.to_owned(),
            entity: 10,
        });
        assert_eq!(
            accept_start(&overflowing, &expect),
            Err(ManifestMismatch::SeatOutsideIsland {
                slot: 9,
                island_seats: 8
            }),
            "an active seat outside the declared island is not a seat"
        );

        let mut absent_recipient = manifest();
        absent_recipient.witness_recipients = vec![0, 7];
        assert_eq!(
            accept_start(&absent_recipient, &expect),
            Err(ManifestMismatch::WitnessRecipientNotActive { slot: 7 }),
            "witness frames must not be addressed to a seat nobody is in"
        );

        let mut self_recipient = manifest();
        self_recipient.witness_recipients = vec![4];
        assert_eq!(
            accept_start(&self_recipient, &expect),
            Err(ManifestMismatch::WitnessRecipientIsSelf { slot: 4 }),
            "a subject cannot be its own witness"
        );
    }

    /// A refusal must name the disagreement, so an operator reading a client
    /// log and a player reading the screen are looking at the same fact.
    #[test]
    fn a_manifest_refusal_says_which_condition_failed() {
        let sentence = ManifestMismatch::SeatMissing { slot: 4 }.player_sentence();
        assert!(sentence.contains("seat 4"), "{sentence}");
        assert!(sentence.is_ascii(), "{sentence}");
    }

    #[test]
    fn a_manifest_round_trips_through_its_wire_encoding() {
        let encoded = manifest().encode();
        assert_eq!(
            StartManifest::decode(&encoded).expect("round trip"),
            manifest()
        );
        assert!(StartManifest::decode(b"not a manifest").is_err());
    }

    /// An empty seat draws its state and *no name*. #484's rule, one level up
    /// from craft labels: a placeholder can be mistaken for a name somebody
    /// chose, and "PLAYER 3" in an empty seat says a person is there.
    #[test]
    fn an_empty_seat_is_drawn_as_absence_rather_than_a_placeholder() {
        let view = LobbyView {
            seats: vec![
                Seat {
                    slot: 4,
                    kind: SeatKind::Human,
                    state: SeatState::Connected,
                    nickname: Some("ada".to_owned()),
                },
                Seat {
                    slot: 5,
                    kind: SeatKind::Human,
                    state: SeatState::Empty,
                    nickname: None,
                },
            ],
            phase: Some(LobbyPhase::Lobby),
            starts_in_s: None,
            own_slot: Some(4),
            notice: None,
        };
        let lines = view.lines();
        assert!(
            lines.iter().any(|line| line == "seat 4  you      ada"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "seat 5  empty"),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.to_lowercase().contains("unknown")
                    || line.to_lowercase().contains("player 5")),
            "no placeholder name may appear: {lines:?}"
        );
        assert!(lines.iter().all(|line| line.is_ascii()), "{lines:?}");
    }

    /// The countdown is the host's to state. Without `starts_in_s` there is no
    /// countdown line at all, rather than one this client extrapolated.
    #[test]
    fn no_countdown_is_drawn_unless_the_host_sent_one() {
        let mut view = LobbyView {
            seats: vec![Seat {
                slot: 7,
                kind: SeatKind::Human,
                state: SeatState::Empty,
                nickname: None,
            }],
            phase: Some(LobbyPhase::Lobby),
            starts_in_s: None,
            own_slot: None,
            notice: None,
        };
        assert!(
            !view.lines().iter().any(|line| line.contains("starts in")),
            "a silent host means no countdown"
        );
        view.starts_in_s = Some(37);
        assert!(view.lines().iter().any(|line| line == "starts in 37 s"));
    }

    /// A service that says nothing about seats gets no waiting room drawn.
    #[test]
    fn a_service_that_names_no_phase_draws_no_waiting_room() {
        let legacy = LobbyView {
            seats: vec![Seat {
                slot: 8,
                kind: SeatKind::parse(None),
                state: SeatState::parse(None),
                nickname: Some("ada".to_owned()),
            }],
            phase: None,
            starts_in_s: None,
            own_slot: Some(8),
            notice: None,
        };
        assert!(!legacy.is_describable());
    }

    #[test]
    fn occupancy_counts_only_seats_somebody_holds() {
        let view = LobbyView {
            seats: vec![
                Seat {
                    slot: 0,
                    kind: SeatKind::Bot,
                    state: SeatState::Active,
                    nickname: Some("shakedown-1".to_owned()),
                },
                Seat {
                    slot: 4,
                    kind: SeatKind::Human,
                    state: SeatState::Connected,
                    nickname: Some("ada".to_owned()),
                },
                Seat {
                    slot: 5,
                    kind: SeatKind::Human,
                    state: SeatState::Reserved,
                    nickname: Some("lin".to_owned()),
                },
                Seat {
                    slot: 6,
                    kind: SeatKind::Human,
                    state: SeatState::Vacant,
                    nickname: None,
                },
                Seat {
                    slot: 7,
                    kind: SeatKind::Human,
                    state: SeatState::Empty,
                    nickname: None,
                },
            ],
            phase: Some(LobbyPhase::Lobby),
            starts_in_s: None,
            own_slot: Some(4),
            notice: None,
        };
        assert_eq!((view.human_seats_held(), view.human_seats()), (2, 4));
        assert!(view
            .lines()
            .iter()
            .any(|line| line == "2 of 4 player seats taken"));
        assert!(view.lines().iter().any(|line| line == "seat 6  left"));
        assert!(view.is_describable());
    }

    /// The refusal the owner asked for by name: what happened, and when to
    /// come back.
    #[test]
    fn a_full_campaign_says_so_and_says_when_to_return() {
        let sentence = refusal_sentence(
            Some("campaign_full"),
            Some("All 3 player seats are full; try the next lobby."),
            Some(126),
        );
        assert_eq!(
            sentence,
            "All 3 player seats are full; try the next lobby. Next lobby in about 2 minutes."
        );
        assert!(sentence.is_ascii());

        // A code with no drawable detail still says something a person reads.
        assert_eq!(
            refusal_sentence(Some("campaign_full"), Some("   "), None),
            "Every player seat is taken."
        );
        assert_eq!(
            refusal_sentence(Some("session_started"), None, Some(45)),
            "This attempt has already started. Next lobby in 45 seconds."
        );
        // Nothing a service can put in `detail` reaches the screen unfiltered.
        assert!(refusal_sentence(None, Some("full \u{202E}now\u{7}"), None).is_ascii());
    }

    #[test]
    fn a_wait_is_said_in_words() {
        assert_eq!(about_duration(0), "a moment");
        assert_eq!(about_duration(1), "1 second");
        assert_eq!(about_duration(45), "45 seconds");
        assert_eq!(about_duration(100), "about 2 minutes");
        assert_eq!(about_duration(91), "about 2 minutes");
        assert_eq!(about_duration(600), "about 10 minutes");
    }

    #[test]
    fn every_phase_sentence_is_ascii_and_says_something() {
        for phase in [
            LobbyPhase::parse("open"),
            LobbyPhase::parse("lobby"),
            LobbyPhase::parse("running"),
            LobbyPhase::parse("full"),
            LobbyPhase::parse("restarting"),
            LobbyPhase::parse("closed"),
            LobbyPhase::parse("paused"),
            LobbyPhase::parse("something-new"),
            LobbyPhase::parse("\u{202E}"),
        ] {
            let sentence = phase.sentence();
            assert!(sentence.is_ascii(), "{sentence}");
            assert!(!sentence.trim().is_empty(), "{phase:?}");
        }
        assert!(LobbyPhase::parse("lobby").is_waiting());
        assert!(!LobbyPhase::parse("running").is_waiting());
    }
}
