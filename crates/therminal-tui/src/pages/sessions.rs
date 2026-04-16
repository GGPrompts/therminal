//! Sessions page — list/preview/detail side-panel layout.
//!
//! Ported from thermal-desktop's Sessions page. Shows all daemon sessions
//! in a left panel with a preview/detail panel on the right.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use therminal_protocol::SessionId;
use therminal_protocol::daemon::{AgentSummary, PaneSummary};

use super::{KeyResult, TuiPage};
use crate::backend::{BackendResponse, DaemonBackend};
use crate::palette::*;

// ---------------------------------------------------------------------------
// Panel focus (mirroring thermal-desktop's 3-panel pattern)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FocusedPanel {
    SessionList,
    Preview,
}

impl FocusedPanel {
    fn toggle(self) -> Self {
        match self {
            Self::SessionList => Self::Preview,
            Self::Preview => Self::SessionList,
        }
    }
}

// ---------------------------------------------------------------------------
// Session display row
// ---------------------------------------------------------------------------

struct SessionRow {
    session_id: SessionId,
    #[allow(dead_code)]
    name: Option<String>,
    pane_count: usize,
    agent_count: usize,
}

// ---------------------------------------------------------------------------
// Sessions page state
// ---------------------------------------------------------------------------

pub struct SessionsPage {
    rows: Vec<SessionRow>,
    panes: Vec<PaneSummary>,
    agents: Vec<AgentSummary>,
    table_state: TableState,
    focused_panel: FocusedPanel,

    /// Preview content for the selected session's panes.
    preview_lines: Vec<Line<'static>>,
    preview_scroll: usize,

    /// Last tick time for throttling IPC calls.
    last_refresh: Instant,

    /// Connection status message.
    status_msg: Option<(String, bool)>,
}

