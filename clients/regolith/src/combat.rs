//! A read-only view of the ruleset's combat state, and the tracer tracks the
//! ruleset's own events describe.
//!
//! Nothing in this module decides anything. `lock_progress`, `lock_target`,
//! `hull`, `shield`, `cooldown` and a projectile's remaining `flight_ticks`
//! are all owned by `orrery_games::regolith` and are copied here verbatim for
//! drawing. `docs/15` §7 and #320 constraint 3 both land on the same rule: the
//! skin displays, it never computes, and it emits no orders.

use bevy::prelude::Resource;
use orrery_core::Executor;
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{LockBreakReason, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::regolith::weapon::{Weapon, WeaponKind};
use orrery_games::regolith::LOCK_ACQUISITION_TICKS;
use orrery_games::Regolith;
use orrery_protocol::PersistId;

/// Tick marks on the acquisition ring: one per held-trigger tick the ruleset
/// requires, so a full ring is exactly `LOCK_ACQUISITION_TICKS`.
pub const LOCK_RING_SEGMENTS: usize = LOCK_ACQUISITION_TICKS as usize;

/// How many tracers the skin can draw at once.
pub const TRACER_POOL: usize = 24;

/// Ticks a lock-break banner stays up after the ruleset reports the break.
pub const BREAK_BANNER_TICKS: u16 = 90;

/// The three states the design draws for a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPhase {
    /// No `lock_target`: the ring is gone.
    Idle,
    /// `lock_target` is set and `lock_progress < LOCK_ACQUISITION_TICKS`.
    Acquiring,
    /// `lock_progress` reached the threshold; the trigger is live.
    Locked,
}

impl LockPhase {
    /// The design's label above the reticle.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "NO LOCK",
            Self::Acquiring => "ACQUIRING",
            Self::Locked => "LOCKED",
        }
    }
}

/// Everything the lock readout draws, copied out of one [`Craft`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockView {
    /// `Craft::lock_target`.
    pub target: Option<PersistId>,
    /// `Craft::lock_progress`.
    pub progress: u16,
    /// `Craft::locks_acquired`.
    pub acquired: u32,
}

impl LockView {
    /// Copies the lock fields out of a craft. This is the only place the skin
    /// reads them, and it copies — it never recomputes.
    #[must_use]
    pub const fn of(craft: &Craft) -> Self {
        Self {
            target: craft.lock_target,
            progress: craft.lock_progress,
            acquired: craft.locks_acquired,
        }
    }

    /// Which of the design's three reticle states this is.
    #[must_use]
    pub const fn phase(self) -> LockPhase {
        match self.target {
            None => LockPhase::Idle,
            Some(_) if self.progress >= LOCK_ACQUISITION_TICKS => LockPhase::Locked,
            Some(_) => LockPhase::Acquiring,
        }
    }

    /// Tick marks lit on the acquisition ring.
    ///
    /// One per tick of `lock_progress`, saturating at a full ring. With no
    /// target there is nothing to acquire, so the ring is empty regardless of
    /// whatever stale progress a previous target left behind.
    #[must_use]
    pub fn segments_lit(self) -> usize {
        if self.target.is_none() {
            return 0;
        }
        (self.progress as usize).min(LOCK_RING_SEGMENTS)
    }

    /// The reticle's caption line, matching the design's two rows.
    #[must_use]
    pub fn caption(self) -> String {
        match self.phase() {
            LockPhase::Idle => "no target".to_owned(),
            LockPhase::Acquiring => format!(
                "{} / {} t · {:.2} s",
                self.progress,
                LOCK_ACQUISITION_TICKS,
                f64::from(self.progress) / f64::from(orrery_core::TICK_HZ)
            ),
            LockPhase::Locked => format!(
                "{LOCK_ACQUISITION_TICKS} / {LOCK_ACQUISITION_TICKS} t · lock #{}",
                self.acquired
            ),
        }
    }
}

