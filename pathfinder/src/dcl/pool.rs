use near_min_api::types::{AccountId, Balance};
use ruint::aliases::U256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::math::{
    CurrentState, LEFT_MOST_POINT, RIGHT_MOST_POINT, RangeRet, SwapError, SwapResult, as_u128,
    checked_add, checked_sub, get_sqrt_price, mul_div_ceil, mul_div_floor, x2y_at_price,
    x2y_at_price_desire, x2y_range, x2y_range_desire, y2x_at_price, y2x_at_price_desire, y2x_range,
    y2x_range_desire,
};

const FEE_DENOMINATOR: u128 = 1_000_000;
const PROTOCOL_FEE_DENOMINATOR: u128 = 10_000;
/// Bounds that `quote` and `Swap` pass for exact-input swaps (swap.rs). Exact-output swaps pass
/// ±800001, which the swap loops clamp to ±800000.
const EXACT_IN_LOW_POINT: i32 = -799_999;
const EXACT_IN_HIGH_POINT: i32 = 799_999;

const ENDPOINT: u8 = 1;
const ORDER: u8 = 2;
/// Points of a word of the contract's bitmap of points
const BITMAP_WORD_POINTS: i32 = 256;

/// Pool data that doesn't change while swaps are simulated
#[derive(Debug)]
pub struct PoolData {
    pub id: String,
    pub token_x: AccountId,
    pub token_y: AccountId,
    fee: u32,
    point_delta: i32,
    tax: Option<PoolTax>,
    /// Points with liquidity_sum != 0 (even with a zero net delta) and the liquidity delta when
    /// crossed from left to right
    endpoints: BTreeMap<i32, i128>,
    /// (selling_x, selling_y) of the limit orders at each point
    orders: BTreeMap<i32, (Balance, Balance)>,
    /// Points where a swap step stops: endpoints and limit orders
    marked: BTreeSet<i32>,
}

#[derive(Debug, Clone)]
pub struct Pool {
    pub data: Arc<PoolData>,
    state: CurrentState,
    fee_charged_x: Balance,
    fee_charged_y: Balance,
    protocol_fee_rate: u32,
    /// The tax of the swap being simulated, which the protocol takes its portion of at the end
    tax_charged: Balance,
    /// Limit orders changed by simulated swaps
    filled_orders: BTreeMap<i32, (Balance, Balance)>,
    /// Steps of the swaps simulated on this pool
    work: Work,
}

/// Steps the contract takes in swaps, which their gas depends on
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    /// Swaps within a range of the same liquidity
    pub ranges: u64,
    /// Liquidity endpoints crossed
    pub crossings: u64,
    /// Points with limit orders reached
    pub orders: u64,
    /// Words of the bitmap of points searched for the next point
    pub words: u64,
}

/// A pool's tax, both out of PROTOCOL_FEE_DENOMINATOR: the tax takes `tax_rate` of each
/// swap fee for the pool's creator, and the protocol keeps `protocol_portion` of the tax
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PoolTax {
    pub tax_rate: u16,
    pub protocol_portion: u16,
}

pub struct PoolInit {
    pub id: String,
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub fee: u32,
    pub point_delta: i32,
    pub tax: Option<PoolTax>,
    pub current_point: i32,
    /// Whether positions start or end at the current point, even if they cancel each other out
    pub current_point_is_endpoint: bool,
    pub liquidity: Balance,
    pub liquidity_x: Balance,
    pub fee_charged_x: Balance,
    pub fee_charged_y: Balance,
    pub protocol_fee_rate: u32,
}

