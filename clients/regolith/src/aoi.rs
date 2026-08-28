//! The fog-of-war fade at the interest boundary (#533).
//!
//! # What this is, and what it is not
//!
//! A replicated body that crosses out of this client's interest set stops
//! being replicated, and [`crate::sync_rendered_state`] drops its body on the
//! frame the state goes away. Before #533 that was a hard cut: a contact drawn
//! at full fidelity one frame and absent the next, which reads as the game
//! losing track of a ship rather than as the limit of what the client can see.
//!
//! This module turns the last stretch before that boundary into a visible
//! thinning. **It is presentation only and asserts nothing** (#519): a faded
//! craft is not damaged, not cloaked, and not further away than it is. The
//! fade describes *the client's knowledge*, and nothing here is readable by
//! intent submission, range, arc, lock or collision code — the only thing this
//! module produces is a multiplier on a `StandardMaterial`'s alpha.
//!
//! # This is a fade on distance, never on staleness
//!
//! #505 expires a replica 120 ticks after its last refresh and #527 pinned
//! `campaign_slow_but_live_replica_never_expires` in deliberate opposition to
//! `campaign_replica_expires_instead_of_freezing_on_screen`. A craft that
//! simply stops being replicated must still vanish on that schedule, and it
//! does: nothing here touches `expire_stale_replicas`. Fading on staleness
//! would be the easy shortcut and would trade one of those two tests away.
//! The input to this module is a **position**, and only a position.
//!
//! # Where the boundary comes from
//!
//! It is derived from the same two facts the host uses, not invented here:
//!
//! * the interest cell edge — `orrery_games::regolith::CAMPAIGN_CELL_EDGE_M`,
//!   512 m since #532 and deliberately still 512 m after #545, which cut the
//!   weapon table to fit this edge rather than widening the edge to fit a
//!   long gun — a block that swallowed the encounter would delete the
//!   interest churn this fade exists to make legible. It is the
//!   same constant `CampaignRuntime::committed_cell` divides by when it tells
//!   the host which cell this craft is in (`campaign.rs:544`, `campaign.rs:1045`);
//! * the 27-cell topology — the AOI is the committed cell plus its 26
//!   neighbours, "one 3×3×3 cell block"
//!   (`crates/orrery_spatial/src/interest.rs:27-30`), which #532's own doc
//!   comment restates as the topology it deliberately did not change
//!   (`crates/orrery_games/src/regolith/mod.rs:94-99`).
//!
//! Two independent definitions of "the edge" is exactly what produced #499 and
//! #502, so this module takes the edge length as an argument from the session
//! rather than holding a copy, and expresses the topology as one constant.

use bevy::prelude::*;
use orrery_games::regolith::archetype::Archetype;

/// Cells per axis in the replication interest set: 27 cells is 3×3×3.
///
/// The observer's committed cell plus one ring of neighbours. See the module
/// docs for the two sources this is read from.
pub const AOI_CELL_SPAN: i32 = 3;

/// Seconds of travel the fade band is sized to cover.
///
/// The band has to be wide enough that crossing it is a legible event rather
/// than a flicker. One second at the ruleset's own speed ceiling is the
/// smallest span that guarantees that for every chassis: the fastest thing in
/// the game takes at least this long to cross it, and everything else takes
/// longer.
pub const FADE_BAND_SECONDS: f32 = 1.0;

/// The alpha a body holds exactly at the boundary, before it is dropped.
///
/// **Not zero, on purpose.** The starfield (#525) sits behind everything and
/// the craft plate is already a dark neutral; a body taken to 0.1 against that
/// is indistinguishable from background for the last third of the band, which
/// is strictly worse than a hard cut — the player loses the contact *earlier*
/// and learns nothing from it. 0.35 is the point at which the hull is still
/// clearly a hull and clearly not a full-fidelity contact. The disappearance
/// is still abrupt at the very end; what the fade buys is the second of
/// warning before it, which is the whole request.
pub const AOI_FADE_FLOOR: f32 = 0.35;

