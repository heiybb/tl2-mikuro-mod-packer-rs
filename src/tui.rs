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

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap};
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
}

// ── UI language ────────────────────────────────────────────────────────────
// Only the TUI is localized. Everything the CLI prints (and every line the pack
// / unpack workers stream into the Output tab) stays English: those are the
// same strings the scriptable CLI emits, and people paste them into bug reports.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Lang {
    En,
    Zh,
}

impl Lang {
    /// Pick a string by language. Call sites read `t(en, zh)`, English first,
    /// so the source stays greppable against the CLI's own wording.
    fn t(self, en: &'static str, zh: &'static str) -> &'static str {
        match self {
            Lang::En => en,
            Lang::Zh => zh,
        }
    }

    fn is_zh(self) -> bool {
        self == Lang::Zh
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
        let v = v.to_string_lossy().to_ascii_lowercase();
        if v.starts_with("zh") || v.starts_with("cn") {
            return Lang::Zh;
        }
        if v.starts_with("en") {
            return Lang::En;
        }
    }
    #[cfg(windows)]
    {
        // LANGID low 10 bits = primary language; 0x04 = LANG_CHINESE (any of
        // zh-CN / zh-TW / zh-HK — all get the Simplified strings, which is the
        // audience this tool actually has).
        let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        if id & 0x3FF == 0x04 {
            return Lang::Zh;
        }
        // A non-Chinese Windows UI is a real answer; fall through to English
        // rather than letting a stray POSIX LANG override the OS setting.
        return Lang::En;
    }
    #[cfg(not(windows))]
    {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(v) = std::env::var(key) {
                if v.to_ascii_lowercase().starts_with("zh") {
                    return Lang::Zh;
                }
            }
        }
        Lang::En
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
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Origin::Local => lang.t("local", "本地"),
            Origin::Deployed => lang.t("game mods", "游戏 mods"),
        }
    }
}

/// Where the base game MEDIA can come from, in priority order.
#[derive(Clone)]
enum BaseSource {
    /// A loose `MEDIA/` folder next to the exe (already extracted).
    LooseMedia(PathBuf),
    /// `PAKS/DATA.PAK` next to the exe — targeted extraction (Phase 2).
    DataPak(PathBuf),
    /// Embedded base bundle — always available (baked into the exe), the base
    /// tier used when neither loose MEDIA nor DATA.PAK is present.
    Bundled,
}

impl BaseSource {
    /// Full description, path included — for the Help tab and the run log.
    fn label(&self) -> String {
        match self {
            BaseSource::LooseMedia(p) => format!("loose MEDIA ({})", p.display()),
            BaseSource::DataPak(p) => format!("DATA.PAK ({})", p.display()),
            BaseSource::Bundled => "bundled base (embedded)".to_string(),
        }
    }

