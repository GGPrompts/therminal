//! Panes page — flat list of all panes across sessions with peek preview.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use therminal_protocol::PaneId;
use therminal_protocol::daemon::PaneSummary;

use super::{KeyResult, TuiPage};
use crate::backend::{BackendResponse, DaemonBackend};
use crate::palette::*;

// ---------------------------------------------------------------------------
// Panes page state
// ---------------------------------------------------------------------------

pub struct PanesPage {
    panes: Vec<PaneSummary>,
    table_state: TableState,
    preview_lines: Vec<String>,
    preview_pane_id: Option<PaneId>,
    last_refresh: Instant,
    status_msg: Option<(String, bool)>,
}

impl PanesPage {
    pub fn new() -> Self {
        Self {
            panes: Vec::new(),
            table_state: TableState::default(),
            preview_lines: Vec::new(),
            preview_pane_id: None,
            last_refresh: Instant::now() - std::time::Duration::from_secs(10),
            status_msg: None,
        }
    }

    fn refresh(&mut self, backend: &DaemonBackend) {
        self.panes = match backend.list_panes(None) {
            BackendResponse::Panes { panes } => panes,
            BackendResponse::Error(e) => {
                self.status_msg = Some((format!("panes: {e}"), true));
                return;
            }
            _ => return,
        };

        self.clamp_selection();
        self.update_preview(backend);
        self.status_msg = None;
    }

    fn clamp_selection(&mut self) {
        if self.panes.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected() {
            if i >= self.panes.len() {
                self.table_state.select(Some(self.panes.len() - 1));
            }
        } else {
            self.table_state.select(Some(0));
        }
    }

    fn update_preview(&mut self, backend: &DaemonBackend) {
        let pane_id = match self.table_state.selected().and_then(|i| self.panes.get(i)) {
            Some(p) => p.pane_id,
            None => {
                self.preview_lines = vec!["No pane selected".to_string()];
                self.preview_pane_id = None;
                return;
            }
        };

        // Only re-fetch if the selected pane changed.
        if self.preview_pane_id == Some(pane_id) {
            return;
        }

        self.preview_lines = match backend.capture_pane(pane_id) {
            BackendResponse::PaneCaptured { lines, .. } => lines,
            BackendResponse::Error(e) => vec![format!("capture error: {e}")],
            _ => vec!["unexpected response".to_string()],
        };
        self.preview_pane_id = Some(pane_id);
    }

    fn nav_down(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        let len = self.panes.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state.select(Some((i + 1) % len));
        self.preview_pane_id = None; // force preview refresh
    }

    fn nav_up(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        let len = self.panes.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
        self.preview_pane_id = None;
    }
}

impl TuiPage for PanesPage {
    fn title(&self) -> &str {
        "Panes"
    }

    fn tick(&mut self, backend: &DaemonBackend) {
        if self.last_refresh.elapsed() >= std::time::Duration::from_secs(2) {
            self.refresh(backend);
            self.last_refresh = Instant::now();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40), // pane table
                Constraint::Percentage(60), // preview
            ])
            .margin(1)
            .split(area);

        // -- Pane table --
        let header = Row::new(vec!["", "ID", "Session", "Size", "CWD", "Agent", "Exit"])
            .style(
                Style::default()
                    .fg(ACCENT_COOL)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1);

        let rows: Vec<Row> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let selected = self.table_state.selected() == Some(i);
                let pointer = if selected { "\u{25b8}" } else { " " };
                let row_style = if selected {
                    Style::default().bg(BG_SURFACE).fg(TEXT_BRIGHT)
                } else {
                    Style::default().fg(TEXT)
                };

                let cwd = pane.cwd.as_deref().unwrap_or("-");
                let agent = pane.agent_name.as_deref().unwrap_or("-");
                let exit = pane
                    .last_exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string());

                Row::new(vec![
                    Cell::from(Span::styled(pointer, Style::default().fg(ACCENT_COOL))),
                    Cell::from(format!("{}", pane.pane_id)),
                    Cell::from(format!("{}", pane.session_id)),
                    Cell::from(format!("{}x{}", pane.cols, pane.rows)),
                    Cell::from(cwd.to_string()),
                    Cell::from(Span::styled(
                        agent.to_string(),
                        Style::default().fg(if agent != "-" {
                            ACCENT_WARM
                        } else {
                            TEXT_MUTED
                        }),
                    )),
                    Cell::from(exit),
                ])
                .style(row_style)
            })
            .collect();

        let pane_count = self.panes.len();
        let table = Table::new(
            rows,
            [
                Constraint::Length(2),  // pointer
                Constraint::Length(6),  // pane ID
                Constraint::Length(8),  // session ID
                Constraint::Length(9),  // size
                Constraint::Min(15),    // cwd
                Constraint::Length(10), // agent
                Constraint::Length(5),  // exit
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(format!(" Panes ({pane_count}) "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG)),
        );

        f.render_stateful_widget(table, chunks[0], &mut self.table_state);

        // -- Preview --
        let preview_title = self
            .preview_pane_id
            .map(|id| format!(" Pane #{id} output "))
            .unwrap_or_else(|| " Preview ".to_string());

        let lines: Vec<Line> = self
            .preview_lines
            .iter()
            .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(TEXT))))
            .collect();

        let preview = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(preview_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG)),
        );
        f.render_widget(preview, chunks[1]);

        // Status
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { STATUS_ERROR } else { WARM };
            let footer_area = Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(1),
                width: area.width.saturating_sub(2),
                height: 1,
            };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, footer_area);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> KeyResult {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
            KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
            _ => {}
        }
        KeyResult::NONE
    }

    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        match event.kind {
            MouseEventKind::ScrollDown => self.nav_down(),
            MouseEventKind::ScrollUp => self.nav_up(),
            _ => {}
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_page_title() {
        let page = PanesPage::new();
        assert_eq!(page.title(), "Panes");
    }

    #[test]
    fn panes_page_starts_empty() {
        let page = PanesPage::new();
        assert!(page.panes.is_empty());
    }
}
