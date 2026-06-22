//! The Activity-view TUI — a terminal recreation of the desktop app's main Activity view.
//!
//! Opens a workspace (saved via `--workspace`, or ad-hoc over one or more folders + `--kb`) and
//! renders a live **recent-activity** feed plus an index status line, driven by the same
//! `looper-core` event bus the desktop UI subscribes to. Events are drained non-blocking each tick.
//!
//! Focus model: **Tab** cycles `feed` (free scroll) → `list` (select rows with ↑/↓) → back. In the
//! list, **Enter** opens the selected document in a markdown **viewer** (a small built-in styler).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use looper_ipc::EngineEvent;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::broadcast::error::TryRecvError;

use crate::session::open;
use crate::SourceArgs;

/// Cap on retained activity rows (newest kept).
const MAX_ROWS: usize = 1000;

/// Which pane has focus / what the arrow keys do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Live feed; ↑/↓ free-scroll.
    Feed,
    /// Browse rows; ↑/↓ move the selection, Enter opens the doc.
    List,
    /// Markdown viewer for the selected doc; ↑/↓ scroll.
    Viewer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Scanning,
    Watching,
    Stopped,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Scanning => "Scanning",
            Status::Watching => "Watching",
            Status::Stopped => "Stopped",
        }
    }

    fn color(self) -> Color {
        match self {
            Status::Scanning => Color::Yellow,
            Status::Watching => Color::Green,
            Status::Stopped => Color::Red,
        }
    }
}

/// One activity-feed row.
struct Activity {
    time: String,
    kind: &'static str,
    title: String,
    detail: String,
    /// Absolute source path, when this row is an openable document (indexed/changed); else `None`.
    path: Option<PathBuf>,
}

impl Activity {
    fn color(&self) -> Color {
        match self.kind {
            "indexed" => Color::Green,
            "changed" => Color::Yellow,
            "removed" => Color::Red,
            "warn" => Color::Magenta,
            _ => Color::Gray,
        }
    }
}

/// TUI state.
struct App {
    label: String,
    folders: Vec<PathBuf>,
    kb_dir: PathBuf,
    status: Status,
    indexed: u32,
    removed: u32,
    doc_count: usize,
    rows: Vec<Activity>,
    focus: Focus,
    scroll: usize,   // feed free-scroll offset
    selected: usize, // list selection index
    view_title: String,
    view_body: String,
    view_scroll: u16,
}

impl App {
    fn new(label: &str, kb_dir: &Path, folders: &[PathBuf]) -> Self {
        Self {
            label: label.to_string(),
            folders: folders.to_vec(),
            kb_dir: kb_dir.to_path_buf(),
            status: Status::Scanning,
            indexed: 0,
            removed: 0,
            doc_count: 0,
            rows: Vec::new(),
            focus: Focus::Feed,
            scroll: 0,
            selected: 0,
            view_title: String::new(),
            view_body: String::new(),
            view_scroll: 0,
        }
    }

    /// Fold one engine event (belonging to `wid`) into the state.
    fn apply(&mut self, event: EngineEvent, wid: &str, doc_count: usize) {
        self.doc_count = doc_count;
        match event {
            EngineEvent::ScanStarted { workspace_id, .. } if workspace_id == wid => {
                self.status = Status::Scanning;
            }
            EngineEvent::ScanCompleted {
                workspace_id,
                indexed,
                removed,
            } if workspace_id == wid => {
                self.status = Status::Watching;
                self.indexed = indexed;
                self.removed = removed;
            }
            EngineEvent::DocIndexed {
                workspace_id,
                title,
                path,
                ..
            } if workspace_id == wid => {
                let detail = self.rel(&path);
                self.push("indexed", title, detail, Some(PathBuf::from(path)));
            }
            EngineEvent::DocRemoved { workspace_id, path } if workspace_id == wid => {
                let detail = self.rel(&path);
                self.push("removed", String::new(), detail, None);
            }
            EngineEvent::FileChanged {
                workspace_id,
                path,
                kind,
            } if workspace_id == wid => {
                let detail = self.rel(&path);
                self.push(
                    "changed",
                    format!("{kind:?}"),
                    detail,
                    Some(PathBuf::from(path)),
                );
            }
            EngineEvent::Warning {
                workspace_id,
                message,
            } if workspace_id == wid => {
                self.push("warn", String::new(), message, None);
            }
            _ => {}
        }
    }