impl Pool {
    /// Builds a pool from `get_liquidity_range` segments (left, right, liquidity) and
    /// `get_pointorder_range` orders (point, selling_x, selling_y) over the full point range.
    pub fn new(
        init: PoolInit,
        segments: impl IntoIterator<Item = (i32, i32, Balance)>,
        orders: impl IntoIterator<Item = (i32, Balance, Balance)>,
    ) -> Result<Self, anyhow::Error> {
        let mut deltas: BTreeMap<i32, i128> = BTreeMap::new();
        let mut liquidity_at_current_point = 0;
        for (left_point, right_point, liquidity) in segments {
            let liquidity_delta = i128::try_from(liquidity)?;
            *deltas.entry(left_point).or_default() += liquidity_delta;
            *deltas.entry(right_point).or_default() -= liquidity_delta;
            if (left_point..right_point).contains(&init.current_point) {
                liquidity_at_current_point = liquidity;
            }
        }
        if liquidity_at_current_point != init.liquidity {
            return Err(anyhow::anyhow!(
                "Liquidity at the current point is {liquidity_at_current_point} according to liquidity ranges, but the pool reports {}",
                init.liquidity
            ));
        }
        // The view cuts segments at every endpoint, at the current point (the left and right walks
        // meet there) and at the query bounds, so every boundary except those two is an endpoint.
        // The current point is one too if positions start or end there, which the contract steps
        // on and crosses even when they cancel each other out.
        let endpoints: BTreeMap<i32, i128> = deltas
            .into_iter()
            .filter(|(point, delta)| {
                point % init.point_delta == 0
                    && (*delta != 0
                        || (*point == init.current_point && init.current_point_is_endpoint)
                        || ![init.current_point, LEFT_MOST_POINT, RIGHT_MOST_POINT].contains(point))
            })
            .collect();
        let orders: BTreeMap<i32, (Balance, Balance)> = orders
            .into_iter()
            .filter(|(_, selling_x, selling_y)| *selling_x > 0 || *selling_y > 0)
            .map(|(point, selling_x, selling_y)| (point, (selling_x, selling_y)))
            .collect();
        let marked = endpoints.keys().chain(orders.keys()).copied().collect();
        Ok(Self {
            data: Arc::new(PoolData {
                id: init.id,
                token_x: init.token_x,
                token_y: init.token_y,
                fee: init.fee,
                point_delta: init.point_delta,
                tax: init.tax,
                endpoints,
                orders,
                marked,
            }),
            state: CurrentState {
                point: init.current_point,
                sqrt_price: get_sqrt_price(init.current_point)?,
                liquidity: init.liquidity,
                liquidity_x: init.liquidity_x,
            },
            fee_charged_x: init.fee_charged_x,
            fee_charged_y: init.fee_charged_y,
            protocol_fee_rate: init.protocol_fee_rate,
            tax_charged: 0,
            filled_orders: BTreeMap::new(),
            work: Work::default(),
        })
    }

    /// Steps of the swaps simulated on this pool since it was built
    pub fn work(&self) -> Work {
        self.work
    }

    pub fn set_protocol_fee_rate(&mut self, protocol_fee_rate: u32) {
        self.protocol_fee_rate = protocol_fee_rate;
    }

    /// Sells `amount_in` of `token_in`, returns the output amount
    pub fn swap_exact_in(
        &mut self,
        token_in: &AccountId,
        amount_in: Balance,
    ) -> SwapResult<Balance> {
        let in_token_x = if *token_in == self.data.token_x {
            true
        } else if *token_in == self.data.token_y {
            false
        } else {
            return Err(SwapError::WrongToken);
        };
        let amount_out = if in_token_x {
            self.swap_x2y(amount_in)
        } else {
            self.swap_y2x(amount_in)
        };
        // On every exit, so a failed swap doesn't leave its tax to the next one
        self.charge_tax_portion(in_token_x)?;
        amount_out
    }

    /// Buys `amount_out` of `token_out`, returns the input amount needed
    pub fn swap_exact_out(
        &mut self,
        token_out: &AccountId,
        amount_out: Balance,
    ) -> SwapResult<Balance> {
        let in_token_x = if *token_out == self.data.token_y {
            true
        } else if *token_out == self.data.token_x {
            false
        } else {
            return Err(SwapError::WrongToken);
        };
        let amount_in = if in_token_x {
            self.swap_x2y_desire_y(amount_out)
        } else {
            self.swap_y2x_desire_x(amount_out)
        };
        self.charge_tax_portion(in_token_x)?;
        amount_in
    }

    fn order(&self, point: i32) -> (Balance, Balance) {
        self.filled_orders
            .get(&point)
            .or_else(|| self.data.orders.get(&point))
            .copied()
            .unwrap_or((0, 0))
    }

