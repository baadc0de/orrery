//! Representation C — voxels: occupancy at a stated edge, stored as per-column run lengths
//! (terrain columns are solid below the surface; scatter instances are rasterised shells).
//! A ray walks the column grid (2D DDA, exact) and inside each column intersects its z-runs.

use std::error::Error;

use crate::format::{read_header, Cursor, Hit, Ray, DIR_SCALE, KIND_INSTANCE, KIND_TERRAIN};
use crate::geom::{Aabb, Rat, Tracer};

pub struct VoxelWorld {
    nx: i64,
    ny: i64,
    nz: i64,
    x0: i64,
    y0: i64,
    z0: i64,
    edge: i64,
    /// `runs[col]` = inclusive `(lo, hi)` voxel-z intervals, sorted.
    runs: Vec<Vec<(i32, i32)>>,
    /// The terrain's top voxel per column (so a hit can be tagged terrain vs instance).
    terrain_top: Vec<i32>,
}

impl VoxelWorld {
    pub fn load(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        let mut c = Cursor::new(bytes);
        let _h = read_header(&mut c, b"ORRYVOX1")?;
        let nx = c.u32()? as i64;
        let ny = c.u32()? as i64;
        let nz = c.u32()? as i64;
        let x0 = c.i64()?;
        let y0 = c.i64()?;
        let z0 = c.i64()?;
        let edge = c.u32()? as i64;
        let cols = (nx * ny) as usize;
        let mut runs = Vec::with_capacity(cols);
        let mut terrain_top = Vec::with_capacity(cols);
        for _ in 0..cols {
            let n = c.u16()? as usize;
            let mut r = Vec::with_capacity(n);
            for _ in 0..n {
                r.push((c.i32()?, c.i32()?));
            }
            // The cook writes the terrain run first and it starts at 0; anything else is shell.
            terrain_top.push(
                r.first()
                    .filter(|(lo, _)| *lo == 0)
                    .map_or(-1, |(_, hi)| *hi),
            );
            runs.push(r);
        }
        Ok(Self {
            nx,
            ny,
            nz,
            x0,
            y0,
            z0,
            edge,
            runs,
            terrain_top,
        })
    }

    fn column_hit(
        &self,
        ray: &Ray,
        i: i64,
        j: i64,
        t_in: &Rat,
        t_out: &Rat,
    ) -> Option<(Rat, u8, [i32; 3])> {
        let col = &self.runs[(j * self.nx + i) as usize];
        if col.is_empty() {
            return None;
        }
        // z range the ray covers inside this column: exact endpoints, expanded to voxel indices.
        let z_at = |t: &Rat| -> i128 {
            let n = ray.o[2] as i128 * DIR_SCALE as i128 * t.den + ray.d[2] as i128 * t.num;
            n.div_euclid(DIR_SCALE as i128 * t.den)
        };
        let (za, zb) = (z_at(t_in), z_at(t_out));
        let (zlo, zhi) = if za <= zb { (za, zb) } else { (zb, za) };
        let klo = ((zlo - self.z0 as i128).div_euclid(self.edge as i128)).clamp(-1, self.nz as i128)
            as i32;
        let khi = ((zhi - self.z0 as i128).div_euclid(self.edge as i128)).clamp(-1, self.nz as i128)
            as i32;
        // Candidate voxels: every occupied k in [klo, khi]; test the ray against each voxel box and keep the earliest.
        let mut best: Option<(Rat, i32)> = None;
        for &(lo, hi) in col {
            if hi < klo || lo > khi {
                continue;
            }
            let (a, b) = (lo.max(klo), hi.min(khi));
            // Walk in ray direction so the first hit is the earliest; break as soon as one is found.
            let ks: Vec<i32> = if ray.d[2] >= 0 {
                (a..=b).collect()
            } else {
                (a..=b).rev().collect()
            };
            for k in ks {
                let bx = Aabb {
                    min: [
                        self.x0 + i * self.edge,
                        self.y0 + j * self.edge,
                        self.z0 + k as i64 * self.edge,
                    ],
                    max: [
                        self.x0 + (i + 1) * self.edge,
                        self.y0 + (j + 1) * self.edge,
                        self.z0 + (k as i64 + 1) * self.edge,
                    ],
                };
                if let Some((t0, _)) = bx.ray_interval(ray) {
                    if best.as_ref().is_none_or(|(bt, _)| t0.lt(bt)) {
                        best = Some((t0, k));
                    }
                    break;
                }
            }
        }
        let (t, k) = best?;
        let kind = if k <= self.terrain_top[(j * self.nx + i) as usize] {
            KIND_TERRAIN
        } else {
            KIND_INSTANCE
        };
        // Which face was entered: the axis whose slab bounds t. Cheap reconstruction: compare against each plane time.
        let mut normal = [0i32; 3];
        for axis in 0..3 {
            let d = ray.d[axis];
            if d == 0 {
                continue;
            }
            let cell = match axis {
                0 => i,
                1 => j,
                _ => k as i64,
            };
            let origin = match axis {
                0 => self.x0,
                1 => self.y0,
                _ => self.z0,
            };
            let plane = origin + (cell + i64::from(d < 0)) * self.edge;
            let tp = Rat::new(
                (plane as i128 - ray.o[axis] as i128) * DIR_SCALE as i128,
                d as i128,
            );
            if tp.cmp(&t) == core::cmp::Ordering::Equal {
                normal = [0; 3];
                normal[axis] = if d > 0 { -1_000_000 } else { 1_000_000 };
                break;
            }
        }
        Some((t, kind, normal))
    }
}

