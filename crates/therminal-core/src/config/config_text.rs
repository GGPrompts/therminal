//! Generates the fully-commented default TOML config text.

use super::{TherminalConfig, TrustTier};

/// Return the fully-commented default config as a TOML string.
///
/// Every line is either a comment or a commented-out value so that the
/// file round-trips back to defaults when un-commented.
pub(super) fn default_config_text() -> String {
    let d = TherminalConfig::default();
    let mut out = String::new();

    out.push_str("# Therminal config — hot-reloaded on save\n");
    out.push_str("# Uncomment and edit any value to override the default.\n");
    out.push('\n');

    // ── [general] ───────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [general] — Window geometry, shell, scrollback, and environment.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[general]\n");
    out.push_str(&format!("# title = {:?}\n", d.general.title));
    out.push_str(&format!("# window_width = {}\n", d.general.window_width));
    out.push_str(&format!("# window_height = {}\n", d.general.window_height));
    out.push_str(&format!(
        "# scrollback_lines = {}\n",
        d.general.scrollback_lines
    ));
    out.push_str("# shell = \"\"  # empty = user's default shell\n");
    out.push_str(&format!("# padding = {}\n", d.general.padding));
    out.push_str(&format!(
        "# show_status_bar = {}\n",
        d.general.show_status_bar
    ));
    out.push_str(&format!("# show_tab_bar = {}\n", d.general.show_tab_bar));
    out.push_str(&format!(
        "# use_csd = {}  # client-side decorations (default: true on Linux)\n",
        d.general.use_csd
    ));
    out.push_str("# [general.env]  # extra PTY environment variables\n");
    out.push_str("# MY_VAR = \"value\"\n");
    out.push('\n');

    // ── [font] ───────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [font] — Font family, size, and rendering options.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[font]\n");
    out.push_str(&format!("# family = {:?}\n", d.font.family));
    out.push_str(&format!("# size = {}\n", d.font.size));
    out.push_str(&format!(
        "# line_height_scale = {}\n",
        d.font.line_height_scale
    ));
    out.push_str("# extra_fallbacks = [\"Noto Color Emoji\"]\n");
    out.push_str(&format!("# nerd_font = {}\n", d.font.nerd_font));
    out.push_str(&format!("# ui_font_family = {:?}\n", d.font.ui_font_family));
    out.push_str(&format!(
        "# display_font_family = {:?}\n",
        d.font.display_font_family
    ));
    out.push('\n');

    // ── [colors] ─────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [colors] — Terminal color palette overrides (hex: \"#RRGGBB\").\n");
    out.push_str("# Leave a field absent (or comment it out) to use the built-in palette.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[colors]\n");
    out.push_str("# background = \"#060a12\"\n");
    out.push_str("# foreground = \"#e7f0ff\"\n");
    out.push_str("# foreground_bright = \"#e7f0ff\"\n");
    out.push_str("# foreground_muted = \"#a9b8cd\"\n");
    out.push_str("# surface = \"#18263a\"\n");
    out.push_str("# cursor = \"#56a7ff\"\n");
    out.push_str("# selection = \"#22324a\"\n");
    out.push_str("# ansi = [  # 16-entry ANSI palette override\n");
    out.push_str("#   \"#000000\", \"#cc0000\", \"#00cc00\", \"#cccc00\",\n");
    out.push_str("#   \"#0000cc\", \"#cc00cc\", \"#00cccc\", \"#cccccc\",\n");
    out.push_str("#   \"#888888\", \"#ff5555\", \"#55ff55\", \"#ffff55\",\n");
    out.push_str("#   \"#5555ff\", \"#ff55ff\", \"#55ffff\", \"#ffffff\",\n");
    out.push_str("# ]\n");
    out.push('\n');

    // ── [terminal] ───────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [terminal] — OSC sequence interceptor.\n");
    out.push_str("# Controls which escape-sequence families are intercepted for AI-awareness\n");
    out.push_str("# and shell integration.  Disable a family only if a third-party tool\n");
    out.push_str("# conflicts.  All families are enabled by default.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[terminal]\n");
    out.push_str(&format!(
        "# osc_633 = {}  # VS Code shell integration\n",
        d.terminal.osc_633
    ));
    out.push_str(&format!(
        "# osc_133 = {}  # FinalTerm shell integration\n",
        d.terminal.osc_133
    ));
    out.push_str(&format!(
        "# osc_7 = {}   # current working directory\n",
        d.terminal.osc_7
    ));
    out.push_str(&format!(
        "# osc_1337 = {}  # iTerm2 extensions\n",
        d.terminal.osc_1337
    ));
    out.push_str(&format!(
        "# osc_7777 = {}  # cooperative agent self-reporting\n",
        d.terminal.osc_7777
    ));
    out.push('\n');

    // ── [trust] ──────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [trust] — AI agent trust tiers.\n");
    out.push_str("# default_tier: \"sandboxed\" | \"supervised\" | \"trusted\"\n");
    out.push_str("# agent_scan_interval: seconds between process-tree scans (0 = disabled).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[trust]\n");
    out.push_str(&format!(
        "# default_tier = {:?}\n",
        trust_tier_str(&d.trust.default_tier)
    ));
    out.push_str(&format!(
        "# show_agent_indicator = {}\n",
        d.trust.show_agent_indicator
    ));
    out.push_str(&format!(
        "# agent_scan_interval = {}  # seconds (0 = disabled)\n",
        d.trust.agent_scan_interval
    ));
    out.push_str(&format!(
        "# destructive_rate_limit = {}  # max destructive ops per agent per minute (0 = unlimited)\n",
        d.trust.destructive_rate_limit
    ));
    out.push('\n');
    out.push_str("# Per-agent overrides — tier: \"sandboxed\" | \"supervised\" | \"trusted\"\n");
    out.push_str("# [trust.agents.claude]\n");
    out.push_str("# tier = \"trusted\"\n");
    out.push_str("# allowed_tools = [\"read_file\", \"write_file\"]\n");
    out.push('\n');

    // ── [keybindings] ────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [keybindings] — Key-action bindings merged on top of built-in defaults.\n");
    out.push_str("# Actions: copy | paste | font_size_up | font_size_down | font_size_reset\n");
    out.push_str("#          split_horizontal | split_vertical | split_auto | close_pane\n");
    out.push_str("#          resize_grow | resize_shrink | focus_next | focus_prev\n");
    out.push_str("#          focus_up | focus_down | focus_left | focus_right | zoom_pane\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[keybindings]\n");
    out.push_str("# [[keybindings.bindings]]\n");
    out.push_str("# key = \"ctrl+shift+c\"\n");
    out.push_str("# action = \"copy\"\n");
    out.push('\n');
    out.push_str("# [[keybindings.bindings]]\n");
    out.push_str("# key = \"ctrl+shift+v\"\n");
    out.push_str("# action = \"paste\"\n");
    out.push('\n');
    out.push_str("# [[keybindings.bindings]]\n");
    out.push_str("# key = \"ctrl+plus\"\n");
    out.push_str("# action = \"font_size_up\"\n");
    out.push('\n');
    out.push_str("# [[keybindings.bindings]]\n");
    out.push_str("# key = \"ctrl+minus\"\n");
    out.push_str("# action = \"font_size_down\"\n");
    out.push('\n');
    out.push_str("# [[keybindings.bindings]]\n");
    out.push_str("# key = \"ctrl+0\"\n");
    out.push_str("# action = \"font_size_reset\"\n");
    out.push('\n');

    // ── [profiles] ───────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [profiles] — Named session profiles with per-profile overrides.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [profiles.dev]\n");
    out.push_str("# shell = \"/bin/zsh\"\n");
    out.push_str("# working_directory = \"~/dev\"\n");
    out.push_str("# font_size = 14.0\n");
    out.push_str("# scrollback_lines = 50000\n");
    out.push_str("# [profiles.dev.env]\n");
    out.push_str("# EDITOR = \"nvim\"\n");
    out.push('\n');

    // ── [mcp] ────────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [mcp] — MCP (Model Context Protocol) server for external tool access.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[mcp]\n");
    out.push_str(&format!("# enabled = {}\n", d.mcp.enabled));
    out.push_str("# socket_path = \"\"  # empty = default runtime dir socket\n");
    out.push('\n');

    out
}

