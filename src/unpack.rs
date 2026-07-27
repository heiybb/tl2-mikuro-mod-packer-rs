//! `.MOD` / `DATA.PAK` → editable source tree — the inverse of the whole
//! packer, and the counterpart of `packer::pack_mod`.
//!
//! A `.MOD` is not a plain archive of the modder's files: the compilers ran
//! first, and the container stores the COMPILED bytes under the SOURCE name
//! (`FOO.DAT` holds BINDAT, `FOO.LAYOUT` holds BINLAYOUT — see the comment on
//! `packer::PACK_EXCLUDE_SUFFIXES`). Unpacking therefore has to compile
//! backwards, or the tree it produces would be full of files that look like
//! sources and are not:
//!
//! ```text
//! .MOD ─┬─ header      → MOD.DAT          (packer::build_header, inverted)
//!       ├─ *.DAT       → bindat_decompile → UTF-16LE text .DAT
//!       ├─ *.LAYOUT    → binlayout_decompile → UTF-16LE text .LAYOUT
//!       └─ everything else                → written verbatim
//! ```
//!
//! Repacking the result reproduces the original: every file's content, CRC and
//! FILETIME come back identical (the decompilers are fixed-point-verified
//! against the shipped corpus, and the manifest timestamps are restored — even
//! the FILETIME 0 the packer stamps on a file it synthesized). Three things do
//! not survive the FIRST cycle, all of them inherent rather than lossy:
//!   * `gameVersion` — the packer reads it from the installed `Torchlight2.exe`
//!     rather than from MOD.DAT; export `TL2_MOD_GAMEVER` to pin it.
//!   * the readable name of a DAT key the embedded dictionary cannot reverse —
//!     it decompiles to the `UNK_<HEX8>` escape, which compiles back to the
//!     SAME hash (so the game still reads it) but reads as a placeholder.
//!   * the manifest ORDER of files the pack generated (`MEDIA/*.RAW`): they had
//!     no source file, so the packer appended them after the disk scan, whereas
//!     unpacking materialises them and the next scan sorts them in place. The
//!     record set, the bytes and both validated hashes are unaffected, and
//!     unpack→repack is byte-identical from the second cycle on.
//!
//! One more caveat on the restored timestamps: `packer::filetime_of` converts an
//! mtime through `f64` seconds, which cannot hold a full FILETIME (57 bits of
//! ticks vs 53 bits of mantissa), so a repack can read a restored stamp back
//! ±2 ticks (200 ns) off. Harmless — the field is informational — but it does
//! mean a mod whose files carry sub-microsecond mtimes will not repack to the
//! identical byte string.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bindat_decompile;
use crate::binlayout_decompile;
use crate::pak::{Entry, PakReader};
use crate::packer::Header;

/// Extensions whose container payload is a compiled BINDAT (the packer's
/// `content_for` list).
const DAT_SOURCES: [&str; 4] = [".DAT", ".TEMPLATE", ".ANIMATION", ".HIE"];

pub struct Options {
    /// Write container payloads verbatim instead of decompiling them, so the
    /// tree mirrors the archive byte for byte (`.DAT` files then hold BINDAT).
    pub raw: bool,
    /// Restore each file's modification time from the manifest FILETIME.
    pub restore_times: bool,
    /// Write MOD.DAT from the container header (`.MOD` only).
    pub write_mod_dat: bool,
    /// Extract only entries whose manifest path contains this (case-insensitive)
    /// substring. `None` extracts everything.
    pub filter: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { raw: false, restore_times: true, write_mod_dat: true, filter: None }
    }
}

#[derive(Default)]
pub struct Report {
    /// Files written.
    pub files: usize,
    /// Of those, BINDAT decompiled back to text.
    pub dats: usize,
    /// Of those, BINLAYOUT decompiled back to text.
    pub layouts: usize,
    /// Of those, copied through untouched.
    pub copied: usize,
    /// Total uncompressed bytes written.
    pub bytes: u64,
    /// `UNK_<HEX8>` placeholders emitted, with the files that produced them.
    pub unknown_names: BTreeSet<String>,
    /// Entries the original pack SYNTHESIZED rather than read from disk (a
    /// FILETIME of 0 is the packer's marker) — the generated `.RAW` indexes.
    /// They are written out like everything else; this just names them, since
    /// the next pack rebuilds them from the DATs regardless.
    pub generated: Vec<String>,
    /// Non-fatal problems (a payload that would not decompile and was written
    /// verbatim, a header field a repack cannot reproduce, …).
    pub warnings: Vec<String>,
}

