//! .MOD container writer + pipeline orchestration (port of packer.py).
//!
//! Everything is synthesized FROM SCRATCH — no reference .MOD is consulted.
//! Header from MOD.DAT (build_header), manifest tree from a MEDIA scan
//! (build_manifest_dirs), PAK data section with the byte-exact writers.
//! The PAK rollingHash and the manifest mhash are BOTH validated by the game
//! and are computed here (leaving either 0 breaks the mod — see the RE notes
//! in the Python original / project memory).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rayon::prelude::*;
use regex::Regex;

use crate::binlayout::compile_layout_file;
use crate::raw;

/// In-memory converted files, keyed by UPPER rel path (no MEDIA/ prefix),
/// '/'-separated. Insertion-ordered (mirrors the Python dict).
pub type Overrides = IndexMap<String, Vec<u8>>;

/// Phase timing (enabled by env MIKURO_TIMING=1): eprintln a labelled duration.
fn timing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MIKURO_TIMING").map(|v| v == "1").unwrap_or(false))
}

struct Phase(&'static str, std::time::Instant);

impl Phase {
    fn start(name: &'static str) -> Self {
        Phase(name, std::time::Instant::now())
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if timing_enabled() {
            eprintln!("    [timing] {:<28} {:>8.3}s", self.0, self.1.elapsed().as_secs_f64());
        }
    }
}

/// What to tell someone who asks for the removed byte-exact MPP backend.
///
/// Every caller of `--mpp dll` gets this exact text — it is the whole
/// replacement for the code that used to be here, so it has to be actionable:
/// the command, and the one constraint that made the wrapper messy in the first
/// place (the editor only builds projects under its own mods folder).
pub const MPP_DLL_REMOVED: &str = "the byte-exact `dll` MPP backend was removed. It shelled out to TL2's own \
     console host, which only builds projects under <install>\\mods\\<name> — so it had to copy your \
     whole mod there (destroying anything already using that name) and, in the default in-memory mode, \
     into a temp dir first. Drive the console host yourself instead:\n  \
     cd \"<TL2 install>\" && TL2-Mikuro-Console.exe build <MOD_FILE_NAME> --clean --twice\n  \
     (needs the editor environment: a working D3D9 device, and the mod living in <install>\\mods\\)\n  \
     Then pack with --mpp none to keep the .mpp files it wrote.";

pub const TYPE_DIR: u8 = 7;
pub const TYPE_UNKNOWN: u8 = 18;

const MOD_MAGIC: u16 = 4;
const MOD_FLAGS: u32 = 0;
const DEFAULT_GAMEVER: u64 = 0x0005_0009_0019_0001;

/// TL2 base install MEDIA dir (env-overridable).
pub fn tl2_media_dir() -> PathBuf {
    std::env::var("TL2_MEDIA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"E:\Torchlight 2\MEDIA"))
}

/// Extension → manifest TYPE code (EditorGuts sub_102A1EA0 + sub_102A24F0).
fn ext_type_of(ext: &str) -> Option<u8> {
    Some(match ext {
        ".DAT" | ".TEMPLATE" | ".BINDAT" => 0,
        ".LAYOUT" | ".BINLAYOUT" => 1,
        ".MESH" => 2,
        ".SKELETON" => 3,
        ".DDS" => 4,
        ".PNG" => 5,
        ".WAV" | ".OGG" => 6,
        ".MATERIAL" => 8,
        ".RAW" => 9,
        ".UILAYOUT" => 10,
        ".IMAGESET" => 11,
        ".TTF" | ".TTC" => 12,
        ".FONT" => 13,
        ".ANIMATION" => 16,
        ".HIE" => 17,
        ".SCHEME" => 19,
        ".LOOKNFEEL" => 20,
        ".MPP" => 21,
        ".BIK" => 23,
        ".JPG" => 24,
        _ => return None,
    })
}

/// Manifest type code for an UPPER file name; .BINDAT/.BINLAYOUT names are
/// classified by the SOURCE extension they wrap.
pub fn ext_type(name_upper: &str) -> u8 {
    let mut base = name_upper;
    for suf in [".BINDAT", ".BINLAYOUT"] {
        if let Some(b) = base.strip_suffix(suf) {
            base = b;
            break;
        }
    }
    match base.rfind('.') {
        None => TYPE_UNKNOWN,
        Some(dot) => ext_type_of(&base[dot..]).unwrap_or(TYPE_UNKNOWN),
    }
}

/// Store-vs-compress: everything zlib-compressed except .JPG (type 24), and
/// anything >= 0x1900000 is stored (GUTS' compress-buffer cap).
fn is_store_type(typ: u8) -> bool {
    typ == 24
}

// ── gamever (Torchlight2.exe VS_FIXEDFILEINFO) ─────────────────────────────

#[cfg(windows)]
fn read_gamever_exe(exe: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let wide: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        let sub: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
        let mut lp: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut ln: u32 = 0;
        if VerQueryValueW(buf.as_ptr() as *const _, sub.as_ptr(), &mut lp, &mut ln) == 0 {
            return None;
        }
        let ffi = lp as *const u32;
        if *ffi != 0xFEEF_04BD {
            return None;
        }
        let ms = *ffi.add(2);
        let ls = *ffi.add(3);
        let (w4, w5) = ((ms & 0xFFFF) as u64, ((ms >> 16) & 0xFFFF) as u64);
        let (w6, w7) = ((ls & 0xFFFF) as u64, ((ls >> 16) & 0xFFFF) as u64);
        Some(w5 | (w4 << 16) | (w7 << 32) | (w6 << 48))
    }
}

#[cfg(not(windows))]
fn read_gamever_exe(_exe: &Path) -> Option<u64> {
    None
}

/// gameVersion u64 from Torchlight2.exe (== EditorGuts sub_103F8CD0), else the
/// 1.25.9.5 default. Env `TL2_MOD_GAMEVER` overrides (handled by the caller).
pub fn read_gamever(install_dir: Option<&Path>) -> u64 {
    let install = match install_dir {
        Some(d) => d.to_path_buf(),
        None => tl2_media_dir()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default(),
    };
    let exe = install.join("Torchlight2.exe");
    if !exe.is_file() {
        return DEFAULT_GAMEVER;
    }
    read_gamever_exe(&exe).unwrap_or(DEFAULT_GAMEVER)
}

// ── MOD.DAT reader ─────────────────────────────────────────────────────────

pub struct ModMeta {
    pub name: String,
    pub title: String,
}

pub fn read_mod_metadata(mod_dir: &Path) -> ModMeta {
    let base = mod_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mod_dat = mod_dir.join("MOD.DAT");
    if !mod_dat.exists() {
        return ModMeta {
            name: base.clone(),
            title: base,
        };
    }
    let text = read_mod_dat_text(&mod_dat);
    let get = |key: &str| -> Option<String> {
        let re = Regex::new(&format!(r"<STRING>{key}:([^\r\n]+)")).unwrap();
        re.captures(&text).map(|c| c[1].trim().to_string())
    };
    let name = get("MOD_FILE_NAME").unwrap_or_else(|| base.clone());
    let title = get("DISPLAYNAME")
        .or_else(|| get("MOD_TITLE"))
        .unwrap_or_else(|| name.clone());
    ModMeta { name, title }
}

fn read_mod_dat_text(path: &Path) -> String {
    let raw = std::fs::read(path).unwrap_or_default();
    // UTF-16-LE strict, falling back to UTF-8 (mirrors _parse_mod_dat).
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let text = match String::from_utf16(&units) {
        Ok(t) if raw.len() % 2 == 0 => t,
        _ => String::from_utf8_lossy(&raw).to_string(),
    };
    text.trim_start_matches('\u{FEFF}').to_string()
}

pub struct Header {
    pub ver: u16,
    pub modver: u16,
    pub gamever: u64,
    pub title: String,
    pub author: String,
    pub descr: String,
    pub website: String,
    pub download: String,
    pub modid: u64,
    pub flags: u32,
    pub req_hash: u64,
    pub reqs: Vec<(String, u64, u16)>,
    pub dels: Vec<String>,
    pub mhash: u32,
    pub root: String,
    pub fc: u32,
}

// MOD.DAT regexes — constant patterns, so compile each once for the process
// rather than on every parse_mod_dat call.
static MODDAT_FIELD_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"<[^>]+>([A-Za-z0-9_]+)\s*:([^\r\n]*)").unwrap());
static MODDAT_REQ_BLOCK_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?is)\[REQUIRED_MODS\](.*?)\[/REQUIRED_MODS\]").unwrap());
static MODDAT_REQ_NAME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)<[^>]+>NAME\s*:([^\r\n]*)").unwrap());
static MODDAT_REQ_GID_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)<[^>]+>(?:MOD_ID|GUID|ID)\s*:\s*(-?\d+)").unwrap());
static MODDAT_REQ_VER_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)<[^>]+>VERSION\s*:\s*(-?\d+)").unwrap());
static MODDAT_REMOVE_BLOCK_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?is)\[REMOVE_FILES\](.*?)\[/REMOVE_FILES\]").unwrap());
static MODDAT_REMOVE_ITEM_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"<STRING>[^:]*:([^\r\n]*)").unwrap());

