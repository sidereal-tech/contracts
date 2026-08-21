# Sidereal live user-simulation — findings

Date: 2026-07-10 · Source commit under test: **fa2deb7** · Network: Stellar testnet
Method: rolling epochs of background agents funding real testnet wallets and using the
freshly deployed contracts + frontend as real users. Every agent-claimed finding was
re-verified against source and/or on-chain by the orchestrator before being recorded here.

## Deployment under test (fresh, from fa2deb7, Blend v2 custody)

| Component | Contract ID |
|---|---|
| SY wrapper | `CBX2B6NGQW2SZ7BV2ATJZY7AHVUO2Y47QUATJOEUR7ZCXOL7VMV7EENS` |
| PT token | `CBH33TRW36ESW5AIL4ELOBNFI6K3NSSPLI77WYJIBW54X22AZBUHULXS` |
| YT token | `CAKVWNZKB7ZT24GHKLAMC36A2LJLWCXYFOWSB2XYOZQXWXVIRFQ5U45I` |
| Tokenizer | `CCLNQII4FKGNMJQRYQGPE4BPPBCH32MU3NNS646WFB673NS5K62KL33H` |
| AMM | `CBLIBCVL4NEZ4EX6LFKAD2OBZZPNWDWKFKMMWAJFIXPBQF6C4KEXYSHN` |
| Underlying USDC SAC | `CAQCFVLOBK5GIULPNZRGATJJMIZL5BSP7X5YJVMGCPTUEPFM4AVSRCJU` (Blend testnet USDC) |
| Blend pool | `CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF` |

Maturity 1791431478 (~2026-10-08) · 7 decimals · AMM fee 10bps · TWAP window 1800s.
Frontend: production https://www.sidereal.tech redeployed to these addresses; env
verified by pulling Vercel prod values back after the update. Manifest:
`deployments/testnet.toml`.

## Headline: all three hardening rounds now have LIVE on-chain proof

The three audit-fix rounds (`6511900`, `09ff467`, `fa2deb7`) had only ever been proven in
the integration harness. This simulation proved each against real Blend accrual on testnet:

- **Round-1 allowance TTL** — the allowance entry's `liveUntilLedgerSeq` now equals the
  requested `expiration_ledger` (not the 720-ledger temp-storage minimum). The cohort-sim
  allowance bug is dead. (Finding 10.)
- **Round-2 observation-based maturity freeze** — `freeze_maturity_rate` pinned the last
  pre-maturity observation (R_obs = 1.000000012) and ignored the higher live post-maturity
  rate (1.000000033), re-verified by independent `maturity_rate` read. The split-brain
  blocker is closed in live conditions. (Finding 13.)
- **Round-3 direct-YT settlement** — direct YT transfer/burn banks yield through the
  tokenizer observation; the sender keeps only their own accrued yield, the receiver starts
  fresh at the transfer rate. Proven with independently reproduced arithmetic. (Finding 11.)

