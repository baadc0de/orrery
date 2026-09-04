//! Representation B — heightfield plus prisms: the terrain as a regular height grid (i32 mm at
//! the nodes, sampled from the intermediate by the cook), each cell split into two triangles on
//! a fixed diagonal; each scatter instance as a 26-DOP (13 integer slab directions), i.e. a convex
//! prism around the instance's world-space vertices.

use std::error::Error;

use crate::format::{read_header, Cursor, Hit, Ray, DIR_SCALE, KIND_INSTANCE, KIND_TERRAIN};
use crate::geom::{normal_1e6, ray_triangle, Aabb, Bvh, Rat, Tracer, V3};

/// Same table as the cook's `KDop[13][3]`.
const KDOP: [[i64; 3]; 13] = [
    [1, 0, 0],
    [0, 1, 0],
    [0, 0, 1],
    [1, 1, 0],
    [1, -1, 0],
    [1, 0, 1],
    [1, 0, -1],
    [0, 1, 1],
    [0, 1, -1],
    [1, 1, 1],
    [1, 1, -1],
    [1, -1, 1],
    [-1, 1, 1],
];

const NO_DATA: i32 = i32::MIN;

pub struct HeightfieldWorld {
    nx: usize,
    ny: usize,
    x0: i64,
    y0: i64,
    cell: i64,
    heights: Vec<i32>,
    dops: Vec<[(i64, i64); 13]>,
    dop_bvh: Bvh,
}

impl HeightfieldWorld {
    pub fn load(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        let mut c = Cursor::new(bytes);
        let _h = read_header(&mut c, b"ORRYHF_1")?;
        let nx = c.u32()? as usize;
        let ny = c.u32()? as usize;
        let x0 = c.i64()?;
        let y0 = c.i64()?;
        let cell = c.u32()? as i64;
        let mut heights = Vec::with_capacity(nx * ny);
        for _ in 0..nx * ny {
            heights.push(c.i32()?);
        }
        let n_inst = c.u32()? as usize;
        let mut dops = Vec::with_capacity(n_inst);
        let mut bounds = Vec::with_capacity(n_inst);
        for _ in 0..n_inst {
            let mut d = [(0i64, 0i64); 13];
            for slot in &mut d {
                *slot = (c.i64()?, c.i64()?);
            }
            bounds.push(Aabb {
                min: [d[0].0, d[1].0, d[2].0],
                max: [d[0].1, d[1].1, d[2].1],
            });
            dops.push(d);
        }
        let dop_bvh = Bvh::build(&bounds);
        Ok(Self {
            nx,
            ny,
            x0,
            y0,
            cell,
            heights,
            dops,
            dop_bvh,
        })
    }

    fn node(&self, i: i64, j: i64) -> Option<V3> {
        if i < 0 || j < 0 || i >= self.nx as i64 || j >= self.ny as i64 {
            return None;
        }
        let h = self.heights[j as usize * self.nx + i as usize];
        if h == NO_DATA {
            return None;
        }
        Some([self.x0 + i * self.cell, self.y0 + j * self.cell, h as i64])
    }

    /// The two triangles of cell (i, j), diagonal from (i, j) to (i+1, j+1).
    fn cell_tris(&self, i: i64, j: i64) -> [Option<[V3; 3]>; 2] {
        let (a, b, c, d) = (
            self.node(i, j),
            self.node(i + 1, j),
            self.node(i + 1, j + 1),
            self.node(i, j + 1),
        );
        [
            match (a, b, c) {
                (Some(a), Some(b), Some(c)) => Some([a, b, c]),
                _ => None,
            },
            match (a, c, d) {
                (Some(a), Some(c), Some(d)) => Some([a, c, d]),
                _ => None,
            },
        ]
    }

