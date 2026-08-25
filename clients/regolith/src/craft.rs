//! Per-archetype craft models composed from Bevy primitives, nose along `+X`.
//!
//! The owner's visual design builds each chassis in three.js out of
//! `BoxGeometry`, `ConeGeometry`, `CylinderGeometry`, `SphereGeometry` and one
//! `ExtrudeGeometry`. Nothing about that composition is web-specific: it is a
//! list of primitives with a transform each, so it is reproduced here as data
//! and turned into Bevy meshes at spawn.
//!
//! Two things about this module matter beyond how it looks.
//!
//! * It is **pure description**. [`parts`] returns a `Vec<Part>` with no Bevy
//!   world access, so the shapes can be asserted in a headless test and, more
//!   importantly, so nothing here can reach a collision shape or a hitbox.
//!   `docs/15` §7: art never enters the simulation. The ruleset owns
//!   `Limits::radius_mm`; this module never reads it back into anything.
//! * Every part is authored **nose along `+X`**, because the ruleset thrusts
//!   along `(cos yaw, 0, sin yaw)` and therefore treats yaw zero as `+X`.
//!   A model authored this way needs no `NOSE_TO_PLUS_X` correction, unlike
//!   the bare `Cone` fallback it replaces.

use core::f32::consts::FRAC_PI_2;

use bevy::prelude::*;
use orrery_games::regolith::archetype::Archetype;

/// Craft are drawn at this multiple of their true size.
///
/// A 3 m interceptor, a 40 m rock and a 300 m weapon optimal cannot all be
/// true scale and legible at once. The design's answer is to enlarge the
/// craft and leave ranges honest; this is that multiplier. It is applied to
/// the rendered root transform, never to anything the ruleset reads.
pub const CRAFT_DISPLAY_SCALE: f32 = 3.0;

/// The design's four shared materials plus the emissive thruster face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Finish {
    /// `hull_plate` — the light structural plate.
    Plate,
    /// `dark_panel` — recessed panels, nacelles, drums.
    Panel,
    /// `accent_trim` — the "this one is mine" stripe, probes and barrels.
    Trim,
    /// `canopy_glass` — dark glass.
    Glass,
    /// `thruster_glow` — the emissive exhaust face.
    Glow,
}

/// One primitive solid, in the same vocabulary the three.js reference uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// `BoxGeometry(x, y, z)`.
    Cuboid {
        /// Full extent along `+X`.
        x: f32,
        /// Full extent along `+Y`.
        y: f32,
        /// Full extent along `+Z`.
        z: f32,
    },
    /// `ConeGeometry(radius, height)`, apex along `+Y` before rotation.
    Cone {
        /// Base radius.
        radius: f32,
        /// Apex height.
        height: f32,
    },
    /// `CylinderGeometry(r, r, height)`.
    Cylinder {
        /// Barrel radius.
        radius: f32,
        /// Full height along `+Y` before rotation.
        height: f32,
    },
    /// `CylinderGeometry(radius_top, radius_bottom, height)`.
    Frustum {
        /// Radius at the `+Y` end.
        radius_top: f32,
        /// Radius at the `-Y` end.
        radius_bottom: f32,
        /// Full height along `+Y` before rotation.
        height: f32,
    },
    /// `SphereGeometry(radius)`.
    Sphere {
        /// Sphere radius before [`Part::scale`].
        radius: f32,
    },
    /// One triangular prism: `Extrusion<Triangle2d>`.
    ///
    /// This is the stand-in for `ExtrudeGeometry`. See [`plated_shape`].
    Prism {
        /// Triangle in the plate's own 2D frame: `x` forward, `y` outboard.
        vertices: [Vec2; 3],
        /// Plate thickness, extruded to vertical once the part is rotated.
        depth: f32,
    },
}

/// One named primitive with its placement inside the craft.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Part {
    /// The reference model's mesh name, kept so the two can be compared.
    pub name: &'static str,
    /// The primitive solid.
    pub shape: Shape,
    /// Which shared material this part wears.
    pub finish: Finish,
    /// Placement in craft-local metres, nose along `+X`.
    pub translation: Vec3,
    /// Local rotation applied before [`Part::translation`].
    pub rotation: Quat,
    /// Local scale; only the canopy uses anything but one.
    pub scale: Vec3,
}

