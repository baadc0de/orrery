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

/// A weapon envelope ring drawn around the player's own craft.
#[derive(Component)]
pub struct RangeRing {
    /// True for the solid `optimal_mm` ring, false for the dashed falloff edge.
    pub optimal: bool,
}

/// One slot in the tracer pool.
#[derive(Component)]
pub struct Tracer(pub usize);

/// The world-space burst drawn on a shot's target while an arrival cue lives.
#[derive(Component)]
pub struct ImpactFlash;

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
        Text::new("—"),
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
        radius: RETICLE_RADIUS_M * 0.12,
    });
    let flash_material = materials.add(StandardMaterial {
        base_color: TRACER,
        emissive: LinearRgba::WHITE * 8.0,
        unlit: true,
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
pub fn sync_lock_reticle(
    view: Res<CombatView>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut reticle: Query<(&mut Transform, &mut Visibility), With<LockReticle>>,
    mut segments: SegmentQuery,
    mut brackets: BracketQuery,
    mut glow: GlowQuery,
) {
    let phase = view.lock.phase();
    let lit = view.lock.segments_lit();
    let anchor = view
        .lock
        .target
        .and_then(|target| world_position(&bodies, target));

    if let Ok((mut transform, mut visibility)) = reticle.single_mut() {
        match anchor {
            Some(position) => {
                transform.translation = position;
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

/// Places one tracer streak per shot the ruleset says is still in the air.
///
/// Every input comes from the event: `attacker_pos` is the muzzle the
/// ruleset stamped, and the head fraction is the ruleset's own `flight_ticks`
/// countdown. There is no skin-side velocity integration, so a tracer cannot
/// be somewhere the ruleset does not put it, and it cannot outlive the shot.
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
        let Some(destination) = world_position(&bodies, track.target) else {
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

/// Draws the impact flash on the shot's target while a cue says one is live.
///
/// A provisional arrival and an authoritative hit both draw; a verdict of
/// miss retracts it — [`ShotFeedback::flash_target`] encodes that choice, so
/// this system stays a pure placement: anchor on the target's body or hide.
pub fn sync_impact_flash(
    feedback: Res<ShotFeedback>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform)>,
    mut flashes: Query<(&mut Transform, &mut Visibility), With<ImpactFlash>>,
) {
    let anchor = feedback
        .flash_target()
        .and_then(|target| world_position(&bodies, target));
    if let Ok((mut transform, mut visibility)) = flashes.single_mut() {
        match anchor {
            Some(position) => {
                transform.translation = position;
                *visibility = Visibility::Inherited;
            }
            None => *visibility = Visibility::Hidden,
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
                .map(|target| fraction(target.hull, target.max_hull())),
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
    tracks: Res<ProjectileTracks>,
    broken: Res<LockBreak>,
    feedback: Res<ShotFeedback>,
    mut lines: Query<(&Readout, &mut Text, &mut TextColor)>,
) {
    let phase = view.lock.phase();
    for (readout, mut text, mut colour) in &mut lines {
        let (body, tint) = match readout {
            Readout::OwnTitle => (
                view.own
                    .map_or_else(|| "—".to_owned(), |own| own.chassis_name().to_owned()),
                ACCENT_BRIGHT,
            ),
            Readout::OwnHull => (
                view.own.map_or_else(
                    || "—".to_owned(),
                    |own| format!("{}/{}", own.hull.max(0), own.max_hull()),
                ),
                MUTED,
            ),
            Readout::OwnShield => (
                view.own.map_or_else(
                    || "—".to_owned(),
                    |own| format!("{}/{}", own.shield.max(0), own.max_shield()),
                ),
                MUTED,
            ),
            Readout::OwnVitals => (
                view.own.map_or_else(
                    || "—".to_owned(),
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
                    || "—".to_owned(),
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
                    .map_or_else(|| "—".to_owned(), |own| weapon_spec(own.weapon_table())),
                DIM,
            ),
            Readout::WeaponCooldown => (
                view.own
                    .map_or_else(|| "—".to_owned(), |own| format!("{} t", own.cooldown)),
                if view.own.is_some_and(|own| own.cooldown > 0) {
                    INK
                } else {
                    FAINT
                },
            ),
            Readout::WeaponEnvelope => (
                view.own
                    .map_or_else(|| "—".to_owned(), |own| weapon_envelope(own.weapon_table())),
                MUTED,
            ),
            Readout::LockLabel => (
                phase.label().to_owned(),
                match phase {
                    LockPhase::Idle => FAINT,
                    LockPhase::Acquiring => MUTED,
                    LockPhase::Locked => ACCENT_PALE,
                },
            ),
            Readout::LockCaption => (view.lock.caption(), DIM),
            Readout::TargetTitle => (target_title(&view), MUTED),
            Readout::TargetHull => (
                view.target.map_or_else(
                    || "—".to_owned(),
                    |target| format!("{}/{}", target.hull.max(0), target.max_hull()),
                ),
                MUTED,
            ),
            Readout::TargetShield => (
                view.target.map_or_else(
                    || "—".to_owned(),
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

/// `10–13 dmg × 1 roll · 20 t cycle`, from the weapon's published row.
#[must_use]
pub fn weapon_spec(weapon: Weapon) -> String {
    let low = weapon.damage_base;
    let high = weapon
        .damage_base
        .saturating_add(weapon.damage_spread.max(1).saturating_sub(1));
    format!(
        "{low}–{high} dmg × {} roll{} · {} t cycle",
        weapon.rolls,
        if weapon.rolls == 1 { "" } else { "s" },
        weapon.cooldown_ticks
    )
}

/// The four envelope numbers the design prints under the weapon name.
#[must_use]
pub fn weapon_envelope(weapon: Weapon) -> String {
    format!(
        "optimal {} m · falloff +{} m\nprojectile {} m/s · tracking {} µrad/s",
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
    match (view.target, view.lock.target) {
        (Some(target), _) => format!("{} · #{}", target.chassis_name(), target.entity.0),
        (None, Some(id)) => format!("#{:#x} · no window here", id.0),
        (None, None) => "—".to_owned(),
    }
}

/// Range, band and the ruleset's own remaining time of flight.
#[must_use]
pub fn target_relation(view: &CombatView, tracks: &ProjectileTracks) -> String {
    let mut parts = Vec::new();
    if let Some(range_mm) = view.range_mm() {
        parts.push(format!("{} m", range_mm / 1_000));
    }
    if let Some(band) = view.band() {
        parts.push(band.label().to_owned());
        if let (Some(own), RangeBand::Falloff | RangeBand::Beyond) = (view.own, band) {
            let over = view.range_mm().unwrap_or(0) - own.weapon_table().optimal_mm;
            parts.push(format!("{} m past optimal", over / 1_000));
        }
    }
    match view.own.and_then(|own| tracks.own_shot(own.entity)) {
        Some(shot) => parts.push(format!(
            "flight {} t · {:.2} s",
            shot.remaining,
            f64::from(shot.remaining) / f64::from(orrery_core::TICK_HZ)
        )),
        None => parts.push("nothing in the air".to_owned()),
    }
    parts.join(" · ")
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
        None => "—".to_owned(),
        Some(band) => format!("{}  ·  {}", band.label(), band.note()),
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
    view.map_or("—", |view| view.chassis_name())
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
            .init_resource::<ShotFeedback>();
        let world = app.world_mut();
        world.spawn((LockReticle, Transform::default(), Visibility::Hidden));
        world.spawn((LockBrackets, Transform::default(), Visibility::Inherited));
        world.spawn((LockGlow, Transform::default(), Visibility::Hidden));
        world.spawn((ImpactFlash, Transform::default(), Visibility::Hidden));
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
                    progress,
                    acquired: 0,
                },
                target: Some(craft_view(THEM, Archetype::Cruiser, 120.0)),
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
                progress: 30,
                acquired: 3,
            },
            target: Some(craft_view(THEM, Archetype::Cruiser, 120.0)),
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

        // Muzzle only: the ruleset has not given this shot a flight time yet.
        app.world_mut()
            .resource_mut::<ProjectileTracks>()
            .observe(&[shot(None)]);
        app.update();
        assert!(tracer_geometry(&mut app, 0).is_none());

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

        // Provisional arrival: drawn on the target's own body.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Arrival { target: THEM }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, at) = flash_state(&mut app);
        assert!(visible);
        assert_eq!(at, Vec3::new(100.0, 0.0, -40.0));
        // A miss verdict withdraws what the provisional layer put up.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Miss,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(!visible, "a miss must retract the flash");

        // A hit keeps it up.
        *app.world_mut().resource_mut::<ShotFeedback>() = ShotFeedback {
            cue: Some(ShotCue::Resolved {
                target: THEM,
                result: ShotResult::Hit,
            }),
            ticks_left: SHOT_CUE_TICKS,
        };
        app.update();
        let (visible, _) = flash_state(&mut app);
        assert!(visible);
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

        let held = crate::intent::Controls {
            fire: true,
            thrust: true,
            ..crate::intent::Controls::default()
        };
        let mut pending = std::collections::BTreeMap::<PersistId, Vec<_>>::new();
        for raw in 0..40u64 {
            // Ticks 0..179 are the pilot table's combat row, where the target
            // selector picks the adjacent craft rather than a rock lineage.
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
                progress: 30,
                acquired: 1,
            },
            target: None,
        };
        let line = target_title(&view);
        assert!(line.contains("no window here"), "{line}");
        assert!(line.contains("b1"), "{line}");
    }

    #[test]
    fn a_full_track_reports_the_flight_it_has_left() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[in_flight(7)]);
        let view = CombatView {
            own: Some(craft_view(ME, Archetype::Interceptor, 0.0)),
            lock: LockView {
                target: Some(THEM),
                progress: 30,
                acquired: 1,
            },
            target: Some(craft_view(THEM, Archetype::Cruiser, 250.0)),
        };
        let line = target_relation(&view, &tracks);
        assert!(line.contains("250 m"), "{line}");
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
            },
            tracks.tracks()[0]
        );
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
        assert_eq!(hit_band_line(&idle), "—");
        assert_eq!(idle.hit_forecast(), None);
    }

    /// The band travels on the same refresh path as every other readout: the
    /// whole HUD is rewritten from one `CombatView`, so it cannot go stale
    /// against the lock beside it.
    #[test]
    fn the_refresh_system_writes_the_band_line() {
        let mut app = App::new();
        app.insert_resource(locked_on(60_000))
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .add_systems(Update, refresh_combat_hud);
        let line = app
            .world_mut()
            .spawn((Readout::HitBandLine, Text::new("—"), TextColor(MUTED)))
            .id();
        app.update();
        let text = app.world().get::<Text>(line).expect("the line exists");
        assert_eq!(**text, hit_band_line(&locked_on(60_000)));
        assert!(**text != *"—", "the band line was never written");
    }
}
