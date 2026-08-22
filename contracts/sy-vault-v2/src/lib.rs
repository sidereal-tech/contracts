// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

//! Standardized Yield vault, V2 — yield-source agnostic.
//!
//! V1 (`sy-wrapper`) compiled Blend in: a `pool: Option<Address>` field and an
//! `if pool.is_some()` branch through deposit, redeem, and the rate path. This
//! vault holds one **immutable strategy address** and calls the `YieldStrategy`
//! ABI. It never learns what is behind the seam, which is what makes a second
//! yield source an adapter rather than a fork of the vault.
//!
//! Everything above SY is unchanged. The tokenizer, YT, and AMM read
//! `exchange_rate()` by symbol and move SY as a SEP-41 token; both surfaces are
//! wire-identical to V1, so the seniority and shortfall machinery is
//! adapter-agnostic by construction.
//!
//! **The rate stays derived, never set.** There is no rate setter anywhere in
//! this contract — the `#9 Insolvent` root cause cannot re-enter through a new
//! adapter:
//!
//! ```text
//! exchange_rate = strategy.total_assets() * WAD / total_sy_supply
//! ```
//!
//! Two things keep that quotient honest, because a derived rate is only as
//! trustworthy as its numerator and denominator:
//!
//! - The numerator is the strategy's obligation: `total_assets` values only the
//!   position the strategy itself put to work, never underlying that merely sits
//!   at its address, which anyone can put there with a plain token transfer.
//! - The denominator can never return to zero. [`MINIMUM_SHARES`] are minted to
//!   nobody on the first deposit and are unowned forever, so the market cannot be
//!   re-bootstrapped at `WAD` against a residual position, and no depositor can
//!   ever hold the entire supply — the two preconditions of a first-depositor
//!   inflation attack.
//!
//! Excluding idle must remain true after a redemption uses it. The Blend
//! strategy records every unit of unvalued idle it spends as an equal exclusion
//! from its supplied position, so `total_assets` and share supply fall together.
//! A donation can provide temporary liquidity, but it cannot create a delayed
//! exchange-rate step for the AMM or maturity freeze to observe.

use sidereal_shared_types::StandardizedYield;
use sidereal_strategy_interface::{derived_exchange_rate, YieldStrategyClient, WAD};
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    vec, Address, Env, IntoVal, MuxedAddress, String, Symbol,
};

/// Display decimals for SY, matching the 7-decimal underlying.
const DECIMALS: u32 = 7;

/// Shares minted to nobody on the very first deposit and never redeemable.
///
/// The same device, and the same constant, as the AMM's `MINIMUM_LIQUIDITY`
/// (`contracts/amm/src/lib.rs:18`). It buys two properties:
///
/// 1. **The supply never returns to zero.** Once a market has been opened, its
///    exchange rate is always a real quotient. Without this, the last redeemer
///    leaves residual assets behind an empty supply and the bootstrap branch
///    hands the whole position to the next one-stroop depositor.
/// 2. **Nobody ever owns the entire supply.** A first-depositor inflation
///    attack needs the attacker to hold ~100% of the shares while they raise the
///    rate; the locked shares dilute every recovery by `MINIMUM_SHARES /
///    total_supply`, so the attacker's own donation is the larger part of what
///    they forfeit.
///
/// 1_000 stroops is 0.0001 of a 7-decimal underlying — a rounding error to the
/// first depositor, and roughly a 1_000× multiplier on the cost of the attack.
pub const MINIMUM_SHARES: i128 = 1_000;

/// TTL policy, matching V1 and the AMM: bump when within 30 days of expiry,
/// extend to 120 days, so a periodically-touched vault never archives mid-term.
const LEDGERS_PER_DAY: u32 = 17_280;
const TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

/// `strategy` and `underlying` are fixed at initialization. A new yield source
/// means a new market, never a rebinding under live depositors — so there is no
/// setter for either.
///
/// `admin` grants exactly one power, [`SyVaultV2::set_deposit_cap`], and it is
/// deposit-only. The admin can halt or throttle *new* deposits and can do
/// nothing else: no rate, no custody, no pause on redemption, no reach into a
/// balance. The strongest thing a compromised admin key achieves is closing the
/// front door, which is why the cap is settable at all rather than frozen at
/// init — a pilot ramp needs to move, and this is the only admin model that
/// lets it move without weakening "no admin can touch depositor funds".
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    pub underlying: Address,
    pub strategy: Address,
}

