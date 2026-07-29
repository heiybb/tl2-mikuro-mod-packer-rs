//! Reverse dictionary for BINDAT key/section name hashes.
//!
//! A BINDAT stores every key and section name only as its `rghash`, so unpacking needs a
//! hash -> name table. This replaces the old flat `data/dat_keys.txt` (2888 lines) with a
//! tiered, front-coded, DEFLATE-compressed table of 465,361 names.
//!
//! # Why the tiers matter
//!
//! A false positive in this table costs almost nothing -- a name nobody looks up is dead
//! weight. The one real cost is a hash SLOT being occupied: a key that appears in no tier
//! can land on a slot some junk string already claimed and come back with a plausible
//! wrong name instead of an honest `UNK_`, which nothing flags. Entries are therefore
//! merged highest-trust first, and the tier travels with every hit:
//!
//! | tier              | entries | source                                        |
//! |-------------------|---------|-----------------------------------------------|
//! | `t1_dat_key`      |   3,335 | names seen in real DAT key/section positions  |
//! | `t2_engine_ident` |   3,122 | UTF-16 literals in Torchlight2.exe/EditorGuts |
//! | `t3_ext_dict`     |   6,122 | curated `hash:text` files, each line verified |
//! | `t4_enum_family`  |  48,248 | generated `NAME_0..300` families              |
//! | `t5_basename`     | 223,477 | every shipped/mod file's basename             |
//! | `t6_dat_value`    | 181,057 | identifier-shaped DAT values + path segments  |
//!
//! `t1..t4` are [`Tier::is_authoritative`]; `t5`/`t6` are strong but generic, so a
//! caller-supplied hint (the file's own basename) should be preferred over them -- the
//! hint is specific to the file being unpacked, a global basename table is not.
//!
//! # Measured
//!
//! Against the 2888-entry table this replaces: all 2888 hashes resolve to the **identical**
//! name (0 lost, 0 changed) -- it is a strict superset. On a mod source tree that was not
//! part of the corpus, of 2073 distinct names the old table named 1516 and the new one
//! names 1912, with **zero** wrong names in either.
//!
//! # Format
//!
//! ```text
//! "TL2FCX01" | u32 n_tiers | u32 count[n_tiers] | u32 names_len | tier names ('\n')
//!            | u32 raw_len | raw-DEFLATE( front-coded blob )
//! ```
//!
//! Front coding, per entry: `[shared prefix length][remaining suffix][NUL]`, the prefix
//! measured in BYTES against the previous entry **within the same tier** (the run resets
//! at every tier boundary). Byte-wise rather than char-wise because mod filenames contain
//! CJK and a character index would split a UTF-8 sequence.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::rghash::rg_hash_str;

const MAGIC: &[u8; 8] = b"TL2FCX01";
const PACKED: &[u8] = include_bytes!("../data/dat_keys.fcx");

/// Which source claimed a hash. Ordered most trustworthy first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    DatKey,
    EngineIdent,
    ExtDict,
    EnumFamily,
    Basename,
    DatValue,
}

impl Tier {
    fn from_index(i: usize) -> Tier {
        match i {
            0 => Tier::DatKey,
            1 => Tier::EngineIdent,
            2 => Tier::ExtDict,
            3 => Tier::EnumFamily,
            4 => Tier::Basename,
            _ => Tier::DatValue,
        }
    }

    /// True when a hit needs no corroboration. `t5`/`t6` are derived from filenames and
    /// values rather than from anything observed in a key position, so a caller that has
    /// a hint for the hash should trust the hint over them.
    pub fn is_authoritative(self) -> bool {
        matches!(
            self,
            Tier::DatKey | Tier::EngineIdent | Tier::ExtDict | Tier::EnumFamily
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::DatKey => "t1_dat_key",
            Tier::EngineIdent => "t2_engine_ident",
            Tier::ExtDict => "t3_ext_dict",
            Tier::EnumFamily => "t4_enum_family",
            Tier::Basename => "t5_basename",
            Tier::DatValue => "t6_dat_value",
        }
    }
}

pub struct DatKeys {
    names: Vec<String>,
    tier: Vec<u8>,
    by_hash: HashMap<u32, u32>,
}

static KEYS: LazyLock<DatKeys> = LazyLock::new(|| {
    // A/B escape hatch: `TL2_LEGACY_DAT_KEYS=1` rebuilds the table from the flat 2888-line
    // list this replaced, so an unpack can be run twice and the `UNK_` counts diffed on
    // real data. Kept because "it should only ever resolve more" is a claim worth being
    // able to re-measure, not just assert.
    if std::env::var_os("TL2_LEGACY_DAT_KEYS").is_some() {
        return DatKeys::from_legacy(include_str!("../data/dat_keys.txt"));
    }
    DatKeys::decode(PACKED).expect("dat_keys.fcx")
});

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn u32(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.p..self.p + 4).ok_or("dat_keys: eof u32")?;
        self.p += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self.b.get(self.p..self.p + n).ok_or("dat_keys: eof bytes")?;
        self.p += n;
        Ok(s)
    }
}

