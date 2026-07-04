//! High-level offline .mpp generation (port of mpp/pipeline.py).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::dat::{load_all_levelsets, PieceDef};
use super::geom::{Aabb, Vec3};
use super::layout::{load_layout_file, Layout};
use super::ogre_mesh::{load_mesh_file, Mesh};
use super::region::{
    compute_region_aabb_collision, grid_dims, grid_origin, header_floats, pack_header,
    resolve_mesh_path, snap_box_outward, RegionResult, SNAP,
};

/// Files with no room pieces default to a 20x20 box.
const DEFAULT_PORTAL_HALF: f64 = 10.0;

/// Shared guid table + mesh caches so a batch run parses everything once.
/// Thread-safe (used from rayon workers).
pub struct Context {
    pub media_dir: PathBuf,
    pub fallback_dir: Option<PathBuf>,
    pub guid: HashMap<String, PieceDef>,
    bounds_cache: Mutex<HashMap<String, Option<Aabb>>>,
    geom_cache: Mutex<HashMap<String, Option<Arc<Mesh>>>>,
}

impl Context {
    /// mod-over-base overlay: `fallback_dir` = the base install MEDIA; the
    /// mod's own MEDIA wins for both LEVELSETS defs and mesh resolution.
    pub fn new(media_dir: &Path, fallback_dir: Option<&Path>) -> Self {
        let guid = match fallback_dir {
            Some(fb) => {
                let mut g = load_all_levelsets(&fb.join("LEVELSETS"));
                g.extend(load_all_levelsets(&media_dir.join("LEVELSETS")));
                g
            }
            None => load_all_levelsets(&media_dir.join("LEVELSETS")),
        };
        Context {
            media_dir: media_dir.to_path_buf(),
            fallback_dir: fallback_dir.map(|p| p.to_path_buf()),
            guid,
            bounds_cache: Mutex::new(HashMap::new()),
            geom_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Collision-mesh M_MESH_BOUNDS (shared across tiles; pure per-file).
    pub fn get_bounds(&self, rel: &str) -> Option<Aabb> {
        if let Some(v) = self.bounds_cache.lock().unwrap().get(rel) {
            return *v;
        }
        let b = resolve_mesh_path(&self.media_dir, rel, self.fallback_dir.as_deref())
            .and_then(|p| load_mesh_file(&p).ok())
            .and_then(|m| if m.bounds.null { None } else { Some(m.bounds) });
        self.bounds_cache.lock().unwrap().insert(rel.to_string(), b);
        b
    }

    /// Full collision-mesh geometry (shared, read-only).
    pub fn get_mesh(&self, rel: &str) -> Option<Arc<Mesh>> {
        if let Some(v) = self.geom_cache.lock().unwrap().get(rel) {
            return v.clone();
        }
        let m = resolve_mesh_path(&self.media_dir, rel, self.fallback_dir.as_deref())
            .and_then(|p| load_mesh_file(&p).ok())
            .map(Arc::new);
        self.geom_cache.lock().unwrap().insert(rel.to_string(), m.clone());
        m
    }
}

pub struct HeaderInfo {
    pub grid_w: i32,
    pub grid_h: i32,
    pub world_ext_x: f32,
    pub world_ext_z: f32,
    pub bounds_x: f32,
    pub bounds_z: f32,
    pub origin_x: f64,
    pub origin_z: f64,
    pub bx: Aabb,
    pub region: RegionResult,
}

/// Region AABB (X,Z), snapped outward; geometry-free layouts get the default
/// 20x20 region at the origin.
pub fn compute_region(layout: &Layout, ctx: &Context, snap: f32) -> (Aabb, RegionResult) {
    let res = compute_region_aabb_collision(layout, ctx);
    let bx = if res.aabb.null {
        let h = DEFAULT_PORTAL_HALF;
        Aabb::new(Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h))
    } else if snap != 0.0 {
        snap_box_outward(&res.aabb, snap)
    } else {
        res.aabb
    };
    (bx, res)
}

pub fn generate_header(layout: &Layout, ctx: &Context) -> HeaderInfo {
    let (bx, region) = compute_region(layout, ctx, SNAP);
    let we = Vec3::new(0.0, 0.0, 0.0);
    let (ox, oz) = grid_origin(bx.min);
    let (gw, gh) = grid_dims(bx.min, bx.max, ox, oz);
    let (ww, wd, bw, bd) = header_floats(bx.min, bx.max, we);
    HeaderInfo {
        grid_w: gw,
        grid_h: gh,
        world_ext_x: ww,
        world_ext_z: wd,
        bounds_x: bw,
        bounds_z: bd,
        origin_x: ox,
        origin_z: oz,
        bx,
        region,
    }
}

/// Full .mpp content (24-byte header + grid body) for one .LAYOUT.
pub fn generate_mpp_bytes(layout_path: &Path, ctx: &Context) -> Result<Vec<u8>, String> {
    let layout = load_layout_file(layout_path).map_err(|e| format!("{}: {e}", layout_path.display()))?;
    let hdr = generate_header(&layout, ctx);
    if hdr.grid_w <= 0 || hdr.grid_h <= 0 {
        return Err(format!(
            "{}: degenerate grid {}x{}",
            layout_path.display(),
            hdr.grid_w,
            hdr.grid_h
        ));
    }
    let cells = hdr.grid_w as u64 * hdr.grid_h as u64;
    if cells > 512_000_000 {
        return Err(format!(
            "{}: grid too large {}x{}",
            layout_path.display(),
            hdr.grid_w,
            hdr.grid_h
        ));
    }
    // assemble_links defaults ON (matches pipeline._generate_mpp).
    let grid = super::native::build_grid_fast(
        &layout,
        ctx,
        hdr.bx.min,
        hdr.origin_x,
        hdr.origin_z,
        hdr.grid_w,
        hdr.grid_h,
        true,
    );
    let mut out = pack_header(
        hdr.grid_w,
        hdr.grid_h,
        hdr.world_ext_x,
        hdr.world_ext_z,
        hdr.bounds_x,
        hdr.bounds_z,
    );
    out.extend_from_slice(&grid);
    Ok(out)
}
