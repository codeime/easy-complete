#!/bin/bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MAIN_SVG="${REPO_DIR}/assets/logo.svg"
MENU_BAR_SVG="${REPO_DIR}/assets/menu-bar.svg"
SVG_RENDERER_SOURCE="${REPO_DIR}/scripts/render-svg.m"
DESKTOP_ICONS="${REPO_DIR}/crates/fastab_desktop/icons"
APP_ICONSET="${DESKTOP_ICONS}/AppIcon.iconset"
IME_ICON="${REPO_DIR}/crates/fastab_input_method/resources/product_icon.icns"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

for tool in clang iconutil; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: $tool is required (run this script on macOS)" >&2
    exit 1
  }
done

SVG_RENDERER="${WORK_DIR}/render-svg"

clang -fobjc-arc -framework AppKit "$SVG_RENDERER_SOURCE" -o "$SVG_RENDERER"

render_png() {
  local source="$1"
  local size="$2"
  local output="$3"
  local inset="${4:-0}"
  local candidate="${WORK_DIR}/$(basename "$output").${size}.${inset}.png"

  # Render every target directly from its SVG source. Scaling a 1024 px
  # intermediate bitmap with sips makes the small AppKit icons noticeably soft.
  "$SVG_RENDERER" "$source" "$size" "$candidate" "$inset"
  "$SVG_RENDERER" --check-transparent-corners "$candidate"
  if [[ ! -f "$output" ]] || ! cmp -s "$candidate" "$output"; then
    cp "$candidate" "$output"
  fi
}

copy_if_changed() {
  local source="$1"
  local output="$2"

  if [[ ! -f "$output" ]] || ! cmp -s "$source" "$output"; then
    cp "$source" "$output"
  fi
}

mkdir -p "$APP_ICONSET"

declare -a icon_names=(
  "16x16.png:16"
  "16x16@2x.png:32"
  "32x32.png:32"
  "32x32@2x.png:64"
  "128x128.png:128"
  "128x128@2x.png:256"
  "256x256.png:256"
  "256x256@2x.png:512"
  "512x512.png:512"
  "512x512@2x.png:1024"
)

for entry in "${icon_names[@]}"; do
  name="${entry%%:*}"
  size="${entry#*:}"
  # Keep the same 100/1024 transparent margin at every scale. The inset is
  # fractional for small targets, so calculate it instead of rounding pixels.
  inset="$(awk -v size="$size" 'BEGIN { printf "%.6f", size * 100 / 1024 }')"
  render_png "$MAIN_SVG" "$size" "${DESKTOP_ICONS}/${name}" "$inset"
  copy_if_changed "${DESKTOP_ICONS}/${name}" "${APP_ICONSET}/icon_${name}"
done

# icon.png is embedded by the desktop window and follows the padded app-icon
# scale. Keep the public artwork assets full bleed as the SVG source itself.
render_png "$MAIN_SVG" 512 "${DESKTOP_ICONS}/icon.png" "$(awk 'BEGIN { printf "%.6f", 512 * 100 / 1024 }')"
render_png "$MAIN_SVG" 512 "${REPO_DIR}/assets/logo.png"
render_png "$MAIN_SVG" 180 "${REPO_DIR}/website/src/assets/logo.png"

iconutil -c icns "$APP_ICONSET" -o "${DESKTOP_ICONS}/icon.icns"
copy_if_changed "${DESKTOP_ICONS}/icon.icns" "$IME_ICON"

render_png "$MENU_BAR_SVG" 512 "${REPO_DIR}/assets/menu-bar.png"
# tray-icon displays macOS tray icons at 18 pt. Keep a logical 18 px asset for
# 1x displays and provide a native 36 px @2x asset for Retina displays.
render_png "$MENU_BAR_SVG" 18 "${DESKTOP_ICONS}/icon-monochrome.png"
render_png "$MENU_BAR_SVG" 36 "${DESKTOP_ICONS}/icon-monochrome@2x.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/icon-monochrome-light.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/not-logged-in.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome@2x.png" "${DESKTOP_ICONS}/not-logged-in@2x.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/not-logged-in-light.png"

echo "Generated Easy Complete app and menu-bar icons."
