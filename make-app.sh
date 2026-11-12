#!/bin/sh
# Bundles the release binary as "Internet Status.app".
#
# The bundle sets LSUIElement, which makes this a menu-bar app: no Dock icon
# and no app switcher entry. The window is reached from the menu bar instead,
# which is why the tray menu has "Show window".
set -eu

cargo build --release

APP="target/Internet Status.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/internet_status "$APP/Contents/MacOS/internet_status"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Internet Status</string>
  <key>CFBundleDisplayName</key>       <string>Internet Status</string>
  <key>CFBundleIdentifier</key>        <string>net.local.internet-status</string>
  <key>CFBundleExecutable</key>        <string>internet_status</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key>           <string>0.2.0</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <!-- Menu-bar app: no Dock icon, no app switcher entry. -->
  <key>LSUIElement</key>               <true/>
  <!-- The router probe is LAN traffic, which macOS gates behind Local
       Network privacy. Without this string the app is never asked about. -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>Internet Status pings your router to tell a local fault from an ISP outage.</string>
</dict>
</plist>
PLIST

# Sign the whole bundle under its bundle identifier. cargo's linker leaves the
# binary ad-hoc signed as "internet_status-<hash>", which does not match the
# bundle ID that the Local Network grant is stored under: macOS then blocks
# every LAN packet (ping says "No route to host") even with the toggle on.
# Ad-hoc by default; set SIGN_IDENTITY to a certificate so the grant
# survives rebuilds.
codesign --force --sign "${SIGN_IDENTITY:--}" \
  --identifier net.local.internet-status "$APP"

echo "built $APP"
echo "run it:            open '$APP'"
echo "start at login:    System Settings > General > Login Items > +"
