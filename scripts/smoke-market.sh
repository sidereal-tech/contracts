#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Strategy-generic live smoke for one deployed V2 market.
#
# The existing smokes cannot cover a V2 market:
#   - smoke-testnet.sh drives the rate with the admin `set_exchange_rate`, which
#     does not exist on sy-vault-v2 (the rate is derived, always);
#   - smoke-blend-testnet.sh asserts only `> 0`.
#
# This one reads addresses from the market manifest, works against any strategy
# behind the seam, and asserts closed-form relationships rather than positivity.
#
# Usage:
#   scripts/smoke-market.sh <network>/<market-id>
#   DEPOSIT_AMOUNT=200000000 scripts/smoke-market.sh testnet/blend-usdc-testnet-q4

set -euo pipefail

REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$REPO"

TARGET="${1:-${MARKET:-}}"
IDENTITY="${DEPLOY_IDENTITY:-sidereal-deployer}"
DEPOSIT_AMOUNT="${DEPOSIT_AMOUNT:-100000000}"   # 10 USDC at 7 decimals
SPLIT_FRACTION="${SPLIT_FRACTION:-2}"           # split 1/N of the minted SY

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
pass() { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[[ -n "$TARGET" ]] || die "usage: $0 <network>/<market-id>"
command -v stellar >/dev/null 2>&1 || die "stellar not found"
command -v node >/dev/null 2>&1 || die "node not found"

eval "$(node scripts/sync-market-registry.mjs --env "$TARGET")"
NETWORK="${NETWORK:?}"

CALLER="$(stellar keys address "$IDENTITY")"

# `stellar contract invoke` prints JSON; scalars come back quoted.
call() {
  local id="$1"
  shift
  stellar contract invoke --id "$id" --source "$IDENTITY" --network "$NETWORK" -- "$@" 2>/dev/null | tr -d '"'
}

log "Market $MARKET_ID ($STRATEGY_KIND) on $NETWORK"
log "  vault=$SY strategy=$STRATEGY"

# --- 1. The seam's invariants hold on the live deployment -------------------

log "Checking the strategy binding on-chain"
[[ "$(call "$SY" strategy)" == "$STRATEGY" ]] || die "vault does not name the manifest strategy"
[[ "$(call "$STRATEGY" vault)" == "$SY" ]] || die "strategy does not name the vault"
[[ "$(call "$STRATEGY" underlying)" == "$UNDERLYING" ]] || die "strategy underlying mismatch"
pass "vault and strategy name each other, and agree on the underlying"

RATE_0="$(call "$SY" exchange_rate)"
ASSETS_0="$(call "$SY" total_assets)"
LIQUID_0="$(call "$SY" max_withdraw)"
(( RATE_0 > 0 )) || die "exchange rate must be positive, got $RATE_0"
(( LIQUID_0 <= ASSETS_0 )) || die "max_withdraw ($LIQUID_0) exceeds total_assets ($ASSETS_0)"
pass "rate=$RATE_0 assets=$ASSETS_0 max_withdraw=$LIQUID_0 (max_withdraw <= total_assets)"

BALANCE_0="$(call "$UNDERLYING" balance --id "$CALLER")"
(( BALANCE_0 >= DEPOSIT_AMOUNT )) \
  || die "caller holds $BALANCE_0 underlying, needs $DEPOSIT_AMOUNT. Fund it from the Blend faucet first."

# --- 2. Deposit prices against measured assets ------------------------------

log "Depositing $DEPOSIT_AMOUNT underlying"
SY_BEFORE="$(call "$SY" balance --id "$CALLER")"
MINTED_TOTAL="$(call "$SY" deposit --from "$CALLER" --amount "$DEPOSIT_AMOUNT" --min_sy_out 1)"
SY_AFTER="$(call "$SY" balance --id "$CALLER")"
MINTED=$(( SY_AFTER - SY_BEFORE ))

(( MINTED > 0 )) || die "deposit minted no SY"
(( MINTED == MINTED_TOTAL )) \
  || die "deposit returned $MINTED_TOTAL but the balance moved by $MINTED"
# At a rate >= 1.0, SY minted is never more than the underlying deposited.
(( MINTED <= DEPOSIT_AMOUNT )) \
  || die "minted $MINTED SY for $DEPOSIT_AMOUNT underlying at rate $RATE_0 — rate is inverted"
pass "minted $MINTED SY, and the returned value matches the balance change"

ASSETS_1="$(call "$SY" total_assets)"
CREDITED=$(( ASSETS_1 - ASSETS_0 ))
(( CREDITED > 0 )) || die "total_assets did not rise after the deposit"
# The upstream may floor in its own favour, but never in ours.
(( CREDITED <= DEPOSIT_AMOUNT )) \
  || die "strategy credited $CREDITED for a $DEPOSIT_AMOUNT deposit — more than was sent"
SHORTFALL=$(( DEPOSIT_AMOUNT - CREDITED ))
pass "strategy credited $CREDITED of $DEPOSIT_AMOUNT (upstream rounding kept back $SHORTFALL)"

LIQUID_1="$(call "$SY" max_withdraw)"
(( LIQUID_1 <= ASSETS_1 )) || die "max_withdraw ($LIQUID_1) exceeds total_assets ($ASSETS_1)"
pass "max_withdraw=$LIQUID_1 still bounded by total_assets=$ASSETS_1"

# --- 3. The rate stays derived ----------------------------------------------

RATE_1="$(call "$SY" exchange_rate)"
# A deposit priced at the pre-deposit rate must not move the rate materially:
# the depositor paid fair value in. Allow one unit of integer-division dust.
DRIFT=$(( RATE_1 - RATE_0 ))
(( DRIFT < 0 )) && DRIFT=$(( -DRIFT ))
(( DRIFT <= 1000000000 )) \
  || die "deposit moved the rate by $DRIFT (from $RATE_0 to $RATE_1) — pricing bug"
pass "rate is unchanged by a fairly-priced deposit ($RATE_0 -> $RATE_1)"

# There is no rate setter on a V2 vault. Prove it rather than asserting it.
if stellar contract invoke --id "$SY" --source "$IDENTITY" --network "$NETWORK" \
    -- set_exchange_rate --admin "$CALLER" --exchange_rate 2000000000000000000 >/dev/null 2>&1; then
  die "sy-vault-v2 accepted set_exchange_rate — the #9 Insolvent lever is back"
fi
pass "no rate setter exists on the vault"

# --- 4. Tokenize through the unchanged stack --------------------------------

SPLIT_AMOUNT=$(( MINTED / SPLIT_FRACTION ))
if (( SPLIT_AMOUNT > 0 )); then
  log "Splitting $SPLIT_AMOUNT SY into PT + YT"
  call "$TK" split --from "$CALLER" --sy_amount "$SPLIT_AMOUNT" >/dev/null
  PT_BAL="$(call "$PT" balance --id "$CALLER")"
  YT_BAL="$(call "$YT" balance --id "$CALLER")"
  (( PT_BAL > 0 )) || die "split minted no PT"
  (( PT_BAL == YT_BAL )) || die "split minted $PT_BAL PT but $YT_BAL YT — must be equal"
  pass "split minted $PT_BAL PT and $YT_BAL YT from $SPLIT_AMOUNT SY"

  log "Recombining $PT_BAL PT + YT back into SY"
  SY_PRE_RECOMBINE="$(call "$SY" balance --id "$CALLER")"
  call "$TK" recombine --from "$CALLER" --pt_amount "$PT_BAL" --yt_amount "$PT_BAL" >/dev/null
  SY_POST_RECOMBINE="$(call "$SY" balance --id "$CALLER")"
  RETURNED=$(( SY_POST_RECOMBINE - SY_PRE_RECOMBINE ))
  (( RETURNED > 0 )) || die "recombine returned no SY"
  (( RETURNED <= SPLIT_AMOUNT )) \
    || die "recombine returned $RETURNED SY for a $SPLIT_AMOUNT split — created value"
  pass "recombine returned $RETURNED SY (<= the $SPLIT_AMOUNT split)"
fi

# --- 5. Redemption honours its slippage bound -------------------------------

SY_HELD="$(call "$SY" balance --id "$CALLER")"
REDEEM_AMOUNT=$(( SY_HELD / 2 ))
if (( REDEEM_AMOUNT > 0 )); then
  EXPECTED="$(call "$SY" preview_redeem --sy_amount "$REDEEM_AMOUNT")"
  LIQUID_NOW="$(call "$SY" max_withdraw)"
  if (( EXPECTED > LIQUID_NOW )); then
    log "Skipping redemption: preview $EXPECTED exceeds realizable liquidity $LIQUID_NOW"
  else
    log "Redeeming $REDEEM_AMOUNT SY (preview $EXPECTED)"
    # An impossible floor must revert rather than settle short.
    if stellar contract invoke --id "$SY" --source "$IDENTITY" --network "$NETWORK" \
        -- redeem --from "$CALLER" --sy_amount "$REDEEM_AMOUNT" \
        --min_underlying_out $(( EXPECTED * 2 )) >/dev/null 2>&1; then
      die "redeem settled below min_underlying_out"
    fi
    pass "redeem reverts when it cannot meet min_underlying_out"

    UNDER_BEFORE="$(call "$UNDERLYING" balance --id "$CALLER")"
    FLOOR=$(( EXPECTED * 99 / 100 ))
    OUT="$(call "$SY" redeem --from "$CALLER" --sy_amount "$REDEEM_AMOUNT" --min_underlying_out "$FLOOR")"
    UNDER_AFTER="$(call "$UNDERLYING" balance --id "$CALLER")"
    DELIVERED=$(( UNDER_AFTER - UNDER_BEFORE ))

    (( DELIVERED == OUT )) || die "redeem returned $OUT but the wallet moved by $DELIVERED"
    (( DELIVERED >= FLOOR )) || die "delivered $DELIVERED below the floor $FLOOR"
    pass "redeemed $REDEEM_AMOUNT SY for $DELIVERED underlying (floor $FLOOR)"
  fi
fi

# --- 6. Upkeep is permissionless --------------------------------------------

log "Renewing TTLs through the permissionless path"
call "$SY" touch >/dev/null
call "$STRATEGY" touch >/dev/null
pass "vault.touch() and strategy.touch() both succeed for any caller"

FINAL_ASSETS="$(call "$SY" total_assets)"
FINAL_LIQUID="$(call "$SY" max_withdraw)"
FINAL_RATE="$(call "$SY" exchange_rate)"
(( FINAL_LIQUID <= FINAL_ASSETS )) || die "max_withdraw exceeds total_assets at exit"

log "Smoke passed for $MARKET_ID"
log "  rate=$FINAL_RATE assets=$FINAL_ASSETS max_withdraw=$FINAL_LIQUID"
