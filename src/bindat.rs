//! Real Torchlight 2 .DAT -> .BINDAT compiler (port of bindat.py).
//!
//! From-scratch serializer: parses the .DAT text into a node tree and emits the
//! BINDAT bytes exactly as GUTS (EditorGuts.dll sub_10289A40) does. String
//! VALUES use per-file hash ids (`HashStringDict`): the game resolves a
//! BINDAT's ids through THAT file's own table, so id values only need to be
//! unique within the file — zero corpus dependency, fully parallel.
//!
//! Format (little-endian):
//!   header: u32 version(=2); u32 string_count; u32 first_id
//!   string table ascending by id: entry0 `u16 len; wchar[]`,
//!                                 entry1.. `u32 id; u16 len; wchar[]`
//!   body: recursive NODE { u32 key_hash; u32 prop_count; PROP*;
//!                          u32 child_count; NODE* }
//!   PROP { u32 key_hash; u32 type; value (8B if type in {3,7} else 4B) }

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::rghash::rg_hash;
use crate::text::{
    decode_dat_bytes, py_splitlines, utf16_unit_len, wstr_to_utf16le, wstrip, WStr,
};

const EMPTY_ID: u32 = 0xFFFF_FFFF;

/// Property type codes from the <TAG> in the DAT text.
fn type_code(tag: &[u32]) -> u32 {
    // _norm_tag: upper + remove spaces (tags are ASCII).
    let norm: String = tag
        .iter()
        .filter(|&&c| c != 0x20)
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}').to_ascii_uppercase())
        .collect();
    match norm.as_str() {
        "INTEGER" | "INT" => 1,
        "FLOAT" => 2,
        "UNSIGNEDINT" | "UINT" => 4,
        "STRING" => 5,
        "BOOL" | "BOOLEAN" => 6,
        "INTEGER64" | "INT64" => 7,
        "TRANSLATE" => 8,
        _ => 5,
    }
}

pub struct Node {
    pub name: WStr,
    /// (type_code, key, raw_value)
    pub props: Vec<(u32, WStr, WStr)>,
    pub children: Vec<Node>,
}

/// Parse .DAT text (code points) into the top-level node list.
/// Mirrors `parse_dat_text`: `[NAME]`/`[/NAME]` sections nest; property lines
/// are `<TYPE>KEY:VALUE` (KEY may be empty; VALUE keeps trailing spaces).
pub fn parse_dat_text(text: &[u32]) -> Vec<Node> {
    // text.lstrip('﻿')
    let mut t = text;
    while !t.is_empty() && t[0] == 0xFEFF {
        t = &t[1..];
    }
    let lines = py_splitlines(t);
    let mut roots: Vec<Node> = Vec::new();
    // Stack of index paths into the tree under construction.
    // We build with an explicit ownership stack: nodes are pushed and later
    // attached to their parent when popped.
    let mut stack: Vec<Node> = Vec::new();

    fn attach(roots: &mut Vec<Node>, stack: &mut Vec<Node>, node: Node) {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else {
            roots.push(node);
        }
    }

    for raw in lines {
        // line = raw.lstrip(' \t').rstrip('\r')
        let mut a = 0;
        while a < raw.len() && (raw[a] == 0x20 || raw[a] == 0x09) {
            a += 1;
        }
        let mut b = raw.len();
        while b > a && raw[b - 1] == 0x0D {
            b -= 1;
        }
        let line = &raw[a..b];
        if line.is_empty() {
            continue;
        }
        if line[0] == b'[' as u32 {
            let close = match line.iter().position(|&c| c == b']' as u32) {
                Some(p) => p,
                None => continue,
            };
            let name = wstrip(&line[1..close]);
            if name.first() == Some(&(b'/' as u32)) {
                // closing tag: pop and attach to parent
                if let Some(done) = stack.pop() {
                    attach(&mut roots, &mut stack, done);
                }
                continue;
            }
            stack.push(Node {
                name: name.to_vec(),
                props: Vec::new(),
                children: Vec::new(),
            });
            continue;
        }
        // _PROP_RE: ^<([A-Za-z0-9_ ]+)>\s*([^:]*?)\s*:(.*)$
        if let Some((tag, key, value)) = match_prop_line(line) {
            if let Some(top) = stack.last_mut() {
                top.props.push((type_code(&tag), key, value));
            }
        }
    }
    // Unclosed sections: Python leaves them on the stack (they were attached at
    // open time). Our attach-on-close means we must flush leftovers bottom-up.
    while let Some(done) = stack.pop() {
        attach(&mut roots, &mut stack, done);
    }
    roots
}

