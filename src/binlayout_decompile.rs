//! BINLAYOUT → text `.LAYOUT` decompiler — the inverse of
//! `binlayout::compile_layout_bytes`.
//!
//! Unlike BINDAT (whose keys survive only as one-way hashes), a BINLAYOUT is
//! fully self-describing given the schema: every property is `[u8 code][value]`
//! under a descriptor whose own code names it, so the SAME embedded schema the
//! compiler writes with — `data/binlayout_schema.json`, inverted here — names
//! everything back. No dictionary, no guessing.
//!
//! Information the compiler drops is reconstructed rather than read back, in a
//! way that re-compiles to the identical bytes:
//!   * `PARENTID` — never serialized; recovered from the object tree.
//!   * `CHOICE` / `GAME MODE` / `TAG` / `RANDOMIZATION` / `NUMBER` /
//!     `NO TAG FOUND` / `LEVEL UNIQUE` / `ACTIVE|DEACTIVE THEMES` on a Group —
//!     not in the Group descriptor's property schema at all; they live only in
//!     the trailing datagroup, which is parsed and merged back onto the Group
//!     objects (the datagroup tree is the object tree with non-Groups elided,
//!     so the two zip structurally).
//!   * defaults the compiler substitutes for an absent key (RANDOMIZATION 1,
//!     NUMBER 1, TAG none) — emitted only when they differ from the default.
//!
//! One class of property is deliberately NOT reconstructed: a default-valued
//! `FORWARD`/`RIGHT`/`UP` (`binlayout::skip_default_orientation` drops those
//! whose axis component is 1, because the reader supplies the default). The
//! shipped `.LAYOUT` sources omit them too, so leaving them out reproduces a
//! normal source — whereas writing the unit axis back would ASSERT an
//! orientation to every downstream reader, and a reader that derives `UP` from
//! `FORWARD × RIGHT` would then take the injected `0,1,0` over the real one.
//!
//! Correctness criterion (see `verify binlayout-roundtrip`): decompiling a
//! BINLAYOUT and re-compiling the text must reproduce the input byte for byte.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use crate::binlayout::{choice_name, datagroup_tags, gamemode_name, interp_name, schema_map};

// ── inverted schema ────────────────────────────────────────────────────────

struct DescInfo {
    name: &'static str,
    /// property code → (key, encoder)
    props: HashMap<u8, (&'static str, &'static str)>,
}

/// descriptor code (as written: `code & 0xFF`) → descriptor.
fn by_code() -> &'static HashMap<u8, DescInfo> {
    static IDX: OnceLock<HashMap<u8, DescInfo>> = OnceLock::new();
    IDX.get_or_init(|| {
        let mut m: HashMap<u8, DescInfo> = HashMap::new();
        for (name, d) in schema_map() {
            let Some(code) = d.code else { continue };
            let mut props = HashMap::new();
            for (k, spec) in &d.props {
                props.insert(spec.code, (k.as_str(), spec.enc.as_str()));
            }
            m.insert((code & 0xFF) as u8, DescInfo { name: name.as_str(), props });
        }
        m
    })
}

/// datagroup TAG id → name.
fn tag_by_id() -> &'static HashMap<u32, &'static str> {
    static IDX: OnceLock<HashMap<u32, &'static str>> = OnceLock::new();
    IDX.get_or_init(|| {
        datagroup_tags()
            .iter()
            .map(|(name, id)| (*id, name.as_str()))
            .collect()
    })
}

/// The `u32int` keys written as `<UNSIGNED INT>` rather than `<INTEGER>`.
///
/// The DLL's descriptor schema knows each property's real UI type, but the
/// embedded dump (`binlayout_schema.json`) collapses both onto `enc: u32int`, so
/// the spelling is recovered from usage instead: every `u32int` key in the
/// shipped `.LAYOUT` corpus whose INTEGER/UNSIGNED-INT usage is dominantly
/// unsigned, plus three keys that never appear in shipped text but that GUTS'
/// own unpacker writes unsigned (`SECONDARY INDEX`, `INDEX_`,
/// `SECONDARY INDEX_`). Anything unlisted defaults to `<INTEGER>`.
///
/// Purely cosmetic for correctness — `bindat`/`binlayout` read key+value and
/// ignore the tag — but it is what makes a decompiled file diff cleanly against
/// its original source and against GUTS' output.
const UNSIGNED_INT_KEYS: [&str; 19] = [
    "BRIGHTNESS",
    "COUNT",
    "CURRENT STATE",
    "FAME",
    "GOLD",
    "INDEX",
    "INDEX_",
    "MAX ALLOWED",
    "MAX LENGTH",
    "NUM RICOCHETS",
    "NUMBER OF POINTS",
    "NUMBER REQUIRED",
    "RENDER ORDER",
    "SECONDARY INDEX",
    "SECONDARY INDEX_",
    "SEGMENTS",
    "SELECTED INDEX",
    "VISUAL QUOTA",
    "XP",
];

