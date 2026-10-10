//! Workspace-wide pane identity summaries and mouse pane navigation.
use crate::menu::{ContextMenu, MenuContext, MenuItem, MenuSection};
use crate::pane::PaneId;
use therminal_core::config::KeyAction;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneRow {
    pub pane_id: PaneId,
    pub icon: String,
    pub app: String,
    pub environment: String,
    pub shell: String,
    pub detail: String,
    pub status: Option<String>,
}

/// Preserve pane order and group only identical app/environment identities.
/// Keep different hosts separate; their full identities live in the hover list.
/// Tab labels stay icon-only so every pane group has room to be seen.
pub(super) fn summarize(rows: &[PaneRow]) -> String {
    let mut groups: Vec<(&str, &str, &str, usize)> = Vec::new();
    for row in rows {
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.0 == row.icon && g.1 == row.app && g.2 == row.environment)
        {
            group.3 += 1;
        } else {
            groups.push((&row.icon, &row.app, &row.environment, 1));
        }
    }
    groups
        .into_iter()
        .map(|(icon, _, _, count)| {
            if count > 1 {
                format!("{icon}×{count}")
            } else {
                icon.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep the workspace number first even when a narrow tab clips the name.
pub(super) fn decorate_label(workspace_id: usize, label: &str, rows: &[PaneRow]) -> String {
    if rows.is_empty() {
        return label.to_string();
    }
    let summary = compact(&summarize(rows), 32);
    let prefix = format!("{workspace_id}: ");
    let name = label.strip_prefix(&prefix).unwrap_or("");
    if name.is_empty() {
        format!("{workspace_id} {summary}")
    } else {
        format!("{workspace_id} {summary} · {}", compact(name, 24))
    }
}

pub(super) fn compact(text: &str, limit: usize) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().count() <= limit {
        text
    } else {
        text.chars()
            .take(limit.saturating_sub(1))
            .chain(['…'])
            .collect()
    }
}

pub(super) fn app_icon(app: &str) -> &'static str {
    match app.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" => "✳",
        "codex" => "◈",
        "aider" | "copilot" | "agy" => "◆",
        "tfe" => "\u{f07b}",
        "browser" => "◎",
        "powershell" | "windows powershell" | "pwsh" | "powershell.exe" | "pwsh.exe" => ">_",
        "command prompt" | "cmd" => "C>",
        "bash" | "zsh" | "fish" | "sh" => "$",
        "htop" | "btop" | "top" => "\u{f080}",
        _ => "\u{f120}",
    }
}

pub(super) struct PanePicker {
    pub workspace_id: usize,
    pub rows: Vec<PaneRow>,
    pub menu: ContextMenu,
    pub offset: usize,
    pub page_size: usize,
}

impl PanePicker {
    pub fn new(
        workspace_id: usize,
        rows: Vec<PaneRow>,
        position: (f32, f32),
        width: f32,
        height: f32,
    ) -> Self {
        let page_size = (((height - position.1 - 16.0) / 28.0) as usize).max(1);
        let mut picker = Self {
            workspace_id,
            rows,
            offset: 0,
            page_size,
            menu: ContextMenu {
                sections: vec![],
                position,
                selected_index: None,
                context: MenuContext::Tab { workspace_id },
            },
        };
        picker.rebuild(width);
        picker
    }

