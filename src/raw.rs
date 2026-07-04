//! RAW index file writers (port of raw.py, itself ported from TL2Lib rgio.raw).
//!
//! A RAW file indexes a category of source .DAT/.LAYOUT files. All byte-exact
//! vs GUTS on the shipped base game (AFFIXES/SKILLS/TRIGGERABLES/MISSILES/
//! UNITDATA/UI verified; ROOMPIECES follows the reference).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use regex::Regex;

use crate::text::decode_utf16le_lossy;

/// Base-game install MEDIA (BASEFILE inheritance fallback for UNITDATA).
pub fn install_media() -> PathBuf {
    std::env::var("TL2_MEDIA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"E:\Torchlight 2\MEDIA"))
}

// ── stream primitives ──────────────────────────────────────────────────────

/// WriteShortString: u16 UTF-16 unit count + UTF-16-LE.
pub fn ss(s: &str, out: &mut Vec<u8>) {
    let units: Vec<u16> = s.encode_utf16().collect();
    out.extend_from_slice(&(units.len() as u16).to_le_bytes());
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
}

fn u32le(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn u16le(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn qle(v: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

// ── source scan (FindFirst order) ──────────────────────────────────────────

fn dir_entries(root: &Path) -> Vec<(String, PathBuf, bool)> {
    let mut items = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            items.push((name.to_uppercase(), e.path(), is_dir));
        }
    }
    items
}

/// `_scan`: per directory, this dir's files first (name-sorted, upper), then
/// recurse into subdirs (also name-sorted). Files-before-dirs.
pub fn scan(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_into(root, &ext.to_uppercase(), &mut out);
    out
}

fn scan_into(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let entries = dir_entries(root);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for (eu, p, is_dir) in entries {
        if is_dir {
            dirs.push((eu, p));
        } else if eu.ends_with(ext) && !eu.contains(".BIN") {
            files.push((eu, p));
        }
    }
    files.sort();
    dirs.sort();
    for (_n, p) in files {
        out.push(p);
    }
    for (_n, p) in dirs {
        scan_into(&p, ext, out);
    }
}

/// `_scan_interleaved`: files and subdirs sorted TOGETHER by name; yield a file
/// or recurse in that merged order (GUTS unit-scan order).
pub fn scan_interleaved(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_interleaved_into(root, &ext.to_uppercase(), &mut out);
    out
}

fn scan_interleaved_into(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let mut items: Vec<(String, PathBuf, bool)> = Vec::new();
    for (eu, p, is_dir) in dir_entries(root) {
        if is_dir {
            items.push((eu, p, true));
        } else if eu.ends_with(ext) && !eu.contains(".BIN") {
            items.push((eu, p, false));
        }
    }
    items.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for (_n, p, is_dir) in items {
        if is_dir {
            scan_interleaved_into(&p, ext, out);
        } else {
            out.push(p);
        }
    }
}

// ── text helpers ───────────────────────────────────────────────────────────

/// Python `open(path, encoding='utf-16-le').read()` — BOM NOT stripped.
fn read_dat(path: &Path) -> String {
    let data = std::fs::read(path).unwrap_or_default();
    decode_utf16le_lossy(&data)
}

/// Compiled-regex cache. THREAD-LOCAL, not a global Mutex: the RAW builders
/// call this from every rayon worker for every field of every .DAT — a shared
/// lock serialized the whole phase (measured: 16 threads gave only 3.5x over
/// 1 thread). Per-thread compilation costs ~20 patterns per worker once.
fn re_cached(pattern: &str) -> Regex {
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<String, Regex>> =
            std::cell::RefCell::new(HashMap::new());
    }
    CACHE.with(|c| {
        c.borrow_mut()
            .entry(pattern.to_string())
            .or_insert_with(|| Regex::new(pattern).expect("valid regex"))
            .clone()
    })
}

/// `_field`: first `<[A-Z0-9 ]+>{name}\s*:\s*([^\r\n]*)` match, trimmed.
fn field<'a>(text: &'a str, name: &str) -> Option<String> {
    let re = re_cached(&format!(r"<[A-Z0-9 ]+>{name}\s*:\s*([^\r\n]*)"));
    re.captures(text).map(|c| c[1].trim().to_string())
}

