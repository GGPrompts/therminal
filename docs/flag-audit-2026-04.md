# Flag Audit: `show_*` and Aesthetic Toggles — April 2026

**Test applied:** "If this feature is visible by default, will any real user turn it off?"

Three verdicts:
- **DELETE** — nobody would turn this off; bake as always-on behavior
- **PANEL** — legitimate user preference; surface in the settings panel (tn-avjv)
- **REDESIGN** — flag exists because the feature is loud/badly placed; fix the design and delete the flag

Implementer references: changes to DELETE flags belong in tn-t2yd; PANEL additions belong in tn-avjv.

---

## Summary

| Flag | Section | Default | Verdict |
|---|---|---|---|
| `trust.show_agent_indicator` | `[trust]` | `true` | DELETE |
| `general.show_pane_headers` | `[general]` | `true` | DELETE (resolved tn-t2yd.2) |
| `general.show_status_bar` | `[general]` | `true` | DELETE (resolved tn-t2yd.2) |
| `general.show_tab_bar` | `[general]` | `true` | REDESIGN (resolved tn-t2yd.3) |
| `font.nerd_font` | `[font]` | `true` | PANEL |
| `general.use_csd` | `[general]` | platform | PANEL |
| `general.auto_tile` | `[general]` | `true` | PANEL |

Verdict counts: **3 DELETE, 3 PANEL, 1 REDESIGN**

---

## Per-Flag Entries

### `trust.show_agent_indicator`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | DELETE |

**Justification.** The agent indicator in the status bar is core observability chrome — it shows which harness is running in the focused pane. Per the integration taxonomy in CLAUDE.md, Claude Code, Codex, Copilot, and OpenCode are all first-class harnesses. Hiding the indicator would leave users blind to agent activity. Nobody would want this off unless the indicator is poorly designed (in which case the answer is to improve the design, not gate it). Bake it as always-on. The flag should be deleted and the guard in the status bar renderer removed.

**Code references:**
- `crates/therminal-core/src/config/mod.rs` — struct field `TrustConfig::show_agent_indicator` (line ~541) and default (line ~558)
- `crates/therminal-core/src/config/config_text.rs` — template comment output (line ~150)
- `crates/therminal-app/src/window/render_driver.rs` — passes `show_agent_indicator` into `StatusBarInfo` (line ~178)
- `crates/therminal-app/src/window/chrome/status_bar.rs` — `StatusBarInfo::show_agent_indicator` field and three gate sites (lines ~23, ~137, ~247, ~328); test fixture default (line ~516)
- `crates/therminal-app/CLAUDE.md` — config note (line ~73, ~79)

---

### `general.show_pane_headers`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | DELETE (resolved tn-t2yd.2) |

**Resolution (tn-t2yd.2).** Config field removed. Pane headers are now on unconditionally whenever multiple panes exist and `App.focus_mode = false`. The original "PANEL" verdict assumed display density was a real preference — in practice the setting sat in the overlay unused, and the actual use case (maximum terminal real estate during focused work) is better served by a single F11 `KeyAction::FocusMode` runtime toggle that also hides the status bar and tab bar in one gesture. Parameters like `show_pane_headers: bool` are still threaded through `effective_header_height`, `resize_all_panes`, and `render_panes_recursive`, but every call site computes them as `!self.focus_mode` from the new runtime flag.

---

### `general.show_status_bar`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | DELETE (resolved tn-t2yd.2) |

**Resolution (tn-t2yd.2).** Config field removed. The original verdict called for an auto-hide-by-row-count heuristic; the actual landing was simpler: the bar is on by default and can be hidden alongside the other chrome via the F11 focus mode toggle (`KeyAction::FocusMode`). Call sites (`content_area_rect_csd`, `effective_status_bar_height`) still take a `bool` parameter but every site computes it from `App.focus_mode` at the moment of the call.

---

### `general.show_tab_bar`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | REDESIGN (resolved tn-t2yd.3) |

**Justification.** The tab bar is the workspace switcher and, under CSD mode, serves as the title bar. Hiding it entirely when CSD is on is already blocked in `effective_tab_bar_height_csd` (the bar is forced visible in CSD mode regardless of the flag). The legitimate use-case is: "I only ever have one workspace, the tab bar wastes space." The right design is auto-suppress the tab bar when there is exactly one workspace (with no rename in progress), not a manual toggle. Once that heuristic is in place the flag is redundant. This flag is a companion to `show_status_bar` in tn-t2yd.

**Resolution (tn-t2yd.3).** Field removed from `GeneralConfig` and the settings overlay "Show tab bar" row deleted. All call sites funnel through `crate::pane::should_show_tab_bar(workspace_count)` / `effective_tab_bar_height_csd(workspace_count, use_csd)`: single-workspace layouts collapse the bar to 0 px; a second workspace makes it appear automatically; CSD mode keeps the title-bar strip reserved regardless (for the window control buttons). Stale `show_tab_bar = ...` entries in existing `therminal.toml` files are silently ignored because `GeneralConfig` has `#[serde(default)]` without `deny_unknown_fields`.

