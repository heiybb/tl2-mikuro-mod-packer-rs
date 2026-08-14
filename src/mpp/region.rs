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
///
/// This IS a region-box gate, and unlike `BAKE:false` (see `registers_for_pathing`)
/// the disassembly agrees: gate2 @0x10203b62 branches to a "skip this piece" block
/// that reaches neither the StaticGeometry registration nor the box merge. Measured
/// both ways too — un-gating it takes header agreement from 1275/1293 down to
/// 610/1293. Do not "fix" a too-small box by relaxing this.
/// A/B switch: `MPP_BOX_CE_GATE=0`.
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
    // Index by VISUAL only when the collision and visual lists are the same length,
    // else fall back to 0. REFUTED alternatives (1293 shipped headers, baseline 1279):
    // always-0 scores 1253 / 99.2669% cells; always-VISUAL is byte-identical to this
    // (the clamp below already collapses the two). So a mismatched collision variant is
    // NOT why the remaining boxes come out a quantum too large, even though a piece
    // named `floor_gear_02` resolving to `..._gear_01_collision.mesh` looks like it.
    let mut col_idx = if ncol == nvis { v as usize } else { 0 };
    if col_idx >= ncol {
        col_idx = 0;
    }
    &colls[col_idx]
}

/// Whether a piece's collision box extends the region box (backdrop/feature split).
///
/// # The editor's per-piece gate list is COMPLETE (EditorGuts.dll, IDA 2026-08-14)
///
/// Read off the whole loop body of `CLevel_RebuildLevel_GenPathing` @0x10203710, not
/// the summary in `MPP生成完整分支图.md` (which omits the second gate):
///
/// ```text
///   0x10203b31  dynamic_cast<CRoomPiece>                     fail -> next piece
///   0x10203b5a  piece[+0x108] vtable[+4] returns null        -> next piece   (undocumented)
///   0x10203b62  sub_10230CA0 == COLLISION ENABLED, false     -> next piece
///   0x10203ca2  [collRes+0x58] == 0 (no bounds)              -> log "Collision mesh
///                                                               ... has no faces!", next
///   0x10203c03  matrix = translate * rotate(Quaternion) * setScale   <- piece scale APPLIES
///   0x10203e0e  build the 4-point piece box (see transform_bounds_f32)
///   0x10203fbf  sub_1022FF80 detail? -> var_BA8 (never snapped, never reaches header)
///   0x10203fdd  BAKE:false -> skips ONLY sub_10068CB0; still merges into var_C64
/// ```
///
/// **There is no BAKE gate and no other gate.** So `BAKE:false` pieces legitimately
/// size the region box, and R45 (gating them out) is mechanically wrong — even though
/// it scores +9 on the shipped corpus. Kept OFF behind
/// `MPP_BOX_BAKE_GATE=1|direct|link`.
///
/// # Where the remaining 14 header mismatches stand
///
/// All 14 are BENIGN: reconstructing the stamp offset from the worldExt delta shows our
/// grid contains GUTS's verbatim with a **100% void** margin, and void never overwrites
/// at stamp time. Shared-window cell agreement on them is 0.016%-0.41%, i.e. our
/// triangles land correctly — the box is bigger but the CONTENT is identical, because
/// the piece that inflates it is `BAKE:false` and emits no triangles at all.
///
/// For the 9 that R45 still masks, the failing axis is set by a `BAKE:false` decorative
/// prop in 9 cases out of 9 — `herkon_embercraft`, `light_standing_01`,
/// `nether_tree_01`, `flytrap_01` (scaled 4-5x), `floor_gear_02`. Since the gate list
/// above is complete, the divergence has to be in the box VALUE for those pieces.
/// Value-level causes tried and REFUTED (baseline 1279/1293):
///   * strip the piece's own scale               1241
///   * strip it only for `SCALABLE:false` defs   no-op (never at an extremum)
///   * collision variant always 0 / always VISUAL  1253 / identical
///   * bounds from vertices instead of M_MESH_BOUNDS  identical (no stale chunk anywhere)
///   * 4-corner variants other than the editor's  1272-1274 (see transform_bounds_f32)
/// and the rotation is ruled out independently: if the piece transform were wrong the
/// triangles would be misplaced, and they are not.
///
/// Leading remaining hypothesis, untested: the shipped .MPP corpus may have been baked
/// by an EARLIER GUTS build than this DLL. That would explain a discrepancy that is
/// stable, benign, and has no gate in the current code. Testing it means regenerating a
/// vanilla tile with the editor itself and comparing to the shipped file.
fn registers_for_pathing(
    layout: &Layout,
    piece_idx: usize,
    weighted_backdrop: bool,
    ctx: &Context,
) -> bool {
    if super::native::in_backdrop_group(layout, piece_idx, weighted_backdrop) {
        return false;
    }
    if super::native::link_under_feature_tagged_group(layout, piece_idx) {
        return false;
    }
    // `weighted_backdrop` is true only on the link-merged walk, so it doubles as
    // "was this piece reached through a Layout Link".
    let via_link = weighted_backdrop;
    let hidden = match super::native::hidden_gate_mode() {
        "any" => super::native::under_hidden_group(layout, piece_idx, 16),
        "parent" => super::native::under_hidden_group(layout, piece_idx, 1),
        "link" => via_link && super::native::under_hidden_group(layout, piece_idx, 16),
        "direct" => !via_link && super::native::under_hidden_group(layout, piece_idx, 16),
        _ => false,
    };
    if hidden {
        return false;
    }
    let obj = &layout.objects[piece_idx];
    if box_bake_gate_applies(via_link) && !contributes_geometry(obj, ctx.guid.get(&obj.guid)) {
        return false;
    }
    true
}

