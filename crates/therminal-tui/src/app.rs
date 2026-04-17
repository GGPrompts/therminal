//! App state and main event loop.
//!
//! Ported from thermal-desktop's `thc tui` with the conductor-mcp backend
//! replaced by therminal-daemon-client IPC.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    /// Dedupe window for tab-bar click handling (tn-p846): some terminals
    /// deliver `Moved` + `Down` in rapid succession or repeat a click as
    /// the cursor drifts a single cell, which previously flipped tabs on
    /// every hover. We drop a second click on the same tab within 100 ms.
    last_tab_click: Option<(usize, Instant)>,
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
            last_tab_click: None,
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

    /// Drive the active page's tick. Returns `true` if the page did
    /// real work (and the rendered state may have changed), `false` if
    /// the call was a throttled no-op. The run loop uses this to
    /// decide whether to repaint.
    fn tick(&mut self) -> bool {
        // Only tick the active page to avoid blocking the UI thread with
        // IPC calls for invisible pages (each call can block up to 5s if
        // the daemon is unreachable).
        if let Some(page) = self.pages.get_mut(self.active_tab) {
            page.tick(&self.backend)
        } else {
            false
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

/// Maximum redraw cadence when idle. The loop never repaints faster than
/// this — even bursts of input or rapid `tick()` state changes coalesce.
/// 16ms ≈ 60 fps, which is the upper bound; the real cadence is set by
/// the dirty flag and is usually much slower.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(16);

/// Idle redraw interval. With a clean dirty flag and no events, we still
/// repaint at this cadence so that wall-clock-derived state (none today,
/// but reserved for future "X seconds ago" labels) eventually catches up.
/// Anything faster than this is perceived as flicker on most terminals.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for a crossterm event before falling through to a
/// tick. This is the upper bound on input latency; the loop does NOT
/// redraw on every poll wakeup, so this can be short without burning
/// frames.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Launch the TUI dashboard. Blocks until the user quits.
pub fn run(socket_path: PathBuf) -> Result<()> {
    let backend = DaemonBackend::connect(&socket_path)?;

    let _screen = ScreenGuard::enter()?;
    let terminal_backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(terminal_backend)?;

    // Hide the hardware cursor for the lifetime of the TUI. Ratatui's
    // `Terminal::draw` re-positions the cursor on every flush, so a
    // visible cursor visibly jumps between draws and reads as "flicker"
    // even when no cells changed. The Drop impl on `ScreenGuard`
    // re-enables the cursor via `Show` on shutdown (tn-dyo1).
    terminal.hide_cursor()?;

    let mut app = App::new(backend);

    // Initial tick + initial draw.
    app.tick();
    terminal.draw(|f| ui(f, &mut app))?;
    let mut last_draw = Instant::now();
    let mut dirty = false;

    loop {
        // Wait for input. Short timeout because we don't redraw on
        // wakeup unless something actually changed.
        let event_arrived = event::poll(POLL_INTERVAL)?;
        if event_arrived {
            match event::read()? {
                Event::Key(key) => {
                    dirty = true;
                    match key.code {
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
                    }
                }
                Event::Mouse(mouse) => {
                    // Only treat actionable mouse events as dirty —
                    // motion and release don't change rendered state, so
                    // they shouldn't force a redraw. (Mouse drag events
                    // during a button hold can otherwise flood the loop.)
                    let actionable = matches!(
                        mouse.kind,
                        MouseEventKind::Down(_)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                    );
                    if !actionable {
                        continue;
                    }
                    dirty = true;
                    // Tab bar occupies rows 0..3 (top border = row 0, tab
                    // labels = row 1, bottom border = row 2). Only row 1
                    // carries the actual tab text; the border rows used
                    // to trigger hits too, which combined with some
                    // terminals' tendency to re-emit clicks on drift to
                    // produce rapid tab-flipping on hover (tn-p846).
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        && mouse.row == 1
                    {
                        let titles: Vec<&str> = app.pages.iter().map(|p| p.title()).collect();
                        if let Some(idx) = tab_hit_index(&titles, mouse.column) {
                            // Dedupe: ignore a repeated click on the same
                            // tab within 100 ms.
                            let now = Instant::now();
                            let is_dup = matches!(
                                app.last_tab_click,
                                Some((prev_idx, prev_at))
                                    if prev_idx == idx
                                        && now.duration_since(prev_at)
                                            < Duration::from_millis(100)
                            );
                            if !is_dup {
                                app.set_tab(idx);
                            }
                            app.last_tab_click = Some((idx, now));
                        }
                    } else if mouse.row >= 3 {
                        if let Some(page) = app.pages.get_mut(app.active_tab) {
                            page.handle_mouse(mouse);
                        }
                    }
                }
                Event::Resize(_, _) => {
                    // The terminal backend handles the resize, but we
                    // need to redraw at the new size.
                    dirty = true;
                }
                _ => {
                    // FocusGained/FocusLost/Paste — none of these change
                    // anything rendered, ignore.
                }
            }
        }

        // Tick the active page. `tick()` is internally throttled to
        // its own refresh cadence (typically 2s) and returns `true`
        // only when it actually fetched new data. Most calls are
        // sub-microsecond no-ops.
        if app.tick() {
            dirty = true;
        }

        // Draw gating: only redraw if something changed, AND respect
        // the minimum redraw interval. Also force a redraw at the idle
        // cadence so any time-derived state catches up.
        let elapsed = last_draw.elapsed();
        let should_draw =
            (dirty && elapsed >= MIN_REDRAW_INTERVAL) || elapsed >= IDLE_REDRAW_INTERVAL;
        if should_draw {
            terminal.draw(|f| ui(f, &mut app))?;
            last_draw = Instant::now();
            dirty = false;
        }

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

    /// tn-dyo1: the TUI flickered because the run loop repainted on
    /// every iteration — including on mouse-motion events and after
    /// every no-op tick. These invariants guard the redraw gate:
    ///
    /// - Idle redraws at 2 Hz, not 60+ Hz.
    /// - Dirty-flag redraws still respect a minimum interval to
    ///   coalesce input bursts.
    /// - Poll interval is short enough for snappy input but shorter
    ///   than both redraw windows, so wakeups don't force a paint.
    #[test]
    fn redraw_cadence_sane() {
        assert!(
            POLL_INTERVAL < IDLE_REDRAW_INTERVAL,
            "poll must be faster than idle redraws so wakeups don't force frames"
        );
        assert!(
            POLL_INTERVAL >= MIN_REDRAW_INTERVAL,
            "poll should be at least one frame; otherwise we wake hot for no reason"
        );
        assert!(
            MIN_REDRAW_INTERVAL <= Duration::from_millis(33),
            "min redraw interval must stay within ≤ 30 fps to feel responsive"
        );
        assert!(
            IDLE_REDRAW_INTERVAL >= Duration::from_millis(250),
            "idle redraws faster than 4 Hz look like flicker on most terminals"
        );
    }
}
