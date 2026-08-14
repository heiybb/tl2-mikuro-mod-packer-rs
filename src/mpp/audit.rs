//! Pack-time interface health check for room-block pathing grids.
//!
//! Two adjacent chunks share a footprint edge line, and that line falls exactly
//! on a 0.4 cell boundary (footprints are TILEBASIS multiples). Nothing in the
//! engine welds the two grids: a tile's cells stop where its floor geometry
//! stops, so the joint is only walkable if some tile puts walkable cells on both
//! sides of the line. Plenty of shipped tiles reach their own edge and no
//! further, which means their neighbour has to OVERHANG — spill its floor across
//! the line — for the pair to connect at all.
//!
//! A new variant that stops neatly at its own footprint edge therefore looks
//! correct in the editor and still seals the joint, because the sibling variant
//! it replaces was carrying the overhang. That defect is invisible per-tile and
//! costs hundreds of whole-zone assembly runs to find, so it is worth one check
//! here: compare each variant's coverage of the shared boundary band against the
//! other variants of the same chunk type, base-install ones included.
//!
//! The block-type NAME is not usable as the interface spec. `1X1_CONCAVE_S2W1`
//! declares its exits at local (+40,+40) and (0,0), which by the engine's own
//! edge classifier (`sub_600490`) are the EAST and NORTH edges — the letters do
//! not name the sides. The zone's `*_RULES.TEMPLATE` `[EXIT]` coordinates are
//! the ground truth, so that is what this reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::native::{CELL, HALF};

/// `region::grid_origin` for one axis, duplicated here so a `Grid` can be read from a
/// bare .mpp header with no Layout in hand.
fn grid_origin_x(box_min: f64) -> f64 {
    let t = ((box_min as f32) as f64 - super::region::PAD32 as f64) / super::region::CELL32 as f64;
    let t = t.floor();
    (((t - super::region::EXPAND) * super::region::CELL32 as f64) as f32) as f64
}

/// Walkable-domain edges, in report order.
pub const EDGE_NAMES: [&str; 4] = ["N", "S", "W", "E"];

#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeCoverage {
    /// Cells of the band that are walkable and in the main component.
    pub inside: u32,
    /// Same, for the band on the neighbour's side of the shared line.
    pub outside: u32,
    /// Band length in cells (the part of the edge that lies inside the grid).
    pub span: u32,
}

impl EdgeCoverage {
    pub fn inside_frac(&self) -> f64 {
        self.inside as f64 / self.span.max(1) as f64
    }
    pub fn outside_frac(&self) -> f64 {
        self.outside as f64 / self.span.max(1) as f64
    }
}

#[derive(Clone, Debug)]
pub struct ChunkTypeDef {
    pub width: f64,
    pub height: f64,
    /// Declared exits in tile-local coords: (x, z, type).
    pub exits: Vec<(f64, f64, i64)>,
}

impl ChunkTypeDef {
    /// Which edges this type actually joins a neighbour on: the set of edges its
    /// declared exits sit on, by the engine's own exit-edge classifier
    /// (RE sub_600490 — `d = z - depth/2`, then the dominant axis of `(x, d)`).
    ///
    /// This is what makes the audit quiet enough to be useful. An edge with no
    /// declared exit is not a joint, so variation there means nothing; and a
    /// standalone room (a boss arena, no exits at all) joins on no edge and is
    /// skipped entirely.
    pub fn exit_edges(&self, depth: f64) -> [bool; 4] {
        let mut e = [false; 4];
        for &(x, z, _) in &self.exits {
            let d = z - depth * 0.5;
            let idx = if d > 0.0 && x.abs() < d {
                1 // S
            } else if d < 0.0 && x.abs() < -d {
                0 // N
            } else if x > 0.0 {
                3 // E
            } else {
                2 // W
            };
            e[idx] = true;
        }
        e
    }
}

/// One zone's `*_RULES.TEMPLATE`: the tile grid basis plus every chunk type.
#[derive(Clone, Debug)]
pub struct ZoneRules {
    pub tile_basis: f64,
    pub chunk_w_basis: f64,
    pub chunk_h_basis: f64,
    pub types: HashMap<String, ChunkTypeDef>,
}

impl ZoneRules {
    /// Footprint of a chunk type in the tile's own (== .mpp) local frame, as
    /// `(centre_x, half_w, centre_z, half_d)`.
    ///
    /// Straight out of the engine's exit normaliser (RE sub_6087A0:537-543,
    /// `nx = (x - (W-1)*cwb*tb)/tb`, `nz = (tb/2 + z - H*chb*tb/2)/tb`): X is
    /// centred on the chunk, Z is centred HALF A TILE short of centre. For a 1x1
    /// chunk with tb=8, cwb=chb=11 that gives x in [-44,+44] and z in [-4,+84],
    /// which is what placed vanilla tiles measure: their walkable domains span
    /// exactly 220 cells on each axis and meet neighbours on the lines
    /// `pos.x +- 44` and `pos.z + 84`.
    pub fn footprint(&self, ct: &ChunkTypeDef) -> (f64, f64, f64, f64) {
        let (tb, cwb, chb) = (self.tile_basis, self.chunk_w_basis, self.chunk_h_basis);
        let half_w = tb * cwb * ct.width * 0.5;
        let half_d = tb * chb * ct.height * 0.5;
        (
            (ct.width - 1.0) * cwb * tb,
            half_w,
            half_d - tb * 0.5,
            half_d,
        )
    }

