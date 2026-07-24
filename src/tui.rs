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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use tl2_mikuro_mod_packer_rs::packer::{
    convert_all, pack_mod, read_mod_metadata, tl2_mods_dir, Overrides,
};

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
enum BaseSource {
    /// A loose `MEDIA/` folder next to the exe (already extracted).
    LooseMedia(PathBuf),
    /// `PAKS/DATA.PAK` next to the exe — targeted extraction (Phase 2).
    DataPak(PathBuf),
    /// Embedded base bundle (Phase 3), for running with no game present.
    Bundled,
    /// Nothing found — MPP regeneration is unavailable.
    None,
}

impl BaseSource {
    fn label(&self) -> String {
        match self {
            BaseSource::LooseMedia(p) => format!("loose MEDIA ({})", p.display()),
            BaseSource::DataPak(p) => format!("DATA.PAK ({})", p.display()),
            BaseSource::Bundled => "bundled base".to_string(),
            BaseSource::None => "none — pathing regen unavailable".to_string(),
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
    if cfg!(feature = "bundled-base") {
        return BaseSource::Bundled;
    }
    BaseSource::None
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

    // Selection UI (restores the terminal on the way out).
    let picks = match select_mods(&mods, &base) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("TUI error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if picks.is_empty() {
        println!("Nothing selected.");
        return ExitCode::SUCCESS;
    }

    // Pack each selected mod with the normal console output.
    let mods_out = tl2_mods_dir();
    if let Err(e) = std::fs::create_dir_all(&mods_out) {
        eprintln!("Cannot create output dir {}: {e}", mods_out.display());
        pause();
        return ExitCode::FAILURE;
    }
    println!("Output folder: {}\n", mods_out.display());

    let mut failures = 0;
    for &i in &picks {
        let m = &mods[i];
        println!("═══ Packing: {} ═══", m.title);
        match pack_one(m, &mods_out) {
            Ok((path, size)) => {
                println!("  ✔ {}  ({:.1} MB)\n", path.display(), size as f64 / 1.0e6);
            }
            Err(e) => {
                failures += 1;
                eprintln!("  x FAILED: {e}\n");
            }
        }
    }

    println!(
        "Done — {} packed, {} failed.",
        picks.len() - failures,
        failures
    );
    pause();
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Convert + pack a single mod into `mods_out/<FILE_NAME>.MOD`. Mirrors the
/// in-memory (non-mutating) path of the CLI, with MPP skipped for now — level
/// pathing regen (which needs base data) arrives with Phase 2.
fn pack_one(m: &ModEntry, mods_out: &Path) -> Result<(PathBuf, usize), String> {
    let media_dir = m.dir.join("MEDIA");
    if !media_dir.is_dir() {
        return Err(format!("no MEDIA/ in {}", m.dir.display()));
    }
    let mut ov = Overrides::new();
    // --mpp none: content mods need no base data; keeps any existing .mpp.
    let results = convert_all(&media_dir, Some(&mut ov), "none", "auto");
    for e in results.errors.iter().take(3) {
        eprintln!("    warn: {e}");
    }
    let out_path = mods_out.join(format!("{}.MOD", m.file_name));
    let size = pack_mod(&media_dir, &out_path, Some(&m.dir), Some(&ov))
        .map_err(|e| format!("pack: {e}"))?;
    Ok((out_path, size))
}

// ── ratatui selection screen ───────────────────────────────────────────────

struct App<'a> {
    mods: &'a [ModEntry],
    base: &'a BaseSource,
    picked: Vec<bool>,
    state: ListState,
}

impl<'a> App<'a> {
    fn new(mods: &'a [ModEntry], base: &'a BaseSource) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        App {
            mods,
            base,
            picked: vec![false; mods.len()],
            state,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.mods.len() as isize;
        let cur = self.state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(n);
        self.state.select(Some(next as usize));
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
}

/// Run the picker; returns the chosen mod indices (empty = user quit).
fn select_mods(mods: &[ModEntry], base: &BaseSource) -> std::io::Result<Vec<usize>> {
    let mut terminal = ratatui::init();
    let mut app = App::new(mods, base);
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<Vec<usize>> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Vec::new()),
                KeyCode::Up | KeyCode::Char('k') => app.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => app.move_by(1),
                KeyCode::Char(' ') => app.toggle_current(),
                KeyCode::Char('a') => app.toggle_all(),
                KeyCode::Enter => return Ok(app.resolved()),
                _ => {}
            }
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(4), // header
        Constraint::Min(3),    // list
        Constraint::Length(3), // footer
    ])
    .split(f.area());

    draw_header(f, areas[0], app);
    draw_list(f, areas[1], app);
    draw_footer(f, areas[2]);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let picked = app.picked.iter().filter(|&&b| b).count();
    let lines = vec![
        Line::from(vec![Span::styled(
            "Torchlight II Mod Packer",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::styled("base: ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.base.label()),
            Span::styled("   selected: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{picked}/{}", app.mods.len())),
        ]),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
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

fn draw_footer(f: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::styled("↑↓/jk", Style::default().fg(Color::Cyan)),
        Span::raw(" move   "),
        Span::styled("Space", Style::default().fg(Color::Cyan)),
        Span::raw(" select   "),
        Span::styled("a", Style::default().fg(Color::Cyan)),
        Span::raw(" all   "),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" pack   "),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ]);
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

        // No loose MEDIA and no DATA.PAK next to the root → base None.
        assert!(matches!(detect_base(&root), BaseSource::None));
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
        let base = BaseSource::None;
        let mut app = App::new(&mods, &base);
        app.toggle_current(); // pick Alpha

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Torchlight II Mod Packer"), "header");
        assert!(text.contains("Alpha Content"), "mod row");
        assert!(text.contains("Beta Level"), "mod row");
        assert!(text.contains("(level mod)"), "level tag");
        assert!(text.contains("[x]"), "selected mark");
        assert!(text.contains("selected: 1/2"), "count");
        assert!(text.contains("Enter"), "footer hint");
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
