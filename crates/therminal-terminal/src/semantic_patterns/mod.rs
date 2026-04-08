//! Semantic pattern-matching engine (the pattern-pack surface).
//!
//! See `docs/pattern-matching-spec.md`, `docs/pattern-performance-model.md`,
//! and `docs/pattern-packs-authoring.md` for the full specification. Every
//! design decision in this module is traceable to one of those documents.
//!
//! ## Module layout
//!
//! - `schema` — raw TOML shapes deserialized straight from pack files.
//! - `loader` — TOML-to-compiled-pack translation + validation.
//! - `types` — compiled pattern / pack / match types + template expansion.
//! - `engine` — the runtime: scope-indexed dispatch, metrics, reload.
//!
//! ## Public API
//!
//! The daemon constructs a single [`PatternEngine`] at startup
//! (`PatternEngine::new(PatternEngineConfig { ... })`), shares it between
//! MCP tool handlers and pane workers, and calls
//! [`PatternEngine::process_finalized_line`] /
//! [`PatternEngine::process_prompt_boundary`] from the hot path. Returned
//! [`PatternMatch`] values are then routed to the hotspot registry, widget
//! substrate, or event bus by the daemon (the engine itself does not
//! perform dispatch routing — it's a pure library component).

mod engine;
pub mod loader;
pub mod schema;
pub mod types;

#[cfg(test)]
mod tests;

pub use engine::{
    EngineStats, GlobalStats, PackLoadErrorInfo, PackStats, PatternEngine, PatternEngineConfig,
    PatternLoadErrorInfo, PatternStats,
};
pub use loader::{PackLoadError, load_pack_from_file, load_pack_from_str, load_packs_from_dir};
pub use types::{
    AppliesTo, CompiledPack, CompiledPattern, EmitEventAction, HotspotAction, HotspotOnClick,
    PatternAction, PatternMatch, PatternScope, ResolvedAction, ResolvedEmitEvent, ResolvedHotspot,
    ResolvedWidget, WidgetAction, WidgetAnchor, WidgetKind, expand_template,
};