impl Part {
    const fn new(name: &'static str, shape: Shape, finish: Finish) -> Self {
        Self {
            name,
            shape,
            finish,
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }

    fn at(mut self, x: f32, y: f32, z: f32) -> Self {
        self.translation = Vec3::new(x, y, z);
        self
    }

    fn turned(mut self, rotation: Quat) -> Self {
        self.rotation = rotation;
        self
    }

    fn scaled(mut self, x: f32, y: f32, z: f32) -> Self {
        self.scale = Vec3::new(x, y, z);
        self
    }

    /// Where this part's origin sits once placed, in craft-local metres.
    #[must_use]
    pub const fn origin(&self) -> Vec3 {
        self.translation
    }
}

/// Half of a plated outline: the `+y` side, from nose to tail.
///
/// The reference mirrors this list about `y = 0` and closes the loop, exactly
/// as [`plated_outline`] does.
pub type HalfOutline = &'static [Vec2];

/// One plated shape: its outline, its thickness and the fan hub that
/// decomposes it. These three travel together so a test cannot check one hub
/// while the model is built from another.
#[derive(Debug, Clone, Copy)]
pub struct Plate {
    /// Mesh name shared with the reference model.
    pub name: &'static str,
    /// The `+y` half of the outline, nose to tail.
    pub outline: HalfOutline,
    /// Extruded thickness, vertical once placed.
    pub thickness: f32,
    /// Centreline `x` the triangle fan radiates from. Must lie in the
    /// outline's star kernel; `every_fan_triangle_stays_inside_the_outline`
    /// is what says it does.
    pub fan_x: f32,
    /// Height the plate sits at above the craft's own origin.
    pub lift: f32,
}

/// Every plated shape either chassis is built from.
pub const PLATES: &[Plate] = &[
    Plate {
        name: "hull",
        outline: INTERCEPTOR_HULL,
        thickness: 0.52,
        fan_x: -0.075,
        lift: 0.0,
    },
    Plate {
        name: "spine",
        outline: INTERCEPTOR_SPINE,
        thickness: 0.34,
        fan_x: 0.20,
        lift: 0.62,
    },
    Plate {
        name: "hull",
        outline: CRUISER_HULL,
        thickness: 1.35,
        fan_x: -0.09,
        lift: 0.0,
    },
    Plate {
        name: "dorsal_spine",
        outline: CRUISER_SPINE,
        thickness: 0.95,
        fan_x: -0.40,
        lift: 1.58,
    },
];

/// The interceptor's hull plate.
pub const INTERCEPTOR_HULL_PLATE: &Plate = &PLATES[0];
/// The interceptor's spine plate.
pub const INTERCEPTOR_SPINE_PLATE: &Plate = &PLATES[1];
/// The cruiser's hull plate.
pub const CRUISER_HULL_PLATE: &Plate = &PLATES[2];
/// The cruiser's spine plate.
pub const CRUISER_SPINE_PLATE: &Plate = &PLATES[3];

/// Interceptor hull, 7.4 m long over a 6 m span.
pub const INTERCEPTOR_HULL: HalfOutline = &[
    Vec2::new(4.30, 0.00),
    Vec2::new(3.30, 0.30),
    Vec2::new(1.60, 0.52),
    Vec2::new(0.60, 0.72),
    Vec2::new(-0.30, 2.55),
    Vec2::new(-1.05, 3.00),
    Vec2::new(-1.62, 2.86),
    Vec2::new(-1.30, 1.05),
    Vec2::new(-2.35, 0.98),
    Vec2::new(-2.92, 0.62),
    Vec2::new(-3.10, 0.00),
];

/// Interceptor dorsal spine.
pub const INTERCEPTOR_SPINE: HalfOutline = &[
    Vec2::new(3.10, 0.00),
    Vec2::new(1.40, 0.30),
    Vec2::new(0.20, 0.46),
    Vec2::new(-1.00, 0.60),
    Vec2::new(-2.30, 0.48),
    Vec2::new(-2.70, 0.00),
];

/// Cruiser hull, 14.6 m long over an 11 m span.
pub const CRUISER_HULL: HalfOutline = &[
    Vec2::new(7.30, 0.00),
    Vec2::new(6.20, 0.95),
    Vec2::new(4.20, 1.62),
    Vec2::new(2.10, 1.90),
    Vec2::new(1.60, 4.10),
    Vec2::new(0.10, 5.05),
    Vec2::new(-1.90, 5.20),
    Vec2::new(-2.35, 2.35),
    Vec2::new(-4.30, 2.45),
    Vec2::new(-5.90, 2.05),
    Vec2::new(-6.60, 1.10),
    Vec2::new(-7.30, 0.00),
];

/// Cruiser dorsal spine.
pub const CRUISER_SPINE: HalfOutline = &[
    Vec2::new(4.60, 0.00),
    Vec2::new(3.40, 1.05),
    Vec2::new(1.20, 1.35),
    Vec2::new(-1.60, 1.50),
    Vec2::new(-4.20, 1.30),
    Vec2::new(-5.40, 0.00),
];

/// Mirrors a half outline about `y = 0` into one closed, counter-clockwise loop.
///
/// This reproduces the reference's `platedShape`: walk the `+y` side from nose
/// to tail, then walk the mirrored interior points back, skipping both
/// endpoints because they already sit on the centreline.
#[must_use]
pub fn plated_outline(half: HalfOutline) -> Vec<Vec2> {
    let mut loop_points = half.to_vec();
    for point in half.iter().rev().skip(1).take(half.len().saturating_sub(2)) {
        loop_points.push(Vec2::new(point.x, -point.y));
    }
    loop_points
}

/// Turns a plated outline into extruded triangular prisms.
///
/// # What this approximates, and how
///
/// The reference calls `THREE.ExtrudeGeometry` on the closed outline with a
/// bevel. Bevy's [`Extrusion`] only extrudes a *primitive* 2D shape, and the
/// hull outlines are concave — the wing root folds back toward the centreline
/// — so no single Bevy primitive covers one. `ConvexPolygon` would reject them.
///
/// Both outlines are, however, **star-shaped** about a point on their
/// centreline: every vertex is visible from that point without leaving the
/// polygon. So the polygon is fanned from that point into triangles, and each
/// triangle is extruded as `Extrusion<Triangle2d>`. The union of the prisms is
/// the extruded polygon. Two things are lost against the reference: the bevel,
/// which is dropped entirely, and the shared interior faces, which are hidden
/// inside the solid and never seen. `fan_x` is checked by
/// `every_fan_triangle_stays_inside_the_outline`.
#[must_use]
pub fn plated_shape(plate: &Plate, finish: Finish) -> Vec<Part> {
    let outline = plated_outline(plate.outline);
    let hub = Vec2::new(plate.fan_x, 0.0);
    let thickness = plate.thickness;
    // The reference's `geo.rotateX(-PI/2)` then `translate(0, thickness/2)`:
    // the plate's own 2D frame becomes the world's horizontal plane and the
    // extrusion depth becomes vertical thickness, sitting on `y = lift`.
    let rotation = Quat::from_rotation_x(-FRAC_PI_2);
    let translation = Vec3::new(0.0, plate.lift + thickness / 2.0, 0.0);
    (0..outline.len())
        .map(|index| {
            let a = outline[index];
            let b = outline[(index + 1) % outline.len()];
            Part {
                translation,
                rotation,
                ..Part::new(
                    plate.name,
                    Shape::Prism {
                        vertices: [hub, a, b],
                        depth: thickness,
                    },
                    finish,
                )
            }
        })
        .collect()
}

/// Every primitive of one chassis, in the reference model's own order.
#[must_use]
pub fn parts(archetype: Archetype) -> Vec<Part> {
    match archetype {
        Archetype::Interceptor => interceptor(),
        Archetype::Cruiser => cruiser(),
    }
}

/// Nose-to-tail length of a chassis's hull plate, in true metres.
///
/// The design publishes 7.4 m and 14.6 m; both are the hull outline, not the
/// tip of the trim probe that overhangs it.
#[must_use]
pub fn hull_length(archetype: Archetype) -> f32 {
    let parts = parts(archetype);
    let mut min = f32::MAX;
    let mut max = f32::MIN;
    for part in parts.iter().filter(|part| part.name == "hull") {
        let (near, far) = extent_x(part);
        min = min.min(near);
        max = max.max(far);
    }
    max - min
}

fn extent_x(part: &Part) -> (f32, f32) {
    let half = match part.shape {
        Shape::Cuboid { x, y, z } => Vec3::new(x, y, z) / 2.0,
        Shape::Cone { radius, height } => Vec3::new(radius, height / 2.0, radius),
        Shape::Cylinder { radius, height } => Vec3::new(radius, height / 2.0, radius),
        Shape::Frustum {
            radius_top,
            radius_bottom,
            height,
        } => Vec3::new(
            radius_top.max(radius_bottom),
            height / 2.0,
            radius_top.max(radius_bottom),
        ),
        Shape::Sphere { radius } => Vec3::splat(radius),
        Shape::Prism { vertices, depth } => {
            let mut lo = Vec2::splat(f32::MAX);
            let mut hi = Vec2::splat(f32::MIN);
            for vertex in vertices {
                lo = lo.min(vertex);
                hi = hi.max(vertex);
            }
            // Prism vertices already carry position, so fold the midpoint in
            // and report the half-extent about it.
            let centre = (lo + hi) / 2.0;
            let half = Vec3::new((hi.x - lo.x) / 2.0, (hi.y - lo.y) / 2.0, depth / 2.0);
            let rotated = (part.rotation * (half * part.scale)).abs();
            let origin = part.translation + part.rotation * (centre.extend(0.0) * part.scale);
            return (origin.x - rotated.x, origin.x + rotated.x);
        }
    };
    let rotated = (part.rotation * (half * part.scale)).abs();
    (
        part.translation.x - rotated.x,
        part.translation.x + rotated.x,
    )
}

/// A forward-swept dart: one plate of fuselage, a long nose, two outboard
/// thrusters. Reads as a single arrowhead from 500 m up.
fn interceptor() -> Vec<Part> {
    let mut parts = plated_shape(INTERCEPTOR_HULL_PLATE, Finish::Plate);
    parts.extend(plated_shape(INTERCEPTOR_SPINE_PLATE, Finish::Panel));
    parts.push(
        // `SphereGeometry(0.52, …, 0, PI/2)` is an open upper hemisphere.
        // Bevy's `Sphere` has no polar sweep, so this is a whole sphere with
        // the same scale: its lower half is buried in the hull plate and the
        // silhouette above the deck is identical.
        Part::new("canopy", Shape::Sphere { radius: 0.52 }, Finish::Glass)
            .at(1.35, 0.66, 0.0)
            .scaled(1.55, 0.62, 0.85),
    );
    for side in [1.0f32, -1.0] {
        parts.push(
            Part::new(
                if side > 0.0 {
                    "thruster_port"
                } else {
                    "thruster_starboard"
                },
                Shape::Frustum {
                    radius_top: 0.38,
                    radius_bottom: 0.30,
                    height: 2.30,
                },
                Finish::Panel,
            )
            .at(-1.55, 0.30, side * 1.55)
            .turned(Quat::from_rotation_z(FRAC_PI_2)),
        );
        parts.push(
            Part::new(
                if side > 0.0 {
                    "thruster_face_port"
                } else {
                    "thruster_face_starboard"
                },
                Shape::Cylinder {
                    radius: 0.30,
                    height: 0.10,
                },
                Finish::Glow,
            )
            .at(-2.71, 0.30, side * 1.55)
            .turned(Quat::from_rotation_z(FRAC_PI_2)),
        );
        parts.push(
            Part::new(
                if side > 0.0 {
                    "accent_trim_port"
                } else {
                    "accent_trim_starboard"
                },
                Shape::Cuboid {
                    x: 1.70,
                    y: 0.09,
                    z: 0.16,
                },
                Finish::Trim,
            )
            .at(-0.35, 0.66, side * 1.45)
            .turned(Quat::from_rotation_y(side * 0.28)),
        );
    }
    parts.push(
        Part::new(
            "nose_probe",
            Shape::Cone {
                radius: 0.22,
                height: 1.10,
            },
            Finish::Trim,
        )
        .at(4.72, 0.28, 0.0)
        .turned(Quat::from_rotation_z(-FRAC_PI_2)),
    );
    parts.push(
        Part::new(
            "fin",
            Shape::Cuboid {
                x: 0.34,
                y: 0.86,
                z: 0.12,
            },
            Finish::Panel,
        )
        .at(-2.55, 0.86, 0.0),
    );
    parts
}

/// Twice the radius and five times the mass on screen: a blunt slab hull, a
/// dorsal spine, two turret drums and four thrusters.
fn cruiser() -> Vec<Part> {
    let mut parts = plated_shape(CRUISER_HULL_PLATE, Finish::Plate);
    parts.extend(plated_shape(CRUISER_SPINE_PLATE, Finish::Panel));
    parts.push(
        Part::new(
            "bridge",
            Shape::Cuboid {
                x: 2.30,
                y: 0.72,
                z: 1.90,
            },
            Finish::Panel,
        )
        .at(2.30, 2.80, 0.0),
    );
    parts.push(
        Part::new(
            "bridge_glass",
            Shape::Cuboid {
                x: 0.14,
                y: 0.42,
                z: 1.55,
            },
            Finish::Glass,
        )
        .at(3.47, 2.86, 0.0),
    );
    for side in [1.0f32, -1.0] {
        let port = side > 0.0;
        parts.push(
            Part::new(
                if port {
                    "turret_drum_port"
                } else {
                    "turret_drum_starboard"
                },
                Shape::Frustum {
                    radius_top: 1.05,
                    radius_bottom: 0.95,
                    height: 1.05,
                },
                Finish::Panel,
            )
            .at(0.20, 2.10, side * 2.55),
        );
        parts.push(
            Part::new(
                if port {
                    "turret_barrel_port"
                } else {
                    "turret_barrel_starboard"
                },
                Shape::Cuboid {
                    x: 3.20,
                    y: 0.34,
                    z: 0.40,
                },
                Finish::Trim,
            )
            .at(1.95, 2.30, side * 2.55),
        );
        for (index, offset) in [(0usize, 1.30f32), (1, 3.15)] {
            parts.push(
                Part::new(
                    thruster_name(port, index),
                    Shape::Frustum {
                        radius_top: 0.62,
                        radius_bottom: 0.52,
                        height: 2.60,
                    },
                    Finish::Panel,
                )
                .at(-6.10, 0.72, side * offset)
                .turned(Quat::from_rotation_z(FRAC_PI_2)),
            );
            parts.push(
                Part::new(
                    thruster_face_name(port, index),
                    Shape::Cylinder {
                        radius: 0.52,
                        height: 0.12,
                    },
                    Finish::Glow,
                )
                .at(-7.42, 0.72, side * offset)
                .turned(Quat::from_rotation_z(FRAC_PI_2)),
            );
        }
        parts.push(
            Part::new(
                if port {
                    "accent_trim_port"
                } else {
                    "accent_trim_starboard"
                },
                Shape::Cuboid {
                    x: 4.40,
                    y: 0.14,
                    z: 0.22,
                },
                Finish::Trim,
            )
            .at(1.20, 1.44, side * 2.95)
            .turned(Quat::from_rotation_y(side * 0.16)),
        );
    }
    parts.push(
        Part::new(
            "tail_fin",
            Shape::Cuboid {
                x: 1.10,
                y: 1.55,
                z: 0.30,
            },
            Finish::Panel,
        )
        .at(-6.30, 2.30, 0.0),
    );
    parts.push(
        Part::new(
            "nose_ram",
            Shape::Cone {
                radius: 0.42,
                height: 1.40,
            },
            Finish::Trim,
        )
        .at(7.90, 0.72, 0.0)
        .turned(Quat::from_rotation_z(-FRAC_PI_2)),
    );
    parts
}

const fn thruster_name(port: bool, index: usize) -> &'static str {
    match (port, index) {
        (true, 0) => "thruster_port_0",
        (true, _) => "thruster_port_1",
        (false, 0) => "thruster_starboard_0",
        (false, _) => "thruster_starboard_1",
    }
}

