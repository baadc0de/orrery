//! The combat HUD: a world-space lock reticle, weapon range rings, tracers,
//! and the Bevy UI panels the design lays out around them.
//!
//! Every system here is a pure function of [`CombatView`], [`ProjectileTracks`]
//! and [`LockBreak`], which are themselves copies of ruleset state. None of
//! them writes to the executor, produces an order, or feeds a collision shape.

use bevy::prelude::*;
use orrery_games::regolith::weapon::Weapon;
use orrery_games::regolith::LOCK_ACQUISITION_TICKS;
use orrery_protocol::PersistId;

/// The reticle's tick marks, excluded from the sibling overlay queries.
type SegmentQuery<'w, 's> = Query<
    'w,
    's,
    (&'static LockSegment, &'static mut Visibility),
    (
        Without<LockReticle>,
        Without<LockBrackets>,
        Without<LockGlow>,
    ),
>;
/// The bracket parent, likewise disjoint.
type BracketQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Transform, &'static mut Visibility),
    (With<LockBrackets>, Without<LockReticle>, Without<LockGlow>),
>;
/// The impact burst's own transform, visibility and material handle. Named
/// because the tuple is past clippy's complexity bar inline.
type FlashQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Transform,
        &'static mut Visibility,
        Option<&'static MeshMaterial3d<StandardMaterial>>,
    ),
    With<ImpactFlash>,
>;
/// The impact marker ring, disjoint from the burst it points at.
type MarkerQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Transform, &'static mut Visibility),
    (With<ImpactMarker>, Without<ImpactFlash>),
>;
/// The impact burst, read-only: what [`sync_impact_flash`] left on it.
///
/// The capture in `crate::capture_impact_geometry` reads the transform the
/// system wrote rather than recomputing the size beside it, which is the whole
/// point of measuring in the render world.
pub type FlashReadQuery<'w, 's> =
    Query<'w, 's, (&'static Transform, &'static Visibility), With<ImpactFlash>>;
/// The marker ring, read-only and disjoint from the burst.
pub type MarkerReadQuery<'w, 's> = Query<
    'w,
    's,
    (&'static Transform, &'static Visibility),
    (With<ImpactMarker>, Without<ImpactFlash>),
>;
/// The glow disc, likewise disjoint.
type GlowQuery<'w, 's> = Query<
    'w,
    's,
    &'static mut Visibility,
    (With<LockGlow>, Without<LockReticle>, Without<LockBrackets>),
>;

use orrery_games::regolith::order::ShotResult;

use crate::combat::{
    CombatView, CraftView, HitBand, LockBreak, LockPhase, ProjectileTracks, RangeBand, ShotCue,
    ShotFeedback, LOCK_RING_SEGMENTS, TRACER_POOL,
};

/// Accent: "this one is mine". The design gives the accent hue to the player's
/// craft, lock, rings and tracers, and leaves everything else on the neutral
/// ramp so the picture still separates without colour.
pub const ACCENT: Color = Color::srgb(0.569, 0.518, 0.851);
/// A brighter accent for filled gauges and the locked ring.
pub const ACCENT_BRIGHT: Color = Color::srgb(0.710, 0.671, 0.988);
/// The palest accent, for headings.
pub const ACCENT_PALE: Color = Color::srgb(0.824, 0.808, 0.992);
/// Brightest neutral: live numbers.
pub const INK: Color = Color::srgb(0.914, 0.914, 0.929);
/// Ordinary neutral: values.
pub const MUTED: Color = Color::srgb(0.698, 0.714, 0.792);
/// Dim neutral: labels.
pub const DIM: Color = Color::srgb(0.459, 0.475, 0.549);
/// Faintest neutral: spent values.
pub const FAINT: Color = Color::srgb(0.349, 0.365, 0.424);
/// The unfilled part of a gauge.
pub const GAUGE_TRACK: Color = Color::srgb(0.161, 0.169, 0.192);
/// Panel fill.
pub const PANEL: Color = Color::srgba(0.086, 0.094, 0.149, 0.86);
/// Tracer white.
pub const TRACER: Color = Color::srgb(0.961, 0.957, 1.0);
/// Mining lock amber, distinct from the ship-lock accent.
pub const MINING_AMBER: Color = Color::srgb(0.95, 0.62, 0.22);
/// Confirmed-impact orange.
///
/// Hotter and redder than [`MINING_AMBER`] so the two never read as the same
/// event: amber says "this is a rock, and mining it is a lock away", orange
/// says "the ruleset has adjudicated damage". Both sit off the accent hue, so
/// neither can be confused with the player's own accent furniture, and the
/// burst is emissive-unlit rather than the reserved exhaust glow finish.
pub const IMPACT_ORANGE: Color = Color::srgb(1.0, 0.45, 0.10);

/// The burst's drawn radius at its birth and at its death, as multiples of the
/// **target's own ruleset radius**.
///
/// It opens fast and keeps growing while fading, which is what makes a small
/// sphere read as a burst rather than as a lamp switching on. The multiplicand
/// changed in #531: it used to be the flash mesh's own radius scaled to hold a
/// constant *apparent* size, which meant the same adjudicated hit was drawn 27
/// times larger in world terms at 4 km than at 150 m. Camera height is a fact
/// about the observer, not about the event.
pub const IMPACT_BURST_START_SCALE: f32 = 0.55;
/// See [`IMPACT_BURST_START_SCALE`].
pub const IMPACT_BURST_END_SCALE: f32 = 2.3;

/// The flash mesh's authored radius in metres, at scale one.
pub const IMPACT_FLASH_MESH_RADIUS_M: f32 = RETICLE_RADIUS_M * 0.12;

/// The impact marker ring's radius in metres at the default zoom.
///
/// The ring is the one part of the impact that holds a constant *apparent*
/// size, the way the lock reticle does, because it is a pointer rather than a
/// measurement — see [`sync_impact_flash`]. Sized to about what the old burst
/// occupied at the default zoom, so the event is no harder to spot than it was
/// before #531.
pub const IMPACT_MARKER_RADIUS_M: f32 = RETICLE_RADIUS_M * 0.30;

/// The marker ring's tube radius as a fraction of its own radius.
///
/// The camera's vertical field of view is [`crate::CAMERA_FOV_Y`], so on a
/// 1080-line window the visible world height is `2·h·tan(FOV/2)` metres for a
/// camera height `h`, and the ring's own radius is
/// `IMPACT_MARKER_RADIUS_M · h / CAMERA_DEFAULT_HEIGHT_M`. Both are linear in
/// `h`, so the ring's apparent thickness is the same at every zoom — about two
/// and a half pixels at this ratio, which is a hairline that renders rather
/// than a hairline that does not.
pub const IMPACT_MARKER_TUBE_RATIO: f32 = 0.06;

/// The radius the burst falls back to when the target's state is already gone.
///
/// A hit that destroyed its target retires the replica, so the ruleset radius
/// is no longer readable. The smallest chassis in the game is the most
/// conservative thing left to claim.
#[must_use]
pub fn fallback_target_radius_m() -> f32 {
    orrery_games::regolith::archetype::Archetype::Interceptor
        .limits()
        .radius_mm as f32
        / 1_000.0
}

/// The target's own radius in metres, as the ruleset states it.
///
/// This is the same `radius_mm` `projectile_resolution` adds to the weapon's
/// reach when it decides whether a shot connects
/// (`crates/orrery_games/src/regolith/mod.rs:1055`), so the drawn burst spans
/// the ruleset's own target rather than a size the skin picked.
#[must_use]
pub fn target_radius_m(
    executor: &orrery_core::Executor<orrery_games::Regolith>,
    entity: crate::PersistId,
) -> f32 {
    use orrery_games::regolith::state::RegolithState;
    let radius_mm = match executor.state(entity) {
        Some(RegolithState::Craft(craft)) => craft.archetype.limits().radius_mm,
        Some(RegolithState::Rock(rock)) => rock.tier.limits().radius_mm,
        _ => return fallback_target_radius_m(),
    };
    radius_mm as f32 / 1_000.0
}

/// Whether the constant-apparent-size marker ring is drawn, and how big.
///
/// Returns the ring's world radius, or `None` when the burst itself is already
/// at least as large on screen as the ring would be — at that point the
/// pointer is redundant and drawing it would just add a second circle around
/// an event you can already see.
#[must_use]
pub fn impact_marker_radius_m(burst_radius_m: f32, glyph_scale: f32) -> Option<f32> {
    let marker = IMPACT_MARKER_RADIUS_M * glyph_scale;
    (marker > burst_radius_m).then_some(marker)
}

/// The pool cuboid's length along its nose axis at scale one, in metres.
pub const TRACER_MESH_LENGTH_M: f32 = 18.0;

/// How many ruleset ticks of flight the tracer's persistence trail covers.
///
/// #383: "the tracers look good but are actually a bit too fast." The fix is
/// *apparent* duration, never actual duration — `flight_ticks` derives from
/// the weapon table's `projectile_speed_mms`, and touching that is a balance
/// change wearing a legibility fix's clothes. Instead each tracer is drawn as
/// a streak spanning where the shot was over the most recent
/// [`TRACER_PERSISTENCE_TICKS`] ticks — a motion-blur window of
/// `12 / TICK_HZ = 0.2 s`, roughly how long the eye integrates a moving
/// light. A stock round covers 60 m in that window, a Heavy round 36 m:
/// every weapon's streak then reads as "this far per blink", which is exactly
/// the speed cue the owner asked to slow down.
///
/// The trail is history, not ballistics: it renders points on the same
/// muzzle→target line the event already defines, so the skin adds no physics
/// the ruleset does not state.
pub const TRACER_PERSISTENCE_TICKS: u16 = 12;

/// Floor on a tracer streak's drawn length, in metres.
///
/// The instant a shot leaves the muzzle its flown path is shorter than any
/// persistence window, which would scale the mesh to zero (and Bevy warns on
/// singular scales). The streak keeps this minimum and grows *backwards* from
/// the head, so the extra length can only ever cover ground the shot has
/// already crossed or empty corridor behind it — never lead the position the
/// ruleset reports.
pub const TRACER_MIN_SPAN_M: f32 = 8.0;

/// Head and tail of a tracer streak, as fractions of the muzzle→target line.
///
/// `flown` ticks have run of a `total`-tick flight. The head sits at the
/// ruleset's own flown fraction; the tail lags [`TRACER_PERSISTENCE_TICKS`]
/// behind it, clamped at the muzzle — early in a flight the whole flown path
/// is lit, and once the flight outlasts the window the streak holds a fixed
/// length while travelling. Both outputs are in `0.0..=1.0` with
/// `tail <= head`.
#[must_use]
pub fn streak_fractions(flown: u16, total: u16) -> (f32, f32) {
    if total == 0 {
        return (1.0, 1.0);
    }
    let head = flown.min(total);
    let tail = flown.saturating_sub(TRACER_PERSISTENCE_TICKS).min(total);
    (
        f32::from(head) / f32::from(total),
        f32::from(tail) / f32::from(total),
    )
}

/// Placement of one tracer streak this frame.
///
/// Returns the entity translation and the streak's length in metres. The
/// leading edge lands exactly on the ruleset's head point — the streak is
/// centred half a length back along the line — so flooring the length at
/// [`TRACER_MIN_SPAN_M`] can stretch the trail but never push the visible
/// front past where the event says the shot is.
#[must_use]
pub fn tracer_streak(
    muzzle: Vec3,
    destination: Vec3,
    head_fraction: f32,
    tail_fraction: f32,
) -> (Vec3, f32) {
    let along = destination - muzzle;
    let direction = along.normalize_or_zero();
    let span = (along.length() * (head_fraction - tail_fraction)).max(TRACER_MIN_SPAN_M);
    let centre = muzzle + along * head_fraction - direction * (span / 2.0);
    (centre, span)
}

