//! Interactive terminal UI (ratatui) for the drop-in-and-run flow.
//!
//! When the exe is launched with no arguments — e.g. double-clicked, or dropped
//! into the game folder / a mod-source folder and run — `run()` takes over:
//! it discovers packable mod sources under the exe's directory, shows a ratatui
//! picker, and packs the chosen mod(s). The picker is selection-only; once the
//! user confirms we restore the terminal and pack with the normal console
//! output, so the proven `convert_all` / `pack_mod` core is untouched.
//!
//! Binary-only: this module is never part of the library the wasm front-end
//! reuses via `#[path]`, so its ratatui dependency stays off the wasm target.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use tl2_mikuro_mod_packer_rs::pak::PakReader;
use tl2_mikuro_mod_packer_rs::packer::{read_mod_metadata, tl2_mods_dir};

/// Emberworks palette (余烬工坊) mapped to truecolor terminal colors — the same
/// tokens the web site uses, so the packer reads as the same brand. Semantics:
/// ember = primary/new, gold = meta/in-progress, set-green = ok/done,
/// magic-blue = tagged/level, flame = warning, muted/faint = secondary text.
mod theme {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(0x10, 0x0E, 0x0B); // 炉底黑, frame
    pub const PANEL: Color = Color::Rgb(0x19, 0x14, 0x10); // panel/card
    pub const LINE: Color = Color::Rgb(0x2C, 0x24, 0x1B); // border
    pub const LINE_HI: Color = Color::Rgb(0x45, 0x37, 0x24); // strong border / selection
    pub const TEXT: Color = Color::Rgb(0xD9, 0xCD, 0xB6);
    pub const MUTED: Color = Color::Rgb(0x94, 0x85, 0x6D);
    pub const FAINT: Color = Color::Rgb(0x67, 0x5B, 0x48);
    pub const EMBER: Color = Color::Rgb(0xFF, 0x9A, 0x3C); // 余烬橙, primary accent
    pub const EMBER_HI: Color = Color::Rgb(0xFF, 0xC2, 0x6B); // hover / selected text
    pub const GOLD: Color = Color::Rgb(0xD8, 0xB2, 0x6A); // meta / in-progress
    pub const MAGIC: Color = Color::Rgb(0x6F, 0xA8, 0xFF); // tagged / level mod
    pub const SET: Color = Color::Rgb(0x7C, 0xC9, 0x6E); // ok / done / selected mark
    /// A name the dictionary could not recover. Same warm red as the web
    /// front-end's `.bad` class, so the two surfaces read alike.
    pub const WARN: Color = Color::Rgb(0xE0, 0x59, 0x2E);
}

// ── UI language ────────────────────────────────────────────────────────────
// Only the TUI is localized. Everything the CLI prints (and every line the pack
// / unpack workers stream into the Output tab) stays English: those are the
// same strings the scriptable CLI emits, and people paste them into bug reports.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Lang {
    En,
    Zh,
    Ru,
}

impl Lang {
    fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
            Lang::Ru => "ru",
        }
    }

    /// The catalogue compiled into the exe for this language. Keeping them
    /// embedded is what preserves the single-file, drop-it-anywhere property
    /// (the same reason the base bundle is baked in) — the `.ini` layout is for
    /// the benefit of whoever edits the translations, not a runtime dependency.
    fn builtin(self) -> &'static str {
        match self {
            Lang::En => include_str!("../data/lang/en.ini"),
            Lang::Zh => include_str!("../data/lang/zh.ini"),
            Lang::Ru => include_str!("../data/lang/ru.ini"),
        }
    }

    /// Short tag for the header, in the language's own script.
    fn tag(self) -> &'static str {
        match self {
            Lang::En => "EN",
            Lang::Zh => "中文",
            Lang::Ru => "RU",
        }
    }

    /// Cycle order for the in-app switch.
    fn next(self) -> Lang {
        match self {
            Lang::En => Lang::Zh,
            Lang::Zh => Lang::Ru,
            Lang::Ru => Lang::En,
        }
    }
}

/// `TL2_LANG=en|zh` wins; otherwise the Windows user UI language; otherwise the
/// POSIX `LC_ALL`/`LANG`; otherwise English.
///
/// The env override is not just for testing — this session's own shell exports
/// `LANG=C.UTF-8`, and a user whose Windows is English but who wants the Chinese
/// UI (or the reverse) has no other way to say so.
fn detect_lang() -> Lang {
    if let Some(v) = std::env::var_os("TL2_LANG") {
        if let Some(l) = lang_from_code(&v.to_string_lossy()) {
            return l;
        }
    }
    #[cfg(windows)]
    {
        // LANGID low 10 bits = primary language.
        //   0x04 LANG_CHINESE (zh-CN / TW / HK — all get the Simplified strings,
        //                      which is the audience this tool actually has)
        //   0x19 LANG_RUSSIAN
        let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        return match id & 0x3FF {
            0x04 => Lang::Zh,
            0x19 => Lang::Ru,
            // A Windows UI in some other language is a real answer; fall to
            // English rather than letting a stray POSIX LANG override the OS.
            _ => Lang::En,
        };
    }
    #[cfg(not(windows))]
    {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(v) = std::env::var(key) {
                if let Some(l) = lang_from_code(&v) {
                    return l;
                }
            }
        }
        Lang::En
    }
}

/// Map a locale-ish string (`zh`, `zh_CN.UTF-8`, `ru-RU`, `en_AU`) to a UI
/// language. `None` when it names none of them, so detection keeps looking.
fn lang_from_code(v: &str) -> Option<Lang> {
    let v = v.to_ascii_lowercase();
    if v.starts_with("zh") || v.starts_with("cn") {
        Some(Lang::Zh)
    } else if v.starts_with("ru") {
        Some(Lang::Ru)
    } else if v.starts_with("en") {
        Some(Lang::En)
    } else {
        None
    }
}

/// The active language's string catalogue.
///
/// Built from the embedded `.ini`, then overlaid with `<exe dir>/lang/<code>.ini`
/// if the user dropped one there — so wording can be fixed, or a whole language
/// added, without rebuilding. A key missing from the chosen language falls back
/// to English, and a key missing everywhere renders as the key itself: visibly
/// wrong beats invisibly blank.
struct Strings {
    map: std::collections::HashMap<String, String>,
}

impl Strings {
    fn load(lang: Lang, root: &Path) -> Strings {
        let mut map = std::collections::HashMap::new();
        // Four layers, each overriding the one before. Order is by SPECIFICITY,
        // not by source: both English layers go down first as the substrate a
        // partial translation falls back to, then both layers of the chosen
        // language. Ordering it the other way (…, builtin zh, disk en, disk zh)
        // means editing `lang/en.ini` to fix the English wording silently turns
        // those lines English in the Chinese and Russian UIs too.
        let mut layer = |l: Lang, from_disk: bool| {
            if from_disk {
                let p = root.join("lang").join(format!("{}.ini", l.code()));
                if let Ok(text) = std::fs::read_to_string(&p) {
                    parse_ini(&text, &mut map);
                }
            } else {
                parse_ini(l.builtin(), &mut map);
            }
        };
        if lang != Lang::En {
            layer(Lang::En, false);
            layer(Lang::En, true);
        }
        layer(lang, false);
        layer(lang, true);
        Strings { map }
    }

    /// English is always layered in underneath, so a key that resolves to
    /// `MISSING_STRING` is a code/catalogue mismatch rather than an untranslated
    /// entry — `every_key_used_in_code_exists_in_the_catalogues` makes that
    /// impossible to ship.
    fn get(&self, key: &str) -> &str {
        self.map.get(key).map(String::as_str).unwrap_or("MISSING_STRING")
    }
}

/// `key=value`, one per line. Everything after the FIRST `=` is the value
/// VERBATIM — leading spaces are significant (they indent the Help page), so
/// nothing is trimmed off the value. `#` starts a comment line.
fn parse_ini(text: &str, out: &mut std::collections::HashMap<String, String>) {
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
}

/// Shared, worker-appended log rendered live in the Output tab.
type Log = Arc<Mutex<Vec<String>>>;

fn logln(log: &Log, msg: impl Into<String>) {
    if let Ok(mut v) = log.lock() {
        v.push(msg.into());
    }
}

/// What the picker will do with a row.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    /// A mod SOURCE folder (MOD.DAT + MEDIA/) → pack it into a `.MOD`.
    Pack,
    /// A built `.MOD` file → unpack it back into an editable source folder.
    Unpack,
}

/// One row of the picker: a mod source to pack, or a `.MOD` file to unpack.
struct ModEntry {
    /// Human title (MOD.DAT DISPLAYNAME/MOD_TITLE, else folder name; for a
    /// `.MOD` file, the title from its header).
    title: String,
    /// Output base name (MOD.DAT MOD_FILE_NAME, else folder name).
    file_name: String,
    /// The mod source directory (holds MOD.DAT + MEDIA/), or the `.MOD` file.
    dir: PathBuf,
    /// True when MEDIA/LAYOUTS holds at least one *.LAYOUT — a level mod whose
    /// pathing (MPP) would want base collision geometry to regenerate.
    has_layouts: bool,
    action: Action,
    /// Where the row was found — shown on unpack rows, where the same mod can
    /// sit both next to the exe and in the game's deployed mods folder.
    origin: Origin,
}

/// Which of the scanned locations a row came from.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Origin {
    /// Next to the exe (or its `mods/` subfolder).
    Local,
    /// The game's deployed mods folder under Documents — what `--deploy` writes.
    Deployed,
}

impl Origin {
    fn label(self, s: &Strings) -> &str {
        match self {
            Origin::Local => s.get("local"),
            Origin::Deployed => s.get("game_mods"),
        }
    }
}

/// Where the base game MEDIA can come from, in priority order.
#[derive(Clone, Debug)]
enum BaseSource {
    /// A loose `MEDIA/` folder next to the exe (already extracted).
    LooseMedia(PathBuf),
    /// `PAKS/DATA.PAK` next to the exe — targeted extraction (Phase 2).
    DataPak(PathBuf),
    /// Nothing beside the exe, no `TL2_MEDIA_DIR`, and nothing remembered. The
    /// user has to choose: let the packer look for an install, or name a path.
    Unresolved,
}

/// Where the chosen base is remembered, next to the exe.
///
/// One line: the path the user picked. Kept beside the cache rather than in the
/// registry or a profile dir so that a copied tool folder carries its own answer
/// — and so deleting the folder forgets it, which is what someone clearing up
/// expects.
fn base_pref_file(root: &Path) -> PathBuf {
    root.join(".tl2packer_base_dir")
}

/// The remembered choice, if it still points at usable game data.
///
/// Re-validated on every launch rather than trusted: an install can be moved or
/// uninstalled, and silently baking against a path that no longer holds what it
/// used to is the failure this whole area keeps producing. A stale entry is
/// treated as no answer, so the user gets asked again.
fn load_base_pref(root: &Path) -> Option<BaseSource> {
    let raw = std::fs::read_to_string(base_pref_file(root)).ok()?;
    let p = PathBuf::from(raw.trim());
    if p.as_os_str().is_empty() {
        return None;
    }
    if p.is_dir() && p.join("LEVELSETS").is_dir() {
        return Some(BaseSource::LooseMedia(p));
    }
    if p.is_file() {
        return Some(BaseSource::DataPak(p));
    }
    None
}

/// Remember a choice the user made. Env-var and next-to-the-exe sources are not
/// written back — they are already stated somewhere the user controls, and
/// recording a copy of them would go stale on its own.
fn save_base_pref(root: &Path, base: &BaseSource) {
    let p = match base {
        BaseSource::LooseMedia(p) | BaseSource::DataPak(p) => p,
        BaseSource::Unresolved => return,
    };
    let _ = std::fs::write(base_pref_file(root), p.display().to_string());
}

impl BaseSource {
    /// Full description, path included — for the Help tab and the run log.
    fn label(&self) -> String {
        match self {
            BaseSource::LooseMedia(p) => format!("loose MEDIA ({})", p.display()),
            BaseSource::DataPak(p) => format!("DATA.PAK ({})", p.display()),
            BaseSource::Unresolved => "not set — choose one before packing levels".to_string(),
        }
    }

    /// Just the tier, for the header — a base path can be 60 columns wide and
    /// would push the run status off the end of the bar.
    fn short(&self) -> &'static str {
        match self {
            BaseSource::LooseMedia(_) => "loose MEDIA",
            BaseSource::DataPak(_) => "DATA.PAK",
            BaseSource::Unresolved => "not set",
        }
    }

    fn is_set(&self) -> bool {
        !matches!(self, BaseSource::Unresolved)
    }
}

/// The exe's own directory (where the user "dropped" it). Falls back to the
/// process working directory if the exe path can't be resolved.
fn app_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Detect the base MEDIA source: loose MEDIA under `root`, then a DATA.PAK under
/// `root`, then the installed game, then the embedded bundle.
///
/// The installed game is checked BEFORE the bundle because the bundle is a
/// subset, and a subset fails silently: a `.LAYOUT` it does not carry makes
/// `resolve_link_layout` return None, the linked geometry never reaches the bake,
/// and the tile ends up with VOID where that geometry's floor should be. Nothing
/// errors — the pack just quietly produces less walkable ground. When a real
/// install is on the machine there is no reason to prefer the subset.
fn detect_base(root: &Path) -> BaseSource {
    detect_base_with(
        root,
        tl2_mikuro_mod_packer_rs::packer::tl2_media_dir().as_deref(),
        load_base_pref(root),
    )
}

/// Resolve the base source WITHOUT searching the machine.
///
/// Order: loose `MEDIA/` beside the exe, a `DATA.PAK` beside the exe,
/// `TL2_MEDIA_DIR`, then whatever the user chose last time. If none answer, the
/// result is `Unresolved` and the user gets asked — the packer does not go
/// hunting for an install on its own.
///
/// The ordering puts the two "you put it right here" sources first because they
/// are unambiguous, then the env var (set per shell, so it is the deliberate
/// override), and the remembered answer last: it is a convenience, and anything
/// stated more locally should beat it.
///
/// `env_media` and `remembered` are parameters so the tier order is testable
/// without the answer depending on the machine running the tests.
fn detect_base_with(
    root: &Path,
    env_media: Option<&Path>,
    remembered: Option<BaseSource>,
) -> BaseSource {
    let media = root.join("MEDIA");
    if media.is_dir() {
        return BaseSource::LooseMedia(media);
    }
    for cand in [root.join("PAKS").join("DATA.PAK"), root.join("DATA.PAK")] {
        if cand.is_file() {
            return BaseSource::DataPak(cand);
        }
    }
    if let Some(p) = env_media {
        if p.is_dir() {
            return BaseSource::LooseMedia(p.to_path_buf());
        }
    }
    remembered.unwrap_or(BaseSource::Unresolved)
}

/// A subdir with MOD.DAT is a mod source. Reads its metadata and level status.
fn mod_entry_for(dir: &Path) -> Option<ModEntry> {
    if !dir.join("MOD.DAT").is_file() {
        return None;
    }
    let meta = read_mod_metadata(dir);
    let media = dir.join("MEDIA");
    let has_layouts = media.join("LAYOUTS").is_dir()
        && has_layout_file(&media.join("LAYOUTS"));
    Some(ModEntry {
        title: meta.title,
        file_name: meta.name,
        dir: dir.to_path_buf(),
        has_layouts,
        action: Action::Pack,
        origin: Origin::Local,
    })
}

