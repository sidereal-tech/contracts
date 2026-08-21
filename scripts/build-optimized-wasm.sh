#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Build deployable Sidereal contract Wasm artifacts.
#
# This script uses Stellar's optimized Soroban build path and writes upload-ready
# artifacts into a separate directory so deploy scripts never upload debug-only
# sections.

set -euo pipefail

REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$REPO"

OPT_WASM_DIR="${OPT_WASM_DIR:-target/wasm32v1-none/release/optimized}"

# Every cdylib crate in the workspace. `sidereal_strategy_interface` is a plain
# rlib (client bindings only) and is deliberately absent, like the Blend adapter.
contracts=(
  sidereal_sy_wrapper
  sidereal_sy_vault_v2
  sidereal_strategy_blend
  sidereal_pt_token
  sidereal_yt_token
  sidereal_tokenizer
  sidereal_amm
)

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "$1 not found"
}

require_cmd cargo
require_cmd stellar

# The workspace [profile.release] carries the size settings (opt-level=z,
# lto, codegen-units=1, panic=abort, strip), so one uniform build covers every
# contract; stellar-cli additionally runs wasm-opt (--optimize defaults to on).
log "Building optimized deploy artifacts into $OPT_WASM_DIR"
stellar contract build --locked --out-dir "$OPT_WASM_DIR"

if [[ "${SKIP_WASM_FLOAT_CHECK:-0}" != "1" ]]; then
  require_cmd wasm-objdump
  float_check_args=()
  for contract in "${contracts[@]}"; do
    float_check_args+=("$OPT_WASM_DIR/$contract.wasm")
  done
  bash scripts/check-wasm-floats.sh "${float_check_args[@]}"
fi

log "Deployable Wasm sizes"
for contract in "${contracts[@]}"; do
  bytes="$(wc -c < "$OPT_WASM_DIR/$contract.wasm" | tr -d ' ')"
  printf '%s %s bytes\n' "$contract" "$bytes"
done
