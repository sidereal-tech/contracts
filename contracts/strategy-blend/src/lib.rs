// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

//! Blend v2 yield strategy — V1's proven custody path, extracted behind the
//! `YieldStrategy` seam.
//!
//! This is an extraction, not a rewrite. It carries over, unchanged in
//! substance:
//!
//! - **Plain supply only.** Never `SupplyCollateral`, never borrow, so the
//!   position stays liquid and is never seizable.
//! - **Only the bToken position is valued.** V1's `blend_assets_under_management`
//!   deliberately excluded the wrapper's own `underlying_balance`, and so does
//!   this. Anyone can send underlying to this contract's address with an
//!   ordinary SAC transfer — no auth, no vault call — so counting that balance
//!   would let a stranger set the market's exchange rate. Blend's `submit`
//!   requires `from.require_auth()`, and `from` is always this contract, so the
//!   supplied position is the one quantity nobody outside can move.
//! - **A config-pinned pool and a validated reserve index.** The index is
//!   derived from the pool at init and cross-checked against the pool's own
//!   `get_reserve().config.index`; a later mismatch traps rather than pricing
//!   the wrong reserve.
//! - **Argument-pinned authorization trees with balance-change verification.**
//! - **Reserve-index recovery constrained to another slot for the _same_
//!   underlying** (`migrate_reserve_index`), so the strongest thing an admin can
//!   do is re-point at the same asset under its new slot.
//!
//! What is new is the seam's own contract: `deposit` and `withdraw` accept calls
//! only from the vault fixed at init, and both return *measured deltas*.

use sidereal_blend_adapter::{
    assets_from_b_tokens, BlendPoolClient, Request, REQUEST_SUPPLY, REQUEST_WITHDRAW,
};
use sidereal_strategy_interface::StrategyError;
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contractevent, contractimpl, contracttype, panic_with_error, token, vec, Address, Env,
    IntoVal, MuxedAddress, Symbol,
};

/// Underlying decimals this strategy supports. Blend reserves carry their own
/// decimals; we pin to 7 (Stellar USDC) and refuse anything else at init rather
/// than silently mis-scaling.
const DECIMALS: u32 = 7;

/// TTL policy, matching the vault and AMM: bump when within 30 days of expiry,
/// extend to 120 days.
const LEDGERS_PER_DAY: u32 = 17_280;
const TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

/// Every field is fixed at initialization except `reserve_index`, which only
/// `migrate_reserve_index` may move, and only to another slot holding the same
/// `underlying`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    /// The SY vault. The only address allowed to call `deposit` / `withdraw`.
    pub vault: Address,
    pub underlying: Address,
    pub pool: Address,
    pub reserve_index: u32,
}

#[derive(Clone)]
#[contracttype]
enum DataKey {
    Config,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposited {
    pub amount: i128,
    /// Measured increase in `total_assets` — what the vault mints against.
    pub credited: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Withdrawn {
    pub requested: i128,
    pub delivered: i128,
}

/// Emitted when an admin re-syncs the stored reserve index after the pool
/// reindexed the underlying. Both indices are carried so integrators can audit
/// the move.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveMigrated {
    pub old_index: u32,
    pub new_index: u32,
}

#[contract]
pub struct StrategyBlend;

#[contractimpl]
impl StrategyBlend {
    /// Binds this strategy permanently to one vault, one underlying, and one
    /// Blend pool. There is no rebinding entrypoint: a different yield source,
    /// pool, or vault means a new strategy instance and a new market.
    pub fn initialize(
        env: Env,
        admin: Address,
        vault: Address,
        underlying: Address,
        pool: Address,
    ) -> Result<(), StrategyError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(StrategyError::AlreadyInitialized);
        }
        admin.require_auth();