/// `MEDIA/`-relative UPPER path with '/' separators.
fn media_path(dat_path: &Path, media_dir: &Path) -> String {
    let rel = dat_path
        .strip_prefix(media_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| dat_path.to_string_lossy().replace('\\', "/"));
    format!("MEDIA/{rel}").to_uppercase()
}

fn to_int(v: Option<&str>, default: i64) -> i64 {
    match v {
        None => default,
        Some(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                i
            } else if let Ok(u) = t.parse::<u64>() {
                u as i64 // Python int() is unbounded; the writers mask to 64 bits
            } else if let Ok(f) = t.parse::<f64>() {
                f.trunc() as i64
            } else {
                default
            }
        }
    }
}

/// Split `body` into pieces each beginning at "[PROPERTIES]" (Python
/// `re.split(r'(?=\[PROPERTIES\])', body)`; first piece is the prefix).
fn split_before_properties(body: &str) -> Vec<&str> {
    let marker = "[PROPERTIES]";
    let mut cuts = vec![0usize];
    let mut from = 0;
    while let Some(pos) = body[from..].find(marker) {
        let at = from + pos;
        if at != 0 {
            cuts.push(at);
        }
        from = at + marker.len();
    }
    cuts.push(body.len());
    cuts.windows(2).map(|w| &body[w[0]..w[1]]).collect()
}

/// `[OBJECTS]...[/OBJECTS]` body (DOTALL, IGNORECASE), else the whole text.
fn objects_body(text: &str) -> &str {
    let re = re_cached(r"(?is)\[OBJECTS\](.*?)\[/OBJECTS\]");
    match re.captures(text) {
        Some(c) => c.get(1).map(|m| &text[m.range()]).unwrap_or(text),
        None => text,
    }
}

// ── AFFIXES ────────────────────────────────────────────────────────────────

fn affix_unittypes(text: &str, block: &str) -> Vec<String> {
    let outer = re_cached(&format!(r"(?s)\[{block}\](.*?)\[/{block}\]"));
    let inner = re_cached(r"<STRING>[A-Z0-9_ ]+\s*:\s*([^\r\n]+)");
    let mut out = Vec::new();
    for m in outer.captures_iter(text) {
        for c in inner.captures_iter(&m[1]) {
            out.push(c[1].trim().to_string());
        }
    }
    out
}

fn affix_entry(dp: &Path, media_dir: &Path) -> Vec<u8> {
    let t = read_dat(dp);
    let name = field(&t, "NAME")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "NA".to_string())
        .to_uppercase();
    let mn = to_int(field(&t, "MIN_SPAWN_RANGE").as_deref(), 0);
    let mx = to_int(field(&t, "MAX_SPAWN_RANGE").as_deref(), 999_999);
    let w = to_int(field(&t, "WEIGHT").as_deref(), 1);
    let diff = to_int(field(&t, "DIFFICULTIES_ALLOWED").as_deref(), -1);
    let ut = affix_unittypes(&t, "UNITTYPES");
    let nut = affix_unittypes(&t, "NOT_UNITTYPES");
    let mut out = Vec::new();
    ss(&media_path(dp, media_dir), &mut out);
    ss(&name, &mut out);
    u32le(mn as u32, &mut out);
    u32le(mx as u32, &mut out);
    u32le(w as u32, &mut out);
    u32le(diff as u32, &mut out);
    out.push(ut.len() as u8);
    for x in &ut {
        ss(x, &mut out);
    }
    out.push(nut.len() as u8);
    for x in &nut {
        ss(x, &mut out);
    }
    out
}

pub fn build_affixes(media_dir: &Path) -> Option<Vec<u8>> {
    let affixes = scan_interleaved(&media_dir.join("AFFIXES"), ".DAT");
    if affixes.is_empty() {
        return None;
    }
    let entries: Vec<Vec<u8>> = affixes
        .par_iter()
        .map(|dp| affix_entry(dp, media_dir))
        .collect();
    let mut out = Vec::new();
    u16le(affixes.len() as u16, &mut out);
    for e in entries {
        out.extend_from_slice(&e);
    }
    Some(out)
}

