//! The client-side, deliberately non-authoritative hearsay contact view (#609).
//!
//! # What this is, and what it is not
//!
//! The campaign host can send a delayed, coarse cell report for a craft this
//! client does not currently replicate. This module keeps that latest report
//! briefly and turns it into one **render-only** snapshot for the future
//! screen-edge-arrow skin. It does not draw an arrow itself.
//!
//! This is presentation state, never game state. Nothing here is readable by
//! intent submission, range, arc, lock, or collision code; it is not inserted
//! into Bevy's world as a resource, is not passed to the executor, and this
//! module is private to the Regolith skin crate. Its one output is a
//! crate-private render view. In particular, an absent roster label remains
//! absent rather than becoming a made-up name.
//!
//! ADR-0050 is **Proposed**, not binding architecture, but this feature follows
//! its stated posture: H1 keeps hearsay out of simulation inputs; H2 lets it
//! gate neither replication membership nor rate; H3 preserves the supplied
//! source and age for visible labelling by the rendering piece. The host owns
//! the H2 behaviour; this client only receives its Meta record and writes
//! nothing back.
//!
//! # This is expiry on fold absence, not contact motion
//!
//! A host fold arrives every five seconds. A seat unnamed for three complete
//! fold periods disappears after fifteen seconds: there is no "last known"
//! ghost, inferred velocity, or stale-data placeholder. Until a fresher fold
//! arrives, its reported cell is frozen. That differs from [`crate::aoi`]'s
//! distance-based fade: the AOI module fades a *live replica* as it approaches
//! the interest boundary, whereas this one has no replica and expires only the
//! no-longer-reported hearsay fact.

#![allow(
    dead_code,
    reason = "A16 piece 4 is the first render consumer of this deliberately isolated view"
)]

use std::collections::BTreeMap;

use orrery_protocol::CellId;

use crate::net::{HearsayContacts, HearsaySource};
use crate::roster::{entity_of_slot, ShipRoster};

/// The campaign host's hearsay fold period: five seconds.
pub(crate) const HEARSAY_FOLD_TICKS: u64 = orrery_core::TICK_HZ as u64 * 5;

/// A contact expires after three fold periods without being named again.
pub(crate) const HEARSAY_EXPIRY_TICKS: u64 = HEARSAY_FOLD_TICKS * 3;

#[derive(Debug, Clone, Copy)]
struct RememberedContact {
    cell: CellId,
    source: HearsaySource,
    fold_tick: u64,
    fact_age_ticks: u16,
    last_named_tick: u64,
}

/// Client-only cache of the latest contact facts, owned by `CampaignRuntime`.
///
/// The type and its mutators are crate-private. The transport runtime owns it;
/// its sole consumer is [`HearsayRenderView`], so ruleset code cannot acquire
/// either current hearsay or an old contact for an intent or predicate.
#[derive(Debug, Default)]
pub(crate) struct HearsayState {
    latest_fold_tick: Option<u64>,
    contacts: BTreeMap<u8, RememberedContact>,
}

impl HearsayState {
    /// Accepts one new host fold at the local campaign tick that received it.
    ///
    /// An older or duplicate fold never refreshes a contact: doing so would
    /// let an out-of-date record survive beyond the three-period bound.
    pub(crate) fn accept(&mut self, record: HearsayContacts, received_tick: u64) {
        if self
            .latest_fold_tick
            .is_some_and(|latest| record.fold_tick <= latest)
        {
            return;
        }
        self.latest_fold_tick = Some(record.fold_tick);
        for contact in record.contacts {
            let Some(cell) = CellId::from_bits(contact.cell) else {
                continue;
            };
            self.contacts.insert(
                contact.seat,
                RememberedContact {
                    cell,
                    source: record.source,
                    fold_tick: record.fold_tick,
                    fact_age_ticks: contact.fact_age_ticks,
                    last_named_tick: received_tick,
                },
            );
        }
    }

    /// Removes each seat the host has omitted for three fold periods.
    pub(crate) fn expire(&mut self, now_tick: u64) {
        self.contacts.retain(|_, contact| {
            now_tick.saturating_sub(contact.last_named_tick) < HEARSAY_EXPIRY_TICKS
        });
    }

    /// Produces the only view the arrow-rendering skin may consume.
    #[must_use]
    pub(crate) fn render_view(&self, roster: &ShipRoster, now_tick: u64) -> HearsayRenderView {
        HearsayRenderView {
            contacts: self
                .contacts
                .iter()
                .map(|(&seat, contact)| HearsayRenderContact {
                    seat,
                    cell: contact.cell,
                    source: contact.source,
                    // H3's total age is the source's stamped fact age plus
                    // time since that source took this fold, never a local
                    // guess about contact motion.
                    age_ticks: now_tick
                        .saturating_sub(contact.fold_tick)
                        .saturating_add(u64::from(contact.fact_age_ticks)),
                    label: roster
                        .label(entity_of_slot(usize::from(seat)))
                        .map(str::to_owned),
                })
                .collect(),
        }
    }
}