#[derive(Clone)]
#[contracttype]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[derive(Clone)]
#[contracttype]
enum DataKey {
    Config,
    TotalSupply,
    /// Ceiling on `total_assets()` after a deposit credits. Absent or `0` means
    /// uncapped, matching the `deposit_cap = 0` convention in the market
    /// manifests.
    DepositCap,
    Balance(Address),
    /// Underlying principal a holder deposited, used for accrued-yield display.
    Principal(Address),
    /// (owner, spender)
    Allowance(Address, Address),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InvalidExchangeRate = 4,
    InsufficientBalance = 5,
    MathOverflow = 6,
    InsufficientAllowance = 7,
    InvalidExpiration = 8,
    /// The strategy does not point back at this vault, or its underlying
    /// disagrees with the one supplied at init.
    StrategyMismatch = 9,
    /// The strategy delivered less underlying than the caller's minimum.
    SlippageExceeded = 10,
    /// The strategy delivered nothing, or less than it reported.
    StrategyDeliveryFailed = 11,
    /// The first deposit must exceed `MINIMUM_SHARES`, which are locked away
    /// permanently so the supply can never return to zero.
    InitialDepositTooSmall = 12,
    /// The deposit would push `total_assets()` past the admin-set cap. Deposits
    /// only — nothing else in this contract reads the cap.
    DepositCapExceeded = 13,
    /// Caller is not the admin recorded at initialization.
    NotAdmin = 14,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposited {
    pub from: Address,
    pub underlying_in: i128,
    /// Measured assets the strategy actually credited, which is what SY is
    /// minted against.
    pub assets_credited: i128,
    pub sy_out: i128,
    pub rate: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositCapSet {
    pub old_cap: i128,
    pub new_cap: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub from: Address,
    pub sy_in: i128,
    pub underlying_out: i128,
    pub rate: i128,
}

#[contract]
pub struct SyVaultV2;

#[contractimpl]
impl SyVaultV2 {
    /// Binds the vault to one strategy, permanently.
    ///
    /// The strategy must already name this vault as its own (`strategy.vault()`)
    /// and agree on the underlying. Deploy both contracts, initialize the
    /// strategy with the vault's address, then initialize the vault — the check
    /// here closes the loop and makes a mis-wired market impossible to
    /// initialize rather than merely wrong at runtime.
    pub fn initialize(env: Env, admin: Address, strategy: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();

        let client = YieldStrategyClient::new(&env, &strategy);
        if client.vault() != env.current_contract_address() {
            return Err(Error::StrategyMismatch);
        }
        let underlying = client.underlying();

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                underlying,
                strategy,
            },
        );
        env.storage().instance().set(&DataKey::TotalSupply, &0_i128);
        Self::bump_instance_ttl(&env);
        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, Error> {
        Self::read_config(&env)
    }

    /// The strategy behind this vault. Immutable.
    pub fn strategy(env: Env) -> Result<Address, Error> {
        Ok(Self::read_config(&env)?.strategy)
    }

    /// Underlying the whole vault is worth, straight from the strategy.
    pub fn total_assets(env: Env) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        Ok(YieldStrategyClient::new(&env, &config.strategy).total_assets())
    }

