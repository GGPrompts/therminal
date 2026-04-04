//! Wire types, MCP schema, and semantic events for Therminal.

/// Semantic region types for scrollback tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RegionKind {
    Prompt,
    Command,
    Output,
    Error,
    ToolCall,
    Thinking,
    Annotation,
}