fn parse_mod_dat(mod_dir: &Path) -> (BTreeMap<String, String>, Vec<(String, u64, u16)>, Vec<String>) {
    let path = mod_dir.join("MOD.DAT");
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut reqs = Vec::new();
    let mut dels = Vec::new();
    if !path.exists() {
        return (fields, reqs, dels);
    }
    let text = read_mod_dat_text(&path);

    // Top-level scalar fields (first occurrence wins; case-insensitive key).
    for c in MODDAT_FIELD_RE.captures_iter(&text) {
        let key = c[1].to_uppercase();
        fields.entry(key).or_insert_with(|| c[2].trim().to_string());
    }

    // REQUIRED_MODS block.
    if let Some(rm) = MODDAT_REQ_BLOCK_RE.captures(&text) {
        let body = &rm[1];
        for blk in split_before_tag(body) {
            if let Some(g) = MODDAT_REQ_GID_RE.captures(blk) {
                let name = MODDAT_REQ_NAME_RE
                    .captures(blk)
                    .map(|c| c[1].trim().to_string())
                    .unwrap_or_default();
                let gid = g[1].parse::<i64>().unwrap_or(0) as u64;
                let ver = MODDAT_REQ_VER_RE
                    .captures(blk)
                    .and_then(|c| c[1].parse::<i64>().ok())
                    .unwrap_or(0) as u64 as u16;
                reqs.push((name, gid, ver));
            }
        }
    }

    // REMOVE_FILES block.
    if let Some(rf) = MODDAT_REMOVE_BLOCK_RE.captures(&text) {
        for c in MODDAT_REMOVE_ITEM_RE.captures_iter(&rf[1]) {
            let v = c[1].trim();
            if !v.is_empty() {
                dels.push(v.to_string());
            }
        }
    }
    (fields, reqs, dels)
}

/// Python `re.split(r'(?=\[[A-Za-z0-9_]+\])', body)` — split before each tag.
fn split_before_tag(body: &str) -> Vec<&str> {
    let tag_re = Regex::new(r"\[[A-Za-z0-9_]+\]").unwrap();
    let mut cuts = vec![0usize];
    for m in tag_re.find_iter(body) {
        if m.start() != 0 {
            cuts.push(m.start());
        }
    }
    cuts.push(body.len());
    cuts.windows(2).map(|w| &body[w[0]..w[1]]).collect()
}

/// Synthesize the .MOD header from MOD.DAT (mirrors build_header).
pub fn build_header(mod_dir: &Path, gamever: Option<u64>) -> Header {
    let (fields, reqs, dels) = parse_mod_dat(mod_dir);
    let get_i = |key: &str| -> i64 {
        fields
            .get(key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
    };
    let gamever = gamever.unwrap_or_else(|| {
        match std::env::var("TL2_MOD_GAMEVER") {
            Ok(env) if !env.is_empty() => {
                let t = env.trim();
                if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    u64::from_str_radix(hex, 16).unwrap_or_else(|_| read_gamever(None))
                } else {
                    t.parse::<u64>().unwrap_or_else(|_| read_gamever(None))
                }
            }
            _ => read_gamever(None),
        }
    });
    let base = mod_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Header {
        ver: MOD_MAGIC,
        modver: ((get_i("VERSION") + 1) & 0xFFFF) as u16,
        gamever,
        title: fields.get("NAME").cloned().unwrap_or(base),
        author: fields.get("AUTHOR").cloned().unwrap_or_default(),
        descr: fields.get("DESCRIPTION").cloned().unwrap_or_default(),
        website: fields.get("WEBSITE").cloned().unwrap_or_default(),
        download: fields.get("DOWNLOAD_URL").cloned().unwrap_or_default(),
        modid: get_i("MOD_ID") as u64,
        flags: MOD_FLAGS,
        req_hash: req_hash(&reqs),
        reqs,
        dels,
        mhash: 0,
        root: "MEDIA/".to_string(),
        fc: 0,
    }
}

// ── REQUIRED_MODS hash (header req_hash) ───────────────────────────────────

/// MurmurHash64B — EditorGuts `sub_10285330`, verified instruction by
/// instruction (m = 0x5BD1E995, r = 24, twin 32-bit lanes, 8 bytes per round,
/// 3/2/1 tail into h2, four-step avalanche). GUTS declares the seed as a single
/// dword, so the canonical `h2 = seed >> 32` term is always zero here. The
/// function returns the full 64-bit value in EDX:EAX, i.e. `(h1 << 32) | h2`.
fn murmur64b(data: &[u8], seed: u32) -> u64 {
    const M: u32 = 0x5BD1_E995;
    const R: u32 = 24;

    let mut h1: u32 = seed ^ (data.len() as u32);
    let mut h2: u32 = 0;

    let mut d = data;
    while d.len() >= 8 {
        let mut k1 = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        k1 = k1.wrapping_mul(M);
        k1 ^= k1 >> R;
        k1 = k1.wrapping_mul(M);
        h1 = h1.wrapping_mul(M);
        h1 ^= k1;

        let mut k2 = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
        k2 = k2.wrapping_mul(M);
        k2 ^= k2 >> R;
        k2 = k2.wrapping_mul(M);
        h2 = h2.wrapping_mul(M);
        h2 ^= k2;

        d = &d[8..];
    }
    if d.len() >= 4 {
        let mut k1 = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        k1 = k1.wrapping_mul(M);
        k1 ^= k1 >> R;
        k1 = k1.wrapping_mul(M);
        h1 = h1.wrapping_mul(M);
        h1 ^= k1;
        d = &d[4..];
    }
    // Tail bytes fall through 3 → 2 → 1, then one multiply (h2 lane only).
    if !d.is_empty() {
        if d.len() == 3 {
            h2 ^= (d[2] as u32) << 16;
        }
        if d.len() >= 2 {
            h2 ^= (d[1] as u32) << 8;
        }
        h2 ^= d[0] as u32;
        h2 = h2.wrapping_mul(M);
    }

    h1 ^= h2 >> 18;
    h1 = h1.wrapping_mul(M);
    h2 ^= h1 >> 22;
    h2 = h2.wrapping_mul(M);
    h1 ^= h2 >> 17;
    h1 = h1.wrapping_mul(M);
    h2 ^= h1 >> 19;
    h2 = h2.wrapping_mul(M);

    ((h1 as u64) << 32) | (h2 as u64)
}

