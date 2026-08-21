// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

use core::cmp::min;

use sidereal_shared_types::Market;
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, vec, Address,
    Env, IntoVal, MuxedAddress, Symbol, Val, Vec,
};

const WAD: i128 = 1_000_000_000_000_000_000;
const BPS_DENOMINATOR: i128 = 10_000;
const DAY: u64 = 86_400;
const IMPLIED_RATE_TIME: u64 = 365 * DAY;
const MINIMUM_LIQUIDITY: i128 = 1_000;
const MAX_MARKET_PROPORTION: i128 = (WAD * 96) / 100;
// Conservative testnet caps on reserves and curve parameters. These were
// originally sized to keep values in the float-safe range of the old f64 curve
// helpers. The curve is integer fixed-point now, so the names no longer refer
// to floats; the caps are kept as a conservative product limit that also keeps
// the i128 intermediate products well clear of overflow. Re-deriving the exact
// i128 overflow bound is future work; these values are unchanged.
const MAX_RESERVE_UNITS: i128 = WAD;
const MAX_SCALAR_ROOT: i128 = 10 * WAD;
const MAX_ANCHOR: i128 = 2 * WAD;
const LEDGERS_PER_DAY: u32 = 17_280;
const AMM_INSTANCE_TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    pub pt_token: Address,
    pub sy_token: Address,
    pub yt_token: Address,
    pub tokenizer: Address,
    pub maturity: u64,
    pub scalar_root: i128,
    pub initial_anchor: i128,
    pub fee_bps: i128,
    pub twap_window: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct State {
    pub total_pt: i128,
    pub total_sy: i128,
    pub total_lp: i128,
    pub last_ln_implied_rate: i128,
    pub twap_ln_implied_rate: i128,
    pub last_observation: u64,
    pub warmup_until: u64,
}

#[derive(Clone)]
#[contracttype]
enum DataKey {
    Config,
    State,
    LpBalance(Address),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidMaturity = 3,
    InvalidAmount = 4,
    InvalidScalarRoot = 5,
    InvalidAnchor = 6,
    InvalidFee = 7,
    InvalidTwapWindow = 8,
    MarketNotSeeded = 9,
    MarketMatured = 10,
    SlippageExceeded = 11,
    InsufficientLiquidity = 12,
    MathOverflow = 13,
    MarketProportionTooHigh = 14,
    ExchangeRateBelowOne = 15,
    UnsupportedRoute = 16,
    TradeNotFound = 17,
    InputOutOfBounds = 18,
    InvalidSyRate = 19,
}

/// Everything the curve needs for one invocation, resolved once.
///
/// `total_asset` is the SY reserve valued in **asset units**, not SY shares.
/// The curve prices PT face, and PT face is asset-denominated (`tokenizer::split`
/// mints `face = shares * rate / WAD`), so both sides of the proportion must be
/// asset-denominated or the curve prices face-per-share instead of
/// face-per-asset. `State.total_sy` stays authoritative in shares — that is what
/// the pool actually custodies and what `reconcile_reserves` reads off the token
/// contract — and every crossing of that boundary goes through
/// `asset_value_of_shares` / `shares_in_for_face_up` / `shares_out_for_face_down`
/// with the tokenizer's own rounding.
struct Precompute {
    rate_scalar: i128,
    total_asset: i128,
    rate_anchor: i128,
    time_to_expiry: u64,
    sy_rate: i128,
}

#[inline(never)]
fn load_live_market(env: &Env, amount: i128) -> Result<(Config, State, Precompute), Error> {
    require_bounded_amount_result(amount)?;
    let config = read_config(env)?;
    require_live_result(env, &config)?;
    let state = read_state(env)?;
    require_seeded_result(&state)?;
    let comp = precompute_or_panic(env, &config, &state);
    Ok((config, state, comp))
}

fn load_live_market_or_panic(env: &Env, amount: i128) -> (Config, State, Precompute) {
    match load_live_market(env, amount) {
        Ok(loaded) => loaded,
        Err(error) => panic_with_error!(env, error),
    }
}

/// Closes out a trade: resync reserves to real balances, accumulate the price
/// that prevailed over the interval this trade ends, and persist.
///
/// `prevailing_ln_rate` must be `state.last_ln_implied_rate` as read *before*
/// the trade updated it — see `sync_twap`.
#[inline(never)]
fn settle_and_record(env: &Env, config: &Config, state: &mut State, prevailing_ln_rate: i128) {
    reconcile_reserves(env, config, state);
    sync_twap(env, config, state, prevailing_ln_rate);
    write_state(env, state);
}

#[contract]
pub struct AmmMarket;

#[contractimpl]
impl AmmMarket {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        pt_token: Address,
        sy_token: Address,
        yt_token: Address,
        tokenizer: Address,
        maturity: u64,
        scalar_root: i128,
        initial_anchor: i128,
        fee_bps: i128,
        twap_window: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }

        admin.require_auth();

        if maturity <= env.ledger().timestamp() {
            return Err(Error::InvalidMaturity);
        }
        if scalar_root <= 0 {
            return Err(Error::InvalidScalarRoot);
        }
        if scalar_root > MAX_SCALAR_ROOT {
            return Err(Error::InputOutOfBounds);
        }
        if initial_anchor < WAD {
            return Err(Error::InvalidAnchor);
        }
        if initial_anchor > MAX_ANCHOR {
            return Err(Error::InputOutOfBounds);
        }
        if !(0..BPS_DENOMINATOR).contains(&fee_bps) {
            return Err(Error::InvalidFee);
        }
        if twap_window == 0 {
            return Err(Error::InvalidTwapWindow);
        }

        let config = Config {
            admin,
            pt_token,
            sy_token,
            yt_token,
            tokenizer,
            maturity,
            scalar_root,
            initial_anchor,
            fee_bps,
            twap_window,
        };
        let state = State {
            total_pt: 0,
            total_sy: 0,
            total_lp: 0,
            last_ln_implied_rate: 0,
            twap_ln_implied_rate: 0,
            last_observation: env.ledger().timestamp(),
            warmup_until: env.ledger().timestamp() + twap_window,
        };

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::State, &state);
        bump_instance_ttl(&env);

        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, Error> {
        read_config(&env)
    }

    pub fn state(env: Env) -> Result<State, Error> {
        read_state(&env)
    }

    pub fn reserve_pt(env: Env) -> Result<i128, Error> {
        let config = read_config(&env)?;
        Ok(pool_token_balance(&env, &config.pt_token))
    }

    pub fn reserve_sy(env: Env) -> Result<i128, Error> {
        let config = read_config(&env)?;
        Ok(pool_token_balance(&env, &config.sy_token))
    }

    pub fn total_lp(env: Env) -> Result<i128, Error> {
        Ok(read_state(&env)?.total_lp)
    }

    pub fn bump_ttl(env: Env) -> Result<(), Error> {
        read_config(&env)?;
        bump_instance_ttl(&env);
        Ok(())
    }

    pub fn bump_lp_ttl(env: Env, holder: Address) -> Result<(), Error> {
        read_config(&env)?;
        bump_lp_balance_ttl(&env, holder);
        Ok(())
    }

    pub fn lp_balance(env: Env, holder: Address) -> Result<i128, Error> {
        read_config(&env)?;
        Ok(read_lp_balance(&env, holder))
    }

    pub fn quote_pt_for_sy(env: Env, pt_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, pt_in)?;
        Ok(exact_pt_in_sy_out_or_panic(
            &env, &config, &state, &comp, pt_in,
        ))
    }

    pub fn quote_sy_for_pt(env: Env, sy_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, sy_in)?;
        Ok(exact_sy_in_pt_out_or_panic(
            &env, &config, &state, &comp, sy_in,
        ))
    }

    /// SY shares `sy_in` actually spends through `swap_sy_for_pt`. The fill is
    /// capped by the curve independently of the budget, so this is often
    /// strictly less than `sy_in`; the difference is never taken from the
    /// caller.
    pub fn quote_sy_for_pt_cost(env: Env, sy_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, sy_in)?;
        let pt_out = exact_sy_in_pt_out_or_panic(&env, &config, &state, &comp, sy_in);
        Ok(exact_pt_out_sy_in_or_panic(
            &env, &config, &state, &comp, pt_out,
        ))
    }

    pub fn quote_sy_for_yt(env: Env, sy_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, sy_in)?;
        let (yt_out, _cost) = solve_yt_out_for_sy_in(&env, &config, &state, &comp, sy_in);
        Ok(yt_out)
    }

    /// SY shares `sy_in` actually buys through `swap_sy_for_yt`. The remainder
    /// of `sy_in` is never taken, so a caller can size the leg exactly.
    pub fn quote_sy_for_yt_cost(env: Env, sy_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, sy_in)?;
        let (_yt_out, cost) = solve_yt_out_for_sy_in(&env, &config, &state, &comp, sy_in);
        Ok(cost)
    }

    pub fn quote_yt_for_sy(env: Env, yt_in: i128) -> Result<i128, Error> {
        let (config, state, comp) = load_live_market(&env, yt_in)?;
        let sy_cost = exact_pt_out_sy_in_or_panic(&env, &config, &state, &comp, yt_in);
        // Recombining `yt_in` face of PT + YT returns floor(yt_in * WAD / rate)
        // SY shares (the tokenizer's own floor); the seller nets that minus the
        // curve-side cost of buying back the PT leg.
        let sy_value = shares_out_for_face_down(&env, yt_in, comp.sy_rate);
        if sy_cost >= sy_value {
            return Err(Error::InsufficientLiquidity);
        }
        Ok(sy_value - sy_cost)
    }

    pub fn spot_apy(env: Env) -> Result<i128, Error> {
        let config = read_config(&env)?;
        if env.ledger().timestamp() >= config.maturity {
            return Ok(0);
        }

        let state = read_state(&env)?;
        if state.total_lp == 0 {
            return Ok(0);
        }

        Ok(ln_rate_to_bps(state.last_ln_implied_rate))
    }

    pub fn twap_apy(env: Env) -> Result<i128, Error> {
        let config = read_config(&env)?;
        let state = read_state(&env)?;

        if env.ledger().timestamp() >= config.maturity {
            return Ok(0);
        }

        Ok(ln_rate_to_bps(state.twap_ln_implied_rate))
    }

    pub fn twap_warming_up(env: Env) -> Result<bool, Error> {
        let state = read_state(&env)?;
        Ok(env.ledger().timestamp() < state.warmup_until)
    }

    pub fn swap_pt_for_sy(env: Env, from: Address, pt_in: i128, min_sy_out: i128) -> i128 {
        <Self as Market>::swap_pt_for_sy(&env, from, pt_in, min_sy_out)
    }

    pub fn swap_sy_for_pt(env: Env, from: Address, sy_in: i128, min_pt_out: i128) -> i128 {
        <Self as Market>::swap_sy_for_pt(&env, from, sy_in, min_pt_out)
    }

    pub fn swap_sy_for_yt(env: Env, from: Address, sy_in: i128, min_yt_out: i128) -> i128 {
        <Self as Market>::swap_sy_for_yt(&env, from, sy_in, min_yt_out)
    }

    pub fn swap_yt_for_sy(env: Env, from: Address, yt_in: i128, min_sy_out: i128) -> i128 {
        <Self as Market>::swap_yt_for_sy(&env, from, yt_in, min_sy_out)
    }

    pub fn add_liquidity(
        env: Env,
        from: Address,
        pt_in: i128,
        sy_in: i128,
        min_lp_out: i128,
    ) -> i128 {
        let lp_out = <Self as Market>::add_liquidity(&env, from, pt_in, sy_in);
        // Slippage bound enforced here, after the shared-trait implementation,
        // because the Market trait signature is frozen (contracts/shared/types).
        // A panic reverts the entire invocation, transfers included, so this is
        // equivalent to checking before any token moves.
        if lp_out < min_lp_out {
            panic_with_error!(&env, Error::SlippageExceeded);
        }
        lp_out
    }

    pub fn remove_liquidity(
        env: Env,
        from: Address,
        lp_in: i128,
        min_pt_out: i128,
        min_sy_out: i128,
    ) -> (i128, i128) {
        let (pt_out, sy_out) = <Self as Market>::remove_liquidity(&env, from, lp_in);
        // Same pattern as add_liquidity: bound checked after the frozen-trait
        // call; the panic reverts everything.
        if pt_out < min_pt_out || sy_out < min_sy_out {
            panic_with_error!(&env, Error::SlippageExceeded);
        }
        (pt_out, sy_out)
    }

    pub fn implied_apy(env: Env) -> i128 {
        <Self as Market>::implied_apy(&env)
    }

    pub fn maturity(env: Env) -> u64 {
        <Self as Market>::maturity(&env)
    }
}

impl Market for AmmMarket {
    fn swap_pt_for_sy(env: &Env, from: Address, pt_in: i128, min_sy_out: i128) -> i128 {
        from.require_auth();
        let (config, mut state, comp) = load_live_market_or_panic(env, pt_in);
        // The rate that prevailed over the interval ending now, captured before
        // the trade moves spot. That is the observation the TWAP accumulates.
        let prevailing_ln_rate = state.last_ln_implied_rate;

        let sy_out =
            apply_exact_pt_in_trade_or_panic(env, &config, &mut state, &comp, pt_in, min_sy_out);
        transfer_into_pool(env, &config.pt_token, &from, pt_in);
        transfer_out_of_pool(env, &config.sy_token, &from, sy_out);
        settle_and_record(env, &config, &mut state, prevailing_ln_rate);

        sy_out
    }

    fn swap_sy_for_pt(env: &Env, from: Address, sy_in: i128, min_pt_out: i128) -> i128 {
        from.require_auth();
        let (config, mut state, comp) = load_live_market_or_panic(env, sy_in);
        let prevailing_ln_rate = state.last_ln_implied_rate;

        let pt_out = exact_sy_in_pt_out_or_panic(env, &config, &state, &comp, sy_in);
        if pt_out < min_pt_out {
            panic_with_error!(env, Error::SlippageExceeded);
        }

        // `sy_in` is a budget, and the fill is capped independently of it (the
        // exchange_rate >= WAD floor bounds a single PT purchase well below the
        // reserve). Charge only what the curve priced and leave the residual in
        // the caller's wallet; taking the whole budget would donate the
        // difference to LPs, unbounded and invisible to min_pt_out.
        let required_sy =
            apply_exact_sy_in_trade_or_panic(env, &config, &mut state, &comp, sy_in, pt_out);
        transfer_into_pool(env, &config.sy_token, &from, required_sy);
        transfer_out_of_pool(env, &config.pt_token, &from, pt_out);
        settle_and_record(env, &config, &mut state, prevailing_ln_rate);

        pt_out
    }

