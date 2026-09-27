#!/usr/bin/env bash
# Build DuckPlus and install it. Re-run any time to upgrade in place.
#
#   scripts/install.sh               # /Applications (or ~/Applications) + `duckplus` CLI
#   scripts/install.sh --no-cli      # skip the CLI shim
#   scripts/install.sh --open        # launch the app when done
#   DUCKPLUS_INSTALL_DIR=~/Apps scripts/install.sh
set -euo pipefail
cd "$(dirname "$0")/.."

[[ "$(uname)" == "Darwin" ]] || { echo "install.sh currently supports macOS only"; exit 1; }
command -v cargo >/dev/null || { echo "cargo not found — install Rust from https://rustup.rs"; exit 1; }

WITH_CLI=1
OPEN_APP=0
for arg in "$@"; do
  case "$arg" in
    --no-cli) WITH_CLI=0 ;;
    --open) OPEN_APP=1 ;;
    *) echo "unknown option: $arg"; exit 1 ;;
  esac
done

# Pick the Applications folder we can write to.
if [[ -n "${DUCKPLUS_INSTALL_DIR:-}" ]]; then
  DEST="${DUCKPLUS_INSTALL_DIR/#\~/$HOME}"
elif [[ -w /Applications ]]; then
  DEST=/Applications
else
  DEST="$HOME/Applications"
fi
mkdir -p "$DEST"

echo "==> Building DuckPlus (release)…"
scripts/bundle-macos.sh >/dev/null
echo "    built target/bundle/DuckPlus.app"

# Quit a running copy so the bundle can be replaced cleanly.
if pgrep -f "$DEST/DuckPlus.app/Contents/MacOS/DuckPlus" >/dev/null 2>&1; then
  echo "==> Quitting running DuckPlus…"
  osascript -e 'tell application id "app.duckplus.DuckPlus" to quit' >/dev/null 2>&1 || true
  for _ in {1..20}; do
    pgrep -f "$DEST/DuckPlus.app/Contents/MacOS/DuckPlus" >/dev/null 2>&1 || break
    sleep 0.25
  done
  pkill -f "$DEST/DuckPlus.app/Contents/MacOS/DuckPlus" 2>/dev/null || true
fi

echo "==> Installing to $DEST/DuckPlus.app"
rm -rf "$DEST/DuckPlus.app"
ditto target/bundle/DuckPlus.app "$DEST/DuckPlus.app"
# Locally built, so clear quarantine in case the source tree was downloaded.
xattr -dr com.apple.quarantine "$DEST/DuckPlus.app" 2>/dev/null || true
# Refresh Launch Services so Spotlight/Dock pick up the new icon and version.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$DEST/DuckPlus.app" >/dev/null 2>&1 || true

if [[ "$WITH_CLI" == 1 ]]; then
  if [[ -w /usr/local/bin ]]; then
    BIN_DIR=/usr/local/bin
  else
    BIN_DIR="$HOME/.local/bin"
    mkdir -p "$BIN_DIR"
  fi
  # A shim rather than a symlink: launches detached so the terminal stays free.
  cat > "$BIN_DIR/duckplus" <<SH
#!/usr/bin/env bash
# DuckPlus CLI: duckplus [quack:host[:port]] [-t TOKEN] [-c SQL]
APP="$DEST/DuckPlus.app"
if [[ \$# -eq 0 ]]; then
  exec open "\$APP"
fi
nohup "\$APP/Contents/MacOS/DuckPlus" "\$@" >/dev/null 2>&1 &
disown
SH
  chmod +x "$BIN_DIR/duckplus"
  echo "==> CLI installed: $BIN_DIR/duckplus"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "    note: add $BIN_DIR to your PATH to use it" ;;
  esac
fi

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
echo "==> DuckPlus $VERSION installed. Launch it from Spotlight, or: open -a DuckPlus"

if [[ "$OPEN_APP" == 1 ]]; then
  echo "==> Launching DuckPlus…"
  open "$DEST/DuckPlus.app"
fi
