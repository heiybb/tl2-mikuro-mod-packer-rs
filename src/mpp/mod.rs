//! .MPP pathing-grid generation (port of the mikuro_mod_packer.mpp package):
//! a clean reimplementation of TL2's pathing bake, grounded in the RE of
//! EditorGuts.dll. Offline backend ≈99.7% cell-accurate (a known, accepted
//! ceiling); the `dll` backend drives the real editor for byte-exact output.

pub mod dat;
pub mod dll;
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
