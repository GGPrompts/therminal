//! Agents page — live agent status across all panes.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use therminal_protocol::daemon::AgentSummary;

use super::{KeyResult, TuiPage};
use crate::backend::{BackendResponse, DaemonBackend};
use crate::palette::*;

// ---------------------------------------------------------------------------
// Agents page state
// ---------------------------------------------------------------------------

pub struct AgentsPage {
    agents: Vec<AgentSummary>,
    table_state: TableState,
    last_refresh: Instant,
    status_msg: Option<(String, bool)>,
}

impl AgentsPage {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            table_state: TableState::default(),
            last_refresh: Instant::now() - std::time::Duration::from_secs(10),
            status_msg: None,
        }
    }

    fn refresh(&mut self, backend: &DaemonBackend) {
        self.agents = match backend.list_agents() {
            BackendResponse::Agents { agents } => agents,
            BackendResponse::Error(e) => {
                self.status_msg = Some((format!("agents: {e}"), true));
                return;
            }
            _ => return,
        };

        self.clamp_selection();
        self.status_msg = None;
    }

    fn clamp_selection(&mut self) {
        if self.agents.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected() {
            if i >= self.agents.len() {
                self.table_state.select(Some(self.agents.len() - 1));
            }
        } else if !self.agents.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn nav_down(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let len = self.agents.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state.select(Some((i + 1) % len));
    }

    fn nav_up(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let len = self.agents.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }
}

impl TuiPage for AgentsPage {
    fn title(&self) -> &str {
        "Agents"
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
                Constraint::Length(2), // title
                Constraint::Min(5),    // agent table
                Constraint::Length(2), // hints
                Constraint::Length(1), // status
            ])
            .margin(1)
            .split(area);

        // Title.
        let active = self
            .agents
            .iter()
            .filter(|a| a.status != "Idle" && a.status != "idle")
            .count();
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                "Agents",
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({} total, {} active)", self.agents.len(), active),
                Style::default().fg(TEXT_MUTED),
            ),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(title, chunks[0]);

        // Agent table.
        let header = Row::new(vec!["", "Name", "Type", "Status", "Tool", "Pane", "PID"])
            .style(
                Style::default()
                    .fg(ACCENT_COOL)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1);

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let selected = self.table_state.selected() == Some(i);
                let pointer = if selected { "\u{25b8}" } else { " " };

                let status_color = match agent.status.as_str() {
                    "idle" | "Idle" => STATUS_OK,
                    "processing" | "Processing" => ACCENT_WARM,
                    "ToolUse" => WARM,
                    "AwaitingInput" => STATUS_WARN,
                    _ => TEXT_MUTED,
                };

                let agent_color = match agent.agent_type.as_str() {
                    "claude" => ACCENT_WARM,
                    "codex" => ACCENT_COOL,
                    "copilot" => HOT,
                    _ => TEXT,
                };

                let row_style = if selected {
                    Style::default().bg(BG_SURFACE).fg(TEXT_BRIGHT)
                } else {
                    Style::default().fg(TEXT)
                };

                let tool = agent.current_tool.as_deref().unwrap_or("-");
                let pid = agent
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string());

                Row::new(vec![
                    Cell::from(Span::styled(pointer, Style::default().fg(ACCENT_COOL))),
                    Cell::from(Span::styled(
                        agent.name.clone(),
                        Style::default()
                            .fg(agent_color)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        agent.agent_type.clone(),
                        Style::default().fg(TEXT_MUTED),
                    )),
                    Cell::from(Span::styled(
                        agent.status.clone(),
                        Style::default()
                            .fg(status_color)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(tool.to_string(), Style::default().fg(WARM))),
                    Cell::from(format!("#{}", agent.pane_id)),
                    Cell::from(Span::styled(pid, Style::default().fg(TEXT_MUTED))),
                ])
                .style(row_style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(2),  // pointer
                Constraint::Length(12), // name
                Constraint::Length(8),  // type
                Constraint::Length(12), // status
                Constraint::Min(10),    // tool
                Constraint::Length(8),  // pane
                Constraint::Length(8),  // PID
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG)),
        );

        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        // Hints.
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "j/k",
                Style::default()
                    .fg(ACCENT_COOL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": navigate  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "r",
                Style::default()
                    .fg(ACCENT_COOL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": refresh", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, chunks[2]);

        // Status.
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { STATUS_ERROR } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, chunks[3]);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> KeyResult {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
            KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
            KeyCode::Char('r') => {
                self.last_refresh = Instant::now() - std::time::Duration::from_secs(10);
            }
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
    fn agents_page_title() {
        let page = AgentsPage::new();
        assert_eq!(page.title(), "Agents");
    }

    #[test]
    fn agents_page_starts_empty() {
        let page = AgentsPage::new();
        assert!(page.agents.is_empty());
    }
}