fn type_tag(key: &str, enc: &str) -> &'static str {
    match enc {
        "u32bool" => "BOOL",
        "int64" => "INTEGER64",
        // curve / u32list are comma-joined scalars inside one STRING value.
        "string" | "curve" | "u32list" => "STRING",
        "u32int" => {
            if UNSIGNED_INT_KEYS.contains(&key) {
                "UNSIGNED INT"
            } else {
                "INTEGER"
            }
        }
        _ => "FLOAT", // float, g2, g3
    }
}

/// Shortest decimal that parses back to the same f32 (Rust's Display already
/// guarantees round-trip; going through f64 on the way back in is safe because
/// f64 has more than 2p+2 bits for f32's p=24, so the double rounding is exact).
///
/// Float spelling is shared with the BINDAT decompiler — see
/// [`crate::text::fmt_f32_source`] for why it follows `%g`'s CHOICE of notation
/// but not its digits.
use crate::text::fmt_f32_source as fmt_f32;

// ── byte reader ────────────────────────────────────────────────────────────

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.p).ok_or("binlayout: eof (u8)")?;
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let s = self.b.get(self.p..self.p + 2).ok_or("binlayout: eof (u16)")?;
        self.p += 2;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.p..self.p + 4).ok_or("binlayout: eof (u32)")?;
        self.p += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i64(&mut self) -> Result<i64, String> {
        let s = self.b.get(self.p..self.p + 8).ok_or("binlayout: eof (i64)")?;
        self.p += 8;
        Ok(i64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self
            .b
            .get(self.p..self.p + n)
            .ok_or_else(|| format!("binlayout: eof (take {n})"))?;
        self.p += n;
        Ok(s)
    }
    /// Short string: `u16` UTF-16LE unit count, then the units.
    fn ss(&mut self) -> Result<String, String> {
        let n = self.u16()? as usize;
        let bytes = self.take(2 * n)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }
}

// ── parsed shapes ──────────────────────────────────────────────────────────

/// One decoded property: key, type tag, and 1..4 component values (a g2/g3 is
/// re-split into the `KEYX`/`KEYY`/`KEYZ` lines the compiler folds back).
struct Prop {
    key: &'static str,
    tag: &'static str,
    vals: Vec<String>,
}

struct LogicLink {
    linkingto: u8,
    output: String,
    input: String,
}

struct LogicObj {
    id: u8,
    objectid: i64,
    x: f32,
    y: f32,
    links: Vec<LogicLink>,
}

struct TlPoint {
    time: f32,
    interp: u8,
    value: Option<String>,
}

struct TlEntry {
    property: String,
    event: String,
    points: Vec<TlPoint>,
}

struct TlObj {
    objectid: i64,
    entries: Vec<TlEntry>,
}

enum Adprop {
    None,
    Logic(Vec<LogicObj>),
    Timeline(Vec<TlObj>),
}

struct Obj {
    desc: &'static str,
    oid: i64,
    name: String,
    props: Vec<Prop>,
    adprop: Adprop,
    children: Vec<Obj>,
    /// Group-only fields merged in from the datagroup.
    dg: Option<DgFields>,
}

#[derive(Default)]
struct DgFields {
    id: i64,
    choice: u8,
    randomization: u32,
    number: u8,
    tag: u32,
    no_tag_found: bool,
    level_unique: bool,
    game_mode: u8,
    active: Vec<i64>,
    deactive: Vec<i64>,
}

struct DgNode {
    f: DgFields,
    children: Vec<DgNode>,
}

// ── object stream ──────────────────────────────────────────────────────────

fn decode_value(enc: &str, body: &[u8]) -> Result<Vec<String>, String> {
    let mut r = Rd { b: body, p: 0 };
    Ok(match enc {
        "float" => vec![fmt_f32(r.f32()?)],
        "u32int" => vec![(r.u32()? as i32).to_string()],
        "u32bool" => vec![if r.u32()? != 0 { "true" } else { "false" }.to_string()],
        "int64" => vec![r.i64()?.to_string()],
        "string" => vec![r.ss()?],
        "g2" | "g3" => {
            let n = if enc == "g2" { 2 } else { 3 };
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(fmt_f32(r.f32()?));
            }
            v
        }
        "u32list" => {
            let n = r.u16()? as usize;
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                parts.push(r.u32()?.to_string());
            }
            vec![parts.join(",")]
        }
        "curve" => {
            let n = r.u16()? as usize;
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                parts.push(fmt_f32(r.f32()?));
            }
            vec![parts.join(",")]
        }
        other => return Err(format!("unknown encoder {other:?}")),
    })
}

