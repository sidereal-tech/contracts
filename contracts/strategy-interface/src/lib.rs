// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

//! The strategy seam: the one ABI every Sidereal yield source implements.
//!
//! V1 compiled Blend directly into the SY wrapper (`Config.pool: Option<Address>`
//! plus `if pool.is_some()` branches through deposit, redeem, and the rate path).
//! That made a second yield source a fork of the vault. This crate replaces that
//! coupling with a contract-to-contract boundary: `sy-vault-v2` holds one
//! immutable strategy address and knows nothing about what is behind it.
//!
//! Two properties are promoted from Blend implementation details to obligations
//! of the seam itself, so every future adapter inherits them instead of
//! re-deriving them:
//!
//! 1. **`deposit` and `withdraw` return measured deltas, never requested
//!    amounts.** An upstream that floors in its own favour can then never mint SY
//!    the position does not back — the credited-delta rule the V1 wrapper applied
//!    to Blend's bToken rounding.
//! 2. **`max_withdraw` is separate from `total_assets`.** A strategy can value
//!    assets it cannot presently pay out. Collapsing the two would let a market
//!    quote a redemption it cannot honour.
//! 3. **`total_assets` values only assets the strategy itself put to work.**
//!    Underlying that merely *sits* at the strategy's address — anyone can send
//!    it there with an ordinary SAC transfer, no auth and no vault interaction —
//!    must not enter the valuation. Counting it makes the exchange rate a
//!    function of a permissionless token transfer, which is a donation/
//!    first-depositor inflation vector straight through to the tokenizer's
//!    PT reservation. The V1 wrapper valued only its bToken position for exactly
//!    this reason; the seam now states it as an obligation.
//!
//!    Exclusion must survive withdrawal too. If an adapter spends unvalued idle
//!    while the vault burns shares, it must reduce its valued position by the
//!    same amount; otherwise the smaller denominator turns the donation into a
//!    delayed rate increase. `strategy-blend` records that reduction as
//!    excluded supplied assets.

use soroban_sdk::{contractclient, contracterror, Address, Env};

/// Asset-per-share fixed-point scale (18 decimals), shared by every vault and
/// strategy. Matches `sy-wrapper`'s `WAD` so V1 and V2 rates are comparable.
pub const WAD: i128 = 1_000_000_000_000_000_000;

/// The contract surface a yield source must expose to be usable as a Sidereal
/// strategy. `#[contractclient]` generates `YieldStrategyClient`, which the SY
/// vault uses for every cross-contract call, including `try_` variants.
///
/// Implementations must uphold:
///
/// - `deposit` / `withdraw` accept calls **only** from the vault address fixed at
///   initialization, and return the *actual* change in assets.
/// - `total_assets` values only what the strategy can eventually realize in the
///   underlying. Unclaimed or unsold reward tokens are excluded — counting them
///   would move the exchange rate on a claim's timing rather than on accrual.
///   **Underlying held idle at the strategy's own address is excluded too**: it
///   can be put there by anyone, so counting it would make the rate settable by
///   a plain token transfer.
/// - `max_withdraw` reports presently realizable liquidity, which may be less
///   than `total_assets`.
/// - `touch` is permissionless and renews the upstream persistent entry.
#[contractclient(name = "YieldStrategyClient")]
pub trait YieldStrategy {
    /// The underlying asset this strategy consumes and returns.
    fn underlying(env: Env) -> Address;

    /// The SY vault allowed to call `deposit` and `withdraw`. Fixed at init.
    fn vault(env: Env) -> Address;

    /// Underlying the strategy's whole position is currently worth, in the
    /// underlying's own decimals. Excludes unconverted rewards.
    fn total_assets(env: Env) -> i128;

    /// Underlying that could be withdrawn right now, bounded by upstream
    /// liquidity. Always `<= total_assets`.
    fn max_withdraw(env: Env) -> i128;

    /// Moves `amount` of underlying from `vault` into the yield source.
    /// Returns the measured increase in `total_assets`, which may be less than
    /// `amount` when the upstream rounds in its own favour.
    fn deposit(env: Env, vault: Address, amount: i128) -> i128;