Also: the full lifecycle smoke (deposit → split → claim → recombine → freeze → PT redeem →
SY redeem) **passed end-to-end** on the fresh bytecode after a one-line script fix
(finding 1). PT-senior surplus cap held on-chain: escrow covered the senior PT reservation
before any YT payout in every settlement (findings 11, 13). Empty-pool swap attempts
rejected cleanly with `MarketNotSeeded` (#9), correctly mapped in the frontend.

## Findings by severity

Nothing CRITICAL or HIGH was found. No fund-loss, no value-from-nothing, no invariant
violation that any economic path depends on. The items below are error-semantics, UX, and
one design decision.

### MEDIUM

**M1 — Previews are point-in-time; `recombine` has no min-out floor** (finding 2).
`preview_claim_yield`/`preview_recombine` read the live, continuously-accruing Blend rate
at call time, so a rate move between preview and submission shifts the result (observed:
preview 5, executed 6). Direction of harm is benign — the supply-only rate is monotonic up,
so claims pay ≥ preview and recombine returns fewer-but-more-valuable shares (value
conserved). But `recombine(from, pt_amount, yt_amount)` (tokenizer lib.rs:247) is the one
value-moving entrypoint with no slippage floor, unlike every AMM call. **Decision owed:**
document previews as point-in-time, or add an optional `min_sy_out` to `recombine`.
**Status (2026-07-10): FIXED — Path B (document, defer).** Point-in-time doc notes added to
`preview_recombine` (tokenizer), `preview_claim_yield` (yt-token), the SDK recombine
builder + types, and ARCHITECTURE §3; no ABI change. A `min_sy_out` floor was deliberately
deferred — it only helps a contract composing on `recombine`, and none does today.

**M2 — `twapWarmingUp` not gated on trade/pool pages** (finding 4).
During the full 1800s TWAP warm-up, `/trade` and `/pool` render implied APY as a confident
amber "live feed" percentage — and `/trade` labels it "Implied APY (TWAP)", asserting a
TWAP that has not warmed — while `/mint`'s yield-choice card correctly shows a "Warming up"
pill. Display-only (swaps price off live reserves, no funds at risk), but misleading. Both
pages already fetch `twapWarmingUp`; the fix is mechanical. Source: pool/page.tsx:305-310,
trade/page.tsx:183-188; only lib/yieldChoice.ts:32 gates. (Flagged for Rahul in
evaluation.md §2; severity raised from display-only after seeing the "(TWAP)" label.)

**M3 — Stale-quote fee burn on `remove_liquidity`** (finding 3).
Under concurrent trading, a withdrawal with tight min-outs passed simulation then trapped
(`SlippageExceeded`) on submission, burning ~33.8k stroops on a guaranteed-revert tx. The
contract guard behaved exactly right — no partial withdrawal. The pool page does apply a
user-selectable 50bps default buffer to all three LP min-outs (slippage.ts:9,
pool/page.tsx:83-85), but previews are computed at render and not re-simulated at signature
time. **Recommendation:** re-simulate immediately before wallet signature.

**M4 — Misleading error code for wrong-caller on SY admin gates** (finding 7).
`set_exchange_rate` and `migrate_reserve_index` check `admin != config.admin` and return
`Error::NotInitialized` (code 2 — false, the contract IS initialized) before reaching the
Blend-custody guard. Rejection is correct (verified on-chain: non-admin sims both return
`Error(Contract,#2)`); only the code is wrong. Source: sy-wrapper/src/lib.rs:142-159,
181-189.
**Status (2026-07-10): FIXED.** Added `NotAuthorized = 12` to the sy-wrapper Error enum;
both wrong-caller sites now return it (the two genuine not-initialized uses are untouched);
mapped `12: "Not authorized."` in app/lib/errors.ts; added a `#12` unit test. sy-wrapper
suite 14 passed / exit 0, still builds to wasm.

**M5 — Standalone YT burn breaks the stated PT==YT supply invariant, but inertly**
(finding 12). A holder-privileged standalone SEP-41 YT `burn` reduces YT total_supply below
PT total_supply (observed gap == burned amount). **Verified economically inert:** the
tokenizer reads only *PT* total_supply for escrow coverage / PT-senior cap / pro-rata
(lib.rs:184,277,319,378); YT total_supply feeds no economic path, and per-holder recombine
operates on the holder's own balances. The burner simply forfeits their own future yield to
escrow, favoring PT seniority. **Action:** qualify the invariant statement (holds under
split/recombine; standalone YT burn may reduce YT supply), or reconsider exposing
standalone YT burn.
**Status (2026-07-10): FIXED — documentation.** Added a §8 risk row in ARCHITECTURE plus a
comment on the yt-token `burn` explaining the parity break is by-design and inert (no
economic path reads YT supply). Kept the entrypoint. Note: `journey.rs` asserts global
PT==YT supply, but only in a split/flash/recombine test that never calls standalone burn,
so the note doesn't contradict it — flagged for if that test ever grows a burn case.

### LOW

**L1 — `scripts/smoke-testnet.sh` drifted from the interface** (finding 1).
Line 246 calls `preview_claim_yield` on the tokenizer, but the function lives on the YT
token (SDK agrees, sdk/src/client.ts:193). The documented post-deploy regression check dies
mid-run on any current deployment. One-line fix; with it applied the full lifecycle passes.

**L2 — Frontend AMM error table is incomplete** (finding 8).
`app/lib/errors.ts` maps only AMM codes 4/9/10/11/12; the enum (amm/src/lib.rs:72-90) runs
to 19. User-reachable codes `MarketProportionTooHigh`(14), `ExchangeRateBelowOne`(15),
`InputOutOfBounds`(18), `InvalidSyRate`(19) fall back to a generic "Transaction failed
(error #N)".

**L3 — Pages don't poll; "Live feed" label overstates freshness** (finding 5).
A 6-minute idle tab showed byte-identical stats while on-chain reserves moved. Fresh loads
are correct and match chain to the base unit, so it's a labeling (or polling) gap.

**L4 — Post-maturity `observe_rate` returns a bare `#6`** (finding 14).
A keeper polling `observe_rate` on a just-matured market gets `Error(Contract,#6)` (Matured)
with no "call freeze_maturity_rate" hint. Safe (no fee, no state change), but unmapped for
keepers/UI. Same family as L2, tokenizer side.

### PROCESS

**P1 — Adversarial-framed subagents get chain access blocked** (finding 9).
An agent framed as "try to break it / attack financial infra" had its `stellar`/RPC calls
denied by a harness intent classifier (plain Bash still worked), producing zero on-chain
adversarial coverage that epoch. Correct agent behavior (it stopped rather than seek a
bypass). **Mitigation:** frame negative testing explicitly as authorized security testing,
or have the orchestrator run the probes directly (it did, for finding 7). The donation-attack
surface separately looks unexploitable by source read — LP mint prices off internal
`state.total_pt/total_sy` (amm/src/lib.rs:525-535), not live balance, plus the
`MINIMUM_LIQUIDITY` burn — but this was not executed live and remains an inference.

## Epochs run

- **Epoch 1** (complete) — LP whale (seeded the empty pool as first LP), yield trader
  (swaps + both flash routes, empty-pool probe), frontend user (all six routes, e2e suite,
  API routes, production rebake check). Findings 1–6. Orchestrator ran the lifecycle smoke.
- **Epoch 2** (complete) — adversarial (source-level after chain block; findings 7–9),
  token mechanics (allowance TTL + direct-YT settlement proofs; findings 10–12), maturity
  drill (own 900s market, the freeze headline; findings 13–14).
- **Epoch 3** (not run) — an authorized-security-tester and a concurrency persona were
  queued but blocked by session limits (agent hit the session cap; the safety classifier
  for launching the second was temporarily unavailable). Uncovered next time: on-chain
  negative/robustness probes with authorized framing, and multi-user concurrent load on the
  reserve/TWAP accounting. The queued prompts and coverage targets are described in the
  epoch-3 section of the git history / prior session; re-run with corrected framing.

## What this simulation did and did not cover

Covered live on-chain: wrap/split/recombine/claim, PT/SY swaps, both YT flash routes, LP
add/remove, PT/YT transfers, allowances (incl. TTL), direct YT transfer/burn settlement, the
full maturity freeze/redeem against real accrual, empty-pool and wrong-caller rejections,
and the six frontend routes + API routes + production rebake.

Not covered (still owed, see evaluation.md): a wallet-signed end-to-end run through the UI
(the e2e flow spec is still gated, no automated signer), an LP-archival restore drill, and
multi-user concurrency under sustained load. No mainnet action was taken — this is testnet
validation of the fa2deb7 bytecode, not a deploy.

**M6 — AMM curve mixes SY shares with asset-unit PT face** (post-launch review, 2026-07-18).
The time-decay curve consumes `total_sy` as raw SY *shares* while `total_pt` is asset-unit
face. The mismatch is a single line: `amm/src/lib.rs:799` reads `let total_asset =
state.total_sy;` — a variable named for assets, assigned shares, with no index conversion.
Pendle's `MarketMathCore` values the SY reserve at the index (`totalSy → asset`) before
curve math and converts outputs back; in v1 only the YT flash routes perform that boundary
conversion, and the plain PT↔SY legs do not. So `exchange_rate` (lib.rs:1216-1242) is a
face-per-*share* factor rather than face-per-asset, and its reciprocal is shares paid per
unit of PT face.
**Consequences at SY rate R > 1:** the curve's maturity convergence pins PT face to one SY
share (= R assets) against a redemption value of one asset — an (R−1)-bounded LP leak near
maturity; and the quoted implied APY (lib.rs:1183-1200) is share-denominated, so it drifts
from the true fixed rate as R accrues.
**Bounding:** the live market is short (30 days) and small (~5 PT + 5 SY seeded), but its
exact loss cannot be derived from the `1.0063` initial anchor — that parameter is a seeded
target, not the live SY rate. At an illustrative 8% annualized SY return, 30 days of accrual
is roughly 0.66% before the single 10-bps PT-sale fee; actual impact must be measured from
on-chain rate observations. The defect scales with term and with realized SY return.
**Why the suites missed it:** the 10,000-case AMM property test
(`amm/src/lib.rs:2488-2495`) builds on a fixture whose SY sits at the default rate 1.0 —
"where SY shares and asset units coincide", as the fixture comment itself says
(lib.rs:1506-1510). That is precisely the rate at which this deviation vanishes identically.
The non-par cases live in `tests/integration` and exercise only the flash routes, which *do*
perform the boundary conversion.
**Status (2026-07-18): ACCEPTED for v1, FIX SCOPED for v2.** The deployed AMM is immutable
and runs out its term with the deviation documented. The factory-built AMM v2 normalizes
units per Pendle (SY reserves valued at the index inside the curve, outputs converted back)
and gates on the property suite re-run at non-par SY rates (R ∈ {1.0, 1.01, 1.05, 1.1}),
direct PT↔SY quote/execution tests at each rate, and a maturity-convergence test asserting
PT → asset par. Hard prerequisite for maturities longer than the current 30 days.

**RESOLVED 2026-08-18** (audit finding C1, see `docs/audit/2026-08-internal-audit.md`).
The curve is now asset-denominated on both sides: `Precompute` carries the SY rate, read
once per invocation from the same `exchange_rate` entrypoint the tokenizer prices splits
against, and outputs convert back to shares with ceil-in/floor-out at the boundary. Fixing
it also repaired the YT routes, which had been reverting for every sell size once the
wrapper accrued past ~0.5% (finding H1).

One correction to the bounding above: the break-even is `e < R`, where `R` is the SY
wrapper's *cumulative* appreciation since its own inception — not the market's term. A
wrapper reused across a series of maturities carries all prior accrual forward, so the
"30 days of accrual ≈ 0.66%" figure understates it. After a year at 8%, the first 30-day
market minted on that wrapper opens with a standing 2–7% arbitrage against LPs regardless
of its own length. The correct bound is `1 − e/R`.

The property suite now sweeps R ∈ {1.0, 1.01, 1.05, 1.10} across the plain PT↔SY legs *and*
both YT flash routes, with a maturity-convergence test at non-unit rates. Reverting the fix
turns 5 tests plus the proptest red.
