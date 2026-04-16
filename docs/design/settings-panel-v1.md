# Settings Panel V1 Design Decisions

Status: Accepted (design baseline for tn-avjv)
Date: 2026-04-09
Decision issue: tn-avjv.1
Parent epic: tn-avjv

## Context

Therminal needs a GUI settings panel for everyday user preferences, while staying intentionally small (Philosophy B). The panel must not become a full mirror of `therminal.toml` and must not absorb security or system-plumbing configuration.

This document resolves the blockers called out in tn-avjv: panel placement, keyboard model, live preview semantics, save/cancel behavior, and behavior when `therminal.toml` changes externally while the panel is open.

## Scope Boundary (V1)

Implemented sections (in order as they appear in the panel):

1. **Shell** — default shell path, shell arguments, new-pane cwd behavior (inherit from focused pane or home directory)
2. **Hotspots** — editor chain entries (`Editor #1`…`#16`), folder pane command, folder opener entries
3. **Theme Presets** — one-click apply for bundled palette presets (Original Therminal, Paper, Tokyo Night Light, Tomorrow Night Bright, Retro Terminal)
4. **Accessibility** — high contrast mode toggle, reduced motion toggle, UI text scale (75%–300%)

Out of scope for V1 (not implemented):
- Font family and size controls (see Future section below)
- Cursor style and blink controls (see Future section below)
- Trust-tier editing and harness configuration
- MCP/daemon/runtime path configuration
- Pattern-pack management
- Separate keybinding viewer panel (existing help overlay remains the source of truth)
- Full keybinding editor
- Command actions (restart daemon, clear scrollback, etc.)

## Future

Planned additions beyond V1:

- **Font controls** (tn-340u) — font family selector and font size control, to replace the current keyboard-only `Ctrl+=`/`Ctrl+-`/`Ctrl+0` bindings with a visible panel control.
- **Cursor style/blink** — cursor shape (block, underline, beam) and blink toggle.

## Decision 1: Placement and Layout

Decision:
- Implement settings as an overlay mode (`OverlayMode::Settings`) rendered via the existing two-pass overlay system.
- Use a large centered panel (not a full window takeover), with a left section list and right content pane.
- Keep terminal panes visible but dimmed behind the panel.

Rationale:
- Reuses existing overlay plumbing and avoids immediate window-layout refactors.
- Keeps user context visible while editing preferences.
- Preserves the existing "single window, everything visible" product direction better than launching a separate workspace/tab.

## Decision 2: Interaction and Keyboard Model

Decision:
- Keyboard-complete operation is mandatory; mouse support is additive.
- Baseline keys:
  - `Tab` / `Shift+Tab`: move focus between controls
  - Arrow keys: move within section lists and option groups
  - `Enter` / `Space`: activate or toggle focused control
  - `Esc`: close panel (with unsaved-change handling)

Rationale:
- Matches terminal-user expectations and accessibility needs.
- Aligns with existing overlay interaction patterns.

Note:
- Keybinding discoverability continues to use the existing keybinding help overlay (`Ctrl+Shift+?`) in v1.

## Decision 3: State Model (Draft, Applied, Saved)

Decision:
- Track three layers while the panel is open:
  - `saved`: last known on-disk configuration snapshot at panel open (or after explicit save)
  - `draft`: current in-panel edits
  - `applied`: runtime config currently active in app/daemon
- On each edit, update `draft` and apply changes live to runtime (`applied`) using debounced updates for expensive operations (font/theme).
- Disk is only updated on explicit Save.

Rationale:
- Gives modern live-preview UX without forcing disk writes on every keystroke.
- Supports reversible experimentation and explicit persistence.

## Decision 4: Save/Cancel/Close Semantics

Decision:
- `Save` writes `draft` to `therminal.toml` and updates `saved`.
- `Cancel` reverts runtime to `saved` and closes panel.
- `Esc` behaves like close request:
  - If no unsaved changes, close immediately.
  - If unsaved changes exist, prompt: `Save`, `Discard`, or `Keep editing`.
- `Apply` button is not included in v1 because live preview already applies to runtime.

Rationale:
- Removes ambiguity between "previewed" and "persisted" state.
- Keeps the control surface small.

## Decision 5: External Edit Conflict Handling

Decision:
- While settings is open, watch config file fingerprint (`mtime` + content hash).
- If external edits arrive and panel is clean (no unsaved draft), silently reload panel from disk.
- If external edits arrive and panel is dirty, show conflict prompt:
  - `Reload from disk` (drop local draft)
  - `Keep local draft` (continue, then Save overwrites)
  - `Review diff` (optional in v1; if not implemented, show a concise summary and two-way choice)

Rationale:
- Avoids accidental clobbering while still keeping behavior deterministic.

## Decision 6: Persistence Format

Decision:
- Use `toml_edit` for writes to preserve comments/formatting where practical.
- Only mutate keys owned by settings v1 scope.
- Do not rewrite unrelated sections.

Rationale:
- Preserves hand-edited config readability.
- Minimizes surprising file churn.

## Implementation Order

1. tn-avjv.2 - Widget primitives for settings overlay
2. tn-avjv.3 - Settings overlay framework and navigation
3. tn-avjv.8 - Config persistence and round-trip semantics
4. tn-avjv.4, tn-avjv.5, tn-avjv.6 - Section implementations

Note: tn-avjv.8 can begin once tn-avjv.3 is in place; persistence plumbing does not need all sections completed.

## Risks and Mitigations

- Risk: font/theme live preview causes jank.
  - Mitigation: debounce heavy updates and cache-shape where possible.
- Risk: overlay complexity drifts toward mini-toolkit sprawl.
  - Mitigation: keep widget primitives minimal and scoped to settings v1 needs.
- Risk: settings surface drifts into trust/system domains.
  - Mitigation: enforce scope boundary in code review against this document.
