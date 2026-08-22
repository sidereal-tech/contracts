// SPDX-License-Identifier: Apache-2.0

//! Economics suite for the corrected PT/YT model (audit Layer 1 findings 3, 4,
//! and the post-maturity finding).
//!
//! Covers the intended Pendle-style behavior:
//!   - YT holders receive their accrued yield, paid in SY, on claim.
//!   - PT redeems to its asset face (principal at the maturity rate), not 1:1
//!     in SY shares.
//!   - Yield is conserved across transfers (no loss, no double count).
//!   - The escrow covers outstanding PT face plus unclaimed YT yield at every
//!     state transition, including a 10k-step random property test.
//!   - Insolvency: PT redemption is capped pro-rata on a rate regression; YT
//!     claims never freeze (the settle math pays zero on a dip, and banked
//!     yield stays payable).
//!   - The maturity rate is frozen, so post-maturity rate moves do not change
//!     redemption.
//!
//! These started as RED specs against the old code (PT redeemed 1:1, YT paid
//! nothing) and are now green. See docs/PROGRESS.md.

use sidereal_pt_token::{PtToken, PtTokenClient};
use sidereal_sy_wrapper::{SyWrapper, SyWrapperClient};
use sidereal_tokenizer::{Tokenizer, TokenizerClient};
use sidereal_yt_token::{YtToken, YtTokenClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env,
};

const WAD: i128 = 1_000_000_000_000_000_000;
/// One whole token at the 7-decimal underlying precision.
const UNIT: i128 = 10_000_000;
const MATURITY: u64 = 1_000_000;
const RATE_1_10: i128 = 1_100_000_000_000_000_000;

struct Market {
    env: Env,
    admin: Address,
    underlying: Address,
    sy: Address,
    pt: Address,
    yt: Address,
    tokenizer: Address,
    fee_recipient: Address,
}

fn deploy() -> Market {
    deploy_with_fee(0)
}

/// `yield_fee_bps` is the protocol's cut of claimed yield, fixed at
/// initialization. Every test above runs fee-free so its arithmetic reads as the
/// protocol's own; the fee tests opt in.
fn deploy_with_fee(yield_fee_bps: i128) -> Market {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let fee_recipient = Address::generate(&env);

    let underlying = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let sy = env.register(SyWrapper, ());
    let pt = env.register(PtToken, ());
    let yt = env.register(YtToken, ());
    let tokenizer = env.register(Tokenizer, ());

    SyWrapperClient::new(&env, &sy).initialize(&admin, &underlying);
    PtTokenClient::new(&env, &pt).initialize(&admin, &tokenizer, &sy, &MATURITY);
    YtTokenClient::new(&env, &yt).initialize(&admin, &tokenizer, &sy, &MATURITY);
    TokenizerClient::new(&env, &tokenizer).initialize(
        &admin,
        &sy,
        &pt,
        &yt,
        &MATURITY,
        &fee_recipient,
        &yield_fee_bps,
    );

    Market {
        env,
        admin,
        underlying,
        sy,
        pt,
        yt,
        tokenizer,
        fee_recipient,
    }
}

impl Market {
    /// Mints `amount` underlying to a fresh holder and returns their address.
    fn fund(&self, amount: i128) -> Address {
        let user = Address::generate(&self.env);
        token::StellarAssetClient::new(&self.env, &self.underlying).mint(&user, &amount);
        user
    }

    fn deposit(&self, who: &Address, amount: i128) -> i128 {
        SyWrapperClient::new(&self.env, &self.sy).deposit(who, &amount)
    }

    fn split(&self, who: &Address, sy_amount: i128) {
        TokenizerClient::new(&self.env, &self.tokenizer).split(who, &sy_amount);
    }

    /// A bare address that holds no underlying (it can still receive YT and be
    /// paid SY yield).
    fn account(&self) -> Address {
        Address::generate(&self.env)
    }

    fn transfer_yt(&self, from: &Address, to: &Address, amount: i128) {
        YtTokenClient::new(&self.env, &self.yt).transfer(from, to, &amount);
    }

    /// A holder burning their own YT directly on the YT contract (not through
    /// the tokenizer's recombine).
    fn burn_yt(&self, from: &Address, amount: i128) {
        YtTokenClient::new(&self.env, &self.yt).burn(from, &amount);
    }

    fn recombine(&self, who: &Address, amount: i128) -> i128 {
        TokenizerClient::new(&self.env, &self.tokenizer).recombine(who, &amount, &amount)
    }

    fn yt_balance(&self, who: &Address) -> i128 {
        YtTokenClient::new(&self.env, &self.yt).balance(who)
    }

    fn underlying_balance(&self, who: &Address) -> i128 {
        token::TokenClient::new(&self.env, &self.underlying).balance(who)
    }

    fn redeem_pt(&self, who: &Address, pt_amount: i128) -> i128 {
        TokenizerClient::new(&self.env, &self.tokenizer).redeem_at_maturity(who, &pt_amount)
    }

    fn maturity_rate(&self) -> i128 {
        TokenizerClient::new(&self.env, &self.tokenizer).maturity_rate()
    }

    /// Claims YT yield through the tokenizer, which pays SY out of escrow.
    fn claim(&self, holder: &Address) -> i128 {
        TokenizerClient::new(&self.env, &self.tokenizer).claim_yield(holder)
    }

    fn set_rate(&self, rate: i128) {
        SyWrapperClient::new(&self.env, &self.sy).set_exchange_rate(&self.admin, &rate);
    }

    /// Records the current SY rate as the tokenizer's latest pre-maturity
    /// observation (the permissionless poke). Tests that model "the rate at
    /// maturity is X" must call this after their final pre-maturity set_rate:
    /// the maturity freeze uses the last observed rate, never a live
    /// post-maturity read.
    fn observe(&self) -> i128 {
        TokenizerClient::new(&self.env, &self.tokenizer).observe_rate()
    }

    fn rate(&self) -> i128 {
        SyWrapperClient::new(&self.env, &self.sy).exchange_rate()
    }

    fn sy_balance(&self, who: &Address) -> i128 {
        SyWrapperClient::new(&self.env, &self.sy).balance(who)
    }

    /// SY shares the tokenizer custodies in escrow.
    fn escrow_shares(&self) -> i128 {
        SyWrapperClient::new(&self.env, &self.sy).balance(&self.tokenizer)
    }

    fn pt_balance(&self, who: &Address) -> i128 {
        PtTokenClient::new(&self.env, &self.pt).balance(who)
    }

    fn pt_supply(&self) -> i128 {
        PtTokenClient::new(&self.env, &self.pt).total_supply()
    }

    /// SY-share YT yield owed but unclaimed, summed over the known holders.
    fn yt_outstanding(&self, holders: &[&Address]) -> i128 {
        let yt = YtTokenClient::new(&self.env, &self.yt);
        holders
            .iter()
            .map(|h| yt.preview_claim_yield(h))
            .sum::<i128>()
    }

    fn yt_basis(&self, who: &Address) -> i128 {
        YtTokenClient::new(&self.env, &self.yt).yield_basis(who)
    }

    /// The YT contract's aggregate ledger: every holder's yield basis plus every
    /// holder's banked-but-unclaimed yield, in SY shares. Read in one call, with
    /// no holder enumeration.
    fn yt_ledger_total(&self) -> i128 {
        let yt = YtTokenClient::new(&self.env, &self.yt);
        yt.total_yield_basis() + yt.total_accrued_yield()
    }

