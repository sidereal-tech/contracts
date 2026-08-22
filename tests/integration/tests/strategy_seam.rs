// SPDX-License-Identifier: Apache-2.0

//! Strategy-seam coverage: the conformance battery, the vault's defenses, and
//! V1/V2 equivalence over identical Blend state.
//!
//! The battery in [`conformance`] runs against *every* `YieldStrategy`
//! implementation. A third adapter is gated by the same battery that gated the
//! second — that is the point of having a seam at all.

mod mocks;

use mocks::{MockBlendPool, MockBlendPoolClient, MockStrategy, MockStrategyClient, UNIT, WAD};
use sidereal_blend_adapter::BLEND_SCALAR_12;
use sidereal_pt_token::{PtToken, PtTokenClient};
use sidereal_strategy_blend::{StrategyBlend, StrategyBlendClient};
use sidereal_strategy_interface::YieldStrategyClient;
use sidereal_sy_vault_v2::{Error as VaultError, SyVaultV2, SyVaultV2Client, MINIMUM_SHARES};
use sidereal_sy_wrapper::{SyWrapper, SyWrapperClient};
use sidereal_tokenizer::{Tokenizer, TokenizerClient};
use sidereal_yt_token::{YtToken, YtTokenClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env, Error as HostError, MuxedAddress,
};

const MATURITY: u64 = 1_000_000;

/// A donation, performed exactly the way an attacker performs one: a plain SEP-41
/// `transfer` to the strategy's address, from an address the market has never
/// seen. No auth from the strategy, no call into it, no vault interaction. This
/// is the whole of the C2 attack setup, and nothing about it can be prevented —
/// which is why the valuation, not the transfer, has to be the thing that
/// refuses it.
fn donate(env: &Env, underlying: &Address, to: &Address, amount: i128) -> Address {
    let donor = Address::generate(env);
    token::StellarAssetClient::new(env, underlying).mint(&donor, &amount);
    donate_from(env, underlying, &donor, to, amount);
    donor
}

/// The same donation, out of a named address's own funds, so a test can measure
/// what the donor gets back against what the donation cost them.
fn donate_from(env: &Env, underlying: &Address, from: &Address, to: &Address, amount: i128) {
    token::TokenClient::new(env, underlying).transfer(from, &MuxedAddress::from(to), &amount);
}