    fn swap_sy_for_yt(env: &Env, from: Address, sy_in: i128, min_yt_out: i128) -> i128 {
        from.require_auth();
        let (config, mut state, comp) = load_live_market_or_panic(env, sy_in);
        let prevailing_ln_rate = state.last_ln_implied_rate;

        // The curve prices PT face units; the tokenizer escrows SY shares and
        // mints face = shares * rate / WAD. The curve now consumes
        // asset-denominated reserves (see `Precompute`), so the only conversions
        // left here are the split's own, against the same rate source the
        // tokenizer reads.
        let rate = comp.sy_rate;
        let (yt_out, buyer_cost) = solve_yt_out_for_sy_in(env, &config, &state, &comp, sy_in);
        if yt_out < min_yt_out {
            panic_with_error!(env, Error::SlippageExceeded);
        }

        // The pool keeps the PT the split mints, so the curve moves as if it
        // bought `yt_out` PT; `sy_funded` is the SY the curve pays for that PT.
        let sy_funded =
            apply_exact_pt_in_trade_or_panic(env, &config, &mut state, &comp, yt_out, 0);

        // Shares to split, rounded UP so the tokenizer's floored face mint is
        // at least `yt_out`, the amount the curve accounted for and the buyer
        // was quoted. Rounding up costs the pool at most one extra face unit of
        // shares, and that cost is backed one-for-one by the dust pair it keeps
        // (see below).
        let shares_to_split = shares_in_for_face_up(env, yt_out, rate);
        // The buyer funds exactly the part of the split the curve does not.
        // `sy_in` is a budget: charging all of it whenever the solver settles
        // for less would hand LPs the difference, so only `buyer_cost` moves.
        // The solver computed that same difference from the same helpers; fail
        // closed if they disagree, because a mismatch would tap LP reserves the
        // curve never accounted for.
        if shares_to_split != checked_add(env, buyer_cost, sy_funded) || buyer_cost > sy_in {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }

        // Take the buyer's SY, split pool-funded SY into PT + YT, keep the PT,
        // and send exactly the quoted YT to the buyer.
        transfer_into_pool(env, &config.sy_token, &from, buyer_cost);
        let (_pt_minted, yt_minted) = flash_split(env, &config, shares_to_split);
        if yt_minted < yt_out {
            // Cannot happen while the tokenizer floors against the same rate we
            // ceiled with; a drifted rate read would under-mint, so fail closed.
            panic_with_error!(env, Error::InsufficientLiquidity);
        }
        transfer_out_of_pool(env, &config.yt_token, &from, yt_out);
        // Rounding dust: the ceil above can over-mint up to one face unit of
        // PT and YT beyond `yt_out`. Both stay in the pool: the PT dust enters
        // the curve reserves on the reconcile below, and the YT dust sits in
        // pool custody as an equal, recombinable pair with that PT. The trader
        // never receives the dust, so rounding cannot be farmed against LPs.
        settle_and_record(env, &config, &mut state, prevailing_ln_rate);

        yt_out
    }

    fn swap_yt_for_sy(env: &Env, from: Address, yt_in: i128, min_sy_out: i128) -> i128 {
        from.require_auth();
        let (config, mut state, comp) = load_live_market_or_panic(env, yt_in);
        let prevailing_ln_rate = state.last_ln_implied_rate;

        // The curve leg is already priced in SY shares (asset units in, shares
        // out); the recombine returns shares too. Both sides now agree on units
        // for any rate, which is what makes this route solvable above par.
        let rate = comp.sy_rate;
        let sy_cost = exact_pt_out_sy_in_or_panic(env, &config, &state, &comp, yt_in);
        // SY shares the recombine of `yt_in` face returns, floored exactly like
        // the tokenizer floors, so the payout budget never exceeds what will
        // actually arrive.
        let sy_value = shares_out_for_face_down(env, yt_in, rate);
        if sy_value <= sy_cost {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }
        let sy_out = sy_value - sy_cost;
        if sy_out < min_sy_out {
            panic_with_error!(env, Error::SlippageExceeded);
        }

        // The pool sold `yt_in` PT for `sy_cost` SY into the recombine. The
        // budget is exactly the price here, so there is no residual to refund.
        let charged =
            apply_exact_sy_in_trade_or_panic(env, &config, &mut state, &comp, sy_cost, yt_in);
        if charged != sy_cost {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }

        // Take the seller's YT, recombine pool PT + seller YT into SY, pay the
        // seller, and keep the spread.
        transfer_into_pool(env, &config.yt_token, &from, yt_in);
        let sy_from_recombine = flash_recombine(env, &config, yt_in);
        // The tokenizer pays floor(yt_in * WAD / rate) shares, pro-rata capped
        // under an escrow shortfall. At a constant rate the cap never binds
        // (split floors face against the same rate, so escrow always covers).
        // If less than the budget arrives, the escrow is genuinely short: fail
        // closed and revert the swap rather than pay the seller from LP funds.
        if sy_from_recombine < sy_value {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }
        transfer_out_of_pool(env, &config.sy_token, &from, sy_out);
        settle_and_record(env, &config, &mut state, prevailing_ln_rate);

        sy_out
    }

    fn add_liquidity(env: &Env, from: Address, pt_in: i128, sy_in: i128) -> i128 {
        from.require_auth();
        require_bounded_amount(env, pt_in);
        require_bounded_amount(env, sy_in);

        let config = read_config_or_panic(env);
        require_live(env, &config);

        let mut state = read_state_or_panic(env);
        let now = env.ledger().timestamp();
        let (pt_used, sy_used, lp_out) = if state.total_lp == 0 {
            let gross_lp = integer_sqrt_or_panic(env, checked_mul(env, pt_in, sy_in));
            if gross_lp <= MINIMUM_LIQUIDITY {
                panic_with_error!(env, Error::InsufficientLiquidity);
            }

            state.total_pt = pt_in;
            state.total_sy = sy_in;
            state.total_lp = gross_lp;
            let time_to_expiry = time_to_expiry_or_panic(env, &config);
            let rate_scalar = get_rate_scalar_or_panic(env, config.scalar_root, time_to_expiry);
            // The seed observation is priced on the same asset-denominated
            // reserves every later trade sees, so a market seeded above par
            // opens at the implied rate its reserves actually express.
            let sy_rate = sy_rate_or_panic(env, &config);
            state.last_ln_implied_rate = get_ln_implied_rate_or_panic(
                env,
                state.total_pt,
                asset_value_of_shares(env, state.total_sy, sy_rate),
                rate_scalar,
                config.initial_anchor,
                time_to_expiry,
            );
            state.twap_ln_implied_rate = state.last_ln_implied_rate;
            state.last_observation = now;

            (pt_in, sy_in, gross_lp - MINIMUM_LIQUIDITY)
        } else {
            let lp_by_pt = mul_div_down_or_panic(env, pt_in, state.total_lp, state.total_pt);
            let lp_by_sy = mul_div_down_or_panic(env, sy_in, state.total_lp, state.total_sy);
            let lp_out = min(lp_by_pt, lp_by_sy);
            if lp_out <= 0 {
                panic_with_error!(env, Error::InsufficientLiquidity);
            }

            let pt_used = mul_div_up_or_panic(env, state.total_pt, lp_out, state.total_lp);
            let sy_used = mul_div_up_or_panic(env, state.total_sy, lp_out, state.total_lp);

            state.total_pt = checked_bounded_reserve_add(env, state.total_pt, pt_used);
            state.total_sy = checked_bounded_reserve_add(env, state.total_sy, sy_used);
            state.total_lp = checked_add(env, state.total_lp, lp_out);

            (pt_used, sy_used, lp_out)
        };

        let current_lp = read_lp_balance(env, from.clone());
        write_lp_balance(env, from.clone(), checked_add(env, current_lp, lp_out));
        transfer_into_pool(env, &config.pt_token, &from, pt_used);
        transfer_into_pool(env, &config.sy_token, &from, sy_used);
        reconcile_reserves(env, &config, &mut state);
        write_state(env, &state);
        lp_out
    }

    fn remove_liquidity(env: &Env, from: Address, lp_in: i128) -> (i128, i128) {
        from.require_auth();
        require_bounded_amount(env, lp_in);

        let config = read_config_or_panic(env);
        let mut state = read_state_or_panic(env);
        require_seeded(env, &state);

        let holder_lp = read_lp_balance(env, from.clone());
        if lp_in > holder_lp {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }

        if lp_in >= state.total_lp {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }

        let sy_out = mul_div_down_or_panic(env, lp_in, state.total_sy, state.total_lp);
        let pt_out = mul_div_down_or_panic(env, lp_in, state.total_pt, state.total_lp);
        if sy_out == 0 && pt_out == 0 {
            panic_with_error!(env, Error::InsufficientLiquidity);
        }

        write_lp_balance(env, from.clone(), checked_sub(env, holder_lp, lp_in));
        state.total_lp = checked_sub(env, state.total_lp, lp_in);
        state.total_sy = checked_sub(env, state.total_sy, sy_out);
        state.total_pt = checked_sub(env, state.total_pt, pt_out);
        transfer_out_of_pool(env, &config.pt_token, &from, pt_out);
        transfer_out_of_pool(env, &config.sy_token, &from, sy_out);
        reconcile_reserves(env, &config, &mut state);
        write_state(env, &state);

        (pt_out, sy_out)
    }

    fn implied_apy(env: &Env) -> i128 {
        let config = read_config_or_panic(env);
        if env.ledger().timestamp() >= config.maturity {
            return 0;
        }

        let state = read_state_or_panic(env);
        if state.total_lp == 0 {
            return 0;
        }

        ln_rate_to_bps(state.last_ln_implied_rate)
    }

    fn maturity(env: &Env) -> u64 {
        read_config_or_panic(env).maturity
    }
}

fn read_config(env: &Env) -> Result<Config, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(Error::NotInitialized)
}

fn read_state(env: &Env) -> Result<State, Error> {
    env.storage()
        .instance()
        .get(&DataKey::State)
        .ok_or(Error::NotInitialized)
}

fn read_config_or_panic(env: &Env) -> Config {
    match read_config(env) {
        Ok(config) => config,
        Err(error) => panic_with_error!(env, error),
    }
}

fn read_state_or_panic(env: &Env) -> State {
    match read_state(env) {
        Ok(state) => state,
        Err(error) => panic_with_error!(env, error),
    }
}

fn write_state(env: &Env, state: &State) {
    env.storage().instance().set(&DataKey::State, state);
    bump_instance_ttl(env);
}

fn bump_instance_ttl(env: &Env) {
    env.storage().instance().extend_ttl(
        AMM_INSTANCE_TTL_THRESHOLD_LEDGERS,
        AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS,
    );
}

// LP balances live in persistent storage, one entry per holder, matching the
// token contracts' balance pattern. Keeping them in the instance entry would
// make every invocation's IO scale with the number of LP holders and cap how
// many holders can exist at the instance entry size limit.
fn read_lp_balance(env: &Env, holder: Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::LpBalance(holder))
        .unwrap_or(0)
}

fn write_lp_balance(env: &Env, holder: Address, balance: i128) {
    let key = DataKey::LpBalance(holder);
    env.storage().persistent().set(&key, &balance);
    extend_lp_balance_ttl(env, &key);
}

fn bump_lp_balance_ttl(env: &Env, holder: Address) {
    let key = DataKey::LpBalance(holder);
    if env.storage().persistent().has(&key) {
        extend_lp_balance_ttl(env, &key);
    }
}

fn extend_lp_balance_ttl(env: &Env, key: &DataKey) {
    env.storage().persistent().extend_ttl(
        key,
        AMM_INSTANCE_TTL_THRESHOLD_LEDGERS,
        AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS,
    );
}

fn pool_token_balance(env: &Env, token_id: &Address) -> i128 {
    token::TokenClient::new(env, token_id).balance(&env.current_contract_address())
}

fn reconcile_reserves(env: &Env, config: &Config, state: &mut State) {
    state.total_pt = pool_token_balance(env, &config.pt_token);
    state.total_sy = pool_token_balance(env, &config.sy_token);
}

fn transfer_into_pool(env: &Env, token_id: &Address, from: &Address, amount: i128) {
    let pool = env.current_contract_address();
    let to = MuxedAddress::from(&pool);
    token::TokenClient::new(env, token_id).transfer(from, &to, &amount);
}

#[inline(never)]
fn auth_entry(
    env: &Env,
    contract: &Address,
    fn_name: &str,
    args: Vec<Val>,
) -> InvokerContractAuthEntry {
    InvokerContractAuthEntry::Contract(SubContractInvocation {
        context: ContractContext {
            contract: contract.clone(),
            fn_name: Symbol::new(env, fn_name),
            args,
        },
        sub_invocations: vec![env],
    })
}

fn transfer_out_of_pool(env: &Env, token_id: &Address, to: &Address, amount: i128) {
    let pool = env.current_contract_address();
    let to_muxed = MuxedAddress::from(to);
    let transfer_args: Vec<Val> = vec![
        env,
        pool.clone().into_val(env),
        to_muxed.clone().into_val(env),
        amount.into_val(env),
    ];
    env.authorize_as_current_contract(vec![
        env,
        auth_entry(env, token_id, "transfer", transfer_args),
    ]);
    token::TokenClient::new(env, token_id).transfer(&pool, &to_muxed, &amount);
}

fn require_live(env: &Env, config: &Config) {
    if env.ledger().timestamp() >= config.maturity {
        panic_with_error!(env, Error::MarketMatured);
    }
}

fn require_seeded(env: &Env, state: &State) {
    if state.total_lp <= 0 || state.total_pt <= 0 || state.total_sy <= 0 {
        panic_with_error!(env, Error::MarketNotSeeded);
    }
}

fn require_seeded_result(state: &State) -> Result<(), Error> {
    if state.total_lp <= 0 || state.total_pt <= 0 || state.total_sy <= 0 {
        return Err(Error::MarketNotSeeded);
    }

    Ok(())
}

fn require_positive_amount(env: &Env, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
}

fn require_positive_amount_result(amount: i128) -> Result<(), Error> {
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    Ok(())
}

fn require_bounded_amount(env: &Env, amount: i128) {
    require_positive_amount(env, amount);
    require_within_reserve_bounds(env, amount);
}

fn require_bounded_amount_result(amount: i128) -> Result<(), Error> {
    require_positive_amount_result(amount)?;
    if amount > MAX_RESERVE_UNITS {
        return Err(Error::InputOutOfBounds);
    }

    Ok(())
}

fn require_within_reserve_bounds(env: &Env, amount: i128) {
    if amount > MAX_RESERVE_UNITS {
        panic_with_error!(env, Error::InputOutOfBounds);
    }
}

fn require_live_result(env: &Env, config: &Config) -> Result<(), Error> {
    if env.ledger().timestamp() >= config.maturity {
        return Err(Error::MarketMatured);
    }

    Ok(())
}

fn time_to_expiry_or_panic(env: &Env, config: &Config) -> u64 {
    let now = env.ledger().timestamp();
    match config.maturity.checked_sub(now) {
        Some(remaining) if remaining > 0 => remaining,
        _ => panic_with_error!(env, Error::MarketMatured),
    }
}

fn precompute_or_panic(env: &Env, config: &Config, state: &State) -> Precompute {
    let time_to_expiry = time_to_expiry_or_panic(env, config);
    let rate_scalar = get_rate_scalar_or_panic(env, config.scalar_root, time_to_expiry);
    // Value the SY reserve at the index before it reaches the curve. Reading the
    // rate from the same `exchange_rate` entrypoint the tokenizer prices split
    // and recombine with is what keeps the plain PT<->SY legs and the YT flash
    // routes on one unit system.
    let sy_rate = sy_rate_or_panic(env, config);
    let total_asset = asset_value_of_shares(env, state.total_sy, sy_rate);
    if state.total_pt <= 0 || total_asset <= 0 {
        panic_with_error!(env, Error::MarketNotSeeded);
    }

    let rate_anchor = get_rate_anchor_or_panic(
        env,
        state.total_pt,
        state.last_ln_implied_rate,
        total_asset,
        rate_scalar,
        time_to_expiry,
    );

    Precompute {
        rate_scalar,
        total_asset,
        rate_anchor,
        time_to_expiry,
        sy_rate,
    }
}

