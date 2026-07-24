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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use tl2_mikuro_mod_packer_rs::pak::PakReader;
use tl2_mikuro_mod_packer_rs::packer::read_mod_metadata;

/// Shared, worker-appended log rendered live in the Output tab.
type Log = Arc<Mutex<Vec<String>>>;

fn logln(log: &Log, msg: impl Into<String>) {
    if let Ok(mut v) = log.lock() {
        v.push(msg.into());
    }
}

/// A packable mod source: a folder with MOD.DAT (and a MEDIA/ tree).
struct ModEntry {
    /// Human title (MOD.DAT DISPLAYNAME/MOD_TITLE, else folder name).
    title: String,
    /// Output base name (MOD.DAT MOD_FILE_NAME, else folder name).
    file_name: String,
    /// The mod source directory (holds MOD.DAT + MEDIA/).
    dir: PathBuf,
    /// True when MEDIA/LAYOUTS holds at least one *.LAYOUT — a level mod whose
    /// pathing (MPP) would want base collision geometry to regenerate.
    has_layouts: bool,
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

/// Discover packable mods under `root`: its immediate subdirs (the "dropped
/// into the challenger-continent folder" case) plus `root/mods/*` (the "dropped
/// into the game folder" case). Deduped by path, sorted by title.
fn discover_mods(root: &Path) -> Vec<ModEntry> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut scan = |base: &Path| {
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                let p = e.path();
                if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                if !seen.insert(key) {
                    continue;
                }
                if let Some(m) = mod_entry_for(&p) {
                    out.push(m);
                }
            }
        }
    };
    scan(root);
    scan(&root.join("mods"));
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
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
            println!(
                "  - {}  [out={}.MOD]  {}",
                m.title,
                m.file_name,
                if m.has_layouts { "level" } else { "content" }
            );
        }
        return ExitCode::SUCCESS;
    }

    if mods.is_empty() {
        println!("Torchlight II Mod Packer");
        println!();
        println!("No packable mods found under:");
        println!("  {}", root.display());
        println!("  {}", root.join("mods").display());
        println!();
        println!("A mod source is a folder containing MOD.DAT (and a MEDIA/ tree).");
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
fn pack_worker(jobs: Vec<(String, PathBuf, bool)>, base: BaseSource, root: PathBuf, log: Log) {
    let exe = std::env::current_exe().unwrap_or_default();
    let any_level = jobs.iter().any(|(_, _, lvl)| *lvl);
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

    let mut ok = 0;
    let mut fail = 0;
    for (title, dir, has_layouts) in &jobs {
        let mpp = if *has_layouts && base_media.is_some() {
            "re"
        } else {
            "none"
        };
        logln(&log, format!("═══════ {title}  (mpp={mpp}) ═══════"));
        match run_child(&exe, dir, mpp, base_media.as_deref(), &log) {
            true => ok += 1,
            false => fail += 1,
        }
        logln(&log, "");
    }
    logln(&log, format!("── Done: {ok} packed, {fail} failed.  [q] to quit ──"));
}

/// Pack one mod via a child `<exe> <dir> --deploy --mpp <mpp>`, streaming its
/// stdout+stderr into the log. Returns whether it succeeded.
fn run_child(exe: &Path, dir: &Path, mpp: &str, base_media: Option<&Path>, log: &Log) -> bool {
    let mut cmd = Command::new(exe);
    cmd.arg(dir)
        .arg("--deploy")
        .arg("--mpp")
        .arg(mpp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
        let jobs: Vec<(String, PathBuf, bool)> = picks
            .iter()
            .map(|&i| {
                let m = &self.mods[i];
                (m.title.clone(), m.dir.clone(), m.has_layouts)
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
    let areas = Layout::vertical([
        Constraint::Length(3), // header / tabs
        Constraint::Min(3),    // body (mods list or output)
        Constraint::Length(3), // footer
    ])
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
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Span::styled(format!(" {label} "), style)
    };
    let picked = app.picked.iter().filter(|&&b| b).count();
    let status = if app.is_packing() {
        "packing…".to_string()
    } else if app.finished.load(Ordering::Relaxed) {
        "done".to_string()
    } else {
        format!("selected: {picked}/{}", app.mods.len())
    };
    let line = Line::from(vec![
        tab(app.tab == 0, "Mods"),
        Span::raw(" "),
        tab(app.tab == 1, "Output"),
        Span::styled("    base: ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.base.label()),
        Span::styled("    ", Style::default()),
        Span::styled(status, Style::default().fg(Color::Cyan)),
    ]);
    let p = Paragraph::new(line)
        .block(Block::default().borders(Borders::ALL).title(" Torchlight II Mod Packer "))
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
            let tag = if m.has_layouts {
                Span::styled("  (level mod)", Style::default().fg(Color::Magenta))
            } else {
                Span::styled("  (content mod)", Style::default().fg(Color::DarkGray))
            };
            let mark_style = if app.picked[i] {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, mark_style),
                Span::raw(m.title.clone()),
                tag,
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" mods (Space to select, Enter to pack) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
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
    let title = if app.follow {
        " Output (following) ".to_string()
    } else {
        format!(" Output ({}/{}) ", scroll.min(total), total)
    };
    let p = Paragraph::new(log.join("\n"))
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let key = |s: &'static str| Span::styled(s, Style::default().fg(Color::Cyan));
    let hint = if app.tab == 1 {
        Line::from(vec![
            key("↑↓"),
            Span::raw(" scroll   "),
            key("End"),
            Span::raw(" follow   "),
            key("Tab"),
            Span::raw(" mods   "),
            key("q"),
            Span::raw(if app.is_packing() { " (packing…)" } else { " quit" }),
        ])
    } else {
        Line::from(vec![
            key("↑↓"),
            Span::raw(" move   "),
            key("Space"),
            Span::raw(" select   "),
            key("a"),
            Span::raw(" all   "),
            key("Enter"),
            Span::raw(" pack   "),
            key("Tab"),
            Span::raw(" output   "),
            key("q"),
            Span::raw(" quit"),
        ])
    };
    let p = Paragraph::new(hint).block(Block::default().borders(Borders::ALL));
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

        let mods = discover_mods(&root);
        let titles: Vec<&str> = mods.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["AlphaContent", "BetaLevel", "GammaNested"]);
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
            },
            ModEntry {
                title: "Beta Level".into(),
                file_name: "BETA".into(),
                dir: PathBuf::from("/x/Beta"),
                has_layouts: true,
            },
        ];
        let mut app = App::new(mods, BaseSource::Bundled, PathBuf::from("/x"));
        app.toggle_current(); // pick Alpha (row under cursor)

        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        // Mods tab.
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Torchlight II Mod Packer"), "header");
        assert!(text.contains("Mods") && text.contains("Output"), "tab bar");
        assert!(text.contains("Alpha Content"), "mod row");
        assert!(text.contains("Beta Level"), "mod row");
        assert!(text.contains("(level mod)"), "level tag");
        assert!(text.contains("[x]"), "selected mark");
        assert!(text.contains("selected: 1/2"), "count");
        assert!(text.contains("Enter"), "footer hint");

        // Output tab renders the (empty) log pane.
        app.tab = 1;
        logln(&app.log, "hello from worker");
        terminal.draw(|f| draw(f, &app)).unwrap();
        let out = buffer_text(terminal.backend().buffer());
        assert!(out.contains("Output"), "output tab title");
        assert!(out.contains("hello from worker"), "log line rendered");
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
