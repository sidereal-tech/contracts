// SPDX-License-Identifier: Apache-2.0

//! Shared Soroban test doubles for strategy-seam coverage.
//!
//! Two contracts live here:
//!
//! - [`MockBlendPool`] — the same pool double `blend_wrapper.rs` uses for V1,
//!   plus a `drain` helper so a test can starve the pool of underlying and
//!   exercise `max_withdraw`'s liquidity bound.
//! - [`MockStrategy`] — a deliberately hostile `YieldStrategy` implementation.
//!   It can floor in its own favour on deposit, cap its realizable liquidity,
//!   over-report what it delivered, and pause. It exists so the conformance
//!   battery can prove the *vault's* defenses rather than only Blend's good
//!   behaviour.
//!
//! A donation is expressed against either contract the same way an attacker
//! expresses it on-chain: a plain SAC `transfer` to the strategy's address, with
//! no auth from and no call into the strategy. `MockStrategy` tracks `invested`
//! separately from its token balance so it can model both shapes of strategy —
//! `set_exclude_idle(false)` (the default) counts every token it holds, which is
//! the donation-inflatable strategy the vault must survive anyway, and
//! `set_exclude_idle(true)` counts only what it was actually given to invest,
//! which is what `strategy-blend` and the V1 wrapper do.

#![allow(dead_code)]

use sidereal_blend_adapter::{
    assets_from_b_tokens, b_tokens_from_assets, Positions, Request, Reserve, ReserveConfig,
    ReserveData, BLEND_SCALAR_12, REQUEST_SUPPLY, REQUEST_WITHDRAW,
};
use sidereal_strategy_interface::StrategyError;
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, vec, Address, Env,
    IntoVal, Map, MuxedAddress, Symbol, Vec,
};

pub const UNIT: i128 = 10_000_000;
pub const WAD: i128 = 1_000_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Blend pool double
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub struct MockConfig {
    pub underlying: Address,
    pub b_rate: i128,
}

#[derive(Clone)]
#[contracttype]
enum PoolKey {
    Config,
    Supply(Address),
    ReserveIndex,
    ReserveList,
    FailWithdraw,
}

#[contracterror]
#[derive(Copy, Clone)]
#[repr(u32)]
enum MockPoolError {
    WithdrawUnavailable = 1,
}

#[contract]
pub struct MockBlendPool;

#[contractimpl]
impl MockBlendPool {
    pub fn initialize(env: Env, underlying: Address) {
        env.storage().instance().set(
            &PoolKey::Config,
            &MockConfig {
                underlying,
                b_rate: BLEND_SCALAR_12,
            },
        );
    }

    pub fn set_b_rate(env: Env, b_rate: i128) {
        let mut config: MockConfig = env.storage().instance().get(&PoolKey::Config).unwrap();
        config.b_rate = b_rate;
        env.storage().instance().set(&PoolKey::Config, &config);
    }

    pub fn set_reserve_index(env: Env, index: u32) {
        env.storage().instance().set(&PoolKey::ReserveIndex, &index);
    }

    pub fn set_reserve_list(env: Env, list: Vec<Address>) {
        env.storage().instance().set(&PoolKey::ReserveList, &list);
    }

    pub fn set_should_fail_withdraw(env: Env, fail: bool) {
        env.storage().instance().set(&PoolKey::FailWithdraw, &fail);
    }

    /// Moves underlying out of the pool without touching bToken accounting, so a
    /// test can model a pool whose supplied position is nominally intact but
    /// whose cash cannot presently cover it.
    pub fn drain(env: Env, to: Address, amount: i128) {
        let config: MockConfig = env.storage().instance().get(&PoolKey::Config).unwrap();
        authorize_transfer(&env, &config.underlying, &to, amount);
        token::TokenClient::new(&env, &config.underlying).transfer(
            &env.current_contract_address(),
            &MuxedAddress::from(&to),
            &amount,
        );
    }

    pub fn get_reserve_list(env: Env) -> Vec<Address> {
        match env.storage().instance().get(&PoolKey::ReserveList) {
            Some(list) => list,
            None => {
                let config: MockConfig = env.storage().instance().get(&PoolKey::Config).unwrap();
                vec![&env, config.underlying]
            }
        }
    }

    pub fn get_reserve(env: Env, asset: Address) -> Reserve {
        let config: MockConfig = env.storage().instance().get(&PoolKey::Config).unwrap();
        assert_eq!(asset, config.underlying);
        pool_reserve(&env, config)
    }

    pub fn get_positions(env: Env, address: Address) -> Positions {
        pool_positions(&env, pool_reserve_index(&env), pool_supply(&env, &address))
    }