        let reserve_index = Self::derive_reserve_index(&env, &pool, &underlying)?;

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                vault,
                underlying,
                pool,
                reserve_index,
            },
        );
        Self::bump_instance_ttl(&env);
        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, StrategyError> {
        Self::read_config(&env)
    }

    // --- YieldStrategy ABI -------------------------------------------------

    pub fn underlying(env: Env) -> Address {
        Self::config_or_panic(&env).underlying
    }

    pub fn vault(env: Env) -> Address {
        Self::config_or_panic(&env).vault
    }

    /// The bToken position at Blend's live `b_rate`, and nothing else.
    ///
    /// Two things are deliberately excluded:
    ///
    /// - **Idle underlying sitting at this address.** It is not this strategy's
    ///   to value: any address can put it there with a plain SAC transfer, so
    ///   including it would make the vault's exchange rate — and through it the
    ///   tokenizer's PT reservation — a function of an unauthorized token
    ///   transfer. `withdraw` still pays it out (idle first), so a donation is
    ///   recoverable rather than stranded; it simply accrues to the holders who
    ///   remain instead of repricing everyone's shares the instant it lands.
    /// - **BLND emissions**, which this strategy never claims or converts. A
    ///   reward token the position cannot presently redeem must not move the
    ///   exchange rate.
    pub fn total_assets(env: Env) -> i128 {
        let config = Self::config_or_panic(&env);
        Self::assets_of(&env, &config)
    }

    /// What a redemption could actually be paid right now.
    ///
    /// `withdraw` pays from idle underlying first and asks Blend only for the
    /// shortfall, so idle genuinely substitutes for pool cash — but only up to
    /// `total_assets`, because the vault can never legitimately request more
    /// than its shares are worth. Reporting the uncounted idle on top would
    /// break the seam's `max_withdraw <= total_assets` obligation and quote
    /// liquidity no share is entitled to.
    pub fn max_withdraw(env: Env) -> i128 {
        let config = Self::config_or_panic(&env);
        let idle = Self::idle_balance(&env, &config.underlying);
        let supplied = Self::supplied_assets(&env, &config);
        let pool_liquidity = token::TokenClient::new(&env, &config.underlying).balance(&config.pool);
        let realizable = if supplied < pool_liquidity {
            supplied
        } else {
            pool_liquidity
        };
        let deliverable = match idle.checked_add(realizable) {
            Some(value) => value,
            None => panic_with_error!(&env, StrategyError::MathOverflow),
        };
        if deliverable < supplied {
            deliverable
        } else {
            supplied
        }
    }

    /// Pulls `amount` of underlying from the vault and supplies it to Blend.
    ///
    /// Returns the **measured** increase in `total_assets`, not `amount`. Blend
    /// floors the bTokens it credits, so the two differ by up to one unit of
    /// rounding; minting SY against the requested amount instead would create
    /// shares the position does not back.
    ///
    /// Supplies exactly `amount` and deliberately does **not** sweep any idle
    /// balance in with it. Sweeping would move a donation into the valued
    /// position, which is the same permissionless rate bump excluding idle
    /// exists to prevent — only now anyone could trigger it by depositing one
    /// stroop.
    pub fn deposit(env: Env, vault: Address, amount: i128) -> i128 {
        let config = Self::config_or_panic(&env);
        Self::require_vault(&env, &config, &vault);
        if amount <= 0 {
            panic_with_error!(&env, StrategyError::InvalidAmount);
        }
        Self::bump_instance_ttl(&env);

        let before = Self::assets_of(&env, &config);

        // The vault pre-authorized exactly this transfer leg before calling us.
        token::TokenClient::new(&env, &config.underlying).transfer(
            &vault,
            &MuxedAddress::from(&env.current_contract_address()),
            &amount,
        );
        Self::blend_submit(&env, &config, REQUEST_SUPPLY, amount, false);

        let after = Self::assets_of(&env, &config);
        let credited = match after.checked_sub(before) {
            Some(value) => value,
            None => panic_with_error!(&env, StrategyError::MathOverflow),
        };
        if credited <= 0 {
            panic_with_error!(&env, StrategyError::UpstreamMismatch);
        }

        Deposited { amount, credited }.publish(&env);
        credited
    }

    /// Delivers underlying worth `amount` to the vault, **idle first**.
    ///
    /// Any underlying already sitting at this address is paid out before Blend
    /// is touched, and only the shortfall is requested from the pool. Measuring
    /// the idle *delta* around the Blend call alone — as this did before — made
    /// pre-existing idle unreachable by any entrypoint while `max_withdraw`
    /// still counted it, so a strategy holding idle against an illiquid pool
    /// reported liquidity it could never deliver and stranded the balance
    /// permanently. Spending it first also settles the only route by which a
    /// donation can reach the vault: it leaves as underlying to a redeemer
    /// instead of sitting unspendable.
    ///
    /// Returns the underlying actually delivered, which may be less than
    /// `amount` when the pool is short. `min_underlying_out` is the caller's
    /// floor; the vault burns SY against the delivered amount, not the request.
    pub fn withdraw(env: Env, vault: Address, amount: i128, min_underlying_out: i128) -> i128 {
        let config = Self::config_or_panic(&env);
        Self::require_vault(&env, &config, &vault);
        if amount <= 0 {
            panic_with_error!(&env, StrategyError::InvalidAmount);
        }
        Self::bump_instance_ttl(&env);

        let idle_before = Self::idle_balance(&env, &config.underlying);
        let idle_used = if idle_before < amount {
            idle_before
        } else {
            amount
        };
        let shortfall = amount - idle_used;

        // Ask Blend only for what idle could not cover. The submit is tolerated
        // so a pool failure surfaces as a typed error rather than leaking
        // Blend's raw panic; the sub-call's own writes roll back with it.
        //
        // A failed leg falls through to `blend_delta = 0` rather than trapping,
        // so a pool outage cannot strand underlying this contract is already
        // holding. With idle at zero — the normal state — that still ends in
        // `delivered == 0` and the same `WithdrawalFailed` as before. When idle
        // covers part of the request the caller gets a short fill instead of a
        // revert, which `min_underlying_out` bounds and the vault burns SY
        // against proportionally.
        let blend_delta = if shortfall > 0
            && Self::blend_submit(&env, &config, REQUEST_WITHDRAW, shortfall, true)
        {
            let idle_after = Self::idle_balance(&env, &config.underlying);
            match idle_after.checked_sub(idle_before) {
                Some(value) if value > 0 => value,
                Some(_) => 0,
                None => panic_with_error!(&env, StrategyError::MathOverflow),
            }
        } else {
            0
        };

        let delivered = match idle_used.checked_add(blend_delta) {
            Some(value) => value,
            None => panic_with_error!(&env, StrategyError::MathOverflow),
        };
        if delivered <= 0 {
            panic_with_error!(&env, StrategyError::WithdrawalFailed);
        }
        if delivered < min_underlying_out {
            panic_with_error!(&env, StrategyError::SlippageExceeded);
        }

        Self::push_underlying(&env, &config.underlying, &vault, delivered);

        Withdrawn {
            requested: amount,
            delivered,
        }
        .publish(&env);
        delivered
    }

    /// Permissionless upkeep. Reading the pool's view of this strategy's
    /// position renews Blend's own persistent entry for it — an entry this
    /// contract cannot bump directly, because it belongs to Blend.
    pub fn touch(env: Env) {
        let config = Self::config_or_panic(&env);
        let pool_client = BlendPoolClient::new(&env, &config.pool);
        pool_client.get_positions(&env.current_contract_address());
        pool_client.get_reserve(&config.underlying);
        Self::bump_instance_ttl(&env);
    }

    // --- constrained recovery ----------------------------------------------

    /// Recovers from a Blend reserve reindex.
    ///
    /// Does NOT trust a caller-supplied index: it re-derives the index the same
    /// way `initialize` does, from the pool's reserve list, and cross-checks it
    /// against the pool's authoritative `get_reserve(underlying).config.index`.
    /// The new index is accepted only if the asset sitting there is still
    /// `config.underlying`, so an admin can re-point at the same underlying
    /// under its new slot and nothing more. Returns the new index.
    pub fn migrate_reserve_index(env: Env, admin: Address) -> Result<u32, StrategyError> {
        let mut config = Self::read_config(&env)?;
        admin.require_auth();
        if admin != config.admin {
            return Err(StrategyError::NotVault);
        }

        let new_index = Self::derive_reserve_index(&env, &config.pool, &config.underlying)?;
        let old_index = config.reserve_index;
        config.reserve_index = new_index;
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance_ttl(&env);

        ReserveMigrated {
            old_index,
            new_index,
        }
        .publish(&env);
        Ok(new_index)
    }

    // --- internals ----------------------------------------------------------

    fn read_config(env: &Env) -> Result<Config, StrategyError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(StrategyError::NotInitialized)
    }

    fn config_or_panic(env: &Env) -> Config {
        match Self::read_config(env) {
            Ok(config) => config,
            Err(error) => panic_with_error!(env, error),
        }
    }

    fn require_vault(env: &Env, config: &Config, caller: &Address) {
        if *caller != config.vault {
            panic_with_error!(env, StrategyError::NotVault);
        }
        caller.require_auth();
    }

    /// Finds the reserve slot holding `underlying` and validates it against the
    /// pool's own record. Both must agree, and the decimals must match.
    fn derive_reserve_index(
        env: &Env,
        pool: &Address,
        underlying: &Address,
    ) -> Result<u32, StrategyError> {
        let pool_client = BlendPoolClient::new(env, pool);
        let reserves = pool_client.get_reserve_list();
        let mut found = None;
        for (index, asset) in reserves.iter().enumerate() {
            if asset == *underlying {
                found = Some(index as u32);
                break;
            }
        }
        let index = found.ok_or(StrategyError::UpstreamMismatch)?;
        let reserve = pool_client.get_reserve(underlying);
        if reserve.config.index != index || reserve.config.decimals != DECIMALS {
            return Err(StrategyError::UpstreamMismatch);
        }
        Ok(index)
    }

    fn idle_balance(env: &Env, underlying: &Address) -> i128 {
        token::TokenClient::new(env, underlying).balance(&env.current_contract_address())
    }

    /// The supplied position's value at Blend's live `b_rate`, with the
    /// reserve-index check that refuses to price the wrong reserve.
    fn supplied_assets(env: &Env, config: &Config) -> i128 {
        let pool_client = BlendPoolClient::new(env, &config.pool);
        let positions = pool_client.get_positions(&env.current_contract_address());
        let b_tokens = positions.supply.get(config.reserve_index).unwrap_or(0);
        let reserve = pool_client.get_reserve(&config.underlying);
        if reserve.config.index != config.reserve_index {
            panic_with_error!(env, StrategyError::UpstreamMismatch);
        }
        match assets_from_b_tokens(b_tokens, reserve.data.b_rate) {
            Some(value) => value,
            None => panic_with_error!(env, StrategyError::MathOverflow),
        }
    }

    /// The valuation the vault mints and prices against: the supplied position
    /// only. See `total_assets` for why idle is excluded.
    fn assets_of(env: &Env, config: &Config) -> i128 {
        Self::supplied_assets(env, config)
    }

    /// Submits one plain-supply or withdraw request as this strategy. The
    /// strategy is the direct invoker of `submit`, so Blend's
    /// `spender.require_auth()` is satisfied by invoker auth. Supply separately
    /// authorizes the nested token transfer to the pool, pinned to exact
    /// arguments. Blend authorizes its own outgoing transfer during withdraw.
    fn blend_submit(
        env: &Env,
        config: &Config,
        request_type: u32,
        amount: i128,
        tolerate_failure: bool,
    ) -> bool {
        let me = env.current_contract_address();
        let requests = vec![
            env,
            Request {
                address: config.underlying.clone(),
                amount,
                request_type,
            },
        ];
        if request_type == REQUEST_SUPPLY {
            env.authorize_as_current_contract(vec![
                env,
                InvokerContractAuthEntry::Contract(SubContractInvocation {
                    context: ContractContext {
                        contract: config.underlying.clone(),
                        fn_name: Symbol::new(env, "transfer"),
                        args: vec![
                            env,
                            me.clone().into_val(env),
                            config.pool.clone().into_val(env),
                            amount.into_val(env),
                        ],
                    },
                    sub_invocations: vec![env],
                }),
            ]);
        }

        let client = BlendPoolClient::new(env, &config.pool);
        if tolerate_failure {
            matches!(client.try_submit(&me, &me, &me, &requests), Ok(Ok(_)))
        } else {
            client.submit(&me, &me, &me, &requests);
            true
        }
    }

    fn push_underlying(env: &Env, underlying: &Address, to: &Address, amount: i128) {
        if amount <= 0 {
            return;
        }
        let me = env.current_contract_address();
        let to_muxed = MuxedAddress::from(to);
        env.authorize_as_current_contract(vec![
            env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: underlying.clone(),
                    fn_name: Symbol::new(env, "transfer"),
                    args: vec![
                        env,
                        me.clone().into_val(env),
                        to_muxed.clone().into_val(env),
                        amount.into_val(env),
                    ],
                },
                sub_invocations: vec![env],
            }),
        ]);
        token::TokenClient::new(env, underlying).transfer(&me, &to_muxed, &amount);
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }
}

#[cfg(test)]
extern crate std;