/// Unpack a `.MOD` into `out_dir` (which becomes a directory the packer accepts
/// as a mod source: `MOD.DAT` + `MEDIA/…`).
pub fn unpack_mod(mod_path: &Path, out_dir: &Path, opt: &Options) -> Result<Report, String> {
    unpack_mod_with_progress(mod_path, out_dir, opt, |_, _| {})
}

/// `unpack_mod`, calling `progress(done, total)` after each file — an 11k-file
/// mod takes long enough that a silent run looks hung.
pub fn unpack_mod_with_progress(
    mod_path: &Path,
    out_dir: &Path,
    opt: &Options,
    progress: impl FnMut(usize, usize),
) -> Result<Report, String> {
    let mut r = PakReader::open_mod(mod_path)?;
    let mut report = Report::default();
    if opt.write_mod_dat {
        if let Some(h) = r.header() {
            let stem = mod_path
                .file_stem()
                .map(|s| strip_pack_timestamp(&s.to_string_lossy()))
                .unwrap_or_else(|| h.title.clone());
            let text = mod_dat_text(h, &stem);
            write_file(&out_dir.join("MOD.DAT"), &to_utf16le_bom(&text), None)?;
            check_header_fidelity(h, &mut report);
        }
    }
    unpack_entries(&mut r, out_dir, opt, &mut report, progress)?;
    Ok(report)
}

/// Unpack a base `DATA.PAK` (+ its sibling `.MAN`) the same way. There is no
/// mod header, so no MOD.DAT is written.
pub fn unpack_pak(
    pak_path: &Path,
    out_dir: &Path,
    opt: &Options,
    progress: impl FnMut(usize, usize),
) -> Result<Report, String> {
    let mut r = PakReader::open_pak(pak_path)?;
    let mut report = Report::default();
    unpack_entries(&mut r, out_dir, opt, &mut report, progress)?;
    Ok(report)
}

/// Extract every (selected) entry, decompiling as it goes.
pub fn unpack_entries(
    r: &mut PakReader,
    out_dir: &Path,
    opt: &Options,
    report: &mut Report,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), String> {
    let want = opt.filter.as_ref().map(|f| f.to_uppercase());
    let entries: Vec<Entry> = r
        .entries()
        .iter()
        .filter(|e| match &want {
            Some(f) => e.path.to_uppercase().contains(f.as_str()),
            None => true,
        })
        .cloned()
        .collect();
    let total = entries.len();
    for (i, e) in entries.iter().enumerate() {
        // Manifest paths are '/'-separated and already rooted at MEDIA/, which
        // is exactly the layout a mod source directory wants.
        let payload = r.read_entry(e)?;
        let c = convert(&e.path, payload, opt, report);
        let rel = sanitize_rel(&format!("{}{}", e.path, c.suffix))?;
        let dst = out_dir.join(rel);
        match c.kind {
            Kind::Dat => report.dats += 1,
            Kind::Layout => report.layouts += 1,
            Kind::Copy => report.copied += 1,
        }
        report.files += 1;
        report.bytes += c.bytes.len() as u64;
        if e.ftime == 0 {
            report.generated.push(e.path.clone());
        }
        write_file(&dst, &c.bytes, opt.restore_times.then_some(e.ftime))?;
        progress(i + 1, total);
    }
    Ok(())
}

enum Kind {
    Dat,
    Layout,
    Copy,
}

/// The result of converting one entry: the bytes to write, the file name to
/// write them under (relative to the entry's directory), and what happened.
struct Converted {
    bytes: Vec<u8>,
    /// Suffix appended to the entry name — empty for the normal case,
    /// `.BINDAT` / `.BINLAYOUT` when the payload could not be decompiled.
    suffix: &'static str,
    kind: Kind,
}