/// Header `req_hash` over REQUIRED_MODS — EditorGuts `sub_103F5500`.
///
/// Per entry (the 40-byte record is `guid:u64`, `ver:u16`, `name:wstring@+12`;
/// the name is not hashed):
///
/// ```text
/// t1  = murmur64b(guid_le, 0x22D0)
/// t2  = murmur64b(ver_le,  low32(t1))
/// acc = murmur64b(t2_le,   low32(acc))      // acc seeded 34832 (0x8810)
/// ```
///
/// Chaining feeds only the low dword of each result back as the next seed while
/// hashing the full 64-bit value — that asymmetry is in the disassembly (the
/// seed is pushed as one dword; EDX is stored beside EAX and the pair is hashed
/// as 8 bytes). No REQUIRED_MODS → 0, matching the DLL's early-out.
///
/// **Offline limit.** When GUTS resolves a dependency it hashes the *installed*
/// mod's version and folds in that mod's own `req_hash` recursively. We cannot
/// see the installed set, so we hash the version declared in MOD.DAT and do not
/// recurse. The two agree whenever the declared version matches the installed
/// one and the dependency declares no REQUIRED_MODS of its own — the flat case.
/// Deeper dependency graphs are not reproducible offline by construction.
pub fn req_hash(reqs: &[(String, u64, u16)]) -> u64 {
    if reqs.is_empty() {
        return 0;
    }
    let mut acc: u64 = 34832;
    for (_name, guid, ver) in reqs {
        let t1 = murmur64b(&guid.to_le_bytes(), 0x22D0);
        let t2 = murmur64b(&ver.to_le_bytes(), t1 as u32);
        acc = murmur64b(&t2.to_le_bytes(), acc as u32);
    }
    acc
}

// ── byte writers ───────────────────────────────────────────────────────────

fn w_ss(s: &str, out: &mut Vec<u8>) {
    raw::ss(s, out);
}

pub fn w_header(h: &Header, off_data: u32, off_man: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&h.ver.to_le_bytes());
    out.extend_from_slice(&h.modver.to_le_bytes());
    out.extend_from_slice(&h.gamever.to_le_bytes());
    out.extend_from_slice(&off_data.to_le_bytes());
    out.extend_from_slice(&off_man.to_le_bytes());
    for s in [&h.title, &h.author, &h.descr, &h.website, &h.download] {
        w_ss(s, &mut out);
    }
    out.extend_from_slice(&h.modid.to_le_bytes());
    out.extend_from_slice(&h.flags.to_le_bytes());
    out.extend_from_slice(&h.req_hash.to_le_bytes());
    out.extend_from_slice(&(h.reqs.len() as u16).to_le_bytes());
    for (n, i, v) in &h.reqs {
        w_ss(n, &mut out);
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(h.dels.len() as u16).to_le_bytes());
    for d in &h.dels {
        w_ss(d, &mut out);
    }
    out
}

/// Manifest record: (crc, type, name, off, size, ftime).
pub type Rec = (u32, u8, String, u32, u32, u64);
pub type Dirs = Vec<(String, Vec<Rec>)>;

pub fn w_manifest(h: &Header, dirs: &Dirs) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&h.mhash.to_le_bytes());
    w_ss(&h.root, &mut out);
    out.extend_from_slice(&h.fc.to_le_bytes());
    out.extend_from_slice(&(dirs.len() as u32).to_le_bytes());
    for (dname, recs) in dirs {
        w_ss(dname, &mut out);
        out.extend_from_slice(&(recs.len() as u32).to_le_bytes());
        for (crc, typ, name, off, size, ft) in recs {
            out.extend_from_slice(&crc.to_le_bytes());
            out.push(*typ);
            w_ss(name, &mut out);
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&ft.to_le_bytes());
        }
    }
    out
}

// ── the two validated hashes ───────────────────────────────────────────────

/// PAK data-section rolling hash (VALIDATED by the game at mod-load).
/// divisor = 25 + (695696193*N mod 2^32) mod 51; stride = max(2, N/divisor);
/// h = N; fold every stride-th byte from offset 8 as (i8)b + 33*h; + last byte.
pub fn pak_rolling_hash(data: &[u8]) -> u32 {
    let n = data.len();
    if n <= 8 {
        return 0;
    }
    let divisor = 25 + (695_696_193u32.wrapping_mul(n as u32)) % 51;
    let mut stride = n / divisor as usize;
    if stride <= 1 {
        stride = 2;
    }
    let mut h = n as u32;
    let mut k = 8usize;
    while k < n {
        h = (data[k] as i8 as i32 as u32).wrapping_add(h.wrapping_mul(33));
        k += stride;
    }
    (data[n - 1] as i8 as i32 as u32).wrapping_add(h.wrapping_mul(33))
}

/// Manifest-TOC hashValue (mhash) — VALIDATED on game-ENTER. Seed 8234,
/// stride = 15 + (695696193*N mod 2^32) mod 11, samples from offset 0.
pub fn pak_manifest_hash(manifest: &[u8]) -> u32 {
    let n = manifest.len();
    if n == 0 {
        return 0;
    }
    let stride = (15 + (695_696_193u32.wrapping_mul(n as u32)) % 11) as usize;
    let mut h: u32 = 8234;
    let mut k = 0usize;
    while k < n {
        h = (manifest[k] as i8 as i32 as u32).wrapping_add(h.wrapping_mul(33));
        k += stride;
    }
    h
}

// ── MEDIA scan → manifest tree ─────────────────────────────────────────────

const FILETIME_EPOCH_DELTA: u64 = 116_444_736_000_000_000;

