//! .LAYOUT reader for the pathing pipeline (port of mpp/layout.py):
//! room-piece placements + recursive world transforms
//! (world = parent_world @ local, local = T(pos) @ R @ S, UP = FWD x RIGHT).

use std::collections::HashMap;
use std::path::Path;

use super::geom::{orientation_axes, yaw_axes, Mat4, Vec3};

#[derive(Clone, Debug, Default)]
pub struct LayoutObject {
    pub id: String,
    pub parent_id: String,
    pub descriptor: String,
    pub name: String,
    pub guid: String,
    pub pos: Vec3,
    pub has_pos: bool,
    pub fwd: Option<Vec3>,
    pub right: Option<Vec3>,
    pub yaw: Option<f64>,
    pub scale: Vec3,
    pub nopath: bool,
    pub visual: i64,
    pub props: HashMap<String, String>,
    pub children: Vec<usize>,
    pub world: Mat4,
}

impl LayoutObject {
    pub fn local_matrix(&self) -> Mat4 {
        let t = Mat4::translation(if self.has_pos { self.pos } else { Vec3::default() });
        let rot = if let (Some(f), Some(r)) = (self.fwd, self.right) {
            let (r, u, f) = orientation_axes(f, r);
            Mat4::from_axes(r, u, f)
        } else if let Some(yaw) = self.yaw {
            let (r, u, f) = yaw_axes(yaw);
            Mat4::from_axes(r, u, f)
        } else {
            Mat4::identity()
        };
        let s = Mat4::scale(self.scale.x, self.scale.y, self.scale.z);
        t.matmul(&rot).matmul(&s)
    }
}

pub struct Layout {
    /// Pre-order flattened objects (Python `all_objects`).
    pub objects: Vec<LayoutObject>,
    pub by_id: HashMap<String, usize>,
}

impl Layout {
    pub fn room_piece_indices(&self) -> Vec<usize> {
        self.objects
            .iter()
            .enumerate()
            .filter(|(_i, o)| o.descriptor == "Room Piece")
            .map(|(i, _o)| i)
            .collect()
    }
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
        data.iter().map(|&b| b as char).collect()
    }
}

/// `<([^>]+)>([A-Z0-9 _]+):(.*)` anchored on the stripped line.
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

struct RawObj {
    props: HashMap<String, String>,
    children: Vec<RawObj>,
}

struct TreeParser<'a> {
    lines: Vec<&'a str>,
    i: usize,
}

impl<'a> TreeParser<'a> {
    fn parse_objects(&mut self) -> Vec<RawObj> {
        let mut objs = Vec::new();
        while self.i < self.lines.len() {
            let s = self.lines[self.i].trim();
            if s == "[BASEOBJECT]" {
                objs.push(self.parse_baseobject());
            } else if s == "[/OBJECTS]" || s == "[/CHILDREN]" {
                self.i += 1;
                break;
            } else {
                self.i += 1;
            }
        }
        objs
    }

    fn parse_baseobject(&mut self) -> RawObj {
        self.i += 1; // consume [BASEOBJECT]
        let mut props: HashMap<String, String> = HashMap::new();
        let mut children = Vec::new();
        while self.i < self.lines.len() {
            let s = self.lines[self.i].trim();
            if s == "[PROPERTIES]" {
                self.i += 1;
                while self.i < self.lines.len() && self.lines[self.i].trim() != "[/PROPERTIES]" {
                    if let Some((k, v)) = match_prop(self.lines[self.i].trim()) {
                        props.insert(k.trim().to_string(), v.trim().to_string());
                    }
                    self.i += 1;
                }
                self.i += 1; // consume [/PROPERTIES]
            } else if s == "[CHILDREN]" {
                self.i += 1;
                children = self.parse_objects();
            } else if s == "[/BASEOBJECT]" {
                self.i += 1;
                break;
            } else {
                self.i += 1;
            }
        }
        RawObj { props, children }
    }
}