/// Decompile one entry's payload back to its source form.
///
/// A payload that will not decompile is written under its COMPILED name
/// (`FOO.LAYOUT.BINLAYOUT`) rather than under `FOO.LAYOUT`. That looks like the
/// timid choice and is the opposite: `packer::content_for` compiles whatever
/// sits at `FOO.LAYOUT`, and "compiling" a binary blob as layout text yields an
/// EMPTY 8-byte layout — the repack would silently replace the file with
/// nothing. Parking the bytes under the compiled name keeps them intact and
/// visible, and the warning says what it costs (a repack drops the entry until
/// a real source is supplied).
fn convert(path: &str, payload: Vec<u8>, opt: &Options, report: &mut Report) -> Converted {
    let copy = |bytes: Vec<u8>| Converted { bytes, suffix: "", kind: Kind::Copy };
    if opt.raw {
        return copy(payload);
    }
    let up = path.to_uppercase();
    if DAT_SOURCES.iter().any(|s| up.ends_with(s)) && !is_text(&payload) {
        // A mod's own section name is usually the file's basename (and sometimes
        // its directory) — hashes the embedded dictionary cannot know but the
        // path can supply. GUTS recovers these from its runtime string interner;
        // this is the same information, taken from where it is still written down.
        let hints = path_name_hints(&up);
        let hint_refs: Vec<&str> = hints.iter().map(String::as_str).collect();
        match bindat_decompile::decompile_bindat_hinted(&payload, &hint_refs) {
            Ok(d) => {
                for n in &d.unknown_names {
                    report.unknown_names.insert(format!("{n}  ({path})"));
                }
                return Converted {
                    bytes: bindat_decompile::to_dat_bytes(&d.text),
                    suffix: "",
                    kind: Kind::Dat,
                };
            }
            Err(e) => {
                report.warnings.push(format!(
                    "{path}: will not decompile ({e}); kept as {path}.BINDAT — a repack \
                     will NOT include this entry until a text source exists"
                ));
                return Converted { bytes: payload, suffix: ".BINDAT", kind: Kind::Copy };
            }
        }
    } else if up.ends_with(".LAYOUT") && !is_text(&payload) {
        // The compiler hardcodes FLAG (byte 1) to 0x04, which is what the whole
        // shipped corpus uses; a mod built by some other tool can carry another
        // value that a repack would normalise away. Say so rather than let one
        // byte change silently.
        if payload.get(1) != Some(&0x04) {
            report.warnings.push(format!(
                "{path}: BINLAYOUT flag byte is 0x{:02X}, not the usual 0x04 — a repack \
                 writes 0x04",
                payload.get(1).copied().unwrap_or(0)
            ));
        }
        match binlayout_decompile::decompile_binlayout(&payload) {
            Ok(text) => {
                return Converted {
                    bytes: binlayout_decompile::to_layout_bytes(&text),
                    suffix: "",
                    kind: Kind::Layout,
                }
            }
            Err(e) => {
                report.warnings.push(format!(
                    "{path}: will not decompile ({e}); kept as {path}.BINLAYOUT — a repack \
                     will NOT include this entry until a text source exists"
                ));
                return Converted { bytes: payload, suffix: ".BINLAYOUT", kind: Kind::Copy };
            }
        }
    }
    copy(payload)
}

/// Candidate section/key names taken from an entry's own path: the file stem
/// (`UNITTYPES/MERCHANT_PETS.DAT` → `MERCHANT_PETS`) and its parent directory.
/// Wrong guesses cost nothing — a hint is only consulted when its `rg_hash`
/// matches the hash actually stored in the file.
fn path_name_hints(upper_path: &str) -> Vec<String> {
    let mut parts: Vec<&str> = upper_path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::new();
    if let Some(file) = parts.pop() {
        let stem = file.split('.').next().unwrap_or(file);
        if !stem.is_empty() {
            out.push(stem.to_string());
        }
    }
    if let Some(dir) = parts.pop() {
        out.push(dir.to_string());
    }
    out
}

/// True when the payload is already a TEXT source rather than a compiled blob.
///
/// Deciding by "does it start with a BOM" rather than by "does it look like
/// BINDAT/BINLAYOUT" is the safe direction: every text `.DAT`/`.LAYOUT` the
/// tools read carries the UTF-16LE BOM (`packer::read_mod_dat_text`,
/// `binlayout::read_text`), whereas a compiled payload can vary — this crate's
/// own flag byte assumption already missed a mod whose BINLAYOUT flag was
/// 0x36, silently passing the binary through as if it were a source.
fn is_text(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xEF, 0xBB, 0xBF]) || b.starts_with(b"[")
}

/// Reject anything that would escape `out_dir` — a manifest is untrusted input,
/// and `..` or a drive letter in a record name must not become a write outside
/// the target directory.
fn sanitize_rel(path: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') || part.contains('\\') {
            return Err(format!("refusing unsafe manifest path {path:?}"));
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(format!("empty manifest path {path:?}"));
    }
    Ok(out)
}

