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
</dict>
</plist>
PLIST

echo "built $APP"
echo "run it:            open '$APP'"
echo "start at login:    System Settings > General > Login Items > +"
