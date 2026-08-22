// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractevent, contractimpl, contracttype, token, vec, Address, Env,
    IntoVal, MuxedAddress, Symbol, Val, Vec, I256,
};

const WAD: i128 = 1_000_000_000_000_000_000;

/// TTL policy, matching the AMM: bump when within 30 days of expiry, extend to
/// 120 days, so a periodically-touched market never archives mid-term.
const LEDGERS_PER_DAY: u32 = 17_280;
const TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

/// Basis-point denominator for `yield_fee_bps`.
const BPS_DENOMINATOR: i128 = 10_000;

/// Hard ceiling on the protocol's cut of claimed yield, enforced both at
/// initialization and on every admin update. Because it is checked in the wasm
/// rather than in a deploy script, it holds for every market anyone deploys
/// from this code. 20% is well above the 3-5% that comparable protocols charge,
/// and far below a level that would make YT pointless.
const MAX_YIELD_FEE_BPS: i128 = 2_000;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    pub sy_token: Address,
    pub pt_token: Address,
    pub yt_token: Address,
    pub maturity: u64,
    /// Where the protocol's cut of claimed yield is paid, in SY shares.
    pub fee_recipient: Address,
    /// The protocol's cut of *claimed yield*, in basis points. The configured
    /// admin may update it, subject to [`MAX_YIELD_FEE_BPS`].
    ///
    /// The fee is taken from the PT-senior-capped junior surplus (see
    /// `claim_yield`), never from principal and never from the escrow reserved
    /// for PT, so it cannot affect PT's face redemption. It moves SY shares
    /// between the tokenizer's escrow and the recipient without touching
    /// `total_assets` or the SY supply, so the derived exchange rate is
    /// unaffected — which is what makes it safe to add at all.
    pub yield_fee_bps: i128,
}

/// A holder's PT and YT balances, read from the real token contracts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Position {
    pub pt_balance: i128,
    pub yt_balance: i128,
}

#[derive(Clone)]
#[contracttype]
enum DataKey {
    Config,
    /// SY rate frozen at maturity, used for all post-maturity redemption so
    /// later rate moves cannot change what PT redeems for.
    MaturityRate,
    /// The most recent SY rate read by a pre-maturity mutating operation (or a
    /// permissionless `observe_rate` poke). The maturity freeze uses this
    /// observation instead of a live post-maturity read, so yield that a live
    /// source (Blend has no maturity concept) keeps accruing after maturity can
    /// never leak into redemption, no matter how late the first post-maturity
    /// call arrives.
    LastObservedRate,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidMaturity = 3,
    InvalidAmount = 4,
    AmountMismatch = 5,
    Matured = 6,
    MathOverflow = 7,
    LiveMarket = 8,
    /// Retired: no entrypoint gates on escrow coverage anymore (shortfalls are
    /// priced pro-rata at redemption instead). Kept so code 9 stays reserved.
    Insolvent = 9,
    /// `yield_fee_bps` was negative or above [`MAX_YIELD_FEE_BPS`].
    InvalidFee = 10,
    /// Caller is not the admin recorded at initialization.
    NotAdmin = 11,
    /// A fee-bearing market named a `fee_recipient` that would break the
    /// `escrow_out == ledger_debit` identity (the tokenizer itself) or strand
    /// the fee in a contract that cannot move it (one of this market's tokens).
    ///
    /// Numbered 12 rather than 11: both sides of the merge that introduced this
    /// claimed 11, and `NotAdmin` was already published on main with recorded
    /// snapshots, so it keeps the lower code.
    InvalidFeeRecipient = 12,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YieldFeeSet {
    pub admin: Address,
    pub old_fee_bps: i128,
    pub new_fee_bps: i128,
}

#[contract]
pub struct Tokenizer;

#[contractimpl]
impl Tokenizer {
    /// `yield_fee_bps` is the initial protocol cut of claimed yield. Pass 0 for
    /// a fee-free launch. The configured admin may update it later with
    /// [`Tokenizer::set_fee`], always subject to [`MAX_YIELD_FEE_BPS`].
    pub fn initialize(
        env: Env,
        admin: Address,
        sy_token: Address,
        pt_token: Address,
        yt_token: Address,
        maturity: u64,
        fee_recipient: Address,
        yield_fee_bps: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }

        admin.require_auth();

        if maturity <= env.ledger().timestamp() {
            return Err(Error::InvalidMaturity);
        }

        if !(0..=MAX_YIELD_FEE_BPS).contains(&yield_fee_bps) {
            return Err(Error::InvalidFee);
        }

        // Reject recipients that break the accounting identity the fee relies
        // on: `escrow_out == ledger_debit`. Paying the tokenizer itself makes
        // the push a self-transfer, so the YT ledger is debited the gross while
        // only the net actually leaves escrow — the difference is not lost, but
        // it silently inflates the next claimant's surplus and the apparent PT
        // coverage, unattributed. Paying a token in this market strands the
        // shares in a contract that cannot move them. This is the last moment
        // either can be caught: `fee_recipient` is immutable after this call.
        //
        // Scope, deliberately: these are the addresses this signature can see.
        // The strategy and the underlying token strand the fee just as
        // completely, and the AMM would absorb it into LP reserves via
        // `reconcile_reserves` — but the tokenizer is never told any of them,
        // and taking them as parameters would invert the dependency (the AMM
        // names the tokenizer, not the reverse). `scripts/deploy-market.sh`
        // carries the check for those, where all the addresses are in hand.
        if yield_fee_bps > 0
            && (fee_recipient == env.current_contract_address()
                || fee_recipient == sy_token
                || fee_recipient == pt_token
                || fee_recipient == yt_token)
        {
            return Err(Error::InvalidFeeRecipient);
        }

