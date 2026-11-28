#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="2.2.5"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
PKG_NAME="furcadia-native"
STAGE_DIR="$ROOT_DIR/dist/${PKG_NAME}_${VERSION}_${ARCH}"

cd "$ROOT_DIR"
cargo build --release --locked --bins

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/DEBIAN" "$STAGE_DIR/usr/bin" "$STAGE_DIR/usr/share/applications" "$STAGE_DIR/usr/share/icons/hicolor/256x256/apps" "$STAGE_DIR/usr/share/doc/${PKG_NAME}"
chmod 0755 "$STAGE_DIR" "$STAGE_DIR/DEBIAN" "$STAGE_DIR/usr" "$STAGE_DIR/usr/bin" "$STAGE_DIR/usr/share" "$STAGE_DIR/usr/share/applications" "$STAGE_DIR/usr/share/icons" "$STAGE_DIR/usr/share/icons/hicolor" "$STAGE_DIR/usr/share/icons/hicolor/256x256" "$STAGE_DIR/usr/share/icons/hicolor/256x256/apps" "$STAGE_DIR/usr/share/doc" "$STAGE_DIR/usr/share/doc/${PKG_NAME}"

install -m 755 "$ROOT_DIR/target/release/furc-gui" "$STAGE_DIR/usr/bin/furc-gui"
install -m 755 "$ROOT_DIR/target/release/furc-cli" "$STAGE_DIR/usr/bin/furc-cli"
install -m 644 "$ROOT_DIR/packaging/linux/furcadia-native.desktop" "$STAGE_DIR/usr/share/applications/furcadia-native.desktop"
install -m 644 "$ROOT_DIR/packaging/linux/furcadia-native.png" "$STAGE_DIR/usr/share/icons/hicolor/256x256/apps/furcadia-native.png"
install -m 644 "$ROOT_DIR/LICENSE" "$STAGE_DIR/usr/share/doc/${PKG_NAME}/copyright"

cat > "$STAGE_DIR/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: games
Priority: optional
Architecture: ${ARCH}
Maintainer: Morgan Lowe <https://github.com/morganlowe/native-furcadia>
Depends: libc6, libgl1, libx11-6, libxcb1, libxkbcommon0, curl, bzip2, unzip
Description: Native Furcadia client (GUI and CLI)
 A native Rust Furcadia client supporting modern dreams, custom portraits,
 Pounce watchlist, whisper tabs, DragonSpeak, and Windows asset import.
EOF
chmod 0644 "$STAGE_DIR/DEBIAN/control"

mkdir -p "$ROOT_DIR/dist"
dpkg-deb --build --root-owner-group "$STAGE_DIR" "$ROOT_DIR/dist/${PKG_NAME}_${VERSION}_${ARCH}.deb"
echo "Built: $ROOT_DIR/dist/${PKG_NAME}_${VERSION}_${ARCH}.deb"