// ── SKILLS ─────────────────────────────────────────────────────────────────

fn skill_entry(dp: &Path, media_dir: &Path) -> Option<Vec<u8>> {
    let t = read_dat(dp);
    let name = field(&t, "NAME").unwrap_or_default().to_uppercase();
    if name.is_empty() {
        return None;
    }
    let guid = to_int(field(&t, "UNIQUE_GUID").as_deref(), -1);
    let mut out = Vec::new();
    ss(&name, &mut out);
    ss(&media_path(dp, media_dir), &mut out);
    qle(guid as u64, &mut out);
    Some(out)
}

pub fn build_skills(media_dir: &Path) -> Option<Vec<u8>> {
    let dats = scan_interleaved(&media_dir.join("SKILLS"), ".DAT");
    if dats.is_empty() {
        return None;
    }
    let entries: Vec<Vec<u8>> = dats
        .par_iter()
        .filter_map(|dp| skill_entry(dp, media_dir))
        .collect();
    let mut out = Vec::new();
    u32le(entries.len() as u32, &mut out);
    for e in entries {
        out.extend_from_slice(&e);
    }
    Some(out)
}

// ── TRIGGERABLES ───────────────────────────────────────────────────────────

pub fn build_triggerables(media_dir: &Path) -> Option<Vec<u8>> {
    let dats = scan(&media_dir.join("TRIGGERABLES"), ".DAT");
    if dats.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    u16le(dats.len() as u16, &mut out);
    for dp in &dats {
        let t = read_dat(dp);
        let stem = dp
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = field(&t, "NAME").unwrap_or(stem);
        ss(&media_path(dp, media_dir), &mut out);
        ss(&name, &mut out);
    }
    Some(out)
}

// ── MISSILES ───────────────────────────────────────────────────────────────

pub fn build_missiles(media_dir: &Path) -> Option<Vec<u8>> {
    let lays = scan_interleaved(&media_dir.join("MISSILES"), ".LAYOUT");
    if lays.is_empty() {
        return None;
    }
    // Per-layout entries are independent — order-preserving parallel map.
    let entries: Vec<Vec<u8>> = lays
        .par_iter()
        .map(|lp| {
            let desc_re = re_cached(r"(?m)<STRING>DESCRIPTOR\s*:\s*Missile\s*$");
            let name_re = re_cached(r"<STRING>MISSILE NAME\s*:\s*([^\r\n]*)");
            let t = read_dat(lp);
            let objs = objects_body(&t);
            let mut names = Vec::new();
            for pb in split_before_properties(objs) {
                if desc_re.is_match(pb) {
                    let n = name_re
                        .captures(pb)
                        .map(|c| c[1].trim().to_string())
                        .unwrap_or_else(|| "NA".to_string());
                    names.push(n.to_uppercase());
                }
            }
            let mut e = Vec::new();
            ss(&media_path(lp, media_dir), &mut e);
            e.push(names.len() as u8);
            for n in &names {
                ss(n, &mut e);
            }
            e
        })
        .collect();
    let mut out = Vec::new();
    u16le(lays.len() as u16, &mut out);
    for e in entries {
        out.extend_from_slice(&e);
    }
    Some(out)
}

// ── UI ─────────────────────────────────────────────────────────────────────

const UI_TYPES: [Option<&str>; 20] = [
    None,
    Some("TEST MENU"),
    Some("HUD MENU"),
    Some("CHAT MENU"),
    Some("PLAYER STATS MENU"),
    Some("MERCHANT MENU"),
    Some("SKILL MENU"),
    Some("QUEST DIALOG MENU"),
    Some("QUEST MENU"),
    Some("MESSAGE BOX MENU"),
    Some("MAIN MENU"),
    Some("SERVER MENU"),
    Some("CONSOLE MENU"),
    Some("RESURRECTION MENU"),
    Some("TOWNPORTAL MENU"),
    Some("HOTBAR CONTEXT MENU"),
    Some("STANDALONE SERVER MENU"),
    Some("LOGIN MENU"),
    Some("UPSELL MENU"),
    Some("CLIENT OPTIONS"),
];