    /// point_info.rs:157: endpoint and active limit order flags
    fn flags(&self, point: i32) -> u8 {
        let mut flags = 0;
        if self.data.endpoints.contains_key(&point) {
            flags |= ENDPOINT;
        }
        let (selling_x, selling_y) = self.order(point);
        if selling_x > 0 || selling_y > 0 {
            flags |= ORDER;
        }
        flags
    }

    /// slot_bitmap.rs:70: the nearest marked point at or left of `point`. Unlike iZiSwap, the
    /// search doesn't stop at bitmap word boundaries.
    fn nearest_left(&mut self, point: i32, low: i32) -> Option<i32> {
        let found = self
            .data
            .marked
            .range(..=point)
            .rev()
            .copied()
            .find(|point| self.flags(*point) != 0)
            .filter(|point| *point >= low);
        self.work.words += self.words_between(found.unwrap_or(low), point);
        found
    }

    /// slot_bitmap.rs:117: the nearest marked point right of `point`
    fn nearest_right(&mut self, point: i32) -> i32 {
        let found = self
            .data
            .marked
            .range(point + 1..)
            .copied()
            .find(|point| self.flags(*point) != 0)
            .unwrap_or(RIGHT_MOST_POINT);
        self.work.words += self.words_between(point, found);
        found
    }

    /// Bitmap words a search from `left` to `right` reads
    fn words_between(&self, left: i32, right: i32) -> u64 {
        let word = |point: i32| (point / self.data.point_delta).div_euclid(BITMAP_WORD_POINTS);
        (word(right) - word(left)).unsigned_abs() as u64 + 1
    }

    fn move_to(&mut self, point: i32) -> SwapResult<()> {
        self.state.point = point;
        self.state.sqrt_price = get_sqrt_price(point)?;
        self.state.liquidity_x = 0;
        Ok(())
    }

    fn apply<Cost, Acquire>(&mut self, range: &RangeRet<Cost, Acquire>) {
        self.state.point = range.final_point;
        self.state.sqrt_price = range.sqrt_final_price;
        self.state.liquidity_x = range.liquidity_x;
    }

    fn add_liquidity_delta(&mut self, delta: i128) -> SwapResult<()> {
        self.state.liquidity = if delta >= 0 {
            self.state.liquidity.checked_add(delta.unsigned_abs())
        } else {
            self.state.liquidity.checked_sub(delta.unsigned_abs())
        }
        .ok_or(SwapError::Panic("E203: liquidity overflow"))?;
        Ok(())
    }

    fn cross_endpoint_leftwards(&mut self) -> SwapResult<()> {
        self.work.crossings += 1;
        let delta = self
            .data
            .endpoints
            .get(&self.state.point)
            .copied()
            .unwrap_or(0);
        self.add_liquidity_delta(
            delta
                .checked_neg()
                .ok_or(SwapError::Panic("E203: liquidity overflow"))?,
        )?;
        self.move_to(self.state.point - 1)
    }

    fn cross_endpoint_rightwards(&mut self, point: i32) -> SwapResult<()> {
        self.work.crossings += 1;
        let delta = self.data.endpoints.get(&point).copied().unwrap_or(0);
        self.add_liquidity_delta(delta)
    }

    fn amount_no_fee(&self, amount: Balance) -> SwapResult<Balance> {
        as_u128(mul_div_floor(
            U256::from(amount),
            U256::from(checked_sub(FEE_DENOMINATOR, self.data.fee as u128)?),
            U256::from(FEE_DENOMINATOR),
        )?)
    }

    fn exact_in_fee(
        &self,
        amount: Balance,
        cost: Balance,
        amount_no_fee: Balance,
    ) -> SwapResult<Balance> {
        if cost >= amount_no_fee {
            checked_sub(amount, cost)
        } else {
            self.exact_out_fee(U256::from(cost))
        }
    }

    fn exact_out_fee(&self, cost: U256) -> SwapResult<Balance> {
        as_u128(mul_div_ceil(
            cost,
            U256::from(self.data.fee),
            U256::from(checked_sub(FEE_DENOMINATOR, self.data.fee as u128)?),
        )?)
    }

