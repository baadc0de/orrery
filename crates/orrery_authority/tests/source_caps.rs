//! The sum-of-caps audit (#923).
//!
//! The rule: an unsheddable wire family must be admission-capped at its
//! source, such that the sum of every family's cap fits inside the peer
//! upload budget (D6, D16: ≤ 1 Mbps) with headroom left for replication —
//! the one sheddable lane, and the one the backstop is allowed to police.
//! Nothing enforced that sum before this file. The witness lane declared a
//! 20 % share, the hit lane now declares one, and everything else was
//! asserted "sparse" in docs/02-networking.md §7 with nothing behind the word.
//!
//! Three things this audit does, and one it refuses to do:
//!
//! 1. sums the **declared** caps and fails if they exceed the budget minus
//!    the replication headroom, naming every family in the sum;
//! 2. proves the check bites, by raising one cap past the budget;
//! 3. **names** every unsheddable family that has no cap, as a pinned list —
//!    a new family without a cap fails this test rather than passing it
//!    silently, and a family that gains a cap must be moved out of the list.
//!
//! What it does not do is assert the undeclared families' traffic is small.
//! That is the claim docs/02 §7 makes, and the whole point here is that a
//! claim without a bound is not a cap.

use orrery_net::budget::{datagram_wire_bytes, Bandwidth, UploadBudget};
use orrery_predict::config::PredictConfig;
use orrery_protocol::{
    channels::encode_hit, HitClaim, HitClaimCap, HitMsg, HitOutcome, HitRefusal, HitSurface,
    HitVerdict, InterpBasis, LatticePoint, PersistId, QuantizedDir, QuantizedRay, Tick, UNorm16,
    WeaponRef,
};
use orrery_witness::plugin::WITNESS_LANE_SHARE_PCT;

/// The share of the budget reserved for replication, in percent.
///
/// Derived, not chosen: docs/02-networking.md §6 puts a naive mesh's upload at
/// `12·(n−1) kb/s` (Donnybrook's Quake III figure), which at D16's mesh
/// ceiling of 32 players is 372 kbps — 37 % of the budget — before the
/// per-link framing floor the same section says makes the real figure worse.
/// Half the budget is the smallest round number above that, and the sum of
/// every unsheddable cap must fit in the other half.
const REPLICATION_HEADROOM_PCT: u64 = 50;

/// One wire family, and what bounds it at its source.
#[derive(Debug, Clone, Copy)]
struct Family {
    name: &'static str,
    status: CapStatus,
}