const fn thruster_face_name(port: bool, index: usize) -> &'static str {
    match (port, index) {
        (true, 0) => "thruster_face_port_0",
        (true, _) => "thruster_face_port_1",
        (false, 0) => "thruster_face_starboard_0",
        (false, _) => "thruster_face_starboard_1",
    }
}

/// Builds the Bevy mesh for one primitive.
#[must_use]
pub fn mesh_for(shape: Shape) -> Mesh {
    match shape {
        Shape::Cuboid { x, y, z } => Cuboid::new(x, y, z).into(),
        Shape::Cone { radius, height } => Cone { radius, height }.into(),
        Shape::Cylinder { radius, height } => Cylinder {
            radius,
            half_height: height / 2.0,
        }
        .into(),
        Shape::Frustum {
            radius_top,
            radius_bottom,
            height,
        } => ConicalFrustum {
            radius_top,
            radius_bottom,
            height,
        }
        .into(),
        Shape::Sphere { radius } => Sphere::new(radius).into(),
        Shape::Prism { vertices, depth } => Extrusion::new(
            Triangle2d::new(vertices[0], vertices[1], vertices[2]),
            depth,
        )
        .into(),
    }
}

/// One chassis's weapon arc, in the **ruleset's own bearing convention**.
///
/// A bearing is measured in micro-radians from the craft's nose, positive the
/// way the ruleset's yaw runs, and the direction it names is
/// `(cos θ, 0, sin θ)` in craft-local space — the same expression
/// `step_craft` thrusts along. Authoring the arc in that form is what keeps
/// it off the wrong axis: bearing zero is `+X`, which is exactly where the
/// nose-along-`+X` models point, so an arc is a rotation of the hull's own
/// frame and inherits [`crate::heading_rotation`] from the craft root rather
/// than re-deriving a world rotation of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiringArc {
    /// Mesh name, kept so a headless test can name what it found.
    pub name: &'static str,
    /// Centre bearing from the nose, micro-radians.
    pub centre_urad: i32,
    /// Half the arc's total width, micro-radians.
    pub half_width_urad: i32,
}