fn funded(env: &Env, underlying: &Address, amount: i128) -> Address {
    let who = Address::generate(env);
    token::StellarAssetClient::new(env, underlying).mint(&who, &amount);
    who
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct BlendMarket {
    admin: Address,
    user: Address,
    underlying: Address,
    pool: MockBlendPoolClient<'static>,
    strategy: StrategyBlendClient<'static>,
    vault: SyVaultV2Client<'static>,
}

/// Deploys a full V2 market: token, mock Blend pool, `strategy-blend`, and
/// `sy-vault-v2` wired in the production order — strategy first (naming the
/// vault's address), then the vault, whose `initialize` closes the loop.
fn blend_market(env: &Env) -> BlendMarket {
    let admin = Address::generate(env);
    let user = Address::generate(env);

    let underlying = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(env, &underlying).mint(&user, &(10_000 * UNIT));

    let pool_id = env.register(MockBlendPool, ());
    let pool = MockBlendPoolClient::new(env, &pool_id);
    pool.initialize(&underlying);

    // Both contracts exist before either is initialized, so each can name the
    // other. The vault's init then verifies the strategy points back at it.
    let vault_id = env.register(SyVaultV2, ());
    let strategy_id = env.register(StrategyBlend, ());

    let strategy = StrategyBlendClient::new(env, &strategy_id);
    strategy.initialize(&admin, &vault_id, &underlying, &pool_id);

    let vault = SyVaultV2Client::new(env, &vault_id);
    vault.initialize(&admin, &strategy_id);

    BlendMarket {
        admin,
        user,
        underlying,
        pool,
        strategy,
        vault,
    }
}

struct MockMarket {
    admin: Address,
    user: Address,
    underlying: Address,
    sink: Address,
    strategy: MockStrategyClient<'static>,
    vault: SyVaultV2Client<'static>,
}

/// The same wiring, but behind the hostile [`MockStrategy`], so the vault's own
/// defenses can be driven independently of Blend's good behaviour.
fn mock_market(env: &Env) -> MockMarket {
    let admin = Address::generate(env);
    let user = Address::generate(env);
    let sink = Address::generate(env);

    let underlying = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(env, &underlying).mint(&user, &(10_000 * UNIT));

    let vault_id = env.register(SyVaultV2, ());
    let strategy_id = env.register(MockStrategy, ());

    let strategy = MockStrategyClient::new(env, &strategy_id);
    strategy.initialize(&vault_id, &underlying, &sink);

    let vault = SyVaultV2Client::new(env, &vault_id);
    vault.initialize(&admin, &strategy_id);

    MockMarket {
        admin,
        user,
        underlying,
        sink,
        strategy,
        vault,
    }
}

// ---------------------------------------------------------------------------
// Conformance battery — runs against every YieldStrategy implementation
// ---------------------------------------------------------------------------

mod conformance {
    use super::*;

    /// Every strategy must agree with its vault about who it serves and what it
    /// consumes. A market whose strategy names a different vault is
    /// unconstructable, not merely misconfigured.
    fn asserts_binding(env: &Env, strategy: &Address, vault: &Address, underlying: &Address) {
        let client = YieldStrategyClient::new(env, strategy);
        assert_eq!(&client.vault(), vault, "strategy must name its vault");
        assert_eq!(
            &client.underlying(),
            underlying,
            "strategy must name its underlying"
        );
    }

    /// `max_withdraw` is a separate question from `total_assets`, and never the
    /// larger of the two. A strategy that collapsed them would let its market
    /// quote a redemption it cannot honour.
    fn asserts_liquidity_bound(env: &Env, strategy: &Address) {
        let client = YieldStrategyClient::new(env, strategy);
        assert!(
            client.max_withdraw() <= client.total_assets(),
            "max_withdraw must never exceed total_assets"
        );
    }

    /// `deposit` and `withdraw` accept calls only from the vault fixed at init.
    /// Note this holds even under `mock_all_auths` — the address check is
    /// independent of the signature check, which is what makes it a real gate
    /// rather than an auth-framework artifact.
    fn asserts_vault_only(env: &Env, strategy: &Address) {
        let client = YieldStrategyClient::new(env, strategy);
        let stranger = Address::generate(env);
        assert!(
            client.try_deposit(&stranger, &UNIT).is_err(),
            "deposit must reject a non-vault caller"
        );
        assert!(
            client.try_withdraw(&stranger, &UNIT, &0).is_err(),
            "withdraw must reject a non-vault caller"
        );
    }

    /// `touch` is permissionless — an idle market must be renewable by anyone,
    /// not only by a privileged keeper.
    fn asserts_touch_is_permissionless(env: &Env, strategy: &Address) {
        YieldStrategyClient::new(env, strategy).touch();
    }

    fn run(env: &Env, strategy: &Address, vault: &Address, underlying: &Address) {
        asserts_binding(env, strategy, vault, underlying);
        asserts_liquidity_bound(env, strategy);
        asserts_vault_only(env, strategy);
        asserts_touch_is_permissionless(env, strategy);
    }

    #[test]
    fn strategy_blend_satisfies_the_battery() {
        let env = Env::default();
        env.mock_all_auths();
        let market = blend_market(&env);
        market.vault.deposit(&market.user, &(100 * UNIT), &0);
        run(
            &env,
            &market.strategy.address,
            &market.vault.address,
            &market.underlying,
        );
    }

    #[test]
    fn mock_strategy_satisfies_the_battery() {
        let env = Env::default();
        env.mock_all_auths();
        let market = mock_market(&env);
        market.vault.deposit(&market.user, &(100 * UNIT), &0);
        run(
            &env,
            &market.strategy.address,
            &market.vault.address,
            &market.underlying,
        );
    }

    #[test]
    fn deposit_returns_measured_delta_not_the_requested_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let market = mock_market(&env);

        // An upstream that keeps back 3 units of every deposit.
        market.strategy.set_shave(&3);

        let before = market.strategy.total_assets();
        let credited = market.strategy_deposit_via_vault(&env, 100 * UNIT);
        let after = market.strategy.total_assets();

        assert_eq!(
            credited,
            after - before,
            "deposit must report the measured change in assets"
        );
        assert_eq!(credited, 100 * UNIT - 3, "the shaved units must not be credited");
    }

    #[test]
    fn withdraw_enforces_min_underlying_out() {
        let env = Env::default();
        env.mock_all_auths();
        let market = mock_market(&env);
        market.vault.deposit(&market.user, &(100 * UNIT), &0);

        // Only 40 is presently realizable.
        market.strategy.set_liquidity_cap(&(40 * UNIT));

        let client = YieldStrategyClient::new(&env, &market.strategy.address);
        assert!(
            client
                .try_withdraw(&market.vault.address, &(100 * UNIT), &(90 * UNIT))
                .is_err(),
            "a short delivery below min_underlying_out must revert"
        );
        // The same call with an honest floor succeeds.
        assert_eq!(
            client.withdraw(&market.vault.address, &(100 * UNIT), &(40 * UNIT)),
            40 * UNIT
        );
    }

    #[test]
    fn max_withdraw_tracks_realizable_liquidity_not_nominal_assets() {
        let env = Env::default();
        env.mock_all_auths();
        let market = mock_market(&env);
        market.vault.deposit(&market.user, &(100 * UNIT), &0);

        assert_eq!(market.strategy.max_withdraw(), 100 * UNIT);
        market.strategy.set_liquidity_cap(&(25 * UNIT));
        assert_eq!(market.strategy.max_withdraw(), 25 * UNIT);
        assert_eq!(
            market.strategy.total_assets(),
            100 * UNIT,
            "valuation is unchanged by a liquidity squeeze"
        );
    }

    /// A Blend pool can be nominally solvent for this strategy while holding
    /// less cash than the position is worth. `max_withdraw` must report the
    /// cash, not the position.
    #[test]
    fn blend_max_withdraw_is_bounded_by_pool_cash() {
        let env = Env::default();
        env.mock_all_auths();
        let market = blend_market(&env);
        market.vault.deposit(&market.user, &(100 * UNIT), &0);

        assert_eq!(market.strategy.total_assets(), 100 * UNIT);
        assert_eq!(market.strategy.max_withdraw(), 100 * UNIT);

        let borrower = Address::generate(&env);
        market.pool.drain(&borrower, &(70 * UNIT));

        assert_eq!(
            market.strategy.total_assets(),
            100 * UNIT,
            "the supplied position is still worth 100"
        );
        assert_eq!(
            market.strategy.max_withdraw(),
            30 * UNIT,
            "but only 30 can presently be paid out"
        );
    }

    #[test]
    fn strategy_binding_is_immutable() {
        let env = Env::default();
        env.mock_all_auths();
        let market = blend_market(&env);

        let other_vault = Address::generate(&env);
        assert!(
            market
                .strategy
                .try_initialize(&market.admin, &other_vault, &market.underlying, &market.pool.address)
                .is_err(),
            "a strategy cannot be re-initialized onto another vault"
        );
        assert!(
            market
                .vault
                .try_initialize(&market.admin, &market.strategy.address)
                .is_err(),
            "a vault cannot be re-initialized onto another strategy"
        );
    }
}

impl MockMarket {
    /// Drives a deposit through the vault and returns what the strategy
    /// credited, read back from the emitted accounting.
    fn strategy_deposit_via_vault(&self, env: &Env, amount: i128) -> i128 {
        let before = self.strategy.total_assets();
        self.vault.deposit(&self.user, &amount, &0);
        let _ = env;
        self.strategy.total_assets() - before
    }
}

// ---------------------------------------------------------------------------
// Vault behaviour
// ---------------------------------------------------------------------------

#[test]
fn exchange_rate_is_derived_from_the_strategy_and_has_no_setter() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    assert_eq!(market.vault.exchange_rate(), WAD, "bootstraps at 1.0");
    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert_eq!(market.vault.exchange_rate(), WAD);

    // Interest accrues upstream; the rate follows without anyone setting it.
    market.pool.set_b_rate(&1_250_000_000_000);
    assert_eq!(market.vault.exchange_rate(), 1_250_000_000_000_000_000);
    assert_eq!(market.vault.total_assets(), 125 * UNIT);
}

#[test]
fn deposit_mints_against_credited_assets_not_the_requested_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    market.strategy.set_shave(&(1 * UNIT));
    let minted = market.vault.deposit(&market.user, &(100 * UNIT), &0);

    assert_eq!(
        minted,
        99 * UNIT - MINIMUM_SHARES,
        "SY is minted against what the strategy actually credited, less the \
         opening lock"
    );
    assert_eq!(market.vault.total_supply(), 99 * UNIT);
    // The vault is not left short: 99 SY at rate 1.0 is backed by 99 assets.
    assert_eq!(market.vault.total_assets(), 99 * UNIT);
    assert_eq!(market.vault.exchange_rate(), WAD);
}

