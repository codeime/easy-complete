#!/bin/bash
set -euo pipefail

# ── Fastab macOS uninstaller ───────────────────────────────────────────────
# Also removes leftover Easy Complete names from the previous product identity.

APP_NAME="fastab"
APP_DISPLAY="Fastab"
BUNDLE_ID="app.fastab"
IME_BUNDLE_ID="app.fastab.inputmethod"
PREV_APP_NAME="easy-complete"
PREV_APP_DISPLAY="Easy Complete"
PREV_BUNDLE_ID="dev.emmmm.easy-complete"
PREV_IME_BUNDLE_ID="dev.emmmm.easy-complete.inputmethod"

APP_BUNDLE="/Applications/${APP_DISPLAY}.app"
PREV_APP_BUNDLE="/Applications/${PREV_APP_DISPLAY}.app"
LOCAL_BIN="${HOME}/.local/bin"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"
PLIST_PATH="${LAUNCH_AGENTS}/${BUNDLE_ID}.plist"
PREV_PLIST_PATH="${LAUNCH_AGENTS}/${PREV_BUNDLE_ID}.plist"
UPSTREAM_PLIST_PATH="${LAUNCH_AGENTS}/com.amazon.codewhisperer.launcher.plist"
INPUT_METHODS_DIR="${HOME}/Library/Input Methods"
IME_SYMLINK="${INPUT_METHODS_DIR}/FastabInputMethod.app"
PREV_IME_SYMLINK="${INPUT_METHODS_DIR}/EasyCompleteInputMethod.app"
APP_SUPPORT="${HOME}/Library/Application Support/${APP_NAME}"
PREV_APP_SUPPORT="${HOME}/Library/Application Support/${PREV_APP_NAME}"
CACHE_DIR="${HOME}/Library/Caches/${APP_NAME}"
PREV_CACHE_DIR="${HOME}/Library/Caches/${PREV_APP_NAME}"
TMP_ROOT="${TMPDIR:-/tmp}"
TMP_ROOT="${TMP_ROOT%/}"

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}==>${NC} $*"; }
warn()  { echo -e "${YELLOW}==>${NC} $*"; }
error() { echo -e "${RED}==>${NC} $*" >&2; }

