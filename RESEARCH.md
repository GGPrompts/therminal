# Therminal: Architecture Blueprint for an AI-Native Terminal

**Therminal's planned tech stack — wgpu + glyphon + cosmic-text for rendering, vendored alacritty_terminal for emulation, and a daemon-based multiplexer with MCP integration — is architecturally sound and validated by production terminals.** The combination positions Therminal in a unique gap: no open-source terminal today offers GPU-accelerated rendering, built-in multiplexing, AI agent awareness, and MCP queryability together. Warp is the only terminal with AI features but is closed-source and cloud-dependent. Ghostty offers the best native performance but has zero AI capabilities. Zellij has the best plugin system but isn't a terminal emulator. Therminal can be the first open-source, AI-native terminal that gives both humans and agents structured visibility into terminal sessions.

This report covers GPU rendering approaches, terminal emulation backends, multiplexer architectures, AI agent detection techniques, MCP integration, cross-platform strategy, competitive analysis, and novel feature design — with specific technical recommendations for each.

---

## GPU rendering: wgpu is the right bet, validated by Rio and Warp

Every major GPU-accelerated terminal uses the same fundamental pattern: rasterize glyphs on the CPU, cache them in a GPU texture atlas, and render textured quads onto a cell grid. The differences lie in which graphics API they target and how they manage the caching hierarchy.

**Alacritty** uses OpenGL 3.3+ with a two-draw-call pipeline (backgrounds, then text), achieving **~3ms input latency** and ~2ms render time. **Kitty** also uses OpenGL with a GPU sprite cache and threaded rendering, claiming 2x throughput over other terminals. **Ghostty** goes native — Metal on macOS, OpenGL on Linux — with a multi-pass pipeline (background → cell backgrounds → text → images → cursor → custom shaders) and SIMD-optimized parsing, achieving **~2ms input latency**. **WezTerm** supports both OpenGL and wgpu backends with a sophisticated 4-level cache hierarchy (shape cache → glyph cache → line state cache → line quad cache).

**Rio is the most directly relevant precedent** for Therminal. It uses wgpu via a custom renderer called Sugarloaf with a Redux-style state machine that only redraws changed lines. It supports configurable backends (Vulkan, Metal, DX12, GL) and performance modes. **Warp's Linux version** also uses wgpu + winit + cosmic-text with ~98% shared code across platforms, proving this exact stack works in production at scale with **>144 FPS** and 1.9ms average redraw time.

The wgpu + glyphon + cosmic-text combination is validated by multiple production uses:

- **glyphon** (by grovesNL, 698 GitHub stars) is the recommended wgpu text renderer, superseding wgpu_glyph. It uses cosmic-text for shaping, etagere for shelf-packing atlas allocation, and renders all glyphs in a single instanced draw call (`pass.draw(0..4, 0..glyph_count)`). It's used by the Iced GUI framework.
- **cosmic-text** (by System76 for the COSMIC desktop) provides full Unicode text shaping via HarfRust (a complete Rust port of HarfBuzz), glyph rasterization via swash, and has been tested against the Universal Declaration of Human Rights in ~500 languages. It handles ligatures, emoji, BiDi text, and complex scripts.
- **swash** handles font rasterization under cosmic-text — pure Rust, no FreeType dependency, supports variable fonts and color emoji.

**Key rendering architecture recommendations:**

Use a multi-pass pipeline modeled on Ghostty: (1) background fill, (2) cell backgrounds, (3) text from glyphon atlas, (4) cursor, (5) images/graphics protocol, (6) overlay widgets. Adopt Rio's dirty-tracking pattern — only redraw lines that changed. Use event-driven rendering (not continuous) to save battery, with the monitor refresh rate as the FPS cap. Default to `LowPower` GPU preference for integrated GPU selection. Implement **linear alpha blending** from the start — Ghostty's v1.2.0 rework to fix text artifacts with gamma-incorrect blending shows this is a common pitfall.