impl DatKeys {
    fn decode(data: &[u8]) -> Result<DatKeys, String> {
        if data.len() < 8 || &data[..8] != MAGIC {
            return Err("dat_keys: bad magic".into());
        }
        let mut c = Cursor { b: data, p: 8 };
        let n_tiers = c.u32()? as usize;
        let mut counts = Vec::with_capacity(n_tiers);
        for _ in 0..n_tiers {
            counts.push(c.u32()? as usize);
        }
        let names_len = c.u32()? as usize;
        c.take(names_len)?; // tier names -- documentation only, order is the contract
        let raw_len = c.u32()? as usize;

        let mut blob = vec![0u8; raw_len];
        libdeflater::Decompressor::new()
            .deflate_decompress(&data[c.p..], &mut blob)
            .map_err(|e| format!("dat_keys: inflate failed: {e:?}"))?;

        let total: usize = counts.iter().sum();
        let mut names = Vec::with_capacity(total);
        let mut tier = Vec::with_capacity(total);
        let mut by_hash = HashMap::with_capacity(total * 2);

        let mut p = 0usize;
        for (ti, &count) in counts.iter().enumerate() {
            // The shared-prefix run restarts at each tier boundary.
            let mut prev: Vec<u8> = Vec::new();
            for _ in 0..count {
                let k = *blob.get(p).ok_or("dat_keys: eof prefix")? as usize;
                p += 1;
                let end = blob[p..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or("dat_keys: unterminated entry")?
                    + p;
                if k > prev.len() {
                    return Err("dat_keys: prefix longer than previous entry".into());
                }
                let mut cur = Vec::with_capacity(k + (end - p));
                cur.extend_from_slice(&prev[..k]);
                cur.extend_from_slice(&blob[p..end]);
                p = end + 1;

                let name = String::from_utf8(cur.clone())
                    .map_err(|_| "dat_keys: entry is not utf-8".to_string())?;
                // First writer wins, which is exactly the tier priority: the table is
                // emitted highest-trust first, so a later tier can never displace one.
                by_hash.entry(rg_hash_str(&name)).or_insert(names.len() as u32);
                names.push(name);
                tier.push(ti as u8);
                prev = cur;
            }
        }
        Ok(DatKeys { names, tier, by_hash })
    }

    fn from_legacy(text: &str) -> DatKeys {
        let mut d = DatKeys { names: Vec::new(), tier: Vec::new(), by_hash: HashMap::new() };
        for line in text.lines() {
            let n = line.trim_end_matches('\r');
            if n.is_empty() {
                continue;
            }
            d.by_hash.entry(rg_hash_str(n)).or_insert(d.names.len() as u32);
            d.names.push(n.to_string());
            d.tier.push(0);
        }
        d
    }

    pub fn get(&self, hash: u32) -> Option<(&str, Tier)> {
        let i = *self.by_hash.get(&hash)? as usize;
        Some((self.names[i].as_str(), Tier::from_index(self.tier[i] as usize)))
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// Resolve a name hash. Returns the name and the tier that claimed it.
pub fn lookup(hash: u32) -> Option<(&'static str, Tier)> {
    KEYS.get(hash)
}

/// The marker every surface agrees on for a name that could not be recovered.
/// The TUI and the browser page both colour a line red on finding this substring,
/// so the placeholder text itself is the contract -- no side channel to keep in sync.
pub const UNK_PREFIX: &str = "UNK_";

/// The matching marker for a name that WAS unknown to the shipped table and got solved
/// anyway. Same contract as [`UNK_PREFIX`]: the text is the signal, every surface just
/// matches it. Deliberately free of `UNK_` so the red rule can never fire on a green line.
pub const RECOVERED_MARKER: &str = "recovered:";

/// Wrap `s` in ANSI green under the same TTY rule as [`paint_warn`].
pub fn paint_ok(s: &str, is_tty: bool) -> String {
    if is_tty {
        format!("\x1b[1;32m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Wrap `s` in ANSI red, but ONLY when the stream is a terminal.
///
/// This has to be conditional. The TUI runs the CLI as a child process and pipes its
/// stdout into a ratatui `Paragraph`, which does not interpret escape sequences -- an
/// unconditional colour would paint `[31m` into the log as literal garbage. Piped
/// output stays plain and the TUI colours it itself by matching [`UNK_PREFIX`].
pub fn paint_warn(s: &str, is_tty: bool) -> String {
    if is_tty {
        format!("\x1b[1;31m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Number of distinct hashes the table can reverse.
pub fn len() -> usize {
    KEYS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_reverses() {
        assert!(len() > 400_000, "table looks truncated: {}", len());
        for (name, tier) in [
            ("SECOND_STAT", Tier::EngineIdent),
            ("SHOULDERS_OVERRIDE", Tier::DatKey),
            ("NAME", Tier::DatKey),
            ("GUID", Tier::DatKey),
        ] {
            let (got, t) = lookup(rg_hash_str(name)).expect(name);
            assert_eq!(got, name);
            assert_eq!(t, tier, "{name} tier");
        }
    }

    /// Every entry must round-trip: the hash it is filed under must be its own rghash.
    #[test]
    fn every_entry_hashes_to_its_slot() {
        for (&h, &i) in KEYS.by_hash.iter() {
            assert_eq!(rg_hash_str(&KEYS.names[i as usize]), h);
        }
    }

    /// The table must be a strict superset of the flat list it replaces: every name in
    /// the old `dat_keys.txt` has to come back byte-identical, or an unpack that used to
    /// be correct would silently change.
    #[test]
    fn superset_of_legacy_dat_keys() {
        let legacy = include_str!("../data/dat_keys.txt");
        let mut seen: HashMap<u32, &str> = HashMap::new();
        for line in legacy.lines() {
            let n = line.trim_end_matches('\r');
            if !n.is_empty() {
                seen.entry(rg_hash_str(n)).or_insert(n);
            }
        }
        let mut lost = Vec::new();
        for (&h, &n) in seen.iter() {
            match lookup(h) {
                Some((got, _)) if got == n => {}
                other => lost.push((n, other.map(|(s, _)| s))),
            }
        }
        assert!(lost.is_empty(), "{} legacy names changed: {:?}", lost.len(), &lost[..lost.len().min(8)]);
    }
}