        let config = Config {
            admin,
            sy_token,
            pt_token,
            yt_token,
            maturity,
            fee_recipient,
            yield_fee_bps,
        };
        env.storage().instance().set(&DataKey::Config, &config);

        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, Error> {
        Self::read_config(&env)
    }

    /// Updates the protocol cut of claimed yield. Admin only.
    pub fn set_fee(env: Env, admin: Address, yield_fee_bps: i128) -> Result<(), Error> {
        let mut config = Self::read_config(&env)?;
        admin.require_auth();
        if admin != config.admin {
            return Err(Error::NotAdmin);
        }
        if !(0..=MAX_YIELD_FEE_BPS).contains(&yield_fee_bps) {
            return Err(Error::InvalidFee);
        }

        // Re-run `initialize`'s recipient guard. That check is gated on the fee
        // actually being charged, so a market opened at 0 bps is allowed to name
        // a recipient the guard would otherwise reject -- including this
        // contract itself. Without this, raising the fee here would walk such a
        // market straight into the hazard `InvalidFeeRecipient` exists to
        // prevent: `escrow_out == ledger_debit` broken by a self-transfer, or
        // the fee stranded in one of this market's own tokens.
        //
        // `fee_recipient` is still immutable, so this fails closed rather than
        // offering a way out: a 0 bps market that named a bad recipient can
        // never charge a fee. That is the intended trade for keeping the
        // recipient fixed at initialization.
        if yield_fee_bps > 0
            && (config.fee_recipient == env.current_contract_address()
                || config.fee_recipient == config.sy_token
                || config.fee_recipient == config.pt_token
                || config.fee_recipient == config.yt_token)
        {
            return Err(Error::InvalidFeeRecipient);
        }

        let old_fee_bps = config.yield_fee_bps;
        config.yield_fee_bps = yield_fee_bps;
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance_ttl(&env);

        YieldFeeSet {
            admin,
            old_fee_bps,
            new_fee_bps: yield_fee_bps,
        }
        .publish(&env);
        Ok(())
    }

    pub fn maturity(env: Env) -> Result<u64, Error> {
        Ok(Self::read_config(&env)?.maturity)
    }

    pub fn is_matured(env: Env) -> Result<bool, Error> {
        let config = Self::read_config(&env)?;
        Ok(env.ledger().timestamp() >= config.maturity)
    }

    /// Permissionless: before maturity, read the live SY rate and record it as
    /// the latest observation the maturity freeze may use. Every mutating
    /// operation records one as a side effect; this poke exists so anyone (a
    /// keeper, or a YT holder who wants the freeze to credit yield accrued
    /// right up to maturity) can refresh the observation on an otherwise idle
    /// market without moving tokens. Returns the observed rate.
    pub fn observe_rate(env: Env) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        Self::require_live(&env, &config)?;
        Self::bump_instance_ttl(&env);
        Ok(observe_live_rate(&env, &config))
    }

