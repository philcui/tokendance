#!/bin/bash
# Build TokenDance.app from source (macOS only).
# Usage: ./scripts/build_app.sh
#   BUNDLE_ID=com.yourname.tokendance ./scripts/build_app.sh  # custom bundle id
set -euo pipefail
cd "$(dirname "$0")/.."

BUNDLE_ID="${BUNDLE_ID:-com.tokendance.app}"
APP="build/TokenDance.app"

echo "==> Compiling Swift menubar app..."
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# 远端服务地址：客户端里唯一的出处（见 ServiceConfig.swift.in）。默认官方地址，
# 自建/改名用 `SERVICE_BASE=https://… ./scripts/build_app.sh` 覆盖，源码一行不用动。
SERVICE_BASE="${SERVICE_BASE:-https://fanshitou.cn/tokendance}"
GEN_DIR="build/gen"
mkdir -p "$GEN_DIR"
sed -e "s|__SERVICE_BASE__|$SERVICE_BASE|" ServiceConfig.swift.in > "$GEN_DIR/ServiceConfig.swift"
echo "    service base: $SERVICE_BASE"
# Every Swift source in the project root must be listed here; swiftc has no
# implicit "compile the directory" mode when invoked with -o. The app is split
# by concern: AppMain is the delegate, the rest are the HUD views, the update
# helper, telemetry and the leaderboard.
swiftc -O AppMain.swift HUDViews.swift Leaderboard.swift Telemetry.swift \
    Updater.swift RingRenderer.swift "$GEN_DIR/ServiceConfig.swift" \
    -o "$APP/Contents/MacOS/TokenUsage"

echo "==> Writing Info.plist (bundle id: $BUNDLE_ID)..."
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>TokenUsage</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <!-- The app menu in the menu bar takes its title from CFBundleName, not
         from CFBundleDisplayName, so this has to be the product name or the
         menu bar would read "TokenUsage" while the Dock reads "TokenDance". -->
    <key>CFBundleName</key><string>TokenDance</string>
    <key>CFBundleDisplayName</key><string>TokenDance</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.3.1</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <!-- No LSUIElement: the app is a regular app so it has a Dock icon and a
         menu bar. It used to be an accessory (menu-bar item only), which is why
         this key was here; AppMain also sets .regular to match. -->
    <key>NSAppTransportSecurity</key>
    <dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict>
</plist>
PLIST

echo "==> Bundling dashboard + settings + icon..."
cp dashboard.html "$APP/Contents/Resources/dashboard.html"
cp settings.html "$APP/Contents/Resources/settings.html"
cp about.html "$APP/Contents/Resources/about.html"
cp sources.html "$APP/Contents/Resources/sources.html"
cp Assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# Vendored third-party assets, served by the server at /vendor/… (see
# vendor/README.md). Bundling them is what keeps the dashboard off a CDN.
mkdir -p "$APP/Contents/Resources/vendor"
cp vendor/*.js "$APP/Contents/Resources/vendor/"

# The app is only a client: every number it shows comes from the local server,
# and it starts that server itself when 127.0.0.1:8737 is not answering. Shipping
# the binary inside the bundle is what makes the app self-contained, i.e. what
# makes "double-click it and it works" true — without this it would fall back to
# ~/.local/bin and refuse to start on a clean machine.
echo "==> Bundling the local server..."
SERVER_BIN="rust-server/target/release/tokendance-server"
# Build it here rather than trusting whatever happens to sit in target/.
# It did happen: HTML/CSS edits go through this script without touching Rust, so
# "run build_app.sh" felt like a full build — and a server change made in the
# same sitting was packaged as the *previous* binary, install and release alike.
if command -v cargo >/dev/null 2>&1; then
  echo "    cargo build --release (rust-server)..."
  (cd rust-server && cargo build --release --quiet)
fi
if [ -x "$SERVER_BIN" ]; then
  # Belt and braces: refuse to ship a binary older than the sources it came from.
  NEWEST_SRC=$(find rust-server/src -newer "$SERVER_BIN" -type f 2>/dev/null | head -1)
  if [ -n "$NEWEST_SRC" ]; then
    echo "    WARNING: $SERVER_BIN is older than $NEWEST_SRC — the bundle may be stale."
  fi
  cp "$SERVER_BIN" "$APP/Contents/Resources/tokendance-server"
else
  echo "    WARNING: $SERVER_BIN not built."
  echo "    The app will still run, but only if a server is installed at"
  echo "    ~/.local/bin/tokendance-server. Build it with:"
  echo "        cd rust-server && cargo build --release"
fi

echo "==> Ad-hoc signing..."
codesign --force --deep -s - "$APP"

echo ""
echo "Done: $APP"
echo "Run it:  open $APP"
echo "Server:  cd rust-server && cargo run --release -- --port 8737"
