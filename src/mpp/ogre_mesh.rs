//! Ogre .mesh reader for the pathing pipeline (faithful port of mpp/ogre_mesh.py,
//! including its structural re-sync behaviour on unreliable chunk lengths).
//! All reads are bounds-checked and return Err where the Python reference would
//! raise (struct.error / IndexError) — the caller treats a failed mesh as None.

use super::geom::{Aabb, Vec3};

pub const M_HEADER: u16 = 0x1000;
pub const M_MESH: u16 = 0x3000;
pub const M_SUBMESH: u16 = 0x4000;
pub const M_SUBMESH_OPERATION: u16 = 0x4010;
pub const M_SUBMESH_BONE_ASSIGNMENT: u16 = 0x4100;
pub const M_SUBMESH_TEXTURE_ALIAS: u16 = 0x4200;
pub const M_GEOMETRY: u16 = 0x5000;
pub const M_GEOMETRY_VERTEX_DECLARATION: u16 = 0x5100;
pub const M_GEOMETRY_VERTEX_ELEMENT: u16 = 0x5110;
pub const M_GEOMETRY_VERTEX_BUFFER: u16 = 0x5200;
pub const M_GEOMETRY_VERTEX_BUFFER_DATA: u16 = 0x5210;
pub const M_MESH_SKELETON_LINK: u16 = 0x6000;
pub const M_MESH_BONE_ASSIGNMENT: u16 = 0x7000;
pub const M_MESH_LOD: u16 = 0x8000;
pub const M_MESH_BOUNDS: u16 = 0x9000;
pub const M_SUBMESH_NAME_TABLE: u16 = 0xA000;
pub const M_EDGE_LISTS: u16 = 0xB000;
pub const M_POSES: u16 = 0xC000;
pub const M_ANIMATIONS: u16 = 0xE000;
pub const M_TABLE_EXTREMES: u16 = 0xF000;

const VES_POSITION: u16 = 1;

type Res<T> = Result<T, String>;

fn known_id(id: u16) -> bool {
    matches!(
        id,
        M_SUBMESH
            | M_SUBMESH_OPERATION
            | M_SUBMESH_BONE_ASSIGNMENT
            | M_SUBMESH_TEXTURE_ALIAS
            | M_GEOMETRY
            | M_GEOMETRY_VERTEX_DECLARATION
            | M_GEOMETRY_VERTEX_ELEMENT
            | M_GEOMETRY_VERTEX_BUFFER
            | M_GEOMETRY_VERTEX_BUFFER_DATA
            | M_MESH_SKELETON_LINK
            | M_MESH_BONE_ASSIGNMENT
            | M_MESH_LOD
            | M_MESH_BOUNDS
            | M_SUBMESH_NAME_TABLE
            | M_EDGE_LISTS
            | M_POSES
            | M_ANIMATIONS
            | M_TABLE_EXTREMES
    )
}

#[derive(Default, Clone)]
pub struct Geometry {
    pub vertex_count: usize,
    /// Parsed f32 positions widened to f64 (matches Python's float).
    pub positions: Vec<[f64; 3]>,
}

#[derive(Clone)]
pub struct SubMesh {
    pub use_shared: bool,
    pub operation: u16,
    pub indices: Vec<u32>,
    pub geometry: Option<Geometry>,
    pub material: String,
}

pub struct Mesh {
    pub bounds: Aabb,
    pub bounds_from_chunk: bool,
    pub shared_geometry: Option<Geometry>,
    pub submeshes: Vec<SubMesh>,
}

struct R<'a> {
    d: &'a [u8],
}

