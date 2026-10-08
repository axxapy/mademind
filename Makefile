.PHONY: help bootstrap test lint fmt build run dist clean
.DEFAULT_GOAL := help

CARGO_TARGET_DIR ?= target
BIN := mademind

help:
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {sub("\\n",sprintf("\n%22c"," "), $$2);printf "\033[36m%-25s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

bootstrap: ## Fetch dependencies
	cargo fetch

test: ## Run tests
	cargo test

lint: ## Run clippy (warnings as errors)
	cargo clippy --all-targets -- -D warnings

fmt: ## Check formatting
	cargo fmt --check

build: ## Build release binary (bin path: $(CARGO_TARGET_DIR)/release/$(BIN))
	cargo build --release

# Local run with ./config.toml; index and models go to ./.cache. Collection
# paths in config.toml must exist on this machine.
run: ## Run locally (./config.toml, index + models in ./.cache)
	MADEMIND_CONFIG=config.toml MADEMIND_CACHE_DIR=.cache cargo run

# TARGETS="linux-x86_64-gnu linux-x86_64-musl" limits the platforms. From
# Linux, the four linux-* targets build (docker + zig); macOS and Windows
# build on their own runners in .github/workflows/release.yml.
dist: ## Build release archives into dist/ (see scripts/dist.sh)
	TARGETS="$(TARGETS)" scripts/dist.sh

clean: ## Clean build artifacts
	cargo clean