    pub fn submit(
        env: Env,
        from: Address,
        spender: Address,
        to: Address,
        requests: Vec<Request>,
    ) -> Positions {
        spender.require_auth();
        if from != spender {
            from.require_auth();
        }

        let config: MockConfig = env.storage().instance().get(&PoolKey::Config).unwrap();
        let mut supply = pool_supply(&env, &from);
        for request in requests {
            assert_eq!(request.address, config.underlying);
            if request.request_type == REQUEST_SUPPLY {
                env.invoke_contract::<()>(
                    &config.underlying,
                    &Symbol::new(&env, "transfer"),
                    vec![
                        &env,
                        spender.clone().into_val(&env),
                        env.current_contract_address().into_val(&env),
                        request.amount.into_val(&env),
                    ],
                );
                supply += b_tokens_from_assets(request.amount, config.b_rate).unwrap();
            } else if request.request_type == REQUEST_WITHDRAW {
                if env
                    .storage()
                    .instance()
                    .get(&PoolKey::FailWithdraw)
                    .unwrap_or(false)
                {
                    panic_with_error!(&env, MockPoolError::WithdrawUnavailable);
                }
                let requested_b = request
                    .amount
                    .checked_mul(BLEND_SCALAR_12)
                    .and_then(|value| value.checked_add(config.b_rate - 1))
                    .unwrap()
                    / config.b_rate;
                let burned_b = requested_b.min(supply);
                let assets_out = if burned_b == requested_b {
                    request.amount
                } else {
                    assets_from_b_tokens(burned_b, config.b_rate).unwrap()
                };
                supply -= burned_b;
                authorize_transfer(&env, &config.underlying, &to, assets_out);
                env.invoke_contract::<()>(
                    &config.underlying,
                    &Symbol::new(&env, "transfer"),
                    vec![
                        &env,
                        env.current_contract_address().into_val(&env),
                        to.clone().into_val(&env),
                        assets_out.into_val(&env),
                    ],
                );
            } else {
                panic!("unsupported request type");
            }
        }
        env.storage().persistent().set(&PoolKey::Supply(from), &supply);
        pool_positions(&env, pool_reserve_index(&env), supply)
    }
}

fn pool_reserve_index(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&PoolKey::ReserveIndex)
        .unwrap_or(0)
}

fn pool_reserve(env: &Env, config: MockConfig) -> Reserve {
    let index: u32 = pool_reserve_index(env);
    Reserve {
        asset: config.underlying,
        config: ReserveConfig {
            c_factor: 0,
            decimals: 7,
            enabled: true,
            index,
            l_factor: 0,
            max_util: 0,
            r_base: 0,
            r_one: 0,
            r_three: 0,
            r_two: 0,
            reactivity: 0,
            supply_cap: i128::MAX,
            util: 0,
        },
        data: ReserveData {
            b_rate: config.b_rate,
            b_supply: 0,
            backstop_credit: 0,
            d_rate: BLEND_SCALAR_12,
            d_supply: 0,
            ir_mod: 0,
            last_time: env.ledger().timestamp(),
        },
        scalar: UNIT,
    }
}

fn pool_positions(env: &Env, index: u32, supply: i128) -> Positions {
    let mut supplies = Map::new(env);
    if supply > 0 {
        supplies.set(index, supply);
    }
    Positions {
        collateral: Map::new(env),
        liabilities: Map::new(env),
        supply: supplies,
    }
}