    /// Parse a RULES.TEMPLATE. Accepts the UTF-16LE the game ships and UTF-8.
    pub fn load(path: &Path) -> Option<ZoneRules> {
        let raw = std::fs::read(path).ok()?;
        let text = decode_template(&raw);
        let mut zr = ZoneRules {
            tile_basis: 8.0,
            chunk_w_basis: 11.0,
            chunk_h_basis: 11.0,
            types: HashMap::new(),
        };
        // The LEVEL-level bases appear before any [CHUNKTYPE]; per-type WIDTH /
        // HEIGHT / [EXIT] blocks follow. A flat scan is enough — the only
        // nesting that matters is "am I inside a [CHUNKTYPE], and inside an
        // [EXIT] of it".
        let mut cur_name: Option<String> = None;
        let mut cur = ChunkTypeDef { width: 1.0, height: 1.0, exits: Vec::new() };
        let mut in_exit = false;
        let (mut ex, mut ez, mut et) = (0.0f64, 0.0f64, 0i64);
        let mut seen_chunktype = false;
        for line in text.lines() {
            let l = line.trim();
            let val = |l: &str| -> Option<f64> { l.rsplit(':').next()?.trim().parse().ok() };
            if l == "[CHUNKTYPE]" {
                if let Some(n) = cur_name.take() {
                    zr.types.insert(n, std::mem::replace(
                        &mut cur,
                        ChunkTypeDef { width: 1.0, height: 1.0, exits: Vec::new() },
                    ));
                }
                cur = ChunkTypeDef { width: 1.0, height: 1.0, exits: Vec::new() };
                seen_chunktype = true;
                continue;
            }
            if l == "[EXIT]" {
                in_exit = true;
                (ex, ez, et) = (0.0, 0.0, 0);
                continue;
            }
            if l == "[/EXIT]" {
                if in_exit {
                    cur.exits.push((ex, ez, et));
                }
                in_exit = false;
                continue;
            }
            if let Some(rest) = l.strip_prefix("<STRING>NAME:") {
                if seen_chunktype {
                    cur_name = Some(rest.trim().to_uppercase());
                }
            } else if let Some(v) = l.strip_prefix("<FLOAT>TILEBASIS:").and_then(|_| val(l)) {
                if !seen_chunktype {
                    zr.tile_basis = v;
                }
            } else if l.starts_with("<FLOAT>CHUNKWIDTHBASIS:") {
                if let Some(v) = val(l) {
                    zr.chunk_w_basis = v;
                    zr.chunk_h_basis = v;
                }
            } else if l.starts_with("<FLOAT>CHUNKHEIGHTBASIS:") {
                if let Some(v) = val(l) {
                    zr.chunk_h_basis = v;
                }
            } else if l.starts_with("<INTEGER>WIDTH:") {
                if let Some(v) = val(l) {
                    cur.width = v;
                }
            } else if l.starts_with("<INTEGER>HEIGHT:") {
                if let Some(v) = val(l) {
                    cur.height = v;
                }
            } else if in_exit && l.starts_with("<FLOAT>X:") {
                ex = val(l).unwrap_or(0.0);
            } else if in_exit && l.starts_with("<FLOAT>Z:") {
                ez = val(l).unwrap_or(0.0);
            } else if in_exit && l.starts_with("<INTEGER>TYPE:") {
                et = val(l).unwrap_or(0.0) as i64;
            }
        }
        if let Some(n) = cur_name {
            zr.types.insert(n, cur);
        }
        Some(zr)
    }
}

fn decode_template(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let u: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u)
    } else {
        String::from_utf8_lossy(raw).trim_start_matches('\u{feff}').to_string()
    }
}

/// A baked grid plus the header fields needed to place it.
pub struct Grid<'a> {
    pub cells: &'a [u8],
    pub gw: i32,
    pub gh: i32,
    /// Header worldExt — the offset from the tile origin to the grid box min.
    pub ext_x: f32,
    pub ext_z: f32,
}