fn write_file(dst: &Path, data: &[u8], ftime: Option<u64>) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(dst, data).map_err(|e| format!("{}: {e}", dst.display()))?;
    if let Some(ft) = ftime {
        if let Some(t) = filetime_to_system(ft) {
            // Best-effort: a filesystem that cannot store the time is not a
            // reason to fail the unpack. (On WASI this is where the synthesized
            // FILETIME 0 → 1601 stamp is dropped — `filestat_set_times` takes
            // UNSIGNED nanoseconds since 1970, so a pre-epoch time has no
            // representation there and those files keep the host's clock.)
            if let Ok(f) = std::fs::File::options().write(true).open(dst) {
                let _ = f.set_modified(t);
            }
        }
    }
    Ok(())
}

/// Windows FILETIME (100 ns ticks since 1601-01-01) → `SystemTime`.
///
/// Pre-1970 stamps are restored too, and deliberately so: the packer records
/// FILETIME 0 for a file it SYNTHESIZED (a generated `.RAW` never existed on
/// disk, so `filetime_of` had nothing to read). Writing that back as
/// 1601-01-01 is what makes a repack of the unpacked tree reproduce the
/// original manifest — `filetime_of` maps any pre-1970 mtime back to 0.
fn filetime_to_system(ft: u64) -> Option<std::time::SystemTime> {
    const EPOCH_DELTA: u64 = 116_444_736_000_000_000;
    // Win32 SetFileTime reads an all-zero FILETIME as "leave this timestamp
    // alone", so a literal 0 cannot be written. One tick later is the same
    // thing for our purpose — `filetime_of` maps ANY pre-1970 mtime back to 0.
    let ft = ft.max(1);
    let d = std::time::Duration::from_nanos(ft.abs_diff(EPOCH_DELTA).checked_mul(100)?);
    if ft >= EPOCH_DELTA {
        std::time::UNIX_EPOCH.checked_add(d)
    } else {
        std::time::UNIX_EPOCH.checked_sub(d)
    }
}

// ── MOD.DAT reconstruction ─────────────────────────────────────────────────

fn utf16_field(out: &mut String, tag: &str, key: &str, value: &str) {
    out.push_str(&format!("\t<{tag}>{key}:{value}\r\n"));
}

/// Rebuild the MOD.DAT `packer::build_header` would have read. Field order
/// matters: `parse_mod_dat` takes the FIRST occurrence of each key across the
/// whole file, so the top-level scalars must precede the REQUIRED_MODS block
/// (whose sub-blocks carry their own NAME / VERSION).
pub fn mod_dat_text(h: &Header, mod_file_name: &str) -> String {
    let mut s = String::from("\u{FEFF}[MOD]\r\n");
    utf16_field(&mut s, "STRING", "NAME", &h.title);
    utf16_field(&mut s, "STRING", "AUTHOR", &h.author);
    utf16_field(&mut s, "STRING", "DESCRIPTION", &h.descr);
    utf16_field(&mut s, "INTEGER64", "MOD_ID", &(h.modid as i64).to_string());
    utf16_field(&mut s, "STRING", "WEBSITE", &h.website);
    if !h.download.is_empty() {
        utf16_field(&mut s, "STRING", "DOWNLOAD_URL", &h.download);
    }
    utf16_field(&mut s, "STRING", "MOD_FILE_NAME", mod_file_name);
    // The header stores version+1 (`build_header`), so undo that here.
    utf16_field(&mut s, "INTEGER", "VERSION", &(h.modver as i64 - 1).to_string());
    if !h.reqs.is_empty() {
        s.push_str("\t[REQUIRED_MODS]\r\n");
        for (name, id, ver) in &h.reqs {
            s.push_str("\t\t[MOD]\r\n\t");
            utf16_field(&mut s, "STRING", "NAME", name);
            s.push('\t');
            utf16_field(&mut s, "INTEGER64", "MOD_ID", &(*id as i64).to_string());
            s.push('\t');
            utf16_field(&mut s, "INTEGER", "VERSION", &ver.to_string());
            s.push_str("\t\t[/MOD]\r\n");
        }
        s.push_str("\t[/REQUIRED_MODS]\r\n");
    }
    if !h.dels.is_empty() {
        s.push_str("\t[REMOVE_FILES]\r\n");
        for d in &h.dels {
            s.push('\t');
            utf16_field(&mut s, "STRING", "FILE", d);
        }
        s.push_str("\t[/REMOVE_FILES]\r\n");
    }
    s.push_str("[/MOD]\r\n");
    s
}