impl FiringArc {
    /// Craft-local unit direction of a bearing relative to this craft's nose,
    /// written the way the ruleset writes it: `(cos θ, 0, sin θ)`.
    #[must_use]
    pub fn direction(bearing_urad: f32) -> Vec3 {
        let theta = bearing_urad / 1_000_000.0;
        Vec3::new(theta.cos(), 0.0, theta.sin())
    }

    /// The arc's centreline, in craft-local space.
    #[must_use]
    pub fn centre_direction(self) -> Vec3 {
        Self::direction(self.centre_urad as f32)
    }

    /// Whether a bearing relative to the nose falls inside this arc.
    #[must_use]
    pub fn contains_urad(self, bearing_urad: i32) -> bool {
        let mut delta = (bearing_urad - self.centre_urad).rem_euclid(TAU_URAD_I32);
        if delta > TAU_URAD_I32 / 2 {
            delta -= TAU_URAD_I32;
        }
        delta.abs() <= self.half_width_urad
    }
}

/// One full turn in micro-radians, matching the ruleset's `TAU_URAD`.
const TAU_URAD_I32: i32 = 6_283_185;

/// Half of 90°, in micro-radians.
const DEG_45_URAD: i32 = 785_398;
/// Half of 45°, in micro-radians.
const DEG_22_5_URAD: i32 = 392_699;
/// A quarter turn, in micro-radians.
const DEG_90_URAD: i32 = 1_570_796;