/// Per-exit inspection result: (local x, local z, usable, why).
pub type ExitStatus = (f64, f64, bool, &'static str);

impl<'a> Grid<'a> {
    /// Parse a complete .mpp (24-byte header + body).
    pub fn parse(buf: &'a [u8]) -> Option<Grid<'a>> {
        if buf.len() < 24 {
            return None;
        }
        let gw = i32::from_le_bytes(buf[0..4].try_into().ok()?);
        let gh = i32::from_le_bytes(buf[4..8].try_into().ok()?);
        let ext_x = f32::from_le_bytes(buf[8..12].try_into().ok()?);
        let ext_z = f32::from_le_bytes(buf[12..16].try_into().ok()?);
        if gw <= 0 || gh <= 0 {
            return None;
        }
        let cells = &buf[24..];
        if cells.len() != gw as usize * gh as usize {
            return None;
        }
        Some(Grid { cells, gw, gh, ext_x, ext_z })
    }

    /// Tile-local coord -> cell index.
    ///
    /// Cell centres are NOT at `-ext + HALF + i*CELL`. `build_grid_fast` puts them at
    /// `origin + HALF + (i0 + i)*CELL` where `origin = grid_origin(box_min)` is f32
    /// quantised and `i0 = floor((box_min - origin)/CELL)`. That f32 step makes
    /// `(box_min - origin)/CELL` land at 1.999998 for `box_min = -40` but 2.000008 for
    /// `box_min = -70`, so **i0 is 1 for some tiles and 2 for others** and the naive
    /// inverse is off by a whole cell on the i0==1 ones. Getting this wrong made two
    /// variants of the same chunk type look 0.4u apart when their floors in fact end
    /// at the identical local position.
    pub fn idx(local: f64, ext: f32) -> i64 {
        let box_min = -(ext as f64);
        let origin = grid_origin_x(box_min);
        let i0 = ((box_min - origin) / CELL).floor();
        (((local - HALF - origin) / CELL) - i0).round() as i64
    }

    fn at(&self, x: i64, z: i64) -> Option<u8> {
        if x < 0 || z < 0 || x >= self.gw as i64 || z >= self.gh as i64 {
            return None;
        }
        Some(self.cells[(z * self.gw as i64 + x) as usize])
    }

    /// Mask of the largest 4-connected walkable component. A walkable pocket cut
    /// off from the tile body reaches the edge without connecting anything, so
    /// coverage is only counted on the main body.
    pub fn main_component(&self) -> Vec<bool> {
        let (w, h) = (self.gw as usize, self.gh as usize);
        let n = w * h;
        let mut seen = vec![false; n];
        let mut best = vec![false; n];
        let mut best_len = 0usize;
        let mut stack: Vec<usize> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        for start in 0..n {
            if self.cells[start] != 0 || seen[start] {
                continue;
            }
            cur.clear();
            stack.push(start);
            seen[start] = true;
            while let Some(p) = stack.pop() {
                cur.push(p);
                let (z, x) = (p / w, p % w);
                if z > 0 && self.cells[p - w] == 0 && !seen[p - w] {
                    seen[p - w] = true;
                    stack.push(p - w);
                }
                if z + 1 < h && self.cells[p + w] == 0 && !seen[p + w] {
                    seen[p + w] = true;
                    stack.push(p + w);
                }
                if x > 0 && self.cells[p - 1] == 0 && !seen[p - 1] {
                    seen[p - 1] = true;
                    stack.push(p - 1);
                }
                if x + 1 < w && self.cells[p + 1] == 0 && !seen[p + 1] {
                    seen[p + 1] = true;
                    stack.push(p + 1);
                }
            }
            if cur.len() > best_len {
                best_len = cur.len();
                best.iter_mut().for_each(|b| *b = false);
                for &c in &cur {
                    best[c] = true;
                }
            }
        }
        best
    }

    /// How far the main walkable component reaches on each edge, measured in ENGINE
    /// PLACEMENT CELLS relative to the tile origin, in EDGE_NAMES order (N,S,W,E).
    ///
    /// This is the only frame in which the number means anything. The engine stamps
    /// tile cell `i` at `floor((pos - worldExt)/CELL) + i`, so a tile's reach relative
    /// to its own origin is `floor(-worldExt/CELL) + i` — independent of where it is
    /// placed (verified against four different placements). Measuring instead in baked
    /// LOCAL coordinates is what produced a confident wrong answer earlier: the `i0`
    /// straddle shifts two variants' local readings by a cell in opposite directions
    /// and exactly cancels the real difference.
    ///
    /// For a 1x1 chunk (tb 8, cwb/chb 11) the footprint edges land at W -110, E +110,
    /// N -10, S +210 cells. Reaching 109 means "my own last cell"; reaching 110 means
    /// "one cell into the neighbour". Both are legal — a joint works if both sides
    /// reach their own last cell, OR if one side overhangs to cover for a neighbour
    /// that does not. Hence sibling comparison rather than an absolute threshold.
    pub fn edge_reach(&self) -> [i64; 4] {
        let main = self.main_component();
        let (w, h) = (self.gw as usize, self.gh as usize);
        let (mut zmin, mut zmax, mut xmin, mut xmax) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for z in 0..h {
            for x in 0..w {
                if main[z * w + x] {
                    zmin = zmin.min(z as i64);
                    zmax = zmax.max(z as i64);
                    xmin = xmin.min(x as i64);
                    xmax = xmax.max(x as i64);
                }
            }
        }
        if xmin == i64::MAX {
            return [0; 4];
        }
        let bx = (-(self.ext_x as f64) / CELL).floor() as i64;
        let bz = (-(self.ext_z as f64) / CELL).floor() as i64;
        [bz + zmin, bz + zmax, bx + xmin, bx + xmax]
    }

    /// Coverage of the four shared boundary bands, in EDGE_NAMES order.
    ///
    /// `fp` is `(centre_x, half_w, centre_z, half_d)` from `ZoneRules::footprint`.
    /// For each edge, `inside` counts the last cell ring still inside the
    /// footprint and `outside` the first ring across the shared line — the ring
    /// a neighbour that stops at its own edge needs someone else to fill.
    pub fn edge_coverage(&self, fp: (f64, f64, f64, f64)) -> [EdgeCoverage; 4] {
        let (cx, hw, cz, hd) = fp;
        let (x_lo, x_hi, z_lo, z_hi) = (cx - hw, cx + hw, cz - hd, cz + hd);
        let main = self.main_component();
        let hit = |x: i64, z: i64| -> bool {
            if x < 0 || z < 0 || x >= self.gw as i64 || z >= self.gh as i64 {
                return false;
            }
            main[(z * self.gw as i64 + x) as usize]
        };
        // Scan each edge over the footprint's own extent, not the whole grid: an
        // oversized box (geometry spilling into neighbours) would otherwise
        // dilute the fraction with cells that belong to no joint.
        let zin_lo = Self::idx(z_lo + HALF, self.ext_z);
        let zin_hi = Self::idx(z_hi - HALF, self.ext_z);
        let xin_lo = Self::idx(x_lo + HALF, self.ext_x);
        let xin_hi = Self::idx(x_hi - HALF, self.ext_x);

        let mut out = [EdgeCoverage::default(); 4];
        // N / S: vary x, fixed z.
        for (e, (zin, zout)) in [
            (0usize, (zin_lo, Self::idx(z_lo - HALF, self.ext_z))),
            (1, (zin_hi, Self::idx(z_hi + HALF, self.ext_z))),
        ] {
            for x in xin_lo..=xin_hi {
                if self.at(x, zin).is_none() && self.at(x, zout).is_none() {
                    continue;
                }
                out[e].span += 1;
                out[e].inside += hit(x, zin) as u32;
                out[e].outside += hit(x, zout) as u32;
            }
        }
        // W / E: vary z, fixed x.
        for (e, (xin, xout)) in [
            (2usize, (xin_lo, Self::idx(x_lo - HALF, self.ext_x))),
            (3, (xin_hi, Self::idx(x_hi + HALF, self.ext_x))),
        ] {
            for z in zin_lo..=zin_hi {
                if self.at(xin, z).is_none() && self.at(xout, z).is_none() {
                    continue;
                }
                out[e].span += 1;
                out[e].inside += hit(xin, z) as u32;
                out[e].outside += hit(xout, z) as u32;
            }
        }
        out
    }

    /// Is each declared exit walkable and part of the tile's main body?
    pub fn exit_status(&self, exits: &[(f64, f64, i64)]) -> Vec<ExitStatus> {
        let main = self.main_component();
        exits
            .iter()
            .map(|&(x, z, _t)| {
                let (xi, zi) = (Self::idx(x, self.ext_x), Self::idx(z, self.ext_z));
                match self.at(xi, zi) {
                    None => (x, z, false, "outside grid"),
                    Some(0) => {
                        let ok = main[(zi * self.gw as i64 + xi) as usize];
                        (x, z, ok, if ok { "walkable" } else { "walkable but stranded" })
                    }
                    Some(0xFF) => (x, z, false, "void (no geometry)"),
                    Some(_) => (x, z, false, "wall"),
                }
            })
            .collect()
    }
}

/// The stamp-alignment index `i0` this grid was baked with.
///
/// `build_grid_fast` places cell `i` at `origin + HALF + (i0+i)*CELL`, but the engine
/// stamps tile cell `i` at level cell `floor((pos - worldExt)/CELL) + i` — it does NOT
/// know about `i0`. The two agree only when `i0 == 2`. Because `origin` is f32
/// quantised, `(box_min - origin)/CELL` comes out 1.999998 for `box_min = -40` and
/// 2.000008 for `box_min = -70`, so **i0 is 1 for some boxes and 2 for others**, and an
/// i0==1 tile has its whole grid stamped one cell off from where it was baked.
///
/// Two variants of one chunk type with DIFFERENT i0 therefore land one cell apart at
/// runtime even when their floors end at the identical local position. Salt Barrens:
/// `_LM_A` (worldExt 40, i0=1) is stamped one level-cell further east than `_PB_A`
/// (worldExt 70, i0=2), because the mod variant's oversized collision hull pushed its
/// snapped box from 90 to 120 wide.
///
/// NOT usable as a warning, measured: emitting a finding whenever siblings disagree on
/// i0 gives 27 hits on component 04 but **309 on the base game alone**, which plainly
/// does not seal. So an i0 mismatch is NECESSARY-not-sufficient — it only bites where
/// the joint is already marginal (the neighbour contributes nothing, so that one cell
/// is the only bridge). Kept as a diagnostic helper, not a check.
pub fn stamp_i0(ext: f32) -> i64 {
    let box_min = -(ext as f64);
    let origin = grid_origin_x(box_min);
    ((box_min - origin) / CELL).floor() as i64
}

/// One variant's audited state.
///
/// Deliberately no "is every declared exit walkable" verdict here. Door
/// thresholds are void in a baked grid by design — the door props do not take
/// part in the bake and the engine fills the band by raycast at load (stamp
/// point 4 of sub_5F36D0) — so an exit sitting on void is normal, not a defect.
/// Tested against the shipped corpus, that check fired on 701 known-good vanilla
/// variants. `Grid::exit_status` is kept for inspection, not for warnings.
pub struct VariantAudit {
    pub name: String,
    pub from_base: bool,
    /// N,S,W,E reach in engine placement cells relative to the tile origin.
    pub reach: [i64; 4],
    pub cov: [EdgeCoverage; 4],
    /// header worldExt and the resulting stamp-alignment index, per axis
    pub ext: (f32, f32),
    pub i0: (i64, i64),
}

/// A problem worth printing at pack time.
pub struct Finding {
    pub zone: String,
    pub chunk_type: String,
    pub variant: String,
    pub message: String,
}

/// Compare the variants of one chunk type and report edges where a variant
/// carries far less of the shared boundary band than its siblings.
///
/// Fires on the one shape that actually seals a joint: this variant stops at its own
/// last cell while a sibling reaches one further, into the neighbour's first cell.
#[allow(clippy::too_many_arguments)]
pub fn compare_variants(
    zone: &str,
    chunk_type: &str,
    audits: &[VariantAudit],
    exit_edges: [bool; 4],
    fp: (f64, f64, f64, f64),
) -> Vec<Finding> {
    // Footprint edges in engine placement cells: the first cell BEYOND the tile, i.e.
    // the neighbour's first cell. Own last cell is one less.
    let (cx, hw, cz, hd) = fp;
    let edge_cell = [
        ((cz - hd) / CELL).round() as i64,   // N (outward negative)
        ((cz + hd) / CELL).round() as i64,   // S
        ((cx - hw) / CELL).round() as i64,   // W (outward negative)
        ((cx + hw) / CELL).round() as i64,   // E
    ];

    let mut out = Vec::new();
    // Reach comparison, in ENGINE PLACEMENT CELLS. A variant that does not reach as far
    // as a sibling on an edge that carries a declared exit will land short of wherever
    // that sibling used to bridge, and a neighbour which only reaches its own last cell
    // has nothing to meet it with. One cell is enough — that is the whole Salt Barrens
    // bug (110 vs 109 on E).
    //
    // Deliberately NOT measured in baked-local coordinates: the `i0` straddle shifts two
    // variants' local readings in opposite directions and cancels the real difference,
    // which is how an earlier version of this check reported a confident wrong answer.
    for e in 0..4 {
        if !exit_edges[e] {
            continue;
        }
        // N and W are "more negative is further"; S and E are "more positive".
        let outward_neg = e == 0 || e == 2;
        let better = |a: i64, b: i64| if outward_neg { a < b } else { a > b };
        let mut best = None::<(&VariantAudit, i64)>;
        for b in audits.iter() {
            let r = b.reach[e];
            if best.map(|(_, v)| better(r, v)).unwrap_or(true) {
                best = Some((b, r));
            }
        }
        let Some((bv, br)) = best else { continue };
        // Only the marginal shape matters. A variant that falls far short simply has a
        // closed edge there by design (a `W0` tile is supposed to be void on its west);
        // the dangerous case is stopping at your OWN last cell while a sibling reaches
        // one further, into the neighbour's first cell. Anything else is noise: without
        // this gate the check fires 360 times on the base game, which does not seal.
        let edge = edge_cell[e];
        let own_last = if outward_neg { edge + 1 } else { edge - 1 };
        let sibling_overhangs = if outward_neg { br <= edge } else { br >= edge };
        if !sibling_overhangs {
            continue;
        }
        for a in audits.iter().filter(|a| !a.from_base) {
            let ar = a.reach[e];
            if ar != own_last {
                continue;
            }
            let short = (br - ar).abs();
            out.push(Finding {
                zone: zone.to_string(),
                chunk_type: chunk_type.to_string(),
                variant: a.name.clone(),
                message: format!(
                    "{} edge carries a declared exit, and this variant's walkable reaches {ar} (engine placement cells from the tile origin) while sibling {} reaches {br} — {short} cell(s) short. Where the neighbour only reaches its own last cell, the sibling's extra cell is the only thing bridging that joint, so swapping it for this variant seals it.",
                    EDGE_NAMES[e],
                    bv.name
                ),
            });
        }
    }

    out
}

/// Audit every baked room-block grid, grouped by chunk type.
///
/// `baked` maps a `.LAYOUT` path under `media_dir` to its freshly compiled .mpp
/// bytes, borrowed — a whole mod's grids are hundreds of MB and there is no
/// reason to copy them to audit them. Siblings from the base install are pulled in read-only so that a mod
/// which ships ONE variant of a chunk type is still compared against the shipped
/// ones — which is the whole point, since the variant it displaces in the pool is
/// usually a base-game one.
pub fn audit_tree(
    media_dir: &Path,
    base_media: Option<&Path>,
    baked: &[(PathBuf, &[u8])],
) -> Vec<Finding> {
    // group: (zone, chunk_type_dir) -> [(variant file name, its grid)]
    type Groups<'b> = HashMap<(String, String), Vec<(String, &'b [u8])>>;
    let mut groups: Groups = HashMap::new();
    let layouts = media_dir.join("LAYOUTS");
    for (lp, bytes) in baked {
        let Ok(rel) = lp.strip_prefix(&layouts) else { continue };
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        // Only <ZONE>/<CHUNKTYPE>/<variant>.LAYOUT has a chunk type to look up.
        if parts.len() != 3 {
            continue;
        }
        let variant = parts[2].trim_end_matches(|_| false).to_string();
        groups
            .entry((parts[0].to_uppercase(), parts[1].to_uppercase()))
            .or_default()
            .push((variant, bytes));
    }

    let mut rules_cache: HashMap<String, Vec<ZoneRules>> = HashMap::new();
    let mut findings = Vec::new();
    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let (zone, ctype) = (&key.0, &key.1);
        let rules = rules_cache
            .entry(zone.clone())
            .or_insert_with(|| load_zone_rules(media_dir, base_media, zone));
        let Some((zr, ct)) = find_chunk_type(rules, ctype) else { continue };
        let fp = zr.footprint(ct);

        let mut audits: Vec<VariantAudit> = Vec::new();
        for (name, bytes) in &groups[&key] {
            if let Some(g) = Grid::parse(bytes) {
                audits.push(VariantAudit {
                    name: name.clone(),
                    from_base: false,
                    reach: g.edge_reach(),
                    cov: g.edge_coverage(fp),
                    ext: (g.ext_x, g.ext_z),
                    i0: (stamp_i0(g.ext_x), stamp_i0(g.ext_z)),
                });
            }
        }
        // base-install siblings (shipped .MPP), skipping names the mod overrides
        if let Some(bm) = base_media {
            let dir = bm.join("LAYOUTS").join(zone).join(ctype);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                entries.sort();
                for p in entries {
                    let fname = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if !fname.to_uppercase().ends_with(".MPP") {
                        continue;
                    }
                    let stem = fname[..fname.len() - 4].to_uppercase();
                    if audits
                        .iter()
                        .any(|a| a.name.to_uppercase().starts_with(&stem))
                    {
                        continue; // the mod replaces this variant
                    }
                    let Ok(buf) = std::fs::read(&p) else { continue };
                    if let Some(g) = Grid::parse(&buf) {
                        audits.push(VariantAudit {
                            name: format!("{fname} (base)"),
                            from_base: true,
                            reach: g.edge_reach(),
                            cov: g.edge_coverage(fp),
                            ext: (g.ext_x, g.ext_z),
                            i0: (stamp_i0(g.ext_x), stamp_i0(g.ext_z)),
                        });
                    }
                }
            }
        }
        if audits.iter().any(|a| !a.from_base) {
            let edges = ct.exit_edges(2.0 * fp.3);
            findings.extend(compare_variants(zone, ctype, &audits, edges, fp));
        }
    }
    findings
}

