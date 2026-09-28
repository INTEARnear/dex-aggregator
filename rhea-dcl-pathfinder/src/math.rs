// Port of iZiSwap-core's discretized-liquidity math (GPL-2.0-or-later), with the behavior of the
// deployed dclv2.ref-labs.near binary where the two differ. `file.rs:line` point at its sources.

use lazy_static::lazy_static;
use near_min_api::types::Balance;
use uint::construct_uint;

construct_uint! {
    /// Wide enough for every intermediate product. Values the contract keeps in U256 are
    /// range-checked, so a product of two of them never overflows.
    pub struct U512(8);
}

construct_uint! {
    struct U256(4);
}

pub const LEFT_MOST_POINT: i32 = -800_000;
pub const RIGHT_MOST_POINT: i32 = 800_000;

pub const POW96: U512 = U512([0, 1 << 32, 0, 0, 0, 0, 0, 0]);
const U256_MAX: U512 = U512([u64::MAX, u64::MAX, u64::MAX, u64::MAX, 0, 0, 0, 0]);

/// sqrt(1.0001^(2^i)) * 2^128 for i in 1..=19, `LogPowMath.getSqrtPrice`
const SQRT_PRICE_FACTORS: [u128; 19] = [
    0xfff97272373d413259a46990580e213a,
    0xfff2e50f5f656932ef12357cf3c7fdcc,
    0xffe5caca7e10e4e61c3624eaa0941cd0,
    0xffcb9843d60f6159c9db58835c926644,
    0xff973b41fa98c081472e6896dfb254c0,
    0xff2ea16466c96a3843ec78b326b52861,
    0xfe5dee046a99a2a811c461f1969c3053,
    0xfcbe86c7900a88aedcffc83b479aa3a4,
    0xf987a7253ac413176f2b074cf7815e54,
    0xf3392b0822b70005940c7a398e4b70f3,
    0xe7159475a2c29b7443b29c7fa6e889d9,
    0xd097f3bdfd2022b8845ad8f792aa5825,
    0xa9f746462d870fdf8a65dc1f90e061e5,
    0x70d869a156d2a1b890bb3df62baf32f7,
    0x31be135f97d08fd981231505542fcfa6,
    0x9aa508b5b7a84e1c677de54f3e99bc9,
    0x5d6af8dedb81196699c329225ee604,
    0x2216e584f5fa1ea926041bedfe98,
    0x48a170391f7dc42444e8fa2,
];

lazy_static! {
    pub static ref SQRT_RATE_96: U512 = get_sqrt_price(1).unwrap();
    static ref MIN_SQRT_PRICE: U512 = get_sqrt_price(LEFT_MOST_POINT).unwrap();
    static ref MAX_SQRT_PRICE: U512 = get_sqrt_price(RIGHT_MOST_POINT).unwrap();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapError {
    /// The contract panics here, so its quote view fails and a swap would revert
    Panic(&'static str),
    /// Liquidity runs out before the amount is filled, the contract's quote returns 0
    NotFilled,
    /// The token isn't traded by the pool
    WrongToken,
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapError::Panic(message) => write!(f, "Contract would panic: {message}"),
            SwapError::NotFilled => write!(f, "Not enough liquidity"),
            SwapError::WrongToken => write!(f, "Token is not in the pool"),
        }
    }
}

impl std::error::Error for SwapError {}

pub type SwapResult<T> = Result<T, SwapError>;

fn fits_u256(value: U512) -> SwapResult<U512> {
    if value > U256_MAX {
        return Err(SwapError::Panic("U256 overflow"));
    }
    Ok(value)
}

/// utils.rs:45: U256 -> u128 narrowing panics on overflow
pub fn as_u128(value: U512) -> SwapResult<Balance> {
    if value.0[2..].iter().any(|&word| word != 0) {
        return Err(SwapError::Panic("u128 overflow"));
    }
    Ok(value.0[0] as u128 | (value.0[1] as u128) << 64)
}

pub fn checked_add(a: Balance, b: Balance) -> SwapResult<Balance> {
    a.checked_add(b)
        .ok_or(SwapError::Panic("attempt to add with overflow"))
}

pub fn checked_sub(a: Balance, b: Balance) -> SwapResult<Balance> {
    a.checked_sub(b)
        .ok_or(SwapError::Panic("attempt to subtract with overflow"))
}

fn add(a: U512, b: U512) -> SwapResult<U512> {
    fits_u256(a + b)
}

