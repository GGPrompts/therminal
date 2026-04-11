//! `PaneDispatchCtx`: pattern-dispatch wiring handed to every new pane.
//!
//! Built once on the `SessionManager` (tn-86us) and cloned into each
//! `Pane::spawn` / `Pane::from_raw_fd` so the new pane is born with the
//! right pattern engine + bus + match counter triple.

use std::sync::Arc;

use therminal_protocol::PaneId;

/// Pattern-dispatch wiring handed to every new pane (tn-86us).
#[derive(Clone, Default)]
pub struct PaneDispatchCtx {
    pub engine: Option<Arc<therminal_terminal::semantic_patterns::PatternEngine>>,
    pub bus: Option<Arc<crate::event_bus::EventBus>>,
    pub matches_total: Arc<std::sync::atomic::AtomicU64>,
}

impl PaneDispatchCtx {
    pub(super) fn build_dispatcher(
        &self,
        pane_id: PaneId,
    ) -> Option<crate::pattern_dispatch::PatternDispatcher> {
        let engine = self.engine.as_ref()?.clone();
        let bus = self.bus.as_ref()?.clone();
        Some(crate::pattern_dispatch::PatternDispatcher::new(
            engine,
            bus,
            Arc::clone(&self.matches_total),
            pane_id,
        ))
    }
}