    /// Permissionless: after maturity, snapshot and return the SY rate used for
    /// all redemption. Any caller may poke this so the maturity rate is captured
    /// promptly; redemption also snapshots it lazily on first use. Idempotent
    /// once set. The snapshot is the last rate observed at or before maturity,
    /// never a live post-maturity read (see `effective_rate`), so the timing of
    /// this call cannot move value between PT and YT.
    pub fn freeze_maturity_rate(env: Env) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        if env.ledger().timestamp() < config.maturity {
            return Err(Error::LiveMarket);
        }
        Ok(effective_rate(&env, &config))
    }

    /// The frozen maturity rate, or 0 if not yet snapshotted.
    pub fn maturity_rate(env: Env) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::MaturityRate)
            .unwrap_or(0))
    }

    /// PT and YT minted for `sy_amount` SY at the current rate, in asset units.
    pub fn preview_split(env: Env, sy_amount: i128) -> Result<(i128, i128), Error> {
        let config = Self::read_config(&env)?;
        Self::require_live(&env, &config)?;
        Self::require_positive_amount(sy_amount)?;

        let rate = current_rate(&env, &config.sy_token);
        let face = mul_div_floor(&env, sy_amount, rate, WAD)?;
        Ok((face, face))
    }

    /// SY shares returned for recombining equal PT and YT (asset units) at the
    /// current rate. This is the principal only; any accrued YT yield is settled
    /// separately into the holder's claim ledger. Mirrors `recombine` exactly,
    /// including the pro-rata escrow cap, so the preview never overquotes
    /// during a rate-regression shortfall.
    ///
    /// Point-in-time read of the live Blend SY rate: if the rate moves between
    /// this quote and submission, the executed `recombine` share count can
    /// differ. The underlying value redeemed does not — `recombine` always
    /// returns `pt_face` worth of principal regardless of rate, so a moved rate
    /// changes the SY share count, not what it's worth. `recombine` has no
    /// on-chain `min_sy_out` floor by design; a caller needing an exact share
    /// count should compare this preview to its bound client-side before
    /// submitting.
    pub fn preview_recombine(env: Env, pt_amount: i128, yt_amount: i128) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        Self::require_live(&env, &config)?;
        Self::require_positive_amount(pt_amount)?;
        Self::require_positive_amount(yt_amount)?;

        if pt_amount != yt_amount {
            return Err(Error::AmountMismatch);
        }

        let rate = current_rate(&env, &config.sy_token);
        let full = mul_div_floor(&env, pt_amount, WAD, rate)?;
        let escrow_shares = token_balance(&env, &config.sy_token, &env.current_contract_address());
        let pt_supply = pt_total_supply(&env, &config.pt_token);
        let pro_rata = mul_div_floor(&env, escrow_shares, pt_amount, pt_supply)?;
        Ok(if full < pro_rata { full } else { pro_rata })
    }

    /// PT and YT balances the holder currently owns, read from the token
    /// contracts.
    pub fn position(env: Env, holder: Address) -> Result<Position, Error> {
        let config = Self::read_config(&env)?;
        Ok(Position {
            pt_balance: token_balance(&env, &config.pt_token, &holder),
            yt_balance: token_balance(&env, &config.yt_token, &holder),
        })
    }

    /// SY the tokenizer custodies, equal to the outstanding PT (and YT) supply.
    pub fn escrowed_sy(env: Env) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        Ok(token_balance(
            &env,
            &config.sy_token,
            &env.current_contract_address(),
        ))
    }

    /// Pulls `sy_amount` SY from `from` into escrow and mints equal PT and YT,
    /// denominated in asset units: `face = sy_amount * rate / WAD`. At rate 1.00
    /// this equals `sy_amount`. PT is the fixed principal claim; YT is the yield
    /// claim. The escrow holds the SY shares; their asset value at the current
    /// rate equals the PT face exactly at mint, which is the coverage invariant.
    pub fn split(env: Env, from: Address, sy_amount: i128) -> Result<(i128, i128), Error> {
        from.require_auth();
        let config = Self::read_config(&env)?;
        Self::require_live(&env, &config)?;
        Self::require_positive_amount(sy_amount)?;
        Self::bump_instance_ttl(&env);

        let rate = observe_live_rate(&env, &config);
        let face = mul_div_floor(&env, sy_amount, rate, WAD)?;
        Self::require_positive_amount(face)?;

        pull_token(&env, &config.sy_token, &from, sy_amount);
        mint_token(&env, &config.pt_token, &from, face);
        mint_token(&env, &config.yt_token, &from, face);

        // No solvency gate here. Split is collateral-neutral: it adds `sy_amount`
        // SY to escrow (asset value `face` at the current rate) and mints exactly
        // `face` of PT, so escrow coverage moves by an equal amount on both sides
        // and cannot worsen. Gating it only bricked new mints on a market that was
        // already underwater, which is not how Pendle behaves: minting PT/YT never
        // reverts on collateralization (PendleYieldToken._mintPY has no such
        // check). Any shortfall is priced at redemption by the pro-rata cap in
        // `recombine` and `redeem_at_maturity`, not blocked at mint.
        Ok((face, face))
    }

    /// Burns equal PT and YT (asset units) from `from` and returns principal in SY
    /// shares: `pt_amount * WAD / rate`, capped to the holder's pro-rata share of
    /// escrow under a shortfall (identical cap to `redeem_at_maturity`). Burning the
    /// YT settles the holder's accrued yield first (the YT burn hook banks it into
    /// the holder's claim ledger), so recombine returns only principal and the
    /// banked yield stays owed and covered by the remaining escrow. Never reverts on
    /// collateralization: a shortfall is priced as a haircut, matching Pendle.
    pub fn recombine(
        env: Env,
        from: Address,
        pt_amount: i128,
        yt_amount: i128,
    ) -> Result<i128, Error> {
        from.require_auth();
        let config = Self::read_config(&env)?;
        Self::require_live(&env, &config)?;
        Self::require_positive_amount(pt_amount)?;
        Self::require_positive_amount(yt_amount)?;

        if pt_amount != yt_amount {
            return Err(Error::AmountMismatch);
        }

        Self::bump_instance_ttl(&env);
        let rate = observe_live_rate(&env, &config);
        let full = mul_div_floor(&env, pt_amount, WAD, rate)?;

        // Cap the principal returned to the holder's pro-rata share of escrow, the
        // same guard `redeem_at_maturity` uses. When escrow fully covers PT this is
        // the full principal (`pro_rata >= full`), so solvent recombine is
        // unchanged; under a rate regression it is a fair haircut that preserves the
        // escrow/PT ratio for holders who have not yet exited. This replaces the old
        // hard solvency revert: like Pendle, recombine never blocks a redemption on
        // collateralization, it prices the shortfall instead. The YT burn above
        // settles the holder's accrued yield into their claim ledger first, so that
        // yield is not lost to the haircut.
        let escrow_shares = token_balance(&env, &config.sy_token, &env.current_contract_address());
        let pt_supply = pt_total_supply(&env, &config.pt_token);
        let pro_rata = mul_div_floor(&env, escrow_shares, pt_amount, pt_supply)?;
        let sy_equivalent = if full < pro_rata { full } else { pro_rata };
        Self::require_positive_amount(sy_equivalent)?;

        burn_token(&env, &config.pt_token, &from, pt_amount);
        // YT burns through the tokenizer-gated `burn_settled` with the rate
        // observed above: yt cannot call back into this contract for a rate
        // while it is on the stack, and handing the same observation down
        // keeps the yield banked by the settle consistent with what the
        // maturity freeze has on record.
        burn_settled_yt(&env, &config.yt_token, &from, yt_amount, rate);
        push_token(&env, &config.sy_token, &from, sy_equivalent);

        Ok(sy_equivalent)
    }

    /// After maturity, burns `pt_amount` PT (asset units) from `from` and returns
    /// principal in SY shares: `pt_amount * WAD / rate`, capped to the holder's
    /// pro-rata share of escrow.
    ///
    /// Insolvency guard: if a rate regression (negative yield, a slash) has left
    /// the escrow unable to cover all PT principal, the payout is capped to
    /// `escrow_shares * pt_amount / pt_supply`, so PT holders share the shortfall
    /// pro-rata rather than letting the first redeemers drain the escrow at the
    /// expense of the last. When solvent, the ideal payout is the smaller of the
    /// two, so this pays principal in full. Capping preserves the escrow/PT ratio,
    /// keeping every later redeemer's share fair.
    ///
    /// The rate read here is the current SY rate; Phase 3 step 9 snapshots a
    /// maturity rate so post-maturity rate moves do not change redemption.
    pub fn redeem_at_maturity(env: Env, from: Address, pt_amount: i128) -> Result<i128, Error> {
        from.require_auth();
        let config = Self::read_config(&env)?;
        Self::require_matured(&env, &config)?;
        Self::require_positive_amount(pt_amount)?;
        Self::bump_instance_ttl(&env);

        let rate = effective_rate(&env, &config);
        let full = mul_div_floor(&env, pt_amount, WAD, rate)?;

        let escrow_shares = token_balance(&env, &config.sy_token, &env.current_contract_address());
        let pt_supply = pt_total_supply(&env, &config.pt_token);
        let pro_rata = mul_div_floor(&env, escrow_shares, pt_amount, pt_supply)?;
        let sy_to_pay = if full < pro_rata { full } else { pro_rata };
        Self::require_positive_amount(sy_to_pay)?;

        burn_token(&env, &config.pt_token, &from, pt_amount);
        push_token(&env, &config.sy_token, &from, sy_to_pay);

        Ok(sy_to_pay)
    }

    /// Pays `holder` their accrued YT yield in SY out of escrow, capped so PT
    /// principal is always senior to banked YT yield, and returns the SY amount
    /// the holder actually received — net of `yield_fee_bps`. Allowed any time,
    /// including after maturity, so a holder can always collect yield earned
    /// over the term.
    ///
    /// PT-senior surplus cap. The YT contract settles the holder and reports the
    /// banked total `owed` WITHOUT zeroing it (`settle`). The tokenizer then pays
    /// only `min(owed, surplus)`, where
    ///   `surplus = max(0, escrow_shares - pt_face_reservation)`
    /// and `pt_face_reservation = ceil(pt_supply * WAD / rate)` is the SY escrow
    /// needed to redeem every outstanding PT at its face at `rate`. The
    /// reservation is rounded UP, so PT is never shorted by a rounding notch and
    /// the surplus is the conservative (smaller) amount. It then `consume`s
    /// exactly `pay` from the YT ledger and pushes `pay` SY. Anything owed beyond
    /// `pay` stays banked in the YT ledger, claimable later once the rate
    /// recovers (a transient sub-stroop dip) or, under a permanent slash, capped
    /// there forever by the short escrow: never overpaid, never lost.
    ///
    /// This makes already-banked YT yield JUNIOR to PT principal, correcting the
    /// prior first-come behavior where a YT holder could drain escrow in full
    /// while PT redemption was capped pro-rata, effectively senioring YT over PT.
    pub fn claim_yield(env: Env, holder: Address) -> Result<i128, Error> {
        holder.require_auth();
        let config = Self::read_config(&env)?;
        Self::bump_instance_ttl(&env);

        // Establish the canonical rate here, in the tokenizer, and hand it to
        // the YT contract. Before maturity this is the live SY rate; after
        // maturity `effective_rate` snapshots the frozen `MaturityRate` (or
        // reuses the existing snapshot), the same single value PT redemption
        // uses. YT cannot fetch it itself on this path: the tokenizer is already
        // on the call stack, so a callback into it would re-enter (prohibited).
        // Passing it down also fixes the ordering trap where a YT claim is the
        // first post-maturity action: the freeze happens here, before the YT
        // settle, so both sides settle against the same rate regardless of order.
        let rate = effective_rate(&env, &config);

        // Settle to learn what the holder is owed (their full banked total) at
        // this rate. `settle` banks and returns; it does not zero the ledger, so
        // if the surplus cannot cover all of it the remainder is left banked.
        let owed = settle_yt(&env, &config.yt_token, &holder, rate);

        // PT-senior surplus. Reserve enough escrow to redeem all outstanding PT
        // face at `rate` (rounded UP so PT is never shorted), and pay YT only out
        // of the remainder. `escrow_shares` and `pt_supply` are read the same way
        // `redeem_at_maturity` reads them.
        let escrow_shares =
            token_balance(&env, &config.sy_token, &env.current_contract_address());
        let pt_supply = pt_total_supply(&env, &config.pt_token);
        let pt_face_reservation = mul_div_ceil(&env, pt_supply, WAD, rate)?;
        let surplus = if escrow_shares > pt_face_reservation {
            escrow_shares - pt_face_reservation
        } else {
            0
        };
        let pay = if owed < surplus { owed } else { surplus };

        // Consume exactly what we pay, then push it. The remainder (owed - pay)
        // stays banked automatically because `settle` never zeroed it. Neither
        // `consume` (YT) nor `push_token` (SY) calls back into the tokenizer, so
        // the re-entrancy model is intact: the rate was computed and frozen above
        // and handed down, never fetched by a callee.
        // Protocol fee, taken from `pay` and never from `owed`.
        //
        // `pay` is already the junior surplus: what is left after
        // `pt_face_reservation` has been carved out and rounded UP. Skimming
        // here is therefore structurally incapable of reaching PT's principal —
        // PT stays senior to the protocol's own cut with no extra logic, and
        // during a shortfall `pay` is small or zero, so the protocol takes
        // little or nothing rather than competing with the YT holder for the
        // last of the escrow.
        //
        // It also cannot move the exchange rate. The rate is derived from the
        // strategy's assets over the SY supply; this only moves SY shares from
        // escrow to the recipient, changing neither. Rounded DOWN so the notch
        // favours the holder over the protocol.
        // `fee = floor(pay * bps / 10_000)` with `0 <= bps <= MAX_YIELD_FEE_BPS`
        // and `pay >= 0`, so `0 <= fee <= pay` and the subtraction cannot
        // underflow.
        let fee = mul_div_floor(&env, pay, config.yield_fee_bps, BPS_DENOMINATOR)?;
        let net = pay - fee;

        // `consume` the gross: the holder's banked ledger is settled for the
        // full amount that left escrow on their behalf, fee included, so the
        // fee cannot be re-claimed later.
        if pay > 0 {
            consume_yt(&env, &config.yt_token, &holder, pay);
            if net > 0 {
                push_token(&env, &config.sy_token, &holder, net);
            }
            if fee > 0 {
                push_token(&env, &config.sy_token, &config.fee_recipient, fee);
            }
        }

        Ok(net)
    }

    // Note: there is deliberately no `preview_claim_yield` wrapper here. YT's
    // `preview_claim_yield` is read directly (the SDK already calls it on the YT
    // contract). Wrapping it in the tokenizer would re-enter: post-maturity YT's
    // preview reads the canonical rate back from the tokenizer's `maturity_rate`,
    // and a tokenizer -> YT -> tokenizer hop is prohibited re-entry.

    fn read_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    fn require_live(env: &Env, config: &Config) -> Result<(), Error> {
        if env.ledger().timestamp() >= config.maturity {
            return Err(Error::Matured);
        }

        Ok(())
    }

    fn require_matured(env: &Env, config: &Config) -> Result<(), Error> {
        if env.ledger().timestamp() < config.maturity {
            return Err(Error::LiveMarket);
        }

        Ok(())
    }

    fn require_positive_amount(amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        Ok(())
    }
}

