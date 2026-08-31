//! Pins quantize-before-hash in `Executor::step_entity` (VC-7), issue #425.
//!
//! A7's mutation X-C swapped the `own.quantize()` / `state_hash(&own)` pair in
//! `step_entity` and every suite passed, because every in-tree `CoreState`
//! stores lattice integers and every step writes lattice points — the
//! executor's snap was a no-op on every fixture, so the ordering was pinned by
//! nothing. This ruleset is the counterexample: its state is a raw micrometre
//! integer, its `Quantized::quantize` genuinely rounds to the millimetre
//! lattice, and its step lands off-lattice on **every** tick, so the hash of
//! the raw post-step state and the hash of the quantized one are different
//! bytes and only one of them can be in `TickOutcome::state_hash`.
//!
//! The vacuity self-check is the point of the exercise: if a future edit puts
//! this fixture back on the lattice, the `quantized != raw` assertion fails
//! loudly instead of letting the pin pass while asserting nothing — which is
//! exactly how X-C survived the first time.

use std::collections::BTreeMap;

use orrery_core::executor::Executor;
use orrery_core::quantize::Quantized;
use orrery_core::rng::tick_rng;
use orrery_core::ruleset::{
    state_hash, CodecError, CoreCodec, OrderedInputs, Ruleset, StateView, StepOutput,
};
use orrery_protocol::{PersistId, RulesetId, Tick, UniverseSeed};

/// Micrometres per millimetre: the fixture's lattice quantum.
const UM_PER_MM: i64 = 1_000;

/// The per-tick displacement in micrometres. Deliberately **not** a multiple
/// of [`UM_PER_MM`]: starting from any lattice point, one step lands 567 µm
/// off-lattice, and because the executor snaps back to the lattice before the
/// next tick reads the state, every subsequent step is off-lattice again.
const STEP_UM: i64 = 1_234_567;

/// One entity's state: a position in raw micrometres, off-lattice after every
/// step until [`Quantized::quantize`] rounds it to whole millimetres.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    /// Position along one axis, in micrometres.
    pos_um: i64,
}

/// Round micrometres to the millimetre lattice, half away from zero — the same
/// tie rule `orrery_core::quantize::quantum` pins for the f64 lattice.
fn snap_um_to_mm_lattice(um: i64) -> i64 {
    let magnitude = (um.abs() + UM_PER_MM / 2) / UM_PER_MM * UM_PER_MM;
    magnitude * um.signum()
}

impl Quantized for Probe {
    fn quantize(&mut self) {
        self.pos_um = snap_um_to_mm_lattice(self.pos_um);
    }
}

impl CoreCodec for Probe {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pos_um.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CodecError("probe is 8 bytes"))?;
        Ok(Self {
            pos_um: i64::from_le_bytes(raw),
        })
    }
}

/// This fixture drives no inputs and emits no events; both channels are
/// uninhabited so the codec has nothing to encode.
#[derive(Debug, Clone)]
enum Never {}

impl CoreCodec for Never {
    fn encode(&self, _out: &mut Vec<u8>) {
        match *self {}
    }
    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Err(CodecError("never is uninhabited"))
    }
}

/// The off-lattice kernel: every step writes a state the lattice must move.
struct OffLattice;

impl Ruleset for OffLattice {
    type CoreState = Probe;
    type CoreInput = Never;
    type CoreEvent = Never;

    fn id(&self) -> RulesetId {
        RulesetId {
            version: 1,
            digest: [0xA5; 32],
        }
    }

    fn step(
        &self,
        view: &mut StateView<'_, Probe>,
        _inputs: &OrderedInputs<'_, Never>,
        _rng: &mut orrery_core::rng::TickRng,
    ) -> StepOutput<Never> {
        view.own_mut().pos_um += STEP_UM;
        StepOutput::default()
    }
}

const SEED: UniverseSeed = UniverseSeed([0x42; 32]);
const ENTITY: PersistId = PersistId(7);