/// One shot the ruleset says is still in the air.
///
/// Every field is lifted straight off an [`Outcome::DamageDealt`]. The skin
/// runs no ballistics of its own: if the ruleset stops emitting the event, the
/// tracer stops existing on the same tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    /// Who fired.
    pub attacker: PersistId,
    /// Who it is aimed at; also whose step re-emits the event each tick.
    pub target: PersistId,
    /// `attacker_pos` from the event — the muzzle, in the ruleset's lattice.
    pub origin: orrery_core::QPos,
    /// `attacker_weapon` from the event.
    pub weapon: WeaponKind,
    /// `flight_ticks` from the event: ticks of flight still to run.
    pub remaining: u16,
    /// Total flight, recovered from the first event of this shot.
    pub total: u16,
}

impl Track {
    /// Fraction of the flight already run, in `0.0..=1.0`.
    #[must_use]
    pub fn travelled(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        let flown = f32::from(self.total.saturating_sub(self.remaining));
        (flown / f32::from(self.total)).clamp(0.0, 1.0)
    }
}

/// The shots currently in the air, rebuilt from scratch every ruleset tick.
#[derive(Debug, Default, Resource)]
pub struct ProjectileTracks {
    tracks: Vec<Track>,
}

impl ProjectileTracks {
    /// Replaces the track set with the one this tick's events describe.
    ///
    /// The ruleset re-emits a `DamageDealt` with a decremented `flight_ticks`
    /// on every tick a shot is still travelling, so one tick of events *is*
    /// the complete set of shots in the air. Rebuilding rather than advancing
    /// is deliberate: there is no skin-side clock that could drift from the
    /// ruleset's, and a shot that resolved this tick simply has no event and
    /// so has no tracer.
    ///
    /// A muzzle event carries `flight_ticks: None` and is skipped, because the
    /// same shot reappears one tick later with a real remaining count and the
    /// identical `attacker_pos`. Matching a continuation to last tick's track
    /// by `remaining + 1` recovers `total` for the flown fraction.
    pub fn observe(&mut self, events: &[Outcome]) {
        let mut carried = Vec::with_capacity(events.len());
        let mut consumed = vec![false; self.tracks.len()];
        for event in events {
            let Outcome::DamageDealt {
                attacker,
                target,
                attacker_pos,
                attacker_weapon,
                flight_ticks: Some(remaining),
                ..
            } = event
            else {
                continue;
            };
            let prior = self.tracks.iter().enumerate().find_map(|(index, track)| {
                (!consumed[index]
                    && track.attacker == *attacker
                    && track.target == *target
                    && track.remaining == remaining.saturating_add(1))
                .then_some(index)
            });
            let total = match prior {
                Some(index) => {
                    consumed[index] = true;
                    self.tracks[index].total
                }
                None => remaining.saturating_add(1),
            };
            carried.push(Track {
                attacker: *attacker,
                target: *target,
                origin: *attacker_pos,
                weapon: *attacker_weapon,
                remaining: *remaining,
                total,
            });
        }
        self.tracks = carried;
    }

    /// The shots in the air, in event order.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The nearest-to-landing shot this entity fired, if any.
    #[must_use]
    pub fn own_shot(&self, attacker: PersistId) -> Option<Track> {
        self.tracks
            .iter()
            .filter(|track| track.attacker == attacker)
            .min_by_key(|track| track.remaining)
            .copied()
    }
}

/// A lock break the ruleset reported, held on screen long enough to read.
#[derive(Debug, Default, Resource)]
pub struct LockBreak {
    /// Why the ruleset dropped the lock, while the banner is up.
    pub reason: Option<LockBreakReason>,
    /// Ticks the banner still has to run.
    pub ticks_left: u16,
}

impl LockBreak {
    /// Raises the banner for every break the ruleset reported for `locker`.
    pub fn observe(&mut self, events: &[Outcome], locker: PersistId) {
        for event in events {
            if let Outcome::LockBroken {
                locker: who,
                reason,
                ..
            } = event
            {
                if *who == locker {
                    self.reason = Some(*reason);
                    self.ticks_left = BREAK_BANNER_TICKS;
                }
            }
        }
    }