    fn push(&mut self, kind: &'static str, title: String, detail: String, path: Option<PathBuf>) {
        self.rows.insert(
            0,
            Activity {
                time: clock(),
                kind,
                title,
                detail,
                path,
            },
        );
        self.rows.truncate(MAX_ROWS);
        // Keep the feed pinned to where the user scrolled, and (in list focus) keep the highlighted
        // item under the cursor as newer rows push in at the top.
        if self.scroll > 0 {
            self.scroll = (self.scroll + 1).min(self.rows.len().saturating_sub(1));
        }
        if self.focus == Focus::List {
            self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
        }
    }

    /// Display `path` relative to whichever source folder contains it (longest match), else as-is.
    fn rel(&self, path: &str) -> String {
        let p = Path::new(path);
        let mut best: Option<&Path> = None;
        for f in &self.folders {
            let f = f.as_path();
            if p.starts_with(f) && best.is_none_or(|b| f.as_os_str().len() > b.as_os_str().len()) {
                best = Some(f);
            }
        }
        match best {
            Some(f) => p
                .strip_prefix(f)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| path.to_string()),
            None => path.to_string(),
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Feed => {
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
                Focus::List
            }
            Focus::List => Focus::Feed,
            Focus::Viewer => Focus::List,
        };
    }

    fn select_next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1).min(self.rows.len() - 1);
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Open the selected row's document in the markdown viewer (no-op for non-document rows).
    fn open_selected(&mut self) {
        let Some(path) = self.rows.get(self.selected).and_then(|r| r.path.clone()) else {
            return;
        };
        self.view_title = path.display().to_string();
        self.view_body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(err) => format!("# Cannot open\n\n`{}`\n\n{err}\n", path.display()),
        };
        self.view_scroll = 0;
        self.focus = Focus::Viewer;
    }

    fn render(&self, frame: &mut Frame) {
        match self.focus {
            Focus::Viewer => self.render_viewer(frame),
            _ => self.render_dashboard(frame),
        }
    }

    fn render_dashboard(&self, frame: &mut Frame) {
        let [title, status, body, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                " Looper — Activity "
                    .bold()
                    .bg(Color::Blue)
                    .fg(Color::White),
                Span::raw(format!("  {}", self.label)).bold(),
                Span::raw(format!(
                    "  ({} folder{})",
                    self.folders.len(),
                    plural(self.folders.len())
                ))
                .dim(),
            ])),
            title,
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", self.status.label()),
                    Style::new()
                        .fg(Color::Black)
                        .bg(self.status.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {} docs indexed", self.doc_count)),
                Span::raw(format!("   (scan: +{} / -{})", self.indexed, self.removed)).dim(),
                Span::raw(format!("   index → {}", self.kb_dir.display())).dim(),
            ])),
            status,
        );

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|a| {
                let mut spans = vec![
                    Span::styled(format!("{} ", a.time), Style::new().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:<8}", a.kind),
                        Style::new().fg(a.color()).add_modifier(Modifier::BOLD),
                    ),
                ];
                if !a.title.is_empty() {
                    spans.push(Span::styled(
                        format!("{}  ", a.title),
                        Style::new().add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(Span::styled(a.detail.clone(), Style::new().fg(Color::Gray)));
                ListItem::new(Line::from(spans))
            })
            .collect();

        let browsing = self.focus == Focus::List;
        let title = if browsing {
            format!(" Recent activity ({}) — browsing ", self.rows.len())
        } else {
            format!(" Recent activity ({}) ", self.rows.len())
        };
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

        let mut state = ListState::default();
        if browsing {
            state.select(Some(self.selected.min(self.rows.len().saturating_sub(1))));
        } else {
            *state.offset_mut() = self.scroll;
        }
        frame.render_stateful_widget(list, body, &mut state);

        let help_line = if browsing {
            " ↑/↓ select · Enter open · Tab/Esc back · q quit "
        } else {
            " Tab browse · ↑/↓ scroll · g top · q quit "
        };
        frame.render_widget(Paragraph::new(help_line).dim(), help);
    }

    fn render_viewer(&self, frame: &mut Frame) {
        let [body, help] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
        frame.render_widget(
            Paragraph::new(markdown_text(&self.view_body))
                .block(Block::bordered().title(format!(" {} ", self.view_title)))
                .wrap(Wrap { trim: false })
                .scroll((self.view_scroll, 0)),
            body,
        );
        frame.render_widget(
            Paragraph::new(" ↑/↓ scroll · PgUp/PgDn · Esc back · q quit ").dim(),
            help,
        );
    }
}

