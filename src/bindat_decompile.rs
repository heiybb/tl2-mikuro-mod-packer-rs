//! BINDAT → text `.DAT` decompiler — the inverse of `bindat::compile_tree`.
//!
//! A BINDAT keeps property VALUES verbatim (string values in its string table,
//! numbers/bools inline), but stores every KEY and SECTION name only as its
//! one-way `rghash`. Values are therefore fully recoverable; keys/section names
//! are recovered through an embedded reverse dictionary — every distinct DAT
//! key/section name in the shipped game (1678 of them, hashed with ZERO
//! collisions, ~23 KB). A hash with no dictionary entry (a mod's brand-new key)
//! is emitted as `UNK_<hex>`, which still round-trips structurally.
//!
//! Used to reconstruct base `LEVELSETS` piece tables from `DATA.PAK` (whose
//! entries are compiled) so the offline MPP pipeline — which parses TEXT DATs —
//! can regenerate pathing without a loose MEDIA extraction; and as the basis
//! for unpacking a `.MOD` back to editable sources.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use crate::rghash::rg_hash_str;

/// hash → key/section name, built once from the embedded dictionary.
static REVERSE: LazyLock<HashMap<u32, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for line in include_str!("../data/dat_keys.txt").lines() {
        let name = line.trim_end_matches('\r');
        if !name.is_empty() {
            m.entry(rg_hash_str(name)).or_insert(name);
        }
    }
    m
});

/// Resolve a name hash to text, falling back to a stable `UNK_<hex>` token
/// (valid as both a section name and a key for the DAT parsers).
fn name_of(hash: u32) -> String {
    REVERSE
        .get(&hash)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("UNK_{hash:08X}"))
}

/// `name_of`, but first consulting per-file HINTS, and recording the
/// placeholder when neither can name the hash — so an unpack can tell the user
/// WHICH names it could not recover.
///
/// The hints matter more than they look: a mod's own section name is very often
/// the file's own basename (`UNITTYPES/MERCHANT_PETS.DAT` opens `[MERCHANT_PETS]`),
/// which the embedded dictionary cannot know but the caller can simply hand over.
/// That is also how GUTS gets those names back — its interner has seen every
/// string the editor ever loaded.
fn name_of_hinted(hash: u32, hints: &HashMap<u32, String>, unknown: &mut BTreeSet<String>) -> String {
    if let Some(n) = REVERSE.get(&hash) {
        return n.to_string();
    }
    if let Some(n) = hints.get(&hash) {
        return n.clone();
    }
    let n = format!("UNK_{hash:08X}");
    unknown.insert(n.clone());
    n
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn u16(&mut self) -> Result<u16, String> {
        let s = self.b.get(self.p..self.p + 2).ok_or("bindat: eof u16")?;
        self.p += 2;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.p..self.p + 4).ok_or("bindat: eof u32")?;
        self.p += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i64(&mut self) -> Result<i64, String> {
        let s = self.b.get(self.p..self.p + 8).ok_or("bindat: eof i64")?;
        self.p += 8;
        Ok(i64::from_le_bytes(s.try_into().unwrap()))
    }
    fn utf16(&mut self, units: usize) -> Result<String, String> {
        let s = self.b.get(self.p..self.p + 2 * units).ok_or("bindat: eof str")?;
        self.p += 2 * units;
        let u: Vec<u16> = s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(String::from_utf16_lossy(&u))
    }
}

/// The type tag emitted for a stored type code (inverse of `bindat::type_code`,
/// which normalises by removing spaces — so the spelling here is free to match
/// what the shipped sources and GUTS' own unpacker write).
fn type_tag(tc: u32) -> &'static str {
    match tc {
        1 => "INTEGER",
        2 => "FLOAT",
        4 => "UNSIGNED INT",
        5 => "STRING",
        6 => "BOOL",
        7 => "INTEGER64",
        8 => "TRANSLATE",
        _ => "STRING",
    }
}

/// Format a float the way TL2 DATs do: the shortest decimal that round-trips,
/// with NO forced `.0` on whole values — `<FLOAT>DURATION:0` is what both the
/// shipped sources and GUTS' own unpacker write, and the `<FLOAT>` tag is what
/// makes the value a float on recompile, not the decimal point.
///
/// One deliberate divergence from GUTS: it prints large values through MSVC's
/// `%g` (`1.9108e+006`), which keeps only 6 significant digits and therefore
/// LOSES precision — GUTS' own unpack→repack does not reproduce such a value.
/// Shortest-round-trip always re-reads as the identical f32, so we keep it and
/// accept the textual difference.
/// Float spelling is shared with the BINLAYOUT decompiler — see
/// [`crate::text::fmt_f32_source`]. Shipped `.DAT` sources follow the same `%g`
/// notation rule as layouts do; they just rarely leave the plain range (the only
/// two out-of-range values in the whole corpus are written `1e-005`).
use crate::text::fmt_f32_source as fmt_f32;