/// How far an arc is drawn, as a multiple of the chassis's hull length.
///
/// The arc is a **facing marking on the hull**, not a range envelope: the
/// range envelope is already drawn, to scale, by [`crate::hud::RangeRing`].
/// Drawing the arc out to weapon optimal (300 m against a 7.4 m hull) would
/// swamp the plan view and would also imply the arc bounds where the weapon
/// reaches, which it does not.
pub const ARC_RADIUS_HULL_LENGTHS: f32 = 2.2;

/// The arcs a chassis wears: **interceptor 90° front, cruiser 45° side**.
///
/// # What these do and do not mean
///
/// They are a **presentation** fact. The Regolith ruleset resolves a shot
/// from range, relative velocity and the weapon table only —
/// `projectile_resolution` and `hit_chance_ppm` never read the shooter's
/// `yaw_urad`, and `Order::Fire` is accepted at any bearing. So an arc does
/// not gate anything the adjudicator does, and nothing in the skin may make
/// it look as though it does: the hit band beside the target
/// ([`crate::combat::HitBand`]) is computed from the tracking maths alone and
/// is never narrowed by whether the target sits inside an arc.
///
/// What the arc is honestly for is the chassis read the owner asked for —
/// which hull is a nose-on brawler and which is a broadside — and it is drawn
/// as hull livery for that reason.
#[must_use]
pub fn firing_arcs(archetype: Archetype) -> Vec<FiringArc> {
    match archetype {
        // 90° front: one arc, centred on the nose, ±45°.
        Archetype::Interceptor => vec![FiringArc {
            name: "arc_front",
            centre_urad: 0,
            half_width_urad: DEG_45_URAD,
        }],
        // 45° side: two arcs, centred on each beam, ±22.5°.
        Archetype::Cruiser => vec![
            FiringArc {
                name: "arc_starboard",
                centre_urad: DEG_90_URAD,
                half_width_urad: DEG_22_5_URAD,
            },
            FiringArc {
                name: "arc_port",
                centre_urad: -DEG_90_URAD,
                half_width_urad: DEG_22_5_URAD,
            },
        ],
    }
}

/// Segments in one drawn arc's fan. Enough that the 45° arc's outer edge
/// reads as a curve rather than a chord.
const ARC_SEGMENTS: usize = 24;

