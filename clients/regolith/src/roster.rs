//! Nicknames, asserted by admission and drawn as labels on ships.
//!
//! "You have to know who you are fighting" (#523). The admission service
//! already receives a nickname at join and keeps it. The successful join reply
//! is the source for this client's own label; the public roster labels other
//! craft. This module keeps those two statements separate.
//!
//! ## A nickname is a label, and only a label
//!
//! The owner settled this in #484: nicknames are labels. Everything here
//! follows from that one sentence.
//!
//! * **It never travels with simulation state.** Other-craft labels come from
//!   `GET /v1/campaigns/<id>/roster`; the own label comes from the successful
//!   join reply. Neither is a field on replicated craft state. Unbounded
//!   player-supplied text on the replication hot path would spend the
//!   determinism-critical budget on decoration, and would make a label
//!   something a state hash could disagree about.
//! * **A public row never says who this client is.** It may be late, stale,
//!   missing or wrong without affecting simulation, but attributing its label
//!   to the local craft would turn public hearsay plus a slot assumption into
//!   an identity claim. [`OwnLabelGrant`] is the only source for that craft.
//!   Nothing downstream of [`ShipRoster`] is read by the intent pipeline, the
//!   executor, the banking row, or any addressing decision.
//! * **A missing label reads as absent.** [`ShipRoster::label`] returns
//!   `None` and the caller draws no text at all. There is deliberately no
//!   placeholder: "UNKNOWN", "PLAYER 3" or an empty quoted string could all be
//!   mistaken for a name somebody actually chose.
//!
//! ## Sanitising
//!
//! A roster label can originate with a player or the campaign's generated
//! crowd. Both take the same defensive display path before anything reaches
//! Bevy text.
//!
//! ## The font gap, stated rather than hidden
//!
//! No font asset is loaded, so Bevy's built-in ASCII-only face is what draws
//! these labels (#526). Unsupported glyphs are removed, never replaced with a
//! question-mark string that could be mistaken for a real name.

use std::collections::BTreeMap;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use orrery_protocol::PersistId;
use serde::Deserialize;

/// How often the client asks admission for a fresh roster.
///
/// A label may be seconds out of date with no consequence, so this is tuned to
/// be invisible to the service rather than to be quick: one small GET every
/// five seconds per playing client.
pub const ROSTER_REFRESH: Duration = Duration::from_secs(5);

/// How long a roster fetch may take before it is a failure. Short, because a
/// slow answer is worth nothing — the next one is five seconds away.
pub const ROSTER_TIMEOUT: Duration = Duration::from_secs(4);

/// The longest label drawn, in characters. Admission's own bound.
pub const NICKNAME_MAX_CHARS: usize = 32;

/// One row of the campaign roster.
///
/// #573 turned this endpoint's answer from "labelled craft" into a **complete
/// seat map**: every configured seat, its kind, its state, and who holds it,
/// with an empty seat representable. Both shapes decode into this one type —
/// `kind` and `state` are absent from a service older than #573, and
/// `nickname` became nullable when an empty seat gained the right to have no
/// label at all.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RosterRow {
    /// The swarm slot whose craft carries this display label.
    pub slot: usize,
    /// The generated or player-supplied label, unsanitised as the service
    /// holds it. `null` for a seat with nobody in it, which draws no name.
    #[serde(default)]
    pub nickname: Option<String>,
    /// `bot` or `human`, when the service says which.
    #[serde(default)]
    pub kind: Option<String>,
    /// `active`, `reserved`, `connected`, `empty` or `vacant`, when the
    /// service says which.
    #[serde(default)]
    pub state: Option<String>,
}

impl RosterRow {
    /// A labelled row, as a pre-#573 service sends it. Test convenience.
    #[must_use]
    pub fn labelled(slot: usize, nickname: &str) -> Self {
        Self {
            slot,
            nickname: Some(nickname.to_owned()),
            kind: None,
            state: None,
        }
    }
}

