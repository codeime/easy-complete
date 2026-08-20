# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`Easy Complete` is a macOS terminal autocomplete app — a fork of the Amazon Q Developer CLI. It provides IDE-style inline completions in the terminal via a native overlay window. Key identifiers:

- **App bundle ID**: `dev.emmmm.easy-complete`
- **IME bundle ID**: `dev.emmmm.easy-complete.inputmethod`
- **CLI binary**: `ec`
- **PTY binary**: `ecterm`
- **Desktop binary**: `easy-complete`

## Build & Run

### Rust

```bash
# Build all release binaries
cargo build --release -p fig_desktop -p figterm -p ec_cli -p fig_input_method

# Run a specific crate in dev mode
cargo run --bin ec -- <subcommand>
cargo run --bin easy-complete

# Headless completion (uses bundle/specs-ir)
cargo run --bin ec -- engine complete --buffer "git ch"

# Lint (CI enforces -D warnings)
cargo clippy --locked --workspace --color always -- -D warnings

# Format
cargo fmt

# Test a specific crate
cargo test -p <crate_name>

# Test a single test by name
cargo test -p <crate_name> <test_name>
```

### TypeScript

```bash
# Build all packages
pnpm turbo build --filter="./packages/*"

# Compile bundled Fig specs to JSON IR + extracted JS hooks
node scripts/compile-spec-ir.mjs

# Lint all packages
pnpm lint
pnpm lint:fix

# Run tests (Vitest)
pnpm test
pnpm test:ci   # with coverage
```

### Full Install (macOS)

```bash
./scripts/install.sh    # builds Rust + TS, assembles .app, installs to /Applications
./scripts/uninstall.sh  # complete removal
```

### Release

```bash
# Bump version across Cargo.toml and TS packages, then follow the printed steps
./scripts/bump-version.sh <version>   # e.g. ./scripts/bump-version.sh 2.0.18
```

The script outputs the exact next steps:

1. Add a `## v<version>` entry to both `CHANGELOG.md` (English) and `CHANGELOG.zh-CN.md` (Chinese) — keep them in sync, one entry each
2. `git add -A && git commit -m "chore: bump version to v<version>"`
3. `git tag v<version> && git push origin main --tags`

## Architecture

### Multi-Process Design

Three cooperating native processes communicate via Unix domain sockets (protobuf messages):

1. **`easy-complete`** (`fig_desktop`) — Native desktop app. Owns the GPUI autocomplete overlay (`ec_gpui` + `overlay.rs`) and the GPUI settings window (`settings_ui.rs`). Hosts the completion engine worker, system tray, Accessibility / IME plumbing, and process-wide `NSApplication` via `gpui_host.rs`.

2. **`ecterm`** (`figterm`) — Pseudoterminal that sits between the user's shell and their terminal emulator. Intercepts keystrokes and the shell edit buffer to drive autocomplete. Built on a vendored fork of `alacritty_terminal`. Process title is rewritten to `<shell> (ecterm)`.

3. **`ec`** (`ec_cli`) — CLI entry point. Subcommands include `setup`, `integrations`, `hook`, `settings`, `diagnostic`, `engine`, and more.

**`ecterm` is the process that multiplies, so its grid must not cache rows it cannot reach.** One stays resident per terminal tab, which makes it the largest consumer in a normal session — seven tabs outweighed the desktop app 88 MB to 53 MB. `figterm` builds its `Term` with `max_scroll_limit = 1` and reads only the current prompt line down to the viewport bottom. `Storage::initialize` used a flat `MAX_CACHE_SIZE = 1000` spare-row cache, so the first scroll allocated a thousand full-width rows the grid could never index and held them for the life of the tab: **one 40x120 `Term` cost 3.6 MB of grid where it now costs 0.4 MB**, so roughly 3 MB per tab, more at wider columns (process totals are 10–17 MB). `initialize`, `grow_visible_lines` and `shrink_lines` now take that ceiling (`Grid::max_addressable_rows` — scrollback plus viewport) and clamp the cache to it. A deep scrollback still gets the full cache, which `a_deep_scrollback_still_gets_a_cache` pins, and the `max(max_rows, required)` floor must stay: the buffer has to remain long enough to index every active line whatever ceiling the caller passes.

