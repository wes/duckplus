#!/usr/bin/env bash
# Build a release DuckPlus.app (and optionally a zip) in target/bundle/.
#   scripts/bundle-macos.sh            # native arch
#   scripts/bundle-macos.sh --universal # arm64 + x86_64
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=target/bundle/DuckPlus.app

if [[ "${1:-}" == "--universal" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  # Use rustup's toolchain by path, which has both targets; a Homebrew
  # cargo/rustc earlier on PATH only has the native one.
  SYSROOT=$(rustup run stable rustc --print sysroot)
  export RUSTC="$SYSROOT/bin/rustc"
  "$SYSROOT/bin/cargo" build --release --target aarch64-apple-darwin
  "$SYSROOT/bin/cargo" build --release --target x86_64-apple-darwin
  BIN=target/bundle/duckplus-universal
  mkdir -p target/bundle
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/duckplus \
    target/x86_64-apple-darwin/release/duckplus
else
  cargo build --release
  BIN=target/release/duckplus
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/DuckPlus"

# Icon: 1024px master -> .icns
ICONSET=target/bundle/DuckPlus.iconset
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon/duckplus-1024.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) assets/icon/duckplus-1024.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/DuckPlus.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DuckPlus</string>
  <key>CFBundleDisplayName</key><string>DuckPlus</string>
  <key>CFBundleIdentifier</key><string>app.duckplus.DuckPlus</string>
  <key>CFBundleExecutable</key><string>DuckPlus</string>
  <key>CFBundleIconFile</key><string>DuckPlus</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>UTImportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>org.duckdb.database</string>
      <key>UTTypeDescription</key><string>DuckDB Database</string>
      <key>UTTypeConformsTo</key>
      <array><string>public.data</string><string>public.database</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>duckdb</string><string>ddb</string></array>
      </dict>
    </dict>
  </array>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>DuckDB Database</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Default</string>
      <key>LSItemContentTypes</key><array><string>org.duckdb.database</string></array>
    </dict>
    <dict>
      <!-- .db is shared with SQLite and others: offer DuckPlus, don't claim it. -->
      <key>CFBundleTypeName</key><string>Database</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>CFBundleTypeExtensions</key><array><string>db</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Ad-hoc sign so Gatekeeper/Keychain treat it as a stable app identity locally.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

(cd target/bundle && rm -f "DuckPlus-${VERSION}-macos.zip" && ditto -c -k --keepParent DuckPlus.app "DuckPlus-${VERSION}-macos.zip")
echo "Built $APP ($(du -sh "$APP" | cut -f1)) and target/bundle/DuckPlus-${VERSION}-macos.zip"