fn read_logicgroup(body: &[u8]) -> Result<Vec<LogicObj>, String> {
    let mut r = Rd { b: body, p: 0 };
    let n = r.u8()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let id = r.u8()?;
        let objectid = r.i64()?;
        let x = r.f32()?;
        let y = r.f32()?;
        let _end_off = r.u32()?; // absolute stream offset, recomputed on compile
        let nl = r.u8()? as usize;
        let mut links = Vec::with_capacity(nl);
        for _ in 0..nl {
            links.push(LogicLink {
                linkingto: r.u8()?,
                output: r.ss()?,
                input: r.ss()?,
            });
        }
        out.push(LogicObj { id, objectid, x, y, links });
    }
    Ok(out)
}

fn read_timelinedata(body: &[u8]) -> Result<Vec<TlObj>, String> {
    let mut r = Rd { b: body, p: 0 };
    let n = r.u8()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let objectid = r.i64()?;
        let ne = r.u8()? as usize;
        let mut entries = Vec::with_capacity(ne);
        for _ in 0..ne {
            let property = r.ss()?;
            let event = r.ss()?;
            // An EVENT entry stores no per-point value; a PROPERTY entry does.
            let is_property = event.is_empty();
            let np = r.u8()? as usize;
            let mut points = Vec::with_capacity(np);
            for _ in 0..np {
                let time = r.f32()?;
                let interp = r.u8()?;
                let value = if is_property { Some(r.ss()?) } else { None };
                points.push(TlPoint { time, interp, value });
            }
            entries.push(TlEntry { property, event, points });
        }
        out.push(TlObj { objectid, entries });
    }
    Ok(out)
}

fn read_object(r: &mut Rd) -> Result<Obj, String> {
    let start = r.p;
    let size = r.u32()? as usize;
    let end = start
        .checked_add(size)
        .ok_or("binlayout: object size overflow")?;
    if size < 4 || end > r.b.len() {
        return Err(format!("binlayout: object at {start} claims {size} bytes"));
    }
    let code = r.u8()?;
    let oid = r.i64()?;
    let name_field = r.ss()?;
    let di = by_code()
        .get(&code)
        .ok_or_else(|| format!("binlayout: unknown descriptor code {code}"))?;

    let nprops = r.u8()? as usize;
    let mut props = Vec::with_capacity(nprops);
    for _ in 0..nprops {
        let blen = r.u16()? as usize;
        let body = r.take(blen)?;
        let pcode = *body.first().ok_or("binlayout: empty property block")?;
        // A code the descriptor's schema does not know cannot be named, but the
        // block is self-delimiting, so skipping it keeps the rest readable.
        let Some((key, enc)) = di.props.get(&pcode).copied() else {
            continue;
        };
        props.push(Prop {
            key,
            tag: type_tag(key, enc),
            vals: decode_value(enc, &body[1..])?,
        });
    }

    let ad_len = r.u32()? as usize;
    let ad_body = r.take(ad_len)?;
    let adprop = if ad_len == 0 {
        Adprop::None
    } else {
        // The two graph payloads are structurally ambiguous; the descriptor is
        // what disambiguates them (`Logic Group` / `Timeline` are the ONLY
        // descriptors that carry either, across the whole shipped corpus).
        match di.name {
            "Logic Group" => Adprop::Logic(read_logicgroup(ad_body)?),
            "Timeline" => Adprop::Timeline(read_timelinedata(ad_body)?),
            other => {
                return Err(format!(
                    "binlayout: descriptor {other:?} carries a {ad_len}-byte adprop region"
                ))
            }
        }
    };

    let nch = r.u16()? as usize;
    let mut children = Vec::with_capacity(nch);
    for _ in 0..nch {
        children.push(read_object(r)?);
    }
    if r.p != end {
        return Err(format!(
            "binlayout: object {} ended at {} but its block claims {end}",
            di.name, r.p
        ));
    }
    Ok(Obj {
        desc: di.name,
        oid,
        // An empty name field means "same as the descriptor" (compile writes
        // "" exactly when NAME == DESCRIPTOR).
        name: if name_field.is_empty() {
            di.name.to_string()
        } else {
            name_field
        },
        props,
        adprop,
        children,
        dg: None,
    })
}

// ── datagroup ──────────────────────────────────────────────────────────────

fn read_dg_node(r: &mut Rd) -> Result<DgNode, String> {
    let mut f = DgFields {
        id: r.i64()?,
        choice: r.u8()?,
        randomization: r.u32()?,
        number: r.u8()?,
        ..Default::default()
    };
    let _off28 = r.u32()?; // stream offset of the Group's object block
    f.tag = r.u32()?;
    f.no_tag_found = r.u8()? != 0;
    f.level_unique = r.u8()? != 0;
    f.game_mode = r.u8()?;
    let na = r.u32()? as usize;
    for _ in 0..na {
        f.active.push(r.i64()?);
    }
    let nd = r.u32()? as usize;
    for _ in 0..nd {
        f.deactive.push(r.i64()?);
    }
    let nch = r.u16()? as usize;
    let mut children = Vec::with_capacity(nch);
    for _ in 0..nch {
        children.push(read_dg_node(r)?);
    }
    Ok(DgNode { f, children })
}

