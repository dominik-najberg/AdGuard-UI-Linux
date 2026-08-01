#!/bin/bash
#
# Build adguard-ui_<version>_<arch>.deb from a hand-assembled tree.
#
# `dpkg-deb -b` and nothing else — `debhelper` is not installed on the reference
# machine and this deliberately does not need it (docs/building.md §5). Nothing
# here needs root: `fakeroot` is what lets the tree be recorded as root-owned
# without any of it actually being so.
#
# Usage: packaging/deb.sh [output directory]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/target/package}"
NAME=adguard-ui
ARCH="$(dpkg --print-architecture)"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "deb.sh: could not read the version out of Cargo.toml" >&2; exit 1; }

# The maintainer comes from git rather than from a constant, so a fork's
# packages are not attributed to this repository's author.
MAINTAINER_NAME="$(git -C "$REPO" config user.name || true)"
MAINTAINER_EMAIL="$(git -C "$REPO" config user.email || true)"
MAINTAINER="${MAINTAINER_NAME:-unknown} <${MAINTAINER_EMAIL:-unknown@invalid}>"

TREE="$OUT/${NAME}_${VERSION}_${ARCH}"
rm -rf "$TREE"
mkdir -p "$TREE/DEBIAN" "$OUT"

echo "deb.sh: building $NAME $VERSION for $ARCH"
cargo build --release --manifest-path "$REPO/Cargo.toml"

# --- payload ---------------------------------------------------------------
#
# The /usr mapping of docs/building.md §4's ~/.local layout, one for one, plus
# the AppStream file §4 has no reason to install and a package does: without it
# GNOME Software shows the app with no description at all.

install -Dm755 "$REPO/target/release/$NAME" "$TREE/usr/bin/$NAME"
# Stripped here rather than through a `[profile.release]` in Cargo.toml: the
# packaging step is where the 2.4 MB of Rust symbol names stop being useful, and
# a backtrace from a `cargo build` is worth keeping for everyone else.
strip --strip-unneeded "$TREE/usr/bin/$NAME"

install -Dm644 "$REPO/data/io.github.dominik-najberg.AdGuardUI.desktop" \
    "$TREE/usr/share/applications/io.github.dominik-najberg.AdGuardUI.desktop"
install -Dm644 "$REPO/data/io.github.dominik-najberg.AdGuardUI.metainfo.xml" \
    "$TREE/usr/share/metainfo/io.github.dominik-najberg.AdGuardUI.metainfo.xml"

