//! The witness lane against its share of the peer upload budget (docs/03 §5.3a).
//!
//! P1's 32-peer swarm is where this was found and is where it is measured; what
//! is checked here is the arithmetic that swarm ran on, so a change to the wire
//! types or to the cadence rule fails in the workspace suite rather than three
//! minutes into a simulated hour.
//!
//! The numbers these tests defend: at the D16 defaults (1 Mbps peer upload, 60
//! Hz sim, 2 Hz claims, ≤ 7 witness links) a frame covers **10 ticks**, and the
//! lane measures **190 kb/s per peer** on the 32-peer swarm — inside its 200
//! kb/s share, which one frame per 20 Hz send was never inside at 384 kb/s.

use orrery_protocol::channels::encode_witness;
use orrery_protocol::{
    ChainHash, EntitySlice, FrameHead, InputRecord, LogFrame, PersistId, RecordSource, RollingHead,
    RulesetId, Signature, Tick, WitnessMsg,
};
use orrery_witness::plugin::{
    frame_interval_ticks, CLAIM_WIRE_BYTES, FRAME_FIXED_WIRE_BYTES, FRAME_TICK_WIRE_BYTES,
    MAX_WITNESS_LINKS, WITNESS_LANE_SHARE_PCT,
};

/// D16's peer upload budget.
const BUDGET_BITS: u64 = 1_000_000;
/// D16's sim tick rate.
const TICK_HZ: u64 = 60;
/// docs/06 §6's 2 Hz claim cadence, in ticks.
const CLAIM_EVERY: u64 = 30;
/// `orrery_net::budget::DATAGRAM_OVERHEAD_BYTES` — IP+UDP 28 B, QUIC ≈ 32 B.
const DATAGRAM_OVERHEAD: u64 = 60;

/// A one-entity frame covering `ticks`, shaped like what an authority streaming
/// its own player actually sends: one `OwnPlayer` record per tick carrying a
/// canonical thrust command (13 B under the reference ruleset).
fn frame_wire_bytes(ticks: u16) -> u64 {
    let entity = PersistId::new(1);
    let records: Vec<InputRecord> = (0..ticks)
        .map(|offset| InputRecord {
            tick_off: offset,
            seq: 0,
            source: RecordSource::OwnPlayer {
                input_seq: u32::from(offset),
            },
            payload: bytes::Bytes::from(vec![0u8; 13]),
        })
        .collect();
    let frame = LogFrame {
        ruleset: RulesetId {
            version: 1,
            digest: [7; 32],
        },
        first_tick: Tick::new(180_000),
        tick_count: ticks,
        entities: vec![EntitySlice {
            entity,
            chain_epoch: 0,
            prev_head: RollingHead([1; 8]),
            records,
            head: RollingHead([2; 8]),
        }],
        sig: Signature::from_bytes(&[3; 64]),
    };
    let heads = vec![FrameHead {
        entity,
        prev_head: ChainHash([4; 32]),
        head: ChainHash([5; 32]),
    }];
    encode_witness(&WitnessMsg::Frame { frame, heads }).len() as u64 + DATAGRAM_OVERHEAD
}

/// The cadence is chosen by arithmetic over a frame's wire cost. If a wire type
/// grows and the constants do not, every cadence this crate derives is quietly
/// wrong and the lane silently reclaims the budget it was moved out of.
#[test]
fn a_frame_costs_what_the_cadence_arithmetic_assumes() {
    // The fixed part — signature, ruleset digest, head pair, framing, datagram
    // overhead — is what the cadence divides down, so it is the number that
    // matters and it must not be *under*-stated.
    let fixed = frame_wire_bytes(0);
    assert!(
        fixed <= FRAME_FIXED_WIRE_BYTES,
        "a records-free frame costs {fixed} wire bytes, over the assumed \
         {FRAME_FIXED_WIRE_BYTES}: the derived cadence no longer fits the share"
    );

    // The per-tick part is the floor no cadence can go below.
    let per_tick = (frame_wire_bytes(10) - fixed) / 10;
    assert!(
        per_tick <= FRAME_TICK_WIRE_BYTES,
        "one tick of input records costs {per_tick} wire bytes, over the \
         assumed {FRAME_TICK_WIRE_BYTES}"
    );

    // And the two together must still describe a real frame, not bound it so
    // loosely that the arithmetic is meaningless.
    let modelled = FRAME_FIXED_WIRE_BYTES + 10 * FRAME_TICK_WIRE_BYTES;
    let actual = frame_wire_bytes(10);
    assert!(
        modelled <= actual + actual / 4,
        "the model ({modelled} B) has drifted far above a real 10-tick frame \
         ({actual} B); the lane is being budgeted for traffic nobody sends"
    );
}

