# Internal Contract Audit — August 2026

Date: 2026-08-18 · Working tree at `784528f` + uncommitted V2 seam
Scope: all eight contracts (`amm`, `tokenizer`, `pt-token`, `yt-token`, `sy-wrapper`,
`blend-adapter`, `sy-vault-v2`, `strategy-blend`, `strategy-interface`), the deploy
and keeper scripts, and the live mainnet + testnet deployments.

Method: static review of every contract, plus read-only simulation against live
mainnet and testnet RPC, plus a bit-exact Python reimplementation of the AMM curve
math (`scripts/amm-curve-model.py`) used to produce the quantitative figures in C1,
H1, H2 and H3. `cargo test --workspace --locked` passes: **153 tests, 0 failures**
(AMM 36, economics 19, strategy_seam 23, yt 14, sy-wrapper 14, pt 10, blend-adapter 9,
tokenizer 8, journey 7, blend_wrapper 7, auth_invariants 2, strategy-interface 4).

**The suite passing is not evidence of correctness for anything below.** Two of the
most severe findings (C1, H4) are invisible to the suite by construction: the AMM
property tests are pinned to SY rate exactly 1.0, and the economics random-sequence
test only ever moves the rate upward.

## Verdict

| Area | State |
|---|---|
| V1 mainnet SY wrapper | Sound. No fund-loss vector, no admin drain. One November TTL deadline. |
| V1 tokenizer / PT / YT | Solvency holds under a rising rate. Broken by H4 under a rate dip. |
| V1 AMM | **Not safe to seed with third-party liquidity.** C1, H1, H2, H3. |
| V2 vault + strategy seam | **Not safe to deploy.** C2 is a launch blocker. |
| Revenue | Zero, by construction, everywhere. Decision window closes at first V2 `initialize`. |

---

## Remediation status

Updated 2026-08-18, same day. Everything below is in the working tree and
uncommitted. Status is `fixed` only where a test proves it and the full suite is
green; `documented` means the code was already right and the claim about it was
wrong.

| ID | Finding | Status |
|----|---------|--------|
| C1 | AMM prices PT face against raw SY shares | **fixed** — curve is asset-denominated, converts at the boundary with ceil-in/floor-out |
| C2 | V2 vault rate inflatable by a plain transfer | **fixed** — idle excluded from valuation (matching V1), plus a permanent `MINIMUM_SHARES` lock |
| H1 | YT flash routes break above ~1.005 | **fixed** — falls out of C1; sell condition is now rate-independent, buy solver rebuilt as a three-way probe |
| H2 | SY-in swaps keep the unspent budget | **fixed** — only the priced amount is charged; `quote_sy_for_pt_cost` / `quote_sy_for_yt_cost` added |
| H3 | TWAP settable by one atomic trade-and-reverse | **fixed** — accumulator weights the rate that actually prevailed over the interval |
| H4 | YT transfer during a dip re-opens a paid interval | **fixed** — per-holder yield *basis* in SY shares, additive and transferable with the tokens |
| H5 | `max_withdraw` counts undeliverable idle | **fixed** — `withdraw` pays from idle first, asking upstream only for the shortfall |
| H6 | V2 burns full shares on a short delivery | **fixed** — V1's proportional burn restored, rounded ceil (closes L8) |
| M1 | Maturity freeze pins a stale observation | **closed — won't fix.** Code is correct as designed; the one affected market is retired (see below) |
| M2 | Keeper cannot see the mainnet market | **fixed** — legacy loader, V1 TTL fallback, RPC override, plus two latent keeper bugs found while fixing it |
| M3 | Deployed bytecode unreproducible from its claimed commit | **fixed** — untracked files count as dirty; untracked `contracts/` refused outright |
| M4 | Address reuse forfeits yield on new YT | **fixed** — same basis construction as H4 |
| M5 | No fee anywhere; V2 immutable so the window closes at deploy | **fixed (plumbing); rate is yours to choose** — see below |
| M6 | YT routes charge the fee on leveraged PT face | **documented** — incidence is Pendle's model; the docs claimed 0.3% and were wrong |
| M7 | "Rate is monotonic" is asserted but false | **fixed** — claim corrected, with a test pinning that the rate *falls* when Blend socializes bad debt |
| M8 | SY not SEP-41 conformant, emits no events | **open** — V1 is immutable; carry into `sy-vault-v2` before it ships |
| M9 | `deposit_cap` is decorative | **fixed** — enforced against credited assets, admin-settable deposit-only, applied by the deploy script |
| M10 | Blend custody error codes unmapped | **fixed** — and corrected: the app's `sy` context talks to the V2 vault, whose codes differ from V1's |
| M11 | Live admin is the deploy key, not the designated wallet | **documented** — uncorrectable on-chain (no `set_admin`); guard proposed for the next deploy |
| L2 | PT→SY fee floors to zero under 1000 stroops | **fixed** — both directions now round the fee up |
| L8 | Partial-fill burn rounds the wrong way | **fixed** with H6 |
| L13 | Dead `yt_token.burn` auth entry | **fixed** — removed |
| L14 | `zap_out_at_maturity` documented but never built | **fixed** — doc corrected |
| L1, L3–L7, L9–L12 | see the LOW section | **open** — recorded, none fund-affecting |

### The V1 mainnet market is retired, not remediated

