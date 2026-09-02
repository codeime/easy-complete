#!/bin/bash
set -euo pipefail

# ── Easy Complete macOS installer ──────────────────────────────────────────

APP_NAME="easy-complete"          # binary / process name (no spaces)
APP_DISPLAY="Easy Complete"       # human-readable / bundle directory name
BUNDLE_ID="dev.emmmm.easy-complete"

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
STAGING_BUNDLE="${REPO_DIR}/build/${APP_DISPLAY}.app"
APP_BUNDLE="/Applications/${APP_DISPLAY}.app"
LOCAL_BIN="${HOME}/.local/bin"

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}==>${NC} $*"; }
warn()  { echo -e "${YELLOW}==>${NC} $*"; }
error() { echo -e "${RED}==>${NC} $*" >&2; }

file_sha() {
  local f="$1"
  if [ -f "$f" ]; then
    shasum -a 256 "$f" | awk '{print $1}'
  fi
}

process_running() {
  pgrep -x "$1" >/dev/null 2>&1
}

# SIGTERM and wait for the process to actually be gone, so the bundle is only
# replaced (and `open` only called) once nothing is holding the old one.
stop_process() {
  local name="$1"
  process_running "${name}" || return 0

  pkill -x "${name}" 2>/dev/null || true
  local waited=0
  while [ "${waited}" -lt 20 ]; do
    process_running "${name}" || return 0
    sleep 0.1
    waited=$((waited + 1))
  done

  warn "${name} did not exit; forcing it."
  pkill -9 -x "${name}" 2>/dev/null || true
  sleep 0.2
}

# Ask the relaunched desktop process whether its Accessibility grant is in
# effect, and leave the answer in `accessibility_state`: true, false, or
# unknown when the app never answered. `ec debug accessibility status` reports
# the desktop process's own `AXIsProcessTrusted()` over its local socket, so it
# speaks for the binary that was just installed, not for this shell. The socket
# takes a moment to come up after `open`, and a `false` only counts once it has
# been repeated, so a TCC lookup that is still settling cannot trigger a reset.
probe_accessibility() {
  accessibility_state=unknown
  local answer="" false_count=0 tries=0
  while [ "${tries}" -lt 15 ]; do
    answer="$(ec debug accessibility status 2>/dev/null | awk '/^Accessibility Enabled: /{print $3}' || true)"
    case "${answer}" in
      true)
        accessibility_state=true
        return 0
        ;;
      false)
        false_count=$((false_count + 1))
        if [ "${false_count}" -ge 3 ]; then
          accessibility_state=false
          return 0
        fi
        ;;
    esac
    sleep 1
    tries=$((tries + 1))
  done
  # Under `set -e` the caller must see success either way; the answer is in
  # `accessibility_state`, not the return code.
  return 0
}

# ── 1. Build & assemble the .app ──────────────────────────────────────────────
# Shared with CI (see .github/workflows/release.yml) so the bundle is assembled
# identically whether installed locally or packaged into a release DMG.
"${REPO_DIR}/scripts/build-app.sh"

# ── 3. Install to /Applications ───────────────────────────────────────────────
info "Installing to /Applications/..."

DESKTOP_BIN="Contents/MacOS/${APP_NAME}"
IME_BIN="Contents/Helpers/EasyCompleteInputMethod.app/Contents/MacOS/fig_input_method"

for required in "${DESKTOP_BIN}" "${IME_BIN}"; do
  if [ ! -f "${STAGING_BUNDLE}/${required}" ]; then
    error "Build produced no ${required}. Refusing to install over the current bundle."
    exit 1
  fi
done

desktop_changed=1
if [ "$(file_sha "${APP_BUNDLE}/${DESKTOP_BIN}")" = "$(file_sha "${STAGING_BUNDLE}/${DESKTOP_BIN}")" ]; then
  desktop_changed=0
fi
ime_changed=1
if [ "$(file_sha "${APP_BUNDLE}/${IME_BIN}")" = "$(file_sha "${STAGING_BUNDLE}/${IME_BIN}")" ]; then
  ime_changed=0
fi

# The desktop app always goes down, whether or not its binary changed: the
# bundle below is wiped and re-dittoed, and Contents/Resources/specs-ir is read
# lazily at completion time, so replacing it under a live process silently
# breaks completions until the next restart. It has no clients to preserve and
# is relaunched at the end of this script.
stop_process "${APP_NAME}"

# The IME is the one process worth keeping alive: open Otty / Ghostty / Kitty
# windows hold IMK connections to it and macOS never re-attaches them to a
# replacement. Same bytes → leave it running.
keep_ime=0
if [ "${ime_changed}" -eq 1 ]; then
  stop_process fig_input_method
