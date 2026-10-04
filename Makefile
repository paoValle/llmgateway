.DEFAULT_GOAL := help
.PHONY: help setup dev test lint fmt bench ci

help: ## mostra questo aiuto
	@grep -E '^[a-z-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## scarica le dipendenze
	cargo fetch

dev: ## esempio eseguibile
	cargo run --example demo

test: ## suite completa
	cargo test --all-targets

lint: ## clippy, i warning sono errori
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## scrive i file
	cargo fmt

bench: ## benchmark
	cargo bench

ci: fmt lint test ## esattamente quello che gira in CI
