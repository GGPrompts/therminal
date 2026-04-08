# Pattern Matching Engine — Performance Model

**Companion document to:** `docs/pattern-matching-spec.md`

**Status:** SPEC — no implementation yet. Consumer issue: `tn-yrjd`.

The performance model for the semantic pattern matching engine exists to
provide hard guarantees: a user with 200 loaded patterns should not see any
measurable latency increase compared to a user with zero patterns.

---

## 1. Compilation model

### 1.1 Compile once, reuse forever

Regex objects are compiled exactly once, at pack load time. Every subsequent
match call on a given pane reuses the same compiled `Regex` instance. There
is no per-match compilation overhead.

Hot-reload (when a pack file changes) recompiles the affected pack and swaps
it atomically. During the swap, in-flight matches complete with the old
compiled set; new matches pick up the updated set. There is no lock held
across match execution.

### 1.2 Bounded-time guarantee

All patterns use the Rust `regex` crate, which provides a bounded-time
guarantee: match time is O(input_length × pattern_complexity) and is immune to
catastrophic backtracking. This guarantee is the reason `fancy-regex` is
excluded (see `docs/adr/0003-semantic-pattern-matching.md`).

The bounded-time guarantee applies per-pattern, not per-pack. A pack with
10 patterns that each take 0.1ms will take ~1ms total — linear in pattern
count, not exponential.

---

## 2. Dispatch model

### 2.1 Scope-indexed dispatch

The engine maintains a scope index at load time:

```
scope_index: {
    finalized_line:   Vec<PatternRef>,   // patterns with scope = "finalized_line"
    prompt_boundary:  Vec<PatternRef>,   // patterns with scope = "prompt_boundary"
    region:           Vec<PatternRef>,   // patterns with scope = "region"
}
```

When a `finalized_line` event fires, only `scope_index.finalized_line` is
consulted. Patterns in other scopes are not evaluated, not iterated, not
touched.

### 2.2 Pane-scope filtering

Before invoking any match, the engine applies `applies_to` filtering (§6 of
the spec):

1. Query the process map for the pane's active harnesses.
2. Filter `scope_index[scope]` to patterns whose `applies_to` is satisfied.
3. Match only the filtered subset.

Step 1 is a single hash-map read. Step 2 is a linear scan of the patterns
already in the scope index, comparing `applies_to` values. Neither touches the
PTY stream.

**Net cost per finalized line:**

```
O(patterns_in_finalized_line_scope_applicable_to_this_pane)
```

A pane running bash with no harness active and no command-scoped patterns
loaded will have zero `prompt_boundary` evaluations and zero harness-scoped
`finalized_line` evaluations. Only globally-scoped `finalized_line` patterns
run.

### 2.3 Parallelism

Pattern matching is CPU-bound and embarrassingly parallel per pattern. The
implementation MAY run patterns in parallel using a thread pool when the
scope-filtered pattern count exceeds a threshold (recommended: 10 patterns).
The threshold is an implementation detail; the SPEC does not mandate it.

---

## 3. Per-pattern budget

### 3.1 Slow-match threshold

A pattern match that takes **more than 1ms** on a **200-character input** is
considered slow. This threshold is chosen to be 10× below the 10ms threshold
at which single-frame latency becomes perceptible at 60fps.

### 3.2 Strike counter

Each pattern carries a `slow_strike_count` that increments when a match
exceeds the slow threshold. After **3 consecutive slow strikes**, the pattern
is **disabled** for the remainder of the daemon session:

- Disabled patterns are skipped during dispatch (zero match cost).
- A `WARN`-level daemon log is emitted: `pattern-match: slow pattern disabled:
  <pack>/<name>, avg_ms=<N>`.
- The pattern appears in `terminal.patterns.stats` with `status = "disabled"`.

"Consecutive" means three slow matches in a row without an intervening fast
match. A fast match resets the strike counter to 0.

### 3.3 Measurement

Match time is measured using a monotonic clock (not wall clock) taken
immediately before and after the `regex.find()` or `regex.captures()` call.
The measurement includes capture extraction but not capture reference
expansion (that cost is paid at action dispatch time, not match time).

The implementation MUST NOT measure time across async yield points. If the
match is run on a background thread, the measurement covers only the thread's
compute time.

---

## 4. Load-time limits

### 4.1 Pattern count cap

The engine enforces a global limit of **500 patterns** across all loaded packs
combined. If loading would exceed this limit, the engine:

1. Loads packs in filesystem order (alphabetical within each directory).
2. Accepts patterns until the cap is reached.
3. Skips all remaining patterns with a `WARN`-level log:
   `pattern-pack: global cap (500) reached, skipping <pack>/<name>`.
4. Reports the overflow in `terminal.patterns.stats`.