/// Reticle ring radius in world metres.
///
/// The design draws the ring at 78 px on a plan view scaled 1.6 px to the
/// metre, which is 49 m; every other reticle dimension below is a ratio of
/// that same 78 px, so the whole assembly keeps the design's proportions at
/// world scale. A scaled cruiser is about 44 m nose to tail, so the ring
/// clears it.
pub const RETICLE_RADIUS_M: f32 = 48.0;

/// Height above the deck at which a firing-arc fan is drawn, craft-local
/// metres. Enough to clear the hull plate's own lift without lifting the arc
/// out of the plane the duel happens in.
pub const FIRING_ARC_LIFT_M: f32 = 0.02;

/// How much of the accent a firing-arc fan keeps. The arc is a persistent hull
/// marking, not a transient readout: it has to be legible without competing with the reticle, the
/// range rings or a tracer.
pub const FIRING_ARC_ALPHA: f32 = 0.14;

/// The flat, unlit, translucent material one firing-arc fan wears.
#[must_use]
pub fn firing_arc_material(seat: crate::craft::Seat, accent: Color) -> StandardMaterial {
    let tint = match seat {
        crate::craft::Seat::Player => accent,
        crate::craft::Seat::Bot => MUTED,
    }
    .with_alpha(FIRING_ARC_ALPHA);
    StandardMaterial {
        base_color: tint,
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        double_sided: true,
        ..Default::default()
    }
}

/// Marks the reticle root, which is moved onto the locked target each frame.
#[derive(Component)]
pub struct LockReticle;

/// One acquisition tick mark. `0` is the mark at the top of the screen.
#[derive(Component)]
pub struct LockSegment(pub usize);

/// The four corner brackets, as one parent so the lock snap can pull them in.
#[derive(Component)]
pub struct LockBrackets;

/// The soft disc that lands when the lock closes.
#[derive(Component)]
pub struct LockGlow;

#[derive(Resource)]
pub(crate) struct LockMaterials {
    lit: Handle<StandardMaterial>,
    glow: Handle<StandardMaterial>,
}

/// A weapon envelope ring drawn around the player's own craft.
#[derive(Component)]
pub struct RangeRing {
    /// True for the solid `optimal_mm` ring, false for the dashed falloff edge.
    pub optimal: bool,
}

/// The ruleset's pickup reach, drawn around the player's own craft.
///
/// True scale, in metres, like the weapon envelope rings and unlike the lock
/// reticle: it *means* 25 m, so holding it to an apparent screen size would
/// state a reach the ruleset does not have. What keeps reach legible at the
/// 4 km zoom extreme, where 25 m is under a pixel, is the panel's own
/// [`Readout::PickupReach`] line, which prints both numbers.
#[derive(Component)]
pub struct GrabReachRing;

/// The reach ring's own material, so its tint can follow the ruleset's
/// claimability predicate without touching any other overlay.
#[derive(Resource)]
pub(crate) struct GrabMaterials {
    ring: Handle<StandardMaterial>,
}

/// One slot in the tracer pool.
#[derive(Component)]
pub struct Tracer(pub usize);

/// The world-space burst drawn on a shot's target while an arrival cue lives.
#[derive(Component)]
pub struct ImpactFlash;

/// The constant-apparent-size ring that points at a confirmed impact.
#[derive(Component)]
pub struct ImpactMarker;

/// A gauge's filled bar. The system sets its width.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gauge {
    /// Own hull against the chassis ceiling.
    OwnHull,
    /// Own shield against the chassis ceiling.
    OwnShield,
    /// Weapon cooldown remaining against the weapon's cycle.
    Cooldown,
    /// Locked target's hull.
    TargetHull,
    /// Locked target's shield.
    TargetShield,
}

/// A text line the HUD refreshes.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Readout {
    /// Own chassis name and id.
    OwnTitle,
    /// Own hull numerator.
    OwnHull,
    /// Own shield numerator.
    OwnShield,
    /// Own speed and score.
    OwnVitals,
    /// Equipped weapon name.
    WeaponName,
    /// Damage, rolls and cycle.
    WeaponSpec,
    /// Cooldown ticks remaining.
    WeaponCooldown,
    /// Optimal, falloff, projectile speed, tracking.
    WeaponEnvelope,
    /// `NO LOCK` / `ACQUIRING` / `LOCKED`.
    LockLabel,
    /// The acquisition counter under the label.
    LockCaption,
    /// The locked target's chassis and id.
    TargetTitle,
    /// The locked target's hull numerator.
    TargetHull,
    /// The locked target's shield numerator.
    TargetShield,
    /// Range, band and time of flight.
    TargetRelation,
    /// The qualitative hit-chance band beside the locked target.
    HitBandLine,
    /// The lock-break banner.
    BreakBanner,
    /// The shot-result cue line: provisional impact, then hit, miss, or refusal.
    ShotResult,
    /// The nearest live pickup's separation against the ruleset's reach, or
    /// the statement of how pickups are collected when none is in view.
    PickupReach,
    /// Whether the island tether is acting on this craft, and how hard (#955).
    TetherState,
    /// The announced bloom site: the campaign's own reason to stay (#955).
    BloomBeacon,
}

fn label(text: &str) -> impl Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(10.0),
        TextColor(DIM),
    )
}

fn value(readout: Readout, size: f32, color: Color) -> impl Bundle {
    (
        Text::new("-"),
        TextFont::from_font_size(size),
        TextColor(color),
        readout,
    )
}

fn panel(width: f32) -> Node {
    Node {
        width: Val::Px(width),
        flex_direction: FlexDirection::Column,
        row_gap: Val::Px(5.6),
        padding: UiRect::all(Val::Px(12.0)),
        border_radius: BorderRadius::all(Val::Px(8.0)),
        ..Default::default()
    }
}

fn gauge_row(name: &str, gauge: Gauge, readout: Readout) -> impl Bundle {
    (
        Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(8.0),
            ..Default::default()
        },
        children![
            (
                Node {
                    width: Val::Px(46.0),
                    ..Default::default()
                },
                children![label(name)]
            ),
            (
                Node {
                    flex_grow: 1.0,
                    height: Val::Px(8.0),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    overflow: Overflow::clip(),
                    ..Default::default()
                },
                BackgroundColor(GAUGE_TRACK),
                children![(
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(100.0),
                        ..Default::default()
                    },
                    BackgroundColor(ACCENT_BRIGHT),
                    gauge,
                )]
            ),
            (
                Node {
                    width: Val::Px(64.0),
                    ..Default::default()
                },
                children![value(readout, 11.0, MUTED)]
            ),
        ],
    )
}

/// Spawns every UI panel the design places around the plan view.
pub fn spawn_hud(commands: &mut Commands) {
    // Bottom left: the weapon panel and the own-craft panel, side by side.
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(22.0),
            bottom: Val::Px(22.0),
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(11.0),
            ..Default::default()
        },
        children![
            (
                panel(340.0),
                BackgroundColor(PANEL),
                children![
                    value(Readout::WeaponName, 16.0, ACCENT_PALE),
                    value(Readout::WeaponSpec, 11.0, DIM),
                    gauge_row("COOLDOWN", Gauge::Cooldown, Readout::WeaponCooldown),
                    value(Readout::WeaponEnvelope, 11.0, MUTED),
                ]
            ),
            (
                panel(268.0),
                BackgroundColor(PANEL),
                children![
                    value(Readout::OwnTitle, 13.0, ACCENT_BRIGHT),
                    gauge_row("HULL", Gauge::OwnHull, Readout::OwnHull),
                    gauge_row("SHIELD", Gauge::OwnShield, Readout::OwnShield),
                    value(Readout::OwnVitals, 11.0, DIM),
                    value(Readout::PickupReach, 11.0, MUTED),
                    // #955's two anchor lines sit on the *own* panel, beside
                    // speed and score, because both are facts about this
                    // pilot's own position rather than about a target.
                    value(Readout::TetherState, 11.0, MUTED),
                    value(Readout::BloomBeacon, 11.0, MINING_AMBER),
                ]
            ),
        ],
    ));

    // Right: the lock and target panel.
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(22.0),
            top: Val::Px(22.0),
            flex_direction: FlexDirection::Column,
            ..Default::default()
        },
        children![(
            panel(330.0),
            BackgroundColor(PANEL),
            children![
                value(Readout::LockLabel, 16.0, ACCENT_PALE),
                value(Readout::LockCaption, 11.0, DIM),
                value(Readout::TargetTitle, 13.0, MUTED),
                gauge_row("HULL", Gauge::TargetHull, Readout::TargetHull),
                gauge_row("SHIELD", Gauge::TargetShield, Readout::TargetShield),
                value(Readout::TargetRelation, 11.0, MUTED),
                value(Readout::HitBandLine, 13.0, MUTED),
                value(Readout::BreakBanner, 12.0, Color::srgb(0.95, 0.62, 0.45)),
                value(Readout::ShotResult, 13.0, MUTED),
            ]
        )],
    ));
}

