//! App state and main event loop.
//!
//! Ported from thermal-desktop's `thc tui` with the conductor-mcp backend
//! replaced by therminal-daemon-client IPC.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Tabs},
};

use crate::backend::DaemonBackend;
use crate::pages::TuiPage;
use crate::pages::agents::AgentsPage;
use crate::pages::panes::PanesPage;
use crate::pages::sessions::SessionsPage;
use crate::palette::*;

// ---------------------------------------------------------------------------
// Tab bar helpers
// ---------------------------------------------------------------------------

const TAB_DIVIDER: &str = " | ";
const TAB_PADDING_LEFT: &str = " ";
const TAB_PADDING_RIGHT: &str = " ";

fn tab_title_line(index: usize, title: &str) -> Line<'static> {
    let num = format!("{}", index + 1);
    Line::from(vec![
        Span::styled(
            num,
            Style::default()
                .fg(ACCENT_COOL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(":", Style::default().fg(TEXT_MUTED)),
        Span::styled(title.to_owned(), Style::default().fg(TEXT_BRIGHT)),
    ])
}

fn tab_hit_index(titles: &[&str], column: u16) -> Option<usize> {
    let left_padding = Line::from(TAB_PADDING_LEFT).width() as u16;
    let right_padding = Line::from(TAB_PADDING_RIGHT).width() as u16;
    let divider_width = Span::raw(TAB_DIVIDER).width() as u16;
    let mut x = 0u16;

    for (i, title) in titles.iter().enumerate() {
        let title_width = tab_title_line(i, title).width() as u16;
        let tab_width = left_padding + title_width + right_padding;
        if column >= x && column < x + tab_width {
            return Some(i);
        }
        x += tab_width;
        if i + 1 < titles.len() {
            x += divider_width;
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Screen guard (RAII raw mode + alternate screen)
// ---------------------------------------------------------------------------

struct ScreenGuard {
    _active: bool,
}

impl ScreenGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(Self { _active: true })
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture, Show);
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    backend: DaemonBackend,
    pages: Vec<Box<dyn TuiPage>>,
    active_tab: usize,
    should_quit: bool,
    /// Connection status displayed in the tab bar title.
    daemon_status: String,
}

impl App {
    fn new(backend: DaemonBackend) -> Self {
        // Probe daemon on startup.
        let daemon_status = match backend.ping() {
            crate::backend::BackendResponse::Pong {
                version,
                uptime_secs,
                sessions,
                ..
            } => format!("v{version} up:{uptime_secs}s sessions:{sessions}"),
            crate::backend::BackendResponse::Error(e) => format!("error: {e}"),
            _ => "unknown".to_string(),
        };

        let pages: Vec<Box<dyn TuiPage>> = vec![
            Box::new(SessionsPage::new()),
            Box::new(PanesPage::new()),
            Box::new(AgentsPage::new()),
        ];

        Self {
            backend,
            pages,
            active_tab: 0,
            should_quit: false,
            daemon_status,
        }
    }

    fn set_tab(&mut self, idx: usize) {
        if idx < self.pages.len() {
            self.active_tab = idx;
        }
    }

    fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.pages.len();
    }

    fn prev_tab(&mut self) {
        if self.active_tab == 0 {
            self.active_tab = self.pages.len() - 1;
        } else {
            self.active_tab -= 1;
        }
    }

    fn tick(&mut self) {
        // Only tick the active page to avoid blocking the UI thread with
        // IPC calls for invisible pages (each call can block up to 5s if
        // the daemon is unreachable).
        if let Some(page) = self.pages.get_mut(self.active_tab) {
            page.tick(&self.backend);
        }
    }

    fn is_text_input(&self) -> bool {
        self.pages
            .get(self.active_tab)
            .is_some_and(|p| p.has_text_focus())
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(5),    // page content
        ])
        .split(f.area());

    // Background.
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    // -- Tab bar --
    let titles: Vec<Line> = app
        .pages
        .iter()
        .enumerate()
        .map(|(i, page)| tab_title_line(i, page.title()))
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab)
        .style(Style::default().fg(TEXT_MUTED).bg(BG_SURFACE))
        .highlight_style(
            Style::default()
                .fg(TEXT_BRIGHT)
                .bg(BG)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(Span::styled(TAB_DIVIDER, Style::default().fg(TEXT_MUTED)))
        .padding(TAB_PADDING_LEFT, TAB_PADDING_RIGHT)
        .block(
            Block::default()
                .title(format!(" THERMINAL [{}] ", app.daemon_status))
                .title_alignment(Alignment::Center)
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG_SURFACE)),
        );
    f.render_widget(tabs, chunks[0]);

    // -- Active page --
    if let Some(page) = app.pages.get_mut(app.active_tab) {
        page.render(f, chunks[1]);
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Launch the TUI dashboard. Blocks until the user quits.
pub fn run(socket_path: PathBuf) -> Result<()> {
    let backend = DaemonBackend::connect(&socket_path)?;

    let _screen = ScreenGuard::enter()?;
    let terminal_backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(terminal_backend)?;

    let mut app = App::new(backend);

    // Initial tick.
    app.tick();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') if !app.is_text_input() => {
                        app.should_quit = true;
                    }
                    KeyCode::Char('1') if !app.is_text_input() => app.set_tab(0),
                    KeyCode::Char('2') if !app.is_text_input() => app.set_tab(1),
                    KeyCode::Char('3') if !app.is_text_input() => app.set_tab(2),
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        app.should_quit = true;
                    }
                    KeyCode::Char('n')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        app.next_tab();
                    }
                    KeyCode::Char('p')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        app.prev_tab();
                    }
                    KeyCode::BackTab if !app.is_text_input() => {
                        app.prev_tab();
                    }
                    _ => {
                        if let Some(page) = app.pages.get_mut(app.active_tab) {
                            let result = page.handle_key(key);
                            if result.quit {
                                app.should_quit = true;
                            }
                        }
                    }
                },
                Event::Mouse(mouse) => {
                    // Tab bar occupies rows 0..3.
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        && mouse.row < 3
                    {
                        let titles: Vec<&str> = app.pages.iter().map(|p| p.title()).collect();
                        if let Some(idx) = tab_hit_index(&titles, mouse.column) {
                            app.set_tab(idx);
                        }
                    } else if mouse.row >= 3 {
                        if let Some(page) = app.pages.get_mut(app.active_tab) {
                            page.handle_mouse(mouse);
                        }
                    }
                }
                _ => {}
            }
        }

        app.tick();

        if app.should_quit {
            break;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_hit_testing() {
        let titles = ["Sessions", "Panes", "Agents"];
        // First tab starts at column 0.
        assert_eq!(tab_hit_index(&titles, 0), Some(0));
        // Way past all tabs.
        assert_eq!(tab_hit_index(&titles, 200), None);
    }

    #[test]
    fn tab_title_line_format() {
        let line = tab_title_line(0, "Sessions");
        assert_eq!(line.spans.len(), 3);
    }
}
