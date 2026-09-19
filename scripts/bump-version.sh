#!/bin/bash
# Usage: ./scripts/bump-version.sh <version>
# Example: ./scripts/bump-version.sh 2.0.11
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>" >&2
  echo "Example: $0 2.0.11" >&2
  exit 1
fi

# Strip leading 'v' if present
VERSION="${VERSION#v}"

echo "Bumping version to $VERSION"

# Cargo workspace (all crates inherit this)
sed -i '' "s/^version = \".*\"$/version = \"$VERSION\"/" "$REPO_DIR/Cargo.toml"

# The About section reads the version from the Cargo workspace now that the
# settings window is native, so no TypeScript package carries it.

# App version shared by the website's SoftwareApplication data and DMG links.
sed -i '' \
  "s/^export const APP_VERSION = \".*\";$/export const APP_VERSION = \"$VERSION\";/" \
  "$REPO_DIR/website/src/download.ts"
grep -qxF "export const APP_VERSION = \"$VERSION\";" "$REPO_DIR/website/src/download.ts" \
  || { echo "Failed to update website/src/download.ts" >&2; exit 1; }

# GitHub's /releases/latest excludes prereleases. Link the current release tag
# directly, and advance both README links with each version bump.
for readme in "$REPO_DIR/README.md" "$REPO_DIR/README.zh-CN.md"; do
  sed -i '' -E \
    "s#https://github.com/codeime/easy-complete/releases/(latest/download|download/v[^/]+)/Fastab-arm64\\.dmg#https://github.com/codeime/easy-complete/releases/download/v${VERSION}/Fastab-arm64.dmg#g" \
    "$readme"
  grep -Fq "https://github.com/codeime/easy-complete/releases/download/v${VERSION}/Fastab-arm64.dmg" "$readme" \
    || { echo "Failed to update $readme download link" >&2; exit 1; }
done

# Refresh Cargo.lock so the bumped workspace versions are reflected there too.
# Without this the release commit ships a stale lock and CI's
# `cargo clippy/test --locked` fails. --workspace only touches our own crates
# (no external dependency bumps); --offline because a version bump needs no fetch.
echo "Refreshing Cargo.lock..."
(cd "$REPO_DIR" && cargo update --workspace --offline)

# The vendored crates carry the workspace version, so every bump makes the generated
# notices stale — and `build-app.sh` refuses to assemble the bundle until they match.
echo "Regenerating THIRD_PARTY_NOTICES.txt..."
(cd "$REPO_DIR" && node scripts/generate-third-party-notices.mjs)

echo "Done. Next steps:"
echo "  1. Add a ## v${VERSION} entry to both CHANGELOG.md (English) and CHANGELOG.zh-CN.md (Chinese)"
echo "  2. git add -A && git commit -m \"chore: bump version to v${VERSION}\"  # includes Cargo.lock"
echo "  3. git tag v${VERSION} && git push origin main --tags"