Decision, 2026-08-18: the pilot is written off rather than wound down. It matured
2026-08-09 having proven what it existed to prove — five immutable contracts, real
funds, a full lifecycle on mainnet — and V2 deploys at new addresses regardless.
`deployments/mainnet.toml` is marked `retired = true` and the keeper skips it, so
it will not report upkeep nobody intends to perform.

That closes M1 and the November TTL deadline in M2 as *accepted*, not fixed:

- The maturity rate was never frozen and can no longer be improved
  (`observe_rate` is gated on a live market). If anyone ever redeems there, YT
  settles at roughly 2% of what it earned. Total at stake is ~$10.70, of which
  the deployer holds 96% of the YT and the AMM holds 96% of the PT.
- The two ledger entries expiring ~2026-11-11 will simply be allowed to archive.
  Funds remain restorable by anyone (`RestoreFootprint` is permissionless) if
  that ever matters.

**The M2 code fixes still stand and still matter**, because two of the three bugs
found while fixing it were not mainnet-specific: a failed contract read scored as
a satisfied invariant (`BigInt("")` is `0n`), and PT coverage was checked against
the live rate rather than the settlement rate. Both applied to V2 markets too.

**M5 needs a number from you, and only before the first V2 `initialize`.** The
plumbing is in and tested: `yield_fee_bps` and `fee_recipient` are `Config`
fields fixed at initialization, with no setter, a 20% ceiling enforced in the
wasm, and a skim taken from the PT-senior-capped surplus. Deploy scripts default
to **0 bps**, so nothing charges anything until someone decides it should. Peer
protocols sit around 3–5% of claimed yield. After a market's `initialize`
confirms, its fee is fixed for that market's entire life.

---

## CRITICAL

### C1. The AMM prices asset-denominated PT face against raw SY *shares*

`contracts/amm/src/lib.rs:799` — `let total_asset = state.total_sy;`

`total_pt` is PT **face** in asset units (`tokenizer::split` mints
`face = shares * rate / WAD`, `tokenizer/src/lib.rs:231`), but `total_asset` is
assigned raw SY **shares** with no index conversion. `get_exchange_rate`
(`amm/src/lib.rs:1216-1242`) therefore yields face-per-*share*, and
`exact_pt_in_sy_out` (`:836`) pays `pt_in * WAD / exchange_rate` shares for
asset-denominated face. The curve enforces `exchange_rate >= WAD` and decays it
toward 1 as maturity approaches, so the pool converges to "1 PT face = 1 SY share"
while `tokenizer::redeem_at_maturity` (`:325`) pays `pt_face * WAD / R` shares.

Anyone can mint PT for `1/R` shares and sell it to the pool for `~1/e` shares.
Profit is riskless whenever `e < R`.

Measured (30-day term, `initial_anchor` 1.0063, `scalar_root` 2e18, fee 10bps,
1000 PT / 1000 SY pool), single optimal trade, as a percentage of the pool's SY reserve:

| SY rate R | at t−30d | t−15d | t−1h |
|---|---|---|---|
| 1.0063 | 0 | 0 | 0 |
| 1.02 | 0.047% | 0.094% | 1.101% |
| 1.05 | 0.502% | 0.950% | **3.633%** |
| 1.10 | 2.066% | 3.479% | **7.546%** |

This is tracked as M6 in `findings.md` and marked "ACCEPTED for v1", **but the
accepted bound is wrong in a way that matters.** `findings.md` bounds the loss at
"30 days of accrual ≈ 0.66%". The real break-even is `e < R`, where `e ≈ initial_anchor`
is the *remaining-term* discount and `R` is the SY wrapper's **cumulative** appreciation
since its own inception. A wrapper reused across a series of maturities carries all
prior accrual forward: after a year at 8%, `R = 1.08`, and the first 30-day market
minted on that wrapper opens with a standing 2–7% arbitrage against LPs regardless
of its own term length. The bound is `1 − e/R`, not "term accrual".

Realized loss to date is zero only because no third party ever traded the mainnet
pool. Seeding V2 liquidity before fixing this converts it from theoretical to realized,
exactly as `docs/plans/V2_REMAINING_WORK.md:33-49` warns.

### C2. The V2 vault's exchange rate can be inflated by a plain token transfer

`contracts/strategy-blend/src/lib.rs:355-362` (`assets_of`), `:335-337` (`idle_balance`),
`contracts/sy-vault-v2/src/lib.rs:704-712`, `contracts/strategy-interface/src/lib.rs:109-114`

`assets_of` is `idle_balance + supplied_assets`, where `idle_balance` is the strategy
contract's **raw token balance**. `total_assets()` returns it, and the vault derives
`exchange_rate = total_assets * WAD / total_supply` with no virtual-share offset, no
dead shares, and no minimum liquidity anywhere in the vault or in
`derived_exchange_rate`.

So **any address can raise the vault's exchange rate by sending underlying to the
strategy with an ordinary SAC transfer** — no auth, no deposit call, no interaction
with the vault at all.

First-depositor exploit, in stroops, fresh market at `b_rate = 1.0`:

1. Attacker `deposit(1)` → 1 share, `total_assets = 1`.
2. Attacker transfers `4_999_999_999` straight to the strategy address.
   `total_assets = 5_000_000_000`, rate = 5e27.
3. Victim deposits `9_999_999_999` → `floor(9_999_999_999 * 1e18 / 5e27) = 1` share.
   Non-zero, so the `shares <= 0` guard at `sy-vault-v2:335` does not fire.