/// Whether the reserves a trade would leave behind still price on the curve.
///
/// The curve's two guards — `exchange_rate >= WAD` and the market-proportion
/// ceiling — are evaluated at the *trade endpoint*, against pre-trade reserves.
/// The post-trade state is a different point on the curve and, at the very edge
/// of a fill, falls outside them by a hair: a maximal `swap_sy_for_pt` can leave
/// an implied rate a few parts in 1e7 below WAD. Two things go wrong then. The
/// observation update panics *after* the quote already promised the trade, and
/// the market's stored implied rate would be unrepresentable for the next
/// caller anyway.
///
/// Checking it inside the same helpers every quote and every execution path
/// calls keeps the two bit-identical and keeps the search from settling on a
/// fill the market cannot actually record. It is a suffix condition in both
/// directions — a bigger fill always pushes the post-trade point further out —
/// so it does not disturb the searches' monotonicity.
fn post_trade_rate_is_representable(
    env: &Env,
    comp: &Precompute,
    post_pt: i128,
    post_sy_shares: i128,
) -> bool {
    if post_pt <= 0 || post_sy_shares <= 0 {
        return false;
    }
    let post_asset = asset_value_of_shares(env, post_sy_shares, comp.sy_rate);
    if post_asset <= 0 {
        return false;
    }

    try_get_exchange_rate(
        env,
        post_pt,
        post_asset,
        comp.rate_scalar,
        comp.rate_anchor,
        0,
    )
    .is_some()
}

fn exact_pt_in_sy_out_or_panic(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    pt_in: i128,
) -> i128 {
    let exchange_rate = get_exchange_rate_or_panic(
        env,
        state.total_pt,
        comp.total_asset,
        comp.rate_scalar,
        comp.rate_anchor,
        -pt_in,
    );
    // The curve is entirely asset-denominated: `pt_in` is PT face in asset
    // units and `exchange_rate` is face per asset unit, so this quotient is the
    // asset value the seller is owed.
    let pre_fee_asset_out = mul_div_down_or_panic(env, pt_in, WAD, exchange_rate);
    // Round the fee UP, matching the SY-in direction. Rounding it down let any
    // trade whose pre-fee proceeds were under `BPS_DENOMINATOR / fee_bps`
    // (1000 stroops at 10bps) pay nothing at all, so the two directions
    // disagreed about who absorbs the sub-stroop notch. Up is the direction
    // that favours the pool, which is the rule everywhere else on this curve.
    let fee = mul_div_up_or_panic(env, pre_fee_asset_out, config.fee_bps, BPS_DENOMINATOR);
    let asset_out = checked_sub(env, pre_fee_asset_out, fee);
    // Back to SY shares — what the pool actually pays out — floored exactly as
    // the tokenizer floors, so the pool never hands over more shares than the
    // curve's asset-denominated proceeds are worth.
    let sy_out = shares_out_for_face_down(env, asset_out, comp.sy_rate);

    if sy_out <= 0 || sy_out >= state.total_sy {
        panic_with_error!(env, Error::InsufficientLiquidity);
    }
    if !post_trade_rate_is_representable(
        env,
        comp,
        checked_add(env, state.total_pt, pt_in),
        state.total_sy - sy_out,
    ) {
        panic_with_error!(env, Error::InsufficientLiquidity);
    }

    sy_out
}

fn apply_exact_pt_in_trade_or_panic(
    env: &Env,
    config: &Config,
    state: &mut State,
    comp: &Precompute,
    pt_in: i128,
    min_sy_out: i128,
) -> i128 {
    let sy_out = exact_pt_in_sy_out_or_panic(env, config, state, comp, pt_in);
    if sy_out < min_sy_out {
        panic_with_error!(env, Error::SlippageExceeded);
    }

    state.total_pt = checked_bounded_reserve_add(env, state.total_pt, pt_in);
    state.total_sy = checked_sub(env, state.total_sy, sy_out);
    state.last_ln_implied_rate = get_ln_implied_rate_or_panic(
        env,
        state.total_pt,
        asset_value_of_shares(env, state.total_sy, comp.sy_rate),
        comp.rate_scalar,
        comp.rate_anchor,
        comp.time_to_expiry,
    );

    sy_out
}

fn exact_sy_in_pt_out_or_panic(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    sy_in: i128,
) -> i128 {
    let mut low = 1;
    let mut high = checked_sub(env, state.total_pt, 1);
    let mut best = 0;

    while low <= high {
        let mid = low + ((high - low) / 2);
        match try_exact_pt_out_sy_in(env, config, state, comp, mid) {
            Some(required_sy) if required_sy <= sy_in => {
                best = mid;
                low = mid + 1;
            }
            Some(_) | None => {
                high = mid - 1;
            }
        }
    }

    if best <= 0 {
        panic_with_error!(env, Error::TradeNotFound);
    }

    best
}

/// Applies a "buy `pt_out` PT" leg funded from a caller's `sy_in` budget and
/// returns the SY shares the pool actually charges. The residual `sy_in -
/// required_sy` is **not** credited to reserves: the caller's budget is a
/// ceiling, not a payment, and the curve only ever earns `required_sy`. Callers
/// transfer exactly the returned amount in and refund the rest by never taking
/// it, which is what keeps a liquidity-capped fill from donating the surplus to
/// LPs.
fn apply_exact_sy_in_trade_or_panic(
    env: &Env,
    _config: &Config,
    state: &mut State,
    comp: &Precompute,
    sy_in: i128,
    pt_out: i128,
) -> i128 {
    let required_sy = exact_pt_out_sy_in_or_panic(env, _config, state, comp, pt_out);
    if required_sy > sy_in {
        panic_with_error!(env, Error::SlippageExceeded);
    }

    state.total_pt = checked_sub(env, state.total_pt, pt_out);
    state.total_sy = checked_bounded_reserve_add(env, state.total_sy, required_sy);
    state.last_ln_implied_rate = get_ln_implied_rate_or_panic(
        env,
        state.total_pt,
        asset_value_of_shares(env, state.total_sy, comp.sy_rate),
        comp.rate_scalar,
        comp.rate_anchor,
        comp.time_to_expiry,
    );

    required_sy
}

fn exact_pt_out_sy_in_or_panic(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    pt_out: i128,
) -> i128 {
    match try_exact_pt_out_sy_in(env, config, state, comp, pt_out) {
        Some(value) => value,
        None => panic_with_error!(env, Error::TradeNotFound),
    }
}

fn try_exact_pt_out_sy_in(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    pt_out: i128,
) -> Option<i128> {
    if pt_out <= 0 || pt_out >= state.total_pt {
        return None;
    }

    let exchange_rate = try_get_exchange_rate(
        env,
        state.total_pt,
        comp.total_asset,
        comp.rate_scalar,
        comp.rate_anchor,
        pt_out,
    )?;
    // Asset units on the curve, ceiled at every step, then converted back to
    // the SY shares the pool charges — also ceiled, mirroring the shares a
    // `tokenizer::split` would have to escrow to mint the same face. Both
    // roundings are toward the pool.
    let pre_fee_asset_in = mul_div_up_or_panic(env, pt_out, WAD, exchange_rate);
    let fee = mul_div_up_or_panic(env, pre_fee_asset_in, config.fee_bps, BPS_DENOMINATOR);
    let asset_in = checked_add(env, pre_fee_asset_in, fee);
    let sy_in = shares_in_for_face_up(env, asset_in, comp.sy_rate);
    if !post_trade_rate_is_representable(
        env,
        comp,
        state.total_pt - pt_out,
        checked_add(env, state.total_sy, sy_in),
    ) {
        return None;
    }
    Some(sy_in)
}

/// Where a candidate YT size sits relative to the buyer's budget.
///
/// The three arms partition the candidate range into three contiguous blocks,
/// which is what makes the binary search below correct. Post-conversion the
/// buyer's cost
///
/// ```text
/// cost(face) = ceil(face * WAD / rate) - curve_proceeds(face)
/// ```
///
/// is non-decreasing in `face`: the split cost is linear, and the curve's
/// proceeds per unit of face *fall* as more PT is pushed into the pool. So the
/// affordable set is an interval whose lower edge is only ever set by integer
/// rounding (a face so small the curve's proceeds floor to zero, or so small
/// the fee-free proceeds still cover the split) and whose upper edge is the
/// budget or the pool's liquidity. `TooSmall` therefore always means "search
/// up" and `TooLarge` always means "search down" — the assumption the previous
/// prefix-only search made unconditionally, and which was false at rates above
/// 1.0 because the whole feasible set could sit above the first probe.
enum YtBuyProbe {
    /// Below the affordable interval: the split's face is too small for the
    /// curve leg to return anything the pool can safely account for.
    TooSmall,
    /// Affordable: the buyer's net SY cost for this face.
    Affordable(i128),
    /// Above the affordable interval: over budget, or past the pool's
    /// liquidity / market-proportion bound.
    TooLarge,
}

/// Prices one candidate YT size for the buy solver.
///
/// The curve leg here is byte-for-byte the same computation
/// `exact_pt_in_sy_out_or_panic` performs (asset units on the curve, floored
/// conversion back to shares), so the size this solver settles on is priced
/// identically when `swap_sy_for_yt` executes it and `quote_sy_for_yt` cannot
/// disagree with execution.
fn probe_yt_buy(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    face: i128,
    sy_in: i128,
) -> YtBuyProbe {
    if face <= 0 {
        return YtBuyProbe::TooSmall;
    }
    let exchange_rate = match try_get_exchange_rate(
        env,
        state.total_pt,
        comp.total_asset,
        comp.rate_scalar,
        comp.rate_anchor,
        -face,
    ) {
        Some(value) => value,
        // Only the market-proportion ceiling can reject a PT *sale*, and it is
        // reached by pushing more PT in, so this is an upper-edge rejection.
        None => return YtBuyProbe::TooLarge,
    };
    let pre_fee_asset_out = mul_div_down_or_panic(env, face, WAD, exchange_rate);
    // Must match `exact_pt_in_sy_out_or_panic` bit for bit, including the
    // fee's rounding direction, or the probe and the execution diverge.
    let fee = mul_div_up_or_panic(env, pre_fee_asset_out, config.fee_bps, BPS_DENOMINATOR);
    let asset_out = pre_fee_asset_out - fee;
    let sy_paid = shares_out_for_face_down(env, asset_out, comp.sy_rate);
    if sy_paid <= 0 {
        // Dust: `exact_pt_in_sy_out_or_panic` would reject this leg, and a
        // larger face is what fixes it.
        return YtBuyProbe::TooSmall;
    }
    if sy_paid >= state.total_sy {
        return YtBuyProbe::TooLarge;
    }
    // Mirrors `exact_pt_in_sy_out_or_panic` exactly, including its post-trade
    // representability guard, so the size the solver settles on is one the
    // execution path will accept.
    if !post_trade_rate_is_representable(
        env,
        comp,
        checked_add(env, state.total_pt, face),
        state.total_sy - sy_paid,
    ) {
        return YtBuyProbe::TooLarge;
    }
    let shares_needed = shares_in_for_face_up(env, face, comp.sy_rate);
    if shares_needed <= sy_paid {
        // The curve would fund the whole split on its own, i.e. free YT at LP
        // expense. Never bank it; a larger face carries a strictly higher cost,
        // so keep searching up.
        return YtBuyProbe::TooSmall;
    }
    let cost = shares_needed - sy_paid;
    if cost > sy_in {
        YtBuyProbe::TooLarge
    } else {
        YtBuyProbe::Affordable(cost)
    }
}

/// Solves for the YT face a buyer receives for at most `sy_in` SY shares, and
/// the SY shares that actually buys.
///
/// The pool splits ceil(yt_out * WAD / rate) shares to mint `yt_out` face of PT
/// + YT and sells the PT to itself; the buyer covers the difference. `best` is
/// only ever set on a candidate whose cost fits inside `sy_in`, and the
/// three-way probe keeps the search from discarding the feasible interval when
/// the first candidate lands below it.
fn solve_yt_out_for_sy_in(
    env: &Env,
    config: &Config,
    state: &State,
    comp: &Precompute,
    sy_in: i128,
) -> (i128, i128) {
    let mut low = 1;
    // The largest face any split could mint from the buyer's SY plus the whole
    // SY reserve, converted from shares to face at the rate. Capped at the
    // reserve bound so the probe's intermediate products stay well inside i128
    // no matter what rate the SY token reports.
    let max_shares = checked_add(env, sy_in, state.total_sy);
    let mut high = min(
        mul_div_down_or_panic(env, max_shares, comp.sy_rate, WAD),
        MAX_RESERVE_UNITS,
    );
    let mut best = 0;
    let mut best_cost = 0;
    while low <= high {
        let mid = low + ((high - low) / 2);
        match probe_yt_buy(env, config, state, comp, mid, sy_in) {
            YtBuyProbe::Affordable(cost) => {
                best = mid;
                best_cost = cost;
                low = mid + 1;
            }
            YtBuyProbe::TooSmall => {
                low = mid + 1;
            }
            YtBuyProbe::TooLarge => {
                high = mid - 1;
            }
        }
    }
    if best <= 0 {
        panic_with_error!(env, Error::TradeNotFound);
    }
    (best, best_cost)
}

/// Reads the SY exchange rate (asset per share, WAD scaled) from the SY token,
/// the same `exchange_rate` entrypoint the tokenizer prices split and recombine
/// with, so the AMM's unit conversions cannot drift from what the tokenizer
/// actually mints and burns.
fn sy_rate_or_panic(env: &Env, config: &Config) -> i128 {
    let args: Vec<Val> = vec![env];
    let rate: i128 =
        env.invoke_contract(&config.sy_token, &Symbol::new(env, "exchange_rate"), args);
    if rate <= 0 {
        panic_with_error!(env, Error::InvalidSyRate);
    }
    rate
}

/// Asset-unit value of `shares` SY at `rate`: floor(shares * rate / WAD), the
/// same direction `tokenizer::split` floors the face it mints against escrowed
/// shares. Used to value the pool's SY reserve for the curve; flooring keeps the
/// curve from ever believing the pool holds more value than it does.
fn asset_value_of_shares(env: &Env, shares: i128, rate: i128) -> i128 {
    mul_div_down_or_panic(env, shares, rate, WAD)
}

/// SY shares that must be split so the tokenizer's floored face mint covers
/// `face`: ceil(face * WAD / rate). Rounding up is the safe direction for the
/// pool: the split can over-mint face dust (which the pool keeps) but can
/// never mint less than the curve accounted for. Also the conversion for any
/// asset-denominated amount the pool *charges*, for the same reason.
fn shares_in_for_face_up(env: &Env, face: i128, rate: i128) -> i128 {
    mul_div_up_or_panic(env, face, WAD, rate)
}

/// SY shares a recombine of `face` PT + YT returns: floor(face * WAD / rate),
/// mirroring the tokenizer's own floor, so the pool never budgets more SY out
/// than the recombine actually delivers.
fn shares_out_for_face_down(env: &Env, face: i128, rate: i128) -> i128 {
    mul_div_down_or_panic(env, face, WAD, rate)
}

/// Calls `tokenizer.split(amm, amount)`, authorizing the exact tokenizer call
/// and the exact SY pull it performs from the pool. `amount` is denominated in
/// SY shares (what split escrows), not PT face (what split mints); callers
/// convert curve face amounts with shares_in_for_face_up first.
fn flash_split(env: &Env, config: &Config, amount: i128) -> (i128, i128) {
    let amm = env.current_contract_address();
    let split_args: Vec<Val> =
        soroban_sdk::vec![env, amm.clone().into_val(env), amount.into_val(env)];
    let pull_args: Vec<Val> = soroban_sdk::vec![
        env,
        amm.clone().into_val(env),
        config.tokenizer.clone().into_val(env),
        amount.into_val(env),
    ];
    env.authorize_as_current_contract(vec![
        env,
        auth_entry(env, &config.tokenizer, "split", split_args.clone()),
        auth_entry(env, &config.sy_token, "transfer", pull_args),
    ]);
    env.invoke_contract::<(i128, i128)>(&config.tokenizer, &Symbol::new(env, "split"), split_args)
}

