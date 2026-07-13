#!/usr/bin/env bash
# Build a .deb for LinuxSCP without debhelper: compile in release mode, stage
# the files under a FHS tree, compute dependencies, and call dpkg-deb.
#
#   scripts/build-deb.sh
#
# Output: target/deb/linuxscp_<version>_<arch>.deb
set -euo pipefail

cd "$(dirname "$0")/.."

APP_ID="io.github.theflyingjay.LinuxSCP"
PKG="linuxscp"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${DEB_MAINTAINER:-Jacob Petrosky <theflyingjay@gmail.com>}"

# Resolve the real target directory (respects CARGO_TARGET_DIR and configs).
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
TARGET_DIR="${TARGET_DIR:-target}"
RELEASE="$TARGET_DIR/release"

STAGE="$TARGET_DIR/deb/${PKG}_${VERSION}_${ARCH}"
OUT="$TARGET_DIR/deb/${PKG}_${VERSION}_${ARCH}.deb"

echo ">> Building release binaries"
cargo build --release --all

echo ">> Verifying transparent app icons"
for png in "data/$APP_ID.png" data/icons/hicolor/*/apps/"$APP_ID.png"; do
    # PNG IHDR byte 25 is the color type: 6 means truecolor with alpha.
    COLOR_TYPE="$(od -An -t u1 -j 25 -N 1 "$png" | tr -d ' ')"
    if [ "$COLOR_TYPE" != "6" ]; then
        echo "error: $png is not an RGBA PNG (PNG color type: $COLOR_TYPE)" >&2
        exit 1
    fi
done

echo ">> Validating AppStream metadata"
if command -v appstreamcli >/dev/null 2>&1; then
    # --no-net: the GitHub URLs may not exist yet; that must not fail builds.
    appstreamcli validate --no-net "data/$APP_ID.metainfo.xml"
else
    echo "   appstreamcli not found, skipping"
fi

echo ">> Staging files in $STAGE"
rm -rf "$STAGE"
install -Dm755 "$RELEASE/linuxscp"              "$STAGE/usr/bin/linuxscp"
install -Dm755 "$RELEASE/linuxscp-askpass"      "$STAGE/usr/bin/linuxscp-askpass"
install -Dm644 "data/$APP_ID.desktop"           "$STAGE/usr/share/applications/$APP_ID.desktop"
install -Dm644 "data/$APP_ID.metainfo.xml"      "$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"

# Ship every rendered icon size so the launcher, dock and window all look sharp.
for png in data/icons/hicolor/*/apps/"$APP_ID.png"; do
    dir="$(basename "$(dirname "$(dirname "$png")")")"
    install -Dm644 "$png" "$STAGE/usr/share/icons/hicolor/$dir/apps/$APP_ID.png"
done

# Transfer-completion sound.
install -Dm644 data/sounds/success.mp3 "$STAGE/usr/share/linuxscp/sounds/success.mp3"

# Debian policy: copyright file, plus a changelog.
install -Dm644 LICENSE "$STAGE/usr/share/doc/$PKG/copyright"
{
    echo "$PKG ($VERSION) unstable; urgency=medium"
    echo
    echo "  * Release $VERSION."
    echo
    echo " -- ${MAINTAINER}  $(date -R)"
} | gzip -9n > /tmp/changelog.Debian.gz
install -Dm644 /tmp/changelog.Debian.gz "$STAGE/usr/share/doc/$PKG/changelog.Debian.gz"
rm -f /tmp/changelog.Debian.gz

# Dependencies: derive library deps from the binaries when possible, then add
# the runtime tools we shell out to.
DEPS="libgtk-4-1 (>= 4.6), libadwaita-1-0 (>= 1.2)"
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
    echo ">> Computing library dependencies with dpkg-shlibdeps"
    SHLIB_TMP="$(mktemp -d)"
    mkdir -p "$SHLIB_TMP/debian"
    touch "$SHLIB_TMP/debian/control"
    BIN1="$(readlink -f "$STAGE/usr/bin/linuxscp")"
    BIN2="$(readlink -f "$STAGE/usr/bin/linuxscp-askpass")"
    if ( cd "$SHLIB_TMP" && dpkg-shlibdeps -O --ignore-missing-info \
            "$BIN1" "$BIN2" 2>/dev/null ) > "$SHLIB_TMP/out"; then
        COMPUTED="$(sed -n 's/^shlibs:Depends=//p' "$SHLIB_TMP/out")"
        [ -n "$COMPUTED" ] && DEPS="$COMPUTED"
    fi
    rm -rf "$SHLIB_TMP"
fi
DEPS="$DEPS, openssh-client"

INSTALLED_KB="$(du -ks "$STAGE" | cut -f1)"

echo ">> Writing control files"
mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $INSTALLED_KB
Depends: $DEPS
Section: net
Priority: optional
Homepage: https://github.com/theflyingjay/linuxscp
Description: Commander-style SFTP client for GNOME
 LinuxSCP is a native GTK4/libadwaita SFTP client with a dual-pane,
 keyboard-driven layout for people moving from Windows and WinSCP to Linux.
 It works directly with ~/.ssh/config by driving the system OpenSSH, supports
 sudo/su elevation for root file management, and offers resumable transfers.
EOF

# Refresh the icon cache and desktop database so the icon and launcher appear
# immediately after install/removal.
cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications || true
    fi
fi
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications || true
    fi
fi
EOF
chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/postrm"

echo ">> Building package"
# Root-owned files inside the archive via the fakeroot mapping.
if command -v fakeroot >/dev/null 2>&1; then
    fakeroot dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
else
    dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
fi

echo
echo "Built: $OUT"
echo "Install with: sudo apt install $OUT   (or: sudo dpkg -i $OUT)"
