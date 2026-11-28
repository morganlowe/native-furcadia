#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="2.2.6"
APP_NAME="Furcadia Native Client"
ARCH="$(uname -m)"
DMG_STAGE="$ROOT_DIR/dist/macos-dmg-stage"
ICONSET_DIR="$ROOT_DIR/dist/AppIcon.iconset"
APP_DIR="$DMG_STAGE/${APP_NAME}.app"
OUT_DMG="$ROOT_DIR/dist/FurcadiaNative-${VERSION}-macos-${ARCH}.dmg"

cd "$ROOT_DIR"
cargo build --release --locked --bins

rm -rf "$DMG_STAGE" "$ICONSET_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources" "$ICONSET_DIR" "$ROOT_DIR/dist"

install -m 755 "$ROOT_DIR/target/release/furc-gui" "$APP_DIR/Contents/MacOS/furc-gui"
install -m 755 "$ROOT_DIR/target/release/furc-cli" "$APP_DIR/Contents/MacOS/furc-cli"
install -m 644 "$ROOT_DIR/LICENSE" "$APP_DIR/Contents/Resources/LICENSE"

SRC_ICON="$ROOT_DIR/native-icon.png"
sips -z 16 16     "$SRC_ICON" --out "$ICONSET_DIR/icon_16x16.png"      >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET_DIR/icon_16x16@2x.png"   >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET_DIR/icon_32x32.png"      >/dev/null
sips -z 64 64     "$SRC_ICON" --out "$ICONSET_DIR/icon_32x32@2x.png"   >/dev/null
sips -z 128 128   "$SRC_ICON" --out "$ICONSET_DIR/icon_128x128.png"    >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET_DIR/icon_128x128@2x.png" >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET_DIR/icon_256x256.png"    >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET_DIR/icon_256x256@2x.png" >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET_DIR/icon_512x512.png"    >/dev/null
sips -z 1024 1024 "$SRC_ICON" --out "$ICONSET_DIR/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$ICONSET_DIR" -o "$APP_DIR/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET_DIR"

cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleExecutable</key>
    <string>furc-gui</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.bearbuilt.furcadia-native</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.role-playing-games</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$APP_DIR"
ln -s /Applications "$DMG_STAGE/Applications"

rm -f "$OUT_DMG"
for attempt in 1 2 3; do
    if hdiutil create -volname "${APP_NAME} ${VERSION}" -srcfolder "$DMG_STAGE" -ov -format UDZO "$OUT_DMG"; then
        break
    fi
    if [ "$attempt" -eq 3 ]; then
        echo "hdiutil create failed after 3 attempts" >&2
        exit 1
    fi
    sleep 2
done

rm -rf "$DMG_STAGE"
echo "Built: $OUT_DMG"
