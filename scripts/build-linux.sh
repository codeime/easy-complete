#!/usr/bin/env bash
set -euo pipefail

# Assemble a prefix-layout Linux tree. Not an .app bundle.
# Output: dist/linux/easy-complete/ and dist/linux/easy-complete-<version>-<arch>.tar.gz
#
# Do not install WebKit. UI is GPUI.

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: scripts/build-linux.sh must run on Linux" >&2
  exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='fig_desktop'))" 2>/dev/null || echo "dev")
ARCH=$(uname -m)
CARGO_PROFILE="${CARGO_PROFILE:-release}"
CARGO_OUT_DIR="${CARGO_TARGET_DIR:-target}"
if [ "$CARGO_PROFILE" = "dev" ]; then
  TARGET_DIR="${CARGO_OUT_DIR}/debug"
else
  TARGET_DIR="${CARGO_OUT_DIR}/${CARGO_PROFILE}"
fi

DEST="${REPO_DIR}/dist/linux/easy-complete"
BIN="${DEST}/bin"
SHARE="${DEST}/share/easy-complete"
APPLICATIONS="${DEST}/share/applications"
ICONS="${DEST}/share/icons/hicolor/512x512/apps"

info() { echo "==> $*"; }

cd "$REPO_DIR"

info "Building Linux binaries (profile: ${CARGO_PROFILE})..."
cargo build --profile "$CARGO_PROFILE" -p fig_desktop -p figterm -p ec_cli

if [ ! -f "${REPO_DIR}/bundle/specs/index.json" ]; then
  info "Bundled specs are missing; syncing them now..."
  node "${REPO_DIR}/scripts/sync-bundled-specs.mjs"
fi
info "Compiling spec IR..."
node "${REPO_DIR}/scripts/compile-spec-ir.mjs"

rm -rf "$DEST"
mkdir -p "$BIN" "$SHARE" "$APPLICATIONS" "$ICONS"

install -m 755 "${TARGET_DIR}/easy-complete" "${BIN}/easy-complete"
install -m 755 "${TARGET_DIR}/ec" "${BIN}/ec"
install -m 755 "${TARGET_DIR}/ecterm" "${BIN}/ecterm"

cp -R "${REPO_DIR}/bundle/specs-ir" "${SHARE}/specs-ir"
install -m 644 "${REPO_DIR}/scripts/linux/easy-complete.desktop" "${APPLICATIONS}/easy-complete.desktop"
install -m 644 "${REPO_DIR}/crates/fig_desktop/icons/512x512.png" "${ICONS}/easy-complete.png"

cat > "${DEST}/README" <<EOF
Easy Complete ${VERSION} (${ARCH})

Prefix layout:
  bin/easy-complete   desktop host
  bin/ec              CLI
  bin/ecterm          PTY interceptor
  share/easy-complete/specs-ir
  share/applications/easy-complete.desktop
  share/icons/hicolor/512x512/apps/easy-complete.png

Install (example):
  sudo ./scripts/install-linux.sh --prefix /usr/local

Completions read specs from EC_SPECS_DIR, then beside the prefix bin
(../share/easy-complete/specs-ir), then XDG_DATA_DIRS, then /usr/share.
EOF

TARBALL="${REPO_DIR}/dist/linux/easy-complete-${VERSION}-${ARCH}.tar.gz"
mkdir -p "$(dirname "$TARBALL")"
tar -C "${REPO_DIR}/dist/linux" -czf "$TARBALL" easy-complete
info "Wrote $DEST"
info "Wrote $TARBALL"