    /// pool.rs:206 / :744: the protocol's part of a fee, in checked u128 math. With a tax (2.4.0),
    /// the tax is taken off the fee first and summed for `charge_tax_portion`. The protocol takes
    /// its rate of the rest, or all of it when there's no liquidity to give it to.
    fn charge_fee(&mut self, in_token_x: bool, fee: Balance) -> SwapResult<()> {
        let taxed = match self.data.tax {
            Some(tax) => fee_part(fee, tax.tax_rate as u32)?,
            None => 0,
        };
        self.tax_charged = checked_add(self.tax_charged, taxed)?;
        let untaxed = checked_sub(fee, taxed)?;
        let charged = if self.state.liquidity > 0 {
            fee_part(untaxed, self.protocol_fee_rate)?
        } else {
            untaxed
        };
        self.add_fee_charged(in_token_x, charged)
    }

    /// The protocol's portion of the tax a swap charged, which the contract takes once at the
    /// end of each pool's swap, from the summed tax
    fn charge_tax_portion(&mut self, in_token_x: bool) -> SwapResult<()> {
        let taxed = std::mem::take(&mut self.tax_charged);
        match self.data.tax {
            Some(tax) => {
                self.add_fee_charged(in_token_x, fee_part(taxed, tax.protocol_portion as u32)?)
            }
            None => Ok(()),
        }
    }

    fn add_fee_charged(&mut self, in_token_x: bool, charged: Balance) -> SwapResult<()> {
        let total = if in_token_x {
            &mut self.fee_charged_x
        } else {
            &mut self.fee_charged_y
        };
        *total = checked_add(*total, charged)?;
        Ok(())
    }

    /// pool.rs:193: swaps down to `left` (inclusive), returns (finished, paid, acquired)
    fn x2y_range_step(
        &mut self,
        left: i32,
        amount: Balance,
    ) -> SwapResult<(bool, Balance, Balance)> {
        let amount_no_fee = self.amount_no_fee(amount)?;
        if amount_no_fee == 0 {
            return Ok((true, 0, 0));
        }
        if self.state.liquidity == 0 {
            if self.state.point != left {
                self.move_to(left)?;
            }
            return Ok((false, 0, 0));
        }
        self.work.ranges += 1;
        let range = x2y_range(&self.state, left, amount_no_fee)?;
        let fee = self.exact_in_fee(amount, range.cost, amount_no_fee)?;
        self.charge_fee(true, fee)?;
        self.apply(&range);
        Ok((
            range.finished,
            checked_add(range.cost, fee)?,
            as_u128(range.acquire)?,
        ))
    }

    /// pool.rs:140: exact-input X -> Y. Differs from iZiSwap: a point's limit order is filled
    /// before the point's AMM liquidity (the range step stops right above an order point).
    fn swap_x2y(&mut self, mut amount: Balance) -> SwapResult<Balance> {
        let low = EXACT_IN_LOW_POINT;
        let mut acquired: Balance = 0;
        while low <= self.state.point {
            let flags = self.flags(self.state.point);
            if flags & ORDER != 0 {
                self.work.orders += 1;
                let amount_no_fee = self.amount_no_fee(amount)?;
                if amount_no_fee == 0 {
                    return Ok(acquired);
                }
                let point = self.state.point;
                let (selling_x, selling_y) = self.order(point);
                let (cost, acquire) =
                    x2y_at_price(amount_no_fee, self.state.sqrt_price, selling_y)?;
                let finished = acquire < selling_y || cost >= amount_no_fee;
                let fee = self.exact_in_fee(amount, cost, amount_no_fee)?;
                self.charge_fee(true, fee)?;
                self.filled_orders
                    .insert(point, (selling_x, checked_sub(selling_y, acquire)?));
                acquired = checked_add(acquired, acquire)?;
                if finished {
                    return Ok(acquired);
                }
                amount = checked_sub(amount, checked_add(cost, fee)?)?;
            }
            let search_start = if flags & ENDPOINT != 0 {
                let (finished, paid, acquire) = self.x2y_range_step(self.state.point, amount)?;
                acquired = checked_add(acquired, acquire)?;
                if finished {
                    return Ok(acquired);
                }
                self.cross_endpoint_leftwards()?;
                if self.state.point < low {
                    return Err(SwapError::NotFilled);
                }
                amount = checked_sub(amount, paid)?;
                if self.flags(self.state.point) != 0 {
                    continue;
                }
                self.state.point
            } else {
                self.state.point - 1
            };
            let (left, stops_above_order) = match self.nearest_left(search_start, low) {
                None => (low, false),
                Some(point) if self.flags(point) & ORDER != 0 => (point + 1, true),
                Some(point) => (point, false),
            };
            let (finished, paid, acquire) = self.x2y_range_step(left, amount)?;
            acquired = checked_add(acquired, acquire)?;
            if finished {
                return Ok(acquired);
            }
            if self.state.point <= low {
                return Err(SwapError::NotFilled);
            }
            amount = checked_sub(amount, paid)?;
            if stops_above_order {
                self.move_to(self.state.point - 1)?;
            }
        }
        Err(SwapError::NotFilled)
    }