fn filetime_of(md: &std::fs::Metadata) -> u64 {
    // Python: int(st_mtime * 1e7) + delta, st_mtime = float seconds.
    match md.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => {
                let secs = d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9;
                (secs * 10_000_000.0) as u64 + FILETIME_EPOCH_DELTA
            }
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

/// Suffixes EditorGuts strips from a .MOD (editor / tool / intermediate
/// artifacts). GUTS' pack keep-drop predicate `sub_103F4340` walks a 13-entry
/// suffix table built at `sub_11D44D40` (@unk_13E51C50): a name ending in any
/// entry is dropped from the pack list.
///
/// Four of the 13 are the compile sources (`.DAT` `.LAYOUT` `.ANIMATION`
/// `.HIE`) and a fifth is `.LAYOUT` + `.BINDAT`; we do not replicate those
/// here — GUTS drops the source and carries `<source>.BINDAT`, renaming it back
/// to the source name at manifest-write, whereas we keep the source name as the
/// manifest key and pull the compiled bytes through `content_for`. Same output,
/// mirrored mechanism (verified against a real GUTS `EDITORMOD.MOD`: manifest
/// holds source names, zero `.BINDAT`/`.BINLAYOUT` names). `.LAYOUT.BINDAT` is
/// already covered by the `.BINDAT` skip below. That leaves these eight.
const PACK_EXCLUDE_SUFFIXES: [&str; 8] = [
    ".CMP",
    ".THUMBNAIL.PNG",
    ".XLS",
    ".XML",
    ".MAX",
    ".MPD",
    ".LNK",
    ".LOG",
];

/// True when an UPPER file name is one GUTS would strip from the pack.
fn is_pack_excluded(name_upper: &str) -> bool {
    PACK_EXCLUDE_SUFFIXES
        .iter()
        .any(|s| name_upper.ends_with(s))
}

/// (rel_upper_with_MEDIA, manifest_name, type, ftime) in name-interleaved DFS
/// order, with the PNG→DDS dedup, unioned with in-memory overrides.
fn collect_media_files(
    media_dir: &Path,
    overrides: Option<&Overrides>,
) -> Vec<(String, String, u8, u64)> {
    let mut files = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    fn walk(
        disk_dir: &Path,
        rel_prefix: &str,
        overrides: Option<&Overrides>,
        files: &mut Vec<(String, String, u8, u64)>,
        seen: &mut BTreeSet<String>,
    ) {
        let mut entries: Vec<(String, std::fs::DirEntry)> = match std::fs::read_dir(disk_dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| (e.file_name().to_string_lossy().to_uppercase(), e))
                .collect(),
            Err(_) => return,
        };
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let names_u: BTreeSet<String> = entries.iter().map(|(n, _)| n.clone()).collect();
        for (nmu, e) in entries {
            let name = e.file_name().to_string_lossy().to_string();
            let rel = format!("{rel_prefix}{name}");
            let relu = rel.to_uppercase();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&e.path(), &format!("{rel}/"), overrides, files, seen);
                continue;
            }
            // Compiled outputs ride under the source name.
            if nmu.ends_with(".BINDAT") || nmu.ends_with(".BINLAYOUT") {
                continue;
            }
            // Editor / tool / intermediate artifacts GUTS strips (sub_103F4340).
            // Checked before the PNG rule so .THUMBNAIL.PNG is dropped outright.
            if is_pack_excluded(&nmu) {
                continue;
            }
            // PNG→DDS dedup (same dir OR in overrides).
            if nmu.ends_with(".PNG") {
                let dds_sib = format!("{}.DDS", &nmu[..nmu.len() - 4]);
                let dds_ov = format!("{}.DDS", &relu[6..relu.len() - 4]);
                if names_u.contains(&dds_sib)
                    || overrides.map(|o| o.contains_key(&dds_ov)).unwrap_or(false)
                {
                    continue;
                }
            }
            seen.insert(relu.clone());
            let ft = e.metadata().map(|m| filetime_of(&m)).unwrap_or(0);
            let typ = ext_type(&nmu);
            files.push((relu, nmu, typ, ft));
        }
    }

    walk(media_dir, "MEDIA/", overrides, &mut files, &mut seen);

    if let Some(ov) = overrides {
        for key in ov.keys() {
            let ku = key.to_uppercase();
            if ku.ends_with(".BINDAT") || ku.ends_with(".BINLAYOUT") || is_pack_excluded(&ku) {
                continue;
            }
            let relu = format!("MEDIA/{ku}");
            if seen.contains(&relu) {
                continue;
            }
            seen.insert(relu.clone());
            let name = ku.rsplit('/').next().unwrap_or(&ku).to_string();
            let typ = ext_type(&ku);
            files.push((relu, name, typ, 0));
        }
    }
    files
}

/// Manifest dir-tree from a MEDIA scan (mirrors build_manifest_dirs).
pub fn build_manifest_dirs(media_dir: &Path, overrides: Option<&Overrides>) -> (Dirs, u32) {
    let files = collect_media_files(media_dir, overrides);

    let mut dir_files: BTreeMap<String, Vec<Rec>> = BTreeMap::new();
    let mut dir_subdirs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();

    fn ensure_dir(
        dpath: &str,
        all_dirs: &mut BTreeSet<String>,
        dir_files: &mut BTreeMap<String, Vec<Rec>>,
        dir_subdirs: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        if all_dirs.contains(dpath) {
            return;
        }
        all_dirs.insert(dpath.to_string());
        dir_files.entry(dpath.to_string()).or_default();
        dir_subdirs.entry(dpath.to_string()).or_default();
        if dpath == "MEDIA/" {
            return;
        }
        let trimmed = &dpath[..dpath.len() - 1];
        let parent = match trimmed.rfind('/') {
            Some(i) => format!("{}/", &trimmed[..i]),
            None => "MEDIA/".to_string(),
        };
        ensure_dir(&parent, all_dirs, dir_files, dir_subdirs);
        let leaf = dpath[parent.len()..].to_string();
        dir_subdirs.get_mut(&parent).unwrap().insert(leaf);
    }

    ensure_dir("MEDIA/", &mut all_dirs, &mut dir_files, &mut dir_subdirs);
    for (relu, name, typ, ft) in files {
        let dpath = match relu.rfind('/') {
            Some(i) => format!("{}/", &relu[..i]),
            None => "MEDIA/".to_string(),
        };
        ensure_dir(&dpath, &mut all_dirs, &mut dir_files, &mut dir_subdirs);
        dir_files
            .get_mut(&dpath)
            .unwrap()
            .push((0, typ, name, 0, 0, ft));
    }

    let mut dirs: Dirs = vec![(
        String::new(),
        vec![(0, TYPE_DIR, "MEDIA/".to_string(), 0, 0, 0)],
    )];
    let mut file_count: u32 = 1;
    for dpath in &all_dirs {
        let mut recs = dir_files.remove(dpath).unwrap_or_default();
        for leaf in &dir_subdirs[dpath] {
            recs.push((0, TYPE_DIR, leaf.clone(), 0, 0, 0));
        }
        file_count += recs.len() as u32;
        dirs.push((dpath.clone(), recs));
    }
    (dirs, file_count)
}

