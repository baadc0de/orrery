//! A world-anchored starfield, so that flying reads as travel.
//!
//! A starfield pinned to the camera is wallpaper: it moves with you and
//! therefore says nothing. The requirement (#525) is that the stars stay put
//! in **world coordinates** while the ship moves through them, which is what
//! produces the sense of travel at all.
//!
//! ## The precision problem, and the tiling that solves it
//!
//! Craft positions are in millimetres of lattice and the campaign crowd orbits
//! at ~2.5 km, but nothing bounds how far a session can travel. A naive mesh
//! of stars scattered over a huge world extent is exactly the thing that
//! jitters: `f32` has 24 bits of mantissa, so at 100 km out the representable
//! step is already ~6 mm and the sub-metre offsets between a star and the
//! camera are computed as the difference of two large numbers.
//!
//! So the field is **tiled**. Each layer owns one tile mesh whose stars live in
//! `0.0..tile_m` local coordinates, and a [`STAR_GRID`] square of instances of
//! that mesh is kept around the camera. Every instance's translation is an
//! exact integer multiple of `tile_m` ([`tile_origin`]), so:
//!
//! * a star's world position is `k * tile_m + local`, which is the same value
//!   every frame — a star does not drift, because nothing recomputes it;
//! * an instance's translation changes only in whole tile steps, and when it
//!   does the tile lands on a lattice point the field already contained, so
//!   the recycling is invisible;
//! * the vertex coordinates the GPU interpolates never exceed one tile,
//!   whatever the world coordinate is, so the precision of a star's position
//!   on screen does not degrade with distance from the origin.
//!
//! The cost, stated plainly: the field **repeats** with the tile period. At
//! 3/9/30 km periods, three layers that repeat at different rates and a field
//! this sparse, the repeat is not something the eye picks up in play — but it
//! is a repeat, not a truly unbounded sky, and a player who flew a straight
//! line for a few minutes at max zoom could in principle notice it.
//!
//! ## Depth
//!
//! One plane of world-fixed stars gives motion, but flat motion. Three layers
//! sit at increasing depth *behind the deck plane*, and the parallax is then
//! simply what a perspective camera does: a layer at distance `d` from the
//! camera shifts on screen at `1/d`, so relative to the deck at height `h` a
//! layer at depth `D` moves at `h / (h + D)`. At the default 900 m that is
//! 0.31, 0.13 and 0.04 of the deck's rate. No special-casing, and it stays
//! coherent across the zoom range for the same reason.
//!
//! ## The smear, and why it is exactly `speed × exposure`
//!
//! Perspective parallax alone is a *rate*, and a rate is only legible once the
//! eye has watched it for a while. The readout the player actually needs is
//! instantaneous, because the thing being read out is thrust: after the speed
//! caps rise, an interceptor holding 50 m/s and the same interceptor holding
//! 450 m/s have to be one glance apart. So every star is drawn as a **smear**
//! along the craft's own replicated velocity.
//!
//! The length is not a tuned number. A star's apparent motion comes entirely
//! from the camera translating parallel to the layer's plane, so over a window
//! of `T` seconds the star traces, *in that layer's own plane*, a segment of
//! exactly `speed × T` metres. Drawing that segment is drawing the star's real
//! trail:
//!
//! ```text
//! smear_m       = speed_ms * STAR_EXPOSURE_SECONDS
//! screen_px     = smear_m * viewport_px / (2 * tan(fov/2) * (height + depth))
//! ```
//!
//! The `(height + depth)` denominator is the same one the parallax rate comes
//! from, so this needs no per-layer tuning and no zoom term: a near star's
//! smear is long, a deep star's smear is short, in the same 0.31 / 0.13 / 0.04
//! ratio their motion already had. **The parallax itself became directional.**
//!
//! `T` is [`STAR_EXPOSURE_SECONDS`], which is
//! [`crate::hud::TRACER_PERSISTENCE_TICKS`] over `TICK_HZ` — the same 0.2 s
//! window #518's tracer trails already stand for, described there as "roughly
//! how long the eye integrates a moving light". A tracer streak and a star
//! streak then mean the same thing on the same screen: *this far per blink*.
//!
//! ## Speed is length; thrust is light
//!
//! Length alone would make acceleration merely the derivative of something the
//! player has to watch change. The second channel is brightness: the field
//! rests at [`STAR_REST_LEVEL`] of each layer's own grey, and at the full grey
//! and half again as wide when the craft is gaining speed at its chassis's
//! published acceleration ceiling. Width is in there because at a pixel and a
//! half across it is coverage, not declared colour, that decides how bright a
//! star actually renders — see [`STAR_FLARE_WIDTH_GAIN`]. The two readouts are independent, which is the whole
//! point of the physics they serve — **length is velocity, brightness is
//! force**. Coasting fast is long and dim; opening the throttle from a stop is
//! short and bright; and because Regolith's drag pulls back proportionally to
//! speed, holding full thrust at terminal velocity settles the light back down
//! on its own. Nothing models that: it falls out of measuring the craft's
//! actual change of speed.
//!
//! ## Presentation only
//!
//! Nothing here reads or writes simulation state, and nothing here is a source
//! of truth (#519). Specifically:
//!
//! * a star's **centre stays exactly where it was**, and is the bright end of
//!   its own mark. The rest of the mark covers only the screen positions the
//!   star's image passed through while the camera crossed them — see
//!   [`smear_corners`] for the derivation of which way that runs — so nothing
//!   is drawn where the image has not been. The tile lattice in
//!   [`tile_translation`] is untouched by any of this;
//! * the only inputs are the own craft's replicated `QVel` and its own
//!   `Archetype::limits()`. The speed the smear stands for is **clamped to
//!   that chassis's published ceiling** ([`smear_length_m`]), so the sky can
//!   never say the craft is going faster than the ruleset allows it to — and
//!   when the ruleset raises the ceiling, the clamp rises with it, because
//!   there is no copy of the number here;
//! * with no own craft to follow there is **no smear at all**, rather than a
//!   remembered one: a client that has lost its craft has no velocity to draw;
//! * the quantity shown is the one the HUD already prints in full precision
//!   (`hud.rs`'s `speed_ms` / `max_speed_ms` readout), at far lower precision.
//!   There is no tactical fact readable here that is not already on screen as
//!   a number, and heading is *not* an input — [`CraftView::yaw_urad`] is what
//!   the firing arc is adjudicated against and it is deliberately not what the
//!   field streaks along. Velocity is.
//!
//! The filters that smooth the velocity are interpolation, which the skin may
//! do freely; the ceiling clamp is applied to the filtered value, so the
//! smoothing cannot carry the field past the ruleset's own bound.
//!
//! The finish is flat `unlit` grey — deliberately not the emissive glow, which
//! the skin reserves for exhaust — and every layer is dimmer than
//! [`crate::hud::TRACER`], so #518's thin pale tracers, the range rings, the
//! arc wedge and the reticle all keep the top of the value range. Brightness
//! is carried in vertex colours, which Bevy's PBR fragment shader multiplies
//! *into* the material's base colour, so the flare can only ever dim a layer
//! towards its rest level — it cannot outshine the grey the layer declares.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};