fn emit_node(
    c: &mut Cursor,
    strs: &HashMap<u32, String>,
    out: &mut String,
    unknown: &mut BTreeSet<String>,
    hints: &HashMap<u32, String>,
    depth: usize,
) -> Result<(), String> {
    let name_hash = c.u32()?;
    let nprops = c.u32()?;
    let name = name_of_hinted(name_hash, hints, unknown);
    // Tab-indent by nesting depth — that is what the shipped .DAT sources and
    // GUTS' own unpacker emit, so a decompiled file reads (and diffs) like one.
    let indent = |out: &mut String, d: usize| {
        for _ in 0..d {
            out.push('\t');
        }
    };
    indent(out, depth);
    out.push('[');
    out.push_str(&name);
    out.push_str("]\r\n");
    for _ in 0..nprops {
        indent(out, depth + 1);
        let key_hash = c.u32()?;
        let tc = c.u32()?;
        let value = match tc {
            5 | 8 => {
                let sid = c.u32()?;
                strs.get(&sid).cloned().unwrap_or_default()
            }
            6 => {
                let v = c.u32()?;
                if v != 0 { "true".to_string() } else { "false".to_string() }
            }
            2 => fmt_f32(f32::from_bits(c.u32()?)),
            4 => c.u32()?.to_string(),
            7 => c.i64()?.to_string(),
            _ => (c.u32()? as i32).to_string(),
        };
        out.push('<');
        out.push_str(type_tag(tc));
        out.push('>');
        out.push_str(&name_of_hinted(key_hash, hints, unknown));
        out.push(':');
        out.push_str(&value);
        out.push_str("\r\n");
    }
    let nchildren = c.u32()?;
    for _ in 0..nchildren {
        emit_node(c, strs, out, unknown, hints, depth + 1)?;
    }
    indent(out, depth);
    out.push_str("[/");
    out.push_str(&name);
    out.push_str("]\r\n");
    Ok(())
}

/// A decompiled BINDAT plus the names the dictionary could not reverse.
pub struct Decompiled {
    pub text: String,
    /// The `UNK_<HEX8>` placeholders emitted, if any. They still compile back to
    /// the ORIGINAL hash (`bindat::raw_hash_token`), so the data survives — but
    /// the readable name is gone, which is worth telling the user about.
    pub unknown_names: BTreeSet<String>,
}

/// Decompile BINDAT bytes into the text `.DAT` they were compiled from
/// (keys/section names via the embedded dictionary; values verbatim), reporting
/// the unrecoverable names.
pub fn decompile_bindat_reported(data: &[u8]) -> Result<Decompiled, String> {
    decompile_bindat_hinted(data, &[])
}

/// `decompile_bindat_reported`, plus caller-supplied candidate names tried when
/// the embedded dictionary cannot reverse a hash — pass the file's own basename
/// (and anything else you know) to recover a mod's private section names.
pub fn decompile_bindat_hinted(data: &[u8], hints: &[&str]) -> Result<Decompiled, String> {
    let hint_map: HashMap<u32, String> = hints
        .iter()
        .filter(|h| !h.is_empty())
        .map(|h| (rg_hash_str(h), h.to_string()))
        .collect();
    let mut c = Cursor { b: data, p: 0 };
    let _ver = c.u32()?; // 2
    let count = c.u32()? as usize;
    let first_id = c.u32()?;
    let mut strs: HashMap<u32, String> = HashMap::with_capacity(count);
    for i in 0..count {
        let sid = if i == 0 { first_id } else { c.u32()? };
        let len = c.u16()? as usize;
        let s = c.utf16(len)?;
        strs.insert(sid, s);
    }
    let mut out = String::new();
    let mut unknown = BTreeSet::new();
    while c.p < data.len() {
        emit_node(&mut c, &strs, &mut out, &mut unknown, &hint_map, 0)?;
    }
    Ok(Decompiled { text: out, unknown_names: unknown })
}

/// Decompile BINDAT bytes into the text `.DAT` they were compiled from.
pub fn decompile_bindat(data: &[u8]) -> Result<String, String> {
    decompile_bindat_reported(data).map(|d| d.text)
}

