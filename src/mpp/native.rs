//! Faithful port of TL2's MPP pathing-grid classifier (mpp/native.py), itself a
//! function-by-function port of EditorGuts.dll (CLevel_BuildWritePathingGrid_MPP
//! @ 0x10200920, multi_raycast sub_101EF170, swept_tri sub_10067670, ...).
//! Output is byte-identical to the Python build_grid / build_grid_fast; the
//! per-cell classification is embarrassingly parallel and rayon-parallelized
//! per row (the enclosure pass stays sequential — it cascades).

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use super::geom::Mat4;
use super::layout::{Layout, LayoutObject};
use super::pipeline::Context;
use super::region::{
    collision_enabled, link_target_kind, resolve_link_layout, select_collision_file,
    strip_link_scale, LinkKind,
};

pub const CELL: f64 = 0.4;
pub const HALF: f64 = 0.2;
pub const RADIUS: f64 = 0.10000001;
pub const CLEARANCE_RADIUS: f64 = 0.0;
pub const CLEARANCE_LEN: f64 = 0.30000001;
pub const CLEARANCE_LIFT: f64 = 1.5;
pub const HEIGHT_CLAMP: f64 = 80.0;
pub const SIDE_EPS: f64 = 9.9999997e-06;
pub const BUCKET: f64 = 2.0;
const NOCOLLIDE_MAT: &str = "nocollide";

/// Layout-Link targets that are runtime/random/spawned content (NOT baked).
const RUNTIME_LINK_SUBSTR: [&str; 8] = [
    "CHEST",
    "LOOTABLE",
    "ADVENTURER",
    "DAPPLE",
    "BONEPILE",
    "SUNDIAL",
    "FISHINGHOLE",
    "BIGMONSTER",
];

type P3 = [f64; 3];

#[derive(Clone)]
pub struct Tri {
    pub a: P3,
    pub b: P3,
    pub c: P3,
    pub nop: bool,
}

// ── group-ancestry filters ─────────────────────────────────────────────────

fn walk_ancestors<F: FnMut(&LayoutObject) -> bool>(
    layout: &Layout,
    obj: &LayoutObject,
    limit: usize,
    mut pred: F,
) -> bool {
    let mut pid = obj.props.get("PARENTID").cloned();
    let mut seen = 0usize;
    while let Some(p) = pid {
        if p.is_empty() || p == "-1" || seen >= limit {
            break;
        }
        let o = match layout.by_id.get(&p) {
            Some(&i) => &layout.objects[i],
            None => break,
        };
        if pred(o) {
            return true;
        }
        pid = o.props.get("PARENTID").cloned();
        seen += 1;
    }
    false
}

/// CRandomGroup CHOICE / ACTIVE THEMES / DEACTIVE THEMES on any ancestor =>
/// excluded from the .mpp master collision (sub_1022FF80). `weighted` is vestigial.
///
/// sub_1022FF80 tests FIVE consecutive wstring theme fields on the CRandomGroup
/// (offsets +372/+400/+428/+456/+484); of these only ACTIVE THEMES and DEACTIVE
/// THEMES are ever authored on shipped level-tile groups (corpus scan: ACTIVE 912,
/// DEACTIVE 93, other three 0). GROUND TRUTH (frida gate-input trace of
/// CLevel_RebuildLevel_GenPathing 0x10203fb4 on GENERIC_CAVE 1X1_SEW): all 17
/// over-wall residual pieces sit under Group 'no goblin' DEACTIVE THEMES=... and
/// are backdrop-excluded by the DLL (bd=1) — omitting DEACTIVE THEMES over-walled
/// them. Adding it: full-corpus -6307 over-wall / -10858 total diff, W2w +20 (flat),
/// 52 tiles better / 1 worse (+5) / +1 dim-mismatch fixed. Refutes the earlier
/// "per-instance runtime nocollide" residual conclusion — the drop is a
/// deterministic, text-derivable DEACTIVE-THEMES group gate, not runtime state.
pub fn in_backdrop_group(layout: &Layout, obj_idx: usize, _weighted: bool) -> bool {
    let obj = &layout.objects[obj_idx];
    walk_ancestors(layout, obj, 16, |o| {
        let choice = o.props.get("CHOICE").map(|s| !s.trim().is_empty()).unwrap_or(false);
        let nonempty = |k: &str| o.props.get(k).map(|s| !s.trim().is_empty()).unwrap_or(false);
        choice || nonempty("ACTIVE THEMES") || nonempty("DEACTIVE THEMES")
    })
}