    /// pool.rs:347: exact-input Y -> X (iZiSwap `swapY2X` without bitmap word boundaries)
    fn swap_y2x(&mut self, mut amount: Balance) -> SwapResult<Balance> {
        let high = EXACT_IN_HIGH_POINT;
        let mut acquired: Balance = 0;
        let mut flags = self.flags(self.state.point);
        while self.state.point < high {
            if flags & ORDER != 0 {
                self.work.orders += 1;
                let amount_no_fee = self.amount_no_fee(amount)?;
                if amount_no_fee == 0 {
                    return Ok(acquired);
                }
                let point = self.state.point;
                let (selling_x, selling_y) = self.order(point);
                let (cost, acquire) =
                    y2x_at_price(amount_no_fee, self.state.sqrt_price, selling_x)?;
                let finished = acquire < selling_x || cost >= amount_no_fee;
                let fee = self.exact_in_fee(amount, cost, amount_no_fee)?;
                self.charge_fee(false, fee)?;
                self.filled_orders
                    .insert(point, (checked_sub(selling_x, acquire)?, selling_y));
                acquired = checked_add(acquired, acquire)?;
                if finished {
                    return Ok(acquired);
                }
                amount = checked_sub(amount, checked_add(cost, fee)?)?;
            }
            let (next_point, next_flags) = match self.nearest_right(self.state.point) {
                point if point > high => (high, 0),
                point => (point, self.flags(point)),
            };
            if self.state.liquidity == 0 {
                self.state.point = next_point;
                self.state.sqrt_price = get_sqrt_price(next_point)?;
                if next_flags & ENDPOINT != 0 {
                    self.cross_endpoint_rightwards(next_point)?;
                    self.state.liquidity_x = self.state.liquidity;
                }
                flags = next_flags;
                continue;
            }
            let amount_no_fee = self.amount_no_fee(amount)?;
            if amount_no_fee == 0 {
                return Ok(acquired);
            }
            self.work.ranges += 1;
            let range = y2x_range(&self.state, next_point, amount_no_fee)?;
            let fee = self.exact_in_fee(amount, range.cost, amount_no_fee)?;
            self.charge_fee(false, fee)?;
            acquired = checked_add(acquired, as_u128(range.acquire)?)?;
            amount = checked_sub(amount, checked_add(range.cost, fee)?)?;
            self.apply(&range);
            if self.state.point == next_point {
                if next_flags & ENDPOINT != 0 {
                    self.cross_endpoint_rightwards(next_point)?;
                }
                self.state.liquidity_x = self.state.liquidity;
                flags = next_flags;
            } else {
                flags = 0;
            }
            if range.finished {
                return Ok(acquired);
            }
        }
        Err(SwapError::NotFilled)
    }