/// Return the TOML string representation of a [`TrustTier`] variant.
fn trust_tier_str(tier: &TrustTier) -> &'static str {
    match tier {
        TrustTier::Sandboxed => "sandboxed",
        TrustTier::Supervised => "supervised",
        TrustTier::Trusted => "trusted",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn default_config_text_has_header() {
        let text = TherminalConfig::default_config_text();
        assert!(text.starts_with("# Therminal config — hot-reloaded on save\n"));
    }

    #[test]
    fn default_config_text_all_lines_are_comments_or_empty() {
        let text = TherminalConfig::default_config_text();
        for line in text.lines() {
            assert!(
                line.is_empty() || line.starts_with('#') || line.starts_with('['),
                "unexpected non-comment line: {line:?}"
            );
        }
    }

    #[test]
    fn default_config_text_parses_as_empty_toml() {
        // Since every value line is commented out, the text should parse
        // successfully and yield the same result as an empty TOML document
        // (i.e. all defaults).
        let text = TherminalConfig::default_config_text();
        let config: TherminalConfig =
            toml::from_str(&text).expect("default config text must parse");
        assert_eq!(config.general.title, "Therminal");
        assert_eq!(config.font.size, 17.0);
        assert_eq!(config.trust.default_tier, TrustTier::Supervised);
        assert_eq!(config.trust.agent_scan_interval, 3);
        assert!(config.terminal.osc_633);
        assert!(config.terminal.osc_133);
        assert!(config.terminal.osc_7);
        assert!(config.terminal.osc_1337);
    }

    #[test]
    fn save_default_to_writes_parseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        TherminalConfig::default()
            .save_default_to(&path)
            .expect("save_default_to should succeed");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("# Therminal config"));
        let _config: TherminalConfig =
            toml::from_str(&contents).expect("written default config must parse");
    }
}