    /// The rate-INDEPENDENT form of the coverage invariant, made checkable by
    /// the aggregate ledger: the escrow, in shares, covers the SY that backs
    /// every outstanding YT position at the rates it was acquired at, plus every
    /// share already banked as yield.
    ///
    /// This is strictly stronger than `assert_escrow_covers` and needs no rate
    /// at all, because a holder's basis is exactly "PT reservation at the
    /// current rate + yield owed at the current rate" for any rate at or above
    /// the one their basis was struck at. It is the invariant a pro-rata junior
    /// split would be sized against.
    fn assert_escrow_covers_yt_ledger(&self) {
        let escrow = self.escrow_shares();
        let ledger = self.yt_ledger_total();
        assert!(
            escrow >= ledger,
            "escrow {} shares must cover the YT ledger (basis + banked) {}",
            escrow,
            ledger
        );
    }

    /// The hard invariant: escrow, valued at the current rate, must cover every
    /// outstanding PT at face plus every YT's unclaimed yield. All terms are in
    /// asset units (YT yield is reported in SY shares, so convert at the rate).
    fn assert_escrow_covers(&self, holders: &[&Address]) {
        let rate = self.rate();
        let escrow_asset = self.escrow_shares() * rate / WAD;
        let yt_asset = self.yt_outstanding(holders) * rate / WAD;
        let covered = self.pt_supply() + yt_asset;
        assert!(
            escrow_asset >= covered,
            "escrow {} asset units must cover PT+YT claims {}",
            escrow_asset,
            covered
        );
    }
}

#[test]
fn yt_receives_yield_on_claim() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT); // 100*UNIT SY shares at rate 1.0
    m.split(&alice, 100 * UNIT); // PT and YT, asset-denominated
    assert_eq!(m.sy_balance(&alice), 0, "split escrows all of Alice's SY");

    m.set_rate(RATE_1_10); // +10% accrues to YT holders

    let reported = m.claim(&alice);

    // Yield in asset units = 100*UNIT * 0.10 = 10*UNIT. Paid in SY at rate 1.10:
    // 10*UNIT * WAD / 1.10 = ~9.0909 * UNIT.
    let expected_sy = (10 * UNIT) * WAD / RATE_1_10;
    let got = m.sy_balance(&alice);
    assert!(
        (got - expected_sy).abs() <= 2,
        "YT holder should receive ~{} SY of yield, got {}",
        expected_sy,
        got
    );
    assert!(reported > 0, "claim should report the accrued amount");
}

#[test]
fn pt_redeems_to_principal_not_share() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    let pt = m.pt_balance(&alice);

    m.set_rate(RATE_1_10);
    m.observe(); // pin 1.10 as the observed maturity rate
    m.env.ledger().set_timestamp(MATURITY + 1);

    let sy_out = m.redeem_pt(&alice, pt);

    // PT is principal: pt_amount * WAD / R_maturity SY, NOT pt_amount of shares.
    let expected = pt * WAD / RATE_1_10;
    assert!(
        (sy_out - expected).abs() <= 2,
        "PT should redeem to {} SY (principal at the maturity rate), got {}",
        expected,
        sy_out
    );
    assert!(
        sy_out < pt,
        "PT must not redeem 1:1 in shares when the rate has grown above 1.0"
    );
}

#[test]
fn escrow_covers_outstanding_claims() {
    let m = deploy();
    let alice = m.fund(50 * UNIT);
    let bob = m.fund(50 * UNIT);
    m.deposit(&alice, 50 * UNIT);
    m.deposit(&bob, 50 * UNIT);
    m.split(&alice, 50 * UNIT);
    m.split(&bob, 50 * UNIT);

    let holders = [&alice, &bob];
    m.assert_escrow_covers(&holders);

    m.set_rate(RATE_1_10);
    m.assert_escrow_covers(&holders);

    // Each holder claims YT yield and must actually receive SY for it.
    m.claim(&alice);
    assert!(
        m.sy_balance(&alice) > 0,
        "Alice must receive her YT yield in SY"
    );
    m.assert_escrow_covers(&holders);

    m.claim(&bob);
    assert!(m.sy_balance(&bob) > 0, "Bob must receive his YT yield in SY");
    m.assert_escrow_covers(&holders);

    // Both redeem PT at maturity; the invariant holds after each.
    m.env.ledger().set_timestamp(MATURITY + 1);
    m.redeem_pt(&alice, m.pt_balance(&alice));
    m.assert_escrow_covers(&holders);
    m.redeem_pt(&bob, m.pt_balance(&bob));

    // With every claim settled, escrow drains to ~0 (within rounding dust).
    assert!(
        m.escrow_shares() <= 4,
        "escrow should drain to ~0, {} shares left",
        m.escrow_shares()
    );
}

#[test]
fn redemption_uses_frozen_maturity_rate() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    // The rate at maturity is 1.10, pinned by an observation.
    m.set_rate(RATE_1_10);
    m.observe();
    m.env.ledger().set_timestamp(MATURITY + 1);

    let half = 50 * UNIT;
    let expected = half * WAD / RATE_1_10;
    let sy1 = m.redeem_pt(&alice, half); // first post-maturity redeem snapshots 1.10
    assert!((sy1 - expected).abs() <= 4, "first redeem at the maturity rate");
    assert_eq!(m.maturity_rate(), RATE_1_10, "rate frozen at maturity");

    // The admin bumps the rate AFTER maturity; redemption must ignore it.
    let rate_1_20: i128 = 1_200_000_000_000_000_000;
    m.set_rate(rate_1_20);
    let sy2 = m.redeem_pt(&alice, half);
    assert!(
        (sy2 - expected).abs() <= 4,
        "post-maturity rate bump ignored: {} vs {}",
        sy2,
        expected
    );
}

/// The freeze-timing blocker: Blend has no maturity concept and keeps accruing
/// after maturity, so if the freeze read the live rate on first post-maturity
/// touch, a later first touch would pin a higher rate, moving value from PT to
/// YT and making redemption a race. The freeze must instead use the last rate
/// observed at or before maturity, no matter how late the first touch lands or
/// how far the live rate has drifted by then.
#[test]
fn freeze_ignores_post_maturity_accrual_even_on_late_first_touch() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    // Pre-maturity accrual to 1.05, observed by the poke.
    let rate_1_05: i128 = 1_050_000_000_000_000_000;
    m.set_rate(rate_1_05);
    assert_eq!(m.observe(), rate_1_05);

    // Maturity passes with no on-chain touch, and the live source keeps
    // accruing well past it before anyone shows up.
    m.env.ledger().set_timestamp(MATURITY + 100_000);
    m.set_rate(RATE_1_10);

    // The late first touch must freeze the pre-maturity observation, not the
    // live 1.10 read.
    let pt = m.pt_balance(&alice);
    let sy_out = m.redeem_pt(&alice, pt);
    assert_eq!(m.maturity_rate(), rate_1_05, "frozen at the observation");
    let expected = pt * WAD / rate_1_05;
    assert!(
        (sy_out - expected).abs() <= 4,
        "redeemed at the observed 1.05, not the drifted 1.10: {} vs {}",
        sy_out,
        expected
    );
}