Use grayscale antialiasing with subpixel *positioning* (cosmic-text's 4×4 SubpixelBin). Skip subpixel color rendering entirely — Apple removed it in Mojave, it's incompatible with GPU compositing, and it's invisible on HiDPI displays. Budget for **wgpu version churn**: Rio tracked upgrades from v22→v23→v27, each requiring migration work.

Performance targets based on the competitive landscape: **≤5ms input latency**, ≥100 MB/s terminal parsing throughput, ≤3ms per-frame render time, ≤80 MB base memory footprint.

---

## Terminal emulation: vendor alacritty_terminal now, watch libghostty-vt

The terminal emulation backend choice comes down to three viable options, each with distinct tradeoffs.

**alacritty_terminal** (v0.25.1, Apache-2.0, ~8K SLoC) provides full VT100/xterm emulation with a Grid/Cell model, damage tracking (`TermDamage`/`LineDamageBounds`), built-in PTY management, scrollback with regex search, Vi mode, hyperlink support (OSC 8), and a renderable content iterator designed for GPU rendering. **Zed editor** is the primary proof that vendoring works — it wraps `Term<ZedListener>` and renders by iterating the grid cells. The crate has 458K all-time downloads and 9 direct dependents.

The critical limitation is that alacritty_terminal was **not designed for embedding**. It has no parser extension points for custom OSC sequences, the `EventListener` trait is minimal, and 42 published versions show frequent breaking changes (struct renames, trait changes, module reorganizations). It also lacks Kitty graphics protocol, Sixel support, and any hook system for intercepting escape sequences — all features Therminal needs for AI detection.

**WezTerm's termwiz** is often mentioned as an alternative but is fundamentally different in scope. The published crate is a *terminal interaction toolkit* (escape parser, input decoder, surface buffer, widget system) — **not a terminal emulator**. The actual terminal emulation lives in `wezterm-term`, which is internal to the WezTerm monorepo and explicitly not published to crates.io. Wez Furlong confirmed: "you need a terminal emulator to parse the bytes and compute the model for the display... wezterm's term crate is not published because its API is not intended to be public." Using termwiz alone would require building your own terminal emulation layer on top.

**libghostty-vt** is the most architecturally correct long-term option. Mitchell Hashimoto's vision of "stop reinventing terminal emulation" aims to provide a shared core with zero dependencies, SIMD-optimized parsing, full Kitty graphics/keyboard protocol support, and a render state API consumers build on. Community Rust bindings exist (`libghostty-vt` crate by Uzaaft), and the ecosystem is already impressive — VSCode extension, Emacs, JupyterLab, Flutter, and GPUI integrations. However, the API is pre-1.0 (no tagged version yet), Rust bindings are community-maintained, and it **requires a Zig toolchain** in the build chain.

**Recommendation: vendor alacritty_terminal for MVP, but add an xterm.js-inspired hook layer.**

Pin to a specific commit rather than tracking HEAD. Add a `SequenceInterceptor` trait between the VTE parser and Term's handler that allows registering callbacks for specific OSC codes (like OSC 133, 633, 1337) without modifying the core emulation:

```
Layer 1: vte::Parser (byte-level parsing)
  ↓ [SequenceInterceptor — intercept for AI/semantic analysis]
Layer 2: Term<TherminalListener> (terminal state management)
  ↓ [damage tracking API]  
Layer 3: Therminal GPU renderer
```

This architecture mirrors xterm.js's `parser.addOscHandler()` extensibility model and allows swapping to libghostty-vt later without rewriting the hook system or renderer.

---

## Multiplexing architecture: build a hybrid daemon with WezTerm's domain model

Terminal multiplexing architectures fall into three patterns, each demonstrated by a major project.

**tmux** uses a single-server, multi-client model where one background process manages all sessions via Unix domain sockets at `/tmp/tmux-{UID}/default`. The session→window→pane hierarchy (with unique IDs: `$N` for sessions, `@N` for windows, `%N` for panes) is the gold standard. tmux's **control mode** (`-CC`) is the programmatic interface — structured text-based output with `%begin`/`%end` blocks for command results and `%output %PANE_ID DATA` for pane content, plus async notifications for window/session/layout changes. iTerm2 uses control mode to render tmux sessions as native macOS tabs. The critical limitation is **double-parsing**: every byte passes through tmux's emulator and then the terminal's emulator, adding latency and breaking modern features (images, ligatures, custom escape sequences).

**Zellij** (Rust, MIT) uses a multi-threaded server with dedicated threads for PTY, Screen, Plugin, and Route, communicating via a typed Thread Bus wrapping MPSC channels with strongly-typed Rust instruction enums (`ScreenInstruction`, `PtyInstruction`, etc.). IPC uses **Protocol Buffers** over Unix domain sockets. Its WASM plugin system (wasmi runtime) lets plugins in any language react to terminal events. Zellij v0.44.0 (March 2026) added native Windows support, making it the first multiplexer on all three platforms.

**WezTerm** demonstrates the ideal pattern: **both** built-in local multiplexing (fast, no IPC overhead) and a Unix domain mux server for session persistence. Its `Mux` singleton manages a hierarchy of Windows→Tabs→Panes with a domain abstraction (Local, Unix, SSH, TLS, Serial, ExecDomain) that cleanly separates connection context from session state. The `MuxNotification` subscriber system distributes state changes to consumers. Its key insight from Wez Furlong: "Performance is much faster when bypassing [Unix domain multiplexing] and just using the multiplexing built into the gui."

**For Therminal's daemon architecture, adopt a hybrid model:**

A central daemon process manages the session registry, event bus, and IPC listener, while per-session PTY workers (Rust threads, not separate processes) provide isolation through ownership semantics. Each worker maintains headless terminal emulation state (via the vendored alacritty_terminal), so reconnection sends a state snapshot rather than replaying bytes — the pattern proven by Zed's pty-host and the zmx project.

For IPC, the **`interprocess` crate** correctly abstracts Unix domain sockets (Unix/macOS) and named pipes (Windows) behind a single `LocalSocket` API with Tokio async support. **MessagePack via `rmp-serde`** is the right wire protocol — it's the most compact binary format (~35ns serialize, ~125ns deserialize), supports zero-copy deserialization, and is the same protocol Neovim uses for its RPC layer. The total IPC budget for 60fps rendering (16.6ms per frame) is easily met: raw Unix socket latency ~5-10μs + MessagePack round-trip ~160ns = well under 500μs.

For PTY management, use **`portable-pty`** (v0.9.0, MIT, 4.7M+ downloads, created by WezTerm's author). Fork or vendor it to add modern ConPTY flags (`PSEUDOCONSOLE_RESIZE_QUIRK`, `WIN32_INPUT_MODE`, `PASSTHROUGH_MODE`) that the upstream doesn't yet pass. This gives cross-platform PTY abstraction covering Linux forkpty, macOS forkpty, and Windows ConPTY.

Build a **control mode equivalent** from day one — a machine-readable text protocol so AI agents and scripts can drive Therminal programmatically. This is critical for the AI agent ecosystem where tools like claude-squad and agent-deck currently depend on tmux's scripting API.

---

## AI agent detection: a layered approach from protocols to heuristics

Therminal's passive agent detection should use four complementary signal layers, ordered by reliability.

**Layer 1: Shell integration protocols (highest confidence).** OSC 133 (FinalTerm semantic prompts) is the de facto standard, supported by iTerm2, Kitty, WezTerm, Ghostty, and VS Code. It divides the terminal stream into semantic zones using four markers: `\x1b]133;A` (prompt start), `\x1b]133;B` (command input start), `\x1b]133;C` (execution start), `\x1b]133;D;{exitcode}` (execution finished). **OSC 633** (VS Code extension) adds command text reporting (`E;commandline`), properties (`P;Cwd=...`), and nonce-based spoofing prevention. OSC 7 reports current working directory (`\x1b]7;file://hostname/path`). OSC 1337 (iTerm2) provides user variables, remote host info, and inline images. Therminal should parse all four protocol families and emit its own shell integration scripts for bash/zsh/fish/PowerShell that output OSC 133 sequences without requiring `TERM_PROGRAM == vscode`.

**Layer 2: Process tree inspection (high confidence).** Use the `sysinfo` crate (cross-platform: Linux, macOS, Windows) to enumerate processes sharing the PTY's TTY device number. On Linux, supplement with the `procfs` crate for deeper `/proc` access including environment variables (`/proc/{pid}/environ`). Known agent process signatures:

- **Claude Code**: process name `claude` (Node.js), env var `CLAUDECODE=1` set in spawned shells
- **Codex CLI**: process name `codex` (Rust binary), config at `~/.codex/config.toml`
- **GitHub Copilot CLI**: process name `copilot`, env vars `GH_TOKEN`/`GITHUB_TOKEN`
- **Aider**: process name `aider` (Python), env vars `AIDER_MODEL`, config `.aider.conf.yml`

Match by TTY device number → walk parent-child tree → check process names and environment variables. Confidence: **~95%** for exact process name matches.

**Layer 3: Output stream analysis (medium-high confidence).** Human typing produces 1-3 characters at 50-200ms intervals with ~4% backspace corrections and high variance. Agent output arrives in large bursts (100-4000+ characters) at <1ms inter-character delay within bursts, with zero backspaces, at sustained >500 chars/sec throughput. Spinner patterns (Braille: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, cycling via `\r` + cursor control) indicate processing state. Full-screen TUI frameworks (Ink for Claude Code, Rich for Aider) produce characteristic ANSI cursor save/restore and alternate screen buffer patterns.

**Layer 4: State machine inference (composite confidence).** Combine all signals into a state machine tracking six states:

| State | Primary Signals | Confidence |
|-------|----------------|------------|
| **idle** | OSC 133 A→B with no C, PTY quiescent >2s, low CPU | 95% |
| **processing** | Spinner pattern, network activity, no child processes | 80-85% |
| **streaming** | Sustained >500 chars/sec, monotonic output, no backspaces | 90% |
| **tool_use** | New child processes under agent PID, OSC 133 C marker | 90% |
| **awaiting_input** | Output stops after question text, `[y/N]` pattern, agent in read() | 75% |
| **thinking** | "Thinking" indicator, extended delay, no visible output | 75% |

**Define a custom OSC extension** (e.g., OSC 7777) for cooperative agents to voluntarily report their state, enabling zero-heuristic detection. Publish the spec as an open standard. Claude Code already has a feature request (#22528) to emit OSC 133 sequences — Therminal could champion broader agent shell integration.

---

## MCP integration: use the official Rust SDK with tools as the primary interface

The Model Context Protocol reached **version 2025-11-25** with significant maturity: ~2,000 indexed servers in the MCP Registry, adoption by Claude Desktop, VS Code, Cursor, ChatGPT (OpenAI adopted March 2025), and SDKs in 7+ languages. The November 2025 release added async Tasks, parallel tool calls, server-side agent loops, and OAuth 2.1 — all relevant for Therminal.

The **official Rust SDK is `rmcp`** (v0.16.0, published on crates.io under `github.com/modelcontextprotocol/rust-sdk`). It provides `ServerHandler` trait implementation with `#[tool]` procedural macros, supports stdio and Streamable HTTP transports, and requires Tokio async runtime. An alternative community SDK (`rust-mcp-sdk`) offers additional features like Hyper HTTP server and auth providers, but `rmcp` should be the primary choice given its official status.

**For transport, use stdio** (local) for the daemon's MCP server. This gives zero network overhead, microsecond latency, and no authentication complexity — the terminal process controls which clients connect. Add Streamable HTTP later for remote/multi-client scenarios.

**Design tools as the primary interface** rather than resources. Current MCP clients have better tool support than resource subscription support. Recommended tool set:

| Tool | Type | Description |
|------|------|-------------|
| `list_panes` | Read-only | All panes with ID, title, shell, dimensions, agent state |
| `read_pane_content` | Read-only | Visible content of a pane, optionally with scrollback |
| `query_semantic_history` | Read-only | Search typed/semantic scrollback by pattern or natural language |
| `get_pane_geometry` | Read-only | Dimensions, position, layout info |
| `spawn_pane` | Destructive | Create pane, optionally run command, set working directory |
| `send_input` | Destructive | Send text/keystrokes to a pane (high risk, requires trust tier 2+) |
| `wait_for_output` | Read-only | Block until pattern appears in pane output (with timeout) |
| `close_pane` | Destructive | Kill a terminal pane |

Use MCP tool annotations (introduced in 2025-06-18 spec) to communicate risk: `destructiveHint: true` for spawn/send/close, `readOnlyHint: true` for list/read/query. Expose pane content and session lists as MCP Resources for forward compatibility with subscription-based clients. Implement `notifications/resources/updated` for real-time state changes.

For security, implement **per-agent trust tiers** enforced at the MCP handler layer: Tier 0 (Observer: read output only), Tier 1 (Reader: read + query), Tier 2 (Writer: read + send input + spawn), Tier 3 (Admin: full control). Filter available tools based on the connecting agent's trust level during MCP initialization. Log all tool invocations with timestamps, agent identity, and parameters. Rate-limit destructive operations (e.g., 60 calls/minute).

---

## Cross-platform strategy: winit + portable-pty + arboard, with Wayland-first on Linux

**Windowing with winit** is proven by Alacritty and Rio. winit handles X11, Wayland, macOS AppKit, and Windows Win32 with raw window handles for wgpu surface creation. It has terminal-specific support: `ImePurpose::Terminal` for Wayland on-screen keyboards, `with_resize_increments` for cell-sized snapping (works on macOS and X11, not Wayland). Known limitations include no native menus, no clipboard API, pre-1.0 API instability, and Wayland resize increment gaps. WezTerm deliberately built its own windowing layer for full control; Ghostty uses native toolkits (Swift/AppKit, GTK4). **Start with winit for time-to-market, but architect a trait-based windowing abstraction** (like WezTerm's `WindowOps`/`ConnectionOps`) to enable future platform-native backends.

**Clipboard via arboard** (v3.6.1, maintained by 1Password, 18.5M+ downloads) with the `wayland-data-control` feature flag. On Linux, handle the clipboard ownership model carefully — if the `Clipboard` object is dropped, clipboard contents may vanish (unlike macOS/Windows where clipboard persists system-wide). **Implement OSC 52** in the terminal emulation layer (`\x1b]52;c;{base64}\a`) for remote clipboard access through SSH — supported by all major terminals and essential for server workflows.

**Platform-specific concerns to address early:**

- **Linux**: Support both X11 and Wayland (Wayland is becoming default on major distros). Use DBus for desktop notifications. Consider systemd socket activation for the daemon. Font discovery via fontconfig/FreeType.
- **macOS**: Invest in proper `.app` bundle with notarization pipeline early (required since Catalina). wgpu auto-selects Metal. Handle Retina scale factors (≥2.0). Consider native menu bar integration via tao or Cocoa bindings.
- **Windows**: Fork portable-pty to pass modern ConPTY flags (`PASSTHROUGH_MODE` for Win11 22H2+, `WIN32_INPUT_MODE`, `RESIZE_QUIRK`). ConPTY always outputs UTF-8 and adds trailing spaces — handle both quirks. Target Windows 10 1809+ minimum.

**WebView integration via wry** should be optional, behind a feature flag. The wgpu + WebView compositing problem is unsolved — Tauri discussions document 300ms latency when trying to composite wgpu frames over WebView. Instead, implement WebView panes as **separate OS child windows** managed by the tiling layout, letting the OS compositor handle layering. Use WebView panes only for ancillary content (markdown preview, agent dashboards, charts) — never for the terminal itself. On Linux, WebKitGTK adds a heavy dependency; make it truly optional.

---

## Competitive landscape reveals a clear gap for Therminal

The competitive analysis reveals that **no existing terminal combines GPU rendering, AI awareness, multiplexing, and extensibility**:

**Warp** is the only terminal with deep AI features (natural language → commands, agent mode, proactive suggestions, BYOK). It uses Rust + wgpu + cosmic-text on Linux, achieves >144 FPS, and pioneered semantic "blocks" via shell integration hooks. But it's **closed-source**, requires login (relaxed but still nudges), collects telemetry, depends on cloud infrastructure, has no plugin system, no multiplexer, and no terminal protocol innovations. User criticism centers on privacy, vendor lock-in, and the subscription model ($20-200/month).

**Ghostty** (Zig, MIT, non-profit) represents peak terminal engineering: SIMD-optimized parser, Metal/OpenGL rendering with ~2ms input latency, native UI per platform (Swift/AppKit on macOS, GTK4 on Linux), and the libghostty embeddable core vision. But it has **zero AI features**, no plugins, no multiplexer, and no collaboration — purely a terminal emulator.

**Kitty** (C/Python/Go, GPLv3) created the de facto image protocol standard, has an excellent Python kitten extension system, and a JSON-based remote control protocol with encrypted communication. But it's **OpenGL-only with no Windows support**, no AI features, and Python-only extensibility.

**Zellij** (Rust, MIT) has the best plugin architecture — WASM sandbox with any-language support, typed Thread Bus for internal communication, KDL declarative layouts, and built-in web client. But it's **not a terminal emulator** (runs inside another terminal), has no GPU rendering, no AI features, and higher resource usage than tmux.

**Rio** (Rust, MIT) validates the wgpu approach via its Sugarloaf renderer with Redux-style dirty tracking. But it has **no AI, no plugins, no multiplexer, no remote control**, and a limited feature set.

**Therminal's differentiation matrix:**

| vs Warp | Open source, privacy-first, local AI option, WASM plugins, no login/telemetry |
|---------|------------------------------------------------------------------------------|
| vs Ghostty | AI agent awareness, MCP integration, multiplexing, semantic scrollback |
| vs Kitty | Modern Rust/wgpu stack, AI features, WASM plugins, Windows support |
| vs Zellij | Full terminal emulator with GPU rendering, AI, graphics protocol |
| vs Rio | AI, plugins, multiplexing, semantic history, remote control protocol |

The strategic opportunity is clear: **Therminal can be the first open-source, AI-native terminal** — combining Warp's AI vision with Ghostty's performance ethos, Rio's wgpu approach, Kitty's protocol innovations, and Zellij's extensibility model.

---

## Novel features: semantic scrollback is the foundation, trust tiers the gate

The novel features Therminal plans should be built in a specific order, because they have architectural dependencies.

**Semantic scrollback is the foundation everything else builds on.** Go beyond Warp's blocks with fine-grained typed regions: Prompt, Command, Output, Error (detected via stderr fd and exit codes), ToolCall (detected via patterns and MCP invocations), ThinkingBlock (agent reasoning output), and Annotation (user/agent metadata). Store a hybrid format: raw terminal byte stream for faithful replay alongside a structured SQLite sidecar containing region types, timestamps, byte offsets, parsed metadata (file paths, URLs, error codes), and a full-text search index. Expose this as an MCP tool (`query_semantic_history`) so any connected AI agent can query terminal history semantically. Integrate with Atuin's SQLite schema for shell history portability.

**Trust tiers are the gate for production agent use.** Implement four tiers: Observer (read output only), Reader (read + query), Writer (read + write + spawn within workspace), Admin (full access). Define per-agent permissions in `therminal.toml` with writable paths, allowed/denied commands, and network access controls. Enforce at the PTY layer — intercept commands before they reach the shell. Integrate with OS-level sandboxing: Seatbelt on macOS (as Cursor does), bubblewrap/Landlock on Linux (as Codex does), Windows Sandbox. Implement trust escalation via GPU-rendered modal overlays requiring explicit user approval.

**Multi-agent coordination is the killer differentiator.** No terminal today provides native coordination for parallel agents. Build an agent registry with live status tracking (idle/working/blocked/error), auto-tiling that dynamically arranges panes as agents spawn (grid layout for ≤9 agents, summary tiles for larger swarms), and an agent-to-agent messaging bus over Unix domain sockets. The terminal becomes the coordination layer — replacing ad-hoc tmux scripting with structured primitives for fan-out, sequencing, and DAG execution.

**GPU overlay widgets** enable the agent UX without leaving terminal context. Use a two-pass rendering pipeline: opaque terminal grid first, then semi-transparent overlays with alpha blending. Render overlay widgets (context gauge, tool call cards, thinking indicator, toast notifications) as textured quads pre-rasterized via tiny-skia into textures. Only re-rasterize when data changes, not every frame. Use SDF (Signed Distance Field) rendering for resolution-independent icons. This builds on well-understood game engine HUD techniques.

**Actionable hotspots** transform terminal output from passive text to interactive interface. Build a multi-pattern detection engine matching file paths (`/path/to/file.ts:42:15`), URLs, Git refs, issue numbers (`#1234`, `JIRA-567`), error codes (`E0308`, `TS2345`), IP addresses, and stack trace frames. Implement OSC 8 hyperlink protocol natively. On hover, render a GPU overlay action palette: "Open in Editor," "Explain Error (AI)," "Show Git Blame," "Copy Path." Ship configurable regex patterns (like iTerm2's Smart Selection) with sensible defaults for common languages.

---

## Conclusion: what Therminal should build first and why

The research validates Therminal's core architecture while revealing specific refinements and a clear build order.

**The tech stack is correct.** wgpu + glyphon + cosmic-text is production-validated by Rio and Warp Linux. Vendored alacritty_terminal with a hook layer provides the fastest path to a working terminal. MessagePack over `interprocess` LocalSockets is the right IPC choice. `rmcp` (official MCP Rust SDK) over stdio is the right MCP integration path. `portable-pty` (forked for ConPTY flags) handles cross-platform PTY management.

**Three architectural decisions will determine Therminal's long-term success.** First, the xterm.js-inspired `SequenceInterceptor` hook layer between parser and terminal state enables AI detection, semantic analysis, and custom protocols without forking the emulation core — and makes future backend swaps (to libghostty-vt) possible. Second, the hybrid daemon model (central daemon + per-session workers with headless VTE state) combines tmux's persistence with WezTerm's performance insight that in-process multiplexing should be the fast default. Third, semantic scrollback as a structured SQLite-backed data model transforms the terminal from an opaque byte stream into a queryable knowledge base that every other feature — MCP tools, hotspots, AI queries, agent coordination — can build upon.

**The recommended build order, based on architectural dependencies and market differentiation:**

1. **Core rendering + terminal emulation** (wgpu pipeline, vendored alacritty_terminal, basic PTY management)
2. **Semantic scrollback** (shell integration parsing, typed regions, SQLite sidecar) — the foundation
3. **Daemon multiplexer** (session/window/pane hierarchy, attach/detach, IPC)
4. **AI agent detection** (OSC 133/633 parsing, process tree inspection, state inference)
5. **MCP server** (tools for pane listing, content reading, semantic queries, spawning)
6. **Trust tiers** (per-agent permissions, command filtering, OS sandboxing integration)
7. **GPU overlays** (agent status widgets, tool call cards, thinking indicators)
8. **Actionable hotspots** (pattern detection, OSC 8, action palettes)
9. **Multi-agent coordination** (agent registry, auto-tiling, messaging bus)
10. **WebView hybrid panes** (wry integration, agent dashboards, rich content)

The largest unsolved technical risk is wgpu + WebView compositing for hybrid panes — no clean solution exists today. Keep WebView strictly optional behind a feature flag and use OS child windows rather than attempting in-surface compositing.

Therminal's positioning should be **the open-source AI-native terminal** — combining the AI awareness that only Warp offers (but open-source and privacy-first), the rendering performance that Ghostty achieves (via wgpu), the multiplexing that tmux provides (but built-in with semantic awareness), and the extensibility that Zellij pioneered (via MCP rather than WASM plugins for the agent ecosystem). No terminal in the current landscape occupies this intersection.