4. Attacker redeems 1 share → `7_499_999_999`. **Net +249.99 USDC on a 500.00 outlay.**
   Victim recovers ~250 of 1000.

`min_sy_out` is not a defense: `preview_deposit` (`sy-vault-v2:189-192`) divides by the
same poisoned rate, and `sdk/src/client.ts:505` defaults `minSyOut` to `0n`.

**The vector reaches through the vault into the tokenizer.** `tokenizer/src/lib.rs:388-393`
computes `pt_face_reservation = ceil(pt_supply * WAD / rate)`, so a *higher* rate shrinks
the PT reservation and releases escrowed SY to YT claimants. A YT holder can donate to
inflate the rate, claim the released escrow, and redeem it for the donation back —
leaving PT under-covered for the cost of gas. That is the `#9 Insolvent` failure mode
re-entering through a door the design did not anticipate; `sy-vault-v2:18-24` claims it
"cannot re-enter through a new adapter".

**V1 is immune and V2 regressed.** `sy-wrapper/src/lib.rs:712-732` values *only* the
bToken position; `underlying_balance` exists as a separate function and is deliberately
excluded from AUM. Blend's `submit` requires `from.require_auth()`, so nobody can supply
into the wrapper's position on its behalf. Removing the admin rate-setter closed one path
to an attacker-influenced rate; counting idle balance opened another to the same place.

Related: `derived_exchange_rate` returns `WAD` when `sy_supply <= 0` without checking
that assets are also zero, so any market whose supply returns to zero with residual
assets hands the whole position to the next 1-stroop depositor.

Exposure today is testnet only — no V2 contract is on mainnet.

---

## HIGH

### H1. Both YT flash routes break once the SY rate exceeds ~1.005

`contracts/amm/src/lib.rs:457-465` (sell), `:1008-1041` (buy solver)

The second half of C1. The flash routes *do* perform the face↔share conversion, which
is precisely why they are inconsistent with the index-blind curve.

**Sell side.** `swap_yt_for_sy` requires `e > R·(1+fee)`. At the deployed anchor
`e ≈ 1.0063`, that threshold is `R > 1.0053`. Selling 10 YT:

| R | result |
|---|---|
| 1.0000 | ok, 0.0445 SY out |
| 1.0050 | revert `InsufficientLiquidity` |
| 1.0500 | revert (value 9.5238 ≤ cost 9.9555) |

Not size-dependent. **Once the wrapper has accrued ~0.5%, no one can ever sell YT**,
and `quote_yt_for_sy` errors for every input. As maturity approaches `e → 1` and the
threshold falls to `R > 1/(1+fee)`, so any nonzero accrual kills it.

**Buy side.** `solve_yt_out_for_sy_in` binary-searches assuming the predicate holds on
a prefix, but at `R > 1` the feasible set is an interior interval. When the first probe
lands below it the search discards everything above and returns dust — while keeping
the user's input:

| `sy_in` | `yt_out` | SY used | **user's SY kept by the pool for nothing** |
|---|---|---|---|
| 1.00 | 0.0000003 | 0.0000001 | **0.9999999 (100%)** |
| 100.00 | 920.00 (capped) | 67.79 | **32.21 (32%)** |

`quote_sy_for_yt` runs the identical solver, so a client deriving `min_yt_out` from the
quote passes its own check and still loses everything. The comment at `:1001-1007`
anticipates non-monotonicity but concludes the result "can only be suboptimal for the
buyer"; a 100% loss of principal is not suboptimality.

### H2. SY-in swaps transfer the full `sy_in` even when the trade is liquidity-capped, with no refund

`contracts/amm/src/lib.rs:390` and `:432` (full transfer), `:920` (`total_sy += sy_in`),
`:882-904` (search capped at `high = total_pt - 1`)

`exact_sy_in_pt_out_or_panic` returns the largest `pt_out` affordable within `sy_in`;
`apply_exact_sy_in_trade_or_panic` only rejects `required_sy > sy_in`. Nothing checks
that the cost is *close to* `sy_in`, and the surplus is absorbed by `reconcile_reserves`
as an LP donation. Independent of C1 — reproduces at R = 1.0.

The `exchange_rate >= WAD` floor caps a single PT purchase at **7.65% of the PT reserve**
at market open, so the cap binds early:

| `sy_in` | `pt_out` | required SY | **donated to LPs** |
|---|---|---|---|
| 100 | 76.50 | 76.58 | **23.42 (23.4%)** |
| 200 | 76.50 | 76.58 | **123.42 (61.7%)** |
| 500 | 76.50 | 76.58 | **423.42 (84.7%)** |

`AUDIT.md:148` characterizes this as "dust accrues to LPs". It is not dust and it is not
bounded. `min_pt_out` does not defend against it because `quote_sy_for_pt` returns the
same capped `pt_out`. Fix is a refund of `sy_in − required_sy`, or a revert when the
residual exceeds a caller-supplied tolerance.

### H3. One atomic trade-and-reverse sets the TWAP to an arbitrary value

`contracts/amm/src/lib.rs:1121-1147` (`sync_twap`)

`sync_twap` blends the **post-trade** rate in with `weight = elapsed / twap_window`,
where `elapsed` is the interval during which the **pre-trade** rate was actually in
effect. A correct accumulator weights the *previous* rate over the elapsed interval.
Compounding it, `elapsed == 0` returns early (`:1125`), so a same-ledger reversal is
free, and the `elapsed >= twap_window` warm-up re-entry (`:1136-1137`) does not fire at
`elapsed = window − 1`.