    /// Withdraws underlying worth `amount` back to `vault`. Reverts if the
    /// amount actually delivered is below `min_underlying_out`. Returns the
    /// underlying actually delivered.
    fn withdraw(env: Env, vault: Address, amount: i128, min_underlying_out: i128) -> i128;

    /// Permissionless upkeep: reads the upstream position so its persistent
    /// entry is renewed, and bumps the strategy's own TTLs.
    fn touch(env: Env);
}

/// Errors shared by every strategy implementation. Codes are stable across
/// adapters so the SDK, keeper, and conformance battery can assert on them
/// without knowing which yield source is behind the seam.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum StrategyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Amount was zero or negative.
    InvalidAmount = 3,
    /// Caller is not the vault fixed at initialization.
    NotVault = 4,
    MathOverflow = 5,
    /// The upstream protocol rejected or short-filled a withdrawal.
    WithdrawalFailed = 6,
    /// Delivered underlying was below the caller's `min_underlying_out`.
    SlippageExceeded = 7,
    /// The upstream position no longer matches the configuration pinned at init
    /// (reserve reindex, asset-count change, unexpected vault shape).
    UpstreamMismatch = 8,
    /// The upstream protocol is paused or otherwise not accepting deposits.
    UpstreamPaused = 9,
}

/// The SY exchange rate (asset-per-share, WAD) derived from a strategy's real
/// holdings: `total_assets * WAD / sy_supply`.
///
/// This is the *only* way an SY vault may obtain a rate. There is deliberately
/// no setter anywhere in V2 — the `#9 Insolvent` root cause was an admin-set
/// rate, and a derived-only rate cannot re-enter through a new adapter.
///
/// Two degenerate inputs return `None` rather than a number:
///
/// - **Overflow.** `total_assets * WAD` beyond `i128`.
/// - **Assets with no shares.** The bootstrap branch returns `WAD` only when the
///   position is *also* empty. A vault whose supply has returned to zero while
///   assets remain has no meaningful asset-per-share, and answering `WAD` would
///   hand the whole residual position to the next one-stroop depositor. The
///   caller must fail closed instead. `sy-vault-v2` additionally makes the state
///   unreachable by locking a minimum share balance on the first deposit, so
///   this is a backstop, not the primary defense.
pub fn derived_exchange_rate(total_assets: i128, sy_supply: i128) -> Option<i128> {
    if sy_supply <= 0 {
        return if total_assets <= 0 { Some(WAD) } else { None };
    }
    total_assets.checked_mul(WAD).map(|value| value / sy_supply)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test {
    use super::*;

    const UNIT: i128 = 10_000_000; // 1.0 at 7 decimals, like Stellar USDC.

    #[test]
    fn rate_bootstraps_to_wad_only_when_the_position_is_also_empty() {
        assert_eq!(derived_exchange_rate(0, 0).unwrap(), WAD);
        assert_eq!(derived_exchange_rate(-1, 0).unwrap(), WAD);
    }

    /// Residual assets with no shares outstanding is not a bootstrap — it is a
    /// broken vault. Answering `WAD` would sell the whole position to the next
    /// one-stroop depositor, so the rate is undefined and the caller must fail
    /// closed.
    #[test]
    fn assets_with_no_supply_have_no_rate() {
        assert_eq!(derived_exchange_rate(50 * UNIT, 0), None);
        assert_eq!(derived_exchange_rate(1, 0), None);
        assert_eq!(derived_exchange_rate(1, -5), None);
    }

    #[test]
    fn rate_tracks_assets_over_supply() {
        assert_eq!(
            derived_exchange_rate(110 * UNIT, 100 * UNIT).unwrap(),
            1_100_000_000_000_000_000
        );
    }

    #[test]
    fn rate_rises_only_with_assets() {
        let supply = 100 * UNIT;
        let r0 = derived_exchange_rate(100 * UNIT, supply).unwrap();
        let r1 = derived_exchange_rate(105 * UNIT, supply).unwrap();
        assert!(r1 > r0, "rate must rise with assets: {r0} {r1}");
    }

    #[test]
    fn overflow_is_reported_not_panicked() {
        assert_eq!(derived_exchange_rate(i128::MAX, 1), None);
    }
}