/// The fade band's width in metres, for an interest cell of `edge_m`.
///
/// `FADE_BAND_SECONDS` of the fastest chassis the ruleset defines, capped at
/// half a cell edge. The cap is what keeps the player's own craft at full
/// opacity for free rather than by a special case: the observer sits in the
/// centre cell of the block, so its distance to the block's own boundary is
/// between one and two full cell edges — always at least twice the capped
/// band. `the_observers_own_craft_never_fades` pins that.
#[must_use]
pub fn fade_band_m(edge_m: f32) -> f32 {
    let fastest_mms = Archetype::ALL
        .iter()
        .map(|archetype| archetype.limits().max_speed_mms)
        .max()
        .unwrap_or(0);
    let band = (fastest_mms as f32 / 1_000.0) * FADE_BAND_SECONDS;
    band.min(edge_m * 0.5).max(0.0)
}

/// How deep inside the interest boundary `subject` sits, in metres, for an
/// observer at `observer`.
///
/// The AOI is an axis-aligned block of `AOI_CELL_SPAN` cells per axis centred
/// on the observer's own cell, so the boundary is a box and the honest measure
/// is the distance to its nearest face. Positive inside, negative outside,
/// zero on the face. All three axes are measured because the host's own cell
/// derivation (`cell_id_from_metres`) uses all three — Regolith's craft happen
/// to fly at y = 0, but reading only x and z here would be a second, weaker
/// definition of the same boundary.
#[must_use]
pub fn depth_inside_aoi(observer: Vec3, subject: Vec3, edge_m: f32) -> f32 {
    if !edge_m.is_finite() || edge_m <= 0.0 {
        return f32::INFINITY;
    }
    #[allow(clippy::cast_precision_loss)]
    let reach = ((AOI_CELL_SPAN - 1) / 2) as f32;
    let axis = |o: f32, s: f32| {
        // The observer's cell spans [cell*edge, (cell+1)*edge); the block adds
        // `reach` whole cells on each side of it.
        let cell = (o / edge_m).floor();
        let low = (cell - reach) * edge_m;
        let high = (cell + reach + 1.0) * edge_m;
        (s - low).min(high - s)
    };
    axis(observer.x, subject.x)
        .min(axis(observer.y, subject.y))
        .min(axis(observer.z, subject.z))
}

/// The opacity multiplier for a body `depth_m` inside the boundary.
///
/// Full opacity until the last `band_m` metres, then a linear ramp down to
/// [`AOI_FADE_FLOOR`] at the face itself. A body already outside the box holds
/// the floor rather than going to zero: it is about to stop being replicated,
/// and the frame it does, its body goes with it.
///
/// **There is no zoom term here, deliberately.** The owner asked for this "at
/// low zoom" because that is the zoom at which the boundary is on screen at
/// all — not because the fade should behave differently when it is not. A
/// fade that depended on camera height would make the same craft, at the same
/// place, in the same world, look different to the same player for a reason
/// that lives in the camera; that is the skin asserting something. Zoomed in
/// far enough that the boundary is off screen, this still runs and still
/// changes nothing visible, because the bodies it would fade are not in frame.
#[must_use]
pub fn aoi_opacity(depth_m: f32, band_m: f32) -> f32 {
    if !band_m.is_finite() || band_m <= 0.0 || !depth_m.is_finite() {
        return 1.0;
    }
    let t = (depth_m / band_m).clamp(0.0, 1.0);
    AOI_FADE_FLOOR + (1.0 - AOI_FADE_FLOOR) * t
}

/// A material this body's interest-boundary fade may drive, and the finish it
/// wears at full opacity.
///
/// The base values are captured at spawn so the fade is a pure function of
/// position: re-applying it every frame from the original finish can neither
/// accumulate nor drift, and a body that walks back inland recovers exactly
/// the finish it was authored with.
#[derive(Component, Debug, Clone, Copy)]
pub struct AoiFadeable {
    /// The core entity whose position decides this material's fade.
    pub owner: orrery_protocol::PersistId,
    /// The authored `base_color`.
    pub base_color: Color,
    /// The authored `emissive`, scaled by the same factor so a glow strip
    /// cannot stay at full brightness on a hull that has faded out from
    /// under it.
    pub emissive: LinearRgba,
    /// The authored `alpha_mode`, restored whenever the fade is inert.
    pub alpha_mode: AlphaMode,
}

/// Capture `finish`'s authored look so [`sync_aoi_fade`] can drive it.
#[must_use]
pub fn fadeable(owner: orrery_protocol::PersistId, finish: &StandardMaterial) -> AoiFadeable {
    AoiFadeable {
        owner,
        base_color: finish.base_color,
        emissive: finish.emissive,
        alpha_mode: finish.alpha_mode,
    }
}

