//! LAYOUT -> BINLAYOUT compiler (port of binlayout.py).
//!
//! Real compiler: parses the .LAYOUT text and serializes the .BINLAYOUT binary
//! from scratch, byte-exact to the GUTS editor (EditorGuts.dll writer chain
//! sub_101169B0 -> ... -> sub_10115320 per object, sub_101150F0 datagroup).
//! The per-descriptor property schema is the PURE-DLL dump embedded from
//! data/binlayout_schema.json (159 descriptors / 2258 props); the datagroup
//! TAG map from data/binlayout_datagroup_tags.json (131 tags).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::text::decode_utf16le_lossy;

const MAGIC: u8 = 0x0B;
const FLAG: u8 = 0x04;

// ── embedded schema ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SchemaFile {
    descriptors: HashMap<String, Descriptor>,
}

#[derive(Deserialize)]
pub struct Descriptor {
    pub code: Option<i64>,
    pub props: HashMap<String, PropSpec>,
}

#[derive(Deserialize)]
pub struct PropSpec {
    pub code: u8,
    pub enc: String,
}

fn schema() -> &'static HashMap<String, Descriptor> {
    static SCHEMA: OnceLock<HashMap<String, Descriptor>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let f: SchemaFile =
            serde_json::from_str(include_str!("../data/binlayout_schema.json"))
                .expect("embedded binlayout_schema.json is valid");
        f.descriptors
    })
}

fn dgtags() -> &'static HashMap<String, u32> {
    static TAGS: OnceLock<HashMap<String, u32>> = OnceLock::new();
    TAGS.get_or_init(|| {
        serde_json::from_str(include_str!("../data/binlayout_datagroup_tags.json"))
            .expect("embedded binlayout_datagroup_tags.json is valid")
    })
}

// ── .LAYOUT text parser ────────────────────────────────────────────────────

#[derive(Default)]
pub struct LObject {
    /// (type_tag, key, raw_value) in source order
    pub props: Vec<(String, String, String)>,
    pub children: Vec<LObject>,
    pub logic: Option<Vec<LogicObj>>,
    pub timeline: Option<Vec<TimelineObj>>,
}

pub struct LogicObj {
    pub id: i64,
    pub objectid: i64,
    pub x: f64,
    pub y: f64,
    pub links: Vec<LogicLink>,
}

pub struct LogicLink {
    pub linkingto: i64,
    pub output: String,
    pub input: String,
}

pub struct TimelineObj {
    pub objectid: i64,
    pub entries: Vec<TimelineEntry>,
}

pub struct TimelineEntry {
    pub property: String,
    pub event: String,
    pub points: Vec<TimelinePoint>,
}

pub struct TimelinePoint {
    pub time: f64,
    pub interp: String,
    pub value: String,
}

/// `_PROP_RE = <([^>]+)>([^:]+):(.*)` anchored at line start (re.match).
fn match_prop(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix('<')?;
    let gt = rest.find('>')?;
    if gt == 0 {
        return None; // ([^>]+) needs at least one char
    }
    let tag = &rest[..gt];
    let after = &rest[gt + 1..];
    let colon = after.find(':')?;
    if colon == 0 {
        return None; // ([^:]+) needs at least one char
    }
    Some((tag, &after[..colon], &after[colon + 1..]))
}

/// Python `_parse_int`: strip; 0x hex; "."/"e"/"E" -> trunc(float); else int.
fn parse_int(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let tl = t.to_lowercase();
    if tl.starts_with("0x") {
        return i64::from_str_radix(&t[2..], 16).map_err(|e| format!("int({t:?}): {e}"));
    }
    if t.contains('.') || tl.contains('e') {
        let f: f64 = t.parse().map_err(|e| format!("float({t:?}): {e}"))?;
        return Ok(f.trunc() as i64);
    }
    t.parse::<i64>().map_err(|e| format!("int({t:?}): {e}"))
}

fn parse_f64(s: &str) -> Result<f64, String> {
    s.trim().parse::<f64>().map_err(|e| format!("float({s:?}): {e}"))
}

fn read_text(data: &[u8]) -> String {
    let raw = if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        &data[2..]
    } else {
        data
    };
    decode_utf16le_lossy(raw)
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