/// Any ancestor group carrying a non-empty TAG or ACTIVE THEMES => the object
/// is runtime/conditional content (feature-tagged split).
pub fn link_under_feature_tagged_group(layout: &Layout, obj_idx: usize) -> bool {
    let obj = &layout.objects[obj_idx];
    walk_ancestors(layout, obj, 24, |o| {
        let tag = o.props.get("TAG").map(|s| !s.trim().is_empty()).unwrap_or(false);
        let themes = o
            .props
            .get("ACTIVE THEMES")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        tag || themes
    })
}

/// SINGLE SOURCE OF TRUTH for "does the editor bake this Layout Link's
/// collision into the pathing pass" (body AND header share it).
pub fn link_is_baked_prop(layout: &Layout, obj_idx: usize) -> bool {
    let obj = &layout.objects[obj_idx];
    if !matches!(
        obj.descriptor.as_str(),
        "Layout Link" | "Layout Link Timeline" | "Layout Link Controller"
    ) {
        return false;
    }
    let lf = obj.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
    let tgt = lf
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_uppercase();
    if tgt.is_empty() {
        return false;
    }
    let is_timeline = obj.descriptor == "Layout Link Timeline";
    if let Some(norpc) = obj.props.get("NO ROOMPIECE COLLISION") {
        if norpc.trim().to_lowercase() == "true" {
            return false;
        }
    }
    // "RAISED_CHEST" = a structural raised platform (Layout Link Timeline, walkable-floor +
    // wall pieces); only the chest ON it is runtime. It merely CONTAINS "CHEST". Frida trace
    // (CATACOMB 1X1_NEW_JT_A): the DLL bakes its floor pieces, so offline voided the footprint.
    // A/B: w2V -1956, everything else flat, 5 tiles / 0 regressions. Except like NOSPAWNER.
    if RUNTIME_LINK_SUBSTR.iter().any(|s| tgt.contains(s)) && !tgt.contains("RAISED_CHEST") {
        return false;
    }
    // "WRANDOMDECO" (DESK02_WRANDOMDECO) = a structural desk with random DECO on top; the desk
    // base is baked collision, only the deco is random. Frida under-wall trace (VAULT 1X1_S/NS):
    // the DLL bakes it at the link origin. Other RANDOM furniture (TABLE_WITH_RANDOMPROPS,
    // WALL_RANDOM_01, FIREPLACE_RANDOM) traced NOT baked, so they stay rejected. Except like FLYER.
    if tgt.contains("RANDOM") && !tgt.contains("RANDOMIZED") && !tgt.contains("WRANDOMDECO") {
        return false;
    }
    let baked_nest = tgt == "SPAWNER_01.LAYOUT" || tgt.starts_with("JACKALBEAST_SPAWNER");
    // "NOSPAWNER" = a structural building/cabin variant with the spawner REMOVED
    // (PB_CEM_BUILDING_3B_NOSPAWNER / CABIN_0x_NOSPAWNER); it only CONTAINS "SPAWNER".
    // The DLL bakes it as static geometry (frida under-wall trace of ACT3_PASS2 PASS2_LM_A:
    // 4 PB_CEM_BUILDING_3B_NOSPAWNER pieces baked at their link origins). Except it like FLYER.
    if tgt.contains("SPAWNER")
        && !tgt.contains("FLYER")
        && !tgt.contains("NOSPAWNER")
        && !is_timeline
        && !baked_nest
    {
        return false;
    }
    if link_under_feature_tagged_group(layout, obj_idx) {
        return false;
    }
    if in_backdrop_group(layout, obj_idx, false) {
        return false;
    }
    true
}

// ── geometry kernels (exact ports) ─────────────────────────────────────────

