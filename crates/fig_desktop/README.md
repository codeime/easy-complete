# Easy Complete Desktop

Native macOS host (`easy-complete`). The autocomplete overlay and settings
window are GPUI views. Completions run in `ec_engine` on a worker thread.

Follow the [root README](../../README.md) for toolchain and install steps.

## Developing

```bash
cargo run --bin easy-complete
```

Settings open from the menu bar or a `ec://` deep link. There is no dashboard
dev server and no `DASHBOARD_URL`.

Headless completions (no overlay):

```bash
cargo run --bin ec -- engine complete --buffer "git ch"
```

## Layout

- `src/bootstrap/` — event loop, tray, IPC, starts GPUI (`AppRuntime`)
- `src/gpui_host.rs` — process-wide `NSApplication`, event dispatch
- `src/overlay.rs` — completion requests, insertion, caret placement
- `src/settings_ui.rs` — native settings window
- `crates/ec_gpui` — list rendering, theme, AppKit frame
- `crates/ec_engine` — IR lookup, generators, QuickJS hooks