/// Trim exactly like Python: l.rstrip("\r").rstrip("\t").lstrip("\t ")
fn py_trim(l: &str) -> &str {
    let l = l.trim_end_matches('\r');
    let l = l.trim_end_matches('\t');
    l.trim_start_matches(['\t', ' '])
}

impl<'a> Parser<'a> {
    fn new(txt: &'a str) -> Self {
        Parser {
            lines: txt.split('\n').map(py_trim).collect(),
            pos: 0,
        }
    }

    fn line(&self) -> &str {
        self.lines[self.pos]
    }

    fn baseobjs(&mut self, end_tag: &str) -> Result<Vec<LObject>, String> {
        let mut objs = Vec::new();
        while self.pos < self.lines.len() {
            let l = self.line();
            if l == end_tag {
                return Ok(objs);
            }
            if l == "[BASEOBJECT]" {
                self.pos += 1;
                let mut obj = LObject::default();
                while self.pos < self.lines.len() {
                    let l = self.line();
                    if l == "[/BASEOBJECT]" {
                        self.pos += 1;
                        break;
                    }
                    if l == "[PROPERTIES]" {
                        self.pos += 1;
                        while self.pos < self.lines.len() && self.line() != "[/PROPERTIES]" {
                            let ln = self.line();
                            if ln == "[LOGICGROUP]" {
                                obj.logic = Some(self.parse_logicgroup()?);
                                continue;
                            }
                            if ln == "[TIMELINEDATA]" {
                                obj.timeline = Some(self.parse_timelinedata()?);
                                continue;
                            }
                            if let Some((t, k, v)) = match_prop(ln) {
                                obj.props.push((
                                    t.trim().to_string(),
                                    k.trim().to_string(),
                                    v.to_string(),
                                ));
                            }
                            self.pos += 1;
                        }
                        self.pos += 1;
                    } else if l == "[CHILDREN]" {
                        self.pos += 1;
                        obj.children = self.baseobjs("[/CHILDREN]")?;
                        self.pos += 1;
                    } else {
                        self.pos += 1;
                    }
                }
                objs.push(obj);
            } else {
                self.pos += 1;
            }
        }
        Ok(objs)
    }

    fn parse_logicgroup(&mut self) -> Result<Vec<LogicObj>, String> {
        self.pos += 1; // consume [LOGICGROUP]
        let mut lobjs = Vec::new();
        while self.pos < self.lines.len() {
            let l = self.line();
            if l == "[/LOGICGROUP]" {
                self.pos += 1;
                break;
            }
            if l == "[LOGICOBJECT]" {
                self.pos += 1;
                let mut lo = LogicObj {
                    id: 0,
                    objectid: 0,
                    x: 0.0,
                    y: 0.0,
                    links: Vec::new(),
                };
                while self.pos < self.lines.len() {
                    let l = self.line();
                    if l == "[/LOGICOBJECT]" {
                        self.pos += 1;
                        break;
                    }
                    if l == "[LOGICLINK]" {
                        self.pos += 1;
                        let mut link = LogicLink {
                            linkingto: 0,
                            output: String::new(),
                            input: String::new(),
                        };
                        while self.pos < self.lines.len() {
                            let l = self.line();
                            if l == "[/LOGICLINK]" {
                                self.pos += 1;
                                break;
                            }
                            if let Some((_t, k, v)) = match_prop(l) {
                                match k.trim() {
                                    "LINKINGTO" => link.linkingto = parse_int(v)?,
                                    "OUTPUTNAME" => link.output = v.to_string(),
                                    "INPUTNAME" => link.input = v.to_string(),
                                    _ => {}
                                }
                            }
                            self.pos += 1;
                        }
                        lo.links.push(link);
                        continue;
                    }
                    if let Some((_t, k, v)) = match_prop(l) {
                        match k.trim() {
                            "ID" => lo.id = parse_int(v)?,
                            "OBJECTID" => lo.objectid = parse_int(v)?,
                            "X" => lo.x = parse_f64(v)?,
                            "Y" => lo.y = parse_f64(v)?,
                            _ => {}
                        }
                    }
                    self.pos += 1;
                }
                lobjs.push(lo);
            } else {
                self.pos += 1;
            }
        }
        Ok(lobjs)
    }

