//! Integer geometry: exact ray/triangle, ray/AABB and ray/slab tests in i128,
//! a deterministic BVH, and the `Tracer` trait every representation implements.
//!
//! A ray is `P(t) = O + D·t / DIR_SCALE`, `t` in millimetres, `0 ≤ t ≤ max_mm`.
//! Every "time" is carried as an exact rational `num/den` (den > 0) until the
//! final rounding to a millimetre, so two machines cannot disagree.

use crate::format::{Hit, Ray, DIR_SCALE};

pub trait Tracer {
    fn name(&self) -> &'static str;
    fn trace(&self, ray: &Ray) -> Hit;
}

/// An exact non-negative rational time with a positive denominator.
#[derive(Clone, Copy, Debug)]
pub struct Rat {
    pub num: i128,
    pub den: i128,
}

impl Rat {
    pub const ZERO: Self = Self { num: 0, den: 1 };

    pub fn new(num: i128, den: i128) -> Self {
        if den < 0 {
            Self {
                num: -num,
                den: -den,
            }
        } else {
            Self { num, den }
        }
    }

    /// Exact comparison without cross-multiplication (which overflows i128 for two triangle
    /// times: numerators reach 1e27 and denominators 1e21). Euclidean division peels one
    /// integer part per round and flips the remainders, the Stern–Brocot way.
    pub fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let (mut a, mut b, mut c, mut d) = (self.num, self.den, other.num, other.den);
        let mut flipped = false;
        loop {
            let (qa, qc) = (a.div_euclid(b), c.div_euclid(d));
            if qa != qc {
                let o = qa.cmp(&qc);
                return if flipped { o.reverse() } else { o };
            }
            let (ra, rc) = (a.rem_euclid(b), c.rem_euclid(d));
            let o = match (ra == 0, rc == 0) {
                (true, true) => return core::cmp::Ordering::Equal,
                (true, false) => core::cmp::Ordering::Less,
                (false, true) => core::cmp::Ordering::Greater,
                (false, false) => {
                    // ra/b vs rc/d  ==  reverse of  d/rc vs b/ra
                    (a, b, c, d) = (d, rc, b, ra);
                    flipped = !flipped;
                    continue;
                }
            };
            return if flipped { o.reverse() } else { o };
        }
    }

    pub fn lt(&self, other: &Self) -> bool {
        self.cmp(other) == core::cmp::Ordering::Less
    }

    pub fn le(&self, other: &Self) -> bool {
        self.cmp(other) != core::cmp::Ordering::Greater
    }

    pub fn max(self, other: Self) -> Self {
        if self.lt(&other) {
            other
        } else {
            self
        }
    }

    pub fn min(self, other: Self) -> Self {
        if other.lt(&self) {
            other
        } else {
            self
        }
    }

    /// Round to the nearest millimetre (ties away from zero), exactly.
    pub fn round_mm(&self) -> i64 {
        let n = self.num;
        let d = self.den;
        let q = (n.abs() * 2 + d) / (2 * d);
        (if n < 0 { -q } else { q }) as i64
    }

    pub fn is_negative(&self) -> bool {
        self.num < 0
    }

    pub fn exceeds_mm(&self, max_mm: i64) -> bool {
        self.num > max_mm as i128 * self.den
    }
}

pub type V3 = [i64; 3];

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn cross(a: V3, b: V3) -> [i128; 3] {
    let a = [a[0] as i128, a[1] as i128, a[2] as i128];
    let b = [b[0] as i128, b[1] as i128, b[2] as i128];
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn dot(a: V3, b: [i128; 3]) -> i128 {
    a[0] as i128 * b[0] + a[1] as i128 * b[1] + a[2] as i128 * b[2]
}

/// Normal of a triangle scaled to 1e6, from the exact integer cross product (one float-free
/// square root would be overkill for a presentation-only field; this is reported, never adjudicated).
pub fn normal_1e6(a: V3, b: V3, c: V3) -> [i32; 3] {
    let n = cross(sub(b, a), sub(c, a));
    let len = ((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]) as f64).sqrt();
    if len == 0.0 {
        return [0; 3];
    }
    [
        (n[0] as f64 / len * 1e6).round() as i32,
        (n[1] as f64 / len * 1e6).round() as i32,
        (n[2] as f64 / len * 1e6).round() as i32,
    ]
}

/// Möller–Trumbore in exact integers. Two-sided (Chaos trimesh queries are two-sided as well).
/// Returns the hit time as an exact rational if the ray hits within `[0, max_mm]`.
pub fn ray_triangle(ray: &Ray, a: V3, b: V3, c: V3) -> Option<Rat> {
    let e1 = sub(b, a);
    let e2 = sub(c, a);
    let pv = cross(ray.d, e2);
    let det = dot(e1, pv);
    if det == 0 {
        return None;
    }
    let tv = sub(ray.o, a);
    let u = dot(tv, pv);
    let qv = cross(tv, e1);
    let v = dot(ray.d, qv);
    let (u, v, det_abs) = if det > 0 { (u, v, det) } else { (-u, -v, -det) };
    if u < 0 || v < 0 || u + v > det_abs {
        return None;
    }
    let t_num = dot(e2, qv) * DIR_SCALE as i128; // t = (e2·qv / det) · DIR_SCALE
    let t = Rat::new(t_num, det);
    if t.is_negative() || t.exceeds_mm(ray.max_mm) {
        return None;
    }
    Some(t)
}

