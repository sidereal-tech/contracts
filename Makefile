# SPDX-License-Identifier: Apache-2.0
# Convenience targets for the sidereal contracts repo.

.PHONY: help test wasm build deploy seed clean testnet-amm-routes

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

test: ## Run all Soroban contract tests
	cargo test --workspace

wasm: ## Build optimized deployable contract Wasm artifacts
	bash scripts/build-optimized-wasm.sh

build: wasm ## Build the contracts (wasm)

deploy: ## Deploy the protocol to testnet
	bash scripts/deploy-testnet-resilient.sh

seed: ## Seed the deployed market with activity so the demo shows live numbers
	bash scripts/seed-demo.sh

testnet-amm-routes: wasm ## Deploy a throwaway market and prove all AMM routes on testnet
	bash scripts/prove-testnet-amm-routes.sh

clean: ## Remove build artifacts
	cargo clean
