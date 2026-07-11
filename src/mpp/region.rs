//! Region-AABB computation -> grid dims + the 4 .mpp header floats
//! (port of mpp/region.py; all float32 steps mirrored exactly — that rounding
//! is load-bearing at the ULP boundaries the 10-unit snap straddles).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::geom::{Aabb, Mat4, Vec3};
use super::layout::{Layout, LayoutObject};
use super::pipeline::Context;

pub const CELL32: f32 = 0.4;
pub const PAD32: f32 = 0.2;
pub const EXPAND: f64 = 1.0;
pub const SNAP: f32 = 10.0;

/// Resolve a DAT-relative mesh path ("media/...") against mod-over-base dirs.
pub fn resolve_mesh_path(media_dir: &Path, rel: &str, fallback_dir: Option<&Path>) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let norm = rel.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').collect();
    let rel2 = if parts
        .first()
        .map(|p| p.eq_ignore_ascii_case("media"))
        .unwrap_or(false)
    {
        parts[1..].join("/")
    } else {
        norm.clone()
    };
    for d in [Some(media_dir), fallback_dir].into_iter().flatten() {
        let cand = d.join(rel2.replace('/', std::path::MAIN_SEPARATOR_STR));
        // Windows FS is case-insensitive; exists() covers the Python
        // case-insensitive-listdir fallback there.
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// COLLISION ENABLED room-piece property; absent => true.
pub fn collision_enabled(piece: &LayoutObject) -> bool {
    match piece.props.get("COLLISION ENABLED") {
        None => true,
        Some(ce) => ce.trim().to_lowercase() != "false",
    }
}

/// Pick the ACTIVE collision mesh for a stored VISUAL index
/// (sub_102317F0 / sub_10230ED0 / sub_1000CF20).
pub fn select_collision_file<'a>(pd: &'a super::dat::PieceDef, visual: i64) -> &'a str {
    let colls: &[String] = &pd.collision_files;
    if colls.is_empty() {
        return "";
    }
    let nvis = if !pd.files.is_empty() { pd.files.len() } else { colls.len() };
    let ncol = colls.len();
    let mut v = visual;
    if !(0 <= v && (v as usize) < nvis.max(1)) {
        v = 0;
    }
    let mut col_idx = if ncol == nvis { v as usize } else { 0 };
    if col_idx >= ncol {
        col_idx = 0;
    }
    &colls[col_idx]
}

/// Whether a piece's collision box extends the region box (backdrop/feature
/// split; BAKE is NOT a box gate).
fn registers_for_pathing(layout: &Layout, piece_idx: usize, weighted_backdrop: bool) -> bool {
    if super::native::in_backdrop_group(layout, piece_idx, weighted_backdrop) {
        return false;
    }
    if super::native::link_under_feature_tagged_group(layout, piece_idx) {
        return false;
    }
    true
}

/// Ogre AxisAlignedBox::transform in float32; returns (minx, minz, maxx, maxz).
pub fn transform_bounds_f32(bx: &Aabb, m: &Mat4) -> (f32, f32, f32, f32) {
    let mf: Vec<[f32; 4]> = m
        .m
        .iter()
        .map(|r| [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32])
        .collect();
    let (mn, mx) = (bx.min, bx.max);
    let corners = [
        (mn.x, mn.y, mn.z),
        (mx.x, mn.y, mn.z),
        (mn.x, mx.y, mn.z),
        (mx.x, mx.y, mn.z),
        (mn.x, mn.y, mx.z),
        (mx.x, mn.y, mx.z),
        (mn.x, mx.y, mx.z),
        (mx.x, mx.y, mx.z),
    ];
    let mut minx = f32::INFINITY;
    let mut minz = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxz = f32::NEG_INFINITY;
    for (cx, cy, cz) in corners {
        let (cx, cy, cz) = (cx as f32, cy as f32, cz as f32);
        let x = (mf[0][0] * cx + mf[0][1] * cy) + (mf[0][2] * cz + mf[0][3]);
        let z = (mf[2][0] * cx + mf[2][1] * cy) + (mf[2][2] * cz + mf[2][3]);
        minx = minx.min(x);
        maxx = maxx.max(x);
        minz = minz.min(z);
        maxz = maxz.max(z);
    }
    (minx, minz, maxx, maxz)
}

/// Structural EXIT doorway prop layouts a connection tile's "Exit" link bakes.
const STRUCT_EXIT_LAYOUTS: [&str; 9] = [
    "EXIT.LAYOUT",
    "CAVE_EXIT.LAYOUT",
    "ENTRANCE_EXIT.LAYOUT",
    "CATACOMBS_EXIT.LAYOUT",
    "EXIT_01.LAYOUT",
    "EXIT_TOWER.LAYOUT",
    "CAVEEXIT_LIT.LAYOUT",
    "ARENA_EXIT.LAYOUT",
    "ARENA_EXIT2.LAYOUT",
];

