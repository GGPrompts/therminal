# ADR 0003 — Semantic Pattern Matching: Design Decisions and Rejected Alternatives

**Date:** 2026-04-08
**Status:** Accepted
**Issue:** `tn-3vzi`
**Implementation issue:** `tn-yrjd`

---

## Context

Therminal needs a user-facing customization surface for matching terminal text
output and taking actions (hotspots, widgets, events). This ADR records the
key design decisions and the alternatives considered and rejected.

The adopted design is a config-driven, regex-based pattern matching engine.
Pattern packs are TOML files; the engine compiles regexes at load time and
matches them against finalized lines, prompt boundaries, and semantic regions.
The full schema is in `docs/pattern-matching-spec.md`.

---

## Decision 1: TOML + regex, not a global OSC marker protocol

### Rejected alternative

An earlier scope (tn-xy4a) explored a global OSC marker protocol: every
terminal source would be asked to emit structured OSC escape sequences, and
therminal would parse those sequences to understand what was happening. This
is essentially the approach OSC 7777 (agent self-reporting) and OSC 633 (shell
integration) take.

### Why rejected

The primary customization problem is **uncooperative sources**: `cargo`, pytest,
tsc, bundlers, arbitrary scripts, custom CLIs. None of these emit structured
OSC sequences, and we cannot make them do so. A marker-only approach
solves the cooperative case (where harness crates already work) and leaves
the uncooperative case entirely unaddressed.

The TOML + regex approach works on every source that produces text output,
regardless of cooperation. Cooperative sources with rich structured output
belong in harness crates, not pattern packs — the integration taxonomy in root
`CLAUDE.md` is precise about this distinction.

---

## Decision 2: No in-process plugin trait (dynamic library loading)

### Rejected alternative

An in-process plugin system where users compile shared libraries implementing a
`PatternPlugin` trait. Therminal would `dlopen` the library and call well-known
entry points. This is the approach taken by Neovim plugins, Zellij plugins
(via WASM), and many editor extension ecosystems.

### Why rejected

**Security:** A `dlopen`'d library runs with therminal's full privileges. A
malicious or buggy pack can read all pane content, exfiltrate data over the
network, or crash the terminal. The config-driven approach limits packs to what
the TOML schema expresses; there is no ambient authority surface.

**AI authoring:** The stated design goal is a 20KB skill doc that lets an AI
agent write a correct pack without touching therminal source. A trait-based
plugin requires Rust knowledge, a build toolchain, and understanding of
therminal's internal types. TOML regex requires none of these.

**Shareability:** A compiled `.so` or `.dylib` is platform-specific and
version-coupled. A TOML file works on every platform and survives therminal
version bumps as long as the schema is stable. The schema stability guarantee
is exactly what this SPEC establishes.

**Maintenance burden:** Supporting a stable in-process plugin ABI requires
either semantic versioning discipline on internal types (major cost) or a thin
stable ABI layer (significant design work for marginal benefit given that WASM
solves the isolation problem better anyway).

---

## Decision 3: No embedded scripting language (Lua, WASM, Rhai, Rune)

### Rejected alternative

Embed a scripting runtime (Lua via `mlua`, WebAssembly via Wasmtime, or a
Rust-native scripting language) and let packs contain executable code in
addition to or instead of regexes. This is the approach taken by Neovim
(Lua), Helix (planned WASM), and several terminal emulators with plugin systems.

### Why rejected

**Complexity budget:** Embedding a scripting runtime adds a significant build
and maintenance cost. Wasmtime alone is ~20MB of dependency. For the 90% use
case — "match this text and do one of three actions" — a regex is expressive
enough and carries none of this cost.

**The 10% case has a better answer:** If a pattern pack would need scripting
logic, that is the signal that the source is cooperative enough to justify a
harness crate. Harness crates are in-process Rust code with full access to
therminal APIs; they are the right place for logic that exceeds regex + actions.
The integration taxonomy tripwire makes this decision explicit:

> Does the source cooperate and emit structured data you control?
> → harness crate