const UI_STATES: [Option<&str>; 7] = [
    None,
    Some("Testing"),
    Some("All"),
    Some("In Game"),
    Some("Server Only"),
    Some("Loading"),
    Some("Main Menu Mod"),
];

fn idx_of(arr: &[Option<&str>], val: Option<&str>) -> u32 {
    if let Some(v) = val {
        for (j, x) in arr.iter().enumerate() {
            if let Some(x) = x {
                if x.eq_ignore_ascii_case(v) {
                    return j as u32;
                }
            }
        }
    }
    0
}

fn menu_definition_props(text: &str) -> Option<String> {
    let body = objects_body(text);
    let re = re_cached(r"(?im)<STRING>DESCRIPTOR\s*:\s*Menu Definition\s*$");
    split_before_properties(body)
        .into_iter()
        .find(|pb| re.is_match(pb))
        .map(|s| s.to_string())
}

fn menu_str(block: &str, name: &str) -> String {
    let re = re_cached(&format!(r"<STRING>{}\s*:([^\r\n]*)", regex::escape(name)));
    re.captures(block)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default()
}

fn menu_bool(block: &str, name: &str) -> bool {
    let re = re_cached(&format!(r"<BOOL>{}\s*:([^\r\n]*)", regex::escape(name)));
    re.captures(block)
        .map(|c| matches!(c[1].trim().to_uppercase().as_str(), "TRUE" | "1" | "YES"))
        .unwrap_or(false)
}

pub fn build_ui(media_dir: &Path) -> Option<Vec<u8>> {
    let lays = scan(&media_dir.join("UI"), ".LAYOUT");
    if lays.is_empty() {
        return None;
    }
    // Per-layout entries are independent — order-preserving parallel map
    // (the UI tree can be 100+ MB of .LAYOUT text; serial reads dominated).
    let entries: Vec<Option<Vec<u8>>> = lays
        .par_iter()
        .map(|lp| {
            let blk = menu_definition_props(&read_dat(lp))?;
            let menu_name = menu_str(&blk, "MENU NAME");
            if menu_name.is_empty() || menu_bool(&blk, "DO NOT CREATE") {
                return None;
            }
            let f0 =
                (menu_bool(&blk, "ALWAYS VISIBLE") || menu_bool(&blk, "CREATE ON LOAD")) as u8;
            let mut e = Vec::new();
            ss(&menu_name, &mut e);
            ss(&media_path(lp, media_dir), &mut e);
            let ty = menu_str(&blk, "TYPE");
            let st = menu_str(&blk, "GAME STATE");
            u32le(idx_of(&UI_TYPES, if ty.is_empty() { None } else { Some(&ty) }), &mut e);
            u32le(idx_of(&UI_STATES, if st.is_empty() { None } else { Some(&st) }), &mut e);
            e.push(f0);
            e.push(menu_bool(&blk, "MULTIPLAYER ONLY") as u8);
            e.push(menu_bool(&blk, "SINGLEPLAYER ONLY") as u8);
            ss(&menu_str(&blk, "KEY BINDING"), &mut e);
            Some(e)
        })
        .collect();
    let included: Vec<&Vec<u8>> = entries.iter().flatten().collect();
    let mut out = Vec::new();
    u32le(included.len() as u32, &mut out);
    for e in included {
        out.extend_from_slice(e);
    }
    Some(out)
}

// ── UNITDATA ───────────────────────────────────────────────────────────────

const UNIT_CATEGORIES: [&str; 4] = ["ITEMS", "MONSTERS", "PLAYERS", "PROPS"];

type Attrs = HashMap<String, String>;

/// Sharded path->attrs cache: the UNITDATA BASEFILE-chain resolution hits it
/// several times per unit from every worker, so a single Mutex serializes.
pub struct AttrCache {
    shards: Vec<Mutex<HashMap<String, Option<std::sync::Arc<Attrs>>>>>,
}

