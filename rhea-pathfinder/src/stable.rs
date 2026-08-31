use near_min_api::types::Balance;

use crate::{FEE_DIVISOR, U256, u128_ratio};

pub const TARGET_DECIMAL: u8 = 18;
pub const MIN_RESERVE: u128 = 10u128.pow(TARGET_DECIMAL as u32) / 1_000_u128;

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

/// The StableSwap invariant calculator.
pub struct StableSwap {
    amp: u128,
}

impl StableSwap {
    pub fn new(amp: u64) -> Self {
        Self { amp: amp as u128 }
    }

    /// Compute the amplification coefficient (A)
    pub fn compute_amp_factor(&self) -> Option<Balance> {
        Some(self.amp)
    }

    /// Compute stable swap invariant (D)
    /// Equation:
    /// A * sum(x_i) * n**n + D = A * D * n**n + D**(n+1) / (n**n * prod(x_i))
    pub fn compute_d(&self, c_amounts: &[Balance]) -> Option<U256> {
        let n_coins = c_amounts.len() as u128;
        let sum_x = c_amounts.iter().sum::<u128>();
        if sum_x == 0 {
            Some(0.into())
        } else {
            let amp_factor = self.compute_amp_factor()?;
            let mut d_prev: U256;
            let mut d: U256 = sum_x.into();
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
                let leverage = (U256::from(sum_x)).checked_mul(ann.into())?;
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
        current_c_amounts: &[Balance], // in-pool tokens amount in comparable precision,
        index_x: usize,      // x token's index
        index_y: usize,      // y token's index
    ) -> Option<U256> {
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
        let mut y_prev: U256;
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
        token_in_idx: usize,           // token_in index in token vector,
        token_in_amount: Balance,      // token_in amount in comparable precision (1e18),
        token_out_idx: usize,          // token_out index in token vector,
        current_c_amounts: &[Balance], // in-pool tokens comparable amounts vector,
        fees: &Fees,
    ) -> Option<SwapResult> {
        let y = self
            .compute_y(
                token_in_amount + current_c_amounts[token_in_idx],
                current_c_amounts,
                token_in_idx,
                token_out_idx,
            )?
            .as_u128();
        // https://github.com/curvefi/curve-contract/blob/b0bbf77f8f93c9c5f4e415bce9cd71f0cdee960e/contracts/pool-templates/base/SwapTemplateBase.vy#L466
        let dy = current_c_amounts[token_out_idx]
            .checked_sub(y)?
            .saturating_sub(1);

        let trade_fee = fees.trade_fee(dy);
        let amount_swapped = dy.checked_sub(trade_fee)?;

        let new_destination_amount =
            current_c_amounts[token_out_idx].checked_sub(amount_swapped)?;
        let new_source_amount = current_c_amounts[token_in_idx].checked_add(token_in_amount)?;

        Some(SwapResult {
            new_source_amount,
            new_destination_amount,
            amount_swapped,
        })
    }
}