    /// Ages the banner by one tick.
    pub fn age(&mut self) {
        self.ticks_left = self.ticks_left.saturating_sub(1);
        if self.ticks_left == 0 {
            self.reason = None;
        }
    }

    /// The banner line, empty when nothing broke recently.
    #[must_use]
    pub fn banner(&self) -> String {
        match self.reason {
            None => String::new(),
            Some(LockBreakReason::RangeExceeded) => "LOCK BROKEN · RANGE EXCEEDED".to_owned(),
            Some(LockBreakReason::TargetDestroyed) => "LOCK BROKEN · TARGET DESTROYED".to_owned(),
        }
    }
}

/// One craft's drawable numbers, all copied from its hashed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CraftView {
    /// Who this is.
    pub entity: PersistId,
    /// `Craft::archetype`.
    pub archetype: Archetype,
    /// `Craft::weapon`.
    pub weapon: WeaponKind,
    /// `Craft::hull`.
    pub hull: i32,
    /// `Craft::shield`.
    pub shield: i32,
    /// `Craft::cooldown`.
    pub cooldown: u16,
    /// `Craft::respawn_in`.
    pub respawn_in: u16,
    /// `Craft::shots`.
    pub shots: u32,
    /// Lattice position, for the range readout.
    pub pos: orrery_core::QPos,
    /// Lattice velocity, for the speed readout.
    pub vel: orrery_core::QVel,
    /// `Craft::score`, already derived by the ruleset.
    pub score: u64,
}

impl CraftView {
    /// Copies one craft.
    #[must_use]
    pub fn of(entity: PersistId, craft: &Craft) -> Self {
        Self {
            entity,
            archetype: craft.archetype,
            weapon: craft.weapon,
            hull: craft.hull,
            shield: craft.shield,
            cooldown: craft.cooldown,
            respawn_in: craft.respawn_in,
            shots: craft.shots,
            pos: craft.pos,
            vel: craft.vel,
            score: craft.score(),
        }
    }

    /// Hull ceiling published by the chassis.
    #[must_use]
    pub fn max_hull(&self) -> i32 {
        self.archetype.limits().max_hull
    }

    /// Shield ceiling published by the chassis.
    #[must_use]
    pub fn max_shield(&self) -> i32 {
        self.archetype.limits().max_shield
    }

    /// Speed ceiling published by the chassis, in metres per second.
    #[must_use]
    pub fn max_speed_ms(&self) -> f64 {
        self.archetype.limits().max_speed_mms as f64 / 1_000.0
    }

    /// Current speed, in metres per second.
    #[must_use]
    pub fn speed_ms(&self) -> f64 {
        let (x, y, z) = self.vel.to_metres_per_sec();
        (x * x + y * y + z * z).sqrt()
    }

    /// The equipped weapon's published table row.
    #[must_use]
    pub fn weapon_table(&self) -> Weapon {
        self.weapon.weapon()
    }

    /// The chassis name the HUD prints.
    #[must_use]
    pub const fn chassis_name(&self) -> &'static str {
        match self.archetype {
            Archetype::Interceptor => "INTERCEPTOR",
            Archetype::Cruiser => "CRUISER",
        }
    }
}

/// Where a target sits relative to the shooter's weapon envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeBand {
    /// Inside `optimal_mm`: no range penalty.
    Optimal,
    /// Between `optimal_mm` and `optimal_mm + falloff_mm`.
    Falloff,
    /// Past the falloff edge, where the ruleset raises `RangeExceeded`.
    Beyond,
}

impl RangeBand {
    /// The HUD's phrase for this band.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Optimal => "inside optimal",
            Self::Falloff => "in falloff",
            Self::Beyond => "beyond reach",
        }
    }

    /// Classifies a separation against a weapon's own published limits.
    ///
    /// This reads the weapon table and compares two numbers. It deliberately
    /// stops short of the hit roll: reproducing `hit_chance_ppm` in the skin
    /// would be a second implementation of a ruleset rule, and a second
    /// implementation is a second thing to be wrong.
    #[must_use]
    pub fn of(range_mm: i64, weapon: Weapon) -> Self {
        if range_mm <= weapon.optimal_mm {
            Self::Optimal
        } else if range_mm <= weapon.optimal_mm.saturating_add(weapon.falloff_mm) {
            Self::Falloff
        } else {
            Self::Beyond
        }
    }
}