use crate::combat::CraftView;

/// The empty sky between the stars.
///
/// Not pure black: the HUD's own panel fill is a very dark blue-grey, and a
/// pure black ground makes that panel read as a lighter box floating on
/// nothing. This is a shade under the panel, on the same hue.
pub const SPACE: Color = Color::srgb(0.031, 0.035, 0.055);

/// One depth layer of the field.
#[derive(Debug, Clone, Copy)]
pub struct StarLayer {
    /// How far behind the deck plane the layer sits, in metres.
    pub depth_m: f32,
    /// The world period of the layer's tile, in metres.
    pub tile_m: f32,
    /// Stars in one tile.
    pub stars: usize,
    /// Half the edge of one star's quad, in metres.
    ///
    /// Scaled with depth so every layer draws stars of about the same apparent
    /// size — roughly a pixel and a half at the default zoom on a 1080-line
    /// window — rather than the deep layers vanishing.
    pub half_size_m: f32,
    /// The layer's flat grey. Dimmer with depth, and every one of them well
    /// under the tracer white.
    pub grey: f32,
}

/// The three layers, nearest first. See the module docs for the parallax
/// arithmetic these numbers produce.
pub const STAR_LAYERS: [StarLayer; 3] = [
    StarLayer {
        depth_m: 2_000.0,
        tile_m: 3_000.0,
        stars: 120,
        half_size_m: 1.7,
        grey: 0.62,
    },
    StarLayer {
        depth_m: 6_000.0,
        tile_m: 9_000.0,
        stars: 220,
        half_size_m: 4.0,
        grey: 0.42,
    },
    StarLayer {
        depth_m: 20_000.0,
        tile_m: 30_000.0,
        stars: 380,
        half_size_m: 12.0,
        grey: 0.28,
    },
];

/// Tiles per side kept around the camera, per layer.
///
/// Odd, so one tile is the camera's own and the rest surround it. Seven gives
/// at least three whole tiles of cover in every direction, which has to clear
/// the visible half-*width* at full zoom-out — the widest case, since the
/// window is wider than it is tall. Asserted by
/// `the_field_covers_the_screen_at_full_zoom_out`.
pub const STAR_GRID: i32 = 7;

/// The widest aspect ratio the coverage arithmetic is checked against.
///
/// Ultrawide, deliberately: a 21:9 window is the one that can see furthest
/// sideways, and a starfield that runs out at the edge of the screen is worse
/// than no starfield.
pub const STAR_COVER_ASPECT: f32 = 21.0 / 9.0;

/// The exposure window one smear stands for, in seconds.
///
/// Not a free parameter: it is #518's tracer persistence, `12 / 60 = 0.2 s`,
/// which that module documents as roughly how long the eye integrates a moving
/// light. Sharing it means a tracer streak and a star streak on the same
/// screen mean the same thing — *this far per blink*.
pub const STAR_EXPOSURE_SECONDS: f32 =
    crate::hud::TRACER_PERSISTENCE_TICKS as f32 / orrery_core::TICK_HZ as f32;

/// What fraction of a layer's own grey the field holds when nothing is
/// gaining speed.
///
/// The remaining fraction is the thrust flare. It is a dim rather than a
/// brighten because vertex colours multiply into the material's base colour:
/// the layer's declared grey stays the ceiling, so #518's tracers keep the top
/// of the value range whatever the throttle is doing.
pub const STAR_REST_LEVEL: f32 = 0.62;

/// How dark the trailing end of a fully stretched smear goes, as a fraction of
/// its head.
///
/// A smear drawn as a flat bar reads as a longer star, not as motion. Fading
/// the tail is what makes it read as a trail, and it puts the brightest point
/// of the mark on the star's own true position rather than in the middle of
/// something invented behind it.
pub const STAR_TAIL_LEVEL: f32 = 0.3;

/// How much wider a fully stretched smear is drawn than the star it came from.
///
/// A 1.7 m star is about a pixel and a half across at the default zoom, and a
/// mark that long and that thin crawls in and out of the raster as it moves.
/// Widening the smear as it stretches keeps it a mark rather than a shimmer,
/// and costs nothing in honesty: width says nothing.
pub const STAR_SMEAR_WIDTH_GAIN: f32 = 0.7;

/// How much wider a mark is drawn at full thrust.
///
/// Measured, not guessed: at the sizes this field draws, a star covers a pixel
/// and a half, so how bright it *renders* is set by how much of the pixel it
/// covers rather than by the colour it declares. A value-only flare moved the
/// captured peak from 22 to 25 of 255 — real, but not something a player
/// notices while flying. Width is the lever that actually reaches the screen,
/// and it says nothing: a wider mark is not a longer one, a faster one, or one
/// anywhere else.
pub const STAR_FLARE_WIDTH_GAIN: f32 = 0.5;

/// The slowest filtered speed the field will take a direction from, in metres
/// per second.
///
/// A millimetre per second. Under it the filtered vector is numerically
/// nothing and normalising it yields noise, so the last real direction is held
/// — which is invisible either way, because a smear that short is a square.
pub const STAR_DIRECTION_FLOOR_MS: f32 = 1.0e-3;

/// Time constant of the fast velocity filter, in seconds.
///
/// Short enough that the length reads as instantaneous — a fifth of the
/// exposure window it feeds — and long enough that the field does not step
/// once per 60 Hz tick while the renderer runs faster than that.
pub const STAR_DRIFT_FAST_S: f32 = 0.04;

/// Time constant of the slow velocity filter, in seconds.
///
/// Only the *gap* between the two filters is used, and for a steady
/// acceleration `a` that gap settles at `a * (slow - fast)`. So this number
/// sets nothing but how much of a burn has to have happened before the flare
/// is at full — a third of a second.
pub const STAR_DRIFT_SLOW_S: f32 = 0.36;

/// Marks one tile instance of one layer.
#[derive(Component, Debug, Clone, Copy)]
pub struct StarTile {
    /// Index into [`STAR_LAYERS`].
    pub layer: usize,
    /// The tile's offset from the camera's own tile, in whole tiles.
    pub offset: IVec2,
}

/// One star in a tile: where its centre is and how big it is.
///
/// Kept on the CPU so the smear can be rebuilt each frame *around* the centre
/// without ever recomputing the centre itself. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    /// The star's centre in the tile's own `0.0..tile_m` coordinates.
    pub centre: Vec2,
    /// Half the edge of the star's unsmeared quad, in metres.
    pub half_m: f32,
}