With `twap_window = 1800`, wait until 1799s after the last trade (an idle market is the
normal case) and, in one transaction: swap 100 PT for SY — spot implied APY moves
764 → 1748 bps, `weight = 0.99944`, TWAP becomes ≈1747 bps, and `twap_warming_up()`
stays **false**. Then reverse at the same timestamp: `elapsed == 0`, TWAP untouched,
spot restored. **Cost: 1.01 SY on a 1000-SY pool (0.10%).** 300 PT reaches 3736 bps for
0.8%; 600 PT reaches 7055 bps.

`ARCHITECTURE.md:264-269` markets this TWAP to external lending protocols as
manipulation-resistant collateral pricing, citing the February 2026 YieldBlox oracle
exploit. At these costs that claim does not hold. Fixing it needs a real cumulative-price
accumulator, not a change to the `elapsed == 0` branch.

### H4. A YT transfer during a rate dip re-opens an already-paid yield interval

`contracts/yt-token/src/lib.rs:438-447` (`settle_into_ledger`), specifically `:441-444`

```rust
let last = match Self::read_checkpoint(env, holder) {
    Some(c) => c,
    None => { Self::write_checkpoint(env, holder, rate); return; }   // :441-444
};
if rate <= last { return; }                                          // :447
```

The checkpoint is correctly held as a high-water mark for an *existing* holder on a dip
(`:447`), but a receiver with **no** checkpoint is initialized to the *current, dipped*
rate. Transferring YT to a fresh address during a dip therefore resets that YT's
high-water mark downward and makes the sender's already-paid interval claimable a
second time.

Reproduced by the auditor against the integration harness (7-dec units):

1. Alice splits 100 UNIT at rate 1.00. Rate → 1.10. Alice claims **90,909,090**;
   her checkpoint advances to 1.10.
2. Rate regresses to 1.05. Carol splits a fresh, fully-funded position (checkpoint 1.05).
3. Alice transfers her fully-paid-up YT to a fresh address she controls → checkpoint 1.05.
4. Rate merely returns to 1.10 — no new yield beyond what Alice was already paid.

`escrow = 1,861,471,862`, `reservation = 1,818,181,818`, `surplus = 43,290,044`, and
**two** claims of `43,290,042` are outstanding against it. Bob claims first and takes it;
Carol receives 2 stroops, and after PT redemption drains escrow her later claim returns 1.
Carol's entire entitlement transfers to Bob. The repo's own invariant fails:

```
assertion failed at economics.rs:185
escrow 2047619048 asset units must cover PT+YT claims 2095238091
```

Self-profitable and repeatable: claim at a peak, wait for any dip, self-transfer to a
fresh address, claim again on recovery. The repo already treats sub-stroop regressions as
routine (`economics.rs:695`, "the Blend rounding notch"), and M7 below shows Blend can
produce a *large* one. `conservation_holds_across_random_sequences` misses it because
`economics.rs:983-985` only ever bumps the rate upward.

Fix: on transfer/mint, initialize the receiver's checkpoint to
`max(current_rate, sender's checkpoint)`, or move to a global yield-index accumulator.

### H5. `max_withdraw` counts idle underlying that `withdraw` can never deliver

`contracts/strategy-blend/src/lib.rs:153-167` vs `:209-244`

`max_withdraw` returns `idle + min(supplied, pool_cash)` and its comment asserts "idle
underlying is always realizable" — but `withdraw` only submits a request to Blend and
measures the idle *delta* around that call, so pre-existing idle is never paid out and is
unreachable by any entrypoint. A strategy holding 100 USDC idle with a zero Blend position
reports `max_withdraw = 1_000_000_000` while every `withdraw` reverts `WithdrawalFailed`.
There is no sweep and no rescue. Fixing this also makes any C2 donation harmless-but-
recoverable instead of stranded-and-rate-poisoning.

### H6. V2 burns the full `sy_amount` on a short delivery; V1 burned only the delivered portion

`contracts/sy-vault-v2/src/lib.rs:395-417` vs `contracts/sy-wrapper/src/lib.rs:571-584`

V1 recomputes `burn = mul_div(received, WAD, rate)` when `received < requested`, so a
short fill consumes only the SY it actually paid for. V2 burns all of `sy_amount` and
accepts whatever arrives so long as it clears `min_underlying_out`. With the SDK's default
`minUnderlyingOut = 0n` (`sdk/src/client.ts:566`), a user redeeming 60 SY against a
50-USDC-realizable position burns **60 SY for 50 USDC**, gifting 10 SY to the remaining
holders.

The comment at `sy-vault-v2:387-389` claims "a short delivery reverts the whole
transaction, so there is no state in which SY is burned without the matching underlying
arriving." That is true only when `min_underlying_out` is set tightly, which neither the
SDK default nor the trait-level `redeem` (`:698-700`) does.

`v1_and_v2_agree_over_identical_blend_state` misses this — it only exercises fully
satisfiable redemptions.

---

## MEDIUM

### M1. The maturity freeze pins the last *pre-maturity* observation, so PT is paid above face — realized on mainnet right now

`contracts/tokenizer/src/lib.rs:565-583` (`effective_rate`), `:124-129` (`observe_rate`,
gated `require_live`)

