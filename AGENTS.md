# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd prime` for full workflow context.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
bd-pull               # Pull beads data from Hetzner Dolt remote
bd-push               # Push beads data to Hetzner Dolt remote
```

## Embedded Dolt Setup

This repo is configured for **embedded Dolt mode**:

```json
{
  "backend": "dolt",
  "dolt_mode": "embedded",
  "dolt_database": "tn"
}
```

Use the workflow from `~/BeadsHive/plugins/beads-planner/commands/gg-migrate.md`:

- Daily issue work happens locally through `bd`
- Cross-device sync uses the shell helpers `bd-pull` and `bd-push`
- Do **not** rely on `bd sync`
- Do **not** rely on `bd dolt push` in embedded mode here; the migration doc notes it does not pass `--user` correctly for remotesapi

If the shell helpers are not loaded in the current shell, use the documented fallback:

```bash
DB="$(command grep -oP '"dolt_database"\s*:\s*"\K[^"]+' .beads/metadata.json)"

# Pull from Hetzner
(cd ".beads/embeddeddolt/$DB" && dolt pull --user beads origin main)

# Push to Hetzner
(cd ".beads/embeddeddolt/$DB" && dolt push --user beads origin main)
```

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
bd-pull               # Pull beads data from Hetzner Dolt remote
bd-push               # Push beads data to Hetzner Dolt remote
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   bd-pull                 # if switching from another device or before reconciling tracker state
   git pull --rebase       # only when the worktree is clean enough to rebase safely
   bd-push                 # or the documented dolt fallback if helper functions are unavailable
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
- In embedded Dolt mode, `bd-push`/`bd-pull` are the expected sync path; if they are unavailable, use the explicit `dolt push --user beads` / `dolt pull --user beads` fallback inside `.beads/embeddeddolt/<db>`
<!-- END BEADS INTEGRATION -->