/// The mesh each layer's tiles share, and the stars it was built from.
#[derive(Resource, Debug, Clone)]
pub struct StarMeshes {
    /// One entry per [`STAR_LAYERS`] entry, in the same order.
    pub layers: Vec<(Handle<Mesh>, Vec<Star>)>,
    /// The `(direction, length_m, flare)` last written, so a parked client
    /// does not re-upload three meshes every frame for no change.
    pub written: Option<(Vec2, f32, f32)>,
}

/// The world coordinate of the tile containing `position`.
///
/// Always an exact integer multiple of `tile_m`, which is what keeps a star's
/// world position identical from frame to frame. See the module docs.
#[must_use]
pub fn tile_origin(position: f32, tile_m: f32) -> f32 {
    (position / tile_m).floor() * tile_m
}

/// A tiny deterministic hash, so the sky is the same sky every run without
/// pulling in an RNG dependency the client does not otherwise need.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(state: &mut u64) -> f32 {
    (splitmix(state) >> 40) as f32 / f32::from(1u16 << 8) / 65_536.0
}

/// The stars of one layer's tile, scattered deterministically from `seed`.
#[must_use]
pub fn layer_stars(layer: StarLayer, seed: u64) -> Vec<Star> {
    let mut state = seed;
    (0..layer.stars)
        .map(|_| {
            let x = unit(&mut state) * layer.tile_m;
            let z = unit(&mut state) * layer.tile_m;
            // A little size variance so the field does not read as a regular
            // stipple. Never larger than the layer's nominal size.
            let half_m = layer.half_size_m * (0.45 + 0.55 * unit(&mut state));
            Star {
                centre: Vec2::new(x, z),
                half_m,
            }
        })
        .collect()
}

/// The own craft's replicated horizontal velocity, filtered for drawing.
///
/// Two first-order filters over the same input. The fast one carries the
/// direction and the length; the difference between the two is what stands in
/// for "is this craft gaining speed", which is the flare. Neither is allowed
/// to reach the drawn geometry without passing the ceiling clamp in
/// [`smear_length_m`].
#[derive(Resource, Debug, Clone, Copy)]
pub struct StarDrift {
    /// Fast filter of the replicated velocity, in metres per second.
    pub fast_ms: Vec2,
    /// Slow filter of the same.
    pub slow_ms: Vec2,
    /// The last direction the field had a definite one, held through a stop so
    /// a stationary star is a square in *some* orientation rather than
    /// flickering between orientations at zero length.
    pub along: Vec2,
    /// Whether the client currently holds a craft to follow at all.
    pub crewed: bool,
}

impl Default for StarDrift {
    fn default() -> Self {
        Self {
            fast_ms: Vec2::ZERO,
            slow_ms: Vec2::ZERO,
            along: Vec2::X,
            crewed: false,
        }
    }
}

/// One first-order step towards `target`, framerate-independent.
fn filter(current: Vec2, target: Vec2, dt_s: f32, tau_s: f32) -> Vec2 {
    if dt_s <= 0.0 || tau_s <= 0.0 || !dt_s.is_finite() {
        return current;
    }
    let alpha = (1.0 - (-dt_s / tau_s).exp()).clamp(0.0, 1.0);
    current + (target - current) * alpha
}

impl StarDrift {
    /// Folds one frame's replicated velocity in.
    ///
    /// `velocity_ms` is `None` when this client holds no craft — between the
    /// join handshake and the first replicated state, or while dead. That is
    /// not "velocity zero, smoothly": it is *no statement*, and the filters
    /// are cleared rather than decayed so the field cannot keep drawing motion
    /// on the strength of a craft that is no longer there.
    pub fn observe(&mut self, velocity_ms: Option<Vec2>, dt_s: f32) {
        let Some(velocity) = velocity_ms.filter(|v| v.is_finite()) else {
            self.crewed = false;
            self.fast_ms = Vec2::ZERO;
            self.slow_ms = Vec2::ZERO;
            return;
        };
        self.crewed = true;
        self.fast_ms = filter(self.fast_ms, velocity, dt_s, STAR_DRIFT_FAST_S);
        self.slow_ms = filter(self.slow_ms, velocity, dt_s, STAR_DRIFT_SLOW_S);
        // Below a millimetre per second there is no direction to take: the
        // filtered vector is numerically nothing, and normalising it produces
        // noise rather than a heading. The last real one is held instead.
        let speed = self.fast_ms.length();
        if speed > STAR_DIRECTION_FLOOR_MS {
            self.along = self.fast_ms / speed;
        }
    }

    /// The filtered speed the smear is drawn from, in metres per second.
    #[must_use]
    pub fn speed_ms(&self) -> f32 {
        if self.crewed {
            self.fast_ms.length()
        } else {
            0.0
        }
    }

    /// How fast the craft is *gaining* speed, in metres per second squared.
    ///
    /// The gap between two filters of the same signal is `a * (slow - fast)`
    /// for a steady `a`, so this is that gap divided back out. Only the
    /// positive part: Regolith's controls thrust forward and nothing else, so
    /// gaining speed is thrusting, while losing it is drag — a force, but not
    /// one the player is applying, and lighting the sky for it would say the
    /// opposite of what the flare means.
    #[must_use]
    pub fn gaining_mss(&self) -> f32 {
        if !self.crewed {
            return 0.0;
        }
        let gap = self.fast_ms.length() - self.slow_ms.length();
        (gap / (STAR_DRIFT_SLOW_S - STAR_DRIFT_FAST_S)).max(0.0)
    }
}

/// How long a star's smear is drawn, in metres of the layer's own plane.
///
/// `speed × exposure`, with the speed **clamped to the chassis's own published
/// ceiling** before it is used. That clamp is the whole authority argument for
/// this module: the filters above may lag, overshoot on a spike, or be handed
/// a garbage sample, and the field still cannot draw a craft moving faster
/// than `Archetype::limits().max_speed_mms` says it can. There is no copy of
/// that number here, so raising the ceiling raises the clamp.
#[must_use]
pub fn smear_length_m(speed_ms: f32, max_speed_ms: f32) -> f32 {
    if !speed_ms.is_finite() || !max_speed_ms.is_finite() || max_speed_ms <= 0.0 {
        return 0.0;
    }
    speed_ms.clamp(0.0, max_speed_ms) * STAR_EXPOSURE_SECONDS
}

/// How hard the field is lit, `0.0` coasting and `1.0` at the chassis's own
/// acceleration ceiling.
#[must_use]
pub fn thrust_flare(gaining_mss: f32, max_accel_mss: f32) -> f32 {
    if !gaining_mss.is_finite() || !max_accel_mss.is_finite() || max_accel_mss <= 0.0 {
        return 0.0;
    }
    (gaining_mss / max_accel_mss).clamp(0.0, 1.0)
}

