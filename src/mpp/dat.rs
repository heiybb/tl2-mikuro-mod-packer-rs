//! LEVELSETS .DAT reader (port of mpp/dat.py): [PIECE] blocks -> guid -> PieceDef.

use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct PieceDef {
    pub name: String,
    pub guid: String,
    pub scalable: bool,
    pub neverbake: bool,
    pub alwaysbakecollision: bool,
    /// All render-mesh variants in declared order; collision variants paired.
    pub files: Vec<String>,
    pub collision_files: Vec<String>,
}

fn decode(data: &[u8]) -> String {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        crate::text::decode_utf16le_lossy(&data[2..])
    } else if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
        let units: Vec<u16> = data[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        data.iter().map(|&b| b as char).collect() // latin-1
    }
}

/// `<([^>]+)>([A-Z0-9 _]+):(.*)` anchored (re.match) on the stripped line.
fn match_prop(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('<')?;
    let gt = rest.find('>')?;
    if gt == 0 {
        return None;
    }
    let after = &rest[gt + 1..];
    let colon = after.find(':')?;
    if colon == 0 {
        return None;
    }
    let key = &after[..colon];
    if !key
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ' || c == '_')
    {
        return None;
    }
    Some((key, &after[colon + 1..]))
}

pub fn parse_dat(data: &[u8], out: &mut HashMap<String, PieceDef>) {
    let txt = decode(data);
    let lines: Vec<&str> = txt.lines().collect();
    let mut i = 0;
    let n = lines.len();
    while i < n {
        if lines[i].trim() == "[PIECE]" {
            i += 1;
            let mut props: HashMap<String, Vec<String>> = HashMap::new();
            while i < n {
                let s = lines[i].trim();
                if s == "[/PIECE]" {
                    i += 1;
                    break;
                }
                if let Some((k, v)) = match_prop(s) {
                    props
                        .entry(k.trim().to_string())
                        .or_default()
                        .push(v.trim().to_string());
                }
                i += 1;
            }
            let get1 = |k: &str, d: &str| -> String {
                props
                    .get(k)
                    .and_then(|v| v.first())
                    .cloned()
                    .unwrap_or_else(|| d.to_string())
            };
            let files = props.get("FILE").cloned().unwrap_or_default();
            let colls = props.get("COLLISIONFILE").cloned().unwrap_or_default();
            let pd = PieceDef {
                name: get1("NAME", ""),
                guid: get1("GUID", ""),
                scalable: get1("SCALABLE", "true").to_lowercase() == "true",
                neverbake: get1("NEVERBAKE", "false").to_lowercase() == "true",
                alwaysbakecollision: get1("ALWAYSBAKECOLLISION", "false").to_lowercase() == "true",
                files,
                collision_files: colls,
            };
            if !pd.guid.is_empty() {
                out.insert(pd.guid.clone(), pd);
            }
        } else {
            i += 1;
        }
    }
}

/// Scan every *.DAT (not *.BINDAT) under levelsets_dir, merged guid->PieceDef.
pub fn load_all_levelsets(levelsets_dir: &Path) -> HashMap<String, PieceDef> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    walk_sorted(levelsets_dir, &mut files);
    for p in files {
        let fu = p
            .file_name()
            .map(|s| s.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        if fu.ends_with(".DAT") && !fu.ends_with(".BINDAT") {
            if let Ok(data) = std::fs::read(&p) {
                parse_dat(&data, &mut out);
            }
        }
    }
    out
}

fn walk_sorted(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<(String, std::path::PathBuf, bool)> = match std::fs::read_dir(root) {
        Ok(rd) => rd
            .flatten()
            .map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (e.file_name().to_string_lossy().to_uppercase(), e.path(), is_dir)
            })
            .collect(),
        Err(_) => return,
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut dirs = Vec::new();
    for (_n, p, is_dir) in entries {
        if is_dir {
            dirs.push(p);
        } else {
            out.push(p);
        }
    }
    for d in dirs {
        walk_sorted(&d, out);
    }
}