    /// Underlying the strategy could presently pay out. Kept separate from
    /// `total_assets` so the SDK and keeper can see a shortfall *before* a user
    /// commits to a redemption the market cannot honour.
    pub fn max_withdraw(env: Env) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        Ok(YieldStrategyClient::new(&env, &config.strategy).max_withdraw())
    }

    /// Ceiling on `total_assets()`, enforced on deposit only. `0` is uncapped,
    /// which is both the initial value and the `deposit_cap = 0` every market
    /// manifest ships with today.
    pub fn deposit_cap(env: Env) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_deposit_cap(&env))
    }

    /// Raises, lowers, or removes (`0`) the deposit cap. Admin only.
    ///
    /// This is the whole of the vault's admin surface. It gates `deposit` and is
    /// read nowhere else — not by `redeem`, not by `transfer`, not by the rate —
    /// so setting it to 1 stroop halts new deposits and still cannot trap,
    /// reprice, or redirect a single unit of what is already deposited. Existing
    /// holders exit at any cap level.
    pub fn set_deposit_cap(env: Env, admin: Address, cap: i128) -> Result<(), Error> {
        let config = Self::read_config(&env)?;
        admin.require_auth();
        if admin != config.admin {
            return Err(Error::NotAdmin);
        }
        if cap < 0 {
            return Err(Error::InvalidAmount);
        }
        let old_cap = Self::read_deposit_cap(&env);
        env.storage().instance().set(&DataKey::DepositCap, &cap);
        Self::bump_instance_ttl(&env);

        DepositCapSet {
            old_cap,
            new_cap: cap,
        }
        .publish(&env);
        Ok(())
    }

    pub fn share_balance(env: Env, holder: Address) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_balance(&env, &holder))
    }

    pub fn total_shares(env: Env) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_total_supply(&env))
    }

    /// SY that `amount` of underlying would mint at the current rate, ignoring
    /// upstream rounding. The realized amount can be marginally lower, which is
    /// why `deposit` takes a `min_sy_out`.
    ///
    /// On an empty market this subtracts the [`MINIMUM_SHARES`] the first
    /// deposit locks away, so the quote is what the depositor actually receives
    /// rather than what is minted.
    /// A quote that succeeds where the call reverts is worse than no quote: a
    /// caller gating on `preview >= min` reads a number and signs a transaction
    /// that cannot land. So this refuses everything it can prove `deposit` will
    /// refuse, with the same error code.
    ///
    /// It cannot be an absolute guarantee, and the gap is worth stating rather
    /// than implying. `deposit` checks the cap against `total_assets()` *after*
    /// the strategy credits, and the credit is a measured delta this function
    /// has no way to predict — so a deposit with room left under the cap may
    /// still breach it, and a paused or shaving strategy can reject a deposit
    /// no preview could have known about. What is certain is checked below;
    /// what depends on the strategy's behaviour at call time is not.
    pub fn preview_deposit(env: Env, amount: i128) -> Result<i128, Error> {
        // `deposit` rejects this first, before it ever reaches the share math.
        // Quoting it as a successful 0 — or, for a negative amount, as a
        // negative number of shares — is the same defect in a different place.
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        // A market already at or over its cap rejects every positive deposit,
        // and that is knowable here without predicting the credit. Quoting a
        // number for a capped market was the exact "reads a quote where it
        // should have read a failure" case this function exists to prevent.
        let cap = Self::read_deposit_cap(&env);
        if cap > 0 {
            let config = Self::config_or_panic(&env);
            if YieldStrategyClient::new(&env, &config.strategy).total_assets() >= cap {
                return Err(Error::DepositCapExceeded);
            }
        }

        let rate = <Self as StandardizedYield>::exchange_rate(&env);
        let minted = Self::mul_div(&env, amount, WAD, rate);
        if Self::read_total_supply(&env) == 0 {
            // Report the same refusal `deposit` will make rather than clamping
            // to a successful zero.
            if minted <= MINIMUM_SHARES {
                return Err(Error::InitialDepositTooSmall);
            }
            Ok(minted - MINIMUM_SHARES)
        } else {
            // Dust at a rate above WAD floors to zero shares, which `deposit`
            // rejects as InvalidAmount. Mirror that rather than quoting a free
            // no-op.
            if minted <= 0 {
                return Err(Error::InvalidAmount);
            }
            Ok(minted)
        }
    }

    /// Underlying that `sy_amount` would redeem at the current rate, before any
    /// upstream liquidity constraint. Compare against `max_withdraw`.
    pub fn preview_redeem(env: Env, sy_amount: i128) -> Result<i128, Error> {
        // Same rule as `preview_deposit`: refuse where `redeem` refuses, with
        // the same error. Without this, `preview_redeem(-5)` returned `Ok(-5)` —
        // a negative quantity of underlying for a call that reverts.
        if sy_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let rate = <Self as StandardizedYield>::exchange_rate(&env);
        Ok(Self::mul_div(&env, sy_amount, rate, WAD))
    }

    // --- SEP-41 token interface (SY is a transferable share) ---------------

    pub fn balance(env: Env, id: Address) -> i128 {
        Self::read_balance(&env, &id)
    }

    pub fn total_supply(env: Env) -> i128 {
        Self::read_total_supply(&env)
    }

    pub fn decimals(_env: Env) -> u32 {
        DECIMALS
    }

    pub fn name(env: Env) -> String {
        String::from_str(&env, "Sidereal Standardized Yield")
    }

    pub fn symbol(env: Env) -> String {
        String::from_str(&env, "sSY")
    }

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        Self::read_allowance(&env, &from, &spender).amount
    }

    pub fn approve(env: Env, from: Address, spender: Address, amount: i128, expiration_ledger: u32) {
        from.require_auth();
        if amount < 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if amount > 0 && expiration_ledger < env.ledger().sequence() {
            panic_with_error!(&env, Error::InvalidExpiration);
        }
        Self::bump_instance_ttl(&env);
        Self::write_allowance(&env, &from, &spender, amount, expiration_ledger);
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        Self::move_balance(&env, &from, &to, amount);
    }

    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        Self::spend_allowance(&env, &from, &spender, amount);
        Self::move_balance(&env, &from, &to, amount);
    }

    // --- vault entrypoints --------------------------------------------------

    /// Deposits underlying and mints SY, never fewer than `min_sy_out`.
    ///
    /// Prices against the pre-deposit rate and the strategy's **measured**
    /// balance change, so an upstream that floors in its own favour can never
    /// mint SY the position does not back. The first deposit into an empty
    /// market additionally locks [`MINIMUM_SHARES`] away permanently. Reverts if
    /// the resulting `total_assets()` would exceed the admin-set deposit cap.
    pub fn deposit(env: Env, from: Address, amount: i128, min_sy_out: i128) -> i128 {
        Self::deposit_inner(&env, from, amount, min_sy_out)
    }

    /// Burns SY and returns underlying, never less than `min_underlying_out`.
    ///
    /// SY is burned before the strategy call, and the underlying actually
    /// delivered to this vault is verified afterwards. A delivery below
    /// `min_underlying_out` reverts the whole transaction; a short delivery that
    /// still clears the floor — including the `min_underlying_out = 0` the
    /// trait-level `redeem` and the SDK default both pass — settles for what
    /// arrived and burns only the SY that arrival paid for. The unfilled
    /// remainder stays with the holder.
    ///
    /// The cap never gates this path. A market can be closed to deposits and
    /// still be exited in full.
    pub fn redeem(env: Env, from: Address, sy_amount: i128, min_underlying_out: i128) -> i128 {
        Self::redeem_inner(&env, from, sy_amount, min_underlying_out)
    }

    pub fn exchange_rate(env: Env) -> i128 {
        <Self as StandardizedYield>::exchange_rate(&env)
    }

    pub fn underlying(env: Env) -> Address {
        <Self as StandardizedYield>::underlying(&env)
    }

    pub fn accrued_yield(env: Env, holder: Address) -> i128 {
        <Self as StandardizedYield>::accrued_yield(&env, holder)
    }

    /// Permissionless upkeep: renews this vault's instance entry and forwards to
    /// the strategy so the upstream position's entry is renewed too. An actively
    /// traded market keeps itself alive; only an idle one needs the keeper.
    pub fn touch(env: Env) -> Result<(), Error> {
        let config = Self::read_config(&env)?;
        Self::bump_instance_ttl(&env);
        YieldStrategyClient::new(&env, &config.strategy).touch();
        Ok(())
    }

    /// Renews one holder's persistent balance entry without moving funds.
    pub fn bump_holder_ttl(env: Env, holder: Address) -> Result<(), Error> {
        Self::read_config(&env)?;
        Self::bump_balance_ttl(&env, &holder);
        Ok(())
    }
}

