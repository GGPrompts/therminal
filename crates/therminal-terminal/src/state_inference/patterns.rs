//! Compiled regex patterns for agent output matching.

use regex::Regex;

/// Compiled regex patterns used by the inference engine.
///
/// Constructed once and reused across all `feed_bytes` / `infer_status` calls.
pub(crate) struct Patterns {
    /// Braille spinner characters used by Claude/Codex during processing.
    pub spinner: Regex,
    /// Tool call patterns: "Read(", "Edit(", "Bash(", "Write(", etc.
    pub tool_call: Regex,
    /// Awaiting input indicators.
    pub awaiting_input: Regex,
    /// Context percentage patterns (e.g., "Context: 42%", "42% context").
    pub context_percent: Regex,
    /// Agent identification from output (e.g., "Claude Code", "Codex").
    pub agent_ident_claude: Regex,
    pub agent_ident_codex: Regex,
    pub agent_ident_copilot: Regex,
    /// Model name extraction from output.
    ///
    /// NOTE: The canonical model family list lives in `therminal-core`'s
    /// `MODEL_REGISTRY` (`claude_state.rs`). Keep this regex's alternations
    /// in sync when adding new model families there.
    pub model_pattern: Regex,
}

impl Patterns {
    pub fn new() -> Self {
        Self {
            spinner: Regex::new(r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]|Thinking\.{2,}").unwrap(),
            tool_call: Regex::new(
                r"(?:^|\s)(Read|Edit|Write|Bash|Glob|Grep|TodoRead|TodoWrite|WebSearch|WebFetch|mcp_|ListFiles|SearchFiles|ExecuteCommand|ReplaceInFile|ReadFile|WriteToFile)\s*[\(\{(]"
            ).unwrap(),
            awaiting_input: Regex::new(
                r"(?:^>\s|esc to interrupt|waiting for (?:input|response)|^\s*\$\s*$|Press Enter|Type (?:a|your) (?:message|response))"
            ).unwrap(),
            context_percent: Regex::new(r"(\d{1,3})(?:\.\d)?%\s*(?:context|ctx)").unwrap(),
            agent_ident_claude: Regex::new(r"(?i)claude\s*(?:code|3|4)?").unwrap(),
            agent_ident_codex: Regex::new(r"(?i)codex").unwrap(),
            // Anchor copilot detection to product-name contexts so generic TUI
            // text containing the bare word "copilot" (e.g. a Bubble Tea TUI
            // rendering a repo description, help text, or news feed) does not
            // false-positive as a Copilot agent session. See tn-3pkv.
            agent_ident_copilot: Regex::new(
                r"(?i)\b(?:github\s+copilot|copilot\s+(?:cli|chat|using|v\d|version))\b",
            )
            .unwrap(),
            model_pattern: Regex::new(
                r"(?:model|using)[\s:]+([a-zA-Z0-9._-]+(?:opus|sonnet|haiku|gpt[0-9.-]+|o[134]-?[a-z]*|gemini[a-zA-Z0-9._-]*)[a-zA-Z0-9._-]*)"
            ).unwrap(),
        }
    }
}