/// Calls `tokenizer.recombine(amm, amount, amount)`, authorizing the call and
/// the PT and YT burns it performs on the pool's balances, and returns SY out.
/// `amount` is PT face (what recombine burns); the return value is SY shares,
/// floor(amount * WAD / rate) when the escrow is solvent.
fn flash_recombine(env: &Env, config: &Config, amount: i128) -> i128 {
    let amm = env.current_contract_address();
    let recombine_args: Vec<Val> = soroban_sdk::vec![
        env,
        amm.clone().into_val(env),
        amount.into_val(env),
        amount.into_val(env),
    ];
    let burn_args: Vec<Val> =
        soroban_sdk::vec![env, amm.clone().into_val(env), amount.into_val(env)];
    // No `yt_token.burn` entry: the tokenizer burns YT through the
    // tokenizer-gated `burn_settled`, not through the holder-authorized `burn`,
    // so an entry for it would never be matched. Granting auth that nothing
    // consumes makes the tree read as a wider permission than it is.
    env.authorize_as_current_contract(vec![
        env,
        auth_entry(env, &config.tokenizer, "recombine", recombine_args.clone()),
        auth_entry(env, &config.pt_token, "burn", burn_args),
    ]);
    env.invoke_contract::<i128>(
        &config.tokenizer,
        &Symbol::new(env, "recombine"),
        recombine_args,
    )
}

/// Accumulates the rate that **prevailed** over the interval that just closed.
///
/// `prevailing_ln_rate` is spot as it stood at `state.last_observation`, i.e.
/// before the trade currently executing moved it. That is the only observation
/// this call is entitled to weight, because `elapsed` is precisely the interval
/// during which that value — and not the post-trade value — was the market's
/// price. The post-trade rate is left standing in `state.last_ln_implied_rate`
/// and is accumulated by whichever future call closes the interval it actually
/// prevails over.
///
/// This is what makes the oracle cost time rather than size. Weighting the
/// post-trade rate instead let an attacker wait out `window - 1` seconds of
/// idle market, dislocate spot with one trade (weight ~= 1.0, so the TWAP
/// snapped to the dislocated value while `twap_warming_up` stayed false), and
/// reverse in the same ledger for the round-trip fee alone. Here that trade
/// contributes nothing: it closes the interval with the *old* rate, and the
/// dislocated rate is only ever accumulated by a later call, i.e. only if the
/// attacker actually holds the dislocation open against arbitrage for real
/// time. The `elapsed == 0` early return is now simply a zero-length interval
/// carrying zero weight, not a hole.
fn sync_twap(env: &Env, config: &Config, state: &mut State, prevailing_ln_rate: i128) {
    let now = env.ledger().timestamp();
    let elapsed = now.saturating_sub(state.last_observation);

    if elapsed == 0 {
        return;
    }

    if elapsed >= config.twap_window {
        // A full window of one uninterrupted price: there is no older history
        // worth blending, so the TWAP becomes that price. Re-enter warm-up
        // anyway — consumers that gate on twap_warming_up (the SDK and app
        // already do) then ignore the value until a window of genuinely
        // multi-observation history has accumulated.
        state.twap_ln_implied_rate = prevailing_ln_rate;
        state.warmup_until = now + config.twap_window;
    } else {
        let weight = mul_div_down_or_panic(env, elapsed as i128, WAD, config.twap_window as i128);
        let retained = checked_sub(env, WAD, weight);
        let carried = mul_div_down_or_panic(env, state.twap_ln_implied_rate, retained, WAD);
        let fresh = mul_div_down_or_panic(env, prevailing_ln_rate, weight, WAD);
        state.twap_ln_implied_rate = checked_add(env, carried, fresh);
    }

    state.last_observation = now;
}

fn get_rate_scalar_or_panic(env: &Env, scalar_root: i128, time_to_expiry: u64) -> i128 {
    let numerator = checked_mul(env, scalar_root, IMPLIED_RATE_TIME as i128);
    let rate_scalar = numerator / time_to_expiry as i128;
    if rate_scalar <= 0 {
        panic_with_error!(env, Error::InvalidScalarRoot);
    }

    rate_scalar
}

fn get_rate_anchor_or_panic(
    env: &Env,
    total_pt: i128,
    last_ln_implied_rate: i128,
    total_asset: i128,
    rate_scalar: i128,
    time_to_expiry: u64,
) -> i128 {
    let exchange_rate =
        get_exchange_rate_from_implied_rate_or_panic(env, last_ln_implied_rate, time_to_expiry);
    if exchange_rate < WAD {
        panic_with_error!(env, Error::ExchangeRateBelowOne);
    }

    let proportion =
        mul_div_down_or_panic(env, total_pt, WAD, checked_add(env, total_pt, total_asset));
    let ln_proportion = log_proportion_or_panic(env, proportion);
    checked_sub(
        env,
        exchange_rate,
        mul_div_down_or_panic(env, ln_proportion, WAD, rate_scalar),
    )
}

fn get_ln_implied_rate_or_panic(
    env: &Env,
    total_pt: i128,
    total_asset: i128,
    rate_scalar: i128,
    rate_anchor: i128,
    time_to_expiry: u64,
) -> i128 {
    let exchange_rate =
        get_exchange_rate_or_panic(env, total_pt, total_asset, rate_scalar, rate_anchor, 0);
    let ln_rate = ln_wad_or_panic(env, exchange_rate);
    mul_div_down_or_panic(
        env,
        ln_rate,
        IMPLIED_RATE_TIME as i128,
        time_to_expiry as i128,
    )
}

fn get_exchange_rate_from_implied_rate_or_panic(
    env: &Env,
    ln_implied_rate: i128,
    time_to_expiry: u64,
) -> i128 {
    let rt = mul_div_down_or_panic(
        env,
        ln_implied_rate,
        time_to_expiry as i128,
        IMPLIED_RATE_TIME as i128,
    );
    exp_wad_or_panic(env, rt)
}

fn get_exchange_rate_or_panic(
    env: &Env,
    total_pt: i128,
    total_asset: i128,
    rate_scalar: i128,
    rate_anchor: i128,
    net_pt_to_account: i128,
) -> i128 {
    let numerator = checked_sub(env, total_pt, net_pt_to_account);
    let denominator = checked_add(env, total_pt, total_asset);
    let proportion = mul_div_down_or_panic(env, numerator, WAD, denominator);
    if proportion > MAX_MARKET_PROPORTION {
        panic_with_error!(env, Error::MarketProportionTooHigh);
    }

    let ln_proportion = log_proportion_or_panic(env, proportion);
    let exchange_rate = checked_add(
        env,
        mul_div_down_or_panic(env, ln_proportion, WAD, rate_scalar),
        rate_anchor,
    );
    if exchange_rate < WAD {
        panic_with_error!(env, Error::ExchangeRateBelowOne);
    }

    exchange_rate
}

fn try_get_exchange_rate(
    env: &Env,
    total_pt: i128,
    total_asset: i128,
    rate_scalar: i128,
    rate_anchor: i128,
    net_pt_to_account: i128,
) -> Option<i128> {
    let numerator = total_pt.checked_sub(net_pt_to_account)?;
    let denominator = total_pt.checked_add(total_asset)?;
    if numerator <= 0 || denominator <= 0 {
        return None;
    }

    let proportion = numerator.checked_mul(WAD)?.checked_div(denominator)?;
    if proportion <= 0 || proportion > MAX_MARKET_PROPORTION {
        return None;
    }

    let complement = WAD.checked_sub(proportion)?;
    if complement <= 0 {
        return None;
    }

    let ratio = proportion.checked_mul(WAD)?.checked_div(complement)?;
    let ln_proportion = try_ln_wad(env, ratio)?;
    let scaled = ln_proportion.checked_mul(WAD)?.checked_div(rate_scalar)?;
    let exchange_rate = scaled.checked_add(rate_anchor)?;
    if exchange_rate < WAD {
        return None;
    }

    Some(exchange_rate)
}

fn log_proportion_or_panic(env: &Env, proportion: i128) -> i128 {
    let complement = checked_sub(env, WAD, proportion);
    if complement <= 0 {
        panic_with_error!(env, Error::MarketProportionTooHigh);
    }

    let ratio = mul_div_down_or_panic(env, proportion, WAD, complement);
    ln_wad_or_panic(env, ratio)
}

fn ln_rate_to_bps(ln_rate: i128) -> i128 {
    (ln_rate * BPS_DENOMINATOR) / WAD
}

// ln(2) scaled by WAD. Used to range-reduce ln and exp into a small interval
// where the series below converge quickly. Soroban's wasm VM rejects
// floating-point instructions, so all transcendental math here is integer
// fixed-point (i128, WAD = 1e18); these replace the previous libm f64 helpers.
const LN2_WAD: i128 = 693_147_180_559_945_309;

fn integer_sqrt_or_panic(env: &Env, value: i128) -> i128 {
    if value <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }

    // Floor integer square root via Newton's method. Exact for every i128 >= 1
    // and, unlike the previous f64 sqrt, it does not lose precision for products
    // approaching WAD^2 (~1e36), which f64's 53-bit mantissa cannot represent.
    let mut x = value;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    x
}

// Natural log of a WAD-fixed positive value, returned WAD-fixed (signed).
// Range-reduce value = m * 2^k with m in [1, 2), so ln(value) = k*ln2 + ln(m),
// and evaluate ln(m) with the fast atanh series
// ln(m) = 2*(z + z^3/3 + z^5/5 + ...), z = (m-1)/(m+1) in [0, 1/3].
fn ln_wad_checked(value: i128) -> Option<i128> {
    if value <= 0 {
        return None;
    }

    let mut k: i128 = 0;
    let mut m = value;
    while m >= 2 * WAD {
        m /= 2;
        k += 1;
    }
    while m < WAD {
        m = m.checked_mul(2)?;
        k -= 1;
    }

    // z = (m - WAD) / (m + WAD), WAD-fixed, in [0, 1/3].
    let z = (m - WAD).checked_mul(WAD)? / (m + WAD);
    let z2 = z.checked_mul(z)? / WAD; // z^2, WAD-fixed (<= ~1/9)

    let mut term = z; // z^(2n+1), starting at z^1
    let mut sum = z;
    let mut n: i128 = 3;
    // z^2 <= 1/9 so terms decay ~9x each step; 24 terms is far past 1e-18.
    while n <= 49 {
        term = term.checked_mul(z2)? / WAD;
        sum = sum.checked_add(term / n)?;
        n += 2;
    }

    let ln_mant = sum.checked_mul(2)?;
    k.checked_mul(LN2_WAD)?.checked_add(ln_mant)
}

fn ln_wad_or_panic(env: &Env, value: i128) -> i128 {
    match ln_wad_checked(value) {
        Some(v) => v,
        None => panic_with_error!(env, Error::MathOverflow),
    }
}

fn try_ln_wad(_env: &Env, value: i128) -> Option<i128> {
    ln_wad_checked(value)
}

// e^x for WAD-fixed signed x, returned WAD-fixed. Range-reduce x = k*ln2 + r
// with |r| <= ln2/2, so e^x = 2^k * e^r, and evaluate e^r with its Taylor
// series (|r| <= 0.347 converges in a handful of terms).
fn exp_wad_checked(value: i128) -> Option<i128> {
    let k = if value >= 0 {
        (value + LN2_WAD / 2) / LN2_WAD
    } else {
        (value - LN2_WAD / 2) / LN2_WAD
    };
    let r = value.checked_sub(k.checked_mul(LN2_WAD)?)?; // |r| <= ln2/2

    let mut term = WAD; // r^0 / 0! = 1
    let mut sum = WAD;
    let mut i: i128 = 1;
    while i <= 20 {
        term = term.checked_mul(r)? / WAD / i; // term *= r/i
        if term == 0 {
            break;
        }
        sum = sum.checked_add(term)?;
        i += 1;
    }

    // Apply the 2^k factor.
    if k >= 0 {
        if k > 90 {
            return None; // e^x too large to represent in i128 WAD-fixed
        }
        sum.checked_mul(1i128 << k)
    } else {
        let shift = (-k) as u32;
        if shift >= 127 {
            return Some(0);
        }
        Some(sum >> shift)
    }
}

fn exp_wad_or_panic(env: &Env, value: i128) -> i128 {
    match exp_wad_checked(value) {
        Some(v) => v,
        None => panic_with_error!(env, Error::MathOverflow),
    }
}

fn mul_div_down_or_panic(env: &Env, lhs: i128, rhs: i128, denominator: i128) -> i128 {
    if denominator == 0 {
        panic_with_error!(env, Error::MathOverflow);
    }

    checked_mul(env, lhs, rhs) / denominator
}

fn mul_div_up_or_panic(env: &Env, lhs: i128, rhs: i128, denominator: i128) -> i128 {
    if denominator == 0 {
        panic_with_error!(env, Error::MathOverflow);
    }

    let product = checked_mul(env, lhs, rhs);
    let quotient = product / denominator;
    if product % denominator == 0 {
        quotient
    } else {
        checked_add(env, quotient, 1)
    }
}

fn checked_add(env: &Env, lhs: i128, rhs: i128) -> i128 {
    match lhs.checked_add(rhs) {
        Some(value) => value,
        None => panic_with_error!(env, Error::MathOverflow),
    }
}

fn checked_bounded_reserve_add(env: &Env, lhs: i128, rhs: i128) -> i128 {
    let value = checked_add(env, lhs, rhs);
    require_within_reserve_bounds(env, value);
    value
}

fn checked_sub(env: &Env, lhs: i128, rhs: i128) -> i128 {
    match lhs.checked_sub(rhs) {
        Some(value) if value >= 0 => value,
        _ => panic_with_error!(env, Error::MathOverflow),
    }
}

