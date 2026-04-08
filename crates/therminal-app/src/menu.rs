//! GPU-rendered right-click context menus.
//!
//! Provides data model, lifecycle, and rendering for context menus that appear
//! as overlays on top of the terminal content. Menus are composed of sections
//! (groups of items separated visually), each containing menu items with labels,
//! optional hotkey hints, and associated actions.

use therminal_core::config::KeyAction;

use crate::pane::PaneId;

// ── Menu data model ───────────────────────────────────────────────────

/// Context in which the menu was opened, determining which items to show.
#[derive(Debug, Clone)]
pub(crate) enum MenuContext {
    /// Right-click on a pane area (no selection).
    Pane {
        #[allow(dead_code)]
        pane_id: PaneId,
    },
    /// Right-click with text selected.
    Selection {
        #[allow(dead_code)]
        text: String,
    },
    /// Right-click on a tab in the tab bar.
    Tab {
        /// The workspace ID of the tab that was right-clicked.
        #[allow(dead_code)]
        workspace_id: usize,
    },
}

/// A single item in a context menu.
#[derive(Debug, Clone)]
pub(crate) struct MenuItem {
    /// Display label for the item.
    pub label: &'static str,
    /// Optional hotkey hint displayed right-aligned (e.g. "Ctrl+Shift+C").
    pub hotkey_hint: Option<String>,
    /// Action to execute when the item is selected.
    pub action: KeyAction,
    /// Whether the item is currently enabled/clickable.
    pub enabled: bool,
}

/// A group of menu items that are visually separated from other sections.
#[derive(Debug, Clone)]
pub(crate) struct MenuSection(pub Vec<MenuItem>);

/// An open context menu with its position, items, and selection state.
#[derive(Debug, Clone)]
pub(crate) struct ContextMenu {
    /// Sections of grouped menu items.
    pub sections: Vec<MenuSection>,
    /// Position where the menu was opened (physical pixels, top-left corner).
    pub position: (f32, f32),
    /// Index of the currently highlighted item (flat index across all sections).
    pub selected_index: Option<usize>,
    /// The context that triggered this menu.
    #[allow(dead_code)]
    pub context: MenuContext,
}

impl ContextMenu {
    /// Total number of items across all sections.
    pub fn item_count(&self) -> usize {
        self.sections.iter().map(|s| s.0.len()).sum()
    }

    /// Get a flat list of references to all menu items.
    pub fn flat_items(&self) -> Vec<&MenuItem> {
        self.sections.iter().flat_map(|s| s.0.iter()).collect()
    }

    /// Get the action of the currently selected item, if any.
    pub fn selected_action(&self) -> Option<KeyAction> {
        let idx = self.selected_index?;
        let items = self.flat_items();
        let item = items.get(idx)?;
        if item.enabled {
            Some(item.action.clone())
        } else {
            None
        }
    }

    /// Move selection up by one item, wrapping around.
    pub fn move_up(&mut self) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(0) | None => count - 1,
            Some(i) => i - 1,
        });
    }

    /// Move selection down by one item, wrapping around.
    pub fn move_down(&mut self) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            None => 0,
            Some(i) => (i + 1) % count,
        });
    }

    /// Determine which item (if any) is at the given pixel position,
    /// given the menu's rendered geometry.
    #[allow(clippy::too_many_arguments)]
    pub fn item_at_position(
        &self,
        px: f32,
        py: f32,
        menu_x: f32,
        menu_y: f32,
        menu_width: f32,
        item_height: f32,
        section_gap: f32,
    ) -> Option<usize> {
        if px < menu_x || px > menu_x + menu_width {
            return None;
        }

        let mut y_offset = menu_y + MENU_PADDING_Y;
        let mut flat_idx = 0;
        for (section_idx, section) in self.sections.iter().enumerate() {
            if section_idx > 0 {
                y_offset += section_gap;
            }
            for _item in &section.0 {
                if py >= y_offset && py < y_offset + item_height {
                    return Some(flat_idx);
                }
                y_offset += item_height;
                flat_idx += 1;
            }
        }
        None
    }

    /// Check if a pixel position is inside the menu bounds.
    pub fn contains_point(&self, px: f32, py: f32, menu_width: f32, menu_height: f32) -> bool {
        let (mx, my) = self.position;
        px >= mx && px <= mx + menu_width && py >= my && py <= my + menu_height
    }
}