fn token_balance(env: &Env, token_id: &Address, who: &Address) -> i128 {
    token::TokenClient::new(env, token_id).balance(who)
}

/// Reads the SY exchange rate (asset per share, WAD scaled) from the SY contract.
fn current_rate(env: &Env, sy_token: &Address) -> i128 {
    let args: Vec<Val> = vec![env];
    env.invoke_contract(sy_token, &Symbol::new(env, "exchange_rate"), args)
}

/// `a * b / c`, rounded down (toward zero).
///
/// The product `a * b` is computed through a 256-bit intermediate, so it cannot
/// overflow before the divide even when the final quotient fits i128. The audit
/// flagged that the old i128 `checked_mul` path could spuriously revert on
/// reservation math (`pt_supply * WAD`) whose quotient is small but whose product
/// exceeds i128. Any i128 * i128 product fits in i256 (max ~5.79e76 > ~2.89e76),
/// so `mul` never overflows here; `MathOverflow` is returned only when the true
/// quotient does not fit i128. Callers pass strictly positive `a`, `b`, `c`, so
/// truncation toward zero equals floor.
fn mul_div_floor(env: &Env, a: i128, b: i128, c: i128) -> Result<i128, Error> {
    let prod = I256::from_i128(env, a).mul(&I256::from_i128(env, b));
    prod.div(&I256::from_i128(env, c))
        .to_i128()
        .ok_or(Error::MathOverflow)
}