impl SyVaultV2 {
    fn deposit_inner(env: &Env, from: Address, amount: i128, min_sy_out: i128) -> i128 {
        from.require_auth();
        if amount <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        if min_sy_out < 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        Self::bump_instance_ttl(env);

        let config = Self::config_or_panic(env);

        // Price before the assets enter the strategy.
        let rate = <Self as StandardizedYield>::exchange_rate(env);

        Self::pull_underlying(env, &config.underlying, &from, amount);

        // Pre-authorize exactly the transfer leg the strategy will perform, and
        // nothing else. The strategy pulls from this vault; every other call in
        // its subtree acts on its own behalf.
        let vault = env.current_contract_address();
        Self::authorize_transfer(env, &config.underlying, &vault, &config.strategy, amount);

        let strategy_client = YieldStrategyClient::new(env, &config.strategy);
        let assets_credited = strategy_client.deposit(&vault, &amount);
        if assets_credited <= 0 {
            panic_with_error!(env, Error::StrategyDeliveryFailed);
        }

        // The cap bounds *real backing*, so it is read after the strategy has
        // credited rather than against the requested amount: an upstream that
        // shaves a deposit must not consume cap it never took on. A panic here
        // reverts the transfer and the strategy credit along with everything
        // else, so a rejected deposit mints nothing and moves nothing.
        let cap = Self::read_deposit_cap(env);
        if cap > 0 && strategy_client.total_assets() > cap {
            panic_with_error!(env, Error::DepositCapExceeded);
        }

        let minted = Self::mul_div(env, assets_credited, WAD, rate);
        let total = Self::read_total_supply(env);

        // The first deposit funds MINIMUM_SHARES that belong to nobody: they are
        // added to the supply and to no balance, so the supply can never return
        // to zero and no holder can ever own all of it.
        let shares = if total == 0 {
            if minted <= MINIMUM_SHARES {
                panic_with_error!(env, Error::InitialDepositTooSmall);
            }
            minted - MINIMUM_SHARES
        } else {
            minted
        };
        if shares <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        if shares < min_sy_out {
            panic_with_error!(env, Error::SlippageExceeded);
        }

        let current_shares = Self::read_balance(env, &from);
        let current_principal = Self::read_principal(env, &from);

        // Principal is credited for the shares the depositor actually received,
        // not for the ones locked away, so `accrued_yield` opens at zero rather
        // than at a small negative. The locked shares are not theirs and never
        // were; they are a cost of opening the market, not a loss on a position.
        // Identical to `amount` on every deposit after the first, where
        // `shares == minted`.
        let principal_credit = Self::mul_div(env, amount, shares, minted);

        Self::write_balance(env, &from, Self::add(env, current_shares, shares));
        Self::write_principal(
            env,
            &from,
            Self::add(env, current_principal, principal_credit),
        );
        Self::write_total_supply(env, Self::add(env, total, minted));

        Deposited {
            from,
            underlying_in: amount,
            assets_credited,
            sy_out: shares,
            rate,
        }
        .publish(env);

        shares
    }