/// A body's position in metres, or `None` for anything the ruleset does not
/// place in the lattice.
#[must_use]
pub fn body_position_m(
    executor: &orrery_core::Executor<orrery_games::Regolith>,
    entity: orrery_protocol::PersistId,
) -> Option<Vec3> {
    use orrery_games::regolith::state::RegolithState;
    let pos = match executor.state(entity)? {
        RegolithState::Craft(craft) => craft.pos,
        RegolithState::Rock(rock) => rock.pos,
        RegolithState::Pickup(pickup) => pickup.pos,
        // A scheduler occupies no point in the lattice; see
        // `sync_rendered_state`, which skips it for the same reason.
        RegolithState::BloomDirector(_) => return None,
    };
    let (x, y, z) = pos.to_metres();
    #[allow(clippy::cast_possible_truncation)]
    Some(Vec3::new(x as f32, y as f32, z as f32))
}

/// The interest boundary this frame is drawn against.
///
/// Held as a resource rather than recomputed inside the fade system so the two
/// halves can be judged separately: *where the boundary is* (derived from the
/// session, and `None` whenever the session has none) and *what the fade does
/// with it* (pure geometry over the bodies on screen).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct AoiBoundary(pub Option<AoiFrame>);

/// One frame's interest boundary: the block's edge and where its centre is.
#[derive(Debug, Clone, Copy)]
pub struct AoiFrame {
    /// Interest cell edge in metres, from the session's own committed value.
    pub edge_m: f32,
    /// The observer's position; the block is centred on the cell holding it.
    pub observer: Vec3,
}

impl AoiFrame {
    /// The opacity a body at `subject` is drawn with.
    #[must_use]
    pub fn opacity_at(&self, subject: Vec3) -> f32 {
        aoi_opacity(
            depth_inside_aoi(self.observer, subject, self.edge_m),
            fade_band_m(self.edge_m),
        )
    }
}

/// Derives this frame's interest boundary from the session.
///
/// `None` — and therefore no fade at all — whenever the session has no
/// interest set: the offline sandbox holds both seats in one local executor,
/// so there is no host, no replication and no boundary, and drawing one would
/// be the skin asserting a limit the run does not have (#519).
pub fn read_aoi_boundary(session: Res<crate::ActiveSession>, mut boundary: ResMut<AoiBoundary>) {
    boundary.0 = session.aoi_edge_m().and_then(|edge_m| {
        Some(AoiFrame {
            edge_m,
            observer: body_position_m(session.executor(), session.local_entity())?,
        })
    });
}

/// What the fade actually did this frame, and the darkest it has ever gone.
///
/// **An evidence affordance, and deliberately a thin one**, in the same spirit
/// as the rock census. #533 is a claim about what the player sees, and
/// the failure mode every presentation bug in this client has had (#517, #524,
/// #514) is a correct number that never reaches a pixel — so this is recorded
/// *inside* [`sync_aoi_fade`], from the opacity that was written to the
/// material, rather than recomputed from the geometry beside it. A live run
/// that never dips below 1.0 is a run in which nothing approached the
/// boundary; a run that reports fades is the fade happening.
#[derive(Resource, Debug, Clone, Default)]
pub struct AoiFadeCensus {
    /// Whether a boundary existed this frame.
    pub bounded: bool,
    /// Bodies the fade considered this frame.
    pub bodies: usize,
    /// Bodies drawn below full opacity this frame.
    pub faded: usize,
    /// The dimmest opacity written this frame; `1.0` when nothing faded.
    pub dimmest: f32,
    /// The dimmest opacity written since the process started.
    pub dimmest_ever: f32,
    /// How many distinct owners have ever been drawn faded.
    pub ever_faded: std::collections::BTreeSet<orrery_protocol::PersistId>,
}

impl AoiFadeCensus {
    fn open_frame(&mut self, bounded: bool) {
        if self.dimmest_ever == 0.0 {
            self.dimmest_ever = 1.0;
        }
        self.bounded = bounded;
        self.bodies = 0;
        self.faded = 0;
        self.dimmest = 1.0;
    }

    fn observe(&mut self, owner: orrery_protocol::PersistId, opacity: f32) {
        self.bodies += 1;
        if opacity < 1.0 {
            self.faded += 1;
            self.ever_faded.insert(owner);
        }
        self.dimmest = self.dimmest.min(opacity);
        self.dimmest_ever = self.dimmest_ever.min(opacity);
    }

