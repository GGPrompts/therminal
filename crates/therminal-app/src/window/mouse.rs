//! Mouse event handling: clicks, drags, selections, scroll, cursor motion.
//!
//! All mouse input routing lives here, including selection state management,
//! pixel-to-grid coordinate conversion, and header hit-testing.

use std::io::Write as IoWrite;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use tracing::warn;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use crate::pane::{LayoutNode, PaneId};
use alacritty_terminal::grid::Dimensions;
use therminal_terminal::input::{self, MouseButton as InputMouseButton};

use super::chrome::{HEADER_BUTTON_MARGIN, HEADER_BUTTON_WIDTH};
use super::App;

// ── Header button actions ──────────────────────────────────────────────

/// Action resulting from a click in a pane header.
pub(crate) enum HeaderAction {
    /// Focus the pane (click anywhere in header that isn't a button).
    Focus(PaneId),
    /// Close the pane (click on X button).
    Close(PaneId),
    /// Split the pane horizontally (click on H button).
    SplitH(PaneId),
    /// Split the pane vertically (click on V button).
    SplitV(PaneId),
}

// ── Coordinate conversion ──────────────────────────────────────────────

impl App {
    /// Convert physical pixel coordinates to terminal grid (col, row) for the focused pane.
    #[allow(dead_code)]
    pub(crate) fn pixel_to_grid(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let focused = self.workspaces.as_ref()?.focused_pane()?;
        self.pixel_to_grid_for_pane(px, py, focused)
    }

    /// Convert physical pixel coordinates to terminal grid (col, row) for a specific pane.
    pub(crate) fn pixel_to_grid_for_pane(
        &self,
        px: f64,
        py: f64,
        pane_id: PaneId,
    ) -> Option<(usize, usize)> {
        let renderer = self.grid_renderer.as_ref()?;
        let layout = self.workspaces.as_ref().map(|wm| wm.layout())?;
        let pane_count = layout.pane_count();
        let pane = layout.find_pane(pane_id)?;

        let vp = pane.viewport;
        let header_h = crate::pane::effective_header_height(pane_count);
        let col = ((px as f32 - vp.x() - renderer.padding_x()) / renderer.cell_width).floor();
        let row =
            ((py as f32 - vp.y() - renderer.padding_y() - header_h) / renderer.cell_height).floor();
        if col < 0.0 || row < 0.0 {
            return None;
        }
        let col = col as usize;
        let row = row as usize;

        let term_guard = pane.term.lock();
        let max_col = term_guard.columns().saturating_sub(1);
        let max_row = term_guard.screen_lines().saturating_sub(1);
        Some((col.min(max_col), row.min(max_row)))
    }