    fn redeem_inner(env: &Env, from: Address, sy_amount: i128, min_underlying_out: i128) -> i128 {
        from.require_auth();
        if sy_amount <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        if min_underlying_out < 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        Self::bump_instance_ttl(env);

        let config = Self::config_or_panic(env);
        let rate = <Self as StandardizedYield>::exchange_rate(env);

        let current_shares = Self::read_balance(env, &from);
        let current_principal = Self::read_principal(env, &from);
        let total = Self::read_total_supply(env);
        if sy_amount > current_shares {
            panic_with_error!(env, Error::InsufficientBalance);
        }

        let requested = Self::mul_div(env, sy_amount, rate, WAD);
        if requested <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }

        // Burn the full request up front, before any external call: while the
        // strategy is executing, the holder holds no balance to re-enter
        // against. What the strategy actually delivers is only known afterwards,
        // so the delivered portion stays burned and any unfilled remainder is
        // credited straight back below.
        //
        // `min_underlying_out` does not make this redundant. The trait-level
        // `redeem` passes 0, and so does the SDK by default, so a short delivery
        // settles rather than reverting — and burning `sy_amount` against a
        // partial fill would hand the difference to the remaining holders.
        let principal_out = if current_shares == 0 {
            0
        } else {
            Self::mul_div(env, current_principal, sy_amount, current_shares)
        };
        Self::write_balance(env, &from, Self::sub(env, current_shares, sy_amount));
        Self::write_principal(env, &from, Self::sub(env, current_principal, principal_out));
        Self::write_total_supply(env, Self::sub(env, total, sy_amount));

