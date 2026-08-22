#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Deploys one complete Sidereal V2 market: strategy + SY vault + PT + YT +
# tokenizer + AMM + resting orderbook.
#
# This is the multi-market successor to deploy-testnet-resilient.sh. The
# difference that matters: state and manifest paths are keyed by MARKET_ID, not
# by network. The old script defaults both to deployments/$NETWORK.{state.env,toml},
# so deploying a second market silently overwrites the first one's record. Here,
# every market gets its own directory entry and markets accumulate.
#
# Resumable: every deployed address is persisted before the next step, so a
# rerun with the same MARKET_ID picks up where it stopped.
#
# No private keys are written. State and manifest carry public contract
# addresses, the public deployer address, the source commit, parameters, and
# bytecode hashes.

set -euo pipefail

REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$REPO"

NETWORK="${NETWORK:-testnet}"
NETWORK_PASSPHRASE="${NETWORK_PASSPHRASE:-Test SDF Network ; September 2015}"
IDENTITY="${DEPLOY_IDENTITY:-sidereal-deployer}"
WASM_DIR="${WASM_DIR:-target/wasm32v1-none/release/optimized}"

STRATEGY_KIND="${STRATEGY_KIND:-blend}"
MARKET_ID="${MARKET_ID:-}"
MARKET_LABEL="${MARKET_LABEL:-}"

MARKETS_DIR="${MARKETS_DIR:-deployments/markets/$NETWORK}"
STATE_FILE="${STATE_FILE:-}"
MANIFEST_OUT="${MANIFEST_OUT:-}"

# Blend v2 testnet defaults. Override for another pool or reserve.
BLEND_POOL="${BLEND_POOL:-CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF}"
BLEND_USDC="${BLEND_USDC:-CAQCFVLOBK5GIULPNZRGATJJMIZL5BSP7X5YJVMGCPTUEPFM4AVSRCJU}"
BLEND_USDC_ASSET="${BLEND_USDC_ASSET:-USDC:GATALTGTWIOT6BUDBCZM3Q4OQ4BO2COLOAZ7IYSKPLC2PMSOPPGF5V56}"

SCALAR_ROOT="${SCALAR_ROOT:-2000000000000000000}"
INITIAL_ANCHOR="${INITIAL_ANCHOR:-1050000000000000000}"
FEE_BPS="${FEE_BPS:-10}"
TWAP_WINDOW="${TWAP_WINDOW:-1800}"
MATURITY="${MATURITY:-$(( $(date -u +%s) + 90 * 86400 ))}"

# Risk metadata carried into the manifest so the app can label a market without
# a second source of truth.
RISK_TIER="${RISK_TIER:-pilot}"
DEPOSIT_CAP="${DEPOSIT_CAP:-0}"

# Initial protocol fee on *claimed yield*. The tokenizer admin can update it
# later with set_fee; every update remains subject to the on-chain 20% ceiling.
# It is taken from the PT-senior-capped junior surplus, so it can never touch PT
# principal or move the exchange rate.
#
# Defaults to 0: a market deploys fee-free unless someone decides otherwise, and
# the decision is explicit rather than inherited from a script default. The
# contract rejects anything above 20%.
#
# Remember whether the operator set these EXPLICITLY, before defaults or a
# resumed state file overwrite them. Persisting the fee fixed a manifest that
# lied on resume, but a plain `source` then made the stored value win even while
# the tokenizer was still uninitialized -- which is the one window where an
# immutable parameter is still correctable. An operator retrying a crashed
# deploy to fix a typo'd recipient would have had the typo silently reinstated.
YIELD_FEE_BPS_ENV="${YIELD_FEE_BPS-}"
FEE_RECIPIENT_ENV="${FEE_RECIPIENT-}"
YIELD_FEE_BPS="${YIELD_FEE_BPS:-0}"
# Taker fee on resting-order fills. The orderbook admin may update it later,
# subject to the contract's 10% ceiling. Defaults to fee-free.
ORDERBOOK_FEE_BPS="${ORDERBOOK_FEE_BPS:-0}"
# FEE_RECIPIENT defaults to the deployer, resolved after the identity is read.
RISK_NOTES="${RISK_NOTES:-Unaudited pilot market. Deposit only what you can afford to lose.}"