---

### `font.nerd_font`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | PANEL |

**Justification.** Whether to use Nerd Font glyph variants is a real aesthetic preference tied to the user's installed fonts. Users without a Nerd Font patched face will see tofu/fallback glyphs for powerline symbols and dev icons if this is forced on. Conversely, users with Nerd Fonts installed expect the glyphs. This is exactly the kind of "do you have this font installed?" toggle that belongs in the settings panel under "Font". Already default-true so most users see the intended experience; power users who want vanilla fonts can opt out.

**Code references:**
- `crates/therminal-core/src/config/mod.rs` — `FontConfig::nerd_font` field (line ~320) and default (line ~343); conversion to `CoreFontConfig` (line ~218)
- `crates/therminal-core/src/config/config_text.rs` — template comment (line ~75)
- `crates/therminal-core/src/font.rs` — `FontConfig::nerd_font` field (line ~95); default (line ~105); used in `build_family_chain` to append Nerd Font suffix (lines ~134–135)
- `crates/therminal-app/src/window/mod.rs` — included in font-changed hot-reload detection (line ~483)

---

### `general.use_csd`

| | |
|---|---|
| **Default** | `true` on Linux/Windows; `false` on macOS |
| **Verdict** | PANEL |

**Justification.** Client-side decorations versus native window chrome is a platform-and-environment preference. CSD integrates the tab bar into the title bar area for zero vertical waste; native decorations hand control to the WM/OS. On Linux under certain compositors or tiling WMs users may strongly prefer native decorations. On Windows the CSD gives the custom look; some users may prefer native. This is a legitimate environment-sensitive toggle. It is already correctly defaulted per platform. Belongs in the settings panel under an "Appearance / Window" group.

**Code references:**
- `crates/therminal-core/src/config/mod.rs` — `GeneralConfig::use_csd` field (line ~254); `default_use_csd()` fn (line ~280); default (line ~297)
- `crates/therminal-core/src/config/config_text.rs` — template comment (line ~41)
- `crates/therminal-app/src/pane/geometry.rs` — `effective_tab_bar_height_csd(workspace_count, use_csd)` (lines ~57–60, ~87, ~90)
- `crates/therminal-app/src/window/mouse.rs` — CSD button hover (line ~479); edge resize gating (line ~502, ~986); hit-test (line ~817)
- `crates/therminal-app/src/window/event_handler.rs` — click dispatch into CSD controls (lines ~540, ~581, ~586, ~643, ~695)
- `crates/therminal-app/src/window/mod.rs` — `with_decorations(false)` when CSD (lines ~898, ~906–907); layout reflow (lines ~558, ~649)
- `crates/therminal-app/src/window/render_driver.rs` — CSD title bar draw path (lines ~205–206, ~227, ~254)
- `crates/therminal-app/src/window/init.rs` — init layout call (line ~320)
- `crates/therminal-app/CLAUDE.md` — CSD tab bar doc (line ~91)

---

### `general.auto_tile`

| | |
|---|---|
| **Default** | `true` |
| **Verdict** | PANEL |

**Justification.** Auto-tiling is a resource and layout behavior toggle with real footprint: it subscribes to agent registry events, starts a swarm-watcher background thread, and restructures the pane layout without explicit user action. Users who prefer manual control over their workspace — or who are running Therminal on constrained hardware — have a legitimate reason to disable it. Also gating the swarm-watcher on this flag means disabling auto-tile reduces background CPU load. Belongs in the settings panel under "Agent Behavior". The companion `auto_tile_debounce_ms` is a performance knob (out of scope for the panel per tn-avjv philosophy).

**Code references:**
- `crates/therminal-core/src/config/mod.rs` — `GeneralConfig::auto_tile` field (line ~256) and default (line ~298); `auto_tile_debounce_ms` companion (line ~258, ~299)
- `crates/therminal-core/src/config/config_text.rs` — template comments (lines ~45, ~49)
- `crates/therminal-app/src/window/init.rs` — gates `AutoTileDebouncer` construction on `config.general.auto_tile` (line ~64–68); gates swarm-watcher construction (lines ~121, ~128)
- `crates/therminal-app/src/window/mod.rs` — `auto_tile_debouncer: Option<AutoTileDebouncer>` field (line ~243)
- `crates/therminal-app/src/window/event_handler.rs` — `poll_auto_tile()` call on each redraw (line ~405)
- `crates/therminal-app/src/window/pane_ops.rs` — `poll_auto_tile` implementation (lines ~2079–2080); `register_auto_tiled` calls (lines ~2120–2121, ~2184–2185)
- `crates/therminal-app/src/pane/auto_tile.rs` — `AutoTileDebouncer` and `auto_tiled_panes` map
- `crates/therminal-app/CLAUDE.md` — architecture note (line ~112)
