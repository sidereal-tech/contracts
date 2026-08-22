// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, vec, Address, Env, I256,
    String, Symbol, Val,
};

const WAD: i128 = 1_000_000_000_000_000_000;

/// Display decimals for YT, matching SY and the 7-decimal underlying.
const DECIMALS: u32 = 7;

/// TTL policy, matching the AMM: bump when within 30 days of expiry, extend to
/// 120 days. A 90-day market that is touched periodically never archives.
const LEDGERS_PER_DAY: u32 = 17_280;
const TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    pub tokenizer: Address,
    pub sy_token: Address,
    pub maturity: u64,
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
    Balance(Address),
    /// (owner, spender)
    Allowance(Address, Address),
    /// The holder's yield BASIS, in SY shares: the number of SY shares that
    /// back their YT face at the rates that YT was acquired at. Persistent,
    /// holder-keyed (the contract is single-maturity, so maturity is implicit).
    ///
    /// This replaces the old per-address rate `Checkpoint`. A single rate per
    /// address is not a sound representation of a fungible position: it
    /// regressed when YT moved to an address with no checkpoint (audit H4,
    /// re-opening an already-paid interval) and over-held when new YT landed on
    /// an address already settled at a higher rate (audit M4, stranding the new
    /// position's yield in escrow). A share basis is additive, so it splits and
    /// merges with the tokens themselves and both failure modes disappear.
    YieldBasis(Address),
    /// SY shares accrued to the holder but not yet claimed, carried across
    /// transfers. Persistent, holder-keyed.
    AccruedYield(Address),
    /// Aggregate of every holder's `YieldBasis`. Instance-scoped.
    TotalYieldBasis,
    /// Aggregate of every holder's `AccruedYield`: the protocol-wide banked
    /// yield ledger. Instance-scoped.
    TotalAccruedYield,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidMaturity = 3,
    InvalidAmount = 4,
    InvalidExchangeRate = 5,
    ExchangeRateRegression = 6,
    InsufficientBalance = 7,
    InsufficientAllowance = 8,
    MathOverflow = 9,
    InvalidExpiration = 10,
    /// `consume` was asked to remove more than the holder's banked balance. This
    /// is a tokenizer-side invariant violation (it only consumes what a prior
    /// `settle` reported as banked), surfaced as an error rather than a silent
    /// underflow.
    ConsumeExceedsBanked = 11,
}

#[contract]
pub struct YtToken;

#[contractimpl]
impl YtToken {
    pub fn initialize(
        env: Env,
        admin: Address,
        tokenizer: Address,
        sy_token: Address,
        maturity: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }

        admin.require_auth();

        if maturity <= env.ledger().timestamp() {
            return Err(Error::InvalidMaturity);
        }

        let config = Config {
            admin,
            tokenizer,
            sy_token,
            maturity,
        };
        env.storage().instance().set(&DataKey::Config, &config);

        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, Error> {
        Self::read_config(&env)
    }

    pub fn maturity(env: Env) -> Result<u64, Error> {
        Ok(Self::read_config(&env)?.maturity)
    }

    // --- Yield accounting --------------------------------------------------