> Is the source uncooperative and you need to parse its text?
> → pattern pack

Adding scripting to the pattern pack surface blurs this boundary and creates
a third unlabeled category.

**AI authoring:** A scripting language requires the AI to generate correct,
safe, sandboxed code in an unfamiliar runtime. Regex + static action fields
are trivially verifiable and require no runtime reasoning about side effects.

---

## Decision 4: `regex` crate, not `fancy-regex`

### Rejected alternative

Use the `fancy-regex` crate, which adds lookahead, lookbehind, and
backreferences on top of the `regex` crate via a hybrid NFA/backtracking
engine.

### Why rejected

The bounded-time guarantee provided by the `regex` crate is **load-bearing**
for the performance model. The performance model in
`docs/pattern-performance-model.md` relies on the property that a pattern's
match time on an N-character input is bounded by a constant multiple of N,
regardless of the pattern's content. This property makes the slow-match
threshold (1ms on 200 chars) a reliable circuit breaker.

`fancy-regex` allows patterns that exhibit catastrophic backtracking. A single
poorly-written pattern could consume unbounded CPU on specific input, defeating
the performance model entirely.

The practical cost of the restriction is low: lookahead and backreferences are
infrequently needed for the "match terminal output and highlight/annotate it"
use case. The patterns that need them are almost always better served by a
harness crate with proper parsing.

---

## Decision 5: `{name}` capture reference syntax

### Rejected alternatives

Several capture reference syntaxes were considered:

| Syntax | Source | Reason rejected |
|---|---|---|
| `$name` | Shell-style | Ambiguous when capture name is followed by alphanumerics: `$status_code` vs `${status}_code`. Requires sigil-escaping. |
| `$1`, `$2` | Regex traditional | Positional, fragile when capture order changes. Poor readability. |
| `%(name)s` | Python 2 `%`-format | Unusual outside Python 2; the trailing `s` is surprising in a TOML config context. |
| `${name}` | Shell `${}`-style | Better than `$name` but the `$` character suggests shell execution, which is misleading in a config file. |

### Why `{name}` was chosen

The `{name}` syntax is:

- Unambiguous: `{` is not a legal start of a TOML value, so there is no
  confusion with literal text.
- Familiar to Python 3 f-string and `str.format()` users, which includes most
  AI training data for config authoring.
- Easy to escape: `{{` and `}}` for literal braces, consistent with Python
  `str.format()`.
- Short and readable in label strings: `label = "Error {code} in {file}"` is
  immediately clear.

---

## Decision 6: One action per pattern

### Rejected alternative

Allow a `[[pattern.actions]]` array so a single match can trigger multiple
actions (e.g., both a hotspot and an event).

### Why rejected

**Simplicity:** The schema is significantly simpler with one action per
pattern. A single `action = "hotspot"` field and a single sub-table is
trivially parseable and teachable.

**No practical cost:** Two patterns with the same `match` field achieve the
same result. The engine evaluates patterns independently and there is no
performance penalty for multiple patterns with identical regexes — the `regex`
crate compiles them as separate objects, but match cost is still bounded.

**Composition over configuration:** If a user finds themselves wanting five
actions from one match, that is a signal the pattern is doing too much and
should be refactored into a harness crate.

---

## Consequences

The adopted design establishes the following stable contracts that
implementation (`tn-yrjd`) and downstream tooling (`tn-6o1u`, `tn-jqgf`) must
honor:

1. The TOML field names in `docs/pattern-matching-spec.md` are frozen. Adding
   fields is backward-compatible (unknown fields are ignored); renaming or
   removing fields is a breaking change requiring a major version bump.

2. The `regex` crate is the mandatory regex engine. No pack may depend on
   `fancy-regex` features. The authoring guide (`docs/pattern-packs-authoring.md`)
   documents this as a constraint.

3. Capture reference syntax `{name}` is stable. Alternative syntaxes will not
   be supported.

4. The three action types (`hotspot`, `widget`, `emit_event`) and their
   sub-tables are stable. `run_command` and `modify_pane` remain explicitly
   deferred to v2.
