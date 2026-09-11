<p align="center">
  <img src="./assets/logo.png" alt="Easy Complete" width="140px">
</p>

<h1 align="center">Easy Complete (Native)</h1>

<p align="center">
  <b>IDE-style inline autocomplete for your macOS terminal — native GPUI, not a WebView.</b><br/>
  An open-source, Fig-style completion engine for <code>zsh</code>, <code>bash</code> & <code>fish</code>.
</p>

<p align="center">
  <a href="https://github.com/codeime/easy-complete/releases"><img alt="Release" src="https://img.shields.io/github/v/release/codeime/easy-complete?color=brightgreen"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS-lightgrey">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange">
  <img alt="Native GPUI" src="https://img.shields.io/badge/UI-native%20GPUI-8A2BE2">
  <a href="#-license"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>
  <a href="https://github.com/codeime/easy-complete/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/codeime/easy-complete?style=social"></a>
</p>

<p align="center">
  <b>English</b> · <a href="./README.zh-CN.md">简体中文</a>
</p>

**Easy Complete (Native)** is a macOS terminal autocomplete app — IDE-style inline
completions for your shell, rendered in a native GPUI overlay that follows your
cursor. The popup and the settings window are real native views, not WKWebView.
Completions run in a local Rust engine. It is a local-first completion engine
focused purely on autocomplete — a lightweight, fully local alternative to Fig.