    fn parse_timelinedata(&mut self) -> Result<Vec<TimelineObj>, String> {
        self.pos += 1; // consume [TIMELINEDATA]
        let mut tobjs = Vec::new();
        while self.pos < self.lines.len() {
            let l = self.line();
            if l == "[/TIMELINEDATA]" {
                self.pos += 1;
                break;
            }
            if l == "[TIMELINEOBJECT]" {
                self.pos += 1;
                let mut to = TimelineObj {
                    objectid: 0,
                    entries: Vec::new(),
                };
                while self.pos < self.lines.len() {
                    let l = self.line();
                    if l == "[/TIMELINEOBJECT]" {
                        self.pos += 1;
                        break;
                    }
                    if l == "[TIMELINEOBJECTEVENT]" || l == "[TIMELINEOBJECTPROPERTY]" {
                        let end = format!("[/{}", &l[1..]);
                        self.pos += 1;
                        let mut entry = TimelineEntry {
                            property: String::new(),
                            event: String::new(),
                            points: Vec::new(),
                        };
                        while self.pos < self.lines.len() {
                            let l = self.line();
                            if l == end {
                                self.pos += 1;
                                break;
                            }
                            if l == "[TIMELINEPOINT]" {
                                self.pos += 1;
                                let mut pt = TimelinePoint {
                                    time: 0.0,
                                    interp: "Linear".to_string(),
                                    value: String::new(),
                                };
                                while self.pos < self.lines.len() {
                                    let l = self.line();
                                    if l == "[/TIMELINEPOINT]" {
                                        self.pos += 1;
                                        break;
                                    }
                                    if let Some((_t, k, v)) = match_prop(l) {
                                        match k.trim() {
                                            "TIMEPERCENT" => pt.time = parse_f64(v)?,
                                            "INTERPOLATION" => pt.interp = v.to_string(),
                                            "VALUE" => pt.value = v.to_string(),
                                            _ => {}
                                        }
                                    }
                                    self.pos += 1;
                                }
                                entry.points.push(pt);
                                continue;
                            }
                            if let Some((_t, k, v)) = match_prop(l) {
                                match k.trim() {
                                    "OBJECTEVENTNAME" => entry.event = v.to_string(),
                                    "OBJECTPROPERTYNAME" => entry.property = v.to_string(),
                                    _ => {}
                                }
                            }
                            self.pos += 1;
                        }
                        to.entries.push(entry);
                        continue;
                    }
                    if let Some((_t, k, v)) = match_prop(l) {
                        if k.trim() == "OBJECTID" {
                            to.objectid = parse_int(v)?;
                        }
                    }
                    self.pos += 1;
                }
                tobjs.push(to);
            } else {
                self.pos += 1;
            }
        }
        Ok(tobjs)
    }
}

/// Parse a .LAYOUT into top-level objects (property order preserved).
pub fn parse_layout(data: &[u8]) -> Result<Vec<LObject>, String> {
    let txt = read_text(data);
    let mut p = Parser::new(&txt);
    while p.pos < p.lines.len() && p.line() != "[OBJECTS]" {
        p.pos += 1;
    }
    p.pos += 1;
    if p.pos > p.lines.len() {
        p.pos = p.lines.len();
    }
    p.baseobjs("[/OBJECTS]")
}

// ── property grouping ──────────────────────────────────────────────────────