/// A built `.MOD` file the picker can unpack. The title comes from the
/// container header, so the row reads the same as the folder it came from.
fn mod_file_entry_for(path: &Path, origin: Origin) -> Option<ModEntry> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    let title = PakReader::open_mod(path)
        .ok()
        .and_then(|r| r.header().map(|h| h.title.clone()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| stem.clone());
    Some(ModEntry {
        title,
        file_name: stem,
        dir: path.to_path_buf(),
        has_layouts: false,
        action: Action::Unpack,
        origin,
    })
}

/// Any `*.LAYOUT` under `dir` (recursive, short-circuits on the first hit).
fn has_layout_file(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if has_layout_file(&p) {
                return true;
            }
        } else if p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("LAYOUT"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Mod SOURCES to pack: subdirs holding a MOD.DAT, under `root` (the "dropped
/// into the challenger-continent folder" case) and `root/mods/*` (the "dropped
/// into the game folder" case). Deduped by path, sorted by title.
fn discover_pack_sources(root: &Path) -> Vec<ModEntry> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    // The exe dropped INSIDE the mod it builds — root itself is the source.
    // Only the subdirectories used to be scanned, so that arrangement reported
    // "nothing to pack" while standing in a perfectly packable folder.
    seen.insert(std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    if let Some(m) = mod_entry_for(root) {
        out.push(m);
    }
    for base in [root.to_path_buf(), root.join("mods")] {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if !seen.insert(std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone())) {
                continue;
            }
            if let Some(m) = mod_entry_for(&p) {
                out.push(m);
            }
        }
    }
    out.sort_by_key(|m| m.title.to_lowercase());
    out
}

/// Built `.MOD` files to unpack. Three places, because that covers where a
/// `.MOD` actually ends up: next to the exe, in `./mods`, and — the one that
/// matters most — the game's deployed mods folder under Documents, which is
/// exactly where `--deploy` (and this TUI's own pack runs) write to.
/// Deduped by canonical path, so a file reachable two ways is listed once.
fn discover_mod_files(root: &Path) -> Vec<ModEntry> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let deployed = tl2_mods_dir();
    let places = [
        (root.to_path_buf(), Origin::Local),
        (root.join("mods"), Origin::Local),
        (deployed, Origin::Deployed),
    ];
    for (base, origin) in places {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let is_mod = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("MOD"))
                .unwrap_or(false);
            if !is_mod {
                continue;
            }
            if !seen.insert(std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone())) {
                continue;
            }
            if let Some(m) = mod_file_entry_for(&p, origin) {
                out.push(m);
            }
        }
    }
    // Local files first (you are most likely to want the one you just built by
    // hand), then the deployed set; alphabetical within each.
    out.sort_by(|a, b| {
        (a.origin == Origin::Deployed, a.title.to_lowercase())
            .cmp(&(b.origin == Origin::Deployed, b.title.to_lowercase()))
    });
    out
}

/// Pause so a double-clicked console window stays up to read the result.
fn pause() {
    print!("\nPress Enter to exit...");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

/// TUI entry point (called from `main` when there are no CLI arguments).
pub fn run() -> ExitCode {
    let root = app_root();
    let base = detect_base(&root);
    let packs = discover_pack_sources(&root);
    let unpacks = discover_mod_files(&root);

    // Non-interactive discovery dump (diagnostics / CI): list what the TUI would
    // show, then exit without touching the terminal.
    if std::env::var_os("TL2_MODPACK_LIST").is_some() {
        println!("root: {}", root.display());
        println!("base: {}", base.label());
        println!("pack sources: {}", packs.len());
        for m in &packs {
            println!(
                "  - pack   {}  [out={}.MOD]  {}",
                m.title,
                m.file_name,
                if m.has_layouts { "level" } else { "content" }
            );
        }
        println!("mod files: {}", unpacks.len());
        for m in &unpacks {
            // The diagnostic dump stays English whatever the UI language is —
            // it is machine-facing output that ends up in bug reports.
            println!(
                "  - unpack {}  [{}]  {}",
                m.title,
                m.origin.label(&Strings::load(Lang::En, Path::new(""))),
                m.dir.display()
            );
        }
        return ExitCode::SUCCESS;
    }

    if packs.is_empty() && unpacks.is_empty() {
        println!("Mikuro Torchlight II Mod Un/Packer");
        println!();
        println!("Nothing to pack or unpack under:");
        println!("  {}", root.display());
        println!("  {}", root.join("mods").display());
        println!("  {}", tl2_mods_dir().display());
        println!();
        println!("A mod source is a folder containing MOD.DAT (and a MEDIA/ tree);");
        println!("a built .MOD file in any of those places can be unpacked back into one.");
        println!("Put this exe in your game folder, or in a folder of mod sources.");
        pause();
        return ExitCode::FAILURE;
    }

    // Interactive app: pick mods, run them in-app (output streams to the
    // Output tab, the app stays open on it when done).
    match run_app(packs, unpacks, base, root) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("TUI error: {e}");
            ExitCode::FAILURE
        }
    }
}

use tl2_mikuro_mod_packer_rs::pak::base_cache_ready;

/// Resolve a base MEDIA directory for MPP regeneration, logging progress to the
/// Output tab. Loose MEDIA is used directly; a DATA.PAK is extracted once into a
/// persistent cache next to the exe and reused after — but only while its
/// version stamp matches (see `BASE_CACHE_VERSION`).
///
/// There is deliberately no last-resort tier here. There used to be an embedded
/// bundle, which was a second hand-built copy of what `extract_base` produces;
/// the two drifted, the bundle kept no `.LAYOUT` files, and packs against it
/// silently dropped every linked piece of geometry. A tier that can go stale
/// without anyone noticing is worse than no tier: `Unresolved` asks instead.
fn resolve_base_media(root: &Path, base: &BaseSource, log: &Log) -> Option<PathBuf> {
    let cache = root.join(".tl2packer_base");
    match base {
        // Reachable only if a run starts with no layouts to bake; `start_run`
        // asks before any level pack gets here.
        BaseSource::Unresolved => None,
        BaseSource::LooseMedia(p) => Some(p.clone()),
        BaseSource::DataPak(pak) => {
            if base_cache_ready(&cache) {
                return Some(cache);
            }
            if cache.join("LEVELSETS").is_dir() {
                logln(log, "Base cache is from an older version — re-extracting…");
                let _ = std::fs::remove_dir_all(&cache);
            } else {
                logln(log, "Extracting base data from DATA.PAK (first run)…");
            }
            match PakReader::open_pak(pak).and_then(|mut r| r.extract_base(&cache)) {
                Ok((d, mesh, lay)) => {
                    logln(
                        log,
                        format!(
                            "  base ready: {d} DATs + {mesh} collision meshes + {lay} layouts"
                        ),
                    );
                    Some(cache)
                }
                Err(e) => {
                    logln(log, format!("  base extract failed: {e}"));
                    None
                }
            }
        }
    }
}

/// The packing worker: resolve the base once, then pack each selected mod by
/// invoking THIS exe in CLI mode with piped output, streaming every line into
/// the shared log. Running the proven CLI as a child keeps the packing core and
/// its detailed, timed output exactly as-is.
fn pack_worker(jobs: Vec<Job>, base: BaseSource, root: PathBuf, log: Log) {
    let exe = std::env::current_exe().unwrap_or_default();
    let any_level = jobs
        .iter()
        .any(|j| j.action == Action::Pack && j.has_layouts);
    let base_media = if any_level {
        resolve_base_media(&root, &base, &log)
    } else {
        None
    };
    if any_level {
        match &base_media {
            Some(p) => logln(&log, format!("Pathing base: {}", p.display())),
            None => logln(&log, "No base data — level pathing left as-is (--mpp none)."),
        }
    }
    logln(&log, "");

    let (mut ok, mut fail, mut unpacked) = (0, 0, 0);
    // Destination folder names already handed out this run. The unpack picker
    // deliberately lists a `.MOD` found next to the exe AND the deployed copy
    // of the same name as separate rows, so "same stem, different file" is a
    // normal selection — and both would otherwise unpack into one folder and
    // silently merge two mods into a source tree that is neither.
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for j in &jobs {
        let args: Vec<PathBuf> = match j.action {
            Action::Pack => {
                let mpp = if j.has_layouts && base_media.is_some() { "re" } else { "none" };
                logln(&log, format!("═══════ pack {}  (mpp={mpp}) ═══════", j.title));
                vec![
                    j.path.clone(),
                    PathBuf::from("--deploy"),
                    PathBuf::from("--mpp"),
                    PathBuf::from(mpp),
                ]
            }
            Action::Unpack => {
                // Unpack next to the .MOD, under `unpacked/<name>/` — never on
                // top of an existing source folder.
                let name = unique_dir_name(&j.file_name, &mut used);
                let dst = root.join("unpacked").join(&name);
                if name != j.file_name {
                    logln(
                        &log,
                        format!(
                            "  note: another selected .MOD is also named {}; unpacking this one into {name}/",
                            j.file_name
                        ),
                    );
                }
                logln(&log, format!("═══════ unpack {} → {} ═══════", j.title, dst.display()));
                vec![PathBuf::from("unpack"), j.path.clone(), dst]
            }
        };
        let good = run_child(&exe, &args, base_media.as_deref(), &log);
        match (j.action, good) {
            (Action::Pack, true) => ok += 1,
            (Action::Unpack, true) => unpacked += 1,
            (_, false) => fail += 1,
        }
        logln(&log, "");
    }
    logln(
        &log,
        format!("── Done: {ok} packed, {unpacked} unpacked, {fail} failed.  [q] to quit ──"),
    );
}

/// `name`, or `name_2` / `name_3` … if this run already used it. Records the
/// answer so the next caller sees it taken.
fn unique_dir_name(name: &str, used: &mut std::collections::BTreeSet<String>) -> String {
    if used.insert(name.to_string()) {
        return name.to_string();
    }
    for n in 2u32.. {
        let cand = format!("{name}_{n}");
        if used.insert(cand.clone()) {
            return cand;
        }
    }
    unreachable!("2..u32::MAX cannot all be taken")
}

/// One selected row, resolved for the worker.
struct Job {
    title: String,
    file_name: String,
    /// Source folder (pack) or `.MOD` file (unpack).
    path: PathBuf,
    has_layouts: bool,
    action: Action,
}

/// Run one child `<exe> <args…>`, streaming its stdout+stderr into the log.
/// Returns whether it succeeded.
fn run_child(exe: &Path, args: &[PathBuf], base_media: Option<&Path>, log: &Log) -> bool {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(b) = base_media {
        cmd.env("TL2_MEDIA_DIR", b);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            logln(log, format!("  spawn failed: {e}"));
            return false;
        }
    };
    let h_out = child.stdout.take().map(|s| {
        let l = log.clone();
        thread::spawn(move || stream_lines(s, &l))
    });
    let h_err = child.stderr.take().map(|s| {
        let l = log.clone();
        thread::spawn(move || stream_lines(s, &l))
    });
    let status = child.wait();
    if let Some(h) = h_out {
        let _ = h.join();
    }
    if let Some(h) = h_err {
        let _ = h.join();
    }
    status.map(|s| s.success()).unwrap_or(false)
}

fn stream_lines(r: impl Read, log: &Log) {
    for line in BufReader::new(r).lines().map_while(Result::ok) {
        logln(log, line);
    }
}

// ── ratatui selection screen ───────────────────────────────────────────────

/// Tab indices. PACK and UNPACK are independent pickers; OUTPUT is the shared
/// run log. Ordering is deliberate — the two things you DO, then the thing you
/// WATCH.
const TAB_PACK: usize = 0;
const TAB_UNPACK: usize = 1;
const TAB_OUTPUT: usize = 2;
const TAB_HELP: usize = 3;
const TAB_COUNT: usize = 4;

/// One picker: its rows, what is ticked, and where the cursor sits. PACK and
/// UNPACK each own one, so switching tabs never disturbs the other's selection.
struct Pane {
    items: Vec<ModEntry>,
    picked: Vec<bool>,
    state: ListState,
    /// Index of the first visible row. ratatui computes this while rendering,
    /// into the state it is handed; carrying it across frames is what makes the
    /// view hold still. See `draw_list`.
    offset: Cell<usize>,
}

impl Pane {
    fn new(items: Vec<ModEntry>) -> Self {
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        let n = items.len();
        Pane { items, picked: vec![false; n], state, offset: Cell::new(0) }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.items.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        self.state.select(Some((cur + delta).rem_euclid(n) as usize));
    }

    fn toggle_current(&mut self) {
        if let Some(i) = self.state.selected() {
            if i < self.picked.len() {
                self.picked[i] = !self.picked[i];
            }
        }
    }

    fn toggle_all(&mut self) {
        let all_on = !self.picked.is_empty() && self.picked.iter().all(|&b| b);
        for b in &mut self.picked {
            *b = !all_on;
        }
    }

    fn picked_count(&self) -> usize {
        self.picked.iter().filter(|&&b| b).count()
    }

    /// Swap in a fresh scan, carrying over the ticks and the cursor.
    ///
    /// Both are carried BY PATH, not by index: a folder appearing or vanishing
    /// shifts everything after it, so index-keyed state would silently move the
    /// ticks onto whatever slid into those slots — the one way a refresh could
    /// pack the wrong mod.
    ///
    /// The scroll offset is left alone. It belongs to the widget and is only
    /// meaningful against the list it was computed from, but ratatui clamps it
    /// and re-derives it from the selection on the very next render, so keeping
    /// it is what stops an unchanged list from jumping back to the top.
    fn refresh(&mut self, items: Vec<ModEntry>) {
        let ticked: std::collections::BTreeSet<PathBuf> = self
            .items
            .iter()
            .zip(&self.picked)
            .filter(|(_, &on)| on)
            .map(|(m, _)| m.dir.clone())
            .collect();
        let under_cursor = self
            .state
            .selected()
            .and_then(|i| self.items.get(i))
            .map(|m| m.dir.clone());
        let picked = items.iter().map(|m| ticked.contains(&m.dir)).collect();
        let selected = match under_cursor {
            _ if items.is_empty() => None,
            Some(dir) => Some(
                items.iter().position(|m| m.dir == dir).unwrap_or_else(|| {
                    // The row the cursor was on is gone. Hold the POSITION
                    // instead, so the cursor stays where the eye left it and
                    // lands on whatever took that row's place.
                    self.state.selected().unwrap_or(0).min(items.len() - 1)
                }),
            ),
            None => Some(0),
        };
        self.items = items;
        self.picked = picked;
        self.state.select(selected);
    }

    /// Selected indices; if none ticked, the row under the cursor.
    fn resolved(&self) -> Vec<usize> {
        let explicit: Vec<usize> = self
            .picked
            .iter()
            .enumerate()
            .filter(|(_, &b)| b)
            .map(|(i, _)| i)
            .collect();
        if !explicit.is_empty() {
            return explicit;
        }
        self.state.selected().into_iter().collect()
    }
}

/// The base-source question, asked instead of guessed.
///
/// `Choose` is the menu; `Pick` is the list of installs a search turned up. The
/// search only ever runs after the user selects it from `Choose`, and even then
/// its results are offered rather than applied — on a machine with both a Steam
/// install and a separately unpacked reference tree, only the user knows which
/// one their mod is built against.
enum Prompt {
    Choose { sel: usize },
    Pick { found: Vec<PathBuf>, sel: usize },
    /// Type or paste a path. Terminals deliver a paste as ordinary key events,
    /// so this needs no clipboard integration — but it does need to accept what
    /// Explorer's "copy as path" produces, quotes and all.
    Input { buf: String, err: bool },
    NoneFound,
}

