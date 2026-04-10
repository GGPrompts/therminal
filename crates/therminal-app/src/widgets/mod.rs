//! Widget pre-rasterization substrate (tn-npd).
//!
//! Phase 6 overlay widgets (context gauges, tool call cards, agent badges,
//! thinking indicators) are rendered as pre-rasterized textures composited
//! in the overlay pass. Re-rasterization is gated on a data hash so that
//! widgets only redraw their pixmap when their source data actually
//! changes — not every frame.
//!
//! ## Architecture
//!
//! ```text
//!   Data source  ──►  WidgetSpec  ──►  WidgetRasterizer (tiny-skia)
//!                                          │
//!                                          ▼
//!                                      Pixmap  ──► wgpu::Texture
//!                                                       │
//!                   WidgetManager caches CachedWidget ◄─┘
//!                                          │
//!                                          ▼
//!                                   WidgetRenderer   (composite in
//!                                                     overlay pass,
//!                                                     after OverlayLayer)
//! ```
//!
//! ## Freshness tracking
//!
//! Each `WidgetSpec` carries a `data_hash: u64`. The `WidgetManager`
//! only re-invokes the rasterizer when the incoming spec's hash differs
//! from the cached entry's hash. Consumers (PoC badge, future tool-call
//! cards, context gauges) own the hashing policy — they hash whatever
//! inputs they care about and feed the result in via `spec.data_hash`.
//!
//! ## v1 scope (tn-npd)
//!
//! * `WidgetRasterizer` — tiny-skia pixmap rendering for rounded-pill
//!   backgrounds. Text labels are composited separately via the existing
//!   glyphon overlay text renderer — this keeps the rasterizer dependency
//!   surface small and avoids pulling in a second text stack just for the
//!   PoC. A future follow-up can move text into the pixmap once a proper
//!   fontdue / cosmic-text integration path is chosen.
//! * `WidgetManager` — `HashMap<WidgetId, CachedWidget>` freshness cache.
//! * `WidgetRenderer` — minimal textured-quad wgpu pipeline.
//! * `AgentBadgeSource` — PoC widget that surfaces the focused pane's
//!   agent state in the top-right of the window.
//!
//! Placement APIs, the pattern-engine widget bridge, tool-call cards,
//! thinking indicators, and context gauges are intentionally out of
//! scope for v1.

pub mod agent_timeline;
pub mod badge;
pub mod gpu;
pub mod pattern_widget;
pub mod rasterizer;

// Public re-exports for convenient access from the render path.
// The `#[allow(unused_imports)]` is defensive: the types are used by
// name today only via the submodule paths (see `render_driver.rs`),
// but future widgets will want to reach them through `crate::widgets::*`.
#[allow(unused_imports)]
pub use agent_timeline::{AgentTimelineSource, TIMELINE_WIDGET_ID};
#[allow(unused_imports)]
pub use badge::{AgentBadgeSource, BADGE_WIDGET_ID};
#[allow(unused_imports)]
pub use gpu::{WidgetManager, WidgetRenderer};
#[allow(unused_imports)]
pub use pattern_widget::PatternWidgetMatch;
#[allow(unused_imports)]
pub use rasterizer::{WidgetRasterizer, WidgetSpec};

/// Stable identifier for a widget instance.
///
/// The PoC uses a single hard-coded badge id (`BADGE_WIDGET_ID`). A full
/// placement API is a follow-up; widget producers will own id allocation
/// once pattern packs and harness crates start emitting widgets.
pub type WidgetId = u64;
