#!/usr/bin/env bash
# Build a signed, notarized DuckPlus-<version>.dmg (arm64 + x86_64) in target/bundle/.
#
#   scripts/release-macos.sh                  # build, sign, notarize, staple
#   scripts/release-macos.sh --skip-notarize  # build and sign only
#   scripts/release-macos.sh --publish        # …then create the GitHub release (as Latest)
#   scripts/release-macos.sh --bump=patch     # patch|minor|major|keep, skips the prompt
#
# It first asks whether to bump the version (patch, minor, major or keep) and
# commits the bump; --publish pushes that commit and tags it.
#
# Besides DuckPlus-<version>.dmg it writes DuckPlus.dmg, the same file under a
# fixed name, so this link always serves the newest release:
#   https://github.com/wes/duckplus/releases/latest/download/DuckPlus.dmg
#
# Needs a "Developer ID Application" certificate in the keychain, and (for
# notarization) credentials saved once with:
#
#   xcrun notarytool store-credentials duckplus-notary \
#     --apple-id you@example.com --team-id TEAMID
#
# Override with DUCKPLUS_SIGN_ID="Developer ID Application: …" and
# DUCKPLUS_NOTARY_PROFILE=name.
set -euo pipefail
cd "$(dirname "$0")/.."

[[ "$(uname)" == "Darwin" ]] || { echo "release-macos.sh needs macOS"; exit 1; }

NOTARIZE=1
PUBLISH=0
BUMP=""
for arg in "$@"; do
  case "$arg" in
    --skip-notarize) NOTARIZE=0 ;;
    --publish) PUBLISH=1 ;;
    --bump=*) BUMP="${arg#--bump=}" ;;
    *) echo "unknown option: $arg"; exit 1 ;;
  esac
done

# Bump the version first, so a release never ships under the last one's number.
CURRENT=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
IFS=. read -r MAJOR MINOR PATCH <<<"$CURRENT"
if [[ -z "$BUMP" && -t 0 ]]; then
  # A version with no release yet was bumped by an earlier, unfinished run:
  # default to keeping it rather than skipping a number.
  DEFAULT=1
  if ! gh release view "v$CURRENT" >/dev/null 2>&1; then
    DEFAULT=4
    echo "v$CURRENT isn't released yet."
  fi
  echo "Current version: $CURRENT"
  echo "  1) patch  → $MAJOR.$MINOR.$((PATCH + 1))"
  echo "  2) minor  → $MAJOR.$((MINOR + 1)).0"
  echo "  3) major  → $((MAJOR + 1)).0.0"
  echo "  4) keep   → $CURRENT"
  read -r -p "Bump version [$DEFAULT]: " choice
  case "${choice:-$DEFAULT}" in
    1|patch) BUMP=patch ;;
    2|minor) BUMP=minor ;;
    3|major) BUMP=major ;;
    4|keep) BUMP=keep ;;
    *) echo "unknown choice: $choice"; exit 1 ;;
  esac
fi
case "${BUMP:-keep}" in
  patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
  minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
  major) NEW="$((MAJOR + 1)).0.0" ;;
  keep) NEW="$CURRENT" ;;
  *) echo "--bump must be patch, minor, major or keep"; exit 1 ;;
esac

if [[ "$PUBLISH" == 1 ]] && gh release view "v$NEW" >/dev/null 2>&1; then
  echo "v$NEW is already released; bump the version"
  exit 1
fi

if [[ "$NEW" != "$CURRENT" ]]; then
  sed -i '' "1,/^version = /s/^version = \"$CURRENT\"/version = \"$NEW\"/" Cargo.toml
  cargo update --workspace --offline --quiet
  git commit --quiet -m "Release $NEW" -- Cargo.toml Cargo.lock
  echo "==> Bumped $CURRENT → $NEW (committed; pushed with --publish)"
fi

SIGN_ID="${DUCKPLUS_SIGN_ID:-$(security find-identity -v -p codesigning \
  | grep -m1 -o '"Developer ID Application: [^"]*"' | tr -d '"' || true)}"
[[ -n "$SIGN_ID" ]] || { echo "No Developer ID Application certificate found in the keychain"; exit 1; }
PROFILE="${DUCKPLUS_NOTARY_PROFILE:-duckplus-notary}"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=target/bundle/DuckPlus.app
DMG=target/bundle/DuckPlus-${VERSION}.dmg

echo "==> Building universal DuckPlus ${VERSION}…"
scripts/bundle-macos.sh --universal

echo "==> Signing with $SIGN_ID"
# Hardened runtime + secure timestamp are required for notarization.
codesign --force --options runtime --timestamp \
  --entitlements scripts/DuckPlus.entitlements \
  --sign "$SIGN_ID" "$APP"
codesign --verify --strict --verbose=2 "$APP"

echo "==> Packaging $DMG"
STAGE=target/bundle/dmg
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/DuckPlus.app"
# Drag-to-install: the app next to a link to /Applications.
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "DuckPlus $VERSION" -srcfolder "$STAGE" \
  -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
rm -rf "$STAGE"
codesign --force --timestamp --sign "$SIGN_ID" "$DMG"

if [[ "$NOTARIZE" == 1 ]]; then
  echo "==> Notarizing (this usually takes a few minutes)…"
  xcrun notarytool submit "$DMG" --keychain-profile "$PROFILE" --wait \
    | tee target/bundle/notarize.log
  if ! grep -q "status: Accepted" target/bundle/notarize.log; then
    ID=$(grep -m1 -o 'id: [0-9a-f-]*' target/bundle/notarize.log | cut -d' ' -f2)
    echo "Notarization failed. Details:"
    [[ -n "$ID" ]] && xcrun notarytool log "$ID" --keychain-profile "$PROFILE"
    exit 1
  fi
  xcrun stapler staple "$DMG"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi

cp "$DMG" target/bundle/DuckPlus.dmg
(cd target/bundle && for f in "DuckPlus-${VERSION}.dmg" DuckPlus.dmg; do
  shasum -a 256 "$f" | tee "$f.sha256"
done)
echo "==> Done: $DMG ($(du -h "$DMG" | cut -f1))"

if [[ "$PUBLISH" == 1 ]]; then
  [[ "$NOTARIZE" == 1 ]] || { echo "Refusing to publish a build that wasn't notarized"; exit 1; }
  echo "==> Publishing GitHub release v${VERSION}…"
  # The tag goes on this commit, so GitHub needs it (and the bump) first.
  git push --quiet origin HEAD
  gh release create "v${VERSION}" --target "$(git rev-parse HEAD)" \
    --title "DuckPlus ${VERSION}" --generate-notes --latest \
    "$DMG" "$DMG.sha256" target/bundle/DuckPlus.dmg target/bundle/DuckPlus.dmg.sha256
fi
