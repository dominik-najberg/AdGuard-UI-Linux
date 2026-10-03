#!/bin/bash
#
# Build adguard-ui-<version>-1.<arch>.rpm for Fedora, from the same payload as
# the .deb.
#
# `rpmbuild -bb` on a spec this script writes, and the spec builds nothing: the
# binary is compiled and stripped out here, exactly as deb.sh does it, and
# `%install` only copies the tree below into the buildroot. A spec that ran
# `cargo build` itself would need the toolchain inside rpmbuild's environment
# and would be a second description of the build; this way there is one, and
# the two packages cannot drift apart in what they contain. Nothing here needs
# root — rpmbuild records every file as root-owned through `%defattr` without
# any of it being so, which is what `--root-owner-group` does for dpkg-deb.
#
# Usage: packaging/rpm.sh [output directory]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/target/package}"
NAME=adguard-ui
ARCH="$(rpm --eval '%{_arch}')"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | sed -n '1p')"
[ -n "$VERSION" ] || { echo "rpm.sh: could not read the version out of Cargo.toml" >&2; exit 1; }

# From git, with the last commit's author as the fallback a CI checkout needs —
# deb.sh says why at length, and the reason is the same one: this is the field a
# user reads to find out who to report a bug to.
MAINTAINER_NAME="$(git -C "$REPO" config user.name || true)"
MAINTAINER_EMAIL="$(git -C "$REPO" config user.email || true)"
if [ -z "$MAINTAINER_NAME" ] && [ -z "$MAINTAINER_EMAIL" ]; then
    MAINTAINER="$(git -C "$REPO" log -1 --format='%an <%ae>' 2>/dev/null || true)"
fi
MAINTAINER="${MAINTAINER:-${MAINTAINER_NAME:-unknown} <${MAINTAINER_EMAIL:-unknown@invalid}>}"

TOP="$OUT/rpmbuild"
TREE="$TOP/SOURCES/tree"
rm -rf "$TOP"
mkdir -p "$TREE" "$TOP/SPECS" "$OUT"

echo "rpm.sh: building $NAME $VERSION for $ARCH"
cargo build --release --manifest-path "$REPO/Cargo.toml"

# --- payload ---------------------------------------------------------------
#
# deb.sh's payload, path for path, so that a user moving between the two
# distributions finds the same files in the same places. The one addition is
# the licence, which a .deb points at in /usr/share/common-licenses and an .rpm
# carries itself: Fedora has no shared copy to point at.

install -Dm755 "$REPO/target/release/$NAME" "$TREE/usr/bin/$NAME"
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

# An example and not a launcher, for deb.sh's reason: in /etc/xdg/autostart it
# would start the tray at login for every user of the machine.
install -Dm644 "$REPO/data/autostart/io.github.dominik-najberg.AdGuardUI.desktop" \
    "$TREE/usr/share/doc/$NAME/examples/autostart/io.github.dominik-najberg.AdGuardUI.desktop"

install -Dm644 "$REPO/LICENSE" "$TREE/usr/share/licenses/$NAME/LICENSE"

# --- spec ------------------------------------------------------------------
#
# **No `Requires:` at all**, and that is the derived kind, not the forgotten
# kind. rpmbuild's ELF dependency generator reads the binary's DT_NEEDED set
# and the symbol versions it binds against, and writes `libgtk-4.so.1()(64bit)`,
# `libc.so.6(GLIBC_2.xx)(64bit)` and the rest by itself — the symbol-level
# minimum, which is what deb.sh needs `dpkg-shlibdeps` and a stub `debian/`
# directory to get. It runs on every build and has no failure mode that falls
# back to a written list, so there is no list here to age.
#
# No `Requires: adguard-cli`: there is no such package, for the reason deb.sh
# gives — naming it would make this .rpm uninstallable everywhere.
#
# **No `%{?dist}` in the release.** A dist tag (`.fc43`) says the package was
# built for one Fedora release, and this one is built once, against the oldest
# release the workflow supports, for all of them: the generated glibc
# requirement is what decides where it installs, and it says so precisely. A
# `.fc43` in the filename would tell a Fedora 44 user the opposite.
#
# `debug_package %{nil}` because the binary arrives already built: rpmbuild's
# find-debuginfo would extract nothing useful from a stripped file and then
# fail the build over an empty debugsource list.
#
# No scriptlets. Fedora's file triggers refresh the icon cache and the desktop
# database on anything landing in /usr/share/icons and /usr/share/applications,
# as dpkg's do on Debian.
#
# No `%changelog`: CHANGELOG.md is the one changelog, as for the .deb.
#
# The `%dir` above `%license` is not redundant. `%license` given an absolute
# path marks that one file and owns nothing around it, so without the line
# `dnf remove` leaves an empty /usr/share/licenses/adguard-ui behind — measured
# in a fedora:43 container, 3 October 2026.
cat > "$TOP/SPECS/$NAME.spec" <<EOF
%global debug_package %{nil}

Name:           $NAME
Version:        $VERSION
Release:        1
Summary:        GTK4 desktop front-end for AdGuard CLI
License:        GPL-3.0-or-later
URL:            https://github.com/dominik-najberg/AdGuard-UI-Linux
Packager:       $MAINTAINER
Recommends:     hicolor-icon-theme

%description
A GTK4 and libadwaita interface for controlling AdGuard CLI on Linux:
start and stop the filtering proxy, manage filter lists, and configure
protection settings without using the terminal.

This is an unofficial, community-built interface. It requires AdGuard CLI
to be installed separately, from AdGuard, and is not affiliated with or
endorsed by AdGuard.

%install
cp -a %{_sourcedir}/tree/. %{buildroot}/

%files
%{_bindir}/$NAME
%{_datadir}/applications/io.github.dominik-najberg.AdGuardUI.desktop
%{_datadir}/metainfo/io.github.dominik-najberg.AdGuardUI.metainfo.xml
%{_datadir}/icons/hicolor/*/apps/io.github.dominik-najberg.AdGuardUI*
%dir %{_docdir}/$NAME
%{_docdir}/$NAME/examples
%dir %{_datadir}/licenses/$NAME
%license %{_datadir}/licenses/$NAME/LICENSE
EOF

# --- build -----------------------------------------------------------------
#
# `_topdir` keeps every rpmbuild directory inside target/, where a stock
# rpmbuild would create ~/rpmbuild and leave it behind. `_rpmdir` and
# `_build_name_fmt` put the file straight into $OUT under its plain name,
# without the per-arch subdirectory rpmbuild makes by default.
#
# `source_date_epoch_from_changelog 0` because there is no `%changelog` to take
# a date from, and Fedora's macros look for one and warn on every build when
# they find none.
rpmbuild -bb --quiet \
    --define "_topdir $TOP" \
    --define "source_date_epoch_from_changelog 0" \
    --define "_rpmdir $OUT" \
    --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
    "$TOP/SPECS/$NAME.spec"

RPM="$OUT/$NAME-$VERSION-1.$ARCH.rpm"
rm -rf "$TOP"
echo "rpm.sh: $RPM"
# `sed -n` rather than `head`, for tarball.sh's reason: a reader that stops
# early hands its writer a SIGPIPE, and under `pipefail` that fails the build.
rpm -qpi "$RPM" | sed -n '1,4p'
echo "rpm.sh: requires"
rpm -qpR "$RPM" | sed 's/^/  /'