fn is_tag_char(c: u32) -> bool {
    matches!(c, 0x41..=0x5A | 0x61..=0x7A | 0x30..=0x39 | 0x5F | 0x20)
}

/// Manual port of `^<([A-Za-z0-9_ ]+)>\s*([^:]*?)\s*:(.*)$` + caller strips.
/// Returns (type_tag_stripped, key_stripped, raw_value).
fn match_prop_line(line: &[u32]) -> Option<(WStr, WStr, WStr)> {
    if line.first() != Some(&(b'<' as u32)) {
        return None;
    }
    let gt = line.iter().position(|&c| c == b'>' as u32)?;
    if gt < 2 {
        return None; // empty tag
    }
    let tag = &line[1..gt];
    if !tag.iter().all(|&c| is_tag_char(c)) {
        return None;
    }
    let rest = &line[gt + 1..];
    // first ':' after the tag terminates the key ([^:]*? is non-greedy)
    let colon = rest.iter().position(|&c| c == b':' as u32)?;
    let key = wstrip(&rest[..colon]);
    let value = &rest[colon + 1..];
    Some((wstrip(tag).to_vec(), key.to_vec(), value.to_vec()))
}

// ---- value encoding (Python-compatible int()/float() fallbacks) ----

fn py_parse_i64(s: &[u32]) -> Option<i64> {
    let t: String = wstrip(s)
        .iter()
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}'))
        .collect();
    if t.is_empty() {
        return None;
    }
    t.parse::<i64>().ok()
}

fn py_parse_f64(s: &[u32]) -> Option<f64> {
    let t: String = wstrip(s)
        .iter()
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}'))
        .collect();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

/// `_encode_int`: int(raw) else int(float(raw)) else 0.
fn encode_int(raw: &[u32]) -> i64 {
    if let Some(v) = py_parse_i64(raw) {
        return v;
    }
    if let Some(f) = py_parse_f64(raw) {
        return f.trunc() as i64;
    }
    0
}

/// `_encode_float_bits`: float(raw) else 0.0, as f32 bits.
fn encode_float_bits(raw: &[u32]) -> u32 {
    let f = py_parse_f64(raw).unwrap_or(0.0);
    (f as f32).to_bits()
}

/// `_encode_bool`: strip().upper() in (TRUE, 1, YES).
fn encode_bool(raw: &[u32]) -> u32 {
    let t: String = wstrip(raw)
        .iter()
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}').to_ascii_uppercase())
        .collect();
    matches!(t.as_str(), "TRUE" | "1" | "YES") as u32
}

// ---- per-file hash string dictionary ----

/// Per-FILE string->id via RGHash with intra-file linear probing.
/// Ids depend only on the file's string SET (iterated sorted, i.e. code-point
/// lexicographic like Python's `sorted(strings)`).
pub struct HashStringDict {
    s2id: BTreeMap<WStr, u32>,
}

impl HashStringDict {
    pub fn new(strings: &BTreeSet<WStr>) -> Self {
        let mut s2id = BTreeMap::new();
        let mut used: HashSet<u32> = HashSet::new();
        for s in strings {
            let mut i = rg_hash(s);
            while i == EMPTY_ID || used.contains(&i) {
                i = i.wrapping_add(1);
            }
            used.insert(i);
            s2id.insert(s.clone(), i);
        }
        HashStringDict { s2id }
    }