/// The audit gap in the first freeze cut: direct YT operations (transfer,
/// burn) settle yield at a rate the tokenizer never saw, because they read SY
/// directly. YT's ledger could then recognize yield at 1.10 while the freeze
/// later pinned 1.00, starving YT of already-banked yield and over-crediting
/// PT. Direct YT paths now resolve their rate THROUGH the tokenizer's
/// observe_rate, so any rate YT banks at is on record for the freeze.
#[test]
fn direct_yt_transfer_is_observed_by_the_maturity_freeze() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.account();
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT); // tokenizer observes 1.00

    // Rate accrues to 1.10; the ONLY subsequent pre-maturity action is a
    // direct YT transfer. No tokenizer operation, no explicit poke. The
    // transfer settles Alice at 1.10, banking her yield, and must leave that
    // 1.10 on record for the freeze.
    m.set_rate(RATE_1_10);
    m.transfer_yt(&alice, &bob, UNIT);
    let yt = YtTokenClient::new(&m.env, &m.yt);
    let banked = yt.preview_claim_yield(&alice);
    assert!(banked > 0, "the transfer banked Alice's accrued yield at 1.10");

    m.env.ledger().set_timestamp(MATURITY + 1);

    // First post-maturity touch freezes the 1.10 the transfer observed, not
    // the older 1.00 from the split.
    let pt = m.pt_balance(&alice);
    let sy_out = m.redeem_pt(&alice, pt);
    assert_eq!(
        m.maturity_rate(),
        RATE_1_10,
        "the direct transfer's rate must be the frozen observation"
    );
    let expected = pt * WAD / RATE_1_10;
    assert!(
        (sy_out - expected).abs() <= 4,
        "PT redeems at the transfer-observed 1.10: {} vs {}",
        sy_out,
        expected
    );

    // And the yield the transfer banked is actually payable: at a frozen 1.10
    // the escrow holds a surplus over the PT reservation equal to the yield,
    // so Alice collects what her ledger recognized. Under the old gap (frozen
    // 1.00) the reservation would swallow the whole escrow and pay her zero.
    let claimed = m.claim(&alice);
    assert!(
        (banked - claimed).abs() <= 2,
        "banked yield is payable because the freeze saw the same rate it was banked at: banked {} vs claimed {}",
        banked,
        claimed
    );
}

/// Same gap, burn flavor: a holder burning their own YT directly settles
/// first, and that settle's rate must reach the freeze. The tokenizer-driven
/// burn (recombine) needs no observation of its own because recombine records
/// one in the same transaction; this covers the holder-direct path.
#[test]
fn direct_yt_burn_is_observed_by_the_maturity_freeze() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT); // tokenizer observes 1.00

    let rate_1_08: i128 = 1_080_000_000_000_000_000;
    m.set_rate(rate_1_08);
    m.burn_yt(&alice, UNIT); // direct burn, the only action at 1.08

    m.env.ledger().set_timestamp(MATURITY + 1);
    let pt = m.pt_balance(&alice);
    m.redeem_pt(&alice, pt);
    assert_eq!(
        m.maturity_rate(),
        rate_1_08,
        "the direct burn's settle rate must be the frozen observation"
    );
}

/// The unobserved tail is conservative in PT's favor, consistent with PT
/// seniority: rate moves after the last observation but before maturity are
/// not priced into the freeze unless someone (any keeper or YT holder, the
/// poke is permissionless) records them. Yield that IS observed pre-maturity
/// is pinned exactly.
#[test]
fn unobserved_pre_maturity_tail_freezes_at_the_last_observation() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT); // last mutating op observes rate 1.00

    // The rate climbs pre-maturity, but nothing observes it before maturity.
    let rate_1_08: i128 = 1_080_000_000_000_000_000;
    m.set_rate(rate_1_08);
    m.env.ledger().set_timestamp(MATURITY + 1);

    let pt = m.pt_balance(&alice);
    let sy_out = m.redeem_pt(&alice, pt);
    assert_eq!(
        m.maturity_rate(),
        WAD,
        "freeze falls back to the split-time observation, never a live post-maturity read"
    );
    // At the frozen 1.00 the full face is capped by pro-rata escrow coverage,
    // so PT redeems its escrow slice and no post-maturity read decided it.
    assert!(sy_out > 0);
}

/// Cross-contract maturity-rate consistency: the split-brain fix.
///
/// A matured market whose SY rate keeps drifting after maturity (idle
/// mock-custody mode, admin rate setter). PT redemption and YT yield must both
/// settle against the tokenizer's single canonical frozen rate, so the outcome
/// is identical whether PT is redeemed first or YT is claimed first.
///
/// Under the old split-brain, each contract froze its own rate on its own first
/// post-maturity call, so whichever action ran second pinned a later, different
/// rate. With PT-redeem-first that left the escrow unable to cover the (larger)
/// YT claim computed at the higher rate, which trapped; with YT-claim-first it
/// silently paid PT at the wrong rate. This test would fail (panic or mismatch)
/// on the old code and passes now that both read the one frozen rate.
#[test]
fn pt_redeem_and_yt_claim_use_one_frozen_rate_regardless_of_order() {
    const RATE_1_15: i128 = 1_150_000_000_000_000_000;
    const RATE_1_20: i128 = 1_200_000_000_000_000_000;

    // YT yield in SY shares for `bal` held from rate `c` to rate `r`, matching
    // the contract's telescoping form (floor division).
    fn owed(bal: i128, c: i128, r: i128) -> i128 {
        (bal * (r - c) / c) * WAD / r
    }

    // Runs one ordering and returns
    // (pt_redeem_sy, yt_claim_sy, escrow_left, tokenizer_frozen_rate).
    fn run(redeem_first: bool) -> (i128, i128, i128, i128) {
        let m = deploy();
        let alice = m.fund(100 * UNIT);
        m.deposit(&alice, 100 * UNIT);
        m.split(&alice, 100 * UNIT); // 100 UNIT PT + YT, checkpoint at rate 1.00

        // Rate climbs to 1.10 by maturity, observed, so YT has real accrued
        // yield to claim and the freeze has a pre-maturity rate to pin.
        m.set_rate(RATE_1_10);
        m.observe();
        m.env.ledger().set_timestamp(MATURITY + 1);

        // The SY rate is still drifting after maturity: it is 1.15 when the
        // first post-maturity action fires and 1.20 by the second. NONE of
        // that drift may reach redemption: the freeze must pin the observed
        // 1.10 for both PT and YT, regardless of which acts first or how far
        // the live rate has moved by then.
        m.set_rate(RATE_1_15);

        let pt = m.pt_balance(&alice);
        let redeem_sy;
        let claim_sy;
        if redeem_first {
            redeem_sy = m.redeem_pt(&alice, pt);
            m.set_rate(RATE_1_20);
            claim_sy = m.claim(&alice);
        } else {
            claim_sy = m.claim(&alice);
            m.set_rate(RATE_1_20);
            redeem_sy = m.redeem_pt(&alice, pt);
        }
        (redeem_sy, claim_sy, m.escrow_shares(), m.maturity_rate())
    }

    let a = run(true); // PT redeem first, then YT claim
    let b = run(false); // YT claim first, then PT redeem

    // Both orderings freeze the SAME canonical rate: the last pre-maturity
    // observation (1.10), never the 1.15 or 1.20 live at the post-maturity
    // touches. First-touch timing must be economically irrelevant.
    assert_eq!(a.3, RATE_1_10, "PT-first must freeze the observed rate");
    assert_eq!(b.3, RATE_1_10, "YT-first must freeze the observed rate");

    // Identical economic outcome regardless of order.
    assert_eq!(a.0, b.0, "PT redemption SY must not depend on order");
    assert_eq!(a.1, b.1, "YT claim SY must not depend on order");
    assert_eq!(a.0 + a.1, b.0 + b.1, "total SY paid out must not depend on order");
    assert_eq!(a.2, b.2, "escrow left must not depend on order");

    // The rate both contracts used equals the tokenizer's frozen maturity_rate:
    // cross-check the amounts against a 1.10 computation, and confirm the
    // drifted rates would differ, so the test fails if either contract read a
    // live post-maturity rate instead of the observation.
    let expected_redeem = 100 * UNIT * WAD / RATE_1_10;
    let expected_claim = owed(100 * UNIT, WAD, RATE_1_10);
    assert!(
        (a.0 - expected_redeem).abs() <= 4,
        "PT must redeem at the observed 1.10: {} vs {}",
        a.0,
        expected_redeem
    );
    assert!(
        (a.1 - expected_claim).abs() <= 4,
        "YT must claim at the observed 1.10: {} vs {}",
        a.1,
        expected_claim
    );
    assert!(
        (100 * UNIT * WAD / RATE_1_15 - expected_redeem).abs() > 4
            && (100 * UNIT * WAD / RATE_1_20 - expected_redeem).abs() > 4,
        "the drifted rates must differ from 1.10, else this test proves nothing"
    );
}