// ── Menu building ─────────────────────────────────────────────────────

/// Look up the first keybinding string for a given action from the config.
pub(crate) fn hotkey_for_action(
    bindings: &[therminal_core::config::Keybinding],
    action: &KeyAction,
) -> Option<String> {
    bindings
        .iter()
        .find(|b| &b.action == action)
        .map(|b| format_hotkey(&b.key))
}

/// Format a keybinding string for display (capitalize parts).
fn format_hotkey(key: &str) -> String {
    key.split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Build the context menu for a pane (no selection).
pub(crate) fn build_pane_menu(
    pane_id: PaneId,
    bindings: &[therminal_core::config::Keybinding],
    position: (f32, f32),
) -> ContextMenu {
    let hint = |action: &KeyAction| hotkey_for_action(bindings, action);

    ContextMenu {
        sections: vec![
            MenuSection(vec![
                MenuItem {
                    label: "Split Horizontal",
                    hotkey_hint: hint(&KeyAction::SplitHorizontal),
                    action: KeyAction::SplitHorizontal,
                    enabled: true,
                },
                MenuItem {
                    label: "Split Vertical",
                    hotkey_hint: hint(&KeyAction::SplitVertical),
                    action: KeyAction::SplitVertical,
                    enabled: true,
                },
            ]),
            MenuSection(vec![MenuItem {
                label: "Close Pane",
                hotkey_hint: hint(&KeyAction::ClosePane),
                action: KeyAction::ClosePane,
                enabled: true,
            }]),
            MenuSection(vec![
                MenuItem {
                    label: "Copy",
                    hotkey_hint: hint(&KeyAction::Copy),
                    action: KeyAction::Copy,
                    enabled: true,
                },
                MenuItem {
                    label: "Paste",
                    hotkey_hint: hint(&KeyAction::Paste),
                    action: KeyAction::Paste,
                    enabled: true,
                },
                MenuItem {
                    label: "Copy pane ID",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(pane_id.to_string()),
                    enabled: true,
                },
            ]),
        ],
        position,
        selected_index: None,
        context: MenuContext::Pane { pane_id },
    }
}

/// Build the context menu for a selection.
pub(crate) fn build_selection_menu(
    text: String,
    bindings: &[therminal_core::config::Keybinding],
    position: (f32, f32),
) -> ContextMenu {
    let hint = |action: &KeyAction| hotkey_for_action(bindings, action);

    ContextMenu {
        sections: vec![
            MenuSection(vec![MenuItem {
                label: "Copy",
                hotkey_hint: hint(&KeyAction::Copy),
                action: KeyAction::Copy,
                enabled: true,
            }]),
            MenuSection(vec![MenuItem {
                label: "Paste",
                hotkey_hint: hint(&KeyAction::Paste),
                action: KeyAction::Paste,
                enabled: true,
            }]),
        ],
        position,
        selected_index: None,
        context: MenuContext::Selection { text },
    }
}

/// Build the context menu for a tab bar right-click.
pub(crate) fn build_tab_menu(
    workspace_id: usize,
    bindings: &[therminal_core::config::Keybinding],
    position: (f32, f32),
) -> ContextMenu {
    let hint = |action: &KeyAction| hotkey_for_action(bindings, action);

    ContextMenu {
        sections: vec![
            MenuSection(vec![
                MenuItem {
                    label: "New Tab",
                    hotkey_hint: hint(&KeyAction::NewWorkspace),
                    action: KeyAction::NewWorkspace,
                    enabled: true,
                },
                MenuItem {
                    label: "Rename Tab",
                    hotkey_hint: hint(&KeyAction::RenameWorkspace),
                    action: KeyAction::RenameWorkspace,
                    enabled: true,
                },
                MenuItem {
                    label: "Close Tab",
                    hotkey_hint: hint(&KeyAction::CloseAllPanes),
                    action: KeyAction::CloseAllPanes,
                    enabled: true,
                },
            ]),
            MenuSection(vec![
                MenuItem {
                    label: "Split Horizontal",
                    hotkey_hint: hint(&KeyAction::SplitHorizontal),
                    action: KeyAction::SplitHorizontal,
                    enabled: true,
                },
                MenuItem {
                    label: "Split Vertical",
                    hotkey_hint: hint(&KeyAction::SplitVertical),
                    action: KeyAction::SplitVertical,
                    enabled: true,
                },
            ]),
        ],
        position,
        selected_index: None,
        context: MenuContext::Tab { workspace_id },
    }
}

// ── Hotspot action palette ────────────────────────────────────────────

/// Build an action palette for a detected hotspot.
///
/// Shows contextual actions based on the hotspot kind (file path, error
/// location, git ref, issue ref). When `is_dir` is `true` and the kind is
/// `FilePath`, the menu shows directory-specific actions (open in new pane
/// via `folder_pane_command`, open in file manager) instead of the editor
/// chain (tn-zqwg). Actions use `HotspotCopy` / `HotspotOpenInEditor` /
/// `HotspotOpenExternal` / `HotspotOpenFolderInPane` /
/// `HotspotOpenFolderInFileManager` KeyAction variants.
pub(crate) fn build_hotspot_palette(
    kind: therminal_terminal::hotspot_detection::HotspotKind,
    text: String,
    is_dir: bool,
    position: (f32, f32),
) -> ContextMenu {
    use therminal_terminal::hotspot_detection::HotspotKind;

    let sections = match kind {
        HotspotKind::FilePath if is_dir => {
            // Directory hotspot: route through the folder-open action set
            // instead of the file editor fallback chain. The default action
            // (top of the menu) spawns `folder_pane_command` in a new pane.
            let path = text.clone();
            vec![MenuSection(vec![
                MenuItem {
                    label: "Open in new pane",
                    hotkey_hint: None,
                    action: KeyAction::HotspotOpenFolderInPane(path.clone()),
                    enabled: true,
                },
                MenuItem {
                    label: "Open in file manager",
                    hotkey_hint: None,
                    action: KeyAction::HotspotOpenFolderInFileManager(path.clone()),
                    enabled: true,
                },
                MenuItem {
                    label: "Copy path",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(path),
                    enabled: true,
                },
            ])]
        }
        HotspotKind::FilePath | HotspotKind::ErrorLocation => {
            // Parse path and optional line:col for display.
            let (path, line_suffix) = parse_file_path_parts(&text);
            let mut items = vec![
                MenuItem {
                    label: "Open in editor",
                    hotkey_hint: None,
                    action: KeyAction::HotspotOpenInEditor(text.clone()),
                    enabled: true,
                },
                MenuItem {
                    label: "Copy path",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(path.to_string()),
                    enabled: true,
                },
            ];
            if !line_suffix.is_empty() {
                items.push(MenuItem {
                    label: "Copy path:line",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(text.clone()),
                    enabled: true,
                });
            }
            vec![MenuSection(items)]
        }
        HotspotKind::GitRef => {
            vec![MenuSection(vec![
                MenuItem {
                    label: "Copy hash",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(text.clone()),
                    enabled: true,
                },
                MenuItem {
                    label: "Show in git log",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(format!("git log {text}")),
                    enabled: true,
                },
            ])]
        }
        HotspotKind::IssueRef => {
            vec![MenuSection(vec![
                MenuItem {
                    label: "Copy ref",
                    hotkey_hint: None,
                    action: KeyAction::HotspotCopy(text.clone()),
                    enabled: true,
                },
                MenuItem {
                    label: "Open issue",
                    hotkey_hint: None,
                    action: KeyAction::HotspotOpenExternal(text.clone()),
                    enabled: true,
                },
            ])]
        }
        HotspotKind::Url => {
            // URLs are handled by hyperlink click, but included for completeness.
            vec![MenuSection(vec![MenuItem {
                label: "Open URL",
                hotkey_hint: None,
                action: KeyAction::HotspotOpenExternal(text.clone()),
                enabled: true,
            }])]
        }
    };

    ContextMenu {
        sections,
        position,
        selected_index: None,
        context: MenuContext::Pane { pane_id: 0 },
    }
}

/// Split a file path like `src/main.rs:42:5` into (`src/main.rs`, `:42:5`).
fn parse_file_path_parts(text: &str) -> (&str, &str) {
    // Find the first colon that is followed by a digit (line number).
    if let Some(idx) = text.find(':')
        && text[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        return (&text[..idx], &text[idx..]);
    }
    (text, "")
}

// ── Menu rendering constants ──────────────────────────────────────────

/// Horizontal padding inside the menu.
const MENU_PADDING_X: f32 = 12.0;
/// Vertical padding at top/bottom of the menu.
const MENU_PADDING_Y: f32 = 6.0;
/// Height of each menu item row.
const MENU_ITEM_HEIGHT: f32 = 28.0;
/// Gap between sections (separator space).
const MENU_SECTION_GAP: f32 = 8.0;
/// Minimum gap between label text and hotkey hint.
const MENU_HINT_GAP: f32 = 24.0;
/// Border thickness.
const MENU_BORDER: f32 = 1.0;

// ── Menu geometry calculation ─────────────────────────────────────────

/// Computed menu geometry for rendering and hit testing.
pub(crate) struct MenuGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub item_height: f32,
    pub section_gap: f32,
}