#[inline]
fn sub(a: P3, b: P3) -> P3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
fn dot(a: P3, b: P3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn cross(a: P3, b: P3) -> P3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[inline]
fn len3(a: P3) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// sub_10293D50: ray (s->e) vs the plane through p0 with normal n.
#[inline]
fn ray_plane(s: P3, e: P3, p0: P3, n: P3) -> Option<P3> {
    let (dx, dy, dz) = (e[0] - s[0], e[1] - s[1], e[2] - s[2]);
    let denom = n[0] * dx + n[1] * dy + n[2] * dz;
    if denom.abs() < 9.9999997e-06 {
        return None;
    }
    let t = (n[0] * (p0[0] - s[0]) + n[1] * (p0[1] - s[1]) + n[2] * (p0[2] - s[2])) / denom;
    Some([s[0] + dx * t, s[1] + dy * t, s[2] + dz * t])
}

/// sub_10066E50: barycentric point-in-triangle.
#[inline]
fn point_in_tri(p: P3, a: P3, b: P3, c: P3) -> bool {
    let v0 = sub(c, a);
    let v1 = sub(b, a);
    let v2 = sub(p, a);
    let d00 = dot(v0, v0);
    let d01 = dot(v0, v1);
    let d02 = dot(v0, v2);
    let d11 = dot(v1, v1);
    let d12 = dot(v1, v2);
    let den = d00 * d11 - d01 * d01;
    if den.abs() < 1e-20 {
        return false;
    }
    let inv = 1.0 / den;
    let u = (d11 * d02 - d01 * d12) * inv;
    let v = (d00 * d12 - d01 * d02) * inv;
    u >= -1e-6 && v >= -1e-6 && (u + v) <= 1.0 + 1e-6
}

/// Closest point on triangle (Ericson RTCD) — the swept-radius edge rounding.
#[inline]
fn closest_on_tri(p: P3, a: P3, b: P3, c: P3) -> P3 {
    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(p, a);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = sub(p, b);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return [a[0] + ab[0] * v, a[1] + ab[1] * v, a[2] + ab[2] * v];
    }
    let cp = sub(p, c);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return [a[0] + ac[0] * w, a[1] + ac[1] * w, a[2] + ac[2] * w];
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return [
            b[0] + (c[0] - b[0]) * w,
            b[1] + (c[1] - b[1]) * w,
            b[2] + (c[2] - b[2]) * w,
        ];
    }
    let den = 1.0 / (va + vb + vc);
    let v = vb * den;
    let w = vc * den;
    [
        a[0] + ab[0] * v + ac[0] * w,
        a[1] + ab[1] * v + ac[1] * w,
        a[2] + ab[2] * v + ac[2] * w,
    ]
}

/// sub_10067670 BVH leaf: swept (radius) ray vs one triangle.
#[inline]
pub fn swept_tri(s: P3, e: P3, radius: f64, a: P3, b: P3, c: P3) -> Option<(P3, f64)> {
    let n = cross(sub(b, a), sub(c, a));
    let hp = ray_plane(s, e, a, n)?;
    let (dx, dy, dz) = (e[0] - s[0], e[1] - s[1], e[2] - s[2]);
    let seg2 = dx * dx + dy * dy + dz * dz;
    let t = ((hp[0] - s[0]) * dx + (hp[1] - s[1]) * dy + (hp[2] - s[2]) * dz) / seg2;
    let slack = radius / seg2.sqrt();
    if t < -slack || t > 1.0 + slack {
        return None;
    }
    if point_in_tri(hp, a, b, c) {
        return Some((hp, len3(sub(hp, s))));
    }
    let cp = closest_on_tri(hp, a, b, c);
    if len3(sub(cp, hp)) <= radius {
        // back-face cull for grazes: accept only when the ray END is strictly
        // on the +normal side of the triangle's plane (sub_10293B50 gate).
        if dot(sub(a, e), n) < -SIDE_EPS {
            return Some((cp, len3(sub(cp, s))));
        }
    }
    None
}

// ── collision gather (mirrors native.gather) ──────────────────────────────