/// Recover `MOD_FILE_NAME` from a `.MOD` file name. A non-`--deploy` pack names
/// its output `<MOD_FILE_NAME>_YYYYMMDD_HHMMSS.MOD`, so drop that suffix when it
/// is present — otherwise every unpack→repack cycle would stack another stamp.
fn strip_pack_timestamp(stem: &str) -> String {
    let b = stem.as_bytes();
    // …_<8 digits>_<6 digits>
    if b.len() > 16 && b[b.len() - 16] == b'_' && b[b.len() - 7] == b'_' {
        let d1 = &stem[b.len() - 15..b.len() - 7];
        let d2 = &stem[b.len() - 6..];
        if d1.bytes().all(|c| c.is_ascii_digit()) && d2.bytes().all(|c| c.is_ascii_digit()) {
            return stem[..b.len() - 16].to_string();
        }
    }
    stem.to_string()
}

fn to_utf16le_bom(text: &str) -> Vec<u8> {
    // The BOM is part of `text`, so plain encoding produces the FF FE prefix.
    let mut out = Vec::with_capacity(text.len() * 2);
    for u in text.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// Flag header fields a repack of the unpacked tree cannot reproduce, so the
/// user learns it here rather than from a byte diff later.
fn check_header_fidelity(h: &Header, report: &mut Report) {
    if h.ver != 4 {
        report
            .warnings
            .push(format!("container version is {} (the packer always writes 4)", h.ver));
    }
    if h.flags != 0 {
        report
            .warnings
            .push(format!("header flags are 0x{:X} (the packer always writes 0)", h.flags));
    }
    report.warnings.push(format!(
        "gameVersion 0x{:016X} is not stored in MOD.DAT — a repack reads it from the \
         installed Torchlight2.exe; export TL2_MOD_GAMEVER=0x{:016X} to pin it",
        h.gamever, h.gamever
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packer::{build_header, pack_mod, Overrides};

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, to_utf16le_bom(&format!("\u{FEFF}{s}"))).unwrap();
    }

    /// Pack a tiny mod, unpack it, and repack the result: the two .MOD files
    /// must be byte-identical. That covers the header → MOD.DAT → header loop,
    /// both decompilers, the manifest order, and the restored FILETIMEs.
    #[test]
    fn pack_unpack_repack_is_byte_identical() {
        let tmp = std::env::temp_dir().join(format!("tl2unpack_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        write(
            &src.join("MOD.DAT"),
            "[MOD]\r\n\t<STRING>NAME:Test Mod\r\n\t<STRING>AUTHOR:Mikuro\r\n\
             \t<STRING>DESCRIPTION:round trip\r\n\t<INTEGER64>MOD_ID:1234567890123\r\n\
             \t<STRING>WEBSITE:https://example.invalid/\r\n\t<STRING>MOD_FILE_NAME:TESTMOD\r\n\
             \t<INTEGER>VERSION:7\r\n[/MOD]\r\n",
        );
        write(
            &src.join("MEDIA/UNITS/ITEMS/RING.DAT"),
            "[UNIT]\r\n<STRING>NAME:Ring\r\n<INTEGER>LEVEL:5\r\n<FLOAT>SPEED:1.25\r\n[/UNIT]\r\n",
        );
        write(
            &src.join("MEDIA/LAYOUTS/T/T.LAYOUT"),
            "[Layout]\r\n\t[OBJECTS]\r\n\t\t[BASEOBJECT]\r\n\t\t\t[PROPERTIES]\r\n\
             \t\t\t\t<STRING>DESCRIPTOR:Group\r\n\t\t\t\t<STRING>NAME:Root\r\n\
             \t\t\t\t<INTEGER64>PARENTID:-1\r\n\t\t\t\t<INTEGER64>ID:99\r\n\
             \t\t\t\t<STRING>CHOICE:Weight\r\n\t\t\t\t<UNSIGNED INT>RANDOMIZATION:40\r\n\
             \t\t\t[/PROPERTIES]\r\n\t\t[/BASEOBJECT]\r\n\t[/OBJECTS]\r\n[/Layout]\r\n",
        );
        // A non-compiled payload rides through verbatim.
        std::fs::write(src.join("MEDIA/UNITS/ITEMS/ICON.PNG"), b"\x89PNG\r\n\x1a\nnot-really").unwrap();

        // Pack (in-memory conversion, no RAW/MPP passes — nothing to index).
        let media = src.join("MEDIA");
        let mut ov = Overrides::new();
        crate::packer::convert_all(&media, Some(&mut ov), "none", "none");
        let mod_a = tmp.join("a.MOD");
        pack_mod(&media, &mod_a, Some(&src), Some(&ov)).expect("pack A");

        // Unpack, then repack from the unpacked tree.
        let out = tmp.join("out");
        let gamever = build_header(&src, None).gamever;
        let rep = unpack_mod(&mod_a, &out, &Options::default()).expect("unpack");
        assert_eq!(rep.dats, 1, "the .DAT decompiled");
        assert_eq!(rep.layouts, 1, "the .LAYOUT decompiled");
        assert_eq!(rep.copied, 1, "the .PNG rode through");
        assert!(rep.unknown_names.is_empty(), "no unnameable keys: {:?}", rep.unknown_names);

        // The unpacked sources must be TEXT again, not the compiled payloads.
        let dat = std::fs::read(out.join("MEDIA/UNITS/ITEMS/RING.DAT")).unwrap();
        assert_eq!(&dat[..2], &[0xFF, 0xFE], "unpacked .DAT is UTF-16LE text");

        let media_b = out.join("MEDIA");
        let mut ov_b = Overrides::new();
        crate::packer::convert_all(&media_b, Some(&mut ov_b), "none", "none");
        let mod_b = tmp.join("b.MOD");
        // Pin gameVersion so the comparison does not depend on the machine.
        std::env::set_var("TL2_MOD_GAMEVER", format!("0x{gamever:016X}"));
        pack_mod(&media_b, &mod_b, Some(&out), Some(&ov_b)).expect("pack B");
        std::env::remove_var("TL2_MOD_GAMEVER");

        let a = std::fs::read(&mod_a).unwrap();
        let b = std::fs::read(&mod_b).unwrap();
        assert_eq!(
            a.len(),
            b.len(),
            "repacked .MOD size differs ({} vs {})",
            a.len(),
            b.len()
        );
        let diff = a.iter().zip(&b).position(|(x, y)| x != y);
        assert_eq!(diff, None, "repacked .MOD differs at byte {diff:?}");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn strips_only_a_real_pack_timestamp() {
        assert_eq!(strip_pack_timestamp("MYMOD_20260726_165407"), "MYMOD");
        assert_eq!(strip_pack_timestamp("挑战者大陆--组件10_20260726_165407"), "挑战者大陆--组件10");
        // Not a stamp: wrong digit counts, or a name that just ends in numbers.
        assert_eq!(strip_pack_timestamp("MYMOD_2026072_165407"), "MYMOD_2026072_165407");
        assert_eq!(strip_pack_timestamp("MOD_VERSION_2"), "MOD_VERSION_2");
        assert_eq!(strip_pack_timestamp("SHORT"), "SHORT");
    }

    /// FILETIME 0 marks a file the packer SYNTHESIZED. It must survive the
    /// round-trip as 0, or every generated .RAW would come back stamped "now"
    /// and the repacked manifest would differ.
    #[test]
    fn synthesized_filetime_zero_roundtrips() {
        let t = filetime_to_system(0).expect("1601 is representable");
        assert!(t < std::time::UNIX_EPOCH, "FILETIME 0 predates the unix epoch");
        // Anything before 1970 makes packer::filetime_of yield 0 again.
        assert!(t.duration_since(std::time::UNIX_EPOCH).is_err());
    }

    /// A manifest is untrusted input; a record that walks out of the target
    /// directory must be refused rather than written.
    #[test]
    fn rejects_path_traversal() {
        assert!(sanitize_rel("MEDIA/../../evil.dat").is_err());
        assert!(sanitize_rel("C:/windows/system32/x.dll").is_err());
        assert!(sanitize_rel("MEDIA\\UNITS\\X.DAT").is_err());
        assert_eq!(
            sanitize_rel("MEDIA/UNITS/X.DAT").unwrap(),
            PathBuf::from("MEDIA").join("UNITS").join("X.DAT")
        );
    }
}
