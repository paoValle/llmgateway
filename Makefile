# `cargo` è lo strumento nativo e basta: su Windows `make` spesso non esiste,
# su GitHub Actions su Linux esiste. Il Makefile è qui per chi lo preferisce, e
# non definisce un solo comando che `cargo` non abbia già.
.DEFAULT_GOAL := help
.PHONY: help setup build test lint fmt fmt-check ci doc

help: ## mostra questo aiuto
	@grep -E '^[a-z-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## scarica le dipendenze
	cargo fetch --locked

build: ## compila
	cargo build

test: ## suite completa
	cargo test --all-targets

lint: ## clippy, i warning sono errori
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## scrive i file
	cargo fmt

fmt-check: ## verifica la formattazione senza scrivere
	cargo fmt --check

doc: ## apre la documentazione
	cargo doc --open

ci: fmt-check lint test ## esattamente quello che gira in CI