fn emit_piece(piece: &LayoutObject, m: &Mat4, ctx: &Context, tris: &mut Vec<Tri>) {
    if !collision_enabled(piece) {
        return;
    }
    let pd = match ctx.guid.get(&piece.guid) {
        Some(pd) => pd,
        None => return,
    };
    // Master-collision add gate: excluded when <BOOL>BAKE:false unless the
    // descriptor is ALWAYSBAKECOLLISION (NEVERBAKE does NOT rescue it).
    if let Some(bake) = piece.props.get("BAKE") {
        if bake.trim().to_lowercase() == "false" && !pd.alwaysbakecollision {
            return;
        }
    }
    let coll = select_collision_file(pd, piece.visual);
    if coll.is_empty() {
        return;
    }
    let mesh = match ctx.get_mesh(coll) {
        Some(m) => m,
        None => return,
    };
    let pnop = piece.nopath;
    let mm = &m.m;
    for sm in &mesh.submeshes {
        let geo = if sm.geometry.is_some() && !sm.use_shared {
            sm.geometry.as_ref()
        } else {
            mesh.shared_geometry.as_ref()
        };
        let geo = match geo {
            Some(g) if !g.positions.is_empty() => g,
            _ => continue,
        };
        let nop = pnop || sm.material.to_lowercase().contains(NOCOLLIDE_MAT);
        let idx = &sm.indices;
        let vc = geo.vertex_count;
        let pos = &geo.positions;
        let w = |vi: usize| -> P3 {
            let p = pos[vi];
            [
                mm[0][0] * p[0] + mm[0][1] * p[1] + mm[0][2] * p[2] + mm[0][3],
                mm[1][0] * p[0] + mm[1][1] * p[1] + mm[1][2] * p[2] + mm[1][3],
                mm[2][0] * p[0] + mm[2][1] * p[1] + mm[2][2] * p[2] + mm[2][3],
            ]
        };
        if sm.operation == 4 || sm.operation == 0 {
            let mut t = 0usize;
            while t + 2 < idx.len() {
                let (a, b, c) = (idx[t] as usize, idx[t + 1] as usize, idx[t + 2] as usize);
                if a < vc && b < vc && c < vc && a < pos.len() && b < pos.len() && c < pos.len() {
                    tris.push(Tri {
                        a: w(a),
                        b: w(b),
                        c: w(c),
                        nop,
                    });
                }
                t += 3;
            }
        } else if sm.operation == 5 {
            for t in 0..idx.len().saturating_sub(2) {
                let (a, mut b, mut c) = (idx[t] as usize, idx[t + 1] as usize, idx[t + 2] as usize);
                if t & 1 == 1 {
                    std::mem::swap(&mut b, &mut c);
                }
                if a < vc && b < vc && c < vc && a < pos.len() && b < pos.len() && c < pos.len() {
                    tris.push(Tri {
                        a: w(a),
                        b: w(b),
                        c: w(c),
                        nop,
                    });
                }
            }
        }
    }
}

/// Also follow non-tile baked links nested inside tile-alias sub-layouts.
/// DEFAULT ON (env MPP_NESTED_LINKS=0 to disable) — mirrors Python native._NESTED_PROP_LINKS.
fn nested_prop_links() -> bool {
    std::env::var("MPP_NESTED_LINKS").map(|v| v == "1").unwrap_or(true)
}

/// Follow `lay`'s non-tile baked-prop Layout Links (ONE level) and emit their sub-pieces,
/// transformed by the accumulated `world0`. Runs on the top layout AND (when nested is on)
/// inside tile-alias sub-layouts. Deliberately NOT prop->prop recursive: full recursion
/// pulls in decoration the DLL does not bake (A/B: +191 over-wall for -41 under-wall).
fn emit_prop_links(
    lay: &Layout,
    world0: Option<&Mat4>,
    ctx: &Context,
    sub_cache: &mut HashMap<String, Option<Layout>>,
    tris: &mut Vec<Tri>,
) {
    for (oi, o) in lay.objects.iter().enumerate() {
        if link_target_kind(o) == LinkKind::Tile {
            continue;
        }
        if !link_is_baked_prop(lay, oi) {
            continue;
        }
        let lf = o.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
        let sub_path = match resolve_link_layout(&ctx.media_dir, lf) {
            Some(p) => p,
            None => continue,
        };
        let key = sub_path.to_string_lossy().to_string();
        let sub = sub_cache
            .entry(key)
            .or_insert_with(|| super::layout::load_layout_file(&sub_path).ok());
        let sub = match sub {
            Some(s) => s,
            None => continue,
        };
        let lw = o.world; // NOT scale-stripped here (mirrors Python)
        let link_world = match world0 {
            None => lw,
            Some(w) => w.matmul(&lw),
        };
        for pi in sub.room_piece_indices() {
            if in_backdrop_group(sub, pi, true) {
                continue;
            }
            let piece = &sub.objects[pi];
            let world = link_world.matmul(&piece.world);
            emit_piece(piece, &world, ctx, tris);
        }
    }
}