    /// Determine the `Side` of a cell the cursor is on based on sub-cell pixel position.
    fn pixel_to_side(&self, px: f64, pane_id: PaneId) -> Side {
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return Side::Left,
        };
        let layout = match self.workspaces.as_ref().map(|wm| wm.layout()) {
            Some(l) => l,
            None => return Side::Left,
        };
        let pane = match layout.find_pane(pane_id) {
            Some(p) => p,
            None => return Side::Left,
        };
        let vp = pane.viewport;
        let cell_x = (px as f32 - vp.x() - renderer.padding_x()) % renderer.cell_width;
        if cell_x > renderer.cell_width / 2.0 {
            Side::Right
        } else {
            Side::Left
        }
    }

    // ── Pane lookup ────────────────────────────────────────────────────

    /// Find which pane contains the given physical pixel coordinates.
    pub(crate) fn pane_at_position(&self, px: f64, py: f64) -> Option<PaneId> {
        let layout = self.workspaces.as_ref().map(|wm| wm.layout())?;
        self.find_pane_at(layout, px as f32, py as f32)
    }

    fn find_pane_at(&self, node: &LayoutNode, px: f32, py: f32) -> Option<PaneId> {
        use therminal_core::geometry::Point;
        match node {
            LayoutNode::Leaf(pane) => {
                if pane.viewport.contains(Point::new(px, py)) {
                    Some(pane.id)
                } else {
                    None
                }
            }
            LayoutNode::Split { first, second, .. } => self
                .find_pane_at(first, px, py)
                .or_else(|| self.find_pane_at(second, px, py)),
            LayoutNode::Empty => None,
        }
    }

    /// Test if a click at (px, py) lands on a pane header and which button (if any).
    /// Returns `None` if not in a header or if there's only one pane (headers hidden).
    pub(crate) fn header_hit_test(&self, px: f64, py: f64) -> Option<HeaderAction> {
        let layout = self.workspaces.as_ref().map(|wm| wm.layout())?;
        let pane_count = layout.pane_count();
        if pane_count <= 1 {
            return None;
        }

        let header_h = crate::pane::PANE_HEADER_HEIGHT;
        let px = px as f32;
        let py = py as f32;

        // Find which pane's viewport contains this point.
        let pane_id = self.find_pane_at(layout, px, py)?;
        let pane = layout.find_pane(pane_id)?;
        let vp = pane.viewport;

        // Check if click is within the header strip (top `header_h` pixels of viewport).
        let header_top = vp.y();
        let header_bottom = vp.y() + header_h;
        if py < header_top || py >= header_bottom {
            return None;
        }

        // Button hit regions (right-aligned): [H] [V] [X]
        let btn_x_close = vp.x() + vp.width() - HEADER_BUTTON_MARGIN - HEADER_BUTTON_WIDTH;
        let btn_x_vsplit = btn_x_close - HEADER_BUTTON_WIDTH;
        let btn_x_hsplit = btn_x_vsplit - HEADER_BUTTON_WIDTH;

        if px >= btn_x_close && px < btn_x_close + HEADER_BUTTON_WIDTH {
            Some(HeaderAction::Close(pane_id))
        } else if px >= btn_x_vsplit && px < btn_x_vsplit + HEADER_BUTTON_WIDTH {
            Some(HeaderAction::SplitV(pane_id))
        } else if px >= btn_x_hsplit && px < btn_x_hsplit + HEADER_BUTTON_WIDTH {
            Some(HeaderAction::SplitH(pane_id))
        } else {
            // Anywhere else in the header -> focus.
            Some(HeaderAction::Focus(pane_id))
        }
    }

    // ── Selection helpers ──────────────────────────────────────────────

    /// Start a new selection on the given pane at the specified grid position.
    pub(crate) fn start_selection(
        &mut self,
        pane_id: PaneId,
        col: usize,
        row: usize,
        ty: SelectionType,
    ) {
        // Compute side before borrowing layout mutably.
        let side = self.pixel_to_side(self.cursor_position.map(|(x, _)| x).unwrap_or(0.0), pane_id);
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(pane_id) {
            Some(p) => p,
            None => return,
        };
        let point = Point::new(Line(row as i32), Column(col));
        let selection = Selection::new(ty, point, side);
        pane.term.lock().selection = Some(selection);
        self.selection_in_progress = true;
        self.selection_pane = Some(pane_id);
    }

    /// Update the current in-progress selection to a new grid position.
    pub(crate) fn update_selection(&mut self, pane_id: PaneId, col: usize, row: usize) {
        // Compute side before borrowing layout mutably.
        let side = self.pixel_to_side(self.cursor_position.map(|(x, _)| x).unwrap_or(0.0), pane_id);
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(pane_id) {
            Some(p) => p,
            None => return,
        };
        let point = Point::new(Line(row as i32), Column(col));
        let mut term_guard = pane.term.lock();
        if let Some(ref mut selection) = term_guard.selection {
            selection.update(point, side);
        }
    }

    /// Finalize the selection: extract selected text and copy to clipboard.
    pub(crate) fn finalize_selection(&mut self) {
        self.selection_in_progress = false;
        let pane_id = match self.selection_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.workspaces.as_ref().map(|wm| wm.layout()) {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(pane_id) {
            Some(p) => p,
            None => return,
        };
        let term_guard = pane.term.lock();
        if let Some(text) = term_guard.selection_to_string() {
            if !text.is_empty() {
                crate::clipboard::copy_to_clipboard(&text);
            }
        }
    }

    // ── Mouse event handlers ───────────────────────────────────────────

    /// Handle mouse button press/release -- routes to the pane under the pointer.
    ///
    /// Left-button handling:
    ///   - In mouse-reporting mode: forward press/release to the PTY.
    ///   - Otherwise: drive text selection (single / double / triple click,
    ///     shift-extend, drag).
    pub(crate) fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let (px, py) = match self.cursor_position {
            Some(pos) => pos,
            None => return,
        };

        // For button release during a drag, route to the pane where the drag
        // started so mouse-reporting apps don't get mismatched press/release.
        let target_pane = if state == ElementState::Released && button == MouseButton::Left {
            self.mouse_drag_pane
                .or_else(|| self.pane_at_position(px, py))
        } else {
            self.pane_at_position(px, py)
        };
        let target_pane = match target_pane {
            Some(id) => id,
            None => return,
        };

        let (col, row) = match self.pixel_to_grid_for_pane(px, py, target_pane) {
            Some(pos) => pos,
            None => return,
        };

        if button == MouseButton::Left {
            self.mouse_left_held = state == ElementState::Pressed;
            if state == ElementState::Pressed {
                self.mouse_drag_pane = Some(target_pane);
            } else {
                self.mouse_drag_pane = None;
            }
        }

        let mode = self.pane_term_mode(target_pane);
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        // ── Selection logic (only when the terminal is NOT in mouse mode) ──
        if button == MouseButton::Left && !mouse_mode {
            if state == ElementState::Pressed {
                let shift_held = self.modifiers.state().shift_key();

                // Detect multi-click (double/triple) by timing and position.
                let now = Instant::now();
                let same_pos = self.last_click_pos == Some((col, row));
                let quick = self
                    .last_click_time
                    .is_some_and(|t| now.duration_since(t) < Duration::from_millis(300));

                if quick && same_pos && self.click_count < 3 {
                    self.click_count += 1;
                } else {
                    self.click_count = 1;
                }
                self.last_click_time = Some(now);
                self.last_click_pos = Some((col, row));

                if shift_held && self.selection_pane == Some(target_pane) {
                    // Shift+click: extend existing selection.
                    self.update_selection(target_pane, col, row);
                } else {
                    // Start a new selection of the appropriate type.
                    let ty = match self.click_count {
                        2 => SelectionType::Semantic,
                        3 => SelectionType::Lines,
                        _ => SelectionType::Simple,
                    };
                    self.start_selection(target_pane, col, row, ty);
                }

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            } else {
                // Button released -- finalize selection (copy to clipboard).
                self.finalize_selection();

                // Single click on a hyperlinked cell: open the URL.
                // Single click on a hotspot cell: open the action palette.
                if self.click_count == 1 && self.last_click_pos == Some((col, row)) {
                    if let Some(url) = self.hyperlink_at(target_pane, row, col) {
                        self.open_hyperlink(&url);
                    } else {
                        self.handle_hotspot_click(target_pane, row, col);
                    }
                }
            }
            return;
        }

        // ── Mouse-reporting mode: forward to PTY ────────────────────────
        if !mouse_mode {
            return;
        }

        let input_button = match button {
            MouseButton::Left => InputMouseButton::Left,
            MouseButton::Middle => InputMouseButton::Middle,
            MouseButton::Right => InputMouseButton::Right,
            _ => return,
        };

        let mods = self.input_mods();
        let bytes = match state {
            ElementState::Pressed => input::encode_mouse_press(input_button, col, row, &mods),
            ElementState::Released => input::encode_mouse_release(input_button, col, row, &mods),
        };
        self.pty_write_to_pane(&bytes, target_pane);
    }

    /// Handle mouse motion -- routes drag events to the pane where the drag started,
    /// and motion events to the pane under the pointer.
    pub(crate) fn handle_cursor_moved(&mut self, position: winit::dpi::PhysicalPosition<f64>) {
        let (px, py) = (position.x, position.y);
        self.cursor_position = Some((px, py));

        // ── Separator drag in progress ────────────────────────────────────
        if self.separator_drag.is_some() {
            self.update_separator_drag(px as f32, py as f32);
            return;
        }

        // ── Separator hover detection (cursor icon) ──────────────────────
        if !self.mouse_left_held {
            self.update_separator_hover(px as f32, py as f32);
        }

        if self.mouse_left_held {
            // During a drag, route to the pane where the drag started so that
            // selections and drag reporting stay consistent even if the pointer
            // crosses a pane boundary.
            let target = match self.mouse_drag_pane {
                Some(id) => id,
                None => return,
            };

            let (col, row) = match self.pixel_to_grid_for_pane(px, py, target) {
                Some(pos) => pos,
                None => return,
            };

            // If a selection drag is in progress, update the selection endpoint.
            if self.selection_in_progress {
                self.update_selection(target, col, row);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }

            let mode = self.pane_term_mode(target);
            if mode.contains(TermMode::MOUSE_DRAG) || mode.contains(TermMode::MOUSE_MOTION) {
                let mods = self.input_mods();
                let bytes = input::encode_mouse_drag(InputMouseButton::Left, col, row, &mods);
                self.pty_write_to_pane(&bytes, target);
            }
        } else {
            // No button held -- route motion to the pane under the pointer.
            let target = match self.pane_at_position(px, py) {
                Some(id) => id,
                None => return,
            };

            let (col, row) = match self.pixel_to_grid_for_pane(px, py, target) {
                Some(pos) => pos,
                None => return,
            };

            // Hyperlink hover: show pointer cursor when over a hyperlinked cell.
            if !self.separator_cursor_active {
                self.update_hyperlink_hover(target, row, col);
            }

            let mode = self.pane_term_mode(target);
            if mode.contains(TermMode::MOUSE_MOTION) {
                let mods = self.input_mods();
                let bytes = input::encode_mouse_motion(col, row, &mods);
                self.pty_write_to_pane(&bytes, target);
            }
        }
    }

    /// Handle mouse scroll -- routes to the pane under the pointer.
    pub(crate) fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        // Resolve the hovered pane from cursor position.
        let (px, py) = match self.cursor_position {
            Some(pos) => pos,
            None => return,
        };
        let target_pane = match self.pane_at_position(px, py) {
            Some(id) => id,
            None => return,
        };

        let (col, row) = self
            .pixel_to_grid_for_pane(px, py, target_pane)
            .unwrap_or((0, 0));

        let lines = match delta {
            MouseScrollDelta::LineDelta(_x, y) => y,
            MouseScrollDelta::PixelDelta(pos) => {
                let cell_h = self
                    .grid_renderer
                    .as_ref()
                    .map(|r| r.cell_height)
                    .unwrap_or(20.0);
                (pos.y as f32) / cell_h
            }
        };

        if lines.abs() < 0.01 {
            return;
        }

        let mode = self.pane_term_mode(target_pane);
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        if mouse_mode {
            let mods = self.input_mods();
            let button = if lines > 0.0 {
                InputMouseButton::ScrollUp
            } else {
                InputMouseButton::ScrollDown
            };
            let steps = lines.abs().ceil() as usize;
            let mut seq = Vec::new();
            for _ in 0..steps {
                seq.extend_from_slice(&input::encode_mouse_press(button, col, row, &mods));
            }
            self.pty_write_to_pane(&seq, target_pane);
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let steps = lines.abs().ceil() as usize;
            let arrow = if lines > 0.0 { b"\x1b[A" } else { b"\x1b[B" };
            let mut seq = Vec::with_capacity(steps * 3);
            for _ in 0..steps {
                seq.extend_from_slice(arrow);
            }
            self.pty_write_to_pane(&seq, target_pane);
        } else {
            // Normal scrollback -- scroll the hovered pane.
            if let Some(layout) = self.workspaces.as_ref().map(|wm| wm.layout()) {
                if let Some(pane) = layout.find_pane(target_pane) {
                    let scroll_lines = (lines * 3.0).round() as i32;
                    let mut term_guard = pane.term.lock();
                    term_guard.scroll_display(Scroll::Delta(scroll_lines));
                }
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    // ── PTY helpers (used by mouse handlers) ───────────────────────────

    /// Get focused pane's TermMode.
    pub(crate) fn focused_term_mode(&self) -> TermMode {
        let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
            Some(id) => id,
            None => return TermMode::empty(),
        };
        self.pane_term_mode(focused)
    }

    /// Get a specific pane's TermMode.
    pub(crate) fn pane_term_mode(&self, pane_id: PaneId) -> TermMode {
        let layout = match self.workspaces.as_ref().map(|wm| wm.layout()) {
            Some(l) => l,
            None => return TermMode::empty(),
        };
        match layout.find_pane(pane_id) {
            Some(pane) => *pane.term.lock().mode(),
            None => TermMode::empty(),
        }
    }

    /// Get input modifiers from the current winit modifier state.
    pub(crate) fn input_mods(&self) -> therminal_terminal::input::Modifiers {
        let state = self.modifiers.state();
        therminal_terminal::input::Modifiers {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
        }
    }

    /// Write bytes to the focused pane's PTY.
    #[allow(dead_code)]
    pub(crate) fn pty_write(&mut self, bytes: &[u8]) {
        let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
            Some(id) => id,
            None => return,
        };
        self.pty_write_to_pane(bytes, focused);
    }

    /// Write bytes to a specific pane's PTY.
    pub(crate) fn pty_write_to_pane(&mut self, bytes: &[u8], pane_id: PaneId) {
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        if let Some(pane) = layout.find_pane_mut(pane_id) {
            if let Err(e) = pane.pty_writer.write_all(bytes) {
                warn!("Failed to write to pane {} PTY: {e}", pane.id);
            }
        }
    }

    /// Get the current terminal mode flags from the focused pane.
    #[allow(dead_code)]
    pub(crate) fn term_mode(&self) -> TermMode {
        self.focused_term_mode()
    }

    // ── Hyperlink hover and click helpers ───────────────────────────────

    /// Look up the hyperlink URL at a given grid (row, col) for a specific pane.
    fn hyperlink_at(&self, pane_id: PaneId, row: usize, col: usize) -> Option<String> {
        self.grid_renderer
            .as_ref()
            .and_then(|r| r.hyperlink_map.get(&(pane_id, row, col)).cloned())
    }

    /// Update cursor icon based on whether the hovered cell has a hyperlink or hotspot.
    fn update_hyperlink_hover(&mut self, pane_id: PaneId, row: usize, col: usize) {
        use winit::window::CursorIcon;

        let on_link = self.hyperlink_at(pane_id, row, col).is_some();
        let on_hotspot = self.hotspot_at(pane_id, row, col).is_some();
        let want_pointer = on_link || on_hotspot;
        if want_pointer && !self.hyperlink_cursor_active {
            self.hyperlink_cursor_active = true;
            if let Some(w) = self.window.as_ref() {
                w.set_cursor(CursorIcon::Pointer);
            }
        } else if !want_pointer && self.hyperlink_cursor_active {
            self.hyperlink_cursor_active = false;
            if let Some(w) = self.window.as_ref() {
                w.set_cursor(CursorIcon::Default);
            }
        }
    }

    /// Look up the hotspot at a given grid (row, col) for a specific pane.
    fn hotspot_at(
        &self,
        pane_id: PaneId,
        row: usize,
        col: usize,
    ) -> Option<(crate::hotspot_detection::HotspotKind, String)> {
        self.grid_renderer
            .as_ref()
            .and_then(|r| r.hotspot_map.get(&(pane_id, row, col)).cloned())
    }

    /// Handle a click on a hotspot cell: open an action palette.
    fn handle_hotspot_click(&mut self, pane_id: PaneId, row: usize, col: usize) -> bool {
        let (kind, text) = match self.hotspot_at(pane_id, row, col) {
            Some(h) => h,
            None => return false,
        };
        let (px, py) = match self.cursor_position {
            Some(pos) => pos,
            None => return false,
        };
        let menu = crate::menu::build_hotspot_palette(kind, text, (px as f32, py as f32));
        self.active_menu = Some(menu);
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
        true
    }

    /// Open a hyperlink URL using the platform default handler.
    fn open_hyperlink(&self, url: &str) {
        if let Err(e) = open::that(url) {
            warn!("Failed to open hyperlink {url}: {e}");
        }
    }

    // ── Separator drag helpers ────────────────────────────────────────

    /// Hit-tolerance in pixels for separator detection.
    const SEPARATOR_HIT_TOLERANCE: f32 = 4.0;

    /// Compute the layout area rect (window minus status bar and tab bar).
    fn layout_area_rect(&self) -> Option<therminal_core::geometry::Rect> {
        let gpu = self.gpu.as_ref()?;
        let tab_bar_h = crate::pane::effective_tab_bar_height(self.config.general.show_tab_bar);
        Some(therminal_core::geometry::Rect::new(
            0.0,
            tab_bar_h,
            gpu.config.width as f32,
            gpu.config.height as f32
                - crate::pane::effective_status_bar_height(self.config.general.show_status_bar)
                - tab_bar_h,
        ))
    }

    /// Test if `(px, py)` is near a separator and return hit info.
    pub(crate) fn separator_hit(
        &self,
        px: f32,
        py: f32,
    ) -> Option<(
        Vec<bool>,
        crate::pane::SplitDirection,
        therminal_core::geometry::Rect,
    )> {
        let layout = self.workspaces.as_ref().map(|wm| wm.layout())?;
        let area = self.layout_area_rect()?;
        layout.separator_hit_test(px, py, Self::SEPARATOR_HIT_TOLERANCE, area)
    }

    /// Update cursor icon based on separator hover state.
    fn update_separator_hover(&mut self, px: f32, py: f32) {
        use winit::window::CursorIcon;

        let hit = self.separator_hit(px, py);
        match hit {
            Some((_, dir, _)) => {
                if !self.separator_cursor_active {
                    self.separator_cursor_active = true;
                    let icon = match dir {
                        crate::pane::SplitDirection::Horizontal => CursorIcon::EwResize,
                        crate::pane::SplitDirection::Vertical => CursorIcon::NsResize,
                    };
                    if let Some(w) = self.window.as_ref() {
                        w.set_cursor(icon);
                    }
                }
            }
            None => {
                if self.separator_cursor_active {
                    self.separator_cursor_active = false;
                    if let Some(w) = self.window.as_ref() {
                        w.set_cursor(CursorIcon::Default);
                    }
                }
            }
        }
    }

    /// Try to start a separator drag at `(px, py)`. Returns true if a drag was started.
    pub(crate) fn try_start_separator_drag(&mut self, px: f32, py: f32) -> bool {
        use super::SeparatorDrag;
        use winit::window::CursorIcon;

        if let Some((path, direction, parent_rect)) = self.separator_hit(px, py) {
            let icon = match direction {
                crate::pane::SplitDirection::Horizontal => CursorIcon::EwResize,
                crate::pane::SplitDirection::Vertical => CursorIcon::NsResize,
            };
            if let Some(w) = self.window.as_ref() {
                w.set_cursor(icon);
            }
            self.separator_cursor_active = true;
            self.separator_drag = Some(SeparatorDrag {
                path,
                direction,
                parent_rect,
            });
            true
        } else {
            false
        }
    }

    /// Update the separator ratio during a drag.
    fn update_separator_drag(&mut self, px: f32, py: f32) {
        let (path, direction, parent_rect) = {
            let drag = match self.separator_drag.as_ref() {
                Some(d) => d,
                None => return,
            };
            (drag.path.clone(), drag.direction, drag.parent_rect)
        };

        // Compute new ratio from mouse position relative to parent rect.
        let new_ratio = match direction {
            crate::pane::SplitDirection::Horizontal => {
                let usable = parent_rect.width() - crate::pane::SEPARATOR_GAP;
                if usable <= 0.0 {
                    return;
                }
                (px - parent_rect.x()) / usable
            }
            crate::pane::SplitDirection::Vertical => {
                let usable = parent_rect.height() - crate::pane::SEPARATOR_GAP;
                if usable <= 0.0 {
                    return;
                }
                (py - parent_rect.y()) / usable
            }
        };

        let new_ratio = new_ratio.clamp(0.1, 0.9);

        // Compute area rect before borrowing layout mutably.
        let area = self.layout_area_rect();
        if let Some(layout) = self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            layout.set_ratio_at_path(&path, new_ratio);
            // Re-layout and resize panes.
            if let Some(area) = area {
                layout.layout(area);
            }
        }
        // Resize panes (needs shared borrow of both layout and renderer).
        if let (Some(layout), Some(renderer)) = (
            self.workspaces.as_mut().map(|wm| wm.layout_mut()),
            self.grid_renderer.as_ref(),
        ) {
            layout.resize_all_panes(renderer);
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// End a separator drag and restore cursor.
    pub(crate) fn end_separator_drag(&mut self) {
        self.separator_drag = None;
        // Keep resize cursor if still hovering over a separator.
        if let Some((px, py)) = self.cursor_position {
            self.update_separator_hover(px as f32, py as f32);
        } else {
            self.separator_cursor_active = false;
            if let Some(w) = self.window.as_ref() {
                w.set_cursor(winit::window::CursorIcon::Default);
            }
        }
    }

    /// Handle double-click on separator: reset to 50/50.
    pub(crate) fn try_separator_double_click(&mut self, px: f32, py: f32) -> bool {
        if let Some((path, _, _)) = self.separator_hit(px, py) {
            let area = self.layout_area_rect();
            if let Some(layout) = self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
                layout.set_ratio_at_path(&path, 0.5);
                if let Some(area) = area {
                    layout.layout(area);
                }
            }
            if let (Some(wm), Some(renderer)) =
                (self.workspaces.as_mut(), self.grid_renderer.as_ref())
            {
                wm.layout_mut().resize_all_panes(renderer);
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
            true
        } else {
            false
        }
    }
}