    /// Walk the ray's XY footprint over the cells (2D DDA, exact) and test each cell's two triangles;
    /// the first cell with a hit wins because cells are visited in ray order.
    fn trace_terrain(&self, ray: &Ray) -> Option<(Rat, [V3; 3])> {
        let field = Aabb {
            min: [self.x0, self.y0, i64::MIN / 4],
            max: [
                self.x0 + (self.nx as i64 - 1) * self.cell,
                self.y0 + (self.ny as i64 - 1) * self.cell,
                i64::MAX / 4,
            ],
        };
        let (t_in, t_out) = field.ray_interval(ray)?;
        // Entry point, floored to a cell in exact arithmetic: x(t) = o + d·t/S with t = num/den.
        let coord = |axis: usize, t: &Rat| -> i128 {
            // floor((o·S·den + d·num) / (S·den))
            let n = ray.o[axis] as i128 * DIR_SCALE as i128 * t.den + ray.d[axis] as i128 * t.num;
            n.div_euclid(DIR_SCALE as i128 * t.den)
        };
        let cell_of = |axis: usize, t: &Rat, origin: i64| -> i64 {
            ((coord(axis, t) - origin as i128).div_euclid(self.cell as i128)) as i64
        };
        let mut i = cell_of(0, &t_in, self.x0).clamp(0, self.nx as i64 - 2);
        let mut j = cell_of(1, &t_in, self.y0).clamp(0, self.ny as i64 - 2);
        let step = |d: i64| -> i64 { d.signum() };
        // Next crossing time for an axis: plane = origin + (cell + (d>0)) * cell_size
        let next_t = |axis: usize, cell: i64, origin: i64| -> Option<Rat> {
            let d = ray.d[axis];
            if d == 0 {
                return None;
            }
            let plane = origin + (cell + i64::from(d > 0)) * self.cell;
            Some(Rat::new(
                (plane as i128 - ray.o[axis] as i128) * DIR_SCALE as i128,
                d as i128,
            ))
        };
        let mut t_cur = t_in;
        for _ in 0..(self.nx + self.ny) * 2 {
            let mut best: Option<(Rat, [V3; 3])> = None;
            for tri in self.cell_tris(i, j).into_iter().flatten() {
                if let Some(t) = ray_triangle(ray, tri[0], tri[1], tri[2]) {
                    if best.as_ref().is_none_or(|(bt, _)| t.lt(bt)) {
                        best = Some((t, tri));
                    }
                }
            }
            if best.is_some() {
                return best;
            }
            let tx = next_t(0, i, self.x0);
            let ty = next_t(1, j, self.y0);
            let (t_next, advance_x) = match (tx, ty) {
                (None, None) => return None, // vertical ray: one cell only
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
            if t_out.lt(&t_next) || t_next.exceeds_mm(ray.max_mm) {
                return None;
            }
            if advance_x {
                i += step(ray.d[0]);
            } else {
                j += step(ray.d[1]);
            }
            if i < 0 || j < 0 || i >= self.nx as i64 - 1 || j >= self.ny as i64 - 1 {
                return None;
            }
            t_cur = t_next;
        }
        let _ = t_cur;
        None
    }

    /// Ray versus a 26-DOP: the entering time is the max over slabs of the entering plane, exact.
    fn trace_dop(&self, ray: &Ray, k: usize) -> Option<(Rat, [i32; 3])> {
        let dop = &self.dops[k];
        let mut t0 = Rat::ZERO;
        let mut t1 = Rat::new(ray.max_mm as i128, 1);
        let mut enter_axis = usize::MAX;
        let mut enter_sign = 1i64;
        for (s, &(lo, hi)) in dop.iter().enumerate() {
            let n = KDOP[s];
            let nd = n[0] as i128 * ray.d[0] as i128
                + n[1] as i128 * ray.d[1] as i128
                + n[2] as i128 * ray.d[2] as i128;
            let no = n[0] as i128 * ray.o[0] as i128
                + n[1] as i128 * ray.o[1] as i128
                + n[2] as i128 * ray.o[2] as i128;
            if nd == 0 {
                if no < lo as i128 || no > hi as i128 {
                    return None;
                }
                continue;
            }
            let ta = Rat::new((lo as i128 - no) * DIR_SCALE as i128, nd);
            let tb = Rat::new((hi as i128 - no) * DIR_SCALE as i128, nd);
            let (near, far, sign) = if ta.lt(&tb) {
                (ta, tb, -1)
            } else {
                (tb, ta, 1)
            };
            if t0.lt(&near) {
                t0 = near;
                enter_axis = s;
                enter_sign = sign;
            }
            t1 = t1.min(far);
            if t1.lt(&t0) {
                return None;
            }
        }
        let normal = if enter_axis == usize::MAX {
            [0; 3]
        } else {
            let n = KDOP[enter_axis];
            let len = ((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]) as f64).sqrt();
            [
                (n[0] as f64 * enter_sign as f64 / len * 1e6).round() as i32,
                (n[1] as f64 * enter_sign as f64 / len * 1e6).round() as i32,
                (n[2] as f64 * enter_sign as f64 / len * 1e6).round() as i32,
            ]
        };
        Some((t0, normal))
    }
}