**That one line of scrollback is load-bearing — do not take `max_scroll_limit` to 0.** `get_current_buffer` bails when `topmost_line() > cmd_cursor.line`, and `topmost_line()` is `Line(-history_size)`. With one line of history a prompt that has just scrolled off the top of the viewport is still readable; with none, that read returns `None` and the edit buffer is lost for the rest of the command.

`ecterm`'s Tokio pool is two workers, not one per core. The main loop is a single `block_on`; the rest is I/O. A ten-core machine used to start ten worker threads in every tab just to sit idle.

**`fig_util` must not link AppKit.** `ecterm` depends on it, and a single `NSWorkspace` call in `open_url` pulled AppKit + Metal + IOAccelerator into every tab (measured: `otool -L` listed AppKit, vmmap showed a 132 MB IOAccelerator mapping). URL opens go through `/usr/bin/open`. Do not put `objc2-app-kit` / `macos-utils` back on `fig_util` to make that call in-process.

Two things measured as *not* worth doing there. Dropping `figterm`'s ten unused direct dependencies (`fig_install`, `crossterm`, `serde`, `parking_lot`, …) shrank the release `ecterm` by 2.8 KB, not the megabytes it looks like it should — the linker was already dead-stripping all of it, so that edit buys build time and an honest manifest, nothing else. And the completion engine is not where the desktop app's memory goes: `Registry` indexes `specs-ir` (a 22 KB `index.json`, ~734 names) and parses at most 48 spec trees under an LRU, so the 35 MB on disk is never resident. `JsHost.sources` does cache hook JS forever with no eviction, but the whole `hooks/` tree is 5.8 MB across 1480 files and a session touches a handful, so it sits in the low hundreds of KB — left alone deliberately.

### IPC

- **Local IPC**: `fig_ipc` — Unix sockets, typed via Protobuf (`proto/` → `fig_proto`)
- **Remote IPC**: `fig_remote_ipc` — WebSocket-based, used for SSH/remote sessions
- Protobuf `.proto` files live in `proto/`; generated Rust types in `fig_proto`; generated TS types in `packages/api-bindings`

### Shell Integration

`fig_integrations` and `fig_install` inject hooks into shell rc files (`.zshrc`, `.bashrc`, fish config). These hooks report shell state (CWD, command text, cursor position) back to `figterm` via IPC on every prompt and keystroke.

`ec init` stands down entirely when the desktop app is not running (`suppress_without_desktop_app`), so VS Code Terminal Suggest, Otty and friends keep their own completions. This is a point-in-time decision made when the shell starts: a terminal opened while the app is down remains uninstrumented for that session. After launching Easy Complete, open a new terminal to enable its completions.

### macOS Input Method (IME)

`fig_input_method` is an IMKit helper app (`EasyCompleteInputMethod.app`) bundled inside the main `.app` at `Contents/Helpers/`. It enables cursor position tracking in terminals that bypass the standard PTY path (Ghostty, Otty, Kitty, WezTerm, Zed, Alacritty).

- The IME self-registers with TIS on startup via `TISRegisterInputSource` (requires NSApplication context)
- Integration install/uninstall is managed via `ec integrations install input-method`
- Enabled state is stored in SQLite: `~/Library/Application Support/easy-complete/data.sqlite3`, table `state`, key `input-method=dev.emmmm.easy-complete.inputmethod.enabled`

**A terminal binds to the IME process, not to the input source.** macOS hands Otty / Ghostty / Kitty an IMK connection when the window is created and never re-attaches it, so killing the IME leaves every open window talking to a dead server. `TISRegisterInputSource` and `TISEnableInputSource` do not rebind those clients. Do not disable the source to chase a reconnect: `TISDisableInputSource` followed by `TISEnableInputSource` — same turn or a later one, four retries — still leaves the palette source disabled. That is what took Otty down after install (no caret, so the overlay stayed hidden). The IME only enables itself. Existing windows stay bound to the dead process; taking the source down does not rebind them and also costs every new window. A HIToolbox fallback that writes only `AppleSelectedInputSources` is the same hole: the palette looks selected, `is_installed` used to say OK, and `AppleEnabledInputSources` still omits us, so a new Otty window never gets an IMK connection. `force_enable_in_hitoolbox` writes both lists.

