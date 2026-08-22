// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_family = "wasm", no_std)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, Env,
    MuxedAddress, Vec, I256,
};

const WAD: i128 = 1_000_000_000_000_000_000;
const BPS_DENOMINATOR: i128 = 10_000;
const MAX_TAKER_FEE_BPS: i128 = 1_000;
const MAX_PAGE_SIZE: u32 = 50;
/// Orders fit inside the 120-day persistent-entry TTL even if nobody touches
/// them between placement and expiry.
const MAX_ORDER_LIFETIME_SECONDS: u64 = 90 * 24 * 60 * 60;
const LEDGERS_PER_DAY: u32 = 17_280;
const TTL_THRESHOLD_LEDGERS: u32 = 30 * LEDGERS_PER_DAY;
const TTL_EXTEND_TO_LEDGERS: u32 = 120 * LEDGERS_PER_DAY;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Config {
    pub admin: Address,
    pub pt_token: Address,
    pub sy_token: Address,
    pub maturity: u64,
    pub fee_recipient: Address,
    pub taker_fee_bps: i128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[contracttype]
pub enum Side {
    /// A resting maker sells PT and receives SY.
    Ask,
    /// A resting maker buys PT and escrows SY.
    Bid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Order {
    pub id: u64,
    pub maker: Address,
    pub side: Side,
    /// SY shares per PT face unit, scaled by 1e18.
    pub price_wad: i128,
    pub original_base: i128,
    pub remaining_base: i128,
    /// PT for asks and SY for bids still held by the contract for this order.
    pub escrow_remaining: i128,
    pub expiry: u64,
    pub created_at: u64,
    pub prev: Option<u64>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct FillReceipt {
    pub order_id: u64,
    pub maker: Address,
    pub taker: Address,
    pub side: Side,
    pub base_filled: i128,
    pub quote_amount: i128,
    pub taker_fee: i128,
    pub remaining_base: i128,
}

#[derive(Clone)]
#[contracttype]
enum DataKey {
    Config,
    NextOrderId,
    AskHead,
    BidHead,
    OpenCount,
    Order(u64),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
#[contracterror]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidMaturity = 3,
    InvalidAmount = 4,
    InvalidPrice = 5,
    InvalidExpiry = 6,
    InvalidFee = 7,
    NotAdmin = 8,
    OrderNotFound = 9,
    NotMaker = 10,
    WrongSide = 11,
    InvalidPredecessor = 12,
    OrderWouldCross = 13,
    NotBestOrder = 14,
    LimitPriceExceeded = 15,
    OrderExpired = 16,
    MarketMatured = 17,
    MathOverflow = 18,
    PageTooLarge = 19,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderPlaced {
    pub id: u64,
    pub maker: Address,
    pub side: Side,
    pub price_wad: i128,
    pub base_amount: i128,
    pub expiry: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderFilled {
    pub id: u64,
    pub maker: Address,
    pub taker: Address,
    pub base_filled: i128,
    pub quote_amount: i128,
    pub taker_fee: i128,
    pub remaining_base: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderCancelled {
    pub id: u64,
    pub maker: Address,
    pub remaining_base: i128,
    pub expired: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeSet {
    pub admin: Address,
    pub old_fee_bps: i128,
    pub new_fee_bps: i128,
}

#[contract]
pub struct Orderbook;

#[contractimpl]
impl Orderbook {
    pub fn initialize(
        env: Env,
        admin: Address,
        pt_token: Address,
        sy_token: Address,
        maturity: u64,
        fee_recipient: Address,
        taker_fee_bps: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        if maturity <= env.ledger().timestamp() {
            return Err(Error::InvalidMaturity);
        }
        require_fee(taker_fee_bps)?;

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                pt_token,
                sy_token,
                maturity,
                fee_recipient,
                taker_fee_bps,
            },
        );
        env.storage().instance().set(&DataKey::NextOrderId, &1_u64);
        env.storage().instance().set(&DataKey::OpenCount, &0_u64);
        bump_instance_ttl(&env);
        Ok(())
    }

    pub fn config(env: Env) -> Result<Config, Error> {
        read_config(&env)
    }

    pub fn set_fee(env: Env, admin: Address, taker_fee_bps: i128) -> Result<(), Error> {
        let mut config = read_config(&env)?;
        admin.require_auth();
        if admin != config.admin {
            return Err(Error::NotAdmin);
        }
        require_fee(taker_fee_bps)?;
        let old_fee_bps = config.taker_fee_bps;
        config.taker_fee_bps = taker_fee_bps;
        env.storage().instance().set(&DataKey::Config, &config);
        bump_instance_ttl(&env);
        FeeSet {
            admin,
            old_fee_bps,
            new_fee_bps: taker_fee_bps,
        }
        .publish(&env);
        Ok(())
    }

    pub fn get_order(env: Env, order_id: u64) -> Option<Order> {
        read_order(&env, order_id)
    }

    pub fn best_order(env: Env, side: Side) -> Option<Order> {
        read_head(&env, side).and_then(|id| read_order(&env, id))
    }

    pub fn open_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::OpenCount)
            .unwrap_or(0)
    }

    /// Returns active orders in price-time priority. If `cursor` is supplied,
    /// the page begins after that order.
    pub fn list_orders(
        env: Env,
        side: Side,
        cursor: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Order>, Error> {
        read_config(&env)?;
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(Error::PageTooLarge);
        }
        let mut next = match cursor {
            Some(id) => {
                let order = read_order(&env, id).ok_or(Error::OrderNotFound)?;
                if order.side != side {
                    return Err(Error::WrongSide);
                }
                order.next
            }
            None => read_head(&env, side),
        };
        let mut result = Vec::new(&env);
        while result.len() < limit {
            let Some(id) = next else { break };
            let order = read_order(&env, id).ok_or(Error::OrderNotFound)?;
            next = order.next;
            result.push_back(order);
        }
        Ok(result)
    }

    /// Places a non-marketable resting order. The caller supplies the intended
    /// predecessor; local ordering checks make insertion O(1) while the linked
    /// list still enforces price-time priority on-chain.
    pub fn place_order(
        env: Env,
        maker: Address,
        side: Side,
        base_amount: i128,
        price_wad: i128,
        expiry: u64,
        predecessor: Option<u64>,
    ) -> Result<u64, Error> {
        let config = read_live_config(&env)?;
        maker.require_auth();
        if base_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if price_wad <= 0 {
            return Err(Error::InvalidPrice);
        }
        let now = env.ledger().timestamp();
        if expiry <= now || expiry > config.maturity || expiry - now > MAX_ORDER_LIFETIME_SECONDS {
            return Err(Error::InvalidExpiry);
        }
        reject_crossing_order(&env, side, price_wad)?;

        let id = next_order_id(&env)?;
        let (prev, next) = insertion_neighbors(&env, side, price_wad, predecessor)?;
        let escrow_remaining = match side {
            Side::Ask => base_amount,
            Side::Bid => cumulative_quote(&env, base_amount, price_wad)?,
        };
        if escrow_remaining <= 0 {
            return Err(Error::InvalidAmount);
        }

        let escrow_token = match side {
            Side::Ask => &config.pt_token,
            Side::Bid => &config.sy_token,
        };
        transfer(
            &env,
            escrow_token,
            &maker,
            &env.current_contract_address(),
            escrow_remaining,
        );

        let order = Order {
            id,
            maker: maker.clone(),
            side,
            price_wad,
            original_base: base_amount,
            remaining_base: base_amount,
            escrow_remaining,
            expiry,
            created_at: env.ledger().timestamp(),
            prev,
            next,
        };
        link_order(&env, &order)?;
        write_order(&env, &order);
        increment_open_count(&env)?;
        bump_instance_ttl(&env);

        OrderPlaced {
            id,
            maker,
            side,
            price_wad,
            base_amount,
            expiry,
        }
        .publish(&env);
        Ok(id)
    }

    /// Fills the best resting order on `resting_side`. The maker's limit price
    /// is the execution price. `limit_price_wad` protects the taker: it is a
    /// maximum when taking an ask and a minimum when taking a bid.
    pub fn fill_best(
        env: Env,
        taker: Address,
        resting_side: Side,
        base_amount: i128,
        limit_price_wad: i128,
    ) -> Result<FillReceipt, Error> {
        let config = read_live_config(&env)?;
        taker.require_auth();
        if base_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if limit_price_wad <= 0 {
            return Err(Error::InvalidPrice);
        }
        let id = read_head(&env, resting_side).ok_or(Error::OrderNotFound)?;
        let mut order = read_order(&env, id).ok_or(Error::OrderNotFound)?;
        if order.side != resting_side {
            return Err(Error::WrongSide);
        }
        if order.expiry <= env.ledger().timestamp() {
            return Err(Error::OrderExpired);
        }
        match resting_side {
            Side::Ask if order.price_wad > limit_price_wad => {
                return Err(Error::LimitPriceExceeded)
            }
            Side::Bid if order.price_wad < limit_price_wad => {
                return Err(Error::LimitPriceExceeded)
            }
            _ => {}
        }
        if base_amount > order.remaining_base {
            return Err(Error::InvalidAmount);
        }

        let filled_before = order.original_base - order.remaining_base;
        let quote_before = cumulative_quote(&env, filled_before, order.price_wad)?;
        let quote_after = cumulative_quote(&env, filled_before + base_amount, order.price_wad)?;
        let quote_amount = quote_after - quote_before;
        if quote_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let taker_fee = mul_div_floor(&env, quote_amount, config.taker_fee_bps, BPS_DENOMINATOR)?;
        let escrow = env.current_contract_address();

        match resting_side {
            Side::Ask => {
                let total_due = quote_amount
                    .checked_add(taker_fee)
                    .ok_or(Error::MathOverflow)?;
                transfer(&env, &config.sy_token, &taker, &escrow, total_due);
                transfer(&env, &config.sy_token, &escrow, &order.maker, quote_amount);
                if taker_fee > 0 {
                    transfer(
                        &env,
                        &config.sy_token,
                        &escrow,
                        &config.fee_recipient,
                        taker_fee,
                    );
                }
                transfer(&env, &config.pt_token, &escrow, &taker, base_amount);
                order.escrow_remaining -= base_amount;
            }
            Side::Bid => {
                if quote_amount > order.escrow_remaining {
                    return Err(Error::MathOverflow);
                }
                transfer(&env, &config.pt_token, &taker, &escrow, base_amount);
                transfer(&env, &config.pt_token, &escrow, &order.maker, base_amount);
                transfer(
                    &env,
                    &config.sy_token,
                    &escrow,
                    &taker,
                    quote_amount - taker_fee,
                );
                if taker_fee > 0 {
                    transfer(
                        &env,
                        &config.sy_token,
                        &escrow,
                        &config.fee_recipient,
                        taker_fee,
                    );
                }
                order.escrow_remaining -= quote_amount;
            }
        }

        order.remaining_base -= base_amount;
        let remaining_base = order.remaining_base;
        if remaining_base == 0 {
            unlink_order(&env, &order)?;
            remove_order(&env, order.id);
            decrement_open_count(&env)?;
        } else {
            write_order(&env, &order);
        }
        bump_instance_ttl(&env);

        OrderFilled {
            id,
            maker: order.maker.clone(),
            taker: taker.clone(),
            base_filled: base_amount,
            quote_amount,
            taker_fee,
            remaining_base,
        }
        .publish(&env);
        Ok(FillReceipt {
            order_id: id,
            maker: order.maker,
            taker,
            side: resting_side,
            base_filled: base_amount,
            quote_amount,
            taker_fee,
            remaining_base,
        })
    }

    pub fn cancel_order(env: Env, maker: Address, order_id: u64) -> Result<(), Error> {
        let config = read_config(&env)?;
        maker.require_auth();
        let order = read_order(&env, order_id).ok_or(Error::OrderNotFound)?;
        if order.maker != maker {
            return Err(Error::NotMaker);
        }
        close_order(&env, &config, order, false)
    }

    /// Permissionlessly removes expired head orders and refunds their makers.
    /// Keeping this bounded avoids a single invocation doing unbounded work.
    pub fn prune_expired(env: Env, side: Side, max_orders: u32) -> Result<u32, Error> {
        let config = read_config(&env)?;
        if max_orders == 0 || max_orders > MAX_PAGE_SIZE {
            return Err(Error::PageTooLarge);
        }
        let mut pruned = 0_u32;
        while pruned < max_orders {
            let Some(id) = read_head(&env, side) else {
                break;
            };
            let order = read_order(&env, id).ok_or(Error::OrderNotFound)?;
            if order.expiry > env.ledger().timestamp() {
                break;
            }
            close_order(&env, &config, order, true)?;
            pruned += 1;
        }
        Ok(pruned)
    }
}

fn read_config(env: &Env) -> Result<Config, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(Error::NotInitialized)
}

fn read_live_config(env: &Env) -> Result<Config, Error> {
    let config = read_config(env)?;
    if env.ledger().timestamp() >= config.maturity {
        return Err(Error::MarketMatured);
    }
    Ok(config)
}

fn require_fee(fee_bps: i128) -> Result<(), Error> {
    if !(0..=MAX_TAKER_FEE_BPS).contains(&fee_bps) {
        return Err(Error::InvalidFee);
    }
    Ok(())
}

fn next_order_id(env: &Env) -> Result<u64, Error> {
    let id: u64 = env
        .storage()
        .instance()
        .get(&DataKey::NextOrderId)
        .unwrap_or(1);
    let next = id.checked_add(1).ok_or(Error::MathOverflow)?;
    env.storage().instance().set(&DataKey::NextOrderId, &next);
    Ok(id)
}

fn head_key(side: Side) -> DataKey {
    match side {
        Side::Ask => DataKey::AskHead,
        Side::Bid => DataKey::BidHead,
    }
}

fn read_head(env: &Env, side: Side) -> Option<u64> {
    env.storage().instance().get(&head_key(side))
}

fn write_head(env: &Env, side: Side, id: Option<u64>) {
    let key = head_key(side);
    match id {
        Some(value) => env.storage().instance().set(&key, &value),
        None => env.storage().instance().remove(&key),
    }
}

fn read_order(env: &Env, id: u64) -> Option<Order> {
    let key = DataKey::Order(id);
    let order = env.storage().persistent().get(&key);
    if order.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
    }
    order
}

fn write_order(env: &Env, order: &Order) {
    let key = DataKey::Order(order.id);
    env.storage().persistent().set(&key, order);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
}

fn remove_order(env: &Env, id: u64) {
    env.storage().persistent().remove(&DataKey::Order(id));
}

fn insertion_neighbors(
    env: &Env,
    side: Side,
    price_wad: i128,
    predecessor: Option<u64>,
) -> Result<(Option<u64>, Option<u64>), Error> {
    let successor = match predecessor {
        Some(id) => {
            let previous = read_order(env, id).ok_or(Error::InvalidPredecessor)?;
            if previous.side != side || !ordered_before(side, previous.price_wad, price_wad, false)
            {
                return Err(Error::InvalidPredecessor);
            }
            previous.next
        }
        None => read_head(env, side),
    };
    if let Some(id) = successor {
        let next = read_order(env, id).ok_or(Error::InvalidPredecessor)?;
        if next.side != side || !ordered_before(side, price_wad, next.price_wad, true) {
            return Err(Error::InvalidPredecessor);
        }
    }
    Ok((predecessor, successor))
}

/// `strict` is used against the successor to force new equal-price orders
/// behind every existing order at that price.
fn ordered_before(side: Side, left: i128, right: i128, strict: bool) -> bool {
    match (side, strict) {
        (Side::Ask, false) => left <= right,
        (Side::Ask, true) => left < right,
        (Side::Bid, false) => left >= right,
        (Side::Bid, true) => left > right,
    }
}

fn reject_crossing_order(env: &Env, side: Side, price_wad: i128) -> Result<(), Error> {
    let opposite = match side {
        Side::Ask => Side::Bid,
        Side::Bid => Side::Ask,
    };
    let Some(best) = read_head(env, opposite).and_then(|id| read_order(env, id)) else {
        return Ok(());
    };
    let crosses = match side {
        Side::Ask => price_wad <= best.price_wad,
        Side::Bid => price_wad >= best.price_wad,
    };
    if crosses {
        return Err(Error::OrderWouldCross);
    }
    Ok(())
}

fn link_order(env: &Env, order: &Order) -> Result<(), Error> {
    if let Some(prev_id) = order.prev {
        let mut prev = read_order(env, prev_id).ok_or(Error::InvalidPredecessor)?;
        prev.next = Some(order.id);
        write_order(env, &prev);
    } else {
        write_head(env, order.side, Some(order.id));
    }
    if let Some(next_id) = order.next {
        let mut next = read_order(env, next_id).ok_or(Error::InvalidPredecessor)?;
        next.prev = Some(order.id);
        write_order(env, &next);
    }
    Ok(())
}

fn unlink_order(env: &Env, order: &Order) -> Result<(), Error> {
    if let Some(prev_id) = order.prev {
        let mut prev = read_order(env, prev_id).ok_or(Error::OrderNotFound)?;
        prev.next = order.next;
        write_order(env, &prev);
    } else {
        write_head(env, order.side, order.next);
    }
    if let Some(next_id) = order.next {
        let mut next = read_order(env, next_id).ok_or(Error::OrderNotFound)?;
        next.prev = order.prev;
        write_order(env, &next);
    }
    Ok(())
}

fn close_order(env: &Env, config: &Config, order: Order, expired: bool) -> Result<(), Error> {
    unlink_order(env, &order)?;
    remove_order(env, order.id);
    decrement_open_count(env)?;
    let token_id = match order.side {
        Side::Ask => &config.pt_token,
        Side::Bid => &config.sy_token,
    };
    transfer(
        env,
        token_id,
        &env.current_contract_address(),
        &order.maker,
        order.escrow_remaining,
    );
    bump_instance_ttl(env);
    OrderCancelled {
        id: order.id,
        maker: order.maker,
        remaining_base: order.remaining_base,
        expired,
    }
    .publish(env);
    Ok(())
}

fn increment_open_count(env: &Env) -> Result<(), Error> {
    let count: u64 = env
        .storage()
        .instance()
        .get(&DataKey::OpenCount)
        .unwrap_or(0);
    env.storage().instance().set(
        &DataKey::OpenCount,
        &count.checked_add(1).ok_or(Error::MathOverflow)?,
    );
    Ok(())
}

fn decrement_open_count(env: &Env) -> Result<(), Error> {
    let count: u64 = env
        .storage()
        .instance()
        .get(&DataKey::OpenCount)
        .unwrap_or(0);
    env.storage().instance().set(
        &DataKey::OpenCount,
        &count.checked_sub(1).ok_or(Error::MathOverflow)?,
    );
    Ok(())
}

fn cumulative_quote(env: &Env, base_amount: i128, price_wad: i128) -> Result<i128, Error> {
    if base_amount == 0 {
        return Ok(0);
    }
    mul_div_ceil(env, base_amount, price_wad, WAD)
}

fn mul_div_floor(env: &Env, a: i128, b: i128, c: i128) -> Result<i128, Error> {
    I256::from_i128(env, a)
        .mul(&I256::from_i128(env, b))
        .div(&I256::from_i128(env, c))
        .to_i128()
        .ok_or(Error::MathOverflow)
}

fn mul_div_ceil(env: &Env, a: i128, b: i128, c: i128) -> Result<i128, Error> {
    let denominator = I256::from_i128(env, c);
    I256::from_i128(env, a)
        .mul(&I256::from_i128(env, b))
        .add(&denominator)
        .sub(&I256::from_i128(env, 1))
        .div(&denominator)
        .to_i128()
        .ok_or(Error::MathOverflow)
}

fn transfer(env: &Env, token_id: &Address, from: &Address, to: &Address, amount: i128) {
    if amount <= 0 {
        return;
    }
    let to = MuxedAddress::from(to);
    token::TokenClient::new(env, token_id).transfer(from, &to, &amount);
}

fn bump_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        token, Env,
    };

    const NOW: u64 = 1_780_000_000;
    const MATURITY: u64 = NOW + 90 * 24 * 60 * 60;
    const UNIT: i128 = 10_000_000;
    const PRICE: i128 = 950_000_000_000_000_000;

    struct Fixture {
        env: Env,
        admin: Address,
        maker: Address,
        maker_two: Address,
        taker: Address,
        fee_recipient: Address,
        pt: Address,
        sy: Address,
        client: OrderbookClient<'static>,
    }

    fn fixture() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|ledger| ledger.timestamp = NOW);
        let admin = Address::generate(&env);
        let maker = Address::generate(&env);
        let maker_two = Address::generate(&env);
        let taker = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        let pt = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let sy = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let id = env.register(Orderbook, ());
        let client = OrderbookClient::new(&env, &id);
        client.initialize(&admin, &pt, &sy, &MATURITY, &fee_recipient, &25);
        token::StellarAssetClient::new(&env, &pt).mint(&maker, &(1_000 * UNIT));
        token::StellarAssetClient::new(&env, &pt).mint(&maker_two, &(1_000 * UNIT));
        token::StellarAssetClient::new(&env, &pt).mint(&taker, &(1_000 * UNIT));
        token::StellarAssetClient::new(&env, &sy).mint(&maker, &(1_000 * UNIT));
        token::StellarAssetClient::new(&env, &sy).mint(&maker_two, &(1_000 * UNIT));
        token::StellarAssetClient::new(&env, &sy).mint(&taker, &(1_000 * UNIT));
        Fixture {
            env,
            admin,
            maker,
            maker_two,
            taker,
            fee_recipient,
            pt,
            sy,
            client,
        }
    }

    fn balance(env: &Env, token_id: &Address, holder: &Address) -> i128 {
        token::TokenClient::new(env, token_id).balance(holder)
    }

    #[test]
    fn asks_are_price_time_ordered_and_escrowed() {
        let f = fixture();
        let later = f.client.place_order(
            &f.maker,
            &Side::Ask,
            &(100 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &None,
        );
        let better = f.client.place_order(
            &f.maker_two,
            &Side::Ask,
            &(50 * UNIT),
            &(PRICE - WAD / 100),
            &(NOW + 1_000),
            &None,
        );
        let equal = f.client.place_order(
            &f.maker_two,
            &Side::Ask,
            &(25 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &Some(later),
        );

        let orders = f.client.list_orders(&Side::Ask, &None, &10);
        assert_eq!(orders.len(), 3);
        assert_eq!(orders.get(0).unwrap().id, better);
        assert_eq!(orders.get(1).unwrap().id, later);
        assert_eq!(orders.get(2).unwrap().id, equal);
        assert_eq!(f.client.open_count(), 3);
        assert_eq!(balance(&f.env, &f.pt, &f.client.address), 175 * UNIT);
    }

    #[test]
    fn ask_partial_fill_charges_fee_and_preserves_priority() {
        let f = fixture();
        let id = f.client.place_order(
            &f.maker,
            &Side::Ask,
            &(100 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &None,
        );
        let maker_sy_before = balance(&f.env, &f.sy, &f.maker);
        let taker_pt_before = balance(&f.env, &f.pt, &f.taker);
        let receipt = f
            .client
            .fill_best(&f.taker, &Side::Ask, &(40 * UNIT), &PRICE);

        assert_eq!(receipt.order_id, id);
        assert_eq!(receipt.quote_amount, 38 * UNIT);
        assert_eq!(receipt.taker_fee, 950_000);
        assert_eq!(receipt.remaining_base, 60 * UNIT);
        assert_eq!(
            balance(&f.env, &f.sy, &f.maker),
            maker_sy_before + 38 * UNIT
        );
        assert_eq!(
            balance(&f.env, &f.pt, &f.taker),
            taker_pt_before + 40 * UNIT
        );
        assert_eq!(balance(&f.env, &f.sy, &f.fee_recipient), 950_000);
        assert_eq!(f.client.best_order(&Side::Ask).unwrap().id, id);
    }

    #[test]
    fn bid_fill_and_cancel_return_exact_escrow() {
        let f = fixture();
        let id = f.client.place_order(
            &f.maker,
            &Side::Bid,
            &(100 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &None,
        );
        let maker_sy_after_place = balance(&f.env, &f.sy, &f.maker);
        assert_eq!(balance(&f.env, &f.sy, &f.client.address), 95 * UNIT);

        let receipt = f
            .client
            .fill_best(&f.taker, &Side::Bid, &(33 * UNIT), &PRICE);
        assert_eq!(receipt.quote_amount, 31_350_000_0);
        assert_eq!(f.client.get_order(&id).unwrap().remaining_base, 67 * UNIT);
        f.client.cancel_order(&f.maker, &id);

        assert_eq!(f.client.get_order(&id), None);
        assert_eq!(f.client.open_count(), 0);
        assert_eq!(balance(&f.env, &f.sy, &f.client.address), 0);
        assert_eq!(
            balance(&f.env, &f.sy, &f.maker),
            maker_sy_after_place + (95 * UNIT - receipt.quote_amount)
        );
    }

    #[test]
    fn partial_fill_rounding_telescopes_to_the_full_order_quote() {
        let f = fixture();
        let awkward_price = 923_456_789_012_345_678;
        let base = 101;
        let id = f.client.place_order(
            &f.maker,
            &Side::Bid,
            &base,
            &awkward_price,
            &(NOW + 1_000),
            &None,
        );
        let first = f
            .client
            .fill_best(&f.taker, &Side::Bid, &33, &awkward_price);
        let second = f
            .client
            .fill_best(&f.taker, &Side::Bid, &68, &awkward_price);

        assert_eq!(first.quote_amount + second.quote_amount, 94);
        assert_eq!(second.remaining_base, 0);
        assert_eq!(f.client.get_order(&id), None);
        assert_eq!(balance(&f.env, &f.sy, &f.client.address), 0);
    }

    #[test]
    fn crossing_orders_are_rejected_and_only_best_price_fills() {
        let f = fixture();
        f.client.place_order(
            &f.maker,
            &Side::Ask,
            &(100 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &None,
        );
        let crossing = f.client.try_place_order(
            &f.maker_two,
            &Side::Bid,
            &(10 * UNIT),
            &PRICE,
            &(NOW + 1_000),
            &None,
        );
        assert_eq!(crossing, Err(Ok(Error::OrderWouldCross)));

        let protected = f
            .client
            .try_fill_best(&f.taker, &Side::Ask, &(10 * UNIT), &(PRICE - 1));
        assert_eq!(protected, Err(Ok(Error::LimitPriceExceeded)));
    }

    #[test]
    fn expired_orders_are_permissionlessly_pruned_and_refunded() {
        let f = fixture();
        let maker_before = balance(&f.env, &f.pt, &f.maker);
        let id = f.client.place_order(
            &f.maker,
            &Side::Ask,
            &(100 * UNIT),
            &PRICE,
            &(NOW + 10),
            &None,
        );
        f.env
            .ledger()
            .with_mut(|ledger| ledger.timestamp = NOW + 10);
        assert_eq!(f.client.prune_expired(&Side::Ask, &10), 1);
        assert_eq!(f.client.get_order(&id), None);
        assert_eq!(balance(&f.env, &f.pt, &f.maker), maker_before);
    }

    #[test]
    fn only_admin_can_change_the_bounded_fee() {
        let f = fixture();
        f.client.set_fee(&f.admin, &50);
        assert_eq!(f.client.config().taker_fee_bps, 50);
        assert_eq!(
            f.client.try_set_fee(&f.maker, &50),
            Err(Ok(Error::NotAdmin))
        );
        assert_eq!(
            f.client.try_set_fee(&f.admin, &(MAX_TAKER_FEE_BPS + 1)),
            Err(Ok(Error::InvalidFee))
        );
    }

    #[test]
    fn maturity_stops_placement_and_fills_but_not_cancellation() {
        let f = fixture();
        let id = f.client.place_order(
            &f.maker,
            &Side::Ask,
            &(100 * UNIT),
            &PRICE,
            &MATURITY,
            &None,
        );
        f.env
            .ledger()
            .with_mut(|ledger| ledger.timestamp = MATURITY);
        assert_eq!(
            f.client
                .try_fill_best(&f.taker, &Side::Ask, &(10 * UNIT), &PRICE),
            Err(Ok(Error::MarketMatured))
        );
        f.client.cancel_order(&f.maker, &id);
        assert_eq!(f.client.open_count(), 0);
    }
}