    pub fn rebuild(&mut self, width: f32) {
        // The common menu renderer estimates glyph widths conservatively.
        // Budget bytes as well as chars so icon glyphs cannot push it offscreen.
        let budget = ((width - 28.0) / 12.6).max(8.0) as usize;
        self.menu.sections = vec![MenuSection(
            self.rows
                .iter()
                .skip(self.offset)
                .take(self.page_size)
                .enumerate()
                .map(|(i, row)| {
                    let status = row
                        .status
                        .as_ref()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default();
                    let detail =
                        if row.detail.is_empty() || row.detail == format!("Pane {}", row.pane_id) {
                            String::new()
                        } else {
                            format!(" · {}", row.detail)
                        };
                    let text = format!(
                        "{}) {} {} · {} / {}{}{}",
                        row.pane_id, row.icon, row.app, row.environment, row.shell, status, detail
                    );
                    let fit = |text: &str| {
                        let mut label = compact(text, budget);
                        if label.len() > budget {
                            while label.len() + '…'.len_utf8() > budget {
                                label.pop();
                            }
                            label.push('…');
                        }
                        label
                    };
                    let has_more = self.rows.len() > self.page_size;
                    let identity = if has_more && i == 0 {
                        format!("↕ {text}")
                    } else {
                        text
                    };
                    MenuItem {
                        label: fit(&identity).into(),
                        hotkey_hint: None,
                        action: KeyAction::FocusNext,
                        enabled: true,
                    }
                })
                .collect(),
        )];
        self.menu.selected_index = None;
    }

    pub fn row_at(&self, x: f32, y: f32, width: f32, height: f32) -> Option<PaneId> {
        let g = self.menu.geometry(width, height);
        let index =
            self.menu
                .item_at_position(x, y, g.x, g.y, g.width, g.item_height, g.section_gap)?;
        self.rows.get(self.offset + index).map(|row| row.pane_id)
    }

    pub fn contains(&self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let g = self.menu.geometry(width, height);
        x >= g.x && x <= g.x + g.width && y >= g.y && y <= g.y + g.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: PaneId, app: &str, env: &str) -> PaneRow {
        PaneRow {
            pane_id: id,
            icon: app_icon(app).into(),
            app: app.into(),
            environment: env.into(),
            shell: "bash".into(),
            detail: "/tmp".into(),
            status: None,
        }
    }
    #[test]
    fn counts_all_panes_but_keeps_environments_distinct() {
        let rows = vec![
            row(1, "codex", "Ubuntu"),
            row(2, "codex", "Ubuntu"),
            row(3, "codex", "Windows"),
            row(4, "browser", ""),
        ];
        assert_eq!(summarize(&rows), "◈×2 ◈ ◎");
    }
    #[test]
    fn popup_targets_stable_pane_ids_even_after_scrolling() {
        let rows = vec![
            row(41, "bash", "Ubuntu"),
            row(99, "codex", "Ubuntu"),
            row(7, "browser", ""),
        ];
        let mut picker = PanePicker::new(2, rows, (0.0, 28.0), 800.0, 120.0);
        assert_eq!(picker.row_at(10.0, 40.0, 800.0, 120.0), Some(41));
        picker.offset = 1;
        picker.rebuild(800.0);
        assert_eq!(picker.row_at(10.0, 40.0, 800.0, 120.0), Some(99));
        assert_eq!(picker.row_at(799.0, 40.0, 800.0, 120.0), None);
    }
    #[test]
    fn summary_preserves_workspace_number_and_name() {
        let rows = vec![row(1, "bash", "Ubuntu")];
        assert_eq!(decorate_label(3, "3: Work", &rows), "3 $ · Work");
        assert_eq!(decorate_label(3, "3", &rows), "3 $");
    }

    #[test]
    fn browser_only_panes_have_one_numbered_entry_each() {
        let rows = vec![row(42, "browser", ""), row(91, "browser", "")];
        assert_eq!(summarize(&rows), "◎×2");
        let picker = PanePicker::new(1, rows, (0.0, 28.0), 800.0, 400.0);
        assert_eq!(picker.row_at(10.0, 40.0, 800.0, 400.0), Some(42));
        assert_eq!(picker.row_at(10.0, 70.0, 800.0, 400.0), Some(91));
        assert_eq!(picker.row_at(10.0, 105.0, 800.0, 400.0), None);
        assert_eq!(picker.menu.item_count(), 2);
        assert!(picker.menu.sections[0].0[0].label.starts_with("42)"));
        assert!(picker.menu.sections[0].0[1].label.starts_with("91)"));
    }