**The palette write lives in `ec_hitoolbox` and must stay in-process.** Both the IME (from its own `NSApplication`) and `ec integrations install input-method` (from a CLI with no run loop) have to repair those two lists, so a single leaf crate over CoreFoundation owns the read-modify-write and both link it. Two reasons it is not a subprocess. TIS launches the helper with a bare `PATH`, where `python3` is the Command Line Tools stub — absent on a machine that never installed them — and a palette write that fails costs the caret in every IME-only terminal. And `defaults export`/`import` rewrites the *whole* domain: `install.sh` runs the CLI install and an IME launch in the same pass, so whichever finished second dropped the other's entry. `ec_hitoolbox` writes the two keys with `CFPreferencesSetAppValue`, synchronizes the domain before reading so a long-lived process does not merge into its own cached snapshot, and preserves every foreign entry verbatim. The only thing it ever removes is a `Non Keyboard Input Method` under the same reverse-DNS vendor prefix, i.e. this palette under an older bundle ID; an unparsable bundle ID yields an empty prefix, which must keep matching nothing rather than every palette on the machine. Its round-trip tests run against a scratch domain — never `com.apple.HIToolbox` — and skip when cfprefsd refuses a write, which is what a sandboxed runner does.

**Replace the IME process only when its binary changed.** `scripts/install.sh` compares SHA-256 of the staged helper against the installed one. Identical → leave the process running and keep `Contents/Helpers` in place (`ditto` merges, so the rest of `Contents` is still cleared; it allocates a new inode per file, so the live process keeps its own mapped copy). Different → stop it, write the new file, and launch it. The replacement only enables its TIS source; it does not disable it first. `InputMethod::install` applies the same rule through the `input-method.launched-binary-sha256` state key, with `ensure_current_binary_running` as the single place that decides. A missing tracker is *not* a reason to kill — that would undo a same-hash keep — so pin the hash instead. Never pkill because TIS failed to recognise the source: from a CLI process with no `NSApplication` that check is almost always false. After SIGTERM, wait until the process is actually gone (SIGKILL if it is not) before launching, because `open` on a live bundle only activates the old process and recording the new hash against it would hide the stale helper forever. Do not add a "restart your terminal" prompt back, and do not add a second symlink-fixing pass: `install` already repoints and re-registers the symlink, and a parallel `migrate` task raced it on the same path under `~/Library/Input Methods`.

**The desktop app is not exempt from restarting.** `install.sh` wipes and re-dittos the bundle, and `Contents/Resources/specs-ir` is read lazily at completion time (`js_host` loads `hooks/*.js` on first use), so leaving the old process up points a live app at deleted files and completions fail silently until the next launch. It holds no IMK connections worth preserving, so it is always stopped first and relaunched at the end. Its hash gates exactly one thing: `tccutil reset Accessibility`, since an identical binary keeps its code-signing identity and therefore the grant the user already gave.

When the caret never arrives, the overlay stays hidden. There is no window-rect fallback: a list placed from the window frame lands away from the real cursor, which is worse than no list.

**`fig_input_method` deliberately links almost nothing.** It stays resident for as long as a terminal holds an IMK connection, so it must not pull the desktop stack: `fig_ipc`, `fig_proto`, `fig_log`, `fig_util` and `macos-utils` between them drag in tokio, prost, tracing-subscriber, regex and sysinfo, which cost ~9 MB of footprint for a process whose whole job is one caret query per keystroke. The four pieces it actually needs are reimplemented locally and pinned to their originals by tests that take `fig_proto` / `fig_util` as **dev**-dependencies: `paths.rs` (socket + log paths vs `fig_util::directories`), `wire.rs` (the caret frame, byte-for-byte vs the prost encoder), `terminals.rs` (bundle IDs vs `Terminal::supports_macos_input_method`) and `logging.rs` (a file and a `Q_LOG_LEVEL` level). Add a dependency here only after checking what it costs; if you need something from `fig_util`, copy it and extend the equivalence test. `ec_hitoolbox` is the one exception, and only because it is a leaf over CoreFoundation, which this process already links for TIS.

The caret sender is one `std` thread that coalesces queued frames to the newest, skips the connect when the desktop socket is absent, and retires itself after 30 s idle. An idle IME is therefore just its AppKit main thread (~7 MB).

### Native UI

The overlay and settings window are GPUI views, not WebViews. Do not reintroduce `wry` / WKWebView for either surface.