fn follow_tile_links(
    lay: &Layout,
    world0: Option<&Mat4>,
    visited: &HashSet<String>,
    depth: usize,
    ctx: &Context,
    assemble_links: bool,
    sub_cache: &mut HashMap<String, Option<Layout>>,
    tris: &mut Vec<Tri>,
) {
    if depth > 6 {
        return;
    }
    for o in lay.objects.iter() {
        if link_target_kind(o) != LinkKind::Tile {
            continue;
        }
        let lf = o.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
        let sp = match resolve_link_layout(&ctx.media_dir, lf) {
            Some(p) => p,
            None => continue,
        };
        let key = sp.to_string_lossy().to_lowercase();
        if visited.contains(&key) {
            continue;
        }
        let sub = match super::layout::load_layout_file(&sp) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let lw = strip_link_scale(&o.world);
        let w0 = match world0 {
            None => lw,
            Some(w) => w.matmul(&lw),
        };
        for pi in sub.room_piece_indices() {
            if in_backdrop_group(&sub, pi, true) || link_under_feature_tagged_group(&sub, pi) {
                continue;
            }
            let piece = &sub.objects[pi];
            let world = w0.matmul(&piece.world);
            emit_piece(piece, &world, ctx, tris);
        }
        // The DLL loads the whole nested level, so a prop-link INSIDE this tile-alias sub
        // (e.g. PASS corridors: 1X1_ALPHAZONE_LM_A -> ECHOINN/TREE_03) is baked too.
        if assemble_links && nested_prop_links() {
            emit_prop_links(&sub, Some(&w0), ctx, sub_cache, tris);
        }
        let mut v2 = visited.clone();
        v2.insert(key);
        follow_tile_links(&sub, Some(&w0), &v2, depth + 1, ctx, assemble_links, sub_cache, tris);
    }
}

/// Collect world-space collision triangles with the per-triangle NOPATH flag.
pub fn gather(layout: &Layout, ctx: &Context, assemble_links: bool) -> Vec<Tri> {
    let mut tris: Vec<Tri> = Vec::new();

    // the tile's own room pieces (backdrop + feature-tagged split applied)
    for pi in layout.room_piece_indices() {
        if in_backdrop_group(layout, pi, false) || link_under_feature_tagged_group(layout, pi) {
            continue;
        }
        let piece = &layout.objects[pi];
        let world = piece.world;
        emit_piece(piece, &world, ctx, &mut tris);
    }

    // Shared across the tile-alias follow (for nested prop-links) and the top-level
    // prop-link pass, mirroring Python gather's single sub_cache closure.
    let mut sub_cache: HashMap<String, Option<Layout>> = HashMap::new();

    // ALWAYS follow tile-geometry ALIAS links (and, default-on, their nested prop-links).
    follow_tile_links(
        layout,
        None,
        &HashSet::new(),
        0,
        ctx,
        assemble_links,
        &mut sub_cache,
        &mut tris,
    );

    if assemble_links {
        emit_prop_links(layout, None, ctx, &mut sub_cache, &mut tris);
    }

    tris
}

// ── enclosure pass ─────────────────────────────────────────────────────────

/// In-place, X-outer Z-inner (cascading): wall a walkable cell if both ends of
/// any axis are blocked (edge or wall; 0xFF counts as open).
pub fn enclosure_pass(grid: &mut [u8], gw: i32, gh: i32) {
    let blk = |grid: &[u8], x: i32, z: i32| -> bool {
        if x < 0 || z < 0 || x >= gw || z >= gh {
            return true;
        }
        grid[(z * gw + x) as usize] == 1
    };
    for x in 0..gw {
        for z in 0..gh {
            if grid[(z * gw + x) as usize] != 0 {
                continue;
            }
            if (blk(grid, x - 1, z) && blk(grid, x + 1, z))
                || (blk(grid, x, z - 1) && blk(grid, x, z + 1))
                || (blk(grid, x - 1, z - 1) && blk(grid, x + 1, z + 1))
                || (blk(grid, x + 1, z - 1) && blk(grid, x - 1, z + 1))
            {
                grid[(z * gw + x) as usize] = 1;
            }
        }
    }
}

// ── production grid builder (bucket-culled == build_grid_fast) ─────────────

type BucketIdx = HashMap<(i64, i64), Vec<u32>>;

fn bucket_of(v: f64) -> i64 {
    (v / BUCKET).floor() as i64
}

