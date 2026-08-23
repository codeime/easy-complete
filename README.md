<p align="center">
  <img src="./assets/logo.png" alt="Easy Complete" width="140px">
</p>

<h1 align="center">Easy Complete</h1>

<p align="center">
  <b>IDE-style inline autocomplete for your macOS terminal.</b><br/>
  An open-source, Fig-style completion engine for <code>zsh</code>, <code>bash</code> & <code>fish</code>.
</p>

<p align="center">
  <a href="https://github.com/chen86860/easy-complete/releases"><img alt="Release" src="https://img.shields.io/github/v/release/chen86860/easy-complete?color=brightgreen"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS-lightgrey">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange">
  <a href="#-license"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>
  <a href="https://github.com/chen86860/easy-complete/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/chen86860/easy-complete?style=social"></a>
</p>

<p align="center">
  <b>English</b> · <a href="./README.zh-CN.md">简体中文</a>
</p>

**Easy Complete** is a macOS terminal autocomplete app — IDE-style inline completions
for your shell, rendered in a native overlay window that follows your cursor. It is a
local-first terminal completion engine focused purely on autocomplete —
a lightweight, fully local alternative to Fig.

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

> **Platform:** The published product is macOS only (Apple Silicon / ARM64 DMG).
> Linux and Windows are work-in-progress on `feat/cross-platform` (see
> [`CROSS_PLATFORM_PLAN.md`](./CROSS_PLATFORM_PLAN.md)). They are not released:
> no Linux package, no Windows installer. CI jobs on that branch are compile/test
> gates, not a ship.

## Contents

- [Install](#-install)
- [Usage](#-usage)
- [Uninstall](#-uninstall)
- [How it works](#-how-it-works)
- [Development](#-development)
- [License](#-license)

---

## ⚡️ Install

### Homebrew (recommended)

Install Easy Complete with one command:

```bash
brew install --cask chen86860/tap/easy-complete
```

Then launch **Easy Complete** from `/Applications`, grant **Accessibility**
permission when prompted, and reload your shell:

```bash
exec $SHELL
```

On first launch, Easy Complete sets up the bundled CLI binaries, shell integration,
input method, and login startup entries. To verify the installation, run:

```bash
ec doctor
```

### Download the DMG manually

Download the latest Apple Silicon DMG:

[Download latest DMG](https://github.com/chen86860/easy-complete/releases/latest/download/Easy-Complete-arm64.dmg) ·
[All releases](https://github.com/chen86860/easy-complete/releases)

Then:

1. Open `Easy-Complete-arm64.dmg`.
2. Drag **Easy Complete.app** into `/Applications`.
3. Launch **Easy Complete** from `/Applications`.
4. Grant **Accessibility** permission when prompted.
5. Reload your shell:

   ```bash
   exec $SHELL
   ```

To verify the installation, run:

```bash
ec doctor
```

### Build from source

For development, or if you need to build locally, clone the repository and run the
installer:

```bash
git clone https://github.com/chen86860/easy-complete.git
cd easy-complete
./install.sh
```

The source installer will:

1. Build the Rust binaries and the TypeScript frontend.
2. Assemble `Easy Complete.app` and copy it to `/Applications`.
3. Symlink the `ec` and `ecterm` CLIs into `~/.local/bin`.
4. Let you enable **Launch at Login** from Settings (a system Login Item on macOS 13+, with a LaunchAgent fallback on macOS 12).
5. Set up shell integration and register the input method.
6. **Prompt you to grant Accessibility permission** (required — see below).

When it finishes, reload your shell:

```bash
exec $SHELL
```

### Grant Accessibility permission

Easy Complete positions the completion popup relative to your focused terminal
window, which requires the macOS **Accessibility** permission. The installer triggers
the system prompt automatically; approve **Easy Complete** in:

> System Settings → Privacy & Security → Accessibility

If completions never appear, this is almost always the cause. Re-trigger the prompt
with:

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

The settings & onboarding dashboard is available from the **Easy Complete menu bar
icon** (system tray).

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
additionally rely on the bundled input method for cursor tracking — this is registered
automatically during install.

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
| `easy-complete` | `fig_desktop` | Native app host — GPUI overlay and settings, completion engine worker, system tray, and window management |
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

`cargo test` / `cargo build` without `-p` or `--workspace` only build `crates/ec_cli`
(`default-members`). `fig_desktop_api` is still linked by `fig_desktop` (macOS dist).
`ec_overlay_spike` is a Linux overlay lab binary and is not shipped.

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

Licensed under the MIT License. Easy Complete is based on the upstream Amazon Q
Developer CLI; its original copyright notice is retained in [LICENSE](./LICENSE).
Third-party copyright and license terms are collected in
[THIRD_PARTY_NOTICES.txt](./THIRD_PARTY_NOTICES.txt).
