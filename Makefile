# Convenience wrappers over the commands in docs/building.md.
#
# Cargo is still the build system (architecture.md §1) and the first two targets
# do no work of their own — `make build` and `cargo build --workspace` are the
# same thing.
#
# The two packaging targets are the exception, and they earn it: assembling a
# .deb is a dozen steps with a `fakeroot` and a `dpkg-shlibdeps` in the middle,
# which is more than a doc block can carry as copy-pasteable commands. They
# still do not do the *explaining* — that is building.md §5, and the scripts in
# `packaging/` say why each step is the shape it is.
#
# `install` is the only target that touches the machine outside this checkout,
# and the only one that asks for a password. It is a wrapper too — `deb` plus
# the one apt command that would otherwise be typed with the version number
# spelled out by hand — with one piece of cleanup that is not decoration, and
# `uninstall-local` beneath it says why.

ARGS ?=

# Lazy on purpose: `=`, not `:=`. A bare `make` would otherwise shell out to
# `sed` and `dpkg` to answer a question nobody asked, since only `install`
# needs to name the file that `packaging/deb.sh` writes. The version is read
# the same way the script reads it, from the one place it is written down.
NAME = adguard-ui
VERSION = $(shell sed -n 's/^version = "\(.*\)"$$/\1/p' Cargo.toml | head -1)
DEB_ARCH = $(shell dpkg --print-architecture)
DEB = target/package/$(NAME)_$(VERSION)_$(DEB_ARCH).deb

# What a per-user install writes, which is what `uninstall-local` removes. The
# same list the tarball's `install.sh --list` prints, because it is the same
# install — building.md §4 as a script. The `*` are for the shell, not for make:
# a `$(wildcard)` would be evaluated when this file is read, and the point of
# the list is what exists at the moment the target runs.
APP_ID = io.github.dominik-najberg.AdGuardUI
LOCAL_PREFIX ?= $(HOME)/.local
LOCAL_FILES = $(LOCAL_PREFIX)/bin/$(NAME) \
              $(LOCAL_PREFIX)/share/applications/$(APP_ID).desktop \
              $(LOCAL_PREFIX)/share/icons/hicolor/*/apps/$(APP_ID)*.png \
              $(LOCAL_PREFIX)/share/icons/hicolor/*/apps/$(APP_ID)*.svg

.DEFAULT_GOAL := help
.PHONY: help build run deb tarball package install uninstall-local check-path

# A bare `make` says what there is rather than guessing which of these was
# meant. The listing is derived from the `##` comments below, so it cannot
# drift from the targets: each one documents itself on the line that defines
# it, and a target added without a comment is one that does not appear.
help:
	@echo "Targets (docs/building.md §2 has the rest, §5 the packaging ones):"
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-15s %s\n", $$1, $$2}'

build: ## cargo build --workspace
	cargo build --workspace

run: ## cargo run -p adguard-gui — add ARGS=--background to start into the tray
	cargo run -p adguard-gui -- $(ARGS)

deb: ## build target/package/adguard-ui_<version>_<arch>.deb (no root needed)
	packaging/deb.sh

tarball: ## build target/package/adguard-ui-<version>-<arch>.tar.gz for ~/.local
	packaging/tarball.sh

package: deb tarball ## both of the above