/// A flat triangle fan filling one arc, in the craft's own local frame.
///
/// Every rim vertex is [`FiringArc::direction`] of a bearing inside the arc,
/// scaled to `radius`, so the mesh cannot end up on another axis without the
/// bearing convention itself being wrong.
#[must_use]
pub fn arc_mesh(arc: FiringArc, radius: f32) -> Mesh {
    let mut positions = vec![[0.0f32, 0.0, 0.0]];
    let start = (arc.centre_urad - arc.half_width_urad) as f32;
    let span = (2 * arc.half_width_urad) as f32;
    for step in 0..=ARC_SEGMENTS {
        let bearing = start + span * (step as f32 / ARC_SEGMENTS as f32);
        let point = FiringArc::direction(bearing) * radius;
        positions.push([point.x, point.y, point.z]);
    }
    let mut indices = Vec::with_capacity(ARC_SEGMENTS * 3);
    for step in 0..ARC_SEGMENTS as u32 {
        indices.extend_from_slice(&[0, step + 1, step + 2]);
    }
    let normals = vec![[0.0f32, 1.0, 0.0]; positions.len()];
    let uvs = vec![[0.0f32, 0.0]; positions.len()];
    let mut mesh = Mesh::new(
        bevy::render::mesh::PrimitiveTopology::TriangleList,
        bevy::asset::RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(bevy::render::mesh::Indices::U32(indices));
    mesh
}

/// Which seat a drawn craft flies from.
///
/// This is a presentation fact and nothing more: the skin knows which entity
/// it flies, and the ruleset does not care what colour anything is. In the
/// demo session the second seat is always the bot, hence the variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Seat {
    /// The craft this client flies — the player.
    Player,
    /// Every other craft — in this demo, the bot.
    Bot,
}

impl Seat {
    /// The hull-plate tint this seat wears.
    ///
    /// #383 asks for the grey of the ships to become colour that discerns
    /// player from bot. The **plate** is the material that carries it, and
    /// that choice is deliberate:
    ///
    /// * The plate is the one finish with enough area to read at all. The
    ///   camera frames the duel from ~500 m up (`frame_camera`'s floor), where
    ///   a trim stripe is a few pixels and the dark panel and canopy glass are
    ///   near-black; only the light hull plate shows hue across a whole ship.
    /// * Panel, glass, trim and glow keep the design's roles untouched. Trim
    ///   and glow stay on the shared accent scheme ("this one is mine"), so
    ///   the per-archetype silhouettes remain the primary read and the accent
    ///   scheme is not reverted to an old cyan-vs-orange allegiance paint.
    /// * Both tints hold the neutral ramp's lightness (their channels sum to
    ///   ~1.75, as the old grey's did) and separate along the blue–yellow
    ///   axis — cool violet towards the design's accent family for the
    ///   player, warm amber for the bot — which survives the common
    ///   red–green colour-vision deficiencies far better than a red/green or
    ///   cyan/orange pair would.
    #[must_use]
    pub const fn plate(self) -> Color {
        match self {
            Self::Player => Color::srgb(0.550, 0.520, 0.680),
            Self::Bot => Color::srgb(0.680, 0.600, 0.470),
        }
    }
}