        let vault = env.current_contract_address();
        let before = Self::underlying_balance(env, &config.underlying);
        let reported = YieldStrategyClient::new(env, &config.strategy).withdraw(
            &vault,
            &requested,
            &min_underlying_out,
        );
        let after = Self::underlying_balance(env, &config.underlying);
        let delivered = Self::sub(env, after, before);

        // Trust the measurement, not the strategy's own report.
        if delivered <= 0 || delivered < reported {
            panic_with_error!(env, Error::StrategyDeliveryFailed);
        }
        if delivered < min_underlying_out {
            panic_with_error!(env, Error::SlippageExceeded);
        }

        // V1 recomputed the burn from what actually arrived (`sy-wrapper:579`);
        // V2 had regressed to burning the whole request. Restored here, and
        // rounded **up** rather than V1's down — `L8` in the audit — so the
        // rounding notch on a partial fill favours the vault and the holders who
        // stay, never the redeemer. `ceil(delivered * WAD / rate) <= sy_amount`
        // whenever `delivered < requested`; the clamp makes that structural
        // rather than a proof the reader has to redo.
        let burned = if delivered >= requested {
            sy_amount
        } else {
            let proportional = Self::mul_div_ceil(env, delivered, WAD, rate);
            if proportional < sy_amount {
                proportional
            } else {
                sy_amount
            }
        };
        if burned < sy_amount {
            let refund_shares = Self::sub(env, sy_amount, burned);
            let refund_principal = Self::mul_div(env, principal_out, refund_shares, sy_amount);
            let held_shares = Self::read_balance(env, &from);
            let held_principal = Self::read_principal(env, &from);
            Self::write_balance(env, &from, Self::add(env, held_shares, refund_shares));
            Self::write_principal(
                env,
                &from,
                Self::add(env, held_principal, refund_principal),
            );
            let supply = Self::read_total_supply(env);
            Self::write_total_supply(env, Self::add(env, supply, refund_shares));
        }

        Self::push_underlying(env, &config.underlying, &from, delivered);

        Redeemed {
            from,
            sy_in: burned,
            underlying_out: delivered,
            rate,
        }
        .publish(env);