impl AttrCache {
    fn new() -> Self {
        AttrCache {
            shards: (0..64).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, key: &str) -> &Mutex<HashMap<String, Option<std::sync::Arc<Attrs>>>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        &self.shards[(h.finish() as usize) % self.shards.len()]
    }

    fn get(&self, key: &str) -> Option<Option<std::sync::Arc<Attrs>>> {
        self.shard(key).lock().unwrap().get(key).cloned()
    }

    fn insert(&self, key: String, val: Option<std::sync::Arc<Attrs>>) {
        self.shard(&key).lock().unwrap().insert(key, val);
    }
}

/// Top-level [UNIT] attributes only (depth-1 lines), first occurrence wins.
fn unit_attrs(text: &str) -> Attrs {
    let re = re_cached(r"^<[A-Z0-9 ]+>([A-Z0-9_ ]+)\s*:(.*)$");
    let mut attrs = HashMap::new();
    let mut depth: i32 = 0;
    for line in text.lines() {
        let s = line.trim().trim_start_matches('\u{FEFF}');
        if s.is_empty() {
            continue;
        }
        if s.starts_with("[/") {
            depth -= 1;
            continue;
        }
        if s.starts_with('[') {
            depth += 1;
            continue;
        }
        if depth != 1 {
            continue;
        }
        if let Some(c) = re.captures(s) {
            let k = c[1].trim().to_uppercase();
            attrs.entry(k).or_insert_with(|| c[2].trim().to_string());
        }
    }
    attrs
}

fn basefile_path(basefile: &str, media_dir: &Path) -> PathBuf {
    let mut rel = basefile.replace('\\', "/").trim().to_string();
    if rel.to_uppercase().starts_with("MEDIA/") {
        rel = rel[6..].to_string();
    }
    media_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn resolve_basefile(basefile: &str, media_dir: &Path, install: &Path) -> PathBuf {
    let p = basefile_path(basefile, media_dir);
    if p.is_file() {
        return p;
    }
    if media_dir
        .to_string_lossy()
        .to_lowercase()
        .trim_end_matches(['\\', '/'])
        != install
            .to_string_lossy()
            .to_lowercase()
            .trim_end_matches(['\\', '/'])
    {
        let pi = basefile_path(basefile, install);
        if pi.is_file() {
            return pi;
        }
    }
    p
}

/// Python `normcase(abspath(p))` equivalent for cache/cycle keys: lowercase,
/// backslash separators. Purely lexical (no filesystem access).
fn norm_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase().replace('/', "\\")
}

fn unit_attrs_for(path: &Path, cache: &AttrCache) -> Option<std::sync::Arc<Attrs>> {
    let key = norm_key(path);
    if let Some(v) = cache.get(&key) {
        return v;
    }
    let attrs = if path.is_file() {
        Some(std::sync::Arc::new(unit_attrs(&read_dat(path))))
    } else {
        None
    };
    cache.insert(key, attrs.clone());
    attrs
}

fn unit_chain(
    path: &Path,
    media_dir: &Path,
    install: &Path,
    cache: &AttrCache,
) -> Vec<std::sync::Arc<Attrs>> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = path.to_path_buf();
    for _depth in 0..=30 {
        let key = norm_key(&cur);
        if seen.contains(&key) {
            break;
        }
        seen.insert(key);
        let own = match unit_attrs_for(&cur, cache) {
            Some(a) => a,
            None => break,
        };
        let bf = own.get("BASEFILE").cloned();
        out.push(own);
        match bf {
            Some(bf) if !bf.is_empty() => {
                cur = resolve_basefile(&bf, media_dir, install);
            }
            _ => break,
        }
    }
    out
}

fn chain_str(chain: &[std::sync::Arc<Attrs>], name: &str) -> String {
    for a in chain {
        if let Some(v) = a.get(name) {
            if !v.is_empty() {
                return v.clone();
            }
        }
    }
    String::new()
}

fn chain_int(chain: &[std::sync::Arc<Attrs>], name: &str, default: i64) -> i64 {
    for a in chain {
        match a.get(name) {
            None => continue,
            Some(v) if v.is_empty() => continue,
            Some(v) => {
                let iv = to_int(Some(v), default);
                if iv != default {
                    return iv;
                }
            }
        }
    }
    default
}