- **Overlay** — `crates/ec_gpui` renders the list; `crates/fig_desktop/src/overlay.rs` owns completion requests, insertion, intercept flags, and caret placement. The window is parked with `orderOut` when hidden (last size kept). Caret coordinates are Quartz (top-left, origin at the primary display). Convert to Cocoa with `NSScreen.screens[0]`, **not** `mainScreen` — `mainScreen` is the focused display and breaks external-monitor placement.
- **Settings** — `crates/fig_desktop/src/settings_ui.rs`. Appearance / Behavior / About, plus a permission gate. Language key is still `dashboard.language` (`system`, `en`, `zh-CN`).
- **Host** — `crates/fig_desktop/src/gpui_host.rs` runs one `NSApplication` for overlay + settings + tray.

`overlay.rs` reproduces the WebView's `HIDDEN_UNTIL_KEYPRESS` triggers: backspacing a whole token away, and a buffer change that does not look like typing (a paste or a shell history recall). Both hide the list until the next keystroke; an insertion the overlay made itself is exempt, which is what `self_insertion` tracks.

Relevant settings:

| Key                | Default | Effect                                                                                                              |
| ------------------ | ------- | ------------------------------------------------------------------------------------------------------------------- |
| `dashboard.language` | unset | Settings UI language: `system`, `en`, or `zh-CN`                                                                    |
| `app.silentLaunch` | `false` | Start without opening settings, same as `--no-dashboard`. A `ec://` deep link naming a page overrides it            |
| `autocomplete.scriptTimeout` | `5000` | Per-hook script budget in ms. The overlay retires `···` after this plus 1s (floor 2s) but keeps waiting, so a late result still renders. The engine worker's wedged-thread watchdog stays at ≥30s. |

The WebView overlay and dashboard sources (`packages/autocomplete-app`, `packages/dashboard-app`) were deleted after v2.2.2 — nothing loaded them once both surfaces went native. Read them out of git when you need the legacy insertion / ranking behavior as a reference. The repo carries no tags, so use the v2.2.2 bump commit `edf0936a`: `git show edf0936a:packages/autocomplete-app/src/state/insertion.ts`, or `git worktree add /tmp/ec-baseline edf0936a` for a full tree to diff against.

The Rust half of that bridge is gone too: `fig_desktop/src/protocol/` (the `fig://`, `ecresource://` and `spec://` handlers), `fig_desktop/src/request/`, and most of `fig_desktop_api`. `crates/fig_desktop/src/webview/` keeps its name but is now just the app bootstrap — `WebviewManager` owns the event loop, tray and IPC, and starts GPUI. Do not restore `#![allow(dead_code)]` on those modules; it is what let the bridge rot in place unnoticed. Note that `mod` versus `pub mod` matters here: a `pub mod` at the crate root exempts its items from both the `dead_code` lint and clippy's `avoid_breaking_exported_api` lints, which hides exactly this class of leftover.

`crates/fig_desktop/src/platform/` still carries `linux/` and `windows.rs` behind the `cfg_if!` in `platform/mod.rs`, but **neither compiles** and neither has since before the GPUI migration. `windows.rs` refers to `RelativeDirection` and `FigWindowMap`, two types that no longer exist anywhere in the crate; `linux/` needs `x11rb`, `zbus` and `dbus`, none of which are declared in `Cargo.toml`. Only the macOS branch is real, so `cargo clippy --workspace -- -D warnings` on macOS covers everything that builds. Treat those two branches as fork archaeology: reviving either means repairing it against the current `event.rs` / `webview` types, not just flipping a target.

### Completion engine

`ec_engine` runs on a dedicated worker thread (`EngineClient`). Bundled Fig specs are compiled at build time by `scripts/compile-spec-ir.mjs` into `bundle/specs-ir/` (JSON IR + extracted hook modules). `build-app.sh` copies that tree to `Contents/Resources/specs-ir/`. Override the directory with `EC_SPECS_DIR`.

Most completions are pure Rust (lookup, builtins, file paths, history, ranking). QuickJS (`rquickjs`) runs only when the current argument's generator has a JS hook:

| Hook            | Role                                              |
| --------------- | ------------------------------------------------- |
| `postProcess`   | Native script stdout → suggestion rows            |
| `script`        | JS returns the command line; Rust executes it     |
| `custom`        | Whole generator in JS; may call injected `exec`   |
| `generateSpec`  | Walk-time: JS returns a spec merged into the node |

