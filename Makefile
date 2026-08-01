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

ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help build run deb tarball package

# A bare `make` says what there is rather than guessing which of these was
# meant. The listing is derived from the `##` comments below, so it cannot
# drift from the targets: each one documents itself on the line that defines
# it, and a target added without a comment is one that does not appear.
help:
	@echo "Targets (docs/building.md §2 has the rest, §5 the packaging ones):"
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-8s %s\n", $$1, $$2}'

build: ## cargo build --workspace
	cargo build --workspace

run: ## cargo run -p adguard-gui — add ARGS=--background to start into the tray
	cargo run -p adguard-gui -- $(ARGS)

deb: ## build target/package/adguard-ui_<version>_<arch>.deb (no root needed)
	packaging/deb.sh

tarball: ## build target/package/adguard-ui-<version>-<arch>.tar.gz for ~/.local
	packaging/tarball.sh

package: deb tarball ## both of the above