/// Attach datagroup fields to the Group objects they describe. The datagroup is
/// the object tree with every non-Group elided (`collect_group_nodes`), so a
/// parallel walk in the same order pairs them one-to-one.
fn merge_dg(objs: &mut [Obj], nodes: Vec<DgNode>) -> Result<(), String> {
    let mut it = nodes.into_iter();
    fn walk(objs: &mut [Obj], it: &mut std::vec::IntoIter<DgNode>) -> Result<(), String> {
        for o in objs.iter_mut() {
            if o.desc == "Group" {
                let node = it
                    .next()
                    .ok_or("binlayout: datagroup has fewer nodes than Group objects")?;
                let mut child_it = node.children.into_iter();
                walk(&mut o.children, &mut child_it)?;
                if child_it.next().is_some() {
                    return Err("binlayout: datagroup child count exceeds nested Groups".into());
                }
                o.dg = Some(node.f);
            } else {
                // A non-Group contributes its OWN nested Groups to this level.
                walk(&mut o.children, it)?;
            }
        }
        Ok(())
    }
    walk(objs, &mut it)?;
    if it.next().is_some() {
        return Err("binlayout: datagroup has more nodes than Group objects".into());
    }
    Ok(())
}

// ── text emission ──────────────────────────────────────────────────────────

fn count_objects(objs: &[Obj]) -> usize {
    objs.iter().map(|o| 1 + count_objects(&o.children)).sum()
}

fn line(out: &mut String, depth: usize, s: &str) {
    for _ in 0..depth {
        out.push('\t');
    }
    out.push_str(s);
    out.push_str("\r\n");
}

fn prop_line(out: &mut String, depth: usize, tag: &str, key: &str, val: &str) {
    for _ in 0..depth {
        out.push('\t');
    }
    let _ = write!(out, "<{tag}>{key}:{val}\r\n");
}

/// The datagroup keys that are ALSO ordinary schema properties, so a source can
/// carry two of them. The compiler swallows the FIRST occurrence as the object's
/// metadata (which is what feeds the datagroup) and compiles any later one into
/// the property stream — so when the stream has one, the datagroup copy has to
/// be emitted BEFORE it or the two values would swap on recompile.
const DG_KEYS_ALSO_IN_STREAM: [&str; 2] = ["CHOICE", "GAME MODE"];

/// Emit the datagroup copy of a key the stream also carries, immediately BEFORE
/// the stream copy (which is where the editor puts it, and where the "first
/// occurrence is the metadata" rule assigns it to the datagroup rather than to
/// the property block). Returns whether anything was written.
fn emit_dg_twin(out: &mut String, depth: usize, f: &DgFields, key: &str) -> bool {
    match key {
        "CHOICE" => choice_name(f.choice)
            .map(|c| prop_line(out, depth, "STRING", "CHOICE", c))
            .is_some(),
        "GAME MODE" => gamemode_name(f.game_mode)
            .map(|g| prop_line(out, depth, "STRING", "GAME MODE", g))
            .is_some(),
        _ => false,
    }
}

/// Emit the datagroup-only Group properties, AFTER the stream properties —
/// which is where the editor authors them: across the shipped corpus every one
/// of the 1504 Groups carrying both a datagroup key and a stream key writes the
/// datagroup key last, without exception.
///
/// `stream_keys` are the keys already emitted from the object stream.
/// `LEVEL UNIQUE` is both a schema property AND a datagroup field, so emitting
/// it twice would make the recompiled object carry the property block twice;
/// `CHOICE`/`GAME MODE` went out early when the stream also has them.
fn emit_dg_props(out: &mut String, depth: usize, f: &DgFields, stream_keys: &[&str]) {
    if f.tag != 0xFFFF_FFFF {
        if let Some(name) = tag_by_id().get(&f.tag) {
            prop_line(out, depth, "STRING", "TAG", name);
        }
    }
    if !stream_keys.contains(&"CHOICE") {
        if let Some(c) = choice_name(f.choice) {
            prop_line(out, depth, "STRING", "CHOICE", c);
        }
    }
    if f.randomization != 1 {
        prop_line(out, depth, "UNSIGNED INT", "RANDOMIZATION", &f.randomization.to_string());
    }
    if f.number != 1 {
        prop_line(out, depth, "INTEGER", "NUMBER", &f.number.to_string());
    }
    if f.no_tag_found {
        prop_line(out, depth, "BOOL", "NO TAG FOUND", "true");
    }
    if f.level_unique && !stream_keys.contains(&"LEVEL UNIQUE") {
        prop_line(out, depth, "BOOL", "LEVEL UNIQUE", "true");
    }
    if !stream_keys.contains(&"GAME MODE") {
        if let Some(g) = gamemode_name(f.game_mode) {
            prop_line(out, depth, "STRING", "GAME MODE", g);
        }
    }
    // The two theme lists are written differently by the editor, consistently
    // enough across the shipped corpus to reproduce: ACTIVE THEMES never ends in
    // a comma (1197/1197) while DEACTIVE THEMES does (173/174). The compiler
    // drops empty parts either way, so this is spelling only.
    if !f.active.is_empty() {
        let v: Vec<String> = f.active.iter().map(|i| i.to_string()).collect();
        prop_line(out, depth, "STRING", "ACTIVE THEMES", &v.join(","));
    }
    if !f.deactive.is_empty() {
        let v: Vec<String> = f.deactive.iter().map(|i| i.to_string()).collect();
        prop_line(out, depth, "STRING", "DEACTIVE THEMES", &format!("{},", v.join(",")));
    }
}

