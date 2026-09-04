//! On-disk formats shared with the Unreal commandlets (all little-endian).
//!
//! * `ORRYRAY1` — rays: `u64 seed, u32 n, [i64 ox oy oz, i64 dx dy dz (unit × 1e9), i64 max_mm]`
//! * `ORRYHIT1` — hits: `u32 n, u8 source, [u8 hit, i64 dist_mm, i32 nx ny nz (× 1e6), u8 kind, u8 penetrating, i32 face]`
//! * `ORRYTRI1` / `ORRYHF_1` / `ORRYVOX1` — collision packages, see each representation module.

use std::error::Error;

/// Direction components are the unit vector scaled by this; the ray parameter `t` is then in millimetres.
pub const DIR_SCALE: i64 = 1_000_000_000;

pub const KIND_NONE: u8 = 0;
pub const KIND_TERRAIN: u8 = 1;
pub const KIND_INSTANCE: u8 = 2;

/// `source` byte of a hit file: Unreal writes its `bTraceComplex` flag (0/1); the ruleset writes 2.
pub const SOURCE_RUST: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub o: [i64; 3],
    pub d: [i64; 3],
    pub max_mm: i64,
}

pub struct RayFile {
    pub seed: u64,
    pub rays: Vec<Ray>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hit {
    pub hit: bool,
    pub dist_mm: i64,
    pub normal: [i32; 3],
    pub kind: u8,
    pub penetrating: bool,
    pub face: i32,
}

impl Hit {
    pub const MISS: Self = Self {
        hit: false,
        dist_mm: -1,
        normal: [0; 3],
        kind: KIND_NONE,
        penetrating: false,
        face: -1,
    };
}

pub struct HitFile {
    pub source: u8,
    pub hits: Vec<Hit>,
}

/// A bounds-checked little-endian cursor.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Box<dyn Error>> {
        let end = self.pos.checked_add(n).ok_or("overflow")?;
        let s = self.buf.get(self.pos..end).ok_or("truncated file")?;
        self.pos = end;
        Ok(s)
    }

    pub fn magic(&mut self, expected: &[u8; 8]) -> Result<(), Box<dyn Error>> {
        let m = self.take(8)?;
        if m == expected {
            Ok(())
        } else {
            Err(format!("bad magic {m:?}, expected {expected:?}").into())
        }
    }

    pub fn u8(&mut self) -> Result<u8, Box<dyn Error>> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Box<dyn Error>> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, Box<dyn Error>> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub fn i32(&mut self) -> Result<i32, Box<dyn Error>> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64, Box<dyn Error>> {
        let s = self.take(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(s);
        Ok(u64::from_le_bytes(b))
    }

    pub fn i64(&mut self) -> Result<i64, Box<dyn Error>> {
        Ok(self.u64()? as i64)
    }
}

pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn magic(&mut self, m: &[u8; 8]) {
        self.0.extend_from_slice(m);
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

/// The header every collision package starts with.
pub struct PackageHeader {
    pub seed: u64,
    pub body: u32,
    pub min: [i64; 3],
    pub max: [i64; 3],
}

pub fn read_header(c: &mut Cursor<'_>, magic: &[u8; 8]) -> Result<PackageHeader, Box<dyn Error>> {
    c.magic(magic)?;
    let seed = c.u64()?;
    let body = c.u32()?;
    let min = [c.i64()?, c.i64()?, c.i64()?];
    let max = [c.i64()?, c.i64()?, c.i64()?];
    Ok(PackageHeader {
        seed,
        body,
        min,
        max,
    })
}

/// A triangle soup as the cook writes it: `u32 nv, [i64 x y z], u32 nt, [u32 a b c]`.
pub struct Soup {
    pub verts: Vec<[i64; 3]>,
    pub tris: Vec<[u32; 3]>,
}

pub fn read_soup(c: &mut Cursor<'_>) -> Result<Soup, Box<dyn Error>> {
    let nv = c.u32()? as usize;
    let mut verts = Vec::with_capacity(nv);
    for _ in 0..nv {
        verts.push([c.i64()?, c.i64()?, c.i64()?]);
    }
    let nt = c.u32()? as usize;
    let mut tris = Vec::with_capacity(nt);
    for _ in 0..nt {
        let t = [c.u32()?, c.u32()?, c.u32()?];
        if t.iter().any(|&i| i as usize >= nv) {
            return Err("triangle index out of range".into());
        }
        tris.push(t);
    }
    Ok(Soup { verts, tris })
}

pub fn read_rays(bytes: &[u8]) -> Result<RayFile, Box<dyn Error>> {
    let mut c = Cursor::new(bytes);
    c.magic(b"ORRYRAY1")?;
    let seed = c.u64()?;
    let n = c.u32()? as usize;
    let mut rays = Vec::with_capacity(n);
    for _ in 0..n {
        let o = [c.i64()?, c.i64()?, c.i64()?];
        let d = [c.i64()?, c.i64()?, c.i64()?];
        let max_mm = c.i64()?;
        rays.push(Ray { o, d, max_mm });
    }
    Ok(RayFile { seed, rays })
}

pub fn write_rays(seed: u64, rays: &[Ray]) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.magic(b"ORRYRAY1");
    w.u64(seed);
    w.u32(rays.len() as u32);
    for r in rays {
        for v in r.o {
            w.i64(v);
        }
        for v in r.d {
            w.i64(v);
        }
        w.i64(r.max_mm);
    }
    w.0
}

pub fn read_hits(bytes: &[u8]) -> Result<HitFile, Box<dyn Error>> {
    let mut c = Cursor::new(bytes);
    c.magic(b"ORRYHIT1")?;
    let n = c.u32()? as usize;
    let source = c.u8()?;
    let mut hits = Vec::with_capacity(n);
    for _ in 0..n {
        let hit = c.u8()? != 0;
        let dist_mm = c.i64()?;
        let normal = [c.i32()?, c.i32()?, c.i32()?];
        let kind = c.u8()?;
        let penetrating = c.u8()? != 0;
        let face = c.i32()?;
        hits.push(Hit {
            hit,
            dist_mm,
            normal,
            kind,
            penetrating,
            face,
        });
    }
    Ok(HitFile { source, hits })
}

pub fn write_hits(hits: &[Hit], source: u8) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.magic(b"ORRYHIT1");
    w.u32(hits.len() as u32);
    w.u8(source);
    for h in hits {
        w.u8(u8::from(h.hit));
        w.i64(h.dist_mm);
        for v in h.normal {
            w.i32(v);
        }
        w.u8(h.kind);
        w.u8(u8::from(h.penetrating));
        w.i32(h.face);
    }
    w.0
}
