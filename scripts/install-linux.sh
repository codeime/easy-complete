#!/usr/bin/env bash
set -euo pipefail

# Copy a tree produced by scripts/build-linux.sh into PREFIX.
# Default PREFIX=/usr/local. No WebKit, no IME symlink, no tccutil.

PREFIX="/usr/local"
SRC=""
UNINSTALL=0

usage() {
  cat <<EOF
Usage: $0 [--prefix DIR] [--from DIR] [--uninstall]
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --from) SRC="$2"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "$SRC" ]; then
  SRC="${REPO_DIR}/dist/linux/easy-complete"
fi

if [ "$UNINSTALL" = 1 ]; then
  rm -f "${PREFIX}/bin/easy-complete" "${PREFIX}/bin/ec" "${PREFIX}/bin/ecterm"
  rm -rf "${PREFIX}/share/easy-complete"
  rm -f "${PREFIX}/share/applications/easy-complete.desktop"
  rm -f "${PREFIX}/share/icons/hicolor/512x512/apps/easy-complete.png"
  rm -f "${HOME}/.config/autostart/easy-complete.desktop"
  echo "Removed Easy Complete from ${PREFIX}"
  exit 0
fi

if [ ! -x "${SRC}/bin/easy-complete" ]; then
  echo "error: missing ${SRC}/bin/easy-complete (run scripts/build-linux.sh first)" >&2
  exit 1
fi

mkdir -p "${PREFIX}/bin" \
  "${PREFIX}/share/easy-complete" \
  "${PREFIX}/share/applications" \
  "${PREFIX}/share/icons/hicolor/512x512/apps"

install -m 755 "${SRC}/bin/easy-complete" "${PREFIX}/bin/easy-complete"
install -m 755 "${SRC}/bin/ec" "${PREFIX}/bin/ec"
install -m 755 "${SRC}/bin/ecterm" "${PREFIX}/bin/ecterm"
rm -rf "${PREFIX}/share/easy-complete/specs-ir"
cp -R "${SRC}/share/easy-complete/specs-ir" "${PREFIX}/share/easy-complete/specs-ir"
desktop_dest="${PREFIX}/share/applications/easy-complete.desktop"
exec_path="${PREFIX}/bin/easy-complete"
sed -e "s|^Exec=.*|Exec=${exec_path}|" -e "s|^TryExec=.*|TryExec=${exec_path}|" \
  "${SRC}/share/applications/easy-complete.desktop" > "${desktop_dest}"
chmod 644 "${desktop_dest}"
install -m 644 "${SRC}/share/icons/hicolor/512x512/apps/easy-complete.png" \
  "${PREFIX}/share/icons/hicolor/512x512/apps/easy-complete.png"

echo "Installed Easy Complete under ${PREFIX}"
echo "Launch: ${PREFIX}/bin/easy-complete"
echo "Shell hooks: ${PREFIX}/bin/ec integrations install dotfiles"
echo "Launch at login: Settings, or ${PREFIX}/bin/ec integrations install autostart-entry"
