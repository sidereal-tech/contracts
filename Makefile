# SPDX-License-Identifier: Apache-2.0
# Convenience targets for the sidereal contracts repo.

.PHONY: help test wasm build deploy deploy-market smoke-market keeper keeper-run seed clean testnet-amm-routes

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

test: ## Run all Soroban contract tests
	cargo test --workspace

wasm: ## Build optimized deployable contract Wasm artifacts
	bash scripts/build-optimized-wasm.sh

build: wasm ## Build the contracts (wasm)

deploy: ## Deploy a single-market V1 protocol to testnet
	bash scripts/deploy-testnet-resilient.sh

deploy-market: ## Deploy one V2 market (strategy + vault + PT/YT/tokenizer/AMM). Requires MARKET_ID
	bash scripts/deploy-market.sh

smoke-market: ## Live end-to-end smoke for one market. Usage: make smoke-market MARKET=testnet/<id>
	bash scripts/smoke-market.sh $(MARKET)

keeper: ## Check every market's invariants and pending upkeep (read-only)
	node scripts/keeper.mjs

keeper-run: ## Perform due upkeep: TTL renewal, rate observation, maturity freeze
	node scripts/keeper.mjs --run

seed: ## Seed the deployed market with activity so the demo shows live numbers
	bash scripts/seed-demo.sh

testnet-amm-routes: wasm ## Deploy a throwaway market and prove all AMM routes on testnet
	bash scripts/prove-testnet-amm-routes.sh

clean: ## Remove build artifacts
	cargo clean