The JS runtime is thread-local and created on first hook. Empty `cwd` skips JS hooks. Results are cached (`cached_suggestions` / `cached_spec`, capped at 512 entries each). A request that turns on the `···` marker owns that latch and must clear it even if its result is stale.

Every hook runs under a hard wall-clock deadline (its script budget plus a 2s margin) enforced by a QuickJS interrupt handler, and `executeCommand` calls are clamped to the hook's remaining budget — a spinning or slow hook is aborted instead of wedging the attempt thread until the 30s supervisor watchdog. Watchdog timeouts/panics log at ERROR (the default log filter) with the root command and cwd.

Fig semantics the Rust side has to reproduce exactly: a generator's `splitOn` wins over its `postProcess`, and `custom` hooks get the shell's process name and environment variables on their context argument (`JsHost::enter_with_context`, fed from `CompleteRequest::environment_variables`).

`packages/autocomplete-engine` is a TypeScript experiment and is not on the desktop path.

### Website Tailwind CSS

The product website under `website/src` uses Tailwind CSS v4. When editing it:

- Prefer canonical utilities and do not leave `suggestCanonicalClasses` warnings in edited files.
- Use the spacing scale for exact equivalents: `py-3.5` instead of `py-[14px]`, `gap-6.5` instead of `gap-[26px]`, and `max-w-310` instead of `max-w-[1240px]`.
- Use Tailwind v4 CSS-variable shorthand: `bg-(--accent)`, `text-(--accent)`, `border-(--accent-line)`, and `font-(--font-display)` instead of `[var(...)]` forms.
- Prefer exact named utilities such as `rounded-md`, `rounded-xl`, and `tracking-wider`.
- Use arbitrary values only when no canonical utility expresses the design, such as a custom `11px` radius or a complex shadow.
- Before finishing website changes, clear Tailwind CSS IntelliSense canonical-class warnings in touched files and run `cd website && pnpm build`.

### Bundled Specs

Completion specs are **bundled into the `.app` at build time**, not fetched at runtime. `scripts/sync-bundled-specs.mjs` assembles the Fig JS sources into `bundle/specs/`. `scripts/compile-spec-ir.mjs` then writes `bundle/specs-ir/` (JSON IR + `hooks/*.js`). `build-app.sh` always recompiles IR and ships **only `specs-ir`** in `Contents/Resources/`. `bundle/specs` stays out of the `.app`: it exists to feed the IR compiler and to supply the icons `ec_gpui` embeds with `include_bytes!`, both build-time concerns. Bundling it too cost 28 MB of dead weight until it was dropped. The engine reads **`specs-ir` only** — a spec missing from that tree has no completion.