elif process_running fig_input_method; then
  keep_ime=1
fi

# Preserve framework symlinks and bundle metadata. On macOS, `cp -r` follows
# symlinks while traversing and expands Sparkle.framework's versioned layout,
# which makes the installed app fail `codesign --verify --deep --strict`.
# When the IME stays up, keep Contents/Helpers where it is — deleting that
# bundle out from under the process drops its IMK clients. Everything else
# still has to go, because `ditto` merges into the destination and would
# otherwise leave files from the previous build behind.
if [ "${keep_ime}" -eq 1 ] && [ -d "${APP_BUNDLE}/Contents" ]; then
  find "${APP_BUNDLE}/Contents" -mindepth 1 -maxdepth 1 ! -name Helpers -print0 | xargs -0 rm -rf
else
  rm -rf "${APP_BUNDLE}"
fi
ditto "${STAGING_BUNDLE}" "${APP_BUNDLE}"

# ── 4. Symlink CLI binaries to ~/.local/bin ───────────────────────────────────
info "Linking binaries to ${LOCAL_BIN}..."
mkdir -p "${LOCAL_BIN}"
ln -sf "/Applications/${APP_DISPLAY}.app/Contents/MacOS/ec"     "${LOCAL_BIN}/ec"
ln -sf "/Applications/${APP_DISPLAY}.app/Contents/MacOS/ecterm" "${LOCAL_BIN}/ecterm"

# ── 6. Shell integration ───────────────────────────────────────────────────────
info "Installing shell integration..."
export PATH="${LOCAL_BIN}:${PATH}"
ec integrations install --silent dotfiles 2>/dev/null || {
  warn "Automatic shell setup skipped. Run manually:"
  warn "  ec integrations install dotfiles"
}

# ── 7. Input Method ────────────────────────────────────────────────────────────
info "Registering Input Method..."
# `ec integrations install input-method` launches the IME when it is down, and
# replaces it only when the on-disk helper is not the binary we last started.
ec integrations install --silent input-method 2>/dev/null || {
  warn "Input method registration skipped (only needed for Kitty/Alacritty/Zed/Ghostty/WezTerm)"
}

# ── 8. Accessibility permission ─────────────────────────────────────────────────
open "${APP_BUNDLE}"

# A new desktop binary carries a new ad-hoc signature, and TCC pins the
# Accessibility grant to a cdhash — but whether that costs the grant depends on
# the OS. macOS 26 re-pins the stored requirement to the new binary on its own
# (TCC.db ends up holding the fresh cdhash with auth_reason "System Set") and
# the app comes up trusted; older releases keep the stale requirement, leave the
# checkbox in System Settings ticked, and fail every AX call. So ask the new
# process instead of assuming. Resetting first would throw away a grant that
# carried over and send the user back to System Settings on every install.
# Same binary → same signature → nothing to check.
accessibility_reset=0
if [ "${desktop_changed}" -eq 1 ]; then
  info "Checking whether the Accessibility grant survived the new binary..."
  probe_accessibility
  case "${accessibility_state}" in
    true)
      info "Accessibility grant carried over."
      ;;
    false)
      info "Accessibility grant did not survive; resetting it..."
      tccutil reset Accessibility "${BUNDLE_ID}" 2>/dev/null || true
      accessibility_reset=1
      info "Requesting Accessibility permission..."
      # The probe just reached the desktop process over its local socket, so a
      # single attempt is enough here.
      if ec debug prompt-accessibility 2>/dev/null; then
        warn "Grant '${APP_DISPLAY}' in System Settings → Privacy & Security → Accessibility."
      else
        warn "Could not reach the desktop app to prompt for Accessibility. Once it is running, run:"
        warn "  ec debug prompt-accessibility"
      fi
      ;;
    *)
      warn "Could not reach the desktop app to check its Accessibility grant. Once it is running, run:"
      warn "  ec debug accessibility status"
      warn "and, if that reports false, 'ec debug accessibility refresh' to reset and re-prompt."
      ;;
  esac
fi

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
info "Installation complete!"
echo ""
echo "  App:  /Applications/${APP_DISPLAY}.app"
echo "  CLI:  ${LOCAL_BIN}/ec  ($(ec --version 2>/dev/null || echo 'restart shell to verify'))"
echo ""
if [ "${accessibility_reset}" -eq 1 ]; then
  echo "  If autocomplete does not appear, grant Accessibility to '${APP_DISPLAY}' in"
  echo "    System Settings → Privacy & Security → Accessibility"
fi