    /// pool.rs:558: buys up to `desire` moving down to `left` (inclusive), returns
    /// (finished, paid, acquired)
    fn x2y_desire_range_step(
        &mut self,
        left: i32,
        desire: Balance,
    ) -> SwapResult<(bool, Balance, Balance)> {
        if desire == 0 {
            return Ok((true, 0, 0));
        }
        if self.state.liquidity == 0 {
            if self.state.point != left {
                self.move_to(left)?;
            }
            return Ok((false, 0, 0));
        }
        self.work.ranges += 1;
        let range = x2y_range_desire(&self.state, left, desire)?;
        let fee = self.exact_out_fee(range.cost)?;
        self.charge_fee(true, fee)?;
        self.apply(&range);
        let paid = as_u128(range.cost)?;
        Ok((range.finished, checked_add(paid, fee)?, range.acquire))
    }

    /// pool.rs:521: exact-output X -> Y, same control flow as `swap_x2y`
    fn swap_x2y_desire_y(&mut self, mut desire: Balance) -> SwapResult<Balance> {
        let low = LEFT_MOST_POINT;
        let mut paid: Balance = 0;
        while low <= self.state.point {
            let flags = self.flags(self.state.point);
            if flags & ORDER != 0 {
                self.work.orders += 1;
                let point = self.state.point;
                let (selling_x, selling_y) = self.order(point);
                let (cost, acquire) =
                    x2y_at_price_desire(desire, self.state.sqrt_price, selling_y)?;
                let fee = self.exact_out_fee(U256::from(cost))?;
                self.charge_fee(true, fee)?;
                paid = checked_add(paid, checked_add(cost, fee)?)?;
                let finished = acquire >= desire;
                desire = desire.saturating_sub(acquire);
                self.filled_orders
                    .insert(point, (selling_x, checked_sub(selling_y, acquire)?));
                if finished {
                    return Ok(paid);
                }
            }
            let search_start = if flags & ENDPOINT != 0 {
                let (finished, step_paid, acquire) =
                    self.x2y_desire_range_step(self.state.point, desire)?;
                paid = checked_add(paid, step_paid)?;
                desire -= desire.min(acquire);
                if finished {
                    return Ok(paid);
                }
                self.cross_endpoint_leftwards()?;
                if self.state.point < low {
                    return Err(SwapError::NotFilled);
                }
                if self.flags(self.state.point) != 0 {
                    continue;
                }
                self.state.point
            } else {
                self.state.point - 1
            };
            let (left, stops_above_order) = match self.nearest_left(search_start, low) {
                None => (low, false),
                Some(point) if self.flags(point) & ORDER != 0 => (point + 1, true),
                Some(point) => (point, false),
            };
            let (finished, step_paid, acquire) = self.x2y_desire_range_step(left, desire)?;
            paid = checked_add(paid, step_paid)?;
            desire -= desire.min(acquire);
            if finished {
                return Ok(paid);
            }
            if self.state.point <= low {
                return Err(SwapError::NotFilled);
            }
            if stops_above_order {
                self.move_to(self.state.point - 1)?;
            }
        }
        Err(SwapError::NotFilled)
    }