/// UTF-16LE with BOM — the on-disk `.DAT` encoding the parsers expect.
pub fn to_dat_bytes(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_dictionary_has_no_collisions_and_core_keys() {
        // Every embedded name must resolve to itself (built with .or_insert, so
        // a collision would have silently dropped one — assert none happened).
        let raw = include_str!("../data/dat_keys.txt");
        let n = raw.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(REVERSE.len(), n, "reverse dict has hash collisions");
        for k in ["PIECE", "TILESET", "NAME", "GUID", "COLLISIONFILE", "NEVERBAKE"] {
            assert_eq!(name_of(rg_hash_str(k)), k, "core key {k} must reverse");
        }
    }

    /// A name the dictionary cannot reverse comes out as `UNK_<HEX8>`; compiling
    /// that text back MUST reproduce the original hash byte-for-byte. Before the
    /// escape existed the compiler re-hashed the placeholder TEXT, silently
    /// turning an unpacked `UNK_…` section into a key the engine never looks up —
    /// the whole section's data went dark with no error anywhere.
    #[test]
    fn unknown_name_roundtrips_hash_exact() {
        use crate::bindat::{compile_dat_bytes, compile_dat_bytes_reported};

        // A mod's brand-new section name: no shipped DAT uses it, and it belongs
        // to no numbered family the dictionary generates.
        const NOVEL: &str = "MIKURO_NOVEL_SECTION_QQ";
        assert!(
            !REVERSE.contains_key(&rg_hash_str(NOVEL)),
            "test premise: {NOVEL} must be outside the dictionary"
        );
        let src = &format!(
            "[SKILL]\r\n<STRING>NAME:T\r\n[{NOVEL}]\r\n<FLOAT>DAMAGE:3.5\r\n[/{NOVEL}]\r\n[/SKILL]\r\n"
        );
        let original = compile_dat_bytes(&to_dat_bytes(src)).expect("compile");

        let text = decompile_bindat(&original).expect("decompile");
        assert!(text.contains("UNK_"), "{NOVEL} should decompile to a placeholder: {text}");

        let out = compile_dat_bytes_reported(&to_dat_bytes(&text), None).expect("recompile");
        assert_eq!(out.bytes, original, "decompile→compile must be byte-identical");
        assert_eq!(
            out.raw_hash_names,
            vec![format!("UNK_{:08X}", rg_hash_str(NOVEL))],
            "the placeholder must be reported so the packer can warn"
        );

        // A normal name is untouched, and a look-alike that is not exactly
        // `UNK_` + 8 uppercase hex digits stays an ordinary (hashed) name.
        let plain = compile_dat_bytes_reported(&to_dat_bytes(src), None).expect("compile");
        assert!(plain.raw_hash_names.is_empty());
        for lookalike in ["UNK_1a2b3c4d", "UNK_1A2B3C4", "UNK_1A2B3C4D5"] {
            assert!(
                crate::bindat::raw_hash_token(&lookalike.chars().map(|c| c as u32).collect::<Vec<_>>())
                    .is_none(),
                "{lookalike} must NOT be treated as a raw-hash escape"
            );
        }
    }

    /// End-to-end (only where the game is installed): every LEVELSETS DAT in
    /// DATA.PAK, decompiled and re-parsed by the MPP DAT reader, must yield the
    /// SAME guid→piece table as the loose-MEDIA text source — i.e. the offline
    /// MPP pipeline gets identical base data whether it reads the extracted text
    /// or our PAK reconstruction.
    #[test]
    fn levelsets_roundtrip_matches_loose_source() {
        use std::path::Path;
        let pak = Path::new(r"E:\Torchlight 2\PAKS\DATA.PAK");
        if !pak.is_file() {
            eprintln!("skip: no DATA.PAK");
            return;
        }
        let mut r = crate::pak::PakReader::open_pak(pak).expect("open pak");
        let ls: Vec<crate::pak::Entry> = r
            .entries()
            .iter()
            .filter(|e| {
                let p = e.path.to_uppercase();
                p.contains("LEVELSETS/") && p.ends_with(".DAT")
            })
            .cloned()
            .collect();
        assert!(!ls.is_empty(), "PAK has LEVELSETS DATs");

        let mut checked = 0;
        let mut pieces_total = 0;
        for e in &ls {
            let bindat = r.read_entry(e).expect("read entry");
            let text = decompile_bindat(&bindat).expect("decompile");
            let dat = to_dat_bytes(&text);

            let mut from_pak = std::collections::HashMap::new();
            crate::mpp::dat::parse_dat(&dat, &mut from_pak);

            // Compare against the loose text source if present.
            let loose = Path::new(r"E:\Torchlight 2").join(&e.path);
            let Ok(disk) = std::fs::read(&loose) else { continue };
            let mut from_loose = std::collections::HashMap::new();
            crate::mpp::dat::parse_dat(&disk, &mut from_loose);

            let mut a: Vec<&String> = from_pak.keys().collect();
            let mut b: Vec<&String> = from_loose.keys().collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{}: guid set matches", e.path);
            for (g, p_loose) in &from_loose {
                let p_pak = &from_pak[g];
                assert_eq!(p_pak.name, p_loose.name, "{} guid {g}: name", e.path);
                assert_eq!(
                    p_pak.collision_files, p_loose.collision_files,
                    "{} guid {g}: collision_files",
                    e.path
                );
                assert_eq!(p_pak.files, p_loose.files, "{} guid {g}: files", e.path);
            }
            checked += 1;
            pieces_total += from_loose.len();
        }
        eprintln!("roundtrip OK: {checked} LEVELSETS DATs, {pieces_total} pieces");
        assert!(checked > 0, "checked at least one against loose source");
    }
}