#[test]
fn redeem_allowed_at_exact_maturity() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    m.env.ledger().set_timestamp(MATURITY); // exactly at maturity
    let pt = m.pt_balance(&alice);
    let out = m.redeem_pt(&alice, pt);
    assert!(out > 0, "redemption works at exactly the maturity timestamp");
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn split_rejects_at_exact_maturity() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);

    m.env.ledger().set_timestamp(MATURITY); // the market is no longer live
    m.split(&alice, 100 * UNIT);
}

#[test]
fn redemption_is_capped_when_rate_regresses() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.deposit(&bob, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    m.split(&bob, 100 * UNIT);
    // Solvent: escrow 200 shares worth 200, PT principal 200, rate 1.00.

    // The yield source is slashed to 0.95: escrow now worth 190 < 200 of PT.
    // Observed pre-maturity, so the freeze prices the slash.
    let rate_0_95: i128 = 950_000_000_000_000_000;
    m.set_rate(rate_0_95);
    m.observe();
    m.env.ledger().set_timestamp(MATURITY + 1);

    let alice_pt = m.pt_balance(&alice);
    let full_uncapped = alice_pt * WAD / rate_0_95;
    let sy_alice = m.redeem_pt(&alice, alice_pt);
    assert!(
        sy_alice < full_uncapped,
        "redemption must be capped below full principal under insolvency: {} vs {}",
        sy_alice,
        full_uncapped
    );

    let bob_pt = m.pt_balance(&bob);
    let sy_bob = m.redeem_pt(&bob, bob_pt);

    // The shortfall is shared pro-rata: equal PT redeems for equal SY, and the
    // escrow drains to ~0 with no redeemer favored over another.
    assert!(
        (sy_alice - sy_bob).abs() <= 4,
        "loss shared equally: {} vs {}",
        sy_alice,
        sy_bob
    );
    assert!(
        m.escrow_shares() <= 4,
        "escrow drains, {} shares left",
        m.escrow_shares()
    );
}

/// Pendle alignment: on a rate regression the market is underwater, but the
/// collateral-neutral operations no longer revert. A new depositor can always
/// split (mint never blocks on coverage, like `PendleYieldToken._mintPY`), and
/// recombine prices the shortfall as a pro-rata haircut instead of bricking with
/// Insolvent (#9). Claims do not guard either; see the claim regression tests
/// below. This is exactly the state the frontend hit before the fix.
#[test]
fn split_and_recombine_survive_a_rate_regression() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    // Solvent: escrow 100 shares worth 100 at rate 1.00, PT principal 100.

    // The yield source is slashed to 0.90: escrow is now worth 90 < 100 of PT.
    // This is the state that used to revert `split` with Insolvent (#9).
    let rate_0_90: i128 = 900_000_000_000_000_000;
    m.set_rate(rate_0_90);

    // A brand-new depositor can still mint. Split is collateral-neutral, so it
    // does not worsen coverage and must not revert.
    let bob = m.fund(50 * UNIT);
    m.deposit(&bob, 50 * UNIT);
    let bob_sy = m.sy_balance(&bob);
    m.split(&bob, bob_sy); // previously panicked with Error(Contract, #9)
    assert!(
        m.pt_balance(&bob) > 0,
        "a new depositor must be able to mint PT while the market is underwater"
    );

    // Recombine succeeds too, returning a fair haircut rather than reverting.
    // Alice's uncapped principal is pt/rate; underwater she gets her pro-rata
    // slice of escrow, which is strictly less. The preview must quote the
    // SAME capped number, not the uncapped principal, or the UI overpromises
    // during exactly the shortfall it matters most in.
    let alice_pt = m.pt_balance(&alice);
    let uncapped = alice_pt * WAD / rate_0_90;
    let quoted = TokenizerClient::new(&m.env, &m.tokenizer).preview_recombine(&alice_pt, &alice_pt);
    let got = m.recombine(&alice, alice_pt);
    assert!(
        got > 0 && got < uncapped,
        "recombine must haircut under a shortfall: got {} vs uncapped {}",
        got,
        uncapped
    );
    assert_eq!(
        quoted, got,
        "preview_recombine must quote the capped payout the recombine actually pays"
    );
}

/// The incident class Fix 1 was meant to end, on the claim path. With a
/// Blend-derived rate, split at rate exactly 1.00 leaves the escrow with zero
/// coverage slack, and a later redeem can tick the rate down by a sub-stroop
/// rounding notch (Blend's bToken burn rounds in the pool's favor). The old
/// post-claim solvency gate turned that dust into Insolvent (#9) on every
/// claim, even ones that owed nothing. Claims must instead succeed and pay
/// zero: the YT settle math holds the checkpoint on a dip.
#[test]
fn claim_yield_survives_a_sub_stroop_rate_regression() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    // Zero slack: escrow 100 shares at rate exactly 1.00 backs exactly 100 PT.

    // The smallest representable regression; the Blend rounding notch at demo
    // scale (WAD minus 1e8) hits the same way, any dip below 1.00 did it.
    m.set_rate(WAD - 1);

    let previewed = YtTokenClient::new(&m.env, &m.yt).preview_claim_yield(&alice);
    assert_eq!(previewed, 0, "preview must report zero, not trap");

    let claimed = m.claim(&alice); // previously panicked with Error(Contract, #9)
    assert_eq!(claimed, 0, "no yield accrued, so the claim pays zero");
    assert_eq!(m.sy_balance(&alice), 0);
    assert_eq!(
        m.pt_supply(),
        100 * UNIT,
        "the claim must not touch principal"
    );
}

/// SY shares needed to redeem all outstanding PT at `rate`, rounded up the same
/// way the tokenizer's PT-senior cap reserves escrow. Kept local to the test so
/// the assertions recompute the reservation independently of the contract.
fn pt_face_reservation(pt_supply: i128, rate: i128) -> i128 {
    (pt_supply * WAD + rate - 1) / rate
}

/// PT is senior. When the escrow cannot even cover outstanding PT principal,
/// there is no surplus, so a YT claim pays zero. The unpaid yield is not
/// forfeited: it stays banked and claimable once coverage returns
/// (banked_yield_becomes_claimable_after_the_rate_recovers proves the recovery).
#[test]
fn yt_claim_is_subordinated_when_pt_is_under_covered() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.account();
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    // Rate rises; Alice banks her yield by moving 1 unit of YT (which settles
    // her at 1.10), then the yield source crashes below PT coverage.
    m.set_rate(RATE_1_10);
    m.transfer_yt(&alice, &bob, UNIT);
    let yt = YtTokenClient::new(&m.env, &m.yt);
    let owed_before = yt.preview_claim_yield(&alice);
    assert!(owed_before > 0, "Alice has banked yield to claim");

    let rate_0_90: i128 = 900_000_000_000_000_000;
    m.set_rate(rate_0_90);

    // Escrow (100 shares) at rate 0.90 covers only ~90 asset units of the 100
    // PT face, so the PT reservation exceeds the whole escrow: surplus is zero.
    assert!(
        pt_face_reservation(m.pt_supply(), rate_0_90) > m.escrow_shares(),
        "precondition: PT is under-covered, so there is no YT surplus"
    );

    let claimed = m.claim(&alice);
    assert_eq!(claimed, 0, "YT is subordinate to PT: no surplus, no payout");
    assert_eq!(m.sy_balance(&alice), 0, "nothing is paid out");
    assert_eq!(
        yt.preview_claim_yield(&alice),
        owed_before,
        "the unpaid yield stays banked, not forfeited"
    );
}

