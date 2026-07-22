//! Faithful port of TL2's MPP pathing-grid classifier (mpp/native.py), itself a
//! function-by-function port of EditorGuts.dll (CLevel_BuildWritePathingGrid_MPP
//! @ 0x10200920, multi_raycast sub_101EF170, swept_tri sub_10067670, ...).
//! Output is byte-identical to the Python build_grid / build_grid_fast; the
//! per-cell classification is embarrassingly parallel and rayon-parallelized
//! per row (the enclosure pass stays sequential — it cascades).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use rayon::prelude::*;

use super::geom::{Mat4, Vec3};
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

// ── phase-timing probes (CPU-seconds across threads; report via timing_report) ──
static T_GATHER: AtomicU64 = AtomicU64::new(0);
static T_INDEX: AtomicU64 = AtomicU64::new(0);
static T_CLASSIFY: AtomicU64 = AtomicU64::new(0);
static T_ENCLOSURE: AtomicU64 = AtomicU64::new(0);
pub static T_RECONNECT: AtomicU64 = AtomicU64::new(0);

pub fn add_nanos(counter: &AtomicU64, t0: std::time::Instant) {
    counter.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

pub fn timing_report() -> String {
    let s = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64 / 1e9;
    format!(
        "phase cpu-seconds: gather={:.2} index={:.2} classify={:.2} enclosure={:.2} reconnect={:.2}",
        s(&T_GATHER), s(&T_INDEX), s(&T_CLASSIFY), s(&T_ENCLOSURE), s(&T_RECONNECT)
    )
}

/// R38 NEVERBAKE-floor gate (mirrors Python _NEVERBAKE_FLOOR, default ON,
/// kill-switch MPP_NEVERBAKE_FLOOR=0): a BAKE:false piece whose descriptor is
/// NEVERBAKE is still force-baked by the editor (SetMesh @0x1023116b sets the
/// effective-BAKE byte piece[+0x191]=1) — but its nocollide submeshes never wall,
/// so emitting ONLY the non-nop (real-collision) submeshes reproduces the grid.
fn neverbake_floor_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MPP_NEVERBAKE_FLOOR").map(|v| v == "1").unwrap_or(true)
    })
}

