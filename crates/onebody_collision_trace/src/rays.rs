//! The seeded ray generator (rand_chacha; the seed is recorded in the ray file).
//!
//! Origins are uniform over the body's XY bounds, at a height uniform in
//! `[surface − 2 m, surface + 200 m]` where `surface` is the terrain height under the origin
//! (found with a vertical ray through the tri representation); directions are uniform on the
//! sphere; length is `max_mm` (500 m by default). Floats live only here — the file is integers.

use std::error::Error;
use std::path::Path;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::format::{write_rays, Ray, DIR_SCALE};
use crate::geom::Tracer;
use crate::tri::TriWorld;

pub fn run(
    collision: &Path,
    out: &Path,
    n: u32,
    seed: u64,
    max_mm: i64,
) -> Result<(), Box<dyn Error>> {
    let bytes = std::fs::read(collision)?;
    let mut c = crate::format::Cursor::new(&bytes);
    let header = crate::format::read_header(&mut c, b"ORRYTRI1")?;
    let world = TriWorld::load(&bytes)?;
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut rays = Vec::with_capacity(n as usize);
    let mut no_surface = 0u32;
    while rays.len() < n as usize {
        let x = rng.random_range(header.min[0]..=header.max[0]);
        let y = rng.random_range(header.min[1]..=header.max[1]);
        // Surface under (x, y): a vertical probe from above the bounds.
        let probe = Ray {
            o: [x, y, header.max[2] + 10_000],
            d: [0, 0, -DIR_SCALE],
            max_mm: header.max[2] - header.min[2] + 20_000,
        };
        let hit = world.trace(&probe);
        if !hit.hit {
            no_surface += 1;
            if no_surface > n * 10 {
                return Err("could not find the surface under sampled origins".into());
            }
            continue;
        }
        let surface = probe.o[2] - hit.dist_mm;
        let z = surface + rng.random_range(-2_000..=200_000);
        // Uniform direction on the sphere (Marsaglia), scaled to DIR_SCALE and rounded.
        let (dx, dy, dz) = loop {
            let a: f64 = rng.random_range(-1.0..1.0);
            let b: f64 = rng.random_range(-1.0..1.0);
            let s = a * a + b * b;
            if s > 0.0 && s < 1.0 {
                let k = 2.0 * (1.0 - s).sqrt();
                break (a * k, b * k, 1.0 - 2.0 * s);
            }
        };
        let scale = |v: f64| (v * DIR_SCALE as f64).round() as i64;
        rays.push(Ray {
            o: [x, y, z],
            d: [scale(dx), scale(dy), scale(dz)],
            max_mm,
        });
    }
    std::fs::write(out, write_rays(seed, &rays))?;
    println!(
        "{}",
        serde_json::json!({
            "rays": rays.len(),
            "seed": seed,
            "max_mm": max_mm,
            "origin_z_relative_to_surface_mm": [-2000, 200_000],
            "bounds_mm": {"min": header.min, "max": header.max},
            "body": header.body,
            "body_seed": header.seed,
            "out": out.display().to_string(),
        })
    );
    Ok(())
}
