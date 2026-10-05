use near_min_api::types::{AccountId, Balance};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::math::{
    CurrentState, LEFT_MOST_POINT, RIGHT_MOST_POINT, RangeRet, SwapError, SwapResult, U512,
    as_u128, checked_add, checked_sub, get_sqrt_price, mul_div_ceil, mul_div_floor, x2y_at_price,
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

pub struct PoolInit {
    pub id: String,
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub fee: u32,
    pub point_delta: i32,
    pub current_point: i32,
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
        let endpoints: BTreeMap<i32, i128> = deltas
            .into_iter()
            .filter(|(point, delta)| {
                point % init.point_delta == 0
                    && (*delta != 0
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
        if *token_in == self.data.token_x {
            self.swap_x2y(amount_in)
        } else if *token_in == self.data.token_y {
            self.swap_y2x(amount_in)
        } else {
            Err(SwapError::WrongToken)
        }
    }

    /// Buys `amount_out` of `token_out`, returns the input amount needed
    pub fn swap_exact_out(
        &mut self,
        token_out: &AccountId,
        amount_out: Balance,
    ) -> SwapResult<Balance> {
        if *token_out == self.data.token_y {
            self.swap_x2y_desire_y(amount_out)
        } else if *token_out == self.data.token_x {
            self.swap_y2x_desire_x(amount_out)
        } else {
            Err(SwapError::WrongToken)
        }
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
            U512::from(amount),
            U512::from(checked_sub(FEE_DENOMINATOR, self.data.fee as u128)?),
            U512::from(FEE_DENOMINATOR),
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
            self.exact_out_fee(U512::from(cost))
        }
    }

    fn exact_out_fee(&self, cost: U512) -> SwapResult<Balance> {
        as_u128(mul_div_ceil(
            cost,
            U512::from(self.data.fee),
            U512::from(checked_sub(FEE_DENOMINATOR, self.data.fee as u128)?),
        )?)
    }

    /// pool.rs:206 / :744: the protocol's part of a fee, in checked u128 math. It takes the whole
    /// fee when there's no liquidity to give the rest to.
    fn charge_fee(&mut self, in_token_x: bool, fee: Balance) -> SwapResult<()> {
        let charged = if self.state.liquidity > 0 {
            fee.checked_mul(self.protocol_fee_rate as u128)
                .ok_or(SwapError::Panic("attempt to multiply with overflow"))?
                / PROTOCOL_FEE_DENOMINATOR
        } else {
            fee
        };
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
                let fee = self.exact_out_fee(U512::from(cost))?;
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
                let fee = self.exact_out_fee(U512::from(cost))?;
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