/// The cadence exists to hold the lane inside its share. If it stops doing
/// that, the peer goes over its 1 Mbps ceiling and the backstop starts shedding
/// — which is how a witness stops watching and reports nothing.
#[test]
fn seven_links_at_the_derived_cadence_stay_inside_the_lane_share() {
    let ticks = u64::from(frame_interval_ticks(
        BUDGET_BITS,
        MAX_WITNESS_LINKS,
        TICK_HZ,
        CLAIM_EVERY,
    ));

    let per_link_bytes_per_sec = (TICK_HZ / ticks)
        * (FRAME_FIXED_WIRE_BYTES + ticks * FRAME_TICK_WIRE_BYTES)
        + (TICK_HZ / CLAIM_EVERY) * CLAIM_WIRE_BYTES;
    let lane_bits = per_link_bytes_per_sec * MAX_WITNESS_LINKS as u64 * 8;
    let allowance = BUDGET_BITS * WITNESS_LANE_SHARE_PCT / 100;

    assert!(
        lane_bits <= allowance,
        "the lane wants {lane_bits} bit/s across {MAX_WITNESS_LINKS} links, \
         over its {allowance} bit/s share"
    );
    // And it is not so far under that the cadence is costing detection latency
    // for nothing: a cadence one step coarser should be the only slack left.
    assert!(
        lane_bits * 2 > allowance,
        "the lane uses {lane_bits} of {allowance} bit/s — the cadence is far \
         coarser than the share requires, which is latency bought with nothing"
    );
}

/// docs/07-witnessing.md §3 requires an adjudication window to end at a claim
/// tick. A cadence coprime with the claim interval puts most claims mid-frame,
/// where a witness holding a partial fold defers instead of judging — which
/// shows up as observation coverage falling, not as an error.
#[test]
fn the_derived_cadence_lands_frame_boundaries_on_claim_ticks() {
    for links in 1..=MAX_WITNESS_LINKS {
        let ticks = u64::from(frame_interval_ticks(
            BUDGET_BITS,
            links,
            TICK_HZ,
            CLAIM_EVERY,
        ));
        assert!(
            CLAIM_EVERY.is_multiple_of(ticks),
            "at {links} links the cadence is {ticks} ticks, which does not \
             divide the {CLAIM_EVERY}-tick claim interval"
        );
    }
}

/// Fewer links buy a finer cadence and more links pay for it. If that ever
/// inverts, the derivation has stopped being about bandwidth.
#[test]
fn more_witness_links_buy_a_coarser_cadence_not_a_finer_one() {
    let mut previous = 0u16;
    for links in 1..=MAX_WITNESS_LINKS {
        let ticks = frame_interval_ticks(BUDGET_BITS, links, TICK_HZ, CLAIM_EVERY);
        assert!(
            ticks >= previous,
            "{links} links derived {ticks} ticks, finer than the {previous} \
             ticks {} links derived",
            links - 1
        );
        previous = ticks;
    }
}

/// A budget too small for the per-tick records alone cannot be fixed by
/// cadence, and must not answer with a zero-tick interval that would divide by
/// nothing downstream.
#[test]
fn a_budget_too_small_for_the_records_still_names_a_usable_cadence() {
    let ticks = frame_interval_ticks(64_000, MAX_WITNESS_LINKS, TICK_HZ, CLAIM_EVERY);
    assert_eq!(u64::from(ticks), CLAIM_EVERY);
    assert_eq!(frame_interval_ticks(0, 0, TICK_HZ, 0), 1);
}