impl ContextMenu {
    /// Calculate the menu geometry, clamping to surface bounds.
    pub fn geometry(&self, surface_width: f32, surface_height: f32) -> MenuGeometry {
        // Calculate the widest row (label + hint).
        let char_width_approx = MENU_ITEM_HEIGHT * 0.45; // rough monospace char width
        let mut max_row_width: f32 = 0.0;
        for section in &self.sections {
            for item in &section.0 {
                let label_w = item.label.len() as f32 * char_width_approx;
                let hint_w = item
                    .hotkey_hint
                    .as_ref()
                    .map(|h| h.len() as f32 * char_width_approx + MENU_HINT_GAP)
                    .unwrap_or(0.0);
                max_row_width = max_row_width.max(label_w + hint_w);
            }
        }

        let width = max_row_width + MENU_PADDING_X * 2.0;
        let sep_count = self.sections.len().saturating_sub(1);
        let height = MENU_PADDING_Y * 2.0
            + self.item_count() as f32 * MENU_ITEM_HEIGHT
            + sep_count as f32 * MENU_SECTION_GAP;

        // Clamp position so menu stays on screen.
        let (raw_x, raw_y) = self.position;
        let x = raw_x.min(surface_width - width - 2.0).max(0.0);
        let y = raw_y.min(surface_height - height - 2.0).max(0.0);

        MenuGeometry {
            x,
            y,
            width,
            height,
            item_height: MENU_ITEM_HEIGHT,
            section_gap: MENU_SECTION_GAP,
        }
    }
}

