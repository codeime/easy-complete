#!/usr/bin/env bash
#
# T4.2 publish gate: no rquickjs in fig_desktop, no runtime JS in the shipped
# tree, specs-ir payload ≤ 35 MiB.
#
#   scripts/assert-no-runtime-js.sh
#     cargo tree + compiled bundle/specs-ir (CI after compile-spec-ir)
#   scripts/assert-no-runtime-js.sh "build/Fastab.app/Contents/Resources"
#     cargo tree + the assembled .app (release after build-app.sh)
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

tree_hits="$(cargo tree -p fig_desktop -e normal | grep -c rquickjs || true)"
if [ "$tree_hits" != "0" ]; then
  echo "error: fig_desktop still links rquickjs ($tree_hits hits)" >&2
  cargo tree -p fig_desktop -e normal | grep rquickjs >&2 || true
  exit 1
fi

if [ "${1:-}" != "" ]; then
  resources="$1"
  if [ ! -d "$resources" ]; then
    echo "error: missing Resources directory: $resources" >&2
    exit 1
  fi
  search_root="$resources"
  specs_ir="$resources/specs-ir"
else
  search_root="${EC_SPECS_IR:-$ROOT/bundle/specs-ir}"
  specs_ir="$search_root"
fi

if [ ! -d "$specs_ir" ]; then
  echo "error: missing specs-ir directory: $specs_ir" >&2
  exit 1
fi

leftovers="$(find "$search_root" \( -name '*.js' -o -name '*.mjs' \) -print)"
if [ -n "$leftovers" ]; then
  echo "error: leftover runtime JS under $search_root:" >&2
  printf '%s\n' "$leftovers" >&2
  exit 1
fi
if [ -e "$specs_ir/hooks" ] || [ -e "$specs_ir/source-modules" ] || [ -e "$specs_ir/hook-modules.json" ]; then
  echo "error: leftover runtime JS artifacts under $specs_ir" >&2
  exit 1
fi

allocated="$(du -sm "$specs_ir" | cut -f1)"
# File payload is ~34 MiB. `du -sm` counts 4 KiB blocks, so a tree of many
# small JSON files reads ~37 on ext4. T4.2's 35 MiB is the payload ceiling.
# Summed with `cat | wc -c` rather than `stat`, whose size flag differs
# between the BSD stat on the release runner and the GNU stat in CI.
apparent_bytes="$(find "$specs_ir" -type f -exec cat {} + | wc -c)"
apparent="$((apparent_bytes / 1024 / 1024))"
echo "specs-ir $specs_ir: ${apparent} MiB apparent, ${allocated} MiB allocated"
if [ "$apparent" -gt 35 ]; then
  echo "error: specs-ir payload is ${apparent} MiB; must be ≤ 35 MiB" >&2
  exit 1
fi
if [ "$allocated" -gt 40 ]; then
  echo "error: specs-ir allocated size is ${allocated} MiB; must be ≤ 40 MiB" >&2
  exit 1
fi