/// Interpret a path the user typed as a base source, or reject it.
///
/// Accepts whichever of the three plausible things they pasted: the MEDIA folder
/// itself, the game root above it, or a DATA.PAK. Rejecting a path that holds no
/// game data is the whole point — a base that resolves but is not one bakes
/// tiles with their linked geometry missing and says nothing.
fn base_from_typed(raw: &str) -> Option<BaseSource> {
    let s = raw.trim().trim_matches('"').trim();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    if p.is_file() {
        return Some(BaseSource::DataPak(p));
    }
    if !p.is_dir() {
        return None;
    }
    if p.join("LEVELSETS").is_dir() {
        return Some(BaseSource::LooseMedia(p));
    }
    if p.join("MEDIA").join("LEVELSETS").is_dir() {
        return Some(BaseSource::LooseMedia(p.join("MEDIA")));
    }
    let pak = p.join("PAKS").join("DATA.PAK");
    if pak.is_file() {
        return Some(BaseSource::DataPak(pak));
    }
    None
}

/// Menu entries of `Prompt::Choose`, in order.
const CHOOSE_N: usize = 2;

struct App {
    pack: Pane,
    unpack: Pane,
    base: BaseSource,
    /// Set while the base question is on screen; swallows keys until answered.
    prompt: Option<Prompt>,
    root: PathBuf,
    tab: usize,
    log: Log,
    packing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    follow: bool, // Output tab auto-scroll to tail
    out_scroll: u16,
    help_scroll: u16,
    // Last scroll maximum each scrollable pane rendered at. Only the renderer
    // knows it (it needs the line count AND the viewport height), but the key
    // handler is where it has to be enforced — see `scroll_by`. Cell, not a
    // plain field, because `draw` takes `&App`.
    out_max: Cell<u16>,
    help_max: Cell<u16>,
    lang: Lang,
    strings: Strings,
    /// What the last rescan found, and when — shown in the header's status slot
    /// until it ages out. A rescan of an unchanged tree changes nothing on
    /// screen, so without this `r` is indistinguishable from a dead key.
    note: Option<(String, Instant)>,
}

/// How long the rescan note stays in the header, if nothing else is pressed
/// first (`handle_key` dismisses it on any other key).
const NOTE_TTL: Duration = Duration::from_secs(3);

/// Move a scroll offset, clamped to what is actually scrollable.
///
/// Clamping ONLY at render time (`offset.min(max)`) looks right and is not:
/// the stored offset keeps climbing while the view is already pinned at the
/// bottom, so holding Down for a second leaves it tens of lines past the end,
/// and every one of those invisible steps has to be paid back one keypress at
/// a time before the first visible move in the other direction.
fn scroll_by(cur: u16, delta: i32, max: u16) -> u16 {
    (cur as i32 + delta).clamp(0, max as i32) as u16
}

impl App {
    fn new(packs: Vec<ModEntry>, unpacks: Vec<ModEntry>, base: BaseSource, root: PathBuf) -> Self {
        // Open on whichever picker actually has rows.
        let tab = if packs.is_empty() && !unpacks.is_empty() { TAB_UNPACK } else { TAB_PACK };
        let lang = detect_lang();
        let strings = Strings::load(lang, &root);
        App {
            pack: Pane::new(packs),
            unpack: Pane::new(unpacks),
            base,
            prompt: None,
            root,
            tab,
            log: Arc::new(Mutex::new(Vec::new())),
            packing: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            follow: true,
            out_scroll: 0,
            help_scroll: 0,
            out_max: Cell::new(0),
            help_max: Cell::new(0),
            lang,
            strings,
            note: None,
        }
    }

    /// Look up a UI string by key (see `data/lang/*.ini`).
    fn tr(&self, key: &str) -> &str {
        self.strings.get(key)
    }

    /// Switch language. Always go through this — `lang` and `strings` must move
    /// together, and setting the field alone leaves the previous catalogue live
    /// (which looks exactly like "the translation did not apply").
    fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
        self.strings = Strings::load(lang, &self.root);
    }

    /// Cycle the UI language. In-app because someone can land in a language
    /// they cannot read — the key is in the footer of every tab, and the
    /// header shows which language is live.
    fn cycle_lang(&mut self) {
        self.set_lang(self.lang.next());
    }

    fn scroll_help(&mut self, delta: i32) {
        self.help_scroll = scroll_by(self.help_scroll, delta, self.help_max.get());
    }

    fn scroll_out(&mut self, delta: i32) {
        self.out_scroll = scroll_by(self.out_scroll, delta, self.out_max.get());
    }

    /// Drop out of tail-follow, keeping the view where it is. Without the
    /// hand-off the stored offset is whatever it was before following began
    /// (usually 0), so the first Up key would teleport to the top of the log
    /// instead of scrolling back one line from the tail.
    fn unfollow(&mut self) {
        if self.follow {
            self.follow = false;
            self.out_scroll = self.out_max.get();
        }
    }

    fn is_packing(&self) -> bool {
        self.packing.load(Ordering::Relaxed)
    }

    /// Scan the disk again and rebuild both pickers in place.
    ///
    /// The tree this app describes is one the user is actively editing while it
    /// is open: a mod folder gets added or renamed, a `.MOD` gets built or
    /// deleted, a `MEDIA/` or `DATA.PAK` turns up. All of it was discovered
    /// once, at startup, so the only way to see any of it was to quit and
    /// relaunch. The base tier is re-detected for the same reason — it is the
    /// same kind of change, and the header claims to show which tier is live.
    ///
    /// Ticks and the cursor survive (see `Pane::refresh`).
    fn refresh(&mut self) {
        self.base = detect_base(&self.root);
        self.pack.refresh(discover_pack_sources(&self.root));
        self.unpack.refresh(discover_mod_files(&self.root));
        let text = self.tr("rescanned").trim().to_string();
        self.note = Some((text, Instant::now()));
        // English, like every other line in the Output tab: it is transcript,
        // and transcripts get pasted into bug reports. It also carries the
        // counts, which the one-word header note has no room for.
        logln(
            &self.log,
            format!(
                "rescanned {}: {} mod source(s), {} .MOD file(s)",
                self.root.display(),
                self.pack.items.len(),
                self.unpack.items.len()
            ),
        );
    }

    /// The rescan note, while it is still fresh. Ageing out here rather than on
    /// a timer is what makes it vanish on its own: the event loop redraws every
    /// poll timeout whether or not a key was pressed.
    fn note(&self) -> Option<&str> {
        self.note
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTE_TTL)
            .map(|(text, _)| text.as_str())
    }

    /// The picker the current tab shows (`None` on the Output tab).
    fn pane(&self) -> Option<&Pane> {
        match self.tab {
            TAB_PACK => Some(&self.pack),
            TAB_UNPACK => Some(&self.unpack),
            _ => None,
        }
    }

    fn pane_mut(&mut self) -> Option<&mut Pane> {
        match self.tab {
            TAB_PACK => Some(&mut self.pack),
            TAB_UNPACK => Some(&mut self.unpack),
            _ => None,
        }
    }

    /// Empty the run log. Allowed mid-run on purpose — the usual reason to
    /// reach for it is that a long pack has buried the part you care about, and
    /// the worker just keeps appending after it. Only the transcript is dropped;
    /// the run itself and the done/running status are untouched.
    fn clear_log(&mut self) {
        if let Ok(mut v) = self.log.lock() {
            v.clear();
        }
        self.out_scroll = 0;
        self.follow = true;
    }

    /// Spawn the worker for the ACTIVE picker and switch to Output.
    fn start_run(&mut self) {
        let Some(pane) = self.pane() else { return };
        let picks = pane.resolved();
        if picks.is_empty() {
            return;
        }
        let jobs: Vec<Job> = picks
            .iter()
            .map(|&i| {
                let m = &pane.items[i];
                Job {
                    title: m.title.clone(),
                    file_name: m.file_name.clone(),
                    path: m.dir.clone(),
                    has_layouts: m.has_layouts,
                    action: m.action,
                }
            })
            .collect();
        // Levels need a base tree. Ask now rather than pack against a guess: the
        // wrong tree costs linked geometry silently, and the run would look
        // clean. Packs with no layouts are unaffected and go straight through.
        if !self.base.is_set() && jobs.iter().any(|j| j.action == Action::Pack && j.has_layouts) {
            self.prompt = Some(Prompt::Choose { sel: 0 });
            return;
        }
        self.tab = TAB_OUTPUT;
        self.follow = true;
        self.packing.store(true, Ordering::Relaxed);
        let (base, root, log) = (self.base.clone(), self.root.clone(), self.log.clone());
        let (packing, finished) = (self.packing.clone(), self.finished.clone());
        thread::spawn(move || {
            pack_worker(jobs, base, root, log);
            packing.store(false, Ordering::Relaxed);
            finished.store(true, Ordering::Relaxed);
        });
    }
}

fn run_app(
    packs: Vec<ModEntry>,
    unpacks: Vec<ModEntry>,
    base: BaseSource,
    root: PathBuf,
) -> std::io::Result<ExitCode> {
    let mut terminal = ratatui::init();
    let mut app = App::new(packs, unpacks, base, root);
    let res = app_loop(&mut terminal, &mut app);
    ratatui::restore();
    res
}

fn app_loop(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<ExitCode> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        // Poll (don't block) so the Output tab keeps refreshing while the worker
        // streams new lines even without keypresses.
        if !event::poll(Duration::from_millis(120))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(code) = handle_key(app, k) {
            return Ok(code);
        }
    }
}

/// One keypress. `Some(code)` means quit. Split out of the loop so the tests
/// can drive real keys against a real rendered frame — scroll clamping depends
/// on what the last frame published, which a hand-poked field cannot show.
fn handle_key(app: &mut App, k: KeyEvent) -> Option<ExitCode> {
    // Raw mode means Ctrl+C arrives as a keypress instead of a signal, so if the
    // app does not answer it, nothing does — and on the Output tab it used to
    // land on the bare `c` binding and CLEAR THE LOG. Answer it exactly like q.
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('c') | KeyCode::Char('C') if !app.is_packing() => {
                Some(ExitCode::SUCCESS)
            }
            _ => None,
        };
    }
    // The base question is modal on purpose: it is asked at the moment it
    // blocks a pack, and answering it is the only way forward.
    if app.prompt.is_some() {
        handle_prompt_key(app, k);
        return None;
    }
    // The rescan note owns the status slot while it is up, and that slot is also
    // where "selected N/M" lives — the ONLY feedback Space has. So anything but
    // `r` itself dismisses the note: press r then Space and the count answers
    // immediately, rather than looking dead until the note ages out.
    if !matches!(k.code, KeyCode::Char('r') | KeyCode::Char('R')) {
        app.note = None;
    }
    match k.code {
            // ←→ is what the footer advertises: it has room for one hint, and an
            // arrow pair needs no explaining. Tab/BackTab and the number keys go
            // on working for whoever reaches for those first.
            KeyCode::Tab | KeyCode::Right => app.tab = (app.tab + 1) % TAB_COUNT,
            KeyCode::BackTab | KeyCode::Left => app.tab = (app.tab + TAB_COUNT - 1) % TAB_COUNT,
            // Direct tab keys: faster than cycling once there are three.
            KeyCode::Char('1') => app.tab = TAB_PACK,
            KeyCode::Char('2') => app.tab = TAB_UNPACK,
            KeyCode::Char('3') => app.tab = TAB_OUTPUT,
            KeyCode::Char('4') | KeyCode::Char('?') | KeyCode::F(1) => app.tab = TAB_HELP,
            // Language switch, live on every tab (see the footer hint).
            KeyCode::Char('l') | KeyCode::Char('L') => app.cycle_lang(),
            // Change the base tree. Without this the remembered answer would be
            // a one-way door — you would have to know the dotfile exists to undo
            // a mis-click, on the one setting where being wrong is invisible.
            KeyCode::Char('b') | KeyCode::Char('B') => {
                if !app.is_packing() {
                    app.prompt = Some(Prompt::Choose { sel: 0 });
                }
            }
            // Rescan, live on every tab: what changed on disk shows up in the
            // tab counts, which are in the header wherever you are standing.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                // Not mid-run. The worker is writing the very files a scan
                // reads, so a rescan then lists a half-written `.MOD` as ready
                // to unpack — and `PakReader` cannot tell that from a finished
                // one, it just falls back to the file stem for the title.
                if !app.is_packing() {
                    app.refresh();
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                // Ignore quit while a run is going (the child would orphan).
                if !app.is_packing() {
                    return Some(ExitCode::SUCCESS);
                }
            }
            _ if app.tab == TAB_OUTPUT => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.unfollow();
                    app.scroll_out(-1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.unfollow();
                    app.scroll_out(1);
                }
                KeyCode::PageUp => {
                    app.unfollow();
                    app.scroll_out(-10);
                }
                KeyCode::PageDown => app.scroll_out(10),
                KeyCode::Home => {
                    app.follow = false;
                    app.out_scroll = 0;
                }
                KeyCode::End | KeyCode::Char('G') => app.follow = true,
                KeyCode::Char('c') => app.clear_log(),
                _ => {}
            },
            _ if app.tab == TAB_HELP => match k.code {
                KeyCode::Up | KeyCode::Char('k') => app.scroll_help(-1),
                KeyCode::Down | KeyCode::Char('j') => app.scroll_help(1),
                KeyCode::PageUp => app.scroll_help(-10),
                KeyCode::PageDown => app.scroll_help(10),
                KeyCode::Home => app.help_scroll = 0,
                KeyCode::End => app.help_scroll = app.help_max.get(),
                _ => {}
            },
            _ if !app.is_packing() => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(p) = app.pane_mut() { p.move_by(-1) }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(p) = app.pane_mut() { p.move_by(1) }
                }
                KeyCode::Char(' ') => {
                    if let Some(p) = app.pane_mut() { p.toggle_current() }
                }
                KeyCode::Char('a') => {
                    if let Some(p) = app.pane_mut() { p.toggle_all() }
                }
                KeyCode::Enter => app.start_run(),
                _ => {}
            },
            _ => {}
    }
    None
}

/// Keys while the base question is up. Esc backs out without choosing, which
/// leaves the base unset — the pack simply does not start, rather than starting
/// against something nobody picked.
fn handle_prompt_key(app: &mut App, k: KeyEvent) {
    let up = matches!(k.code, KeyCode::Up | KeyCode::Char('k'));
    let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
    match app.prompt.as_mut() {
        Some(Prompt::Choose { sel }) => {
            if up {
                *sel = (*sel + CHOOSE_N - 1) % CHOOSE_N;
                return;
            }
            if down {
                *sel = (*sel + 1) % CHOOSE_N;
                return;
            }
            let chosen = match k.code {
                KeyCode::Enter => *sel,
                KeyCode::Char('1') => 0,
                KeyCode::Char('2') => 1,
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.prompt = None;
                    return;
                }
                _ => return,
            };
            if chosen == 0 {
                // Authorised search — only now does anything scan the machine.
                let found = tl2_mikuro_mod_packer_rs::packer::discover_tl2_installs();
                app.prompt = Some(if found.is_empty() {
                    Prompt::NoneFound
                } else {
                    Prompt::Pick { found, sel: 0 }
                });
            } else {
                // Name it yourself — typed or pasted, right here.
                app.prompt = Some(Prompt::Input { buf: String::new(), err: false });
            }
        }
        Some(Prompt::Pick { found, sel }) => {
            let n = found.len();
            if up {
                *sel = (*sel + n - 1) % n;
                return;
            }
            if down {
                *sel = (*sel + 1) % n;
                return;
            }
            match k.code {
                KeyCode::Enter => {
                    let game = found[*sel].clone();
                    let media = game.join("MEDIA");
                    app.base = if media.is_dir() {
                        BaseSource::LooseMedia(media)
                    } else {
                        BaseSource::DataPak(game.join("PAKS").join("DATA.PAK"))
                    };
                    // Remembered, so the question is asked once rather than every
                    // launch. Re-validated next time, and `b` changes it.
                    save_base_pref(&app.root, &app.base);
                    app.prompt = None;
                    app.note = Some((app.base.label(), Instant::now()));
                }
                KeyCode::Esc | KeyCode::Char('q') => app.prompt = None,
                _ => {}
            }
        }
        Some(Prompt::Input { buf, err }) => match k.code {
            // Paste arrives as ordinary characters in every terminal this runs
            // in, so typing and pasting are the same path.
            KeyCode::Char(c) => {
                buf.push(c);
                *err = false;
            }
            KeyCode::Backspace => {
                buf.pop();
                *err = false;
            }
            KeyCode::Enter => match base_from_typed(buf) {
                Some(base) => {
                    app.base = base;
                    save_base_pref(&app.root, &app.base);
                    app.prompt = None;
                    app.note = Some((app.base.label(), Instant::now()));
                }
                // Stay open on a bad path: retyping a long path because the box
                // closed is worse than an inline complaint.
                None => *err = true,
            },
            KeyCode::Esc => app.prompt = Some(Prompt::Choose { sel: 1 }),
            _ => {}
        },
        Some(Prompt::NoneFound) => {
            if matches!(k.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                app.prompt = None;
            }
        }
        None => {}
    }
}

