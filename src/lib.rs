//! mikuro-mod-rs — Torchlight 2 .MOD builder (Rust port of mikuro_mod_packer).
//!
//! Compiles MEDIA sources to binary and packs them into a .MOD container,
//! mirroring EditorGuts' CreateMod. All three binary formats (BINDAT /
//! BINLAYOUT / MPP) are real from-scratch compilers: parse text → serialize
//! binary, reading NO existing binary — so text edits compile correctly.
//!
//! Modules mirror the Python package one-to-one:
//!   bindat     DAT    -> BINDAT    (per-file hash string-ids)
//!   binlayout  LAYOUT -> BINLAYOUT (pure-DLL schema, embedded JSON)
//!   raw        the 7 RAW index builders
//!   rghash     Runic-Games hash (== EditorGuts sub_100CA9A0)
//!   packer     .MOD container writer + orchestration (convert_all / pack_mod)
//!   mpp        .MPP pathing grids: offline classifier + byte-exact DLL driver

pub mod bindat;
pub mod binlayout;
pub mod mpp;
pub mod packer;
pub mod raw;
pub mod rghash;
pub mod text;