impl SessionsPage {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
            table_state: TableState::default(),
            focused_panel: FocusedPanel::SessionList,
            preview_lines: Vec::new(),
            preview_scroll: 0,
            last_refresh: Instant::now() - std::time::Duration::from_secs(10),
            status_msg: None,
        }
    }

    fn refresh(&mut self, backend: &DaemonBackend) {
        // Fetch sessions.
        let session_ids = match backend.list_sessions() {
            BackendResponse::Sessions { session_ids } => session_ids,
            BackendResponse::Error(e) => {
                self.status_msg = Some((format!("sessions: {e}"), true));
                return;
            }
            _ => return,
        };

        // Fetch all panes.
        self.panes = match backend.list_panes(None) {
            BackendResponse::Panes { panes } => panes,
            _ => Vec::new(),
        };

        // Fetch agents.
        self.agents = match backend.list_agents() {
            BackendResponse::Agents { agents } => agents,
            _ => Vec::new(),
        };

        // Build session rows.
        self.rows = session_ids
            .into_iter()
            .map(|sid| {
                let pane_count = self.panes.iter().filter(|p| p.session_id == sid).count();
                let agent_count = self
                    .agents
                    .iter()
                    .filter(|a| {
                        self.panes
                            .iter()
                            .any(|p| p.pane_id == a.pane_id && p.session_id == sid)
                    })
                    .count();
                SessionRow {
                    session_id: sid,
                    name: None,
                    pane_count,
                    agent_count,
                }
            })
            .collect();

        self.clamp_selection();
        self.update_preview(backend);
        self.status_msg = None;
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected() {
            if i >= self.rows.len() {
                self.table_state.select(Some(self.rows.len() - 1));
            }
        } else {
            self.table_state.select(Some(0));
        }
    }

    fn update_preview(&mut self, backend: &DaemonBackend) {
        let sid = match self.table_state.selected().and_then(|i| self.rows.get(i)) {
            Some(row) => row.session_id,
            None => {
                self.preview_lines = vec![Line::from(Span::styled(
                    "No session selected",
                    Style::default().fg(TEXT_MUTED),
                ))];
                return;
            }
        };

        let session_panes: Vec<&PaneSummary> =
            self.panes.iter().filter(|p| p.session_id == sid).collect();

        let session_agents: Vec<&AgentSummary> = self
            .agents
            .iter()
            .filter(|a| session_panes.iter().any(|p| p.pane_id == a.pane_id))
            .collect();

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Session header.
        lines.push(Line::from(vec![Span::styled(
            format!("Session {sid}"),
            Style::default()
                .fg(TEXT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        // Pane summary.
        lines.push(Line::from(vec![Span::styled(
            format!("Panes ({})", session_panes.len()),
            Style::default()
                .fg(ACCENT_COOL)
                .add_modifier(Modifier::BOLD),
        )]));

        for pane in &session_panes {
            let agent_badge = session_agents
                .iter()
                .find(|a| a.pane_id == pane.pane_id)
                .map(|a| {
                    let color = match a.status.as_str() {
                        "idle" | "Idle" => STATUS_OK,
                        "processing" | "Processing" | "ToolUse" => ACCENT_WARM,
                        _ => TEXT_MUTED,
                    };
                    Span::styled(
                        format!(" [{}:{}]", a.name, a.status),
                        Style::default().fg(color),
                    )
                });

            let cwd_text = pane
                .cwd
                .as_deref()
                .map(abbreviate_path)
                .unwrap_or_else(|| "-".to_string());

            let mut spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("#{}", pane.pane_id),
                    Style::default()
                        .fg(ACCENT_COOL)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}x{}", pane.cols, pane.rows),
                    Style::default().fg(TEXT_MUTED),
                ),
                Span::styled(format!(" {cwd_text}"), Style::default().fg(TEXT)),
            ];

            if let Some(badge) = agent_badge {
                spans.push(badge);
            }

            if let Some(code) = pane.last_exit_code {
                let (label, color) = if code == 0 {
                    ("ok", STATUS_OK)
                } else {
                    ("err", STATUS_ERROR)
                };
                spans.push(Span::styled(
                    format!(" [{label}:{code}]"),
                    Style::default().fg(color),
                ));
            }

            if !pane.tags.is_empty() {
                let tags: Vec<String> = pane.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
                spans.push(Span::styled(
                    format!(" {{{}}}", tags.join(", ")),
                    Style::default().fg(TEXT_MUTED),
                ));
            }

            lines.push(Line::from(spans));
        }

        // Agent details.
        if !session_agents.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                format!("Agents ({})", session_agents.len()),
                Style::default()
                    .fg(ACCENT_WARM)
                    .add_modifier(Modifier::BOLD),
            )]));

            for agent in &session_agents {
                let status_color = match agent.status.as_str() {
                    "idle" | "Idle" => STATUS_OK,
                    "processing" | "Processing" | "ToolUse" => ACCENT_WARM,
                    _ => TEXT_MUTED,
                };

                let mut spans = vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        agent.name.to_string(),
                        Style::default()
                            .fg(TEXT_BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" ({})", agent.agent_type),
                        Style::default().fg(TEXT_MUTED),
                    ),
                    Span::styled(
                        format!(" {}", agent.status),
                        Style::default().fg(status_color),
                    ),
                ];

                if let Some(ref tool) = agent.current_tool {
                    spans.push(Span::styled(
                        format!(" -> {tool}"),
                        Style::default().fg(WARM),
                    ));
                }

                if let Some(pid) = agent.pid {
                    spans.push(Span::styled(
                        format!(" pid:{pid}"),
                        Style::default().fg(TEXT_MUTED),
                    ));
                }

                lines.push(Line::from(spans));
            }
        }

        // Pane content preview — show the last pane's captured output.
        if let Some(last_pane) = session_panes.last() {
            if let BackendResponse::PaneCaptured {
                lines: cap_lines, ..
            } = backend.capture_pane(last_pane.pane_id)
            {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![Span::styled(
                    format!("Pane #{} output:", last_pane.pane_id),
                    Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                )]));

                // Show last 20 non-empty lines.
                let content: Vec<&String> = cap_lines
                    .iter()
                    .rev()
                    .filter(|l| !l.trim().is_empty())
                    .take(20)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();

                for line in content {
                    lines.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(TEXT),
                    )));
                }
            }
        }

        self.preview_lines = lines;
        self.preview_scroll = 0;
    }

    fn nav_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state.select(Some((i + 1) % len));
    }

    fn nav_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len();
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }
}

impl TuiPage for SessionsPage {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn tick(&mut self, backend: &DaemonBackend) {
        // Throttle to every 2s.
        if self.last_refresh.elapsed() >= std::time::Duration::from_secs(2) {
            self.refresh(backend);
            self.last_refresh = Instant::now();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        // Side-panel layout: session list (left) + preview (right).
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(40), // session list
                Constraint::Percentage(60), // preview / detail
            ])
            .margin(1)
            .split(area);

        // -- Left panel: session table --
        let list_border = if self.focused_panel == FocusedPanel::SessionList {
            ACCENT_COOL
        } else {
            TEXT_MUTED
        };

        let header = Row::new(vec!["", "ID", "Panes", "Agents"])
            .style(
                Style::default()
                    .fg(ACCENT_COOL)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1);

        let rows: Vec<Row> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let selected = self.table_state.selected() == Some(i);
                let pointer = if selected { "\u{25b8}" } else { " " };

                let row_style = if selected {
                    Style::default().bg(BG_SURFACE).fg(TEXT_BRIGHT)
                } else {
                    Style::default().fg(TEXT)
                };