/// The body of `GET /v1/campaigns/<id>/roster`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RosterResponse {
    /// One row per seat in the live campaign. Empty is ordinary.
    #[serde(default)]
    pub roster: Vec<RosterRow>,
    /// The attempt phase, when the service names one. A service older than
    /// #573 names none, and the client must not guess one for it.
    #[serde(default)]
    pub phase: Option<String>,
    /// Seconds until Start, when the service sends them. Nothing extrapolates
    /// this locally; see [`crate::lobby`].
    #[serde(default)]
    pub starts_in_s: Option<u64>,
}

/// Admission's assertion about this client's own display label.
///
/// The slot and optional nickname arrive together through the successful join
/// path. `nickname: None` means the reply source did not assert a drawable own
/// label, so the local craft and waiting-room row must render no name. In
/// particular, neither may substitute a matching row from the public roster.
#[derive(Debug, Clone, Copy)]
pub struct OwnLabelGrant<'a> {
    /// The slot admission granted to this client.
    pub slot: usize,
    /// The label admission echoed, or absence for an older service/join file.
    pub nickname: Option<&'a str>,
}

impl RosterResponse {
    /// The waiting room this answer describes.
    ///
    /// Every row becomes a seat in service order. Other-seat labels are a
    /// straight transcription; the local seat's label comes only from `own`.
    #[must_use]
    pub fn lobby_view(&self, own: Option<OwnLabelGrant<'_>>) -> crate::lobby::LobbyView {
        crate::lobby::LobbyView {
            seats: self
                .roster
                .iter()
                .map(|row| {
                    let nickname = match own {
                        Some(grant) if row.slot == grant.slot => {
                            grant.nickname.and_then(sanitise_nickname)
                        }
                        _ => row.nickname.as_deref().and_then(sanitise_nickname),
                    };
                    crate::lobby::Seat {
                        slot: row.slot,
                        kind: crate::lobby::SeatKind::parse(row.kind.as_deref()),
                        state: crate::lobby::SeatState::parse(row.state.as_deref()),
                        nickname,
                    }
                })
                .collect(),
            phase: self.phase.as_deref().map(crate::lobby::LobbyPhase::parse),
            starts_in_s: self.starts_in_s,
            own_slot: own.map(|grant| grant.slot),
            notice: None,
        }
    }
}

/// The craft a swarm slot flies.
///
/// Mirrors `CampaignRuntime::launch`, which sets `entity = slot + 1`. Verified
/// against `campaign.rs` rather than assumed; the pinning test below fails if
/// that mapping ever moves.
#[must_use]
pub fn entity_of_slot(slot: usize) -> PersistId {
    PersistId::new(slot as u64 + 1)
}

/// A nickname reduced to something safe to draw, or `None` if nothing is left.
///
/// Consistent with admission's display filter and defensive against a stale or
/// independently implemented service:
///
/// * only visible ASCII survives. The default font has no contract for glyphs
///   beyond it (#526), and ASCII also excludes controls, bidi overrides and
///   zero-width formatting characters in one rule;
/// * surrounding whitespace is trimmed, so a name padded to look longer does
///   not push its neighbours around;
/// * the result is bounded at [`NICKNAME_MAX_CHARS`] characters.
///
/// `None` means **no label**, and the caller must draw nothing rather than
/// substitute a placeholder.
#[must_use]
pub fn sanitise_nickname(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(char::is_ascii)
        .filter(|glyph| !glyph.is_ascii_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(NICKNAME_MAX_CHARS).collect())
}

/// The labels this client currently believes in.
///
/// Presentation only. Empty is the normal state before the first fetch lands
/// and after a fetch fails, and both must look the same on screen as a
/// campaign with no labels: ships drawn without text.
#[derive(Debug, Default, Clone, bevy::prelude::Resource)]
pub struct ShipRoster {
    labels: BTreeMap<PersistId, String>,
    /// How many roster fetches have landed. Diagnostics for the F3 pane; the
    /// count is the difference between "no labels" and "never asked".
    pub fetches: u64,
    /// The last fetch failure, kept so a missing label can be explained.
    pub last_error: Option<String>,
}