/// Spawns the world-space reticle, range rings and tracer pool.
pub fn spawn_world_overlay(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let lit = materials.add(StandardMaterial {
        base_color: ACCENT_BRIGHT,
        emissive: LinearRgba::from(ACCENT) * 3.0,
        unlit: true,
        ..Default::default()
    });
    let glow = materials.add(StandardMaterial {
        base_color: ACCENT.with_alpha(0.10),
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    });
    commands.insert_resource(LockMaterials {
        lit: lit.clone(),
        glow: glow.clone(),
    });
    let tracer = materials.add(StandardMaterial {
        base_color: TRACER,
        emissive: LinearRgba::WHITE * 6.0,
        unlit: true,
        ..Default::default()
    });
    let ring_optimal = materials.add(StandardMaterial {
        base_color: ACCENT.with_alpha(0.42),
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    });
    let ring_falloff = materials.add(StandardMaterial {
        base_color: DIM.with_alpha(0.45),
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    });

    // The acquisition ring: one tick mark per tick of held lock the ruleset
    // demands, so a full ring is `LOCK_ACQUISITION_TICKS` and nothing has to
    // rescale when that constant moves.
    let segment = meshes.add(Cuboid::new(
        RETICLE_RADIUS_M * 0.16,
        0.4,
        RETICLE_RADIUS_M * 0.06,
    ));
    let bracket_long = meshes.add(Cuboid::new(RETICLE_RADIUS_M * 0.31, 0.4, 0.8));
    let bracket_short = meshes.add(Cuboid::new(0.8, 0.4, RETICLE_RADIUS_M * 0.31));
    let glow_disc = meshes.add(Cylinder {
        radius: RETICLE_RADIUS_M * 1.54,
        half_height: 0.02,
    });
    let ring = meshes.add(Torus {
        major_radius: 1.0,
        minor_radius: 0.0035,
    });
    let tracer_mesh = meshes.add(Cuboid::new(TRACER_MESH_LENGTH_M, 0.5, 0.5));
    let flash_mesh = meshes.add(Sphere {
        radius: IMPACT_FLASH_MESH_RADIUS_M,
    });
    // The marker needs its own torus rather than the range rings'. Theirs is
    // scaled to hundreds of metres, so a 0.0035 minor radius is a couple of
    // pixels of tube; scaled to the marker's ~15 m it would be 0.05 m, which
    // is well under a pixel at every zoom and would not render at all.
    // `IMPACT_MARKER_TUBE_RATIO` is chosen so the ring is about two and a half
    // pixels thick across the whole range instead.
    let marker_mesh = meshes.add(Torus {
        major_radius: 1.0,
        minor_radius: IMPACT_MARKER_TUBE_RATIO,
    });
    let flash_material = materials.add(StandardMaterial {
        base_color: IMPACT_ORANGE,
        emissive: LinearRgba::from(IMPACT_ORANGE) * 8.0,
        unlit: true,
        // The burst fades out over its life, which needs a blended pass.
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    });

    commands
        .spawn((LockReticle, Transform::default(), Visibility::Hidden))
        .with_children(|reticle| {
            reticle.spawn((
                LockGlow,
                Mesh3d(glow_disc),
                MeshMaterial3d(glow),
                Transform::from_xyz(0.0, -0.5, 0.0),
                Visibility::Hidden,
            ));
            for index in 0..LOCK_RING_SEGMENTS {
                let angle = core::f32::consts::TAU * index as f32 / LOCK_RING_SEGMENTS as f32;
                // Screen up is `-Z` under the top-down camera, so mark zero
                // sits at the top and the ring fills clockwise, as designed.
                reticle.spawn((
                    LockSegment(index),
                    Mesh3d(segment.clone()),
                    MeshMaterial3d(lit.clone()),
                    Transform::from_xyz(
                        RETICLE_RADIUS_M * angle.sin(),
                        0.0,
                        -RETICLE_RADIUS_M * angle.cos(),
                    )
                    .with_rotation(Quat::from_rotation_y(-angle)),
                    Visibility::Hidden,
                ));
            }
            reticle
                .spawn((LockBrackets, Transform::default(), Visibility::Inherited))
                .with_children(|brackets| {
                    let outer = RETICLE_RADIUS_M * 0.74;
                    let inner = RETICLE_RADIUS_M * 0.44;
                    for (sx, sz) in [(1.0f32, 1.0f32), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                        brackets.spawn((
                            Mesh3d(bracket_long.clone()),
                            MeshMaterial3d(lit.clone()),
                            Transform::from_xyz(sx * (outer + inner) / 2.0, 0.0, sz * outer),
                        ));
                        brackets.spawn((
                            Mesh3d(bracket_short.clone()),
                            MeshMaterial3d(lit.clone()),
                            Transform::from_xyz(sx * outer, 0.0, sz * (outer + inner) / 2.0),
                        ));
                    }
                });
        });

    // The pickup reach ring. It uses the impact marker's fatter tube rather
    // than the range rings' hairline: scaled to 25 m, a 0.0035 minor radius is
    // under a tenth of a millimetre of tube and would not render at all.
    let reach_material = materials.add(StandardMaterial {
        base_color: MINING_AMBER.with_alpha(0.45),
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    });
    commands.insert_resource(GrabMaterials {
        ring: reach_material.clone(),
    });
    commands.spawn((
        GrabReachRing,
        Mesh3d(marker_mesh.clone()),
        MeshMaterial3d(reach_material),
        Transform::default(),
        Visibility::Hidden,
    ));

    for (optimal, material) in [(true, ring_optimal), (false, ring_falloff)] {
        commands.spawn((
            RangeRing { optimal },
            Mesh3d(ring.clone()),
            MeshMaterial3d(material),
            Transform::from_scale(Vec3::ONE),
            Visibility::Hidden,
        ));
    }

    for index in 0..TRACER_POOL {
        commands.spawn((
            Tracer(index),
            Mesh3d(tracer_mesh.clone()),
            MeshMaterial3d(tracer.clone()),
            Transform::default(),
            Visibility::Hidden,
        ));
    }

    commands.spawn((
        ImpactFlash,
        Mesh3d(flash_mesh),
        MeshMaterial3d(flash_material.clone()),
        Transform::default(),
        Visibility::Hidden,
    ));
    // The same unit torus the range rings use, so the marker is a hairline
    // circle rather than a disc: a ring has no interior and therefore claims
    // no volume of fire.
    commands.spawn((
        ImpactMarker,
        Mesh3d(marker_mesh),
        MeshMaterial3d(flash_material),
        Transform::default(),
        Visibility::Hidden,
    ));
}