fn build_index<'a, I: Iterator<Item = (usize, &'a Tri)>>(iter: I, pad: f64) -> BucketIdx {
    let mut idx: BucketIdx = HashMap::new();
    for (ti, t) in iter {
        let minx = t.a[0].min(t.b[0]).min(t.c[0]) - pad;
        let maxx = t.a[0].max(t.b[0]).max(t.c[0]) + pad;
        let minz = t.a[2].min(t.b[2]).min(t.c[2]) - pad;
        let maxz = t.a[2].max(t.b[2]).max(t.c[2]) + pad;
        for gx in bucket_of(minx)..=bucket_of(maxx) {
            for gz in bucket_of(minz)..=bucket_of(maxz) {
                idx.entry((gx, gz)).or_default().push(ti as u32);
            }
        }
    }
    idx
}

/// BYTE-IDENTICAL to the Python build_grid / build_grid_fast. Returns the grid
/// body (gw*gh bytes; 0 walkable, 1 wall, 0xFF oob; index z*gw + x).
pub fn build_grid_fast(
    layout: &Layout,
    ctx: &Context,
    box_min: super::geom::Vec3,
    origin_x: f64,
    origin_z: f64,
    gw: i32,
    gh: i32,
    assemble_links: bool,
) -> Vec<u8> {
    let i0 = ((box_min.x - origin_x) / CELL).floor() as i64;
    let j0 = ((box_min.z - origin_z) / CELL).floor() as i64;

    let tris = gather(layout, ctx, assemble_links);
    let nopath_tris: Vec<&Tri> = tris.iter().filter(|t| t.nop).collect();
    let all_idx = build_index(tris.iter().enumerate(), RADIUS + 1e-4);
    let nop_idx = build_index(
        nopath_tris.iter().enumerate().map(|(i, t)| (i, *t)),
        CLEARANCE_LEN,
    );

    let gw_us = gw.max(0) as usize;
    let gh_us = gh.max(0) as usize;
    let mut grid = vec![0xFFu8; gw_us * gh_us];

    grid.par_chunks_mut(gw_us).enumerate().for_each(|(jj, row)| {
        let cz = (j0 + jj as i64) as f64 * CELL + HALF + origin_z;
        let gz = bucket_of(cz);
        for (ii, cell) in row.iter_mut().enumerate() {
            let cx = (i0 + ii as i64) as f64 * CELL + HALF + origin_x;
            let gx = bucket_of(cx);
            let s: P3 = [cx, 200.0, cz];
            let e: P3 = [cx, -200.0, cz];
            let cand = match all_idx.get(&(gx, gz)) {
                Some(c) if !c.is_empty() => c,
                _ => {
                    *cell = 0xFF;
                    continue;
                }
            };
            // nearest-by-distance over candidates, strict < (first-min tie-break)
            let mut best_d = f64::INFINITY;
            let mut best_hp: P3 = [0.0; 3];
            let mut best_nop = false;
            let mut any = false;
            for &ti in cand {
                let t = &tris[ti as usize];
                if let Some((hp, d)) = swept_tri(s, e, RADIUS, t.a, t.b, t.c) {
                    if !any || d < best_d {
                        any = true;
                        best_d = d;
                        best_hp = hp;
                        best_nop = t.nop;
                    }
                }
            }
            if !any {
                *cell = 0xFF;
                continue;
            }
            let hp = best_hp;
            if hp[1] > HEIGHT_CLAMP || hp[1] < -HEIGHT_CLAMP || best_nop {
                *cell = 1;
                continue;
            }
            let origin: P3 = [hp[0], hp[1] + CLEARANCE_LIFT, hp[2]];
            let mut blocked = false;
            if let Some(ncand) = nop_idx.get(&(gx, gz)) {
                'probes: for (ox, oz) in [
                    (CLEARANCE_LEN, 0.0),
                    (-CLEARANCE_LEN, 0.0),
                    (0.0, CLEARANCE_LEN),
                    (0.0, -CLEARANCE_LEN),
                ] {
                    let ee: P3 = [origin[0] + ox, origin[1], origin[2] + oz];
                    for &ti in ncand {
                        let t = nopath_tris[ti as usize];
                        if swept_tri(origin, ee, CLEARANCE_RADIUS, t.a, t.b, t.c).is_some() {
                            blocked = true;
                            break 'probes;
                        }
                    }
                }
            }
            *cell = if blocked { 1 } else { 0 };
        }
    });

    enclosure_pass(&mut grid, gw, gh);
    grid
}
