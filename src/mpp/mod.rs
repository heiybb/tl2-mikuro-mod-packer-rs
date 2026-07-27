//! .MPP pathing-grid generation (port of the mikuro_mod_packer.mpp package):
//! a clean reimplementation of TL2's pathing bake, grounded in the RE of
//! EditorGuts.dll. ≈99.7% cell-accurate — a known, accepted ceiling; the
//! remainder is cliff/overhang float tie-breaks and nocollide cave walls, which
//! are not reproducible offline.
//!
//! The ceiling is cosmetic where it matters: measured with 1 (wall) and 255
//! (void) treated alike — both are impassable to the pathfinder — walkability
//! agrees with native to the float floor. That is why this is the only backend;
//! the one that drove the real editor for byte-exact grids is gone.

pub mod dat;
pub mod geom;
pub mod layout;
pub mod native;
pub mod ogre_mesh;
pub mod pipeline;
pub mod region;

use std::path::Path;

/// Compile a single .LAYOUT to its .mpp content (24-byte header + grid body)
/// with the OFFLINE backend. Reuse `ctx` across a batch to amortize the
/// LEVELSETS + mesh parsing.
pub fn compile_mpp(layout_path: &Path, ctx: &pipeline::Context) -> Result<Vec<u8>, String> {
    pipeline::generate_mpp_bytes(layout_path, ctx)
}
