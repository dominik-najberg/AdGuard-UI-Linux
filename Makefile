# Convenience wrappers over the two commands in docs/building.md §2.
#
# Cargo is still the build system (architecture.md §1) and nothing here does
# work of its own — `make build` and `cargo build --workspace` are the same
# thing. Anything past these two belongs in the docs, where it can be explained.

ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help build run

# A bare `make` says what there is rather than guessing which of these was
# meant. The listing is derived from the `##` comments below, so it cannot
# drift from the targets: each one documents itself on the line that defines
# it, and a target added without a comment is one that does not appear.
help:
	@echo "Targets (docs/building.md §2 has the rest):"
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-6s %s\n", $$1, $$2}'

build: ## cargo build --workspace
	cargo build --workspace

run: ## cargo run -p adguard-gui — add ARGS=--background to start into the tray
	cargo run -p adguard-gui -- $(ARGS)