/// The design's shared material for one finish.
///
/// `accent` is the "this one is mine" hue: the design gives it to the player's
/// own craft and leaves every other craft on the neutral ramp, so the picture
/// still separates for a pilot who cannot tell the two hues apart.
///
/// `seat` feeds exactly one finish, the plate (#383): every other finish
/// renders identically for both seats, so the allegiance cue lives where the
/// owner asked for it and nowhere else.
#[must_use]
pub fn finish_material(finish: Finish, seat: Seat, accent: Color) -> StandardMaterial {
    match finish {
        Finish::Plate => StandardMaterial {
            base_color: seat.plate(),
            perceptual_roughness: 0.55,
            metallic: 0.35,
            ..Default::default()
        },
        Finish::Panel => StandardMaterial {
            base_color: Color::srgb(0.235, 0.247, 0.298),
            perceptual_roughness: 0.72,
            metallic: 0.30,
            ..Default::default()
        },
        Finish::Trim => StandardMaterial {
            base_color: accent,
            perceptual_roughness: 0.40,
            metallic: 0.30,
            ..Default::default()
        },
        Finish::Glass => StandardMaterial {
            base_color: Color::srgb(0.098, 0.106, 0.157),
            perceptual_roughness: 0.18,
            metallic: 0.25,
            ..Default::default()
        },
        Finish::Glow => StandardMaterial {
            base_color: Color::srgb(0.169, 0.153, 0.255),
            emissive: LinearRgba::from(accent) * 1.5,
            perceptual_roughness: 0.50,
            metallic: 0.10,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::render::mesh::VertexAttributeValues;

    /// Any accent does for material tests: trim and glow pass it through,
    /// and the finishes under test ignore it by design.
    const ACCENT_STAND_IN: Color = Color::srgb(0.5, 0.5, 0.5);

    fn inside(outline: &[Vec2], point: Vec2) -> bool {
        let mut inside = false;
        for index in 0..outline.len() {
            let a = outline[index];
            let b = outline[(index + 1) % outline.len()];
            if (a.y > point.y) != (b.y > point.y) {
                let crossing = a.x + (point.y - a.y) * (b.x - a.x) / (b.y - a.y);
                if point.x < crossing {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// The fan decomposition is only a faithful stand-in for `ExtrudeGeometry`
    /// if no fan triangle pokes outside the outline it is meant to fill. This
    /// samples each triangle's interior and asserts every sample is inside.
    #[test]
    fn every_fan_triangle_stays_inside_the_outline() {
        // Walk `PLATES` itself, not a copy of its numbers: a fan hub the
        // models use but this list does not would be exactly the mutation
        // this test exists to catch.
        for plate in PLATES {
            let name = plate.name;
            let outline = plated_outline(plate.outline);
            let parts = plated_shape(plate, Finish::Plate);
            assert_eq!(parts.len(), outline.len(), "{name}: one prism per edge");
            for part in &parts {
                let Shape::Prism { vertices, .. } = part.shape else {
                    panic!("{name}: a plated shape is prisms only");
                };
                for u in 1..8 {
                    for v in 1..8 - u {
                        let w = 8 - u - v;
                        let sample = (vertices[0] * u as f32
                            + vertices[1] * v as f32
                            + vertices[2] * w as f32)
                            / 8.0;
                        assert!(
                            inside(&outline, sample),
                            "{name}: fan triangle {vertices:?} leaves the outline at {sample}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn plated_outline_closes_and_mirrors() {
        let outline = plated_outline(INTERCEPTOR_HULL);
        // Nose and tail sit on the centreline once each; every other point is
        // present twice, mirrored.
        assert_eq!(outline.len(), 2 * INTERCEPTOR_HULL.len() - 2);
        assert_eq!(outline[0], INTERCEPTOR_HULL[0]);
        assert!(outline.iter().all(|p| p.y.abs() <= 3.0 + f32::EPSILON));
        let area: f32 = (0..outline.len())
            .map(|i| {
                let a = outline[i];
                let b = outline[(i + 1) % outline.len()];
                a.x * b.y - b.x * a.y
            })
            .sum::<f32>()
            / 2.0;
        assert!(area > 0.0, "the outline must wind counter-clockwise");
    }

    #[test]
    fn each_archetype_has_its_own_silhouette() {
        let interceptor = parts(Archetype::Interceptor);
        let cruiser = parts(Archetype::Cruiser);
        assert_ne!(
            interceptor, cruiser,
            "one model for both chassis is the bug"
        );
        // The design's published dimensions, to a tenth of a metre.
        assert!(
            (hull_length(Archetype::Interceptor) - 7.4).abs() < 0.2,
            "interceptor is {} m",
            hull_length(Archetype::Interceptor)
        );
        assert!(
            (hull_length(Archetype::Cruiser) - 14.6).abs() < 0.4,
            "cruiser is {} m",
            hull_length(Archetype::Cruiser)
        );
        assert!(hull_length(Archetype::Cruiser) > hull_length(Archetype::Interceptor));
    }

    #[test]
    fn every_model_is_authored_nose_along_plus_x() {
        for archetype in Archetype::ALL {
            let parts = parts(*archetype);
            let nose = parts
                .iter()
                .find(|part| part.name == "nose_probe" || part.name == "nose_ram")
                .expect("both chassis carry a nose primitive");
            assert!(
                nose.origin().x > 0.0,
                "{archetype:?}: the nose primitive must sit forward of the origin"
            );
            // A cone's apex points `+Y` in Bevy; the rotation must swing it to
            // `+X`, or the craft flies sideways under the ruleset's yaw.
            let apex = nose.rotation * Vec3::Y;
            assert!(
                apex.x > 0.99,
                "{archetype:?}: nose apex points {apex}, not +X"
            );
            let thrusters: Vec<_> = parts
                .iter()
                .filter(|part| part.name.starts_with("thruster_face"))
                .collect();
            assert!(
                !thrusters.is_empty() && thrusters.iter().all(|part| part.origin().x < 0.0),
                "{archetype:?}: exhaust must sit aft"
            );
        }
    }

    #[test]
    fn the_glow_finish_is_reserved_for_exhaust() {
        for archetype in Archetype::ALL {
            for part in parts(*archetype) {
                if part.finish == Finish::Glow {
                    assert!(
                        part.name.starts_with("thruster_face"),
                        "{archetype:?}: {} should not be emissive",
                        part.name
                    );
                }
            }
        }
    }

    /// #383's colour tell: the two seats must not render the same hull.
    ///
    /// This reads `finish_material` — what actually reaches the GPU — not the
    /// constants behind it, so a mutation that makes the seats share a plate
    /// fails here rather than in a test of its own inputs. The rest of the
    /// finishes must stay seat-blind: the allegiance cue lives on the plate
    /// alone, and the design's other materials are not allegiance paint.
    #[test]
    fn the_plate_carries_the_allegiance_tint_and_nothing_else_does() {
        let player = Seat::Player;
        let bot = Seat::Bot;
        let base = |finish, seat| finish_material(finish, seat, ACCENT_STAND_IN).base_color;
        assert_ne!(
            base(Finish::Plate, player),
            base(Finish::Plate, bot),
            "player and bot plates are identical: no at-a-glance allegiance cue"
        );
        for shared in [Finish::Panel, Finish::Glass] {
            assert_eq!(
                base(shared, player),
                base(shared, bot),
                "{shared:?} must not carry the allegiance tint"
            );
        }
        // Trim and glow stay on the accent scheme, whatever the seat.
        assert_eq!(
            base(Finish::Trim, player),
            base(Finish::Trim, bot),
            "trim follows the accent, not the seat"
        );
        // And both plates must still be recognisably the light structural
        // plate: neither may collapse toward the dark panel ramp.
        for seat in [player, bot] {
            let plate = base(Finish::Plate, seat).to_srgba();
            let panel = base(Finish::Panel, seat).to_srgba();
            let channel_sum = |c: bevy::color::Srgba| c.red + c.green + c.blue;
            assert!(
                channel_sum(plate) > channel_sum(panel),
                "{seat:?}: a plate darker than the panel is no longer the plate"
            );
        }
    }

    /// Both chassis must wear the tint on their plated surfaces: every plate
    /// part of every model maps to the seat's plate material through the one
    /// shared `finish_material`, so a model that hardcoded its own grey would
    /// be caught here.
    #[test]
    fn every_hull_part_of_every_chassis_is_a_plated_part() {
        for archetype in Archetype::ALL {
            let plated = parts(*archetype)
                .iter()
                .filter(|part| part.finish == Finish::Plate)
                .count();
            assert!(
                plated > 0,
                "{archetype:?}: no plate means no allegiance surface to tint"
            );
        }
    }

    /// The angles the owner asked for, read back off the data rather than the
    /// constants that built it: **interceptor 90° front, cruiser 45° side**.
    #[test]
    fn each_hull_wears_the_angles_the_owner_named() {
        let degrees = |urad: i32| urad as f64 / 1_000_000.0 * 180.0 / core::f64::consts::PI;

        let front = firing_arcs(Archetype::Interceptor);
        assert_eq!(front.len(), 1, "the interceptor's arc is one front arc");
        assert!(
            (degrees(2 * front[0].half_width_urad) - 90.0).abs() < 0.001,
            "interceptor arc is {}°, not 90°",
            degrees(2 * front[0].half_width_urad)
        );
        assert_eq!(front[0].centre_urad, 0, "a front arc centres on the nose");

        let sides = firing_arcs(Archetype::Cruiser);
        assert_eq!(
            sides.len(),
            2,
            "a side arc is a broadside: port and starboard"
        );
        for arc in &sides {
            assert!(
                (degrees(2 * arc.half_width_urad) - 45.0).abs() < 0.001,
                "cruiser arc {} is {}°, not 45°",
                arc.name,
                degrees(2 * arc.half_width_urad)
            );
            assert!(
                (degrees(arc.centre_urad).abs() - 90.0).abs() < 0.001,
                "cruiser arc {} centres {}° off the nose, not on a beam",
                arc.name,
                degrees(arc.centre_urad)
            );
        }
        assert_ne!(
            sides[0].centre_urad, sides[1].centre_urad,
            "both cruiser arcs landed on the same beam"
        );
    }

    /// The trap #377 paid for, closed at the arc.
    ///
    /// A bearing is `(cos θ, 0, sin θ)` — the ruleset's own expression, in the
    /// deck plane. Bevy's `Cone` points `+Y`; an arc built on that axis would
    /// point at the top-down camera and look entirely plausible. Every vertex
    /// of every arc mesh must have `y == 0`, and no vertex may be the `+Y`
    /// unit direction.
    #[test]
    fn every_arc_vertex_lies_in_the_deck_plane() {
        for archetype in Archetype::ALL {
            for arc in firing_arcs(*archetype) {
                let mesh = arc_mesh(arc, 10.0);
                let VertexAttributeValues::Float32x3(points) = mesh
                    .attribute(Mesh::ATTRIBUTE_POSITION)
                    .expect("the fan has positions")
                else {
                    panic!("arc positions are Float32x3");
                };
                assert!(points.len() > 3, "{}: a fan needs a rim", arc.name);
                for point in points {
                    assert_eq!(
                        point[1], 0.0,
                        "{}: vertex {point:?} left the deck plane — this is the +Y cone trap",
                        arc.name
                    );
                }
            }
        }
    }

    /// The arc's rim spans exactly the arc and nothing outside it, measured in
    /// bearings recovered from the geometry rather than from the inputs.
    #[test]
    fn an_arcs_rim_spans_exactly_its_own_bearings() {
        for archetype in Archetype::ALL {
            for arc in firing_arcs(*archetype) {
                let radius = 40.0f32;
                let mesh = arc_mesh(arc, radius);
                let VertexAttributeValues::Float32x3(points) = mesh
                    .attribute(Mesh::ATTRIBUTE_POSITION)
                    .expect("the fan has positions")
                else {
                    panic!("arc positions are Float32x3");
                };
                let mut extremes: Vec<i32> = Vec::new();
                for point in points.iter().skip(1) {
                    let here = Vec3::new(point[0], point[1], point[2]);
                    assert!(
                        (here.length() - radius).abs() < 1e-2,
                        "{}: rim vertex {here} is not at radius {radius}",
                        arc.name
                    );
                    // Recover the bearing the way the ruleset reads one.
                    let bearing = (here.z.atan2(here.x) * 1_000_000.0).round() as i32;
                    assert!(
                        arc.contains_urad(bearing),
                        "{}: rim bearing {bearing} µrad is outside the arc",
                        arc.name
                    );
                    extremes.push(bearing);
                }
                let widest = extremes
                    .iter()
                    .map(|bearing| {
                        let mut delta = (bearing - arc.centre_urad).rem_euclid(TAU_URAD_I32);
                        if delta > TAU_URAD_I32 / 2 {
                            delta -= TAU_URAD_I32;
                        }
                        delta.abs()
                    })
                    .max()
                    .expect("the rim has vertices");
                assert!(
                    (widest - arc.half_width_urad).abs() < 100,
                    "{}: the rim reaches {widest} µrad of a {} µrad half-width",
                    arc.name,
                    arc.half_width_urad
                );
            }
        }
    }
}