/// Centred popup rect, `w`×`h` at most, clamped to the frame.
fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_prompt(f: &mut Frame, app: &App) {
    let Some(prompt) = app.prompt.as_ref() else { return };
    let (title, mut lines, sel, n) = match prompt {
        Prompt::Choose { sel } => (
            app.tr("base_prompt_title").to_string(),
            vec![
                app.tr("base_prompt_why").to_string(),
                String::new(),
                app.tr("base_opt_search").to_string(),
                app.tr("base_opt_manual").to_string(),
            ],
            *sel,
            CHOOSE_N,
        ),
        Prompt::Pick { found, sel } => {
            let mut v = vec![app.tr("base_pick_title").to_string(), String::new()];
            v.extend(found.iter().map(|p| p.display().to_string()));
            (app.tr("base_prompt_title").to_string(), v, *sel, found.len())
        }
        Prompt::Input { buf, err } => {
            let mut v = vec![
                app.tr("base_input_how").to_string(),
                String::new(),
                format!("  > {buf}_"),
            ];
            if *err {
                v.push(String::new());
                v.push(app.tr("base_input_bad").to_string());
            }
            (app.tr("base_prompt_title").to_string(), v, usize::MAX, 0)
        }
        Prompt::NoneFound => (
            app.tr("base_prompt_title").to_string(),
            vec![
                app.tr("base_manual_how").to_string(),
                String::new(),
                "    set TL2_MEDIA_DIR=<...>\\Torchlight II\\MEDIA".to_string(),
            ],
            usize::MAX,
            0,
        ),
    };
    // The option rows are the tail of `lines`; highlight the selected one.
    let first_opt = lines.len().saturating_sub(n);
    let body: Vec<Line> = lines
        .drain(..)
        .enumerate()
        .map(|(i, s)| {
            let is_opt = n > 0 && i >= first_opt;
            if is_opt && i - first_opt == sel {
                Line::from(Span::styled(
                    format!(" > {s}"),
                    Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
                ))
            } else if is_opt {
                Line::from(Span::styled(format!("   {s}"), Style::default().fg(theme::TEXT)))
            } else {
                Line::from(Span::styled(s, Style::default().fg(theme::MUTED)))
            }
        })
        .collect();

    let h = (body.len() as u16) + 4;
    let area = centred(f.area(), 78, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::EMBER))
                    .title(format!(" {title} "))
                    .style(Style::default().bg(theme::PANEL)),
            ),
        area,
    );
}

fn draw(f: &mut Frame, app: &App) {
    // Furnace-black base under everything; panels paint their own darker fill.
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG)),
        f.area(),
    );
    // 1-cell furnace-black inset so the panels sit inside a thin brand frame
    // (editorial whitespace) instead of bleeding to the terminal edge, plus a
    // blank row between each panel — the three blocks used to touch, which read
    // as one boxed-in slab rather than three surfaces.
    let areas = Layout::vertical([
        Constraint::Length(4), // header: border, gap, tab row, border
        Constraint::Length(1), // ── breathing room
        Constraint::Min(4),    // body (picker or output)
        Constraint::Length(1), // ── breathing room
        Constraint::Length(3), // footer
    ])
    .margin(1)
    .split(f.area());

    draw_header(f, areas[0], app);
    match app.tab {
        TAB_OUTPUT => draw_output(f, areas[2], app),
        TAB_HELP => draw_help(f, areas[2], app),
        _ => draw_list(f, areas[2], app),
    }
    draw_footer(f, areas[4], app);
    // Last, so it sits over whatever tab is underneath.
    draw_prompt(f, app);
}

/// Display width in terminal columns. CJK and full-width punctuation take two
/// cells; everything else this UI draws (Latin, Cyrillic, box-drawing, arrows)
/// takes one. Used to decide what still fits on the single-row header.
fn disp_width(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) >= 0x2E80 { 2 } else { 1 })
        .sum()
}

/// Panel inner padding: one column each side so text never kisses the border.
fn panel(title: Span<'_>, border: Color) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(theme::PANEL))
        .padding(Padding::horizontal(1))
        .title(title)
}

/// `panel` plus a blank row under the title. A panel title sits ON the top
/// border, so without it the first line of content is glued to the heading and
/// the two read as one block of text. Costs one row, which only the 3-row
/// footer cannot spare — it keeps `panel`.
fn panel_body(title: Span<'_>, border: Color) -> Block<'_> {
    panel(title, border).padding(Padding::new(1, 1, 1, 0))
}

/// Rows `panel_body` spends on borders and the title gap — subtract from a
/// panel's height to get how many lines of content actually fit.
const PANEL_CHROME: u16 = 3;

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    // Tabs carry their own row count, so the label alone tells you whether a
    // picker has anything in it before you switch to it.
    let tab = |on: bool, label: &str, count: Option<usize>| {
        let text = match count {
            Some(n) => format!("  {label} {n}  "),
            None => format!("  {label}  "),
        };
        let style = if on {
            Style::default().fg(theme::BG).bg(theme::EMBER).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::MUTED).bg(theme::LINE)
        };
        Span::styled(text, style)
    };
    let (status, status_style) = if app.is_packing() {
        (
            app.tr("running").to_string(),
            Style::default().fg(theme::GOLD).add_modifier(Modifier::BOLD),
        )
    } else if let Some(note) = app.note() {
        // Ahead of "done" on purpose: once a run has finished that status stays
        // up for good, and after a run is exactly when you reach for `r` — so
        // losing to it would leave the most common rescan with no feedback.
        (note.to_string(), Style::default().fg(theme::MAGIC))
    } else if app.finished.load(Ordering::Relaxed) {
        (
            app.tr("done").to_string(),
            Style::default().fg(theme::SET).add_modifier(Modifier::BOLD),
        )
    } else if let Some(p) = app.pane() {
        (
            format!(
                "{} {}/{}",
                app.tr("selected"),
                p.picked_count(),
                p.items.len()
            ),
            Style::default().fg(theme::EMBER),
        )
    } else {
        (String::new(), Style::default())
    };
    let gap = || Span::raw("  ");
    let mut spans = vec![
        tab(app.tab == TAB_PACK, app.tr("pack"), Some(app.pack.items.len())),
        gap(),
        tab(
            app.tab == TAB_UNPACK,
            app.tr("unpack"),
            Some(app.unpack.items.len()),
        ),
        gap(),
        tab(app.tab == TAB_OUTPUT, app.tr("output"), None),
        gap(),
        tab(app.tab == TAB_HELP, app.tr("help"), None),
    ];
    // The header is one row and the labels are longest in Russian, so on a
    // narrow terminal something has to go. Drop in order of what the user can
    // recover elsewhere: the base tier is spelled out in full on the Help tab,
    // and the language switch is advertised in the footer of every tab — the
    // RUN STATUS has no other home, so it is the last thing to be given up.
    let inner = area.width.saturating_sub(4) as usize; // borders + padding
    let used: usize = spans.iter().map(|s| disp_width(&s.content)).sum();
    let status_w = disp_width(&status) + 3;
    let lang_w = disp_width(app.lang.tag()) + 3;
    let base_w = disp_width(app.base.short()) + 3 + 5; // + "base "
    if used + status_w + lang_w + base_w <= inner {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(app.lang.tag(), Style::default().fg(theme::MAGIC)));
        spans.push(Span::styled("   base ", Style::default().fg(theme::MUTED)));
        spans.push(Span::styled(app.base.short(), Style::default().fg(theme::GOLD)));
    } else if used + status_w + lang_w <= inner {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(app.lang.tag(), Style::default().fg(theme::MAGIC)));
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(status, status_style));
    let line = Line::from(spans);
    // NO `Wrap { trim: true }` here. Trimming strips a line's leading spaces —
    // which are INSIDE the first tab's coloured span, so the leftmost tab lost
    // half its padding and sat visibly tighter than every other tab. Wrapping
    // bought nothing anyway: the block is one row tall, so a wrapped second
    // line would never be drawn, and over-long headers are already handled by
    // dropping segments above.
    let p = Paragraph::new(line)
        .block(panel_body(
            Span::styled(
                " Mikuro Torchlight II Mod Un/Packer ",
                Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
            ),
            theme::LINE_HI,
        ))
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL));
    f.render_widget(p, area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let unpacking = app.tab == TAB_UNPACK;
    let Some(pane) = app.pane() else { return };
    let title = Span::styled(
        if unpacking {
            app.tr("unpack_space_select_enter_run")
        } else {
            app.tr("pack_space_select_enter_run")
        },
        Style::default().fg(theme::MUTED),
    );

    // Empty picker: say what was looked for and where, rather than an empty box.
    if pane.items.is_empty() {
        let msg = if unpacking {
            vec![
                Line::from(Span::styled(
                    app.tr("no_mod_files_found"),
                    Style::default().fg(theme::MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    app.tr("looked_in"),
                    Style::default().fg(theme::FAINT),
                )),
                Line::from(Span::styled(
                    format!("  {}", app.root.display()),
                    Style::default().fg(theme::FAINT),
                )),
                Line::from(Span::styled(
                    format!("  {}", app.root.join("mods").display()),
                    Style::default().fg(theme::FAINT),
                )),
                Line::from(Span::styled(
                    format!("  {}", tl2_mods_dir().display()),
                    Style::default().fg(theme::FAINT),
                )),
            ]
        } else {
            vec![
                Line::from(Span::styled(
                    app.tr("no_mod_sources_found"),
                    Style::default().fg(theme::MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    app.tr("a_source_is_a_folder_holding_mod_dat_and_a_m"),
                    Style::default().fg(theme::FAINT),
                )),
            ]
        };
        let p = Paragraph::new(msg)
            .block(panel_body(title, theme::LINE))
            .style(Style::default().bg(theme::PANEL));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = pane
        .items
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let on = pane.picked[i];
            let mark = if on { "[x] " } else { "[ ] " };
            let tag = match (m.action, m.has_layouts) {
                // On the unpack tab every row is a .MOD, so the useful tag is
                // WHERE it came from, not what it is.
                (Action::Unpack, _) => Span::styled(
                    format!("  ({})", m.origin.label(&app.strings)),
                    Style::default().fg(if m.origin == Origin::Deployed {
                        theme::GOLD
                    } else {
                        theme::FAINT
                    }),
                ),
                (Action::Pack, true) => Span::styled(
                    app.tr("level_mod"),
                    Style::default().fg(theme::MAGIC),
                ),
                (Action::Pack, false) => Span::styled(
                    app.tr("content_mod"),
                    Style::default().fg(theme::FAINT),
                ),
            };
            let mark_style = if on {
                Style::default().fg(theme::SET).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FAINT)
            };
            let title_style = if on {
                Style::default().fg(theme::TEXT)
            } else {
                Style::default().fg(theme::MUTED)
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, mark_style),
                Span::styled(m.title.clone(), title_style),
                tag,
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(panel_body(title, theme::LINE))
        .highlight_style(
            Style::default()
                .bg(theme::LINE_HI)
                .fg(theme::EMBER_HI)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    // The scroll offset lives in the state ratatui is handed, and the widget
    // WRITES it there. Handing it a fresh clone every frame threw that away:
    // the offset restarted at 0 and the widget re-derived "just enough to show
    // the selection", which pins the cursor to the last visible row forever —
    // you can never see a row past it, in either direction. Carry it across
    // frames instead, so the view only moves when the cursor leaves it.
    let mut state = pane.state.clone().with_offset(pane.offset.get());
    f.render_stateful_widget(list, area, &mut state);
    pane.offset.set(state.offset());
}

fn draw_output(f: &mut Frame, area: Rect, app: &App) {
    let log = app.log.lock().map(|v| v.clone()).unwrap_or_default();
    let total = log.len() as u16;
    let inner_h = area.height.saturating_sub(PANEL_CHROME);
    let max_scroll = total.saturating_sub(inner_h);
    app.out_max.set(max_scroll); // the key handler clamps against this
    let scroll = if app.follow {
        max_scroll
    } else {
        app.out_scroll.min(max_scroll)
    };
    let (title, tstyle) = if total == 0 {
        (
            app.tr("output_empty").to_string(),
            Style::default().fg(theme::FAINT),
        )
    } else if app.follow {
        (
            format!(
                " {} · {} · {total} {} ",
                app.tr("output_2"),
                app.tr("following"),
                app.tr("lines")
            ),
            Style::default().fg(theme::SET),
        )
    } else {
        (
            format!(
                " {} · {}/{} ",
                app.tr("output_3"),
                scroll.min(total),
                total
            ),
            Style::default().fg(theme::MUTED),
        )
    };
    // Per-line, not one blob: a line carrying a `UNK_` placeholder is a name the
    // dictionary could not recover, and it must stand out — the digits are load-bearing
    // (they recompile to the original hash) and must never be hand-edited.
    let body: Vec<Line> = log
        .iter()
        .map(|l| {
            // RECOVERED is tested FIRST: a recovered line now spells out the placeholder
            // it would have been (`recovered: UNK_091C391F -> SECOND_STAT`), so it does
            // contain `UNK_`. Checking the failure rule first would paint every rescue red.
            if l.contains(tl2_mikuro_mod_packer_rs::dat_keys::RECOVERED_MARKER) {
                Line::styled(l.clone(), Style::default().fg(theme::SET))
            } else if l.contains(tl2_mikuro_mod_packer_rs::dat_keys::UNK_PREFIX) {
                Line::styled(
                    l.clone(),
                    Style::default().fg(theme::WARN).add_modifier(Modifier::BOLD),
                )
            } else {
                Line::raw(l.clone())
            }
        })
        .collect();
    let p = Paragraph::new(body)
        .block(panel_body(Span::styled(title, tstyle), theme::LINE))
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

/// The HELP tab. Everything here is stated from the code paths it describes —
/// `detect_base` / `resolve_base_media` for the base tiers, `discover_*` for the
/// scanned locations, `pack_worker` for where output lands, `main::usage` for
/// the CLI — so it cannot drift into describing a tool we do not ship.
///
/// Translations keep ONE line per line: `chinese_ui_renders_and_keeps_commands_
/// verbatim` asserts the three languages have equal line counts and that no
/// line is blank in one language and not another, so a skipped translation
/// cannot slip through as a stray empty row.
fn help_lines(app: &App) -> Vec<Line<'static>> {
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
        ))
    };
    let body = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(theme::TEXT)));
    let dim = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(theme::MUTED)));
    let path = |s: String| Line::from(Span::styled(s, Style::default().fg(theme::GOLD)));
    let cmd = |c: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {c}"), Style::default().fg(theme::EMBER_HI)),
            Span::styled(format!("  {what}"), Style::default().fg(theme::MUTED)),
        ])
    };
    let blank = || Line::from("");
    let root = app.root.display().to_string();
    let tr = |key: &str| app.tr(key);
    // Command names and flags are never translated — they are what you type.
    let mut v = vec![
        head(tr("base_bundle_what_it_is_and_why_packing_needs")),
        body(tr("two_things_a_mod_build_cannot_compute_from_t")),
        dim(tr("unitdata_raw_follows_each_unit_s_basefile_ch")),
        dim(tr("units_templates_to_read_unittype_level_rarit")),
        dim(tr("mpp_walk_grids_are_baked_against_the_base_ga")),
        body(tr("so_the_packer_needs_base_data_it_takes_the_f")),
        dim(tr("1_loose_media_next_to_this_exe_a_real_extrac")),
        dim(tr("2_paks_data_pak_or_data_pak_beside_it_extrac")),
        dim(tr("3_the_bundle_baked_into_this_exe_always_ther")),
        body(&format!(
            "{} {}",
            tr("right_now"),
            app.base.label()
        )),
        dim(tr("tiers_2_and_3_unpack_once_into_a_cache_next")),
        path(format!("  {}", app.root.join(".tl2packer_base").display())),
        dim(tr("base_choice_remembered")),
        path(format!("  {}", base_pref_file(&app.root).display())),
        dim(tr("all_three_produce_byte_identical_mod_output")),
        blank(),
        head(tr("content_mod_vs_level_mod")),
        body(tr("the_picker_tags_each_source_by_one_test_does")),
        body(tr("layout_file")),
        dim(tr("content_mod_no_layouts_nothing_to_bake_packs")),
        dim(tr("level_mod_has_layouts_its_pathing_walk_grids")),
        dim(tr("regenerated_with_mpp_re_which_is_why_it_need")),
        dim(tr("base_collision_geometry_above_tier_3_is_embe")),
        dim(tr("base_is_always_available_if_resolving_it_fai")),
        dim(tr("the_pack_still_runs_and_keeps_the_existing_m")),
        blank(),
        head(tr("where_files_are_read_from")),
        body(tr("pack_scans_for_folders_holding_mod_dat_plus")),
        path(format!("  {root}")),
        path(format!("  {}", app.root.join("mods").display())),
        dim(tr("that_first_folder_counts_itself_if_it_holds_mod_dat")),
        body(tr("unpack_scans_for_mod_files_in_those_two_plus")),
        body(tr("the_one_deploy_writes_to_so_anything_you_jus")),
        path(format!("  {}", tl2_mods_dir().display())),
        dim(tr("rows_are_deduped_by_real_path_and_tagged_loc")),
        body(tr("both_lists_are_scanned_at_startup_press_r_to")),
        dim(tr("build_a_mod_add_a_mod_folder_or_delete_one_t")),
        blank(),
        head(tr("where_output_goes")),
        body(tr("pack_from_this_tui_always_deploys_i_e_writes")),
        path(format!("  {}", tl2_mods_dir().display())),
        dim(tr("mod_file_name_comes_from_mod_dat_the_folder")),
        body(tr("unpack_writes_a_source_tree_mod_dat_media_un")),
        path(format!("  {}", app.root.join("unpacked").display())),
        dim(tr("one_subfolder_per_mod_the_game_s_mods_folder")),
        dim(tr("an_unpack_so_unpacking_something_you_have_in")),
        blank(),
        head(tr("cli_mode_run_it_with_arguments_instead")),
        dim(tr("launching_with_no_arguments_is_what_opens_th")),
        dim(tr("is_the_ordinary_command_line_packer")),
        blank(),
        cmd(
            "<mod_dir> [--deploy]",
            tr("pack_a_mod_folder"),
        ),
        // These three are one decision — "where do the compiled .BINDAT /
        // .BINLAYOUT files go?" — so spell out all three answers, including
        // the default you get by passing nothing.
        cmd("   (no flag)", tr("mode_default")),
        cmd("   --in-place", tr("mode_in_place")),
        cmd("   --temp-copy", tr("mode_temp_copy")),
        cmd(
            "   --mpp re|none",
            tr("walk_grid_backend_default_re"),
        ),
        cmd(
            "   --raw auto|none",
            tr("raw_index_generation_default_auto"),
        ),
        blank(),
        cmd(
            "unpack <in.MOD|DATA.PAK> <out_dir>",
            tr("container_editable_sources"),
        ),
        cmd(
            "   --raw",
            tr("keep_payloads_compiled_no_decompile"),
        ),
        cmd(
            "   --no-times",
            tr("do_not_restore_file_timestamps"),
        ),
        cmd(
            "   --filter <text>",
            tr("only_entries_whose_path_contains_text"),
        ),
        blank(),
        cmd(
            "compile-dat <in.DAT> <out.BINDAT>",
            tr("single_file"),
        ),
        cmd(
            "compile-layout <in.LAYOUT> <out.BINLAYOUT>",
            tr("single_file_2"),
        ),
        cmd(
            "compile-mpp <in.LAYOUT> <out.mpp>",
            tr("single_tile_walk_grid"),
        ),
        cmd(
            "decompile-dat <in.BINDAT> <out.DAT>",
            tr("inverse_utf8_for_a_plain_dump"),
        ),
        cmd(
            "decompile-layout <in.BINLAYOUT> <out.LAYOUT>",
            tr("inverse"),
        ),
        cmd(
            "extract-base <DATA.PAK> <out_dir>",
            tr("base_dats_collision_meshes"),
        ),
        cmd(
            "unpack-base <out_dir>",
            tr("the_embedded_bundle_no_install_needed"),
        ),
        blank(),
        dim(tr("env_tl2_media_dir_base_media_tl2_mod_gamever")),
        dim(tr("mikuro_timing_1_phase_timings_tl2_modpack_li")),
        dim(tr("tl2_lang_en_zh_ru_force_this_ui_s_language_l")),
    ];
    v.push(blank());
    v.push(dim(tr("pageup_pagedown_to_scroll_this_page")));
    v
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let lines = help_lines(app);
    let total = lines.len() as u16;
    let inner_h = area.height.saturating_sub(PANEL_CHROME);
    let max_scroll = total.saturating_sub(inner_h);
    app.help_max.set(max_scroll); // the key handler clamps against this
    let scroll = app.help_scroll.min(max_scroll);
    let title = if max_scroll > 0 {
        format!(" Help · {}/{} ", scroll, max_scroll)
    } else {
        " Help ".to_string()
    };
    let p = Paragraph::new(lines)
        .block(panel_body(
            Span::styled(title, Style::default().fg(theme::MUTED)),
            theme::LINE,
        ))
        .style(Style::default().bg(theme::PANEL))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