/// How stretched a smear is, `0.0` at rest and approaching `1.0` for a mark
/// far longer than the star it came from.
///
/// Dimensionless and self-scaling — `smear / (star + smear)` — so it needs no
/// per-layer reference length. It drives the tail fade and the width gain,
/// both of which are appearance and neither of which is a quantity.
#[must_use]
pub fn smear_stretch(half_m: f32, length_m: f32) -> f32 {
    let span = 2.0 * half_m + length_m;
    if span <= 0.0 {
        return 0.0;
    }
    (length_m / span).clamp(0.0, 1.0)
}

/// How wide a mark is drawn, in metres, for a star of `half_m`.
///
/// Two gains, both appearance: stretching widens it so a long thin mark stays
/// a mark rather than a shimmer, and thrust widens it because coverage is what
/// carries brightness at this size. Neither is a quantity.
#[must_use]
pub fn smear_width_m(half_m: f32, length_m: f32, flare: f32) -> f32 {
    let stretch = smear_stretch(half_m, length_m);
    half_m * (1.0 + STAR_SMEAR_WIDTH_GAIN * stretch + STAR_FLARE_WIDTH_GAIN * flare.clamp(0.0, 1.0))
}

/// The widest a mark may ever be drawn, as a multiple of the star's own
/// half-edge. Both gains at once.
pub const STAR_MAX_WIDTH_GAIN: f32 = 1.0 + STAR_SMEAR_WIDTH_GAIN + STAR_FLARE_WIDTH_GAIN;

/// The four corners of one star's mark, in the tile's own XZ plane.
///
/// Returned in the winding [`star_tile_mesh`] indexes, and at zero length they
/// are the same four corners the unsmeared field always had.
///
/// **Which way the mark runs, derived rather than guessed.** The star is
/// fixed and the camera translates by `speed × T` along `along` during the
/// exposure, so the star's *image* sweeps backwards across the screen by that
/// much. Expressed as world points at the camera's pose **now**, the positions
/// its image passed through run from the star itself forward along the
/// direction of travel — so the mark spans `centre - half` to
/// `centre + half + length` along `along`, and on screen that reads as a trail
/// streaming away opposite the direction of flight, which is the direction the
/// whole field is visibly moving.
///
/// The star's own centre is **not** moved, and the `-along` end — the bright
/// one — is the star's true point. Everything else covers only screen
/// positions the star's image genuinely occupied while the camera crossed
/// them. Nothing is drawn where the image has not been.
#[must_use]
pub fn smear_corners(
    centre: Vec2,
    half_m: f32,
    along: Vec2,
    length_m: f32,
    flare: f32,
) -> [Vec2; 4] {
    let along = along.try_normalize().unwrap_or(Vec2::X);
    let across = Vec2::new(-along.y, along.x);
    let length = length_m.max(0.0);
    let width = smear_width_m(half_m, length, flare);
    let head = -half_m;
    let tail = half_m + length;
    [
        centre + along * head - across * width,
        centre + along * tail - across * width,
        centre + along * tail + across * width,
        centre + along * head + across * width,
    ]
}

/// The brightness multiplier at a mark's leading and trailing ends.
///
/// Both are fractions of the layer's declared grey and neither can exceed it,
/// which is what keeps the whole field under the tracer white however hard the
/// player is burning.
#[must_use]
pub fn smear_levels(stretch: f32, flare: f32) -> (f32, f32) {
    let head = STAR_REST_LEVEL + (1.0 - STAR_REST_LEVEL) * flare.clamp(0.0, 1.0);
    let tail = head * (1.0 - (1.0 - STAR_TAIL_LEVEL) * stretch.clamp(0.0, 1.0));
    (head, tail)
}

/// Positions and vertex colours for one layer's tile mesh.
///
/// Split out from the mesh so the geometry and the light can be asserted
/// without a render device.
#[must_use]
pub fn tile_vertices(
    stars: &[Star],
    along: Vec2,
    length_m: f32,
    flare: f32,
) -> (Vec<[f32; 3]>, Vec<[f32; 4]>) {
    let mut positions = Vec::with_capacity(stars.len() * 4);
    let mut colours = Vec::with_capacity(stars.len() * 4);
    for star in stars {
        let corners = smear_corners(star.centre, star.half_m, along, length_m, flare);
        let (head, tail) = smear_levels(smear_stretch(star.half_m, length_m), flare);
        // The corner order out of `smear_corners` is head, tail, tail, head.
        for (corner, level) in corners.into_iter().zip([head, tail, tail, head]) {
            positions.push([corner.x, 0.0, corner.y]);
            colours.push([level, level, level, 1.0]);
        }
    }
    (positions, colours)
}

/// One layer's tile: `layer.stars` camera-facing quads in `0.0..tile_m`.
///
/// The quads lie in the XZ plane facing `+Y`. The chase camera looks straight
/// down, so that is exactly face-on with no billboarding work per frame. Built
/// unsmeared and at rest; [`write_smear`] rewrites the positions and colours
/// each frame the drawn velocity changes.
#[must_use]
pub fn star_tile_mesh(stars: &[Star]) -> Mesh {
    let (positions, colours) = tile_vertices(stars, Vec2::X, 0.0, 0.0);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(stars.len() * 4);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(stars.len() * 4);
    let mut indices: Vec<u32> = Vec::with_capacity(stars.len() * 6);
    for star in 0..stars.len() {
        let base = u32::try_from(star * 4).expect("a tile holds far fewer than u32::MAX stars");
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([u, v]);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh::new(
        PrimitiveTopology::TriangleList,
        // MAIN_WORLD as well as RENDER_WORLD, deliberately: `write_smear`
        // rewrites these vertices every frame the drawn velocity moves, and a
        // RENDER_WORLD-only mesh has had its data dropped from the main world
        // by then.
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colours)
    .with_inserted_indices(Indices::U32(indices))
}

/// Rewrites one tile mesh's marks for the velocity now being drawn.
///
/// Only the positions and colours move. The index buffer, the normals and the
/// UVs are the same every frame, and every star's centre is the one
/// [`layer_stars`] scattered once at startup.
pub fn write_smear(mesh: &mut Mesh, stars: &[Star], along: Vec2, length_m: f32, flare: f32) {
    let (positions, colours) = tile_vertices(stars, along, length_m, flare);
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colours);
}