/// `a * b / c`, rounded up (ceil), for non-negative product and positive divisor.
///
/// Uses `(a * b + c - 1) / c`, computed through the same 256-bit intermediate as
/// `mul_div_floor` so neither the product nor the `+ c - 1` bump can overflow
/// before the divide. Used only for the PT face reservation, where rounding up
/// reserves at least enough escrow to cover all outstanding PT and so can never
/// short a PT holder by a rounding notch. Callers pass strictly positive `a`,
/// `b`, `c`.
fn mul_div_ceil(env: &Env, a: i128, b: i128, c: i128) -> Result<i128, Error> {
    let c256 = I256::from_i128(env, c);
    let prod = I256::from_i128(env, a).mul(&I256::from_i128(env, b));
    let numerator = prod.add(&c256).sub(&I256::from_i128(env, 1));
    numerator
        .div(&c256)
        .to_i128()
        .ok_or(Error::MathOverflow)
}

/// Settles `holder` on the YT contract at `rate` and returns their banked total
/// in SY shares WITHOUT consuming it. Authorizes the call as the tokenizer, since
/// YT gates `settle` on the tokenizer's address.
fn settle_yt(env: &Env, yt_token: &Address, holder: &Address, rate: i128) -> i128 {
    let args: Vec<Val> = vec![env, holder.into_val(env), rate.into_val(env)];
    authorize_self_call(env, yt_token, "settle", args.clone());
    env.invoke_contract(yt_token, &Symbol::new(env, "settle"), args)
}

