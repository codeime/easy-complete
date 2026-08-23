#!/usr/bin/env bash

set -e

OS="$(uname -s)"

install_linux_deps() {
  # Source of truth for compile deps is `.github/workflows/ci.yml` `rust-linux`.
  # Do not install WebKit: the overlay and settings UI are GPUI.
  # GTK here is tray-icon/muda at compile time, not the completion list.
  # Runtime overlay: a Vulkan ICD (lavapipe or a GPU) plus X11 (DISPLAY).
  # Runtime caret: IBus and/or AT-SPI. Missing caret ⇒ the overlay parks;
  # there is no window-rect fallback. Do not apt-install those here.
  if [ -f /etc/debian_version ]; then
    echo "Detected Debian/Ubuntu"
    sudo apt update
    sudo apt install -y --no-install-recommends \
      build-essential pkg-config jq dpkg curl wget cmake \
      clang libclang-dev \
      libssl-dev protobuf-compiler sqlite3 zsh fish \
      libgtk-3-dev libayatana-appindicator3-dev \
      libxkbcommon-dev libxkbcommon-x11-dev \
      libwayland-dev libx11-dev libxrandr-dev libxi-dev libxcursor-dev \
      libx11-xcb-dev libxcb-xfixes0-dev libxcb-xkb-dev \
      libvulkan-dev libfreetype6-dev libfontconfig1-dev
  elif [ -f /etc/arch-release ]; then
    echo "Detected Arch"
    sudo pacman -Syu --noconfirm
    sudo pacman -S --noconfirm --needed \
      base-devel curl wget openssl gtk3 libappindicator-gtk3 \
      libxkbcommon libx11 cmake jq pkgconf protobuf clang \
      zsh fish
  elif [ -f /etc/fedora-release ]; then
    echo "Detected Fedora"
    sudo dnf check-update || true
    sudo dnf install -y \
      gcc gcc-c++ make cmake pkgconf-pkg-config openssl-devel \
      curl wget jq protobuf-compiler clang gtk3-devel \
      libappindicator-gtk3 libxkbcommon-devel libX11-devel \
      vulkan-devel freetype-devel fontconfig-devel \
      zsh fish
    sudo dnf group install -y "C Development Tools and Libraries"
  else
    echo "Unsupported Linux distribution. Check the docs for manual installation instructions."
    exit 1
  fi
}

install_macos_deps() {
  echo "Detected macOS"
  xcode-select --install || true
  brew install mise pnpm protobuf zsh bash fish shellcheck jq
}

install_rust() {
  echo "Installing Rust toolchain..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    # Detect shell and source the correct env file

  SHELL_NAME=$(basename "$SHELL")
  case "$SHELL_NAME" in
    fish)
      source "$HOME/.cargo/env.fish"
      ;;
    nu)
      source "$HOME/.cargo/env.nu"
      ;;
    *)
      . "$HOME/.cargo/env"
      ;;
  esac

  # rust-toolchain.toml pins 1.88.0 for this repo. Do not `rustup default
  # stable` — that is how a box ends up on 1.85 while rquickjs needs >=1.87.
  cargo install typos-cli
}
add_mise_to_shell() {
  echo "Adding mise integration to shell..."

  SHELL_NAME=$(basename "$SHELL")

  case "$SHELL_NAME" in
    zsh)
      ZSHRC="${ZDOTDIR:-$HOME}/.zshrc"
      grep -qxF 'eval "$(mise activate zsh)"' "$ZSHRC" || echo 'eval "$(mise activate zsh)"' >> "$ZSHRC"
      ;;
    bash)
      BASHRC="$HOME/.bashrc"
      grep -qxF 'eval "$(mise activate bash)"' "$BASHRC" || echo 'eval "$(mise activate bash)"' >> "$BASHRC"
      ;;
    fish)
      FISH_CONFIG="$HOME/.config/fish/config.fish"
      mkdir -p "$(dirname "$FISH_CONFIG")"
      grep -qxF 'mise activate fish | source' "$FISH_CONFIG" || echo 'mise activate fish | source' >> "$FISH_CONFIG"
      ;;
    *)
      echo "⚠️  Unknown shell '$SHELL_NAME'. Please add mise manually to your shell config."
      ;;
  esac
}

setup_mise() {
  echo "Installing Python and Node with mise..."
  add_mise_to_shell
  mise trust
  mise install
}

setup_precommit() {
  echo "Installing pre-commit hooks..."
  pnpm install --ignore-scripts
}

echo "Setting up project dependencies..."

if [[ "$OS" == "Linux" ]]; then
  install_linux_deps
elif [[ "$OS" == "Darwin" ]]; then
  install_macos_deps
else
  echo "Unsupported OS: $OS"
  exit 1
fi

install_rust
setup_mise
setup_precommit

echo "✅ Setup complete! Follow the instructions in the README to get started."