impl ShipRoster {
    /// Installs admission's own-label assertion without waiting for a public
    /// roster fetch. An absent label removes text for the local craft.
    pub fn set_own(&mut self, grant: OwnLabelGrant<'_>) {
        let entity = entity_of_slot(grant.slot);
        self.labels.remove(&entity);
        if let Some(name) = grant.nickname.and_then(sanitise_nickname) {
            self.labels.insert(entity, name);
        }
    }

    /// Replaces the public labels from one roster answer and reapplies `own`.
    ///
    /// Wholesale replacement, not a merge: a successful answer is the complete
    /// seat map *now*. Admission keeps a bound label across its attempt-pointer
    /// hand-off (#706), so an absent label in a successful answer means the seat
    /// is no longer reserved or bound. Merging would retain a departed player's
    /// label on an empty or reused slot. Fetch failures take [`Self::fail`]
    /// instead and deliberately keep the last good map.
    pub fn accept(&mut self, response: &RosterResponse, own: Option<OwnLabelGrant<'_>>) {
        self.labels = response
            .roster
            .iter()
            .filter_map(|row| {
                let entity = entity_of_slot(row.slot);
                let name = sanitise_nickname(row.nickname.as_deref()?)?;
                Some((entity, name))
            })
            .collect();
        if let Some(grant) = own {
            self.set_own(grant);
        }
        self.fetches = self.fetches.saturating_add(1);
        self.last_error = None;
    }

    /// Records a failed fetch. The labels already held are kept: a label is
    /// allowed to be stale, and blanking every name because one HTTP request
    /// timed out would be a worse answer than a slightly old one.
    pub fn fail(&mut self, error: String) {
        self.last_error = Some(error);
    }

    /// The label for a craft, or `None` for *no label at all*.
    #[must_use]
    pub fn label(&self, entity: PersistId) -> Option<&str> {
        self.labels.get(&entity).map(String::as_str)
    }

    /// The craft carrying an exact display label, when the current roster has one.
    ///
    /// Used by the release preflight to bind a requested peer nickname to the
    /// replicated entity it must actually observe. Finding a roster row alone
    /// is not proof that the peer's craft reached this client.
    #[must_use]
    pub fn entity_named(&self, nickname: &str) -> Option<PersistId> {
        self.labels
            .iter()
            .find_map(|(entity, label)| (label == nickname).then_some(*entity))
    }