impl Tracer for VoxelWorld {
    fn name(&self) -> &'static str {
        "voxel"
    }

    fn trace(&self, ray: &Ray) -> Hit {
        let grid = Aabb {
            min: [self.x0, self.y0, self.z0],
            max: [
                self.x0 + self.nx * self.edge,
                self.y0 + self.ny * self.edge,
                self.z0 + self.nz * self.edge,
            ],
        };
        let Some((t_in, t_out)) = grid.ray_interval(ray) else {
            return Hit::MISS;
        };
        let coord = |axis: usize, t: &Rat| -> i128 {
            let n = ray.o[axis] as i128 * DIR_SCALE as i128 * t.den + ray.d[axis] as i128 * t.num;
            n.div_euclid(DIR_SCALE as i128 * t.den)
        };
        let mut i = ((coord(0, &t_in) - self.x0 as i128).div_euclid(self.edge as i128) as i64)
            .clamp(0, self.nx - 1);
        let mut j = ((coord(1, &t_in) - self.y0 as i128).div_euclid(self.edge as i128) as i64)
            .clamp(0, self.ny - 1);
        let next_t = |axis: usize, cell: i64, origin: i64| -> Option<Rat> {
            let d = ray.d[axis];
            if d == 0 {
                return None;
            }
            let plane = origin + (cell + i64::from(d > 0)) * self.edge;
            Some(Rat::new(
                (plane as i128 - ray.o[axis] as i128) * DIR_SCALE as i128,
                d as i128,
            ))
        };
        let mut t_cur = t_in;
        for _ in 0..(self.nx + self.ny) * 2 {
            let tx = next_t(0, i, self.x0);
            let ty = next_t(1, j, self.y0);
            let (t_next, advance_x) = match (tx, ty) {
                (None, None) => (t_out, false),
                (Some(a), None) => (a, true),
                (None, Some(b)) => (b, false),
                (Some(a), Some(b)) => {
                    if a.lt(&b) {
                        (a, true)
                    } else {
                        (b, false)
                    }
                }
            };
            let t_leave = t_next.min(t_out);
            if let Some((t, kind, normal)) = self.column_hit(ray, i, j, &t_cur, &t_leave) {
                let dist_mm = t.round_mm();
                return Hit {
                    hit: true,
                    dist_mm,
                    normal,
                    kind,
                    penetrating: dist_mm == 0,
                    face: -1,
                };
            }
            if t_out.le(&t_next) || t_next.exceeds_mm(ray.max_mm) {
                return Hit::MISS;
            }
            match (tx, ty) {
                (None, None) => return Hit::MISS,
                _ => {
                    if advance_x {
                        i += ray.d[0].signum();
                    } else {
                        j += ray.d[1].signum();
                    }
                }
            }
            if i < 0 || j < 0 || i >= self.nx || j >= self.ny {
                return Hit::MISS;
            }
            t_cur = t_next;
        }
        Hit::MISS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::DIR_SCALE;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha20Rng;

    fn synthetic(seed: u64) -> VoxelWorld {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let (nx, ny, nz) = (16i64, 14i64, 12i64);
        let mut runs = Vec::new();
        let mut terrain_top = Vec::new();
        for _ in 0..nx * ny {
            let top: i32 = rng.random_range(-1..6);
            let mut r = Vec::new();
            if top >= 0 {
                r.push((0, top));
            }
            if rng.random_range(0..3) == 0 {
                let lo = rng.random_range(top + 2..(nz as i32 - 1));
                r.push((lo, (lo + rng.random_range(0..2)).min(nz as i32 - 1)));
            }
            terrain_top.push(top);
            runs.push(r);
        }
        VoxelWorld {
            nx,
            ny,
            nz,
            x0: -2000,
            y0: 300,
            z0: -1500,
            edge: 400,
            runs,
            terrain_top,
        }
    }

    /// The column walk must find exactly what testing every occupied voxel box finds.
    #[test]
    fn column_walk_matches_brute_force() {
        for seed in 0..4u64 {
            let w = synthetic(seed);
            let mut rng = ChaCha20Rng::seed_from_u64(2000 + seed);
            for _ in 0..3000 {
                let o = [
                    rng.random_range(-5000..8000),
                    rng.random_range(-3000..9000),
                    rng.random_range(-4000..6000),
                ];
                let (a, b): (f64, f64) = (rng.random_range(-1.0..1.0), rng.random_range(-1.0..1.0));
                let s = a * a + b * b;
                if s >= 1.0 || s == 0.0 {
                    continue;
                }
                let k = 2.0 * (1.0 - s).sqrt();
                let d = [
                    ((a * k) * DIR_SCALE as f64) as i64,
                    ((b * k) * DIR_SCALE as f64) as i64,
                    ((1.0 - 2.0 * s) * DIR_SCALE as f64) as i64,
                ];
                let ray = Ray {
                    o,
                    d,
                    max_mm: 30_000,
                };
                let mut brute: Option<i64> = None;
                for j in 0..w.ny {
                    for i in 0..w.nx {
                        for &(lo, hi) in &w.runs[(j * w.nx + i) as usize] {
                            for kk in lo..=hi {
                                let bx = Aabb {
                                    min: [
                                        w.x0 + i * w.edge,
                                        w.y0 + j * w.edge,
                                        w.z0 + kk as i64 * w.edge,
                                    ],
                                    max: [
                                        w.x0 + (i + 1) * w.edge,
                                        w.y0 + (j + 1) * w.edge,
                                        w.z0 + (kk as i64 + 1) * w.edge,
                                    ],
                                };
                                if let Some((t0, _)) = bx.ray_interval(&ray) {
                                    let mm = t0.round_mm();
                                    if brute.is_none_or(|b| mm < b) {
                                        brute = Some(mm);
                                    }
                                }
                            }
                        }
                    }
                }
                let walk = w.trace(&ray);
                let got = if walk.hit { Some(walk.dist_mm) } else { None };
                assert_eq!(brute, got, "seed {seed} ray {ray:?}");
            }
        }
    }
}
