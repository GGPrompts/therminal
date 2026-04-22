# Kitty Graphics Auto-Detection

How image-preview clients (TFE, viu, chafa, `kitty +kitten icat`, matplotlib,
…) discover that they can speak the Kitty graphics protocol when running
inside therminal.

## TL;DR

Therminal advertises itself in two ways. Most clients check one or both:

1. **`KITTY_WINDOW_ID` environment variable** — sniffed by clients that
   decide protocol *before* spawning. Therminal sets this to a non-empty
   decimal id on every child PTY. The exact value is cosmetic — any
   non-empty string is enough. See `crates/therminal-terminal/src/pty.rs`
   (`set_common_env`).
2. **Feature-query APC** — clients that probe at runtime send
   `\x1b_Gi=1,a=q;\x1b\\`. Therminal replies with `\x1b_Gi=1;OK\x1b\\`
   through the PTY writer. The round-trip is implemented by
   `KittyGraphicsParser` + the graphics response sink on
   `TherminalInterceptor` (tn-7xme). Unit test:
   `intercept_apc_feature_query_emits_ok` in
   `crates/therminal-terminal/src/interceptor.rs`.

Users do **not** need to export anything manually. Launching therminal is
enough.

## What therminal deliberately does not do

- **`TERM` is not overridden.** Setting `TERM=xterm-kitty` would
  auto-enable graphics in a wider set of tools, but it also triggers
  terminfo-keyed behavior (true-color assumptions, `modifyOtherKeys`, etc.)
  that can surprise users. Terminfo is tracked as a separate issue.
- **No sixel / iTerm2 inline-image advertisement.** Only Kitty graphics is
  advertised today. Clients that want sixel must be told explicitly by the
  user.

## Verifying auto-detection

Inside a therminal pane:

```sh
echo "$KITTY_WINDOW_ID"     # should print a non-empty number
printf '\x1b_Gi=1,a=q;\x1b\\' ; sleep 0.1 ; echo   # should print '…Gi=1;OK…'
```

From a supported client:

- **TFE** (`GGPrompts/tfe`) — `detectTerminalProtocol()` in
  `terminal_graphics.go` returns `ProtocolKitty` when `KITTY_WINDOW_ID` is
  set.
- **viu** — `viu path/to/image.png` emits APC escape sequences rather than
  Unicode half-blocks.
- **`kitty +kitten icat`** — renders the image at full resolution.

## Related

- tn-xnsv — this feature (env advertise).
- tn-7xme — APC parser + feature-query response envelope.
- tn-avyj — parent epic (Kitty graphics end-to-end).
- `crates/therminal-terminal/CLAUDE.md` — `graphics/` module layout.