# The one target that needs root, and it needs it for exactly one command.
# Building the package stays unprivileged (building.md §5) — `deb` runs as you,
# and only the install step is handed to `sudo`, so a build failure never
# happens under a root shell.
#
# `apt-get install ./file.deb` rather than `dpkg -i`: both unpack the same
# archive, but apt resolves the `Depends:` line deb.sh derived, where dpkg
# leaves the package unconfigured with a "dependency problems" error and hands
# you an `apt-get -f install` to run yourself. The path is absolute because
# that is how apt tells a local file from a package name — a bare
# `adguard-ui_0.1.0_amd64.deb` is looked up in the archive and not found.
install: deb ## build the .deb and install it system-wide (asks for sudo)
	@test "$$(id -u)" != 0 || { echo "Makefile: run this as yourself, not under sudo — it sudoes the one command that needs it" >&2; echo "Makefile: under \`sudo make\` the build runs as root and \$$HOME is root's, so the check below would look at the wrong home directory" >&2; exit 1; }
	@test -f "$(DEB)" || { echo "Makefile: $(DEB) is not what deb.sh built — check the version in Cargo.toml" >&2; exit 1; }
	sudo apt-get install -y $(CURDIR)/$(DEB)
	@$(MAKE) --no-print-directory uninstall-local
	@$(MAKE) --no-print-directory check-path

# The failure this exists to prevent: `make install` succeeds, apt reports the
# package unpacked, /usr/bin/adguard-ui is the new build — and the old one keeps
# launching, because a per-user install from the tarball route sits earlier on
# $PATH and both `.desktop` files run a bare `Exec=adguard-ui`. Nothing about
# it looks like a failure. dpkg is content, the file on disk is correct, and the
# window that opens is weeks old. The version number cannot help: the two
# installs are not two versions of a package, they are two files with the same
# name, and only one of them is a package at all.
#
# So `install` removes the other one rather than warning about it. The two
# routes are alternatives — building.md §5 offers the tarball as what to use
# when you *cannot* install a .deb — and keeping both is not a configuration
# anyone wants, it is the bug above. It runs after apt, never before: an apt
# that fails should leave you with the install you had.
#
# Loud, never silent. Every path it touches is named, and all of them belong to
# this application alone, so there is nothing here to lose by accident. Re-run
# the tarball's `install.sh` to put it back.
uninstall-local: ## remove a per-user ~/.local install (it would shadow the package)
	@found=""; \
	for f in $(LOCAL_FILES); do \
	    if [ -e "$$f" ]; then found="$$found $$f"; fi; \
	done; \
	if [ -n "$$found" ]; then \
	    echo "Makefile: removing the per-user install under $(LOCAL_PREFIX), which would shadow the package:"; \
	    for f in $$found; do echo "  $$f"; rm -f "$$f"; done; \
	    gtk-update-icon-cache -f -t "$(LOCAL_PREFIX)/share/icons/hicolor" 2>/dev/null || true; \
	    update-desktop-database "$(LOCAL_PREFIX)/share/applications" 2>/dev/null || true; \
	fi

# And then check, because `uninstall-local` only knows the one prefix. Anything
# else on $PATH — a `cargo install`, a copy in /usr/local/bin, a second machine's
# habits — shadows the package exactly as well, and this is the one moment where
# saying so costs nothing and saves the afternoon it cost to find it the first
# time.
#
# `-ef` and not `=`: on a usr-merged system $PATH may resolve the package's own
# binary through /bin, which is a symlink to /usr/bin and therefore the same
# file. A string comparison would report the correct install as a shadowed one.
#
# It exits non-zero. An install that will not be what runs has not installed
# anything, whatever apt said about it.
check-path:
	@have="$$(command -v $(NAME) 2>/dev/null || true)"; \
	if [ -z "$$have" ]; then \
	    echo "Makefile: installed /usr/bin/$(NAME), but it is not on \$$PATH — check that /usr/bin is in it" >&2; \
	    exit 1; \
	fi; \
	if [ ! "$$have" -ef "/usr/bin/$(NAME)" ]; then \
	    echo "Makefile: installed /usr/bin/$(NAME), but \$$PATH resolves $(NAME) to $$have" >&2; \
	    echo "Makefile: that file is what will launch, including from the desktop entry, which runs a bare \`$(NAME)\`." >&2; \
	    echo "Makefile: remove it, or move /usr/bin ahead of it in \$$PATH." >&2; \
	    exit 1; \
	fi; \
	echo "Makefile: $(NAME) resolves to $$have"