    /// One line for the F3 pane and the capture log.
    #[must_use]
    pub fn line(&self, edge_m: Option<f32>) -> String {
        match edge_m {
            None => "aoi no interest boundary (local session)".to_owned(),
            Some(edge_m) => format!(
                "aoi edge {edge_m:.0} m | band {:.0} m | {} materials, {} faded | dimmest {:.2} (ever {:.2} over {} craft)",
                fade_band_m(edge_m),
                self.bodies,
                self.faded,
                self.dimmest,
                self.dimmest_ever,
                self.ever_faded.len(),
            ),
        }
    }
}

/// Drives every [`AoiFadeable`] material from its owner's distance to the
/// interest boundary.
///
/// Runs unconditionally and does nothing at all when [`AoiBoundary`] is empty:
/// every material is then restored to its authored finish. That restore is
/// why this is safe to leave running — the fade is recomputed from the
/// captured base each frame, never accumulated onto the live material, so a
/// body that walks back inland recovers exactly the look it was spawned with.
pub fn sync_aoi_fade(
    session: Res<crate::ActiveSession>,
    boundary: Res<AoiBoundary>,
    mut census: ResMut<AoiFadeCensus>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    fadeables: Query<(&AoiFadeable, &MeshMaterial3d<StandardMaterial>)>,
) {
    census.open_frame(boundary.0.is_some());
    for (fade, handle) in &fadeables {
        let opacity = boundary.0.map_or(1.0, |frame| {
            body_position_m(session.executor(), fade.owner)
                .map_or(1.0, |subject| frame.opacity_at(subject))
        });
        census.observe(fade.owner, opacity);
        let Some(mut material) = materials.get_mut(&handle.0) else {
            continue;
        };
        material.base_color = fade
            .base_color
            .with_alpha(fade.base_color.alpha() * opacity);
        material.emissive = fade.emissive * opacity;
        material.alpha_mode = if opacity < 1.0 {
            AlphaMode::Blend
        } else {
            fade.alpha_mode
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The campaign's own edge, read rather than restated: #545 moved it and
    /// a hand-copied literal here would have silently stopped being it.
    const EDGE: f32 = orrery_games::regolith::CAMPAIGN_CELL_EDGE_M as f32;

    /// The block is 3×3×3 cells of the campaign's own edge, so its faces are
    /// where the host's interest set actually ends — not at a radius the skin
    /// picked.
    #[test]
    fn the_boundary_is_the_27_cell_block_of_the_campaigns_own_edge() {
        assert_eq!(AOI_CELL_SPAN, 3, "27 cells is 3 per axis");
        assert!(
            (EDGE - orrery_games::regolith::CAMPAIGN_CELL_EDGE_M as f32).abs() < f32::EPSILON,
            "the fixture edge must be the campaign's own"
        );
        // Observer at the origin: cell 0 spans [0, EDGE), the block spans
        // [-EDGE, 2·EDGE) on every axis.
        let observer = Vec3::ZERO;
        let depth = |x: f32| depth_inside_aoi(observer, Vec3::new(x, 0.0, 0.0), EDGE);
        assert!((depth(0.0) - EDGE).abs() < 0.5, "{}", depth(0.0));
        assert!(depth(-EDGE).abs() < 0.5, "the low face sits at -EDGE");
        assert!(
            depth(2.0 * EDGE).abs() < 0.5,
            "the high face sits at +2·EDGE"
        );
        assert!(
            depth(2.0 * EDGE + 76.0) < 0.0,
            "past the face is outside the block"
        );
    }

    /// The fade must never touch the craft the player is flying. This is a
    /// consequence of the geometry plus the band cap, not a special case, so
    /// it is worth pinning: the observer is in the centre cell and can be no
    /// closer than one whole edge to the block's boundary.
    #[test]
    fn the_observers_own_craft_never_fades() {
        let band = fade_band_m(EDGE);
        for offset in [
            0.0f32,
            1.0,
            EDGE * 0.5,
            EDGE - 0.1,
            -0.1,
            -(EDGE - 0.1),
            4_096.3,
        ] {
            let observer = Vec3::splat(offset);
            let depth = depth_inside_aoi(observer, observer, EDGE);
            assert!(
                depth >= EDGE - 1.0,
                "an observer is at least one cell edge inside its own block; got {depth} at {offset}"
            );
            assert!(
                (aoi_opacity(depth, band) - 1.0).abs() < f32::EPSILON,
                "the player's own craft must never fade"
            );
        }
    }

    /// The band is sized off the ruleset's own speed ceiling, and the floor is
    /// high enough to stay a hull rather than a smudge.
    #[test]
    fn the_fade_ramps_over_a_second_of_travel_and_stops_at_the_floor() {
        let band = fade_band_m(EDGE);
        // Derived, not written down. The literal 120.0 that stood here was
        // correct until v18 raised the interceptor ceiling to 480 m/s, at
        // which point it failed -- and it failed in a standalone workspace the
        // root `check.sh test` does not reach, so it was merged before anyone
        // saw it. Deriving the expectation from the same two inputs the
        // function uses means a future ceiling change moves both together.
        #[allow(clippy::cast_precision_loss)]
        let fastest_ms = Archetype::ALL
            .iter()
            .map(|archetype| archetype.limits().max_speed_mms)
            .max()
            .expect("Regolith publishes at least one chassis") as f32
            / 1_000.0;
        let expected = (fastest_ms * FADE_BAND_SECONDS).min(EDGE * 0.5);
        assert!(
            (band - expected).abs() < 0.5,
            "the band is {FADE_BAND_SECONDS}s of the fastest chassis \
             ({fastest_ms} m/s), capped at half a cell edge; expected \
             {expected}, got {band}"
        );
        assert!(band < EDGE * 0.5 + 0.001);
        assert!(
            (aoi_opacity(band * 4.0, band) - 1.0).abs() < f32::EPSILON,
            "well inland is full opacity"
        );
        assert!(
            (aoi_opacity(0.0, band) - AOI_FADE_FLOOR).abs() < f32::EPSILON,
            "the boundary itself sits on the floor"
        );
        assert!(
            (aoi_opacity(-50.0, band) - AOI_FADE_FLOOR).abs() < f32::EPSILON,
            "outside holds the floor rather than going to zero"
        );
        let half = aoi_opacity(band * 0.5, band);
        assert!(
            half > AOI_FADE_FLOOR && half < 1.0,
            "the ramp is monotonic through the band; got {half}"
        );
        const {
            assert!(
                AOI_FADE_FLOOR >= 0.3,
                "a floor below 0.3 is indistinguishable from the starfield"
            );
        }
    }

    /// The whole point of #533, at the level the player actually sees: a body
    /// approaching the interest boundary is *drawn* dimmer, a body inland is
    /// drawn exactly as authored, and the fade is released the moment there is
    /// no boundary. Asserting the geometry alone would not catch a fade that
    /// computes a correct opacity and never reaches a material — which is the
    /// shape every presentation bug in this client has had (#517, #524, #514).
    #[test]
    fn a_body_near_the_interest_boundary_is_drawn_dimmer_than_one_inland() {
        use bevy::asset::AssetPlugin;
        use orrery_core::{QPos, QVel};
        use orrery_games::regolith::state::{Rock, RockTier};
        use orrery_protocol::PersistId;

        let inland = PersistId::new(50);
        let at_edge = PersistId::new(51);
        let mut local = crate::LocalSession::default();
        // 24 m inside the block's high face, wherever that face now is.
        for (entity, x) in [(inland, 0.0), (at_edge, (2.0 * EDGE - 24.0) as f64)] {
            local.executor.insert(
                entity,
                orrery_games::regolith::state::RegolithState::Rock(Rock::spawned(
                    RockTier::Large,
                    0,
                    QPos::from_metres(x, 0.0, 0.0),
                    QVel::default(),
                )),
            );
        }

        let mut app = App::new();
        app.add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .insert_resource(crate::ActiveSession::Local(Box::new(local)))
            // Observer at the origin: its cell spans [0, EDGE), so the
            // 27-cell block spans [-EDGE, 2·EDGE). The far body is 24 m
            // inside the high face, a fifth of the way through the 120 m band.
            .insert_resource(AoiBoundary(Some(AoiFrame {
                edge_m: EDGE,
                observer: Vec3::ZERO,
            })))
            .init_resource::<AoiFadeCensus>()
            .add_systems(Update, sync_aoi_fade);

        let authored = Color::srgb(0.40, 0.37, 0.34);
        for entity in [inland, at_edge] {
            let handle = app
                .world_mut()
                .resource_mut::<Assets<StandardMaterial>>()
                .add(StandardMaterial {
                    base_color: authored,
                    ..Default::default()
                });
            let finish = StandardMaterial {
                base_color: authored,
                ..Default::default()
            };
            app.world_mut().spawn((
                crate::CoreEntity(entity),
                fadeable(entity, &finish),
                MeshMaterial3d(handle),
            ));
        }

        let alphas = |app: &mut App| {
            let mut found: Vec<(u64, f32, bool)> = app
                .world_mut()
                .query::<(&crate::CoreEntity, &MeshMaterial3d<StandardMaterial>)>()
                .iter(app.world())
                .map(|(core, handle)| (core.0, handle.0.clone()))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|(core, handle)| {
                    let materials = app.world().resource::<Assets<StandardMaterial>>();
                    let material = materials.get(&handle).expect("material");
                    (
                        core.0,
                        material.base_color.alpha(),
                        matches!(material.alpha_mode, AlphaMode::Blend),
                    )
                })
                .collect();
            found.sort_by_key(|(entity, _, _)| *entity);
            found
        };

        app.update();
        let seen = alphas(&mut app);
        assert_eq!(seen.len(), 2);
        assert!(
            (seen[0].1 - 1.0).abs() < 1e-3 && !seen[0].2,
            "an inland body keeps its authored finish; got {seen:?}"
        );
        let expected = AOI_FADE_FLOOR + (1.0 - AOI_FADE_FLOOR) * (24.0 / fade_band_m(EDGE));
        assert!(
            (seen[1].1 - expected).abs() < 1e-3,
            "a body 24 m inside the face must be drawn at {expected}; got {seen:?}"
        );
        assert!(
            seen[1].2,
            "a faded body needs a blended pass or the alpha does nothing"
        );
        assert!(
            seen[1].1 >= AOI_FADE_FLOOR,
            "the fade may never go below the legibility floor"
        );
        // The live-evidence readout must report what actually reached the
        // materials, or a green run proves nothing about the pixels.
        let census = app.world().resource::<AoiFadeCensus>().clone();
        assert_eq!((census.bodies, census.faded), (2, 1), "{census:?}");
        assert!((census.dimmest - expected).abs() < 1e-3, "{census:?}");
        assert!(census.line(Some(EDGE)).contains("1 faded"), "{census:?}");

        // No boundary — the offline sandbox, or a campaign still dialling —
        // restores every material rather than leaving the last fade burnt in.
        app.world_mut().resource_mut::<AoiBoundary>().0 = None;
        app.update();
        for (entity, alpha, blended) in alphas(&mut app) {
            assert!(
                (alpha - 1.0).abs() < 1e-3 && !blended,
                "entity {entity} must recover its authored finish"
            );
        }
    }

    /// The offline sandbox has no host and no interest set, so it has no
    /// boundary to fade against.
    #[test]
    fn a_local_sandbox_session_has_no_interest_boundary() {
        let mut app = App::new();
        app.insert_resource(crate::ActiveSession::Local(Box::default()))
            .init_resource::<AoiBoundary>()
            .add_systems(Update, read_aoi_boundary);
        app.update();
        assert!(
            app.world().resource::<AoiBoundary>().0.is_none(),
            "a local session must not draw a boundary it does not have"
        );
    }

    /// The measure is the distance to the nearest **face**, so a body near a
    /// corner fades on whichever axis runs out first.
    #[test]
    fn the_nearest_face_on_any_axis_decides_the_fade() {
        // The observer's cell spans [0, EDGE), so the block's low faces sit
        // at -EDGE and its high faces at 2·EDGE. Each probe is 12 m inside
        // one of them and deep inland on every other axis.
        let observer = Vec3::new(100.0, 0.0, 100.0);
        let near_x = Vec3::new(-EDGE + 12.0, 0.0, 100.0);
        let near_z = Vec3::new(100.0, 0.0, -EDGE + 12.0);
        assert!(depth_inside_aoi(observer, near_x, EDGE) < 20.0);
        assert!(depth_inside_aoi(observer, near_z, EDGE) < 20.0);
        // y is measured too: a body lifted out of the deck plane leaves the
        // block through its top face like any other.
        let high_y = Vec3::new(100.0, 2.0 * EDGE - 4.0, 100.0);
        assert!(depth_inside_aoi(observer, high_y, EDGE) < 20.0);
    }
}