/// PT is senior but fully covered, leaving a surplus smaller than the YT's
/// banked yield. The claim takes exactly that surplus and no more, draining the
/// escrow down to the PT reservation. The remainder stays banked.
#[test]
fn yt_claim_takes_only_the_surplus_over_pt_reservation() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.account();
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    m.set_rate(RATE_1_10);
    m.transfer_yt(&alice, &bob, UNIT);
    let yt = YtTokenClient::new(&m.env, &m.yt);
    let owed_before = yt.preview_claim_yield(&alice);

    // Rate dips to 1.05. PT still fully coverable (~95.2 of 100 shares), leaving
    // a ~4.76-share surplus, below Alice's ~9.09 banked yield.
    let rate_1_05: i128 = 1_050_000_000_000_000_000;
    m.set_rate(rate_1_05);

    let escrow_before = m.escrow_shares();
    let reservation = pt_face_reservation(m.pt_supply(), rate_1_05);
    let surplus = escrow_before - reservation;
    assert!(
        surplus > 0 && surplus < owed_before,
        "precondition: partial surplus, less than owed ({} of {})",
        surplus,
        owed_before
    );

    let claimed = m.claim(&alice);
    assert_eq!(
        claimed, surplus,
        "the claim pays exactly the surplus over the PT reservation"
    );
    assert_eq!(
        m.escrow_shares(),
        reservation,
        "escrow is drained down to the PT reservation, so PT stays fully covered"
    );
    assert_eq!(
        yt.preview_claim_yield(&alice),
        owed_before - claimed,
        "the unpaid remainder stays banked"
    );
}

/// The transient-dip case the surplus cap must not punish: a claim during a dip
/// pays nothing, but once the rate recovers the previously-unpaid yield becomes
/// claimable in full. Conservation: Alice collects exactly what she was owed
/// across the dip and recovery, with no phantom yield from the round trip (the
/// checkpoint is a high-water mark, so the dip does not re-open accrual).
#[test]
fn banked_yield_becomes_claimable_after_the_rate_recovers() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.account();
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    m.set_rate(RATE_1_10);
    m.transfer_yt(&alice, &bob, UNIT);
    let yt = YtTokenClient::new(&m.env, &m.yt);
    let owed = yt.preview_claim_yield(&alice);
    assert!(owed > 0);

    // Deep dip: no surplus, so the claim pays nothing and banks the whole owed.
    m.set_rate(900_000_000_000_000_000);
    assert_eq!(m.claim(&alice), 0, "no surplus during the dip");
    assert_eq!(
        yt.preview_claim_yield(&alice),
        owed,
        "the full entitlement is still banked"
    );

    // Recovery restores the surplus; the banked yield is now claimable in full.
    m.set_rate(RATE_1_10);
    let claimed = m.claim(&alice);
    assert_eq!(
        claimed, owed,
        "the recovered payout equals the originally banked entitlement, no phantom yield"
    );
    assert_eq!(m.sy_balance(&alice), claimed);
    assert_eq!(
        yt.preview_claim_yield(&alice),
        0,
        "nothing left banked after the full payout"
    );
}

#[test]
fn transfer_conserves_yield_through_claims() {
    let m = deploy();
    let alice = m.fund(100 * UNIT);
    let bob = m.account(); // holds no underlying, only receives YT
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT); // 100 YT to Alice, checkpoint 1.00

    // Rate rises to 1.10, then Alice sends half her YT to Bob without claiming.
    // The transfer settles both: Alice banks her yield on 100 over 1.00->1.10,
    // Bob starts fresh at 1.10.
    m.set_rate(RATE_1_10);
    m.transfer_yt(&alice, &bob, 50 * UNIT);

    // Rate rises again to 1.20; now Alice earns on 50 and Bob earns on 50.
    let rate_1_20: i128 = 1_200_000_000_000_000_000;
    m.set_rate(rate_1_20);

    let claimed_alice = m.claim(&alice);
    let claimed_bob = m.claim(&bob);
    assert!(claimed_alice > 0 && claimed_bob > 0, "both earned yield");
    assert_eq!(m.sy_balance(&alice), claimed_alice);
    assert_eq!(m.sy_balance(&bob), claimed_bob);

    // Conservation: total yield paid equals what one 100-YT holder would have
    // earned over 1.00 -> 1.20. The transfer neither lost nor duplicated yield.
    // owed_shares = 100 * (1/1.00 - 1/1.20) * WAD.
    let asset_yield = (100 * UNIT) * (rate_1_20 - WAD) / WAD;
    let single_holder = asset_yield * WAD / rate_1_20;
    assert!(
        (claimed_alice + claimed_bob - single_holder).abs() <= 4,
        "claimed {} + {} should equal single-holder {}",
        claimed_alice,
        claimed_bob,
        single_holder
    );

    // No PT was redeemed, so escrow still exactly covers the 100 units of
    // principal and nothing more (all yield was claimed out).
    let escrow_asset = m.escrow_shares() * m.rate() / WAD;
    assert!(
        (escrow_asset - 100 * UNIT).abs() <= 4,
        "escrow should hold only principal, {} asset units",
        escrow_asset
    );
}

/// Audit H4, end to end: the exact reproduced exploit sequence.
///
/// A YT transfer during a rate dip used to re-open an already-paid yield
/// interval. The old per-address rate checkpoint initialized a receiver that had
/// none to the CURRENT rate, so moving fully-paid-up YT to a fresh address while
/// the rate sat below its peak reset that YT's high-water mark downward. When the
/// rate merely returned to the peak, two claims stood against one surplus and the
/// first claimant took it, permanently transferring the other's entitlement.
///
/// The yield basis travels with the tokens, so a paid-up position stays paid up
/// wherever it lands and there is no address whose basis is "unset".
#[test]
fn transfer_during_a_dip_cannot_re_open_a_paid_yield_interval() {
    const RATE_1_05: i128 = 1_050_000_000_000_000_000;

    let m = deploy();

    // 1. Alice splits 100 UNIT at rate 1.00, the rate rises to 1.10, and she
    //    claims. Her position is now paid up to 1.10.
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    m.set_rate(RATE_1_10);
    let alice_paid = m.claim(&alice);
    assert_eq!(alice_paid, 90_909_090, "the audit's reproduced first claim");
    assert_eq!(
        YtTokenClient::new(&m.env, &m.yt).preview_claim_yield(&alice),
        0,
        "Alice is paid up at the peak"
    );

    // 2. The rate regresses to 1.05 and Carol splits a fresh, fully-funded
    //    position. Her SY genuinely backs a 1.05 -> 1.10 entitlement.
    m.set_rate(RATE_1_05);
    let carol = m.fund(100 * UNIT);
    m.deposit(&carol, 100 * UNIT);
    m.split(&carol, m.sy_balance(&carol));
    assert_eq!(m.escrow_shares(), 1_861_471_862, "the audit's escrow reading");

    // 3. The exploit: Alice moves her fully-paid-up YT to a fresh address she
    //    controls, which under the old code inherited a checkpoint of 1.05.
    let sock_puppet = m.account();
    m.transfer_yt(&alice, &sock_puppet, m.yt_balance(&alice));

    // 4. The rate merely returns to 1.10. No yield exists beyond what Alice was
    //    already paid, so the fresh address must be owed nothing.
    m.set_rate(RATE_1_10);

    let yt = YtTokenClient::new(&m.env, &m.yt);
    assert_eq!(
        yt.preview_claim_yield(&sock_puppet),
        0,
        "the fresh address must not inherit a re-opened interval"
    );

    // The surplus and Carol's entitlement, recomputed independently.
    let reservation = pt_face_reservation(m.pt_supply(), RATE_1_10);
    let surplus = m.escrow_shares() - reservation;
    assert_eq!(reservation, 1_818_181_818, "the audit's reservation reading");
    assert_eq!(surplus, 43_290_044, "the audit's surplus reading");

    let carol_owed = yt.preview_claim_yield(&carol);
    assert!(
        carol_owed > 0 && carol_owed <= surplus,
        "exactly one claim stands against the surplus: owed {} vs surplus {}",
        carol_owed,
        surplus
    );

    // The invariant the audit's harness reported failing at economics.rs:185.
    let holders = [&alice, &sock_puppet, &carol];
    m.assert_escrow_covers(&holders);
    m.assert_escrow_covers_yt_ledger();

    // Race the claims in the exploit's own order: the sock puppet first. It must
    // take nothing, and Carol must still collect her entitlement in full.
    assert_eq!(m.claim(&sock_puppet), 0, "no payout to the sock puppet");
    assert_eq!(m.sy_balance(&sock_puppet), 0);

    let carol_claimed = m.claim(&carol);
    assert_eq!(
        carol_claimed, carol_owed,
        "Carol's entitlement survives the sock puppet claiming first"
    );
    assert_eq!(m.sy_balance(&carol), carol_claimed);
    m.assert_escrow_covers(&holders);
    m.assert_escrow_covers_yt_ledger();

    // And Alice cannot re-claim from her now-empty address either.
    assert_eq!(m.claim(&alice), 0, "the sender kept nothing to re-claim");
}