    #[test]
    fn labels_are_unicode_safe_and_single_line() {
        assert_eq!(compact("Ubuntu\n日本語", 9), "Ubuntu 日…");
    }
}

impl super::App {
    pub(super) fn workspace_pane_rows(&self, workspace_id: usize) -> Vec<PaneRow> {
        use therminal_protocol::daemon::IdentityObservationStatus;
        let Some(layout) = self
            .workspaces
            .as_ref()
            .and_then(|wm| wm.layout_for(workspace_id))
        else {
            return vec![];
        };
        let registry = self.agent_registry.try_lock().ok();
        layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane_id| {
                let pane = layout.find_pane(pane_id)?;
                let status = pane
                    .status
                    .try_lock()
                    .ok()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                let identity = status.effective_identity();
                if let Some(url) = pane.webview_url() {
                    let url = self
                        .webview_manager
                        .url(pane_id)
                        .unwrap_or_else(|| url.to_string());
                    return Some(PaneRow {
                        pane_id,
                        icon: identity.icon.unwrap_or_else(|| app_icon("browser").into()),
                        app: "Browser".into(),
                        environment: identity.environment.unwrap_or_default(),
                        shell: identity.shell.unwrap_or_else(|| "Web".into()),
                        detail: url,
                        status: None,
                    });
                }
                let app = identity
                    .application
                    .clone()
                    .or_else(|| identity.shell.clone())
                    .unwrap_or_else(|| "Terminal".into());
                let agent = registry.as_ref().and_then(|r| r.get(pane_id));
                // Claude's session integration is authoritative for its richer status.
                // A detected process alone conveys no working/waiting state.
                let meta = agent
                    .and_then(|a| a.pid)
                    .and_then(|pid| self.claude_cwd.chrome_meta_for_pid(pid))
                    .or_else(|| {
                        status
                            .claude_session_id
                            .as_ref()
                            .and_then(|id| self.claude_cwd.chrome_meta_for_session(id))
                    });
                let freshness = status.current_identity.freshness();
                let meta = meta.filter(|_| {
                    freshness == IdentityObservationStatus::Live
                        && app.eq_ignore_ascii_case("claude")
                });
                let observed = match freshness {
                    IdentityObservationStatus::Live => None,
                    IdentityObservationStatus::Stale => Some("process info stale".to_string()),
                    IdentityObservationStatus::Unknown => Some("process info unknown".to_string()),
                };
                let agent_status = meta
                    .as_ref()
                    .map(|m| m.status_label().to_string())
                    .or_else(|| agent.map(|_| "agent status unknown".to_string()));
                let detail = match (meta.and_then(|m| m.header_title()), status.cwd) {
                    (Some(title), Some(cwd)) => format!("{title} · {cwd}"),
                    (Some(title), None) => title,
                    (None, Some(cwd)) => cwd,
                    (None, None) => format!("Pane {pane_id}"),
                };
                Some(PaneRow {
                    pane_id,
                    icon: if identity.application.is_some()
                        && identity.application != status.launch_identity.application
                    {
                        app_icon(&app).into()
                    } else {
                        identity.icon.unwrap_or_else(|| app_icon(&app).into())
                    },
                    app,
                    environment: identity
                        .environment
                        .unwrap_or_else(|| "Unknown environment".into()),
                    shell: identity.shell.unwrap_or_else(|| "Unknown shell".into()),
                    detail,
                    status: match (agent_status, observed) {
                        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                        (a, b) => a.or(b),
                    },
                })
            })
            .collect()
    }
}