/// (name, type_tag, values) with meta keys skipped and X/Y/Z(/W) folded.
fn grouped_props(tprops: &[(String, String, String)]) -> Vec<(String, Vec<String>)> {
    let mut seq = Vec::new();
    let n = tprops.len();
    let mut i = 0;
    let mut seen_name = false;
    let mut seen_choice = false;
    let mut seen_gamemode = false;
    let identity_name = tprops
        .iter()
        .find(|(_t, k, _v)| k == "NAME")
        .map(|(_t, _k, v)| v.clone())
        .unwrap_or_default();
    while i < n {
        let (_t, k, v) = &tprops[i];
        if k == "DESCRIPTOR"
            || k == "PARENTID"
            || k == "ID"
            || (k == "NAME" && !seen_name)
            || (k == "CHOICE" && !seen_choice)
            || (k == "GAME MODE" && !seen_gamemode)
        {
            if k == "NAME" {
                seen_name = true;
            }
            if k == "CHOICE" {
                seen_choice = true;
            }
            if k == "GAME MODE" {
                seen_gamemode = true;
            }
            i += 1;
            continue;
        }
        let v = if k == "NAME" { identity_name.clone() } else { v.clone() };
        // Transform grouping applies ONLY to concatenated names (POSITIONX...):
        // non-empty base that does not end in a space.
        let base = if k.ends_with('X') { &k[..k.len() - 1] } else { "" };
        if k.ends_with('X')
            && !base.is_empty()
            && !base.ends_with(' ')
            && i + 1 < n
            && tprops[i + 1].1 == format!("{base}Y")
        {
            if i + 2 < n && tprops[i + 2].1 == format!("{base}Z") {
                if i + 3 < n && tprops[i + 3].1 == format!("{base}W") {
                    seq.push((
                        base.to_string(),
                        vec![v, tprops[i + 1].2.clone(), tprops[i + 2].2.clone(), tprops[i + 3].2.clone()],
                    ));
                    i += 4;
                } else {
                    seq.push((
                        base.to_string(),
                        vec![v, tprops[i + 1].2.clone(), tprops[i + 2].2.clone()],
                    ));
                    i += 3;
                }
            } else {
                seq.push((base.to_string(), vec![v, tprops[i + 1].2.clone()]));
                i += 2;
            }
        } else {
            seq.push((k.clone(), vec![v]));
            i += 1;
        }
    }
    seq
}

/// DESCRIPTOR (last), NAME (first), ID (last) from an object's props.
fn obj_meta(obj: &LObject) -> (Option<&str>, &str, Option<&str>) {
    let mut desc = None;
    let mut name: Option<&str> = None;
    let mut oid = None;
    for (_t, k, v) in &obj.props {
        match k.as_str() {
            "DESCRIPTOR" => desc = Some(v.as_str()),
            "NAME" if name.is_none() => name = Some(v.as_str()),
            "ID" => oid = Some(v.as_str()),
            _ => {}
        }
    }
    (desc, name.unwrap_or(""), oid)
}

// ── value encoders ─────────────────────────────────────────────────────────

fn f32le(x: f64) -> [u8; 4] {
    (x as f32).to_le_bytes()
}

fn enc_short_string(s: &str, out: &mut Vec<u8>) {
    let units: Vec<u16> = s.encode_utf16().collect();
    out.extend_from_slice(&(units.len() as u16).to_le_bytes());
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
}

fn encode_value(enc: &str, vals: &[String], out: &mut Vec<u8>) -> Result<(), String> {
    match enc {
        "float" => out.extend_from_slice(&f32le(parse_f64(&vals[0])?)),
        "u32int" => out.extend_from_slice(&(parse_int(&vals[0])? as i32).to_le_bytes()),
        "u32bool" => {
            let s = vals[0].trim().to_lowercase();
            let v: u32 = if matches!(s.as_str(), "false" | "0" | "f" | "") { 0 } else { 1 };
            out.extend_from_slice(&v.to_le_bytes());
        }
        "int64" => {
            let t = vals[0].trim();
            let v = if t.is_empty() { 0 } else { t.parse::<i64>().unwrap_or(0) };
            out.extend_from_slice(&v.to_le_bytes());
        }
        "u32list" => {
            let s = vals[0].trim();
            let parts: Vec<&str> = if s.is_empty() {
                Vec::new()
            } else {
                s.split(',').filter(|p| !p.trim().is_empty()).collect()
            };
            out.extend_from_slice(&(parts.len() as u16).to_le_bytes());
            for p in parts {
                let v = p.trim().parse::<i64>().unwrap_or(0);
                out.extend_from_slice(&((v as u64 & 0xFFFF_FFFF) as u32).to_le_bytes());
            }
        }
        "string" => enc_short_string(&vals[0], out),
        "g2" => {
            out.extend_from_slice(&f32le(parse_f64(&vals[0])?));
            out.extend_from_slice(&f32le(parse_f64(&vals[1])?));
        }
        "g3" => {
            out.extend_from_slice(&f32le(parse_f64(&vals[0])?));
            out.extend_from_slice(&f32le(parse_f64(&vals[1])?));
            out.extend_from_slice(&f32le(parse_f64(&vals[2])?));
        }
        "curve" => {
            let parts: Vec<&str> = if vals[0].is_empty() {
                Vec::new()
            } else {
                vals[0].split(',').collect()
            };
            out.extend_from_slice(&(parts.len() as u16).to_le_bytes());
            for p in parts {
                out.extend_from_slice(&f32le(parse_f64(p)?));
            }
        }
        other => return Err(format!("unknown encoder {other:?}")),
    }
    Ok(())
}