# The vault's own TTL policy extends persistent entries to 120 days. A market
# whose maturity plus redemption window runs past that horizon is only safe if
# something renews it; see scripts/keeper.mjs.
TTL_EXTEND_DAYS="${TTL_EXTEND_DAYS:-120}"
REDEMPTION_WINDOW_DAYS="${REDEMPTION_WINDOW_DAYS:-30}"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() { command -v "$1" >/dev/null 2>&1 || die "$1 not found"; }
quote_env() { printf '%q' "$1"; }

save_state() {
  mkdir -p "$(dirname "$STATE_FILE")"
  local tmp
  tmp="$(mktemp "$STATE_FILE.tmp.XXXXXX")"
  {
    printf 'DEPLOY_STARTED_AT=%s\n' "$(quote_env "${DEPLOY_STARTED_AT:-}")"
    printf 'SOURCE_COMMIT=%s\n' "$(quote_env "${SOURCE_COMMIT:-}")"
    printf 'NETWORK=%s\n' "$(quote_env "$NETWORK")"
    printf 'NETWORK_PASSPHRASE=%s\n' "$(quote_env "$NETWORK_PASSPHRASE")"
    printf 'IDENTITY=%s\n' "$(quote_env "$IDENTITY")"
    printf 'DEPLOYER_ADDRESS=%s\n' "$(quote_env "${DEPLOYER_ADDRESS:-}")"
    # Market identity is persisted, not reconstructed from ambient env on rerun.
    printf 'MARKET_ID=%s\n' "$(quote_env "$MARKET_ID")"
    printf 'MARKET_LABEL=%s\n' "$(quote_env "$MARKET_LABEL")"
    printf 'STRATEGY_KIND=%s\n' "$(quote_env "$STRATEGY_KIND")"
    printf 'MATURITY=%s\n' "$(quote_env "$MATURITY")"
    printf 'SCALAR_ROOT=%s\n' "$(quote_env "$SCALAR_ROOT")"
    printf 'INITIAL_ANCHOR=%s\n' "$(quote_env "$INITIAL_ANCHOR")"
    printf 'FEE_BPS=%s\n' "$(quote_env "$FEE_BPS")"
    printf 'YIELD_FEE_BPS=%s\n' "$(quote_env "$YIELD_FEE_BPS")"
    printf 'ORDERBOOK_FEE_BPS=%s\n' "$(quote_env "$ORDERBOOK_FEE_BPS")"
    printf 'FEE_RECIPIENT=%s\n' "$(quote_env "${FEE_RECIPIENT:-}")"
    printf 'TWAP_WINDOW=%s\n' "$(quote_env "$TWAP_WINDOW")"
    printf 'BLEND_POOL=%s\n' "$(quote_env "$BLEND_POOL")"
    printf 'BLEND_USDC=%s\n' "$(quote_env "$BLEND_USDC")"
    printf 'BLEND_USDC_ASSET=%s\n' "$(quote_env "$BLEND_USDC_ASSET")"
    printf 'UNDERLYING_ID=%s\n' "$(quote_env "${UNDERLYING_ID:-}")"
    printf 'UNDERLYING_ASSET=%s\n' "$(quote_env "${UNDERLYING_ASSET:-}")"
    printf 'STRATEGY=%s\n' "$(quote_env "${STRATEGY:-}")"
    printf 'SY=%s\n' "$(quote_env "${SY:-}")"
    printf 'PT=%s\n' "$(quote_env "${PT:-}")"
    printf 'YT=%s\n' "$(quote_env "${YT:-}")"
    printf 'TK=%s\n' "$(quote_env "${TK:-}")"
    printf 'AMM=%s\n' "$(quote_env "${AMM:-}")"
    printf 'ORDERBOOK=%s\n' "$(quote_env "${ORDERBOOK:-}")"
    printf 'INIT_STRATEGY=%s\n' "$(quote_env "${INIT_STRATEGY:-0}")"
    printf 'INIT_SY=%s\n' "$(quote_env "${INIT_SY:-0}")"
    printf 'SET_CAP=%s\n' "$(quote_env "${SET_CAP:-0}")"
    printf 'INIT_PT=%s\n' "$(quote_env "${INIT_PT:-0}")"
    printf 'INIT_YT=%s\n' "$(quote_env "${INIT_YT:-0}")"
    printf 'INIT_TK=%s\n' "$(quote_env "${INIT_TK:-0}")"
    printf 'INIT_AMM=%s\n' "$(quote_env "${INIT_AMM:-0}")"
    printf 'INIT_ORDERBOOK=%s\n' "$(quote_env "${INIT_ORDERBOOK:-0}")"
  } > "$tmp"
  mv "$tmp" "$STATE_FILE"
}