impl Tracer for HeightfieldWorld {
    fn name(&self) -> &'static str {
        "heightfield+prisms"
    }

    fn trace(&self, ray: &Ray) -> Hit {
        let terrain = self.trace_terrain(ray);
        let mut normals: Vec<(u32, [i32; 3])> = Vec::new();
        let inst = self.dop_bvh.closest(ray, |k| {
            self.trace_dop(ray, k as usize).map(|(t, n)| {
                normals.push((k, n));
                t
            })
        });
        let (t, kind, normal, face) = match (terrain, inst) {
            (None, None) => return Hit::MISS,
            (Some((t, tri)), None) => (t, KIND_TERRAIN, normal_1e6(tri[0], tri[1], tri[2]), -1),
            (None, Some((t, k))) => (
                t,
                KIND_INSTANCE,
                normals
                    .iter()
                    .find(|(i, _)| *i == k)
                    .map_or([0; 3], |(_, n)| *n),
                k as i32,
            ),
            (Some((tt, tri)), Some((ti, k))) => {
                if ti.lt(&tt) {
                    (
                        ti,
                        KIND_INSTANCE,
                        normals
                            .iter()
                            .find(|(i, _)| *i == k)
                            .map_or([0; 3], |(_, n)| *n),
                        k as i32,
                    )
                } else {
                    (tt, KIND_TERRAIN, normal_1e6(tri[0], tri[1], tri[2]), -1)
                }
            }
        };
        let dist_mm = t.round_mm();
        Hit {
            hit: true,
            dist_mm,
            normal,
            kind,
            penetrating: dist_mm == 0,
            face,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::DIR_SCALE;
    use crate::geom::Bvh;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha20Rng;

    fn synthetic(seed: u64) -> HeightfieldWorld {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let (nx, ny) = (24usize, 20usize);
        let heights = (0..nx * ny)
            .map(|k| {
                if rng.random_range(0..40) == 0 {
                    NO_DATA
                } else {
                    rng.random_range(-3000..3000) + ((k % nx) as i32) * 100
                }
            })
            .collect();
        HeightfieldWorld {
            nx,
            ny,
            x0: -1000,
            y0: 500,
            cell: 700,
            heights,
            dops: Vec::new(),
            dop_bvh: Bvh::build(&[]),
        }
    }

    /// The DDA must find exactly what a brute-force pass over every cell's two triangles finds.
    #[test]
    fn dda_matches_brute_force() {
        for seed in 0..4u64 {
            let w = synthetic(seed);
            let mut rng = ChaCha20Rng::seed_from_u64(1000 + seed);
            for _ in 0..3000 {
                let o = [
                    rng.random_range(-4000..20000),
                    rng.random_range(-3000..18000),
                    rng.random_range(-6000..12000),
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
                    max_mm: 60_000,
                };
                let mut brute: Option<Rat> = None;
                for j in 0..w.ny as i64 - 1 {
                    for i in 0..w.nx as i64 - 1 {
                        for tri in w.cell_tris(i, j).into_iter().flatten() {
                            if let Some(t) = ray_triangle(&ray, tri[0], tri[1], tri[2]) {
                                if brute.as_ref().is_none_or(|bt| t.lt(bt)) {
                                    brute = Some(t);
                                }
                            }
                        }
                    }
                }
                let dda = w.trace_terrain(&ray).map(|(t, _)| t);
                let (b, d) = (brute.map(|t| t.round_mm()), dda.map(|t| t.round_mm()));
                assert_eq!(b, d, "seed {seed} ray {ray:?}");
            }
        }
    }
}