        delivered
    }

    // --- storage ------------------------------------------------------------

    fn read_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }

    fn config_or_panic(env: &Env) -> Config {
        match Self::read_config(env) {
            Ok(config) => config,
            Err(error) => panic_with_error!(env, error),
        }
    }

    fn read_total_supply(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    fn write_total_supply(env: &Env, value: i128) {
        env.storage().instance().set(&DataKey::TotalSupply, &value);
    }

    /// `0` — the stored default and the manifests' convention — means uncapped.
    fn read_deposit_cap(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::DepositCap)
            .unwrap_or(0)
    }

    fn read_balance(env: &Env, holder: &Address) -> i128 {
        let key = DataKey::Balance(holder.clone());
        let value = env.storage().persistent().get(&key).unwrap_or(0);
        if value != 0 {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
        }
        value
    }

    fn write_balance(env: &Env, holder: &Address, value: i128) {
        let key = DataKey::Balance(holder.clone());
        env.storage().persistent().set(&key, &value);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    fn read_principal(env: &Env, holder: &Address) -> i128 {
        let key = DataKey::Principal(holder.clone());
        let value = env.storage().persistent().get(&key).unwrap_or(0);
        if value != 0 {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
        }
        value
    }

    fn write_principal(env: &Env, holder: &Address, value: i128) {
        let key = DataKey::Principal(holder.clone());
        env.storage().persistent().set(&key, &value);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    fn bump_balance_ttl(env: &Env, holder: &Address) {
        let balance_key = DataKey::Balance(holder.clone());
        if env.storage().persistent().has(&balance_key) {
            env.storage().persistent().extend_ttl(
                &balance_key,
                TTL_THRESHOLD_LEDGERS,
                TTL_EXTEND_TO_LEDGERS,
            );
        }
        let principal_key = DataKey::Principal(holder.clone());
        if env.storage().persistent().has(&principal_key) {
            env.storage().persistent().extend_ttl(
                &principal_key,
                TTL_THRESHOLD_LEDGERS,
                TTL_EXTEND_TO_LEDGERS,
            );
        }
    }

    fn read_allowance(env: &Env, from: &Address, spender: &Address) -> AllowanceValue {
        let key = DataKey::Allowance(from.clone(), spender.clone());
        match env.storage().temporary().get::<_, AllowanceValue>(&key) {
            Some(value) if value.expiration_ledger >= env.ledger().sequence() => value,
            _ => AllowanceValue {
                amount: 0,
                expiration_ledger: 0,
            },
        }
    }

    fn write_allowance(
        env: &Env,
        from: &Address,
        spender: &Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
        let key = DataKey::Allowance(from.clone(), spender.clone());
        env.storage().temporary().set(
            &key,
            &AllowanceValue {
                amount,
                expiration_ledger,
            },
        );
        let current = env.ledger().sequence();
        if amount > 0 && expiration_ledger > current {
            env.storage()
                .temporary()
                .extend_ttl(&key, 0, expiration_ledger - current);
        }
    }

    fn spend_allowance(env: &Env, from: &Address, spender: &Address, amount: i128) {
        let allowance = Self::read_allowance(env, from, spender);
        if allowance.amount < amount {
            panic_with_error!(env, Error::InsufficientAllowance);
        }
        Self::write_allowance(
            env,
            from,
            spender,
            allowance.amount - amount,
            allowance.expiration_ledger,
        );
    }

    /// Moves shares and their matching principal together, so `accrued_yield`
    /// stays correct on both sides of a transfer.
    fn move_balance(env: &Env, from: &Address, to: &Address, amount: i128) {
        let from_shares = Self::read_balance(env, from);
        if amount > from_shares {
            panic_with_error!(env, Error::InsufficientBalance);
        }
        let from_principal = Self::read_principal(env, from);
        let moved_principal = if from_shares == 0 {
            0
        } else {
            Self::mul_div(env, from_principal, amount, from_shares)
        };

        Self::write_balance(env, from, Self::sub(env, from_shares, amount));
        Self::write_principal(env, from, Self::sub(env, from_principal, moved_principal));

        let to_shares = Self::read_balance(env, to);
        let to_principal = Self::read_principal(env, to);
        Self::write_balance(env, to, Self::add(env, to_shares, amount));
        Self::write_principal(env, to, Self::add(env, to_principal, moved_principal));
    }

    fn require_amount_or_panic(env: &Env, amount: i128) {
        if amount <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    // --- token movement -----------------------------------------------------

    fn pull_underlying(env: &Env, underlying: &Address, from: &Address, amount: i128) {
        let vault = MuxedAddress::from(&env.current_contract_address());
        token::TokenClient::new(env, underlying).transfer(from, &vault, &amount);
    }

    fn push_underlying(env: &Env, underlying: &Address, to: &Address, amount: i128) {
        if amount <= 0 {
            return;
        }
        let vault = env.current_contract_address();
        let to_muxed = MuxedAddress::from(to);
        env.authorize_as_current_contract(vec![
            env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: underlying.clone(),
                    fn_name: Symbol::new(env, "transfer"),
                    args: vec![
                        env,
                        vault.clone().into_val(env),
                        to_muxed.clone().into_val(env),
                        amount.into_val(env),
                    ],
                },
                sub_invocations: vec![env],
            }),
        ]);
        token::TokenClient::new(env, underlying).transfer(&vault, &to_muxed, &amount);
    }

    /// Authorizes exactly one `transfer(from, to, amount)` on `underlying`,
    /// argument-pinned, for the strategy call that follows.
    fn authorize_transfer(env: &Env, underlying: &Address, from: &Address, to: &Address, amount: i128) {
        let to_muxed = MuxedAddress::from(to);
        env.authorize_as_current_contract(vec![
            env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: underlying.clone(),
                    fn_name: Symbol::new(env, "transfer"),
                    args: vec![
                        env,
                        from.clone().into_val(env),
                        to_muxed.into_val(env),
                        amount.into_val(env),
                    ],
                },
                sub_invocations: vec![env],
            }),
        ]);
    }

    fn underlying_balance(env: &Env, underlying: &Address) -> i128 {
        token::TokenClient::new(env, underlying).balance(&env.current_contract_address())
    }

    // --- checked math -------------------------------------------------------

    fn add(env: &Env, lhs: i128, rhs: i128) -> i128 {
        match lhs.checked_add(rhs) {
            Some(value) => value,
            None => panic_with_error!(env, Error::MathOverflow),
        }
    }

    fn sub(env: &Env, lhs: i128, rhs: i128) -> i128 {
        match lhs.checked_sub(rhs) {
            Some(value) => value,
            None => panic_with_error!(env, Error::MathOverflow),
        }
    }

    /// `lhs * rhs / denominator`, reducing by GCD first so intermediate products
    /// stay in range. Matches V1's rounding exactly (floor).
    fn mul_div(env: &Env, lhs: i128, rhs: i128, denominator: i128) -> i128 {
        let (product, divisor) = Self::reduced_product(env, lhs, rhs, denominator);
        product / divisor
    }

    /// `ceil(lhs * rhs / denominator)` for non-negative inputs. Used only where
    /// the rounding notch must fall toward the vault rather than the caller: the
    /// partial-fill burn in `redeem`.
    fn mul_div_ceil(env: &Env, lhs: i128, rhs: i128, denominator: i128) -> i128 {
        let (product, divisor) = Self::reduced_product(env, lhs, rhs, denominator);
        let quotient = product / divisor;
        if product % divisor != 0 {
            Self::add(env, quotient, 1)
        } else {
            quotient
        }
    }

    /// Reduces `lhs * rhs / denominator` by GCD on both numerator terms and
    /// returns `(lhs' * rhs', denominator')`, so the caller picks the rounding
    /// direction from an exact quotient-and-remainder pair.
    fn reduced_product(env: &Env, lhs: i128, rhs: i128, denominator: i128) -> (i128, i128) {
        if denominator == 0 {
            panic_with_error!(env, Error::MathOverflow);
        }

        let lhs_gcd = gcd_i128(lhs, denominator);
        let lhs_reduced = lhs / lhs_gcd;
        let denominator_reduced = denominator / lhs_gcd;

        let rhs_gcd = gcd_i128(rhs, denominator_reduced);
        let rhs_reduced = rhs / rhs_gcd;
        let denominator_final = denominator_reduced / rhs_gcd;

        match lhs_reduced.checked_mul(rhs_reduced) {
            Some(product) => (product, denominator_final),
            None => panic_with_error!(env, Error::MathOverflow),
        }
    }
}

