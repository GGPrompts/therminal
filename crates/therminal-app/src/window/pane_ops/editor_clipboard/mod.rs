//! Editor and clipboard operations.
//!
//! Split into focused submodules:
//! - [`clipboard`] — copy_selection, paste_clipboard, clear_selection
//! - [`editor`] — open_in_editor, open_absolute_in_editor, open_in_wsl_pane_editor
//! - [`planner`] — OpenInEditorPlan, plan_open_in_editor, resolve_editor_chain,
//!   which_on_path, shell_quote, plus their unit tests

mod clipboard;
mod editor;
mod planner;

pub(super) use planner::shell_quote;