/// The sole hearsay output available to the Regolith rendering skin.
///
/// It contains only source-labelled, age-stamped fixed-cell contact facts and
/// optional roster text. The view has no rule, intent, range, lock, or
/// collision operation; the arrow renderer decides how to draw it later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HearsayRenderView {
    contacts: Vec<HearsayRenderContact>,
}

impl HearsayRenderView {
    /// The ordered contact facts to draw, one per currently remembered seat.
    #[must_use]
    pub(crate) fn contacts(&self) -> &[HearsayRenderContact] {
        &self.contacts
    }
}

/// One displayable hearsay contact, deliberately without inferred motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HearsayRenderContact {
    /// The roster seat this contact describes.
    pub(crate) seat: u8,
    /// The fixed, host-reported cell.
    pub(crate) cell: CellId,
    /// Who computed this fact, for H3's eventual visible source label.
    pub(crate) source: HearsaySource,
    /// The fact's total age at this view's tick, for H3's visible age label.
    pub(crate) age_ticks: u64,
    /// A roster-provided display name, or `None` for no text at all.
    pub(crate) label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::HearsayContact;

    fn contact(seat: u8) -> HearsayContact {
        HearsayContact {
            seat,
            cell: CellId::from_coords(bevy::math::IVec3::ZERO, orrery_protocol::INTEREST_LEVEL)
                .expect("origin cell")
                .to_bits(),
            fact_age_ticks: 300,
        }
    }

    fn record(contacts: Vec<HearsayContact>) -> HearsayContacts {
        HearsayContacts {
            source: HearsaySource::HostRosterFold,
            fold_tick: 600,
            contacts,
        }
    }

    #[test]
    fn a_seat_omitted_for_three_fold_periods_is_absent() {
        let mut hearsay = HearsayState::default();
        let roster = ShipRoster::default();
        hearsay.accept(record(vec![contact(3)]), 700);
        assert_eq!(hearsay.render_view(&roster, 700).contacts().len(), 1);

        hearsay.expire(700 + HEARSAY_EXPIRY_TICKS - 1);
        assert_eq!(
            hearsay
                .render_view(&roster, 700 + HEARSAY_EXPIRY_TICKS - 1)
                .contacts()
                .len(),
            1,
            "a contact remains until all three complete periods elapse"
        );
        hearsay.expire(700 + HEARSAY_EXPIRY_TICKS);
        assert!(
            hearsay
                .render_view(&roster, 700 + HEARSAY_EXPIRY_TICKS)
                .contacts()
                .is_empty(),
            "an omitted seat disappears at 3F rather than becoming a last-known ghost"
        );
    }

    /// The freshness rule. Without it an out-of-order or replayed fold
    /// refreshes `last_named_tick`, and a contact outlives the three-period
    /// bound on the strength of a fact the host has already superseded.
    #[test]
    fn an_older_fold_never_refreshes_a_contact() {
        let mut hearsay = HearsayState::default();
        let roster = ShipRoster::default();
        hearsay.accept(record(vec![contact(3)]), 700);

        let mut stale = record(vec![contact(3)]);
        stale.fold_tick = 300;
        hearsay.accept(stale, 700 + HEARSAY_EXPIRY_TICKS - 1);
        hearsay.expire(700 + HEARSAY_EXPIRY_TICKS);
        assert!(
            hearsay
                .render_view(&roster, 700 + HEARSAY_EXPIRY_TICKS)
                .contacts()
                .is_empty(),
            "a stale fold must not extend a contact's life"
        );

        let mut duplicate = record(vec![contact(4)]);
        duplicate.fold_tick = 600;
        let mut fresh = HearsayState::default();
        fresh.accept(record(vec![contact(3)]), 700);
        fresh.accept(duplicate, 700);
        let seats: Vec<u8> = fresh
            .render_view(&roster, 700)
            .contacts()
            .iter()
            .map(|c| c.seat)
            .collect();
        assert_eq!(seats, vec![3], "a repeated fold tick adds nothing");
    }

    #[test]
    fn unnamed_seat_has_no_placeholder_label() {
        let mut hearsay = HearsayState::default();
        hearsay.accept(record(vec![contact(3)]), 700);

        let view = hearsay.render_view(&ShipRoster::default(), 700);
        let rendered = view.contacts().first().expect("contact remains drawable");
        assert_eq!(rendered.seat, 3);
        assert_eq!(
            rendered.label, None,
            "a missing roster label is absence, never UNKNOWN or PLAYER <seat>"
        );
    }

    #[test]
    fn h1_keeps_hearsay_state_out_of_the_ruleset_path() {
        let module = include_str!("hearsay.rs");
        let lib = include_str!("lib.rs");
        assert!(
            lib.lines().any(|line| line == "mod hearsay;"),
            "the hearsay module must stay private to the client skin"
        );
        assert!(
            !lib.lines().any(|line| line == "pub mod hearsay;"),
            "exporting the module would let non-skin code acquire hearsay"
        );
        assert!(
            !module
                .lines()
                .any(|line| line.trim() == "pub struct HearsayState {"),
            "exporting the state would let a ruleset path read hearsay"
        );
    }
}
