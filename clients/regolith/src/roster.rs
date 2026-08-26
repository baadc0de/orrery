//! Nicknames, fetched from admission and drawn as labels on ships.
//!
//! "You have to know who you are fighting" (#523). The admission service
//! already receives a nickname at join and keeps it; nothing carried it
//! anywhere the client could see. This module carries it.
//!
//! ## A nickname is a label, and only a label
//!
//! The owner settled this in #484: nicknames are labels. Everything here
//! follows from that one sentence.
//!
//! * **It never travels with simulation state.** The roster is a separate
//!   `GET /v1/campaigns/<id>/roster` off the admission service, not a field on
//!   replicated craft state. Unbounded player-supplied text on the replication
//!   hot path would spend the determinism-critical budget on decoration, and
//!   would make a label something a state hash could disagree about. The
//!   alternative — a display name replicated alongside craft state — is
//!   simpler to consume and was rejected for exactly that.
//! * **It may be late, stale, missing or wrong with no consequence.** Nothing
//!   downstream of [`ShipRoster`] is read by the intent pipeline, the
//!   executor, the banking row, or any addressing decision. Craft are
//!   identified by [`orrery_protocol::PersistId`] everywhere they matter, and
//!   this module maps *to* a label, never *from* one.
//! * **A missing label reads as absent.** [`ShipRoster::label`] returns
//!   `None` and the caller draws no text at all. There is deliberately no
//!   placeholder: "UNKNOWN", "PLAYER 3" or an empty quoted string could all be
//!   mistaken for a name somebody actually chose.
//!
//! ## Sanitising
//!
//! The nickname is player-supplied text that the client draws. Admission's own
//! rule is `[^\t\r\n]{1,32}` — 1 to 32 characters, no tabs or newlines — and
//! [`sanitise_nickname`] stays consistent with it and then goes further,
//! because "no tabs" is not the same as "safe to lay out": other control
//! characters and the Unicode bidi overrides can reorder or blank a line.
//!
//! ## The font gap, stated rather than hidden
//!
//! No font asset is loaded, so Bevy's built-in ASCII-only face is what draws
//! these labels (#526). A nickname containing non-ASCII letters will render as
//! boxes until a font ships (#347). That is left as-is on purpose: mangling a
//! player's chosen name into `????` destroys the identity the label exists to
//! carry, and a box at least says "this glyph could not be drawn" rather than
//! asserting a character nobody typed.

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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RosterRow {
    /// The swarm slot this nickname joined on.
    pub slot: usize,
    /// The player-supplied nickname, unsanitised as the service holds it.
    pub nickname: String,
}

/// The body of `GET /v1/campaigns/<id>/roster`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RosterResponse {
    /// One row per live session. Empty is an ordinary answer.
    #[serde(default)]
    pub roster: Vec<RosterRow>,
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
/// Consistent with admission's `[^\t\r\n]{1,32}` and stricter:
///
/// * every control character goes, not only tab and the newlines — a bell or a
///   `\x0c` in a UI string is at best noise and at worst a layout break;
/// * the Unicode bidi and zero-width formatting characters go, because they
///   are the ones that can silently reorder a line or hide text inside a name
///   that looks shorter than it is;
/// * surrounding whitespace is trimmed, so a name padded to look longer does
///   not push its neighbours around;
/// * the result is bounded at [`NICKNAME_MAX_CHARS`] *characters*, counted as
///   `char`s rather than bytes so a multi-byte name is not cut mid-sequence.
///
/// `None` means **no label**, and the caller must draw nothing rather than
/// substitute a placeholder.
#[must_use]
pub fn sanitise_nickname(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|glyph| !glyph.is_control() && !is_layout_hazard(*glyph))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(NICKNAME_MAX_CHARS).collect())
}

/// Zero-width and directional formatting characters, which are the ones that
/// can rewrite a line's appearance without adding a visible glyph.
fn is_layout_hazard(glyph: char) -> bool {
    matches!(glyph,
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}')
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
    /// Replaces every label from one roster answer.
    ///
    /// Wholesale replacement, not a merge: the service's answer is the set of
    /// players who are live *now*, so a row that stopped being sent means that
    /// player left, and merging would keep labelling a ship nobody is flying.
    pub fn accept(&mut self, response: &RosterResponse) {
        self.labels = response
            .roster
            .iter()
            .filter_map(|row| Some((entity_of_slot(row.slot), sanitise_nickname(&row.nickname)?)))
            .collect();
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

    /// A missing label must read as absent. Nothing here may invent a name.
    #[test]
    fn an_unlabelled_craft_gets_no_label_rather_than_a_placeholder() {
        let mut roster = ShipRoster::default();
        roster.accept(&RosterResponse {
            roster: vec![RosterRow {
                slot: 8,
                nickname: "ada".to_owned(),
            }],
        });
        assert_eq!(roster.label(entity_of_slot(8)), Some("ada"));
        assert_eq!(
            roster.label(entity_of_slot(3)),
            None,
            "a craft with no roster row must have no label"
        );

        // A row whose nickname sanitises away is the same as no row: it must
        // not become an empty label, which would draw as a blank name tag.
        roster.accept(&RosterResponse {
            roster: vec![RosterRow {
                slot: 4,
                nickname: "\u{202E}\u{200B}   ".to_owned(),
            }],
        });
        assert!(
            roster.is_empty(),
            "a nickname with nothing drawable in it is not a label"
        );
    }

    /// The roster is the live set, so a player who left stops being drawn.
    #[test]
    fn a_roster_answer_replaces_the_labels_rather_than_merging_them() {
        let mut roster = ShipRoster::default();
        roster.accept(&RosterResponse {
            roster: vec![
                RosterRow {
                    slot: 8,
                    nickname: "ada".to_owned(),
                },
                RosterRow {
                    slot: 2,
                    nickname: "grace".to_owned(),
                },
            ],
        });
        assert_eq!(roster.len(), 2);
        roster.accept(&RosterResponse {
            roster: vec![RosterRow {
                slot: 8,
                nickname: "ada".to_owned(),
            }],
        });
        assert_eq!(roster.label(entity_of_slot(2)), None, "grace left");
        assert_eq!(roster.len(), 1);
    }

    /// A failed fetch keeps what it had. A label is allowed to be stale.
    #[test]
    fn a_failed_fetch_keeps_the_labels_it_already_had() {
        let mut roster = ShipRoster::default();
        roster.accept(&RosterResponse {
            roster: vec![RosterRow {
                slot: 8,
                nickname: "ada".to_owned(),
            }],
        });
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
        // Bidi overrides and zero-width joiners cannot survive to the screen.
        assert_eq!(
            sanitise_nickname("ada\u{202E}bob"),
            Some("adabob".to_owned())
        );
        assert_eq!(sanitise_nickname("a\u{200B}b"), Some("ab".to_owned()));
        // Nothing drawable means no label, never an empty one.
        assert_eq!(sanitise_nickname(""), None);
        assert_eq!(sanitise_nickname("   "), None);
        assert_eq!(sanitise_nickname("\u{FEFF}"), None);
        // The bound is characters, and the cut is never mid-character.
        let long = "\u{e9}".repeat(64);
        let cut = sanitise_nickname(&long).expect("accented letters are drawable text");
        assert_eq!(cut.chars().count(), NICKNAME_MAX_CHARS);
        assert!(cut.is_char_boundary(cut.len()));
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
) {
    let mut slot = task.0.lock().expect("roster task lock");
    if let Some(receiver) = slot.as_ref() {
        match receiver.try_recv() {
            Ok(Ok(response)) => {
                roster.accept(&response);
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