    pub fn get(&self, s: &[u32]) -> u32 {
        if s.is_empty() {
            EMPTY_ID
        } else {
            self.s2id[s]
        }
    }
}

const STRING_TYPES: [u32; 2] = [5, 8];

fn collect_strings(node: &Node, out: &mut BTreeSet<WStr>) {
    for (tc, _k, v) in &node.props {
        if STRING_TYPES.contains(tc) && !v.is_empty() {
            out.insert(v.clone());
        }
    }
    for ch in &node.children {
        collect_strings(ch, out);
    }
}

fn serialize_node(node: &Node, sdict: &HashStringDict, out: &mut Vec<u8>) {
    out.extend_from_slice(&rg_hash(&node.name).to_le_bytes());
    out.extend_from_slice(&(node.props.len() as u32).to_le_bytes());
    for (tc, key, value) in &node.props {
        out.extend_from_slice(&rg_hash(key).to_le_bytes());
        out.extend_from_slice(&tc.to_le_bytes());
        match tc {
            5 | 8 => out.extend_from_slice(&sdict.get(value).to_le_bytes()),
            6 => out.extend_from_slice(&encode_bool(value).to_le_bytes()),
            2 => out.extend_from_slice(&encode_float_bits(value).to_le_bytes()),
            4 => out.extend_from_slice(&((encode_int(value) as u64 & 0xFFFF_FFFF) as u32).to_le_bytes()),
            7 => out.extend_from_slice(&(encode_int(value) as u64).to_le_bytes()),
            _ => out.extend_from_slice(&(encode_int(value) as i32).to_le_bytes()),
        }
    }
    out.extend_from_slice(&(node.children.len() as u32).to_le_bytes());
    for ch in &node.children {
        serialize_node(ch, sdict, out);
    }
}

/// Serialize a parsed node tree to BINDAT bytes.
pub fn compile_tree(roots: &[Node]) -> Result<Vec<u8>, String> {
    if roots.is_empty() {
        return Err("no root section parsed from DAT".into());
    }
    let mut string_set: BTreeSet<WStr> = BTreeSet::new();
    for r in roots {
        collect_strings(r, &mut string_set);
    }
    let sdict = HashStringDict::new(&string_set);
    // id -> string, ascending by id (BTreeMap iteration).
    let mut id_list: BTreeMap<u32, &WStr> = BTreeMap::new();
    for (s, &i) in &sdict.s2id {
        id_list.insert(i, s);
    }
    let first_id = id_list.keys().next().copied().unwrap_or(0);

    let mut out = Vec::new();
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(id_list.len() as u32).to_le_bytes());
    out.extend_from_slice(&first_id.to_le_bytes());
    for (idx, (sid, s)) in id_list.iter().enumerate() {
        let enc = wstr_to_utf16le(s);
        let ln = (enc.len() / 2) as u16;
        if idx > 0 {
            out.extend_from_slice(&sid.to_le_bytes());
        }
        out.extend_from_slice(&ln.to_le_bytes());
        out.extend_from_slice(&enc);
    }
    for r in roots {
        serialize_node(r, &sdict, &mut out);
    }
    Ok(out)
}

/// Compile .DAT bytes (UTF-16, BOM-aware) into BINDAT bytes from scratch.
pub fn compile_dat_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    let text = decode_dat_bytes(data);
    let roots = parse_dat_text(&text);
    compile_tree(&roots)
}

/// Compile a .DAT file path into BINDAT bytes.
pub fn compile_dat_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    compile_dat_bytes(&data)
}

/// Number of UTF-16 units a string-table entry writes (used by tests).
pub fn table_entry_units(s: &[u32]) -> usize {
    utf16_unit_len(s)
}