load_state() {
  if [[ -f "$STATE_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
    log "Loaded market state from $STATE_FILE"
  fi
}

deploy_wasm_once() {
  local var_name="$1" wasm_name="$2" current="${!var_name:-}"
  if [[ -n "$current" ]]; then
    log "Reusing $var_name=$current"
    return
  fi
  log "Deploying $wasm_name"
  local deployed
  deployed="$(stellar contract deploy \
    --wasm "$WASM_DIR/$wasm_name.wasm" \
    --source "$IDENTITY" \
    --network "$NETWORK" 2>/dev/null)"
  printf -v "$var_name" '%s' "$deployed"
  save_state
  log "  $var_name=$deployed"
}

invoke_once() {
  local marker="$1" contract_id="$2"
  shift 2
  if [[ "${!marker:-0}" == "1" ]]; then
    log "Skipping completed init marker $marker"
    return
  fi
  stellar contract invoke \
    --id "$contract_id" \
    --source "$IDENTITY" \
    --network "$NETWORK" \
    -- "$@" >/dev/null
  printf -v "$marker" '%s' "1"
  save_state
}

wasm_hash() { stellar contract info hash --wasm "$1"; }
onchain_hash() { stellar contract info hash --id "$1" --network "$NETWORK"; }

require_cmd git
require_cmd stellar

case "$STRATEGY_KIND" in
  blend) ;;
  defindex) die "strategy-defindex is not implemented yet" ;;
  *) die "STRATEGY_KIND must be blend, got '$STRATEGY_KIND'" ;;
esac

[[ -n "$MARKET_ID" ]] || die "MARKET_ID is required (e.g. MARKET_ID=blend-usdc-2026q4)"
[[ "$MARKET_ID" =~ ^[a-z0-9][a-z0-9-]*$ ]] \
  || die "MARKET_ID must be lowercase kebab-case, got '$MARKET_ID'"

STATE_FILE="${STATE_FILE:-$MARKETS_DIR/$MARKET_ID.state.env}"
MANIFEST_OUT="${MANIFEST_OUT:-$MARKETS_DIR/$MARKET_ID.toml}"

# Refuse to silently adopt another market's half-finished state.
if [[ -f "$STATE_FILE" ]]; then
  existing_id="$(grep -E '^MARKET_ID=' "$STATE_FILE" | head -1 | cut -d= -f2- | tr -d "'\"")"
  if [[ -n "$existing_id" && "$existing_id" != "$MARKET_ID" ]]; then
    die "state at $STATE_FILE belongs to market '$existing_id', not '$MARKET_ID'"
  fi
fi

# Untracked files count as dirty. A manifest records source_commit as a
# reproducibility claim, and untracked contract source is exactly the case where
# that claim is false: the wasm was built from code the commit does not contain.
dirty="$(git status --porcelain)"
if [[ -n "$dirty" && "${ALLOW_DIRTY:-0}" != "1" ]]; then
  printf '%s\n' "$dirty" >&2
  die "source is dirty (tracked or untracked). Commit first, or set ALLOW_DIRTY=1 for a non-reproducible dry run"
fi

# ALLOW_DIRTY buys a dry run, never a manifest that claims a commit it was not
# built from. Untracked source under contracts/ makes source_commit a lie no
# matter what, so it is refused unconditionally.
untracked_contracts="$(git ls-files --others --exclude-standard -- contracts/)"
if [[ -n "$untracked_contracts" ]]; then
  printf '%s\n' "$untracked_contracts" >&2
  die "untracked source under contracts/ cannot be deployed: the recorded source_commit would not contain it"
fi

CURRENT_SOURCE_COMMIT="$(git rev-parse HEAD)"
load_state
if [[ -n "${SOURCE_COMMIT:-}" && "$SOURCE_COMMIT" != "$CURRENT_SOURCE_COMMIT" ]]; then
  die "state was created from $SOURCE_COMMIT, but current source is $CURRENT_SOURCE_COMMIT"
