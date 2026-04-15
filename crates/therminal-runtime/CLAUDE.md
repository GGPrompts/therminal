# therminal-runtime

Cross-platform path resolution and runtime directory management.

## Module Structure

```
src/
├── lib.rs    # Crate root, re-exports paths and wsl modules
├── paths.rs  # All path helpers and ensure_* functions
└── wsl.rs    # Shared WSL detection helpers (tn-9ixz)
```

## Path Functions

All functions return absolute `PathBuf` values. The `dirs` crate provides platform-native base directories.

| Function | Purpose | Linux | macOS | Windows |
|----------|---------|-------|-------|---------|
| `config_dir()` | Config files (therminal.toml) | `$XDG_CONFIG_HOME/therminal` | `~/Library/Application Support/therminal` | `{RoamingAppData}\therminal` |
| `data_dir()` | Persistent data | `$XDG_DATA_HOME/therminal` | `~/Library/Application Support/therminal` | `{RoamingAppData}\therminal` |
| `cache_dir()` | Ephemeral cache | `$XDG_CACHE_HOME/therminal` | `~/Library/Caches/therminal` | `{LocalAppData}\therminal` |
| `runtime_dir()` | Sockets, pidfiles, lockfiles | `$XDG_RUNTIME_DIR/therminal` | `$TMPDIR/therminal` | `{LocalAppData}\therminal` |
| `resources_dir()` | Shell scripts, bundled assets | Resolved via priority chain (see below) | Same | Same |

### IPC Endpoint Paths

- **`socket_path(name)`** -- Unix: `<runtime_dir>/<name>.sock`. Windows: `\\.\pipe\therminal-<name>`.
- **`pidfile_path(name)`** -- `<runtime_dir>/<name>.pid`
- **`lockfile_path(name)`** -- `<runtime_dir>/<name>.lock`

### Resources Directory Resolution

`resources_dir()` tries these locations in order:

1. `THERMINAL_RESOURCES_DIR` env var (packaging/custom layouts)
2. `<exe_dir>/../resources` (standard install layout)
3. `<exe_dir>/resources` (flat layout)
4. `CARGO_MANIFEST_DIR`-relative workspace root (debug builds only)
5. `<data_dir>/resources` (final fallback)

### Directory Creation

- `ensure_runtime_dir()` -- Creates runtime dir with `0o700` permissions on Unix.
- `ensure_config_dir()`, `ensure_data_dir()`, `ensure_cache_dir()` -- Create respective directories.

## Cross-Platform Considerations

- **Linux**: Uses XDG base directories. Runtime dir falls back to `/tmp/therminal-<user>` if `XDG_RUNTIME_DIR` is unset.
- **macOS**: Uses `~/Library/...` paths. Runtime dir uses `$TMPDIR` (per-user, launchd-managed). Falls back to `/tmp/therminal-<user>`.
- **Windows**: Uses `FOLDERID_RoamingAppData` and `FOLDERID_LocalAppData`. Socket paths return named pipe paths (`\\.\pipe\therminal-*`) instead of filesystem paths.
- **Headless/no-home**: All standard dir functions fall back to `/tmp/therminal` with a `tracing::warn`.

## WSL Detection (tn-9ixz)

Shared WSL helpers extracted from the harness and app crates. All functions are cached in `OnceLock`s — one probe per process instead of two.

| Function | Purpose |
|----------|---------|
| `detect_default_distro()` | Cached `wsl.exe -l -q` output, BOM-stripped, first non-empty line |
| `detect_wsl_home()` | Cached `wsl.exe -e sh -c 'printf %s "$HOME"'` |
| `linux_to_unc(distro, path)` | Build `\\wsl.localhost\<distro>\<path>` from components |
| `is_safe_distro_name(name)` | Allowlist check for known-safe distro names |
| `is_wsl_unc_path(path)` | Check `\\wsl.localhost\` prefix |

Consumers: `therminal-harness-claude/src/wsl_paths.rs` (state file / JSONL path resolution), `therminal-app/src/window/wsl_paths.rs` (hotspot path translation).

## Consumers

This crate is consumed by every other crate that needs canonical paths and WSL detection -- `therminal-core` (config file location), `therminal-daemon` (socket binding, session persistence), `therminal-harness-claude` (WSL path resolution), and `therminal-app` (resource discovery, WSL path translation).
