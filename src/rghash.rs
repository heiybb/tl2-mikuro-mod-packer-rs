//! GUTS / Runic Games string hash (RGHash), == EditorGuts.dll sub_100CA9A0.
//!
//! ```text
//! h = len(s)                          // number of Unicode code points
//! for c in s: h = (h >> 27) ^ (h << 5) ^ c
//! ```
//!
//! The Python reference hashes Python `str` characters (Unicode code points,
//! with lone surrogates preserved by surrogatepass decoding), NOT raw UTF-16
//! units — for BMP text the two are identical. We therefore hash over the
//! code-point sequence (`&[u32]`, see [`crate::text::WStr`]).

/// RGHash over a code-point sequence.
pub fn rg_hash(s: &[u32]) -> u32 {
    let mut h = s.len() as u32;
    for &c in s {
        h = (h >> 27) ^ (h << 5) ^ c;
    }
    h
}

/// RGHash over a Rust string (each `char` is one code point).
pub fn rg_hash_str(s: &str) -> u32 {
    let mut h = s.chars().count() as u32;
    for c in s.chars() {
        h = (h >> 27) ^ (h << 5) ^ (c as u32);
    }
    h
}

/// RGHashUp: uppercase each character first (ASCII-range uppercase is all the
/// corpus ever exercises; matches Python's per-char `.upper()` for those).
pub fn rg_hash_up_str(s: &str) -> u32 {
    let mut h = s.chars().count() as u32;
    for c in s.chars() {
        let cc = if c.is_ascii() {
            c.to_ascii_uppercase() as u32
        } else {
            // Python does ord(c.upper()); multi-char expansions never occur in
            // the corpus (keys are ASCII). Use the first uppercase mapping.
            c.to_uppercase().next().unwrap_or(c) as u32
        };
        h = (h >> 27) ^ (h << 5) ^ cc;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_entries() {
        // Verified against TL2Lib/dictionary.txt: A=97, B=98, '0'=16, AI=105.
        assert_eq!(rg_hash_up_str("A"), 97);
        assert_eq!(rg_hash_up_str("B"), 98);
        assert_eq!(rg_hash_up_str("0"), 16);
        assert_eq!(rg_hash_up_str("AI"), 105);
        // Shipped BINDAT key hashes (UNITTHEMES/MAGICICE.DAT).
        assert_eq!(rg_hash_str("THEME"), 0x0F651DE5);
        assert_eq!(rg_hash_str("NAME"), 0x00660DE5);
        assert_eq!(rg_hash_str("GUID"), 0x0062DD64);
    }
}
