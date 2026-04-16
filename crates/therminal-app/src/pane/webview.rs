//! WebView pane backend — embeds platform-native webviews via wry (tn-s5vj).
//!
//! v1 is "sidecar mode": loads URLs from running servers (e.g. PocketForge
//! at localhost). The webview is a child surface of the winit window,
//! positioned to match the pane's viewport rect. wgpu renders the pane
//! header/chrome around it; the content area is filled by the native
//! webview sitting on top of the wgpu surface.
//!
//! ## Platform webview engines
//! - Linux: WebKitGTK (requires `libwebkit2gtk-4.1-dev`)
//! - Windows: WebView2 (Edge Chromium, ships with Win10+)
//! - macOS: WKWebView (system framework)

use std::collections::HashMap;
use std::sync::Arc;

use therminal_core::geometry::Rect;
use tracing::{debug, info, warn};
use winit::window::Window;
use wry::{Rect as WryRect, WebView, WebViewBuilder};

use super::PaneId;

/// Manages all wry `WebView` instances across the application.
///
/// Each WebView pane maps to one wry `WebView`. The manager handles
/// creation, destruction, repositioning, and visibility toggling.
pub struct WebViewManager {
    /// Map of pane ID to its wry webview.
    views: HashMap<PaneId, WebView>,
}

impl WebViewManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            views: HashMap::new(),
        }
    }

    /// Create a new webview for a pane, loading the given URL.
    ///
    /// The webview is positioned within the pane's content area (below the
    /// header). Returns `Ok(())` if the webview was created, or an error
    /// if wry initialization failed (missing WebKitGTK, etc.).
    pub fn create(
        &mut self,
        pane_id: PaneId,
        url: &str,
        content_rect: Rect,
        window: &Arc<Window>,
    ) -> Result<(), String> {
        if self.views.contains_key(&pane_id) {
            debug!(pane_id, "webview already exists, skipping creation");
            return Ok(());
        }

        let bounds = rect_to_wry_rect(content_rect);

        // Build the webview as a child of the winit window.
        let builder = WebViewBuilder::new()
            .with_url(url)
            .with_bounds(bounds)
            .with_focused(false);

        let webview = builder
            .build_as_child(window.as_ref())
            .map_err(|e| format!("wry WebView creation failed: {e}"))?;

        info!(pane_id, url, "created webview pane");
        self.views.insert(pane_id, webview);
        Ok(())
    }

    /// Destroy the webview for a pane.
    pub fn destroy(&mut self, pane_id: PaneId) {
        if self.views.remove(&pane_id).is_some() {
            info!(pane_id, "destroyed webview");
        }
    }

    /// Reposition and resize a webview to match an updated pane rect.
    pub fn set_bounds(&mut self, pane_id: PaneId, content_rect: Rect) {
        if let Some(wv) = self.views.get(&pane_id)
            && let Err(e) = wv.set_bounds(rect_to_wry_rect(content_rect))
        {
            warn!(pane_id, error = %e, "failed to set webview bounds");
        }
    }

    /// Show or hide a webview (e.g. on workspace switch).
    pub fn set_visible(&mut self, pane_id: PaneId, visible: bool) {
        if let Some(wv) = self.views.get(&pane_id)
            && let Err(e) = wv.set_visible(visible)
        {
            warn!(pane_id, visible, error = %e, "failed to set webview visibility");
        }
    }

    /// Focus a webview (e.g. when user clicks on the webview pane).
    #[allow(dead_code)]
    pub fn focus(&mut self, pane_id: PaneId) {
        if let Some(wv) = self.views.get(&pane_id)
            && let Err(e) = wv.focus()
        {
            debug!(pane_id, error = %e, "failed to focus webview");
        }
    }

    /// Navigate a webview to a new URL.
    #[allow(dead_code)]
    pub fn navigate(&mut self, pane_id: PaneId, url: &str) {
        if let Some(wv) = self.views.get(&pane_id)
            && let Err(e) = wv.load_url(url)
        {
            warn!(pane_id, url, error = %e, "failed to navigate webview");
        }
    }

    /// Get the current URL of a webview (returns the last navigated URL).
    #[allow(dead_code)]
    pub fn url(&self, pane_id: PaneId) -> Option<String> {
        self.views.get(&pane_id).and_then(|wv| wv.url().ok())
    }

    /// Returns true if a webview exists for this pane.
    #[allow(dead_code)]
    pub fn contains(&self, pane_id: PaneId) -> bool {
        self.views.contains_key(&pane_id)
    }

    /// Hide all webviews (e.g. for focus mode or overlay).
    #[allow(dead_code)]
    pub fn hide_all(&mut self) {
        for wv in self.views.values() {
            let _ = wv.set_visible(false);
        }
    }

    /// Show all webviews that should be visible.
    #[allow(dead_code)]
    pub fn show_all(&mut self) {
        for wv in self.views.values() {
            let _ = wv.set_visible(true);
        }
    }

    /// Returns the set of pane IDs with active webviews.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.views.keys().copied().collect()
    }

    /// Returns number of active webviews.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.views.len()
    }
}

/// Convert a therminal `Rect` to a wry `Rect` (physical pixels).
fn rect_to_wry_rect(r: Rect) -> WryRect {
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    WryRect {
        position: wry::dpi::Position::Physical(PhysicalPosition::new(r.x() as i32, r.y() as i32)),
        size: wry::dpi::Size::Physical(PhysicalSize::new(
            r.width().max(1.0) as u32,
            r.height().max(1.0) as u32,
        )),
    }
}

/// Compute the content rect for a webview pane (viewport minus header).
pub fn webview_content_rect(viewport: Rect, header_h: f32) -> Rect {
    Rect::new(
        viewport.x(),
        viewport.y() + header_h,
        viewport.width(),
        (viewport.height() - header_h).max(1.0),
    )
}