/// The complete drawable picture of one tick, from one craft's seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Resource)]
pub struct CombatView {
    /// The craft this client flies, when the executor still holds one.
    pub own: Option<CraftView>,
    /// The lock fields of that craft.
    pub lock: LockView,
    /// The locked target's own state, when it is a craft this client can see.
    pub target: Option<CraftView>,
}

impl CombatView {
    /// Reads one tick out of the shared headless executor.
    ///
    /// Pure: it copies hashed fields into plain numbers and returns. It writes
    /// nothing back, so it cannot change which orders the pipeline emits.
    #[must_use]
    pub fn read(executor: &Executor<Regolith>, me: PersistId) -> Self {
        let own = craft_of(executor, me).map(|craft| CraftView::of(me, craft));
        let lock = craft_of(executor, me).map(LockView::of).unwrap_or_default();
        let target = lock.target.and_then(|target| {
            craft_of(executor, target).map(|craft| CraftView::of(target, craft))
        });
        Self { own, lock, target }
    }

    /// Straight-line separation to the locked target, in millimetres.
    #[must_use]
    pub fn range_mm(&self) -> Option<i64> {
        let own = self.own?;
        let target = self.target?;
        let (dx, dy, dz) = (
            i128::from(target.pos.x) - i128::from(own.pos.x),
            i128::from(target.pos.y) - i128::from(own.pos.y),
            i128::from(target.pos.z) - i128::from(own.pos.z),
        );
        let square = dx * dx + dy * dy + dz * dz;
        i64::try_from(integer_sqrt(square.unsigned_abs())).ok()
    }

    /// Which band the locked target sits in for the equipped weapon.
    #[must_use]
    pub fn band(&self) -> Option<RangeBand> {
        Some(RangeBand::of(self.range_mm()?, self.own?.weapon_table()))
    }
}