fn pool_supply(env: &Env, address: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&PoolKey::Supply(address.clone()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Hostile strategy double
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
enum StratKey {
    Vault,
    Underlying,
    Sink,
    /// Underlying units silently kept back on each deposit, modelling an
    /// upstream that floors in its own favour.
    Shave,
    /// Upper bound on realizable liquidity, or absent for "all of it".
    LiquidityCap,
    /// Amount added to the *reported* withdrawal return beyond what was actually
    /// delivered, modelling a strategy that lies about its own delivery.
    ReportInflation,
    Paused,
    /// Underlying this strategy was actually handed to invest, as opposed to
    /// whatever happens to sit in its token balance. The difference is idle:
    /// donations, or a residue nobody accounted for.
    Invested,
    /// When set, `total_assets` reports `Invested` rather than the raw token
    /// balance — the conformant shape, immune to donation. Default `false`, so
    /// the mock stays hostile unless a test asks otherwise.
    ExcludeIdle,
}

#[contract]
pub struct MockStrategy;

#[contractimpl]
impl MockStrategy {
    pub fn initialize(env: Env, vault: Address, underlying: Address, sink: Address) {
        env.storage().instance().set(&StratKey::Vault, &vault);
        env.storage()
            .instance()
            .set(&StratKey::Underlying, &underlying);
        env.storage().instance().set(&StratKey::Sink, &sink);
    }

    pub fn set_shave(env: Env, shave: i128) {
        env.storage().instance().set(&StratKey::Shave, &shave);
    }

    pub fn set_liquidity_cap(env: Env, cap: i128) {
        env.storage().instance().set(&StratKey::LiquidityCap, &cap);
    }

    pub fn set_report_inflation(env: Env, extra: i128) {
        env.storage()
            .instance()
            .set(&StratKey::ReportInflation, &extra);
    }

    pub fn set_paused(env: Env, paused: bool) {
        env.storage().instance().set(&StratKey::Paused, &paused);
    }

    /// Switches between the two shapes of strategy: raw-balance valuation
    /// (hostile, donation-inflatable) and invested-only valuation (conformant,
    /// what `strategy-blend` does).
    pub fn set_exclude_idle(env: Env, exclude: bool) {
        env.storage().instance().set(&StratKey::ExcludeIdle, &exclude);
    }

    /// Underlying this strategy was handed to invest.
    pub fn invested(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&StratKey::Invested)
            .unwrap_or(0_i128)
    }

    /// Underlying sitting here that nobody deposited — a donation, or a residue.
    pub fn idle(env: Env) -> i128 {
        Self::raw_balance(&env) - Self::invested(env.clone())
    }

    // --- YieldStrategy ABI ---------------------------------------------------

    pub fn underlying(env: Env) -> Address {
        env.storage().instance().get(&StratKey::Underlying).unwrap()
    }

    pub fn vault(env: Env) -> Address {
        env.storage().instance().get(&StratKey::Vault).unwrap()
    }

    pub fn total_assets(env: Env) -> i128 {
        if env
            .storage()
            .instance()
            .get::<_, bool>(&StratKey::ExcludeIdle)
            .unwrap_or(false)
        {
            Self::invested(env)
        } else {
            Self::raw_balance(&env)
        }
    }

    pub fn max_withdraw(env: Env) -> i128 {
        let assets = Self::total_assets(env.clone());
        match env.storage().instance().get::<_, i128>(&StratKey::LiquidityCap) {
            Some(cap) if cap < assets => cap,
            _ => assets,
        }
    }

    pub fn deposit(env: Env, vault: Address, amount: i128) -> i128 {
        Self::require_vault(&env, &vault);
        if env
            .storage()
            .instance()
            .get::<_, bool>(&StratKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, StrategyError::UpstreamPaused);
        }
        let underlying: Address = env.storage().instance().get(&StratKey::Underlying).unwrap();
        let before = Self::raw_balance(&env);
        token::TokenClient::new(&env, &underlying).transfer(
            &vault,
            &MuxedAddress::from(&env.current_contract_address()),
            &amount,
        );

        // Floor in our own favour: quietly move `shave` out of reach.
        let shave: i128 = env
            .storage()
            .instance()
            .get(&StratKey::Shave)
            .unwrap_or(0_i128);
        if shave > 0 {
            let sink: Address = env.storage().instance().get(&StratKey::Sink).unwrap();
            authorize_transfer(&env, &underlying, &sink, shave);
            token::TokenClient::new(&env, &underlying).transfer(
                &env.current_contract_address(),
                &MuxedAddress::from(&sink),
                &shave,
            );
        }

        let after = Self::raw_balance(&env);
        let credited = after - before;
        let invested = Self::invested(env.clone());
        env.storage()
            .instance()
            .set(&StratKey::Invested, &(invested + credited));
        credited
    }

    pub fn withdraw(env: Env, vault: Address, amount: i128, min_underlying_out: i128) -> i128 {
        Self::require_vault(&env, &vault);
        let underlying: Address = env.storage().instance().get(&StratKey::Underlying).unwrap();
        let realizable = Self::max_withdraw(env.clone());
        let delivered = if amount < realizable { amount } else { realizable };
        if delivered <= 0 {
            panic_with_error!(&env, StrategyError::WithdrawalFailed);
        }
        if delivered < min_underlying_out {
            panic_with_error!(&env, StrategyError::SlippageExceeded);
        }

        // Idle first, like the real adapter: a donation leaves as underlying to
        // a redeemer rather than repricing everyone's shares where it sits.
        let idle = Self::idle(env.clone());
        let idle_used = if idle < delivered { idle } else { delivered };
        let invested = Self::invested(env.clone());
        env.storage()
            .instance()
            .set(&StratKey::Invested, &(invested - (delivered - idle_used)));

        authorize_transfer(&env, &underlying, &vault, delivered);
        token::TokenClient::new(&env, &underlying).transfer(
            &env.current_contract_address(),
            &MuxedAddress::from(&vault),
            &delivered,
        );

        let inflation: i128 = env
            .storage()
            .instance()
            .get(&StratKey::ReportInflation)
            .unwrap_or(0_i128);
        delivered + inflation
    }

    pub fn touch(env: Env) {
        env.storage().instance().extend_ttl(1, 100);
    }

    fn require_vault(env: &Env, caller: &Address) {
        let vault: Address = env.storage().instance().get(&StratKey::Vault).unwrap();
        if *caller != vault {
            panic_with_error!(env, StrategyError::NotVault);
        }
        caller.require_auth();
    }

    fn raw_balance(env: &Env) -> i128 {
        let underlying: Address = env.storage().instance().get(&StratKey::Underlying).unwrap();
        token::TokenClient::new(env, &underlying).balance(&env.current_contract_address())
    }
}

// ---------------------------------------------------------------------------

fn authorize_transfer(env: &Env, underlying: &Address, to: &Address, amount: i128) {
    let me = env.current_contract_address();
    env.authorize_as_current_contract(vec![
        env,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: underlying.clone(),
                fn_name: Symbol::new(env, "transfer"),
                args: vec![
                    env,
                    me.into_val(env),
                    to.clone().into_val(env),
                    amount.into_val(env),
                ],
            },
            sub_invocations: vec![env],
        }),
    ]);
}
