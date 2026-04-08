# OSC Code Registry

This file is the canonical table of OSC codes claimed by therminal core and
harness crates. It is the **social contract** that prevents two crates from
racing to claim the same code. Duplicate claims are also caught at runtime by
the registry (which returns `OscRegistrationError::DuplicateCode` and prevents
the daemon from starting), but the table here is the authoritative record.

Normative API reference: `docs/osc-handler-registry.md`.

---

## Reserved ranges

| Range | Owner | Notes |
|---|---|---|
| `0–1023` | therminal core + standard OSCs | Reserved. Harness crates must not claim codes in this range. The registry returns `OscRegistrationError::ReservedCode` if they try. |
| `1024+` | Available for harness crates | Claim by following the process below. |

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
| _(none yet)_ | | The first harness crate to claim a code adds a row here. |

---

## How to claim a new code

To claim an OSC code for a harness crate:

1. **Choose a code `≥ 1024`** that does not appear in the "Claimed codes"
   table above. Pick a specific value, not a range.

2. **Open a PR that does all three of the following atomically:**
   - Add a row to the "Claimed codes" table in this file.
   - Add a call to `register_osc_handler(CODE, "owner", …)` in your crate's
     `activate()` function (see `docs/osc-handler-registry.md` §2.2).
   - Document the marker grammar for the claimed code in your crate's
     `CLAUDE.md` under an "OSC Grammar" section.

3. **Do not split across multiple PRs.** The table row, the `activate()` call,
   and the grammar documentation must land together so the claimed code is
   never in an undocumented or unimplemented state.

4. The runtime duplicate-detection (daemon startup error) is a safety net, not
   a substitute for this table. Races between PRs are resolved by the table;
   the runtime check catches bugs introduced after merge.

### Table row format

```markdown
| CODE | owner-name | Brief description of what sequences this code carries. |
```

`owner-name` must be a lowercase `[a-z0-9_-]+` string and must match the
`owner` argument passed to `register_osc_handler`. It must also match the
`source_id` that will appear in `TerminalEvent` records on the event bus for
events produced by this handler.