fn emit_logicgroup(out: &mut String, depth: usize, lobjs: &[LogicObj]) {
    line(out, depth, "[LOGICGROUP]");
    for lo in lobjs {
        line(out, depth + 1, "[LOGICOBJECT]");
        prop_line(out, depth + 2, "UNSIGNED INT", "ID", &lo.id.to_string());
        prop_line(out, depth + 2, "INTEGER64", "OBJECTID", &lo.objectid.to_string());
        prop_line(out, depth + 2, "FLOAT", "X", &fmt_f32(lo.x));
        prop_line(out, depth + 2, "FLOAT", "Y", &fmt_f32(lo.y));
        for lk in &lo.links {
            line(out, depth + 2, "[LOGICLINK]");
            prop_line(out, depth + 3, "INTEGER", "LINKINGTO", &lk.linkingto.to_string());
            prop_line(out, depth + 3, "STRING", "OUTPUTNAME", &lk.output);
            prop_line(out, depth + 3, "STRING", "INPUTNAME", &lk.input);
            line(out, depth + 2, "[/LOGICLINK]");
        }
        line(out, depth + 1, "[/LOGICOBJECT]");
    }
    line(out, depth, "[/LOGICGROUP]");
}

fn emit_timelinedata(out: &mut String, depth: usize, oid: i64, tobjs: &[TlObj]) {
    line(out, depth, "[TIMELINEDATA]");
    prop_line(out, depth + 1, "INTEGER64", "ID", &oid.to_string());
    for to in tobjs {
        line(out, depth + 1, "[TIMELINEOBJECT]");
        prop_line(out, depth + 2, "INTEGER64", "OBJECTID", &to.objectid.to_string());
        for ent in &to.entries {
            let is_event = !ent.event.is_empty();
            let tagname = if is_event {
                "TIMELINEOBJECTEVENT"
            } else {
                "TIMELINEOBJECTPROPERTY"
            };
            line(out, depth + 2, &format!("[{tagname}]"));
            if is_event {
                prop_line(out, depth + 3, "STRING", "OBJECTEVENTNAME", &ent.event);
            } else {
                prop_line(out, depth + 3, "STRING", "OBJECTPROPERTYNAME", &ent.property);
            }
            for pt in &ent.points {
                line(out, depth + 3, "[TIMELINEPOINT]");
                prop_line(out, depth + 4, "FLOAT", "TIMEPERCENT", &fmt_f32(pt.time));
                prop_line(out, depth + 4, "STRING", "INTERPOLATION", interp_name(pt.interp));
                if let Some(v) = &pt.value {
                    prop_line(out, depth + 4, "STRING", "VALUE", v);
                }
                line(out, depth + 3, "[/TIMELINEPOINT]");
            }
            line(out, depth + 2, &format!("[/{tagname}]"));
        }
        line(out, depth + 1, "[/TIMELINEOBJECT]");
    }
    line(out, depth, "[/TIMELINEDATA]");
}

