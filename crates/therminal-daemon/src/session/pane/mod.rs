//! Pane: owns a headless Term + PTY via `PtyPaneCore`.
//!
//! Split into focused submodules:
//! - [`headless`] — `HeadlessListener` + daemon-side `DaemonPtyHandler`
//!   that drives the OSC interceptor, region index, agent inference, and
//!   pattern dispatch on each chunk of PTY output.
//! - [`fd_master`] — `FdPtyMaster`: a `MasterPty` impl over a raw fd
//!   received via SCM_RIGHTS during daemon handoff (unix-only). Carries
//!   the SAFETY comments from tn-bkf4 on every libc unsafe block.
//! - [`dispatch_ctx`] — `PaneDispatchCtx` shared from `SessionManager` so
//!   each new pane is born with the right pattern engine + bus wiring.
//! - [`lifecycle`] — `Pane` struct definition + `spawn` + `from_raw_fd`
//!   constructors + `Drop`.
//! - [`accessors`] — accessor methods on `Pane` (snapshots, tags, write,
//!   resize, exit code lookups).

mod accessors;
mod dispatch_ctx;
mod fd_master;
mod headless;
mod lifecycle;

pub use dispatch_ctx::PaneDispatchCtx;
pub use lifecycle::Pane;

#[cfg(test)]
pub(crate) use headless::HeadlessListener;
