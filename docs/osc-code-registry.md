# OSC Code Registry

This file is the canonical table of OSC codes claimed by therminal core and
harness crates. It is the **social contract** that prevents two crates from
racing to claim the same code. Duplicate claims are also caught at runtime by
the registry (which returns `OscRegistrationError::DuplicateCode` and prevents
the daemon from starting), but the table here is the authoritative record.

Normative API reference: `docs/osc-handler-registry.md`.
Implementation issue: `tn-hkpz`.

---

## Reserved ranges

| Range | Owner | Notes |
|---|---|---|
| `0–1023` | therminal core + standard OSCs | Reserved. Harness crates must not claim codes in this range. The registry returns `OscRegistrationError::ReservedCode` if they try. |
| `1024–1339` | Reserved for future core / standard extensions | Do not claim. Leaves room to absorb additional widely-deployed OSC codes without touching the harness range. |
| `1340–1399` | Therminal first-party harness crates | Each `therminal-harness-*` crate claims exactly one code in this range. See the "Claimed codes" table below; coordinate across harness crates before picking a value. |
| `1400–1499` | User / community pattern packs and one-off integrations | Treat as experimental. No uniqueness guarantee beyond the individual user's deployment. |
| `1500+` | Unassigned | Future expansion. |

Therminal core currently handles the following codes natively (not via the
registry):

| Code | Protocol | Purpose |
|---|---|---|
| 7 | RFC / iTerm2 | Current working directory (`file://` URI) |
| 9 | ConEmu / mintty | Desktop notifications |
| 133 | FinalTerm | Shell integration prompt/command marks |
| 633 | VS Code | Shell integration (extended marks + command line) |
| 1337 | iTerm2 | Key=value metadata (e.g. `CurrentDir=`) |
| 7777 | Therminal | Cooperative agent self-reporting |

---

## Claimed codes

| Code | Owner | Notes |
|---|---|---|
| 1341 | `claude` (`therminal-harness-claude`) | Claude Code state markers — primary signal for session state (tn-nrur). Keys: `state`, `tool`, `session_id`, `cwd`, `context_percent`, `model`, `subagent_start`, `subagent_stop`. See `crates/therminal-harness-claude/src/markers.rs` for the grammar. Emits `claude.state` and `claude.subagent` events on the harness bus. |

---

## How to claim a new code

To claim an OSC code for a harness crate:

1. **Choose a code in your assigned range** (see "Reserved ranges" above).
   First-party harness crates use `1340–1399`; pick a value that does not
   already appear in the "Claimed codes" table.

2. **Open a PR that does all three of the following atomically:**
   - Add a row to the "Claimed codes" table in this file.
   - Add a call to `OscHandlerRegistry::register(CODE, "owner", …)` (or the
     forwarding `TherminalInterceptor::register_osc_handler(...)`) from your
     crate's `activate(…)` function (see `docs/osc-handler-registry.md` §2.2).
   - Document the marker grammar for the claimed code in your crate's
     `CLAUDE.md` under an "OSC Grammar" section.

3. **Wire the activation call into the daemon.** Add a call to your crate's
   `activate(…)` function in `crates/therminal-daemon/src/ensure.rs`, next
   to the existing `therminal_harness_claude::activate_markers(&osc_registry)`
   call. Use `.expect(...)` on the result so a duplicate-claim bug fails the
   daemon at startup with a clear error message.

4. **Do not split across multiple PRs.** The table row, the `activate()` call,
   the `ensure.rs` wiring, and the grammar documentation must land together
   so the claimed code is never in an undocumented or unimplemented state.

5. The runtime duplicate-detection (daemon startup error) is a safety net, not
   a substitute for this table. Races between PRs are resolved by the table;
   the runtime check catches bugs introduced after merge.

### Table row format

```markdown
| CODE | `owner-name` (`crate-name`) | Brief description of what sequences this code carries. |
```

`owner-name` must be a lowercase `[a-z0-9_-]+` string and must match the
`owner` argument passed to `register_osc_handler`. It must also match the
`source_id` that will appear in `TerminalEvent` records on the event bus for
events produced by this handler.

---

## Implementation notes

- The registry lives in `crates/therminal-terminal/src/osc_registry.rs`.
  `TherminalInterceptor` carries an `Arc<OscHandlerRegistry>`; the daemon
  constructs one registry per process in `ensure.rs` and clones the `Arc`
  into every pane's interceptor via `SessionManager::set_osc_registry`.
- Harness handlers return `Option<HarnessEvent>`. `None` means "drop this
  sequence silently" — use it for dormant harnesses, malformed payloads,
  or markers that carry nothing the harness cares about yet.
- Panics inside a handler are caught by `std::panic::catch_unwind`; on the
  first panic the handler is replaced with a no-op and marked disabled.
  See `docs/osc-handler-registry.md` §3.3.
- The registry is consulted **only after** native OSC handling for codes
  `7 / 9 / 133 / 633 / 1337 / 7777`. A harness crate cannot shadow core
  OSC behaviour regardless of registration order.