fn craft_of(executor: &Executor<Regolith>, entity: PersistId) -> Option<&Craft> {
    match executor.state(entity)? {
        RegolithState::Craft(craft) => Some(craft),
        _ => None,
    }
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut guess = value;
    let mut next = (guess + value / guess) / 2;
    while next < guess {
        guess = next;
        next = (guess + value / guess) / 2;
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::{QPos, QVel};

    fn craft() -> Craft {
        Craft::spawned(Archetype::Interceptor, QPos::from_metres(0.0, 0.0, 0.0), 0)
    }

    fn shot(attacker: u64, target: u64, remaining: Option<u16>) -> Outcome {
        Outcome::DamageDealt {
            attacker: PersistId::new(attacker),
            target: PersistId::new(target),
            amount: 11,
            attacker_pos: QPos::from_metres(10.0, 0.0, 20.0),
            attacker_vel: QVel::default(),
            attacker_weapon: WeaponKind::Stock,
            flight_ticks: remaining,
        }
    }

    #[test]
    fn lock_view_copies_every_acquisition_tick() {
        let mut craft = craft();
        craft.lock_target = Some(PersistId::new(2));
        for progress in 0..=LOCK_ACQUISITION_TICKS + 5 {
            craft.lock_progress = progress;
            let view = LockView::of(&craft);
            assert_eq!(view.progress, progress, "the view must copy lock_progress");
            assert_eq!(
                view.segments_lit(),
                (progress as usize).min(LOCK_RING_SEGMENTS),
                "the ring must light one mark per acquisition tick"
            );
            assert_eq!(
                view.phase(),
                if progress >= LOCK_ACQUISITION_TICKS {
                    LockPhase::Locked
                } else {
                    LockPhase::Acquiring
                }
            );
        }
    }

    #[test]
    fn no_target_means_no_ring() {
        let mut craft = craft();
        craft.lock_target = None;
        craft.lock_progress = 17;
        let view = LockView::of(&craft);
        assert_eq!(view.phase(), LockPhase::Idle);
        assert_eq!(view.segments_lit(), 0);
    }

    #[test]
    fn a_muzzle_event_alone_draws_nothing() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, None)]);
        assert!(
            tracks.tracks().is_empty(),
            "the muzzle event carries no flight time; the continuation does"
        );
    }

    #[test]
    fn a_tracer_walks_the_rulesets_own_flight_ticks() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, None)]);
        tracks.observe(&[shot(1, 2, Some(4))]);
        assert_eq!(tracks.tracks().len(), 1);
        assert_eq!(tracks.tracks()[0].total, 5);
        assert_eq!(tracks.tracks()[0].remaining, 4);
        assert!((tracks.tracks()[0].travelled() - 0.2).abs() < 1e-6);
        for remaining in [3u16, 2, 1] {
            tracks.observe(&[shot(1, 2, Some(remaining))]);
            let track = tracks.tracks()[0];
            assert_eq!(track.total, 5, "total must survive the whole flight");
            assert_eq!(track.remaining, remaining);
            let expected = (5.0 - f32::from(remaining)) / 5.0;
            assert!((track.travelled() - expected).abs() < 1e-6);
        }
        // Resolution emits no event, so the tracer must vanish with it.
        tracks.observe(&[]);
        assert!(tracks.tracks().is_empty());
    }

    #[test]
    fn concurrent_shots_keep_separate_tracks() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, Some(6)), shot(1, 2, Some(3))]);
        assert_eq!(tracks.tracks().len(), 2);
        tracks.observe(&[shot(1, 2, Some(5)), shot(1, 2, Some(2))]);
        let remaining: Vec<_> = tracks.tracks().iter().map(|t| t.remaining).collect();
        assert_eq!(remaining, vec![5, 2]);
        assert!(tracks.tracks().iter().all(|t| t.total == 7 || t.total == 4));
        assert_eq!(
            tracks.own_shot(PersistId::new(1)).map(|t| t.remaining),
            Some(2)
        );
    }

    #[test]
    fn the_break_banner_names_the_rulesets_reason() {
        let mut banner = LockBreak::default();
        assert!(banner.banner().is_empty());
        banner.observe(
            &[Outcome::LockBroken {
                locker: PersistId::new(1),
                target: PersistId::new(2),
                reason: LockBreakReason::RangeExceeded,
            }],
            PersistId::new(1),
        );
        assert!(banner.banner().contains("RANGE"));
        for _ in 0..BREAK_BANNER_TICKS {
            banner.age();
        }
        assert!(banner.banner().is_empty(), "the banner must expire");
    }

    #[test]
    fn a_break_reported_to_someone_else_is_not_mine() {
        let mut banner = LockBreak::default();
        banner.observe(
            &[Outcome::LockBroken {
                locker: PersistId::new(9),
                target: PersistId::new(2),
                reason: LockBreakReason::TargetDestroyed,
            }],
            PersistId::new(1),
        );
        assert!(banner.banner().is_empty());
    }

    #[test]
    fn range_bands_follow_the_published_weapon_table() {
        let stock = WeaponKind::Stock.weapon();
        assert_eq!(RangeBand::of(0, stock), RangeBand::Optimal);
        assert_eq!(RangeBand::of(stock.optimal_mm, stock), RangeBand::Optimal);
        assert_eq!(
            RangeBand::of(stock.optimal_mm + 1, stock),
            RangeBand::Falloff
        );
        assert_eq!(
            RangeBand::of(stock.optimal_mm + stock.falloff_mm, stock),
            RangeBand::Falloff
        );
        assert_eq!(
            RangeBand::of(stock.optimal_mm + stock.falloff_mm + 1, stock),
            RangeBand::Beyond
        );
    }

    #[test]
    fn integer_sqrt_is_exact_on_squares() {
        for value in [0u128, 1, 4, 9, 144, 1_000_000, 250_000 * 250_000] {
            let root = integer_sqrt(value);
            assert!(root * root <= value && (root + 1) * (root + 1) > value);
        }
    }
}