500 was chosen as a limit that is orders of magnitude larger than any
realistic user pattern set while still being small enough that 500 regexes
run against every line cannot cause perceptible latency (a compiled `regex`
match on a 200-char string takes ~1-50 µs depending on pattern complexity;
500 of them is 0.5-25ms in the worst case, which the slow-match circuit above
addresses by disabling outliers).

### 4.2 Regex size limits

The engine rejects regexes that exceed **4096 characters** at load time. This
prevents patterns that are syntactically valid but compile to extremely large
NFA states. Load error: `pattern-pack: <pack>/<name>: regex too long (>4096
chars)`.

---

## 5. Memory model

### 5.1 Compiled regex memory

A compiled `Regex` object for a typical pattern occupies 1-100KB of heap
depending on alternation complexity. 500 patterns × 100KB = 50MB in the
absolute worst case. In practice, patterns are much simpler and the ceiling
is closer to 5MB.

No limit is enforced on compiled regex memory beyond the pattern count cap.

### 5.2 Scope index memory

The scope index holds `PatternRef` values (indices or Arc pointers), not
copies of the compiled regex. Scope index memory is proportional to pattern
count and negligible.

### 5.3 Widget buffer memory

When the widget rendering substrate (`tn-npd`) is not yet available, the
engine buffers pending widget placements in a bounded ring buffer per pane:
capacity **1,000 placements**. Overflow evicts the oldest entry. This prevents
unbounded memory growth when a pattern fires frequently before the renderer is
ready.

---

## 6. Metrics

### 6.1 Per-pattern stats

The engine tracks per-pattern metrics in a compact in-memory table:

| Metric | Description |
|---|---|
| `match_count` | Total successful matches since pack load |
| `miss_count` | Total evaluations with no match |
| `avg_match_ms` | Rolling average match time (last 100 evaluations) |
| `slow_count` | Total matches that exceeded the slow threshold |
| `status` | `active`, `disabled`, or `error` (load error) |
| `last_match_ts` | Unix timestamp (ms) of the most recent match, or null |

### 6.2 Per-pack stats

Pack-level aggregates:

| Metric | Description |
|---|---|
| `pattern_count` | Total patterns in the pack (including errored) |
| `active_count` | Patterns currently active |
| `disabled_count` | Patterns disabled by the slow-match circuit |
| `error_count` | Patterns that failed to load (regex error or schema error) |
| `load_errors` | List of `{ name, error }` objects for load failures |

### 6.3 MCP tool: `terminal.patterns.stats`

```
terminal.patterns.stats
  → { packs: [ { pack_name, active_count, disabled_count, error_count,
                  load_errors: [...],
                  patterns: [ { name, match_count, miss_count, avg_match_ms,
                                slow_count, status, last_match_ts } ] } ],
      global: { total_loaded, total_active, total_disabled,
                cap_reached: bool, cap_limit: 500 } }
```

Trust tier: **Sandboxed** (read-only, no sensitive data).

### 6.4 CLI command: `therminal semantic patterns stats`

```text
therminal semantic patterns stats [--pack <name>] [--json]
```

Default output: one line per pattern, tab-separated:

```
pack_name  pattern_name  status  match_count  avg_match_ms
```

`--pack <name>` filters to a single pack. `--json` emits the full structured
object matching the MCP tool response.

Implementation tracked in `tn-yrjd`. The CLI command is a thin wrapper over
the same daemon-client call the MCP tool uses, per the `therminal` CLI
architecture in `docs/cli.md`.

---

## 7. Config knobs

```toml
[patterns]
# Directory to load user pattern packs from. Default: ~/.config/therminal/patterns
directory = "~/.config/therminal/patterns"

# Enable or disable the pattern engine entirely. Default: true
enabled = true

# Global pattern count cap. Default: 500. Range: 1-2000.
max_patterns = 500

# Slow-match threshold in milliseconds. Default: 1.0
slow_match_threshold_ms = 1.0

# Number of consecutive slow strikes before a pattern is disabled. Default: 3
slow_strike_limit = 3
```

All keys live under `[patterns]` in `therminal.toml`. Hot-reload is supported
for `enabled` (immediately toggles the engine). Changes to `max_patterns`,
`slow_match_threshold_ms`, and `slow_strike_limit` take effect after the next
pack reload.

---

## 8. Summary: cost guarantees at a glance

| Guarantee | Value |
|---|---|
| Compilation cost | Once per pack load, not per match |
| Per-finalized-line cost | O(globally-scoped patterns + harness-scoped patterns for active harness) |
| Slow-match threshold | 1ms on 200-char input |
| Automatic slow-pattern mitigation | Disabled after 3 consecutive slow strikes |
| Global pattern cap | 500 |
| Regex flavor | Rust `regex` — no catastrophic backtracking |
| Widget buffer (substrate unavailable) | 1,000 placements per pane, ring-evicts oldest |