#[test]
fn deposit_enforces_min_sy_out() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);
    market.strategy.set_shave(&(5 * UNIT));

    assert!(
        market
            .vault
            .try_deposit(&market.user, &(100 * UNIT), &(100 * UNIT))
            .is_err(),
        "a deposit that would mint below min_sy_out must revert"
    );
    assert_eq!(market.vault.total_supply(), 0, "nothing was minted");
}

#[test]
fn redeem_verifies_delivery_and_rejects_a_lying_strategy() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);
    market.vault.deposit(&market.user, &(100 * UNIT), &0);

    // The strategy reports 10 more than it actually transfers.
    market.strategy.set_report_inflation(&(10 * UNIT));

    let held = market.vault.balance(&market.user);
    let result = market.vault.try_redeem(&market.user, &(50 * UNIT), &0);
    assert!(
        result.is_err(),
        "the vault must trust its own measurement over the strategy's report"
    );
    // The burn is rolled back with the transaction.
    assert_eq!(market.vault.balance(&market.user), held);
}

#[test]
fn redeem_enforces_min_underlying_out() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);
    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    market.strategy.set_liquidity_cap(&(30 * UNIT));

    assert!(
        market
            .vault
            .try_redeem(&market.user, &(50 * UNIT), &(50 * UNIT))
            .is_err(),
        "a short delivery below the caller's floor must revert rather than settle"
    );
    assert_eq!(market.vault.balance(&market.user), held, "no SY was burned");
}

#[test]
fn redeem_round_trips_and_returns_the_underlying() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let token_client = token::TokenClient::new(&env, &market.underlying);

    let start = token_client.balance(&market.user);
    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert_eq!(token_client.balance(&market.user), start - 100 * UNIT);

    market.pool.set_b_rate(&1_100_000_000_000);
    let out = market.vault.redeem(&market.user, &(50 * UNIT), &(54 * UNIT));

    assert_eq!(out, 55 * UNIT, "50 SY at rate 1.10");
    assert_eq!(token_client.balance(&market.user), start - 100 * UNIT + 55 * UNIT);
    assert_eq!(market.vault.balance(&market.user), held - 50 * UNIT);
}

#[test]
fn vault_rejects_a_strategy_bound_to_another_vault() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let elsewhere = Address::generate(&env);

    let underlying = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool_id = env.register(MockBlendPool, ());
    MockBlendPoolClient::new(&env, &pool_id).initialize(&underlying);

    let strategy_id = env.register(StrategyBlend, ());
    StrategyBlendClient::new(&env, &strategy_id)
        .initialize(&admin, &elsewhere, &underlying, &pool_id);

    let vault_id = env.register(SyVaultV2, ());
    let result = SyVaultV2Client::new(&env, &vault_id).try_initialize(&admin, &strategy_id);

    assert!(
        result.is_err(),
        "a mis-wired market must be unconstructable, not merely wrong at runtime"
    );
}

#[test]
fn accrued_yield_tracks_the_rate_and_survives_transfer() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let other = Address::generate(&env);

    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert_eq!(market.vault.accrued_yield(&market.user), 0);

    market.pool.set_b_rate(&1_200_000_000_000);
    // 20% on the shares actually held. The locked opening shares accrue to
    // nobody, which is the whole point of locking them, so the user's yield is
    // measured against `held` rather than against the 100 they paid in.
    let total_accrued = market.vault.accrued_yield(&market.user);
    assert_eq!(total_accrued, held * 2 / 10);

    // Moving half the shares moves half the principal, so neither side's
    // accrued yield is distorted by the transfer.
    market.vault.transfer(&market.user, &other, &(held / 2));
    let user_accrued = market.vault.accrued_yield(&market.user);
    let other_accrued = market.vault.accrued_yield(&other);
    assert_eq!(
        user_accrued + other_accrued,
        total_accrued,
        "a transfer must not create or destroy accrued yield"
    );
    assert!(
        (user_accrued - other_accrued).abs() <= 1,
        "half the shares carry half the yield: {user_accrued} vs {other_accrued}"
    );
}

// ---------------------------------------------------------------------------
// Donation and first-depositor inflation
//
// `MULTI_STRATEGY.md:279` names these as required and they did not exist. The
// vector they cover: `total_assets` counted the strategy's raw token balance, so
// any address could raise the exchange rate with a plain SAC transfer — no auth,
// no vault call — and the vault's `shares <= 0` guard does not fire on a victim
// deposit that rounds down to 1 rather than to 0.
// ---------------------------------------------------------------------------

/// The headline: a donation is not income. It must not move the rate, the
/// valuation, or what the next depositor receives.
#[test]
fn a_direct_token_donation_cannot_move_the_exchange_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    let rate_before = market.vault.exchange_rate();
    let assets_before = market.vault.total_assets();
    let quote_before = market.vault.preview_deposit(&(10 * UNIT));

    donate(&env, &market.underlying, &market.strategy.address, 900 * UNIT);

    assert_eq!(
        market.vault.exchange_rate(),
        rate_before,
        "a 9x donation must not move the rate by one stroop"
    );
    assert_eq!(
        market.vault.total_assets(),
        assets_before,
        "only the supplied position is valued"
    );
    assert_eq!(
        market.vault.preview_deposit(&(10 * UNIT)),
        quote_before,
        "and the next depositor is quoted exactly what they were before"
    );

    // The donated underlying really is sitting at the strategy's address — the
    // transfer succeeded, it is simply not counted.
    assert_eq!(
        token::TokenClient::new(&env, &market.underlying).balance(&market.strategy.address),
        900 * UNIT
    );
}

