# therminal-core

Color palette, wgpu context, text renderer, TOML config, hot-reload.

## Configuration System

TOML-based config with hot-reload.

### Config File

Location: `therminal_runtime::paths::config_dir() / "therminal.toml"` (e.g. `~/.config/therminal/therminal.toml` on Linux).

Sections: `[general]` (window, scrollback, shell), `[font]` (family, size, line_height_scale), `[colors]` (hex overrides for palette), `[keybindings]` (key/action pairs), `[profiles]` (named session profiles), `[trust]` (agent trust tiers), `[mcp]` (MCP server enable/disable, socket path), `[hotspots]` (clickable terminal content — `editor_chain` for file hotspots, `folder_pane_command` for in-pane directory spawn defaulting to `tfe`, `folder_opener` for the "reveal in file manager" fallback chain — see tn-zqwg).

All fields have sensible defaults. Missing fields fall back to defaults. Invalid TOML logs a warning and uses full defaults.

### Module Structure

```
src/config/
├── mod.rs           # TherminalConfig, load/save, section structs + defaults
├── keybindings.rs   # KeyAction enum, Keybinding, parse_binding()
└── config_text.rs   # default_config_text() — commented TOML generation
```

### Hot-Reload

`ConfigWatcher` (in `config_watcher.rs`) uses the `notify` crate to watch the config directory. Events are debounced (500ms) to handle editor atomic-write patterns. On change, the config is reloaded and a `ConfigChanged` event is sent to the winit event loop via a bridge thread. The `App::apply_config()` method applies changes (window title, font metrics, grid resize) without restart.

### Rules

- **Config fields must be wired**: if a config struct has a field, code must read it. Don't declare config options that nothing uses — dead config misleads users and future contributors.
- `TherminalConfig` is the single source of truth; other crates consume it, not duplicate it.