You get fish-shell-style suggestions for hundreds of CLIs (`git`, `npm`, `docker`,
`cargo`, …): flags, subcommands, file paths, and arguments, completed as you type.
Autocomplete runs fully on-device — no account, no cloud calls, no AI requests, and
your commands never leave your Mac. The app collects anonymous usage statistics
(app opens, daily completion counts — never command content), which you can disable
any time with `ec telemetry disable`. See the [Privacy page](https://easy-complete.emmmm.dev/privacy-policy)
for the full list of what is and isn't collected.

<p align="center">
  <img src="./.github/media/screenshot.png" alt="Easy Complete autocomplete in action">
</p>

> **Platform:** macOS only. The published DMG is Apple Silicon / ARM64 only.

## Native

This repository is **Easy Complete (Native)** — an independent 3.x line at
[`codeime/easy-complete`](https://github.com/codeime/easy-complete). It is not a
pull request back to the WebView project.

The completion popup and the settings window are GPUI views (Zed's UI toolkit).
Completions never enter a web view: `ec_engine` looks up JSON IR compiled at
build time, and QuickJS runs only when a spec hook needs it (`postProcess`,
`script`, `custom`, `generateSpec`).

The WebView line is the fork source:
[`chen86860/easy-complete`](https://github.com/chen86860/easy-complete). Fig and
Amazon Q also used a WebView overlay.

## Performance

Native numbers are `phys_footprint` (the same figure Activity Monitor shows),
via `./scripts/memory-usage.sh`. The WebView column is the upstream project
([`chen86860/easy-complete`](https://github.com/chen86860/easy-complete)), not
this repo. Fig / Amazon Q sizes were not measured here.

| | Easy Complete (Native) | [WebView](https://github.com/chen86860/easy-complete) | Fig / Amazon Q |
| --- | --- | --- | --- |
| Overlay | Native GPUI window | WKWebView | WebView |
| Settings | Native GPUI window | React dashboard in WKWebView | Cloud dashboard |
| Completion engine | Local Rust + JSON IR | JavaScript inside the WebView | Cloud / account |
| Completions leave your Mac | No | No | Fig / Q required an account |
| Desktop memory | ~50 MB, stable | WebKit process + page | — |
| Installed app | ~81 MB | ~109 MB (includes unused specs) | — |
| DMG | ~22 MB | ~25 MB | — |

Native stays near 50 MB in a typical session. Most of the package drop is from
not shipping the unused `bundle/specs` tree (the engine reads `specs-ir` only).
A session also keeps one `ecterm` per terminal tab (~10–17 MB) and an idle
input-method helper (~7 MB).

## Contents

- [Native](#native)
- [Performance](#performance)
- [Install](#-install)
- [Usage](#-usage)
- [Uninstall](#-uninstall)
- [How it works](#-how-it-works)
- [Development](#-development)
- [License](#-license)

---

## ⚡️ Install

### Download the DMG (recommended)

Native builds are the Apple Silicon DMGs from this repository:

[Download latest DMG](https://github.com/codeime/easy-complete/releases/latest/download/Easy-Complete-arm64.dmg) ·
[All releases](https://github.com/codeime/easy-complete/releases)

Then:

1. Open `Easy-Complete-arm64.dmg`.
2. Drag **Easy Complete.app** into `/Applications`.
3. Launch **Easy Complete** from `/Applications`.
4. Open Easy Complete Settings and click **Grant Accessibility**.
5. Reload your shell:

   ```bash
   exec $SHELL
   ```

On first launch, Easy Complete sets up the bundled CLI binaries, shell integration,
and login startup entries. The input method is optional — install it from Settings →
Behavior, or with `ec integrations install input-method`, for Ghostty, Kitty,
WezTerm, Zed, Alacritty, and Otty. To verify the installation, run:

```bash
ec doctor
```

### Homebrew (original WebView project)

The published cask is the fork source,
[chen86860/easy-complete](https://github.com/chen86860/easy-complete), not this
Native line:

```bash
brew install --cask chen86860/tap/easy-complete
```

### Build from source

For development, or if you need to build locally, clone the repository and run the
installer:

```bash
git clone https://github.com/codeime/easy-complete.git
cd easy-complete
./install.sh
```

The source installer will:

1. Build the Rust binaries and compile bundled completion specs.
2. Assemble `Easy Complete.app` and copy it to `/Applications`.
3. Symlink the `ec` and `ecterm` CLIs into `~/.local/bin`.
4. Let you enable **Launch at Login** from Settings (a system Login Item on macOS 13+, with a LaunchAgent fallback on macOS 12).
5. Set up shell integration. `./install.sh` also registers the optional input method (DMG first launch does not).
6. Leave Accessibility for you to grant from Easy Complete Settings (required — see below).

When it finishes, reload your shell:

```bash
exec $SHELL
```

### Grant Accessibility permission

Easy Complete positions the completion popup relative to your focused terminal
window, which requires the macOS **Accessibility** permission. Open Easy Complete
Settings and click **Grant Accessibility**. That opens:

> System Settings → Privacy & Security → Device Control and Data Access
>
> On macOS 26 and earlier the same list is named **Accessibility**. Grant still
> opens it.

and floats a card you can drag **Easy Complete** from into the list. The app never
opens that pane on its own.

If completions never appear, this is almost always the cause. Run the same flow
again from Settings, or with:

```bash
ec debug prompt-accessibility
```

---

## 🚀 Usage

Once installed and granted permission, just start typing in any supported terminal —
suggestions appear inline as you type.

| Key             | Action                            |
| --------------- | --------------------------------- |
| `↑` / `↓`       | Move through suggestions          |
| `⇥` (Tab) / `→` | Accept the highlighted suggestion |
| `Esc`           | Dismiss the popup                 |

The native settings window is available from the **Easy Complete menu bar icon**
(system tray).

Useful CLI commands:

```bash
ec doctor                       # diagnose common problems
ec diagnostic                   # print environment / integration status
ec integrations install input-method   # (re)register the macOS input method
ec settings list                # view settings
ec settings <key> <value>       # change a setting
```

### Supported terminals

Most terminals work out of the box via the PTY integration — including iTerm2, Apple
Terminal, VS Code, Cursor, ChatGPT (Codex), and JetBrains IDE terminals. Terminals that
bypass the standard PTY path (**Ghostty, Kitty, WezTerm, Zed, Alacritty, Otty**)
additionally rely on the bundled input method for cursor tracking. Install it from
Settings → Behavior, or with `ec integrations install input-method`.

---

## 🗑️ Uninstall

```bash
./scripts/uninstall.sh
```

This removes the app bundle, CLI symlinks, LaunchAgent, input method, shell
integration, and all application data. It surgically removes only Easy Complete's own
input source from the system preferences (your other keyboard layouts and input
methods are left untouched).

---

## 🧩 How it works

Easy Complete runs as three cooperating native processes that talk over Unix domain
sockets (Protobuf messages):

| Binary          | Crate         | Role                                                                                                                             |
| --------------- | ------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `easy-complete` | `fig_desktop` | Native app host — GPUI overlay and settings (not WKWebView), completion engine worker, system tray, and window management |
| `ecterm`        | `figterm`     | Pseudoterminal between your shell and terminal emulator; intercepts the shell edit buffer to drive completions                   |
| `ec`            | `ec_cli`      | CLI entry point — `setup`, `integrations`, `diagnostic`, `settings`, and more                                                    |

Shell hooks (`.zshrc`, `.bashrc`, fish config) report shell state — CWD, command text,
cursor position — back to `ecterm` on every prompt and keystroke. On macOS, the
`fig_input_method` helper app reports caret position for terminals that bypass the PTY.

**Identifiers**

- App bundle ID: `dev.emmmm.easy-complete`
- Input method bundle ID: `dev.emmmm.easy-complete.inputmethod`
- App bundle: `/Applications/Easy Complete.app`

---

## 🛠️ Development

### Toolchain

- Rust `1.88.0` (pinned in `rust-toolchain.toml`), edition 2024
- Node `>=22.13 <23`, pnpm `11.14`
- Turborepo for the TypeScript build graph

### Rust

```bash
# Build all release binaries
cargo build --release -p fig_desktop -p figterm -p ec_cli -p fig_input_method

# Run a single crate in dev mode
cargo run --bin ec -- <subcommand>
cargo run --bin easy-complete

cargo clippy --locked --workspace --color always -- -D warnings   # lint (CI: -D warnings)
cargo fmt                                                         # format
cargo test -p <crate_name>                                        # test a crate
```

### TypeScript

```bash
pnpm turbo build --filter="./packages/*"   # build all packages
node scripts/compile-spec-ir.mjs            # Fig specs → JSON IR + JS hooks
pnpm lint                                   # lint
pnpm test                                   # run Vitest
```

Headless completion (no overlay): `cargo run --bin ec -- engine complete --buffer "git ch"`.
Process memory: `./scripts/memory-usage.sh` (`--watch 5`, `--peak`, `--csv mem.csv`).

### Key crates

| Crate                   | Role                                                             |
| ----------------------- | ---------------------------------------------------------------- |
| `fig_desktop`           | Native app host: GPUI overlay + settings, tray, engine client    |
| `ec_gpui`               | Overlay list, theme, macOS window placement                      |
| `ec_engine`             | Headless completion: IR lookup, generators, QuickJS hooks        |
| `figterm`               | PTY interceptor, shell edit-buffer tracking                      |
| `ec_cli`                | CLI crate, providing the `ec` binary and all its subcommands     |
| `fig_input_method`      | macOS input method helper (cursor tracking)                      |
| `fig_integrations`      | Shell/terminal/editor integration install logic                  |
| `fig_ipc` / `fig_proto` | Unix-socket IPC primitives & generated Protobuf types            |

### Key TypeScript packages

| Package               | Role                                                              |
| --------------------- | ----------------------------------------------------------------- |
| `autocomplete-parser` | Evaluates Fig specs at build time for `compile-spec-ir.mjs`       |
| `shell-parser`        | Shell command-line tokenizer                                      |
| `api-bindings`        | Generated TS Protobuf IPC bindings                                |

---

## 📜 License

Licensed under the MIT License. Easy Complete is based on the Amazon Q Developer
CLI; that copyright notice stays in [LICENSE](./LICENSE). Third-party terms are
collected in [THIRD_PARTY_NOTICES.txt](./THIRD_PARTY_NOTICES.txt).