/// C2's second half: the vector reached *through* the vault into the tokenizer.
/// `pt_face_reservation` is `ceil(pt_supply * WAD / rate)`, so a higher rate
/// shrinks the reservation and releases escrowed SY to YT claimants — a YT
/// holder could donate, claim the released escrow, redeem it for the donation
/// back, and leave PT under-covered for the price of gas. With the rate immune
/// to a donation, the reservation is too.
#[test]
fn a_donation_cannot_release_the_tokenizers_pt_reservation() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(0);

    let market = blend_market(&env);
    let sy = market.vault.address.clone();

    let pt = PtTokenClient::new(&env, &env.register(PtToken, ()));
    let yt = YtTokenClient::new(&env, &env.register(YtToken, ()));
    let tokenizer = TokenizerClient::new(&env, &env.register(Tokenizer, ()));
    pt.initialize(&market.admin, &tokenizer.address, &sy, &MATURITY);
    yt.initialize(&market.admin, &tokenizer.address, &sy, &MATURITY);
    tokenizer.initialize(&market.admin, &sy, &pt.address, &yt.address, &MATURITY, &market.admin, &0_i128);

    let sy_held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    tokenizer.split(&market.user, &sy_held);

    // Real yield first, so there is a genuine claim to compare against.
    market.pool.set_b_rate(&1_100_000_000_000);
    let rate_before = market.vault.exchange_rate();
    let claimable_before = yt.preview_claim_yield(&market.user);
    assert!(claimable_before > 0);

    // Nine times the market's TVL, sent straight to the strategy.
    donate(&env, &market.underlying, &market.strategy.address, 900 * UNIT);

    assert_eq!(market.vault.exchange_rate(), rate_before, "the rate holds");
    assert_eq!(
        yt.preview_claim_yield(&market.user),
        claimable_before,
        "so the PT reservation holds, and no escrow is released to YT"
    );
    assert_eq!(
        tokenizer.claim_yield(&market.user),
        claimable_before,
        "and the settled claim is the honest one, not an inflated one"
    );
}

/// The audit's worked exploit, run end to end. Under the old valuation the
/// attacker turned a 500.00 outlay into +249.99 while the victim recovered a
/// quarter of their deposit. Both halves must now fail.
#[test]
fn the_first_depositor_inflation_exploit_loses_money() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let coin = token::TokenClient::new(&env, &market.underlying);

    let attacker = funded(&env, &market.underlying, 10_000 * UNIT);
    let victim = funded(&env, &market.underlying, 10_000 * UNIT);
    let attacker_start = coin.balance(&attacker);
    let victim_start = coin.balance(&victim);

    // 1. Open the market as small as the minimum lock allows.
    let attacker_shares = market.vault.deposit(&attacker, &1_001, &0);
    assert_eq!(attacker_shares, 1, "1001 in, 1000 locked, 1 share out");

    // 2. Donate 500.00 of their own money straight to the strategy.
    donate_from(
        &env,
        &market.underlying,
        &attacker,
        &market.strategy.address,
        4_999_999_999,
    );
    assert_eq!(
        market.vault.exchange_rate(),
        WAD,
        "the rate is still 1.0 — this is the step the exploit depended on"
    );

    // 3. The victim deposits 1000.00 and receives full value, not 1 stroop.
    let victim_shares = market.vault.deposit(&victim, &9_999_999_999, &0);
    assert_eq!(
        victim_shares, 9_999_999_999,
        "the victim's shares are priced at 1.0, not at a poisoned 5e27"
    );

    // 4. The victim exits whole. Their redemption is what consumes the donated
    //    idle, so the donation leaves as underlying rather than sitting stranded.
    let victim_out = market.vault.redeem(&victim, &victim_shares, &0);
    assert_eq!(victim_out, 9_999_999_999);
    assert_eq!(
        coin.balance(&victim),
        victim_start,
        "the victim is exactly whole"
    );

    // 5. The attacker exits. The donation is gone: what it bought was a higher
    //    rate on a supply the attacker owns 1/1001 of, because MINIMUM_SHARES
    //    holds the rest and nobody can redeem those.
    market.vault.redeem(&attacker, &attacker_shares, &0);
    let attacker_net = coin.balance(&attacker) - attacker_start;
    assert!(attacker_net < 0, "the attack must not pay: {attacker_net}");
    assert!(
        attacker_net < -(4_900_000_000_i128),
        "the attacker forfeits essentially the whole donation: {attacker_net}"
    );
}

/// Defense in depth: the same attack behind a strategy that *does* count its raw
/// balance — the shape the seam forbids, and the shape a future adapter might
/// get wrong. The vault's own minimum-share lock has to carry it alone here, and
/// it does: the attacker cannot own the whole supply, so their recovery is
/// diluted by `MINIMUM_SHARES / total_supply` and the donation is a dead loss.
#[test]
fn minimum_shares_defeat_inflation_even_behind_a_donation_counting_strategy() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);
    let coin = token::TokenClient::new(&env, &market.underlying);

    let attacker = funded(&env, &market.underlying, 10_000 * UNIT);
    let victim = funded(&env, &market.underlying, 10_000 * UNIT);
    let attacker_start = coin.balance(&attacker);

    let attacker_shares = market.vault.deposit(&attacker, &1_001, &0);
    assert_eq!(attacker_shares, 1);

    donate_from(
        &env,
        &market.underlying,
        &attacker,
        &market.strategy.address,
        4_999_999_999,
    );
    // This strategy really is inflatable — that is the premise of the test.
    assert!(
        market.vault.exchange_rate() > WAD * 1_000_000,
        "the hostile strategy counts the donation"
    );

    let victim_shares = market.vault.deposit(&victim, &9_999_999_999, &0);
    let victim_value = market.vault.preview_redeem(&victim_shares);

    market.vault.redeem(&attacker, &attacker_shares, &0);
    let attacker_net = coin.balance(&attacker) - attacker_start;

    assert!(
        attacker_net < 0,
        "the attacker must not profit even against an inflatable strategy: {attacker_net}"
    );
    assert!(
        victim_value > 9_000_000_000,
        "and the victim keeps the bulk of their deposit: {victim_value}"
    );
}

// ---------------------------------------------------------------------------
// The minimum share lock
// ---------------------------------------------------------------------------

#[test]
fn the_first_deposit_locks_a_minimum_share_balance_forever() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    assert_eq!(
        market.vault.preview_deposit(&(100 * UNIT)),
        100 * UNIT - MINIMUM_SHARES,
        "the opening quote is what the depositor receives, not what is minted"
    );
    let opening = market.vault.deposit(&market.user, &(100 * UNIT), &0);

    assert_eq!(opening, 100 * UNIT - MINIMUM_SHARES);
    assert_eq!(
        market.vault.total_supply(),
        100 * UNIT,
        "the locked shares are minted, so the rate is unaffected"
    );
    assert_eq!(
        market.vault.total_supply() - market.vault.balance(&market.user),
        MINIMUM_SHARES,
        "and they belong to no address at all"
    );
    assert_eq!(market.vault.exchange_rate(), WAD);

    // Only the opening deposit pays for the lock.
    assert_eq!(market.vault.preview_deposit(&(50 * UNIT)), 50 * UNIT);
    assert_eq!(market.vault.deposit(&market.user, &(50 * UNIT), &0), 50 * UNIT);
}