/// Resolve a Layout Link's `LAYOUT FILE` to an absolute path.
///
/// mod-over-base VFS overlay: try `media_dir` (the mod) first, then `fallback_dir`
/// (the base install), EXACTLY as the collision-mesh resolver already does. A mod
/// tile whose Layout Link targets a BASE-game prop layout (e.g. CABIN_01_NOSPAWNER,
/// which lives only in the install, not the mod) must resolve against the install
/// or its collision never enters the bake -> W2w under-block of the whole prop
/// footprint. The editor loads the mod over the base install, so this is the
/// faithful resolution order, not a heuristic. No-op for vanilla (media == base).
pub fn resolve_link_layout(
    media_dir: &Path,
    link_file: &str,
    fallback_dir: Option<&Path>,
) -> Option<PathBuf> {
    if link_file.is_empty() {
        return None;
    }
    let norm = link_file.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').collect();
    let rel = if parts
        .first()
        .map(|p| p.eq_ignore_ascii_case("media"))
        .unwrap_or(false)
    {
        parts[1..].join("/")
    } else {
        norm.clone()
    };
    let rel_os = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
    for root in [Some(media_dir), fallback_dir].into_iter().flatten() {
        let cand = root.join(&rel_os);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum LinkKind {
    Exit,
    Tile,
    None,
}

/// Classify a Layout Link target: structural EXIT / tile-geometry ALIAS / none.
pub fn link_target_kind(obj: &LayoutObject) -> LinkKind {
    if obj.descriptor != "Layout Link" {
        return LinkKind::None;
    }
    let lf = obj.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
    if lf.is_empty() {
        return LinkKind::None;
    }
    let base = lf
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_uppercase();
    if STRUCT_EXIT_LAYOUTS.contains(&base.as_str()) {
        return LinkKind::Exit;
    }
    let norm = lf.replace('\\', "/").to_uppercase();
    if norm.contains("/LAYOUTS/") {
        return LinkKind::Tile;
    }
    LinkKind::None
}

/// Renormalise the 3x3 basis COLUMNS to unit length (link SCALE stripped),
/// translation preserved. The editor bakes a link collider with position +
/// rotation only, never its scale.
pub fn strip_link_scale(m4: &Mat4) -> Mat4 {
    let mut m = *m4;
    for c in 0..3 {
        let mag = (m.m[0][c].powi(2) + m.m[1][c].powi(2) + m.m[2][c].powi(2)).sqrt();
        if mag > 1e-6 {
            for r in 0..3 {
                m.m[r][c] /= mag;
            }
        }
    }
    m
}

/// A Layout Link the editor bakes into the pathing REGION (header box):
/// "Layout Link" only, gated by the shared runtime-vs-structural predicate.
fn is_baked_structural_link(layout: &Layout, obj_idx: usize) -> bool {
    layout.objects[obj_idx].descriptor == "Layout Link"
        && super::native::link_is_baked_prop(layout, obj_idx)
}

pub struct RegionResult {
    pub aabb: Aabb,
    pub n_pieces_used: usize,
    pub n_pieces_missing: usize,
}

struct Extents {
    minx: f32,
    maxx: f32,
    minz: f32,
    maxz: f32,
    used: usize,
}

#[allow(clippy::too_many_arguments)]
fn merge_baked_links(
    layout: &Layout,
    ctx: &Context,
    ext: &mut Extents,
    world: Option<&Mat4>,
    visited: &HashSet<String>,
    depth: usize,
) {
    if depth > 6 {
        return;
    }
    for (oi, obj) in layout.objects.iter().enumerate() {
        if link_target_kind(obj) == LinkKind::None && !is_baked_structural_link(layout, oi) {
            continue;
        }
        let lf = obj.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
        let sub_path = match resolve_link_layout(&ctx.media_dir, lf, ctx.fallback_dir.as_deref()) {
            Some(p) => p,
            None => continue,
        };
        let key = sub_path.to_string_lossy().to_lowercase();
        if visited.contains(&key) {
            continue;
        }
        let sub = match super::layout::load_layout_file(&sub_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let link_world = strip_link_scale(&obj.world);
        let world0 = match world {
            None => link_world,
            Some(w) => w.matmul(&link_world),
        };
        for pi in sub.room_piece_indices() {
            let piece = &sub.objects[pi];
            if !collision_enabled(piece) {
                continue;
            }
            let pd = match ctx.guid.get(&piece.guid) {
                Some(pd) => pd,
                None => continue,
            };
            if !registers_for_pathing(&sub, pi, true) {
                continue;
            }
            let coll = select_collision_file(pd, piece.visual);
            if coll.is_empty() {
                continue;
            }
            let b = match ctx.get_bounds(coll) {
                Some(b) => b,
                None => continue,
            };
            let w = world0.matmul(&piece.world);
            let (pmnx, pmnz, pmxx, pmxz) = transform_bounds_f32(&b, &w);
            ext.minx = ext.minx.min(pmnx);
            ext.maxx = ext.maxx.max(pmxx);
            ext.minz = ext.minz.min(pmnz);
            ext.maxz = ext.maxz.max(pmxz);
            ext.used += 1;
        }
        let mut v2 = visited.clone();
        v2.insert(key);
        merge_baked_links(&sub, ctx, ext, Some(&world0), &v2, depth + 1);
    }
}

/// Region AABB (X,Z): merge of collision-enabled room pieces' COLLISION mesh
/// bounds (float32 8-corner refit) + the baked Layout-Link geometry.
pub fn compute_region_aabb_collision(layout: &Layout, ctx: &Context) -> RegionResult {
    let mut ext = Extents {
        minx: f32::INFINITY,
        maxx: f32::NEG_INFINITY,
        minz: f32::INFINITY,
        maxz: f32::NEG_INFINITY,
        used: 0,
    };
    let mut missing = 0usize;
    for pi in layout.room_piece_indices() {
        let piece = &layout.objects[pi];
        if !collision_enabled(piece) {
            continue;
        }
        let pd = match ctx.guid.get(&piece.guid) {
            Some(pd) => pd,
            None => continue,
        };
        if !registers_for_pathing(layout, pi, false) {
            continue;
        }
        let coll = select_collision_file(pd, piece.visual);
        if coll.is_empty() {
            continue;
        }
        let b = match ctx.get_bounds(coll) {
            Some(b) => b,
            None => {
                missing += 1;
                continue;
            }
        };
        let (pmnx, pmnz, pmxx, pmxz) = transform_bounds_f32(&b, &piece.world);
        ext.minx = ext.minx.min(pmnx);
        ext.maxx = ext.maxx.max(pmxx);
        ext.minz = ext.minz.min(pmnz);
        ext.maxz = ext.maxz.max(pmxz);
        ext.used += 1;
    }

    merge_baked_links(layout, ctx, &mut ext, None, &HashSet::new(), 0);

    if ext.used == 0 {
        return RegionResult {
            aabb: Aabb::null(),
            n_pieces_used: 0,
            n_pieces_missing: missing,
        };
    }
    RegionResult {
        aabb: Aabb::new(
            Vec3::new(ext.minx as f64, 0.0, ext.minz as f64),
            Vec3::new(ext.maxx as f64, 0.0, ext.maxz as f64),
        ),
        n_pieces_used: ext.used,
        n_pieces_missing: missing,
    }
}

/// Quantize (X,Z) OUTWARD to the 10-unit tile boundary with the 0.2 pad
/// (floor((min-0.2)/10)*10 / ceil((max+0.2)/10)*10, all float32).
pub fn snap_box_outward(bx: &Aabb, quantum: f32) -> Aabb {
    if bx.null {
        return Aabb::null();
    }
    let q = quantum;
    let pad = 0.2f32;
    let lo = |v: f64| -> f64 {
        let t = ((v as f32) - pad) / q;
        (((t as f64).floor() as f32) * q) as f64
    };
    let hi = |v: f64| -> f64 {
        let t = ((v as f32) + pad) / q;
        (((t as f64).ceil() as f32) * q) as f64
    };
    Aabb::new(
        Vec3::new(lo(bx.min.x), bx.min.y, lo(bx.min.z)),
        Vec3::new(hi(bx.max.x), bx.max.y, hi(bx.max.z)),
    )
}

/// CLevel grid origin: float32((floor((f32(min) - 0.2)/0.4) - 1.0) * 0.4).
pub fn grid_origin(box_min: Vec3) -> (f64, f64) {
    let ax = |mn: f64| -> f64 {
        let t = ((mn as f32) as f64 - PAD32 as f64) / CELL32 as f64;
        let t = t.floor();
        (((t - EXPAND) * CELL32 as f64) as f32) as f64
    };
    (ax(box_min.x), ax(box_min.z))
}

/// Writer grid-dim formula (float32 divide, floor/ceil).
pub fn grid_dims(box_min: Vec3, box_max: Vec3, ox: f64, oz: f64) -> (i32, i32) {
    let fl = |coord: f64, o: f64| -> i32 {
        let t = ((coord as f32) - (o as f32)) / CELL32;
        (t as f64).floor() as i32
    };
    let ce = |coord: f64, o: f64| -> i32 {
        let t = ((coord as f32) - (o as f32)) / CELL32;
        (t as f64).ceil() as i32
    };
    let gw = ce(box_max.x, ox) - fl(box_min.x, ox);
    let gh = ce(box_max.z, oz) - fl(box_min.z, oz);
    (gw, gh)
}

pub fn header_floats(box_min: Vec3, box_max: Vec3, world_ext: Vec3) -> (f32, f32, f32, f32) {
    let world_w = (world_ext.x as f32) - (box_min.x as f32);
    let world_d = (world_ext.z as f32) - (box_min.z as f32);
    let bounds_w = (box_max.x as f32) - (box_min.x as f32);
    let bounds_d = (box_max.z as f32) - (box_min.z as f32);
    (world_w, world_d, bounds_w, bounds_d)
}

pub fn pack_header(gw: i32, gh: i32, ww: f32, wd: f32, bw: f32, bd: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&gw.to_le_bytes());
    out.extend_from_slice(&gh.to_le_bytes());
    out.extend_from_slice(&ww.to_le_bytes());
    out.extend_from_slice(&wd.to_le_bytes());
    out.extend_from_slice(&bw.to_le_bytes());
    out.extend_from_slice(&bd.to_le_bytes());
    out
}