`MaturityRate` is set to `LastObservedRate`, and `observe_rate` reverts once matured, so
the frozen rate is whatever was last recorded strictly before maturity. PT redeems
`floor(pt · WAD / R_frozen)` **shares** worth `R_live` each, so a stale `R_frozen` hands
PT holders shares worth `pt · R_live / R_frozen` assets — more than face, funded entirely
out of YT's yield.

The design is sound in one important respect: the freeze deliberately refuses to read a
live post-maturity rate, so freeze *timing* cannot move value and delay costs nothing.
The exposure is the unobserved tail, and **PT has no rate path at all**, so PT↔SY AMM
swaps — the primary market — never refresh the observation.

**Measured live on mainnet (read-only simulation, 2026-08-17):**

| Reading | Value |
|---|---|
| `tokenizer.is_matured()` | `true` (matured 2026-08-09 15:39 UTC) |
| `tokenizer.maturity_rate()` | **0 — never frozen, 8 days on** |
| `freeze_maturity_rate()` simulated | **1.000131105127269445** |
| `sy.exchange_rate()` live | **1.007631141108698867** |
| escrow / PT supply / YT supply | 50,796,162 / 50,798,215 / 50,798,215 |

The frozen value implies the last observation dates from roughly **2026-07-11**, a day
after deployment. Settlement will pay YT approximately 4,600 stroops against a true
accrual near 30,000 — about 2% — with the remainder going to PT. **This is already
unrecoverable**: `observe_rate` cannot run post-maturity, so the value is locked in
whether you freeze today or never.

Scale: the whole market is ~$10.70. The deployer key holds 96% of the YT and the AMM
holds 96% of the PT as liquidity, so the misdirected yield is a fraction of a cent moving
largely between the operator's own positions. The damage is to the evidence, not the
treasury.

### M2. The keeper structurally cannot see the mainnet market, and two ledger entries expire in November

`scripts/keeper.mjs:95-111` (discovery), `:224-236` (TTL duty), `deployments/markets/`

The keeper walks `deployments/markets/<network>/*.toml`, which contains **only the two
testnet markets**. The mainnet market still lives in the legacy flat
`deployments/mainnet.toml`, so the only automation that calls `observe_rate` and
`freeze_maturity_rate` has never looked at it — the root cause of M1. Compounding it, the
TTL duty calls `sy.touch()`, which exists only on `sy-vault-v2:289` and not on the
deployed V1 wrapper.

The same failure hit testnet: `blend-usdc-testnet-short` matured 2026-08-16 and is also
unfrozen (`maturity_rate` = 0, simulated freeze = exactly 1.0). That was the explicit
dated gate in `V2_REMAINING_WORK.md` Phase 2 — "the keeper runs unattended across the
2026-08-16 maturity and freezes the maturity rate without human intervention." Nothing
schedules it; CI has no cron, and `keeper_configured = 0` in both manifests.

**Measured TTL:** the vault instance entry expires at ledger 65,488,004 and Blend's
`Positions(vault)` entry at 65,489,595 — both ≈ **2026-11-11**. `deployments/mainnet.toml:60`
says "no renewal needed for a one-shot cycle", true only if all funds exit first. Funds
are not lost if they lapse (`RestoreFootprint` is permissionless), but the app offers no
restore path. `approve(self, self, 0, 0)` renews the vault instance for free; only a real
deposit or redeem renews Blend's entry.

### M3. Deployed testnet bytecode cannot be reproduced from the commit it claims

`scripts/deploy-market.sh:177`

```sh
dirty="$(git status --porcelain --untracked-files=no)"
```

`--untracked-files=no` excludes untracked files from the dirty check, so the entire V2
contract source — `strategy-blend`, `sy-vault-v2`, `strategy-interface`, all still
untracked — sails through the "tracked source is dirty" gate. The script then stamps
`source_commit = $(git rev-parse HEAD)` into the manifest. Both V2 market manifests claim
`source_commit = 784528f…`, and `git ls-tree` confirms that commit contains none of the
three contracts.