/// One footer entry: the key you press, what it does, and whether the line can
/// be shortened by leaving it out.
struct Hint {
    key: String,
    label: String,
    color: Color,
    /// True when the entry can be dropped on a narrow terminal. The keys that
    /// cannot: moving, running, changing tab, changing language, quitting.
    optional: bool,
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let tr = |key: &str| app.tr(key);
    let quit = if app.is_packing() {
        tr("running_2")
    } else {
        tr("quit")
    };
    // The catalogue pads its footer labels on BOTH sides — a leading space held
    // the key apart from its own label, trailing ones held the entries apart.
    // Spacing is decided here now (it has to shrink on a narrow terminal), so
    // the padding is trimmed back off.
    let hint = |k: &str, label: &str, optional: bool| Hint {
        key: k.to_string(),
        label: label.trim().to_string(),
        color: theme::MUTED,
        optional,
    };
    // Changing tab is the one action available on EVERY tab, and the pickers
    // advertised no way to do it at all — the number keys work but are invisible,
    // and `Tab` reads as a character rather than a key. `←→` explains itself.
    let tabs = || hint("←→", tr("switch"), false);
    // The language switch is on every tab: someone can land in a script they
    // cannot read, and the key must be reachable without understanding the UI.
    // The label is the NEXT language's own tag, so it reads the same to anyone.
    let lang = || Hint {
        key: "l".to_string(),
        label: app.lang.next().tag().to_string(),
        color: theme::MAGIC,
        optional: false,
    };
    // Rescan is on every tab, like the language switch: what it changes — the
    // row counts beside the tab labels — is in the header wherever you stand.
    // Optional, because at 80 columns something has to go and this key is also
    // on the Help page and in the README, which is what `optional` means here.
    let rescan = || hint("r", tr("rescan"), true);

    let mut items: Vec<Hint> = if app.tab == TAB_HELP {
        vec![
            hint("↑↓", tr("scroll"), false),
            hint("Home", tr("top"), true),
            rescan(),
            tabs(),
            lang(),
        ]
    } else if app.tab == TAB_OUTPUT {
        vec![
            hint("↑↓", tr("scroll_2"), false),
            hint("End", tr("follow"), true),
            hint("c", tr("clear"), true),
            rescan(),
            tabs(),
            lang(),
        ]
    } else {
        let run = if app.tab == TAB_UNPACK { tr("unpack_2") } else { tr("pack_2") };
        vec![
            hint("↑↓", tr("move"), false),
            hint("Space", tr("select"), false),
            hint("a", tr("all"), true),
            hint("Enter", run, false),
            rescan(),
            tabs(),
            hint("?", tr("help_2"), true),
            lang(),
        ]
    };
    items.push(hint("q", quit, false));