/// Every `*RULES.TEMPLATE` in `MEDIA/LAYOUTS/<zone>/`, mod first then base install.
///
/// A zone folder routinely holds several — the base game's own plus any a mod
/// adds — and the chunk types are split across them, so picking just one finds
/// the wrong table (or none) for most tiles.
pub fn find_zone_rules(media_dir: &Path, base_media: Option<&Path>, zone: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in [Some(media_dir), base_media].into_iter().flatten() {
        let dir = root.join("LAYOUTS").join(zone);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut here: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_uppercase().ends_with("RULES.TEMPLATE"))
                    .unwrap_or(false)
            })
            .collect();
        here.sort();
        out.extend(here);
    }
    out
}

/// Parse every rules template in a zone, mod first. Kept as a LIST rather than
/// merged into one table because TILEBASIS / CHUNK*BASIS are per-template, so a
/// chunk type has to be read with the bases of the file that declares it.
pub fn load_zone_rules(media_dir: &Path, base_media: Option<&Path>, zone: &str) -> Vec<ZoneRules> {
    find_zone_rules(media_dir, base_media, zone)
        .iter()
        .filter_map(|p| ZoneRules::load(p))
        .collect()
}

/// First rules table declaring `name`, with its own bases. Mod templates come
/// first in the list, so a mod redefinition of a chunk type wins.
pub fn find_chunk_type<'a>(
    rules: &'a [ZoneRules],
    name: &str,
) -> Option<(&'a ZoneRules, &'a ChunkTypeDef)> {
    rules.iter().find_map(|zr| zr.types.get(name).map(|ct| (zr, ct)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_1x1() -> ZoneRules {
        ZoneRules {
            tile_basis: 8.0,
            chunk_w_basis: 11.0,
            chunk_h_basis: 11.0,
            types: HashMap::new(),
        }
    }

    /// The footprint convention is the load-bearing fact here, and it is not the
    /// obvious one: Z is centred half a tile SHORT of centre, so a 1x1 chunk is
    /// x[-44,44] z[-4,84], not z[0,88]. Measured against placed vanilla tiles,
    /// whose walkable domains span exactly 220 cells and meet on those lines.
    #[test]
    fn footprint_matches_the_engines_exit_normaliser() {
        let zr = rules_1x1();
        let ct = ChunkTypeDef { width: 1.0, height: 1.0, exits: vec![] };
        let (cx, hw, cz, hd) = zr.footprint(&ct);
        assert_eq!((cx - hw, cx + hw), (-44.0, 44.0));
        assert_eq!((cz - hd, cz + hd), (-4.0, 84.0));
    }

    /// `1X1_CONCAVE_S2W1_LANDMARK_VAULTOFSOULS` declares (+40,+40) and (0,0).
    /// The letters in the name say S and W; the engine's classifier says E and N,
    /// and the grids agree with the engine. Guard against "fixing" this to match
    /// the name.
    #[test]
    fn exit_edges_ignore_the_block_name_letters() {
        let ct = ChunkTypeDef {
            width: 1.0,
            height: 1.0,
            exits: vec![(40.0, 40.0, 1), (0.0, 0.0, 2)],
        };
        let e = ct.exit_edges(88.0);
        assert_eq!(e, [true, false, false, true], "expected N and E, got {e:?}");
    }

    #[test]
    fn exit_edges_of_a_four_way_chunk_cover_all_sides() {
        let ct = ChunkTypeDef {
            width: 1.0,
            height: 1.0,
            exits: vec![(0.0, 80.0, 0), (40.0, 40.0, 0), (-40.0, 40.0, 2), (0.0, 0.0, 2)],
        };
        assert_eq!(ct.exit_edges(88.0), [true, true, true, true]);
    }

    #[test]
    fn a_standalone_room_joins_on_no_edge() {
        let ct = ChunkTypeDef { width: 1.0, height: 1.0, exits: vec![] };
        assert_eq!(ct.exit_edges(88.0), [false; 4]);
    }

    fn template(body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tl2audit_{}.TEMPLATE", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_bases_and_per_type_exits() {
        let p = template(
            "[LEVEL]\n\t<STRING>NAME:Zone\n\t<FLOAT>TILEBASIS:8\n\
             \t<FLOAT>CHUNKWIDTHBASIS:11\n\t<FLOAT>CHUNKHEIGHTBASIS:11\n\
             \t[CHUNKTYPE]\n\t\t<STRING>NAME:A_TYPE\n\t\t<INTEGER>WIDTH:1\n\
             \t\t<INTEGER>HEIGHT:1\n\t\t[EXIT]\n\t\t\t<FLOAT>X:40\n\t\t\t<FLOAT>Z:40\n\
             \t\t\t<INTEGER>TYPE:1\n\t\t[/EXIT]\n\t[/CHUNKTYPE]\n",
        );
        let zr = ZoneRules::load(&p).expect("parse");
        assert_eq!(zr.tile_basis, 8.0);
        assert_eq!(zr.chunk_h_basis, 11.0);
        let ct = zr.types.get("A_TYPE").expect("type present");
        assert_eq!(ct.exits, vec![(40.0, 40.0, 1)]);
        // The LEVEL's own NAME must not be mistaken for a chunk type.
        assert!(!zr.types.contains_key("ZONE"));
        let _ = std::fs::remove_file(&p);
    }

    /// Build a grid whose walkable stops exactly ON the footprint line: `inside`
    /// full, `outside` zero. That is the shape that seals a joint.
    #[test]
    fn edge_coverage_separates_reaching_from_overhanging() {
        let zr = rules_1x1();
        let ct = ChunkTypeDef { width: 1.0, height: 1.0, exits: vec![] };
        let fp = zr.footprint(&ct);
        // Box mirroring a real tile: local x from -50, z from -10; 0.4 cells.
        let (ext_x, ext_z) = (50.0f32, 10.0f32);
        let (gw, gh) = (251i32, 250i32);
        let mut cells = vec![0xFFu8; (gw * gh) as usize];
        // Walkable up to and including the last cell inside the footprint.
        let x_in = Grid::idx(44.0 - HALF, ext_x) as i32;
        let x_out = Grid::idx(44.0 + HALF, ext_x) as i32;
        assert_eq!(x_out, x_in + 1);
        for z in 0..gh {
            for x in 0..=x_in {
                cells[(z * gw + x) as usize] = 0;
            }
        }
        let g = Grid { cells: &cells, gw, gh, ext_x, ext_z };
        let cov = g.edge_coverage(fp);
        let e = cov[3]; // E
        assert!(e.span >= 200, "band should span the footprint, got {}", e.span);
        assert_eq!(e.inside, e.span, "floor reaches its own edge everywhere");
        assert_eq!(e.outside, 0, "and crosses nowhere");

        // Now extend by that single cell: the joint becomes bridgeable.
        for z in 0..gh {
            cells[(z * gw + x_out) as usize] = 0;
        }
        let g = Grid { cells: &cells, gw, gh, ext_x, ext_z };
        let cov = g.edge_coverage(fp);
        assert_eq!(cov[3].outside, cov[3].span, "one cell of overhang covers the band");
    }

    /// A walkable pocket cut off from the body reaches the edge without
    /// connecting anything, so it must not count.
    #[test]
    fn coverage_counts_only_the_main_component() {
        let zr = rules_1x1();
        let ct = ChunkTypeDef { width: 1.0, height: 1.0, exits: vec![] };
        let fp = zr.footprint(&ct);
        let (ext_x, ext_z) = (50.0f32, 10.0f32);
        let (gw, gh) = (251i32, 250i32);
        let mut cells = vec![0xFFu8; (gw * gh) as usize];
        // Big body well inside, plus an isolated strip sitting on the E band.
        for z in 30..200 {
            for x in 30..150 {
                cells[(z * gw + x) as usize] = 0;
            }
        }
        let x_out = Grid::idx(44.0 + HALF, ext_x) as i32;
        for z in 30..200 {
            cells[(z * gw + x_out) as usize] = 0;
        }
        let g = Grid { cells: &cells, gw, gh, ext_x, ext_z };
        assert_eq!(g.edge_coverage(fp)[3].outside, 0, "stranded strip must not count");
    }

    /// Footprint for a 1x1 chunk (tb 8, cwb/chb 11): edges at W -110, E +110 in engine
    /// placement cells, so "own last cell" on E is 109.
    const FP_1X1: (f64, f64, f64, f64) = (0.0, 44.0, 40.0, 44.0);

    fn va(name: &str, from_base: bool, reach_e: i64) -> VariantAudit {
        VariantAudit {
            name: name.into(),
            from_base,
            reach: [-10, 210, -110, reach_e],
            cov: [EdgeCoverage::default(); 4],
            ext: (40.0, 10.0),
            i0: (1, 1),
        }
    }

    #[test]
    fn compare_is_quiet_when_no_sibling_reaches_further() {
        // both stop at their own last cell — normal, and fine as long as the
        // neighbour reaches its own too
        let audits = vec![va("mod", false, 109), va("base", true, 109)];
        assert!(compare_variants("Z", "T", &audits, [true; 4], FP_1X1).is_empty());
    }

    #[test]
    fn compare_flags_one_cell_short_of_an_overhanging_sibling() {
        // the Salt Barrens shape: sibling reaches 110 (into the neighbour), this one 109
        let audits = vec![va("mod_PB_A", false, 109), va("base_LM_A", true, 110)];
        let f = compare_variants("Z", "T", &audits, [false, false, false, true], FP_1X1);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].variant, "mod_PB_A");
        assert!(f[0].message.starts_with("E edge"), "{}", f[0].message);
        // an edge with no declared exit is not a joint
        assert!(compare_variants("Z", "T", &audits, [false; 4], FP_1X1).is_empty());
    }

    /// A variant that falls FAR short simply has a closed edge there by design; only
    /// the marginal one-cell case is reported. Without this gate the check fires 360
    /// times on the base game, which does not seal.
    #[test]
    fn compare_ignores_a_variant_whose_edge_is_closed() {
        let audits = vec![va("mod", false, 60), va("base", true, 110)];
        assert!(compare_variants("Z", "T", &audits, [false, false, false, true], FP_1X1).is_empty());
    }
}