    /// Just the tier, for the header — a base path can be 60 columns wide and
    /// would push the run status off the end of the bar.
    fn short(&self) -> &'static str {
        match self {
            BaseSource::LooseMedia(_) => "loose MEDIA",
            BaseSource::DataPak(_) => "DATA.PAK",
            BaseSource::Bundled => "bundled",
        }
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

/// Detect the base MEDIA source under `root`: loose MEDIA, then DATA.PAK, then
/// the embedded bundle (if compiled in), else none.
fn detect_base(root: &Path) -> BaseSource {
    let media = root.join("MEDIA");
    if media.is_dir() {
        return BaseSource::LooseMedia(media);
    }
    for cand in [root.join("PAKS").join("DATA.PAK"), root.join("DATA.PAK")] {
        if cand.is_file() {
            return BaseSource::DataPak(cand);
        }
    }
    // The base bundle is always embedded, so this tier is always available.
    BaseSource::Bundled
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
                m.origin.label(Lang::En),
                m.dir.display()
            );
        }
        return ExitCode::SUCCESS;
    }

    if packs.is_empty() && unpacks.is_empty() {
        println!("Mikuro Torchlight II Mod Packer");
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

/// Resolve a base MEDIA directory for MPP regeneration, logging progress to the
/// Output tab. Loose MEDIA is used directly; DATA.PAK / the embedded bundle are
/// unpacked into a persistent cache next to the exe on first use (reused after).
fn resolve_base_media(root: &Path, base: &BaseSource, log: &Log) -> Option<PathBuf> {
    let cache = root.join(".tl2packer_base");
    match base {
        BaseSource::LooseMedia(p) => Some(p.clone()),
        BaseSource::DataPak(pak) => {
            if cache.join("LEVELSETS").is_dir() {
                return Some(cache);
            }
            logln(log, "Extracting base data from DATA.PAK (first run)…");
            match PakReader::open_pak(pak).and_then(|mut r| r.extract_base(&cache)) {
                Ok((d, mesh)) => {
                    logln(log, format!("  base ready: {d} DATs + {mesh} collision meshes"));
                    Some(cache)
                }
                Err(e) => {
                    logln(log, format!("  base extract failed: {e}"));
                    None
                }
            }
        }
        BaseSource::Bundled => {
            if cache.join("LEVELSETS").is_dir() {
                return Some(cache);
            }
            logln(log, "Unpacking embedded base data (first run)…");
            match crate::base_bundle::ensure_extracted(&cache) {
                Ok(()) => {
                    logln(log, "  base ready (bundled)");
                    Some(cache)
                }
                Err(e) => {
                    logln(log, format!("  bundled base failed: {e}"));
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
                let dst = root.join("unpacked").join(&j.file_name);
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
}

impl Pane {
    fn new(items: Vec<ModEntry>) -> Self {
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        let n = items.len();
        Pane { items, picked: vec![false; n], state }
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

struct App {
    pack: Pane,
    unpack: Pane,
    base: BaseSource,
    root: PathBuf,
    tab: usize,
    log: Log,
    packing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    follow: bool, // Output tab auto-scroll to tail
    out_scroll: u16,
    help_scroll: u16,
    lang: Lang,
}

impl App {
    fn new(packs: Vec<ModEntry>, unpacks: Vec<ModEntry>, base: BaseSource, root: PathBuf) -> Self {
        // Open on whichever picker actually has rows.
        let tab = if packs.is_empty() && !unpacks.is_empty() { TAB_UNPACK } else { TAB_PACK };
        App {
            pack: Pane::new(packs),
            unpack: Pane::new(unpacks),
            base,
            root,
            tab,
            log: Arc::new(Mutex::new(Vec::new())),
            packing: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            follow: true,
            out_scroll: 0,
            help_scroll: 0,
            lang: detect_lang(),
        }
    }

    fn t(&self, en: &'static str, zh: &'static str) -> &'static str {
        self.lang.t(en, zh)
    }

    fn is_packing(&self) -> bool {
        self.packing.load(Ordering::Relaxed)
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
        match k.code {
            KeyCode::Tab => app.tab = (app.tab + 1) % TAB_COUNT,
            KeyCode::BackTab => app.tab = (app.tab + TAB_COUNT - 1) % TAB_COUNT,
            // Direct tab keys: faster than cycling once there are three.
            KeyCode::Char('1') => app.tab = TAB_PACK,
            KeyCode::Char('2') => app.tab = TAB_UNPACK,
            KeyCode::Char('3') => app.tab = TAB_OUTPUT,
            KeyCode::Char('4') | KeyCode::Char('?') | KeyCode::F(1) => app.tab = TAB_HELP,
            KeyCode::Char('q') | KeyCode::Esc => {
                // Ignore quit while a run is going (the child would orphan).
                if !app.is_packing() {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            _ if app.tab == TAB_OUTPUT => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.follow = false;
                    app.out_scroll = app.out_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.follow = false;
                    app.out_scroll = app.out_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.follow = false;
                    app.out_scroll = app.out_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => app.out_scroll = app.out_scroll.saturating_add(10),
                KeyCode::End | KeyCode::Char('G') => app.follow = true,
                KeyCode::Char('c') => app.clear_log(),
                _ => {}
            },
            _ if app.tab == TAB_HELP => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.help_scroll = app.help_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.help_scroll = app.help_scroll.saturating_add(1)
                }
                KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
                KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
                KeyCode::Home => app.help_scroll = 0,
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
    }
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
        Constraint::Length(3), // header / tabs
        Constraint::Length(1), // ── breathing room
        Constraint::Min(3),    // body (picker or output)
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
            app.t("running…", "运行中…").to_string(),
            Style::default().fg(theme::GOLD).add_modifier(Modifier::BOLD),
        )
    } else if app.finished.load(Ordering::Relaxed) {
        (
            app.t("done", "完成").to_string(),
            Style::default().fg(theme::SET).add_modifier(Modifier::BOLD),
        )
    } else if let Some(p) = app.pane() {
        (
            format!(
                "{} {}/{}",
                app.t("selected", "已选"),
                p.picked_count(),
                p.items.len()
            ),
            Style::default().fg(theme::EMBER),
        )
    } else {
        (String::new(), Style::default())
    };
    let gap = || Span::raw("  ");
    let line = Line::from(vec![
        tab(app.tab == TAB_PACK, app.t("PACK", "打包"), Some(app.pack.items.len())),
        gap(),
        tab(app.tab == TAB_UNPACK, app.t("UNPACK", "解包"), Some(app.unpack.items.len())),
        gap(),
        tab(app.tab == TAB_OUTPUT, app.t("OUTPUT", "输出"), None),
        gap(),
        tab(app.tab == TAB_HELP, app.t("HELP", "帮助"), None),
        Span::styled(app.t("    base ", "    base "), Style::default().fg(theme::MUTED)),
        Span::styled(app.base.short(), Style::default().fg(theme::GOLD)),
        Span::styled("    ", Style::default()),
        Span::styled(status, status_style),
    ]);
    let p = Paragraph::new(line)
        .block(panel(
            Span::styled(
                " Mikuro Torchlight II Mod Packer ",
                Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
            ),
            theme::LINE_HI,
        ))
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let unpacking = app.tab == TAB_UNPACK;
    let Some(pane) = app.pane() else { return };
    let title = Span::styled(
        if unpacking {
            app.t(" unpack · Space select · Enter run ", " 解包 · Space 选择 · Enter 运行 ")
        } else {
            app.t(" pack · Space select · Enter run ", " 打包 · Space 选择 · Enter 运行 ")
        },
        Style::default().fg(theme::MUTED),
    );

    // Empty picker: say what was looked for and where, rather than an empty box.
    if pane.items.is_empty() {
        let msg = if unpacking {
            vec![
                Line::from(Span::styled(
                    app.t("No .MOD files found.", "没有找到 .MOD 文件。"),
                    Style::default().fg(theme::MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    app.t("Looked in:", "找过这些位置:"),
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
                    app.t("No mod sources found.", "没有找到可打包的 mod 源目录。"),
                    Style::default().fg(theme::MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    app.t(
                        "A source is a folder holding MOD.DAT and a MEDIA/ tree.",
                        "源目录 = 一个含 MOD.DAT 和 MEDIA/ 的文件夹。",
                    ),
                    Style::default().fg(theme::FAINT),
                )),
            ]
        };
        let p = Paragraph::new(msg)
            .block(panel(title, theme::LINE))
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
                    format!("  ({})", m.origin.label(app.lang)),
                    Style::default().fg(if m.origin == Origin::Deployed {
                        theme::GOLD
                    } else {
                        theme::FAINT
                    }),
                ),
                (Action::Pack, true) => Span::styled(
                    app.t("  (level mod)", "  (关卡 mod)"),
                    Style::default().fg(theme::MAGIC),
                ),
                (Action::Pack, false) => Span::styled(
                    app.t("  (content mod)", "  (内容 mod)"),
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
        .block(panel(title, theme::LINE))
        .highlight_style(
            Style::default()
                .bg(theme::LINE_HI)
                .fg(theme::EMBER_HI)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = pane.state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_output(f: &mut Frame, area: Rect, app: &App) {
    let log = app.log.lock().map(|v| v.clone()).unwrap_or_default();
    let total = log.len() as u16;
    let inner_h = area.height.saturating_sub(2); // minus the border
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = if app.follow {
        max_scroll
    } else {
        app.out_scroll.min(max_scroll)
    };
    let (title, tstyle) = if total == 0 {
        (
            app.t(" Output · empty ", " 输出 · 空 ").to_string(),
            Style::default().fg(theme::FAINT),
        )
    } else if app.follow {
        (
            format!(
                " {} · {} · {total} {} ",
                app.t("Output", "输出"),
                app.t("following", "跟随"),
                app.t("lines", "行")
            ),
            Style::default().fg(theme::SET),
        )
    } else {
        (
            format!(" {} · {}/{} ", app.t("Output", "输出"), scroll.min(total), total),
            Style::default().fg(theme::MUTED),
        )
    };
    let p = Paragraph::new(log.join("\n"))
        .block(panel(Span::styled(title, tstyle), theme::LINE))
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

/// The HELP tab. Everything here is stated from the code paths it describes —
/// `detect_base` / `resolve_base_media` for the base tiers, `discover_*` for the
/// scanned locations, `pack_worker` for where output lands, `main::usage` for
/// the CLI — so it cannot drift into describing a tool we do not ship.
fn help_lines(app: &App) -> Vec<Line<'static>> {
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
        ))
    };
    let body = |s: String| Line::from(Span::styled(s, Style::default().fg(theme::TEXT)));
    let dim = |s: String| Line::from(Span::styled(s, Style::default().fg(theme::MUTED)));
    let path = |s: String| Line::from(Span::styled(s, Style::default().fg(theme::GOLD)));
    let cmd = |c: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {c}"), Style::default().fg(theme::EMBER_HI)),
            Span::styled(format!("  {what}"), Style::default().fg(theme::MUTED)),
        ])
    };
    let blank = || Line::from("");
    let root = app.root.display().to_string();
    let t = |en: &'static str, zh: &'static str| app.t(en, zh);
    // Command names and flags are never translated — they are what you type.
    let mut v = vec![
        head(t(
            "BASE BUNDLE — what it is, and why packing needs it",
            "BASE BUNDLE —— 它是什么,为什么打包需要它",
        )),
        body(t(
            "Two things a mod build cannot compute from the mod alone:",
            "有两样东西,光靠 mod 自己算不出来:",
        ).into()),
        dim(t(
            "  · UNITDATA.RAW follows each unit's BASEFILE chain into the BASE GAME's",
            "  · UNITDATA.RAW 要沿每个单位的 BASEFILE 继承链,读「基础游戏」的",
        ).into()),
        dim(t(
            "    UNITS templates to read UNITTYPE / LEVEL / RARITY / CREATEAS.",
            "    UNITS 模板里的 UNITTYPE / LEVEL / RARITY / CREATEAS。",
        ).into()),
        dim(t(
            "  · MPP walk-grids are baked against the base game's COLLISION geometry.",
            "  · MPP 寻路走格要拿基础游戏的碰撞几何来烘。",
        ).into()),
        body(t(
            "So the packer needs base data. It takes the first of three tiers:",
            "所以打包需要 base 数据。按顺序取三层里第一个可用的:",
        ).into()),
        dim(t(
            "  1. loose MEDIA/ next to this exe         (a real extracted install)",
            "  1. exe 旁边的散装 MEDIA/          (真实解包出来的安装目录)",
        ).into()),
        dim(t(
            "  2. PAKS/DATA.PAK — or DATA.PAK — beside it (extracted on first use)",
            "  2. 旁边的 PAKS/DATA.PAK 或 DATA.PAK  (首次使用时抽取)",
        ).into()),
        dim(t(
            "  3. the bundle baked into this exe        (always there, ~3 MB gzip)",
            "  3. 烧在这个 exe 里的 bundle        (永远可用,gzip 约 3 MB)",
        ).into()),
        body(format!("{} {}", t("Right now:", "当前用的是:"), app.base.label())),
        dim(t(
            "Tiers 2 and 3 unpack once into a cache next to the exe and reuse it:",
            "第 2、3 层会在 exe 旁解出一份缓存,之后复用:",
        ).into()),
        path(format!("  {}", app.root.join(".tl2packer_base").display())),
        dim(t(
            "All three produce byte-identical .MOD output — verified on a level mod.",
            "三层产出的 .MOD 逐字节一致 —— 已在一个关卡 mod 上验证过。",
        ).into()),
        blank(),
        head(t("CONTENT MOD vs LEVEL MOD", "CONTENT MOD 与 LEVEL MOD")),
        body(t(
            "The picker tags each source by one test: does MEDIA/LAYOUTS hold any",
            "列表给每个源目录打的标签只看一件事:MEDIA/LAYOUTS 里有没有",
        ).into()),
        body(t("*.LAYOUT file?", "任何 *.LAYOUT 文件?").into()),
        dim(t(
            "  content mod — no layouts. Nothing to bake; packs with --mpp none.",
            "  内容 mod —— 没有 layout。没东西要烘,按 --mpp none 打包。",
        ).into()),
        dim(t(
            "  level mod   — has layouts. Its pathing walk-grids (.MPP) are",
            "  关卡 mod —— 有 layout。它的寻路走格(.MPP)会用 --mpp re",
        ).into()),
        dim(t(
            "                regenerated with --mpp re, which is why it needs the",
            "              重新生成,这正是它需要上面那份基础碰撞几何的原因。",
        ).into()),
        dim(t(
            "                base collision geometry above. Tier 3 is embedded, so",
            "              第 3 层是内嵌的,所以 base 总是可用;",
        ).into()),
        dim(t(
            "                base is always available; if resolving it fails anyway",
            "              万一解析失败,打包照常进行,",
        ).into()),
        dim(t(
            "                the pack still runs and keeps the existing .MPP.",
            "              只是保留源目录里已有的 .MPP。",
        ).into()),
        blank(),
        head(t("WHERE FILES ARE READ FROM", "从哪里读文件")),
        body(t(
            "PACK scans for folders holding MOD.DAT (plus a MEDIA/ tree):",
            "打包:扫描含 MOD.DAT(以及 MEDIA/ 树)的文件夹:",
        ).into()),
        path(format!("  {root}")),
        path(format!("  {}", app.root.join("mods").display())),
        body(t(
            "UNPACK scans for *.MOD files in those two, plus the game's mods folder",
            "解包:在上面两处找 *.MOD,另外加上游戏的 mods 目录 —— 也就是",
        ).into()),
        body(t(
            "— the one --deploy writes to, so anything you just packed shows up:",
            "--deploy 的落点,所以你刚打好的包会直接出现在列表里:",
        ).into()),
        path(format!("  {}", tl2_mods_dir().display())),
        dim(t(
            "Rows are deduped by real path and tagged local / game mods.",
            "按真实路径去重,每行标出处(本地 / 游戏 mods)。",
        ).into()),
        blank(),
        head(t("WHERE OUTPUT GOES", "输出到哪里")),
        body(t(
            "PACK from this TUI always deploys, i.e. writes <MOD_FILE_NAME>.MOD to",
            "在这个界面里打包一律 --deploy,即把 <MOD_FILE_NAME>.MOD 写到:",
        ).into()),
        path(format!("  {}", tl2_mods_dir().display())),
        dim(t(
            "  (MOD_FILE_NAME comes from MOD.DAT; the folder name is the fallback.)",
            "  (MOD_FILE_NAME 取自 MOD.DAT;没有就退回文件夹名。)",
        ).into()),
        body(t(
            "UNPACK writes a source tree — MOD.DAT + MEDIA/ — under:",
            "解包写出一棵源目录树(MOD.DAT + MEDIA/),落在:",
        ).into()),
        path(format!("  {}", app.root.join("unpacked").display())),
        dim(t(
            "  one subfolder per mod. The game's mods folder is never written to by",
            "  每个 mod 一个子目录。解包**从不**写游戏 mods 目录,",
        ).into()),
        dim(t(
            "  an unpack, so unpacking something you have installed is safe.",
            "  所以解包一个你已经装着的 mod 是安全的。",
        ).into()),
        blank(),
        head(t(
            "CLI MODE — run it with arguments instead",
            "CLI 模式 —— 带参数运行",
        )),
        dim(t(
            "Launching with no arguments is what opens this TUI. With arguments it",
            "不带参数启动才会进这个界面;",
        ).into()),
        dim(t(
            "is the ordinary command-line packer:",
            "带参数时它就是普通的命令行打包器:",
        ).into()),
        blank(),
        cmd("<mod_dir> [--deploy]", t("pack a mod folder", "打包一个 mod 目录")),
        cmd(
            "   --in-place | --temp-copy",
            t("write converted files out (default: in memory)", "把转换产物写到磁盘(默认只在内存)"),
        ),
        cmd("   --mpp re|dll|none", t("walk-grid backend (default re)", "走格后端(默认 re)")),
        cmd("   --raw auto|none", t("RAW index generation (default auto)", "RAW 索引生成(默认 auto)")),
        blank(),
        cmd(
            "unpack <in.MOD|DATA.PAK> <out_dir>",
            t("container -> editable sources", "容器 -> 可编辑源目录"),
        ),
        cmd("   --raw", t("keep payloads compiled (no decompile)", "保留编译产物,不反编译")),
        cmd("   --no-times", t("do not restore file timestamps", "不还原文件时间戳")),
        cmd("   --filter <text>", t("only entries whose path contains <text>", "只解包路径含 <text> 的条目")),
        blank(),
        cmd("compile-dat <in.DAT> <out.BINDAT>", t("single file", "单文件")),
        cmd("compile-layout <in.LAYOUT> <out.BINLAYOUT>", t("single file", "单文件")),
        cmd("compile-mpp <in.LAYOUT> <out.mpp>", t("single tile walk-grid", "单块瓦片走格")),
        cmd(
            "decompile-dat <in.BINDAT> <out.DAT>",
            t("inverse; --utf8 for a plain dump", "逆向;--utf8 出纯文本"),
        ),
        cmd("decompile-layout <in.BINLAYOUT> <out.LAYOUT>", t("inverse", "逆向")),
        cmd(
            "extract-base <DATA.PAK> <out_dir>",
            t("base DATs + collision meshes", "基础 DAT + 碰撞网格"),
        ),
        cmd(
            "unpack-base <out_dir>",
            t("the embedded bundle, no install needed", "内嵌 bundle,不需要游戏安装"),
        ),
        blank(),
        dim(t(
            "Env: TL2_MEDIA_DIR (base MEDIA)  TL2_MOD_GAMEVER (pin header gameVersion)",
            "环境变量:TL2_MEDIA_DIR(基础 MEDIA) TL2_MOD_GAMEVER(固定 header gameVersion)",
        ).into()),
        dim(t(
            "     MIKURO_TIMING=1 (phase timings)  TL2_MODPACK_LIST=1 (dump discovery)",
            "     MIKURO_TIMING=1(分阶段计时) TL2_MODPACK_LIST=1(打印发现结果)",
        ).into()),
        dim(t(
            "     TL2_LANG=en|zh (force this UI's language)",
            "     TL2_LANG=en|zh(强制界面语言)",
        ).into()),
    ];
    v.push(blank());
    v.push(dim(t(
        "↑↓ / PageUp / PageDown to scroll this page.",
        "↑↓ / PageUp / PageDown 滚动本页。",
    ).into()));
    v
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let lines = help_lines(app);
    let total = lines.len() as u16;
    let inner_h = area.height.saturating_sub(2);
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = app.help_scroll.min(max_scroll);
    let title = if max_scroll > 0 {
        format!(" Help · {}/{} ", scroll, max_scroll)
    } else {
        " Help ".to_string()
    };
    let p = Paragraph::new(lines)
        .block(panel(
            Span::styled(title, Style::default().fg(theme::MUTED)),
            theme::LINE,
        ))
        .style(Style::default().bg(theme::PANEL))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let key = |s: &'static str| {
        Span::styled(s, Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD))
    };
    let sep = |s: &'static str| Span::styled(s, Style::default().fg(theme::MUTED));
    // Wider gaps between key/label pairs than between a key and its own label,
    // so the eye groups them correctly instead of reading one long run.
    let t = |en, zh| app.t(en, zh);
    let quit = if app.is_packing() {
        t(" (running…)", " (运行中…)")
    } else {
        t(" quit", " 退出")
    };
    let hint = if app.tab == TAB_HELP {
        Line::from(vec![
            key("↑↓"),
            sep(t(" scroll    ", " 滚动    ")),
            key("Home"),
            sep(t(" top    ", " 回顶    ")),
            key("Tab"),
            sep(t(" switch    ", " 切换    ")),
            key("1-4"),
            sep(t(" jump    ", " 直达    ")),
            key("q"),
            sep(quit),
        ])
    } else if app.tab == TAB_OUTPUT {
        Line::from(vec![
            key("↑↓"),
            sep(t(" scroll    ", " 滚动    ")),
            key("End"),
            sep(t(" follow    ", " 跟随    ")),
            key("c"),
            sep(t(" clear    ", " 清空    ")),
            key("Tab"),
            sep(t(" switch    ", " 切换    ")),
            key("1-4"),
            sep(t(" jump    ", " 直达    ")),
            key("q"),
            sep(quit),
        ])
    } else {
        let run = if app.tab == TAB_UNPACK {
            t(" unpack    ", " 解包    ")
        } else {
            t(" pack    ", " 打包    ")
        };
        Line::from(vec![
            key("↑↓"),
            sep(t(" move    ", " 移动    ")),
            key("Space"),
            sep(t(" select    ", " 选择    ")),
            key("a"),
            sep(t(" all    ", " 全选    ")),
            key("Enter"),
            sep(run),
            key("Tab"),
            sep(t(" switch    ", " 切换    ")),
            key("?"),
            sep(t(" help    ", " 帮助    ")),
            key("q"),
            sep(quit),
        ])
    };
    let p = Paragraph::new(hint)
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

        // No loose MEDIA and no DATA.PAK → the always-embedded bundle tier.
        assert!(matches!(detect_base(&root), BaseSource::Bundled));
        // A loose MEDIA folder wins.
        std::fs::create_dir_all(root.join("MEDIA")).unwrap();
        assert!(matches!(detect_base(&root), BaseSource::LooseMedia(_)));

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
        let mut app = App::new(packs, unpacks, BaseSource::Bundled, PathBuf::from("/x"));
        // Pin the language: `App::new` detects it from the OS, so without this
        // the assertions below would pass or fail depending on whose Windows
        // ran them. The Chinese frames get their own test.
        app.lang = Lang::En;
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
        assert!(text.contains("Mikuro Torchlight II Mod Packer"), "header brand");
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
        let mut app = App::new(packs, unpacks, BaseSource::Bundled, PathBuf::from("/x"));
        app.lang = Lang::Zh;

        let mut terminal = Terminal::new(TestBackend::new(96, 18)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let zh = buffer_text(terminal.backend().buffer());
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

        // Help: every line has content in BOTH languages, and the command
        // column is identical in both (you type those, they are not prose).
        app.lang = Lang::En;
        let en_lines = help_lines(&app);
        app.lang = Lang::Zh;
        let zh_lines = help_lines(&app);
        assert_eq!(en_lines.len(), zh_lines.len(), "help line counts must match");
        let flat = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();
        for (i, (e, z)) in en_lines.iter().zip(&zh_lines).enumerate() {
            let (e, z) = (flat(e), flat(z));
            assert_eq!(
                e.trim().is_empty(),
                z.trim().is_empty(),
                "help line {i} is blank in only one language: {e:?} / {z:?}"
            );
        }
        let zh_help = zh_lines.iter().map(flat).collect::<Vec<_>>().join("\n");
        for c in ["unpack <in.MOD|DATA.PAK> <out_dir>", "compile-dat", "decompile-layout",
                  "--mpp re|dll|none", "--no-times", "TL2_LANG=en|zh"] {
            assert!(zh_help.contains(c), "command/flag {c:?} must stay verbatim in Chinese");
        }
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
                          ("en", Lang::En), ("en-US", Lang::En)] {
            std::env::set_var("TL2_LANG", v);
            assert_eq!(detect_lang(), want, "TL2_LANG={v}");
        }
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
        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::Bundled, root.clone());
        app.lang = Lang::En;
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
        for t in ["loose MEDIA", "DATA.PAK", "baked into this exe", "bundled base (embedded)"] {
            assert!(text.contains(t), "base tier {t:?} not explained");
        }
        // The distinction is stated by its actual test (MEDIA/LAYOUTS).
        assert!(text.contains("MEDIA/LAYOUTS") && text.contains("--mpp re"));
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

    /// An empty picker must say what it looked for instead of showing a blank
    /// box — the unpack tab in particular is empty for anyone who has not built
    /// a mod yet, and the three scanned locations are the useful answer.
    #[test]
    fn empty_picker_explains_itself() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(Vec::new(), Vec::new(), BaseSource::Bundled, PathBuf::from("/x"));
        app.lang = Lang::En;
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
        let mut app = App::new(packs, Vec::new(), BaseSource::Bundled, PathBuf::from("/x"));
        app.lang = Lang::En;
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