fn fget(props: &HashMap<String, String>, key: &str) -> Option<f64> {
    props.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

fn build_object(props: HashMap<String, String>) -> LayoutObject {
    let mut o = LayoutObject {
        descriptor: props.get("DESCRIPTOR").cloned().unwrap_or_default(),
        name: props.get("NAME").cloned().unwrap_or_default(),
        id: props.get("ID").cloned().unwrap_or_default(),
        parent_id: props.get("PARENTID").cloned().unwrap_or_else(|| "-1".to_string()),
        guid: props.get("GUID").cloned().unwrap_or_default(),
        scale: Vec3::new(1.0, 1.0, 1.0),
        ..Default::default()
    };
    let (px, py, pz) = (
        fget(&props, "POSITIONX"),
        fget(&props, "POSITIONY"),
        fget(&props, "POSITIONZ"),
    );
    if px.is_some() || py.is_some() || pz.is_some() {
        o.pos = Vec3::new(px.unwrap_or(0.0), py.unwrap_or(0.0), pz.unwrap_or(0.0));
        o.has_pos = true;
    }
    let f = (
        fget(&props, "FORWARDX"),
        fget(&props, "FORWARDY"),
        fget(&props, "FORWARDZ"),
    );
    let r = (
        fget(&props, "RIGHTX"),
        fget(&props, "RIGHTY"),
        fget(&props, "RIGHTZ"),
    );
    if let ((Some(fx), Some(fy), Some(fz)), (Some(rx), Some(ry), Some(rz))) = (f, r) {
        o.fwd = Some(Vec3::new(fx, fy, fz));
        o.right = Some(Vec3::new(rx, ry, rz));
    }
    if o.fwd.is_none() {
        o.yaw = fget(&props, "YAW");
    }
    o.scale = Vec3::new(
        fget(&props, "X").unwrap_or(1.0),
        fget(&props, "Y").unwrap_or(1.0),
        fget(&props, "Z").unwrap_or(1.0),
    );
    o.nopath = props
        .get("NOPATH")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    o.visual = props
        .get("VISUAL")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    o.props = props;
    o
}

pub fn parse_layout(data: &[u8]) -> Layout {
    let txt = decode(data);
    let lines: Vec<&str> = txt.lines().collect();
    // find [OBJECTS]
    let mut start = None;
    for (idx, ln) in lines.iter().enumerate() {
        if ln.trim() == "[OBJECTS]" {
            start = Some(idx + 1);
            break;
        }
    }
    let roots = match start {
        None => Vec::new(),
        Some(s) => {
            let mut tp = TreeParser {
                lines: lines[s..].to_vec(),
                i: 0,
            };
            tp.parse_objects()
        }
    };

    // flatten pre-order (Python index()): object, then its children.
    let mut objects: Vec<LayoutObject> = Vec::new();
    fn flatten(raw: RawObj, objects: &mut Vec<LayoutObject>) -> usize {
        let idx = objects.len();
        objects.push(build_object(raw.props));
        let mut kids = Vec::new();
        for ch in raw.children {
            kids.push(flatten(ch, objects));
        }
        objects[idx].children = kids;
        idx
    }
    for r in roots {
        flatten(r, &mut objects);
    }

    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (i, o) in objects.iter().enumerate() {
        if !o.id.is_empty() {
            by_id.insert(o.id.clone(), i);
        }
    }

    // compose world transforms via PARENTID chains (memoized).
    let mut world: Vec<Option<Mat4>> = vec![None; objects.len()];
    fn compute_world(
        i: usize,
        objects: &[LayoutObject],
        by_id: &HashMap<String, usize>,
        world: &mut Vec<Option<Mat4>>,
        depth: usize,
    ) -> Mat4 {
        if let Some(w) = world[i] {
            return w;
        }
        let local = objects[i].local_matrix();
        let w = match by_id.get(&objects[i].parent_id) {
            Some(&p) if p != i && depth < 64 => {
                compute_world(p, objects, by_id, world, depth + 1).matmul(&local)
            }
            _ => local,
        };
        world[i] = Some(w);
        w
    }
    for i in 0..objects.len() {
        let w = compute_world(i, &objects, &by_id, &mut world, 0);
        objects[i].world = w;
    }

    Layout { objects, by_id }
}

pub fn load_layout_file(path: &Path) -> std::io::Result<Layout> {
    Ok(parse_layout(&std::fs::read(path)?))
}