impl super::App {
    /// Keep a hover card open while crossing from its tab into the card.
    /// No IPC, process queries or blocking locks are allowed in this path.
    pub(super) fn update_tab_hover(&mut self, x: f32, y: f32) -> bool {
        if self.focus_mode
            || self.active_menu.is_some()
            || self.overlay_mode.is_some()
            || self.rename_state.is_some()
        {
            if self.pane_picker.take().is_some() {
                self.request_redraw();
            }
            return false;
        }
        let Some(gpu) = self.gpu.as_ref() else {
            return false;
        };
        let (width, height) = (gpu.config.width as f32, gpu.config.height as f32);
        let ids = self
            .workspaces
            .as_ref()
            .map(|wm| wm.workspace_ids())
            .unwrap_or_default();
        let bar_h = crate::pane::effective_tab_bar_height_csd(
            ids.len(),
            self.config.general.use_csd,
            false,
        );
        // Tabs win at the shared top edge, permitting direct movement between tabs.
        if y >= bar_h
            && let Some(picker) = self.pane_picker.as_mut()
            && picker.contains(x, y, width, height)
        {
            let g = picker.menu.geometry(width, height);
            picker.menu.selected_index =
                picker
                    .menu
                    .item_at_position(x, y, g.x, g.y, g.width, g.item_height, g.section_gap);
            self.request_redraw();
            return true;
        }
        let reserved = if self.config.general.use_csd {
            crate::pane::CSD_BUTTONS_TOTAL_WIDTH
        } else {
            0.0
        };
        let labels = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let target = if y >= 0.0 && y < bar_h {
            super::chrome::tab_bar_hit_test(x, &ids, &labels, width, reserved)
        } else {
            None
        };
        if let Some(workspace_id) = target {
            if self
                .pane_picker
                .as_ref()
                .is_none_or(|p| p.workspace_id != workspace_id)
            {
                let rows = self.workspace_pane_rows(workspace_id);
                if !rows.is_empty() {
                    self.pane_picker = Some(PanePicker::new(
                        workspace_id,
                        rows,
                        (x.max(0.0), bar_h),
                        width,
                        height,
                    ));
                    self.webview_manager.hide_all();
                    self.request_redraw();
                }
            }
        } else if self.pane_picker.take().is_some() {
            self.request_redraw();
        }
        false
    }

    pub(super) fn click_pane_picker(&mut self, x: f32, y: f32) -> bool {
        let Some(gpu) = self.gpu.as_ref() else {
            return false;
        };
        let (width, height) = (gpu.config.width as f32, gpu.config.height as f32);
        let Some(picker) = self.pane_picker.as_ref() else {
            return false;
        };
        if !picker.contains(x, y, width, height) {
            self.pane_picker = None;
            return false;
        }
        let target = picker
            .row_at(x, y, width, height)
            .map(|pane| (picker.workspace_id, pane));
        self.pane_picker = None;
        if let Some((workspace, pane)) = target {
            // The pane may have exited after the card opened. Never create a
            // replacement workspace as a side effect of selecting an old row.
            let exists = self
                .workspaces
                .as_ref()
                .and_then(|wm| wm.layout_for(workspace))
                .is_some_and(|l| l.find_pane(pane).is_some());
            if exists {
                self.switch_workspace(workspace as u8);
                self.set_focused_pane(Some(pane));
                if self
                    .get_layout()
                    .and_then(|l| l.find_pane(pane))
                    .is_some_and(|p| p.is_webview())
                {
                    self.webview_manager.focus(pane);
                }
                self.publish_workspace_state();
            }
        }
        self.request_redraw();
        true
    }

    pub(super) fn scroll_pane_picker(&mut self, delta: winit::event::MouseScrollDelta) -> bool {
        let Some((x, y)) = self.cursor_position else {
            return false;
        };
        let Some(gpu) = self.gpu.as_ref() else {
            return false;
        };
        let (width, height) = (gpu.config.width as f32, gpu.config.height as f32);
        let Some(picker) = self.pane_picker.as_mut() else {
            return false;
        };
        if !picker.contains(x as f32, y as f32, width, height) {
            return false;
        }
        let amount = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => y,
            winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 28.0) as f32,
        };
        let limit = picker.rows.len().saturating_sub(picker.page_size);
        if amount < 0.0 {
            picker.offset = (picker.offset + 1).min(limit);
        } else if amount > 0.0 {
            picker.offset = picker.offset.saturating_sub(1);
        }
        picker.rebuild(width);
        self.request_redraw();
        true
    }
}