impl StandardizedYield for SyVaultV2 {
    fn deposit(env: &Env, from: Address, amount: i128) -> i128 {
        Self::deposit_inner(env, from, amount, 0)
    }

    fn redeem(env: &Env, from: Address, sy_amount: i128) -> i128 {
        Self::redeem_inner(env, from, sy_amount, 0)
    }

    /// `strategy.total_assets() * WAD / total_sy_supply`. Derived on every read;
    /// there is no stored rate and no setter.
    fn exchange_rate(env: &Env) -> i128 {
        let config = Self::config_or_panic(env);
        let assets = YieldStrategyClient::new(env, &config.strategy).total_assets();
        let supply = Self::read_total_supply(env);
        match derived_exchange_rate(assets, supply) {
            Some(value) if value > 0 => value,
            Some(_) => panic_with_error!(env, Error::InvalidExchangeRate),
            // Assets standing behind no shares at all. `MINIMUM_SHARES` makes
            // this unreachable once a market has opened, so reaching it means
            // the strategy is reporting a position this vault never minted
            // against. Fail closed rather than bootstrap at `WAD` and sell the
            // whole position to the next one-stroop depositor.
            None if supply <= 0 => panic_with_error!(env, Error::InvalidExchangeRate),
            None => panic_with_error!(env, Error::MathOverflow),
        }
    }

    fn underlying(env: &Env) -> Address {
        Self::config_or_panic(env).underlying
    }

    fn accrued_yield(env: &Env, holder: Address) -> i128 {
        let rate = <Self as StandardizedYield>::exchange_rate(env);
        let shares = Self::read_balance(env, &holder);
        let principal = Self::read_principal(env, &holder);
        let current_value = Self::mul_div(env, shares, rate, WAD);
        current_value.saturating_sub(principal)
    }
}

fn gcd_i128(mut lhs: i128, mut rhs: i128) -> i128 {
    while rhs != 0 {
        let next = lhs % rhs;
        lhs = rhs;
        rhs = next;
    }

    if lhs < 0 {
        -lhs
    } else {
        lhs
    }
}

#[cfg(test)]
extern crate std;
