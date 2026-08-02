#!/bin/bash
#
# Build adguard-ui-<version>-<arch>.tar.gz: the same payload as the .deb, laid
# out for a per-user install under ~/.local instead of /usr.
#
# The two are for different things. A .deb is for a machine you administer; this
# is for one you do not, or for carrying a build to a second machine of your own
# — which is the case docs/building.md §5 calls "tarball for personal use". The
# install script inside it is docs/building.md §4, verbatim in effect, so there
# is one description of where these files go and not two.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/target/package}"
NAME=adguard-ui
ARCH="$(uname -m)"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | sed -n '1p')"
[ -n "$VERSION" ] || { echo "tarball.sh: could not read the version out of Cargo.toml" >&2; exit 1; }

STAGE="$OUT/$NAME-$VERSION-$ARCH"
rm -rf "$STAGE"
mkdir -p "$STAGE" "$OUT"

echo "tarball.sh: building $NAME $VERSION for $ARCH"
cargo build --release --manifest-path "$REPO/Cargo.toml"

install -Dm755 "$REPO/target/release/$NAME" "$STAGE/bin/$NAME"
strip --strip-unneeded "$STAGE/bin/$NAME"
cp -r "$REPO/data" "$STAGE/data"

# The .deb points its copyright file at /usr/share/common-licenses/GPL-3, which
# Debian policy provides for and a tarball has no equivalent of: this one may be
# carried to a machine that has no such directory. GPL-3.0-or-later §4 wants a
# copy of the licence conveyed with the program, so it travels in the payload.
install -Dm644 "$REPO/LICENSE" "$STAGE/LICENSE"

install -Dm644 /dev/stdin "$STAGE/README" <<EOF
AdGuard UI $VERSION ($ARCH)

A GTK4 front-end for AdGuard CLI. It does not include AdGuard CLI and will
not work without it: install that from AdGuard first, then run ./install.sh
to put this in ~/.local. Nothing here needs root.

The application looks for adguard-cli on \$PATH, then in ~/.local/bin and
~/.local/opt/adguard-cli, and says so plainly if it finds none.

Uninstall by deleting the files install.sh reports, or run it with --list to
see them without installing anything.
EOF

# Kept in step with docs/building.md §4 by being the same commands. The two
# `install -t` flags and the `-t` on gtk-update-icon-cache are all load-bearing
# and all explained there.
install -Dm755 /dev/stdin "$STAGE/install.sh" <<'EOF'
#!/bin/bash
# Install AdGuard UI into ~/.local. See docs/building.md §4 for why each line is
# the shape it is. Nothing here needs root.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"

FILES=(
    "$PREFIX/bin/adguard-ui"
    "$PREFIX/share/applications/io.github.dominik-najberg.AdGuardUI.desktop"
    "$PREFIX/share/icons/hicolor/{scalable,symbolic,<size>}/apps/io.github.dominik-najberg.AdGuardUI.*"
)
if [ "${1:-}" = "--list" ]; then
    printf '%s\n' "${FILES[@]}"
    echo "$HOME/.config/autostart/io.github.dominik-najberg.AdGuardUI.desktop  (only with --autostart)"
    exit 0
fi

install -Dm755 "$HERE/bin/adguard-ui" "$PREFIX/bin/adguard-ui"
# `-t` on every directory destination, including this one. docs/building.md §4
# gets away without it because ~/.local/share/applications already exists on a
# desktop system; a fresh $PREFIX is the case where `install -D` refuses,
# because it only creates leading directories when the destination is a file.
install -Dm644 -t "$PREFIX/share/applications" "$HERE"/data/*.desktop
install -Dm644 -t "$PREFIX/share/icons/hicolor/scalable/apps" "$HERE"/data/icons/hicolor/scalable/apps/*.svg
install -Dm644 -t "$PREFIX/share/icons/hicolor/symbolic/apps" "$HERE"/data/icons/hicolor/symbolic/apps/*.svg
for d in "$HERE"/data/icons/hicolor/*x*/apps; do
    install -Dm644 -t "$PREFIX/share/icons/hicolor/$(basename "$(dirname "$d")")/apps" "$d"/*.png
done

# `-t` here is --ignore-theme-index, not a target directory: ~/.local/share/icons
# /hicolor has no index.theme and never will.
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true

# Separate directory, deliberately: installed among the launchers this shows up
# in the app grid as a second, windowless entry.
if [ "${1:-}" = "--autostart" ]; then
    install -Dm644 -t "$HOME/.config/autostart" "$HERE"/data/autostart/*.desktop
    echo "installed the autostart entry — the tray will start at login"
fi

echo "installed into $PREFIX. Run: adguard-ui"
EOF

tar -czf "$OUT/$NAME-$VERSION-$ARCH.tar.gz" -C "$OUT" "$NAME-$VERSION-$ARCH"
rm -rf "$STAGE"
echo "tarball.sh: $OUT/$NAME-$VERSION-$ARCH.tar.gz"
# `sed -n '1,6p'` rather than `head -6`, and it is not style. `head` closes the
# pipe the moment it has its six lines; under `set -o pipefail` a `tar` that has
# not finished writing by then takes SIGPIPE and the pipeline returns 141, which
# fails the release build after every artefact has already been produced
# correctly. It is a race, so it passes on a fast machine and on a warm cache and
# then fails in the container — measured 2 August 2026, on the v1.1.0 tag. `sed`
# reads its input to the end.
tar -tzf "$OUT/$NAME-$VERSION-$ARCH.tar.gz" | sed -n '1,6p'
