//! UTF-16 text handling with Python-compatible semantics.
//!
//! The BINDAT compiler must reproduce Python's `surrogatepass` behaviour: a
//! shipped .DAT can contain raw binary spliced into a `<STRING>` value that
//! decodes to LONE UTF-16 surrogates. Rust `String` cannot hold those, so the
//! bindat pipeline works on a "wide string" `WStr = Vec<u32>` of Unicode code
//! points where a lone surrogate is kept as its own code point (exactly what
//! Python's surrogatepass produces). Valid surrogate pairs combine into astral
//! code points — so `len`, ordering, hashing and re-encoding all match Python.

/// A Python-str-equivalent string: a sequence of code points, possibly
/// containing lone surrogates (0xD800..=0xDFFF).
pub type WStr = Vec<u32>;

/// Decode UTF-16-LE units into code points, surrogatepass-style: valid pairs
/// combine, lone surrogates are kept verbatim.
pub fn units_to_wstr(units: &[u16]) -> WStr {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() {
            let v = units[i + 1] as u32;
            if (0xDC00..0xE000).contains(&v) {
                out.push(0x10000 + ((u - 0xD800) << 10) + (v - 0xDC00));
                i += 2;
                continue;
            }
        }
        out.push(u);
        i += 1;
    }
    out
}

/// Encode code points back to UTF-16-LE bytes (surrogatepass: lone surrogates
/// re-encode verbatim; astral code points emit a pair). Exact byte round-trip.
pub fn wstr_to_utf16le(s: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        if c >= 0x10000 {
            let v = c - 0x10000;
            let hi = 0xD800 + (v >> 10);
            let lo = 0xDC00 + (v & 0x3FF);
            out.extend_from_slice(&(hi as u16).to_le_bytes());
            out.extend_from_slice(&(lo as u16).to_le_bytes());
        } else {
            out.extend_from_slice(&(c as u16).to_le_bytes());
        }
    }
    out
}

/// Number of UTF-16 code units `s` encodes to (astral code points count 2).
pub fn utf16_unit_len(s: &[u32]) -> usize {
    s.iter().map(|&c| if c >= 0x10000 { 2 } else { 1 }).sum()
}

/// Raw little-endian byte pairs -> u16 units (odd trailing byte dropped, as
/// Python's utf-16-le + surrogatepass would error there; corpus never has one).
pub fn bytes_to_units_le(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn bytes_to_units_be(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect()
}

/// Python `_decode_dat_bytes`: BOM-aware UTF-16 decode with surrogatepass.
/// FF FE -> LE (BOM stripped), FE FF -> BE (BOM stripped), else LE from 0.
pub fn decode_dat_bytes(data: &[u8]) -> WStr {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        units_to_wstr(&bytes_to_units_le(&data[2..]))
    } else if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
        units_to_wstr(&bytes_to_units_be(&data[2..]))
    } else {
        units_to_wstr(&bytes_to_units_le(data))
    }
}

/// Lossy decode of UTF-16-LE bytes to a Rust String (Python errors="replace"
/// equivalent: each lone surrogate / bad unit becomes U+FFFD). BOM NOT stripped.
pub fn decode_utf16le_lossy(data: &[u8]) -> String {
    String::from_utf16_lossy(&bytes_to_units_le(data))
}

/// Python `str.splitlines()` line-break set (code points).
fn is_py_linebreak(c: u32) -> bool {
    matches!(
        c,
        0x0A | 0x0B | 0x0C | 0x0D | 0x1C | 0x1D | 0x1E | 0x85 | 0x2028 | 0x2029
    )
}

/// Python-compatible `splitlines()` over a WStr (handles \r\n as one break).
pub fn py_splitlines(s: &[u32]) -> Vec<&[u32]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < s.len() {
        if is_py_linebreak(s[i]) {
            out.push(&s[start..i]);
            if s[i] == 0x0D && i + 1 < s.len() && s[i + 1] == 0x0A {
                i += 1;
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Is `c` Python `str.strip()` whitespace (the ASCII subset plus the common
/// Unicode spaces; keys in the corpus are ASCII so the ASCII set is what fires).
pub fn is_py_space(c: u32) -> bool {
    matches!(c, 0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0x1C | 0x1D | 0x1E | 0x85 | 0xA0)
        || matches!(c, 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
}

/// strip() both ends by predicate.
pub fn wstrip(s: &[u32]) -> &[u32] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && is_py_space(s[a]) {
        a += 1;
    }
    while b > a && is_py_space(s[b - 1]) {
        b -= 1;
    }
    &s[a..b]
}

/// Lossy display form of a WStr (lone surrogates -> U+FFFD).
pub fn wstr_display(s: &[u32]) -> String {
    s.iter()
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}'))
        .collect()
}