#[test]
fn a_first_deposit_too_small_to_fund_the_lock_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    for amount in [1_i128, MINIMUM_SHARES - 1, MINIMUM_SHARES] {
        assert_eq!(
            market.vault.try_deposit(&market.user, &amount, &0),
            Err(Ok(contract_error(VaultError::InitialDepositTooSmall))),
            "an opening deposit of {amount} cannot fund the lock"
        );
    }
    assert_eq!(market.vault.total_supply(), 0, "and nothing was minted");

    // One stroop over the line opens the market.
    assert_eq!(
        market.vault.deposit(&market.user, &(MINIMUM_SHARES + 1), &0),
        1
    );
    assert_eq!(market.vault.total_supply(), MINIMUM_SHARES + 1);
}

/// A preview that succeeds where the call reverts is worse than no preview: a
/// caller gating on `preview >= min` reads a number, signs, and the transaction
/// cannot land. Pin preview against deposit at every boundary, on both an empty
/// and an open market — the empty branch was fixed first and the open branch
/// carried the identical defect.
#[test]
fn preview_deposit_refuses_exactly_where_deposit_refuses() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    // Empty market: anything that cannot fund the lock must fail, not quote 0.
    for amount in [1_i128, MINIMUM_SHARES - 1, MINIMUM_SHARES] {
        assert_eq!(
            market.vault.try_preview_deposit(&amount),
            Err(Ok(VaultError::InitialDepositTooSmall)),
            "preview_deposit({amount}) on an empty market must refuse like deposit"
        );
    }
    // Non-positive amounts are rejected before the share math, with the same
    // error deposit gives — not InitialDepositTooSmall, and never a negative
    // quote.
    for amount in [0_i128, -5] {
        assert_eq!(
            market.vault.try_preview_deposit(&amount),
            Err(Ok(VaultError::InvalidAmount)),
            "preview_deposit({amount}) must refuse as InvalidAmount"
        );
        assert_eq!(
            market.vault.try_deposit(&market.user, &amount, &0),
            Err(Ok(contract_error(VaultError::InvalidAmount))),
            "and deposit({amount}) agrees"
        );
    }

    // preview_redeem had the identical defect one function below, untouched by
    // the first fix: `Ok(-5)` underlying for a call that reverts.
    for amount in [0_i128, -5] {
        assert_eq!(
            market.vault.try_preview_redeem(&amount),
            Err(Ok(VaultError::InvalidAmount)),
            "preview_redeem({amount}) must refuse like redeem"
        );
    }

    // Open the market, then check the branch that was left alone.
    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert!(market.vault.preview_deposit(&(10 * UNIT)) > 0);
    for amount in [0_i128, -5] {
        assert_eq!(
            market.vault.try_preview_deposit(&amount),
            Err(Ok(VaultError::InvalidAmount)),
            "preview_deposit({amount}) on an open market must refuse too"
        );
    }

    // A market at its cap rejects every deposit, and the preview can prove it
    // without predicting the strategy's credit. Quoting a number here was the
    // remaining "succeeds where the call reverts" case.
    let assets = market.vault.total_assets();
    market.vault.set_deposit_cap(&market.admin, &assets);
    assert_eq!(
        market.vault.try_preview_deposit(&(10 * UNIT)),
        Err(Ok(VaultError::DepositCapExceeded)),
        "a capped market must not quote a deposit it will reject"
    );
    assert_eq!(
        market.vault.try_deposit(&market.user, &(10 * UNIT), &0),
        Err(Ok(contract_error(VaultError::DepositCapExceeded))),
        "and deposit agrees"
    );
}

/// The other half of C2: a market whose supply returns to zero with assets still
/// in it used to re-bootstrap at `WAD` and hand the residual to the next
/// one-stroop depositor. The supply can no longer reach zero, so there is no
/// re-bootstrap to exploit.
#[test]
fn the_supply_never_returns_to_zero_so_residual_assets_are_never_regifted() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    market.vault.redeem(&market.user, &held, &0);

    assert_eq!(market.vault.balance(&market.user), 0, "the holder is out");
    assert_eq!(
        market.vault.total_supply(),
        MINIMUM_SHARES,
        "but the supply floors at the lock rather than emptying"
    );
    assert_eq!(
        market.vault.total_assets(),
        MINIMUM_SHARES,
        "and the residual position is exactly what the locked shares back"
    );
    assert_eq!(market.vault.exchange_rate(), WAD);

    // The next depositor buys at the going rate. Under the old bootstrap branch
    // this one stroop would have claimed the entire residual position.
    let latecomer = funded(&env, &market.underlying, 10 * UNIT);
    assert_eq!(market.vault.deposit(&latecomer, &1, &0), 1);
    assert_eq!(market.vault.preview_redeem(&1), 1);
    assert_eq!(market.vault.total_supply(), MINIMUM_SHARES + 1);
}

/// Assets standing behind no shares at all is a broken vault, not a bootstrap.
/// Reaching it requires a strategy that reports a position the vault never
/// minted against — so the vault fails closed rather than pricing at `WAD` and
/// selling the position to whoever deposits first.
#[test]
fn a_vault_with_assets_and_no_shares_fails_closed() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    assert_eq!(
        market.vault.exchange_rate(),
        WAD,
        "an empty market bootstraps at 1.0"
    );

    donate(&env, &market.underlying, &market.strategy.address, 50 * UNIT);

    assert!(
        market.vault.try_exchange_rate().is_err(),
        "assets with no supply have no asset-per-share, and WAD is not an answer"
    );
    assert!(market.vault.try_deposit(&market.user, &(10 * UNIT), &0).is_err());

    // A strategy that honours the seam's valuation rule is unaffected by the
    // same donation, which is the point: this state is only reachable by
    // breaking the rule.
    market.strategy.set_exclude_idle(&true);
    assert_eq!(market.vault.exchange_rate(), WAD);
    assert_eq!(
        market.vault.deposit(&market.user, &(10 * UNIT), &0),
        10 * UNIT - MINIMUM_SHARES
    );
}

// ---------------------------------------------------------------------------
// Idle underlying is payable (H5) and a short delivery burns proportionally (H6)
// ---------------------------------------------------------------------------