/// Re-run the ruleset's step outside the executor to obtain the **raw**
/// post-step state — the value that exists after `Ruleset::step` returns and
/// before the executor's VC-7 snap. Under X-C's swap, this is the state whose
/// hash lands in `TickOutcome::state_hash`.
fn raw_post_step(pre: &Probe, tick: Tick) -> Probe {
    let mut own = pre.clone();
    let neighbors = BTreeMap::new();
    let observation_ticks = BTreeMap::new();
    let mut view = StateView::new(ENTITY, &mut own, &neighbors, &observation_ticks, tick, 0);
    let mut rng = tick_rng(SEED, ENTITY, tick);
    let output = OffLattice.step(&mut view, &OrderedInputs::new(&[]), &mut rng);
    assert!(output.events.is_empty());
    own
}

#[test]
fn the_claimed_hash_is_of_the_quantized_state_not_the_raw_one() {
    let mut executor = Executor::new(OffLattice, SEED);
    // 5 mm: a lattice point, so tick 0's raw/quantized split is produced by
    // the step alone, not smuggled in through the initial state. (`insert`
    // snaps anyway; starting on-lattice keeps the reconstruction below exact.)
    executor.insert(ENTITY, Probe { pos_um: 5_000 });

    for tick in 0..8u64 {
        // The state the executor will hand to `step` this tick.
        let pre = executor.state(ENTITY).expect("entity was inserted").clone();

        // The two candidate hash preimages: the raw post-step state, and the
        // same state after the VC-7 snap.
        let raw = raw_post_step(&pre, Tick::new(tick));
        let mut quantized = raw.clone();
        quantized.quantize();

        // Vacuity self-check, and the whole reason this file exists: the pin
        // below distinguishes nothing unless the fixture is genuinely
        // off-lattice after the step. If this fires, the fixture has gone
        // on-lattice and the pin has stopped pinning — fix the fixture, do
        // not weaken this assertion.
        assert_ne!(
            quantized, raw,
            "tick {tick}: the fixture's post-step state is on-lattice, so this \
             test can no longer distinguish hash-of-raw from hash-of-quantized \
             and would pass vacuously (the X-C failure mode)"
        );

        let outcome = executor
            .step_entity(ENTITY, Tick::new(tick), &[])
            .expect("entity is held by this executor");

        // The pin (VC-7): the claimed hash commits to the quantized state...
        assert_eq!(
            outcome.state_hash,
            state_hash(&quantized),
            "tick {tick}: TickOutcome::state_hash is not the hash of the \
             quantized post-step state — quantize-before-hash is broken"
        );
        // ...and therefore not to the raw one (X-C's exact swap would make it
        // this instead).
        assert_ne!(
            outcome.state_hash,
            state_hash(&raw),
            "tick {tick}: TickOutcome::state_hash equals the hash of the raw \
             (unquantized) post-step state — the X-C ordering"
        );

        // The quantized value is also what the next tick reads, which is what
        // keeps `raw_post_step`'s reconstruction in lockstep with the
        // executor for the rest of the loop.
        assert_eq!(
            executor.state(ENTITY),
            Some(&quantized),
            "tick {tick}: the stored state is not the quantized one"
        );
    }
}

#[test]
fn the_fixture_lattice_rounds_half_away_from_zero() {
    // The fixture's integer snap must follow the same tie rule as the core's
    // f64 lattice (`libm::round`, half away from zero), or the fixture would
    // be pinning an ordering against a lattice the core does not have.
    assert_eq!(snap_um_to_mm_lattice(500), 1_000);
    assert_eq!(snap_um_to_mm_lattice(-500), -1_000);
    assert_eq!(snap_um_to_mm_lattice(499), 0);
    assert_eq!(snap_um_to_mm_lattice(-499), 0);
    assert_eq!(snap_um_to_mm_lattice(1_234_567), 1_235_000);
    assert_eq!(snap_um_to_mm_lattice(0), 0);
}
