.PHONY: help build test fmt lint run install clean
.DEFAULT_GOAL := help
.NOTPARALLEL:

ARGS ?= --help

help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort -t: -k1,1 | \
		awk 'BEGIN {FS = ":.*?## "; w = 12} { k[NR] = $$1; v[NR] = $$2; if (length($$1) > w) w = length($$1) } \
		     END { for (i = 1; i <= NR; i++) printf "  \033[36m%-*s\033[0m  %s\n", w, k[i], v[i] }'

fmt: ## Format Rust sources
	cargo fmt --all

lint: ## Run clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

build: fmt lint ## Format, lint, then build the release binary
	cargo build --release

test: ## Run tests
	cargo test

run: ## Run goniec with ARGS (default --help)
	cargo run -- $(ARGS)

install: ## Install the goniec binary with cargo
	cargo install --path .

clean: ## Remove build artifacts
	cargo clean