/// `max_withdraw` counted idle underlying that `withdraw` could never deliver:
/// it only ever submitted to Blend and measured the delta around that call, so
/// pre-existing idle was unreachable from every entrypoint. A strategy holding
/// idle against a drained pool reported liquidity and reverted on every attempt.
#[test]
fn withdraw_pays_from_idle_before_it_touches_the_pool() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let coin = token::TokenClient::new(&env, &market.underlying);

    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    donate(&env, &market.underlying, &market.strategy.address, 20 * UNIT);

    // Drain the pool completely: nothing can come out of Blend at all.
    let borrower = Address::generate(&env);
    market.pool.drain(&borrower, &(100 * UNIT));

    assert_eq!(
        market.vault.total_assets(),
        100 * UNIT,
        "the donation is still not valued"
    );
    assert_eq!(
        market.vault.max_withdraw(),
        20 * UNIT,
        "but it is realizable, and that is now the honest report"
    );

    let start = coin.balance(&market.user);
    let out = market.vault.redeem(&market.user, &(20 * UNIT), &(20 * UNIT));
    assert_eq!(out, 20 * UNIT, "and withdraw actually delivers it");
    assert_eq!(coin.balance(&market.user), start + 20 * UNIT);
    assert_eq!(market.vault.total_assets(), 80 * UNIT);
    assert_eq!(
        market.vault.exchange_rate(),
        WAD,
        "spending donated idle must not raise the rate"
    );
    market.pool.set_b_rate(&1_100_000_000_000);
    assert_eq!(
        market.vault.exchange_rate(),
        1_100_000_000_000_000_000,
        "the excluded position units must keep their future interest out too"
    );
    assert_eq!(
        coin.balance(&market.strategy.address),
        0,
        "the idle balance is spent, not stranded"
    );
}

/// Idle and pool cash compose: `max_withdraw` must equal what `withdraw`
/// actually delivers when the request spans both.
#[test]
fn max_withdraw_is_exactly_what_a_mixed_idle_and_pool_withdrawal_delivers() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    donate(&env, &market.underlying, &market.strategy.address, 20 * UNIT);

    let borrower = Address::generate(&env);
    market.pool.drain(&borrower, &(90 * UNIT));

    // 20 idle + 10 of pool cash, and the position is worth 100, so the cap does
    // not bind.
    assert_eq!(market.vault.max_withdraw(), 30 * UNIT);

    let quoted = market.vault.max_withdraw();
    let out = market.vault.redeem(&market.user, &(30 * UNIT), &0);
    assert_eq!(out, quoted, "the quote is the delivery");
    assert_eq!(market.vault.total_assets(), 70 * UNIT);
    assert_eq!(
        market.vault.exchange_rate(),
        WAD,
        "mixed idle and pool liquidity must reduce backing with supply"
    );
}

/// V1 recomputed the burn from what actually arrived; V2 had regressed to
/// burning the whole request. With the SDK's default `minUnderlyingOut = 0n` a
/// user redeeming 60 SY against a 50-realizable position burned 60 for 50 and
/// gifted the difference to everyone who stayed.
#[test]
fn a_short_delivery_at_min_zero_burns_only_the_sy_it_paid_for() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    market.strategy.set_liquidity_cap(&(50 * UNIT));

    let rate_before = market.vault.exchange_rate();
    // min_underlying_out = 0 is what the trait-level `redeem` and the SDK both
    // pass, so this settles short rather than reverting.
    let out = market.vault.redeem(&market.user, &(60 * UNIT), &0);

    assert_eq!(out, 50 * UNIT, "only 50 was realizable");
    assert_eq!(
        market.vault.balance(&market.user),
        held - 50 * UNIT,
        "60 SY was requested; the 10 the strategy could not pay for stays put"
    );
    assert_eq!(
        market.vault.total_supply(),
        100 * UNIT - 50 * UNIT,
        "and the unburned 10 is not silently removed from the supply either"
    );
    assert_eq!(
        market.vault.exchange_rate(),
        rate_before,
        "a short fill must not reprice the holders who stayed"
    );
}

/// The partial-fill burn rounds up, so the notch falls toward the vault. `L8` in
/// the audit: V1 rounded down here, favouring the redeemer.
#[test]
fn the_partial_fill_burn_rounds_toward_the_vault() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    // Lift the rate to 1.1 the only way this hostile double can: it counts its
    // raw balance, so a donation moves it.
    donate(&env, &market.underlying, &market.strategy.address, 10 * UNIT);
    assert_eq!(market.vault.exchange_rate(), 1_100_000_000_000_000_000);

    // A deliberately indivisible fill: 33_333_334 / 1.1 is 30_303_030.90...
    market.strategy.set_liquidity_cap(&33_333_334);
    let out = market.vault.redeem(&market.user, &(50 * UNIT), &0);

    assert_eq!(out, 33_333_334);
    assert_eq!(
        held - market.vault.balance(&market.user),
        30_303_031,
        "ceil, not the 30_303_030 a floor would have burned"
    );
}

// ---------------------------------------------------------------------------
// Deposit cap (M9) — admin-settable, deposit-only
// ---------------------------------------------------------------------------

#[test]
fn the_deposit_cap_reverts_a_crossing_deposit_and_mints_nothing() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);

    assert_eq!(market.vault.deposit_cap(), 0, "uncapped until an admin says so");
    market.vault.set_deposit_cap(&market.admin, &(100 * UNIT));
    assert_eq!(market.vault.deposit_cap(), 100 * UNIT);

    // Exactly at the cap is allowed.
    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert_eq!(market.vault.total_assets(), 100 * UNIT);

    let supply = market.vault.total_supply();
    assert_eq!(
        market.vault.try_deposit(&market.user, &1, &0),
        Err(Ok(contract_error(VaultError::DepositCapExceeded))),
        "one stroop over the cap must revert"
    );
    assert_eq!(market.vault.total_supply(), supply, "and mint nothing");
    assert_eq!(market.vault.balance(&market.user), held);
    assert_eq!(market.vault.total_assets(), 100 * UNIT, "and move nothing");

    // Raising it re-opens the door; removing it entirely does too.
    market.vault.set_deposit_cap(&market.admin, &(150 * UNIT));
    assert_eq!(market.vault.deposit(&market.user, &(50 * UNIT), &0), 50 * UNIT);
    market.vault.set_deposit_cap(&market.admin, &0);
    assert_eq!(market.vault.deposit(&market.user, &(500 * UNIT), &0), 500 * UNIT);
}

/// The cap bounds *real backing*: it is read after the strategy has credited, so
/// an upstream that shaves a deposit consumes only the cap it actually took on.
#[test]
fn the_deposit_cap_is_measured_against_credited_assets_not_the_request() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    market.vault.set_deposit_cap(&market.admin, &(100 * UNIT));
    market.strategy.set_shave(&(5 * UNIT));

    // 100 requested, 95 credited — under a cap of 100, because the cap bounds
    // what is actually backing the shares.
    market.vault.deposit(&market.user, &(100 * UNIT), &0);
    assert_eq!(market.vault.total_assets(), 95 * UNIT);
}