/// Consumes exactly `amount` SY shares from `holder`'s banked YT ledger (YT
/// asserts `amount <= banked`). Moves no tokens; the tokenizer pushes the SY out
/// of escrow separately. Authorizes the call as the tokenizer, since YT gates
/// `consume` on the tokenizer's address.
fn consume_yt(env: &Env, yt_token: &Address, holder: &Address, amount: i128) {
    let args: Vec<Val> = vec![env, holder.into_val(env), amount.into_val(env)];
    authorize_self_call(env, yt_token, "consume", args.clone());
    env.invoke_contract::<()>(yt_token, &Symbol::new(env, "consume"), args);
}

/// Burns `amount` YT from `from` via the tokenizer-gated `burn_settled`,
/// handing down `rate` so the settle inside the burn banks yield at the same
/// rate this contract observed (yt cannot call back in for one mid-call).
fn burn_settled_yt(env: &Env, yt_token: &Address, from: &Address, amount: i128, rate: i128) {
    let args: Vec<Val> = vec![
        env,
        from.into_val(env),
        amount.into_val(env),
        rate.into_val(env),
    ];
    authorize_self_call(env, yt_token, "burn_settled", args.clone());
    env.invoke_contract::<()>(yt_token, &Symbol::new(env, "burn_settled"), args);
}

/// Outstanding PT supply (asset units) read from the PT contract.
fn pt_total_supply(env: &Env, pt_token: &Address) -> i128 {
    let args: Vec<Val> = vec![env];
    env.invoke_contract(pt_token, &Symbol::new(env, "total_supply"), args)
}

/// Reads the live SY rate and records it as the latest pre-maturity
/// observation. Every mutating pre-maturity operation routes its rate read
/// through here, so the maturity freeze always has a rate that was actually
/// observed before maturity to fall back on.
fn observe_live_rate(env: &Env, config: &Config) -> i128 {
    let rate = current_rate(env, &config.sy_token);
    env.storage()
        .instance()
        .set(&DataKey::LastObservedRate, &rate);
    rate
}

/// The rate to value the escrow at: the live SY rate before maturity, and the
/// rate frozen at maturity afterwards. The freeze does NOT read the live rate
/// after maturity: Blend has no maturity concept and keeps accruing, so a live
/// post-maturity read would let the freeze timing move value between PT and YT
/// (a later snapshot means a higher rate, fewer SY shares per PT, and more for
/// YT). Instead the freeze uses the last rate observed at or before maturity
/// (every mutating operation records one, and anyone can record a fresher one
/// with `observe_rate`), so post-maturity accrual can never reach redemption
/// regardless of when the first post-maturity call lands. The unobserved tail
/// between the last observation and the maturity instant is credited to
/// neither side's advantage deterministically: the frozen rate is at most the
/// true maturity rate, which favors PT, consistent with PT being senior. YT
/// holders can narrow that tail to nothing by poking `observe_rate` shortly
/// before maturity. The live-read fallback below is reachable only for a
/// market that never had a single mutating operation, which has no PT or YT
/// outstanding and therefore nothing to misprice.
fn effective_rate(env: &Env, config: &Config) -> i128 {
    if env.ledger().timestamp() < config.maturity {
        return observe_live_rate(env, config);
    }
    if let Some(rate) = env
        .storage()
        .instance()
        .get::<_, i128>(&DataKey::MaturityRate)
    {
        return rate;
    }
    let rate = env
        .storage()
        .instance()
        .get::<_, i128>(&DataKey::LastObservedRate)
        .unwrap_or_else(|| current_rate(env, &config.sy_token));
    env.storage().instance().set(&DataKey::MaturityRate, &rate);
    rate
}

/// Pulls `amount` of `token_id` from `from` into the tokenizer (holder-authorized).
fn pull_token(env: &Env, token_id: &Address, from: &Address, amount: i128) {
    let to = MuxedAddress::from(&env.current_contract_address());
    token::TokenClient::new(env, token_id).transfer(from, &to, &amount);
}

/// Burns `amount` of `token_id` from `from` (holder-authorized).
fn burn_token(env: &Env, token_id: &Address, from: &Address, amount: i128) {
    token::TokenClient::new(env, token_id).burn(from, &amount);
}

/// Mints `amount` of `token_id` to `to`, authorizing the call as the tokenizer
/// since the token gates mint on the tokenizer's address.
fn mint_token(env: &Env, token_id: &Address, to: &Address, amount: i128) {
    let args: Vec<Val> = vec![env, to.into_val(env), amount.into_val(env)];
    authorize_self_call(env, token_id, "mint", args.clone());
    env.invoke_contract::<()>(token_id, &Symbol::new(env, "mint"), args);
}

/// Sends `amount` of `token_id` from the tokenizer to `to`, authorizing the
/// transfer as the tokenizer (it is moving its own custodied balance).
fn push_token(env: &Env, token_id: &Address, to: &Address, amount: i128) {
    let me = env.current_contract_address();
    let to_muxed = MuxedAddress::from(to);
    let args: Vec<Val> = vec![
        env,
        me.clone().into_val(env),
        to_muxed.clone().into_val(env),
        amount.into_val(env),
    ];
    authorize_self_call(env, token_id, "transfer", args);
    token::TokenClient::new(env, token_id).transfer(&me, &to_muxed, &amount);
}