fi
SOURCE_COMMIT="$CURRENT_SOURCE_COMMIT"

# Fee reconciliation, following the SOURCE_COMMIT precedent above. Which source
# wins depends on whether the tokenizer has been initialized. `fee_recipient`
# becomes immutable then; `yield_fee_bps` remains admin-settable on-chain, but
# this deploy script records the opening value and never disguises a resumed
# deployment as a fee-management action:
#
#   INIT_TK != 1 -- still mutable. An explicit env value WINS, so an operator
#                   retrying a crashed deploy can correct a typo.
#   INIT_TK == 1 -- opening values are already on chain. A conflicting env
#                   value is fatal rather than silently ignored or applied.
#                   Use tokenizer.set_fee separately for a live fee change;
#                   fee_recipient cannot be changed at all.
if [[ "${INIT_TK:-0}" == "1" ]]; then
  if [[ -n "$YIELD_FEE_BPS_ENV" && "$YIELD_FEE_BPS_ENV" != "$YIELD_FEE_BPS" ]]; then
    die "tokenizer opened with yield_fee_bps=$YIELD_FEE_BPS; a resumed deploy will not change it to $YIELD_FEE_BPS_ENV -- use tokenizer.set_fee after deployment"
  fi
  if [[ -n "$FEE_RECIPIENT_ENV" && "$FEE_RECIPIENT_ENV" != "${FEE_RECIPIENT:-}" ]]; then
    die "tokenizer is already initialized with fee_recipient=${FEE_RECIPIENT:-}; it is immutable and cannot be changed to $FEE_RECIPIENT_ENV"
  fi
else
  if [[ -n "$YIELD_FEE_BPS_ENV" && "$YIELD_FEE_BPS_ENV" != "$YIELD_FEE_BPS" ]]; then
    warn "overriding stored yield_fee_bps=$YIELD_FEE_BPS with $YIELD_FEE_BPS_ENV (tokenizer not yet initialized)"
    YIELD_FEE_BPS="$YIELD_FEE_BPS_ENV"
  fi
  if [[ -n "$FEE_RECIPIENT_ENV" && "$FEE_RECIPIENT_ENV" != "${FEE_RECIPIENT:-}" ]]; then
    warn "overriding stored fee_recipient=${FEE_RECIPIENT:-} with $FEE_RECIPIENT_ENV (tokenizer not yet initialized)"
    FEE_RECIPIENT="$FEE_RECIPIENT_ENV"
  fi
fi

DEPLOY_STARTED_AT="${DEPLOY_STARTED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

MARKET_LABEL="${MARKET_LABEL:-Blend v2 USDC}"
UNDERLYING_ID="${UNDERLYING_ID:-$BLEND_USDC}"
UNDERLYING_ASSET="${UNDERLYING_ASSET:-$BLEND_USDC_ASSET}"

# A market cannot outlive its storage. The vault extends persistent entries to
# TTL_EXTEND_DAYS; anything past that horizon depends on the keeper actually
# running, so require that to be acknowledged rather than assumed.
now="$(date -u +%s)"
horizon_days=$(( (MATURITY - now) / 86400 + REDEMPTION_WINDOW_DAYS ))
if (( horizon_days > TTL_EXTEND_DAYS )) && [[ "${KEEPER_CONFIGURED:-0}" != "1" ]]; then
  die "maturity + ${REDEMPTION_WINDOW_DAYS}d redemption window is ${horizon_days}d, beyond the ${TTL_EXTEND_DAYS}d TTL policy.
  Run the keeper (scripts/keeper.mjs) for this market and set KEEPER_CONFIGURED=1, or shorten MATURITY."
fi

save_state

log "Building optimized deployable Wasm from $SOURCE_COMMIT"
OPT_WASM_DIR="$WASM_DIR" bash scripts/build-optimized-wasm.sh

STRATEGY_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_strategy_blend.wasm")"
SY_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_sy_vault_v2.wasm")"
PT_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_pt_token.wasm")"
YT_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_yt_token.wasm")"
TK_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_tokenizer.wasm")"
AMM_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_amm.wasm")"
ORDERBOOK_WASM_HASH="$(wasm_hash "$WASM_DIR/sidereal_orderbook.wasm")"