#[derive(Clone, Copy, Debug)]
pub struct Aabb {
    pub min: V3,
    pub max: V3,
}

impl Aabb {
    pub const EMPTY: Self = Self {
        min: [i64::MAX; 3],
        max: [i64::MIN; 3],
    };

    pub fn add(&mut self, p: V3) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
    }

    pub fn union(&mut self, o: &Self) {
        self.add(o.min);
        self.add(o.max);
    }

    /// Slab test: the exact `[t_enter, t_exit]` of the ray inside the box, clipped to `[0, max_mm]`.
    pub fn ray_interval(&self, ray: &Ray) -> Option<(Rat, Rat)> {
        let mut t0 = Rat::ZERO;
        let mut t1 = Rat::new(ray.max_mm as i128, 1);
        for i in 0..3 {
            let d = ray.d[i] as i128;
            let o = ray.o[i] as i128;
            if d == 0 {
                if o < self.min[i] as i128 || o > self.max[i] as i128 {
                    return None;
                }
                continue;
            }
            // t = (plane - o) · DIR_SCALE / d
            let ta = Rat::new((self.min[i] as i128 - o) * DIR_SCALE as i128, d);
            let tb = Rat::new((self.max[i] as i128 - o) * DIR_SCALE as i128, d);
            let (near, far) = if ta.lt(&tb) { (ta, tb) } else { (tb, ta) };
            t0 = t0.max(near);
            t1 = t1.min(far);
            if t1.lt(&t0) {
                return None;
            }
        }
        Some((t0, t1))
    }
}

/// A flat, deterministically built BVH over triangle indices (median split on the longest axis).
pub struct Bvh {
    nodes: Vec<Node>,
    order: Vec<u32>,
}

struct Node {
    bounds: Aabb,
    /// leaf: `(first, count)` into `order`; interior: `(left, right)` node indices.
    a: u32,
    b: u32,
    leaf: bool,
}

const LEAF_SIZE: usize = 8;

impl Bvh {
    pub fn build(tri_bounds: &[Aabb]) -> Self {
        let mut order: Vec<u32> = (0..tri_bounds.len() as u32).collect();
        let mut nodes = Vec::with_capacity(tri_bounds.len() / LEAF_SIZE * 2 + 1);
        if !order.is_empty() {
            Self::split(&mut nodes, &mut order, tri_bounds, 0, tri_bounds.len());
        }
        Self { nodes, order }
    }

    fn split(nodes: &mut Vec<Node>, order: &mut [u32], tb: &[Aabb], lo: usize, hi: usize) -> u32 {
        let mut bounds = Aabb::EMPTY;
        for &i in &order[lo..hi] {
            bounds.union(&tb[i as usize]);
        }
        let idx = nodes.len() as u32;
        if hi - lo <= LEAF_SIZE {
            nodes.push(Node {
                bounds,
                a: lo as u32,
                b: (hi - lo) as u32,
                leaf: true,
            });
            return idx;
        }
        let ext = [
            bounds.max[0] - bounds.min[0],
            bounds.max[1] - bounds.min[1],
            bounds.max[2] - bounds.min[2],
        ];
        let axis = if ext[0] >= ext[1] && ext[0] >= ext[2] {
            0
        } else if ext[1] >= ext[2] {
            1
        } else {
            2
        };
        // Sort by centroid (×2 to stay integral) with the triangle index as tiebreak: fully deterministic.
        order[lo..hi]
            .sort_unstable_by_key(|&i| (tb[i as usize].min[axis] + tb[i as usize].max[axis], i));
        let mid = lo + (hi - lo) / 2;
        nodes.push(Node {
            bounds,
            a: 0,
            b: 0,
            leaf: false,
        });
        let left = Self::split(nodes, order, tb, lo, mid);
        let right = Self::split(nodes, order, tb, mid, hi);
        nodes[idx as usize].a = left;
        nodes[idx as usize].b = right;
        idx
    }

    /// Closest hit: `visit(tri_index) -> Option<Rat>`; returns `(t, tri_index)`.
    /// Ties on `t` resolve to the lower triangle index, so the answer does not depend on traversal order.
    pub fn closest(
        &self,
        ray: &Ray,
        mut visit: impl FnMut(u32) -> Option<Rat>,
    ) -> Option<(Rat, u32)> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut best: Option<(Rat, u32)> = None;
        let mut stack = vec![0u32];
        while let Some(n) = stack.pop() {
            let node = &self.nodes[n as usize];
            let Some((t0, _)) = node.bounds.ray_interval(ray) else {
                continue;
            };
            if let Some((bt, _)) = &best {
                if bt.lt(&t0) {
                    continue;
                }
            }
            if node.leaf {
                for &i in &self.order[node.a as usize..(node.a + node.b) as usize] {
                    if let Some(t) = visit(i) {
                        let better = match &best {
                            None => true,
                            Some((bt, bi)) => t.lt(bt) || (!bt.lt(&t) && i < *bi),
                        };
                        if better {
                            best = Some((t, i));
                        }
                    }
                }
            } else {
                stack.push(node.b);
                stack.push(node.a);
            }
        }
        best
    }
}
