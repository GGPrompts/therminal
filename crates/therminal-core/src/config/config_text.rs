//! Generates the fully-commented default TOML config text.

use super::{TherminalConfig, TrustTier};

/// Version of the default config template. Bump this whenever a new
/// section or documented default is added to [`default_config_text`].
///
/// The version is emitted as a comment line at the top of the generated
/// file (`# template_version = N`) and parsed back by
/// [`super::check_config_template_status`] to detect when a user's
/// `therminal.toml` predates the current template — without polluting the
/// parsed config struct.
pub const CONFIG_TEMPLATE_VERSION: u32 = 6;

/// Return the fully-commented default config as a TOML string.
///
/// Every line is either a comment or a commented-out value so that the
/// file round-trips back to defaults when un-commented.
pub(super) fn default_config_text() -> String {
    let d = TherminalConfig::default();
    let mut out = String::new();

    out.push_str("# Therminal config — hot-reloaded on save\n");
    out.push_str(&format!(
        "# template_version = {CONFIG_TEMPLATE_VERSION}   # used by upgrade detection, do not edit manually\n"
    ));
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
    out.push_str("# shell_args = []  # extra arguments passed to the shell on startup\n");
    out.push_str(
        "# new_pane_cwd = \"inherit\"  # \"inherit\" (focused pane cwd) or \"home\" (home dir)\n",
    );
    out.push_str(&format!("# padding = {}\n", d.general.padding));
    out.push_str(
        "# Focus mode (hides pane headers, status bar, and tab bar for maximum\n\
         # terminal space) is a runtime toggle bound to F11 by default — see\n\
         # [keybindings] and KeyAction::FocusMode (tn-t2yd.2).\n",
    );
    out.push_str(&format!(
        "# use_csd = {}  # client-side decorations (default: true on Linux/Windows)\n",
        d.general.use_csd
    ));
    out.push_str(&format!(
        "# auto_tile = {}  # auto-split panes when AI agents spawn subprocesses\n",
        d.general.auto_tile
    ));
    out.push_str(&format!(
        "# auto_tile_debounce_ms = {}  # debounce interval (ms) for spawn/exit events\n",
        d.general.auto_tile_debounce_ms
    ));
    out.push_str(&format!(
        "# swarm_watch_scope = \"{}\"  # \"all\" or \"current\" — restrict subagent panes to this instance\n",
        match d.general.swarm_watch_scope {
            crate::config::SwarmWatchScope::All => "all",
            crate::config::SwarmWatchScope::Current => "current",
        }
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
    out.push_str(&format!(
        "# background_opacity = {:.1}  # 0.0 (fully transparent) to 1.0 (fully opaque)\n",
        d.colors.background_opacity()
    ));
    out.push('\n');
    out.push_str("# Chrome roles (tn-g7oo) — pane headers, separators, focus border,\n");
    out.push_str("# status bar, tab bar, CSD buttons. Defaults derive from the bundled\n");
    out.push_str("# palette so you only need to set the roles you want to recolor.\n");
    out.push_str("# chrome_focus_border = \"#56a7ff\"   # focus outline + active tab underline\n");
    out.push_str("# chrome_separator     = \"#2f4564\"   # split-pane separator line\n");
    out.push_str("# chrome_header_bg     = \"#111c2d\"   # focused pane header strip\n");
    out.push_str("# chrome_header_bg_dim = \"#060a12\"   # unfocused pane header strip\n");
    out.push_str("# chrome_status_bar_bg = \"#060a12\"   # bottom status bar (also tab bar)\n");
    out.push_str("# chrome_tab_bar_bg    = \"#060a12\"   # workspace tab bar (overrides above)\n");
    out.push_str("# chrome_tab_active_bg = \"#111c2d\"   # active workspace tab background\n");
    out.push_str("# chrome_csd_close     = \"#d94040\"   # CSD close button hover tint\n");
    out.push('\n');
    out.push_str("# Chrome text-color roles — used by pane header labels, status bar text,\n");
    out.push_str("# workspace tab labels, CSD icons. Themes that re-skin chrome backgrounds\n");
    out.push_str("# should set chrome_fg / chrome_fg_muted to a readable contrast.\n");
    out.push_str("# chrome_fg        = \"#e7f0ff\"   # primary chrome text\n");
    out.push_str("# chrome_fg_muted  = \"#a9b8cd\"   # secondary / button labels\n");
    out.push_str("# chrome_fg_focus  = \"#56a7ff\"   # workspace number, agent indicator\n");
    out.push_str("# chrome_fg_warn   = \"#ffb24f\"   # git detached HEAD\n");
    out.push_str("# chrome_fg_alert  = \"#ff5f78\"   # close button glyph\n");
    out.push('\n');
    out.push_str(
        "# Hotspot underline colors (tn-g7oo) — file path / URL / error / git ref / issue ref.\n",
    );
    out.push_str("# hotspot_filepath  = \"#39ffb6\"\n");
    out.push_str("# hotspot_url       = \"#56a7ff\"\n");
    out.push_str("# hotspot_error     = \"#ff5f78\"\n");
    out.push_str("# hotspot_gitref    = \"#eab308\"\n");
    out.push_str("# hotspot_issueref  = \"#b48eff\"\n");
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
        "# osc_9 = {}   # desktop notifications\n",
        d.terminal.osc_9
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
    out.push_str(
        "#          resize_grow | resize_shrink | resize_reset | focus_next | focus_prev\n",
    );
    out.push_str("#          focus_up | focus_down | focus_left | focus_right | zoom_pane\n");
    out.push_str("#          show_help | show_settings | close_all_panes | restore_layout\n");
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
    out.push_str("#\n");
    out.push_str("# Each profile can use either `shell` (+ optional `shell_args`) or\n");
    out.push_str("# `command` (freeform string). If both are set, `command` wins.\n");
    out.push_str("# `shell_integration` auto-detects: true for shell, false for command.\n");
    out.push_str("# `icon` is a Nerd Font glyph for the launcher tile.\n");
    out.push_str("# `color` is a hex background for the launcher tile (#RRGGBB or #RGB).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [profiles.dev]\n");
    out.push_str("# shell = \"/bin/zsh\"\n");
    out.push_str("# shell_args = [\"-l\"]\n");
    out.push_str("# working_directory = \"~/dev\"\n");
    out.push_str("# font_size = 14.0\n");
    out.push_str("# scrollback_lines = 50000\n");
    out.push_str("# icon = \"\\ue62b\"  # Nerd Font: nf-seti-typescript\n");
    out.push_str("# color = \"#1e3a5f\"\n");
    out.push_str("# [profiles.dev.env]\n");
    out.push_str("# EDITOR = \"nvim\"\n");
    out.push_str("#\n");
    out.push_str("# [profiles.docker-app]\n");
    out.push_str("# command = \"docker exec -it my-app /bin/bash\"\n");
    out.push_str("# icon = \"\\uf308\"  # Nerd Font: nf-linux-docker\n");
    out.push_str("# color = \"#0db7ed\"\n");
    out.push_str("#\n");
    out.push_str("# [profiles.ssh-prod]\n");
    out.push_str("# command = \"ssh prod-server\"\n");
    out.push_str("# icon = \"\\uf489\"  # Nerd Font: nf-oct-terminal\n");
    out.push_str("# color = \"#d4443e\"\n");
    out.push_str("#\n");
    out.push_str("# [profiles.powershell]\n");
    out.push_str("# shell = \"pwsh\"\n");
    out.push_str("# shell_args = [\"-NoLogo\"]\n");
    out.push_str("# shell_integration = true\n");
    out.push_str("# icon = \"\\uebc7\"  # Nerd Font: nf-cod-terminal_powershell\n");
    out.push_str("# color = \"#012456\"\n");
    out.push('\n');

    // ── [mcp] ────────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [mcp] — MCP (Model Context Protocol) server for external tool access.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[mcp]\n");
    out.push_str(&format!("# enabled = {}\n", d.mcp.enabled));
    out.push_str("# socket_path = \"\"  # empty = default runtime dir socket\n");
    out.push_str(&format!(
        "# attach_mode = \"{}\"  # \"local\" (GUI owns PTYs) or \"remote\" (stream from daemon)\n",
        match d.mcp.attach_mode {
            crate::config::AttachMode::Local => "local",
            crate::config::AttachMode::Remote => "remote",
        }
    ));
    out.push('\n');

    // ── [daemon] ─────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [daemon] — therminal-daemon discovery and auto-spawn (tn-txs8).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[daemon]\n");
    out.push_str(
        "# binary_path = \"\"  # explicit path to therminal-daemon; empty = auto-detect\n",
    );
    out.push('\n');

    // ── [cursor] ────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [cursor] — Cursor appearance and behavior.\n");
    out.push_str("# style: \"block\" | \"underline\" | \"beam\" | \"hollow_block\"\n");
    out.push_str("# blink: enable cursor blinking (~530ms interval).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[cursor]\n");
    out.push_str(
        "# style = \"block\"  # default cursor shape (programs can override via DECSCUSR)\n",
    );
    out.push_str(&format!(
        "# blink = {}  # cursor blinking (suppressed when reduced_motion = true)\n",
        d.cursor.blink
    ));
    out.push('\n');

    // ── [bell] ──────────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [bell] — BEL character handling.\n");
    out.push_str("# style: \"taskbar\" | \"visual\" | \"audible\" | \"none\"\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[bell]\n");
    out.push_str("# style = \"taskbar\"  # flash the taskbar/dock icon on BEL\n");
    out.push_str(&format!(
        "# visual_bell_duration_ms = {}  # flash duration when style = \"visual\"\n",
        d.bell.visual_bell_duration_ms
    ));
    out.push('\n');

    // ── [notifications] ─────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [notifications] — Desktop notification settings.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[notifications]\n");
    out.push_str(&format!(
        "# agent_waiting = {}  # notify when an agent transitions to awaiting input\n",
        d.notifications.agent_waiting
    ));
    out.push_str(&format!(
        "# osc9_enabled = {}  # send desktop notifications for OSC 9 sequences\n",
        d.notifications.osc9_enabled
    ));
    out.push('\n');

    // ── [hotspots] ──────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [hotspots] — Clickable file/URL handling.\n");
    out.push_str("# editor_chain: ordered list of editor commands to try when opening a\n");
    out.push_str("# file hotspot. The first found on PATH wins. The tokens `$VISUAL` and\n");
    out.push_str("# `$EDITOR` are substituted with those env vars at launch time.\n");
    out.push_str("#\n");
    out.push_str("# folder_pane_command: command (argv form) spawned in a NEW pane when a\n");
    out.push_str("# directory hotspot is clicked. The literal token `{path}` is replaced\n");
    out.push_str("# with the clicked directory. Default is `tfe` (Terminal File Explorer);\n");
    out.push_str("# any TUI file manager that accepts a path argument works (yazi, ranger,\n");
    out.push_str("# nnn, lf, broot, mc, etc.). The new pane's cwd is also set to the path\n");
    out.push_str("# so that if the binary is missing the user lands in the right shell.\n");
    out.push_str("#\n");
    out.push_str("# folder_opener: ordered list of \"reveal in file manager\" commands tried\n");
    out.push_str("# by the secondary directory action. The first found on PATH wins.\n");
    out.push_str("# `$FILE_MANAGER` is substituted from the env var. As a last resort the\n");
    out.push_str("# platform default (xdg-open / open / explorer) is invoked.\n");
    out.push_str("#\n");
    out.push_str("# git_tools: TUI git tools probed on PATH. Each tool that resolves gets\n");
    out.push_str("# a context-menu entry on git commit hash hotspots; clicking it splits a\n");
    out.push_str("# new pane and runs the tool against the hash. Supported invocations:\n");
    out.push_str("#   lazygit  -> `lazygit --filter <hash>`\n");
    out.push_str("#   gitlogue -> `gitlogue -c <hash>`\n");
    out.push_str("#   tig      -> `tig show <hash>`\n");
    out.push_str("# Set to [] to disable git-tool menu entries (tn-fzr0).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[hotspots]\n");
    let chain_toml = d
        .hotspots
        .editor_chain
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("# editor_chain = [{chain_toml}]\n"));
    let folder_pane_toml = d
        .hotspots
        .folder_pane_command
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("# folder_pane_command = [{folder_pane_toml}]\n"));
    let folder_opener_toml = d
        .hotspots
        .folder_opener
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("# folder_opener = [{folder_opener_toml}]\n"));
    let git_tools_toml = d
        .hotspots
        .git_tools
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("# git_tools = [{git_tools_toml}]\n"));
    out.push('\n');

    // ── [patterns] ──────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [patterns] — Semantic pattern-matching engine (tn-yrjd).\n");
    out.push_str("# Pattern packs are TOML files loaded from `directory` (default\n");
    out.push_str("# `~/.config/therminal/patterns/`). See `docs/pattern-packs-authoring.md`\n");
    out.push_str("# for the rule schema; shipped examples live in `plugins/examples/`.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[patterns]\n");
    out.push_str(&format!("# enabled = {}\n", d.patterns.enabled));
    out.push_str("# directory = \"\"  # empty = ~/.config/therminal/patterns\n");
    out.push_str(&format!("# max_patterns = {}\n", d.patterns.max_patterns));
    out.push_str(&format!(
        "# slow_pattern_threshold_us = {}  # disable a pattern after 3 matches slower than this\n",
        d.patterns.slow_pattern_threshold_us
    ));
    out.push_str(&format!(
        "# slow_strike_limit = {}\n",
        d.patterns.slow_strike_limit
    ));
    out.push('\n');

    // ── [delegate] ──────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [delegate] — Sibling delegate profiles (tn-ztv3).\n");
    out.push_str("# Each profile under [delegate.profiles.<name>] describes an isolated AI\n");
    out.push_str("# agent sibling that can be spawned into a new pane with a defined role,\n");
    out.push_str("# working-directory policy, and MCP/permission envelope.\n");
    out.push_str("#\n");
    out.push_str("# Fields per profile:\n");
    out.push_str("#   description     — human-readable label shown in agent listings\n");
    out.push_str(
        "#   command         — REQUIRED launch template; tokens: {pane_id}, {session_id}, {cwd}\n",
    );
    out.push_str("#   working_dir     — \"same\" | \"worktree\" | \"scratch/{random}\" (default: \"same\")\n");
    out.push_str(
        "#   mcp_enabled     — list of MCP tool-domain prefixes granted to the delegate\n",
    );
    out.push_str(
        "#   permission_mode — forwarded verbatim to the delegate (default: \"default\")\n",
    );
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [delegate.profiles.planner]\n");
    out.push_str("# description = \"Strategic planning agent — read-only, no shell execution\"\n");
    out.push_str("# command = \"claude --pane {pane_id} --role planner\"\n");
    out.push_str("# working_dir = \"worktree\"\n");
    out.push_str("# mcp_enabled = [\"terminal.panes\", \"terminal.sessions\"]\n");
    out.push_str("# permission_mode = \"plan\"\n");
    out.push('\n');
    out.push_str("# [delegate.profiles.browser-research]\n");
    out.push_str(
        "# description = \"Web research agent — browser MCP only, no local file writes\"\n",
    );
    out.push_str("# command = \"claude --pane {pane_id} --role researcher\"\n");
    out.push_str("# working_dir = \"scratch/{random}\"\n");
    out.push_str("# mcp_enabled = [\"browser\"]\n");
    out.push_str("# permission_mode = \"default\"\n");
    out.push('\n');
    out.push_str("# [delegate.profiles.adversarial-review]\n");
    out.push_str("# description = \"Adversarial code reviewer — full read access, no writes\"\n");
    out.push_str("# command = \"claude --pane {pane_id} --role adversarial-reviewer\"\n");
    out.push_str("# working_dir = \"same\"\n");
    out.push_str("# mcp_enabled = [\"terminal.panes\", \"terminal.semantic\"]\n");
    out.push_str("# permission_mode = \"default\"\n");
    out.push('\n');

    // ── [accessibility] ─────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [accessibility] — Accessibility settings (tn-avjv.6).\n");
    out.push_str("# high_contrast: boost UI chrome contrast (borders, headers, status bar).\n");
    out.push_str("# reduced_motion: disable cursor blink and animated transitions.\n");
    out.push_str("# ui_text_scale: scale factor for chrome text (1.0 = default, 0.5–3.0).\n");
    out.push_str("#   Does NOT affect terminal cell text — use [font].size for that.\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("[accessibility]\n");
    out.push_str(&format!(
        "# high_contrast = {}\n",
        d.accessibility.high_contrast
    ));
    out.push_str(&format!(
        "# reduced_motion = {}\n",
        d.accessibility.reduced_motion
    ));
    out.push_str(&format!(
        "# ui_text_scale = {:.1}  # scale factor for chrome text (0.5–3.0)\n",
        d.accessibility.ui_text_scale
    ));
    out.push('\n');

    // ── [[bookmarks]] ────────────────────────────────────────────────────
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [[bookmarks]] — Text-first bookmark list (tn-co6n).\n");
    out.push_str("#\n");
    out.push_str("# Print with `therminal bookmarks` (or `tn bookmarks`). URLs become\n");
    out.push_str("# clickable hotspots via the existing URL regex — no overlay code.\n");
    out.push_str("# Filter with `--category <X>` or emit structured output with `--json`.\n");
    out.push_str("# Fields: name (required), url (required), icon (optional),\n");
    out.push_str("#         category (optional).\n");
    out.push_str("# ─────────────────────────────────────────────────────────────────────────\n");
    out.push_str("# [[bookmarks]]\n");
    out.push_str("# name = \"Therminal docs\"\n");
    out.push_str("# url = \"https://docs.therminal.dev\"\n");
    out.push_str("# category = \"reference\"\n");
    out.push_str("#\n");
    out.push_str("# [[bookmarks]]\n");
    out.push_str("# name = \"GitHub\"\n");
    out.push_str("# url = \"https://github.com\"\n");
    out.push_str("# icon = \"\\uf09b\"  # Nerd Font: nf-fa-github\n");
    out.push_str("# category = \"dev\"\n");
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