if ! stellar keys address "$IDENTITY" >/dev/null 2>&1; then
  log "Generating and funding deployer identity '$IDENTITY' on $NETWORK"
  stellar keys generate --global "$IDENTITY" --network "$NETWORK" --fund 2>/dev/null \
    || stellar keys generate "$IDENTITY" --network "$NETWORK" --fund
fi
DEPLOYER_ADDRESS="$(stellar keys address "$IDENTITY")"
FEE_RECIPIENT="${FEE_RECIPIENT:-$DEPLOYER_ADDRESS}"
save_state
log "Deployer: $DEPLOYER_ADDRESS"
log "Market:   $MARKET_ID ($STRATEGY_KIND) maturity=$MATURITY"

# Both the vault and the strategy must exist before either is initialized: each
# names the other, and the vault's initialize verifies the strategy points back
# at it.
deploy_wasm_once STRATEGY sidereal_strategy_blend
deploy_wasm_once SY sidereal_sy_vault_v2
deploy_wasm_once PT sidereal_pt_token
deploy_wasm_once YT sidereal_yt_token
deploy_wasm_once TK sidereal_tokenizer
deploy_wasm_once AMM sidereal_amm
deploy_wasm_once ORDERBOOK sidereal_orderbook

log "Initializing strategy ($STRATEGY_KIND)"
invoke_once INIT_STRATEGY "$STRATEGY" initialize \
  --admin "$DEPLOYER_ADDRESS" \
  --vault "$SY" \
  --underlying "$UNDERLYING_ID" \
  --pool "$BLEND_POOL"

log "Initializing SY vault (binds the strategy permanently)"
invoke_once INIT_SY "$SY" initialize --admin "$DEPLOYER_ADDRESS" --strategy "$STRATEGY"

# Apply the manifest's deposit cap on-chain. Without this the cap is a number in
# a file that no contract keeps -- the manifest would assert a limit the market
# does not enforce. 0 means uncapped, so the call is skipped rather than writing
# a no-op. The cap gates deposits only; it can never block a redemption.
if [[ "$DEPOSIT_CAP" != "0" ]]; then
  invoke_once SET_CAP "$SY" set_deposit_cap --admin "$DEPLOYER_ADDRESS" --cap "$DEPOSIT_CAP"
fi

log "Initializing PT token"
invoke_once INIT_PT "$PT" initialize --admin "$DEPLOYER_ADDRESS" --tokenizer "$TK" --sy_token "$SY" --maturity "$MATURITY"

log "Initializing YT token"
invoke_once INIT_YT "$YT" initialize --admin "$DEPLOYER_ADDRESS" --tokenizer "$TK" --sy_token "$SY" --maturity "$MATURITY"

# The tokenizer rejects a fee_recipient it can see -- itself, and the three
# tokens passed to `initialize`. It cannot see the AMM or the strategy, because
# it is never told their addresses, so those have to be caught here. Pointing
# the fee at the AMM would make every claim an unpriced LP donation that
# `reconcile_reserves` absorbs, which also nudges the implied-rate oracle;
# pointing it at the strategy or the underlying strands the shares outright.
# `fee_recipient` is immutable, while both fees may be raised from zero later,
# so this check must not depend on either opening fee being nonzero.
for reserved in "$TK:tokenizer" "$SY:SY vault" "$PT:PT token" "$YT:YT token" \
                "$AMM:AMM" "$ORDERBOOK:orderbook" "$STRATEGY:strategy" \
                "$UNDERLYING_ID:underlying token"; do
  addr="${reserved%%:*}"; what="${reserved#*:}"
  if [[ -n "$addr" && "$FEE_RECIPIENT" == "$addr" ]]; then
    die "FEE_RECIPIENT is this market's $what ($addr); a future fee would be stranded or silently donated, and the recipient cannot be changed after deploy"
  fi
done

log "Initializing tokenizer"
invoke_once INIT_TK "$TK" initialize --admin "$DEPLOYER_ADDRESS" --sy_token "$SY" --pt_token "$PT" --yt_token "$YT" --maturity "$MATURITY" \
  --fee_recipient "$FEE_RECIPIENT" --yield_fee_bps "$YIELD_FEE_BPS"