fn emit_object(out: &mut String, depth: usize, o: &Obj, parent_id: i64) {
    line(out, depth, "[BASEOBJECT]");
    line(out, depth + 1, "[PROPERTIES]");
    let d = depth + 2;
    prop_line(out, d, "STRING", "DESCRIPTOR", o.desc);
    prop_line(out, d, "STRING", "NAME", &o.name);
    prop_line(out, d, "INTEGER64", "PARENTID", &parent_id.to_string());
    // A Group whose datagroup node reports id -1 while the object block carries
    // 0 had NO ID property in the source (compile writes 0 for a missing ID but
    // records -1 in the datagroup); anything else round-trips as written.
    let omit_id = matches!(&o.dg, Some(f) if f.id == -1 && o.oid == 0);
    if !omit_id {
        prop_line(out, d, "INTEGER64", "ID", &o.oid.to_string());
    }
    let stream_keys: Vec<&str> = o.props.iter().map(|p| p.key).collect();
    for p in &o.props {
        // A key that is BOTH a datagroup field and a schema property appears
        // twice in the source; the datagroup copy goes directly in front of the
        // stream one (see `emit_dg_twin`).
        if DG_KEYS_ALSO_IN_STREAM.contains(&p.key) {
            if let Some(f) = &o.dg {
                emit_dg_twin(out, d, f, p.key);
            }
        }
        if p.vals.len() == 1 {
            prop_line(out, d, p.tag, p.key, &p.vals[0]);
        } else {
            // g2/g3: the compiler folds KEYX/KEYY/KEYZ(/KEYW) back together.
            for (i, v) in p.vals.iter().enumerate() {
                let suffix = ["X", "Y", "Z", "W"][i];
                prop_line(out, d, p.tag, &format!("{}{suffix}", p.key), v);
            }
        }
    }
    if let Some(f) = &o.dg {
        emit_dg_props(out, d, f, &stream_keys);
    }
    match &o.adprop {
        Adprop::None => {}
        Adprop::Logic(l) => emit_logicgroup(out, d, l),
        Adprop::Timeline(t) => emit_timelinedata(out, d, o.oid, t),
    }
    line(out, depth + 1, "[/PROPERTIES]");
    if !o.children.is_empty() {
        line(out, depth + 1, "[CHILDREN]");
        let my_id = if omit_id { -1 } else { o.oid };
        for c in &o.children {
            emit_object(out, depth + 2, c, my_id);
        }
        line(out, depth + 1, "[/CHILDREN]");
    }
    line(out, depth, "[/BASEOBJECT]");
}

// ── entry points ───────────────────────────────────────────────────────────

/// The document-type name a `.LAYOUT` opens with, chosen from where the file
/// lives. It is NOT in the binary (the whole shipped corpus has the same magic
/// and flag bytes) — the editor decides it by which asset folder is being
/// edited, and this rule reproduces all 8985 shipped sources exactly:
/// `MEDIA/PARTICLES/**` → `[Particle Creator]` (3501/3501), `MEDIA/UI/**` →
/// `[UI]` (171/171), everything else → `[Layout]` (5294/5294).
///
/// Cosmetic for the compiler, which scans for `[OBJECTS]` and ignores the root
/// tag — but it is what the editor shows and what GUTS' unpacker writes.
pub fn document_type_for(media_rel_path: &str) -> &'static str {
    let p = media_rel_path.replace('\\', "/").to_uppercase();
    let p = p.strip_prefix("MEDIA/").unwrap_or(&p);
    if p.starts_with("PARTICLES/") {
        "Particle Creator"
    } else if p.starts_with("UI/") {
        "UI"
    } else {
        "Layout"
    }
}

/// Decompile BINLAYOUT bytes into the `.LAYOUT` text they were compiled from.
pub fn decompile_binlayout(data: &[u8]) -> Result<String, String> {
    decompile_binlayout_as(data, "Layout")
}

/// `decompile_binlayout`, with the document-type root tag chosen by the caller
/// (see `document_type_for`).
pub fn decompile_binlayout_as(data: &[u8], doc_type: &str) -> Result<String, String> {
    if data.len() < 8 {
        return Err("binlayout: too short".into());
    }
    if data[0] != 0x0B {
        return Err(format!("binlayout: bad magic 0x{:02X}", data[0]));
    }
    let mut r = Rd { b: data, p: 2 };
    let dg_off = r.u32()? as usize;
    let n = r.u16()? as usize;
    let mut objs = Vec::with_capacity(n);
    for _ in 0..n {
        objs.push(read_object(&mut r)?);
    }
    if dg_off != 0 {
        if dg_off > data.len() {
            return Err(format!("binlayout: datagroup offset {dg_off} past eof"));
        }
        let mut dr = Rd { b: data, p: dg_off };
        let root = read_dg_node(&mut dr)?;
        merge_dg(&mut objs, root.children)?;
    }

    let mut out = String::new();
    out.push('\u{FEFF}');
    line(&mut out, 0, &format!("[{doc_type}]"));
    prop_line(&mut out, 1, "INTEGER", "VERSION", "4");
    prop_line(&mut out, 1, "UNSIGNED INT", "COUNT", &count_objects(&objs).to_string());
    line(&mut out, 1, "[OBJECTS]");
    for o in &objs {
        emit_object(&mut out, 2, o, -1);
    }
    line(&mut out, 1, "[/OBJECTS]");
    line(&mut out, 0, &format!("[/{doc_type}]"));
    Ok(out)
}