/// `MPP_BOX_BAKE_GATE`: unset/`0` = off (faithful, the default), `direct` = direct
/// room pieces only, `link` = link-merged only, `1` = both. Investigation switch for
/// the anomaly described on `registers_for_pathing`; the provenance split is kept
/// because the +4/+8 breakdown is what rules out the known link-instantiation
/// residual as the sole explanation.
fn ce_box_gate_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("MPP_BOX_CE_GATE").map(|v| v != "0").unwrap_or(true))
}

fn box_corner_mode() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| std::env::var("MPP_BOX_CORNERS").unwrap_or_else(|_| "guts4".into()))
        .as_str()
}

fn box_bake_gate_applies(via_link: bool) -> bool {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let v = V.get_or_init(|| std::env::var("MPP_BOX_BAKE_GATE").unwrap_or_else(|_| "0".into()));
    match v.as_str() {
        "direct" => !via_link,
        "link" => via_link,
        "1" => true,
        _ => false,
    }
}

/// The `BAKE:false` half of `native::emit_piece`'s add gate, in isolation.
/// `pd` is the piece's LEVELSETS definition, or None when the GUID is unknown —
/// in which case nothing is known about the rescues and the piece is kept, so an
/// unresolved GUID can never shrink a box.
pub(crate) fn contributes_geometry(piece: &LayoutObject, pd: Option<&super::dat::PieceDef>) -> bool {
    let Some(bake) = piece.props.get("BAKE") else {
        return true;
    };
    if bake.trim().to_lowercase() != "false" {
        return true;
    }
    match pd {
        None => true,
        Some(pd) => pd.alwaysbakecollision || (super::native::neverbake_floor_enabled() && pd.neverbake),
    }
}

/// A piece's ancestor chain rendered for diagnosis, innermost first. The editor's
/// base/detail split (`sub_1022FF80`) keys off an ancestor CRandomGroup, so when a box
/// extremum disagrees with GUTS this is the thing to read.
fn ancestor_chain(layout: &Layout, piece: &LayoutObject) -> Vec<String> {
    let mut out = Vec::new();
    let mut pid = piece.props.get("PARENTID").cloned();
    let mut seen = 0usize;
    while let Some(pv) = pid {
        if pv.is_empty() || pv == "-1" || seen >= 16 {
            break;
        }
        let Some(&i) = layout.by_id.get(&pv) else { break };
        let o = &layout.objects[i];
        let mut kv: Vec<String> = Vec::new();
        for (k, v) in &o.props {
            if matches!(k.as_str(), "PARENTID" | "ID" | "DESCRIPTOR" | "NAME") {
                continue;
            }
            kv.push(format!("{k}={}", v.trim()));
        }
        kv.sort();
        out.push(format!("{} \"{}\" {{{}}}", o.descriptor, o.name, kv.join(", ")));
        pid = o.props.get("PARENTID").cloned();
        seen += 1;
    }
    out
}

