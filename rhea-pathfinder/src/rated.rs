use near_min_api::types::Balance;

use crate::{FEE_DIVISOR, U384, u128_ratio};

pub const TARGET_DECIMAL: u8 = 24;
pub const PRECISION: u128 = 10u128.pow(TARGET_DECIMAL as u32);
pub const MIN_RESERVE: u128 = PRECISION / 1_000_u128;

pub struct Fees {
    pub trade_fee: u32,
}

impl Fees {
    pub fn new(total_fee: u32) -> Self {
        Self {
            trade_fee: total_fee,
        }
    }

    pub fn trade_fee(&self, amount: Balance) -> Balance {
        u128_ratio(amount, self.trade_fee as u128, FEE_DIVISOR as u128)
    }
}

/// Encodes all results of swapping from a source token to a destination token.
#[derive(Debug)]
pub struct SwapResult {
    /// New amount of source token.
    pub new_source_amount: Balance,
    /// New amount of destination token.
    pub new_destination_amount: Balance,
    /// Amount of destination token swapped.
    pub amount_swapped: Balance,
}

impl SwapResult {
    pub fn new(
        new_source_amount: Balance,
        new_destination_amount: Balance,
        amount_swapped: Balance,
    ) -> Self {
        Self {
            new_source_amount,
            new_destination_amount,
            amount_swapped,
        }
    }
}

/// The RatedSwap invariant calculator.
pub struct RatedSwap {
    amp: u128,
    rates: Vec<Balance>,
}

impl RatedSwap {
    pub fn new(amp: u64, rates: &Vec<Balance>) -> Self {
        Self {
            amp: amp as u128,
            rates: rates.clone(),
        }
    }

    /// *
    fn mul_rate(&self, amount: Balance, rate: Balance) -> Balance {
        (U384::from(amount) * U384::from(rate) / U384::from(PRECISION)).as_u128()
    }

    /// *
    fn div_rate(&self, amount: Balance, rate: Balance) -> Balance {
        (U384::from(amount) * U384::from(PRECISION) / U384::from(rate)).as_u128()
    }

    /// *
    fn rate_balances(&self, amounts: &Vec<Balance>) -> Vec<Balance> {
        amounts
            .iter()
            .zip(self.rates.iter())
            .map(|(&amount, &rate)| self.mul_rate(amount, rate))
            .collect()
    }

    /// Compute the amplification coefficient (A)
    pub fn compute_amp_factor(&self) -> Option<Balance> {
        Some(self.amp)
    }

    /// Compute stable swap invariant (D)
    /// Equation:
    /// A * sum(x_i) * n**n + D = A * D * n**n + D**(n+1) / (n**n * prod(x_i))
    pub fn compute_d(&self, c_amounts: &Vec<Balance>) -> Option<U384> {
        let n_coins = c_amounts.len() as u128;
        let sum_x = c_amounts.iter().sum::<u128>();
        if sum_x == 0 {
            Some(0.into())
        } else {
            let amp_factor = self.compute_amp_factor()?;
            let mut d_prev: U384;
            let mut d: U384 = sum_x.into();
            for _ in 0..256 {
                // $ D_{k,prod} = \frac{D_k^{n+1}}{n^n \prod x_{i}} = \frac{D^3}{4xy} $
                let mut d_prod = d;
                for c_amount in c_amounts {
                    d_prod = d_prod
                        .checked_mul(d)?
                        .checked_div((c_amount * n_coins).into())?;
                }
                d_prev = d;

                let ann = amp_factor.checked_mul(n_coins.checked_pow(n_coins as u32)?)?;
                let leverage = (U384::from(sum_x)).checked_mul(ann.into())?;
                // d = (ann * sum_x + d_prod * n_coins) * d_prev / ((ann - 1) * d_prev + (n_coins + 1) * d_prod)
                let numerator = d_prev
                    .checked_mul(d_prod.checked_mul(n_coins.into())?.checked_add(leverage)?)?;
                let denominator = d_prev
                    .checked_mul(ann.checked_sub(1)?.into())?
                    .checked_add(d_prod.checked_mul((n_coins + 1).into())?)?;
                d = numerator.checked_div(denominator)?;

                // Equality with the precision of 1
                if d > d_prev {
                    if d.checked_sub(d_prev)? <= 1.into() {
                        break;
                    }
                } else if d_prev.checked_sub(d)? <= 1.into() {
                    break;
                }
            }
            Some(d)
        }
    }