/// Bytes packed for a manifest entry (UPPER rel path, no MEDIA/ prefix):
/// compiled form first (overrides then disk), else the raw file.
fn content_for(rel_upper: &str, media_dir: &Path, overrides: Option<&Overrides>) -> Option<Vec<u8>> {
    let mut cands: Vec<String> = Vec::new();
    if rel_upper.ends_with(".DAT")
        || rel_upper.ends_with(".TEMPLATE")
        || rel_upper.ends_with(".ANIMATION")
        || rel_upper.ends_with(".HIE")
    {
        cands.push(format!("{rel_upper}.BINDAT"));
    } else if rel_upper.ends_with(".LAYOUT") {
        cands.push(format!("{rel_upper}.BINLAYOUT"));
    }
    cands.push(rel_upper.to_string());
    if let Some(ov) = overrides {
        for c in &cands {
            if let Some(data) = ov.get(c) {
                return Some(data.clone());
            }
        }
    }
    for c in &cands {
        let disk = media_dir.join(c.replace('/', std::path::MAIN_SEPARATOR_STR));
        // read() already errors on a missing path or a directory, so try it
        // directly rather than paying a separate is_file() stat per candidate.
        if let Ok(data) = std::fs::read(&disk) {
            return Some(data);
        }
    }
    None
}

/// Build a .MOD entirely from scratch. Returns the written byte count.
pub fn pack_mod(
    media_dir: &Path,
    output_path: &Path,
    original_mod_dir: Option<&Path>,
    overrides: Option<&Overrides>,
) -> std::io::Result<usize> {
    let default_mod_dir = media_dir.parent().unwrap_or(media_dir).to_path_buf();
    let mod_dir = original_mod_dir.unwrap_or(&default_mod_dir);

    let mut h = build_header(mod_dir, None);
    let ph = Phase::start("pack: manifest scan");
    let (dirs, file_count) = build_manifest_dirs(media_dir, overrides);
    drop(ph);
    h.fc = file_count;

    // Pass A: resolve every record to a placeholder or a content block. The
    // content fetch (disk reads of every non-compiled file) dominates pack
    // time on texture-heavy mods when serial, so it runs in parallel with
    // order preserved (record order is untouched — only the reads overlap).
    let ph = Phase::start("pack: resolve content (A)");
    enum Item {
        Placeholder(Rec),
        Block(usize), // index into blocks
    }
    struct Block {
        typ: u8,
        name: String,
        ft: u64,
        content: Vec<u8>,
        size: u32, // decompressed (original) length
        store: bool,
        comp: Vec<u8>,
        csz: u32,
        crc: u32,
    }
    // A1: enumerate records; collect one fetch job per non-dir record.
    enum Slot {
        Placeholder(Rec),
        File { typ: u8, name: String, ft: u64, job: usize },
    }
    let mut shape: Vec<(String, Vec<Slot>)> = Vec::new();
    let mut jobs: Vec<String> = Vec::new(); // rel_upper per job
    for (dname, recs) in &dirs {
        let mut slots = Vec::new();
        for (_crc, typ, name, _off, _size, ft) in recs {
            if *typ == TYPE_DIR {
                slots.push(Slot::Placeholder((0, *typ, name.clone(), 0, 0, *ft)));
                continue;
            }
            let full_upper = format!("{dname}{name}").to_uppercase();
            let rel_upper = full_upper
                .strip_prefix("MEDIA/")
                .unwrap_or(&full_upper)
                .to_string();
            slots.push(Slot::File {
                typ: *typ,
                name: name.clone(),
                ft: *ft,
                job: jobs.len(),
            });
            jobs.push(rel_upper);
        }
        shape.push((dname.clone(), slots));
    }
    // A2: parallel fetch (order-preserving collect).
    let contents: Vec<Option<Vec<u8>>> = jobs
        .par_iter()
        .map(|rel| content_for(rel, media_dir, overrides))
        .collect();
    // A3: build the ordered plan + block list.
    let mut plan: Vec<(String, Vec<Item>)> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut contents = contents;
    for (dname, slots) in shape {
        let mut items = Vec::new();
        for slot in slots {
            match slot {
                Slot::Placeholder(rec) => items.push(Item::Placeholder(rec)),
                Slot::File { typ, name, ft, job } => match contents[job].take() {
                    None => items.push(Item::Placeholder((0, typ, name, 0, 0, ft))),
                    Some(content) => {
                        let store = is_store_type(typ) || content.len() >= 0x190_0000;
                        let size = content.len() as u32;
                        blocks.push(Block {
                            typ,
                            name,
                            ft,
                            content,
                            size,
                            store,
                            comp: Vec::new(),
                            csz: 0,
                            crc: 0,
                        });
                        items.push(Item::Block(blocks.len() - 1));
                    }
                },
            }
        }
        plan.push((dname, items));
    }

    drop(ph);
    // Pass B: compress + crc in parallel. Backend = libdeflate at level 6
    // (~2.8x miniz_oxide at a slightly BETTER ratio on real mod data); output
    // is a standard zlib stream — the game inflates it identically.
    let ph = Phase::start("pack: compress+crc (B)");
    blocks.par_iter_mut().for_each(|blk| {
        blk.crc = crc32fast::hash(&blk.content);
        if blk.store {
            blk.comp = std::mem::take(&mut blk.content);
            blk.csz = 0;
        } else {
            let mut c = libdeflater::Compressor::new(
                libdeflater::CompressionLvl::new(6).expect("valid zlib level"),
            );
            let mut buf = vec![0u8; c.zlib_compress_bound(blk.content.len())];
            let n = c
                .zlib_compress(&blk.content, &mut buf)
                .expect("zlib_compress_bound-sized buffer");
            buf.truncate(n);
            blk.comp = buf;
            blk.csz = n as u32;
            blk.content = Vec::new();
        }
    });

    drop(ph);
    // Pass C: assemble the data section strictly in manifest order.
    let ph = Phase::start("pack: assemble (C)");
    let mut data: Vec<u8> = vec![0u8; 8]; // [maxCompressedBlockSize][rollingHash]
    let mut max_csz: u32 = 0;
    let mut new_dirs: Dirs = Vec::new();
    for (dname, items) in plan {
        let mut nr: Vec<Rec> = Vec::new();
        for it in items {
            match it {
                Item::Placeholder(rec) => nr.push(rec),
                Item::Block(bi) => {
                    let blk = &blocks[bi];
                    let off = data.len() as u32;
                    data.extend_from_slice(&blk.size.to_le_bytes());
                    data.extend_from_slice(&blk.csz.to_le_bytes());
                    data.extend_from_slice(&blk.comp);
                    if blk.csz > max_csz {
                        max_csz = blk.csz;
                    }
                    nr.push((blk.crc, blk.typ, blk.name.clone(), off, blk.size, blk.ft));
                }
            }
        }
        new_dirs.push((dname, nr));
    }

    data[0..4].copy_from_slice(&max_csz.to_le_bytes());
    let rh = pak_rolling_hash(&data);
    data[4..8].copy_from_slice(&rh.to_le_bytes());

    drop(ph);
    let ph = Phase::start("pack: hash+manifest+write");
    use std::io::Write;
    let header_len = w_header(&h, 0, 0).len();
    // The .MOD stores off_data / off_man / per-record data offsets as u32. A
    // data section past 4 GiB would silently wrap them into a corrupt archive.
    // Real mods are orders of magnitude under this, so the guard never fires —
    // it just turns an undebuggable bad .MOD into a clear error. off_man
    // (header + data) is the largest u32 offset, so bounding it covers all.
    let off_man_usize = header_len
        .checked_add(data.len())
        .filter(|&t| t <= u32::MAX as usize)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "data section too large for the .MOD u32 offset table \
                 ({} bytes, max {})",
                header_len as u64 + data.len() as u64,
                u32::MAX
            ))
        })?;
    let off_data = header_len as u32;
    let off_man = off_man_usize as u32;
    let mut manifest = w_manifest(&h, &new_dirs);
    let mh = pak_manifest_hash(&manifest);
    manifest[2..6].copy_from_slice(&mh.to_le_bytes());

    // Stream header ++ data ++ manifest straight to a BufWriter. Concatenating
    // into one `out` Vec first would hold the whole data section twice (peak
    // ~2x output size); writing the three parts in order keeps one copy live.
    let header = w_header(&h, off_data, off_man);
    let f = std::fs::File::create(output_path)?;
    let mut w = std::io::BufWriter::new(f);
    w.write_all(&header)?;
    w.write_all(&data)?;
    w.write_all(&manifest)?;
    w.flush()?;
    drop(ph);
    Ok(header.len() + data.len() + manifest.len())
}