// ── sub-structure serializers ──────────────────────────────────────────────

fn serialize_logicgroup(lobjs: &[LogicObj], out: &mut Vec<u8>) {
    out.push(lobjs.len() as u8);
    for lo in lobjs {
        out.push((lo.id & 0xFF) as u8);
        out.extend_from_slice(&lo.objectid.to_le_bytes());
        out.extend_from_slice(&f32le(lo.x));
        out.extend_from_slice(&f32le(lo.y));
        let end_pos = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]); // absolute end_offset placeholder
        out.push(lo.links.len() as u8);
        for link in &lo.links {
            out.push((link.linkingto & 0xFF) as u8);
            enc_short_string(&link.output, out);
            enc_short_string(&link.input, out);
        }
        let end = out.len() as u32;
        out[end_pos..end_pos + 4].copy_from_slice(&end.to_le_bytes());
    }
}

fn interp_index(name: &str) -> u8 {
    match name {
        "Linear" => 0,
        "Linear Round" => 1,
        "Linear Round Down" => 2,
        "Linear Round Up" => 3,
        "Spline" => 4,
        "Quaternion" => 5,
        "No Interpolation" => 6,
        "Use Timeline Default" => 7,
        _ => 0,
    }
}

fn serialize_timelinedata(tobjs: &[TimelineObj], out: &mut Vec<u8>) {
    out.push(tobjs.len() as u8);
    for to in tobjs {
        out.extend_from_slice(&to.objectid.to_le_bytes());
        // stable partition: all PROPERTIES first, then all EVENTS
        let ordered: Vec<&TimelineEntry> = to
            .entries
            .iter()
            .filter(|e| e.event.is_empty())
            .chain(to.entries.iter().filter(|e| !e.event.is_empty()))
            .collect();
        out.push(ordered.len() as u8);
        for ent in ordered {
            enc_short_string(&ent.property, out);
            enc_short_string(&ent.event, out);
            let is_property = ent.event.is_empty();
            out.push(ent.points.len() as u8);
            for pt in &ent.points {
                out.extend_from_slice(&f32le(pt.time));
                out.push(interp_index(&pt.interp));
                if is_property {
                    enc_short_string(&pt.value, out);
                }
            }
        }
    }
}

// ── per-object serializer ──────────────────────────────────────────────────

/// Orientation default-skip: FORWARD(40) Z==1, RIGHT(41) X==1, UP(95) Y==1 —
/// gated on the prop being a 3-component g3 (codes are overloaded in UI).
fn skip_default_orientation(code: u8, enc: &str, vals: &[String]) -> bool {
    let idx = match code {
        40 => 2usize,
        41 => 0,
        95 => 1,
        _ => return false,
    };
    if enc != "g3" || idx >= vals.len() {
        return false;
    }
    match vals[idx].trim().parse::<f64>() {
        Ok(f) => f == 1.0,
        Err(_) => false,
    }
}