/// Authorizes a sub-invocation of `token_id` as the current contract, so a
/// callee's `require_auth` on the tokenizer's address succeeds.
fn authorize_self_call(env: &Env, token_id: &Address, fn_name: &str, args: Vec<Val>) {
    env.authorize_as_current_contract(vec![
        env,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: token_id.clone(),
                fn_name: Symbol::new(env, fn_name),
                args,
            },
            sub_invocations: vec![env],
        }),
    ]);
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test {
    use super::*;
    use sidereal_sy_wrapper::{SyWrapper, SyWrapperClient};
    use soroban_sdk::testutils::{Address as _, Ledger};

    const NOW: u64 = 1_770_000_000;
    const MATURITY: u64 = NOW + 90 * 24 * 60 * 60;

    struct Fixture {
        env: Env,
        client: TokenizerClient<'static>,
        admin: Address,
        sy_token: Address,
        pt_token: Address,
        yt_token: Address,
        fee_recipient: Address,
    }

    fn fixture(now: u64) -> Fixture {
        let env = Env::default();
        env.ledger().set_timestamp(now);
        env.mock_all_auths();

        let contract_id = env.register(Tokenizer, ());
        let client = TokenizerClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        // A real SY wrapper supplies the exchange rate the tokenizer reads to
        // size mints and redemptions. It defaults to rate 1.00 after init.
        let sy_token = env.register(SyWrapper, ());
        SyWrapperClient::new(&env, &sy_token).initialize(&admin, &Address::generate(&env));

        let pt_token = Address::generate(&env);
        let yt_token = Address::generate(&env);
        let fee_recipient = Address::generate(&env);

        Fixture {
            env,
            client,
            admin,
            sy_token,
            pt_token,
            yt_token,
            fee_recipient,
        }
    }

    fn initialize(fixture: &Fixture) {
        initialize_with_fee(fixture, 0);
    }

    /// Most tests run fee-free so their arithmetic reads as the protocol's own;
    /// the fee tests opt in explicitly.
    fn initialize_with_fee(fixture: &Fixture, yield_fee_bps: i128) {
        fixture.client.initialize(
            &fixture.admin,
            &fixture.sy_token,
            &fixture.pt_token,
            &fixture.yt_token,
            &MATURITY,
            &fixture.fee_recipient,
            &yield_fee_bps,
        );
    }

    #[test]
    fn initialize_stores_config() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        assert_eq!(
            fixture.client.config(),
            Config {
                admin: fixture.admin.clone(),
                sy_token: fixture.sy_token.clone(),
                pt_token: fixture.pt_token.clone(),
                yt_token: fixture.yt_token.clone(),
                maturity: MATURITY,
                fee_recipient: fixture.fee_recipient.clone(),
                yield_fee_bps: 0,
            }
        );
        assert_eq!(fixture.client.maturity(), MATURITY);
        assert!(!fixture.client.is_matured());
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn initialize_rejects_past_maturity() {
        let fixture = fixture(NOW);
        fixture.client.initialize(
            &fixture.admin,
            &fixture.sy_token,
            &fixture.pt_token,
            &fixture.yt_token,
            &NOW,
            &fixture.fee_recipient,
            &0_i128,
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn initialize_rejects_a_fee_above_the_ceiling() {
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, MAX_YIELD_FEE_BPS + 1);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn initialize_rejects_a_negative_fee() {
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, -1);
    }

    #[test]
    fn initialize_accepts_a_fee_at_the_ceiling() {
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, MAX_YIELD_FEE_BPS);
        assert_eq!(fixture.client.config().yield_fee_bps, MAX_YIELD_FEE_BPS);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #12)")]
    fn a_fee_market_cannot_pay_itself() {
        // Paying the tokenizer would make the fee push a self-transfer: the YT
        // ledger debits the gross while only the net leaves escrow, quietly
        // breaking escrow_out == ledger_debit.
        let fixture = fixture(NOW);
        fixture.client.initialize(
            &fixture.admin,
            &fixture.sy_token,
            &fixture.pt_token,
            &fixture.yt_token,
            &MATURITY,
            &fixture.client.address,
            &500_i128,
        );
    }

    #[test]
    fn a_fee_market_cannot_pay_one_of_its_own_tokens() {
        // Shares sent to PT/YT/SY are stranded — none of those contracts can
        // move an SY balance back out. All three clauses are exercised: with
        // only one covered, deleting either of the others left the suite green.
        for which in ["sy", "pt", "yt"] {
            let fixture = fixture(NOW);
            let target = match which {
                "sy" => fixture.sy_token.clone(),
                "pt" => fixture.pt_token.clone(),
                _ => fixture.yt_token.clone(),
            };
            let result = fixture.client.try_initialize(
                &fixture.admin,
                &fixture.sy_token,
                &fixture.pt_token,
                &fixture.yt_token,
                &MATURITY,
                &target,
                &500_i128,
            );
            // Assert the CODE, not just that it failed: with `is_err()` alone,
            // changing the guard to return InvalidFee left this test green.
            assert_eq!(
                result,
                Err(Ok(Error::InvalidFeeRecipient)),
                "a fee market must refuse to pay its own {which} token"
            );
        }
    }

    #[test]
    fn a_fee_free_market_may_name_any_recipient() {
        // At 0 bps nothing is ever pushed, so the guard would only add a
        // deploy-time failure with no corresponding hazard. Name the tokenizer
        // itself — the address the guard rejects above — to pin that the check
        // is gated on the fee actually being charged.
        //
        // Since `set_fee` exists, such a market is not merely fee-free but
        // permanently so: raising the fee re-runs the recipient guard and
        // fails. That is deliberate: it fails closed. See
        // `raising_the_fee_revalidates_the_recipient`.
        let fixture = fixture(NOW);
        fixture.client.initialize(
            &fixture.admin,
            &fixture.sy_token,
            &fixture.pt_token,
            &fixture.yt_token,
            &MATURITY,
            &fixture.client.address,
            &0_i128,
        );
        assert_eq!(fixture.client.config().yield_fee_bps, 0);
    }

    #[test]
    fn reinitialize_cannot_reprice_a_live_market() {
        // Carried over from `there_is_no_fee_setter`, retired in the merge that
        // brought `set_fee` in: the "no setter" premise is gone, but the
        // re-initialization route to a different fee is still closed.
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, 500);
        assert_eq!(fixture.client.config().yield_fee_bps, 500);
        let repeat = fixture.client.try_initialize(
            &fixture.admin,
            &fixture.sy_token,
            &fixture.pt_token,
            &fixture.yt_token,
            &MATURITY,
            &fixture.fee_recipient,
            &0_i128,
        );
        assert!(repeat.is_err(), "a second initialize must not reprice a live market");
        assert_eq!(fixture.client.config().yield_fee_bps, 500);
    }

    #[test]
    fn admin_can_update_yield_fee() {
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, 500);
        fixture.client.set_fee(&fixture.admin, &750);
        let auths = fixture.env.auths();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].0, fixture.admin);
        assert_eq!(fixture.client.config().yield_fee_bps, 750);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn non_admin_cannot_update_yield_fee() {
        let fixture = fixture(NOW);
        initialize_with_fee(&fixture, 500);
        fixture
            .client
            .set_fee(&Address::generate(&fixture.env), &750);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn yield_fee_update_rejects_a_fee_above_the_ceiling() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture
            .client
            .set_fee(&fixture.admin, &(MAX_YIELD_FEE_BPS + 1));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn yield_fee_update_rejects_a_negative_fee() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture.client.set_fee(&fixture.admin, &-1);
    }

    #[test]
    fn raising_the_fee_revalidates_the_recipient() {
        // The merge that brought `set_fee` in met a recipient guard gated on
        // `yield_fee_bps > 0`. A market opened at 0 bps may legally name a
        // recipient the guard rejects, so without revalidation here, one
        // `set_fee` call would reopen the exact hole the guard closed.
        //
        // Assert the CODE: with `is_err()` alone this passes on NotAdmin.
        for which in ["self", "sy", "pt", "yt"] {
            let fixture = fixture(NOW);
            let target = match which {
                "self" => fixture.client.address.clone(),
                "sy" => fixture.sy_token.clone(),
                "pt" => fixture.pt_token.clone(),
                _ => fixture.yt_token.clone(),
            };
            fixture.client.initialize(
                &fixture.admin,
                &fixture.sy_token,
                &fixture.pt_token,
                &fixture.yt_token,
                &MATURITY,
                &target,
                &0_i128,
            );
            assert_eq!(
                fixture.client.try_set_fee(&fixture.admin, &500),
                Err(Ok(Error::InvalidFeeRecipient)),
                "raising the fee must refuse a {which} recipient"
            );
            // The rejection must not have been recorded.
            assert_eq!(fixture.client.config().yield_fee_bps, 0);
        }
    }

    #[test]
    fn a_good_recipient_still_admits_a_raise_from_zero() {
        // Guards the clause above from over-firing: the same 0 bps -> 500 path
        // must succeed whenever the recipient is a plain address.
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture.client.set_fee(&fixture.admin, &500);
        assert_eq!(fixture.client.config().yield_fee_bps, 500);
    }

    #[test]
    fn preview_split_returns_equal_pt_and_yt() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        assert_eq!(fixture.client.preview_split(&100), (100, 100));
    }

    // preview_recombine's happy path reads real escrow and PT supply for its
    // pro-rata cap, so it is covered in tests/integration (economics.rs)
    // against real tokens; the unit fixture's placeholder PT address cannot
    // answer total_supply. Only the argument gating is asserted here.
    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn preview_recombine_rejects_mismatched_pt_and_yt() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture.client.preview_recombine(&100, &99);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn preview_split_rejects_matured_market() {
        let fixture = fixture(NOW);
        initialize(&fixture);
        fixture.env.ledger().set_timestamp(MATURITY);
        fixture.client.preview_split(&100);
    }

    // The split/recombine/redeem flows move real tokens and are covered
    // end to end in tests/integration. Here we only assert the init gating.

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn split_before_initialize_fails() {
        let fixture = fixture(NOW);
        fixture.client.split(&fixture.admin, &100);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn recombine_before_initialize_fails() {
        let fixture = fixture(NOW);
        fixture.client.recombine(&fixture.admin, &10, &10);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn redeem_at_maturity_before_initialize_fails() {
        let fixture = fixture(NOW);
        fixture.client.redeem_at_maturity(&fixture.admin, &10);
    }
}
