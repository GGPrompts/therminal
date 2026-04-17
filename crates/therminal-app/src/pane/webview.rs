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
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use wry::{Rect as WryRect, WebView, WebViewBuilder};

use super::PaneId;
use crate::window::UserEvent;

/// JS injected into every webview frame (tn-gm6f).
///
/// Clicks inside a wry child HWND don't reach winit, so the page itself
/// has to cooperate: on mousedown it tells therminal to focus this pane,
/// and on shift+contextmenu it calls preventDefault and tells therminal
/// to open the pane context menu at the click point.
///
/// Guarded with `__therminal_hooks_installed` so re-injection into
/// sub-frames is a no-op. Wrapped in try/catch because `window.ipc` may
/// not exist if IPC isn't configured (e.g. tests).
const WEBVIEW_INIT_SCRIPT: &str = r#"
(function () {
  if (window.__therminal_hooks_installed) return;
  window.__therminal_hooks_installed = true;
  document.addEventListener('mousedown', function () {
    try { window.ipc.postMessage('focus'); } catch (_) {}
  }, true);
  document.addEventListener('contextmenu', function (e) {
    if (e.shiftKey) {
      e.preventDefault();
      try {
        window.ipc.postMessage('menu:' + Math.round(e.clientX) + ':' + Math.round(e.clientY));
      } catch (_) {}
    }
  }, true);
})();
"#;

/// Per-pane state owned by [`WebViewManager`]. Wraps the wry `WebView`
/// alongside therminal-specific metadata that isn't part of the browser
/// back-stack (tn-eq9g).
struct WebViewEntry {
    /// Platform-native wry webview.
    view: WebView,
    /// The URL the pane was spawned with. Preserved across navigations
    /// so `navigate_home` can route back to the origin even after the
    /// user has followed links deep into the back-stack.
    origin_url: String,
}

/// Manages all wry `WebView` instances across the application.
///
/// Each WebView pane maps to one wry `WebView`. The manager handles
/// creation, destruction, repositioning, and visibility toggling.
pub struct WebViewManager {
    /// Map of pane ID to its wry webview entry (view + origin URL).
    views: HashMap<PaneId, WebViewEntry>,
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
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<(), String> {
        if self.views.contains_key(&pane_id) {
            debug!(pane_id, "webview already exists, skipping creation");
            return Ok(());
        }

        let bounds = rect_to_wry_rect(content_rect);

        // tn-gm6f: closure captures pane_id + proxy so every IPC message
        // from this webview carries the originating pane_id back to the
        // main thread.
        let ipc_pane_id = pane_id;
        let ipc_proxy = proxy;
        let ipc_handler = move |request: wry::http::Request<String>| {
            let body = request.body().as_str();
            if body == "focus" {
                let _ = ipc_proxy.send_event(UserEvent::WebViewFocusRequest {
                    pane_id: ipc_pane_id,
                });
            } else if let Some(rest) = body.strip_prefix("menu:") {
                let mut parts = rest.split(':');
                if let (Some(xs), Some(ys)) = (parts.next(), parts.next())
                    && let (Ok(x), Ok(y)) = (xs.parse::<f64>(), ys.parse::<f64>())
                {
                    let _ = ipc_proxy.send_event(UserEvent::WebViewContextMenu {
                        pane_id: ipc_pane_id,
                        client_x: x,
                        client_y: y,
                    });
                }
            }
        };

        // Build the webview as a child of the winit window.
        let builder = WebViewBuilder::new()
            .with_url(url)
            .with_bounds(bounds)
            .with_focused(false)
            .with_initialization_script(WEBVIEW_INIT_SCRIPT)
            .with_ipc_handler(ipc_handler);

        let webview = builder
            .build_as_child(window.as_ref())
            .map_err(|e| format!("wry WebView creation failed: {e}"))?;

        info!(pane_id, url, "created webview pane");
        self.views.insert(
            pane_id,
            WebViewEntry {
                view: webview,
                origin_url: url.to_string(),
            },
        );
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
        if let Some(entry) = self.views.get(&pane_id)
            && let Err(e) = entry.view.set_bounds(rect_to_wry_rect(content_rect))
        {
            warn!(pane_id, error = %e, "failed to set webview bounds");
        }
    }

    /// Show or hide a webview (e.g. on workspace switch).
    pub fn set_visible(&mut self, pane_id: PaneId, visible: bool) {
        if let Some(entry) = self.views.get(&pane_id)
            && let Err(e) = entry.view.set_visible(visible)
        {
            warn!(pane_id, visible, error = %e, "failed to set webview visibility");
        }
    }

    /// Focus a webview (e.g. when user clicks on the webview pane).
    #[allow(dead_code)]
    pub fn focus(&mut self, pane_id: PaneId) {
        if let Some(entry) = self.views.get(&pane_id)
            && let Err(e) = entry.view.focus()
        {
            debug!(pane_id, error = %e, "failed to focus webview");
        }
    }

    /// Navigate a webview to a new URL.
    #[allow(dead_code)]
    pub fn navigate(&mut self, pane_id: PaneId, url: &str) {
        if let Some(entry) = self.views.get(&pane_id)
            && let Err(e) = entry.view.load_url(url)
        {
            warn!(pane_id, url, error = %e, "failed to navigate webview");
        }
    }

    /// Get the current URL of a webview (returns the last navigated URL).
    #[allow(dead_code)]
    pub fn url(&self, pane_id: PaneId) -> Option<String> {
        self.views
            .get(&pane_id)
            .and_then(|entry| entry.view.url().ok())
    }

    /// Returns the origin URL — the URL the pane was spawned with (tn-eq9g).
    /// Preserved across navigations so `navigate_home` can always route
    /// back to it.
    #[allow(dead_code)]
    pub fn origin_url(&self, pane_id: PaneId) -> Option<&str> {
        self.views
            .get(&pane_id)
            .map(|entry| entry.origin_url.as_str())
    }

    /// Return the WebView pane to its spawn URL (tn-eq9g). The browser's
    /// back-stack only remembers what the user visited; this method is the
    /// one-shot path back to the URL therminal opened the pane with.
    /// No-op if the pane doesn't exist in the manager.
    pub fn navigate_home(&mut self, pane_id: PaneId) {
        let Some(entry) = self.views.get(&pane_id) else {
            debug!(pane_id, "navigate_home: no webview for pane");
            return;
        };
        let origin = entry.origin_url.clone();
        if let Err(e) = entry.view.load_url(&origin) {
            warn!(pane_id, url = %origin, error = %e, "failed to navigate webview home");
        }
    }

    /// Returns true if a webview exists for this pane.
    pub fn contains(&self, pane_id: PaneId) -> bool {
        self.views.contains_key(&pane_id)
    }

    /// Hide all webviews (e.g. for focus mode or overlay).
    pub fn hide_all(&mut self) {
        for entry in self.views.values() {
            let _ = entry.view.set_visible(false);
        }
    }

    /// Show all webviews that should be visible.
    #[allow(dead_code)]
    pub fn show_all(&mut self) {
        for entry in self.views.values() {
            let _ = entry.view.set_visible(true);
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