fn serialize_object(
    obj: &LObject,
    out: &mut Vec<u8>,
    offsets: &mut HashMap<i64, u32>,
) -> Result<(), String> {
    let (desc, name, oid_s) = obj_meta(obj);
    let desc = desc.unwrap_or("");
    let dinfo = schema()
        .get(desc)
        .filter(|d| d.code.is_some())
        .ok_or_else(|| format!("unknown DESCRIPTOR {desc:?} (no schema)"))?;

    let block_start = out.len();
    let oid: i64 = match oid_s {
        Some(s) => {
            let v = parse_int(s)?;
            offsets.insert(v, block_start as u32);
            v
        }
        None => 0,
    };
    out.extend_from_slice(&[0, 0, 0, 0]); // block_size placeholder
    out.push((dinfo.code.unwrap() & 0xFF) as u8);
    out.extend_from_slice(&oid.to_le_bytes());
    let name_field = if name != desc { name } else { "" };
    enc_short_string(name_field, out);

    // Emit properties in SOURCE order; schema supplies code + encoder.
    let mut emitted: Vec<(u8, &str, Vec<String>)> = Vec::new();
    for (nm, vals) in grouped_props(&obj.props) {
        let spec = match dinfo.props.get(&nm) {
            Some(s) => s,
            None => continue,
        };
        if skip_default_orientation(spec.code, &spec.enc, &vals) {
            continue;
        }
        emitted.push((spec.code, spec.enc.as_str(), vals));
    }
    out.push(emitted.len() as u8);
    for (code, enc, vals) in &emitted {
        let mut body = vec![*code];
        encode_value(enc, vals, &mut body)?;
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&body);
    }

    // ADPROP region (logic graph / timeline graph), else 0.
    let adprop_pos = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    if let Some(logic) = &obj.logic {
        serialize_logicgroup(logic, out);
        let n = (out.len() - (adprop_pos + 4)) as u32;
        out[adprop_pos..adprop_pos + 4].copy_from_slice(&n.to_le_bytes());
    } else if let Some(timeline) = &obj.timeline {
        serialize_timelinedata(timeline, out);
        let n = (out.len() - (adprop_pos + 4)) as u32;
        out[adprop_pos..adprop_pos + 4].copy_from_slice(&n.to_le_bytes());
    }

    out.extend_from_slice(&(obj.children.len() as u16).to_le_bytes());
    for ch in &obj.children {
        serialize_object(ch, out, offsets)?;
    }

    let block_size = (out.len() - block_start) as u32;
    out[block_start..block_start + 4].copy_from_slice(&block_size.to_le_bytes());
    Ok(())
}

// ── datagroup (CLayoutBinaryGroup tree) ────────────────────────────────────

struct DgNode {
    id: i64,
    /// key -> value, LAST occurrence wins (Python dict comprehension).
    props: HashMap<String, String>,
    children: Vec<DgNode>,
}

fn has_group(objs: &[LObject]) -> bool {
    objs.iter().any(|o| {
        let (desc, _, _) = obj_meta(o);
        desc == Some("Group") || has_group(&o.children)
    })
}

fn collect_group_nodes(objs: &[LObject]) -> Result<Vec<DgNode>, String> {
    let mut nodes = Vec::new();
    for o in objs {
        let (desc, _, oid) = obj_meta(o);
        let child_nodes = collect_group_nodes(&o.children)?;
        if desc == Some("Group") {
            let mut tprops = HashMap::new();
            for (_t, k, v) in &o.props {
                tprops.insert(k.clone(), v.clone());
            }
            let id = match oid {
                Some(s) => parse_int(s)?,
                None => -1,
            };
            nodes.push(DgNode {
                id,
                props: tprops,
                children: child_nodes,
            });
        } else {
            nodes.extend(child_nodes);
        }
    }
    Ok(nodes)
}

fn choice_index(s: &str) -> u8 {
    match s.trim().to_uppercase().as_str() {
        "ALL" => 0,
        "WEIGHT" => 1,
        "RANDOM CHANCE" => 2,
        _ => 0,
    }
}

fn gamemode_index(s: &str) -> u8 {
    match s.trim().to_uppercase().as_str() {
        "NORMAL" => 1,
        "NEW GAME PLUS" => 2,
        _ => 0,
    }
}

fn parse_id_list(s: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if !p.is_empty() {
            out.push(p.parse::<i64>().map_err(|e| format!("int({p:?}): {e}"))?);
        }
    }
    Ok(out)
}