log "Initializing AMM"
invoke_once INIT_AMM "$AMM" initialize \
  --admin "$DEPLOYER_ADDRESS" \
  --pt_token "$PT" \
  --sy_token "$SY" \
  --yt_token "$YT" \
  --tokenizer "$TK" \
  --maturity "$MATURITY" \
  --scalar_root "$SCALAR_ROOT" \
  --initial_anchor "$INITIAL_ANCHOR" \
  --fee_bps "$FEE_BPS" \
  --twap_window "$TWAP_WINDOW"

log "Initializing resting orderbook"
invoke_once INIT_ORDERBOOK "$ORDERBOOK" initialize \
  --admin "$DEPLOYER_ADDRESS" \
  --pt_token "$PT" \
  --sy_token "$SY" \
  --maturity "$MATURITY" \
  --fee_recipient "$FEE_RECIPIENT" \
  --taker_fee_bps "$ORDERBOOK_FEE_BPS"
# PT, YT, the tokenizer, the AMM and the orderbook each store their own copy of `maturity`,
# and nothing on-chain cross-checks them. A mismatch is silent at deploy and
# then bricks every YT transfer and burn inside the gap window, because YT picks
# observe-vs-freeze off its own copy while the tokenizer validates against its.
# They all receive one $MATURITY above, so this only fires if a future edit
# splits them -- which is exactly when nobody would be looking.
#
log "Verifying PT, YT, tokenizer, AMM and orderbook agree on maturity"
for pair in "PT:$PT" "YT:$YT" "tokenizer:$TK" "AMM:$AMM" "orderbook:$ORDERBOOK"; do
  name="${pair%%:*}"; id="${pair#*:}"
  # Check the invoke's STATUS, not just its text. An earlier version used
  # `|| true` with `2>&1`, which threw the status away and folded stderr into
  # the value -- so a call that printed the right number and then failed passed
  # the check, and a failure whose last stderr line happened to be the number
  # passed it too. Keep stderr separate, and report it if the call fails.
  err_file="$(mktemp)"
  if ! got="$(stellar contract invoke --id "$id" --source "$IDENTITY" --network "$NETWORK" --send=no -- maturity 2>"$err_file")"; then
    err_text="$(cat "$err_file")"; rm -f "$err_file"
    die "$name: reading maturity failed: $err_text"
  fi
  rm -f "$err_file"
  got="$(printf '%s' "$got" | tr -d '"' | tail -1)"
  if [[ "$got" != "$MATURITY" ]]; then
    die "$name reports maturity $got, expected $MATURITY -- refusing to record a market whose contracts disagree"
  fi
done
log "  all five agree on $MATURITY"

log "Verifying deployed bytecode matches the local build"
STRATEGY_CHAIN_HASH="$(onchain_hash "$STRATEGY")"
SY_CHAIN_HASH="$(onchain_hash "$SY")"
PT_CHAIN_HASH="$(onchain_hash "$PT")"
YT_CHAIN_HASH="$(onchain_hash "$YT")"
TK_CHAIN_HASH="$(onchain_hash "$TK")"
AMM_CHAIN_HASH="$(onchain_hash "$AMM")"
ORDERBOOK_CHAIN_HASH="$(onchain_hash "$ORDERBOOK")"

[[ "$STRATEGY_WASM_HASH" == "$STRATEGY_CHAIN_HASH" ]] || die "strategy Wasm hash mismatch"
[[ "$SY_WASM_HASH" == "$SY_CHAIN_HASH" ]] || die "SY vault Wasm hash mismatch"
[[ "$PT_WASM_HASH" == "$PT_CHAIN_HASH" ]] || die "PT Wasm hash mismatch"
[[ "$YT_WASM_HASH" == "$YT_CHAIN_HASH" ]] || die "YT Wasm hash mismatch"
[[ "$TK_WASM_HASH" == "$TK_CHAIN_HASH" ]] || die "tokenizer Wasm hash mismatch"
[[ "$AMM_WASM_HASH" == "$AMM_CHAIN_HASH" ]] || die "AMM Wasm hash mismatch"
[[ "$ORDERBOOK_WASM_HASH" == "$ORDERBOOK_CHAIN_HASH" ]] || die "orderbook Wasm hash mismatch"

