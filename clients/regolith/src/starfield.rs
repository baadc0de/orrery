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
//! ## Presentation only
//!
//! Nothing here reads or writes simulation state. The field is placed from the
//! camera transform, which is itself a pure function of rendered state, and it
//! claims nothing (#519). The finish is flat `unlit` grey — deliberately not
//! the emissive glow, which the skin reserves for exhaust — and every layer is
//! dimmer than [`crate::hud::TRACER`], so #518's thin pale tracers, the range
//! rings, the arc wedge and the reticle all keep the top of the value range.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};

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

/// Marks one tile instance of one layer.
#[derive(Component, Debug, Clone, Copy)]
pub struct StarTile {
    /// Index into [`STAR_LAYERS`].
    pub layer: usize,
    /// The tile's offset from the camera's own tile, in whole tiles.
    pub offset: IVec2,
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

/// One layer's tile: `layer.stars` camera-facing quads in `0.0..tile_m`.
///
/// The quads lie in the XZ plane facing `+Y`. The chase camera looks straight
/// down, so that is exactly face-on with no billboarding work per frame.
#[must_use]
pub fn star_tile_mesh(layer: StarLayer, seed: u64) -> Mesh {
    let mut state = seed;
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(layer.stars * 4);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(layer.stars * 4);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(layer.stars * 4);
    let mut indices: Vec<u32> = Vec::with_capacity(layer.stars * 6);
    for star in 0..layer.stars {
        let x = unit(&mut state) * layer.tile_m;
        let z = unit(&mut state) * layer.tile_m;
        // A little size variance so the field does not read as a regular
        // stipple. Never larger than the layer's nominal size.
        let half = layer.half_size_m * (0.45 + 0.55 * unit(&mut state));
        let base = u32::try_from(star * 4).expect("a tile holds far fewer than u32::MAX stars");
        for (dx, dz, u, v) in [
            (-half, -half, 0.0, 0.0),
            (half, -half, 1.0, 0.0),
            (half, half, 1.0, 1.0),
            (-half, half, 0.0, 1.0),
        ] {
            positions.push([x + dx, 0.0, z + dz]);
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([u, v]);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}

/// Spawns every tile of every layer. Positions are written by
/// [`sync_starfield`] on the first frame.
pub fn spawn_starfield(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let half = STAR_GRID / 2;
    for (index, layer) in STAR_LAYERS.iter().enumerate() {
        let mesh = meshes.add(star_tile_mesh(*layer, 0x5EED_0000 + index as u64));
        let material = materials.add(StandardMaterial {
            base_color: Color::srgb(layer.grey, layer.grey, layer.grey * 1.06),
            // Flat, never the emissive glow finish the skin reserves for
            // exhaust, and unaffected by the scene's directional light.
            unlit: true,
            // The quads are single-sided and authored facing +Y; not culling
            // removes any chance of a winding mistake blanking the sky.
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CAMERA_FOV_Y, CAMERA_MAX_HEIGHT_M};

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
        let mesh = star_tile_mesh(layer, 7);
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
    }
}