/// R41 Layout Link Controller DATA-field placement (mirrors Python
/// _CONTROLLER_DATA, default ON, kill-switch MPP_CONTROLLER_DATA=0): the editor
/// places controller sub-layout objects by the controller's DATA field (per-object
/// runtime transform relative to the controller), not their authored locals.
fn controller_data_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MPP_CONTROLLER_DATA").map(|v| v == "1").unwrap_or(true)
    })
}

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
    if follow_all_links() {
        // ROBUST (default): rely on the DLL's real per-piece backdrop(CHOICE/theme)+BAKE+CE gate
        // instead of the fragile name-substring list. The DLL never name-matches links; frida
        // proved the distinguisher is the per-piece CHOICE group (TABLE_WITH_RANDOMPROPS is under
        // CHOICE -> backdrop-excluded; RAISED_CHEST's floor is not -> baked). A/B: -5869, 0 regr.
        // Residual: FISHINGHOLE (runtime-interactive well collision, no static discriminator).
        if tgt.contains("FISHINGHOLE") {
            return false;
        }
    } else {
        // LEGACY name-substring heuristics (env MPP_FOLLOW_ALL_LINKS=0), kept for A/B.
        if RUNTIME_LINK_SUBSTR.iter().any(|s| tgt.contains(s)) && !tgt.contains("RAISED_CHEST") {
            return false;
        }
        if tgt.contains("RANDOM") && !tgt.contains("RANDOMIZED") && !tgt.contains("WRANDOMDECO") {
            return false;
        }
        let baked_nest = tgt == "SPAWNER_01.LAYOUT" || tgt.starts_with("JACKALBEAST_SPAWNER");
        if tgt.contains("SPAWNER")
            && !tgt.contains("FLYER")
            && !tgt.contains("NOSPAWNER")
            && !is_timeline
            && !baked_nest
        {
            return false;
        }
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

/// The clearance leaf's side-test gate (sub_100672B0 via sub_10293B50, R19):
/// with s(p) = side of p vs the tri plane {0 back / 1 front / 2 on} (sign of
/// dot(vertA - p, n), n = raw cross(b-a,c-a)), a probe blocks iff
///   s(origin) != s(e)  AND  s(origin) != 1 (front)
/// i.e. the segment straddles the wall plane with its START on the non-front
/// side. The vertical down-ray leaf has NO such gate — clearance only.
#[inline]
fn clearance_hit(origin: P3, e: P3, radius: f64, a: P3, b: P3, c: P3) -> bool {
    if swept_tri(origin, e, radius, a, b, c).is_none() {
        return false;
    }
    let n = cross(sub(b, a), sub(c, a));
    let d0 = n[0] * (a[0] - origin[0]) + n[1] * (a[1] - origin[1]) + n[2] * (a[2] - origin[2]);
    if d0 > SIDE_EPS {
        return false; // s(origin) == 1 (front) -> gate rejects
    }
    let s0 = if d0 < -SIDE_EPS { 0u8 } else { 2u8 };
    let de = n[0] * (a[0] - e[0]) + n[1] * (a[1] - e[1]) + n[2] * (a[2] - e[2]);
    let se = if de < -SIDE_EPS {
        0u8
    } else if de > SIDE_EPS {
        1u8
    } else {
        2u8
    };
    s0 != se // straddle: s(origin) != s(e)
}

// ── Layout Link Controller DATA-field placement (R41) ─────────────────────

/// Row-major rotation matrix for `ang` radians about axis (ax,ay,az).
fn axis_angle_matrix(ang: f64, ax: f64, ay: f64, az: f64) -> Mat4 {
    let n = (ax * ax + ay * ay + az * az).sqrt();
    if n < 1e-9 || ang.abs() < 1e-12 {
        return Mat4::identity();
    }
    let (ax, ay, az) = (ax / n, ay / n, az / n);
    let (c, s) = (ang.cos(), ang.sin());
    let t = 1.0 - c;
    let mut m = Mat4::identity();
    m.m[0][0] = c + ax * ax * t;
    m.m[0][1] = ax * ay * t - az * s;
    m.m[0][2] = ax * az * t + ay * s;
    m.m[1][0] = ay * ax * t + az * s;
    m.m[1][1] = c + ay * ay * t;
    m.m[1][2] = ay * az * t - ax * s;
    m.m[2][0] = az * ax * t - ay * s;
    m.m[2][1] = az * ay * t + ax * s;
    m.m[2][2] = c + az * az * t;
    m
}

/// Parse a Layout Link Controller's DATA field
/// ("<ver>,<count>,{objectID,posX,posY,posZ,angle,axisX,axisY,axisZ}*count")
/// -> {object_id: translation @ rotation} relative to the controller's world.
fn parse_controller_data(ctrl: &LayoutObject) -> HashMap<String, Mat4> {
    let mut out = HashMap::new();
    let raw = match ctrl.props.get("DATA") {
        Some(r) if !r.is_empty() => r,
        _ => return out,
    };
    let parts: Vec<&str> = raw.split(',').filter(|p| !p.trim().is_empty()).collect();
    if parts.len() < 2 {
        return out;
    }
    let count: usize = match parts[1].trim().parse() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let mut i = 2usize;
    for _ in 0..count {
        if i + 8 > parts.len() {
            break;
        }
        let oid = parts[i].to_string();
        let mut vals = [0.0f64; 7];
        let mut ok = true;
        for (k, v) in vals.iter_mut().enumerate() {
            match parts[i + 1 + k].trim().parse::<f64>() {
                Ok(f) => *v = f,
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            break;
        }
        let [x, y, z, ang, axx, axy, axz] = vals;
        out.insert(
            oid,
            Mat4::translation(Vec3::new(x, y, z)).matmul(&axis_angle_matrix(ang, axx, axy, axz)),
        );
        i += 8;
    }
    out
}

/// Recompute a sub-layout piece's transform relative to the controller: walk its
/// PARENTID chain and substitute a DATA-field local for any ancestor listed in
/// `data_map`, else the authored local. == piece.world when no DATA ancestor.
fn data_override_world(piece_idx: usize, sub: &Layout, data_map: &HashMap<String, Mat4>) -> Mat4 {
    let mut chain: Vec<usize> = vec![piece_idx];
    let mut seen: HashSet<String> = HashSet::new();
    let mut pid = sub.objects[piece_idx].props.get("PARENTID").cloned();
    while let Some(p) = pid {
        if p.is_empty() || seen.contains(&p) {
            break;
        }
        let anc = match sub.by_id.get(&p) {
            Some(&i) => i,
            None => break,
        };
        seen.insert(p);
        chain.push(anc);
        pid = sub.objects[anc].props.get("PARENTID").cloned();
    }
    let mut m = Mat4::identity();
    for &oi in chain.iter().rev() {
        let obj = &sub.objects[oi];
        let local = obj
            .props
            .get("ID")
            .and_then(|id| data_map.get(id))
            .copied()
            .unwrap_or_else(|| obj.local_matrix());
        m = m.matmul(&local);
    }
    m
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
    // descriptor is ALWAYSBAKECOLLISION — or (R38) NEVERBAKE, which SetMesh
    // force-bakes; for those emit only the non-nop submeshes (see below).
    let mut neverbake_floor = false;
    if let Some(bake) = piece.props.get("BAKE") {
        if bake.trim().to_lowercase() == "false" && !pd.alwaysbakecollision {
            if neverbake_floor_enabled() && pd.neverbake {
                neverbake_floor = true;
            } else {
                return;
            }
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
        if neverbake_floor && nop {
            continue; // NEVERBAKE-rescued piece: skip nocollide submeshes (never wall)
        }
        let idx = &sm.indices;
        let vc = geo.vertex_count;
        let pos = &geo.positions;
        // (Measured: pre-transforming the whole vertex buffer per submesh/piece is a
        // NET LOSS here — allocation + scattered re-reads beat the ~6x redundant
        // per-corner multiplies. Keep the per-corner transform.)
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

/// Follow all links and gate per-piece by the DLL's real backdrop/BAKE/CE criterion instead of
/// the name-substring list. DEFAULT ON (env MPP_FOLLOW_ALL_LINKS=0 restores the legacy names).
fn follow_all_links() -> bool {
    std::env::var("MPP_FOLLOW_ALL_LINKS").map(|v| v == "1").unwrap_or(true)
}

/// Follow `lay`'s non-tile baked-prop Layout Links (ONE level) and emit their sub-pieces,
/// transformed by the accumulated `world0`. Runs on the top layout AND (when nested is on)
/// inside tile-alias sub-layouts. Deliberately NOT prop->prop recursive: full recursion
/// pulls in decoration the DLL does not bake (A/B: +191 over-wall for -41 under-wall).
fn emit_prop_links(lay: &Layout, world0: Option<&Mat4>, ctx: &Context, tris: &mut Vec<Tri>) {
    for (oi, o) in lay.objects.iter().enumerate() {
        if link_target_kind(o) == LinkKind::Tile {
            continue;
        }
        if !link_is_baked_prop(lay, oi) {
            continue;
        }
        let lf = o.props.get("LAYOUT FILE").map(String::as_str).unwrap_or("");
        let sub_path = match resolve_link_layout(&ctx.media_dir, lf, ctx.fallback_dir.as_deref()) {
            Some(p) => p,
            None => continue,
        };
        let sub_arc = match ctx.get_layout(&sub_path) {
            Some(s) => s,
            None => continue,
        };
        let sub: &Layout = sub_arc.as_ref();
        let lw = o.world; // NOT scale-stripped here (mirrors Python)
        let link_world = match world0 {
            None => lw,
            Some(w) => w.matmul(&lw),
        };
        // Layout Link Controller: the editor places sub-layout objects by the
        // controller's DATA field (per-object runtime transform relative to the
        // controller), NOT their authored sub-layout locals (R41). Empty map
        // (no DATA / non-controller) -> authored piece.world, no change.
        let data_map = if controller_data_enabled() && o.descriptor == "Layout Link Controller" {
            parse_controller_data(o)
        } else {
            HashMap::new()
        };
        for pi in sub.room_piece_indices() {
            if in_backdrop_group(sub, pi, true) {
                continue;
            }
            let piece = &sub.objects[pi];
            let pw = if !data_map.is_empty() {
                data_override_world(pi, sub, &data_map)
            } else {
                piece.world
            };
            let world = link_world.matmul(&pw);
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
        let sp = match resolve_link_layout(&ctx.media_dir, lf, ctx.fallback_dir.as_deref()) {
            Some(p) => p,
            None => continue,
        };
        let key = sp.to_string_lossy().to_lowercase();
        if visited.contains(&key) {
            continue;
        }
        let sub_arc = match ctx.get_layout(&sp) {
            Some(s) => s,
            None => continue,
        };
        let sub: &Layout = sub_arc.as_ref();
        let lw = strip_link_scale(&o.world);
        let w0 = match world0 {
            None => lw,
            Some(w) => w.matmul(&lw),
        };
        for pi in sub.room_piece_indices() {
            if in_backdrop_group(sub, pi, true) || link_under_feature_tagged_group(sub, pi) {
                continue;
            }
            let piece = &sub.objects[pi];
            let world = w0.matmul(&piece.world);
            emit_piece(piece, &world, ctx, tris);
        }
        // The DLL loads the whole nested level, so a prop-link INSIDE this tile-alias sub
        // (e.g. PASS corridors: 1X1_ALPHAZONE_LM_A -> ECHOINN/TREE_03) is baked too.
        if assemble_links && nested_prop_links() {
            emit_prop_links(sub, Some(&w0), ctx, tris);
        }
        let mut v2 = visited.clone();
        v2.insert(key);
        follow_tile_links(sub, Some(&w0), &v2, depth + 1, ctx, assemble_links, tris);
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

    // ALWAYS follow tile-geometry ALIAS links (and, default-on, their nested prop-links).
    // Sub-layout parses are shared cross-tile via ctx.get_layout.
    follow_tile_links(layout, None, &HashSet::new(), 0, ctx, assemble_links, &mut tris);

    if assemble_links {
        emit_prop_links(layout, None, ctx, &mut tris);
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

// ── SAFETY post-process (R36/R37): reconnect trapped walkable regions ──────

/// NOT part of native's algorithm — a gameplay-safety guard for the SHIPPED grid
/// (mirrors Python native.reconnect_walkable, byte-exact same unblock choices).
/// Unblocks the MINIMAL wall-barrier (<= max_barrier cells) separating any large
/// (>= min_region) disconnected walkable component from the main region. Only
/// wall(1) cells are unblocked; void(0xFF) is impassable. Returns cells unblocked.
pub fn reconnect_walkable(g: &mut [u8], gw: i32, gh: i32, min_region: usize, max_barrier: u32) -> usize {
    let (gw, gh) = (gw.max(0) as usize, gh.max(0) as usize);
    let n = gw * gh;
    if n == 0 || g.len() < n {
        return 0;
    }
    // 4-neighbour walkable components, in the Python discovery order
    // (scan order starts, BFS popleft, neighbour order (-z,+z,-x,+x)).
    let components = |g: &[u8]| -> Vec<Vec<usize>> {
        let mut lab = vec![0u32; n];
        let mut comps: Vec<Vec<usize>> = Vec::new();
        let mut q: VecDeque<usize> = VecDeque::new();
        for st in 0..n {
            if g[st] != 0 || lab[st] != 0 {
                continue;
            }
            let cur = comps.len() as u32 + 1;
            lab[st] = cur;
            q.push_back(st);
            let mut cells = vec![st];
            while let Some(p) = q.pop_front() {
                let (z, x) = (p / gw, p % gw);
                for (dz, dx) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                    let (zz, xx) = (z as i64 + dz, x as i64 + dx);
                    if zz < 0 || xx < 0 || zz >= gh as i64 || xx >= gw as i64 {
                        continue;
                    }
                    let ni = zz as usize * gw + xx as usize;
                    if g[ni] == 0 && lab[ni] == 0 {
                        lab[ni] = cur;
                        q.push_back(ni);
                        cells.push(ni);
                    }
                }
            }
            comps.push(cells);
        }
        comps
    };

    const INF: u32 = 1 << 30;
    let mut total = 0usize;
    for _ in 0..30 {
        let comps = components(g);
        if comps.is_empty() {
            break;
        }
        // main = largest component (first max, mirroring Python max())
        let mut main = 0usize;
        for (ci, c) in comps.iter().enumerate() {
            if c.len() > comps[main].len() {
                main = ci;
            }
        }
        let targets: Vec<usize> = (0..comps.len())
            .filter(|&c| c != main && comps[c].len() >= min_region)
            .collect();
        if targets.is_empty() {
            break;
        }
        // 0-1 BFS from every main cell: walkable free, wall costs 1, void impassable.
        let mut dist = vec![INF; n];
        let mut par = vec![usize::MAX; n];
        let mut dq: VecDeque<usize> = VecDeque::new();
        for &p in &comps[main] {
            dist[p] = 0;
            dq.push_back(p);
        }
        while let Some(p) = dq.pop_front() {
            let (z, x) = (p / gw, p % gw);
            let dp = dist[p];
            for (dz, dx) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                let (zz, xx) = (z as i64 + dz, x as i64 + dx);
                if zz < 0 || xx < 0 || zz >= gh as i64 || xx >= gw as i64 {
                    continue;
                }
                let ni = zz as usize * gw + xx as usize;
                if g[ni] == 0xFF {
                    continue;
                }
                let w: u32 = if g[ni] == 0 { 0 } else { 1 };
                if dp + w < dist[ni] {
                    dist[ni] = dp + w;
                    par[ni] = p;
                    if w == 0 {
                        dq.push_front(ni);
                    } else {
                        dq.push_back(ni);
                    }
                }
            }
        }
        let mut reconnected = false;
        for &c in &targets {
            // first-min in cell order (mirrors Python min())
            let mut best = comps[c][0];
            for &p in &comps[c] {
                if dist[p] < dist[best] {
                    best = p;
                }
            }
            if dist[best] <= max_barrier {
                let mut p = best;
                while p != usize::MAX && dist[p] > 0 {
                    if g[p] == 1 {
                        g[p] = 0;
                        total += 1;
                    }
                    p = par[p];
                }
                reconnected = true;
            }
        }
        if !reconnected {
            break;
        }
    }
    total
}

// ── production grid builder (bucket-culled == build_grid_fast) ─────────────

fn bucket_of(v: f64) -> i64 {
    (v / BUCKET).floor() as i64
}

/// Dense CSR bucket grid over the tris' bucket-coordinate range: zero hashing on
/// the per-cell lookup (the HashMap cost was ~2 SipHash lookups x 99M cells).
/// Per-bucket candidate ORDER is ascending tri index, identical to the HashMap
/// build's push order — the classify first-min tie-break depends on it.
struct BucketIdx {
    gx0: i64,
    gz0: i64,
    nx: i64,
    nz: i64,
    offsets: Vec<u32>,
    items: Vec<u32>,
}

impl BucketIdx {
    #[inline]
    fn get(&self, gx: i64, gz: i64) -> &[u32] {
        if gx < self.gx0 || gz < self.gz0 || gx >= self.gx0 + self.nx || gz >= self.gz0 + self.nz {
            return &[];
        }
        let cell = ((gz - self.gz0) * self.nx + (gx - self.gx0)) as usize;
        &self.items[self.offsets[cell] as usize..self.offsets[cell + 1] as usize]
    }
}

fn build_index<'a, I: Iterator<Item = (usize, &'a Tri)> + Clone>(iter: I, pad: f64) -> BucketIdx {
    // bucket span per tri (same formulas as the HashMap build)
    let spans: Vec<(i64, i64, i64, i64)> = iter
        .clone()
        .map(|(_ti, t)| {
            let minx = t.a[0].min(t.b[0]).min(t.c[0]) - pad;
            let maxx = t.a[0].max(t.b[0]).max(t.c[0]) + pad;
            let minz = t.a[2].min(t.b[2]).min(t.c[2]) - pad;
            let maxz = t.a[2].max(t.b[2]).max(t.c[2]) + pad;
            (bucket_of(minx), bucket_of(maxx), bucket_of(minz), bucket_of(maxz))
        })
        .collect();
    if spans.is_empty() {
        return BucketIdx { gx0: 0, gz0: 0, nx: 0, nz: 0, offsets: vec![0], items: Vec::new() };
    }
    let gx0 = spans.iter().map(|s| s.0).min().unwrap();
    let gx1 = spans.iter().map(|s| s.1).max().unwrap();
    let gz0 = spans.iter().map(|s| s.2).min().unwrap();
    let gz1 = spans.iter().map(|s| s.3).max().unwrap();
    let nx = gx1 - gx0 + 1;
    let nz = gz1 - gz0 + 1;
    let ncell = (nx * nz) as usize;
    // pass 1: counts; pass 2: fill in ascending tri order (per-bucket order preserved)
    let mut counts = vec![0u32; ncell];
    for s in &spans {
        for gz in s.2..=s.3 {
            for gx in s.0..=s.1 {
                counts[((gz - gz0) * nx + (gx - gx0)) as usize] += 1;
            }
        }
    }
    let mut offsets = vec![0u32; ncell + 1];
    for i in 0..ncell {
        offsets[i + 1] = offsets[i] + counts[i];
    }
    let mut cursor = offsets[..ncell].to_vec();
    let mut items = vec![0u32; offsets[ncell] as usize];
    for ((ti, _t), s) in iter.zip(&spans) {
        for gz in s.2..=s.3 {
            for gx in s.0..=s.1 {
                let cell = ((gz - gz0) * nx + (gx - gx0)) as usize;
                items[cursor[cell] as usize] = ti as u32;
                cursor[cell] += 1;
            }
        }
    }
    BucketIdx { gx0, gz0, nx, nz, offsets, items }
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

    let t0 = std::time::Instant::now();
    let tris = gather(layout, ctx, assemble_links);
    add_nanos(&T_GATHER, t0);
    let t0 = std::time::Instant::now();
    let nopath_tris: Vec<&Tri> = tris.iter().filter(|t| t.nop).collect();
    let all_idx = build_index(tris.iter().enumerate(), RADIUS + 1e-4);
    let nop_idx = build_index(
        nopath_tris.iter().enumerate().map(|(i, t)| (i, *t)),
        CLEARANCE_LEN,
    );
    // Per-tri AABB pre-filters (pure rejection of tris that CANNOT hit — the
    // accepted paths run the identical float ops, so the grid is bit-exact).
    // Down-ray: a swept hit implies XZ distance from the vertical line <= RADIUS
    // (in-tri: (cx,cz) inside the tri's XZ projection; capsule-edge: closest
    // point within RADIUS of it). Pad RADIUS also swallows point_in_tri's 1e-6
    // barycentric slop (~2e-4 world units on the largest collision tris).
    let aabb_xz: Vec<[f64; 4]> = tris
        .iter()
        .map(|t| {
            [
                t.a[0].min(t.b[0]).min(t.c[0]) - RADIUS,
                t.a[0].max(t.b[0]).max(t.c[0]) + RADIUS,
                t.a[2].min(t.b[2]).min(t.c[2]) - RADIUS,
                t.a[2].max(t.b[2]).max(t.c[2]) + RADIUS,
            ]
        })
        .collect();
    // Clearance probes (radius 0): a hit lies ON the 0.3 segment AND on the tri
    // (within the same barycentric slop) -> 3D AABB overlap, padded 1e-3 (10x
    // the worst-case slop) so the filter can never drop a would-be hit.
    const CPAD: f64 = 1e-3;
    let nop_aabb: Vec<[f64; 6]> = nopath_tris
        .iter()
        .map(|t| {
            [
                t.a[0].min(t.b[0]).min(t.c[0]) - CPAD,
                t.a[0].max(t.b[0]).max(t.c[0]) + CPAD,
                t.a[1].min(t.b[1]).min(t.c[1]) - CPAD,
                t.a[1].max(t.b[1]).max(t.c[1]) + CPAD,
                t.a[2].min(t.b[2]).min(t.c[2]) - CPAD,
                t.a[2].max(t.b[2]).max(t.c[2]) + CPAD,
            ]
        })
        .collect();
    add_nanos(&T_INDEX, t0);

    let gw_us = gw.max(0) as usize;
    let gh_us = gh.max(0) as usize;
    let mut grid = vec![0xFFu8; gw_us * gh_us];

    let t0 = std::time::Instant::now();
    grid.par_chunks_mut(gw_us).enumerate().for_each(|(jj, row)| {
        let cz = (j0 + jj as i64) as f64 * CELL + HALF + origin_z;
        let gz = bucket_of(cz);
        for (ii, cell) in row.iter_mut().enumerate() {
            let cx = (i0 + ii as i64) as f64 * CELL + HALF + origin_x;
            let gx = bucket_of(cx);
            let s: P3 = [cx, 200.0, cz];
            let e: P3 = [cx, -200.0, cz];
            let cand = all_idx.get(gx, gz);
            if cand.is_empty() {
                *cell = 0xFF;
                continue;
            }
            // nearest-by-distance over candidates, strict < (first-min tie-break)
            let mut best_d = f64::INFINITY;
            let mut best_hp: P3 = [0.0; 3];
            let mut best_nop = false;
            let mut any = false;
            for &ti in cand {
                let bb = &aabb_xz[ti as usize];
                if cx < bb[0] || cx > bb[1] || cz < bb[2] || cz > bb[3] {
                    continue; // vertical capsule cannot reach this tri
                }
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
            let ncand = nop_idx.get(gx, gz);
            if !ncand.is_empty() {
                'probes: for (ox, oz) in [
                    (CLEARANCE_LEN, 0.0),
                    (-CLEARANCE_LEN, 0.0),
                    (0.0, CLEARANCE_LEN),
                    (0.0, -CLEARANCE_LEN),
                ] {
                    let ee: P3 = [origin[0] + ox, origin[1], origin[2] + oz];
                    let (sx0, sx1) = if ee[0] < origin[0] { (ee[0], origin[0]) } else { (origin[0], ee[0]) };
                    let (sz0, sz1) = if ee[2] < origin[2] { (ee[2], origin[2]) } else { (origin[2], ee[2]) };
                    let sy = origin[1];
                    for &ti in ncand {
                        let bb = &nop_aabb[ti as usize];
                        if sx1 < bb[0] || sx0 > bb[1] || sy < bb[2] || sy > bb[3] || sz1 < bb[4] || sz0 > bb[5] {
                            continue; // probe segment cannot touch this tri
                        }
                        let t = nopath_tris[ti as usize];
                        // R19: the clearance leaf's side-test gate (clearance_hit),
                        // NOT a bare swept_tri — else nocollide walls the probe exits
                        // front-first over-block (canyon_footer_* etc.).
                        if clearance_hit(origin, ee, CLEARANCE_RADIUS, t.a, t.b, t.c) {
                            blocked = true;
                            break 'probes;
                        }
                    }
                }
            }
            *cell = if blocked { 1 } else { 0 };
        }
    });

    add_nanos(&T_CLASSIFY, t0);

    let t0 = std::time::Instant::now();
    enclosure_pass(&mut grid, gw, gh);
    add_nanos(&T_ENCLOSURE, t0);
    grid
}