fn unit_entry(
    dp: &Path,
    media_dir: &Path,
    install: &Path,
    cache: &AttrCache,
) -> Option<Vec<u8>> {
    let own = unit_attrs_for(dp, cache)?;
    if own
        .get("DONTCREATE")
        .map(|v| v.to_uppercase() == "TRUE")
        .unwrap_or(false)
    {
        return None;
    }
    let chain = unit_chain(dp, media_dir, install, cache);
    let guid_s = chain_str(&chain, "UNIT_GUID");
    let guid = to_int(if guid_s.is_empty() { None } else { Some(&guid_s) }, -1);
    let name = chain_str(&chain, "NAME").to_uppercase();
    let createas = chain_str(&chain, "CREATEAS");
    let setv = chain_str(&chain, "SET");
    let mut flags: u8 = if createas.to_uppercase() == "EQUIPMENT" { 1 } else { 0 };
    if !setv.is_empty() {
        flags |= 2;
    }
    let level = chain_int(&chain, "LEVEL", 1);
    let minlevel = chain_int(&chain, "MINLEVEL", 0);
    let maxlevel = chain_int(&chain, "MAXLEVEL", 0);
    let rarity = chain_int(&chain, "RARITY", 1);
    let rarity_hc = chain_int(&chain, "RARITY_HARDCORE", rarity);
    let unittype = chain_str(&chain, "UNITTYPE").to_uppercase();
    let mut out = Vec::new();
    qle(guid as u64, &mut out);
    ss(&name, &mut out);
    ss(&media_path(dp, media_dir), &mut out);
    out.push(flags);
    u32le(level as u32, &mut out);
    u32le(minlevel as u32, &mut out);
    u32le(maxlevel as u32, &mut out);
    u32le(rarity as u32, &mut out);
    u32le(rarity_hc as u32, &mut out);
    ss(&unittype, &mut out);
    Some(out)
}

pub fn build_unitdata(media_dir: &Path) -> Option<Vec<u8>> {
    let units_root = media_dir.join("UNITS");
    if !units_root.is_dir() {
        return None;
    }
    let install = install_media();
    let cache = AttrCache::new();
    let mut out = Vec::new();
    let mut emitted = 0usize;
    for cat in UNIT_CATEGORIES {
        let paths = scan_interleaved(&units_root.join(cat), ".DAT");
        let entries: Vec<Vec<u8>> = paths
            .par_iter()
            .filter_map(|dp| unit_entry(dp, media_dir, &install, &cache))
            .collect();
        u32le(entries.len() as u32, &mut out);
        for e in &entries {
            out.extend_from_slice(e);
        }
        emitted += entries.len();
    }
    if emitted == 0 {
        return None;
    }
    Some(out)
}

// ── ROOMPIECES ─────────────────────────────────────────────────────────────

pub fn build_roompieces(media_dir: &Path) -> Option<Vec<u8>> {
    let dats = scan(&media_dir.join("LEVELSETS"), ".DAT");
    if dats.is_empty() {
        return None;
    }
    let piece_re = re_cached(r"(?is)\[PIECE\](.*?)\[/PIECE\]");
    let guid_re = re_cached(r"<INTEGER64>GUID\s*:\s*(-?\d+)");
    let mut guids: Vec<Vec<i64>> = Vec::new();
    for dp in &dats {
        let t = read_dat(dp);
        let mut gs = Vec::new();
        for blk in piece_re.captures_iter(&t) {
            if let Some(m) = guid_re.captures(&blk[1]) {
                if let Ok(g) = m[1].parse::<i64>() {
                    gs.push(g);
                }
            }
        }
        guids.push(gs);
    }
    let mut out = Vec::new();
    u32le(dats.len() as u32, &mut out);
    for dp in &dats {
        ss(&media_path(dp, media_dir), &mut out);
    }
    for gs in &guids {
        u32le(gs.len() as u32, &mut out);
        for g in gs {
            qle(*g as u64, &mut out);
        }
    }
    Some(out)
}