impl<'a> R<'a> {
    fn byte(&self, off: usize) -> Res<u8> {
        self.d.get(off).copied().ok_or_else(|| "eof".to_string())
    }
    fn u16(&self, off: usize) -> Res<u16> {
        let b = self.d.get(off..off + 2).ok_or("eof")?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&self, off: usize) -> Res<u32> {
        let b = self.d.get(off..off + 4).ok_or("eof")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f32(&self, off: usize) -> Res<f32> {
        Ok(f32::from_bits(self.u32(off)?))
    }
    fn chunk_hdr(&self, off: usize) -> Res<(u16, u32)> {
        Ok((self.u16(off)?, self.u32(off + 2)?))
    }
}

/// Read an M_GEOMETRY body starting just past the 6-byte header.
fn read_geometry(r: &R, mut off: usize, hard_end: usize) -> Res<(Geometry, usize)> {
    let mut geo = Geometry {
        vertex_count: r.u32(off)? as usize,
        positions: Vec::new(),
    };
    off += 4;

    let mut pos_source: Option<u16> = None;
    let mut pos_offset: usize = 0;
    let mut buffers: std::collections::HashMap<u16, (usize, &[u8])> =
        std::collections::HashMap::new();

    let mut cur = off;
    while cur + 6 <= hard_end {
        let (cid, clen) = r.chunk_hdr(cur)?;
        if cid == M_GEOMETRY_VERTEX_DECLARATION {
            let mut inner = cur + 6;
            while inner + 6 <= hard_end {
                let (eid, elen) = r.chunk_hdr(inner)?;
                if eid != M_GEOMETRY_VERTEX_ELEMENT {
                    break;
                }
                let source = r.u16(inner + 6)?;
                let _vtype = r.u16(inner + 8)?;
                let semantic = r.u16(inner + 10)?;
                let voffset = r.u16(inner + 12)?;
                if semantic == VES_POSITION && pos_source.is_none() {
                    pos_source = Some(source);
                    pos_offset = voffset as usize;
                }
                // element chunk = 6-byte hdr + 10-byte body (5x u16: source,type,
                // semantic,offset,index) = 16. (The old `+= 8+6` dropped the index
                // u16, misaligning every element after POSITION.) Mirrors the fixed
                // Python reference: advance by the chunk's own length when plausible.
                let elen = elen as usize;
                inner += if (6..=64).contains(&elen) { elen } else { 16 };
            }
            let clen = clen as usize;
            cur = if clen >= 6 && clen <= hard_end.saturating_sub(cur) {
                cur + clen
            } else {
                inner
            };
        } else if cid == M_GEOMETRY_VERTEX_BUFFER {
            let bind = r.u16(cur + 6)?;
            let stride = r.u16(cur + 8)? as usize;
            let data_off = cur + 10;
            let _ = r.chunk_hdr(data_off)?; // (did, dlen) read but unused, as in Python
            let raw_start = data_off + 6;
            let nbytes = geo.vertex_count * stride;
            // Python slices leniently (raw may come out short); mirror that.
            let end = (raw_start + nbytes).min(r.d.len());
            let raw = if raw_start <= r.d.len() { &r.d[raw_start..end] } else { &[][..] };
            buffers.insert(bind, (stride, raw));
            cur = raw_start + nbytes;
        } else {
            break;
        }
    }

    if let Some(src) = pos_source {
        if let Some(&(stride, raw)) = buffers.get(&src) {
            let mut positions = Vec::with_capacity(geo.vertex_count);
            for i in 0..geo.vertex_count {
                let base = i * stride + pos_offset;
                if base + 12 <= raw.len() {
                    let x = f32::from_le_bytes(raw[base..base + 4].try_into().unwrap());
                    let y = f32::from_le_bytes(raw[base + 4..base + 8].try_into().unwrap());
                    let z = f32::from_le_bytes(raw[base + 8..base + 12].try_into().unwrap());
                    positions.push([x as f64, y as f64, z as f64]);
                }
            }
            geo.positions = positions;
        }
    }
    Ok((geo, cur))
}

/// Read an M_SUBMESH body (past the 6-byte header).
fn read_submesh(r: &R, mut off: usize, hard_end: usize) -> Res<(SubMesh, usize)> {
    let start = off;
    while off < hard_end && r.byte(off)? != 0x0A {
        off += 1;
    }
    let material: String = r.d[start..off.min(r.d.len())].iter().map(|&b| b as char).collect();
    off += 1; // newline
    let use_shared = r.byte(off)? != 0;
    off += 1;
    let index_count = r.u32(off)? as usize;
    off += 4;
    let index_32bit = r.byte(off)? != 0;
    off += 1;
    // struct.unpack_from would raise on a short buffer — mirror with a check.
    let need = index_count * if index_32bit { 4 } else { 2 };
    if off + need > r.d.len() {
        return Err("short index buffer".into());
    }
    let mut indices = Vec::with_capacity(index_count);
    if index_32bit {
        for i in 0..index_count {
            indices.push(r.u32(off + 4 * i)?);
        }
        off += 4 * index_count;
    } else {
        for i in 0..index_count {
            indices.push(r.u16(off + 2 * i)? as u32);
        }
        off += 2 * index_count;
    }

    let mut sm = SubMesh {
        use_shared,
        operation: 4,
        indices,
        geometry: None,
        material,
    };

    if !sm.use_shared && off + 6 <= hard_end {
        let (cid, _clen) = r.chunk_hdr(off)?;
        if cid == M_GEOMETRY {
            let (geo, after) = read_geometry(r, off + 6, hard_end)?;
            sm.geometry = Some(geo);
            off = after;
        }
    }
    while off + 6 <= hard_end {
        let (cid, clen) = r.chunk_hdr(off)?;
        let clen = clen as usize;
        if cid == M_SUBMESH_OPERATION {
            sm.operation = r.u16(off + 6)?;
            off += if clen >= 6 && clen <= hard_end - off { clen } else { 8 };
        } else if cid == M_SUBMESH_BONE_ASSIGNMENT || cid == M_SUBMESH_TEXTURE_ALIAS {
            off += if clen >= 6 && clen <= hard_end - off { clen } else { 6 };
        } else {
            break;
        }
    }
    Ok((sm, off))
}

fn resync(r: &R, off: usize, end: usize) -> usize {
    let mut o = off;
    while o + 6 <= end {
        if let Ok((cid, clen)) = r.chunk_hdr(o) {
            let clen = clen as usize;
            if known_id(cid) && clen >= 6 && clen <= end - o {
                return o;
            }
        }
        o += 1;
    }
    end
}

pub fn parse_mesh(data: &[u8]) -> Res<Mesh> {
    let r = R { d: data };
    let mut off = 0usize;
    let hid = r.u16(off)?;
    off += 2;
    if hid != M_HEADER {
        return Err(format!("not an Ogre mesh (header 0x{hid:04x})"));
    }
    while off < data.len() && r.byte(off)? != 0x0A {
        off += 1;
    }
    off += 1;

    let (cid, _clen) = r.chunk_hdr(off)?;
    if cid != M_MESH {
        return Err(format!("expected M_MESH, got 0x{cid:04x}"));
    }
    let mesh_end = data.len();
    off += 6;
    off += 1; // bool skeletallyAnimated

    let mut shared_geo: Option<Geometry> = None;
    let mut submeshes: Vec<SubMesh> = Vec::new();
    let mut bounds: Option<Aabb> = None;
    let mut bounds_from_chunk = false;

    while off + 6 <= mesh_end {
        let (cid, clen) = r.chunk_hdr(off)?;
        let clen_us = clen as usize;
        if cid == M_GEOMETRY {
            let (g, after) = read_geometry(&r, off + 6, mesh_end)?;
            shared_geo = Some(g);
            off = after;
        } else if cid == M_SUBMESH {
            let (sm, after) = read_submesh(&r, off + 6, mesh_end)?;
            submeshes.push(sm);
            off = after;
        } else if cid == M_MESH_BOUNDS {
            let mn = Vec3::new(r.f32(off + 6)? as f64, r.f32(off + 10)? as f64, r.f32(off + 14)? as f64);
            let mx = Vec3::new(r.f32(off + 18)? as f64, r.f32(off + 22)? as f64, r.f32(off + 26)? as f64);
            bounds = Some(Aabb::new(mn, mx));
            bounds_from_chunk = true;
            off += 6 + 28;
        } else if cid == M_MESH_SKELETON_LINK {
            off += if clen_us >= 6 && clen_us <= mesh_end - off { clen_us } else { 6 };
        } else if known_id(cid) {
            if clen_us >= 6 && clen_us <= mesh_end - off {
                off += clen_us;
            } else {
                off = resync(&r, off + 6, mesh_end);
            }
        } else {
            let nxt = resync(&r, off + 2, mesh_end);
            if nxt <= off {
                break;
            }
            off = nxt;
        }
    }

    let bounds = match bounds {
        Some(b) => b,
        None => {
            let mut b = Aabb::null();
            if let Some(g) = &shared_geo {
                for p in &g.positions {
                    b.merge_point(Vec3::new(p[0], p[1], p[2]));
                }
            }
            for sm in &submeshes {
                if let Some(g) = &sm.geometry {
                    for p in &g.positions {
                        b.merge_point(Vec3::new(p[0], p[1], p[2]));
                    }
                }
            }
            b
        }
    };

    Ok(Mesh {
        bounds,
        bounds_from_chunk,
        shared_geometry: shared_geo,
        submeshes,
    })
}

pub fn load_mesh_file(path: &std::path::Path) -> Res<Mesh> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_mesh(&data)
}