# The seam's own wiring assertion. record-deploy-provenance.sh refuses to write a
# manifest for a wrapper with idle custody; the V2 equivalent is proving the
# vault and strategy actually name each other on-chain.
log "Verifying the vault/strategy binding on-chain"
chain_strategy="$(stellar contract invoke --id "$SY" --source "$IDENTITY" --network "$NETWORK" -- strategy 2>/dev/null | tr -d '"')"
chain_vault="$(stellar contract invoke --id "$STRATEGY" --source "$IDENTITY" --network "$NETWORK" -- vault 2>/dev/null | tr -d '"')"
[[ "$chain_strategy" == "$STRATEGY" ]] || die "vault names strategy '$chain_strategy', expected '$STRATEGY'"
[[ "$chain_vault" == "$SY" ]] || die "strategy names vault '$chain_vault', expected '$SY'"

DEPLOY_FINISHED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

log "Writing $MANIFEST_OUT"
mkdir -p "$MARKETS_DIR"
cat > "$MANIFEST_OUT" <<EOF
# Generated by scripts/deploy-market.sh.
# Public deployment metadata only. Do not add private keys.

market_id = "$MARKET_ID"
label = "$MARKET_LABEL"
network = "$NETWORK"
network_passphrase = "$NETWORK_PASSPHRASE"
source_commit = "$SOURCE_COMMIT"
deployer_public_key = "$DEPLOYER_ADDRESS"
deployed_at = "$DEPLOY_FINISHED_AT"
maturity = $MATURITY
token_decimals = 7
underlying_asset = "$UNDERLYING_ASSET"

[strategy]
kind = "$STRATEGY_KIND"
contract = "$STRATEGY"
# Upstream protocol addresses, pinned at the strategy's initialization.
pool = "$BLEND_POOL"
reserve = "$BLEND_USDC"
docs_url = "https://docs.blend.capital/"

[risk]
tier = "$RISK_TIER"
deposit_cap = $DEPOSIT_CAP
notes = "$RISK_NOTES"

[curve]
scalar_root = "$SCALAR_ROOT"
initial_anchor = "$INITIAL_ANCHOR"
fee_bps = $FEE_BPS
twap_window = $TWAP_WINDOW

# The protocol's cut of claimed yield, and where it is paid. \`recipient\` is
# fixed at the tokenizer's initialization and can never be changed for this
# market. \`yield_fee_bps\` is the value set at deploy; it is admin-mutable via
# \`tokenizer.set_fee\`, so treat this as the market's opening fee rather than a
# permanent one, and read \`tokenizer.config()\` on chain for the current value.
# Recorded here because a mis-entered value would otherwise leave no artifact
# anywhere else.
[fee]
yield_fee_bps = $YIELD_FEE_BPS
recipient = "$FEE_RECIPIENT"
[orderbook]
taker_fee_bps = $ORDERBOOK_FEE_BPS
fee_recipient = "$FEE_RECIPIENT"

[contracts]
underlying = "$UNDERLYING_ID"
strategy = "$STRATEGY"
sy = "$SY"
pt = "$PT"
yt = "$YT"
tokenizer = "$TK"
amm = "$AMM"
orderbook = "$ORDERBOOK"

[wasm_hashes]
strategy = "$STRATEGY_WASM_HASH"
sy = "$SY_WASM_HASH"
pt = "$PT_WASM_HASH"
yt = "$YT_WASM_HASH"
tokenizer = "$TK_WASM_HASH"
amm = "$AMM_WASM_HASH"
orderbook = "$ORDERBOOK_WASM_HASH"

[onchain_hashes]
strategy = "$STRATEGY_CHAIN_HASH"
sy = "$SY_CHAIN_HASH"
pt = "$PT_CHAIN_HASH"
yt = "$YT_CHAIN_HASH"
tokenizer = "$TK_CHAIN_HASH"
amm = "$AMM_CHAIN_HASH"
orderbook = "$ORDERBOOK_CHAIN_HASH"

[operations]
keeper_configured = ${KEEPER_CONFIGURED:-0}
ttl_extend_days = $TTL_EXTEND_DAYS
redemption_window_days = $REDEMPTION_WINDOW_DAYS
EOF

log "Regenerating the app market registry"
node scripts/sync-market-registry.mjs

log "Market $MARKET_ID deployed"
log "Manifest: $MANIFEST_OUT"
warn "Review and commit $MANIFEST_OUT and the regenerated registry. Do not commit $STATE_FILE."