/// Lights one acquisition mark per tick of `lock_progress` and parks the
/// reticle on the locked target.
///
/// This is the stage that makes a lock visible at all: hide it, freeze it, or
/// drive it from anything other than [`CombatView::lock`] and the player has
/// exactly the blindness #378 reports.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_lock_reticle(
    view: Res<CombatView>,
    zoom: Res<crate::CameraZoom>,
    palette: Option<Res<LockMaterials>>,
    materials: Option<ResMut<Assets<StandardMaterial>>>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut reticle: Query<(&mut Transform, &mut Visibility), With<LockReticle>>,
    mut segments: SegmentQuery,
    mut brackets: BracketQuery,
    mut glow: GlowQuery,
) {
    let phase = view.lock.phase();
    let colour = if view.lock.class == Some(orrery_games::regolith::state::LockClass::Rock) {
        MINING_AMBER
    } else {
        ACCENT_BRIGHT
    };
    if let (Some(palette), Some(mut materials)) = (palette, materials) {
        if let Some(mut material) = materials.get_mut(&palette.lit) {
            material.base_color = colour;
            material.emissive = LinearRgba::from(colour) * 3.0;
        }
        if let Some(mut material) = materials.get_mut(&palette.glow) {
            material.base_color = colour.with_alpha(0.10);
        }
    }
    let lit = view.lock.segments_lit();
    let anchor = view
        .lock
        .target
        .and_then(|target| world_position(&bodies, target));

    if let Ok((mut transform, mut visibility)) = reticle.single_mut() {
        match anchor {
            Some(position) => {
                transform.translation = position;
                // A glyph, not a measurement: it holds its apparent size
                // across #521's zoom range instead of shrinking to nothing at
                // 4 km. The range rings, the arc marking and the tracers
                // deliberately do *not* do this — they mean metres, and would
                // lie about distance if they were held to screen size.
                transform.scale = Vec3::splat(zoom.glyph_scale());
                *visibility = Visibility::Inherited;
            }
            None => *visibility = Visibility::Hidden,
        }
    }
    for (segment, mut visibility) in &mut segments {
        *visibility = if segment.0 < lit {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    if let Ok((mut transform, mut visibility)) = brackets.single_mut() {
        // "Brackets snap solid and pull in" is the one frame of state change
        // at `LOCK_ACQUISITION_TICKS`.
        transform.scale = Vec3::splat(if phase == LockPhase::Locked { 0.9 } else { 1.0 });
        *visibility = if phase == LockPhase::Idle {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
    }
    if let Ok(mut visibility) = glow.single_mut() {
        *visibility = if phase == LockPhase::Locked {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Sizes the two weapon envelope rings from the equipped weapon's own table.
pub fn sync_range_rings(
    view: Res<CombatView>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut rings: Query<(&RangeRing, &mut Transform, &mut Visibility)>,
) {
    let Some(own) = view.own else {
        for (_, _, mut visibility) in &mut rings {
            *visibility = Visibility::Hidden;
        }
        return;
    };
    let Some(centre) = world_position(&bodies, own.entity) else {
        return;
    };
    let weapon = own.weapon_table();
    for (ring, mut transform, mut visibility) in &mut rings {
        let radius_mm = if ring.optimal {
            weapon.optimal_mm
        } else {
            weapon.optimal_mm.saturating_add(weapon.falloff_mm)
        };
        transform.translation = centre;
        transform.scale = Vec3::splat(radius_mm as f32 / 1_000.0);
        *visibility = Visibility::Inherited;
    }
}

/// Draws the ruleset's grab reach around the player's craft while a pickup is
/// worth reaching for, and tints it by the ruleset's own claimability answer.
///
/// The radius is [`crate::grab::GRAB_RADIUS_M`], derived from
/// `GRAB_RADIUS_MM`; nothing here invents a distance. The ring is hidden when
/// no live pickup is in view, so the ordinary picture is unchanged, and it
/// never implies a grant: a lit ring says "the ruleset's reach predicate holds
/// for that pickup right now", which is exactly what the pickup's own step
/// will re-evaluate against its own state.
pub(crate) fn sync_grab_reach_ring(
    view: Res<CombatView>,
    reach: Res<crate::grab::ReachView>,
    palette: Option<Res<GrabMaterials>>,
    materials: Option<ResMut<Assets<StandardMaterial>>>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut rings: Query<(&mut Transform, &mut Visibility), With<GrabReachRing>>,
) {
    let Ok((mut transform, mut visibility)) = rings.single_mut() else {
        return;
    };
    let nearest = reach.nearest();
    let centre = view
        .own
        .and_then(|own| world_position(&bodies, own.entity))
        .filter(|_| nearest.is_some());
    let Some(centre) = centre else {
        *visibility = Visibility::Hidden;
        return;
    };
    transform.translation = centre;
    transform.scale = Vec3::splat(crate::grab::GRAB_RADIUS_M);
    *visibility = Visibility::Inherited;
    if let (Some(palette), Some(mut materials)) = (palette, materials) {
        if let Some(mut material) = materials.get_mut(&palette.ring) {
            let alpha = if nearest.is_some_and(|pickup| pickup.claimable) {
                0.95
            } else {
                0.35
            };
            material.base_color = MINING_AMBER.with_alpha(alpha);
        }
    }
}

/// Places one tracer streak per shot the ruleset says is still in the air.
///
/// `attacker_pos` is always the muzzle the ruleset stamped. Target-authored
/// continuations supply their own countdown; campaign muzzle tracks use the
/// shooter skin's presentation clock and frozen last-known target position.
/// Neither path predicts a result, and an authoritative disposition retires
/// the presentation track.
///
/// Since #383 the tracer is a *streak*: [`tracer_streak`] stretches it back
/// over [`TRACER_PERSISTENCE_TICKS`] of the same flight so its apparent speed
/// reads slower, while the leading edge stays pinned to the ruleset's own
/// travelled position. The projectile's true speed is untouched — that lives
/// in the weapon table and changing it would be a balance change.
pub fn sync_tracers(
    tracks: Res<ProjectileTracks>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut tracers: Query<(&Tracer, &mut Transform, &mut Visibility)>,
) {
    let live = tracks.tracks();
    for (tracer, mut transform, mut visibility) in &mut tracers {
        let Some(track) = live.get(tracer.0) else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let (x, y, z) = track.origin.to_metres();
        let muzzle = Vec3::new(x as f32, y as f32, z as f32);
        let destination = track.destination.map_or_else(
            || world_position(&bodies, track.target),
            |position| {
                let (x, y, z) = position.to_metres();
                Some(Vec3::new(x as f32, y as f32, z as f32))
            },
        );
        let Some(destination) = destination else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let along = destination - muzzle;
        if along.length_squared() < f32::EPSILON {
            *visibility = Visibility::Hidden;
            continue;
        }
        let (head, tail) =
            streak_fractions(track.total.saturating_sub(track.remaining), track.total);
        let (centre, span) = tracer_streak(muzzle, destination, head, tail);
        transform.translation = centre;
        transform.rotation = Quat::from_rotation_y((-along.z).atan2(along.x));
        // The pool's cuboid is TRACER_MESH_LENGTH_M long at scale one; only
        // the streak's run varies, its cross-section does not.
        transform.scale = Vec3::new(span / TRACER_MESH_LENGTH_M, 1.0, 1.0);
        *visibility = Visibility::Inherited;
    }
}

fn world_position(
    bodies: &Query<(&crate::CoreEntity, &GlobalTransform)>,
    entity: PersistId,
) -> Option<Vec3> {
    bodies
        .iter()
        .find_map(|(core, transform)| (core.0 == entity).then(|| transform.translation()))
}

/// Draws the orange impact burst on the target of a **confirmed** hit.
///
/// Placement and animation only. Every decision about *whether* a burst is
/// owed lives in [`ShotFeedback::impact_burst`], which yields one for
/// `ShotResolved { result: Hit }` and for nothing else — not a miss, not an
/// out-of-arc refusal, not a shot whose lock broke in flight, and not the
/// provisional arrival cue. This system may not widen that set.
///
/// The anchor is the target's last replicated position, which is the best the
/// shooter has and is not the true intersection; see
/// [`ShotFeedback::impact_burst`] for why, and for why the burst is drawn as a
/// marker on the thing that was hit rather than as a surveyed point.
///
/// # Why the impact is two things (#531)
///
/// It used to be one filled sphere held to a constant *apparent* size across
/// #521's zoom range. That kept it legible at 4 km and quietly re-created the
/// honesty problem the doc comment above disclaims: the same adjudicated hit
/// was drawn spanning 4 m of world at 150 m of camera height and 118 m at
/// 4 km. Nothing about the event changed between those two frames; only the
/// observer moved. A filled, growing, emissive sphere is a picture of *how
/// much space the impact filled*, so scaling it with the camera is the skin
/// stating a size it does not know.
///
/// So the two jobs are split and each done honestly:
///
/// * **The burst** — the filled sphere — is drawn in world metres, sized from
///   the target's own ruleset radius ([`target_radius_m`]), the same number
///   the ruleset adds to weapon reach when deciding whether the shot
///   connected. It shrinks with distance exactly like the range rings and the
///   tracers do, because like them it means metres.
/// * **The marker** — a hairline ring — holds a constant apparent size, the
///   way the lock reticle does. It is a pointer: it says "the ruleset
///   adjudicated damage here", which is a true statement at any zoom, and its
///   empty interior claims no extent. It is suppressed entirely once the burst
///   is the larger of the two on screen ([`impact_marker_radius_m`]), so at
///   close range the impact is just the burst, as before.
pub fn sync_impact_flash(
    feedback: Res<ShotFeedback>,
    session: Res<crate::ActiveSession>,
    zoom: Res<crate::CameraZoom>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    materials: Option<ResMut<Assets<StandardMaterial>>>,
    mut flashes: FlashQuery,
    mut markers: MarkerQuery,
) {
    let burst = feedback
        .impact_burst()
        .and_then(|(target, progress)| Some((target, world_position(&bodies, target)?, progress)));
    let Ok((mut transform, mut visibility, material)) = flashes.single_mut() else {
        return;
    };
    let Some((target, position, progress)) = burst else {
        *visibility = Visibility::Hidden;
        if let Ok((_, mut marker_visibility)) = markers.single_mut() {
            *marker_visibility = Visibility::Hidden;
        }
        return;
    };
    transform.translation = position;
    let growth =
        IMPACT_BURST_START_SCALE + (IMPACT_BURST_END_SCALE - IMPACT_BURST_START_SCALE) * progress;
    let burst_radius_m = target_radius_m(session.executor(), target) * growth;
    transform.scale = Vec3::splat(burst_radius_m / IMPACT_FLASH_MESH_RADIUS_M);
    *visibility = Visibility::Inherited;
    if let Ok((mut marker_transform, mut marker_visibility)) = markers.single_mut() {
        match impact_marker_radius_m(burst_radius_m, zoom.glyph_scale()) {
            Some(radius_m) => {
                marker_transform.translation = position;
                marker_transform.scale = Vec3::splat(radius_m);
                *marker_visibility = Visibility::Inherited;
            }
            None => *marker_visibility = Visibility::Hidden,
        }
    }
    if let (Some(mut materials), Some(handle)) = (materials, material) {
        if let Some(mut material) = materials.get_mut(&handle.0) {
            let fade = (1.0 - progress).clamp(0.0, 1.0);
            material.base_color = IMPACT_ORANGE.with_alpha(fade);
            material.emissive = LinearRgba::from(IMPACT_ORANGE) * (8.0 * fade);
        }
    }
}

/// Sets every gauge's fill width from the ruleset's own numbers.
pub fn sync_gauges(view: Res<CombatView>, mut gauges: Query<(&Gauge, &mut Node)>) {
    for (gauge, mut node) in &mut gauges {
        let fill = match gauge {
            Gauge::OwnHull => view.own.map(|own| fraction(own.hull, own.max_hull())),
            Gauge::OwnShield => view.own.map(|own| fraction(own.shield, own.max_shield())),
            Gauge::Cooldown => view.own.map(|own| {
                fraction(
                    i32::from(own.cooldown),
                    i32::from(own.weapon_table().cooldown_ticks),
                )
            }),
            Gauge::TargetHull => view
                .target
                .map(|target| fraction(target.hull, target.max_hull()))
                .or_else(|| {
                    view.rock_target
                        .map(|target| fraction(target.hull, target.tier.limits().max_hull))
                }),
            Gauge::TargetShield => view
                .target
                .map(|target| fraction(target.shield, target.max_shield())),
        };
        node.width = Val::Percent(fill.unwrap_or(0.0) * 100.0);
    }
}

/// A gauge fill in `0.0..=1.0`.
#[must_use]
pub fn fraction(current: i32, ceiling: i32) -> f32 {
    if ceiling <= 0 {
        return 0.0;
    }
    (current.max(0) as f32 / ceiling as f32).clamp(0.0, 1.0)
}

/// Rewrites every HUD line from the current view.
pub fn refresh_combat_hud(
    view: Res<CombatView>,
    reach: Res<crate::grab::ReachView>,
    tracks: Res<ProjectileTracks>,
    broken: Res<LockBreak>,
    feedback: Res<ShotFeedback>,
    anchor: Res<crate::anchor::AnchorView>,
    mut lines: Query<(&Readout, &mut Text, &mut TextColor)>,
) {
    let phase = view.lock.phase();
    for (readout, mut text, mut colour) in &mut lines {
        let (body, tint) = match readout {
            Readout::OwnTitle => (
                view.own
                    .map_or_else(|| "-".to_owned(), |own| own.chassis_name().to_owned()),
                ACCENT_BRIGHT,
            ),
            Readout::OwnHull => (
                view.own.map_or_else(
                    || "-".to_owned(),
                    |own| format!("{}/{}", own.hull.max(0), own.max_hull()),
                ),
                MUTED,
            ),
            Readout::OwnShield => (
                view.own.map_or_else(
                    || "-".to_owned(),
                    |own| format!("{}/{}", own.shield.max(0), own.max_shield()),
                ),
                MUTED,
            ),
            Readout::OwnVitals => (
                view.own.map_or_else(
                    || "-".to_owned(),
                    |own| {
                        format!(
                            "#{}  {:.0} / {:.0} m/s   score {}",
                            own.entity.0,
                            own.speed_ms(),
                            own.max_speed_ms(),
                            own.score
                        )
                    },
                ),
                DIM,
            ),
            Readout::WeaponName => (
                view.own.map_or_else(
                    || "-".to_owned(),
                    |own| format!("{:?}", own.weapon).to_uppercase(),
                ),
                if view.own.is_some_and(|own| own.cooldown > 0) {
                    MUTED
                } else {
                    ACCENT_PALE
                },
            ),
            Readout::WeaponSpec => (
                view.own
                    .map_or_else(|| "-".to_owned(), |own| weapon_spec(own.weapon_table())),
                DIM,
            ),
            Readout::WeaponCooldown => (
                view.own
                    .map_or_else(|| "-".to_owned(), |own| format!("{} t", own.cooldown)),
                if view.own.is_some_and(|own| own.cooldown > 0) {
                    INK
                } else {
                    FAINT
                },
            ),
            Readout::WeaponEnvelope => (
                view.own
                    .map_or_else(|| "-".to_owned(), |own| weapon_envelope(own.weapon_table())),
                MUTED,
            ),
            Readout::LockLabel => (
                view.lock.label().to_owned(),
                match phase {
                    LockPhase::Idle => FAINT,
                    LockPhase::Acquiring => MUTED,
                    LockPhase::Locked
                        if view.lock.class
                            == Some(orrery_games::regolith::state::LockClass::Rock) =>
                    {
                        MINING_AMBER
                    }
                    LockPhase::Locked => ACCENT_PALE,
                },
            ),
            Readout::PickupReach => (
                crate::grab::caption(&reach),
                if reach.nearest().is_some_and(|pickup| pickup.claimable) {
                    MINING_AMBER
                } else {
                    DIM
                },
            ),
            // Both #955 lines are pure functions of `AnchorView`, which is a
            // copy of published ruleset state. The tint is the only judgement
            // the skin makes, and it is about legibility, not about the rule:
            // a tether that is acting is worth reading, so it brightens.
            Readout::TetherState => (
                crate::anchor::tether_line(&anchor),
                if anchor.outside_m > 0.0 {
                    IMPACT_ORANGE
                } else {
                    DIM
                },
            ),
            Readout::BloomBeacon => (
                crate::anchor::bloom_line(&anchor),
                if anchor.bloom.is_some() {
                    MINING_AMBER
                } else {
                    DIM
                },
            ),
            Readout::LockCaption => (view.lock.caption(), DIM),
            Readout::TargetTitle => (target_title(&view), MUTED),
            Readout::TargetHull => (
                view.target
                    .map(|target| format!("{}/{}", target.hull.max(0), target.max_hull()))
                    .or_else(|| {
                        view.rock_target.map(|target| {
                            format!(
                                "{}/{} | {} point{}",
                                target.hull.max(0),
                                target.tier.limits().max_hull,
                                target.tier.limits().points,
                                if target.tier.limits().points == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            )
                        })
                    })
                    .unwrap_or_else(|| "-".to_owned()),
                MUTED,
            ),
            Readout::TargetShield => (
                view.target.map_or_else(
                    || "-".to_owned(),
                    |target| format!("{}/{}", target.shield.max(0), target.max_shield()),
                ),
                MUTED,
            ),
            Readout::TargetRelation => (target_relation(&view, &tracks), MUTED),
            Readout::HitBandLine => {
                let band = view.hit_forecast();
                let tint = match band {
                    Some(HitBand::Perfect) => ACCENT_BRIGHT,
                    Some(HitBand::Good) => ACCENT_PALE,
                    Some(HitBand::Fair) => MUTED,
                    Some(HitBand::Poor) => DIM,
                    Some(HitBand::NoChance | HitBand::Unreadable) | None => FAINT,
                };
                (hit_band_line(&view), tint)
            }
            Readout::BreakBanner => (broken.banner(), Color::srgb(0.95, 0.62, 0.45)),
            Readout::ShotResult => {
                let tint = match feedback.cue {
                    Some(ShotCue::Arrival { .. }) => MUTED,
                    Some(ShotCue::Resolved {
                        result: ShotResult::Hit,
                        ..
                    }) => ACCENT_BRIGHT,
                    Some(ShotCue::Resolved {
                        result: ShotResult::Miss,
                        ..
                    })
                    | Some(ShotCue::Resolved {
                        result: ShotResult::OutOfArc,
                        ..
                    })
                    | Some(ShotCue::Resolved {
                        result: ShotResult::NoLock,
                        ..
                    })
                    | Some(ShotCue::Refused { .. })
                    | None => DIM,
                };
                (feedback.banner(), tint)
            }
        };
        **text = body;
        colour.0 = tint;
    }
}

/// `10-13 dmg x 1 roll | 20 t cycle`, from the weapon's published row.
#[must_use]
pub fn weapon_spec(weapon: Weapon) -> String {
    let low = weapon.damage_base;
    let high = weapon
        .damage_base
        .saturating_add(weapon.damage_spread.max(1).saturating_sub(1));
    format!(
        "{low}-{high} dmg x {} roll{} | {} t cycle",
        weapon.rolls,
        if weapon.rolls == 1 { "" } else { "s" },
        weapon.cooldown_ticks
    )
}

/// The four envelope numbers the design prints under the weapon name.
#[must_use]
pub fn weapon_envelope(weapon: Weapon) -> String {
    format!(
        "optimal {} m | falloff +{} m\nprojectile {} m/s | tracking {} urad/s",
        weapon.optimal_mm / 1_000,
        weapon.falloff_mm / 1_000,
        weapon.projectile_speed_mms / 1_000,
        weapon.tracking_urad_per_sec
    )
}

/// The target heading line, which still names a target whose window this
/// client cannot see — that absence is itself worth showing.
#[must_use]
pub fn target_title(view: &CombatView) -> String {
    match (view.target, view.rock_target, view.lock.target) {
        (Some(target), _, _) => format!("{} | #{}", target.chassis_name(), target.entity.0),
        (_, Some(target), _) => format!("{} | #{}", target.tier_name(), target.entity.0),
        // `{:x}`, not `{:#x}`: the alternate form already emits `0x`, so the
        // `#` prefix that matches the two decimal arms above rendered as
        // `#0xa1000015b50002`.
        (None, None, Some(id)) => format!("#{:x} | no window here", id.0),
        (None, None, None) => "-".to_owned(),
    }
}

/// Range, band and the ruleset's own remaining time of flight.
#[must_use]
pub fn target_relation(view: &CombatView, tracks: &ProjectileTracks) -> String {
    let mut parts = Vec::new();
    if let Some(range_mm) = view.range_mm() {
        // A replicated body is frozen between refreshes — nothing dead-reckons
        // it — so the separation is exactly as old as the replica. Printing a
        // bare metre count for a stale body states a precision the client does
        // not have, which is the claim the hearsay arrows already refuse to
        // make: every one of them wears its age (#940).
        if view.target_is_stale() {
            let age = view.target_age_ticks.unwrap_or_default();
            parts.push(format!(
                "~{} m ({:.1} s stale)",
                range_mm / 1_000,
                age as f64 / f64::from(orrery_core::TICK_HZ)
            ));
        } else {
            parts.push(format!("{} m", range_mm / 1_000));
        }
    }
    if let Some(band) = view.band() {
        parts.push(band.label().to_owned());
        if let (Some(own), RangeBand::Falloff | RangeBand::Beyond) = (view.own, band) {
            let over = view.range_mm().unwrap_or(0) - own.weapon_table().optimal_mm;
            parts.push(format!("{} m past optimal", over / 1_000));
        }
    }
    match view.own.and_then(|own| tracks.own_shot(own.entity)) {
        Some(shot) if shot.timed => parts.push(format!(
            "flight {} t ({:.2} s)",
            shot.remaining,
            f64::from(shot.remaining) / f64::from(orrery_core::TICK_HZ)
        )),
        Some(_) => parts.push("leaving muzzle".to_owned()),
        None => parts.push("nothing in the air".to_owned()),
    }
    // The metres above are measured from where this craft is *now*. The
    // resolver measures a shot in the air from where it was when the trigger
    // was pulled (#940), so when those two disagree the disagreement is
    // printed rather than left for the player to discover as a refusal.
    if let Some(frame) = view.firing_frame(tracks) {
        if frame.exceeded() {
            parts.push(format!(
                "shot judged from {} m at fire | {} m past reach - RANGE EXCEEDED",
                frame.range_mm / 1_000,
                frame.past_reach_mm() / 1_000
            ));
        }
    }
    parts.join(" | ")
}

/// The hit-chance band beside the locked target, and the phrase that says
/// what it is reading.
///
/// Deliberately **not** a percentage: #445's owner refinement is that a
/// number invites a precision the simulation does not have. The word comes
/// from [`CombatView::hit_forecast`], which is the ruleset's own
/// `hit_chance_ppm` transcribed, so the band and the adjudicator are reading
/// the same arithmetic.
#[must_use]
pub fn hit_band_line(view: &CombatView) -> String {
    match view.hit_forecast() {
        None => "-".to_owned(),
        Some(band) => format!("{}  |  {}", band.label(), band.note()),
    }
}

/// The lock lines the F3 pane appends, naming every field they came from.
#[must_use]
pub fn lock_debug_lines(view: &CombatView, broken: &LockBreak) -> String {
    format!(
        "lock_target {} | lock_progress {}/{} | locks_acquired {}{}",
        view.lock
            .target
            .map_or_else(|| "none".to_owned(), |id| format!("#{}", id.0)),
        view.lock.progress,
        LOCK_ACQUISITION_TICKS,
        view.lock.acquired,
        match broken.reason {
            Some(reason) => format!(" | last break {reason:?}"),
            None => String::new(),
        }
    )
}

/// The chassis a craft flies, spelled for the HUD.
#[must_use]
pub fn chassis_of(view: Option<CraftView>) -> &'static str {
    view.map_or("-", |view| view.chassis_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combat::{LockView, Track, SHOT_CUE_TICKS};
    use crate::CoreEntity;
    use orrery_core::{Executor, QPos, QVel};
    use orrery_games::regolith::archetype::Archetype;
    use orrery_games::regolith::order::Outcome;
    use orrery_games::regolith::state::{Craft, RegolithState};
    use orrery_games::regolith::weapon::WeaponKind;
    use orrery_games::{Game, Regolith};
    use orrery_protocol::{Tick, UniverseSeed};

    const ME: PersistId = PersistId::new(1);
    const THEM: PersistId = PersistId::new(2);

    fn craft_view(entity: PersistId, archetype: Archetype, x: f64) -> CraftView {
        CraftView::of(
            entity,
            &Craft::spawned(archetype, QPos::from_metres(x, 0.0, 0.0), 0),
        )
    }

    /// An overlay world with the reticle and tracer pool present but no meshes:
    /// the systems under test only read and write `Transform` and `Visibility`.
    fn overlay_app() -> App {
        let mut app = App::new();
        app.init_resource::<CombatView>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<crate::CameraZoom>()
            .init_resource::<ShotFeedback>()
            .init_resource::<crate::anchor::AnchorView>()
            .insert_resource(crate::ActiveSession::Local(Box::default()));
        let world = app.world_mut();
        world.spawn((LockReticle, Transform::default(), Visibility::Hidden));
        world.spawn((LockBrackets, Transform::default(), Visibility::Inherited));
        world.spawn((LockGlow, Transform::default(), Visibility::Hidden));
        world.spawn((ImpactFlash, Transform::default(), Visibility::Hidden));
        world.spawn((ImpactMarker, Transform::default(), Visibility::Hidden));
        for index in 0..LOCK_RING_SEGMENTS {
            world.spawn((LockSegment(index), Transform::default(), Visibility::Hidden));
        }
        for index in 0..TRACER_POOL {
            world.spawn((Tracer(index), Transform::default(), Visibility::Hidden));
        }
        app
    }

    fn body(app: &mut App, entity: PersistId, x: f32, z: f32) {
        app.world_mut().spawn((
            CoreEntity(entity),
            Transform::from_xyz(x, 0.0, z),
            GlobalTransform::from_translation(Vec3::new(x, 0.0, z)),
        ));
    }

    fn lit_marks(app: &mut App) -> usize {
        app.world_mut()
            .query::<(&LockSegment, &Visibility)>()
            .iter(app.world())
            .filter(|(_, visibility)| **visibility != Visibility::Hidden)
            .count()
    }

    /// A live tracer's placement: entity translation and drawn length.
    ///
    /// The pool cuboid runs along local `+X`, so with the axis-aligned test
    /// corridors here the leading edge is `translation + span/2 · X`.
    fn tracer_geometry(app: &mut App, index: usize) -> Option<(Vec3, f32)> {
        app.world_mut()
            .query::<(&Tracer, &Transform, &Visibility)>()
            .iter(app.world())
            .find(|(tracer, _, _)| tracer.0 == index)
            .filter(|(_, _, visibility)| **visibility != Visibility::Hidden)
            .map(|(_, transform, _)| {
                (
                    transform.translation,
                    transform.scale.x * TRACER_MESH_LENGTH_M,
                )
            })
    }

    /// The lock ring must be a *display of `lock_progress`*, one mark per tick.
    ///
    /// This is the guard for the whole point of #378: if the reticle stops
    /// tracking the ruleset's counter — pinned to a constant, to a boolean, or
    /// to anything the skin decides for itself — the player is back to having
    /// no way to tell a lock from no lock.
    #[test]
    fn the_lock_ring_lights_one_mark_per_acquisition_tick() {
        for progress in [0u16, 1, 7, 17, 29, 30, 44] {
            let mut app = overlay_app();
            body(&mut app, ME, 0.0, 0.0);
            body(&mut app, THEM, 120.0, -60.0);
            app.insert_resource(CombatView {
                own: Some(craft_view(ME, Archetype::Interceptor, 0.0)),
                lock: LockView {
                    target: Some(THEM),
                    class: Some(orrery_games::regolith::state::LockClass::Ship),
                    progress,
                    acquired: 0,
                },
                target: Some(craft_view(THEM, Archetype::Cruiser, 120.0)),
                rock_target: None,
                target_age_ticks: None,
            });
            app.add_systems(Update, sync_lock_reticle);
            app.update();
            assert_eq!(
                lit_marks(&mut app),
                (progress as usize).min(LOCK_RING_SEGMENTS),
                "the ring must show lock_progress {progress}, not a constant"
            );
        }
    }

    #[test]
    fn the_reticle_parks_on_the_target_and_leaves_when_the_lock_does() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 120.0, -60.0);
        app.insert_resource(CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 0.0)),
            lock: LockView {
                target: Some(THEM),
                class: Some(orrery_games::regolith::state::LockClass::Ship),
                progress: 30,
                acquired: 3,
            },
            target: Some(craft_view(THEM, Archetype::Cruiser, 120.0)),
            rock_target: None,
            target_age_ticks: None,
        });
        app.add_systems(Update, sync_lock_reticle);
        app.update();
        let (translation, visible) = app
            .world_mut()
            .query::<(&LockReticle, &Transform, &Visibility)>()
            .iter(app.world())
            .map(|(_, transform, visibility)| {
                (transform.translation, *visibility != Visibility::Hidden)
            })
            .next()
            .expect("the reticle exists");
        assert!(visible, "a held lock must be drawn");
        assert_eq!(translation, Vec3::new(120.0, 0.0, -60.0));
        let glow = app
            .world_mut()
            .query::<(&LockGlow, &Visibility)>()
            .iter(app.world())
            .all(|(_, visibility)| *visibility != Visibility::Hidden);
        assert!(glow, "the glow lands when the lock closes");

        app.insert_resource(CombatView::default());
        app.update();
        let hidden = app
            .world_mut()
            .query::<(&LockReticle, &Visibility)>()
            .iter(app.world())
            .all(|(_, visibility)| *visibility == Visibility::Hidden);
        assert!(hidden, "a broken lock must take its reticle with it");
        assert_eq!(lit_marks(&mut app), 0);
    }

    fn shot(flight_ticks: Option<u16>) -> Outcome {
        Outcome::DamageDealt {
            attacker: ME,
            target: THEM,
            amount: 11,
            attacker_pos: QPos::from_metres(0.0, 0.0, 0.0),
            attacker_vel: QVel::default(),
            attacker_yaw_urad: 0,
            attacker_archetype: Archetype::Interceptor,
            attacker_weapon: WeaponKind::Stock,
            flight_ticks,
        }
    }

    fn in_flight(remaining: u16) -> Outcome {
        shot(Some(remaining))
    }

    /// The tracer must be a picture of the ruleset's own `flight_ticks`.
    ///
    /// If the tracer stops reading the event — pinned to the muzzle, teleported
    /// to the target, or advanced by a skin-side clock — the time of flight
    /// that #363 introduced stops being visible, which is the second half of
    /// what #378 reports. Since #383 the drawing is a streak whose *leading
    /// edge* is that picture: this walks the head through the same five-tick
    /// flight and demands it sit where the ruleset says, tick for tick.
    #[test]
    fn a_tracer_walks_the_shot_the_ruleset_reports() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 100.0, 0.0);
        app.add_systems(Update, sync_tracers);

        // Muzzle only: the ruleset says the shot exists, but has not given it
        // a flight time. Draw the minimum streak with its head still pinned to
        // the muzzle; the skin may not advance it.
        app.world_mut()
            .resource_mut::<ProjectileTracks>()
            .observe(&[shot(None)]);
        app.update();
        let (centre, span) = tracer_geometry(&mut app, 0).expect("the muzzle event is drawn");
        assert!(
            (centre.x + span / 2.0).abs() < 0.01,
            "without flight_ticks the tracer head must remain at the muzzle"
        );

        // Five ticks of flight, walked one event at a time. The whole flown
        // path fits inside one persistence window, so the tail rests on the
        // muzzle and the leading edge is exactly the travelled point.
        for (remaining, expected_head_x) in [(4u16, 20.0f32), (3, 40.0), (2, 60.0), (1, 80.0)] {
            app.world_mut()
                .resource_mut::<ProjectileTracks>()
                .observe(&[in_flight(remaining)]);
            app.update();
            let (centre, span) = tracer_geometry(&mut app, 0).expect("a shot in the air is drawn");
            let head_x = centre.x + span / 2.0;
            let tail_x = centre.x - span / 2.0;
            assert!(
                (head_x - expected_head_x).abs() < 0.01,
                "with {remaining} ticks left the shot's front belongs at x={expected_head_x}, not {head_x}"
            );
            assert!(
                tail_x.abs() < 0.01,
                "a short flight's trail must reach the muzzle, not {tail_x}"
            );
        }

        // Resolution emits no event, so the tracer must go with it.
        app.world_mut()
            .resource_mut::<ProjectileTracks>()
            .observe(&[]);
        app.update();
        assert!(
            tracer_geometry(&mut app, 0).is_none(),
            "a resolved shot must stop being drawn"
        );
    }

    /// The persistence window, not the whole corridor: on a long flight the
    /// trail must detach from the muzzle and hold `TRACER_PERSISTENCE_TICKS`
    /// worth of path behind a head still driven by the event.
    ///
    /// This is the guard that makes #383's fix track the flight — pin the
    /// trail to anything constant and either its length or its head position
    /// drifts off the ruleset's numbers below.
    #[test]
    fn the_streak_lags_by_the_persistence_window_on_a_long_flight() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 600.0, 0.0);
        app.add_systems(Update, sync_tracers);

        // A 30-tick flight over a 600 m corridor, walked tick by tick because
        // `observe` stitches totals from consecutive events. Stop with 17
        // ticks flown — five past the persistence window.
        {
            let mut tracks = app.world_mut().resource_mut::<ProjectileTracks>();
            tracks.observe(&[in_flight(29)]);
            for remaining in (13..=28u16).rev() {
                tracks.observe(&[in_flight(remaining)]);
            }
        }
        app.update();
        let (centre, span) = tracer_geometry(&mut app, 0).expect("the mid-flight streak");
        let head = centre.x + span / 2.0;
        let tail = centre.x - span / 2.0;
        assert!(
            (head - 340.0).abs() < 0.01,
            "the head must ride the event's flown fraction (17/30 of 600 m), not {head}"
        );
        assert!(
            (span - 600.0 * (TRACER_PERSISTENCE_TICKS as f32 / 30.0)).abs() < 0.01,
            "the trail must be {} ticks of a 600 m corridor, not {span} m",
            TRACER_PERSISTENCE_TICKS
        );
        assert!(
            tail > 99.0,
            "on a long flight the trail detaches from the muzzle (tail {tail})"
        );

        // Near arrival the front rides the head and the window holds: no
        // growth past what the ruleset's own countdown covers.
        {
            let mut tracks = app.world_mut().resource_mut::<ProjectileTracks>();
            for remaining in (1..=12u16).rev() {
                tracks.observe(&[in_flight(remaining)]);
            }
        }
        app.update();
        let (centre, span) = tracer_geometry(&mut app, 0).expect("the arriving streak");
        let head = centre.x + span / 2.0;
        assert!(
            (head - 580.0).abs() < 0.01,
            "front rides the event's 29/30 point: {head}"
        );
        assert!(
            (span - 600.0 * (TRACER_PERSISTENCE_TICKS as f32 / 30.0)).abs() < 0.01,
            "the window holds all the way in: {span} m"
        );
        assert!(
            head <= 600.0,
            "the streak may never lead the shot past its target"
        );
    }

    /// The window arithmetic in isolation: growth while flown is under the
    /// window, then a fixed-length streak travelling with the head.
    #[test]
    fn streak_fractions_grow_then_hold_the_persistence_window() {
        // Early: everything flown is lit, tail on the muzzle.
        assert_eq!(streak_fractions(5, 30), (5.0 / 30.0, 0.0));
        // Exactly one window in.
        assert_eq!(
            streak_fractions(TRACER_PERSISTENCE_TICKS, 30),
            (12.0 / 30.0, 0.0)
        );
        // Past it: the span holds at one window while the head advances.
        let (head_a, tail_a) = streak_fractions(15, 30);
        let (head_b, tail_b) = streak_fractions(25, 30);
        assert!((head_a - 0.5) < 1e-6 && head_a > 0.49);
        assert!(head_b > head_a, "the head must advance with flown ticks");
        assert!(
            ((head_a - tail_a) - (head_b - tail_b)).abs() < 1e-6,
            "past the window the streak length must hold, not keep growing"
        );
        assert!(
            (head_a - tail_a - TRACER_PERSISTENCE_TICKS as f32 / 30.0).abs() < 1e-6,
            "that held length is exactly {} ticks",
            TRACER_PERSISTENCE_TICKS
        );
        // Arrival: front on the target, window still trailing.
        assert_eq!(
            streak_fractions(30, 30),
            (1.0, (30 - TRACER_PERSISTENCE_TICKS) as f32 / 30.0)
        );
        // Degenerate totals cannot divide.
        assert_eq!(streak_fractions(3, 0), (1.0, 1.0));
    }

    /// Geometry: the leading edge sits on the head point and extra floor
    /// length grows backwards, never forwards past the ruleset's position.
    #[test]
    fn tracer_streak_pins_its_front_to_the_head_point() {
        let muzzle = Vec3::ZERO;
        let destination = Vec3::new(400.0, 0.0, 0.0);

        // Unfloored: centre is the midpoint of [tail, head].
        let (centre, span) = tracer_streak(muzzle, destination, 0.5, 0.3);
        assert!((span - 80.0).abs() < 1e-4);
        assert!((centre.x - 160.0).abs() < 1e-4);
        let front = centre + Vec3::X * span / 2.0;
        let back = centre - Vec3::X * span / 2.0;
        assert!((front.x - 200.0).abs() < 1e-4, "front == head point");
        assert!((back.x - 120.0).abs() < 1e-4, "back == tail point");

        // Floored: a short early flight still shows TRACER_MIN_SPAN_M, and
        // every metre of the floor goes behind the head.
        let (centre, span) = tracer_streak(muzzle, destination, 0.02, 0.0);
        assert_eq!(span, TRACER_MIN_SPAN_M);
        let front = centre + Vec3::X * span / 2.0;
        assert!(
            (front.x - 400.0 * 0.02).abs() < 1e-4,
            "the floored streak must not lead the shot: front {front}"
        );

        // Off-axis corridors get the same treatment along their own line.
        let destination = Vec3::new(300.0, 0.0, -400.0);
        let (centre, span) = tracer_streak(muzzle, destination, 1.0, 0.75);
        assert!((span - 125.0).abs() < 1e-3);
        let front = centre + (destination - muzzle).normalize() * span / 2.0;
        assert!(
            front.distance(destination) < 1e-3,
            "at arrival the front touches the target: {front}"
        );
    }

    #[test]
    fn concurrent_shots_take_separate_tracer_slots() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 100.0, 0.0);
        app.add_systems(Update, sync_tracers);
        app.world_mut()
            .resource_mut::<ProjectileTracks>()
            .observe(&[in_flight(9), in_flight(4)]);
        app.update();
        assert!(tracer_geometry(&mut app, 0).is_some());
        assert!(tracer_geometry(&mut app, 1).is_some());
        assert!(tracer_geometry(&mut app, 2).is_none());
    }

    /// The marker only does its job if it is actually drawn. Reusing the
    /// range rings' torus — authored for radii in the hundreds of metres —
    /// would have put a 0.05 m tube on a 15 m ring, which is well under a
    /// pixel at every zoom: a correct opacity, a correct position, and
    /// nothing on screen. That is the shape of #517, #524 and #514.
    #[test]
    fn the_marker_ring_renders_as_a_hairline_at_every_zoom() {
        const LINES: f32 = 1080.0;
        for height_m in [
            crate::CAMERA_MIN_HEIGHT_M,
            crate::CAMERA_DEFAULT_HEIGHT_M,
            crate::CAMERA_MAX_HEIGHT_M,
        ] {
            let metres_per_pixel = 2.0 * crate::visible_half_height_m(height_m) / LINES;
            let glyph = height_m / crate::CAMERA_DEFAULT_HEIGHT_M;
            let radius_m = IMPACT_MARKER_RADIUS_M * glyph;
            let tube_px = 2.0 * IMPACT_MARKER_TUBE_RATIO * radius_m / metres_per_pixel;
            assert!(
                (1.5_f32..=6.0).contains(&tube_px),
                "the ring is {tube_px} px thick at {height_m} m of camera height"
            );
            let diameter_px = 2.0 * radius_m / metres_per_pixel;
            assert!(
                (24.0..=96.0).contains(&diameter_px),
                "the ring is {diameter_px} px across at {height_m} m of camera height"
            );
        }
    }

    /// #531: the same adjudicated hit must be drawn at the same size in the
    /// world no matter where the camera is, because camera height is a fact
    /// about the observer. The burst's world radius is therefore pinned to the
    /// ruleset's own target radius, and the only thing that tracks the zoom is
    /// the marker ring — which is a pointer with an empty interior, not a
    /// picture of an extent.
    #[test]
    fn the_burst_holds_its_world_size_across_the_zoom_range() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 100.0, -40.0);
        app.add_systems(Update, sync_impact_flash);
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Hit,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };

        let read = |app: &mut App| {
            let burst = app
                .world_mut()
                .query_filtered::<&Transform, With<ImpactFlash>>()
                .iter(app.world())
                .next()
                .expect("the flash exists")
                .scale
                .x
                * IMPACT_FLASH_MESH_RADIUS_M;
            let marker = app
                .world_mut()
                .query_filtered::<(&Transform, &Visibility), With<ImpactMarker>>()
                .iter(app.world())
                .map(|(transform, visibility)| {
                    (*visibility != Visibility::Hidden).then_some(transform.scale.x)
                })
                .next()
                .expect("the marker exists");
            (burst, marker)
        };

        // Fully zoomed out, then fully zoomed in. `zoomed` clamps, so the
        // large notch counts land exactly on the documented limits.
        *app.world_mut().resource_mut::<crate::CameraZoom>() =
            crate::CameraZoom::default().zoomed(-100.0);
        app.update();
        let (far_burst, far_marker) = read(&mut app);

        *app.world_mut().resource_mut::<crate::CameraZoom>() =
            crate::CameraZoom::default().zoomed(100.0);
        app.update();
        let (near_burst, near_marker) = read(&mut app);

        assert!(
            (far_burst - near_burst).abs() < 1e-3,
            "the burst is a world measurement: {far_burst} m at 4 km vs {near_burst} m at 150 m"
        );
        let session_radius = {
            let session = app.world().resource::<crate::ActiveSession>();
            target_radius_m(session.executor(), THEM)
        };
        assert!(
            (far_burst - session_radius * IMPACT_BURST_START_SCALE).abs() < 1e-3,
            "the burst spans the ruleset's own target radius; {far_burst} m against \
             {session_radius} m of target"
        );
        // The pre-#531 drawing held a constant apparent size, which put the
        // burst at 58.9 m of world radius at the far end of the zoom range.
        assert!(
            far_burst < 20.0,
            "a burst spanning {far_burst} m of world claims a precision the \
             anchor does not have"
        );

        let far_marker = far_marker.expect("zoomed out, the burst needs a pointer");
        assert!(
            far_marker > far_burst,
            "the marker only exists while it is the larger of the two"
        );
        assert!(
            near_marker.is_none_or(|marker| marker <= near_burst),
            "zoomed in, the burst is its own marker and the ring is redundant"
        );
        // The suppression itself, independent of where the zoom limits happen
        // to land: a burst already larger than the pointer gets no pointer.
        assert_eq!(
            impact_marker_radius_m(400.0, 1.0),
            None,
            "a burst wider than the ring must not also wear the ring"
        );
        assert_eq!(
            impact_marker_radius_m(1.0, 1.0),
            Some(IMPACT_MARKER_RADIUS_M),
            "a burst smaller than the ring keeps its pointer at apparent size"
        );
    }

    /// #531 asked whether an adjudicated hit is *legible* at both ends of the
    /// zoom range, and the honest answer was that nobody had seen one. It has
    /// now been seen, and this is the number that made it reproducible.
    ///
    /// The other burst tests reason about the constants. This one runs
    /// [`sync_impact_flash`] and measures the transforms it actually wrote,
    /// converted to pixels, which is what the complaint was about: the burst
    /// sphere alone is 0.7 px across at 4 km on a 720-line window, and the cue
    /// is legible anyway because the marker ring is the pointer. Measured live
    /// on 2026-08-27: the ring held 27.8 px across at 4000 m, at 565 m and at
    /// 161 m of camera height, and the burst overtook and suppressed it at
    /// close range.
    ///
    /// The floor is on the **cue**, not on either half of it. A test that
    /// asserted on the burst alone would demand the skin draw an extent it
    /// does not know; a test that asserted on the ring alone would not notice
    /// the ring being suppressed while the burst was still sub-pixel.
    #[test]
    fn the_impact_cue_clears_the_legibility_floor_at_both_zoom_extremes() {
        /// Pixels the whole cue must span at any zoom. Well above
        /// [`crate::MIN_LEGIBLE_DIAMETER_PX`]: a hit is an event the player is
        /// meant to read in the frame it happens, not merely a mark that
        /// renders.
        const CUE_FLOOR_PX: f32 = 20.0;

        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 100.0, -40.0);
        app.add_systems(Update, sync_impact_flash);
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Hit,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };

        // What the system wrote, read back the way `capture_impact_geometry`
        // reads it in a live run: the burst's scale against its authored mesh
        // radius, and the marker's scale, which is its world radius outright.
        let cue_px = |app: &mut App, height_m: f32| {
            *app.world_mut().resource_mut::<crate::CameraZoom>() = crate::CameraZoom::default()
                .zoomed(if height_m > crate::CAMERA_DEFAULT_HEIGHT_M {
                    -100.0
                } else {
                    100.0
                });
            app.update();
            let burst_m = app
                .world_mut()
                .query_filtered::<&Transform, With<ImpactFlash>>()
                .iter(app.world())
                .next()
                .expect("the flash exists")
                .scale
                .x
                * IMPACT_FLASH_MESH_RADIUS_M;
            let marker_m = app
                .world_mut()
                .query_filtered::<(&Transform, &Visibility), With<ImpactMarker>>()
                .iter(app.world())
                .next()
                .map(|(transform, visibility)| {
                    (*visibility != Visibility::Hidden).then_some(transform.scale.x)
                })
                .expect("the marker exists");
            let px = |radius_m: f32| {
                crate::apparent_diameter_px(radius_m, height_m, crate::REFERENCE_VIEWPORT_PX)
            };
            (px(burst_m), marker_m.map(px))
        };

        let (far_burst_px, far_marker_px) = cue_px(&mut app, crate::CAMERA_MAX_HEIGHT_M);
        let (near_burst_px, near_marker_px) = cue_px(&mut app, crate::CAMERA_MIN_HEIGHT_M);

        // The complaint's own number, kept as a fact rather than a target: at
        // full zoom-out the burst on an interceptor is a couple of pixels.
        assert!(
            far_burst_px < CUE_FLOOR_PX,
            "if the burst alone had grown to {far_burst_px} px at 4 km it would be \
             claiming an extent again; #531 is about the cue, not the sphere"
        );
        let far_cue_px = far_marker_px.map_or(far_burst_px, |marker| marker.max(far_burst_px));
        let near_cue_px = near_marker_px.map_or(near_burst_px, |marker| marker.max(near_burst_px));
        for (where_, cue_px) in [("4000 m", far_cue_px), ("150 m", near_cue_px)] {
            assert!(
                cue_px >= CUE_FLOOR_PX,
                "an adjudicated hit is {cue_px} px across at {where_} of camera height, \
                 under the {CUE_FLOOR_PX} px floor"
            );
        }

        // The ring is a glyph, so it is the *same* size on screen at both ends
        // — that is what "constant apparent size" means, and it is the half of
        // the cue that carries it when the burst cannot.
        let far_marker_px = far_marker_px.expect("zoomed out, the ring is the cue");
        assert!(
            (far_marker_px
                - crate::apparent_diameter_px(
                    IMPACT_MARKER_RADIUS_M,
                    crate::CAMERA_DEFAULT_HEIGHT_M,
                    crate::REFERENCE_VIEWPORT_PX,
                ))
            .abs()
                < 0.1,
            "the ring is a glyph: {far_marker_px} px at 4 km must equal its size at 900 m"
        );
    }

    /// The flash is a picture of the cue and nothing else: provisional or
    /// confirmed-hit draws anchored on the target's body, a miss retracts it,
    /// expiry hides it. Pinning it to a constant — always on, or drawn on the
    /// shooter instead of the target — is the failure this catches.
    #[test]
    fn the_impact_flash_follows_the_shot_cue() {
        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 100.0, -40.0);
        app.add_systems(Update, sync_impact_flash);

        let flash_state = |app: &mut App| {
            app.world_mut()
                .query::<(&ImpactFlash, &Transform, &Visibility)>()
                .iter(app.world())
                .map(|(_, transform, visibility)| {
                    (*visibility != Visibility::Hidden, transform.translation)
                })
                .next()
                .expect("the flash exists")
        };

        // Nothing live: hidden.
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(!visible);

        // Provisional arrival: the banner says an adjudication is due, and the
        // world stays clean. Before #522 this drew the burst, which announced
        // a hit a tick before the target had adjudicated one.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Arrival { target: THEM }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(!visible, "a provisional arrival must not burst");

        // A miss verdict draws nothing either.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Miss,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(!visible, "a miss must draw no burst");

        // A confirmed hit bursts, on the target's own body.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Hit,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, at) = flash_state(&mut app);
        assert!(visible, "a confirmed hit must burst");
        assert_eq!(at, Vec3::new(100.0, 0.0, -40.0));

        // And it is spent well before the banner is.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Hit,
            }),
            ticks_left: SHOT_CUE_TICKS - crate::combat::IMPACT_BURST_TICKS,
        };
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(!visible, "the burst expires after IMPACT_BURST_TICKS");
    }

    /// The reach ring is the ruleset's own 25 m, in world metres, and it only
    /// appears when there is a live pickup to reach for.
    #[test]
    fn the_reach_ring_is_the_rulesets_radius_and_only_shows_with_a_pickup_in_view() {
        use crate::grab::{PickupReach, ReachView, GRAB_RADIUS_M};
        use orrery_games::regolith::GRAB_RADIUS_MM;

        let mut app = overlay_app();
        app.init_resource::<ReachView>();
        body(&mut app, ME, 40.0, 0.0);
        app.world_mut()
            .spawn((GrabReachRing, Transform::default(), Visibility::Hidden));
        app.insert_resource(CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 40.0)),
            ..CombatView::default()
        });
        app.add_systems(Update, sync_grab_reach_ring);

        // No pickup in view: nothing is drawn.
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<(&Transform, &Visibility), With<GrabReachRing>>();
        let (_, visibility) = query.single(app.world()).expect("the ring exists");
        assert_eq!(*visibility, Visibility::Hidden);

        // A live pickup 100 m out: the ring appears at the ruleset's radius,
        // centred on the player's own craft.
        app.insert_resource(ReachView {
            live: vec![PickupReach {
                entity: PersistId::new(77),
                range_mm: 100_000,
                claimable: false,
            }],
            ..ReachView::default()
        });
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<(&Transform, &Visibility), With<GrabReachRing>>();
        let (transform, visibility) = query.single(app.world()).expect("the ring exists");
        assert_ne!(*visibility, Visibility::Hidden);
        assert_eq!(transform.translation, Vec3::new(40.0, 0.0, 0.0));
        assert_eq!(transform.scale.x, GRAB_RADIUS_M);
        #[allow(clippy::cast_precision_loss)]
        let from_the_table = GRAB_RADIUS_MM as f32 / 1_000.0;
        assert_eq!(transform.scale.x, from_the_table);
    }

    #[test]
    fn range_rings_take_their_radius_from_the_weapon_table() {
        let mut app = overlay_app();
        body(&mut app, ME, 40.0, 0.0);
        app.world_mut().spawn((
            RangeRing { optimal: true },
            Transform::default(),
            Visibility::Hidden,
        ));
        app.world_mut().spawn((
            RangeRing { optimal: false },
            Transform::default(),
            Visibility::Hidden,
        ));
        app.insert_resource(CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 40.0)),
            ..CombatView::default()
        });
        app.add_systems(Update, sync_range_rings);
        app.update();
        let stock = WeaponKind::Stock.weapon();
        let mut seen = Vec::new();
        for (ring, transform, visibility) in app
            .world_mut()
            .query::<(&RangeRing, &Transform, &Visibility)>()
            .iter(app.world())
        {
            assert!(*visibility != Visibility::Hidden);
            assert_eq!(transform.translation, Vec3::new(40.0, 0.0, 0.0));
            seen.push((ring.optimal, transform.scale.x));
        }
        seen.sort_by_key(|entry| entry.0);
        assert_eq!(seen[1], (true, stock.optimal_mm as f32 / 1_000.0));
        assert_eq!(
            seen[0],
            (
                false,
                (stock.optimal_mm + stock.falloff_mm) as f32 / 1_000.0
            )
        );
    }

    #[test]
    fn gauges_track_hull_and_shield() {
        let mut app = overlay_app();
        let mut craft = Craft::spawned(Archetype::Cruiser, QPos::from_metres(0.0, 0.0, 0.0), 0);
        craft.hull = 150;
        craft.shield = 0;
        app.world_mut().spawn((Gauge::OwnHull, Node::default()));
        app.world_mut().spawn((Gauge::OwnShield, Node::default()));
        app.insert_resource(CombatView {
            own: Some(CraftView::of(ME, &craft)),
            ..CombatView::default()
        });
        app.add_systems(Update, sync_gauges);
        app.update();
        let widths: Vec<_> = app
            .world_mut()
            .query::<(&Gauge, &Node)>()
            .iter(app.world())
            .map(|(gauge, node)| (*gauge, node.width))
            .collect();
        assert!(widths.contains(&(Gauge::OwnHull, Val::Percent(50.0))));
        assert!(widths.contains(&(Gauge::OwnShield, Val::Percent(0.0))));
    }

    /// End to end against the real rules: hold the trigger and watch the ring
    /// fill in step with the ruleset's own counter, tick for tick.
    #[test]
    fn holding_the_trigger_fills_the_ring_in_step_with_the_ruleset() {
        let seed = UniverseSeed([0x61; 32]);
        let game = Regolith::honest();
        let mut executor = Executor::new(game, seed);
        executor.insert(ME, game.spawn(ME, 0));
        executor.insert(THEM, game.spawn(THEM, 1));
        let me = crate::intent::IntentPipeline::new(seed, ME, 0, vec![THEM]);
        let them = crate::intent::IntentPipeline::new(seed, THEM, 1, vec![ME]);

        let mut app = overlay_app();
        body(&mut app, ME, 0.0, 0.0);
        body(&mut app, THEM, 200.0, 0.0);
        app.add_systems(Update, sync_lock_reticle);

        // The target is the player's own, clicked: a human seat no longer
        // inherits the pilot's tick-scheduled lock (#1121), so a test that
        // left `lock_target` empty was exercising the hazard rather than the
        // ring. Clicking the adjacent craft is what the pilot's combat row
        // would have chosen anyway, so the flight below is unchanged.
        let held = crate::intent::Controls {
            fire: true,
            thrust: true,
            lock_target: Some(THEM),
            ..crate::intent::Controls::default()
        };
        let mut pending = std::collections::BTreeMap::<PersistId, Vec<_>>::new();
        for raw in 0..40u64 {
            // Ticks 0..179 are the pilot table's combat row; the seat's own
            // clicked target is the same adjacent craft.
            let tick = Tick::new(raw);
            let mut delivered = std::collections::BTreeMap::<PersistId, Vec<_>>::new();
            for (entity, mut orders) in [
                (ME, me.human_orders(tick, held)),
                (THEM, them.bot_orders(tick)),
            ] {
                let mut inbox = pending.remove(&entity).unwrap_or_default();
                inbox.append(&mut orders);
                let outcome = executor
                    .step_entity(entity, tick, &inbox)
                    .expect("both craft installed");
                for event in &outcome.events {
                    if let Some((target, input)) = executor.ruleset().deliver(event) {
                        delivered.entry(target).or_default().push(input);
                    }
                }
            }
            pending = delivered;

            let view = CombatView::read(&executor, ME);
            app.insert_resource(view);
            app.update();

            let RegolithState::Craft(craft) = executor.state(ME).expect("my state") else {
                panic!("the player is a craft");
            };
            assert_eq!(
                view.lock.progress, craft.lock_progress,
                "tick {raw}: the view drifted from the ruleset"
            );
            assert_eq!(
                lit_marks(&mut app),
                (craft.lock_progress as usize).min(LOCK_RING_SEGMENTS),
                "tick {raw}: the ring drifted from lock_progress {}",
                craft.lock_progress
            );
        }
        let RegolithState::Craft(craft) = executor.state(ME).expect("my state") else {
            panic!("the player is a craft");
        };
        assert_eq!(
            craft.lock_target,
            Some(THEM),
            "the pilot's combat row targets the adjacent craft"
        );
        assert!(
            craft.locks_acquired >= 1,
            "forty held-trigger ticks must clear the thirty-tick threshold"
        );
        assert_eq!(lit_marks(&mut app), LOCK_RING_SEGMENTS, "a full ring");
    }

    #[test]
    fn the_target_line_still_names_a_target_this_client_cannot_see() {
        let phantom = PersistId::new(0xB1_0000_0000_15A2);
        let view = CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 0.0)),
            lock: LockView {
                target: Some(phantom),
                class: None,
                progress: 30,
                acquired: 1,
            },
            target: None,
            rock_target: None,
            target_age_ticks: None,
        };
        let line = target_title(&view);
        assert!(line.contains("no window here"), "{line}");
        assert!(line.contains("b1"), "{line}");
    }

    #[test]
    fn a_full_track_reports_the_flight_it_has_left() {
        // Half of Stock's optimal, read off the table: #545 cut Stock to
        // 240 m and the literal 250 m this used stopped being "inside
        // optimal" without the assertion below noticing.
        let inside_optimal_m = WeaponKind::Stock.weapon().optimal_mm as f64 / 2_000.0;
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[in_flight(7)]);
        let view = CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 0.0)),
            lock: LockView {
                target: Some(THEM),
                class: Some(orrery_games::regolith::state::LockClass::Ship),
                progress: 30,
                acquired: 1,
            },
            target: Some(craft_view(THEM, Archetype::Cruiser, inside_optimal_m)),
            rock_target: None,
            target_age_ticks: None,
        };
        let line = target_relation(&view, &tracks);
        assert!(
            line.contains(&format!("{} m", inside_optimal_m as i64)),
            "{line}"
        );
        assert!(line.contains("inside optimal"), "{line}");
        assert!(line.contains("flight 7 t"), "{line}");
        assert_eq!(
            Track {
                attacker: ME,
                target: THEM,
                origin: QPos::from_metres(0.0, 0.0, 0.0),
                weapon: WeaponKind::Stock,
                remaining: 7,
                total: 8,
                timed: true,
                destination: None,
                presented: false,
            },
            tracks.tracks()[0]
        );

        tracks.observe(&[shot(None)]);
        let muzzle = target_relation(&view, &tracks);
        assert!(muzzle.contains("leaving muzzle"), "{muzzle}");
        assert!(!muzzle.contains("flight 1 t"), "{muzzle}");
    }
}