fn sub(a: U512, b: U512) -> SwapResult<U512> {
    a.checked_sub(b).ok_or(SwapError::Panic("U256 underflow"))
}

/// common_math.rs:29: exact floor(a * b / c)
pub fn mul_div_floor(a: U512, b: U512, c: U512) -> SwapResult<U512> {
    if c.is_zero() {
        return Err(SwapError::Panic("division by zero"));
    }
    fits_u256(a * b / c)
}

/// common_math.rs:41: exact ceil(a * b / c)
pub fn mul_div_ceil(a: U512, b: U512, c: U512) -> SwapResult<U512> {
    if c.is_zero() {
        return Err(SwapError::Panic("division by zero"));
    }
    let (quotient, remainder) = (a * b).div_mod(c);
    fits_u256(if remainder.is_zero() {
        quotient
    } else {
        quotient + 1
    })
}

fn mul_div(a: U512, b: U512, c: U512, round_up: bool) -> SwapResult<U512> {
    if round_up {
        mul_div_ceil(a, b, c)
    } else {
        mul_div_floor(a, b, c)
    }
}

/// common_math.rs:63 (`LogPowMath.getSqrtPrice`): sqrt(1.0001^point) * 2^96
pub fn get_sqrt_price(point: i32) -> SwapResult<U512> {
    if !(LEFT_MOST_POINT..=RIGHT_MOST_POINT).contains(&point) {
        return Err(SwapError::Panic("E202: illegal point"));
    }
    let abs_point = point.unsigned_abs();
    // The value stays below 2^129 and every factor is below 2^128, so U256 products never overflow
    let mut value = if abs_point & 1 != 0 {
        U256::from(0xfffcb933bd6fad37aa2d162d1a594001u128)
    } else {
        U256::one() << 128
    };
    for (bit, factor) in SQRT_PRICE_FACTORS.iter().enumerate() {
        if abs_point & (2 << bit) != 0 {
            value = (value * U256::from(*factor)) >> 128;
        }
    }
    if point > 0 {
        value = U256::MAX / value;
    }
    let mut sqrt_price = value >> 32;
    if value.0[0] & 0xffff_ffff != 0 {
        sqrt_price += U256::one();
    }
    Ok(U512([
        sqrt_price.0[0],
        sqrt_price.0[1],
        sqrt_price.0[2],
        sqrt_price.0[3],
        0,
        0,
        0,
        0,
    ]))
}