    // Fit the line: first close the gaps from 4 columns down to 2, then start
    // leaving out the entries marked optional, last one first. A footer that
    // silently clips is worse than a shorter one — the keys at the END are the
    // ones you cannot discover any other way (language, quit).
    let inner = area.width.saturating_sub(4) as usize;
    let width = |items: &[Hint], gap: usize| -> usize {
        items
            .iter()
            .map(|h| disp_width(&h.key) + 1 + disp_width(&h.label))
            .sum::<usize>()
            + gap * items.len().saturating_sub(1)
    };
    let mut gap = 4;
    while gap > 2 && width(&items, gap) > inner {
        gap -= 1;
    }
    while width(&items, gap) > inner {
        match items.iter().rposition(|h| h.optional) {
            Some(i) => {
                items.remove(i);
            }
            None => break, // nothing left to give; the terminal is simply tiny
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, h) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        spans.push(Span::styled(
            h.key.clone(),
            Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {}", h.label),
            Style::default().fg(h.color),
        ));
    }
    let p = Paragraph::new(Line::from(spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::LINE))
                .style(Style::default().bg(theme::PANEL))
                .padding(Padding::horizontal(1)),
        )
        .style(Style::default().bg(theme::PANEL));
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_mod(root: &Path, name: &str, with_layout: bool) {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("MEDIA").join("UNITS")).unwrap();
        // An empty MOD.DAT is enough to be recognized (metadata falls back to
        // the folder name); presence is what marks a mod source.
        std::fs::write(d.join("MOD.DAT"), b"").unwrap();
        std::fs::write(d.join("MEDIA").join("UNITS").join("X.DAT"), b"x").unwrap();
        if with_layout {
            let l = d.join("MEDIA").join("LAYOUTS").join("SUB");
            std::fs::create_dir_all(&l).unwrap();
            std::fs::write(l.join("TOWN.LAYOUT"), b"x").unwrap();
        }
    }

    /// A typed path is accepted in the three shapes people actually paste, and
    /// rejected when it holds no game data.
    ///
    /// Rejection matters more than acceptance here: a path that resolves but is
    /// not a base tree bakes every tile with its linked geometry missing and
    /// reports nothing, so "close enough" has to fail at the input box.
    #[test]
    fn typed_base_paths_are_validated_not_trusted() {
        let root = std::env::temp_dir().join(format!("tl2typed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let game = root.join("Torchlight II");
        let media = game.join("MEDIA");
        std::fs::create_dir_all(media.join("LEVELSETS")).unwrap();
        std::fs::create_dir_all(game.join("PAKS")).unwrap();
        let pak = game.join("PAKS").join("DATA.PAK");
        std::fs::write(&pak, b"x").unwrap();

        // The MEDIA folder itself.
        match base_from_typed(&media.display().to_string()) {
            Some(BaseSource::LooseMedia(p)) => assert_eq!(p, media),
            other => panic!("MEDIA folder should be accepted, got {other:?}"),
        }
        // The game folder above it — resolved down to MEDIA.
        match base_from_typed(&game.display().to_string()) {
            Some(BaseSource::LooseMedia(p)) => assert_eq!(p, media),
            other => panic!("game folder should resolve to MEDIA, got {other:?}"),
        }
        // Explorer's "copy as path" wraps the whole thing in quotes.
        match base_from_typed(&format!("  \"{}\"  ", media.display())) {
            Some(BaseSource::LooseMedia(p)) => assert_eq!(p, media),
            other => panic!("quoted path should be accepted, got {other:?}"),
        }
        // A DATA.PAK.
        match base_from_typed(&pak.display().to_string()) {
            Some(BaseSource::DataPak(p)) => assert_eq!(p, pak),
            other => panic!("DATA.PAK should be accepted, got {other:?}"),
        }
        // A real folder holding nothing, a path that does not exist, and blank.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(base_from_typed(&empty.display().to_string()).is_none());
        assert!(base_from_typed(&root.join("nope").display().to_string()).is_none());
        assert!(base_from_typed("   ").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_finds_direct_and_nested_mods_and_flags_levels() {
        let root = std::env::temp_dir().join(format!("tl2tui_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        mk_mod(&root, "AlphaContent", false);
        mk_mod(&root, "BetaLevel", true);
        mk_mod(&root.join("mods"), "GammaNested", false);
        // A plain folder with no MOD.DAT must be ignored.
        std::fs::create_dir_all(root.join("NotAMod").join("stuff")).unwrap();

        // Built .MOD files are a SEPARATE list now (the UNPACK tab), not rows
        // mixed into the pack picker. (Neither is a readable container, so the
        // title falls back to the file stem; discovery must still offer them.)
        std::fs::write(root.join("DeltaBuilt.MOD"), b"not really a container").unwrap();
        std::fs::write(root.join("mods").join("EpsilonBuilt.MOD"), b"nor is this").unwrap();

        let packs = discover_pack_sources(&root);
        let titles: Vec<&str> = packs.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["AlphaContent", "BetaLevel", "GammaNested"]);
        assert!(packs.iter().all(|m| m.action == Action::Pack), "pack list holds only sources");
        let beta = packs.iter().find(|m| m.title == "BetaLevel").unwrap();
        assert!(beta.has_layouts, "BetaLevel has a *.LAYOUT");
        let alpha = packs.iter().find(|m| m.title == "AlphaContent").unwrap();
        assert!(!alpha.has_layouts, "AlphaContent has no layouts");

        // The exe dropped INSIDE a mod: that folder is itself the source, and
        // is offered alongside anything nested under it.
        let inner = root.join("AlphaContent");
        let self_packs = discover_pack_sources(&inner);
        let self_titles: Vec<&str> = self_packs.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(self_titles, vec!["AlphaContent"], "root itself is a packable source");
        // …and it is listed once, not twice, when it is also reachable as a child.
        assert_eq!(
            packs.iter().filter(|m| m.title == "AlphaContent").count(),
            1,
            "no duplicate row"
        );

        let unpacks = discover_mod_files(&root);
        let names: Vec<&str> = unpacks
            .iter()
            .filter(|m| m.origin == Origin::Local)
            .map(|m| m.title.as_str())
            .collect();
        assert_eq!(names, vec!["DeltaBuilt", "EpsilonBuilt"], "root and ./mods both scanned");
        assert!(unpacks.iter().all(|m| m.action == Action::Unpack), "unpack list holds only .MOD");
        // The deployed game mods folder is scanned too; whatever it holds on
        // this machine must be tagged as such and must not collide with local.
        assert!(unpacks.iter().filter(|m| m.origin == Origin::Local).count() == 2);

        // Nothing beside the exe, no TL2_MEDIA_DIR, nothing remembered → ask.
        // In particular this must NOT quietly resolve to anything: a machine can
        // hold several usable trees, and a wrong one bakes tiles with their
        // linked geometry missing and reports nothing.
        assert!(matches!(detect_base_with(&root, None, None), BaseSource::Unresolved));

        // A remembered choice answers instead of asking again.
        let saved = root.join("saved").join("MEDIA");
        std::fs::create_dir_all(saved.join("LEVELSETS")).unwrap();
        save_base_pref(&root, &BaseSource::LooseMedia(saved.clone()));
        match detect_base_with(&root, None, load_base_pref(&root)) {
            BaseSource::LooseMedia(p) => assert_eq!(p, saved),
            other => panic!("remembered base should be used, got {other:?}"),
        }
        // But only while it still holds game data: an install that moved must
        // put the question back, not resolve to a path that no longer answers.
        std::fs::remove_dir_all(root.join("saved")).unwrap();
        assert!(load_base_pref(&root).is_none(), "stale pref must not be trusted");
        assert!(
            matches!(detect_base_with(&root, None, load_base_pref(&root)), BaseSource::Unresolved)
        );

        // TL2_MEDIA_DIR, when the user has set it, is taken at face value and
        // outranks the remembered answer — it is the per-shell override.
        let env_media = root.join("fakegame").join("MEDIA");
        std::fs::create_dir_all(env_media.join("LEVELSETS")).unwrap();
        save_base_pref(&root, &BaseSource::LooseMedia(saved.clone()));
        std::fs::create_dir_all(saved.join("LEVELSETS")).unwrap();
        match detect_base_with(&root, Some(&env_media), load_base_pref(&root)) {
            BaseSource::LooseMedia(p) => assert_eq!(p, env_media),
            other => panic!("TL2_MEDIA_DIR should win over the remembered one, got {other:?}"),
        }
        // A path that does not exist is not a base — ask instead of trusting it.
        let missing = root.join("gone").join("MEDIA");
        assert!(matches!(
            detect_base_with(&root, Some(&missing), None),
            BaseSource::Unresolved
        ));
        // A loose MEDIA next to the exe still wins over everything.
        std::fs::create_dir_all(root.join("MEDIA")).unwrap();
        match detect_base_with(&root, Some(&env_media), load_base_pref(&root)) {
            BaseSource::LooseMedia(p) => assert_eq!(p, root.join("MEDIA")),
            other => panic!("local MEDIA should win, got {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renders_picker_frame() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let packs = vec![
            ModEntry {
                title: "Alpha Content".into(),
                file_name: "ALPHA".into(),
                dir: PathBuf::from("/x/Alpha"),
                has_layouts: false,
                action: Action::Pack,
                origin: Origin::Local,
            },
            ModEntry {
                title: "Beta Level".into(),
                file_name: "BETA".into(),
                dir: PathBuf::from("/x/Beta"),
                has_layouts: true,
                action: Action::Pack,
                origin: Origin::Local,
            },
        ];
        let unpacks = vec![ModEntry {
            title: "Gamma Built".into(),
            file_name: "GAMMA".into(),
            dir: PathBuf::from("/x/GAMMA.MOD"),
            has_layouts: false,
            action: Action::Unpack,
            origin: Origin::Deployed,
        }];
        let mut app = App::new(packs, unpacks, BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        // Pin the language: `App::new` detects it from the OS, so without this
        // the assertions below would pass or fail depending on whose Windows
        // ran them. The Chinese frames get their own test.
        app.set_lang(Lang::En);
        app.pack.toggle_current(); // pick Alpha (row under cursor)

        // Layout work is visual: `TL2_TUI_DUMP=1 cargo test --bin
        // tl2-mikuro-mod-packer renders_picker_frame -- --nocapture` prints each
        // frame so spacing can be eyeballed without launching a terminal.
        let dump = |label: &str, text: &str| {
            if std::env::var_os("TL2_TUI_DUMP").is_some() {
                eprintln!("\n──────── {label} ────────\n{text}");
            }
        };

        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        // PACK tab.
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        dump("PACK", &text);
        assert!(text.contains("Mikuro Torchlight II Mod Un/Packer"), "header brand");
        for t in ["PACK", "UNPACK", "OUTPUT"] {
            assert!(text.contains(t), "tab bar shows {t}");
        }
        assert!(
            text.find("PACK").unwrap() < text.find("UNPACK").unwrap()
                && text.find("UNPACK").unwrap() < text.find("OUTPUT").unwrap(),
            "tab order is PACK / UNPACK / OUTPUT"
        );
        assert!(text.contains("Alpha Content"), "mod row");
        assert!(text.contains("Beta Level"), "mod row");
        assert!(text.contains("(level mod)"), "level tag");
        assert!(!text.contains("Gamma Built"), "unpack rows are NOT in the pack picker");
        assert!(text.contains("[x]"), "selected mark");
        assert!(text.contains("selected 1/2"), "count is per-pane");
        assert!(text.contains("Enter"), "footer hint");

        // UNPACK tab: its own rows, its own untouched selection.
        app.tab = TAB_UNPACK;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let un = buffer_text(terminal.backend().buffer());
        dump("UNPACK", &un);
        assert!(un.contains("Gamma Built"), "unpack row");
        assert!(un.contains("game mods"), "origin tag for a deployed .MOD");
        assert!(!un.contains("Alpha Content"), "pack rows are NOT in the unpack picker");
        assert!(un.contains("selected 0/1"), "switching tabs kept PACK's tick to itself");

        // OUTPUT tab renders the log pane.
        app.tab = TAB_OUTPUT;
        logln(&app.log, "hello from worker");
        terminal.draw(|f| draw(f, &app)).unwrap();
        let out = buffer_text(terminal.backend().buffer());
        dump("OUTPUT", &out);
        assert!(out.contains("Output"), "output tab title");
        assert!(out.contains("hello from worker"), "log line rendered");
        assert!(out.contains("clear"), "footer offers the clear key");

        // `c` empties the transcript and returns to following the tail.
        app.follow = false;
        app.out_scroll = 7;
        app.clear_log();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let cleared = buffer_text(terminal.backend().buffer());
        dump("OUTPUT (cleared)", &cleared);
        assert!(!cleared.contains("hello from worker"), "log emptied");
        assert!(cleared.contains("Output · empty"), "title says so");
        assert!(app.follow && app.out_scroll == 0, "clear re-arms tail-following");
        // Clearing the transcript must not rewrite what happened: a finished run
        // still reads as finished in the header.
        app.finished.store(true, Ordering::Relaxed);
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            buffer_text(terminal.backend().buffer()).contains("done"),
            "clear does not reset the run status"
        );

        // HELP tab.
        app.tab = TAB_HELP;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let help = buffer_text(terminal.backend().buffer());
        dump("HELP", &help);
        assert!(help.contains("BASE BUNDLE"), "help covers the base bundle");
    }

    /// The Chinese UI has to be rendered by a test or it rots: a translation
    /// that never runs is a translation nobody notices is broken. This also
    /// pins the two rules the localisation follows — command names and flags
    /// stay verbatim, and every English help line has a Chinese counterpart
    /// (no silently blank rows where a translation was skipped).
    #[test]
    fn chinese_ui_renders_and_keeps_commands_verbatim() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let packs = vec![ModEntry {
            title: "组件10".into(),
            file_name: "C10".into(),
            dir: PathBuf::from("/x/C10"),
            has_layouts: true,
            action: Action::Pack,
            origin: Origin::Local,
        }];
        let unpacks = vec![ModEntry {
            title: "已部署包".into(),
            file_name: "DEP".into(),
            dir: PathBuf::from("/x/DEP.MOD"),
            has_layouts: false,
            action: Action::Unpack,
            origin: Origin::Deployed,
        }];
        let mut app = App::new(packs, unpacks, BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::Zh);
        let dump = |label: &str, text: &str| {
            if std::env::var_os("TL2_TUI_DUMP").is_some() {
                eprintln!("\n──────── {label} ────────\n{text}");
            }
        };

        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let zh = buffer_text(terminal.backend().buffer());
        dump("ZH", &zh);
        for want in ["打包", "解包", "输出", "帮助", "已选", "(关卡 mod)", "移动", "全选"] {
            assert!(zh.contains(want), "Chinese UI missing {want:?}");
        }
        assert!(!zh.contains("PACK") && !zh.contains("selected"), "no English left over");

        app.tab = TAB_UNPACK;
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            buffer_text(terminal.backend().buffer()).contains("游戏 mods"),
            "origin tag translated"
        );

        // Russian too — and every tab in every language must render, with the
        // header naming the live language and the footer advertising the NEXT
        // one (so the switch is readable whatever script you are stuck in).
        for lang in [Lang::En, Lang::Zh, Lang::Ru] {
            app.set_lang(lang);
            for tab in [TAB_PACK, TAB_UNPACK, TAB_OUTPUT, TAB_HELP] {
                app.tab = tab;
                terminal.draw(|f| draw(f, &app)).unwrap();
                let text = buffer_text(terminal.backend().buffer());
                assert!(text.contains(lang.tag()), "{lang:?} tab {tab}: live language tag");
                assert!(text.contains(lang.next().tag()), "{lang:?} tab {tab}: switch hint");
            }
        }
        app.set_lang(Lang::Ru);
        app.tab = TAB_PACK;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let ru = buffer_text(terminal.backend().buffer());
        dump("RU", &ru);
        for want in ["СБОРКА", "РАСПАКОВКА", "ВЫВОД", "СПРАВКА", "выбрано", "(мод уровней)"] {
            assert!(ru.contains(want), "Russian UI missing {want:?}");
        }
        assert!(!ru.contains("PACK") && !ru.contains("selected"), "no English left over");

        // Help: every line has content in EVERY language (a skipped translation
        // would show up as a blank row), and the command column is identical
        // everywhere — you type those, they are not prose.
        let flat = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let page = |app: &mut App, lang| {
            app.set_lang(lang);
            help_lines(app).iter().map(flat).collect::<Vec<_>>()
        };
        let en_lines = page(&mut app, Lang::En);
        for lang in [Lang::Zh, Lang::Ru] {
            let other = page(&mut app, lang);
            assert_eq!(en_lines.len(), other.len(), "{lang:?} help line count");
            for (i, (e, o)) in en_lines.iter().zip(&other).enumerate() {
                assert_eq!(
                    e.trim().is_empty(),
                    o.trim().is_empty(),
                    "{lang:?} help line {i} blank in only one language: {e:?} / {o:?}"
                );
            }
            let joined = other.join("\n");
            for c in ["unpack <in.MOD|DATA.PAK> <out_dir>", "compile-dat", "decompile-layout",
                      "--mpp re|none", "--no-times", "TL2_LANG=en|zh|ru"] {
                assert!(joined.contains(c), "{lang:?}: {c:?} must stay verbatim");
            }
        }
    }

    /// The header packs tabs + language + base tier + status onto ONE row, and
    /// Russian labels are the longest. Check the worst realistic case actually
    /// fits an 80-column terminal instead of silently clipping the status.
    #[test]
    fn russian_header_fits_a_narrow_terminal() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let many = |n: usize, action: Action| {
            (0..n)
                .map(|i| ModEntry {
                    title: format!("m{i}"),
                    file_name: format!("m{i}"),
                    dir: PathBuf::from("/x"),
                    has_layouts: false,
                    action,
                    origin: Origin::Local,
                })
                .collect::<Vec<_>>()
        };
        let mut app = App::new(
            many(12, Action::Pack),
            many(33, Action::Unpack),
            BaseSource::LooseMedia(PathBuf::from("/game/MEDIA")), // longest short() tag
            PathBuf::from("/x"),
        );
        app.set_lang(Lang::Ru);
        for w in [80u16, 96, 120] {
            let mut terminal = Terminal::new(TestBackend::new(w, 18)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text = buffer_text(terminal.backend().buffer());
            // The tab row, wherever the header's internal spacing puts it.
            let header = text
                .lines()
                .find(|l| l.contains("СБОРКА"))
                .unwrap_or_default()
                .to_string();
            assert!(
                header.contains("выбрано 0/12"),
                "at {w} cols the status is clipped out of the header: {header:?}"
            );
        }
    }

    /// Moving the strings out of Rust and into `.ini` gave up the compiler's
    /// "you forgot a translation" error, so buy it back here: every key the
    /// code looks up must exist in ALL catalogues, and no catalogue may carry a
    /// key nothing uses (dead entries mislead whoever translates next).
    #[test]
    fn every_key_used_in_code_exists_in_the_catalogues() {
        use std::collections::{HashMap, HashSet};

        // Only the real UI code — scanning the whole file would also pick up
        // the `tr("` literal inside this very scanner.
        let full = include_str!("tui.rs");
        let src = &full[..full.find("#[cfg(test)]").unwrap_or(full.len())];
        // Both lookup spellings: `tr("key")` and, where only the catalogue is
        // in hand rather than the whole App, `s.get("key")`.
        let used: HashSet<&str> = ["tr(\"", "get(\""]
            .iter()
            .flat_map(|pat| {
                src.match_indices(pat).map(move |(i, _)| {
                    let rest = &src[i + pat.len()..];
                    &rest[..rest.find('"').unwrap_or(0)]
                })
            })
            .filter(|k| !k.is_empty())
            .collect();
        assert!(used.len() > 50, "expected the whole UI to be keyed, found {}", used.len());

        for lang in [Lang::En, Lang::Zh, Lang::Ru] {
            let mut map = HashMap::new();
            parse_ini(lang.builtin(), &mut map);
            let keys: HashSet<&str> = map.keys().map(String::as_str).collect();
            let missing: Vec<_> = used.difference(&keys).copied().collect();
            assert!(missing.is_empty(), "{lang:?}.ini is missing keys: {missing:?}");
            let unused: Vec<_> = keys.difference(&used).copied().collect();
            assert!(unused.is_empty(), "{lang:?}.ini has keys nothing uses: {unused:?}");
            // A blank value renders as an empty line — always a mistake here.
            for (k, v) in &map {
                assert!(!v.trim().is_empty(), "{lang:?}.ini: {k} is empty");
            }
        }
    }

    /// An override file next to the exe wins over the built-in catalogue, and
    /// anything it leaves out still falls back — that is what makes it safe to
    /// ship a two-line correction rather than a whole translated file.
    #[test]
    fn override_file_wins_and_partial_overrides_fall_back() {
        let root = std::env::temp_dir().join(format!("tl2lang_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("lang")).unwrap();
        std::fs::write(root.join("lang").join("en.ini"), "quit=LEAVE\n# a comment\n").unwrap();

        let s = Strings::load(Lang::En, &root);
        assert_eq!(s.get("quit"), "LEAVE", "override applied");
        assert_eq!(
            s.get("pack"),
            Strings::load(Lang::En, Path::new("")).get("pack"),
            "keys the override omits keep the built-in text"
        );
        // Leading spaces are significant (they indent the Help page).
        std::fs::write(root.join("lang").join("en.ini"), "quit=  spaced  \n").unwrap();
        assert_eq!(Strings::load(Lang::En, &root).get("quit"), "  spaced  ");

        // An English override must NOT leak into a translated UI: English is
        // only the substrate the chosen language is laid over. (It used to,
        // because the layers were ordered builtin-en, builtin-zh, disk-en,
        // disk-zh — so fixing an English line un-translated it everywhere.)
        std::fs::write(root.join("lang").join("en.ini"), "quit=LEAVE\n").unwrap();
        let zh = Strings::load(Lang::Zh, &root);
        assert_eq!(
            zh.get("quit"),
            Strings::load(Lang::Zh, Path::new("")).get("quit"),
            "editing en.ini left the Chinese UI alone"
        );
        assert_ne!(zh.get("quit"), "LEAVE");
        // …but a zh override still wins, and English still fills the gaps.
        std::fs::write(root.join("lang").join("zh.ini"), "quit=离开\n").unwrap();
        let zh = Strings::load(Lang::Zh, &root);
        assert_eq!(zh.get("quit"), "离开", "zh override applied");
        assert_eq!(zh.get("mode_default"), Strings::load(Lang::Zh, Path::new("")).get("mode_default"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Language detection: the explicit override wins over everything, and an
    /// unrecognised value falls through to the OS/default rather than erroring.
    #[test]
    fn lang_override_is_honoured() {
        // Serialised via a lock: env is process-global and these tests can run
        // alongside others that read it.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("TL2_LANG");
        for (v, want) in [("zh", Lang::Zh), ("zh-CN", Lang::Zh), ("cn", Lang::Zh),
                          ("ru", Lang::Ru), ("ru_RU.UTF-8", Lang::Ru),
                          ("en", Lang::En), ("en-US", Lang::En)] {
            std::env::set_var("TL2_LANG", v);
            assert_eq!(detect_lang(), want, "TL2_LANG={v}");
        }
        // Cycling visits every language and returns home.
        assert_eq!(Lang::En.next().next().next(), Lang::En, "cycle covers all three");
        // Garbage does not force a language; detection continues.
        std::env::set_var("TL2_LANG", "klingon");
        let _ = detect_lang();
        match prev {
            Some(v) => std::env::set_var("TL2_LANG", v),
            None => std::env::remove_var("TL2_LANG"),
        }
    }

    /// The help text is the tool's own documentation, so assert it actually
    /// answers each question asked of it — and that the paths it quotes are the
    /// ones the code really uses, not prose that can drift.
    #[test]
    fn help_answers_every_question_with_real_paths() {
        let root = PathBuf::from("/x");
        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), root.clone());
        app.set_lang(Lang::En);
        let text = help_lines(&app)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        // Same dump switch as the frame test — this page is prose, and prose
        // wants proofreading, not just substring assertions.
        if std::env::var_os("TL2_TUI_DUMP").is_some() {
            eprintln!("\n──────── HELP TEXT ────────\n{text}");
        }

        for topic in ["BASE BUNDLE", "CONTENT MOD vs LEVEL MOD", "WHERE FILES ARE READ FROM",
                      "WHERE OUTPUT GOES", "CLI MODE"] {
            assert!(text.contains(topic), "help section {topic:?} missing");
        }
        // Base tiers, and the live one.
        for t in ["loose MEDIA", "DATA.PAK", "baked into this exe"] {
            assert!(text.contains(t), "base tier {t:?} not explained");
        }
        // Why the embedded bundle is gone has to survive in the docs, or the
        // next person adds one back: it was a second hand-built copy of the
        // DATA.PAK extraction, it drifted, it carried no .LAYOUT files, and packs
        // against it silently dropped every linked piece of geometry.
        assert!(text.contains(".LAYOUT"), "the bundle's missing-layouts gap is not explained");
        assert!(
            !text.contains("All three produce byte-identical"),
            "stale claim: there are two tiers now, and they were never interchangeable"
        );
        // And that nothing is guessed when neither tier answers.
        assert!(
            text.contains("ASKS if neither answers"),
            "the ask-don't-guess rule is not stated"
        );
        // The distinction is stated by its actual test (MEDIA/LAYOUTS).
        assert!(text.contains("MEDIA/LAYOUTS") && text.contains("--mpp re"));
        // The rescan key. The footer drops it on a narrow terminal, so this page
        // is the one place it is always documented.
        assert!(text.contains("Press r to scan again"), "rescan not documented");
        // Real paths, straight from the same helpers the scanners use.
        assert!(text.contains(&root.join("mods").display().to_string()), "scan: ./mods");
        assert!(text.contains(&tl2_mods_dir().display().to_string()), "scan+deploy: game mods");
        assert!(text.contains(&root.join("unpacked").display().to_string()), "unpack output");
        assert!(text.contains(&root.join(".tl2packer_base").display().to_string()), "base cache");
        // Every CLI subcommand the binary actually accepts.
        for c in ["unpack", "compile-dat", "compile-layout", "compile-mpp", "decompile-dat",
                  "decompile-layout", "extract-base", "unpack-base", "--deploy", "--mpp"] {
            assert!(text.contains(c), "CLI {c:?} not documented");
        }
    }

    /// Reversing direction at either end of a scrollable pane must move on the
    /// FIRST keypress.
    ///
    /// The bug this guards: the offset was clamped only when drawing, so
    /// holding Down at the bottom kept incrementing a counter the view no
    /// longer followed. After a second of key-repeat it sat tens of lines past
    /// the end, and Up had to burn one keypress per phantom line before
    /// anything moved. Both panes shared the defect, so both are checked here,
    /// through the real key handler and against a real frame — the clamp reads
    /// the maximum the last frame published.
    #[test]
    fn reversing_at_either_end_moves_immediately() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let press = |app: &mut App, code: KeyCode, times: usize, terminal: &mut Terminal<TestBackend>| {
            for _ in 0..times {
                terminal.draw(|f| draw(f, app)).unwrap();
                assert!(handle_key(app, KeyEvent::from(code)).is_none());
            }
            terminal.draw(|f| draw(f, app)).unwrap();
        };

        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        // Short enough that the help page overflows it by a lot.
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();

        // HELP — the tab the bug was reported on.
        app.tab = TAB_HELP;
        press(&mut app, KeyCode::Down, 400, &mut terminal); // "held the key down"
        let bottom = app.help_scroll;
        assert_eq!(bottom, app.help_max.get(), "Down parks exactly at the end");
        assert!(bottom > 0 && bottom < 400, "help really does overflow (got {bottom})");
        press(&mut app, KeyCode::Up, 1, &mut terminal);
        assert_eq!(app.help_scroll, bottom - 1, "one Up scrolls one line back");

        press(&mut app, KeyCode::Up, 400, &mut terminal); // and back to the top
        assert_eq!(app.help_scroll, 0, "Up parks exactly at the top");
        press(&mut app, KeyCode::Down, 1, &mut terminal);
        assert_eq!(app.help_scroll, 1, "one Down scrolls one line forward");

        // OUTPUT — same defect, plus the tail-follow hand-off.
        app.tab = TAB_OUTPUT;
        for i in 0..200 {
            logln(&app.log, &format!("line {i}"));
        }
        terminal.draw(|f| draw(f, &app)).unwrap();
        let tail = app.out_max.get();
        assert!(tail > 0, "the log overflows the viewport");
        assert!(app.follow, "a fresh app follows the tail");
        // Leaving follow keeps the position: the view was at the tail, so Up
        // steps back from the tail — not to line 0, which is what a stale
        // offset would have done.
        press(&mut app, KeyCode::Up, 1, &mut terminal);
        assert_eq!(app.out_scroll, tail - 1, "Up leaves follow at the tail, not the top");

        press(&mut app, KeyCode::Down, 400, &mut terminal);
        assert_eq!(app.out_scroll, tail, "Down parks exactly at the end");
        press(&mut app, KeyCode::Up, 1, &mut terminal);
        assert_eq!(app.out_scroll, tail - 1, "one Up scrolls one line back");
    }

    /// Dump the rendered frames — glyphs AND colours — as JSON, for the README
    /// screenshots. Not a test: it asserts nothing and only runs when asked.
    ///
    ///   TL2_TUI_SHOT=<dir> cargo test --bin tl2-mikuro-mod-packer dump_frames -- --ignored
    ///
    /// Rendering here rather than photographing a terminal is what keeps the
    /// images honest: they come out of the same `draw` the program runs, so
    /// they cannot drift from the UI the way a hand-taken screenshot does. The
    /// content is fixed demo data, deliberately IDENTICAL across languages, so
    /// the three sets show exactly what is translated (the chrome) and what is
    /// not (mod names, CLI flags).
    #[test]
    #[ignore = "writes README screenshots"]
    fn dump_frames_for_docs() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let Some(dir) = std::env::var_os("TL2_TUI_SHOT") else {
            eprintln!("set TL2_TUI_SHOT=<dir>");
            return;
        };
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();

        let demo_pack = || {
            vec![
                ModEntry {
                    title: "Extra Maps".into(),
                    file_name: "EXTRA_MAPS".into(),
                    dir: PathBuf::from("/x/ExtraMaps"),
                    has_layouts: true,
                    action: Action::Pack,
                    origin: Origin::Local,
                },
                ModEntry {
                    title: "My First Mod".into(),
                    file_name: "MY_FIRST_MOD".into(),
                    dir: PathBuf::from("/x/MyFirstMod"),
                    has_layouts: false,
                    action: Action::Pack,
                    origin: Origin::Local,
                },
            ]
        };
        let demo_unpack = || {
            vec![
                ModEntry {
                    title: "Extra Maps".into(),
                    file_name: "EXTRA_MAPS".into(),
                    dir: PathBuf::from("/x/EXTRA_MAPS.MOD"),
                    has_layouts: false,
                    action: Action::Unpack,
                    origin: Origin::Local,
                },
                ModEntry {
                    title: "Someone Else's Mod".into(),
                    file_name: "SOMEONE_ELSES_MOD".into(),
                    dir: PathBuf::from("/game/mods/SOMEONE_ELSES_MOD.MOD"),
                    has_layouts: false,
                    action: Action::Unpack,
                    origin: Origin::Deployed,
                },
            ]
        };

        for lang in [Lang::En, Lang::Zh, Lang::Ru] {
            for (tab, name) in [(TAB_PACK, "pack"), (TAB_UNPACK, "unpack"), (TAB_OUTPUT, "output")] {
                let mut app =
                    App::new(demo_pack(), demo_unpack(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
                app.set_lang(lang);
                app.tab = tab;
                match tab {
                    TAB_PACK => app.pack.toggle_current(),
                    TAB_UNPACK => app.unpack.toggle_current(),
                    _ => {
                        // A believable mid-run transcript: the lines the worker
                        // really streams, in the order it streams them.
                        app.packing.store(true, Ordering::Relaxed);
                        for l in [
                            "Pathing base: C:\\Tools\\.tl2packer_base",
                            "",
                            "═══════ pack Extra Maps  (mpp=re) ═══════",
                            "MOD: EXTRA_MAPS",
                            "Title: Extra Maps",
                            "[1/3] Converting in memory (no copy)...",
                            "  Generating MPP files (backend=re)...",
                            "",
                            "  Results:",
                            "    DAT→BINDAT:   184 files",
                            "    LAYOUT→BINLAYOUT: 37 files",
                            "    RAW generated: 3 files",
                            "    MPP generated: 12 files (backend=re)",
                            "    (convert took 1.8s)",
                            "[2/3] Packing .MOD...",
                        ] {
                            logln(&app.log, l);
                        }
                    }
                }
                // The pickers are short lists; the log wants room to look like
                // a log rather than a four-line peephole.
                let rows = if tab == TAB_OUTPUT { 24 } else { 18 };
                let mut terminal = Terminal::new(TestBackend::new(96, rows)).unwrap();
                terminal.draw(|f| draw(f, &app)).unwrap();
                let buf = terminal.backend().buffer();

                let hex = |c: Option<Color>| match c {
                    Some(Color::Rgb(r, g, b)) => format!("\"#{r:02X}{g:02X}{b:02X}\""),
                    _ => "null".to_string(),
                };
                let mut out =
                    format!("{{\"w\":{},\"h\":{},\"cells\":[", buf.area.width, buf.area.height);
                for y in 0..buf.area.height {
                    for x in 0..buf.area.width {
                        let cell = &buf[(x, y)];
                        let st = cell.style();
                        let sym = cell.symbol().replace('\\', "\\\\").replace('"', "\\\"");
                        if !(x == 0 && y == 0) {
                            out.push(',');
                        }
                        out.push_str(&format!(
                            "{{\"s\":\"{sym}\",\"fg\":{},\"bg\":{},\"b\":{}}}",
                            hex(st.fg),
                            hex(st.bg),
                            st.add_modifier.contains(Modifier::BOLD)
                        ));
                    }
                }
                out.push_str("]}");
                let p = dir.join(format!("tui-{}-{name}.json", lang.code()));
                std::fs::write(&p, out).unwrap();
                eprintln!("wrote {}", p.display());
            }
        }
    }

    /// An empty picker must say what it looked for instead of showing a blank
    /// box — the unpack tab in particular is empty for anyone who has not built
    /// a mod yet, and the three scanned locations are the useful answer.
    #[test]
    fn empty_picker_explains_itself() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("No mod sources found"), "pack empty state");

        app.tab = TAB_UNPACK;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("No .MOD files found"), "unpack empty state");
        assert!(text.contains("Looked in"), "and where it looked");
    }

    /// Regression guard: the rendered frame actually uses the Emberworks palette
    /// (not just that it compiles) — furnace-black frame, panel fill, ember accent
    /// and the ember-backed active tab all show up in the buffer's styles.
    /// An unrecoverable key name has to be visually unmissable: the `UNK_<hex>` digits
    /// recompile to the original hash, so a user who "tidies them up" silently detaches
    /// the section from the key the engine looks for, with no error anywhere.
    #[test]
    fn unresolved_is_red_and_recovered_is_green() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        app.tab = TAB_OUTPUT;
        logln(&app.log, "  unpacked 12 files (3 DAT), 400 bytes".to_string());
        logln(&app.log, "      UNK_9DF54BFD  (MEDIA/UNITS/ITEMS/RING.DAT)".to_string());
        logln(&app.log, "      recovered: UNK_9843BB8B -> DISALLOWPVP  (t2_engine_ident)".to_string());
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let mut warn_row = None;
        let mut plain_row = None;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains("UNK_9DF54BFD") {
                warn_row = Some(y);
            } else if row.contains("unpacked 12 files") {
                plain_row = Some(y);
            }
        }
        let wy = warn_row.expect("the UNK_ line must be on screen");
        let py = plain_row.expect("the ordinary line must be on screen");
        let cell_at = |y: u16, needle: &str| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            let col = row.find(needle).unwrap() as u16;
            buf[(col, y)].style()
        };
        assert_eq!(
            cell_at(wy, "UNK_").fg,
            Some(theme::WARN),
            "the placeholder must be painted with the warning colour"
        );
        assert_ne!(
            cell_at(py, "unpacked").fg,
            Some(theme::WARN),
            "ordinary log lines must stay unpainted"
        );

        // The other half of the story: a hash that WAS unknown and got named anyway is
        // green, so the user reads "this was solved" rather than only ever seeing losses.
        let mut rec_row = None;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains("recovered:") {
                rec_row = Some(y);
            }
        }
        let ry = rec_row.expect("the recovered line must be on screen");
        assert_eq!(
            cell_at(ry, "recovered:").fg,
            Some(theme::SET),
            "a recovered name must be painted green"
        );
        assert_ne!(
            cell_at(ry, "recovered:").fg,
            Some(theme::WARN),
            "and must never be mistaken for a failure"
        );
    }

    #[test]
    fn renders_with_emberworks_palette() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let packs = vec![ModEntry {
            title: "Alpha".into(),
            file_name: "A".into(),
            dir: PathBuf::from("/x/A"),
            has_layouts: false,
            action: Action::Pack,
            origin: Origin::Local,
        }];
        let mut app = App::new(packs, Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let mut fgs = std::collections::HashSet::new();
        let mut bgs = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let s = buf[(x, y)].style();
                if let Some(c) = s.fg {
                    fgs.insert(c);
                }
                if let Some(c) = s.bg {
                    bgs.insert(c);
                }
            }
        }
        assert!(bgs.contains(&theme::BG), "furnace-black frame background");
        assert!(bgs.contains(&theme::PANEL), "panel fill");
        assert!(bgs.contains(&theme::EMBER), "ember-backed active tab");
        assert!(fgs.contains(&theme::EMBER), "ember accent text");
        assert!(fgs.contains(&theme::TEXT), "primary parchment text");

        // Every tab is padded by two spaces on each side, INSIDE its coloured
        // span. The header used to render with `Wrap { trim: true }`, which
        // strips a line's leading whitespace — so the leftmost tab lost its two
        // and sat visibly tighter than the rest. Check the padding is painted,
        // not just present in the string.
        let (tx, ty) = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buf[(x, y)].symbol() == "P" && buf[(x, y)].style().bg == Some(theme::EMBER)
            })
            .expect("active PACK tab is drawn");
        for back in 1..=2u16 {
            let cell = &buf[(tx - back, ty)];
            assert_eq!(cell.symbol(), " ", "tab padding cell");
            assert_eq!(cell.style().bg, Some(theme::EMBER), "…and it is inside the tab");
        }
        // Two rows: the title sits on the top border, then a blank line, so the
        // tabs are not glued to the heading.
        assert!(ty >= 2, "tab row is below the title AND a blank row (y={ty})");
    }

    /// Panels put a blank row between the title on their border and the first
    /// line of content, so heading and body do not read as one block.
    #[test]
    fn panels_breathe_under_their_title() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        app.tab = TAB_HELP;
        let mut terminal = Terminal::new(TestBackend::new(96, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let lines: Vec<&str> = text.lines().collect();
        let title_row = lines
            .iter()
            .position(|l| l.contains("Help"))
            .expect("help panel title");
        // "Blank" means blank INSIDE the panel — the border glyphs stay.
        let empty = |l: &str| l.chars().all(|c| c == ' ' || c == '│');
        assert!(
            empty(lines[title_row + 1]),
            "blank row under the panel title, got {:?}",
            lines[title_row + 1]
        );
        assert!(
            !empty(lines[title_row + 2]),
            "content starts right after it, got {:?}",
            lines[title_row + 2]
        );
    }

    /// Read the rendered buffer back as text.
    ///
    /// A double-width glyph (every CJK character in the Chinese UI) occupies
    /// TWO cells and ratatui leaves the second one blank, so walking cells
    /// naively yields "打 包" — and no assertion on Chinese text can ever
    /// match. Skip the continuation cell after a wide glyph.
    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        // Everything this UI draws above U+2E80 is CJK or full-width
        // punctuation (double-width); its box-drawing and arrows sit below.
        let wide = |sym: &str| {
            let mut it = sym.chars();
            matches!((it.next(), it.next()), (Some(c), None) if (c as u32) >= 0x2E80)
        };
        let mut s = String::new();
        for y in 0..buf.area.height {
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                s.push_str(sym);
                x += if wide(sym) { 2 } else { 1 };
            }
            s.push('\n');
        }
        s
    }

    /// A picker with more rows than fit must scroll like a list, not like a
    /// treadmill.
    ///
    /// The bug this guards: `draw_list` handed the widget a CLONE of the list
    /// state, so the scroll offset ratatui writes there was dropped every
    /// frame. Re-derived from 0, it always came out as "just enough to reveal
    /// the selection" — which pins the cursor to the LAST visible row for the
    /// whole rest of the list, in both directions. You could never see a row
    /// past the cursor, and every keypress dragged the entire list.
    ///
    /// Anyone with more deployed mods than terminal rows hits this on the
    /// Unpack tab immediately, which is why 30 rows in an 18-row terminal.
    #[test]
    fn long_picker_scrolls_without_dragging_the_cursor() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let rows: Vec<ModEntry> = (0..30)
            .map(|i| ModEntry {
                title: format!("Mod{i:02}"),
                file_name: format!("M{i:02}"),
                dir: PathBuf::from("/x"),
                has_layouts: false,
                action: Action::Pack,
                origin: Origin::Local,
            })
            .collect();
        let mut app = App::new(rows, Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        // Which rows the frame shows, in order.
        let visible = |app: &App, terminal: &mut Terminal<TestBackend>| -> Vec<String> {
            terminal.draw(|f| draw(f, app)).unwrap();
            buffer_text(terminal.backend().buffer())
                .lines()
                .filter_map(|l| {
                    l.split_whitespace()
                        .find(|w| w.len() == 5 && w.starts_with("Mod"))
                        .map(str::to_string)
                })
                .collect()
        };
        let press = |app: &mut App, code: KeyCode| {
            handle_key(app, KeyEvent::from(code));
        };

        // Walk down until the list has had to scroll.
        for _ in 0..14 {
            visible(&app, &mut terminal);
            press(&mut app, KeyCode::Down);
        }
        let at_bottom = visible(&app, &mut terminal);
        assert_eq!(app.pack.state.selected(), Some(14));
        assert!(at_bottom.contains(&"Mod14".to_string()), "cursor row is on screen");
        assert!(!at_bottom.contains(&"Mod00".to_string()), "the list really scrolled");

        // Coming back up must NOT move the window: rows 9..14 are all on screen,
        // so the cursor just walks up inside it. Under the bug every one of
        // these shifted the whole list by one and kept the cursor on the last
        // row.
        for expect_sel in [13, 12, 11] {
            press(&mut app, KeyCode::Up);
            let now = visible(&app, &mut terminal);
            assert_eq!(app.pack.state.selected(), Some(expect_sel));
            assert_eq!(now, at_bottom, "window held still while the cursor moved up");
            assert!(
                now.last() != Some(&format!("Mod{expect_sel:02}")),
                "cursor is no longer stuck to the bottom row"
            );
        }

        // Wrap-around still works, and jumps the window to the end.
        app.pack.state.select(Some(0));
        press(&mut app, KeyCode::Up);
        let wrapped = visible(&app, &mut terminal);
        assert_eq!(app.pack.state.selected(), Some(29));
        assert!(wrapped.contains(&"Mod29".to_string()), "wrapped to the last row and showed it");
    }

    /// The footer must never clip. It is the only place several keys are named
    /// at all, and the ones at the END — change language, quit — are exactly the
    /// ones nothing else advertises, so a line that silently ran off the edge
    /// took those with it. Russian at 80 columns is the tight case: the labels
    /// are the longest and 80×25 is what a default console window gives you.
    #[test]
    fn the_footer_fits_every_language_at_80_columns() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let packs = || {
            vec![ModEntry {
                title: "Some Mod".into(),
                file_name: "SOME_MOD".into(),
                dir: PathBuf::from("/x/SomeMod"),
                has_layouts: true,
                action: Action::Pack,
                origin: Origin::Local,
            }]
        };
        for lang in [Lang::En, Lang::Zh, Lang::Ru] {
            let mut app =
                App::new(packs(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
            app.set_lang(lang);
            for width in [80u16, 96, 120] {
                for tab in [TAB_PACK, TAB_UNPACK, TAB_OUTPUT, TAB_HELP] {
                    app.tab = tab;
                    let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
                    terminal.draw(|f| draw(f, &app)).unwrap();
                    let text = buffer_text(terminal.backend().buffer());
                    let footer = text
                        .lines()
                        .rev()
                        .find(|l| l.contains('q'))
                        .unwrap_or_default()
                        .to_string();
                    for want in ["←→", "l ", "q "] {
                        assert!(
                            footer.contains(want),
                            "{lang:?} tab {tab} at {width} cols lost {want:?}: {footer:?}"
                        );
                    }
                    assert!(
                        footer.contains(app.lang.next().tag()),
                        "{lang:?} tab {tab} at {width} cols lost the language switch: {footer:?}"
                    );
                    // Rescan is marked optional, so 80 columns is allowed to
                    // drop it (it is on the Help page too). A normal-width
                    // terminal is not: the footer is where keys get found.
                    // Matched on the LABEL — "r " also occurs inside "Enter ".
                    if width >= 120 {
                        assert!(
                            footer.contains(app.tr("rescan").trim()),
                            "{lang:?} tab {tab} at {width} cols lost rescan: {footer:?}"
                        );
                    }
                }
            }
        }
    }

    /// `r` rescans the disk. Both pickers were built once, at startup, and the
    /// folder they describe is one the user keeps editing while the app is open:
    /// a mod folder appears, a `.MOD` gets built or deleted. Seeing any of it
    /// meant quitting and relaunching.
    ///
    /// What must survive a rescan is the state the user typed in — the ticks and
    /// the cursor — carried by PATH. Carried by index they would slide onto
    /// whatever moved into those slots, which is the one way a refresh could
    /// pack the wrong mod.
    #[test]
    fn r_rescans_the_disk_and_carries_the_ticks_over() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let root = std::env::temp_dir().join(format!("tl2tui_refresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        mk_mod(&root, "Bravo", false);
        mk_mod(&root, "Charlie", true);

        let mut app = App::new(
            discover_pack_sources(&root),
            discover_mod_files(&root),
            detect_base(&root),
            root.clone(),
        );
        app.set_lang(Lang::En);
        let titles =
            |a: &App| a.pack.items.iter().map(|m| m.title.clone()).collect::<Vec<String>>();
        assert_eq!(titles(&app), ["Bravo", "Charlie"]);

        // Tick Charlie, cursor on it: the state the rescan has to keep.
        app.pack.state.select(Some(1));
        app.pack.toggle_current();

        // A source that sorts BEFORE both — every index shifts by one.
        mk_mod(&root, "Alpha", false);
        // …and a built .MOD, which is the change the UNPACK tab exists for.
        let fresh = root.join("Fresh.MOD");
        std::fs::write(&fresh, b"not a real container").unwrap();

        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('r'))).is_none());
        assert_eq!(titles(&app), ["Alpha", "Bravo", "Charlie"], "the new source showed up");
        assert_eq!(app.pack.state.selected(), Some(2), "cursor followed Charlie to index 2");
        assert_eq!(app.pack.picked, [false, false, true], "and so did its tick");
        // The deployed mods folder is scanned too, and what THIS machine keeps
        // there is not ours to predict — so look for the file, not for a count.
        assert!(app.unpack.items.iter().any(|m| m.dir == fresh), "the new .MOD showed up");

        // Delete the row under the cursor. The cursor holds its POSITION, so it
        // lands on whatever took that row's place instead of jumping to the top.
        std::fs::remove_dir_all(root.join("Charlie")).unwrap();
        std::fs::remove_file(&fresh).unwrap();
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('r'))).is_none());
        assert_eq!(titles(&app), ["Alpha", "Bravo"]);
        assert_eq!(app.pack.state.selected(), Some(1), "clamped into the shorter list");
        assert_eq!(app.pack.picked, [false, false], "Charlie's tick left with Charlie");
        assert!(!app.unpack.items.iter().any(|m| m.dir == fresh), "the deleted .MOD is gone");

        // The rescan has to SAY so. Rescanning an unchanged tree changes nothing
        // on screen, and a key with no visible answer reads as a broken one.
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        if std::env::var_os("TL2_TUI_DUMP").is_some() {
            eprintln!("\n──────── AFTER RESCAN ────────\n{text}");
        }
        assert!(text.contains("rescanned"), "the header says the scan happened: {text}");
        assert!(
            app.log.lock().unwrap().iter().any(|l| l.contains("2 mod source(s)")),
            "and the transcript keeps the counts the one-word note has no room for"
        );
        // …but it must not squat on the status slot, which is also where
        // "selected N/M" lives. Space would look dead until the note aged out.
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char(' '))).is_none());
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(!text.contains("rescanned"), "the next keypress dismissed the note");
        assert!(text.contains("selected 1/2"), "and the tick count answered: {text}");
        app.pack.toggle_current(); // put the tick back for what follows

        // Empty and back. A picker can legitimately open with no rows (launch
        // somewhere with only `.MOD` files in reach and the PACK tab starts
        // empty), and the row that then appears has to get the cursor — there
        // is no earlier selection to carry over.
        std::fs::remove_dir_all(root.join("Alpha")).unwrap();
        std::fs::remove_dir_all(root.join("Bravo")).unwrap();
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('r'))).is_none());
        assert!(app.pack.items.is_empty() && app.pack.state.selected().is_none());
        mk_mod(&root, "Delta", false);
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('r'))).is_none());
        assert_eq!(app.pack.state.selected(), Some(0), "the first row gets the cursor");

        // Refused mid-run: the worker is writing the very files a scan reads.
        app.packing.store(true, Ordering::Relaxed);
        let before = titles(&app);
        mk_mod(&root, "Zulu", false);
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('r'))).is_none());
        assert_eq!(titles(&app), before, "no rescan while a run is going");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Ctrl+C must quit, not clear the log. In raw mode it is an ordinary
    /// keypress; unanswered, the app cannot be exited with it at all, and on the
    /// Output tab it fell through to the bare `c` = clear binding.
    #[test]
    fn ctrl_c_quits_and_does_not_clear_the_log() {
        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::LooseMedia(PathBuf::from("/base/MEDIA")), PathBuf::from("/x"));
        app.set_lang(Lang::En);
        app.tab = TAB_OUTPUT;
        logln(&app.log, "important line");

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(handle_key(&mut app, ctrl_c).is_some(), "Ctrl+C quits");
        assert_eq!(app.log.lock().unwrap().len(), 1, "and left the transcript alone");

        // Plain `c` still clears, and Ctrl+C is refused mid-run like q is.
        assert!(handle_key(&mut app, KeyEvent::from(KeyCode::Char('c'))).is_none());
        assert!(app.log.lock().unwrap().is_empty(), "bare c still clears");
        app.packing.store(true, Ordering::Relaxed);
        assert!(handle_key(&mut app, ctrl_c).is_none(), "no quitting mid-run");
    }

    /// Two selected `.MOD`s with the same file name (the local build and the
    /// deployed copy — rows the picker shows side by side on purpose) must
    /// unpack into separate folders, not merge into one.
    #[test]
    fn same_named_mods_unpack_to_separate_folders() {
        let mut used = std::collections::BTreeSet::new();
        assert_eq!(unique_dir_name("MIKURO_X", &mut used), "MIKURO_X");
        assert_eq!(unique_dir_name("MIKURO_X", &mut used), "MIKURO_X_2");
        assert_eq!(unique_dir_name("MIKURO_X", &mut used), "MIKURO_X_3");
        assert_eq!(unique_dir_name("OTHER", &mut used), "OTHER", "unrelated names untouched");
        // A name that collides with a generated one still resolves.
        assert_eq!(unique_dir_name("MIKURO_X_2", &mut used), "MIKURO_X_2_2");
    }

    #[test]
    fn datapak_detected_when_no_loose_media() {
        let root = std::env::temp_dir().join(format!("tl2tui_pak_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("PAKS")).unwrap();
        std::fs::write(root.join("PAKS").join("DATA.PAK"), b"fake").unwrap();
        assert!(matches!(detect_base(&root), BaseSource::DataPak(_)));
        std::fs::remove_dir_all(&root).ok();
    }
}