// ── Menu GPU rendering ────────────────────────────────────────────────

/// Render the context menu as an overlay. Call this as the final render pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_context_menu(
    menu: &ContextMenu,
    renderer: &mut crate::grid_renderer::GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;
    use crate::grid_renderer::ColorVertex;
    use glyphon::{
        Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
        TextBounds,
    };
    use therminal_core::palette::Color as PaletteColor;
    use wgpu::util::DeviceExt;

    let sw = surface_width as f32;
    let sh = surface_height as f32;
    let geo = menu.geometry(sw, sh);

    // ── Background rect (dark surface with slight transparency) ─────────
    let bg_color: [f32; 4] = {
        let c = PaletteColor::VOID_1;
        [
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            0.95,
        ]
    };

    let border_color: [f32; 4] = {
        let c = PaletteColor::LINE;
        [
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            0.8,
        ]
    };

    let highlight_color: [f32; 4] = {
        let c = PaletteColor::PLATE_STRONG;
        [
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            0.9,
        ]
    };

    let separator_color: [f32; 4] = {
        let c = PaletteColor::LINE;
        [
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            0.4,
        ]
    };

    let mut verts: Vec<ColorVertex> = Vec::new();

    // Border (draw slightly larger rect behind the background).
    verts.extend_from_slice(&pixel_rect_to_ndc(
        geo.x - MENU_BORDER,
        geo.y - MENU_BORDER,
        geo.width + MENU_BORDER * 2.0,
        geo.height + MENU_BORDER * 2.0,
        sw,
        sh,
        border_color,
    ));

    // Background fill.
    verts.extend_from_slice(&pixel_rect_to_ndc(
        geo.x, geo.y, geo.width, geo.height, sw, sh, bg_color,
    ));

    // Highlight rect for selected item.
    if let Some(sel_idx) = menu.selected_index {
        let mut y_offset = geo.y + MENU_PADDING_Y;
        let mut flat_idx = 0;
        'outer: for (section_idx, section) in menu.sections.iter().enumerate() {
            if section_idx > 0 {
                y_offset += MENU_SECTION_GAP;
            }
            for _item in &section.0 {
                if flat_idx == sel_idx {
                    verts.extend_from_slice(&pixel_rect_to_ndc(
                        geo.x + 2.0,
                        y_offset,
                        geo.width - 4.0,
                        MENU_ITEM_HEIGHT,
                        sw,
                        sh,
                        highlight_color,
                    ));
                    break 'outer;
                }
                y_offset += MENU_ITEM_HEIGHT;
                flat_idx += 1;
            }
        }
    }

    // Section separators.
    {
        let mut y_offset = geo.y + MENU_PADDING_Y;
        for (section_idx, section) in menu.sections.iter().enumerate() {
            if section_idx > 0 {
                let sep_y = y_offset - MENU_SECTION_GAP / 2.0;
                verts.extend_from_slice(&pixel_rect_to_ndc(
                    geo.x + MENU_PADDING_X,
                    sep_y,
                    geo.width - MENU_PADDING_X * 2.0,
                    1.0,
                    sw,
                    sh,
                    separator_color,
                ));
            }
            y_offset += section.0.len() as f32 * MENU_ITEM_HEIGHT;
            if section_idx < menu.sections.len() - 1 {
                y_offset += MENU_SECTION_GAP;
            }
        }
    }

    // Submit background/border/highlight rects.
    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("menu_bg_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("menu_bg_encoder"),
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("menu_bg_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }

    queue.submit(std::iter::once(encoder.finish()));

    // ── Menu text ───────────────────────────────────────────────────────
    let font_size = (MENU_ITEM_HEIGHT * 0.52).max(11.0);
    let line_height = MENU_ITEM_HEIGHT;
    let metrics = Metrics::new(font_size, line_height);

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let label_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        240,
    );
    let label_disabled_color = GlyphColor::rgba(
        PaletteColor::INK_DIM.r,
        PaletteColor::INK_DIM.g,
        PaletteColor::INK_DIM.b,
        180,
    );
    let hint_color = GlyphColor::rgba(
        PaletteColor::INK_DIM.r,
        PaletteColor::INK_DIM.g,
        PaletteColor::INK_DIM.b,
        200,
    );

    // Build text buffers for all items.
    let mut label_buffers: Vec<Buffer> = Vec::new();
    let mut hint_buffers: Vec<Buffer> = Vec::new();
    // Track which items have hints (for pairing with hint_buffers).
    let mut hint_positions: Vec<(f32, f32)> = Vec::new();

    let mut y_offset = geo.y + MENU_PADDING_Y;

    for (section_idx, section) in menu.sections.iter().enumerate() {
        if section_idx > 0 {
            y_offset += MENU_SECTION_GAP;
        }
        for item in &section.0 {
            // Label buffer.
            let mut label_buf = Buffer::new(&mut renderer.font_system, metrics);
            label_buf.set_size(
                &mut renderer.font_system,
                Some(geo.width - MENU_PADDING_X * 2.0),
                Some(MENU_ITEM_HEIGHT),
            );
            label_buf.set_text(
                &mut renderer.font_system,
                item.label,
                &Attrs::new()
                    .family(Family::Name(&renderer.font_config.family))
                    .color(if item.enabled {
                        label_color
                    } else {
                        label_disabled_color
                    }),
                Shaping::Basic,
                None,
            );
            label_buf.shape_until_scroll(&mut renderer.font_system, false);
            label_buffers.push(label_buf);

            // Hotkey hint buffer (if present).
            if let Some(ref hint_text) = item.hotkey_hint {
                let mut hint_buf = Buffer::new(&mut renderer.font_system, metrics);
                hint_buf.set_size(
                    &mut renderer.font_system,
                    Some(geo.width - MENU_PADDING_X * 2.0),
                    Some(MENU_ITEM_HEIGHT),
                );
                hint_buf.set_text(
                    &mut renderer.font_system,
                    hint_text,
                    &Attrs::new()
                        .family(Family::Name(&renderer.font_config.family))
                        .color(hint_color),
                    Shaping::Basic,
                    None,
                );
                hint_buf.shape_until_scroll(&mut renderer.font_system, false);

                let hint_text_width = hint_buf
                    .layout_runs()
                    .next()
                    .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
                    .unwrap_or(0.0);
                let hint_x = geo.x + geo.width - MENU_PADDING_X - hint_text_width;
                hint_positions.push((hint_x, y_offset));
                hint_buffers.push(hint_buf);
            }

            y_offset += MENU_ITEM_HEIGHT;
        }
    }

    // Build TextAreas from the stored buffers.
    let mut text_areas: Vec<TextArea<'_>> = Vec::new();
    let mut y_offset2 = geo.y + MENU_PADDING_Y;
    let mut label_idx = 0;
    let mut hint_idx = 0;

    for (section_idx, section) in menu.sections.iter().enumerate() {
        if section_idx > 0 {
            y_offset2 += MENU_SECTION_GAP;
        }
        for item in &section.0 {
            let color = if item.enabled {
                label_color
            } else {
                label_disabled_color
            };

            text_areas.push(TextArea {
                buffer: &label_buffers[label_idx],
                left: geo.x + MENU_PADDING_X,
                top: y_offset2,
                scale: 1.0,
                bounds,
                default_color: color,
                custom_glyphs: &[],
            });
            label_idx += 1;

            if item.hotkey_hint.is_some() {
                let (hint_x, _hint_y) = hint_positions[hint_idx];
                text_areas.push(TextArea {
                    buffer: &hint_buffers[hint_idx],
                    left: hint_x,
                    top: y_offset2,
                    scale: 1.0,
                    bounds,
                    default_color: hint_color,
                    custom_glyphs: &[],
                });
                hint_idx += 1;
            }

            y_offset2 += MENU_ITEM_HEIGHT;
        }
    }

    // Prepare and render text.
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("menu_text_encoder"),
    });

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("menu text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("menu_text_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        if let Err(e) = renderer.overlay_text_renderer.render(
            &renderer.overlay_atlas,
            &renderer.viewport,
            &mut pass,
        ) {
            tracing::warn!("menu text render failed: {}", e);
        }
    }

    queue.submit(std::iter::once(encoder.finish()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_hotspot_and_pane_menu_prepends_hotspot_sections() {
        use therminal_terminal::hotspot_detection::HotspotKind;

        let pane_menu = build_pane_menu(3, &[], (0.0, 0.0));
        let pane_section_count = pane_menu.sections.len();
        let pane_item_count = pane_menu.item_count();

        let hotspot = build_hotspot_palette(
            HotspotKind::FilePath,
            "src/main.rs:42".to_string(),
            false,
            (0.0, 0.0),
        );
        let hotspot_section_count = hotspot.sections.len();
        let hotspot_item_count = hotspot.item_count();

        // Simulate the merge done in the right-click handler.
        let mut merged = pane_menu;
        let mut new_sections = hotspot.sections;
        new_sections.append(&mut merged.sections);
        merged.sections = new_sections;

        assert_eq!(
            merged.sections.len(),
            hotspot_section_count + pane_section_count
        );
        assert_eq!(merged.item_count(), hotspot_item_count + pane_item_count);
        // First item should be the hotspot "Open in editor" action.
        assert_eq!(merged.flat_items()[0].label, "Open in editor");
        // Pane actions should still be present after the hotspot ones.
        assert!(
            merged
                .flat_items()
                .iter()
                .any(|i| i.label == "Split Horizontal")
        );
    }

    #[test]
    fn pane_menu_alone_has_no_hotspot_actions() {
        let menu = build_pane_menu(1, &[], (0.0, 0.0));
        assert!(
            !menu
                .flat_items()
                .iter()
                .any(|i| i.label == "Open in editor")
        );
    }

    #[test]
    fn pane_menu_has_copy_pane_id_with_numeric_payload() {
        let menu = build_pane_menu(7, &[], (0.0, 0.0));
        let item = menu
            .flat_items()
            .into_iter()
            .find(|i| i.label == "Copy pane ID")
            .expect("Copy pane ID entry present");
        match &item.action {
            KeyAction::HotspotCopy(s) => assert_eq!(s, "7"),
            other => panic!("expected HotspotCopy, got {other:?}"),
        }
    }

    #[test]
    fn directory_hotspot_palette_uses_folder_actions() {
        use therminal_terminal::hotspot_detection::HotspotKind;
        let menu = build_hotspot_palette(
            HotspotKind::FilePath,
            "/home/me/projects".to_string(),
            true,
            (0.0, 0.0),
        );
        let labels: Vec<&str> = menu.flat_items().iter().map(|i| i.label).collect();
        assert_eq!(
            labels,
            vec!["Open in new pane", "Open in file manager", "Copy path"]
        );
        // Default (top) action must be the in-pane spawn carrying the
        // clicked path verbatim.
        match &menu.flat_items()[0].action {
            KeyAction::HotspotOpenFolderInPane(p) => assert_eq!(p, "/home/me/projects"),
            other => panic!("expected HotspotOpenFolderInPane, got {other:?}"),
        }
        match &menu.flat_items()[1].action {
            KeyAction::HotspotOpenFolderInFileManager(p) => assert_eq!(p, "/home/me/projects"),
            other => panic!("expected HotspotOpenFolderInFileManager, got {other:?}"),
        }
        // Crucially: the editor action MUST NOT appear for directories.
        assert!(
            !menu
                .flat_items()
                .iter()
                .any(|i| matches!(i.action, KeyAction::HotspotOpenInEditor(_))),
            "directory menu must not contain Open in editor"
        );
    }

    #[test]
    fn file_hotspot_palette_unchanged_when_is_dir_false() {
        use therminal_terminal::hotspot_detection::HotspotKind;
        let menu = build_hotspot_palette(
            HotspotKind::FilePath,
            "src/main.rs:42".to_string(),
            false,
            (0.0, 0.0),
        );
        let labels: Vec<&str> = menu.flat_items().iter().map(|i| i.label).collect();
        assert!(labels.contains(&"Open in editor"));
        assert!(!labels.contains(&"Open in new pane"));
    }

    #[test]
    fn error_location_ignores_is_dir_flag() {
        // ErrorLocation hotspots are never directories — even if some
        // bug-ish code path passed `is_dir = true`, we still want the
        // editor menu so the user can jump to the offending line.
        use therminal_terminal::hotspot_detection::HotspotKind;
        let menu = build_hotspot_palette(
            HotspotKind::ErrorLocation,
            "src/lib.rs:10:5".to_string(),
            true,
            (0.0, 0.0),
        );
        assert!(
            menu.flat_items()
                .iter()
                .any(|i| matches!(i.action, KeyAction::HotspotOpenInEditor(_)))
        );
    }
}
