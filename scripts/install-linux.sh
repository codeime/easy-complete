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
# Absolute prefix so desktop Exec= is not cwd-relative, and uninstall can
# match autostart entries that belong to this tree only.
PREFIX="$(readlink -f "$PREFIX")"
if [ -z "$SRC" ]; then
  SRC="${REPO_DIR}/dist/linux/easy-complete"
fi
if [ "$UNINSTALL" != 1 ]; then
  SRC="$(readlink -f "$SRC")"
fi

remove_prefix_autostart() {
  # install-linux.sh does not write autostart; `ec integrations install
  # autostart-entry` does. Only delete a file that points at this PREFIX so
  # `--prefix /tmp/foo --uninstall` cannot wipe a different install's login
  # entry.
  local config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  local autostart="${config_home}/autostart/easy-complete.desktop"
  if [ ! -e "$autostart" ] && [ ! -L "$autostart" ]; then
    return 0
  fi
  local target=""
  if [ -L "$autostart" ]; then
    target="$(readlink -f "$autostart" || true)"
  fi
  if grep -F -q "${PREFIX}/bin/easy-complete" "$autostart" 2>/dev/null \
    || [ "$target" = "${PREFIX}/share/applications/easy-complete.desktop" ]; then
    rm -f "$autostart"
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  # Shell rc snippets can only be removed by the binary that wrote them, so run
  # this before deleting it. Same ordering rule as scripts/uninstall.sh. Only
  # this prefix's `ec` is used, so a prefix uninstall cannot ask another
  # install to edit the user's dotfiles.
  #
  # The default prefix needs sudo, and sudo's env_reset points HOME at /root, so
  # running `ec` as-is would edit root's rc files and report nothing. Drop back
  # to the invoking user; if we cannot tell who that is, say so instead of
  # silently doing nothing.
  if [ -x "${PREFIX}/bin/ec" ]; then
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
      if ! sudo -u "${SUDO_USER}" -H "${PREFIX}/bin/ec" integrations uninstall dotfiles; then
        echo "warning: failed to uninstall shell hooks for ${SUDO_USER}" >&2
      fi
    elif [ "$(id -u)" -eq 0 ] && [ -z "${SUDO_USER:-}" ]; then
      echo "note: running as root with no SUDO_USER; run 'ec integrations uninstall dotfiles' as your own user to remove the shell hooks" >&2
    elif ! "${PREFIX}/bin/ec" integrations uninstall dotfiles; then
      echo "warning: failed to uninstall shell hooks" >&2
    fi
  fi
  # Autostart may be a symlink into PREFIX; resolve it before the desktop file
  # is deleted so a prefix uninstall cannot leave a dangling login entry.
  remove_prefix_autostart
  rm -f "${PREFIX}/bin/easy-complete" "${PREFIX}/bin/ec" "${PREFIX}/bin/ecterm"
  rm -rf "${PREFIX}/share/easy-complete"
  rm -f "${PREFIX}/share/applications/easy-complete.desktop"
  rm -f "${PREFIX}/share/icons/hicolor/512x512/apps/easy-complete.png"
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