/// Audit M4, the mirror image: re-using an address must not forfeit yield on YT
/// acquired later at a lower rate.
///
/// The old per-address checkpoint was a single high-water mark over the holder's
/// ENTIRE balance, so a holder settled at a peak who split a second, fully-funded
/// position after a dip inherited the old peak on all of it and was owed nothing
/// until the rate passed that peak — while a clean address doing the identical
/// thing was owed the full amount. The forfeited value sat in escrow with no
/// recovery path. An additive basis gives the new YT its own.
#[test]
fn re_using_an_address_does_not_forfeit_yield_on_yt_split_after_a_dip() {
    const RATE_1_05: i128 = 1_050_000_000_000_000_000;

    let m = deploy();

    // Alice settles at the 1.10 peak on a first position.
    let alice = m.fund(200 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    m.set_rate(RATE_1_10);
    assert!(m.claim(&alice) > 0, "the first position is paid at the peak");

    // The rate regresses to 1.05. Alice splits a second, fully-funded position
    // onto the SAME address; Dave splits an identical one onto a clean address.
    // Alice splits only the shares from the new deposit, leaving the SY she was
    // just paid alone, so the two second positions are the same size.
    m.set_rate(RATE_1_05);
    let alice_sy_before = m.sy_balance(&alice);
    m.deposit(&alice, 100 * UNIT);
    let alice_second = m.sy_balance(&alice) - alice_sy_before;
    m.split(&alice, alice_second);

    let dave = m.fund(100 * UNIT);
    m.deposit(&dave, 100 * UNIT);
    let dave_position = m.sy_balance(&dave);
    assert_eq!(dave_position, alice_second, "identical second positions");
    m.split(&dave, dave_position);

    // Both second positions are struck at 1.05, so both must earn the 1.05 ->
    // 1.10 recovery. Alice's older, already-paid half adds nothing on top.
    assert_eq!(
        m.yt_basis(&dave),
        m.yt_basis(&alice) - 909_090_910,
        "Alice's basis is her paid-up first position plus Dave's second position"
    );

    m.set_rate(RATE_1_10);
    let yt = YtTokenClient::new(&m.env, &m.yt);
    let alice_owed = yt.preview_claim_yield(&alice);
    let dave_owed = yt.preview_claim_yield(&dave);

    assert!(dave_owed > 0, "the clean address earns on the dip recovery");
    assert!(
        (alice_owed - dave_owed).abs() <= 2,
        "a re-used address must earn what a clean one does: alice {} vs dave {}",
        alice_owed,
        dave_owed
    );
    // The audit's measured figure for the clean address, within rounding.
    assert!(
        (dave_owed - 43_290_042).abs() <= 2,
        "the second position earns 1.05 -> 1.10 on ~100 UNIT: {}",
        dave_owed
    );

    // Both are actually payable: the escrow holds the surplus that funds them,
    // so the value is collected rather than stranded.
    let holders = [&alice, &dave];
    m.assert_escrow_covers(&holders);
    m.assert_escrow_covers_yt_ledger();

    let alice_claimed = m.claim(&alice);
    let dave_claimed = m.claim(&dave);
    assert_eq!(alice_claimed, alice_owed, "Alice collects in full");
    assert_eq!(dave_claimed, dave_owed, "Dave collects in full");
    m.assert_escrow_covers(&holders);
    m.assert_escrow_covers_yt_ledger();
}

/// Deterministic LCG so the random sequence is reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Step 10: conservation across a long random sequence of split / transfer /
/// claim / recombine, over a rate that moves BOTH WAYS, then full drain at
/// maturity. The escrow-coverage invariant must hold at every step (the
/// contract also asserts the PT half on every mutation), and the escrow must
/// drain to dust once everyone has claimed and redeemed. The economics code is
/// pure integer math, so the native test path and the wasm path are identical.
///
/// The rate arm used to bump the rate upward only, which is precisely why this
/// test was blind to audit H4 and M4: both are dip phenomena. A monotone rate
/// makes the per-address high-water checkpoint indistinguishable from correct
/// accounting, because the checkpoint never has an opportunity to regress on a
/// receiver or over-hold on a re-used sender. One rate move in four now
/// regresses, so the sequence walks through dips, recoveries, and transfers and
/// splits inside the dip window — the exact shape of both findings.
#[test]
fn conservation_holds_across_random_sequences() {
    const N: u64 = 10_000;
    let m = deploy();
    let holders: std::vec::Vec<Address> = (0..3).map(|_| m.fund(1_000_000 * UNIT)).collect();

    // Bare addresses holding no underlying: YT can only ever ARRIVE here, and
    // each is used at most once, so its first receipt is a transfer to an
    // address the yield ledger has never seen. This is the other half of what
    // made H4 invisible. With three addresses that all split at the start,
    // every transfer lands on someone who already has a yield history, and the
    // old code's "a receiver with no checkpoint starts at the current rate"
    // branch is never reached at all.
    let fresh: std::vec::Vec<Address> = (0..8).map(|_| m.account()).collect();
    let mut fresh_used: usize = 0;
    let mut fresh_receipts_in_dip: u64 = 0;

    let everyone: std::vec::Vec<Address> =
        holders.iter().chain(fresh.iter()).cloned().collect();
    let refs: std::vec::Vec<&Address> = everyone.iter().collect();
    let mut rng = Rng::new(0xC0FFEE);
    let mut rate = WAD;
    // The highest rate the market has ever seen. Yield is a high-water quantity,
    // so this is the reference point for both the recombine arm and the coverage
    // assertion below.
    let mut high_water = WAD;
    let mut value_ops: i128 = 0;
    let mut dips: u64 = 0;
    let mut recoveries: u64 = 0;

    for _ in 0..N {
        let h = &everyone[rng.below(everyone.len() as u64) as usize];
        match rng.below(5) {
            0 => {
                // deposit a random amount, then split all the SY now held.
                // Only the funded holders can do this; a bare address has no
                // underlying to deposit.
                let s = &holders[rng.below(holders.len() as u64) as usize];
                let amt = (1 + rng.below(50)) as i128 * UNIT;
                if m.underlying_balance(s) >= amt {
                    m.deposit(s, amt);
                    let sy = m.sy_balance(s);
                    if sy > 0 {
                        m.split(s, sy);
                        value_ops += 1;
                    }
                }
            }
            1 => {
                // Transfer a portion of YT to anyone. While the rate sits BELOW
                // its high-water mark, half the time route it to a never-used
                // bare address instead. That combination — a dip plus a receiver
                // the ledger has no history for — is the H4 exploit, and it is
                // the one shape a monotone rate over a closed set of holders can
                // never produce, which is why the property test was blind to it.
                // An attacker picks their moment; the pool makes the sequence
                // able to as well.
                let bal = m.yt_balance(h);
                if bal > 1 {
                    let dipped = rate < high_water;
                    let to_idx = if dipped && fresh_used < fresh.len() && rng.below(2) == 0 {
                        fresh_used += 1;
                        fresh_receipts_in_dip += 1;
                        holders.len() + fresh_used - 1
                    } else {
                        rng.below(everyone.len() as u64) as usize
                    };
                    let to = &everyone[to_idx];
                    let amount = 1 + (rng.below(bal as u64) as i128);
                    if amount <= bal {
                        m.transfer_yt(h, to, amount);
                    }
                }
            }
            2 => {
                m.claim(h);
                value_ops += 1;
            }
            3 => {
                // Recombine equal PT and YT. Held to the high-water rate on
                // purpose: below it the market is genuinely under-covered
                // (yield already paid at the peak has left the escrow), and the
                // tokenizer prices that with a pro-rata haircut computed
                // against PT supply alone, which does not reserve for YT's
                // junior claim. That is the tokenizer's accepted seniority
                // policy — covered by split_and_recombine_survive_a_rate_
                // regression — not a YT accounting question, and letting it run
                // here would measure the haircut rather than conservation.
                if rate >= high_water {
                    let pt = m.pt_balance(h);
                    let yt = m.yt_balance(h);
                    let max = if pt < yt { pt } else { yt };
                    if max > 0 {
                        let amount = 1 + (rng.below(max as u64) as i128);
                        if amount <= max {
                            m.recombine(h, amount);
                            value_ops += 1;
                        }
                    }
                }
            }
            _ => {
                // Move the rate. One move in four is a REGRESSION: the audit's
                // H4 and M4 both live entirely below the high-water mark, and
                // the original arm only ever bumped the rate upward, which is
                // why the 10k-step property test could not see either of them.
                // A dip re-opened an already-paid interval for YT moved to a
                // fresh address (H4) and stranded the yield on YT acquired
                // during the dip (M4); neither is reachable on a monotone path.
                if rng.below(4) == 0 {
                    // dip by up to ~2%, floored at 0.50 so the rate stays in a
                    // plausible regime for a yield source that can be slashed
                    let dip = (rng.below(20_000_000_000_000_000) + 1) as i128;
                    if rate - dip > WAD / 2 {
                        rate -= dip;
                        dips += 1;
                    }
                } else {
                    // bump the rate up by 0 to ~2%
                    rate += (rng.below(20_000_000_000_000_000) + 1) as i128;
                    if rate >= high_water {
                        recoveries += 1;
                    }
                }
                if rate > high_water {
                    high_water = rate;
                }
                m.set_rate(rate);
            }
        }

        // The rate-independent invariant holds at EVERY step, dip or not: the
        // escrow always covers the SY backing every outstanding YT position at
        // the rates it was acquired at, plus every share already banked. This
        // is what H4 broke — the exploit manufactured basis out of a dip — and
        // it is checkable in two contract reads thanks to the aggregate ledger.
        m.assert_escrow_covers_yt_ledger();

        // The rate-dependent PT+YT form is asserted at or above the high-water
        // mark. Strictly below it the market can be legitimately under-covered:
        // yield paid out at an earlier peak has already left the escrow, so PT
        // face at the dipped rate exceeds what remains. That shortfall is the
        // priced haircut (redemption_is_capped_when_rate_regresses), not a
        // double count. Every dip in this run is followed by a recovery, so the
        // assertion still fires across the whole exploit window.
        if rate >= high_water {
            m.assert_escrow_covers(&refs);
        }
    }

    assert!(
        dips >= 100 && recoveries >= 100,
        "the regression arm must actually exercise dips and recoveries: {} dips, {} recoveries",
        dips,
        recoveries
    );
    assert_eq!(
        fresh_receipts_in_dip as usize,
        fresh.len(),
        "every bare address must have taken its first YT during a dip, or this \
         sequence never reaches the H4 shape it is here to cover"
    );

    // End the term recovered, so every entitlement earned across the dips is
    // claimable and the drain below is exact rather than a haircut.
    rate = high_water;
    m.set_rate(rate);
    m.observe();
    m.assert_escrow_covers(&refs);
    m.assert_escrow_covers_yt_ledger();

    // Drain everything at maturity: claim all yield, then redeem all PT.
    m.env.ledger().set_timestamp(MATURITY + 1);
    for h in &everyone {
        m.claim(h);
    }
    m.assert_escrow_covers(&refs);
    for h in &everyone {
        let pt = m.pt_balance(h);
        if pt > 0 {
            m.redeem_pt(h, pt);
            value_ops += 1;
        }
    }

    // Half the conservation proof is that the coverage invariants held at every
    // one of the N steps above: the escrow was never short, so no holder could
    // be underpaid, and every claim/redeem transfer succeeded.
    //
    // The other half is this bound, and over a rate that moves both ways it is
    // the sharper of the two. Coverage is a one-sided test: it catches yield
    // being claimed twice but not yield that can never be claimed at all, and
    // in a long mixed sequence the two offset. Under the old per-address
    // checkpoint every split onto an address already settled at a higher rate
    // stranded that whole new position's yield in escrow (M4), which quietly
    // funded the phantom claims a dip-window transfer to a fresh address opened
    // (H4) — so coverage alone stayed green while both bugs were live. What
    // cannot hide is the escrow at the end of the term: once every holder has
    // claimed and every PT is redeemed, anything left is value no one could
    // reach. Measured 7425 shares left under the old model against 349 here,
    // and 349 is pure rounding: each value-moving op, and each transfer that
    // slices a yield basis, rounds a division in the escrow's favor by under a
    // share, so the residue is bounded by the op count, not by the values.
    let left = m.escrow_shares();
    assert!(
        left <= value_ops / 2 + 64,
        "leftover escrow {} exceeds the rounding-dust bound for {} value ops: \
         value that no holder can reach is stranded, not rounded",
        left,
        value_ops
    );
}

// --- protocol fee on claimed yield -----------------------------------------
//
// The fee is taken from `pay` -- the value AFTER the PT-senior cap -- so it is
// structurally incapable of reaching PT principal. These tests pin that, plus
// the two properties that make the fee safe to add to an immutable market at
// all: it does not move the exchange rate, and it cannot be introduced later.

/// 5% of claimed yield, comparable to what peer protocols charge.
const FEE_500_BPS: i128 = 500;

#[test]
fn the_protocol_fee_is_skimmed_from_the_holders_yield_not_from_principal() {
    let m = deploy_with_fee(FEE_500_BPS);
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    let yt = YtTokenClient::new(&m.env, &m.yt);
    m.set_rate(RATE_1_10);

    let owed = yt.preview_claim_yield(&alice);
    assert!(owed > 0, "precondition: there is yield to claim");

    let escrow_before = m.escrow_shares();
    let claimed = m.claim(&alice);
    let fee_paid = m.sy_balance(&m.fee_recipient);

    // The holder is paid net; the recipient is paid the fee; together they are
    // exactly what left escrow. No third party, no rounding leak.
    assert_eq!(fee_paid, owed * FEE_500_BPS / 10_000, "fee is 5% of the claim");
    assert_eq!(claimed, owed - fee_paid, "claim() returns the NET amount");
    assert_eq!(
        m.sy_balance(&alice),
        claimed,
        "the holder receives exactly the net amount"
    );
    assert_eq!(
        escrow_before - m.escrow_shares(),
        claimed + fee_paid,
        "escrow falls by exactly the gross, so nothing is created or stranded"
    );
}

#[test]
fn the_protocol_fee_never_eats_into_pt_coverage() {
    // A dip leaves only a partial surplus over the PT reservation. The fee must
    // come out of that surplus -- never out of the escrow PT needs -- so PT
    // stays exactly as covered as it would be in a fee-free market.
    let m = deploy_with_fee(FEE_500_BPS);
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    m.set_rate(RATE_1_10);
    let rate_1_05: i128 = 1_050_000_000_000_000_000;
    m.set_rate(rate_1_05);

    let escrow_before = m.escrow_shares();
    let reservation = pt_face_reservation(m.pt_supply(), rate_1_05);
    let surplus = escrow_before - reservation;
    assert!(surplus > 0, "precondition: a partial surplus exists");

    let claimed = m.claim(&alice);
    let fee_paid = m.sy_balance(&m.fee_recipient);

    assert_eq!(
        claimed + fee_paid,
        surplus,
        "holder plus protocol together take exactly the junior surplus"
    );
    assert!(
        m.escrow_shares() >= pt_face_reservation(m.pt_supply(), rate_1_05),
        "PT reservation survives the fee: escrow {} < reservation {}",
        m.escrow_shares(),
        pt_face_reservation(m.pt_supply(), rate_1_05)
    );
}

#[test]
fn the_protocol_fee_does_not_move_the_exchange_rate() {
    // This is the property that lets a fee exist at all in a derived-rate
    // system: the skim moves SY shares between escrow and the recipient, so
    // neither the strategy's assets nor the SY supply changes.
    let m = deploy_with_fee(FEE_500_BPS);
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    m.set_rate(RATE_1_10);

    let sy = SyWrapperClient::new(&m.env, &m.sy);
    let rate_before = sy.exchange_rate();
    let supply_before = sy.total_supply();

    let claimed = m.claim(&alice);
    assert!(claimed > 0);

    assert_eq!(sy.exchange_rate(), rate_before, "rate is untouched by the fee");
    assert_eq!(sy.total_supply(), supply_before, "SY supply is untouched");
}

#[test]
fn a_zero_fee_market_pays_the_holder_everything() {
    // The default. Proves the fee path is inert at 0 rather than merely small,
    // so every other test in this file is measuring unfee'd behaviour.
    let m = deploy_with_fee(0);
    let alice = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.split(&alice, 100 * UNIT);

    let yt = YtTokenClient::new(&m.env, &m.yt);
    m.set_rate(RATE_1_10);
    let owed = yt.preview_claim_yield(&alice);

    let claimed = m.claim(&alice);
    assert_eq!(claimed, owed, "the holder receives the whole claim");
    assert_eq!(
        m.sy_balance(&m.fee_recipient),
        0,
        "no fee is paid, and the recipient is never credited"
    );
}

/// The fee path had no property coverage: every fuzz and conservation test runs
/// through `deploy()`, which is fee-free, so the random walk had never executed
/// a single line of the skim. This walks a fee-bearing market through rate rises
/// AND regressions, claiming at every step, and asserts the identity the fee
/// depends on — every stroop that leaves escrow lands on either the holder or
/// the recipient — plus the seniority bound, at every transition.
#[test]
fn a_fee_bearing_market_conserves_escrow_across_a_rate_walk() {
    let m = deploy_with_fee(FEE_500_BPS);
    let alice = m.fund(100 * UNIT);
    let bob = m.fund(100 * UNIT);
    m.deposit(&alice, 100 * UNIT);
    m.deposit(&bob, 100 * UNIT);
    m.split(&alice, 100 * UNIT);
    m.split(&bob, 100 * UNIT);

    // Up, down, up again: a regression is the case where `pay` is capped by the
    // surplus, which is precisely where a fee could eat into PT if it were
    // taken from `owed` instead of from `pay`.
    // Alice claims at each rising step, draining escrow at the peak. The walk
    // then ends at a rate where the arithmetic leaves barely anything: escrow
    // sits near 1.769e9 after her claims, so the PT reservation (2e9 / rate)
    // only drops below it above ~1.1305. At 1.1315 the surplus is ~1.67M
    // against Bob's owed of ~122.8M.
    //
    // That rate is chosen deliberately, not just for being a partial cap. A fee
    // taken from `owed` instead of `pay` is `owed * bps` regardless of what the
    // surplus can afford, so once `pay < owed * bps / 10_000` the fee exceeds
    // the whole payout: `net` goes negative, the `if net > 0` guard skips the
    // holder push, and `push_token(fee)` still fires -- taking escrow BELOW the
    // PT reservation and returning a negative number. That only happens in a
    // narrow band just above the crossover, roughly (1.1305, 1.1315). A partial
    // cap further up (1.14, say) is caught only by the bps-ratio assertion,
    // because there `net + fee == pay` still holds and PT is never touched.
    let walk = [
        1_050_000_000_000_000_000i128,
        1_200_000_000_000_000_000,
        1_300_000_000_000_000_000,
        1_100_000_000_000_000_000, // regression: escrow short, claims pay nothing
        1_131_500_000_000_000_000, // partial cap, and inside the PT-breach band
    ];

    let mut protocol_total = 0i128;
    let mut saw_partial_cap = false;
    let mut high_water = m.rate();
    let yt = YtTokenClient::new(&m.env, &m.yt);

    // Only Alice claims during the walk. Bob keeps a basis from rate 1.0 and
    // never settles, so his owed grows while Alice's claims drain escrow —
    // which is what eventually produces a surplus that is positive but smaller
    // than what he is owed. Without that asymmetry every claim is either
    // fully paid or paid nothing, and the partial-cap arm this test exists to
    // cover is never entered.
    for (step, rate) in walk.iter().enumerate() {
        m.set_rate(*rate);
        if *rate > high_water {
            high_water = *rate;
        }

        for (who, holder) in [("alice", &alice), ("bob", &bob)] {
            // Bob only claims on the final step.
            if who == "bob" && step + 1 < walk.len() {
                continue;
            }

            let owed_before = yt.preview_claim_yield(holder);
            let escrow_before = m.escrow_shares();
            let holder_before = m.sy_balance(holder);
            let fee_before = m.sy_balance(&m.fee_recipient);

            let returned = m.claim(holder);

            let escrow_out = escrow_before - m.escrow_shares();
            let to_holder = m.sy_balance(holder) - holder_before;
            let to_protocol = m.sy_balance(&m.fee_recipient) - fee_before;
            protocol_total += to_protocol;
            if escrow_out > 0 && escrow_out < owed_before {
                saw_partial_cap = true;
            }

            assert_eq!(
                returned, to_holder,
                "step {step} {who}: claim_yield must return the NET the holder received"
            );
            assert_eq!(
                escrow_out,
                to_holder + to_protocol,
                "step {step} {who}: every stroop leaving escrow lands on the holder or the protocol"
            );
            assert!(
                to_protocol * 10_000 <= (to_holder + to_protocol) * FEE_500_BPS,
                "step {step} {who}: the protocol never takes more than its bps of the payout"
            );

            let reservation = pt_face_reservation(m.pt_supply(), *rate);
            if escrow_before >= reservation {
                assert!(
                    m.escrow_shares() >= reservation,
                    "step {step} {who}: the claim pushed escrow {} below the PT reservation {}",
                    m.escrow_shares(),
                    reservation
                );
            } else {
                assert_eq!(
                    escrow_out, 0,
                    "step {step} {who}: escrow was already short, so the claim must pay nothing"
                );
            }
        }

        // The rate-INDEPENDENT ledger invariant holds at every step, including
        // the regressions, because `consume` takes the gross. The fuzz asserts
        // this one ungated and only gates the rate-priced form below; keeping
        // just the gated half would leave the regression steps asserting
        // nothing at all.
        m.assert_escrow_covers_yt_ledger();
        if *rate >= high_water {
            m.assert_escrow_covers(&[&alice, &bob]);
        }
    }

    assert!(
        protocol_total > 0,
        "the walk must actually exercise the fee path, not just the zero branch"
    );
    assert!(
        saw_partial_cap,
        "the walk never produced a partial cap (0 < pay < owed), so the arm where \
         a fee could eat into PT is untested -- taking the fee from `owed` instead \
         of `pay` would pass this test"
    );
}