/// Spawns every tile of every layer. Positions are written by
/// [`sync_starfield`] on the first frame.
pub fn spawn_starfield(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let half = STAR_GRID / 2;
    let mut built: Vec<(Handle<Mesh>, Vec<Star>)> = Vec::with_capacity(STAR_LAYERS.len());
    for (index, layer) in STAR_LAYERS.iter().enumerate() {
        let stars = layer_stars(*layer, 0x5EED_0000 + index as u64);
        let mesh = meshes.add(star_tile_mesh(&stars));
        let material = materials.add(StandardMaterial {
            base_color: Color::srgb(layer.grey, layer.grey, layer.grey * 1.06),
            // Flat, never the emissive glow finish the skin reserves for
            // exhaust, and unaffected by the scene's directional light.
            unlit: true,
            // The quads are single-sided and authored facing +Y; not culling
            // removes any chance of a winding mistake blanking the sky — and
            // a mark whose leading end points into -X is wound the other way
            // round, so with culling on, half of every heading would vanish.
            cull_mode: None,
            ..Default::default()
        });
        for x in -half..=half {
            for z in -half..=half {
                commands.spawn((
                    StarTile {
                        layer: index,
                        offset: IVec2::new(x, z),
                    },
                    Mesh3d(mesh.clone()),
                    MeshMaterial3d(material.clone()),
                    Transform::default(),
                ));
            }
        }
        built.push((mesh, stars));
    }
    commands.insert_resource(StarMeshes {
        layers: built,
        written: None,
    });
}

/// Where one tile instance sits, given where the camera is.
#[must_use]
pub fn tile_translation(tile: StarTile, camera: Vec3) -> Vec3 {
    let layer = STAR_LAYERS[tile.layer];
    Vec3::new(
        tile_origin(camera.x, layer.tile_m) + tile.offset.x as f32 * layer.tile_m,
        -layer.depth_m,
        tile_origin(camera.z, layer.tile_m) + tile.offset.y as f32 * layer.tile_m,
    )
}

/// Recycles the tiles around the camera, on the world lattice.
///
/// A tile's translation is an exact multiple of its layer's period, so this
/// either writes the same value it wrote last frame or moves a tile by a whole
/// period onto a lattice point the field already occupied. Neither is visible;
/// what is visible is the stars staying where they are while the ship flies.
pub fn sync_starfield(
    camera: Query<&Transform, (With<crate::ChaseCamera>, Without<StarTile>)>,
    mut tiles: Query<(&StarTile, &mut Transform), Without<crate::ChaseCamera>>,
) {
    let Ok(view) = camera.single() else {
        return;
    };
    let eye = view.translation;
    for (tile, mut transform) in &mut tiles {
        transform.translation = tile_translation(*tile, eye);
    }
}

/// The horizontal velocity a craft is replicated with, in metres per second.
///
/// Only X and Z: the chase camera looks straight down, so a Y component
/// produces no apparent motion for a smear to stand for, and inventing one
/// would be the field asserting a direction the screen cannot show.
#[must_use]
pub fn drawn_velocity_ms(own: Option<&CraftView>) -> Option<Vec2> {
    let own = own?;
    let (x, _, z) = own.vel.to_metres_per_sec();
    Some(Vec2::new(x as f32, z as f32))
}