/// common_math.rs:111 (`LogPowMath.getLogSqrtPriceFloor`): the greatest point whose sqrt price
/// is <= `sqrt_price_96`. That's exactly what the log2-based original returns, so a binary
/// search over `get_sqrt_price` gives the same result.
pub fn get_log_sqrt_price_floor(sqrt_price_96: U512) -> SwapResult<i32> {
    if sqrt_price_96 < *MIN_SQRT_PRICE || sqrt_price_96 >= *MAX_SQRT_PRICE {
        return Err(SwapError::Panic("E201: invalid sqrt price"));
    }
    // Invariant: sqrt_price(low) <= sqrt_price_96 < sqrt_price(high)
    let (mut low, mut high) = (LEFT_MOST_POINT, RIGHT_MOST_POINT);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if get_sqrt_price(middle)? <= sqrt_price_96 {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// sqrt_price * sqrt(1.0001), as the contract steps one point to the right
fn next_sqrt_price(sqrt_price: U512) -> SwapResult<U512> {
    add(
        sqrt_price,
        mul_div_floor(sqrt_price, *SQRT_RATE_96 - POW96, POW96)?,
    )
}

/// `AmountMath.getAmountY`: token Y held by `liquidity` over the points [left, right)
pub fn get_amount_y(
    liquidity: Balance,
    sqrt_price_l: U512,
    sqrt_price_r: U512,
    upper: bool,
) -> SwapResult<U512> {
    mul_div(
        U512::from(liquidity),
        sub(sqrt_price_r, sqrt_price_l)?,
        *SQRT_RATE_96 - POW96,
        upper,
    )
}

/// common_math.rs:298 (`AmountMath.getAmountX`): token X held by `liquidity` over the points
/// [left, right). sqrt_price(right - 1) is rounded down. Panics (E202) for ranges wider than
/// 800000 points.
pub fn get_amount_x(
    liquidity: Balance,
    left_point: i32,
    right_point: i32,
    sqrt_price_r: U512,
    upper: bool,
) -> SwapResult<U512> {
    let sqrt_price_pr_pl = get_sqrt_price(right_point - left_point)?;
    let sqrt_price_pr_m1 = mul_div_floor(sqrt_price_r, POW96, *SQRT_RATE_96)?;
    mul_div(
        U512::from(liquidity),
        sub(sqrt_price_pr_pl, POW96)?,
        sub(sqrt_price_r, sqrt_price_pr_m1)?,
        upper,
    )
}

/// Pool state that a range swap starts from
#[derive(Debug, Clone, Copy)]
pub struct CurrentState {
    pub point: i32,
    pub sqrt_price: U512,
    pub liquidity: Balance,
    pub liquidity_x: Balance,
}

#[derive(Debug, Clone, Copy)]
pub struct RangeRet<Cost, Acquire> {
    pub finished: bool,
    pub cost: Cost,
    pub acquire: Acquire,
    pub final_point: i32,
    pub sqrt_final_price: U512,
    pub liquidity_x: Balance,
}

struct RangeComplete<Cost, Acquire> {
    cost: Cost,
    acquire: Acquire,
    complete: bool,
    loc_point: i32,
    sqrt_loc: U512,
}

impl<Cost, Acquire> RangeComplete<Cost, Acquire> {
    fn complete(cost: Cost, acquire: Acquire) -> Self {
        Self {
            cost,
            acquire,
            complete: true,
            loc_point: 0,
            sqrt_loc: U512::zero(),
        }
    }

    fn partial(cost: Cost, acquire: Acquire, loc_point: i32, sqrt_loc: U512) -> Self {
        Self {
            cost,
            acquire,
            complete: false,
            loc_point,
            sqrt_loc,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// X -> Y, exact input

/// `SwapMathX2Y.x2YAtPrice`: fills a limit order selling `curr_y` at `sqrt_price`
pub fn x2y_at_price(
    amount_x: Balance,
    sqrt_price: U512,
    curr_y: Balance,
) -> SwapResult<(Balance, Balance)> {
    let l = mul_div_floor(U512::from(amount_x), sqrt_price, POW96)?;
    // Narrowed before the min with the order size, so a huge amount panics even for a small order
    let acquire_y = as_u128(mul_div_floor(l, sqrt_price, POW96)?)?.min(curr_y);
    let l = mul_div_ceil(U512::from(acquire_y), POW96, sqrt_price)?;
    let cost_x = as_u128(mul_div_ceil(l, POW96, sqrt_price)?)?;
    Ok((cost_x, acquire_y))
}

/// swap_math.rs:368 (`SwapMathX2Y.x2YAtPriceLiquidity`)
fn x2y_at_price_liquidity(
    amount_x: Balance,
    sqrt_price: U512,
    liquidity: Balance,
    liquidity_x: Balance,
) -> SwapResult<(Balance, U512, Balance)> {
    let liquidity_y = checked_sub(liquidity, liquidity_x)?;
    let transform =
        mul_div_floor(U512::from(amount_x), sqrt_price, POW96)?.min(U512::from(liquidity_y));
    let cost_x = as_u128(mul_div_ceil(transform, POW96, sqrt_price)?)?;
    let acquire_y = mul_div_floor(transform, sqrt_price, POW96)?;
    let new_liquidity_x = checked_add(liquidity_x, as_u128(transform)?)?;
    Ok((cost_x, acquire_y, new_liquidity_x))
}

/// swap_math.rs:118 (`SwapMathX2Y.x2YRangeComplete`), moving from `right` down to `left`.
/// Differs from iZiSwap: the range max and the partial cost come from `get_amount_x`, which
/// rounds sqrt_price(right - 1) down, while the location search rounds it up (common_math.rs:333),
/// and an out-of-range location panics instead of being clamped.
fn x2y_range_complete(
    liquidity: Balance,
    sqrt_price_l: U512,
    left_point: i32,
    sqrt_price_r: U512,
    right_point: i32,
    amount_x: Balance,
) -> SwapResult<RangeComplete<Balance, U512>> {
    let max_x = as_u128(get_amount_x(
        liquidity,
        left_point,
        right_point,
        sqrt_price_r,
        true,
    )?)?;
    if max_x <= amount_x {
        return Ok(RangeComplete::complete(
            max_x,
            get_amount_y(liquidity, sqrt_price_l, sqrt_price_r, false)?,
        ));
    }
    let sqrt_price_pr_m1 = mul_div_ceil(sqrt_price_r, POW96, *SQRT_RATE_96)?;
    let sqrt_value = add(
        mul_div_floor(
            U512::from(amount_x),
            sub(sqrt_price_r, sqrt_price_pr_m1)?,
            U512::from(liquidity),
        )?,
        POW96,
    )?;
    let loc_point = right_point - get_log_sqrt_price_floor(sqrt_value)?;
    if loc_point > right_point {
        return Err(SwapError::Panic("E208: loc_pt > right_point"));
    }
    if loc_point <= left_point {
        return Err(SwapError::Panic("E209: loc_pt <= left_point"));
    }
    if loc_point == right_point {
        let loc_point = loc_point - 1;
        return Ok(RangeComplete::partial(
            0,
            U512::zero(),
            loc_point,
            get_sqrt_price(loc_point)?,
        ));
    }
    let cost_x = as_u128(
        get_amount_x(liquidity, loc_point, right_point, sqrt_price_r, true)?
            .min(U512::from(amount_x)),
    )?;
    let loc_point = loc_point - 1;
    let sqrt_loc = get_sqrt_price(loc_point)?;
    let acquire_y = get_amount_y(liquidity, next_sqrt_price(sqrt_loc)?, sqrt_price_r, false)?;
    Ok(RangeComplete::partial(
        cost_x, acquire_y, loc_point, sqrt_loc,
    ))
}

/// swap_math.rs:118 (`SwapMathX2Y.x2YRange`): swaps `amount_x` down to `left` (inclusive)
pub fn x2y_range(
    state: &CurrentState,
    left_point: i32,
    mut amount_x: Balance,
) -> SwapResult<RangeRet<Balance, U512>> {
    let (mut point, mut sqrt_price) = (state.point, state.sqrt_price);
    let mut cost_x: Balance = 0;
    let mut acquire_y = U512::zero();
    let mut liquidity_x = 0;
    let current_has_y = state.liquidity_x < state.liquidity;
    if current_has_y && (state.liquidity_x > 0 || left_point == point) {
        let (cost, acquire, new_liquidity_x) =
            x2y_at_price_liquidity(amount_x, sqrt_price, state.liquidity, state.liquidity_x)?;
        (cost_x, acquire_y, liquidity_x) = (cost, acquire, new_liquidity_x);
        if new_liquidity_x < state.liquidity || cost >= amount_x {
            return Ok(RangeRet {
                finished: true,
                cost: cost_x,
                acquire: acquire_y,
                final_point: point,
                sqrt_final_price: sqrt_price,
                liquidity_x,
            });
        }
        amount_x -= cost;
    } else if current_has_y {
        // All liquidity at the current point is in Y, so the point joins the range
        point += 1;
        sqrt_price = next_sqrt_price(sqrt_price)?;
    } else {
        liquidity_x = state.liquidity_x;
    }
    if left_point < point {
        let sqrt_price_l = get_sqrt_price(left_point)?;
        let range = x2y_range_complete(
            state.liquidity,
            sqrt_price_l,
            left_point,
            sqrt_price,
            point,
            amount_x,
        )?;
        cost_x = checked_add(cost_x, range.cost)?;
        amount_x = checked_sub(amount_x, range.cost)?;
        acquire_y = add(acquire_y, range.acquire)?;
        if range.complete {
            return Ok(RangeRet {
                finished: amount_x == 0,
                cost: cost_x,
                acquire: acquire_y,
                final_point: left_point,
                sqrt_final_price: sqrt_price_l,
                liquidity_x: state.liquidity,
            });
        }
        let (cost, acquire, new_liquidity_x) =
            x2y_at_price_liquidity(amount_x, range.sqrt_loc, state.liquidity, 0)?;
        return Ok(RangeRet {
            finished: true,
            cost: checked_add(cost_x, cost)?,
            acquire: add(acquire_y, acquire)?,
            final_point: range.loc_point,
            sqrt_final_price: range.sqrt_loc,
            liquidity_x: new_liquidity_x,
        });
    }
    Ok(RangeRet {
        finished: false,
        cost: cost_x,
        acquire: acquire_y,
        final_point: point,
        sqrt_final_price: sqrt_price,
        liquidity_x,
    })
}

// ---------------------------------------------------------------------------------------------
// Y -> X, exact input

/// `SwapMathY2X.y2XAtPrice`: fills a limit order selling `curr_x` at `sqrt_price`
pub fn y2x_at_price(
    amount_y: Balance,
    sqrt_price: U512,
    curr_x: Balance,
) -> SwapResult<(Balance, Balance)> {
    let l = mul_div_floor(U512::from(amount_y), POW96, sqrt_price)?;
    let acquire_x = as_u128(mul_div_floor(l, POW96, sqrt_price)?.min(U512::from(curr_x)))?;
    let l = mul_div_ceil(U512::from(acquire_x), sqrt_price, POW96)?;
    let cost_y = as_u128(mul_div_ceil(l, sqrt_price, POW96)?)?;
    Ok((cost_y, acquire_x))
}

/// `SwapMathY2X.y2XAtPriceLiquidity`
fn y2x_at_price_liquidity(
    amount_y: Balance,
    sqrt_price: U512,
    liquidity_x: Balance,
) -> SwapResult<(Balance, U512, Balance)> {
    let transform =
        mul_div_floor(U512::from(amount_y), POW96, sqrt_price)?.min(U512::from(liquidity_x));
    let cost_y = as_u128(mul_div_ceil(transform, sqrt_price, POW96)?)?;
    let acquire_x = mul_div_floor(transform, POW96, sqrt_price)?;
    let new_liquidity_x = checked_sub(liquidity_x, as_u128(transform)?)?;
    Ok((cost_y, acquire_x, new_liquidity_x))
}

/// swap_math.rs (`SwapMathY2X.y2XRangeComplete`), moving from `left` up to `right`. An
/// out-of-range location panics (E210/E211) instead of being clamped.
fn y2x_range_complete(
    liquidity: Balance,
    sqrt_price_l: U512,
    left_point: i32,
    sqrt_price_r: U512,
    right_point: i32,
    amount_y: Balance,
) -> SwapResult<RangeComplete<Balance, U512>> {
    let max_y = get_amount_y(liquidity, sqrt_price_l, sqrt_price_r, true)?;
    if max_y <= U512::from(amount_y) {
        return Ok(RangeComplete::complete(
            as_u128(max_y)?,
            get_amount_x(liquidity, left_point, right_point, sqrt_price_r, false)?,
        ));
    }
    let sqrt_loc = add(
        mul_div_floor(
            U512::from(amount_y),
            *SQRT_RATE_96 - POW96,
            U512::from(liquidity),
        )?,
        sqrt_price_l,
    )?;
    let loc_point = get_log_sqrt_price_floor(sqrt_loc)?;
    if loc_point < left_point {
        return Err(SwapError::Panic("E210: loc_pt < left_point"));
    }
    if loc_point >= right_point {
        return Err(SwapError::Panic("E211: loc_pt >= right_point"));
    }
    let sqrt_loc = get_sqrt_price(loc_point)?;
    if loc_point == left_point {
        return Ok(RangeComplete::partial(0, U512::zero(), loc_point, sqrt_loc));
    }
    let cost_y =
        as_u128(get_amount_y(liquidity, sqrt_price_l, sqrt_loc, true)?.min(U512::from(amount_y)))?;
    let acquire_x = get_amount_x(liquidity, left_point, loc_point, sqrt_loc, false)?;
    Ok(RangeComplete::partial(
        cost_y, acquire_x, loc_point, sqrt_loc,
    ))
}

/// swap_math.rs:185 (`SwapMathY2X.y2XRange`): swaps `amount_y` up to `right` (exclusive)
pub fn y2x_range(
    state: &CurrentState,
    right_point: i32,
    mut amount_y: Balance,
) -> SwapResult<RangeRet<Balance, U512>> {
    let (mut point, mut sqrt_price) = (state.point, state.sqrt_price);
    let mut cost_y: Balance = 0;
    let mut acquire_x = U512::zero();
    if state.liquidity_x < state.liquidity {
        let (cost, acquire, new_liquidity_x) =
            y2x_at_price_liquidity(amount_y, sqrt_price, state.liquidity_x)?;
        (cost_y, acquire_x) = (cost, acquire);
        if new_liquidity_x > 0 || cost >= amount_y {
            return Ok(RangeRet {
                finished: true,
                cost: cost_y,
                acquire: acquire_x,
                final_point: point,
                sqrt_final_price: sqrt_price,
                liquidity_x: new_liquidity_x,
            });
        }
        amount_y -= cost;
        point += 1;
        if point == right_point {
            return Ok(RangeRet {
                finished: false,
                cost: cost_y,
                acquire: acquire_x,
                final_point: point,
                sqrt_final_price: get_sqrt_price(right_point)?,
                liquidity_x: 0,
            });
        }
        sqrt_price = next_sqrt_price(sqrt_price)?;
    }
    let sqrt_price_r = get_sqrt_price(right_point)?;
    let range = y2x_range_complete(
        state.liquidity,
        sqrt_price,
        point,
        sqrt_price_r,
        right_point,
        amount_y,
    )?;
    cost_y = checked_add(cost_y, range.cost)?;
    amount_y = checked_sub(amount_y, range.cost)?;
    acquire_x = add(acquire_x, range.acquire)?;
    if range.complete {
        return Ok(RangeRet {
            finished: amount_y == 0,
            cost: cost_y,
            acquire: acquire_x,
            final_point: right_point,
            sqrt_final_price: sqrt_price_r,
            liquidity_x: 0,
        });
    }
    let (cost, acquire, new_liquidity_x) =
        y2x_at_price_liquidity(amount_y, range.sqrt_loc, state.liquidity)?;
    Ok(RangeRet {
        finished: true,
        cost: checked_add(cost_y, cost)?,
        acquire: add(acquire_x, acquire)?,
        final_point: range.loc_point,
        sqrt_final_price: range.sqrt_loc,
        liquidity_x: new_liquidity_x,
    })
}

// ---------------------------------------------------------------------------------------------
// X -> Y, exact output ("desire Y")

/// `SwapMathX2YDesire.x2YAtPrice`: fills a limit order selling `curr_y` at `sqrt_price`
pub fn x2y_at_price_desire(
    desire_y: Balance,
    sqrt_price: U512,
    curr_y: Balance,
) -> SwapResult<(Balance, Balance)> {
    let acquire_y = desire_y.min(curr_y);
    let l = mul_div_ceil(U512::from(acquire_y), POW96, sqrt_price)?;
    let cost_x = as_u128(mul_div_ceil(l, POW96, sqrt_price)?)?;
    Ok((cost_x, acquire_y))
}

/// swap_math.rs:582 (`SwapMathX2YDesire.x2YAtPriceLiquidity`)
fn x2y_at_price_liquidity_desire(
    desire_y: Balance,
    sqrt_price: U512,
    liquidity: Balance,
    liquidity_x: Balance,
) -> SwapResult<(U512, Balance, Balance)> {
    let liquidity_y = checked_sub(liquidity, liquidity_x)?;
    let transform =
        mul_div_ceil(U512::from(desire_y), POW96, sqrt_price)?.min(U512::from(liquidity_y));
    let cost_x = mul_div_ceil(transform, POW96, sqrt_price)?;
    let acquire_y = as_u128(mul_div_floor(transform, sqrt_price, POW96)?)?;
    let new_liquidity_x = checked_add(liquidity_x, as_u128(transform)?)?;
    Ok((cost_x, acquire_y, new_liquidity_x))
}

/// swap_math.rs:636 (`SwapMathX2YDesire.x2YRangeComplete`). Differs from iZiSwap only in
/// narrowing the whole-range max to u128.
fn x2y_range_complete_desire(
    liquidity: Balance,
    sqrt_price_l: U512,
    left_point: i32,
    sqrt_price_r: U512,
    right_point: i32,
    desire_y: Balance,
) -> SwapResult<RangeComplete<U512, Balance>> {
    let max_y = as_u128(get_amount_y(liquidity, sqrt_price_l, sqrt_price_r, false)?)?;
    if max_y <= desire_y {
        return Ok(RangeComplete::complete(
            get_amount_x(liquidity, left_point, right_point, sqrt_price_r, true)?,
            max_y,
        ));
    }
    let cl = sub(
        sqrt_price_r,
        mul_div_floor(
            U512::from(desire_y),
            *SQRT_RATE_96 - POW96,
            U512::from(liquidity),
        )?,
    )?;
    let loc_point = (get_log_sqrt_price_floor(cl)? + 1)
        .min(right_point)
        .max(left_point + 1);
    if loc_point == right_point {
        let loc_point = loc_point - 1;
        return Ok(RangeComplete::partial(
            U512::zero(),
            0,
            loc_point,
            get_sqrt_price(loc_point)?,
        ));
    }
    let sqrt_price_pr_mloc = get_sqrt_price(right_point - loc_point)?;
    let sqrt_price_pr_m1 = mul_div_ceil(sqrt_price_r, POW96, *SQRT_RATE_96)?;
    let cost_x = mul_div_ceil(
        U512::from(liquidity),
        sub(sqrt_price_pr_mloc, POW96)?,
        sub(sqrt_price_r, sqrt_price_pr_m1)?,
    )?;
    let loc_point = loc_point - 1;
    let sqrt_loc = get_sqrt_price(loc_point)?;
    let acquire_y = as_u128(get_amount_y(
        liquidity,
        next_sqrt_price(sqrt_loc)?,
        sqrt_price_r,
        false,
    )?)?
    .min(desire_y);
    Ok(RangeComplete::partial(
        cost_x, acquire_y, loc_point, sqrt_loc,
    ))
}

/// swap_math.rs:253 (`SwapMathX2YDesire.x2YRange`): buys `desire_y` moving down to `left`
pub fn x2y_range_desire(
    state: &CurrentState,
    left_point: i32,
    mut desire_y: Balance,
) -> SwapResult<RangeRet<U512, Balance>> {
    let (mut point, mut sqrt_price) = (state.point, state.sqrt_price);
    let mut cost_x = U512::zero();
    let mut acquire_y: Balance = 0;
    let mut liquidity_x = 0;
    let current_has_y = state.liquidity_x < state.liquidity;
    if current_has_y && (state.liquidity_x > 0 || left_point == point) {
        let (cost, acquire, new_liquidity_x) = x2y_at_price_liquidity_desire(
            desire_y,
            sqrt_price,
            state.liquidity,
            state.liquidity_x,
        )?;
        (cost_x, acquire_y, liquidity_x) = (cost, acquire, new_liquidity_x);
        if new_liquidity_x < state.liquidity || acquire >= desire_y {
            return Ok(RangeRet {
                finished: true,
                cost: cost_x,
                acquire: acquire_y,
                final_point: point,
                sqrt_final_price: sqrt_price,
                liquidity_x,
            });
        }
        desire_y -= acquire;
    } else if current_has_y {
        point += 1;
        sqrt_price = next_sqrt_price(sqrt_price)?;
    } else {
        liquidity_x = state.liquidity_x;
    }
    if left_point < point {
        let sqrt_price_l = get_sqrt_price(left_point)?;
        let range = x2y_range_complete_desire(
            state.liquidity,
            sqrt_price_l,
            left_point,
            sqrt_price,
            point,
            desire_y,
        )?;
        cost_x = add(cost_x, range.cost)?;
        desire_y = checked_sub(desire_y, range.acquire)?;
        acquire_y = checked_add(acquire_y, range.acquire)?;
        if range.complete {
            return Ok(RangeRet {
                finished: desire_y == 0,
                cost: cost_x,
                acquire: acquire_y,
                final_point: left_point,
                sqrt_final_price: sqrt_price_l,
                liquidity_x: state.liquidity,
            });
        }
        let (cost, acquire, new_liquidity_x) =
            x2y_at_price_liquidity_desire(desire_y, range.sqrt_loc, state.liquidity, 0)?;
        return Ok(RangeRet {
            finished: true,
            cost: add(cost_x, cost)?,
            acquire: checked_add(acquire_y, acquire)?,
            final_point: range.loc_point,
            sqrt_final_price: range.sqrt_loc,
            liquidity_x: new_liquidity_x,
        });
    }
    Ok(RangeRet {
        finished: false,
        cost: cost_x,
        acquire: acquire_y,
        final_point: point,
        sqrt_final_price: sqrt_price,
        liquidity_x,
    })
}

// ---------------------------------------------------------------------------------------------
// Y -> X, exact output ("desire X")

/// `SwapMathY2XDesire.y2XAtPrice`: fills a limit order selling `curr_x` at `sqrt_price`
pub fn y2x_at_price_desire(
    desire_x: Balance,
    sqrt_price: U512,
    curr_x: Balance,
) -> SwapResult<(Balance, Balance)> {
    let acquire_x = desire_x.min(curr_x);
    let l = mul_div_ceil(U512::from(acquire_x), sqrt_price, POW96)?;
    let cost_y = as_u128(mul_div_ceil(l, sqrt_price, POW96)?)?;
    Ok((cost_y, acquire_x))
}

/// swap_math.rs:605 (`SwapMathY2XDesire.y2XAtPriceLiquidity`)
fn y2x_at_price_liquidity_desire(
    desire_x: Balance,
    sqrt_price: U512,
    liquidity_x: Balance,
) -> SwapResult<(U512, Balance, Balance)> {
    let transform =
        mul_div_ceil(U512::from(desire_x), sqrt_price, POW96)?.min(U512::from(liquidity_x));
    let cost_y = mul_div_ceil(transform, sqrt_price, POW96)?;
    let acquire_x = as_u128(mul_div_floor(transform, POW96, sqrt_price)?)?;
    let new_liquidity_x = checked_sub(liquidity_x, as_u128(transform)?)?;
    Ok((cost_y, acquire_x, new_liquidity_x))
}

/// swap_math.rs:690 (`SwapMathY2XDesire.y2XRangeComplete`). Differs from iZiSwap only in
/// narrowing the whole-range max to u128.
fn y2x_range_complete_desire(
    liquidity: Balance,
    sqrt_price_l: U512,
    left_point: i32,
    sqrt_price_r: U512,
    right_point: i32,
    desire_x: Balance,
) -> SwapResult<RangeComplete<U512, Balance>> {
    let max_x = as_u128(get_amount_x(
        liquidity,
        left_point,
        right_point,
        sqrt_price_r,
        false,
    )?)?;
    if max_x <= desire_x {
        return Ok(RangeComplete::complete(
            get_amount_y(liquidity, sqrt_price_l, sqrt_price_r, true)?,
            max_x,
        ));
    }
    let sqrt_price_pr_pl = get_sqrt_price(right_point - left_point)?;
    let sqrt_price_pr_m1 = mul_div_floor(sqrt_price_r, POW96, *SQRT_RATE_96)?;
    let div = sub(
        sqrt_price_pr_pl,
        mul_div_floor(
            U512::from(desire_x),
            sub(sqrt_price_r, sqrt_price_pr_m1)?,
            U512::from(liquidity),
        )?,
    )?;
    let loc_point = get_log_sqrt_price_floor(mul_div_floor(sqrt_price_r, POW96, div)?)?
        .max(left_point)
        .min(right_point - 1);
    let sqrt_loc = get_sqrt_price(loc_point)?;
    if loc_point == left_point {
        return Ok(RangeComplete::partial(U512::zero(), 0, loc_point, sqrt_loc));
    }
    let acquire_x = as_u128(get_amount_x(
        liquidity, left_point, loc_point, sqrt_loc, false,
    )?)?
    .min(desire_x);
    let cost_y = get_amount_y(liquidity, sqrt_price_l, sqrt_loc, true)?;
    Ok(RangeComplete::partial(
        cost_y, acquire_x, loc_point, sqrt_loc,
    ))
}

/// swap_math.rs:315 (`SwapMathY2XDesire.y2XRange`): buys `desire_x` moving up to `right`
pub fn y2x_range_desire(
    state: &CurrentState,
    right_point: i32,
    mut desire_x: Balance,
) -> SwapResult<RangeRet<U512, Balance>> {
    let (mut point, mut sqrt_price) = (state.point, state.sqrt_price);
    let mut cost_y = U512::zero();
    let mut acquire_x: Balance = 0;
    if state.liquidity_x < state.liquidity {
        let (cost, acquire, new_liquidity_x) =
            y2x_at_price_liquidity_desire(desire_x, sqrt_price, state.liquidity_x)?;
        (cost_y, acquire_x) = (cost, acquire);
        if new_liquidity_x > 0 || acquire >= desire_x {
            return Ok(RangeRet {
                finished: true,
                cost: cost_y,
                acquire: acquire_x,
                final_point: point,
                sqrt_final_price: sqrt_price,
                liquidity_x: new_liquidity_x,
            });
        }
        desire_x -= acquire;
        point += 1;
        if point == right_point {
            return Ok(RangeRet {
                finished: false,
                cost: cost_y,
                acquire: acquire_x,
                final_point: point,
                sqrt_final_price: get_sqrt_price(right_point)?,
                liquidity_x: 0,
            });
        }
        sqrt_price = next_sqrt_price(sqrt_price)?;
    }
    let sqrt_price_r = get_sqrt_price(right_point)?;
    let range = y2x_range_complete_desire(
        state.liquidity,
        sqrt_price,
        point,
        sqrt_price_r,
        right_point,
        desire_x,
    )?;
    cost_y = add(cost_y, range.cost)?;
    acquire_x = checked_add(acquire_x, range.acquire)?;
    desire_x = checked_sub(desire_x, range.acquire)?;
    if range.complete {
        return Ok(RangeRet {
            finished: desire_x == 0,
            cost: cost_y,
            acquire: acquire_x,
            final_point: right_point,
            sqrt_final_price: sqrt_price_r,
            liquidity_x: 0,
        });
    }
    let (cost, acquire, new_liquidity_x) =
        y2x_at_price_liquidity_desire(desire_x, range.sqrt_loc, state.liquidity)?;
    Ok(RangeRet {
        finished: true,
        cost: add(cost_y, cost)?,
        acquire: checked_add(acquire_x, acquire)?,
        final_point: range.loc_point,
        sqrt_final_price: range.sqrt_loc,
        liquidity_x: new_liquidity_x,
    })
}