#[cfg(test)]
mod band_line {
    use super::*;
    use crate::combat::{CraftView, HitBand, LockView};
    use orrery_core::{QPos, QVel};
    use orrery_games::regolith::archetype::Archetype;
    use orrery_games::regolith::state::Craft;
    use orrery_games::regolith::weapon::WeaponKind;

    fn locked_on(transverse_mms: i64) -> CombatView {
        let mut me = Craft::spawned(Archetype::Interceptor, QPos { x: 0, y: 0, z: 0 }, 0);
        me.lock_target = Some(PersistId::new(2));
        me.lock_progress = LOCK_ACQUISITION_TICKS;
        let mut them = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: WeaponKind::Stock.weapon().optimal_mm,
                y: 0,
                z: 0,
            },
            0,
        );
        them.vel = QVel {
            x: 0,
            y: 0,
            z: transverse_mms,
        };
        CombatView {
            own: Some(CraftView::of(PersistId::new(1), &me)),
            lock: LockView::of(&me),
            target: Some(CraftView::of(PersistId::new(2), &them)),
            rock_target: None,
            target_age_ticks: None,
        }
    }

    /// #940: the range readout printed an exact metre count for a replicated
    /// craft that is *frozen* between refreshes — nothing dead-reckons it —
    /// with no cue at all that the number was up to two seconds old. The
    /// hearsay layer, carrying strictly weaker data, prints its age on every
    /// arrow and refuses to draw past its horizon; the replicated layer made
    /// the stronger claim with no disclosure.
    ///
    /// Ages here are the ones production produces: `replica_age_ticks` is
    /// bounded by `REPLICA_TTL_TICKS` (120) because expiry retires anything
    /// older, and `None` is offline play, which replicates nothing.
    #[test]
    fn a_stale_replica_is_never_reported_as_an_exact_separation() {
        let tracks = ProjectileTracks::default();

        // No replica to age: offline play, where the state is local and the
        // separation really is exact. Disclosure here would be an invention.
        let fresh = CombatView {
            target_age_ticks: None,
            ..locked_on(0)
        };
        let line = target_relation(&fresh, &tracks);
        assert!(
            line.contains("240 m") && !line.contains('~') && !line.contains("stale"),
            "local state is exact and must be printed as such: {line}"
        );

        // Refreshed on this very tick: still exact.
        let current = CombatView {
            target_age_ticks: Some(0),
            ..locked_on(0)
        };
        assert!(
            !target_relation(&current, &tracks).contains("stale"),
            "a replica that refreshed this tick is not stale"
        );

        // Every age a live replica can actually hold.
        for age in 1..=120u64 {
            let view = CombatView {
                target_age_ticks: Some(age),
                ..locked_on(0)
            };
            let line = target_relation(&view, &tracks);
            assert!(
                line.contains("stale"),
                "age {age}: the readout stated a separation to a body that has \
                 not reported in for {age} ticks, with no cue: {line}"
            );
            assert!(
                line.contains('~'),
                "age {age}: the metre count must stop claiming to be exact: {line}"
            );
        }
    }

    /// #445's owner refinement, made executable: the readout is a word, and
    /// there is no percentage anywhere in it.
    #[test]
    fn the_readout_is_a_band_and_never_a_percentage() {
        for transverse in [0i64, 20_000, 60_000, 200_000, 1_000_000_000] {
            let line = hit_band_line(&locked_on(transverse));
            assert!(
                !line.contains('%'),
                "the band line printed a percentage: {line}"
            );
            assert!(
                !line.chars().any(|character| character.is_ascii_digit()),
                "the band line printed a number: {line}"
            );
        }
        assert!(hit_band_line(&locked_on(0)).starts_with(HitBand::Perfect.label()));
        assert!(hit_band_line(&locked_on(1_000_000_000)).starts_with(HitBand::NoChance.label()));
    }

    /// With no lock there is nothing to read, and the HUD says so rather than
    /// naming a band for a target it does not have.
    #[test]
    fn no_target_means_no_band() {
        let mut idle = locked_on(0);
        idle.target = None;
        assert_eq!(hit_band_line(&idle), "-");
        assert_eq!(idle.hit_forecast(), None);
    }

    /// The pickup line rides the same refresh, so what it says about reach is
    /// this tick's reading and not a stale one.
    #[test]
    fn the_refresh_system_writes_the_pickup_reach_line() {
        use crate::grab::{PickupReach, ReachView};

        let mut app = App::new();
        app.insert_resource(locked_on(60_000))
            .insert_resource(ReachView {
                live: vec![PickupReach {
                    entity: PersistId::new(77),
                    range_mm: 12_000,
                    claimable: true,
                }],
                ..ReachView::default()
            })
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .init_resource::<crate::anchor::AnchorView>()
            .add_systems(Update, refresh_combat_hud);
        let line = app
            .world_mut()
            .spawn((Readout::PickupReach, Text::new("-"), TextColor(MUTED)))
            .id();
        app.update();
        let text = app.world().get::<Text>(line).expect("the line exists");
        assert!(
            text.contains("IN REACH") && text.contains("12 m") && text.contains("25 m"),
            "the pickup line did not report the reach: {}",
            **text
        );
    }

    /// The band travels on the same refresh path as every other readout: the
    /// whole HUD is rewritten from one `CombatView`, so it cannot go stale
    /// against the lock beside it.
    #[test]
    fn the_refresh_system_writes_the_band_line() {
        let mut app = App::new();
        app.insert_resource(locked_on(60_000))
            .init_resource::<crate::grab::ReachView>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .init_resource::<crate::anchor::AnchorView>()
            .add_systems(Update, refresh_combat_hud);
        let line = app
            .world_mut()
            .spawn((Readout::HitBandLine, Text::new("-"), TextColor(MUTED)))
            .id();
        app.update();
        let text = app.world().get::<Text>(line).expect("the line exists");
        assert_eq!(**text, hit_band_line(&locked_on(60_000)));
        assert!(**text != *"-", "the band line was never written");
    }

    /// #955's two anchor lines reach the screen, and say the tether's own
    /// numbers rather than a placeholder.
    ///
    /// The failure this guards is not a wrong string — `crate::anchor` tests
    /// the strings — but a line that is spawned and never refreshed, which is
    /// invisible to every test that calls the builder directly.
    #[test]
    fn the_refresh_system_writes_both_anchor_lines() {
        use crate::anchor::{AnchorView, BloomBeacon};

        let outside_m = orrery_games::regolith::TETHER_BAND_MM as f32 / 1_000.0 * 2.0;
        let mut app = App::new();
        app.insert_resource(locked_on(60_000))
            .init_resource::<crate::grab::ReachView>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .insert_resource(AnchorView {
                outside_m,
                tether_ramp: 1.0,
                bloom: Some(BloomBeacon {
                    range_m: 640.0,
                    seconds_left: 12,
                    rocks_alive: 7,
                }),
            })
            .add_systems(Update, refresh_combat_hud);
        let tether = app
            .world_mut()
            .spawn((Readout::TetherState, Text::new("-"), TextColor(MUTED)))
            .id();
        let bloom = app
            .world_mut()
            .spawn((Readout::BloomBeacon, Text::new("-"), TextColor(MUTED)))
            .id();
        app.update();

        let tether_text = app.world().get::<Text>(tether).expect("the line exists");
        assert!(
            tether_text.contains("OUTSIDE ISLAND") && tether_text.contains("TETHER FULL"),
            "the tether line was never written: {}",
            **tether_text
        );
        let bloom_text = app.world().get::<Text>(bloom).expect("the line exists");
        assert!(
            bloom_text.contains("BLOOM") && bloom_text.contains("7 ROCKS"),
            "the bloom beacon was never written: {}",
            **bloom_text
        );
    }
}
