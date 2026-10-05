# `cargo` is the native tool and that is enough: on Windows `make` often does not
# exist, on GitHub Actions on Linux it does. The Makefile is here for whoever prefers
# it, and it does not define a single command `cargo` does not already have.
.DEFAULT_GOAL := help
.PHONY: help setup build test lint fmt fmt-check ci doc

help: ## show this help
	@grep -E '^[a-z-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## fetch dependencies
	cargo fetch --locked

build: ## compile
	cargo build

test: ## full suite
	cargo test --all-targets

lint: ## clippy, warnings are errors
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## writes the files
	cargo fmt

fmt-check: ## checks formatting without writing
	cargo fmt --check

doc: ## opens the documentation
	cargo doc --open

ci: fmt-check lint test ## exactly what runs in CI
