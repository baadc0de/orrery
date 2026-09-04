//! Representation A — lattice triangles: the intermediate's triangles with vertices snapped
//! to integer millimetres, terrain first, then each scatter instance flattened to world space.
//! The BVH is built by the reader (deterministically); the package is the triangle soup.

use std::error::Error;

use crate::format::{read_header, read_soup, Cursor, Hit, Ray, Soup, KIND_INSTANCE, KIND_TERRAIN};
use crate::geom::{normal_1e6, ray_triangle, Aabb, Bvh, Tracer};

pub struct TriWorld {
    verts: Vec<[i64; 3]>,
    tris: Vec<[u32; 3]>,
    /// `u32::MAX` for terrain, else the instance index — the "who answered" tag.
    owner: Vec<u32>,
    bvh: Bvh,
}

impl TriWorld {
    pub fn load(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        let mut c = Cursor::new(bytes);
        let _h = read_header(&mut c, b"ORRYTRI1")?;
        let terrain = read_soup(&mut c)?;
        let n_inst = c.u32()? as usize;
        let mut verts = terrain.verts;
        let mut tris = terrain.tris;
        let mut owner = vec![u32::MAX; tris.len()];
        for i in 0..n_inst {
            let Soup { verts: v, tris: t } = read_soup(&mut c)?;
            let base = verts.len() as u32;
            verts.extend_from_slice(&v);
            tris.extend(
                t.iter()
                    .map(|tri| [tri[0] + base, tri[1] + base, tri[2] + base]),
            );
            owner.extend(core::iter::repeat_n(i as u32, t.len()));
        }
        let bounds: Vec<Aabb> = tris
            .iter()
            .map(|t| {
                let mut b = Aabb::EMPTY;
                for &i in t {
                    b.add(verts[i as usize]);
                }
                b
            })
            .collect();
        let bvh = Bvh::build(&bounds);
        Ok(Self {
            verts,
            tris,
            owner,
            bvh,
        })
    }
}

impl Tracer for TriWorld {
    fn name(&self) -> &'static str {
        "tri"
    }

    fn trace(&self, ray: &Ray) -> Hit {
        let best = self.bvh.closest(ray, |i| {
            let t = self.tris[i as usize];
            ray_triangle(
                ray,
                self.verts[t[0] as usize],
                self.verts[t[1] as usize],
                self.verts[t[2] as usize],
            )
        });
        match best {
            None => Hit::MISS,
            Some((t, i)) => {
                let tri = self.tris[i as usize];
                let dist_mm = t.round_mm();
                Hit {
                    hit: true,
                    dist_mm,
                    normal: normal_1e6(
                        self.verts[tri[0] as usize],
                        self.verts[tri[1] as usize],
                        self.verts[tri[2] as usize],
                    ),
                    kind: if self.owner[i as usize] == u32::MAX {
                        KIND_TERRAIN
                    } else {
                        KIND_INSTANCE
                    },
                    penetrating: dist_mm == 0,
                    face: i as i32,
                }
            }
        }
    }
}