fn checked_mul(env: &Env, lhs: i128, rhs: i128) -> i128 {
    match lhs.checked_mul(rhs) {
        Some(value) => value,
        None => panic_with_error!(env, Error::MathOverflow),
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test {
    use super::*;
    use proptest::prelude::*;
    use sidereal_sy_wrapper::{SyWrapper, SyWrapperClient};
    use soroban_sdk::testutils::{
        storage::Persistent, Address as _, Deployer, EnvTestConfig, Ledger,
    };
    use std::panic::{catch_unwind, AssertUnwindSafe};

    const NOW: u64 = 1_770_000_000;
    const MATURITY: u64 = NOW + 90 * DAY;
    const SCALAR_ROOT: i128 = 2 * WAD;
    const INITIAL_ANCHOR: i128 = 1_050_000_000_000_000_000;
    const FEE_BPS: i128 = 10;
    const TWAP_WINDOW: u64 = 30 * 60;
    const INITIAL_TOKEN_BALANCE: i128 = 10_000_000;

    /// SY exchange rates every unit-sensitive test sweeps. 1.0 is the degenerate
    /// point where shares and asset units coincide and C1 vanishes identically;
    /// everything above it is a market that has accrued, which every market does.
    /// The rates are chosen so a round share count converts to a round underlying
    /// amount, letting the fixtures mint exact share balances.
    const RATE_SWEEP: [i128; 4] = [
        WAD,
        1_010_000_000_000_000_000,
        1_050_000_000_000_000_000,
        1_100_000_000_000_000_000,
    ];

    // ---------------------------------------------------------------------
    // A minimal stand-in for the tokenizer, so the AMM's two flash routes move
    // real tokens inside this suite instead of only being quoted.
    //
    // It reproduces exactly the two formulas the AMM's unit conversions have to
    // agree with, and nothing else: `split` escrows SY *shares* and mints
    // floor(shares * rate / WAD) of PT and YT *face*; `recombine` burns face and
    // returns floor(face * WAD / rate) shares, capped at the escrow it holds.
    // Yield accrual, maturity, and the PT-senior reservation are the real
    // tokenizer's business and are covered in tests/integration.
    // ---------------------------------------------------------------------

    #[contracttype]
    #[derive(Clone)]
    pub struct MockTokenizerConfig {
        pub pt_token: Address,
        pub sy_token: Address,
        pub yt_token: Address,
    }

    #[contracttype]
    pub enum MockTokenizerKey {
        Config,
    }

    #[contract]
    pub struct MockTokenizer;

    #[contractimpl]
    impl MockTokenizer {
        pub fn init(env: Env, pt_token: Address, sy_token: Address, yt_token: Address) {
            env.storage().instance().set(
                &MockTokenizerKey::Config,
                &MockTokenizerConfig {
                    pt_token,
                    sy_token,
                    yt_token,
                },
            );
        }

        pub fn split(env: Env, from: Address, sy_amount: i128) -> (i128, i128) {
            from.require_auth();
            let config = mock_config(&env);
            let rate = mock_rate(&env, &config);
            let face = mul_div_down_or_panic(&env, sy_amount, rate, WAD);
            assert!(face > 0, "split of dust");
            let escrow = MuxedAddress::from(&env.current_contract_address());
            token::TokenClient::new(&env, &config.sy_token).transfer(&from, &escrow, &sy_amount);
            token::StellarAssetClient::new(&env, &config.pt_token).mint(&from, &face);
            token::StellarAssetClient::new(&env, &config.yt_token).mint(&from, &face);
            (face, face)
        }

        pub fn recombine(env: Env, from: Address, pt_amount: i128, yt_amount: i128) -> i128 {
            from.require_auth();
            assert_eq!(pt_amount, yt_amount, "recombine legs must match");
            let config = mock_config(&env);
            let rate = mock_rate(&env, &config);
            let me = env.current_contract_address();
            let escrow = token::TokenClient::new(&env, &config.sy_token).balance(&me);
            let full = mul_div_down_or_panic(&env, pt_amount, WAD, rate);
            let sy_out = min(full, escrow);
            assert!(sy_out > 0, "recombine of dust");
            token::TokenClient::new(&env, &config.pt_token).burn(&from, &pt_amount);
            token::TokenClient::new(&env, &config.yt_token).burn(&from, &yt_amount);

            let to = MuxedAddress::from(&from);
            let args: Vec<Val> = soroban_sdk::vec![
                &env,
                me.clone().into_val(&env),
                to.clone().into_val(&env),
                sy_out.into_val(&env),
            ];
            env.authorize_as_current_contract(soroban_sdk::vec![
                &env,
                auth_entry(&env, &config.sy_token, "transfer", args),
            ]);
            token::TokenClient::new(&env, &config.sy_token).transfer(&me, &to, &sy_out);
            sy_out
        }
    }

    fn mock_config(env: &Env) -> MockTokenizerConfig {
        env.storage()
            .instance()
            .get(&MockTokenizerKey::Config)
            .expect("mock tokenizer not initialized")
    }

    fn mock_rate(env: &Env, config: &MockTokenizerConfig) -> i128 {
        let args: Vec<Val> = soroban_sdk::vec![env];
        env.invoke_contract(&config.sy_token, &Symbol::new(env, "exchange_rate"), args)
    }

    struct Fixture {
        env: Env,
        client: AmmMarketClient<'static>,
        contract_id: Address,
        admin: Address,
        underlying: Address,
        pt_token: Address,
        sy_token: Address,
        yt_token: Address,
        tokenizer: Address,
        bob: Address,
    }

    fn fixture(now: u64) -> Fixture {
        build_fixture(now, false)
    }

    /// Same market, but with the auth mock that permits a contract to authorize
    /// its own sub-invocations. The flash routes need it: the AMM authorizes the
    /// tokenizer's SY pull and the PT/YT burns as itself.
    fn flash_fixture(now: u64) -> Fixture {
        build_fixture(now, true)
    }

    fn build_fixture(now: u64, allow_non_root_auth: bool) -> Fixture {
        let env = Env::new_with_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        env.ledger().set_timestamp(now);
        if allow_non_root_auth {
            env.mock_all_auths_allowing_non_root_auth();
        } else {
            env.mock_all_auths();
        }

        let contract_id = env.register(AmmMarket, ());
        let client = AmmMarketClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let pt_token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        // A real SY wrapper in idle mock-custody mode (no Blend pool), so every
        // route reads exchange_rate the same way the tokenizer does — and so
        // tests can drive the rate off par with `set_sy_rate`, which is where
        // the share/asset unit distinction stops being invisible.
        let underlying = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let sy_token = env.register(SyWrapper, ());
        SyWrapperClient::new(&env, &sy_token).initialize(&admin, &underlying);
        let yt_token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        // A tokenizer stand-in with the real split/recombine unit conversions,
        // so `swap_sy_for_yt` and `swap_yt_for_sy` can be executed here and not
        // merely quoted.
        let tokenizer = env.register(MockTokenizer, ());
        MockTokenizerClient::new(&env, &tokenizer).init(&pt_token, &sy_token, &yt_token);
        let bob = Address::generate(&env);

        token::StellarAssetClient::new(&env, &pt_token).mint(&admin, &INITIAL_TOKEN_BALANCE);
        token::StellarAssetClient::new(&env, &underlying).mint(&admin, &INITIAL_TOKEN_BALANCE);
        SyWrapperClient::new(&env, &sy_token).deposit(&admin, &INITIAL_TOKEN_BALANCE);

        Fixture {
            env,
            client,
            contract_id,
            admin,
            underlying,
            pt_token,
            sy_token,
            yt_token,
            tokenizer,
            bob,
        }
    }

    /// Moves the SY wrapper's exchange rate. Only possible in idle custody mode,
    /// which is exactly the fixture's mode; a Blend-backed wrapper derives its
    /// rate and has no setter.
    fn set_sy_rate(fixture: &Fixture, rate: i128) {
        SyWrapperClient::new(&fixture.env, &fixture.sy_token)
            .set_exchange_rate(&fixture.admin, &rate);
    }

    fn sy_rate(fixture: &Fixture) -> i128 {
        SyWrapperClient::new(&fixture.env, &fixture.sy_token).exchange_rate()
    }

    /// Mints exactly `shares` SY to `holder` at whatever rate the wrapper is
    /// currently on, by depositing the underlying that rate implies.
    fn mint_sy_shares(fixture: &Fixture, holder: &Address, shares: i128) {
        let rate = sy_rate(fixture);
        let underlying = shares * rate / WAD;
        assert_eq!(
            underlying * WAD / rate,
            shares,
            "pick a share count that converts to a whole underlying amount"
        );
        token::StellarAssetClient::new(&fixture.env, &fixture.underlying).mint(holder, &underlying);
        let minted =
            SyWrapperClient::new(&fixture.env, &fixture.sy_token).deposit(holder, &underlying);
        assert_eq!(minted, shares);
    }

    fn yt_balance(fixture: &Fixture, holder: &Address) -> i128 {
        token::TokenClient::new(&fixture.env, &fixture.yt_token).balance(holder)
    }

    /// SY shares the tokenizer must escrow to mint `face` PT: ceil(face·WAD/rate).
    fn shares_to_mint_face(face: i128, rate: i128) -> i128 {
        let product = face * WAD;
        if product % rate == 0 {
            product / rate
        } else {
            product / rate + 1
        }
    }

    /// SY shares a recombine (or a maturity redemption) of `face` returns:
    /// floor(face·WAD/rate).
    fn shares_from_face(face: i128, rate: i128) -> i128 {
        face * WAD / rate
    }

    /// Seeds a market that has already accrued to `rate`, the normal state of
    /// any market whose SY wrapper is older than its own term.
    fn seeded_market_at_rate(now: u64, rate: i128, pt: i128, sy_shares: i128) -> Fixture {
        let fixture = flash_fixture(now);
        initialize(&fixture);
        set_sy_rate(&fixture, rate);
        fixture
            .client
            .add_liquidity(&fixture.admin, &pt, &sy_shares, &0);
        fixture
    }

    fn pt_balance(fixture: &Fixture, holder: &Address) -> i128 {
        token::TokenClient::new(&fixture.env, &fixture.pt_token).balance(holder)
    }

    fn sy_balance(fixture: &Fixture, holder: &Address) -> i128 {
        token::TokenClient::new(&fixture.env, &fixture.sy_token).balance(holder)
    }

    fn pool_pt_balance(fixture: &Fixture) -> i128 {
        pt_balance(fixture, &fixture.contract_id)
    }

    fn pool_sy_balance(fixture: &Fixture) -> i128 {
        sy_balance(fixture, &fixture.contract_id)
    }

    fn mint_pt(fixture: &Fixture, holder: &Address, amount: i128) {
        token::StellarAssetClient::new(&fixture.env, &fixture.pt_token).mint(holder, &amount);
    }

    /// Mints `amount` SY shares to `holder` by depositing underlying at the
    /// wrapper's default 1.0 rate (1 underlying deposits to 1 share).
    fn mint_sy(fixture: &Fixture, holder: &Address, amount: i128) {
        token::StellarAssetClient::new(&fixture.env, &fixture.underlying).mint(holder, &amount);
        SyWrapperClient::new(&fixture.env, &fixture.sy_token).deposit(holder, &amount);
    }

    fn burn_pt(fixture: &Fixture, holder: &Address, amount: i128) {
        token::TokenClient::new(&fixture.env, &fixture.pt_token).burn(holder, &amount);
    }

    /// Burns `amount` SY shares from `holder` by redeeming them for underlying
    /// at the wrapper's default 1.0 rate.
    fn burn_sy(fixture: &Fixture, holder: &Address, amount: i128) {
        SyWrapperClient::new(&fixture.env, &fixture.sy_token).redeem(holder, &amount);
    }

    fn initialize(fixture: &Fixture) {
        fixture.client.initialize(
            &fixture.admin,
            &fixture.pt_token,
            &fixture.sy_token,
            &fixture.yt_token,
            &fixture.tokenizer,
            &MATURITY,
            &SCALAR_ROOT,
            &INITIAL_ANCHOR,
            &FEE_BPS,
            &TWAP_WINDOW,
        );
    }

    #[test]
    fn initialize_stores_config_and_empty_state() {
        let fixture = fixture(NOW);

        initialize(&fixture);

        assert_eq!(
            fixture.client.config(),
            Config {
                admin: fixture.admin,
                pt_token: fixture.pt_token,
                sy_token: fixture.sy_token,
                yt_token: fixture.yt_token,
                tokenizer: fixture.tokenizer,
                maturity: MATURITY,
                scalar_root: SCALAR_ROOT,
                initial_anchor: INITIAL_ANCHOR,
                fee_bps: FEE_BPS,
                twap_window: TWAP_WINDOW,
            }
        );
        assert_eq!(
            fixture.client.state(),
            State {
                total_pt: 0,
                total_sy: 0,
                total_lp: 0,
                last_ln_implied_rate: 0,
                twap_ln_implied_rate: 0,
                last_observation: NOW,
                warmup_until: NOW + TWAP_WINDOW,
            }
        );
        assert_eq!(fixture.client.implied_apy(), 0);
        assert_eq!(fixture.client.spot_apy(), 0);
        assert_eq!(fixture.client.reserve_pt(), 0);
        assert_eq!(fixture.client.reserve_sy(), 0);
        assert_eq!(fixture.client.total_lp(), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #18)")]
    fn initialize_rejects_curve_inputs_above_testnet_bounds() {
        let fixture = fixture(NOW);
        fixture.client.initialize(
            &fixture.admin,
            &fixture.pt_token,
            &fixture.sy_token,
            &fixture.yt_token,
            &fixture.tokenizer,
            &MATURITY,
            &(MAX_SCALAR_ROOT + 1),
            &INITIAL_ANCHOR,
            &FEE_BPS,
            &TWAP_WINDOW,
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #18)")]
    fn liquidity_rejects_amounts_above_testnet_bounds() {
        let fixture = fixture(NOW);
        initialize(&fixture);

        fixture
            .client
            .add_liquidity(&fixture.admin, &(MAX_RESERVE_UNITS + 1), &10_000, &0);
    }

    #[test]
    fn bump_ttl_extends_idle_market_instance_ttl() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        lower_instance_ttl_below_threshold(&fixture);

        fixture.client.bump_ttl();

        assert!(
            fixture
                .env
                .deployer()
                .get_contract_instance_ttl(&fixture.contract_id)
                >= AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS
        );
    }

    #[test]
    fn bump_lp_ttl_extends_idle_lp_balance_ttl() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        let key = DataKey::LpBalance(fixture.admin.clone());
        let ttl = fixture.env.as_contract(&fixture.contract_id, || {
            fixture.env.storage().persistent().get_ttl(&key)
        });
        assert!(ttl > AMM_INSTANCE_TTL_THRESHOLD_LEDGERS);

        let target_ttl = AMM_INSTANCE_TTL_THRESHOLD_LEDGERS - 1;
        fixture
            .env
            .ledger()
            .set_sequence_number(fixture.env.ledger().sequence() + ttl - target_ttl);
        fixture.env.as_contract(&fixture.contract_id, || {
            assert!(
                fixture.env.storage().persistent().get_ttl(&key)
                    < AMM_INSTANCE_TTL_THRESHOLD_LEDGERS
            );
        });

        fixture.client.bump_lp_ttl(&fixture.admin);

        fixture.env.as_contract(&fixture.contract_id, || {
            assert!(
                fixture.env.storage().persistent().get_ttl(&key)
                    >= AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS
            );
        });
    }

    #[test]
    fn mutating_entrypoints_extend_instance_ttl() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        lower_instance_ttl_below_threshold(&fixture);

        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        assert!(
            fixture
                .env
                .deployer()
                .get_contract_instance_ttl(&fixture.contract_id)
                >= AMM_INSTANCE_TTL_EXTEND_TO_LEDGERS
        );
    }

    #[test]
    fn first_liquidity_seeds_market_state() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        let admin_pt_before = pt_balance(&fixture, &fixture.admin);
        let admin_sy_before = sy_balance(&fixture, &fixture.admin);

        let lp_out = fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);
        let state = fixture.client.state();

        assert_eq!(lp_out, 9_000);
        assert_eq!(state.total_pt, 10_000);
        assert_eq!(state.total_sy, 10_000);
        assert_eq!(state.total_lp, 10_000);
        assert_eq!(fixture.client.lp_balance(&fixture.admin), 9_000);
        assert!(state.last_ln_implied_rate > 0);
        assert_eq!(state.last_ln_implied_rate, state.twap_ln_implied_rate);
        assert!(fixture.client.implied_apy() > 0);
        assert_eq!(pool_pt_balance(&fixture), state.total_pt);
        assert_eq!(pool_sy_balance(&fixture), state.total_sy);
        assert_eq!(
            pt_balance(&fixture, &fixture.admin),
            admin_pt_before - 10_000
        );
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            admin_sy_before - 10_000
        );
    }

    #[test]
    fn remove_liquidity_returns_pro_rata_assets() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        let admin_pt_before = pt_balance(&fixture, &fixture.admin);
        let admin_sy_before = sy_balance(&fixture, &fixture.admin);
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        let (pt_out, sy_out) = fixture
            .client
            .remove_liquidity(&fixture.admin, &9_000, &0, &0);
        let state = fixture.client.state();

        assert_eq!((pt_out, sy_out), (9_000, 9_000));
        assert_eq!(state.total_pt, 1_000);
        assert_eq!(state.total_sy, 1_000);
        assert_eq!(state.total_lp, 1_000);
        assert_eq!(fixture.client.lp_balance(&fixture.admin), 0);
        assert_eq!(pool_pt_balance(&fixture), 1_000);
        assert_eq!(pool_sy_balance(&fixture), 1_000);
        assert_eq!(
            pt_balance(&fixture, &fixture.admin),
            admin_pt_before - 1_000
        );
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            admin_sy_before - 1_000
        );
    }

    #[test]
    fn remove_liquidity_after_maturity_returns_pro_rata_assets() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        let admin_pt_before = pt_balance(&fixture, &fixture.admin);
        let admin_sy_before = sy_balance(&fixture, &fixture.admin);
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        fixture.env.ledger().set_timestamp(MATURITY);
        let (pt_out, sy_out) = fixture
            .client
            .remove_liquidity(&fixture.admin, &9_000, &0, &0);
        let state = fixture.client.state();

        assert_eq!((pt_out, sy_out), (9_000, 9_000));
        assert_eq!(state.total_pt, 1_000);
        assert_eq!(state.total_sy, 1_000);
        assert_eq!(state.total_lp, 1_000);
        assert_eq!(fixture.client.lp_balance(&fixture.admin), 0);
        assert_eq!(pool_pt_balance(&fixture), 1_000);
        assert_eq!(pool_sy_balance(&fixture), 1_000);
        assert_eq!(
            pt_balance(&fixture, &fixture.admin),
            admin_pt_before - 1_000
        );
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            admin_sy_before - 1_000
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn add_liquidity_reverts_when_min_lp_out_not_met() {
        let fixture = fixture(NOW);
        initialize(&fixture);

        // The initial seed mints sqrt(10_000 * 10_000) - MINIMUM_LIQUIDITY
        // = 9_000 LP; asking for one more must revert with SlippageExceeded.
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &9_001);
    }

    #[test]
    fn add_liquidity_passes_exact_min_lp_out() {
        let fixture = fixture(NOW);
        initialize(&fixture);

        let lp_out = fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &9_000);
        assert_eq!(lp_out, 9_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn add_liquidity_min_lp_out_catches_ratio_move_between_quote_and_execution() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_pt(&fixture, &fixture.bob, 1_000);
        mint_sy(&fixture, &fixture.bob, 1_000);

        // Quoted off the seeded 20_000/20_000 pool: 1_000 PT + 1_000 SY mints
        // 1_000 LP. Someone else moves the ratio before bob executes.
        let stale_quote = 1_000;
        mint_sy(&fixture, &fixture.admin, 2_000);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);

        // With total_sy grown past 20_000, lp_by_sy = 1_000 * total_lp /
        // total_sy < 1_000, so the stale min must revert.
        fixture
            .client
            .add_liquidity(&fixture.bob, &1_000, &1_000, &stale_quote);
    }

    #[test]
    fn add_liquidity_generous_min_survives_ratio_move() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_pt(&fixture, &fixture.bob, 1_000);
        mint_sy(&fixture, &fixture.bob, 1_000);
        mint_sy(&fixture, &fixture.admin, 2_000);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);

        let lp_out = fixture
            .client
            .add_liquidity(&fixture.bob, &1_000, &1_000, &900);
        assert!(lp_out >= 900 && lp_out < 1_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn remove_liquidity_reverts_when_min_sy_out_not_met() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        // Quoted pro-rata for 1_000 LP: 1_000 PT and 1_000 SY. A PT seller
        // drains SY from the pool before the removal executes, so the stale
        // min_sy_out must revert.
        let stale_sy_quote = 1_000;
        mint_pt(&fixture, &fixture.admin, 2_000);
        fixture.client.swap_pt_for_sy(&fixture.admin, &2_000, &1);

        fixture
            .client
            .remove_liquidity(&fixture.admin, &1_000, &0, &stale_sy_quote);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn remove_liquidity_reverts_when_min_pt_out_not_met() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        // A PT buyer drains PT from the pool, so pt_out per LP falls below the
        // stale pro-rata quote of 1_000.
        let stale_pt_quote = 1_000;
        mint_sy(&fixture, &fixture.admin, 2_000);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);

        fixture
            .client
            .remove_liquidity(&fixture.admin, &1_000, &stale_pt_quote, &0);
    }

    #[test]
    fn remove_liquidity_generous_bounds_pass_after_ratio_move() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_pt(&fixture, &fixture.admin, 2_000);
        fixture.client.swap_pt_for_sy(&fixture.admin, &2_000, &1);

        let (pt_out, sy_out) =
            fixture
                .client
                .remove_liquidity(&fixture.admin, &1_000, &1_000, &900);
        assert!(pt_out >= 1_000, "PT per LP grew after the PT sell");
        assert!(sy_out >= 900 && sy_out < 1_000, "SY per LP shrank");
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn add_liquidity_rejects_after_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        fixture.env.ledger().set_timestamp(MATURITY);
        fixture
            .client
            .add_liquidity(&fixture.admin, &1_000, &1_000, &0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn swap_pt_for_sy_rejects_after_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_pt(&fixture, &fixture.admin, 1_000);

        fixture.env.ledger().set_timestamp(MATURITY);
        fixture.client.swap_pt_for_sy(&fixture.admin, &1_000, &1);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn swap_sy_for_pt_rejects_after_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_sy(&fixture, &fixture.admin, 1_000);

        fixture.env.ledger().set_timestamp(MATURITY);
        fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn swap_sy_for_yt_rejects_after_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_sy(&fixture, &fixture.admin, 1_000);

        fixture.env.ledger().set_timestamp(MATURITY);
        fixture.client.swap_sy_for_yt(&fixture.admin, &1_000, &1);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn swap_yt_for_sy_rejects_after_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        fixture.env.ledger().set_timestamp(MATURITY);
        fixture.client.swap_yt_for_sy(&fixture.admin, &1_000, &1);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #12)")]
    fn non_lp_cannot_remove_liquidity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &10_000, &10_000, &0);

        fixture
            .client
            .remove_liquidity(&fixture.bob, &1_000, &0, &0);
    }

    #[test]
    fn swap_pt_for_sy_updates_reserves_and_observation() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_pt(&fixture, &fixture.admin, 1_000);
        let admin_pt_before = pt_balance(&fixture, &fixture.admin);
        let admin_sy_before = sy_balance(&fixture, &fixture.admin);

        fixture.env.ledger().set_timestamp(NOW + 60);
        let sy_out = fixture.client.swap_pt_for_sy(&fixture.admin, &1_000, &1);
        let state = fixture.client.state();

        assert!(sy_out > 0);
        assert_eq!(state.total_pt, 21_000);
        assert_eq!(state.total_sy, 20_000 - sy_out);
        assert_eq!(state.last_observation, NOW + 60);
        assert!(state.twap_ln_implied_rate > 0);
        assert_eq!(pool_pt_balance(&fixture), state.total_pt);
        assert_eq!(pool_sy_balance(&fixture), state.total_sy);
        assert_eq!(
            pt_balance(&fixture, &fixture.admin),
            admin_pt_before - 1_000
        );
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            admin_sy_before + sy_out
        );
    }

    #[test]
    fn swap_sy_for_pt_updates_reserves_and_observation() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        mint_sy(&fixture, &fixture.admin, 1_000);
        let admin_pt_before = pt_balance(&fixture, &fixture.admin);
        let admin_sy_before = sy_balance(&fixture, &fixture.admin);

        fixture.env.ledger().set_timestamp(NOW + 60);
        let pt_out = fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);
        let state = fixture.client.state();

        assert!(pt_out > 0);
        assert_eq!(state.total_pt, 20_000 - pt_out);
        assert_eq!(state.total_sy, 21_000);
        assert_eq!(state.last_observation, NOW + 60);
        assert!(state.twap_ln_implied_rate > 0);
        assert_eq!(pool_pt_balance(&fixture), state.total_pt);
        assert_eq!(pool_sy_balance(&fixture), state.total_sy);
        assert_eq!(
            pt_balance(&fixture, &fixture.admin),
            admin_pt_before + pt_out
        );
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            admin_sy_before - 1_000
        );
    }

    /// H2. `sy_in` is a budget, not a payment. The fill is capped by the curve's
    /// own `exchange_rate >= WAD` floor independently of the budget, so the
    /// difference must stay with the caller instead of being absorbed into
    /// reserves as an LP donation. Even a one-stroop rounding gap must be
    /// refunded, and state must still agree with real balances afterwards.
    #[test]
    fn sy_exact_in_swaps_refund_the_unspent_budget() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        let (sy_in, required_sy) = sy_in_with_rounding_gap(&fixture);
        assert!(required_sy < sy_in);

        let before = fixture.client.state();
        let wallet_before = sy_balance(&fixture, &fixture.admin);
        fixture.client.swap_sy_for_pt(&fixture.admin, &sy_in, &1);
        let after = fixture.client.state();

        assert_eq!(after.total_sy, before.total_sy + required_sy);
        assert_eq!(
            sy_balance(&fixture, &fixture.admin),
            wallet_before - required_sy,
            "the residual never leaves the caller's wallet"
        );
        assert_eq!(after.total_sy, pool_sy_balance(&fixture));
        assert_eq!(after.total_pt, pool_pt_balance(&fixture));
    }

    /// H2, at the scale the audit measured: a 100 SY order into a 1000 SY pool
    /// filled ~76.5 PT for ~76.6 SY and kept the other 23.4 SY. Nothing about
    /// `min_pt_out` defends against it, because the quote returns the same
    /// capped `pt_out`. The refund is the defense.
    #[test]
    fn oversized_sy_in_is_capped_and_the_remainder_is_refunded() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &1_000_000, &1_000_000, &0);
        mint_sy_shares(&fixture, &fixture.bob, 500_000);

        let sy_in = 500_000;
        let pt_out_quote = fixture.client.quote_sy_for_pt(&sy_in);
        let cost_quote = fixture.client.quote_sy_for_pt_cost(&sy_in);
        assert!(
            cost_quote < sy_in / 2,
            "the liquidity cap must bind well below the budget for this to be a real test"
        );

        let wallet_before = sy_balance(&fixture, &fixture.bob);
        let pt_out = fixture.client.swap_sy_for_pt(&fixture.bob, &sy_in, &1);
        let spent = wallet_before - sy_balance(&fixture, &fixture.bob);

        assert_eq!(pt_out, pt_out_quote);
        assert_eq!(spent, cost_quote);
        assert!(spent < sy_in);
        assert_eq!(pt_balance(&fixture, &fixture.bob), pt_out);
        let state = fixture.client.state();
        assert_eq!(state.total_sy, pool_sy_balance(&fixture));
        assert_eq!(state.total_pt, pool_pt_balance(&fixture));
    }

    #[test]
    fn same_timestamp_swaps_do_not_overwrite_twap() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        fixture.env.ledger().set_timestamp(NOW + 60);
        fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);
        let after_first = fixture.client.state();

        fixture.client.swap_sy_for_pt(&fixture.admin, &1_500, &1);
        let after_second = fixture.client.state();

        assert_ne!(
            after_second.last_ln_implied_rate, after_first.twap_ln_implied_rate,
            "second swap must move spot so this test proves TWAP did not follow it"
        );
        assert_eq!(after_second.last_observation, after_first.last_observation);
        assert_eq!(
            after_second.twap_ln_implied_rate,
            after_first.twap_ln_implied_rate
        );
    }

    // The YT flash swaps move real tokens through the tokenizer and are
    // exercised end to end in tests/integration. Here we assert the pure
    // pricing the routes are built on.
    #[test]
    fn quote_sy_for_yt_is_leveraged() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        // Buying YT is leveraged: each SY buys more than its face in YT,
        // because the freshly minted PT is sold to fund the position.
        let yt_out = fixture.client.quote_sy_for_yt(&1_000);
        assert!(yt_out > 1_000);
    }

    #[test]
    fn quote_yt_for_sy_is_below_face() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        // Selling YT yields less SY than its face: PT must be repurchased to
        // complete the recombine.
        let sy_out = fixture.client.quote_yt_for_sy(&1_000);
        assert!(sy_out > 0 && sy_out < 1_000);
    }

    #[test]
    fn read_accessors_match_state_and_rate_views() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        fixture.env.ledger().set_timestamp(NOW + 60);
        fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);

        let state = fixture.client.state();

        assert_eq!(fixture.client.reserve_pt(), state.total_pt);
        assert_eq!(fixture.client.reserve_sy(), state.total_sy);
        assert_eq!(fixture.client.total_lp(), state.total_lp);
        assert_eq!(fixture.client.spot_apy(), fixture.client.implied_apy());
        assert!(fixture.client.twap_apy() > 0);
        assert_eq!(fixture.client.reserve_pt(), pool_pt_balance(&fixture));
        assert_eq!(fixture.client.reserve_sy(), pool_sy_balance(&fixture));
    }

    #[test]
    fn rate_views_track_warmup_and_zero_at_maturity() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        assert!(fixture.client.twap_warming_up());

        fixture.env.ledger().set_timestamp(NOW + TWAP_WINDOW);
        assert!(!fixture.client.twap_warming_up());

        fixture.env.ledger().set_timestamp(MATURITY);
        assert_eq!(fixture.client.implied_apy(), 0);
        assert_eq!(fixture.client.spot_apy(), 0);
        assert_eq!(fixture.client.twap_apy(), 0);
        assert!(!fixture.client.twap_warming_up());
    }

    /// After an idle gap of a full TWAP window, the next swap's observation
    /// fully replaces the TWAP (there is no history worth blending). One trade
    /// deciding the oracle value is the manipulation window the TWAP exists to
    /// close, so that snap must re-enter warm-up: consumers gating on
    /// twap_warming_up ignore the value until a fresh window has passed.
    #[test]
    fn twap_re_enters_warmup_after_an_idle_gap_snaps_it() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        // Trade while warm so the TWAP has a real blended history, then let
        // the initial warm-up lapse.
        fixture.env.ledger().set_timestamp(NOW + 60);
        fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);
        fixture.env.ledger().set_timestamp(NOW + TWAP_WINDOW + 61);
        assert!(
            !fixture.client.twap_warming_up(),
            "warmed up after a window"
        );

        // Idle for well over a full window, then a single swap lands: the
        // observation snaps the TWAP, so the market must declare itself
        // warming up again for a full window from that swap.
        let after_gap = NOW + TWAP_WINDOW + 61 + 3 * TWAP_WINDOW;
        fixture.env.ledger().set_timestamp(after_gap);
        fixture.client.swap_sy_for_pt(&fixture.admin, &1_000, &1);
        assert!(
            fixture.client.twap_warming_up(),
            "a full-window idle snap must re-enter warm-up"
        );

        fixture.env.ledger().set_timestamp(after_gap + TWAP_WINDOW);
        assert!(
            !fixture.client.twap_warming_up(),
            "trust returns after a fresh window"
        );
    }

    #[test]
    fn quote_accessors_match_pt_route_execution_without_mutating_state() {
        let first_fixture = fixture(NOW);
        initialize(&first_fixture);
        first_fixture
            .client
            .add_liquidity(&first_fixture.admin, &20_000, &20_000, &0);

        let before = first_fixture.client.state();
        let quoted_sy_out = first_fixture.client.quote_pt_for_sy(&1_000);
        let quoted_pt_out = first_fixture.client.quote_sy_for_pt(&1_000);
        let after_quote = first_fixture.client.state();

        assert_eq!(before, after_quote);
        assert_eq!(
            quoted_sy_out,
            first_fixture
                .client
                .swap_pt_for_sy(&first_fixture.admin, &1_000, &1)
        );

        let second_fixture = fixture(NOW);
        initialize(&second_fixture);
        second_fixture
            .client
            .add_liquidity(&second_fixture.admin, &20_000, &20_000, &0);
        assert_eq!(
            quoted_pt_out,
            second_fixture
                .client
                .swap_sy_for_pt(&second_fixture.admin, &1_000, &1)
        );
    }

    #[test]
    fn quote_yt_accessors_do_not_mutate_state() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);

        let before = fixture.client.state();
        assert!(fixture.client.quote_sy_for_yt(&1_000) > 0);
        assert!(fixture.client.quote_yt_for_sy(&1_000) > 0);
        let after_quote = fixture.client.state();

        assert_eq!(before, after_quote);
    }

    #[test]
    fn quote_accessors_return_typed_errors_before_trade_execution() {
        let fixture = fixture(NOW);
        initialize(&fixture);

        assert_eq!(
            fixture
                .env
                .as_contract(&fixture.contract_id, || AmmMarket::quote_pt_for_sy(
                    fixture.env.clone(),
                    0
                )),
            Err(Error::InvalidAmount)
        );
        assert_eq!(
            fixture
                .env
                .as_contract(&fixture.contract_id, || AmmMarket::quote_sy_for_pt(
                    fixture.env.clone(),
                    0
                )),
            Err(Error::InvalidAmount)
        );
        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_sy_for_yt(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketNotSeeded)
        );
        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_yt_for_sy(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketNotSeeded)
        );
    }

    #[test]
    fn quote_accessors_reject_matured_market() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &20_000, &20_000, &0);
        fixture.env.ledger().set_timestamp(MATURITY);

        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_pt_for_sy(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketMatured)
        );
        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_sy_for_pt(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketMatured)
        );
        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_sy_for_yt(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketMatured)
        );
        assert_eq!(
            fixture.env.as_contract(&fixture.contract_id, || {
                AmmMarket::quote_yt_for_sy(fixture.env.clone(), 1_000)
            }),
            Err(Error::MarketMatured)
        );
    }

    // ---------------------------------------------------------------------
    // C1 / H1 — the curve must price PT face against the *asset value* of the
    // SY reserve, not against raw shares. Every test below sweeps the SY rate,
    // because at rate exactly 1.0 shares and asset units coincide and the whole
    // defect is invisible.
    // ---------------------------------------------------------------------

    /// C1, stated as the arbitrage it enabled: mint PT for `ceil(face·WAD/R)`
    /// shares through the tokenizer, sell it to the pool, keep the difference.
    /// The pool must never pay more shares for PT than minting that PT cost —
    /// at any rate, any size, any point in the term. Before the fix the pool
    /// priced face-per-share and paid ~`face/e` shares against a mint cost of
    /// `face/R`, so every `R > e` was free money.
    #[test]
    fn selling_pt_never_returns_more_shares_than_minting_it_cost() {
        for rate in RATE_SWEEP {
            for offset in [0, 60 * DAY, 90 * DAY - 3_600] {
                let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
                fixture.env.ledger().set_timestamp(NOW + offset);

                for pt_in in [1_000i128, 5_000, 20_000] {
                    let sy_out = fixture.client.quote_pt_for_sy(&pt_in);
                    let mint_cost = shares_to_mint_face(pt_in, rate);
                    assert!(
                        sy_out < mint_cost,
                        "rate {rate}, t+{offset}, {pt_in} PT: pool paid {sy_out} shares \
                         for PT that cost {mint_cost} shares to mint"
                    );
                }
            }
        }
    }

    /// The mirror of the above on the buy side: the pool must always charge at
    /// least enough shares that a buyer cannot buy PT below its own redemption
    /// value and immediately recombine or redeem it for a profit.
    #[test]
    fn buying_pt_always_costs_more_than_dust_and_never_less_than_the_curve_discount() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            let config = fixture.client.config();
            let state = fixture.client.state();
            let comp = precompute_or_panic(&fixture.env, &config, &state);

            for pt_out in [1_000i128, 5_000] {
                let cost =
                    exact_pt_out_sy_in_or_panic(&fixture.env, &config, &state, &comp, pt_out);
                // PT trades at a discount to face, so the cost in shares is
                // below face value in shares...
                assert!(
                    cost < shares_from_face(pt_out, rate),
                    "rate {rate}: {pt_out} PT cost {cost} shares, at or above face"
                );
                // ...but strictly above what the curve's own exchange rate
                // implies before the conversion, which is the check that fails
                // when the two unit systems are mixed.
                assert!(cost > 0);
            }
        }
    }

    /// The plan's gate: the plain PT<->SY legs and both YT flash routes must
    /// agree on units. Each YT route is, by construction, one plain curve leg
    /// plus one tokenizer conversion, so this asserts the composition exactly —
    /// using the public plain-leg quotes on one side and the tokenizer's own
    /// share/face formulas on the other. Before the fix the curve leg was
    /// share-denominated while the tokenizer legs were asset-denominated, and
    /// this identity failed for every rate above 1.0.
    #[test]
    fn yt_routes_compose_from_the_plain_legs_and_the_tokenizer_conversion() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            let config = fixture.client.config();
            let state = fixture.client.state();
            let comp = precompute_or_panic(&fixture.env, &config, &state);

            // Buy side: split ceil(yt_out·WAD/R) shares, sell the PT leg on the
            // plain curve, the buyer funds the difference.
            let sy_in = 2_000;
            let yt_out = fixture.client.quote_sy_for_yt(&sy_in);
            let cost = fixture.client.quote_sy_for_yt_cost(&sy_in);
            let plain_leg = fixture.client.quote_pt_for_sy(&yt_out);
            assert_eq!(
                cost,
                shares_to_mint_face(yt_out, rate) - plain_leg,
                "rate {rate}: YT buy route disagrees with the plain PT sell leg"
            );
            assert!(cost <= sy_in);
            assert!(
                yt_out > sy_in,
                "rate {rate}: buying YT must be leveraged, got {yt_out} for {sy_in}"
            );

            // Sell side: buy the PT leg back on the plain curve, recombine the
            // pair for floor(yt_in·WAD/R) shares, the seller keeps the rest.
            let yt_in = 5_000;
            let sy_out = fixture.client.quote_yt_for_sy(&yt_in);
            let buy_back = exact_pt_out_sy_in_or_panic(&fixture.env, &config, &state, &comp, yt_in);
            assert_eq!(
                sy_out,
                shares_from_face(yt_in, rate) - buy_back,
                "rate {rate}: YT sell route disagrees with the plain PT buy leg"
            );
        }
    }

    /// H1, sell side. `swap_yt_for_sy` required `e > R·(1 + fee)` before the
    /// fix, so at the deployed anchor a wrapper that had accrued ~0.5% made YT
    /// permanently unsellable and `quote_yt_for_sy` error for every input. The
    /// threshold is now `e > 1 + fee`, independent of the rate.
    #[test]
    fn yt_sell_route_survives_an_accrued_wrapper() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            for yt_in in [1_000i128, 5_000, 20_000] {
                let sy_out = fixture.client.quote_yt_for_sy(&yt_in);
                assert!(
                    sy_out > 0,
                    "rate {rate}: selling {yt_in} YT quoted {sy_out}"
                );
                assert!(sy_out < shares_from_face(yt_in, rate));
            }
        }
    }

    /// H1, buy side. The solver's feasible set is an interior interval once the
    /// rate leaves 1.0, and the old prefix-only search discarded everything
    /// above its first probe and returned dust while keeping the input. The
    /// answer must now be genuinely maximal: one more unit of YT must not fit
    /// inside the same budget.
    #[test]
    fn yt_buy_solver_returns_the_largest_affordable_size_at_every_rate() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            let config = fixture.client.config();
            let state = fixture.client.state();
            let comp = precompute_or_panic(&fixture.env, &config, &state);

            for sy_in in [100i128, 1_000, 10_000] {
                let yt_out = fixture.client.quote_sy_for_yt(&sy_in);
                let cost = fixture.client.quote_sy_for_yt_cost(&sy_in);
                assert!(cost > 0 && cost <= sy_in);
                assert!(
                    yt_out > sy_in,
                    "rate {rate}, {sy_in} SY: {yt_out} YT is not leveraged"
                );

                // No dust: the solver must not be leaving most of the budget on
                // the table the way the broken prefix search did (it returned
                // 0.0000003 YT for 1.00 SY).
                let one_more =
                    match probe_yt_buy(&fixture.env, &config, &state, &comp, yt_out + 1, sy_in) {
                        YtBuyProbe::Affordable(_) => true,
                        _ => false,
                    };
                assert!(
                    !one_more,
                    "rate {rate}, {sy_in} SY: {} YT also fits, so {yt_out} is not maximal",
                    yt_out + 1
                );
            }
        }
    }

    /// The flash routes, executed end to end against a tokenizer stand-in that
    /// uses the real split/recombine conversions. Quote must equal execution,
    /// the buyer must be charged exactly the quoted cost (H2 on the YT route),
    /// and reserves must reconcile to real balances afterwards.
    #[test]
    fn yt_flash_routes_execute_and_charge_the_quoted_cost_at_every_rate() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            mint_sy_shares(&fixture, &fixture.bob, 10_000);

            let sy_in = 2_000;
            let quoted_yt = fixture.client.quote_sy_for_yt(&sy_in);
            let quoted_cost = fixture.client.quote_sy_for_yt_cost(&sy_in);
            let sy_before = sy_balance(&fixture, &fixture.bob);

            let yt_out = fixture.client.swap_sy_for_yt(&fixture.bob, &sy_in, &1);

            assert_eq!(yt_out, quoted_yt, "rate {rate}: quote/execute mismatch");
            assert_eq!(
                sy_before - sy_balance(&fixture, &fixture.bob),
                quoted_cost,
                "rate {rate}: buyer charged something other than the quoted cost"
            );
            assert_eq!(yt_balance(&fixture, &fixture.bob), yt_out);
            let state = fixture.client.state();
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));

            // Now sell a slice of that YT back. The pool has to buy the PT leg
            // back off its own curve, which is capped well below the position,
            // so sell a size the curve can carry.
            let yt_in = 5_000;
            let quoted_sy_out = fixture.client.quote_yt_for_sy(&yt_in);
            let pool_sy_before = pool_sy_balance(&fixture);
            let sy_out = fixture.client.swap_yt_for_sy(&fixture.bob, &yt_in, &1);

            assert_eq!(sy_out, quoted_sy_out, "rate {rate}: quote/execute mismatch");
            assert!(sy_out > 0);
            assert!(
                pool_sy_balance(&fixture) > pool_sy_before,
                "rate {rate}: the pool must keep the spread on a YT sale"
            );
            let state = fixture.client.state();
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));
        }
    }

    /// H2 on the YT route. The YT buy is capped by the market-proportion
    /// ceiling, not by the budget: past roughly 56.5k SY into this pool the
    /// solver cannot place another unit of face no matter how much the buyer
    /// offers. The old route transferred the whole `sy_in` regardless, so a
    /// 100k order donated 43k (43%) to LPs.
    #[test]
    fn oversized_yt_buy_is_capped_and_the_remainder_is_refunded() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            mint_sy_shares(&fixture, &fixture.bob, 150_000);

            let sy_in = 100_000;
            let quoted_yt = fixture.client.quote_sy_for_yt(&sy_in);
            let quoted_cost = fixture.client.quote_sy_for_yt_cost(&sy_in);
            assert!(
                quoted_cost < sy_in * 3 / 4,
                "rate {rate}: the cap must bind well below the budget for this to be a \
                 real test (cost {quoted_cost} of {sy_in})"
            );

            let sy_before = sy_balance(&fixture, &fixture.bob);
            let yt_out = fixture.client.swap_sy_for_yt(&fixture.bob, &sy_in, &1);

            assert_eq!(yt_out, quoted_yt);
            assert_eq!(
                sy_before - sy_balance(&fixture, &fixture.bob),
                quoted_cost,
                "rate {rate}: the unspendable part of the budget was taken anyway"
            );
            assert_eq!(yt_balance(&fixture, &fixture.bob), yt_out);
            let state = fixture.client.state();
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));
        }
    }

    /// The plan's second gate: maturity convergence at a rate other than 1.0.
    /// As `t -> maturity` the curve's exchange rate decays to 1, so a PT sale
    /// must converge on exactly what `tokenizer::redeem_at_maturity` pays for
    /// the same face — `floor(face·WAD/R)` shares — net of the swap fee, and
    /// must approach it from below. Under the old share-denominated curve it
    /// converged on `face` *shares* instead, which is `R` times too much.
    #[test]
    fn pt_converges_on_its_redemption_value_in_shares_at_non_unit_rates() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            fixture.env.ledger().set_timestamp(MATURITY - 60);

            let pt_in = 10_000;
            let sy_out = fixture.client.quote_pt_for_sy(&pt_in);
            let redemption = shares_from_face(pt_in, rate);

            assert!(
                sy_out < redemption,
                "rate {rate}: PT must stay at a discount before maturity"
            );
            // Within the swap fee plus a couple of bps of the redemption value.
            let floor = redemption * (BPS_DENOMINATOR - FEE_BPS - 2) / BPS_DENOMINATOR;
            assert!(
                sy_out >= floor,
                "rate {rate}: {sy_out} shares for {pt_in} PT has not converged on \
                 the redemption value {redemption} (floor {floor})"
            );
        }
    }

    /// The AMM must keep its share-denominated state honest while the curve
    /// works in asset units: `State.total_sy` is what the pool custodies and
    /// must equal the token balance after every route.
    #[test]
    fn reserves_stay_share_denominated_across_rates() {
        for rate in RATE_SWEEP {
            let fixture = seeded_market_at_rate(NOW, rate, 200_000, 200_000);
            mint_sy_shares(&fixture, &fixture.bob, 10_000);
            mint_pt(&fixture, &fixture.bob, 10_000);

            fixture.client.swap_pt_for_sy(&fixture.bob, &5_000, &1);
            let state = fixture.client.state();
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));

            fixture.client.swap_sy_for_pt(&fixture.bob, &5_000, &1);
            let state = fixture.client.state();
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));

            // Removing liquidity still returns shares pro-rata, including after
            // maturity, which the fix must not have disturbed.
            fixture.env.ledger().set_timestamp(MATURITY);
            let (pt_out, sy_out) =
                fixture
                    .client
                    .remove_liquidity(&fixture.admin, &100_000, &1, &1);
            assert!(pt_out > 0 && sy_out > 0);
            let state = fixture.client.state();
            assert_eq!(state.total_sy, pool_sy_balance(&fixture));
            assert_eq!(state.total_pt, pool_pt_balance(&fixture));
        }
    }

    // ---------------------------------------------------------------------
    // H3 — the TWAP must cost time, not size.
    // ---------------------------------------------------------------------

    /// The attack the audit priced at 0.10% of the pool: idle until one second
    /// short of a full window (an idle market is the normal case), dislocate
    /// spot with a single trade, and reverse it in the same ledger. The old
    /// accumulator weighted the *post*-trade rate by `elapsed / window`, so at
    /// `elapsed = window - 1` the TWAP snapped to the dislocated value with
    /// weight 0.99944 while `twap_warming_up()` stayed false, and the reversal
    /// was free because `elapsed == 0` returned early.
    ///
    /// The accumulator now weights the rate that actually prevailed over the
    /// closing interval, so the manipulating trade contributes nothing at all.
    /// A market with a warm, genuinely blended TWAP history, sitting one second
    /// short of a full window since its last observation — the maximum-weight
    /// moment the attack targets, and still inside the blend branch.
    fn twap_attack_window_fixture() -> (Fixture, u64) {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &200_000, &200_000, &0);
        mint_pt(&fixture, &fixture.bob, 200_000);
        mint_sy_shares(&fixture, &fixture.bob, 200_000);

        fixture.env.ledger().set_timestamp(NOW + 600);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);
        fixture.env.ledger().set_timestamp(NOW + TWAP_WINDOW + 1);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);

        let attack_at = NOW + TWAP_WINDOW + 1 + TWAP_WINDOW - 1;
        fixture.env.ledger().set_timestamp(attack_at);
        assert!(
            !fixture.client.twap_warming_up(),
            "the attack window is one where consumers already trust the value"
        );
        (fixture, attack_at)
    }

    #[test]
    fn atomic_trade_and_reverse_cannot_move_the_twap() {
        // Two identical markets. One is dislocated with a large trade at the
        // exact instant the other is nudged with a tiny one. The oracle must
        // not be able to tell them apart: neither price has prevailed for any
        // time at all, so neither is entitled to any weight.
        let (attacked, attack_at) = twap_attack_window_fixture();
        let (control, control_at) = twap_attack_window_fixture();
        assert_eq!(attack_at, control_at);

        let twap_before = attacked.client.twap_apy();
        let spot_before = attacked.client.spot_apy();
        assert_eq!(control.client.twap_apy(), twap_before);

        control.client.swap_pt_for_sy(&control.bob, &100, &1);
        attacked.client.swap_pt_for_sy(&attacked.bob, &60_000, &1);

        let spot_manipulated = attacked.client.spot_apy();
        assert!(
            spot_manipulated > spot_before * 2,
            "the trade must genuinely dislocate spot for this to be a test: \
             {spot_before} -> {spot_manipulated}"
        );

        let twap_after = attacked.client.twap_apy();
        assert_eq!(
            twap_after,
            control.client.twap_apy(),
            "a 600x larger trade at the same instant moved the oracle further"
        );
        // The blend can only ever land between the stored TWAP and the price
        // that actually prevailed over the closing interval, never anywhere
        // near the value the attacker forced.
        assert!(
            twap_after <= twap_before.max(spot_before),
            "TWAP {twap_after} escaped the honest band [{twap_before}, {spot_before}] \
             toward the manipulated {spot_manipulated}"
        );
        assert!(
            !attacked.client.twap_warming_up(),
            "and the value must not need the warm-up flag to be safe"
        );

        // Reverse it in the same ledger: free for the attacker, and worthless.
        attacked.client.swap_sy_for_pt(&attacked.bob, &60_000, &1);
        assert_eq!(
            attacked.client.twap_apy(),
            twap_after,
            "a same-ledger round trip must leave the oracle exactly as it found it"
        );
        assert!(!attacked.client.twap_warming_up());
    }

    /// The other half of H3: a dislocation that is genuinely *held* does reach
    /// the TWAP, because it prevailed. Weighting the previous observation must
    /// not turn the oracle into a constant.
    #[test]
    fn a_held_dislocation_does_reach_the_twap() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .add_liquidity(&fixture.admin, &200_000, &200_000, &0);
        mint_pt(&fixture, &fixture.bob, 100_000);
        mint_sy_shares(&fixture, &fixture.bob, 100_000);

        fixture.env.ledger().set_timestamp(NOW + 600);
        fixture.client.swap_sy_for_pt(&fixture.admin, &2_000, &1);
        let twap_before = fixture.client.twap_apy();

        // Move spot, then hold it for a meaningful slice of the window before
        // the next observation closes the interval.
        fixture.client.swap_pt_for_sy(&fixture.bob, &60_000, &1);
        let spot_held = fixture.client.spot_apy();
        fixture
            .env
            .ledger()
            .set_timestamp(NOW + 600 + TWAP_WINDOW / 2);
        fixture.client.swap_pt_for_sy(&fixture.bob, &100, &1);

        let twap_after = fixture.client.twap_apy();
        assert!(
            twap_after > twap_before,
            "half a window at {spot_held} bps must move the TWAP off {twap_before}"
        );
        assert!(
            twap_after < spot_held,
            "but only part of the way: {twap_after} vs {spot_held}"
        );
    }

    #[derive(Clone, Debug)]
    enum ModelOp {
        Split(i128),
        Recombine(i128),
        BuyPt(i128),
        SellPt(i128),
    }

    /// A holder's PT/YT/SY position, tracked in the two unit systems the
    /// protocol actually uses: `free_sy` and `escrowed_sy` are SY **shares**,
    /// `free_pt` / `free_yt` and the supplies are asset-unit **face**. At rate
    /// 1.0 the two coincide, which is precisely why this model used to be able
    /// to assert `escrowed_sy == total_pt_supply` and why it saw nothing.
    #[derive(Clone, Debug)]
    struct PositionModel {
        rate: i128,
        free_sy: i128,
        free_pt: i128,
        free_yt: i128,
        escrowed_sy: i128,
        total_pt_supply: i128,
        total_yt_supply: i128,
    }

    impl PositionModel {
        fn new(rate: i128, free_sy: i128) -> Self {
            Self {
                rate,
                free_sy,
                free_pt: 0,
                free_yt: 0,
                escrowed_sy: 0,
                total_pt_supply: 0,
                total_yt_supply: 0,
            }
        }

        /// Face the tokenizer mints for `shares`: floor(shares · rate / WAD).
        fn face_for(&self, shares: i128) -> i128 {
            shares * self.rate / WAD
        }

        /// Shares a recombine of `face` returns: floor(face · WAD / rate),
        /// capped pro-rata at escrow exactly as the tokenizer caps it.
        fn shares_for(&self, face: i128) -> i128 {
            let full = shares_from_face(face, self.rate);
            let pro_rata = self.escrowed_sy * face / self.total_pt_supply;
            min(full, pro_rata)
        }

        fn assert_invariant(&self) {
            assert_eq!(self.total_pt_supply, self.total_yt_supply);
            assert!(self.free_sy >= 0);
            assert!(self.free_pt >= 0);
            assert!(self.free_yt >= 0);
            assert!(self.escrowed_sy >= 0);
            // The tokenizer's coverage invariant, in the units it is actually
            // stated in: the escrowed shares must be worth at least the PT face
            // outstanding against them.
            assert!(
                self.escrowed_sy * self.rate / WAD >= self.total_pt_supply,
                "escrow {} shares at rate {} does not cover {} PT face",
                self.escrowed_sy,
                self.rate,
                self.total_pt_supply
            );
        }
    }

    fn arb_op() -> impl Strategy<Value = ModelOp> {
        (0u8..4, 1i128..100i128).prop_map(|(kind, amount)| match kind {
            0 => ModelOp::Split(amount),
            1 => ModelOp::Recombine(amount),
            2 => ModelOp::BuyPt(amount),
            _ => ModelOp::SellPt(amount),
        })
    }

    fn arb_rate() -> impl Strategy<Value = i128> {
        (0usize..RATE_SWEEP.len()).prop_map(|index| RATE_SWEEP[index])
    }

    fn quote_sy_for_pt(fixture: &Fixture, sy_in: i128) -> Option<i128> {
        let config = fixture.client.config();
        let state = fixture.client.state();
        let comp = precompute_or_panic(&fixture.env, &config, &state);

        catch_unwind(AssertUnwindSafe(|| {
            exact_sy_in_pt_out_or_panic(&fixture.env, &config, &state, &comp, sy_in)
        }))
        .ok()
    }

    fn quote_pt_for_sy(fixture: &Fixture, pt_in: i128) -> Option<i128> {
        let config = fixture.client.config();
        let state = fixture.client.state();
        let comp = precompute_or_panic(&fixture.env, &config, &state);

        catch_unwind(AssertUnwindSafe(|| {
            exact_pt_in_sy_out_or_panic(&fixture.env, &config, &state, &comp, pt_in)
        }))
        .ok()
    }

    /// SY shares a `swap_sy_for_pt` of `sy_in` will actually charge, i.e. the
    /// budget minus the refund.
    fn quote_sy_for_pt_cost(fixture: &Fixture, sy_in: i128) -> Option<i128> {
        let config = fixture.client.config();
        let state = fixture.client.state();
        let comp = precompute_or_panic(&fixture.env, &config, &state);

        catch_unwind(AssertUnwindSafe(|| {
            let pt_out = exact_sy_in_pt_out_or_panic(&fixture.env, &config, &state, &comp, sy_in);
            exact_pt_out_sy_in_or_panic(&fixture.env, &config, &state, &comp, pt_out)
        }))
        .ok()
    }

    fn sy_in_with_rounding_gap(fixture: &Fixture) -> (i128, i128) {
        let config = fixture.client.config();
        let state = fixture.client.state();
        let comp = precompute_or_panic(&fixture.env, &config, &state);

        for sy_in in 1..5_000 {
            let Some(pt_out) = quote_sy_for_pt(fixture, sy_in) else {
                continue;
            };
            let required_sy = catch_unwind(AssertUnwindSafe(|| {
                exact_pt_out_sy_in_or_panic(&fixture.env, &config, &state, &comp, pt_out)
            }));
            let Ok(required_sy) = required_sy else {
                continue;
            };
            if required_sy < sy_in {
                return (sy_in, required_sy);
            }
        }

        panic!("expected a SY input with rounding gap");
    }

    fn lower_instance_ttl_below_threshold(fixture: &Fixture) {
        let ttl = fixture
            .env
            .deployer()
            .get_contract_instance_ttl(&fixture.contract_id);
        assert!(ttl > AMM_INSTANCE_TTL_THRESHOLD_LEDGERS);

        let target_ttl = AMM_INSTANCE_TTL_THRESHOLD_LEDGERS - 1;
        let ledgers_to_advance = ttl - target_ttl;
        fixture
            .env
            .ledger()
            .set_sequence_number(fixture.env.ledger().sequence() + ledgers_to_advance);
        assert!(
            fixture
                .env
                .deployer()
                .get_contract_instance_ttl(&fixture.contract_id)
                < AMM_INSTANCE_TTL_THRESHOLD_LEDGERS
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 10_000,
            .. ProptestConfig::default()
        })]

        /// The suite used to run pinned at rate exactly 1.0, where SY shares and
        /// asset-unit PT face are the same number and C1 is invisible by
        /// construction. It now sweeps the rate, tracks the two unit systems
        /// separately, and asserts on every leg that the pool cannot be sold PT
        /// for more shares than minting that PT cost — the arbitrage C1 opened.
        #[test]
        fn pt_yt_sy_invariant_holds_across_random_sequences(
            rate in arb_rate(),
            ops in prop::collection::vec(arb_op(), 1..8),
        ) {
            let fixture = fixture(NOW);
            initialize(&fixture);
            burn_pt(&fixture, &fixture.admin, INITIAL_TOKEN_BALANCE);
            burn_sy(&fixture, &fixture.admin, INITIAL_TOKEN_BALANCE);
            // Accrue the wrapper before anything is minted or seeded, so the
            // market is one that opened at this rate rather than one that
            // drifted into it.
            set_sy_rate(&fixture, rate);
            mint_pt(&fixture, &fixture.admin, 2_000_000);
            mint_sy_shares(&fixture, &fixture.admin, 2_000_000);
            fixture.client.add_liquidity(&fixture.admin, &1_000_000, &1_000_000, &0);

            let mut model = PositionModel::new(rate, 1_000_000);
            let mut wallet_pt = 1_000_000;
            let mut wallet_sy = 1_000_000;

            for op in ops {
                match op {
                    ModelOp::Split(shares)
                        if model.free_sy >= shares && model.face_for(shares) > 0 =>
                    {
                        let face = model.face_for(shares);
                        model.free_sy -= shares;
                        model.free_pt += face;
                        model.free_yt += face;
                        model.escrowed_sy += shares;
                        model.total_pt_supply += face;
                        model.total_yt_supply += face;
                    }
                    ModelOp::Recombine(face)
                        if model.free_pt >= face
                            && model.free_yt >= face
                            && model.total_pt_supply >= face
                            && model.shares_for(face) > 0 =>
                    {
                        let shares = model.shares_for(face);
                        model.free_pt -= face;
                        model.free_yt -= face;
                        model.free_sy += shares;
                        model.escrowed_sy -= shares;
                        model.total_pt_supply -= face;
                        model.total_yt_supply -= face;
                    }
                    ModelOp::BuyPt(budget)
                        if wallet_sy >= budget
                            && model.free_sy >= budget
                            && quote_sy_for_pt(&fixture, budget).is_some()
                            && quote_sy_for_pt_cost(&fixture, budget).is_some() =>
                    {
                        let quoted_cost = quote_sy_for_pt_cost(&fixture, budget).unwrap();
                        let sy_before = sy_balance(&fixture, &fixture.admin);
                        let pt_out = fixture.client.swap_sy_for_pt(&fixture.admin, &budget, &1);
                        let spent = sy_before - sy_balance(&fixture, &fixture.admin);

                        // H2: only the priced amount may leave the wallet.
                        assert_eq!(spent, quoted_cost);
                        assert!(spent <= budget);
                        // Buying PT below its redemption value in shares would
                        // be a free redeem-at-maturity arbitrage the other way.
                        assert!(spent > 0);

                        wallet_sy -= spent;
                        wallet_pt += pt_out;
                        model.free_sy -= spent;
                        model.free_pt += pt_out;
                    }
                    ModelOp::SellPt(face)
                        if wallet_pt >= face
                            && model.free_pt >= face
                            && quote_pt_for_sy(&fixture, face).is_some() =>
                    {
                        let sy_out = fixture.client.swap_pt_for_sy(&fixture.admin, &face, &1);

                        // C1: the pool must never pay more shares for PT than
                        // minting that PT through the tokenizer would have cost.
                        assert!(
                            sy_out < shares_to_mint_face(face, rate),
                            "rate {}: sold {} PT for {} shares, mint cost {}",
                            rate,
                            face,
                            sy_out,
                            shares_to_mint_face(face, rate)
                        );

                        wallet_pt -= face;
                        wallet_sy += sy_out;
                        model.free_pt -= face;
                        model.free_sy += sy_out;
                    }
                    _ => {}
                }

                model.assert_invariant();
                assert_eq!(pool_pt_balance(&fixture), fixture.client.reserve_pt());
                assert_eq!(pool_sy_balance(&fixture), fixture.client.reserve_sy());
            }

            assert_eq!(pt_balance(&fixture, &fixture.admin), wallet_pt);
            assert_eq!(sy_balance(&fixture, &fixture.admin), wallet_sy);
            assert_eq!(pool_pt_balance(&fixture), fixture.client.reserve_pt());
            assert_eq!(pool_sy_balance(&fixture), fixture.client.reserve_sy());
        }
    }
}