/// Run the Activity TUI until the user quits.
pub fn run(source: &SourceArgs) -> Result<()> {
    let (session, mut events) = open(source)?;
    let mut app = App::new(&session.label, &session.kb_dir, &session.folders);

    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut events,
        &session.engine,
        &session.wid,
    );
    ratatui::restore();
    session.engine.shutdown();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    events: &mut tokio::sync::broadcast::Receiver<EngineEvent>,
    engine: &looper_core::Engine,
    wid: &str,
) -> Result<()> {
    loop {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    let count = engine.doc_count(wid);
                    app.apply(event, wid, count);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => {
                    app.status = Status::Stopped;
                    break;
                }
            }
        }

        terminal.draw(|frame| app.render(frame))?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Global quit.
                if matches!(key.code, KeyCode::Char('q'))
                    || (matches!(key.code, KeyCode::Char('c'))
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                if key.code == KeyCode::Tab {
                    app.cycle_focus();
                    continue;
                }
                match app.focus {
                    Focus::Feed => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.scroll = (app.scroll + 1).min(app.rows.len().saturating_sub(1));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.scroll = app.scroll.saturating_sub(1);
                        }
                        KeyCode::Char('g') => app.scroll = 0,
                        _ => {}
                    },
                    Focus::List => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                        KeyCode::Enter => app.open_selected(),
                        KeyCode::Esc => app.focus = Focus::Feed,
                        _ => {}
                    },
                    Focus::Viewer => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.view_scroll = app.view_scroll.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.view_scroll = app.view_scroll.saturating_add(1);
                        }
                        KeyCode::PageUp => app.view_scroll = app.view_scroll.saturating_sub(10),
                        KeyCode::PageDown => app.view_scroll = app.view_scroll.saturating_add(10),
                        KeyCode::Esc => app.focus = Focus::List,
                        _ => {}
                    },
                }
            }
        }
    }
    Ok(())
}

/// A lightweight markdown → styled `Text` renderer for the viewer: headings, fenced/indented code,
/// and blockquotes get colors/emphasis. Owned (`'static`) so it can outlive the source string.
fn markdown_text(body: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    for raw in body.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            lines.push(styled(raw, Style::new().fg(Color::DarkGray)));
            continue;
        }
        if in_code {
            lines.push(styled(raw, Style::new().fg(Color::Cyan)));
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let color = match hashes {
                1 => Color::Magenta,
                2 => Color::Blue,
                _ => Color::Cyan,
            };
            lines.push(styled(
                raw,
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ));
        } else if trimmed.starts_with("> ") {
            lines.push(styled(
                raw,
                Style::new().fg(Color::Green).add_modifier(Modifier::ITALIC),
            ));
        } else {
            lines.push(Line::from(raw.to_string()));
        }
    }
    Text::from(lines)
}

fn styled(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), style))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Current wall-clock time of day (UTC) as `HH:MM:SS`.
fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
