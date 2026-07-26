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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use tl2_mikuro_mod_packer_rs::pak::PakReader;
use tl2_mikuro_mod_packer_rs::packer::read_mod_metadata;

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
    fn label(&self) -> String {
        match self {
            BaseSource::LooseMedia(p) => format!("loose MEDIA ({})", p.display()),
            BaseSource::DataPak(p) => format!("DATA.PAK ({})", p.display()),
            BaseSource::Bundled => "bundled base (embedded)".to_string(),
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
    })
}

/// A built `.MOD` file the picker can unpack. The title comes from the
/// container header, so the row reads the same as the folder it came from.
fn mod_file_entry_for(path: &Path) -> Option<ModEntry> {
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

/// Discover work under `root`: its immediate subdirs (the "dropped into the
/// challenger-continent folder" case) plus `root/mods/*` (the "dropped into the
/// game folder" case). A subdir with MOD.DAT is a source to PACK; a `.MOD` file
/// is a container to UNPACK. Deduped by path, packs first, then by title.
fn discover_mods(root: &Path) -> Vec<ModEntry> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut scan = |base: &Path| {
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                let p = e.path();
                let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                if !seen.insert(key) {
                    continue;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if let Some(m) = mod_entry_for(&p) {
                        out.push(m);
                    }
                } else if p
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("MOD"))
                    .unwrap_or(false)
                {
                    if let Some(m) = mod_file_entry_for(&p) {
                        out.push(m);
                    }
                }
            }
        }
    };
    scan(root);
    scan(&root.join("mods"));
    out.sort_by(|a, b| {
        (a.action == Action::Unpack, a.title.to_lowercase())
            .cmp(&(b.action == Action::Unpack, b.title.to_lowercase()))
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
    let mods = discover_mods(&root);

    // Non-interactive discovery dump (diagnostics / CI): list what the TUI would
    // show, then exit without touching the terminal.
    if std::env::var_os("TL2_MODPACK_LIST").is_some() {
        println!("root: {}", root.display());
        println!("base: {}", base.label());
        println!("mods: {}", mods.len());
        for m in &mods {
            match m.action {
                Action::Pack => println!(
                    "  - pack   {}  [out={}.MOD]  {}",
                    m.title,
                    m.file_name,
                    if m.has_layouts { "level" } else { "content" }
                ),
                Action::Unpack => println!(
                    "  - unpack {}  [src={}]",
                    m.title,
                    m.dir.display()
                ),
            }
        }
        return ExitCode::SUCCESS;
    }

    if mods.is_empty() {
        println!("Mikuro Torchlight II Mod Packer");
        println!();
        println!("Nothing to pack or unpack under:");
        println!("  {}", root.display());
        println!("  {}", root.join("mods").display());
        println!();
        println!("A mod source is a folder containing MOD.DAT (and a MEDIA/ tree);");
        println!("a built .MOD file next to this exe can be unpacked back into one.");
        println!("Put this exe in your game folder, or in a folder of mod sources.");
        pause();
        return ExitCode::FAILURE;
    }

    // Interactive app: pick mods, pack them in-app (output streams to the
    // Output tab, the app stays open on it when done).
    match run_app(mods, base, root) {
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

struct App {
    mods: Vec<ModEntry>,
    base: BaseSource,
    root: PathBuf,
    picked: Vec<bool>,
    state: ListState,
    tab: usize, // 0 = Mods, 1 = Output
    log: Log,
    packing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    follow: bool, // Output tab auto-scroll to tail
    out_scroll: u16,
}

impl App {
    fn new(mods: Vec<ModEntry>, base: BaseSource, root: PathBuf) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        let n = mods.len();
        App {
            mods,
            base,
            root,
            picked: vec![false; n],
            state,
            tab: 0,
            log: Arc::new(Mutex::new(Vec::new())),
            packing: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            follow: true,
            out_scroll: 0,
        }
    }

    fn is_packing(&self) -> bool {
        self.packing.load(Ordering::Relaxed)
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.mods.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        self.state.select(Some((cur + delta).rem_euclid(n) as usize));
    }

    fn toggle_current(&mut self) {
        if let Some(i) = self.state.selected() {
            self.picked[i] = !self.picked[i];
        }
    }

    fn toggle_all(&mut self) {
        let all_on = self.picked.iter().all(|&b| b);
        for b in &mut self.picked {
            *b = !all_on;
        }
    }

    /// Selected indices; if none toggled, the row under the cursor.
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

    /// Spawn the packing worker and switch to the Output tab.
    fn start_packing(&mut self) {
        let picks = self.resolved();
        if picks.is_empty() {
            return;
        }
        let jobs: Vec<Job> = picks
            .iter()
            .map(|&i| {
                let m = &self.mods[i];
                Job {
                    title: m.title.clone(),
                    file_name: m.file_name.clone(),
                    path: m.dir.clone(),
                    has_layouts: m.has_layouts,
                    action: m.action,
                }
            })
            .collect();
        self.tab = 1;
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

fn run_app(mods: Vec<ModEntry>, base: BaseSource, root: PathBuf) -> std::io::Result<ExitCode> {
    let mut terminal = ratatui::init();
    let mut app = App::new(mods, base, root);
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
            KeyCode::Tab | KeyCode::BackTab => app.tab = 1 - app.tab,
            KeyCode::Char('q') | KeyCode::Esc => {
                // Ignore quit while a pack is running (the child would orphan).
                if !app.is_packing() {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            _ if app.tab == 1 => match k.code {
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
                _ => {}
            },
            _ if !app.is_packing() => match k.code {
                KeyCode::Up | KeyCode::Char('k') => app.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => app.move_by(1),
                KeyCode::Char(' ') => app.toggle_current(),
                KeyCode::Char('a') => app.toggle_all(),
                KeyCode::Enter => app.start_packing(),
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
    // (editorial whitespace) instead of bleeding to the terminal edge.
    let areas = Layout::vertical([
        Constraint::Length(3), // header / tabs
        Constraint::Min(3),    // body (mods list or output)
        Constraint::Length(3), // footer
    ])
    .margin(1)
    .split(f.area());

    draw_header(f, areas[0], app);
    match app.tab {
        1 => draw_output(f, areas[1], app),
        _ => draw_list(f, areas[1], app),
    }
    draw_footer(f, areas[2], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let tab = |on: bool, label: &'static str| {
        let style = if on {
            Style::default().fg(theme::BG).bg(theme::EMBER).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FAINT)
        };
        Span::styled(format!(" {label} "), style)
    };
    let picked = app.picked.iter().filter(|&&b| b).count();
    let (status, status_style) = if app.is_packing() {
        ("packing…".to_string(), Style::default().fg(theme::GOLD).add_modifier(Modifier::BOLD))
    } else if app.finished.load(Ordering::Relaxed) {
        ("done".to_string(), Style::default().fg(theme::SET).add_modifier(Modifier::BOLD))
    } else {
        (format!("selected: {picked}/{}", app.mods.len()), Style::default().fg(theme::EMBER))
    };
    let line = Line::from(vec![
        tab(app.tab == 0, "Mods"),
        Span::raw(" "),
        tab(app.tab == 1, "Output"),
        Span::styled("    base ", Style::default().fg(theme::MUTED)),
        Span::styled(app.base.label(), Style::default().fg(theme::GOLD)),
        Span::raw("    "),
        Span::styled(status, status_style),
    ]);
    let p = Paragraph::new(line)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::LINE_HI))
                .style(Style::default().bg(theme::PANEL))
                .title(Span::styled(
                    " Mikuro Torchlight II Mod Packer ",
                    Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = if app.picked[i] { "[x] " } else { "[ ] " };
            let tag = match (m.action, m.has_layouts) {
                (Action::Unpack, _) => {
                    Span::styled("  (.MOD → unpack)", Style::default().fg(theme::GOLD))
                }
                (Action::Pack, true) => {
                    Span::styled("  (level mod)", Style::default().fg(theme::MAGIC))
                }
                (Action::Pack, false) => {
                    Span::styled("  (content mod)", Style::default().fg(theme::FAINT))
                }
            };
            let mark_style = if app.picked[i] {
                Style::default().fg(theme::SET).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FAINT)
            };
            let title_style = if app.picked[i] {
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::LINE))
                .style(Style::default().bg(theme::PANEL))
                .title(Span::styled(
                    " mods · Space select · Enter pack/unpack ",
                    Style::default().fg(theme::MUTED),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(theme::LINE_HI)
                .fg(theme::EMBER_HI)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = app.state.clone();
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
    let (title, tstyle) = if app.follow {
        (" Output · following ".to_string(), Style::default().fg(theme::SET))
    } else {
        (
            format!(" Output · {}/{} ", scroll.min(total), total),
            Style::default().fg(theme::MUTED),
        )
    };
    let p = Paragraph::new(log.join("\n"))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::LINE))
                .style(Style::default().bg(theme::PANEL))
                .title(Span::styled(title, tstyle)),
        )
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let key = |s: &'static str| {
        Span::styled(s, Style::default().fg(theme::EMBER).add_modifier(Modifier::BOLD))
    };
    let sep = |s: &'static str| Span::styled(s, Style::default().fg(theme::MUTED));
    let hint = if app.tab == 1 {
        Line::from(vec![
            key("↑↓"),
            sep(" scroll   "),
            key("End"),
            sep(" follow   "),
            key("Tab"),
            sep(" mods   "),
            key("q"),
            sep(if app.is_packing() { " (packing…)" } else { " quit" }),
        ])
    } else {
        Line::from(vec![
            key("↑↓"),
            sep(" move   "),
            key("Space"),
            sep(" select   "),
            key("a"),
            sep(" all   "),
            key("Enter"),
            sep(" run   "),
            key("Tab"),
            sep(" output   "),
            key("q"),
            sep(" quit"),
        ])
    };
    let p = Paragraph::new(hint)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::LINE))
                .style(Style::default().bg(theme::PANEL)),
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

        // A built .MOD is discovered too — as an UNPACK row, listed after the
        // pack sources. (This one is not a readable container, so the title
        // falls back to the file stem; discovery must still offer it.)
        std::fs::write(root.join("DeltaBuilt.MOD"), b"not really a container").unwrap();

        let mods = discover_mods(&root);
        let titles: Vec<&str> = mods.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["AlphaContent", "BetaLevel", "GammaNested", "DeltaBuilt"],
            "packs first (by title), then unpacks"
        );
        let delta = mods.iter().find(|m| m.title == "DeltaBuilt").unwrap();
        assert_eq!(delta.action, Action::Unpack);
        assert!(mods.iter().filter(|m| m.action == Action::Pack).count() == 3);
        let beta = mods.iter().find(|m| m.title == "BetaLevel").unwrap();
        assert!(beta.has_layouts, "BetaLevel has a *.LAYOUT");
        let alpha = mods.iter().find(|m| m.title == "AlphaContent").unwrap();
        assert!(!alpha.has_layouts, "AlphaContent has no layouts");

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

        let mods = vec![
            ModEntry {
                title: "Alpha Content".into(),
                file_name: "ALPHA".into(),
                dir: PathBuf::from("/x/Alpha"),
                has_layouts: false,
                action: Action::Pack,
            },
            ModEntry {
                title: "Beta Level".into(),
                file_name: "BETA".into(),
                dir: PathBuf::from("/x/Beta"),
                has_layouts: true,
                action: Action::Pack,
            },
            ModEntry {
                title: "Gamma Built".into(),
                file_name: "GAMMA".into(),
                dir: PathBuf::from("/x/GAMMA.MOD"),
                has_layouts: false,
                action: Action::Unpack,
            },
        ];
        let mut app = App::new(mods, BaseSource::Bundled, PathBuf::from("/x"));
        app.toggle_current(); // pick Alpha (row under cursor)

        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        // Mods tab.
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Mikuro Torchlight II Mod Packer"), "header brand");
        assert!(text.contains("Mods") && text.contains("Output"), "tab bar");
        assert!(text.contains("Alpha Content"), "mod row");
        assert!(text.contains("Beta Level"), "mod row");
        assert!(text.contains("(level mod)"), "level tag");
        assert!(text.contains("Gamma Built") && text.contains("unpack"), "unpack row + tag");
        assert!(text.contains("[x]"), "selected mark");
        assert!(text.contains("selected: 1/3"), "count");
        assert!(text.contains("Enter"), "footer hint");

        // Output tab renders the (empty) log pane.
        app.tab = 1;
        logln(&app.log, "hello from worker");
        terminal.draw(|f| draw(f, &app)).unwrap();
        let out = buffer_text(terminal.backend().buffer());
        assert!(out.contains("Output"), "output tab title");
        assert!(out.contains("hello from worker"), "log line rendered");
    }

    /// Regression guard: the rendered frame actually uses the Emberworks palette
    /// (not just that it compiles) — furnace-black frame, panel fill, ember accent
    /// and the ember-backed active tab all show up in the buffer's styles.
    #[test]
    fn renders_with_emberworks_palette() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mods = vec![ModEntry {
            title: "Alpha".into(),
            file_name: "A".into(),
            dir: PathBuf::from("/x/A"),
            has_layouts: false,
            action: Action::Pack,
        }];
        let app = App::new(mods, BaseSource::Bundled, PathBuf::from("/x"));
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
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

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
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
