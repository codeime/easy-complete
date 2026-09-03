#!/bin/bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MAIN_SVG="${REPO_DIR}/assets/logo.svg"
MENU_BAR_SVG="${REPO_DIR}/assets/menu-bar.svg"
SVG_RENDERER_SOURCE="${REPO_DIR}/scripts/render-svg.m"
DESKTOP_ICONS="${REPO_DIR}/crates/fig_desktop/icons"
APP_ICONSET="${DESKTOP_ICONS}/AppIcon.iconset"
IME_ICON="${REPO_DIR}/crates/fig_input_method/resources/product_icon.icns"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

for tool in clang iconutil sips; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: $tool is required (run this script on macOS)" >&2
    exit 1
  }
done

SVG_RENDERER="${WORK_DIR}/render-svg"
MAIN_RENDER="${WORK_DIR}/logo.png"
MENU_BAR_RENDER="${WORK_DIR}/menu-bar.png"

clang -fobjc-arc -framework AppKit "$SVG_RENDERER_SOURCE" -o "$SVG_RENDERER"
"$SVG_RENDERER" "$MAIN_SVG" 1024 "$MAIN_RENDER"
"$SVG_RENDERER" "$MENU_BAR_SVG" 512 "$MENU_BAR_RENDER"

for rendered in "$MAIN_RENDER" "$MENU_BAR_RENDER"; do
  [[ -f "$rendered" ]] || {
    echo "error: SVG renderer did not produce $(basename "$rendered")" >&2
    exit 1
  }
done

render_png() {
  local source="$1"
  local size="$2"
  local output="$3"
  local inset="${4:-0}"
  local candidate="${WORK_DIR}/$(basename "$output").${size}.${inset}.png"

  if [[ "$inset" == "0" ]]; then
    sips -z "$size" "$size" "$source" --out "$candidate" >/dev/null
  else
    "$SVG_RENDERER" "$source" "$size" "$candidate" "$inset"
  fi
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
  "16x16.png:16:1"
  "16x16@2x.png:32:2"
  "32x32.png:32:0"
  "32x32@2x.png:64:0"
  "128x128.png:128:0"
  "128x128@2x.png:256:0"
  "256x256.png:256:0"
  "256x256@2x.png:512:0"
  "512x512.png:512:0"
  "512x512@2x.png:1024:0"
)

for entry in "${icon_names[@]}"; do
  name="${entry%%:*}"
  dimensions="${entry#*:}"
  size="${dimensions%%:*}"
  inset="${entry##*:}"
  source="$MAIN_RENDER"
  if [[ "$inset" != "0" ]]; then
    source="$MAIN_SVG"
  fi
  render_png "$source" "$size" "${DESKTOP_ICONS}/${name}" "$inset"
  copy_if_changed "${DESKTOP_ICONS}/${name}" "${APP_ICONSET}/icon_${name}"
done

render_png "$MAIN_RENDER" 512 "${DESKTOP_ICONS}/icon.png"
render_png "$MAIN_RENDER" 512 "${REPO_DIR}/assets/logo.png"
render_png "$MAIN_RENDER" 180 "${REPO_DIR}/website/src/assets/logo.png"

iconutil -c icns "$APP_ICONSET" -o "${DESKTOP_ICONS}/icon.icns"
copy_if_changed "${DESKTOP_ICONS}/icon.icns" "$IME_ICON"

render_png "$MENU_BAR_RENDER" 512 "${REPO_DIR}/assets/menu-bar.png"
render_png "$MENU_BAR_RENDER" 22 "${DESKTOP_ICONS}/icon-monochrome.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/icon-monochrome-light.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/not-logged-in.png"
copy_if_changed "${DESKTOP_ICONS}/icon-monochrome.png" "${DESKTOP_ICONS}/not-logged-in-light.png"

echo "Generated Easy Complete app and menu-bar icons."
