//! .MPP pathing-grid generation (port of the mikuro_mod_packer.mpp package):
//! a clean reimplementation of TL2's pathing bake, grounded in the RE of
//! EditorGuts.dll. ≈99.7% cell-accurate — a known, accepted ceiling; the
//! remainder is cliff/overhang float tie-breaks and nocollide cave walls, which
//! are not reproducible offline.
//!
//! This is the ONLY backend. There used to be a `dll` one that shelled out to
//! `TL2-Mikuro-Console.exe` for byte-exact grids; it is gone (see
//! `packer::MPP_DLL_REMOVED` for how to run the console host directly), because
//! wrapping it meant staging the whole mod into the editor's own mods folder —
//! over anything already there.

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