    /// Compute new amount of token 'y' with new amount of token 'x'
    /// return new y_token amount according to the equation
    pub fn compute_y(
        &self,
        x_c_amount: Balance, // new x_token amount in comparable precision,
        current_c_amounts: &Vec<Balance>, // in-pool tokens amount in comparable precision,
        index_x: usize,      // x token's index
        index_y: usize,      // y token's index
    ) -> Option<U384> {
        let n_coins = current_c_amounts.len() as u128;
        let amp_factor = self.compute_amp_factor()?;
        let ann = amp_factor.checked_mul(n_coins.checked_pow(n_coins as u32)?)?;
        // invariant
        let d = self.compute_d(current_c_amounts)?;
        let mut s_ = x_c_amount;
        let mut c = d.checked_mul(d)?.checked_div(x_c_amount.into())?;
        for (idx, c_amount) in current_c_amounts.iter().enumerate() {
            if idx != index_x && idx != index_y {
                s_ += *c_amount;
                c = c.checked_mul(d)?.checked_div((*c_amount).into())?;
            }
        }
        c = c.checked_mul(d)?.checked_div(
            ann.checked_mul(n_coins.checked_pow(n_coins as u32)?)?
                .into(),
        )?;

        let b = d.checked_div(ann.into())?.checked_add(s_.into())?; // d will be subtracted later

        // Solve for y by approximating: y**2 + b*y = c
        let mut y_prev: U384;
        let mut y = d;
        for _ in 0..256 {
            y_prev = y;
            // $ y_{k+1} = \frac{y_k^2 + c}{2y_k + b - D} $
            let y_numerator = y.checked_pow(2.into())?.checked_add(c)?;
            let y_denominator = y.checked_mul(2.into())?.checked_add(b)?.checked_sub(d)?;
            y = y_numerator.checked_div(y_denominator)?;
            if y > y_prev {
                if y.checked_sub(y_prev)? <= 1.into() {
                    break;
                }
            } else if y_prev.checked_sub(y)? <= 1.into() {
                break;
            }
        }
        Some(y)
    }

    /// Compute SwapResult after an exchange
    /// all tokens in and out with comparable precision
    pub fn swap_to(
        &self,
        token_in_idx: usize,              // token_in index in token vector,
        token_in_amount: Balance,         // token_in amount in comparable precision (1e18),
        token_out_idx: usize,             // token_out index in token vector,
        current_c_amounts: &Vec<Balance>, // in-pool tokens comparable amounts vector,
        fees: &Fees,
    ) -> Result<SwapResult, anyhow::Error> {
        let rate_in = self.rates[token_in_idx];
        let rate_out = self.rates[token_out_idx];

        // * rate input
        let token_in_amount_rated = self.mul_rate(token_in_amount, rate_in);
        let current_c_amounts_rated = self.rate_balances(current_c_amounts);

        let y = self
            .compute_y(
                token_in_amount_rated + current_c_amounts_rated[token_in_idx],
                &current_c_amounts_rated,
                token_in_idx,
                token_out_idx,
            )
            .ok_or_else(|| anyhow::anyhow!("Failed to compute y in rated swap"))?
            .as_u128();

        let dy = current_c_amounts_rated[token_out_idx]
            .checked_sub(y)
            .ok_or_else(|| anyhow::anyhow!("Underflow computing dy in rated swap"))?
            .saturating_sub(1); // * ? curve sub -1 just in case there were some rounding errors

        let trade_fee = fees.trade_fee(dy);
        let amount_swapped = dy.checked_sub(trade_fee)
            .ok_or_else(|| anyhow::anyhow!("Underflow subtracting trade fee in rated swap"))?;

        let new_destination_amount = current_c_amounts_rated[token_out_idx]
            .checked_sub(amount_swapped)
            .ok_or_else(|| anyhow::anyhow!("Insufficient liquidity in rated pool"))?;
        let new_source_amount = current_c_amounts_rated[token_in_idx]
            .checked_add(token_in_amount_rated)
            .ok_or_else(|| anyhow::anyhow!("Overflow adding source amount in rated swap"))?;

        // * rate back result
        Ok(SwapResult::new(
            self.div_rate(new_source_amount, rate_in),
            self.div_rate(new_destination_amount, rate_out),
            self.div_rate(amount_swapped, rate_out),
        ))
    }
}