/// Piece world-box in float32; returns (minx, minz, maxx, maxz).
///
/// R46 — the editor does NOT do an 8-corner AABB refit. Read off EditorGuts.dll
/// @0x10203e0e-0x10203f42, it transforms exactly FOUR points by the piece matrix and
/// takes their AABB (`setExtents(p,p)` then three `merge`):
///
/// ```text
///   var_B88 = bounds.min                  ; *(Vector3*)(collRes[+0x58] + 0x08)
///   var_C00 = bounds.max                  ; *(Vector3*)(collRes[+0x58] + 0x14)
///   var_B24 = max, then .x := min.x       ; single float32 copy @0x10203e79
///   var_B60 = min, then .x := max.x       ; single float32 copy @0x10203e9c
/// ```
///
/// So Y and Z are LOCKED TOGETHER and only X varies independently. Projected to XZ
/// that is still all four corners, so for a yaw-only transform it equals an 8-corner
/// refit exactly — which is why 1275 of 1293 tiles never noticed. It is strictly
/// SMALLER once the transform mixes Y into X or Z, i.e. for a tilted piece, and an
/// 8-corner refit over-inflates those.
///
/// A/B over the 1293 shipped headers: 8-corner 1275, **these four 1276**, and the
/// three plausible-looking guesses all worse (four at y=min 1272, at y=max 1274,
/// min/max diagonal 1272). The one tile it fixes is
/// `ACT2_Z2/1X1SINGLE_ROOM_BOSS/CRABTRAP_PB`, whose box extremum was a tilted
/// `lake_shipwreck_01_damaged_03` — mechanism and fix agree, zero regressions,
/// loses-a-side stays 0/1276. `MPP_BOX_CORNERS=c8|lo4|hi4|diag4` reproduces the A/B.
pub fn transform_bounds_f32(bx: &Aabb, m: &Mat4) -> (f32, f32, f32, f32) {
    let mf: Vec<[f32; 4]> = m
        .m
        .iter()
        .map(|r| [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32])
        .collect();
    let (mn, mx) = (bx.min, bx.max);
    // RE @0x10203e60-0x10203fae: the editor builds the piece box from FOUR
    // transformed points (`Matrix4::operator*(Vector3)` x4, then setExtents + 3
    // merges), not an 8-corner refit. Identical in XZ for a yaw-only transform;
    // an 8-corner refit over-inflates anything tilted. `MPP_BOX_CORNERS` selects
    // the set while the exact four are being pinned down.
    let c8 = [
        (mn.x, mn.y, mn.z),
        (mx.x, mn.y, mn.z),
        (mn.x, mx.y, mn.z),
        (mx.x, mx.y, mn.z),
        (mn.x, mn.y, mx.z),
        (mx.x, mn.y, mx.z),
        (mn.x, mx.y, mx.z),
        (mx.x, mx.y, mx.z),
    ];
    let lo4 = [
        (mn.x, mn.y, mn.z),
        (mx.x, mn.y, mn.z),
        (mn.x, mn.y, mx.z),
        (mx.x, mn.y, mx.z),
    ];
    let hi4 = [
        (mn.x, mx.y, mn.z),
        (mx.x, mx.y, mn.z),
        (mn.x, mx.y, mx.z),
        (mx.x, mx.y, mx.z),
    ];
    let diag4 = [
        (mn.x, mn.y, mn.z),
        (mx.x, mx.y, mn.z),
        (mn.x, mn.y, mx.z),
        (mx.x, mx.y, mx.z),
    ];
    // The editor's actual four, read off @0x10203e0e-0x10203f42:
    //   var_B88 = bounds.min                    (res[+0x58]+0x08)
    //   var_C00 = bounds.max                    (res[+0x58]+0x14)
    //   var_B24 = max with .x := min.x          (single float32 copy @0x10203e79)
    //   var_B60 = min with .x := max.x          (single float32 copy @0x10203e9c)
    // Y and Z are LOCKED TOGETHER — only X varies independently. Projected to XZ
    // this is still all four corners, so it is identical to an 8-corner refit for a
    // yaw-only transform; it is strictly smaller whenever the transform mixes Y
    // into X or Z, i.e. for tilted pieces.
    let guts4 = [
        (mn.x, mn.y, mn.z),
        (mx.x, mx.y, mx.z),
        (mn.x, mx.y, mx.z),
        (mx.x, mn.y, mn.z),
    ];
    let corners: &[(f64, f64, f64)] = match box_corner_mode() {
        "c8" => &c8,
        "lo4" => &lo4,
        "hi4" => &hi4,
        "diag4" => &diag4,
        _ => &guts4,
    };
    let mut minx = f32::INFINITY;
    let mut minz = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxz = f32::NEG_INFINITY;
    for &(cx, cy, cz) in corners {
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

/// One piece's contribution to the hull, for diagnosis only.
#[derive(Clone, Debug)]
pub struct HullContrib {
    /// Empty for a direct room piece; otherwise the chain of Layout Link targets
    /// the piece was reached through.
    pub via: String,
    pub piece: String,
    pub mesh: String,
    pub flags: String,
    /// The piece's own scale (LayoutObject `X`/`Y`/`Z`). The editor is known to strip
    /// a Layout Link's scale from its collider (`strip_link_scale`); whether it also
    /// ignores a Room Piece's own scale for the region box is a live question.
    pub scale: (f64, f64, f64),
    /// The piece's ancestor chain, outermost last: `descriptor "name" {props}`.
    /// The editor's base/detail split (`sub_1022FF80`) keys off an ancestor
    /// CRandomGroup, so when a box extremum is wrong this is where to look.
    pub chain: Vec<String>,
    pub minx: f32,
    pub maxx: f32,
    pub minz: f32,
    pub maxz: f32,
}

struct Extents {
    minx: f32,
    maxx: f32,
    minz: f32,
    maxz: f32,
    used: usize,
    /// `Some` only under `hull_contributors`; the bake path leaves it `None` so
    /// the label strings are never built.
    trace: Option<Vec<HullContrib>>,
}

impl Extents {
    fn empty(trace: bool) -> Extents {
        Extents {
            minx: f32::INFINITY,
            maxx: f32::NEG_INFINITY,
            minz: f32::INFINITY,
            maxz: f32::NEG_INFINITY,
            used: 0,
            trace: trace.then(Vec::new),
        }
    }

    fn add(&mut self, b: (f32, f32, f32, f32)) {
        let (mnx, mnz, mxx, mxz) = b;
        self.minx = self.minx.min(mnx);
        self.maxx = self.maxx.max(mxx);
        self.minz = self.minz.min(mnz);
        self.maxz = self.maxz.max(mxz);
        self.used += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn note(
        &mut self,
        b: (f32, f32, f32, f32),
        via: &str,
        piece: &super::dat::PieceDef,
        obj: &LayoutObject,
        mesh: &str,
        chain: &[String],
    ) {
        if self.trace.is_none() {
            return;
        }
        let mut flags = Vec::new();
        if let Some(bk) = obj.props.get("BAKE") {
            flags.push(format!("BAKE:{}", bk.trim()));
        }
        if piece.neverbake {
            flags.push("NEVERBAKE".into());
        }
        if piece.alwaysbakecollision {
            flags.push("ALWAYSBAKECOLLISION".into());
        }
        if obj.nopath {
            flags.push("nopath".into());
        }
        let (mnx, mnz, mxx, mxz) = b;
        self.trace.as_mut().unwrap().push(HullContrib {
            via: via.to_string(),
            chain: chain.to_vec(),
            scale: (obj.scale.x, obj.scale.y, obj.scale.z),
            piece: if piece.name.is_empty() { "<unnamed>".into() } else { piece.name.clone() },
            mesh: mesh.rsplit('/').next().unwrap_or(mesh).to_string(),
            flags: flags.join(","),
            minx: mnx,
            maxx: mxx,
            minz: mnz,
            maxz: mxz,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_baked_links(
    layout: &Layout,
    ctx: &Context,
    ext: &mut Extents,
    world: Option<&Mat4>,
    visited: &HashSet<String>,
    depth: usize,
    via: &str,
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
            None => {
                super::native::link_miss_record(lf);
                continue;
            }
        };
        let key = sub_path.to_string_lossy().to_lowercase();
        if visited.contains(&key) {
            continue;
        }
        let sub_arc = match ctx.get_layout(&sub_path) {
            Some(s) => s,
            None => continue,
        };
        let sub: &Layout = sub_arc.as_ref();
        let link_world = strip_link_scale(&obj.world);
        let world0 = match world {
            None => link_world,
            Some(w) => w.matmul(&link_world),
        };
        for pi in sub.room_piece_indices() {
            let piece = &sub.objects[pi];
            if ce_box_gate_enabled() && !collision_enabled(piece) {
                continue;
            }
            let pd = match ctx.guid.get(&piece.guid) {
                Some(pd) => pd,
                None => continue,
            };
            if !registers_for_pathing(sub, pi, true, ctx) {
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
            let tb = transform_bounds_f32(&b, &w);
            ext.add(tb);
            let chain = if via.is_empty() {
                lf.to_string()
            } else {
                format!("{via} -> {lf}")
            };
            let ac = if ext.trace.is_some() { ancestor_chain(sub, piece) } else { Vec::new() };
            ext.note(tb, &chain, pd, piece, coll, &ac);
        }
        let mut v2 = visited.clone();
        v2.insert(key);
        let chain = if via.is_empty() { lf.to_string() } else { format!("{via} -> {lf}") };
        merge_baked_links(sub, ctx, ext, Some(&world0), &v2, depth + 1, &chain);
    }
}

/// Region AABB (X,Z): merge of collision-enabled room pieces' COLLISION mesh
/// bounds (float32 8-corner refit) + the baked Layout-Link geometry.
pub fn compute_region_aabb_collision(layout: &Layout, ctx: &Context) -> RegionResult {
    region_aabb(layout, ctx, false).0
}

/// Same walk as `compute_region_aabb_collision`, but recording every piece that
/// entered the hull. Diagnosis only (`verify mpp-info --pieces`): a box that
/// disagrees with GUTS by one 10-unit quantum is usually ONE piece reaching too
/// far, and it matters a great deal whether that piece is a direct room piece or
/// something pulled in through a Layout Link.
pub fn hull_contributors(layout: &Layout, ctx: &Context) -> Vec<HullContrib> {
    region_aabb(layout, ctx, true).1
}

fn region_aabb(layout: &Layout, ctx: &Context, trace: bool) -> (RegionResult, Vec<HullContrib>) {
    let mut ext = Extents::empty(trace);
    let mut missing = 0usize;
    for pi in layout.room_piece_indices() {
        let piece = &layout.objects[pi];
        if ce_box_gate_enabled() && !collision_enabled(piece) {
            continue;
        }
        let pd = match ctx.guid.get(&piece.guid) {
            Some(pd) => pd,
            None => continue,
        };
        if !registers_for_pathing(layout, pi, false, ctx) {
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
        // A Room Piece's OWN scale DOES apply to the region box. The editor builds the
        // piece matrix as translate * rotate(quaternion) * scale (`Matrix4::setScale`
        // @0x10203c03), unlike a Layout Link's collider, whose scale it strips
        // (`strip_link_scale`). REFUTED (baseline 1279): stripping piece scale scores
        // 1241; stripping it only for `SCALABLE:false` defs is a no-op, so no shipped
        // piece at a box extremum carries that flag. Worth knowing because a 5x-scaled
        // decoration is exactly the shape of the remaining residual.
        let tb = transform_bounds_f32(&b, &piece.world);
        ext.add(tb);
        let chain = if trace { ancestor_chain(layout, piece) } else { Vec::new() };
        ext.note(tb, "", pd, piece, coll, &chain);
    }

    merge_baked_links(layout, ctx, &mut ext, None, &HashSet::new(), 0, "");

    let contribs = ext.trace.take().unwrap_or_default();
    if ext.used == 0 {
        return (
            RegionResult { aabb: Aabb::null(), n_pieces_used: 0, n_pieces_missing: missing },
            contribs,
        );
    }
    (
        RegionResult {
            aabb: Aabb::new(
                Vec3::new(ext.minx as f64, 0.0, ext.minz as f64),
                Vec3::new(ext.maxx as f64, 0.0, ext.maxz as f64),
            ),
            n_pieces_used: ext.used,
            n_pieces_missing: missing,
        },
        contribs,
    )
}

/// Quantize (X,Z) OUTWARD to the 10-unit tile boundary with the 0.2 pad.
///
/// Confirmed in EditorGuts.dll @0x1020418a-0x1020423d (IDA, 2026-08-14) — the DLL
/// does literally this, per axis:
///   `min: (v - 0.2) / 10 -> floor -> * 10`      (dbl_11E50E88 = 0.2, dbl_11E7ED10 = 10.0)
///   `max: (v + 0.2) / 10 -> ceil  -> * 10`
/// and it snaps **`var_C64`, the BASE-piece box** — the detail/backdrop box
/// (`var_BA8`) is never snapped and never reaches the header. That is why excluding
/// backdrop groups from this box (see `registers_for_pathing`) is correct.
/// Previously this was only FITTED (0.2 leaves 4 of 4868 shipped box edges wrong,
/// 0.0 leaves 176, 0.1 leaves 10, 0.4 leaves 32); now it is read off the code.
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

/// Type-19 "Path Bounds Extender" Property Nodes' merged world-box MIN (x, z),
/// or None if the tile has none (R44). GenPathing's master-AABB loop merges each
/// type-19 Property Node's +-0.5*(BOX WIDTH, BOX LENGTH) X/Z box (at its world
/// transform) into the box the grid ORIGIN comes from (@0x10204702 -> 0x10204FD0);
/// the writer box (worldExt/bounds) stays the pure-collision hull, and a PBE
/// contributes NO collision triangles. Mirrors mpp/region.py pbe_origin_min.
pub fn pbe_origin_min(layout: &Layout) -> Option<(f64, f64)> {
    let mut mn: Option<(f64, f64)> = None;
    for o in &layout.objects {
        if o.descriptor != "Property Node" {
            continue;
        }
        if o.props.get("TYPE").map(|s| s.trim()) != Some("Path Bounds Extender") {
            continue;
        }
        let m = &o.world.m;
        // missing/empty prop -> 0.0; a present-but-unparseable value zeroes BOTH
        // (mirrors the Python try/except around the pair).
        let fval = |k: &str| -> Result<f64, ()> {
            match o.props.get(k) {
                None => Ok(0.0),
                Some(s) if s.trim().is_empty() => Ok(0.0),
                Some(s) => s.trim().parse::<f64>().map_err(|_| ()),
            }
        };
        let (hw, hl) = match (fval("BOX WIDTH"), fval("BOX LENGTH")) {
            (Ok(w), Ok(l)) => (0.5 * w, 0.5 * l),
            _ => (0.0, 0.0),
        };
        for lx in [-hw, hw] {
            for lz in [-hl, hl] {
                let wx = m[0][0] * lx + m[0][2] * lz + m[0][3];
                let wz = m[2][0] * lx + m[2][2] * lz + m[2][3];
                mn = Some(match mn {
                    None => (wx, wz),
                    Some((ax, az)) => (ax.min(wx), az.min(wz)),
                });
            }
        }
    }
    mn
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::dat::PieceDef;

    fn obj(bake: Option<&str>) -> LayoutObject {
        let mut o = LayoutObject::default();
        if let Some(b) = bake {
            o.props.insert("BAKE".into(), b.into());
        }
        o
    }

    fn def(never: bool, always: bool) -> PieceDef {
        PieceDef { neverbake: never, alwaysbakecollision: always, ..Default::default() }
    }

    /// The `BAKE:false` predicate itself, which must still mirror
    /// `native::emit_piece`'s add gate — it gates TRIANGLES there, and is used here
    /// only under the (default-off) `MPP_BOX_BAKE_GATE` investigation switch. The
    /// disassembly at 0x10203fdd proves it is not a box gate; see
    /// `registers_for_pathing`.
    #[test]
    fn bake_false_predicate_matches_emit_piece_with_its_two_rescues() {
        let plain = def(false, false);
        assert!(contributes_geometry(&obj(None), Some(&plain)), "no BAKE prop => counts");
        assert!(contributes_geometry(&obj(Some("true")), Some(&plain)));
        assert!(!contributes_geometry(&obj(Some("false")), Some(&plain)));
        // whitespace / case are how the property actually appears in .LAYOUTs
        assert!(!contributes_geometry(&obj(Some(" False ")), Some(&plain)));
        // the two rescues from emit_piece
        assert!(contributes_geometry(&obj(Some("false")), Some(&def(false, true))));
        assert!(contributes_geometry(&obj(Some("false")), Some(&def(true, false))));
        // an unknown GUID must never shrink the box
        assert!(contributes_geometry(&obj(Some("false")), None));
    }

    /// R46: the four points the editor transforms are min, max, and the two you get
    /// by swapping ONLY x between them — y and z stay locked together. Read off
    /// @0x10203e0e-0x10203f42.
    ///
    /// A yaw-only transform cannot tell this from an 8-corner refit (both give all
    /// four XZ corners), and neither can a transform that maps y to x alone, since y
    /// still reaches both extremes among the four. The divergence needs y and z to
    /// arrive at the same output axis with OPPOSITE sign, which the locked pairing
    /// then cancels. That is the case this pins.
    #[test]
    fn piece_box_uses_the_editors_four_corners_not_an_8_corner_refit() {
        let b = Aabb::new(Vec3::new(-1.0, -4.0, -1.0), Vec3::new(1.0, 4.0, 1.0));

        // world.x = y - z  (y and z anti-correlated into x); world.z = z.
        let mut m = Mat4::identity();
        m.m[0] = [0.0, 1.0, -1.0, 0.0];
        m.m[1] = [0.0, 1.0, 0.0, 0.0];
        m.m[2] = [0.0, 0.0, 1.0, 0.0];

        let (x0, _, x1, _) = transform_bounds_f32(&b, &m);
        // Editor's four: (y,z) is only ever (-4,-1) or (4,1), so y-z is +-3.
        assert!((x0 - -3.0).abs() < 1e-4 && (x1 - 3.0).abs() < 1e-4, "got x[{x0},{x1}]");

        // An 8-corner refit would also see (min.x,max.y,min.z) -> 4-(-1) = 5, so it
        // would report +-5. Confirm the two really disagree here, i.e. that the test
        // is exercising the difference and not a shared path.
        let c8 = [
            (-1.0, -4.0, -1.0), (1.0, -4.0, -1.0), (-1.0, 4.0, -1.0), (1.0, 4.0, -1.0),
            (-1.0, -4.0, 1.0), (1.0, -4.0, 1.0), (-1.0, 4.0, 1.0), (1.0, 4.0, 1.0),
        ];
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for (_lx, ly, lz) in c8 {
            let wx = ly - lz;
            lo = lo.min(wx);
            hi = hi.max(wx);
        }
        assert_eq!((lo, hi), (-5.0, 5.0), "8-corner refit reference");
        assert!((x1 as f64) < hi, "the editor's set must be strictly tighter on a tilt");
    }

    /// The 10-unit snap pad, fitted against 4868 observed box edges in the shipped
    /// corpus: 0.2 leaves 4 wrong, 0.0 leaves 176, 0.1 leaves 10, 0.4 leaves 32.
    /// Only hulls landing within the pad of a quantum reveal it, so this pins the
    /// two cases that do.
    #[test]
    fn snap_pad_is_half_a_cell_outward_on_both_sides() {
        let bx = Aabb::new(Vec3::new(-38.058, 0.0, 0.0), Vec3::new(62.498, 0.0, 0.0));
        let s = snap_box_outward(&bx, SNAP);
        assert_eq!((s.min.x, s.max.x), (-40.0, 70.0));
        // A hull ending exactly ON a quantum still snaps outward: this is the
        // ACT2_PASS1_JT case (hull max z 260.0 -> 270), and it is CORRECT per the
        // fit even though GUTS shipped 260 there for an unrelated reason.
        let on = Aabb::new(Vec3::new(0.0, 0.0, 0.0), Vec3::new(260.0, 0.0, 0.0));
        assert_eq!(snap_box_outward(&on, SNAP).max.x, 270.0);
    }
}