This is the same class of problem `AUDIT.md` flagged historically ("the deployed bytecode
does not correspond to any committed source"), recurring in new code. Fix: drop the flag,
or fail when untracked files exist under `contracts/`.

### M4. Re-using an address forfeits yield on newly acquired YT

`contracts/yt-token/src/lib.rs:447`, with `mint` (`:202-224`) and `transfer` (`:270-279`)

The mirror image of H4. The checkpoint is a single per-address high-water mark, so a
holder settled at a high rate who then acquires new YT at a lower rate inherits the high
checkpoint on the *entire* balance and earns nothing until the rate exceeds the old peak.
Alice, splitting a second fully-funded position after a dip, ends with `owed = 0` on
2,095,454,544 YT while Dave doing the identical thing from a clean address is owed
43,290,042. The forfeited value is stranded in escrow with no recovery path. Same one-line
fix as H4.

### M5. There is no fee anywhere, and V2 is immutable, so the decision must precede deploy

The only fee in the protocol is the AMM's `fee_bps` (10 on mainnet, confirmed on-chain),
applied at `amm/src/lib.rs:837`, `:967`, `:993`, and retained in reserves — it accrues to
LPs pro-rata via `reconcile_reserves` (`:682-685`). Every other path charges zero: split,
recombine, redeem, claim_yield, SY deposit/redeem, strategy deposit/withdraw, add/remove
liquidity.

Grepping the whole repo for `treasury`, `fee_recipient`, `protocol_fee`, `collector`,
`admin_fee`, `performance_fee` returns **no hits in any contract**. There is no sweep,
skim, or collect entrypoint. The live mainnet AMM `Config` read off-chain is
`{admin, fee_bps: 10, initial_anchor, maturity, pt_token, scalar_root, sy_token,
tokenizer, twap_window, yt_token}` — **there is no fee-recipient field to point anywhere.**

`ARCHITECTURE.md:357` still files "Fee distribution … leaning toward (a) for simplicity"
under *open* design questions, a month after it shipped irreversibly.

**Timing.** `sy-vault-v2/src/lib.rs:43-46` states the vault "exposes no admin entrypoint
at all", and the code matches — `admin` is written once at `:143` and never read again.
No contract in the repo has `update_current_contract_wasm`. So a fee cannot be retrofitted
to a deployed V2 market; it must be compiled in and set at `initialize`. Since the AMM is
being redeployed anyway for C1, that redeploy is the free window.

**Where a fee can safely go.** The clean insertion point is `tokenizer/src/lib.rs:394`,
skimming from `pay` — the value *after* the PT-senior cap:

```rust
let pay = if owed < surplus { owed } else { surplus };   // existing :394
let fee = mul_div_floor(&env, pay, config.yield_fee_bps, BPS)?;
consume_yt(&env, &config.yt_token, &holder, pay);        // consume gross
push_token(&env, &config.sy_token, &holder, pay - fee);
push_token(&env, &config.sy_token, &config.fee_recipient, fee);
```

Because `pay` is already the junior surplus, the fee structurally cannot reach
`pt_face_reservation` — PT stays senior to the protocol fee for free. SY moves between
escrow and the recipient without changing `total_assets` or `total_sy_supply`, so the
derived rate is provably untouched. Roughly fifteen lines; `mul_div_floor` already exists
at `:473`. Taking the fee from `owed` (pre-cap) instead would make the protocol compete
with YT holders during a shortfall — don't.

A vault-side withdraw fee (`sy-vault-v2:417`) is also invariant-safe. **Do not** implement
a yield fee as periodic share dilution or an asset skim: both step the exchange rate down,
and the tokenizer reads a rate decline as a strategy slash.

Sizing context (industry figures approximate): at $1M TVL and 8% APY a 3–5% performance
fee earns roughly $2.4–4k/yr, versus ~$600/yr for a 20% AMM swap-fee split — and it scales
with TVL rather than with trading volume Stellar does not yet have. Both are dwarfed by
C1, which leaks 2–7% of pool reserves per arbitrage.

### M6. The YT routes charge `fee_bps` on the leveraged PT face

`contracts/amm/src/lib.rs:993`

The YT route's curve leg is sized by PT face, 30–75× the SY the buyer risks, and the fee
is charged on that notional. Measured at R = 1.0: a 1.00 SY buy pays an effective
**5.02%**; a 10.00 SY buy pays **1.64%**. `ARCHITECTURE.md:275` advertises "0.3% on YT
swaps (which go through two pool operations)" — the YT routes make **one** curve call each
(`:413-414`, `:457`) and the tokenizer leg is free, so the advertised figure is wrong in
both directions: the nominal rate is 10bps, and the realized incidence is an order of
magnitude above 0.3%.

### M7. The "rate is monotonic" invariant is asserted in comments and tests but is false

`contracts/blend-adapter/src/lib.rs:136-139`, test at `:196-206`

The comment claims "a Blend position whose value only rises with accrued interest… no
admin can lower it, which is the whole point", and
`derived_rate_is_monotonic_as_interest_accrues` is documented as "the property that makes
`#9` impossible."

Blend v2 socializes bad debt by **decreasing `b_rate`**: `User::default_liabilities`
(`pool/src/pool/user.rs:110-115`) does `b_rate -= b_rate_loss`, reachable through
`bad_debt(user)`, a **permissionless** pool entrypoint (`contract.rs:591-595`) that fires
when the backstop falls below ~5% of threshold, and it hits all bToken holders including
plain supply positions.

The SY wrapper itself handles a decline correctly and stays solvent. The risk is
downstream: `yt-token` rejects a rate below its checkpoint (`ExchangeRateRegression`) and
the AMM has `ExchangeRateBelowOne`/`InvalidSyRate`, so a dip can halt YT claims and AMM
quotes. It is also the trigger that turns H4 from a rounding-notch leak into a large one.
Not attacker-inducible; requires a genuine Blend credit event.

### M8. SY is a live mainnet token that is not SEP-41 conformant and emits no events

`contracts/sy-wrapper/src/lib.rs:236-293`; the only `#[contractevent]` in the crate is
`ReserveMigrated` (`:77-82`)

No `burn` or `burn_from` (both required by SEP-41; `pt-token:192,199` has them). And
`transfer`, `transfer_from`, `approve`, and the mint/burn inside `deposit`/`redeem`
publish **no events at all**. Wallets, explorers, portfolio trackers, and tax tools that
index SEP-41 transfers see SY balances as permanently zero. A composing contract calling
`sy.burn(...)` per the standard reverts with "function not found". Permanent, since the
contract is immutable. Fix in `sy-vault-v2` before it ships.

### M9. `deposit_cap` is decorative

Every market manifest records a `deposit_cap`; grepping all contracts shows **nothing
reads it**. The manifest asserts a limit the protocol does not keep. Already noted in
`V2_REMAINING_WORK.md:0.2`, still open. Enforce in `sy-vault-v2::deposit` against
`total_assets()` after the strategy credits.

### M10. The two Blend-custody error codes are unmapped in the frontend

`app/lib/errors.ts:26-32` vs `contracts/sy-wrapper/src/lib.rs:59-72`

The `sy` error map covers 3, 4, 5, 6, 12 but omits `InvalidBlendReserve = 10` and
`BlendWithdrawalFailed = 11` — precisely the two codes only reachable under Blend custody.
During a liquidity squeeze a redeeming user sees `Transaction failed (error #11).` with no
guidance. Free to fix, off-chain, and it is the message a maturity-window user is most
likely to see.

### M11. The live admin is the transient deploy key, not the designated wallet

`deployments/mainnet.toml:11-12` vs `docs/deploy/MAINNET_PARAMETERS.md:84-85`

The plan designates a mobile wallet as admin and says the CLI deploy signer "holds no
admin power." On-chain, `sy.config().admin` is the deployer key `GDQX3RT7…YRG3`, i.e. the
hot key in the local `stellar-cli` keystore. Contract damage in either direction (theft or
loss) is genuinely **zero** — see "Verified sound" below — but the deployment's documented
security model does not describe the deployment, which matters for the next one. There is
no `set_admin`, so it cannot be corrected on this contract.

---

## LOW

- **L1 — `initialize` is front-runnable on every contract.** No `__constructor`; deploy and
  initialize are separate transactions (`scripts/deploy-market.sh:239-257`). An attacker can
  claim the contract with their own admin/sy_token/tokenizer. Loud rather than silent (the
  operator's init then fails `AlreadyInitialized` under `set -e`), but the contract is
  bricked and must be redeployed. Use the SDK `__constructor`.
- **L2 — the PT→SY fee floors to zero for trades under 1000 stroops** (`amm:837` uses
  `mul_div_down` while `:967` uses `mul_div_up`). Verified *not* drainable — round trips lose
  ≥1 stroop in every configuration tested including `fee_bps = 0` — and chunking costs more in
  transaction fees than it saves. Both directions should round the fee up.
- **L3 — `preview_claim_yield` over-quotes post-maturity before the freeze**
  (`yt-token:394-409`): with `maturity_rate()` still 0 it falls back to the live,
  still-accruing rate, the one value `effective_rate` exists to never use. View-only.
- **L4 — `owed_shares` is the one value path not hardened to I256** (`yt-token:471-485`);
  a `MathOverflow` there panics rather than returning an error, bricking transfer/burn/claim
  for that holder. Thresholds are far outside a realistic 7-decimal market.
- **L5 — `maturity` is stored three times with no cross-check** (`pt-token:65`, `yt-token:79`,
  `tokenizer:75`). Operationally mitigated by a single `$MATURITY` in the deploy script;
  nothing on-chain enforces it, and a factory-deployed market could diverge.
- **L6 — deposit can over-credit by ≤1 stroop** from double-flooring the AUM delta
  (`sy-wrapper:516-521`). Economically unreachable.
- **L7 — no `min_shares_out` on V1 deposit and no `min_underlying_out` on V1 redeem**
  (`sy-wrapper:487`, `:538`). The rate moves every ledger, so a caller cannot bind either.
  Benign for humans; a composing contract has no floor.
- **L8 — unreachable partial-fill branch rounds the wrong way** (`sy-wrapper:579-583`);
  should be `ceil`. Recorded so it is not carried into V2.
- **L9 — USDC sent directly to the V1 vault is permanently stranded** (no sweep, no admin
  rescue). Live idle balance is currently 0.
- **L10 — the last redeemer strands a few stroops in Blend**, which become a windfall for the
  next depositor via the `sy_supply <= 0` bootstrap branch. Post-maturity there is no next
  depositor.
- **L11 — BLND emissions on the USDC supply reserve would be unclaimable** (no `claim`
  entrypoint). Verified `get_reserve_emissions(3)` returns null — emissions are not configured,
  so nothing is being lost today.
- **L12 — wrong typed error**: `strategy-blend:271` returns `NotVault` for an admin-auth
  failure; `StrategyError` has no `NotAuthorized` variant.
- **L13 — dead auth entry**: `amm:1112` authorizes `yt_token.burn`, but `tokenizer::recombine`
  burns via the tokenizer-gated `burn_settled`. Never matched.
- **L14 — `zap_out_at_maturity` is documented in `ARCHITECTURE.md` but exists in no contract
  and no SDK path.**

---

## Verified sound

Listed so they are not re-audited.

**V1 SY wrapper.** `set_exchange_rate` is **permanently dead** — gated on
`config.pool.is_some()` (`:149-151`), and live config reads `pool = Some(CAJJZSGM…)`.
`pool` can never become `None`: the only post-init writer is `migrate_reserve_index`, which
mutates only `reserve_index`. Re-initialization is blocked. The `#9 Insolvent` root cause is
structurally fixed, not merely deprecated. **A compromised admin key can do nothing.**
`migrate_reserve_index` takes **no index parameter**, re-derives from the pool, and accepts
only the same underlying — and Blend v2 reserve indices are provably immutable
(`initialize_reserve` discards the supplied index; the reserve list is append-only), so the
function is a proven no-op. Deposit mints against measured credited assets; the accrual-timing
theft I checked for does not exist (Blend accrues in memory on load and does not store, and
Soroban's ledger timestamp is constant within a transaction — verified empirically with two
reads 78s apart). Redeem verifies delivery and is CEI-correct. Donation/first-depositor
inflation is closed at three independent levels. Illiquidity is a **hard revert, not a partial
fill**, and no Blend pool status can block a plain type-1 withdraw — Blend's admin cannot
freeze this vault's exit. Plain supply never enters the health factor and is not seizable in a
liquidation. Deployed code hash `8c51e655…9eb9` matches the manifest.

**Tokenizer solvency.** The invariant `E >= (P−Y)·WAD/r + B + Σ b_h·WAD/c_h` holds under a
monotonically non-decreasing rate. `split` is collateral-neutral-or-better; `recombine` and
`redeem_at_maturity` floor the payout and cap it pro-rata, so early exiters cannot drain later
ones. In `claim_yield` the PT-senior reservation **is** computed before the payout and against
a fresh rate (`:374`, `:385-388`), and rounds *up* so PT is never shorted by a notch. Rounding
is uniformly protocol-favoring; a split→recombine round trip is strictly lossy in both
directions, so repeated tiny operations cannot mint value. Donating SY to the tokenizer only
raises coverage. H4 is the sole path that breaks the side condition.

**Auth, everywhere.** `pt.mint`, `yt.mint`, `yt.settle`, `yt.consume`, `yt.burn_settled` all
gate on `config.tokenizer.require_auth()`, and the tokenizer has no `__check_auth`, so only
the tokenizer's own frame can satisfy them — **no external caller can corrupt YT accounting**.
Every `authorize_self_call` is argument-pinned with empty `sub_invocations` and re-issued
immediately before each invoke. The AMM's `Market` trait impl carries no `#[contractimpl]`, so
the min-out-free trait signatures are not externally callable. `strategy-blend`'s
`require_vault` checks address equality *and* `require_auth`, so it holds even under
`mock_all_auths` and an EOA cannot forge it. V2 auth trees are argument-pinned with empty
sub-invocations.

**AMM, everything except C1/H1/H2/H3.** Round trips lose ≥1 stroop in every configuration
tested (11 sizes × 2 fee settings) because both directions price the whole lot at the
post-trade endpoint rate. Rounding directions all favor the pool. LP-share invariants hold and
first-LP inflation strictly loses money for the attacker. Flash-route atomicity is sound — both
routes reconcile to actual balances and fail closed. Behavior at and after maturity is correct:
swaps and `add_liquidity` are blocked, `remove_liquidity` deliberately is not, so LPs can exit
pro-rata (relevant now, since 96% of mainnet PT sits in the pool). **No quote/execute mismatch**
— all four `quote_*` call the identical helpers. **No fee bypass by routing**: buying PT
directly and synthesizing it via split + `swap_yt_for_sy` both pay one fee on one comparable
PT leg.

**V2 seam design rules that are genuinely implemented.** No rate setter anywhere. Deposit mints
against measured deltas and redeem trusts the vault-side balance diff over the strategy's own
report. Bindings immutable. `touch` permissionless. Rounding rounds toward the vault on both
sides, with no accumulation attack. Blend valuation math matches `to_asset_from_b_token`, and
reserve-index handling re-derives from the pool and refuses to price the wrong reserve.
Insufficient Blend liquidity reverts cleanly.

**CI.** Has a wasm float-opcode gate (globbed, so new contracts are covered automatically), a
reproducible-build check, and market-registry drift detection. It does not schedule the keeper.

---

## Recommended order

1. **C2 + H5** — same code area. Kill the donation vector and make idle payable in `withdraw`.
   Launch blocker for V2; the market is exploitable by the second depositor on day one.
   Add the first-depositor and direct-donation tests that `MULTI_STRATEGY.md:279` already
   names as required, before the fix, so it is proven rather than asserted.
2. **C1 + H1** — one root cause (`amm:799`), one fix: value the SY reserve at the index inside
   the curve and convert outputs back to shares. Re-run the property suite at
   R ∈ {1.0, 1.01, 1.05, 1.1} across the plain PT↔SY legs *and* both YT routes; the existing
   10,000-case suite is pinned to R = 1.0 where the defect vanishes identically.
   `scripts/amm-curve-model.py` is the bit-exact model used for the figures above and can serve
   as the regression harness.
3. **H4 + M4** — one line in `yt-token`, plus a rate-*regression* arm in the random-sequence
   test. M7 shows the trigger is real, not hypothetical.
4. **H2, H3, H6** — independent and individually cheap.
5. **M2** — register the mainnet market with the keeper, teach it the V1 `approve(0,0)`
   fallback, and schedule it. Set a hard alarm at ledger 65,000,000 (~2026-10-14) if any funds
   will remain past October. Cleanest alternative: wind the mainnet market down — it matured
   nine days ago and holds $10.70, and redeeming closes M2, L10 and L11 outright.
6. **M5** — decide the fee before the first V2 `initialize`. It is unmakeable afterward.
7. **M3** — drop `--untracked-files=no`, or fail on untracked files under `contracts/`.
   Commit the V2 seam so the manifests' provenance claims become true.
8. **M10** — two lines in `app/lib/errors.ts`. Free.
9. **Docs reconciliation** — `ARCHITECTURE.md:275` (YT fee), `:357` (fee distribution is not
   open, it shipped), `:264-269` (TWAP manipulation resistance, per H3), `zap_out_at_maturity`
   (L14), `blend-adapter:136-139` (monotonicity, per M7), `mainnet.toml:60` (TTL) and `:11-12`
   (admin, per M11), and `REMEDIATION.md:25` (L1 landed; checkbox is stale).