                Row::new(vec![
                    Cell::from(Span::styled(pointer, Style::default().fg(ACCENT_COOL))),
                    Cell::from(Span::styled(
                        format!("{}", row.session_id),
                        Style::default().fg(if selected { TEXT_BRIGHT } else { TEXT }),
                    )),
                    Cell::from(Span::styled(
                        format!("{}", row.pane_count),
                        Style::default().fg(TEXT_MUTED),
                    )),
                    Cell::from(if row.agent_count > 0 {
                        Span::styled(
                            format!("{}", row.agent_count),
                            Style::default().fg(ACCENT_WARM),
                        )
                    } else {
                        Span::styled("-", Style::default().fg(TEXT_MUTED))
                    }),
                ])
                .style(row_style)
            })
            .collect();

        let session_count = self.rows.len();
        let table = Table::new(
            rows,
            [
                Constraint::Length(2), // pointer
                Constraint::Min(8),    // session ID
                Constraint::Length(6), // pane count
                Constraint::Length(7), // agent count
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(format!(" Sessions ({session_count}) "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(list_border))
                .style(Style::default().bg(BG)),
        );

        f.render_stateful_widget(table, main_chunks[0], &mut self.table_state);

        // -- Right panel: preview / detail --
        let preview_border = if self.focused_panel == FocusedPanel::Preview {
            ACCENT_COOL
        } else {
            TEXT_MUTED
        };

        let visible_height = main_chunks[1].height.saturating_sub(2) as usize;
        let max_scroll = self.preview_lines.len().saturating_sub(visible_height);
        self.preview_scroll = self.preview_scroll.min(max_scroll);

        let preview = Paragraph::new(self.preview_lines.clone())
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll as u16, 0))
            .block(
                Block::default()
                    .title(" Detail ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(preview_border))
                    .style(Style::default().bg(BG)),
            );
        f.render_widget(preview, main_chunks[1]);

        // Status message at bottom of left panel (via footer area).
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

        match self.focused_panel {
            FocusedPanel::SessionList => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
                KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
                KeyCode::Tab => self.focused_panel = self.focused_panel.toggle(),
                KeyCode::Enter => {
                    // Refresh preview for selected session.
                    // (Preview is auto-updated on selection change via tick,
                    // but Enter forces an immediate refresh.)
                }
                _ => {}
            },
            FocusedPanel::Preview => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.preview_scroll = self.preview_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.preview_scroll = self.preview_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(10);
                }
                KeyCode::Tab => self.focused_panel = self.focused_panel.toggle(),
                _ => {}
            },
        }

        KeyResult::NONE
    }

    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        match event.kind {
            MouseEventKind::ScrollDown => {
                if self.focused_panel == FocusedPanel::SessionList {
                    self.nav_down();
                } else {
                    self.preview_scroll = self.preview_scroll.saturating_add(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.focused_panel == FocusedPanel::SessionList {
                    self.nav_up();
                } else {
                    self.preview_scroll = self.preview_scroll.saturating_sub(3);
                }
            }
            _ => {}
        }
    }
}

/// Abbreviate a path for display (home -> ~).
fn abbreviate_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviate_path_replaces_home() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        assert_eq!(
            abbreviate_path("/home/testuser/projects/foo"),
            "~/projects/foo"
        );
    }

    #[test]
    fn abbreviate_path_leaves_other_paths() {
        assert_eq!(abbreviate_path("/tmp/something"), "/tmp/something");
    }

    #[test]
    fn sessions_page_title() {
        let page = SessionsPage::new();
        assert_eq!(page.title(), "Sessions");
    }

    #[test]
    fn sessions_page_starts_empty() {
        let page = SessionsPage::new();
        assert!(page.rows.is_empty());
        assert!(page.panes.is_empty());
        assert!(page.agents.is_empty());
    }

    #[test]
    fn nav_down_wraps() {
        let mut page = SessionsPage::new();
        page.rows = vec![
            SessionRow {
                session_id: 1,
                name: None,
                pane_count: 1,
                agent_count: 0,
            },
            SessionRow {
                session_id: 2,
                name: None,
                pane_count: 1,
                agent_count: 0,
            },
        ];
        page.table_state.select(Some(1));
        page.nav_down();
        assert_eq!(page.table_state.selected(), Some(0));
    }

    #[test]
    fn nav_up_wraps() {
        let mut page = SessionsPage::new();
        page.rows = vec![
            SessionRow {
                session_id: 1,
                name: None,
                pane_count: 1,
                agent_count: 0,
            },
            SessionRow {
                session_id: 2,
                name: None,
                pane_count: 1,
                agent_count: 0,
            },
        ];
        page.table_state.select(Some(0));
        page.nav_up();
        assert_eq!(page.table_state.selected(), Some(1));
    }

    #[test]
    fn focused_panel_toggle() {
        assert_eq!(FocusedPanel::SessionList.toggle(), FocusedPanel::Preview);
        assert_eq!(FocusedPanel::Preview.toggle(), FocusedPanel::SessionList);
    }
}