/// The whole point of the deposit-only model: an admin can close the front door
/// and can never touch what is already inside.
#[test]
fn the_deposit_cap_can_never_block_an_exit() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let coin = token::TokenClient::new(&env, &market.underlying);

    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    let start = coin.balance(&market.user);

    // The most hostile cap an admin can set: one stroop, far below TVL.
    market.vault.set_deposit_cap(&market.admin, &1);
    assert!(
        market.vault.try_deposit(&market.user, &(1 * UNIT), &0).is_err(),
        "deposits are closed"
    );

    // Every other surface is untouched.
    assert_eq!(market.vault.exchange_rate(), WAD, "the rate is not the cap's");
    let other = Address::generate(&env);
    market.vault.transfer(&market.user, &other, &(10 * UNIT));
    market.vault.transfer(&other, &market.user, &(10 * UNIT));

    // And the holder exits in full, at the same rate, in one call.
    let out = market.vault.redeem(&market.user, &held, &0);
    assert_eq!(out, held);
    assert_eq!(coin.balance(&market.user), start + held);
    assert_eq!(market.vault.balance(&market.user), 0);
}

#[test]
fn only_the_admin_can_move_the_deposit_cap() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let stranger = Address::generate(&env);

    assert_eq!(
        market.vault.try_set_deposit_cap(&stranger, &(10 * UNIT)),
        Err(Ok(VaultError::NotAdmin)),
        "the cap is the admin's one lever and nobody else's"
    );
    assert_eq!(market.vault.deposit_cap(), 0, "and the rejected call changed nothing");

    assert_eq!(
        market.vault.try_set_deposit_cap(&market.admin, &(-1)),
        Err(Ok(VaultError::InvalidAmount))
    );
    assert_eq!(market.vault.deposit_cap(), 0);
}

// ---------------------------------------------------------------------------
// V1 / V2 equivalence over identical Blend state
// ---------------------------------------------------------------------------

/// The gate on first use of `strategy-blend`: given the same Blend state and the
/// same actions, V2 must produce the same rate, the same total supply, and the
/// same underlying out as the V1 wrapper running on mainnet today.
///
/// **One divergence is deliberate**: V2's opening deposit hands the depositor
/// `MINIMUM_SHARES` fewer shares, because those are locked away permanently so
/// the supply can never return to zero and no holder can ever own all of it. The
/// shares are still *minted* — V2's `total_supply` matches V1's exactly — so the
/// rate, and therefore everything the tokenizer and AMM read, is unaffected.
#[test]
fn v1_and_v2_agree_over_identical_blend_state() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let user = Address::generate(&env);

    // Two independent pools so the two positions cannot interfere.
    let underlying = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(&env, &underlying).mint(&user, &(10_000 * UNIT));

    let v1_pool_id = env.register(MockBlendPool, ());
    let v1_pool = MockBlendPoolClient::new(&env, &v1_pool_id);
    v1_pool.initialize(&underlying);
    let v1 = SyWrapperClient::new(&env, &env.register(SyWrapper, ()));
    v1.initialize_blend(&admin, &underlying, &v1_pool_id);

    let v2_pool_id = env.register(MockBlendPool, ());
    let v2_pool = MockBlendPoolClient::new(&env, &v2_pool_id);
    v2_pool.initialize(&underlying);
    let v2_vault_id = env.register(SyVaultV2, ());
    let v2_strategy_id = env.register(StrategyBlend, ());
    StrategyBlendClient::new(&env, &v2_strategy_id)
        .initialize(&admin, &v2_vault_id, &underlying, &v2_pool_id);
    let v2 = SyVaultV2Client::new(&env, &v2_vault_id);
    v2.initialize(&admin, &v2_strategy_id);

    // 1. Identical opening deposits mint identical supply; V2 withholds the
    //    locked minimum from the depositor and from nobody else.
    let deposit = 137 * UNIT + 3; // deliberately not a round number
    let v1_shares = v1.deposit(&user, &deposit);
    let v2_shares = v2.deposit(&user, &deposit, &0);
    assert_eq!(
        v2_shares,
        v1_shares - MINIMUM_SHARES,
        "V2's opening deposit locks MINIMUM_SHARES away and pays out the rest"
    );
    assert_eq!(
        v1.total_supply(),
        v2.total_supply(),
        "the locked shares are minted, so the supply — and the rate — match"
    );
    assert_eq!(v1.exchange_rate(), v2.exchange_rate(), "opening rate");

    // 2. Identical rate growth produces identical rates.
    for b_rate in [
        1_013_700_000_000_i128,
        1_100_000_000_001,
        1_337_000_000_007,
    ] {
        v1_pool.set_b_rate(&b_rate);
        v2_pool.set_b_rate(&b_rate);
        assert_eq!(
            v1.exchange_rate(),
            v2.exchange_rate(),
            "rate must match at b_rate {b_rate}"
        );
    }

    // 3. A second deposit at a non-unit rate mints identically — this is the
    //    rounding boundary where a credited-delta bug would show up.
    let second = 41 * UNIT + 7;
    assert_eq!(
        v1.deposit(&user, &second),
        v2.deposit(&user, &second, &0),
        "second-deposit shares must match at a non-unit rate"
    );

    // 4. Identical redemptions release identical underlying.
    let redeem = 60 * UNIT + 11;
    assert_eq!(
        v1.redeem(&user, &redeem),
        v2.redeem(&user, &redeem, &0),
        "redemption underlying must match"
    );
    assert_eq!(v1.exchange_rate(), v2.exchange_rate(), "post-redeem rate");
    assert_eq!(
        v1.total_supply(),
        v2.total_supply(),
        "post-redeem supply must match"
    );
}

#[test]
fn v2_reserve_reindex_traps_and_recovers_like_v1() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    market.vault.deposit(&market.user, &(100 * UNIT), &0);

    // The pool moves the underlying from slot 0 to slot 1.
    let other = Address::generate(&env);
    market
        .pool
        .set_reserve_list(&soroban_sdk::vec![&env, other, market.underlying.clone()]);
    market.pool.set_reserve_index(&1);
    assert!(
        market.vault.try_exchange_rate().is_err(),
        "valuing the wrong reserve must fail closed"
    );

    let new_index = market.strategy.migrate_reserve_index(&market.admin);
    assert_eq!(new_index, 1);
    assert_eq!(market.strategy.config().reserve_index, 1);
    assert_eq!(
        market.vault.exchange_rate(),
        WAD,
        "the market resumes at the same rate once re-synced"
    );
}

