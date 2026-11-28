#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="2.2.6"
PKG_NAME="furcadia-native"
RPM_TOPDIR="$ROOT_DIR/dist/rpmbuild"

cd "$ROOT_DIR"
cargo build --release --locked --bins

rm -rf "$RPM_TOPDIR"
mkdir -p \
    "$RPM_TOPDIR/BUILD" \
    "$RPM_TOPDIR/BUILDROOT" \
    "$RPM_TOPDIR/RPMS" \
    "$RPM_TOPDIR/SOURCES" \
    "$RPM_TOPDIR/SPECS" \
    "$RPM_TOPDIR/SRPMS"

rpmbuild -bb \
    --define "_topdir $RPM_TOPDIR" \
    --define "_srcroot $ROOT_DIR" \
    "$ROOT_DIR/packaging/rpm/furcadia-native.spec"

mkdir -p "$ROOT_DIR/dist"
cp "$RPM_TOPDIR"/RPMS/*/*.rpm "$ROOT_DIR/dist/"
rm -rf "$RPM_TOPDIR"

for rpm_file in "$ROOT_DIR"/dist/"${PKG_NAME}"-"${VERSION}"-*.rpm; do
    echo "Built: $rpm_file"
done