fn emit_dg_node(
    node: &DgNode,
    off28: u32,
    offsets: &HashMap<i64, u32>,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let tp = &node.props;
    out.extend_from_slice(&node.id.to_le_bytes());
    // @16 CHOICE index
    out.push(choice_index(tp.get("CHOICE").map(String::as_str).unwrap_or("")));
    // @20 RANDOMIZATION (default 1)
    let rnd: u32 = match tp.get("RANDOMIZATION") {
        None => 1,
        Some(v) => (parse_int(v)? as u64 & 0xFFFF_FFFF) as u32,
    };
    out.extend_from_slice(&rnd.to_le_bytes());
    // @24 NUMBER (default 1)
    let num: i64 = match tp.get("NUMBER") {
        None => 1,
        Some(v) => parse_int(v)?,
    };
    out.push((num & 0xFF) as u8);
    // @28 stream offset of this Group's object block
    out.extend_from_slice(&off28.to_le_bytes());
    // @92 TAG id (default 0xFFFFFFFF)
    let tag = tp
        .get("TAG")
        .map(|s| s.trim().to_uppercase())
        .unwrap_or_default();
    let tagid: u32 = if tag.is_empty() {
        0xFFFF_FFFF
    } else {
        dgtags().get(&tag).copied().unwrap_or(0xFFFF_FFFF)
    };
    out.extend_from_slice(&tagid.to_le_bytes());
    // @25 NO TAG FOUND, @26 LEVEL UNIQUE, @27 GAME MODE
    out.push(0);
    let lu = tp
        .get("LEVEL UNIQUE")
        .map(|s| s.trim().to_lowercase() == "true")
        .unwrap_or(false);
    out.push(lu as u8);
    out.push(gamemode_index(tp.get("GAME MODE").map(String::as_str).unwrap_or("")));
    // active / deactive theme id lists
    let active = parse_id_list(tp.get("ACTIVE THEMES").map(String::as_str).unwrap_or(""))?;
    out.extend_from_slice(&(active.len() as u32).to_le_bytes());
    for v in &active {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let deactive = parse_id_list(tp.get("DEACTIVE THEMES").map(String::as_str).unwrap_or(""))?;
    out.extend_from_slice(&(deactive.len() as u32).to_le_bytes());
    for v in &deactive {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(node.children.len() as u16).to_le_bytes());
    for ch in &node.children {
        emit_dg_node(ch, offsets.get(&ch.id).copied().unwrap_or(0), offsets, out)?;
    }
    Ok(())
}

fn build_datagroup(
    objs: &[LObject],
    offsets: &HashMap<i64, u32>,
    root_off28: u32,
) -> Result<Option<Vec<u8>>, String> {
    if !has_group(objs) {
        return Ok(None);
    }
    let top_nodes = collect_group_nodes(objs)?;
    let mut out = Vec::new();
    // synthetic root: id=-1, all fields 0, @28 = tellp right after the header
    out.extend_from_slice(&(-1i64).to_le_bytes());
    out.push(0); // @16
    out.extend_from_slice(&0u32.to_le_bytes()); // @20 (root randomization = 0)
    out.push(0); // @24 (root number = 0)
    out.extend_from_slice(&root_off28.to_le_bytes()); // @28
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // @92 tag
    out.extend_from_slice(&[0, 0, 0]); // @25 @26 @27
    out.extend_from_slice(&0u32.to_le_bytes()); // active count
    out.extend_from_slice(&0u32.to_le_bytes()); // deactive count
    out.extend_from_slice(&(top_nodes.len() as u16).to_le_bytes());
    for ch in &top_nodes {
        emit_dg_node(ch, offsets.get(&ch.id).copied().unwrap_or(0), offsets, &mut out)?;
    }
    Ok(Some(out))
}

// ── entry points ───────────────────────────────────────────────────────────

/// Compile .LAYOUT bytes into BINLAYOUT bytes from scratch.
pub fn compile_layout_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    let objs = parse_layout(data)?;

    let mut out: Vec<u8> = Vec::new();
    out.push(MAGIC);
    out.push(FLAG);
    let dg_off_pos = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]); // dg_off placeholder
    let root_off28 = out.len() as u32;
    out.extend_from_slice(&(objs.len() as u16).to_le_bytes());

    let mut offsets: HashMap<i64, u32> = HashMap::new();
    for o in &objs {
        serialize_object(o, &mut out, &mut offsets)?;
    }

    if let Some(dg) = build_datagroup(&objs, &offsets, root_off28)? {
        let off = out.len() as u32;
        out[dg_off_pos..dg_off_pos + 4].copy_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&dg);
    }
    Ok(out)
}

/// Compile a .LAYOUT file path.
pub fn compile_layout_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    compile_layout_bytes(&data)
}