# ── Confirm ───────────────────────────────────────────────────────────────────
if [[ "${1:-}" != "--yes" ]]; then
  echo ""
  warn "This will completely remove ${APP_DISPLAY} and all its data."
  echo "  • /Applications/${APP_DISPLAY}.app"
  echo "  • /Applications/${PREV_APP_DISPLAY}.app (if present)"
  echo "  • ${IME_SYMLINK}"
  echo "  • ${PREV_IME_SYMLINK}"
  echo "  • ${PLIST_PATH}"
  echo "  • ${PREV_PLIST_PATH}"
  echo "  • ${LOCAL_BIN}/ftab, fastabterm, ec, ecterm"
  echo "  • ${APP_SUPPORT}/"
  echo "  • ${PREV_APP_SUPPORT}/"
  echo "  • ${CACHE_DIR}/"
  echo "  • ${TMP_ROOT}/ftablog and ${TMP_ROOT}/eclog"
  echo "  • Shell integration lines in ~/.zshrc / ~/.bashrc / ~/.config/fish/config.fish"
  echo ""
  read -r -p "Continue? [y/N] " confirm
  [[ "${confirm}" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 0; }
fi

# ── 0. Telemetry (best-effort; reporting is off in this product) ─────────────
if command -v ftab &>/dev/null; then
  ftab telemetry track app_uninstalled 2>/dev/null || true
elif command -v ec &>/dev/null; then
  ec telemetry track app_uninstalled 2>/dev/null || true
fi

# ── 1. Uninstall integrations via CLI (must run before binary is removed) ─────
info "Uninstalling input method integration..."
if command -v ftab &>/dev/null; then
  ftab integrations uninstall input-method 2>/dev/null || true
elif command -v ec &>/dev/null; then
  ec integrations uninstall input-method 2>/dev/null || true
fi

info "Uninstalling shell integration..."
if command -v ftab &>/dev/null; then
  ftab integrations uninstall shell 2>/dev/null || true
elif command -v ec &>/dev/null; then
  ec integrations uninstall shell 2>/dev/null || true
fi

# ── 2. Kill running processes ─────────────────────────────────────────────────
info "Stopping processes..."
for bundle in "$APP_BUNDLE" "$PREV_APP_BUNDLE"; do
  for exe in "${APP_NAME}" "${PREV_APP_NAME}"; do
    if [[ -x "${bundle}/Contents/MacOS/${exe}" ]]; then
      "${bundle}/Contents/MacOS/${exe}" --unregister-login-item 2>/dev/null || true
    fi
  done
done
pkill -x "${APP_NAME}"       2>/dev/null || true
pkill -x "${PREV_APP_NAME}"  2>/dev/null || true
pkill -f "fig_input_method"  2>/dev/null || true
pkill -f "fastabterm"        2>/dev/null || true
pkill -f "ecterm"            2>/dev/null || true
sleep 0.5

# ── 3. Remove login item and legacy LaunchAgents ─────────────────────────────
info "Removing login startup entries..."
uid="$(id -u)"
for label in "${BUNDLE_ID}" "${PREV_BUNDLE_ID}" "com.amazon.codewhisperer.launcher"; do
  launchctl bootout "gui/${uid}/${label}" 2>/dev/null || true
done
for launch_agent in "$PLIST_PATH" "$PREV_PLIST_PATH" "$UPSTREAM_PLIST_PATH"; do
  if [[ -f "$launch_agent" ]]; then
    launchctl unload "$launch_agent" 2>/dev/null || true
    rm -f "$launch_agent"
  fi
done

# ── 4. Remove IME symlink ──────────────────────────────────────────────────────
info "Removing Input Method..."
for ime in "$IME_SYMLINK" "$PREV_IME_SYMLINK"; do
  if [[ -L "$ime" || -d "$ime" ]]; then
    rm -rf "$ime"
  fi
done

# Remove ONLY our IME entries from HIToolbox prefs so they no longer appear in
# System Settings. We surgically strip our bundle IDs from both the enabled and
# the selected input-source lists — NOT `defaults delete` on the whole array,
# which would wipe every keyboard layout and input method the user has.
info "Removing Input Method from HIToolbox..."
python3 - "$IME_BUNDLE_ID" "$PREV_IME_BUNDLE_ID" <<'PY' 2>/dev/null || true
import subprocess, plistlib, sys
bundle_ids = set(sys.argv[1:])
domain = "com.apple.HIToolbox"
proc = subprocess.run(["defaults", "export", domain, "-"], capture_output=True)
if proc.returncode != 0:
    sys.exit(0)
data = plistlib.loads(proc.stdout)
changed = False
for key in ("AppleEnabledInputSources", "AppleSelectedInputSources"):
    sources = data.get(key)
    if not isinstance(sources, list):
        continue
    kept = [s for s in sources if s.get("Bundle ID") not in bundle_ids]
    if len(kept) != len(sources):
        data[key] = kept
        changed = True
if changed:
    subprocess.run(["defaults", "import", domain, "-"], input=plistlib.dumps(data))
PY

# ── 5. Remove app bundle ───────────────────────────────────────────────────────
info "Removing /Applications/${APP_DISPLAY}.app..."
rm -rf "$APP_BUNDLE"
rm -rf "$PREV_APP_BUNDLE"

# ── 6. Remove CLI symlinks ─────────────────────────────────────────────────────
info "Removing CLI symlinks..."
rm -f "${LOCAL_BIN}/ftab"
rm -f "${LOCAL_BIN}/fastabterm"
rm -f "${LOCAL_BIN}/ec"
rm -f "${LOCAL_BIN}/ecterm"

# ── 7. Fallback shell integration cleanup (in case the CLI was already removed)
# ftab/ec integrations uninstall shell was already called in step 1.
# This fallback removes any remaining lines using targeted patterns only.
info "Verifying shell integration removal..."

strip_shell_integration_fallback() {
  local rc_file="$1"
  [[ -f "$rc_file" ]] || return 0

  local tmp
  tmp="$(mktemp)"
  # Only remove lines that are specifically part of the Fastab / Easy Complete
  # integration block: the block comment headers and the source lines
  # referencing our shell data directory.
  grep -Ev \
    'Fastab (pre|post) block|Easy Complete (pre|post) block|(fastab|easy-complete)/shell/(zshrc|zprofile|bashrc|bash_profile)\.(pre|post)\.(zsh|bash)|eval "\$\((~/.local/bin/)?(ftab|ec|q) init |eval \((~/.local/bin/)?(ftab|ec|q) init |\[ -x ~/.local/bin/(ftab|ec|q) \] && eval |command -v (ftab|ec|q) >/dev/null 2>&1 && eval ' \
    "$rc_file" > "$tmp" || true
  mv "$tmp" "$rc_file"
}

strip_shell_integration_fallback "${HOME}/.zshrc"
strip_shell_integration_fallback "${HOME}/.zprofile"
strip_shell_integration_fallback "${HOME}/.bashrc"
strip_shell_integration_fallback "${HOME}/.bash_profile"
strip_shell_integration_fallback "${HOME}/.config/fish/config.fish"

# Fish shell — remove the dedicated fish integration conf files directly
rm -f "${HOME}/.config/fish/conf.d/00_fig_pre.fish"
rm -f "${HOME}/.config/fish/conf.d/99_fig_post.fish"

# ── 8. Remove application data ────────────────────────────────────────────────
info "Removing application data..."
rm -rf "$APP_SUPPORT"
rm -rf "$PREV_APP_SUPPORT"
rm -rf "$CACHE_DIR"
rm -rf "$PREV_CACHE_DIR"

# IPC sockets / logs live under the process temp dir, not ~/.local/share.
rm -rf "${TMP_ROOT}/fastabrun" "${TMP_ROOT}/ecrun" "${TMP_ROOT}/ftablog" "${TMP_ROOT}/eclog" 2>/dev/null || true
rm -rf /tmp/fastabrun /tmp/ecrun /tmp/ftablog /tmp/eclog 2>/dev/null || true

# Preferences
defaults delete "$BUNDLE_ID"          2>/dev/null || true
defaults delete "$IME_BUNDLE_ID"      2>/dev/null || true
defaults delete "$PREV_BUNDLE_ID"     2>/dev/null || true
defaults delete "$PREV_IME_BUNDLE_ID" 2>/dev/null || true

# Accessibility grant — drop the now-dead TCC entry so it doesn't linger in
# System Settings pointing at a removed binary.
tccutil reset Accessibility "$BUNDLE_ID" 2>/dev/null || true
tccutil reset Accessibility "$PREV_BUNDLE_ID" 2>/dev/null || true

# Keychain entries (best-effort)
security delete-generic-password -s "$BUNDLE_ID" 2>/dev/null || true
security delete-generic-password -s "$PREV_BUNDLE_ID" 2>/dev/null || true

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
info "Fastab has been fully uninstalled."
echo ""
echo "  To remove the ~/.local/bin directory itself (if empty):"
echo "    rmdir ~/.local/bin 2>/dev/null"
echo ""
echo "  Reload your shell to apply PATH changes:"
echo "    exec \$SHELL"
