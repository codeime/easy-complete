#!/usr/bin/env bash
set -euo pipefail

# Prefix-free zip of Windows binaries. Not a DMG, not an MSI.
# Run on windows-latest (or a Windows host with the MSVC toolchain).

# `[ "$x" != "MINGW"* ]` does not glob: the pattern is quoted. Use case.
is_windows=0
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) is_windows=1 ;;
esac
if [ "${OS:-}" = "Windows_NT" ]; then
  is_windows=1
fi
if [ "$is_windows" != 1 ]; then
  echo "error: scripts/build-windows.sh is meant to run on Windows" >&2
  exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
if command -v python >/dev/null 2>&1; then
  PYTHON=python
elif command -v python3 >/dev/null 2>&1; then
  PYTHON=python3
else
  PYTHON=
fi
if [ -n "$PYTHON" ]; then
  VERSION=$(cargo metadata --no-deps --format-version 1 | "$PYTHON" -c "import sys,json; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='fig_desktop'))" 2>/dev/null || echo "dev")
else
  VERSION=dev
fi
ARCH=$(uname -m)
CARGO_PROFILE="${CARGO_PROFILE:-release}"

cd "$REPO_DIR"
# --locked matches rust-windows CI and scripts/build-linux.sh.
cargo build --locked --profile "$CARGO_PROFILE" -p fig_desktop -p figterm -p ec_cli

if [ "$CARGO_PROFILE" = "dev" ]; then
  TARGET_DIR="${CARGO_TARGET_DIR:-target}/debug"
else
  TARGET_DIR="${CARGO_TARGET_DIR:-target}/${CARGO_PROFILE}"
fi

DEST="${REPO_DIR}/dist/windows/easy-complete"
rm -rf "$DEST"
mkdir -p "$DEST" "$DEST/specs-ir"
cp "${TARGET_DIR}/easy-complete.exe" "$DEST/"
cp "${TARGET_DIR}/ec.exe" "$DEST/"
cp "${TARGET_DIR}/ecterm.exe" "$DEST/"
cp -R "${REPO_DIR}/bundle/specs-ir/." "$DEST/specs-ir/"
cat > "$DEST/README.txt" <<EOF
Easy Complete ${VERSION} (${ARCH})

  easy-complete.exe  desktop host
  ec.exe             CLI
  ecterm.exe         PTY interceptor
  specs-ir/          completion specs (or set EC_SPECS_DIR)

Completions read specs from EC_SPECS_DIR, then next to this folder
(specs-ir), then %LOCALAPPDATA%\\easy-complete\\specs-ir.

Launch the desktop app, then:  ec integrations install dotfiles
Launch at login: Settings, or  ec integrations install autostart-entry
EOF

mkdir -p "${REPO_DIR}/dist/windows"
(
  cd "${REPO_DIR}/dist/windows"
  if command -v zip >/dev/null 2>&1; then
    zip -r "easy-complete-${VERSION}-${ARCH}.zip" easy-complete
  else
    tar -czf "easy-complete-${VERSION}-${ARCH}.tar.gz" easy-complete
  fi
)
echo "Wrote $DEST"