/// UTF-16LE with BOM — the on-disk `.LAYOUT` encoding (the leading BOM is part
/// of `text`, so it is encoded like any other character).
pub fn to_layout_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for u in text.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binlayout::compile_layout_bytes;

    /// The inverse tables added for this module must agree with the compiler's
    /// forward ones for every index they can produce.
    #[test]
    fn inverse_tables_agree_with_compiler() {
        // interp: 0..=7 are all distinct names that map back to themselves.
        let mut seen = std::collections::BTreeSet::new();
        for i in 0u8..8 {
            let name = interp_name(i);
            assert!(seen.insert(name), "interp_name({i}) duplicated {name}");
        }
        // choice / gamemode: only the non-default indices are representable.
        assert_eq!(choice_name(0), None);
        assert_eq!(choice_name(1), Some("Weight"));
        assert_eq!(choice_name(2), Some("Random Chance"));
        assert_eq!(gamemode_name(0), None);
        assert_eq!(gamemode_name(1), Some("NORMAL"));
        assert_eq!(gamemode_name(2), Some("NEW GAME PLUS"));
    }

    /// Descriptor codes are written as one byte, so the inverted index would
    /// silently lose descriptors if two of them collided mod 256.
    /// Float spelling must match the shipped convention (MSVC %g's CHOICE of
    /// notation, 3-digit exponent) while keeping digits that read back exactly.
    #[test]
    fn float_spelling_matches_the_shipped_convention() {
        for (v, want) in [
            (0.002f32, "0.002"),          // exponent -3: plain
            (1.0e-5, "1e-005"),           // exponent -5: scientific
            (-1.19209e-7, "-1.19209e-007"),
            (1910800.0, "1.9108e+006"),   // exponent 6: scientific
            (99999.0, "99999"),           // exponent 4: plain
            (0.0, "0"),
            (-5.0, "-5"),
            (19.87122, "19.87122"),       // %g would truncate this to 19.8712
        ] {
            assert_eq!(fmt_f32(v), want, "fmt_f32({v})");
        }
        // Every spelling we emit must read back as the SAME f32.
        for bits in [0x0000_0007u32, 0x3F80_0000, 0x7F7F_FFFF, 0x0080_0000, 0xB380_0000] {
            let v = f32::from_bits(bits);
            let s = fmt_f32(v);
            assert_eq!(
                s.parse::<f64>().expect("parses") as f32,
                v,
                "round-trip of {s} (bits {bits:#010X})"
            );
        }
    }

    #[test]
    fn descriptor_codes_are_unique_mod_256() {
        let with_code = schema_map().values().filter(|d| d.code.is_some()).count();
        assert_eq!(by_code().len(), with_code, "descriptor code collision mod 256");
    }

    /// The test sources already start with a BOM character, so this is exactly
    /// `to_layout_bytes` — encode and let the BOM become the leading FF FE.
    fn to_utf16(s: &str) -> Vec<u8> {
        to_layout_bytes(s)
    }

    /// A hand-built layout exercising the pieces that are NOT read straight back
    /// out of the stream: the elided PARENTID, the skipped default orientation,
    /// and every datagroup-only Group field.
    #[test]
    fn roundtrips_group_datagroup_and_defaults() {
        let src = "\u{FEFF}[Layout]\r\n\t[OBJECTS]\r\n\t\t[BASEOBJECT]\r\n\t\t\t[PROPERTIES]\r\n\
             \t\t\t\t<STRING>DESCRIPTOR:Group\r\n\
             \t\t\t\t<STRING>NAME:Chests\r\n\
             \t\t\t\t<INTEGER64>PARENTID:-1\r\n\
             \t\t\t\t<INTEGER64>ID:42\r\n\
             \t\t\t\t<STRING>TAG:CHESTS STANDARD\r\n\
             \t\t\t\t<STRING>CHOICE:Weight\r\n\
             \t\t\t\t<UNSIGNED INT>RANDOMIZATION:60\r\n\
             \t\t\t\t<INTEGER>NUMBER:3\r\n\
             \t\t\t\t<STRING>GAME MODE:NEW GAME PLUS\r\n\
             \t\t\t\t<STRING>ACTIVE THEMES:-6023112609921429025,896\r\n\
             \t\t\t\t<FLOAT>POSITIONX:1.5\r\n\
             \t\t\t\t<FLOAT>POSITIONY:-5\r\n\
             \t\t\t\t<FLOAT>POSITIONZ:62.7999\r\n\
             \t\t\t[/PROPERTIES]\r\n\t\t\t[CHILDREN]\r\n\t\t\t\t[BASEOBJECT]\r\n\t\t\t\t\t[PROPERTIES]\r\n\
             \t\t\t\t\t\t<STRING>DESCRIPTOR:Light\r\n\
             \t\t\t\t\t\t<STRING>NAME:Light1\r\n\
             \t\t\t\t\t\t<INTEGER64>PARENTID:42\r\n\
             \t\t\t\t\t\t<INTEGER64>ID:43\r\n\
             \t\t\t\t\t\t<FLOAT>FORWARDX:0\r\n\
             \t\t\t\t\t\t<FLOAT>FORWARDY:0\r\n\
             \t\t\t\t\t\t<FLOAT>FORWARDZ:1\r\n\
             \t\t\t\t\t\t<BOOL>SHADOW:true\r\n\
             \t\t\t\t\t[/PROPERTIES]\r\n\t\t\t\t[/BASEOBJECT]\r\n\t\t\t[/CHILDREN]\r\n\
             \t\t[/BASEOBJECT]\r\n\t[/OBJECTS]\r\n[/Layout]\r\n";
        let bin = compile_layout_bytes(&to_utf16(src)).expect("compile");
        let text = decompile_binlayout(&bin).expect("decompile");

        // The Group fields that live ONLY in the datagroup came back by name.
        for want in [
            "<STRING>TAG:CHESTS STANDARD",
            "<STRING>CHOICE:Weight",
            "<UNSIGNED INT>RANDOMIZATION:60",
            "<INTEGER>NUMBER:3",
            "<STRING>GAME MODE:NEW GAME PLUS",
            "<STRING>ACTIVE THEMES:-6023112609921429025,896",
            "<INTEGER64>PARENTID:42", // reconstructed from the tree
        ] {
            assert!(text.contains(want), "decompiled text is missing {want:?}:\n{text}");
        }
        // FORWARD Z==1 was dropped at compile and is NOT invented back — a
        // shipped source omits it too, and the reader defaults it.
        assert!(!text.contains("FORWARD"), "default orientation stays omitted");

        let again = compile_layout_bytes(&to_layout_bytes(&text)).expect("recompile");
        assert_eq!(again, bin, "decompile → compile must be byte-identical");
    }

    /// Round-trip the real corpus where the game is installed.
    ///
    /// The criterion is the FIXED POINT — a BINLAYOUT this crate produced must
    /// decompile and re-compile to itself, which is exactly what unpacking a
    /// `.MOD` and repacking it depends on. Comparing against the SHIPPED bytes
    /// instead would fold in the 19 base files the compiler already cannot
    /// reproduce (uninitialised @25 garbage the editor wrote); the second
    /// assertion pins that down — the decompiler may not add a single mismatch
    /// beyond the ones compilation alone already has.
    #[test]
    fn roundtrips_shipped_binlayouts_if_present() {
        use std::path::Path;
        let media = Path::new(r"E:\Torchlight 2\MEDIA");
        if !media.is_dir() {
            eprintln!("skip: no TL2 install");
            return;
        }
        let mut files = Vec::new();
        fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if path
                        .to_string_lossy()
                        .to_uppercase()
                        .ends_with(".BINLAYOUT")
                    {
                        out.push(path);
                    }
                }
            }
        }
        walk(media, &mut files);
        assert!(!files.is_empty(), "corpus found");
        files.truncate(400); // the full sweep lives in `verify binlayout-roundtrip`

        let roundtrip = |orig: &[u8]| -> Result<Vec<u8>, String> {
            compile_layout_bytes(&to_layout_bytes(&decompile_binlayout(orig)?))
        };

        let mut fixed_point_bad = Vec::new();
        let (mut compile_bad, mut shipped_rt_bad) = (0usize, 0usize);
        for f in &files {
            let shipped = std::fs::read(f).unwrap_or_default();
            let src = std::path::PathBuf::from(
                &f.to_string_lossy()[..f.to_string_lossy().len() - ".BINLAYOUT".len()],
            );
            // Fixed point: our own compile must survive decompile → compile.
            if let Ok(ours) = compile_layout_bytes(&std::fs::read(&src).unwrap_or_default()) {
                if ours != shipped {
                    compile_bad += 1;
                }
                match roundtrip(&ours) {
                    Ok(re) if re == ours => {}
                    Ok(_) => fixed_point_bad.push(format!("{}: not a fixed point", f.display())),
                    Err(e) => fixed_point_bad.push(format!("{}: {e}", f.display())),
                }
            }
            if roundtrip(&shipped).map(|re| re == shipped) != Ok(true) {
                shipped_rt_bad += 1;
            }
        }
        eprintln!(
            "binlayout round-trip over {} files: fixed-point failures {}, \
             shipped-input failures {shipped_rt_bad} (compile-alone failures {compile_bad})",
            files.len(),
            fixed_point_bad.len()
        );
        assert!(
            fixed_point_bad.is_empty(),
            "fixed-point failures:\n{}",
            fixed_point_bad.join("\n")
        );
        assert_eq!(
            shipped_rt_bad, compile_bad,
            "the decompiler must not add mismatches beyond the ones compilation already has"
        );
    }
}