#[test]
fn reserve_migration_rejects_a_non_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let stranger = Address::generate(&env);
    let other = Address::generate(&env);
    market
        .pool
        .set_reserve_list(&soroban_sdk::vec![&env, other, market.underlying.clone()]);
    market.pool.set_reserve_index(&1);

    assert!(
        market.strategy.try_migrate_reserve_index(&stranger).is_err(),
        "only the strategy admin may re-sync the reserve index"
    );
    // Still bricked: the rejected call left the stored index untouched.
    assert_eq!(market.strategy.config().reserve_index, 0);
}

// ---------------------------------------------------------------------------
// Full stack — nothing above SY changes
// ---------------------------------------------------------------------------

/// The tokenizer, PT, and YT are wired to a V2 vault with no modification. They
/// only ever call `exchange_rate()` and move SY as a SEP-41 token, which is
/// exactly why the seam is invisible to them.
#[test]
fn tokenizer_pt_and_yt_run_unchanged_on_a_v2_vault() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(0);

    let market = blend_market(&env);
    let sy = market.vault.address.clone();

    let pt = PtTokenClient::new(&env, &env.register(PtToken, ()));
    let yt = YtTokenClient::new(&env, &env.register(YtToken, ()));
    let tokenizer = TokenizerClient::new(&env, &env.register(Tokenizer, ()));

    pt.initialize(&market.admin, &tokenizer.address, &sy, &MATURITY);
    yt.initialize(&market.admin, &tokenizer.address, &sy, &MATURITY);
    tokenizer.initialize(
        &market.admin,
        &sy,
        &pt.address,
        &yt.address,
        &MATURITY,
        &market.admin,
        &0_i128,
    );

    // Mint SY, then split it into PT + YT.
    let sy_held = market.vault.deposit(&market.user, &(100 * UNIT), &0);
    let (pt_out, yt_out) = tokenizer.split(&market.user, &sy_held);
    assert_eq!(pt_out, sy_held, "at rate 1.0, SY splits one-for-one into face");
    assert_eq!(yt_out, sy_held);

    // Yield accrues upstream and lands on YT, priced through the same
    // derived rate the vault exposes.
    market.pool.set_b_rate(&1_100_000_000_000);
    assert_eq!(market.vault.exchange_rate(), 1_100_000_000_000_000_000);
    let claimable = yt.preview_claim_yield(&market.user);
    assert!(claimable > 0, "YT must accrue the upstream yield: {claimable}");

    let claimed = tokenizer.claim_yield(&market.user);
    assert_eq!(claimed, claimable, "preview must match the settled claim");

    // PT stays senior: the escrow still covers the full PT face at maturity.
    env.ledger().set_timestamp(MATURITY + 1);
    tokenizer.freeze_maturity_rate();
    let redeemed = tokenizer.redeem_at_maturity(&market.user, &sy_held);
    assert!(
        redeemed > 0,
        "PT must redeem against the frozen maturity rate: {redeemed}"
    );
    assert_eq!(pt.balance(&market.user), 0);
}

/// A short-liquidity drill: deposits close while redemption stays priced
/// pro-rata. Gating claims is what froze the V1 market, so the asymmetry is
/// deliberate — the shortfall must be visible *before* a user commits, not
/// enforced by blocking their exit.
#[test]
fn a_liquidity_shortfall_is_visible_without_freezing_redemption() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    market.vault.deposit(&market.user, &(100 * UNIT), &0);

    let borrower = Address::generate(&env);
    market.pool.drain(&borrower, &(80 * UNIT));

    // The shortfall is legible to the SDK and the keeper.
    assert_eq!(market.vault.total_assets(), 100 * UNIT);
    assert_eq!(market.vault.max_withdraw(), 20 * UNIT);
    assert!(market.vault.max_withdraw() < market.vault.total_assets());

    // Redemption within realizable liquidity still settles, at the unchanged
    // pro-rata rate.
    let out = market.vault.redeem(&market.user, &(20 * UNIT), &(20 * UNIT));
    assert_eq!(out, 20 * UNIT);
    assert_eq!(market.vault.exchange_rate(), WAD, "the rate never moved");
}

#[test]
fn touch_is_permissionless_and_reaches_the_upstream() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    market.vault.deposit(&market.user, &(100 * UNIT), &0);

    // Anyone may renew an idle market, through the vault or the strategy.
    market.vault.touch();
    market.strategy.touch();
    assert_eq!(market.vault.exchange_rate(), WAD);
}

#[test]
fn a_failed_upstream_withdrawal_surfaces_as_a_typed_error() {
    let env = Env::default();
    env.mock_all_auths();
    let market = blend_market(&env);
    let held = market.vault.deposit(&market.user, &(100 * UNIT), &0);

    market.pool.set_should_fail_withdraw(&true);
    assert!(
        market.vault.try_redeem(&market.user, &(10 * UNIT), &0).is_err(),
        "a pool failure must not consume the holder's SY"
    );
    assert_eq!(market.vault.balance(&market.user), held);

    market.pool.set_should_fail_withdraw(&false);
    assert_eq!(market.vault.redeem(&market.user, &(10 * UNIT), &0), 10 * UNIT);
}

#[test]
fn vault_errors_are_typed_not_raw_panics() {
    let env = Env::default();
    env.mock_all_auths();
    let market = mock_market(&env);

    // `deposit`/`redeem` return a bare i128 on the wire, so the client's `try_`
    // variant surfaces the raw contract error code rather than the typed enum.
    // Assert on the code the enum pins, which is what an SDK caller sees.
    assert_eq!(
        market.vault.try_deposit(&market.user, &0, &0),
        Err(Ok(contract_error(VaultError::InvalidAmount)))
    );
    assert_eq!(
        market.vault.try_redeem(&market.user, &(-1), &0),
        Err(Ok(contract_error(VaultError::InvalidAmount)))
    );
    market.vault.deposit(&market.user, &(10 * UNIT), &0);
    assert_eq!(
        market.vault.try_redeem(&market.user, &(11 * UNIT), &0),
        Err(Ok(contract_error(VaultError::InsufficientBalance)))
    );
    let _ = &market.sink;
    let _ = BLEND_SCALAR_12;
}

fn contract_error(error: VaultError) -> HostError {
    HostError::from_contract_error(error as u32)
}