**Source.** The default source is the installed npm dependency [`@chen86860/autocomplete-specs`](https://www.npmjs.com/package/@chen86860/autocomplete-specs), published from our forked spec repo [`chen86860/autocomplete-specs`](https://github.com/chen86860/autocomplete-specs). The version is pinned by root `package.json` plus `pnpm-lock.yaml`. The sync script reads the package from `node_modules`, copies `build/*.js` and `icons/*.png` into `bundle/specs`, then derives `index.json` from the bundled file tree.

**Config + pinning.** `specs.config.json` only stores bundle filtering such as `exclude`. Package pinning lives in the normal JS dependency files, **not `latest`**, so the bundle changes only when the dependency changes, never silently. To adopt a newer fork build: run `corepack pnpm add -D @chen86860/autocomplete-specs@<version> -w`, re-run the sync, and commit `package.json`, `pnpm-lock.yaml`, and the regenerated `bundle/specs` together. Env overrides still win for one-off runs: `BUNDLED_SPECS_EXCLUDE=<csv>`, `BUNDLED_SPECS_PACKAGE=<pkg>`, `BUNDLED_SPECS_SOURCE=npm` with `BUNDLED_SPECS_VERSION=<version|latest>` / `BUNDLED_SPECS_PACKAGE_TARBALL=<full-url>` / `BUNDLED_SPECS_NPM_REGISTRY=<registry>`, or `BUNDLED_SPECS_SOURCE=cdn` to fall back to the legacy per-file CDN sync (`https://specs.q.us-east-1.amazonaws.com/`, frozen 2025-05-05).

To keep the bundle small, the sync script supports excluding whole namespaces via `BUNDLED_SPECS_EXCLUDE` (comma-separated; a namespace `ns` drops the top-level `ns` spec and everything under `ns/`). The filter is applied to **both** the downloaded files and the written `index.json`, so the runtime loader never references excluded specs.

- **Default**: `aws` and `az` are excluded (the AWS and Azure CLI specs are large and most users never trigger them). This is intentional — see `specs.config.json`.
- **Bundle everything**: `BUNDLED_SPECS_EXCLUDE="" node scripts/sync-bundled-specs.mjs`
- **Trim more**: `BUNDLED_SPECS_EXCLUDE="aws,gcloud,az" node scripts/sync-bundled-specs.mjs` (saves another ~26 MB)

`build-app.sh` only auto-syncs when `bundle/specs/index.json` is missing, so it reuses whatever filtered set is already on disk. Re-run the sync script after changing the exclusion list.

## Key Crates

| Crate              | Role                                                             |
| ------------------ | ---------------------------------------------------------------- |
| `fig_desktop`      | Native app host: GPUI overlay + settings, tray, engine client    |
| `ec_gpui`          | Overlay list, theme, macOS window placement                      |
| `ec_engine`        | Headless completion: IR lookup, generators, QuickJS hooks        |
| `figterm`          | PTY interceptor, shell edit buffer tracking                      |
| `ec_cli`           | CLI binary, all `ec` subcommands                                 |
| `fig_input_method` | macOS IMKit input method helper                                  |
| `ec_hitoolbox`     | The HIToolbox palette lists, shared by the IME and the CLI        |
| `fig_integrations` | Shell/terminal/editor integration install logic                  |
| `fig_desktop_api`  | All that is left of the WebView bridge: the `install` request      |
| `fig_ipc`          | Unix socket IPC primitives                                       |
| `fig_proto`        | Generated Protobuf message types                                 |
| `fig_settings`     | Settings persistence (JSON)                                      |
| `fig_util`         | Shared constants, directory paths, system info                   |
| `macos-utils`      | macOS Accessibility API, NSWorkspace, AppKit ObjC2 bindings      |

## Key TypeScript Packages

| Package                 | Role                                                                 |
| ----------------------- | -------------------------------------------------------------------- |
| `autocomplete-parser`   | Used by `compile-spec-ir.mjs` to evaluate Fig specs at build time    |
| `shell-parser`          | Shell command-line tokenizer used by the spec compiler               |
| `api-bindings`          | Generated TS Protobuf IPC bindings                                   |
| `api-bindings-wrappers` | Ergonomic wrappers over `api-bindings`                               |

Everything under `packages/` now exists to serve `scripts/compile-spec-ir.mjs`; nothing there ships in the `.app`.

## Toolchain Versions

- Rust: `1.88.0` (pinned in `rust-toolchain.toml`), edition 2024
- Node: `>=22.13 <23`
- pnpm: `11.14.0` (see root `package.json` `packageManager`)
- Turborepo handles the TypeScript build graph

## macOS-Specific Notes

- The `.app` bundle lives at `/Applications/easy-complete.app`
- Launch at login: `SMAppService.mainAppService` on macOS 13+; `~/Library/LaunchAgents/dev.emmmm.easy-complete.plist` fallback on macOS 12
- IME symlink target: `~/Library/Input Methods/EasyCompleteInputMethod.app`
- HIToolbox prefs (`com.apple.HIToolbox`) must include the IME bundle ID for Ghostty/Kitty cursor following to work
- `TISCreateInputSourceList` returns NULL when called without NSApplication; always call TIS APIs via `run_on_main` or from within the IME process itself
- Overlay Y conversion must use `NSScreen.screens[0]` (menu-bar / global-origin display). `NSScreen.mainScreen` is whichever display holds keyboard focus.
- Accessibility: `prompt_for_accessibility()` plus `open_accessibility()` (System Settings deep link). There is no drag-to-authorize coach.
- Process memory: `./scripts/memory-usage.sh` (`--watch`, `--peak`, `--csv`). Reports `phys_footprint`, the same number as Activity Monitor. `ecterm` appears as `<shell> (ecterm)`.