install -Dm644 -t "$TREE/usr/share/icons/hicolor/scalable/apps" \
    "$REPO"/data/icons/hicolor/scalable/apps/*.svg
install -Dm644 -t "$TREE/usr/share/icons/hicolor/symbolic/apps" \
    "$REPO"/data/icons/hicolor/symbolic/apps/*.svg
for dir in "$REPO"/data/icons/hicolor/*x*/apps; do
    size="$(basename "$(dirname "$dir")")"
    install -Dm644 -t "$TREE/usr/share/icons/hicolor/$size/apps" "$dir"/*.png
done

# The autostart entry is an *example*, not a launcher. Installed among the
# applications it would show up in the app grid as a second, windowless entry
# (building.md §4); installed into /etc/xdg/autostart it would start the tray at
# login for every user of the machine, which is a decision for whoever runs it
# and not for whoever packages it.
install -Dm644 "$REPO/data/autostart/io.github.dominik-najberg.AdGuardUI.desktop" \
    "$TREE/usr/share/doc/$NAME/examples/autostart/io.github.dominik-najberg.AdGuardUI.desktop"

install -Dm644 /dev/stdin "$TREE/usr/share/doc/$NAME/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: $NAME
Source: https://github.com/dominik-najberg/AdGuard-UI-Linux

Files: *
Copyright: $MAINTAINER
License: GPL-3+
 This program is free software: you can redistribute it and/or modify it
 under the terms of the GNU General Public License as published by the Free
 Software Foundation, either version 3 of the License, or (at your option)
 any later version.
 .
 This program is distributed in the hope that it will be useful, but WITHOUT
 ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
 FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
 more details.
 .
 On Debian systems, the complete text of the GNU General Public License
 version 3 can be found in "/usr/share/common-licenses/GPL-3".
EOF

# --- dependencies ----------------------------------------------------------
#
# Derived, not written down. `dpkg-shlibdeps` reads the binary's DT_NEEDED set
# and each providing package's `.symbols` file, which gives the *symbol-level*
# minimum — `libc6 (>= 2.39)` for this binary, where copying the build machine's
# installed version would have said 2.43 and refused to install on four years of
# perfectly capable systems.
#
# It insists on a `debian/control` relative to the working directory and reads
# nothing but the package name out of it, so it gets a two-line stub. `-O`
# prints to stdout instead of appending to `debian/substvars`, which is what
# makes it usable without a `debian/` build tree at all.
DEPENDS=""
if command -v dpkg-shlibdeps >/dev/null; then
    STUB="$OUT/shlibdeps"
    mkdir -p "$STUB/debian"
    printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "$NAME" "$NAME" > "$STUB/debian/control"
    # No `--ignore-missing-info`: every library this binary needs comes from a
    # package that ships shlibs data, and the flag's only effect would be to
    # turn a genuinely undeclarable dependency into a silent omission. It warns
    # about the usr-merge diversion of ld-linux either way; that one is noise.
    DEPENDS="$(cd "$STUB" && dpkg-shlibdeps -O "$TREE/usr/bin/$NAME" \
        | sed -n 's/^shlibs:Depends=//p')"
    rm -rf "$STUB"
fi
if [ -z "$DEPENDS" ]; then
    # dpkg-dev absent, or the .symbols files were not there to read. The shlibs
    # fallback for the same DT_NEEDED set, which over-constrains libc but is
    # never wrong in the direction that matters.
    echo "deb.sh: dpkg-shlibdeps unavailable — falling back to a written list" >&2
    DEPENDS="libadwaita-1-0, libgtk-4-1, libglib2.0-0t64 | libglib2.0-0, libc6, libgcc-s1"
fi

# --- control ---------------------------------------------------------------
#
# No `Depends: adguard-cli`. There is no such package — AdGuard CLI is a
# third-party install under $HOME, and naming it would make this .deb
# uninstallable everywhere (building.md §5). The requirement is declared in the
# description instead, and the binary already reports it at runtime rather than
# crashing: `paths::cli_binary` returns None and `missing_cli_view` explains it.
SIZE="$(du -ks "$TREE" | cut -f1)"
cat > "$TREE/DEBIAN/control" <<EOF
Package: $NAME
Version: $VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Section: net
Priority: optional
Homepage: https://github.com/dominik-najberg/AdGuard-UI-Linux
Installed-Size: $SIZE
Depends: $DEPENDS
Recommends: hicolor-icon-theme, desktop-file-utils
Description: GTK4 desktop front-end for AdGuard CLI
 A GTK4 and libadwaita interface for controlling AdGuard CLI on Linux:
 start and stop the filtering proxy, manage filter lists, and configure
 protection settings without using the terminal.
 .
 This is an unofficial, community-built interface. It requires AdGuard CLI
 to be installed separately, from AdGuard, and is not affiliated with or
 endorsed by AdGuard.
EOF

# No maintainer scripts. dpkg's own file triggers already refresh both caches
# this package touches — hicolor-icon-theme on /usr/share/icons/hicolor and
# desktop-file-utils on /usr/share/applications — so a postinst calling
# gtk-update-icon-cache would duplicate work dpkg has done since 2009.

# --- build -----------------------------------------------------------------
#
# `chown` inside fakeroot, so data.tar records 0/0 for every path. Without it
# every file in /usr would be installed owned by whoever built the package.
fakeroot bash -c "chown -R root:root '$TREE' && dpkg-deb --build --root-owner-group '$TREE' '$OUT'" >/dev/null

DEB="$OUT/${NAME}_${VERSION}_${ARCH}.deb"
rm -rf "$TREE"
echo "deb.sh: $DEB"
dpkg-deb --info "$DEB" | sed -n '2,6p'