#[derive(Debug, Clone, Copy)]
enum CapStatus {
    /// Capped at the source; the figure is the family's sustained ceiling.
    Declared(Bandwidth),
    /// Unsheddable and uncapped. The string says what would bound it.
    Undeclared(&'static str),
    /// Not on the P2P links the meter counts (#923: the meter is P2P-only).
    OutsideMeter(&'static str),
    /// The one lane the backstop may shed; the headroom is for it.
    Sheddable,
}

/// The families as the tree stands. Every entry that is not `Declared` is a
/// debt the audit names rather than hides.
fn families() -> Vec<Family> {
    let budget = UploadBudget::default().sustained;
    vec![
        Family {
            name: "replication deltas, proxies, keyframes to non-witness links",
            status: CapStatus::Sheddable,
        },
        Family {
            name: "witness log frames and state claims",
            status: CapStatus::Declared(pct_of(budget, WITNESS_LANE_SHARE_PCT)),
        },
        Family {
            name: "hit claims and verdicts",
            status: CapStatus::Declared(hit_family_cap()),
        },
        Family {
            name: "keyframes to witness-set links",
            status: CapStatus::Undeclared(
                "unsheddable by #923's classification; bounded only by the send \
                 accumulator's keyframe cadence, which is not a byte cap",
            ),
        },
        Family {
            name: "delivered inputs (cross-authority events on the replication stream)",
            status: CapStatus::Undeclared(
                "D46 clause (e) MAX_EVENTS_PER_STEP = 64/entity/tick is specified and \
                 unimplemented; it lives in canonical state (frozen crates)",
            ),
        },
        Family {
            name: "control: witness gap-repair responses",
            status: CapStatus::Undeclared(
                "bounded in count (one outstanding repair per witness link on a \
                 backoff), not in bytes; a 40 kB repair is one message",
            ),
        },
        Family {
            name: "control: handoff acks, handshakes, manifest deltas",
            status: CapStatus::Undeclared("asserted 'sparse' in docs/02 §7; nothing enforces it"),
        },
        Family {
            name: "lease claims to the registrar (D7 §10: 20/s, burst 64)",
            status: CapStatus::OutsideMeter(
                "gateway traffic; the upload meter counts P2P links only, so the cap \
                 exists but is not in this sum",
            ),
        },
    ]
}

impl Family {
    fn describe(self) -> String {
        match self.status {
            CapStatus::Declared(cap) => format!("declared: {cap}"),
            CapStatus::Undeclared(why) => format!("UNDECLARED: {why}"),
            CapStatus::OutsideMeter(why) => format!("outside the meter: {why}"),
            CapStatus::Sheddable => "sheddable (the headroom is for it)".to_string(),
        }
    }
}

fn pct_of(budget: Bandwidth, pct: u64) -> Bandwidth {
    Bandwidth::from_bits_per_sec(budget.bits_per_sec() * pct / 100)
}

/// The widest claim a shooter can put on the wire: every varint at its
/// widest, so the figure is a ceiling and not a typical case.
fn widest_claim() -> HitClaim {
    HitClaim {
        shooter: PersistId::new(u64::MAX),
        target: PersistId::new(u64::MAX),
        weapon: WeaponRef(u32::MAX),
        fire_tick: Tick::new(u64::MAX),
        basis: InterpBasis {
            from: Tick::new(u64::MAX),
            to: Tick::new(u64::MAX),
            alpha: UNorm16::ONE,
        },
        ray: QuantizedRay {
            origin: LatticePoint::new(i64::MIN, i64::MIN, i64::MIN),
            direction: QuantizedDir::new(i16::MIN, i16::MIN, i16::MIN),
        },
        claimed: HitSurface(u16::MAX),
        input_seq: u16::MAX,
    }
}

/// The widest verdict: an acceptance with a maximal pose is the largest arm.
fn widest_verdict() -> HitVerdict {
    let claim = widest_claim();
    let accepted = HitVerdict {
        claim: claim.key(),
        target: claim.target,
        claimed: claim.claimed,
        outcome: HitOutcome::Accepted {
            applied_at: Tick::new(u64::MAX),
            pose: LatticePoint::new(i64::MIN, i64::MIN, i64::MIN),
        },
    };
    let refused = HitVerdict {
        outcome: HitOutcome::Rejected(HitRefusal::BasisNotRetained {
            tick: Tick::new(u64::MAX),
            oldest_retained: Some(Tick::new(u64::MAX)),
            newest_retained: Some(Tick::new(u64::MAX)),
        }),
        ..accepted
    };
    let size = |v: &HitVerdict| encode_hit(&HitMsg::Verdict(*v)).len();
    if size(&refused) > size(&accepted) {
        refused
    } else {
        accepted
    }
}

/// The hit family's per-peer ceiling: the sustained claim rate the source
/// gate admits, times the widest claim datagram, plus the same rate of
/// verdicts for one answered source.
///
/// The verdict direction scales with the number of sources shooting at this
/// peer — each source has its own gate — and this figure counts one. The
/// honest multiplier is the interest set, and honest sources fire at their
/// weapon's rate, twenty times under the cap; the pathological multiplier is
/// [`orrery_authority::MAX_CLAIM_SOURCES`] and is reported, not fitted.
fn hit_family_cap() -> Bandwidth {
    let cfg = PredictConfig::default();
    let cap = HitClaimCap::for_window(cfg.hit_window());
    let per_sec = cap.sustained_claims_per_second(cfg.tick_hz);
    let claim = datagram_wire_bytes(encode_hit(&HitMsg::Claim(widest_claim())).len());
    let verdict = datagram_wire_bytes(encode_hit(&HitMsg::Verdict(widest_verdict())).len());
    Bandwidth::from_bits_per_sec((claim + verdict) * per_sec * 8)
}

/// What the audit found.
#[derive(Debug)]
enum Audit {
    /// The declared caps fit; the sum and the ceiling they fit under.
    Fits { sum: Bandwidth, ceiling: Bandwidth },
    /// They do not, and here is every declared family, largest first.
    OverBudget {
        sum: Bandwidth,
        ceiling: Bandwidth,
        declared: Vec<(&'static str, Bandwidth)>,
    },
}

fn audit(families: &[Family], budget: Bandwidth, headroom_pct: u64) -> Audit {
    let ceiling = pct_of(budget, 100 - headroom_pct);
    let mut declared: Vec<(&'static str, Bandwidth)> = families
        .iter()
        .filter_map(|family| match family.status {
            CapStatus::Declared(cap) => Some((family.name, cap)),
            _ => None,
        })
        .collect();
    declared.sort_by_key(|(_, cap)| std::cmp::Reverse(*cap));
    let sum =
        Bandwidth::from_bits_per_sec(declared.iter().map(|(_, cap)| cap.bits_per_sec()).sum());
    if sum <= ceiling {
        Audit::Fits { sum, ceiling }
    } else {
        Audit::OverBudget {
            sum,
            ceiling,
            declared,
        }
    }
}

fn undeclared(families: &[Family]) -> Vec<&'static str> {
    families
        .iter()
        .filter(|family| matches!(family.status, CapStatus::Undeclared(_)))
        .map(|family| family.name)
        .collect()
}

#[test]
fn the_declared_caps_fit_the_budget_with_replication_headroom() {
    let budget = UploadBudget::default().sustained;
    let families = families();
    for family in &families {
        eprintln!("{:<72} {}", family.name, family.describe());
    }
    match audit(&families, budget, REPLICATION_HEADROOM_PCT) {
        Audit::Fits { sum, ceiling } => {
            eprintln!("declared caps sum to {sum}, ceiling {ceiling} of {budget}");
            // The hit cap is a real number, not a rounding artefact of the
            // witness share: it must be visible in the sum.
            assert!(hit_family_cap().bits_per_sec() > 0);
        }
        Audit::OverBudget {
            sum,
            ceiling,
            declared,
        } => panic!(
            "unsheddable caps sum to {sum}, over the {ceiling} left after \
             {REPLICATION_HEADROOM_PCT} % replication headroom: {declared:?}"
        ),
    }
}

#[test]
fn the_hit_cap_is_smaller_than_the_witness_share() {
    // The derivation's sanity check: 60 claims/s (the fire-rate invariant's
    // floor at D16's 60 Hz) times a *widest-case* claim and verdict — every
    // varint at its maximum, so this is a ceiling, roughly three times a
    // typical claim — must still sit under the witness lane's 20 %. The
    // witness lane carries the complete story of every tick; the hit lane
    // carries one shot at a time. If a wire change ever inverts that, this
    // is where it shows.
    let budget = UploadBudget::default().sustained;
    let cap = hit_family_cap();
    let pct = cap.bits_per_sec() * 100 / budget.bits_per_sec();
    eprintln!("hit family cap {cap} = {pct} % of {budget} (widest-case wire)");
    assert!(
        pct < WITNESS_LANE_SHARE_PCT,
        "hit cap {cap} is {pct} % of {budget}, at or above the witness lane's \
         {WITNESS_LANE_SHARE_PCT} %"
    );
}

#[test]
fn raising_a_cap_past_the_budget_fails_the_audit_by_name() {
    let budget = UploadBudget::default().sustained;
    let mut families = families();
    let hit = families
        .iter_mut()
        .find(|family| family.name == "hit claims and verdicts")
        .expect("the hit family is registered");
    hit.status = CapStatus::Declared(Bandwidth::from_bits_per_sec(budget.bits_per_sec() + 1));
    match audit(&families, budget, REPLICATION_HEADROOM_PCT) {
        Audit::OverBudget { declared, .. } => {
            assert_eq!(
                declared.first().map(|(name, _)| *name),
                Some("hit claims and verdicts"),
                "the offender is named first: {declared:?}"
            );
        }
        Audit::Fits { sum, ceiling } => panic!("a cap above the budget fitted: {sum} <= {ceiling}"),
    }
}

#[test]
fn every_unsheddable_family_without_a_cap_is_named() {
    // Pinned. Adding an unsheddable family without a cap changes this list
    // and fails here; giving one of these a cap means moving it to
    // `Declared` and removing it here — in the same change, by name.
    assert_eq!(
        undeclared(&families()),
        [
            "keyframes to witness-set links",
            "delivered inputs (cross-authority events on the replication stream)",
            "control: witness gap-repair responses",
            "control: handoff acks, handshakes, manifest deltas",
        ]
    );
}

#[test]
fn the_pathological_verdict_fan_in_is_reported_not_fitted() {
    // One gate per source, MAX_CLAIM_SOURCES of them, every one at the cap:
    // the verdict direction alone would exceed the budget. That is the bound
    // on a *malicious* fan-in, and it is stated here so the number is on the
    // record rather than discovered. An honest fan-in is the interest set at
    // weapon rates, twenty times under the cap per source.
    let cfg = PredictConfig::default();
    let cap = HitClaimCap::for_window(cfg.hit_window());
    let per_sec = cap.sustained_claims_per_second(cfg.tick_hz);
    let verdict = datagram_wire_bytes(encode_hit(&HitMsg::Verdict(widest_verdict())).len());
    let worst = Bandwidth::from_bits_per_sec(
        verdict * per_sec * 8 * orrery_authority::MAX_CLAIM_SOURCES as u64,
    );
    let budget = UploadBudget::default().sustained;
    eprintln!(
        "verdict fan-in from {} sources at the cap: {worst} against {budget}",
        orrery_authority::MAX_CLAIM_SOURCES
    );
    assert!(worst > budget, "if this ever fits, the note above is stale");
}