/// Drives the smear from the own craft's replicated velocity.
///
/// Reads [`crate::CombatView`], which is itself a straight copy of hashed
/// state, and writes nothing but mesh vertices.
pub fn drive_star_smear(
    time: Res<Time>,
    view: Res<crate::CombatView>,
    mut drift: ResMut<StarDrift>,
    mut field: Option<ResMut<StarMeshes>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let own = view.own;
    drift.observe(drawn_velocity_ms(own.as_ref()), time.delta_secs());
    let Some(field) = field.as_mut() else {
        return;
    };
    #[allow(clippy::cast_possible_truncation)]
    let (max_speed_ms, max_accel_mss) = own.map_or((0.0, 0.0), |craft| {
        (craft.max_speed_ms() as f32, craft.max_accel_mss() as f32)
    });
    let length_m = smear_length_m(drift.speed_ms(), max_speed_ms);
    let flare = thrust_flare(drift.gaining_mss(), max_accel_mss);
    let along = drift.along;

    // A parked client re-uploading three meshes a frame is pure waste, and the
    // thresholds are far under anything a pixel could show: a millimetre of
    // length, and a thousandth of the direction or the light.
    if let Some((was_along, was_length, was_flare)) = field.written {
        if (was_length - length_m).abs() < 1e-3
            && (was_flare - flare).abs() < 1e-3
            && was_along.distance(along) < 1e-3
        {
            return;
        }
    }
    for (handle, stars) in &field.layers {
        if let Some(mut mesh) = meshes.get_mut(handle) {
            write_smear(&mut mesh, stars, along, length_m, flare);
        }
    }
    field.written = Some((along, length_m, flare));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pixels_per_metre, CAMERA_DEFAULT_HEIGHT_M, CAMERA_FOV_Y, CAMERA_MAX_HEIGHT_M,
        MIN_LEGIBLE_DIAMETER_PX,
    };
    use orrery_games::regolith::archetype::Archetype;

    /// The window #552 records as already tight.
    const TIGHT_VIEWPORT_PX: f32 = 720.0;

    fn interceptor() -> (f32, f32) {
        let limits = Archetype::Interceptor.limits();
        #[allow(clippy::cast_precision_loss)]
        (
            limits.max_speed_mms as f32 / 1_000.0,
            limits.max_accel_mmss as f32 / 1_000.0,
        )
    }

    /// How many pixels a mark of `length_m` in `layer`'s plane covers.
    fn smear_px(layer: StarLayer, length_m: f32, camera_height_m: f32) -> f32 {
        length_m * pixels_per_metre(camera_height_m + layer.depth_m, TIGHT_VIEWPORT_PX)
    }

    /// The whole point of #525: a star's world position does not move when the
    /// ship does. A tile only ever lands on its layer's lattice.
    #[test]
    fn a_tile_only_ever_lands_on_its_layers_world_lattice() {
        let layer = STAR_LAYERS[0];
        let tile = StarTile {
            layer: 0,
            offset: IVec2::new(1, -2),
        };

        let here = tile_translation(tile, Vec3::new(120.0, 900.0, -40.0));
        // A metre of travel must not move the sky at all.
        let nudged = tile_translation(tile, Vec3::new(121.0, 900.0, -39.0));
        assert_eq!(
            here, nudged,
            "sub-tile travel must leave the field exactly where it was"
        );

        // A whole period of travel moves the tile by exactly one period, onto
        // a lattice point the field already covered.
        let far = tile_translation(tile, Vec3::new(120.0 + layer.tile_m, 900.0, -40.0));
        assert!(
            (far.x - here.x - layer.tile_m).abs() < 1e-3,
            "a tile must recycle by exactly one period, moved {} not {}",
            far.x - here.x,
            layer.tile_m
        );

        // And every landing is on the lattice, however far out the session is.
        for distance in [0.0f32, 2_500.0, 100_000.0, -7_777.0] {
            let placed = tile_translation(tile, Vec3::new(distance, 900.0, distance));
            let steps = placed.x / layer.tile_m;
            assert!(
                (steps - steps.round()).abs() < 1e-4,
                "tile x {} is not a whole multiple of the {} m period",
                placed.x,
                layer.tile_m
            );
        }
    }

    /// The tiles are recycled around the camera, so the field must be at least
    /// as wide as the widest thing the camera can see.
    #[test]
    fn the_field_covers_the_screen_at_full_zoom_out() {
        let whole_tiles_of_cover = (STAR_GRID / 2) as f32 - 0.5;
        for layer in STAR_LAYERS {
            let distance = CAMERA_MAX_HEIGHT_M + layer.depth_m;
            let half_width = (CAMERA_FOV_Y / 2.0).tan() * distance * STAR_COVER_ASPECT;
            let cover = whole_tiles_of_cover * layer.tile_m;
            assert!(
                cover > half_width,
                "layer at {} m covers {} m but must reach {} m at full zoom-out",
                layer.depth_m,
                cover,
                half_width
            );
        }
    }

    /// Depth needs the layers to differ, and the HUD needs all of them to stay
    /// under the tracer white it draws over them.
    #[test]
    fn the_layers_recede_and_stay_under_the_huds_value_range() {
        for pair in STAR_LAYERS.windows(2) {
            assert!(
                pair[1].depth_m > pair[0].depth_m,
                "layers must sit at different depths or there is no parallax"
            );
            assert!(pair[1].grey < pair[0].grey, "a deeper layer must be dimmer");
            assert!(
                pair[1].tile_m > pair[0].tile_m,
                "a deeper layer covers more world and must repeat more slowly"
            );
        }
        let tracer = crate::hud::TRACER.to_linear();
        let brightest = STAR_LAYERS[0].grey;
        assert!(
            brightest < tracer.red * 0.75,
            "the brightest star must stay well under the tracer white, {brightest} vs {}",
            tracer.red
        );
    }

    /// A tile mesh is a real mesh: one quad per star, indices in range.
    #[test]
    fn a_tile_mesh_holds_one_quad_per_star_inside_its_own_period() {
        let layer = STAR_LAYERS[1];
        let stars = layer_stars(layer, 7);
        let mesh = star_tile_mesh(&stars);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("positions")
            .as_float3()
            .expect("float3 positions");
        assert_eq!(positions.len(), layer.stars * 4);
        let slack = layer.half_size_m * 1.05;
        for point in positions {
            assert!(
                point[0] >= -slack && point[0] <= layer.tile_m + slack,
                "a star must live inside its own tile, found x {}",
                point[0]
            );
            assert!((point[1]).abs() < f32::EPSILON, "a tile is flat");
        }
        let Some(Indices::U32(indices)) = mesh.indices() else {
            panic!("a tile is indexed with u32");
        };
        assert_eq!(indices.len(), layer.stars * 6);
        assert!(indices
            .iter()
            .all(|index| (*index as usize) < positions.len()));
        assert!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).is_some(),
            "the flare rides on vertex colours, so a tile must carry them"
        );
    }

    /// The authority clamp. The filters may lag or be handed anything at all;
    /// the drawn mark still cannot stand for a speed the chassis the ruleset
    /// published cannot reach.
    #[test]
    fn the_smear_never_outruns_the_chassis_speed_ceiling() {
        for archetype in Archetype::ALL {
            #[allow(clippy::cast_precision_loss)]
            let ceiling_ms = archetype.limits().max_speed_mms as f32 / 1_000.0;
            let allowed_m = ceiling_ms * STAR_EXPOSURE_SECONDS;
            for claimed_ms in [
                ceiling_ms,
                ceiling_ms * 1.001,
                ceiling_ms * 4.0,
                1.0e9,
                f32::INFINITY,
            ] {
                let drawn_m = smear_length_m(claimed_ms, ceiling_ms);
                assert!(
                    drawn_m <= allowed_m + 1e-3,
                    "a {archetype:?} smear drawn for {claimed_ms} m/s is {drawn_m} m long, \
                     but {ceiling_ms} m/s over {STAR_EXPOSURE_SECONDS} s is only {allowed_m} m"
                );
            }
            // And it is not clamped so hard it stops being a readout: at the
            // ceiling it must be the full exposure's worth.
            let at_ceiling = smear_length_m(ceiling_ms, ceiling_ms);
            assert!(
                (at_ceiling - allowed_m).abs() < 1e-3,
                "a {archetype:?} at its own ceiling must smear the whole {allowed_m} m, drew \
                 {at_ceiling} m"
            );
        }
    }

    /// A client with no craft has no velocity, so it states none.
    #[test]
    fn a_client_with_no_craft_to_follow_draws_no_smear() {
        let (ceiling_ms, _) = interceptor();
        let mut drift = StarDrift::default();
        // Fly for a while, then lose the craft.
        for _ in 0..600 {
            drift.observe(Some(Vec2::new(ceiling_ms, 0.0)), 1.0 / 60.0);
        }
        assert!(
            smear_length_m(drift.speed_ms(), ceiling_ms) > 0.0,
            "a crewed client flying at its ceiling must smear at all"
        );

        drift.observe(None, 1.0 / 60.0);
        let drawn_m = smear_length_m(drift.speed_ms(), ceiling_ms);
        assert!(
            drawn_m.abs() < f32::EPSILON,
            "a client with no craft drew a {drawn_m} m smear at {} m/s, which is motion it \
             cannot state",
            drift.speed_ms()
        );
        assert!(
            !drift.crewed,
            "a client with no craft must not report itself crewed"
        );
        assert!(
            drift.gaining_mss().abs() < f32::EPSILON,
            "a client with no craft cannot be gaining speed"
        );
        assert_eq!(
            drawn_velocity_ms(None),
            None,
            "no craft is no velocity, not a zero one"
        );
    }

    /// The mark lies along the replicated velocity, and every part of it that
    /// is not the star itself lies *behind* the star.
    #[test]
    fn the_mark_trails_the_star_along_the_replicated_velocity() {
        let star = Vec2::new(40.0, -12.0);
        let half = 1.7;
        for (along, name) in [
            (Vec2::X, "+x"),
            (Vec2::NEG_X, "-x"),
            (Vec2::new(1.0, 1.0).normalize(), "diagonal"),
        ] {
            let length = 60.0;
            let corners = smear_corners(star, half, along, length, 1.0);
            for corner in corners {
                let ahead = (corner - star).dot(along);
                assert!(
                    ahead >= -(half + 1e-3),
                    "the {name} mark reaches {ahead} m the wrong side of the star, past its \
                     own {half} m half-edge — that is screen the star's image has not been on"
                );
                assert!(
                    ahead <= half + length + 1e-3,
                    "the {name} mark reaches {ahead} m along the velocity, past the {} m the \
                     exposure covers",
                    half + length
                );
                let sideways = (corner - star).dot(Vec2::new(-along.y, along.x)).abs();
                assert!(
                    sideways < half * STAR_MAX_WIDTH_GAIN + 1e-3,
                    "the {name} mark is {sideways} m wide of the velocity, which is wider than \
                     the smear is allowed to spread"
                );
            }
            // The extremes are actually reached: it is a mark, not a point.
            let reach = corners
                .iter()
                .map(|corner| (*corner - star).dot(along))
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                (reach - half - length).abs() < 1e-3,
                "the {name} mark must run the whole {} m of the exposure, reached {reach} m",
                half + length
            );
        }

        // At rest the mark is the square the field has always drawn.
        let still = smear_corners(star, half, Vec2::X, 0.0, 0.0);
        assert_eq!(still[1], star + Vec2::new(half, -half));
        assert_eq!(still[3], star + Vec2::new(-half, half));
    }

    /// Length is velocity and light is force: two readouts that must not be
    /// each other.
    #[test]
    fn thrust_lights_the_field_and_coasting_lets_it_settle() {
        let (ceiling_ms, accel_ceiling) = interceptor();
        let dt = 1.0 / 60.0;

        // Open the throttle from a stop: the flare must come up.
        let mut burning = StarDrift::default();
        let mut speed = 0.0f32;
        let mut peak_flare = 0.0f32;
        for _ in 0..30 {
            speed = (speed + accel_ceiling * dt).min(ceiling_ms);
            burning.observe(Some(Vec2::new(speed, 0.0)), dt);
            peak_flare = peak_flare.max(thrust_flare(burning.gaining_mss(), accel_ceiling));
        }
        assert!(
            peak_flare > 0.6,
            "a craft accelerating at its own {accel_ceiling} m/s^2 ceiling only lit the field \
             to {peak_flare}, so full thrust does not read as full thrust"
        );

        // Then hold that speed: the flare must fall away while the length
        // stays. Coasting fast is long and dim.
        let coasting_at = speed;
        for _ in 0..120 {
            burning.observe(Some(Vec2::new(coasting_at, 0.0)), dt);
        }
        let settled = thrust_flare(burning.gaining_mss(), accel_ceiling);
        assert!(
            settled < 0.05,
            "a craft holding {coasting_at} m/s is not accelerating, but the field is still lit \
             to {settled}"
        );
        let coasting_length = smear_length_m(burning.speed_ms(), ceiling_ms);
        assert!(
            coasting_length > 0.5 * coasting_at * STAR_EXPOSURE_SECONDS,
            "the mark collapsed to {coasting_length} m while still moving at {coasting_at} m/s"
        );

        // The flare is bounded at both ends whatever it is handed.
        assert_eq!(thrust_flare(-1.0, accel_ceiling), 0.0);
        assert_eq!(thrust_flare(accel_ceiling * 9.0, accel_ceiling), 1.0);
        assert_eq!(thrust_flare(f32::NAN, accel_ceiling), 0.0);
        assert_eq!(thrust_flare(1.0, 0.0), 0.0);
    }

    /// Vertex colours multiply into the layer's own grey, so the flare can
    /// only ever bring a layer back up to the grey it declares — never past
    /// it, and never past the tracer white that bound was chosen against.
    #[test]
    fn the_flare_can_never_outshine_the_layers_declared_grey() {
        for stretch in [0.0f32, 0.5, 1.0] {
            for flare in [0.0f32, 0.5, 1.0, 4.0] {
                let (head, tail) = smear_levels(stretch, flare);
                assert!(
                    head <= 1.0 + f32::EPSILON,
                    "a head lit to {head} of the layer grey would put the field over the \
                     tracer bound the layer greys were chosen under"
                );
                assert!(
                    tail <= head + f32::EPSILON,
                    "a mark's tail is never brighter than its head"
                );
                assert!(tail >= 0.0 && head >= 0.0, "brightness is not negative");
            }
        }
        // At full thrust the field reaches exactly its declared grey, so the
        // existing tracer bound is still the binding one.
        let (full, _) = smear_levels(0.0, 1.0);
        assert!(
            (full - 1.0).abs() < 1e-6,
            "full thrust must reach the layer grey, reached {full}"
        );
        // The flare also has to reach the screen. At a pixel and a half across
        // a star renders as its coverage, so the value alone moved the
        // captured peak by three of 255; the width carries the rest.
        let half = STAR_LAYERS[0].half_size_m;
        let resting = smear_width_m(half, 0.0, 0.0);
        let burning = smear_width_m(half, 0.0, 1.0);
        assert!(
            burning > resting * 1.4,
            "full thrust widened a mark from {resting} m to only {burning} m, which is not \
             enough coverage for the flare to be seen at this size"
        );
        for length_m in [0.0f32, 10.0, 200.0] {
            for flare in [0.0f32, 0.5, 1.0] {
                let width = smear_width_m(half, length_m, flare);
                assert!(
                    width <= half * STAR_MAX_WIDTH_GAIN + 1e-4,
                    "a mark {width} m wide is past the {} m the two gains allow",
                    half * STAR_MAX_WIDTH_GAIN
                );
            }
        }

        // And a flat, unstretched mark is one flat value: no ramp at rest.
        let (head, tail) = smear_levels(0.0, 0.0);
        assert!(
            (head - tail).abs() < 1e-6,
            "a star at rest must not be shaded like a trail"
        );
    }

    /// The readout the speed-cap change is for: drifting and burning have to
    /// be one glance apart in the window #552 calls tight, at whatever ceiling
    /// the ruleset currently publishes.
    #[test]
    fn a_drift_and_a_burn_are_a_glance_apart_at_720_lines() {
        let (published_ms, _) = interceptor();
        let near = STAR_LAYERS[0];
        let drift_ms = 50.0f32;
        // The ceiling the ruleset publishes today, and the four-times-higher
        // one the speed-cap work is raising it to. The second is not a
        // prediction the client holds anywhere — it is here so this assertion
        // still means something the day the first number moves.
        for ceiling_ms in [published_ms, published_ms * 4.0] {
            let drift_px = smear_px(
                near,
                smear_length_m(drift_ms.min(ceiling_ms), ceiling_ms),
                CAMERA_DEFAULT_HEIGHT_M,
            );
            let burn_ms = ceiling_ms * 0.94;
            let burn_px = smear_px(
                near,
                smear_length_m(burn_ms, ceiling_ms),
                CAMERA_DEFAULT_HEIGHT_M,
            );
            assert!(
                burn_px >= MIN_LEGIBLE_DIAMETER_PX,
                "at a {ceiling_ms} m/s ceiling a full burn only smears {burn_px} px at 720 \
                 lines, under the {MIN_LEGIBLE_DIAMETER_PX} px legibility floor"
            );
            // The mark states the ratio of the speeds and nothing else: no
            // curve, no floor and no gain that would make one speed read as
            // another.
            let drawn = burn_px / drift_px;
            let stated = burn_ms / drift_ms.min(ceiling_ms);
            assert!(
                (drawn - stated).abs() < 1e-2,
                "at a {ceiling_ms} m/s ceiling the marks are {drawn}x apart while the speeds \
                 they stand for are {stated}x apart"
            );
        }

        // And at the raised ceiling — the whole point of the change this
        // serves — 50 m/s and a full burn are unmistakable at 720 lines.
        let raised_ms = published_ms * 4.0;
        let drift_px = smear_px(
            near,
            smear_length_m(drift_ms, raised_ms),
            CAMERA_DEFAULT_HEIGHT_M,
        );
        let burn_px = smear_px(
            near,
            smear_length_m(raised_ms * 0.94, raised_ms),
            CAMERA_DEFAULT_HEIGHT_M,
        );
        assert!(
            burn_px > drift_px * 3.0,
            "at a {raised_ms} m/s ceiling a burn smears {burn_px} px and a {drift_ms} m/s drift \
             {drift_px} px, which is not a difference a player reads at a glance"
        );
    }

    /// The smear is the star's own trail, so it must keep the layers' parallax
    /// ratio rather than inventing a second one.
    #[test]
    fn a_deeper_layer_smears_shorter_on_screen_in_its_parallax_ratio() {
        let (ceiling_ms, _) = interceptor();
        let length_m = smear_length_m(ceiling_ms, ceiling_ms);
        let height = CAMERA_DEFAULT_HEIGHT_M;
        for pair in STAR_LAYERS.windows(2) {
            let near_px = smear_px(pair[0], length_m, height);
            let far_px = smear_px(pair[1], length_m, height);
            // Screen size goes as `1 / distance`, so the nearer layer's mark
            // is longer by the ratio of the two distances the other way up.
            let expected = (height + pair[1].depth_m) / (height + pair[0].depth_m);
            assert!(
                (near_px / far_px - expected).abs() < 1e-3,
                "the layer at {} m smears {near_px} px and the one at {} m {far_px} px, a \
                 ratio of {} where the parallax ratio is {expected}",
                pair[0].depth_m,
                pair[1].depth_m,
                near_px / far_px
            );
        }
    }

    /// The wiring, end to end: the vertices the GPU is handed really do lie
    /// along the velocity the executor replicated, and behind the star.
    ///
    /// The pure functions above can each be right while the system feeds them
    /// the wrong thing, which is exactly the failure a parameterised test
    /// cannot see.
    #[test]
    fn the_drawn_mesh_follows_the_replicated_velocity_of_the_players_own_craft() {
        use orrery_core::{QPos, QVel};
        use orrery_games::regolith::state::Craft;
        use orrery_protocol::PersistId;

        let layer = STAR_LAYERS[0];
        let stars = layer_stars(layer, 3);
        let rest = tile_vertices(&stars, Vec2::X, 0.0, 0.0).0;

        // Due north on the screen is -Z, which is the least convenient
        // direction to get right by accident.
        let flying = Vec2::new(0.0, -90.0);
        let mut own = CraftView::of(
            PersistId(7),
            &Craft::spawned(Archetype::Interceptor, QPos::from_metres(0.0, 0.0, 0.0), 0),
        );
        own.vel = QVel::from_metres_per_sec(f64::from(flying.x), 0.0, f64::from(flying.y));

        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<StarDrift>()
            .init_resource::<Assets<Mesh>>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(star_tile_mesh(&stars));
        app.insert_resource(StarMeshes {
            layers: vec![(handle.clone(), stars.clone())],
            written: None,
        })
        .insert_resource(crate::CombatView {
            own: Some(own),
            ..Default::default()
        })
        .add_systems(Update, drive_star_smear);

        for _ in 0..60 {
            app.world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_nanos(orrery_core::TICK_NANOS));
            app.update();
        }

        let meshes = app.world().resource::<Assets<Mesh>>();
        let drawn = meshes
            .get(&handle)
            .expect("the layer mesh is still there")
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("positions")
            .as_float3()
            .expect("float3 positions")
            .to_vec();
        assert_eq!(drawn.len(), rest.len());

        let along = flying.normalize();
        let mut moved = 0usize;
        for (star, chunk) in stars.iter().zip(drawn.chunks_exact(4)) {
            for corner in chunk {
                let offset = Vec2::new(corner[0], corner[2]) - star.centre;
                assert!(
                    offset.dot(along) >= -(star.half_m + 1e-2),
                    "a drawn corner sits {} m the wrong side of its star along the \
                     replicated velocity, past the star's own {} m half-edge",
                    offset.dot(along),
                    star.half_m
                );
                if offset.dot(along) > star.half_m + 1e-2 {
                    moved += 1;
                }
            }
            // The mark runs back along the velocity, not along some axis of
            // the tile: nothing may stick out sideways further than the width
            // gain allows.
            let widest = chunk
                .iter()
                .map(|corner| {
                    (Vec2::new(corner[0], corner[2]) - star.centre)
                        .dot(Vec2::new(-along.y, along.x))
                        .abs()
                })
                .fold(0.0f32, f32::max);
            assert!(
                widest < star.half_m * STAR_MAX_WIDTH_GAIN + 1e-2,
                "a star {} m across was drawn {widest} m wide of the velocity",
                star.half_m
            );
        }
        assert!(
            moved >= stars.len(),
            "only {moved} of {} corners trail the craft's velocity; the field is not \
             streaking along it at all",
            stars.len() * 4
        );
        assert!(
            drawn != rest,
            "the mesh handed to the GPU is still the resting field, so nothing the executor \
             replicated reached the screen"
        );
    }

    /// The direction is held, not invented, when there is nothing to take one
    /// from.
    #[test]
    fn a_stopped_craft_holds_the_last_direction_rather_than_snapping() {
        let mut drift = StarDrift::default();
        for _ in 0..120 {
            drift.observe(Some(Vec2::new(0.0, -80.0)), 1.0 / 60.0);
        }
        let flying = drift.along;
        assert!(
            flying.distance(Vec2::NEG_Y) < 0.05,
            "the field must streak along the replicated velocity, streaked {flying:?}"
        );
        for _ in 0..600 {
            drift.observe(Some(Vec2::ZERO), 1.0 / 60.0);
        }
        assert!(
            drift.along.distance(flying) < 0.05,
            "a stopped craft must hold its last direction, not spin to {:?}",
            drift.along
        );
        assert!(
            drift.speed_ms() < 0.01,
            "a stopped craft still smeared {} m/s worth",
            drift.speed_ms()
        );
    }
}