// ── conversion pipeline ────────────────────────────────────────────────────

#[derive(Default)]
pub struct ConvertResults {
    pub bindat: usize,
    pub binlayout: usize,
    pub raw: usize,
    pub mpp: usize,
    pub errors: Vec<String>,
}

/// Persist a converted file: into `overrides` (keyed by UPPER rel path) or to disk.
fn emit_converted(
    path: &Path,
    data: Vec<u8>,
    media_dir: &Path,
    overrides: Option<&mut Overrides>,
) -> std::io::Result<()> {
    match overrides {
        Some(ov) => {
            let rel = path
                .strip_prefix(media_dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
                .to_uppercase();
            ov.insert(rel, data);
            Ok(())
        }
        None => std::fs::write(path, data),
    }
}

fn walk_files_sorted(root: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<(String, PathBuf, bool)> = match std::fs::read_dir(root) {
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
        walk_files_sorted(&d, out);
    }
}

/// Run all conversions on a MEDIA directory (mirrors convert_all).
pub fn convert_all(
    media_dir: &Path,
    mut overrides: Option<&mut Overrides>,
    mpp_backend: &str,
    raw_mode: &str,
) -> ConvertResults {
    let mut results = ConvertResults::default();

    let mut all_files = Vec::new();
    walk_files_sorted(media_dir, &mut all_files);
    let mut dat_jobs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut layout_jobs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for f in &all_files {
        let fu = f
            .file_name()
            .map(|s| s.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        let is_dat = (fu.ends_with(".DAT")
            || fu.ends_with(".TEMPLATE")
            || fu.ends_with(".ANIMATION")
            || fu.ends_with(".HIE"))
            && !fu.contains(".BIN");
        if is_dat {
            let mut bp = f.as_os_str().to_owned();
            bp.push(".BINDAT");
            dat_jobs.push((f.clone(), PathBuf::from(bp)));
        } else if fu.ends_with(".LAYOUT") {
            let mut bp = f.as_os_str().to_owned();
            bp.push(".BINLAYOUT");
            layout_jobs.push((f.clone(), PathBuf::from(bp)));
        }
    }

    // 1. DAT → BINDAT (parallel; per-file hash ids are self-contained).
    // Optional: env MIKURO_BINDAT_DICT=<path to bindat_string_dict.bin> makes
    // BINDAT use the GLOBAL corpus-id scheme (the exact GUTS sub_10289A40 /
    // sub_1023E9F0 behaviour) instead of per-file hash ids. Both load fine in
    // the game (per-file id resolution is proven); corpus mode additionally
    // matches the SHIPPED binaries byte-for-byte where the text matches.
    let corpus_dict = match std::env::var("MIKURO_BINDAT_DICT") {
        Ok(p) if !p.is_empty() => match crate::bindat::CorpusDict::load(Path::new(&p)) {
            Ok(d) => {
                eprintln!("  BINDAT: using GLOBAL corpus string-ids ({} strings)", d.len());
                Some(d)
            }
            Err(e) => {
                results.errors.push(format!("BINDAT dict: {e} (falling back to hash ids)"));
                None
            }
        },
        _ => None,
    };
    eprintln!(
        "  Converting DAT→BINDAT ({}) + LAYOUT→BINLAYOUT ({})...",
        dat_jobs.len(),
        layout_jobs.len()
    );
    let ph = Phase::start("convert: bindat+binlayout");
    // Compile the DAT and LAYOUT jobs in one work-stealing wave: rayon::join lets
    // the two par_iters share the pool, so a lopsided mix (few DAT + many LAYOUT,
    // or the reverse) still saturates every core instead of draining one kind to
    // a barrier before the other starts. Both collects stay order-preserving; the
    // emit order (all BINDAT, then all BINLAYOUT) is unchanged.
    type CompileOut = Vec<(PathBuf, Result<Vec<u8>, String>)>;
    // The DAT arm also carries the source path + the `UNK_<HEX8>` placeholder
    // report, so the raw-hash warning below can name the offending file.
    type DatCompileOut = Vec<(PathBuf, PathBuf, Result<crate::bindat::CompiledDat, String>)>;
    let (bindat_out, layout_out): (DatCompileOut, CompileOut) = rayon::join(
        || {
            dat_jobs
                .par_iter()
                .map(|(dp, bp)| {
                    let res = crate::bindat::compile_dat_file_reported(dp, corpus_dict.as_ref());
                    (
                        dp.clone(),
                        bp.clone(),
                        res.map_err(|e| format!("{}: {e}", dp.display())),
                    )
                })
                .collect()
        },
        || {
            layout_jobs
                .par_iter()
                .map(|(lp, bp)| {
                    (
                        bp.clone(),
                        compile_layout_file(lp).map_err(|e| format!("{}: {e}", lp.display())),
                    )
                })
                .collect()
        },
    );
    // Sources that came from an unpack whose section/key names could not be
    // reversed still carry `UNK_<HEX8>` placeholders. They compile CORRECTLY (the
    // original hash is written back verbatim, so the engine still sees the data),
    // but the readable name is gone — say so once, loudly, with examples.
    let mut raw_hash_hits: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for (dp, bp, res) in bindat_out {
        match res {
            Ok(gen) => {
                if !gen.raw_hash_names.is_empty() {
                    raw_hash_hits.push((dp, gen.raw_hash_names));
                }
                if emit_converted(&bp, gen.bytes, media_dir, overrides.as_deref_mut()).is_ok() {
                    results.bindat += 1;
                }
            }
            Err(e) => results.errors.push(format!("BINDAT {e}")),
        }
    }
    if !raw_hash_hits.is_empty() {
        let total: usize = raw_hash_hits.iter().map(|(_, n)| n.len()).sum();
        eprintln!(
            "  WARNING: {} DAT file(s) still use {} UNK_<hex> placeholder name(s)\n\
             \x20          left by an earlier unpack. Their original hashes were written back\n\
             \x20          verbatim, so the game still reads these sections/properties — but\n\
             \x20          the readable name is lost. Replace them with the real name where\n\
             \x20          you know it; NEVER rename an UNK_ token by hand (it changes the hash).",
            raw_hash_hits.len(),
            total
        );
        for (dp, names) in raw_hash_hits.iter().take(10) {
            let shown: Vec<&str> = names.iter().take(6).map(|s| s.as_str()).collect();
            let more = if names.len() > shown.len() {
                format!(", +{} more", names.len() - shown.len())
            } else {
                String::new()
            };
            eprintln!("    {}: {}{}", dp.display(), shown.join(", "), more);
        }
        if raw_hash_hits.len() > 10 {
            eprintln!("    ... and {} more file(s)", raw_hash_hits.len() - 10);
        }
    }
    for (bp, res) in layout_out {
        match res {
            Ok(gen) => {
                if emit_converted(&bp, gen, media_dir, overrides.as_deref_mut()).is_ok() {
                    results.binlayout += 1;
                }
            }
            Err(e) => results.errors.push(format!("BINLAYOUT {e}")),
        }
    }

    drop(ph);
    // 3. RAW indices (ON-DEMAND per content type).
    let ph = Phase::start("convert: raw");
    if !matches!(raw_mode, "none" | "skip") {
        eprintln!("  Generating RAW files (on-demand per content type)...");
        results.raw = generate_raw_files(media_dir, overrides.as_deref_mut());
    } else {
        eprintln!("  Skipping RAW generation (raw=none).");
    }

    drop(ph);
    // 4. MPP pathing grids.
    let ph = Phase::start("convert: mpp");
    if !matches!(mpp_backend, "none" | "skip") {
        eprintln!("  Generating MPP files (backend={mpp_backend})...");
        match generate_mpp_files(media_dir, overrides.as_deref_mut(), mpp_backend, None) {
            Ok(n) => results.mpp = n,
            // No silent downgrade to the approximate backend: asking for grids
            // and getting different ones without noticing is worse than a
            // failed build you can see.
            Err(e) => results.errors.push(format!("MPP: {e}")),
        }
    }

    drop(ph);
    results
}

fn emit_raw(
    raw_name: &str,
    data: Option<Vec<u8>>,
    media_dir: &Path,
    overrides: Option<&mut Overrides>,
) -> usize {
    let data = match data {
        Some(d) if !d.is_empty() => d,
        _ => return 0,
    };
    match overrides {
        Some(ov) => {
            ov.insert(raw_name.to_uppercase(), data);
            1
        }
        None => {
            if std::fs::write(media_dir.join(raw_name), data).is_ok() {
                1
            } else {
                0
            }
        }
    }
}

/// Generate the 7 RAW index files. The builders are independent, so they run
/// CONCURRENTLY (each internally rayon-parallel too — rayon nests fine); the
/// EMIT order stays the canonical _run_raw_builders sequence, so the manifest
/// record order is unchanged.
pub fn generate_raw_files(media_dir: &Path, mut overrides: Option<&mut Overrides>) -> usize {
    fn timed(name: &'static str, f: impl FnOnce() -> Option<Vec<u8>>) -> Option<Vec<u8>> {
        let t0 = std::time::Instant::now();
        let r = f();
        if timing_enabled() {
            eprintln!("      [timing] raw {:<20} {:>8.3}s", name, t0.elapsed().as_secs_f64());
        }
        r
    }
    let mut roompieces = None;
    let mut skills = None;
    let mut missiles = None;
    let mut affixes = None;
    let mut triggerables = None;
    let mut unitdata = None;
    let mut ui = None;
    rayon::scope(|s| {
        s.spawn(|_| roompieces = timed("ROOMPIECES", || raw::build_roompieces(media_dir)));
        s.spawn(|_| skills = timed("SKILLS", || raw::build_skills(media_dir)));
        s.spawn(|_| missiles = timed("MISSILES", || raw::build_missiles(media_dir)));
        s.spawn(|_| affixes = timed("AFFIXES", || raw::build_affixes(media_dir)));
        s.spawn(|_| triggerables = timed("TRIGGERABLES", || raw::build_triggerables(media_dir)));
        s.spawn(|_| unitdata = timed("UNITDATA", || raw::build_unitdata(media_dir)));
        s.spawn(|_| ui = timed("UI", || raw::build_ui(media_dir)));
    });
    let mut count = 0;
    count += emit_raw("ROOMPIECES.RAW", roompieces, media_dir, overrides.as_deref_mut());
    count += emit_raw("SKILLS.RAW", skills, media_dir, overrides.as_deref_mut());
    count += emit_raw("MISSILES.RAW", missiles, media_dir, overrides.as_deref_mut());
    count += emit_raw("AFFIXES.RAW", affixes, media_dir, overrides.as_deref_mut());
    count += emit_raw("TRIGGERABLES.RAW", triggerables, media_dir, overrides.as_deref_mut());
    count += emit_raw("UNITDATA.RAW", unitdata, media_dir, overrides.as_deref_mut());
    count += emit_raw("UI.RAW", ui, media_dir, overrides.as_deref_mut());
    count
}

/// Generate .MPP pathing grids with the offline backend ("re"), or none.
///
/// There is no `dll` backend here any more. It shelled out to TL2's own console
/// host, which only builds projects under `<install>\mods\<name>` — so it copied
/// the whole mod there (deleting whatever already had that name), and in the
/// default in-memory mode copied it to a temp dir first, two full copies before
/// the build even started. Anyone who needs byte-exact grids should drive the
/// console host directly; the pointer is in `MPP_DLL_REMOVED`.
pub fn generate_mpp_files(
    media_dir: &Path,
    mut overrides: Option<&mut Overrides>,
    backend: &str,
    install_dir: Option<&Path>,
) -> Result<usize, String> {
    match backend {
        "none" | "skip" => return Ok(0),
        "dll" => return Err(MPP_DLL_REMOVED.to_string()),
        "re" => {}
        other => return Err(format!("unknown mpp backend: {other:?}")),
    }

    let base_install = install_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(tl2_media_dir);
    let layouts_dir = media_dir.join("LAYOUTS");
    let mut jobs = Vec::new();
    let mut all = Vec::new();
    walk_files_sorted(&layouts_dir, &mut all);
    for f in all {
        if f.file_name()
            .map(|s| s.to_string_lossy().to_uppercase().ends_with(".LAYOUT"))
            .unwrap_or(false)
        {
            jobs.push(f);
        }
    }
    if jobs.is_empty() {
        return Ok(0);
    }

    let ctx = crate::mpp::pipeline::Context::new(media_dir, Some(&base_install));
    let out: Vec<(PathBuf, Option<Vec<u8>>)> = jobs
        .par_iter()
        .map(|lp| (lp.clone(), crate::mpp::compile_mpp(lp, &ctx).ok()))
        .collect();

    let mut count = 0;
    let mut failed: Vec<(PathBuf, &str)> = Vec::new();
    for (lp, data) in out {
        let lps = lp.to_string_lossy().to_string();
        let mpp_path = if lps.to_uppercase().ends_with(".LAYOUT") {
            PathBuf::from(format!("{}.mpp", &lps[..lps.len() - ".LAYOUT".len()]))
        } else {
            PathBuf::from(format!("{lps}.mpp"))
        };
        match data {
            Some(d) if !d.is_empty() => {
                let _ = emit_converted(&mpp_path, d, media_dir, overrides.as_deref_mut());
                count += 1;
            }
            _ => {
                // fall back to the base install's shipped .mpp (stale but walkable)
                let rel = mpp_path
                    .strip_prefix(media_dir)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| mpp_path.clone());
                let base_mpp = base_install.join(&rel);
                if base_mpp.exists() {
                    if let Ok(bytes) = std::fs::read(&base_mpp) {
                        let _ = emit_converted(&mpp_path, bytes, media_dir, overrides.as_deref_mut());
                        count += 1;
                        failed.push((lp, "compile failed -> copied base .mpp (stale)"));
                        continue;
                    }
                }
                failed.push((lp, "compile failed AND no base .mpp -> tile will be STUBBED"));
            }
        }
    }
    if !failed.is_empty() {
        eprintln!("  WARNING: {} layout(s) failed offline MPP compile:", failed.len());
        for (lp, msg) in failed.iter().take(12) {
            eprintln!("           {}: {msg}", lp.display());
        }
    }
    Ok(count)
}

// ── deploy helpers ─────────────────────────────────────────────────────────

/// Real Documents folder (OneDrive-aware, HKCU Shell Folders\Personal).
pub fn documents_dir() -> PathBuf {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders")
        {
            if let Ok(val) = k.get_value::<String, _>("Personal") {
                return PathBuf::from(expand_env(&val));
            }
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join("Documents")
}

/// %VAR% expansion (os.path.expandvars subset for registry values).
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        if let Some(j) = rest[i + 1..].find('%') {
            let var = &rest[i + 1..i + 1 + j];
            match std::env::var(var) {
                Ok(v) => out.push_str(&v),
                Err(_) => {
                    out.push('%');
                    out.push_str(var);
                    out.push('%');
                }
            }
            rest = &rest[i + 2 + j..];
        } else {
            out.push_str(&rest[i..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// %Documents%\My Games\Runic Games\Torchlight 2\mods
pub fn tl2_mods_dir() -> PathBuf {
    documents_dir()
        .join("My Games")
        .join("Runic Games")
        .join("Torchlight 2")
        .join("mods")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removed byte-exact backend must FAIL, loudly and with directions —
    /// never quietly hand back approximate grids. Someone passing `--mpp dll`
    /// asked for byte-exact output; silently substituting the offline backend
    /// would be the one outcome they cannot detect.
    #[test]
    fn the_dll_backend_is_gone_and_says_where_to_go() {
        let err = generate_mpp_files(Path::new("/nonexistent/MEDIA"), None, "dll", None)
            .expect_err("`dll` must not silently succeed");
        assert!(err.contains("TL2-Mikuro-Console.exe"), "names the tool to run: {err}");
        assert!(err.contains("--mpp none"), "and how to pack afterwards: {err}");
        // `none` and `re` are what is left; an unknown value still says so.
        assert_eq!(generate_mpp_files(Path::new("/x"), None, "none", None), Ok(0));
        assert!(generate_mpp_files(Path::new("/x"), None, "nope", None)
            .unwrap_err()
            .contains("unknown mpp backend"));
    }

    #[test]
    fn murmur64b_matches_reference() {
        // Vectors from an independently written MurmurHash64B reference
        // (published algorithm, h2 seeded 0 to match GUTS' dword seed).
        for (data, seed, want) in [
            (&b""[..], 0u32, 0x0000_0000_0000_0000u64),
            (&b"A"[..], 0, 0x1579_D371_5812_5E2A),
            (&b"AB"[..], 1, 0xF379_0A5B_8A57_176F),
            (&b"ABC"[..], 0x22D0, 0xFC98_F3DA_7FBF_4574),
            (&b"ABCD"[..], 0x8810, 0x8242_8989_757C_335E),
            (&b"ABCDE"[..], 7, 0xB98B_BEF6_4DE9_61A7),
            (&b"ABCDEFGH"[..], 0x22D0, 0xF2B8_C64C_3166_2B70),
            (&b"ABCDEFGHI"[..], 0x8810, 0x9B7F_FB8C_BECF_C439),
            (&[0, 1, 2, 3, 4, 5, 6, 7][..], 0x22D0, 0x8895_E368_A565_7C88),
        ] {
            assert_eq!(
                murmur64b(data, seed),
                want,
                "murmur64b({data:?}, {seed:#X})"
            );
        }
    }

    #[test]
    fn req_hash_chain() {
        let r = |v: &[(u64, u16)]| -> u64 {
            let owned: Vec<(String, u64, u16)> =
                v.iter().map(|(g, ve)| (String::new(), *g, *ve)).collect();
            req_hash(&owned)
        };
        // Empty REQUIRED_MODS → 0, the DLL's early-out (sub_103F5500 line 1).
        assert_eq!(r(&[]), 0);
        assert_eq!(r(&[(0x0123_4567_89AB_CDEF, 1)]), 0xA20A_F901_F06F_605A);
        assert_eq!(
            r(&[(0x0123_4567_89AB_CDEF, 1), (0xFEDC_BA98_7654_3210, 42)]),
            0x42E3_B278_9860_8F92
        );
        assert_eq!(
            r(&[
                (0x1111_1111_1111_1111, 0),
                (0x2222_2222_2222_2222, 65535),
                (0x3333_3333_3333_3333, 7)
            ]),
            0xD6E6_934F_4CF0_8084
        );
    }

    #[test]
    fn pack_exclude_suffixes() {
        // GUTS sub_103F4340 blacklist (table built at sub_11D44D40).
        for n in [
            "FOO.CMP",
            "FOO.THUMBNAIL.PNG",
            "SHEET.XLS",
            "DATA.XML",
            "SCENE.MAX",
            "X.MPD",
            "SHORTCUT.LNK",
            "OGRE.LOG",
        ] {
            assert!(is_pack_excluded(n), "{n} should be excluded");
        }
        // Shipping files must survive — including the .PNG that is not a thumbnail.
        for n in [
            "TEX.PNG",
            "TEX.DDS",
            "MODEL.MESH",
            "UNIT.DAT",
            "TOWN.LAYOUT",
            "SND.OGG",
            "M.MATERIAL",
            "UI.IMAGESET",
        ] {
            assert!(!is_pack_excluded(n), "{n} must not be excluded");
        }
    }

    #[test]
    fn collect_media_files_drops_tool_artifacts() {
        let root = std::env::temp_dir().join(format!("tl2pack_excl_{}", std::process::id()));
        let media = root.join("MEDIA");
        let sub = media.join("UNITS");
        std::fs::create_dir_all(&sub).unwrap();
        for (name, body) in [
            ("KEEP.DAT", "x"),
            ("KEEP.PNG", "x"),
            ("DROP.XLS", "x"),
            ("DROP.LOG", "x"),
            ("DROP.THUMBNAIL.PNG", "x"),
            ("DROP.MAX", "x"),
            ("DROP.CMP", "x"),
            ("DROP.MPD", "x"),
            ("DROP.LNK", "x"),
            ("DROP.XML", "x"),
        ] {
            std::fs::write(sub.join(name), body).unwrap();
        }

        let got: Vec<String> = collect_media_files(&media, None)
            .into_iter()
            .map(|(_, name, _, _)| name)
            .collect();

        assert!(got.contains(&"KEEP.DAT".to_string()), "got {got:?}");
        assert!(got.contains(&"KEEP.PNG".to_string()), "got {got:?}");
        for dropped in got.iter().filter(|n| n.starts_with("DROP")) {
            panic!("tool artifact leaked into the pack list: {dropped}");
        }

        std::fs::remove_dir_all(&root).ok();
    }
}