    /// How many craft currently carry a label.
    #[must_use]
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// Whether nothing is labelled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// `roster 1 labelled | 4 fetches`, for the F3 pane.
    #[must_use]
    pub fn summary_line(&self) -> String {
        match &self.last_error {
            None => format!("roster {} labelled | {} fetches", self.len(), self.fetches),
            Some(error) => format!(
                "roster {} labelled | {} fetches | last fetch failed: {error}",
                self.len(),
                self.fetches
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The roster is keyed by slot and the skin draws on entities. If this
    /// mapping moves, every label lands on the wrong ship — which is worse
    /// than no labels, because a wrong name is believed.
    #[test]
    fn a_slot_labels_the_craft_the_campaign_runtime_gives_it() {
        for slot in [0usize, 1, 7, 8, 31] {
            assert_eq!(
                entity_of_slot(slot),
                PersistId::new(slot as u64 + 1),
                "campaign.rs launches slot {slot} as slot + 1"
            );
        }
    }

    /// #529's acceptance boundary is an opponent, not merely the exterior
    /// craft that admission already named. Pin the complete slot-to-label
    /// resolution so a full service response cannot collapse back to one seat.
    #[test]
    fn a_complete_campaign_roster_resolves_every_opponent() {
        let mut roster = ShipRoster::default();
        roster.accept(
            &RosterResponse {
                roster: (0..=8)
                    .map(|slot| {
                        if slot == 8 {
                            RosterRow::labelled(slot, "ada")
                        } else {
                            RosterRow::labelled(slot, &format!("test-{}", slot + 1))
                        }
                    })
                    .collect(),
                ..Default::default()
            },
            None,
        );

        assert_eq!(roster.len(), 9);
        for slot in 0..8 {
            assert!(
                roster.label(entity_of_slot(slot)).is_some(),
                "opponent slot {slot} must resolve to a display label"
            );
        }
        assert_eq!(roster.label(entity_of_slot(8)), Some("ada"));
    }

    /// #600: a public roster can name every slot but cannot say which name is
    /// this client's. Only the label granted in the join reply may be attached
    /// to the local craft or to the waiting-room row marked `you`.
    #[test]
    fn the_local_craft_is_labelled_only_by_its_join_grant() {
        let response = RosterResponse {
            roster: vec![
                RosterRow {
                    slot: 5,
                    nickname: Some("shooshte".to_owned()),
                    kind: Some("human".to_owned()),
                    state: Some("active".to_owned()),
                },
                RosterRow::labelled(6, "baadc0de"),
            ],
            phase: Some("lobby".to_owned()),
            ..Default::default()
        };
        let local = entity_of_slot(5);
        let other = entity_of_slot(6);
        let unlabelled_grant = Some(OwnLabelGrant {
            slot: 5,
            nickname: None,
        });

        let mut roster = ShipRoster::default();
        roster.accept(&response, unlabelled_grant);
        assert_eq!(
            roster.label(local),
            None,
            "a public row must not attribute another player's label to the local craft"
        );
        assert_eq!(roster.label(other), Some("baadc0de"));
        let room = response.lobby_view(unlabelled_grant);
        assert_eq!(
            room.seats
                .iter()
                .find(|seat| seat.slot == 5)
                .unwrap()
                .nickname,
            None,
            "the waiting-room row marked `you` has the same attribution boundary"
        );

        let granted = Some(OwnLabelGrant {
            slot: 5,
            nickname: Some("themre"),
        });
        roster.accept(&response, granted);
        assert_eq!(
            roster.label(local),
            Some("themre"),
            "the join grant, not the public row, names the local craft"
        );
        assert_eq!(
            response
                .lobby_view(granted)
                .seats
                .into_iter()
                .find(|seat| seat.slot == 5)
                .unwrap()
                .nickname
                .as_deref(),
            Some("themre")
        );
    }

    /// A missing label must read as absent. Nothing here may invent a name.
    #[test]
    fn an_unlabelled_craft_gets_no_label_rather_than_a_placeholder() {
        let mut roster = ShipRoster::default();
        roster.accept(
            &RosterResponse {
                roster: vec![RosterRow::labelled(8, "ada")],
                ..Default::default()
            },
            None,
        );
        assert_eq!(roster.label(entity_of_slot(8)), Some("ada"));
        assert_eq!(
            roster.label(entity_of_slot(3)),
            None,
            "a craft with no roster row must have no label"
        );

        // A row whose nickname sanitises away is the same as no row: it must
        // not become an empty label, which would draw as a blank name tag.
        roster.accept(
            &RosterResponse {
                roster: vec![RosterRow::labelled(4, "\u{202E}\u{200B}   ")],
                ..Default::default()
            },
            None,
        );
        assert!(
            roster.is_empty(),
            "a nickname with nothing drawable in it is not a label"
        );
    }

    /// A complete successful answer clears a departed player's old label. This
    /// is why `accept` cannot merge even though a failed fetch keeps old labels.
    #[test]
    fn a_departed_players_label_disappears_on_the_next_authoritative_answer() {
        let mut roster = ShipRoster::default();
        roster.accept(
            &RosterResponse {
                roster: vec![
                    RosterRow::labelled(8, "ada"),
                    RosterRow::labelled(2, "grace"),
                ],
                ..Default::default()
            },
            None,
        );
        assert_eq!(roster.len(), 2);
        roster.accept(
            &RosterResponse {
                roster: vec![
                    RosterRow::labelled(8, "ada"),
                    RosterRow {
                        slot: 2,
                        nickname: None,
                        kind: Some("human".to_owned()),
                        state: Some("empty".to_owned()),
                    },
                ],
                ..Default::default()
            },
            None,
        );
        assert_eq!(roster.label(entity_of_slot(2)), None, "grace left");
        assert_eq!(roster.len(), 1);
    }

    /// A failed fetch keeps what it had. A label is allowed to be stale.
    #[test]
    fn a_failed_fetch_keeps_the_labels_it_already_had() {
        let mut roster = ShipRoster::default();
        roster.accept(
            &RosterResponse {
                roster: vec![RosterRow::labelled(8, "ada")],
                ..Default::default()
            },
            None,
        );
        roster.fail("connection refused".to_owned());
        assert_eq!(roster.label(entity_of_slot(8)), Some("ada"));
        assert!(roster.summary_line().contains("connection refused"));
    }

    #[test]
    fn a_nickname_is_sanitised_before_it_is_ever_drawn() {
        assert_eq!(sanitise_nickname("ada"), Some("ada".to_owned()));
        assert_eq!(sanitise_nickname("  ada  "), Some("ada".to_owned()));
        // Admission already refuses tabs; a stored row from any other path
        // must not be able to break the layout either.
        assert_eq!(sanitise_nickname("a\tb"), Some("ab".to_owned()));
        assert_eq!(sanitise_nickname("a\u{7}b"), Some("ab".to_owned()));
        // Bidi overrides, zero-width joiners and unsupported font glyphs cannot
        // survive to the screen.
        assert_eq!(
            sanitise_nickname("ada\u{202E}bob"),
            Some("adabob".to_owned())
        );
        assert_eq!(sanitise_nickname("a\u{200B}b"), Some("ab".to_owned()));
        assert_eq!(sanitise_nickname("Ren\u{e9}e"), Some("Rene".to_owned()));
        // Nothing drawable means no label, never an empty one.
        assert_eq!(sanitise_nickname(""), None);
        assert_eq!(sanitise_nickname("   "), None);
        assert_eq!(sanitise_nickname("\u{FEFF}"), None);
        // The bound is characters.
        let long = "x".repeat(64);
        let cut = sanitise_nickname(&long).expect("ASCII letters are drawable text");
        assert_eq!(cut.chars().count(), NICKNAME_MAX_CHARS);
    }
}

/// The in-flight roster request, if any.
#[derive(Resource, Default)]
pub struct RosterTask(Mutex<Option<mpsc::Receiver<Result<RosterResponse, String>>>>);

fn get_roster(url: &str) -> Result<RosterResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(ROSTER_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client.get(url).send().map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("service answered HTTP {}", response.status()));
    }
    response.json().map_err(|error| error.to_string())
}

/// Collects the last roster answer and starts the next request.
///
/// Never blocks the frame: the request runs on its own thread and the reply is
/// picked up whenever it lands. A campaign with no roster URL — the
/// join-from-file path, which never spoke to an admission service — simply
/// never asks, and every ship stays unlabelled, which is the correct answer
/// rather than a degraded one.
pub fn refresh_roster(
    session: Res<crate::ActiveSession>,
    task: Res<RosterTask>,
    mut roster: ResMut<ShipRoster>,
    mut lobby: ResMut<crate::lobby::LobbyView>,
) {
    let mut slot = task.0.lock().expect("roster task lock");
    if let Some(receiver) = slot.as_ref() {
        match receiver.try_recv() {
            Ok(Ok(response)) => {
                let own = match &*session {
                    crate::ActiveSession::Campaign(runtime) => Some(OwnLabelGrant {
                        slot: runtime.config().slot,
                        nickname: runtime.config().own_label.as_deref(),
                    }),
                    crate::ActiveSession::Local(_) => None,
                };
                roster.accept(&response, own);
                // The waiting room is replaced wholesale for the same reason
                // the labels are: the answer is the room *now*, and merging
                // would keep a seat occupied by somebody who left.
                let notice = lobby.notice.take();
                *lobby = response.lobby_view(own);
                lobby.notice = notice;
                *slot = None;
            }
            Ok(Err(error)) => {
                roster.fail(error);
                *slot = None;
            }
            // Still in flight. Asking again would stack requests behind a
            // service that is already slow.
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                roster.fail("roster worker stopped".to_owned());
                *slot = None;
            }
        }
    }
    let crate::ActiveSession::Campaign(runtime) = &*session else {
        return;
    };
    let Some(url) = runtime.config().roster_url.clone() else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(get_roster(&url));
    });
    *slot = Some(receiver);
}