    /// pool.rs:707: exact-output Y -> X (iZiSwap `swapY2XDesireX` without bitmap word boundaries)
    fn swap_y2x_desire_x(&mut self, mut desire: Balance) -> SwapResult<Balance> {
        let high = RIGHT_MOST_POINT;
        let mut paid: Balance = 0;
        let mut flags = self.flags(self.state.point);
        while self.state.point < high {
            if flags & ORDER != 0 {
                self.work.orders += 1;
                let point = self.state.point;
                let (selling_x, selling_y) = self.order(point);
                let (cost, acquire) =
                    y2x_at_price_desire(desire, self.state.sqrt_price, selling_x)?;
                let fee = self.exact_out_fee(U256::from(cost))?;
                self.charge_fee(false, fee)?;
                paid = checked_add(paid, checked_add(cost, fee)?)?;
                let finished = acquire >= desire;
                desire = desire.saturating_sub(acquire);
                self.filled_orders
                    .insert(point, (checked_sub(selling_x, acquire)?, selling_y));
                if finished {
                    return Ok(paid);
                }
            }
            let (next_point, next_flags) = match self.nearest_right(self.state.point) {
                point if point > high => (high, 0),
                point => (point, self.flags(point)),
            };
            if self.state.liquidity == 0 {
                self.state.point = next_point;
                self.state.sqrt_price = get_sqrt_price(next_point)?;
                if next_flags & ENDPOINT != 0 {
                    self.cross_endpoint_rightwards(next_point)?;
                    self.state.liquidity_x = self.state.liquidity;
                }
                flags = next_flags;
                continue;
            }
            if desire == 0 {
                return Ok(paid);
            }
            self.work.ranges += 1;
            let range = y2x_range_desire(&self.state, next_point, desire)?;
            let fee = self.exact_out_fee(range.cost)?;
            self.charge_fee(false, fee)?;
            paid = checked_add(paid, checked_add(as_u128(range.cost)?, fee)?)?;
            desire -= desire.min(range.acquire);
            self.apply(&range);
            if self.state.point == next_point {
                if next_flags & ENDPOINT != 0 {
                    self.cross_endpoint_rightwards(next_point)?;
                }
                self.state.liquidity_x = self.state.liquidity;
                flags = next_flags;
            } else {
                flags = 0;
            }
            if range.finished {
                return Ok(paid);
            }
        }
        Err(SwapError::NotFilled)
    }
}

/// `rate` out of PROTOCOL_FEE_DENOMINATOR of `fee`, rounded down
fn fee_part(fee: Balance, rate: u32) -> SwapResult<Balance> {
    Ok(fee
        .checked_mul(rate as u128)
        .ok_or(SwapError::Panic("attempt to multiply with overflow"))?
        / PROTOCOL_FEE_DENOMINATOR)
}

#[cfg(test)]
mod tests {
    //! Swaps emulated with the wasm of ref-dcl.private-mainnet.ref-dev-team.near (2.4.0) on a
    //! pool of fee 1000, point delta 20 and protocol fee rate 2000, with the current point at
    //! 397940 and only y there, and positions of 3e10, 4e10 and 5e10 or 5e10 each from 397000,
    //! 397600 and 397800 to 398500

    use super::*;

    fn pool(tax: PoolTax, liquidities: [Balance; 3]) -> Pool {
        let [l1, l2, l3] = liquidities;
        let segments = [
            (LEFT_MOST_POINT, 397000, 0),
            (397000, 397600, l1),
            (397600, 397800, l1 + l2),
            (397800, 397940, l1 + l2 + l3),
            (397940, 398500, l1 + l2 + l3),
            (398500, RIGHT_MOST_POINT, 0),
        ];
        Pool::new(
            PoolInit {
                id: "x.near|y.near|1000".to_string(),
                token_x: "x.near".parse().unwrap(),
                token_y: "y.near".parse().unwrap(),
                fee: 1000,
                point_delta: 20,
                tax: Some(tax),
                current_point: 397940,
                current_point_is_endpoint: false,
                liquidity: l1 + l2 + l3,
                liquidity_x: 0,
                fee_charged_x: 0,
                fee_charged_y: 0,
                protocol_fee_rate: 2000,
            },
            segments,
            [],
        )
        .unwrap()
    }

    #[test]
    fn tax_portion_is_taken_once_from_the_summed_tax() {
        let mut taxed = pool(
            PoolTax {
                tax_rate: 7777,
                protocol_portion: 3333,
            },
            [3 * 10u128.pow(10), 4 * 10u128.pow(10), 5 * 10u128.pow(10)],
        );
        assert_eq!(
            taxed.swap_exact_in(&"x.near".parse().unwrap(), 79624),
            Ok(14945749269361629031909)
        );
        // Protocol fee 2 plus 20 of the 61 tax, where flooring per step would give 20 in all
        assert_eq!(taxed.fee_charged_x, 22);

        let mut taxed = pool(
            PoolTax {
                tax_rate: 1000,
                protocol_portion: 4000,
            },
            [5 * 10u128.pow(10); 3],
        );
        assert_eq!(
            taxed.swap_exact_in(&"x.near".parse().unwrap(), 157398),
            Ok(29085689450957559429942)
        );
        assert_eq!(taxed.fee_charged_x, 33);
    }
}