    /// The holder's yield basis in SY shares: how many SY shares back their YT
    /// face at the rates that YT was acquired at. Their claim at rate `R` is
    /// `basis - ceil(balance * WAD / R)` once that is positive, which is exactly
    /// the escrow surplus their position generates. Zero for an address holding
    /// no YT.
    pub fn yield_basis(env: Env, holder: Address) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_basis(&env, &holder))
    }

    /// Aggregate yield basis across every holder, maintained by delta so it
    /// cannot drift from the sum of `yield_basis`.
    ///
    /// **This is an upper bound on the YT claim, not an equality, and it is
    /// only tight before escrow starts leaving the market.** Basis is the whole
    /// escrow slice backing a YT position — principal plus yield — because the
    /// claim is the *difference* `basis - ceil(balance * WAD / R)`. Two
    /// tokenizer paths remove escrow without ever calling into this contract,
    /// so nothing here writes the basis down when they do:
    ///
    /// - `redeem_at_maturity` burns PT and pushes escrow out. PT burn does not
    ///   touch the YT ledger, and there is no write-down path (`write_basis` is
    ///   reachable only from mint/settle/move/burn of *YT*). After a full
    ///   redemption this aggregate can overstate the real obligation by most of
    ///   the position, on the ordinary happy path.
    /// - `recombine` by an *underwater* holder (one whose blended acquisition
    ///   rate is above the current rate) pays out `min(full, pro_rata)` while
    ///   retiring only their basis slice, which is smaller. The PT haircut
    ///   itself is fair, but escrow falls further than the ledger does.
    ///
    /// So `escrow_shares >= total_yield_basis() + total_accrued_yield()` holds
    /// only while no PT has been redeemed and no underwater recombine has
    /// happened. **A post-maturity sweep or a pro-rata junior split must not be
    /// sized off this number** — redemption is exactly the regime a sweep runs
    /// in, and there it reserves far too much. Closing that needs a write-down
    /// hook the tokenizer can call on PT burn, which does not exist yet.
    pub fn total_yield_basis(env: Env) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_total_basis(&env))
    }

    /// The SY rate implied by the holder's basis: `ceil(balance * WAD / basis)`,
    /// the blended rate their whole position last settled at. Exact for a
    /// position acquired entirely at one rate; a weighted blend otherwise. Zero
    /// for an address holding no YT. Kept as the human-readable view of the
    /// basis — the accounting itself uses the share basis, never this rate.
    pub fn checkpoint(env: Env, holder: Address) -> Result<i128, Error> {
        Self::read_config(&env)?;
        let basis = Self::read_basis(&env, &holder);
        let balance = Self::read_balance(&env, &holder);
        if basis <= 0 || balance <= 0 {
            return Ok(0);
        }
        Self::mul_div_ceil(&env, balance, WAD, basis)
    }

    /// SY shares already banked to the holder but not yet claimed.
    pub fn accrued_yield(env: Env, holder: Address) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_accrued(&env, &holder))
    }

    /// Aggregate banked (settled, unclaimed) yield across every holder,
    /// maintained by delta so it cannot drift from the sum of `accrued_yield`.
    /// Readable in one call without enumerating holders, which is the primitive
    /// a pro-rata junior split and a post-maturity escrow sweep both need.
    ///
    /// **It is what escrow *owes*, which is not the same as what escrow can
    /// pay.** `consume` is the only path that lowers it and it fires only on
    /// actual payment, so yield that is banked but permanently unpayable —
    /// because escrow drained below the PT reservation — accumulates here
    /// monotonically with no write-down. A shortfall can therefore leave this
    /// reading a large obligation against an escrow of almost nothing.
    ///
    /// Consumers must treat it as a claim, not a balance: pay out `min(this,
    /// what escrow actually holds above the PT reservation)`, never this alone.
    /// See `total_yield_basis` for the parallel caveat on the other aggregate.
    pub fn total_accrued_yield(env: Env) -> Result<i128, Error> {
        Self::read_config(&env)?;
        Ok(Self::read_total_accrued(&env))
    }

    /// Total SY shares claimable by `holder` right now: already-banked yield
    /// plus what a settle at the current SY rate would add. The contract reads
    /// the rate from the SY contract itself, so no caller can supply a fake one.
    ///
    /// Before maturity this is a point-in-time read of the live rate, so the
    /// executed `claim_yield` amount may differ if the rate moves between this
    /// quote and submission. After maturity it uses the tokenizer's frozen
    /// maturity rate (see `preview_rate`), so it no longer tracks live accrual.
    ///
    /// **This figure is GROSS.** It is the yield the position has earned, which
    /// is the right number for valuing YT — but it is not necessarily what the
    /// holder receives. `Tokenizer::claim_yield` caps the payout at the junior
    /// surplus over PT's reservation and then takes the market's
    /// `yield_fee_bps`, and it returns that net amount. This contract
    /// deliberately does not apply either adjustment: the cap depends on escrow
    /// and PT supply that live in the tokenizer, and reading them from here
    /// during a claim would be re-entry. A UI quoting "you will receive" should
    /// read `yield_fee_bps` from the tokenizer's config and apply it.
    pub fn preview_claim_yield(env: Env, holder: Address) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        let rate = Self::preview_rate(&env, &config);
        let banked = Self::read_accrued(&env, &holder);
        let pending = Self::pending_yield(&env, &holder, rate)?;
        banked.checked_add(pending).ok_or(Error::MathOverflow)
    }

    /// Settles `holder` at the `rate` supplied by the tokenizer and returns their
    /// current banked total in SY shares WITHOUT zeroing it. Restricted to the
    /// tokenizer. Moves no tokens; it is the first half of a claim.
    ///
    /// This is deliberately split from `consume` (they used to be one
    /// `settle_and_consume` that zeroed the whole ledger). All-or-nothing consume
    /// could not express a partial payment, but the tokenizer now caps a claim to
    /// the escrow surplus over the senior PT reservation. So it `settle`s to learn
    /// the owed total, decides how much the surplus can cover, and `consume`s only
    /// that. Whatever it does not consume stays banked and is claimable later.
    ///
    /// The rate is passed in, not read here, on purpose. The tokenizer is already
    /// on the call stack when it invokes this (claim_yield -> here), so yt cannot
    /// call back into the tokenizer to fetch the canonical maturity rate: Soroban
    /// prohibits re-entering a contract already on the stack. The tokenizer instead
    /// computes its single canonical rate (live before maturity, its frozen
    /// snapshot after) and hands it down. Trusting it is safe because this
    /// entrypoint is gated on the tokenizer's own auth, so no other caller can
    /// supply a rate.
    pub fn settle(env: Env, holder: Address, rate: i128) -> Result<i128, Error> {
        let config = Self::read_config(&env)?;
        config.tokenizer.require_auth();
        Self::bump_instance_ttl(&env);
        Self::settle_into_ledger(&env, &holder, rate);
        Ok(Self::read_accrued(&env, &holder))
    }

    /// Subtracts exactly `amount` SY shares from `holder`'s banked ledger.
    /// Restricted to the tokenizer, which calls this after `settle` and pushes the
    /// same `amount` of SY out of escrow itself. Moves no tokens. `amount == 0` is
    /// a no-op; `amount` must be `<= banked` (the tokenizer only ever consumes what
    /// a prior `settle` reported), enforced so the ledger can never go negative.
    /// The remainder stays banked and claimable later once escrow can cover it.
    pub fn consume(env: Env, holder: Address, amount: i128) -> Result<(), Error> {
        let config = Self::read_config(&env)?;
        config.tokenizer.require_auth();
        if amount < 0 {
            return Err(Error::InvalidAmount);
        }
        Self::bump_instance_ttl(&env);
        let banked = Self::read_accrued(&env, &holder);
        if amount > banked {
            return Err(Error::ConsumeExceedsBanked);
        }
        if amount > 0 {
            Self::write_accrued(&env, &holder, banked - amount);
        }
        Ok(())
    }

    // Note: there is deliberately no aggregate `total_claimable(rate)` here.
    // The two aggregates above are exact sums and compose safely; a rate-priced
    // aggregate does not. Per holder the claim is `max(0, basis - required)`,
    // and the max does not distribute over a sum: a holder whose basis rate is
    // above `rate` is held at zero rather than contributing a negative, so
    // `total_yield_basis - ceil(total_supply * WAD / rate)` sits BELOW the true
    // total whenever anyone is underwater. Sizing a sweep or a pro-rata junior
    // split off that would under-reserve exactly when the market is short.
    // Callers wanting a rate-priced total must settle the holders they are
    // paying, which is what the tokenizer's claim path already does.

    // --- Minter-privileged supply control (only the tokenizer) -------------

    /// Mints `amount` YT to `to`. Restricted to the tokenizer recorded at
    /// initialization, which mints YT when a holder splits SY. The recipient is
    /// settled first, so an existing holder's prior yield is banked before the
    /// balance grows; the new YT then adds its OWN basis at the mint rate, on
    /// top of whatever basis the recipient already carried. That is what fixes
    /// audit M4: a second split at a lower rate into an already-settled address
    /// earns from its own, lower basis instead of inheriting the address's
    /// earlier high-water rate and earning nothing.
    pub fn mint(env: Env, to: Address, amount: i128) {
        let config = Self::read_config_or_panic(&env);
        config.tokenizer.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);

        // Mint is only reached through the tokenizer's `split`, which runs
        // pre-maturity with the tokenizer on the call stack, so this cannot
        // route through `committing_rate` (calling back into the tokenizer
        // would re-enter). A plain live read is equivalent here: `split`
        // observed this same rate itself in this same transaction, so the
        // freeze already has it on record.
        let rate = Self::current_rate(&env, &config);
        Self::settle_into_ledger(&env, &to, rate);

        // Basis for the newly minted face, rounded UP so the escrow is never
        // short: the tokenizer floors `face = sy_amount * rate / WAD`, so
        // `ceil(face * WAD / rate) <= sy_amount`, the SY it just escrowed.
        let added = match Self::shares_for_face(&env, amount, rate) {
            Ok(value) => value,
            Err(error) => panic_with_error!(&env, error),
        };
        let basis = Self::read_basis(&env, &to);
        Self::write_basis(&env, &to, Self::add_or_panic(&env, basis, added));

        let balance = Self::read_balance(&env, &to);
        Self::write_balance(&env, &to, Self::add_or_panic(&env, balance, amount));
        let supply = Self::read_total_supply(&env);
        env.storage().instance().set(
            &DataKey::TotalSupply,
            &Self::add_or_panic(&env, supply, amount),
        );
    }

    // --- SEP-41 token interface -------------------------------------------

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
        String::from_str(&env, "Sidereal Yield Token")
    }

    pub fn symbol(env: Env) -> String {
        String::from_str(&env, "sYT")
    }

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        Self::read_allowance(&env, &from, &spender).amount
    }

    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
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

    /// Moves `amount` YT from `from` to `to`. Both parties are settled at the
    /// same rate before any balance moves, so each banks the yield it earned on
    /// the balance it actually held, and the basis backing the moved YT travels
    /// with it (see `move_position`). A self-transfer is a clean no-op: the
    /// second settle finds nothing left to bank and the basis slice leaves and
    /// returns to the same address.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        let config = Self::read_config_or_panic(&env);
        let rate = Self::committing_rate(&env, &config);
        Self::settle_into_ledger(&env, &from, rate);
        Self::settle_into_ledger(&env, &to, rate);
        Self::move_position(&env, &from, &to, amount);
    }

    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        Self::spend_allowance(&env, &from, &spender, amount);
        let config = Self::read_config_or_panic(&env);
        let rate = Self::committing_rate(&env, &config);
        Self::settle_into_ledger(&env, &from, rate);
        Self::settle_into_ledger(&env, &to, rate);
        Self::move_position(&env, &from, &to, amount);
    }

    /// Burns `amount` YT from `from`, on a holder's own direct call. The
    /// holder is settled first so their accrued yield is banked before the
    /// balance shrinks, at a rate observed through the tokenizer. The
    /// tokenizer's recombine burns through `burn_settled` instead, passing the
    /// rate down, because it is on the call stack here and cannot be called
    /// back into.
    ///
    /// This can drop YT total_supply below PT total_supply by design — not a
    /// bug. No economic path reads YT total_supply; the tokenizer's escrow,
    /// PT-senior cap, and pro-rata math read only `pt_total_supply`.
    pub fn burn(env: Env, from: Address, amount: i128) {
        from.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        let config = Self::read_config_or_panic(&env);
        let rate = Self::committing_rate(&env, &config);
        Self::settle_into_ledger(&env, &from, rate);
        Self::burn_position(&env, &from, amount);
    }

    /// Burns `amount` YT from `from`, settling them first at the `rate`
    /// supplied by the tokenizer. Restricted to the tokenizer, which calls
    /// this from `recombine` while it is on the call stack: yt cannot call
    /// back into it to observe a rate, so the tokenizer hands down the same
    /// rate it observed in that transaction. Trusting the argument is safe
    /// because of the auth gate, the same model as `settle` and `consume`.
    /// The holder's own authorization is enforced by `recombine` itself.
    pub fn burn_settled(env: Env, from: Address, amount: i128, rate: i128) -> Result<(), Error> {
        let config = Self::read_config(&env)?;
        config.tokenizer.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        Self::bump_instance_ttl(&env);
        Self::settle_into_ledger(&env, &from, rate);
        Self::burn_position(&env, &from, amount);
        Ok(())
    }

    pub fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        spender.require_auth();
        Self::require_amount_or_panic(&env, amount);
        Self::bump_instance_ttl(&env);
        Self::spend_allowance(&env, &from, &spender, amount);
        let config = Self::read_config_or_panic(&env);
        let rate = Self::committing_rate(&env, &config);
        Self::settle_into_ledger(&env, &from, rate);
        Self::burn_position(&env, &from, amount);
    }

    // --- yield engine ------------------------------------------------------

    /// Reads the live SY exchange rate (asset per share, WAD scaled) from the
    /// SY contract recorded at initialization. The contract reads it itself so
    /// no caller can supply a manipulated rate.
    fn current_rate(env: &Env, config: &Config) -> i128 {
        let args: soroban_sdk::Vec<Val> = vec![env];
        env.invoke_contract(&config.sy_token, &Symbol::new(env, "exchange_rate"), args)
    }

    /// The rate a state-committing settle uses on a path yt is entered
    /// DIRECTLY by a holder (transfer, transfer_from, burn, burn_from). Both
    /// branches go through the tokenizer, never a raw SY read, so every rate
    /// this contract banks yield at is also on record for the maturity freeze:
    ///
    /// - Before maturity: the tokenizer's permissionless `observe_rate`, which
    ///   reads the live SY rate, records it as the freeze's latest
    ///   pre-maturity observation, and returns it. Without this, a direct YT
    ///   transfer could bank yield at a rate the freeze never saw, and the
    ///   first post-maturity touch could then freeze an older, lower rate,
    ///   starving YT of yield its ledger already recognized.
    /// - After maturity: the tokenizer's `freeze_maturity_rate`, the single
    ///   canonical frozen rate. yt keeps no maturity snapshot of its own.
    ///
    /// This must never run while the tokenizer is on the call stack, or the
    /// cross-contract call would re-enter it (prohibited). The callers here
    /// are entered directly by a holder, never by the tokenizer: the
    /// tokenizer-driven paths receive the rate as an argument instead
    /// (`settle`, `consume`, `burn_settled`) or take a plain live read that
    /// the tokenizer observed itself in the same transaction (`mint`).
    fn committing_rate(env: &Env, config: &Config) -> i128 {
        let args: soroban_sdk::Vec<Val> = vec![env];
        if env.ledger().timestamp() < config.maturity {
            return env.invoke_contract(
                &config.tokenizer,
                &Symbol::new(env, "observe_rate"),
                args,
            );
        }
        env.invoke_contract(
            &config.tokenizer,
            &Symbol::new(env, "freeze_maturity_rate"),
            args,
        )
    }

    /// The rate for a read-only yield preview. Before maturity, the live SY
    /// rate. After maturity, the tokenizer's canonical frozen rate once it has
    /// been snapshotted, else the live SY rate as a best estimate (the value the
    /// first post-maturity action will freeze). This never writes, so it is safe
    /// to run in a simulation, and it reads the tokenizer without freezing.
    fn preview_rate(env: &Env, config: &Config) -> i128 {
        if env.ledger().timestamp() < config.maturity {
            return Self::current_rate(env, config);
        }
        let args: soroban_sdk::Vec<Val> = vec![env];
        let frozen: i128 = env.invoke_contract(
            &config.tokenizer,
            &Symbol::new(env, "maturity_rate"),
            args,
        );
        if frozen > 0 {
            frozen
        } else {
            Self::current_rate(env, config)
        }
    }

    /// SY shares needed to back `face` asset units of YT at `rate`, rounded UP.
    /// Rounding up is the escrow-favoring direction on both sides of the
    /// accounting: it shrinks the yield a settle recognizes
    /// (`basis - shares_for_face`) and it makes a mint reserve at least as much
    /// basis as the SY the tokenizer actually escrowed.
    fn shares_for_face(env: &Env, face: i128, rate: i128) -> Result<i128, Error> {
        if rate <= 0 {
            return Err(Error::InvalidExchangeRate);
        }
        if face <= 0 {
            return Ok(0);
        }
        Self::mul_div_ceil(env, face, WAD, rate)
    }

    /// SY shares `holder` would accrue if settled at `rate` right now, without
    /// writing anything. Zero while the rate sits at or below the rate their
    /// basis was struck at.
    fn pending_yield(env: &Env, holder: &Address, rate: i128) -> Result<i128, Error> {
        let balance = Self::read_balance(env, holder);
        if balance <= 0 {
            return Ok(0);
        }
        let basis = Self::read_basis(env, holder);
        let required = Self::shares_for_face(env, balance, rate)?;
        Ok(if basis > required { basis - required } else { 0 })
    }

    /// Banks `holder`'s accrued yield up to `rate` and re-strikes their basis at
    /// `rate`. Bookkeeping only: it never moves SY. The caller sources `rate`
    /// (live before maturity, the tokenizer's canonical frozen rate after) so
    /// this function makes no cross-contract call of its own.
    ///
    /// The whole engine is one identity. A holder's basis is the SY shares that
    /// back their YT face at the rates they acquired it at; the shares that same
    /// face needs at `rate` is `shares_for_face(balance, rate)`; the difference
    /// is exactly the escrow surplus their position has generated, so
    ///
    /// ```text
    /// owed      = basis - shares_for_face(balance, rate)
    /// new_basis =         shares_for_face(balance, rate)
    /// ```
    ///
    /// with `new_basis + owed == basis` exactly — no share is created or
    /// destroyed by a settle, which is why yield telescopes exactly across
    /// intermediate settlements. Settling twice (c -> r1 -> r2) banks
    /// `basis(c) - S(r1) + S(r1) - S(r2)`, identical to one settle c -> r2, and
    /// this holds for the rounded integers too because the same `S(r1)` is both
    /// subtracted and added back.
    ///
    /// On a rate dip `basis <= required` and the basis is HELD, not lowered, so
    /// the dip pays nothing and the holder resumes accruing only once the rate
    /// climbs back above it. Unlike the old per-address rate checkpoint, this
    /// hold cannot be dodged by moving the YT to an address that has none: basis
    /// travels with the tokens (see `move_position`), so there is no address
    /// whose basis is "unset" and gets initialized to a dipped rate (audit H4).
    ///
    /// Named `settle_into_ledger` to distinguish it from the public `settle`
    /// entrypoint, which wraps this and returns the banked total without zeroing.
    fn settle_into_ledger(env: &Env, holder: &Address, rate: i128) {
        let balance = Self::read_balance(env, holder);
        if balance <= 0 {
            // No YT, so no basis and nothing to settle. A fresh recipient is
            // settled here before their balance grows; their basis arrives with
            // the tokens themselves, in `mint` or `move_position`.
            return;
        }
        let basis = Self::read_basis(env, holder);
        let required = match Self::shares_for_face(env, balance, rate) {
            Ok(value) => value,
            Err(error) => panic_with_error!(env, error),
        };
        if basis <= required {
            return;
        }

        let owed = basis - required;
        Self::write_basis(env, holder, required);
        let prev = Self::read_accrued(env, holder);
        Self::write_accrued(env, holder, Self::add_or_panic(env, prev, owed));
    }

    /// `a * b / c` rounded up, via `(a * b + c - 1) / c` computed through a
    /// 256-bit intermediate so the product cannot overflow before the divide
    /// (`balance * WAD` exceeds i128 well before the quotient does). Callers
    /// pass non-negative `a`, `b` and positive `c`.
    fn mul_div_ceil(env: &Env, a: i128, b: i128, c: i128) -> Result<i128, Error> {
        let c256 = I256::from_i128(env, c);
        let prod = I256::from_i128(env, a).mul(&I256::from_i128(env, b));
        prod.add(&c256)
            .sub(&I256::from_i128(env, 1))
            .div(&c256)
            .to_i128()
            .ok_or(Error::MathOverflow)
    }

    // --- internal helpers --------------------------------------------------

    fn read_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }

    fn read_config_or_panic(env: &Env) -> Config {
        match Self::read_config(env) {
            Ok(config) => config,
            Err(error) => panic_with_error!(env, error),
        }
    }

    fn read_basis(env: &Env, holder: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::YieldBasis(holder.clone()))
            .unwrap_or(0)
    }

    /// Writes the holder's basis and keeps `TotalYieldBasis` in step by the
    /// same delta, so the aggregate is exact by construction rather than by a
    /// separate accounting path that could drift.
    fn write_basis(env: &Env, holder: &Address, amount: i128) {
        let prev = Self::read_basis(env, holder);
        let key = DataKey::YieldBasis(holder.clone());
        env.storage().persistent().set(&key, &amount);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);

        let total = Self::read_total_basis(env);
        let delta = match amount.checked_sub(prev) {
            Some(value) => value,
            None => panic_with_error!(env, Error::MathOverflow),
        };
        env.storage()
            .instance()
            .set(&DataKey::TotalYieldBasis, &Self::add_or_panic(env, total, delta));
    }

    fn read_total_basis(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalYieldBasis)
            .unwrap_or(0)
    }

    fn read_total_accrued(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalAccruedYield)
            .unwrap_or(0)
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    fn read_accrued(env: &Env, holder: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccruedYield(holder.clone()))
            .unwrap_or(0)
    }

    /// Writes the holder's banked yield and keeps `TotalAccruedYield` in step by
    /// the same delta, so the aggregate banked-yield ledger is exact.
    fn write_accrued(env: &Env, holder: &Address, amount: i128) {
        let prev = Self::read_accrued(env, holder);
        let key = DataKey::AccruedYield(holder.clone());
        env.storage().persistent().set(&key, &amount);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);

        let total = Self::read_total_accrued(env);
        let delta = match amount.checked_sub(prev) {
            Some(value) => value,
            None => panic_with_error!(env, Error::MathOverflow),
        };
        env.storage().instance().set(
            &DataKey::TotalAccruedYield,
            &Self::add_or_panic(env, total, delta),
        );
    }

    fn require_amount_or_panic(env: &Env, amount: i128) {
        if amount <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
    }

    fn read_balance(env: &Env, id: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(id.clone()))
            .unwrap_or(0)
    }

    fn write_balance(env: &Env, id: &Address, amount: i128) {
        let key = DataKey::Balance(id.clone());
        env.storage().persistent().set(&key, &amount);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }

    fn read_total_supply(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    /// Moves `amount` YT and the basis that backs it from `from` to `to`. Both
    /// parties are settled before this runs, so `from`'s basis is already struck
    /// at the current rate and the pro-rata slice is exact for the common case.
    ///
    /// Carrying the basis is the fix for audit H4. The old code left the
    /// receiver's rate checkpoint to be initialized by the settle, which for an
    /// address that had none meant "start at the current rate" — during a dip
    /// that reset the YT's high-water mark downward and re-opened an interval
    /// the sender had already been paid for. Basis is a share quantity, so it
    /// simply splits: whatever leaves `from` arrives at `to`, the aggregate is
    /// conserved, and a fully-paid-up position stays fully paid up wherever it
    /// lands.
    ///
    /// The slice rounds UP, so the sender parts with at least the proportional
    /// basis and can never keep a sliver that lets the pair claim more than one
    /// undivided holder would have. A self-transfer nets to zero on both the
    /// balance and the basis.
    fn move_position(env: &Env, from: &Address, to: &Address, amount: i128) {
        let from_balance = Self::read_balance(env, from);
        if from_balance < amount {
            panic_with_error!(env, Error::InsufficientBalance);
        }

        let from_basis = Self::read_basis(env, from);
        let moved = Self::basis_slice(env, from_basis, amount, from_balance);
        Self::write_basis(env, from, from_basis - moved);
        let to_basis = Self::read_basis(env, to);
        Self::write_basis(env, to, Self::add_or_panic(env, to_basis, moved));

        Self::write_balance(env, from, from_balance - amount);
        let to_balance = Self::read_balance(env, to);
        Self::write_balance(env, to, Self::add_or_panic(env, to_balance, amount));
    }

    /// Burns `amount` YT from `from` and retires the basis that backed it. The
    /// slice rounds UP so the surviving balance is never left over-based.
    ///
    /// That rounding is enough while the holder is at or above the rate their
    /// position was acquired at, where the retired basis covers what recombine
    /// pays out. It is **not** enough for an underwater holder: `recombine` pays
    /// `min(full, pro_rata)`, and the `pro_rata` cap is measured against escrow
    /// and PT supply, not against this holder's basis slice, so the payout can
    /// exceed what is retired here. Measured on a two-holder market whose rate
    /// collapsed below one holder's acquisition rate, escrow fell 25% further
    /// than the ledger did.
    ///
    /// So this does not preserve `escrow >= total_yield_basis +
    /// total_accrued_yield` in general — see the caveats on
    /// [`Self::total_yield_basis`], which document the two paths that break it.
    /// The haircut itself is fair; the aggregate is what stops being an upper
    /// bound.
    fn burn_position(env: &Env, from: &Address, amount: i128) {
        let from_balance = Self::read_balance(env, from);
        if from_balance < amount {
            panic_with_error!(env, Error::InsufficientBalance);
        }

        let from_basis = Self::read_basis(env, from);
        let retired = Self::basis_slice(env, from_basis, amount, from_balance);
        Self::write_basis(env, from, from_basis - retired);

        Self::write_balance(env, from, from_balance - amount);
        let supply = Self::read_total_supply(env);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply - amount));
    }

    /// The basis backing `amount` out of a `balance`-sized position, rounded up
    /// and never more than the whole basis. Exact when the whole position moves,
    /// which keeps a full transfer or a full burn from stranding basis on an
    /// address with no YT left.
    fn basis_slice(env: &Env, basis: i128, amount: i128, balance: i128) -> i128 {
        if basis <= 0 || amount <= 0 || balance <= 0 {
            return 0;
        }
        if amount >= balance {
            return basis;
        }
        match Self::mul_div_ceil(env, basis, amount, balance) {
            Ok(value) if value <= basis => value,
            Ok(_) => basis,
            Err(error) => panic_with_error!(env, error),
        }
    }

    fn read_allowance(env: &Env, from: &Address, spender: &Address) -> AllowanceValue {
        let key = DataKey::Allowance(from.clone(), spender.clone());
        match env.storage().temporary().get::<_, AllowanceValue>(&key) {
            Some(allowance) if allowance.expiration_ledger >= env.ledger().sequence() => allowance,
            _ => AllowanceValue {
                amount: 0,
                expiration_ledger: 0,
            },
        }
    }

    /// Writes the allowance and, when the allowance is live (amount > 0),
    /// extends the temporary entry's own TTL so it survives until
    /// `expiration_ledger`. A freshly created temporary entry only lives for
    /// the network minimum temporary TTL; bumping the instance TTL does not
    /// keep per-entry temporary storage alive, so without this extension the
    /// allowance archives long before the requested expiration.
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
        if amount > 0 {
            // Callers guarantee expiration_ledger >= the current sequence
            // whenever amount > 0 (approve validates it; spend_allowance only
            // reaches here through a live, unexpired allowance).
            let live_for = expiration_ledger - env.ledger().sequence();
            env.storage()
                .temporary()
                .extend_ttl(&key, live_for, live_for);
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

    fn add_or_panic(env: &Env, lhs: i128, rhs: i128) -> i128 {
        match lhs.checked_add(rhs) {
            Some(value) => value,
            None => panic_with_error!(env, Error::MathOverflow),
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test {
    use super::*;
    use sidereal_sy_wrapper::{SyWrapper, SyWrapperClient};
    use sidereal_tokenizer::{Tokenizer, TokenizerClient};
    use soroban_sdk::testutils::{Address as _, Ledger};

    const NOW: u64 = 1_770_000_000;
    const MATURITY: u64 = NOW + 90 * 24 * 60 * 60;
    const RATE_1_00: i128 = WAD;
    const RATE_1_05: i128 = 1_050_000_000_000_000_000;
    const RATE_1_10: i128 = 1_100_000_000_000_000_000;

    struct Fixture {
        env: Env,
        client: YtTokenClient<'static>,
        sy: SyWrapperClient<'static>,
        admin: Address,
        alice: Address,
        bob: Address,
    }

    fn fixture(now: u64) -> Fixture {
        let env = Env::default();
        env.ledger().set_timestamp(now);
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);

        // A real SY wrapper provides the exchange rate the YT yield engine reads.
        let sy_id = env.register(SyWrapper, ());
        let sy = SyWrapperClient::new(&env, &sy_id);
        let underlying = Address::generate(&env);
        sy.initialize(&admin, &underlying);

        // A real tokenizer too: direct holder paths (transfer, burn) resolve
        // their settle rate through the tokenizer's observe_rate and
        // freeze_maturity_rate, so a placeholder address cannot serve. The PT
        // address it records is a placeholder; the rate paths never touch PT.
        let tokenizer_id = env.register(Tokenizer, ());
        let pt_placeholder = Address::generate(&env);

        let contract_id = env.register(YtToken, ());
        let client = YtTokenClient::new(&env, &contract_id);
        client.initialize(&admin, &tokenizer_id, &sy_id, &MATURITY);
        TokenizerClient::new(&env, &tokenizer_id).initialize(
            &admin,
            &sy_id,
            &pt_placeholder,
            &contract_id,
            &MATURITY,
            &admin,
            &0_i128,
        );

        Fixture {
            env,
            client,
            sy,
            admin,
            alice,
            bob,
        }
    }

    #[test]
    fn mint_extends_instance_ttl() {
        use soroban_sdk::testutils::storage::Instance as _;
        let f = fixture(NOW);
        // A mint is a mutating entrypoint, so it must bump the instance TTL to
        // the 120-day window. Read the live TTL from inside the contract frame.
        f.client.mint(&f.alice, &1_000);
        let ttl = f.env.as_contract(&f.client.address, || {
            f.env.storage().instance().get_ttl()
        });
        assert!(
            ttl >= TTL_EXTEND_TO_LEDGERS,
            "instance TTL {} should be extended to at least {}",
            ttl,
            TTL_EXTEND_TO_LEDGERS
        );
    }

    #[test]
    fn mint_bases_fresh_holder_at_the_current_rate() {
        let f = fixture(NOW);
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        f.client.mint(&f.alice, &(100 * WAD));

        // The basis is struck at the rate the YT was minted at, so prior
        // history is not retroactively claimable. In shares that is
        // ceil(face * WAD / rate).
        let expected_basis = ((100 * WAD) * WAD + RATE_1_05 - 1) / RATE_1_05;
        assert_eq!(f.client.yield_basis(&f.alice), expected_basis);
        assert_eq!(f.client.checkpoint(&f.alice), RATE_1_05, "implied rate");
        assert_eq!(f.client.accrued_yield(&f.alice), 0);

        // The aggregates track the per-holder entries exactly.
        assert_eq!(f.client.total_yield_basis(), expected_basis);
        assert_eq!(f.client.total_accrued_yield(), 0);
    }

    #[test]
    fn claim_banks_yield_using_the_telescoping_formula() {
        let f = fixture(NOW);
        // Split at 1.05, not 1.00, to exercise the correct (r-c)/(c*r) form.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        f.client.mint(&f.alice, &(100 * WAD));

        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        // Pre-maturity, the tokenizer would settle at the live SY rate; pass it.
        // `settle` banks and reports the owed total without zeroing the ledger.
        let claimable = f.client.settle(&f.alice, &RATE_1_10);

        // owed = 100 * (1/1.05 - 1/1.10) * WAD = 100 * 0.0432900 = 4.329 SY.
        // A naive (r-c)/WAD form would wrongly bank 100*0.05 = 5.0 SY.
        let expected = (100 * WAD) * (RATE_1_10 - RATE_1_05) / RATE_1_05 * WAD / RATE_1_10;
        assert!((claimable - expected).abs() <= 2, "claimable {}", claimable);
        assert!(
            (claimable - 4_329_004_329_004_329_000).abs() <= 1_000_000,
            "approx 4.329 SY, got {}",
            claimable
        );
        assert_eq!(f.client.checkpoint(&f.alice), RATE_1_10);
        // The ledger still holds the banked total: settle does not consume it.
        assert_eq!(f.client.accrued_yield(&f.alice), claimable);
        assert_eq!(f.client.total_accrued_yield(), claimable);

        // A settle moves shares from basis to the banked ledger and creates
        // none: basis + banked is invariant across the settle.
        let expected_basis = ((100 * WAD) * WAD + RATE_1_05 - 1) / RATE_1_05;
        assert_eq!(
            f.client.yield_basis(&f.alice) + claimable,
            expected_basis,
            "settle conserves shares"
        );
    }

    /// Yield must telescope exactly: settling at every intermediate rate banks
    /// the same total as one settle at the end. The basis makes this exact
    /// rather than approximate, because each intermediate settle subtracts a
    /// value and immediately stores that same value as the new basis.
    #[test]
    fn settling_at_every_step_banks_the_same_as_one_settle_at_the_end() {
        const STEPS: i128 = 25;

        let stepwise = {
            let f = fixture(NOW);
            f.sy.set_exchange_rate(&f.admin, &RATE_1_00);
            f.client.mint(&f.alice, &(100 * WAD));
            for i in 1..=STEPS {
                f.client.settle(&f.alice, &(RATE_1_00 + i * (WAD / 100)));
            }
            f.client.accrued_yield(&f.alice)
        };

        let one_shot = {
            let f = fixture(NOW);
            f.sy.set_exchange_rate(&f.admin, &RATE_1_00);
            f.client.mint(&f.alice, &(100 * WAD));
            f.client.settle(&f.alice, &(RATE_1_00 + STEPS * (WAD / 100)))
        };

        assert_eq!(
            stepwise, one_shot,
            "{} intermediate settles must bank exactly what one settle banks",
            STEPS
        );
    }

    #[test]
    fn consume_removes_only_the_requested_amount() {
        let f = fixture(NOW);
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        f.client.mint(&f.alice, &(100 * WAD));
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);

        let banked = f.client.settle(&f.alice, &RATE_1_10);
        assert!(banked > 0);

        // Consume half; the remainder must stay banked and claimable later.
        let part = banked / 2;
        f.client.consume(&f.alice, &part);
        assert_eq!(f.client.accrued_yield(&f.alice), banked - part);

        // A second settle at the same rate adds nothing (the basis is already
        // struck there), so the remainder is still exactly what was left.
        let still = f.client.settle(&f.alice, &RATE_1_10);
        assert_eq!(still, banked - part);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn consume_more_than_banked_is_rejected() {
        let f = fixture(NOW);
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        f.client.mint(&f.alice, &(100 * WAD));
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        let banked = f.client.settle(&f.alice, &RATE_1_10);
        f.client.consume(&f.alice, &(banked + 1));
    }

    #[test]
    fn first_claim_at_mint_rate_accrues_nothing() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &(100 * WAD)); // minted at rate 1.00
        let claimable = f.client.settle(&f.alice, &RATE_1_00);
        assert_eq!(claimable, 0);
        assert_eq!(f.client.checkpoint(&f.alice), RATE_1_00);
    }

    #[test]
    fn yield_is_conserved_across_a_transfer() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &(100 * WAD)); // at 1.00

        // Rate rises, Alice accrues, then sends half to Bob without claiming.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        f.client.transfer(&f.alice, &f.bob, &(50 * WAD));

        // Alice keeps the yield she earned on 100 over 1.00 -> 1.10. Bob starts
        // fresh at 1.10, so he has nothing yet.
        let alice_pending = f.client.preview_claim_yield(&f.alice);
        let bob_pending = f.client.preview_claim_yield(&f.bob);
        let expected_alice = (100 * WAD) * (RATE_1_10 - RATE_1_00) / RATE_1_00 * WAD / RATE_1_10;
        assert!((alice_pending - expected_alice).abs() <= 2);
        assert_eq!(bob_pending, 0, "Bob earns only from 1.10 forward");

        // Rate rises again; now both earn on their post-transfer balances.
        f.sy.set_exchange_rate(&f.admin, &(RATE_1_10 + WAD / 10));
        let r2 = RATE_1_10 + WAD / 10;
        let alice2 = f.client.preview_claim_yield(&f.alice);
        let bob2 = f.client.preview_claim_yield(&f.bob);

        // Conservation: Alice's total + Bob's total equals the yield a single
        // 100 balance would have earned from 1.00 to r2.
        let single = (100 * WAD) * (r2 - RATE_1_00) / RATE_1_00 * WAD / r2;
        assert!(
            (alice2 + bob2 - single).abs() <= 4,
            "alice {} + bob {} vs single {}",
            alice2,
            bob2,
            single
        );
    }

    /// Audit H4, at the YT layer. A holder settled at a peak transfers their
    /// fully-paid-up YT to a fresh address during a dip. Under the old
    /// per-address rate checkpoint the receiver's checkpoint was initialized to
    /// the dipped rate, so when the rate merely returned to the peak the
    /// receiver could claim the interval the sender had already been paid for.
    /// The basis travels with the YT, so the receiver inherits a paid-up
    /// position and the round trip pays nothing.
    #[test]
    fn transfer_into_a_fresh_address_during_a_dip_re_opens_nothing() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &(100 * WAD)); // basis struck at 1.00

        // Peak: Alice settles at 1.10 and is (conceptually) paid in full.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        let paid = f.client.settle(&f.alice, &RATE_1_10);
        assert!(paid > 0);
        f.client.consume(&f.alice, &paid); // the tokenizer pushes the SY out
        assert_eq!(f.client.accrued_yield(&f.alice), 0);

        // Dip, then the escape hatch: move the whole paid-up position to a
        // brand-new address that has never been settled.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        let fresh = Address::generate(&f.env);
        f.client.transfer(&f.alice, &fresh, &(100 * WAD));

        // Rate merely returns to the peak. No new yield exists, so the fresh
        // address must be owed nothing.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        assert_eq!(
            f.client.settle(&fresh, &RATE_1_10),
            0,
            "the recipient must not re-claim an interval that was already paid"
        );
        assert_eq!(f.client.settle(&f.alice, &RATE_1_10), 0);
        assert_eq!(f.client.total_accrued_yield(), 0);
    }

    /// Audit M4, at the YT layer. An address already settled at a peak acquires
    /// new YT at a lower rate. Under the per-address high-water checkpoint the
    /// new YT inherited the old peak and earned nothing until the rate passed
    /// it, stranding the value in escrow. With an additive basis the new YT
    /// carries its own, so a re-used address earns exactly what a clean one does.
    #[test]
    fn re_used_address_earns_the_same_as_a_clean_one_on_yt_acquired_after_a_dip() {
        let f = fixture(NOW);

        // Alice settles at the 1.10 peak on an earlier position.
        f.client.mint(&f.alice, &(100 * WAD));
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        let paid = f.client.settle(&f.alice, &RATE_1_10);
        f.client.consume(&f.alice, &paid);

        // Dip to 1.05. Alice mints a second position onto the SAME address;
        // Bob mints an identical one onto a clean address.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        f.client.mint(&f.alice, &(100 * WAD));
        f.client.mint(&f.bob, &(100 * WAD));

        // Recovery to 1.10: the second position earned 1.05 -> 1.10 in both
        // cases, and Alice's older, paid-up half adds nothing.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        let alice_owed = f.client.settle(&f.alice, &RATE_1_10);
        let bob_owed = f.client.settle(&f.bob, &RATE_1_10);
        assert!(bob_owed > 0, "the clean address earns on the dip recovery");
        assert_eq!(
            alice_owed, bob_owed,
            "re-using an address must not forfeit yield on YT acquired later"
        );

        // And it is the right number: 100 * (1/1.05 - 1/1.10) SY shares.
        let expected = (100 * WAD) * (RATE_1_10 - RATE_1_05) / RATE_1_05 * WAD / RATE_1_10;
        assert!(
            (bob_owed - expected).abs() <= 2,
            "owed {} vs expected {}",
            bob_owed,
            expected
        );
    }

    /// A self-transfer must change nothing: not the balance, not the basis, not
    /// the aggregates. The settle runs twice on the same holder and the basis
    /// slice leaves and returns to the same address.
    #[test]
    fn self_transfer_is_a_no_op() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &(100 * WAD));
        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        f.client.settle(&f.alice, &RATE_1_10);

        let balance = f.client.balance(&f.alice);
        let basis = f.client.yield_basis(&f.alice);
        let banked = f.client.accrued_yield(&f.alice);
        let supply = f.client.total_supply();

        f.client.transfer(&f.alice, &f.alice, &(40 * WAD));

        assert_eq!(f.client.balance(&f.alice), balance);
        assert_eq!(f.client.yield_basis(&f.alice), basis);
        assert_eq!(f.client.accrued_yield(&f.alice), banked);
        assert_eq!(f.client.total_supply(), supply);
        assert_eq!(f.client.total_yield_basis(), basis);
        assert_eq!(f.client.total_accrued_yield(), banked);
    }

    /// Splitting one position across many addresses must not manufacture yield.
    /// The basis is conserved by every move, so a fan-out and a fan-back-in
    /// leave exactly the original entitlement.
    #[test]
    fn fanning_a_position_out_and_back_conserves_the_entitlement() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &(100 * WAD)); // basis at 1.00

        let hops: std::vec::Vec<Address> =
            (0..5).map(|_| Address::generate(&f.env)).collect();

        // Fan out during a dip, the window the old checkpoint reset in.
        f.sy.set_exchange_rate(&f.admin, &RATE_1_05);
        for hop in &hops {
            f.client.transfer(&f.alice, hop, &(20 * WAD));
        }
        assert_eq!(f.client.balance(&f.alice), 0);

        f.sy.set_exchange_rate(&f.admin, &RATE_1_10);
        let mut total = f.client.settle(&f.alice, &RATE_1_10);
        for hop in &hops {
            total += f.client.settle(hop, &RATE_1_10);
        }

        // One undivided holder over 1.00 -> 1.10.
        let single = (100 * WAD) * (RATE_1_10 - RATE_1_00) / RATE_1_00 * WAD / RATE_1_10;
        assert!(
            total <= single,
            "fanning out must never manufacture yield: {} vs {}",
            total,
            single
        );
        assert!(
            single - total <= 16,
            "fanning out must not lose more than rounding dust: {} vs {}",
            total,
            single
        );
        assert_eq!(f.client.total_accrued_yield(), total);
    }

    #[test]
    fn mint_increases_balance_and_supply() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);
        assert_eq!(f.client.balance(&f.alice), 1_000);
        assert_eq!(f.client.total_supply(), 1_000);
        assert_eq!(f.client.symbol(), String::from_str(&f.env, "sYT"));
    }

    #[test]
    fn transfer_moves_balance() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);
        f.client.transfer(&f.alice, &f.bob, &400);
        assert_eq!(f.client.balance(&f.alice), 600);
        assert_eq!(f.client.balance(&f.bob), 400);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #7)")]
    fn transfer_rejects_insufficient_balance() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &100);
        f.client.transfer(&f.alice, &f.bob, &101);
    }

    #[test]
    fn approve_and_transfer_from_spend_allowance() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);
        f.client
            .approve(&f.alice, &f.bob, &500, &1_000);
        f.client
            .transfer_from(&f.bob, &f.alice, &f.bob, &300);
        assert_eq!(f.client.balance(&f.alice), 700);
        assert_eq!(f.client.balance(&f.bob), 300);
        assert_eq!(f.client.allowance(&f.alice, &f.bob), 200);
    }

    #[test]
    fn allowance_entry_ttl_covers_requested_expiration() {
        use soroban_sdk::testutils::storage::Temporary as _;

        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);

        // Model mainnet-like conditions: the network minimum temporary-entry
        // TTL is far shorter than the requested allowance lifetime.
        const START_SEQ: u32 = 1_000;
        const MIN_TEMP_TTL: u32 = 1_600;
        const EXPIRATION: u32 = START_SEQ + 500_000;
        f.env.ledger().set_sequence_number(START_SEQ);
        f.env.ledger().set_min_temp_entry_ttl(MIN_TEMP_TTL);

        f.client.approve(&f.alice, &f.bob, &500, &EXPIRATION);

        // The test host never archives entries on a sequence jump, so reading
        // the allowance back after a jump would pass even without the fix.
        // Assert the entry's own TTL instead: it must cover the requested
        // expiration ledger, not just the minimum it gets at creation.
        let key = DataKey::Allowance(f.alice.clone(), f.bob.clone());
        let ttl = f.env.as_contract(&f.client.address, || {
            f.env.storage().temporary().get_ttl(&key)
        });
        assert!(
            START_SEQ + ttl >= EXPIRATION,
            "allowance TTL {} from sequence {} must cover expiration {}",
            ttl,
            START_SEQ,
            EXPIRATION
        );

        // Jump well past the minimum temporary TTL but before expiration; the
        // allowance must still be readable and spendable.
        const JUMPED: u32 = START_SEQ + MIN_TEMP_TTL + 100_000;
        f.env.ledger().set_sequence_number(JUMPED);
        assert_eq!(f.client.allowance(&f.alice, &f.bob), 500);
        f.client.transfer_from(&f.bob, &f.alice, &f.bob, &300);
        assert_eq!(f.client.allowance(&f.alice, &f.bob), 200);
        assert_eq!(f.client.balance(&f.bob), 300);

        // The reduced allowance written back by transfer_from must also keep
        // a TTL covering the stored expiration ledger.
        let ttl = f.env.as_contract(&f.client.address, || {
            f.env.storage().temporary().get_ttl(&key)
        });
        assert!(
            JUMPED + ttl >= EXPIRATION,
            "post-spend allowance TTL {} from sequence {} must cover expiration {}",
            ttl,
            JUMPED,
            EXPIRATION
        );
    }

    #[test]
    fn burn_reduces_balance_and_supply() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);
        f.client.burn(&f.alice, &400);
        assert_eq!(f.client.balance(&f.alice), 600);
        assert_eq!(f.client.total_supply(), 600);
    }

    /// burn_settled trusts its rate argument, so it must be callable by the
    /// tokenizer alone. With auth enforcement on and no tokenizer signature,
    /// the call must fail rather than let an arbitrary caller settle a holder
    /// at a rate of their choosing and burn their balance.
    #[test]
    fn burn_settled_is_gated_on_the_tokenizer() {
        let f = fixture(NOW);
        f.client.mint(&f.alice, &1_000);
        // Switch from mock-all to enforcing mode with no authorizations.
        f.env.set_auths(&[]);
        let result = f.client.try_burn_settled(&f.alice, &400, &WAD);
        assert!(
            result.is_err(),
            "burn_settled without the tokenizer's authorization must fail"
        );
        f.env.mock_all_auths();
        assert_eq!(f.client.balance(&f.alice), 1_000, "nothing was burned");
    }
}
