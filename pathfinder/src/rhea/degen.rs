use near_min_api::types::Balance;

use super::{Amounts, FEE_DIVISOR, U384, u128_ratio};

pub const TARGET_DECIMAL: u8 = 24;
pub const PRECISION: u128 = 10u128.pow(TARGET_DECIMAL as u32);
pub const MIN_RESERVE: u128 = PRECISION / 1_000_u128;

/// Stable Swap Fee calculator.
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

/// The DegenSwap invariant calculator.
pub struct DegenSwap<'a> {
    amp: u128,
    degens: &'a [Balance],
}

impl<'a> DegenSwap<'a> {
    pub fn new(amp: u64, degens: &'a [Balance]) -> Self {
        Self {
            amp: amp as u128,
            degens,
        }
    }

    /// *
    fn mul_degen(&self, amount: Balance, degen: Balance) -> Balance {
        (U384::from(amount) * U384::from(degen) / U384::from(PRECISION)).to::<u128>()
    }

    /// *
    fn div_degen(&self, amount: Balance, degen: Balance) -> Balance {
        (U384::from(amount) * U384::from(PRECISION) / U384::from(degen)).to::<u128>()
    }

    /// *
    pub fn degen_balances(&self, amounts: &[Balance]) -> Amounts {
        amounts
            .iter()
            .zip(self.degens.iter())
            .map(|(&amount, &degen)| self.mul_degen(amount, degen))
            .collect()
    }

    /// Invariant (D) of the pool with `c_amounts`
    pub fn invariant(&self, c_amounts: &[Balance]) -> Option<U384> {
        self.compute_d(&self.degen_balances(c_amounts))
    }

    /// Compute the amplification coefficient (A)
    pub fn compute_amp_factor(&self) -> Option<Balance> {
        Some(self.amp)
    }

    /// Compute stable swap invariant (D)
    /// Equation:
    /// A * sum(x_i) * n**n + D = A * D * n**n + D**(n+1) / (n**n * prod(x_i))
    pub fn compute_d(&self, c_amounts: &[Balance]) -> Option<U384> {
        let n_coins = c_amounts.len() as u128;
        let sum_x = c_amounts.iter().sum::<u128>();
        if sum_x == 0 {
            Some(U384::from(0))
        } else {
            let amp_factor = self.compute_amp_factor()?;
            let mut d_prev: U384;
            let mut d: U384 = U384::from(sum_x);
            for _ in 0..256 {
                // $ D_{k,prod} = \frac{D_k^{n+1}}{n^n \prod x_{i}} = \frac{D^3}{4xy} $
                let mut d_prod = d;
                for c_amount in c_amounts {
                    d_prod = d_prod
                        .checked_mul(d)?
                        .checked_div(U384::from(c_amount * n_coins))?;
                }
                d_prev = d;

                let ann = amp_factor.checked_mul(n_coins.checked_pow(n_coins as u32)?)?;
                let leverage = (U384::from(sum_x)).checked_mul(U384::from(ann))?;
                // d = (ann * sum_x + d_prod * n_coins) * d_prev / ((ann - 1) * d_prev + (n_coins + 1) * d_prod)
                let numerator = d_prev.checked_mul(
                    d_prod
                        .checked_mul(U384::from(n_coins))?
                        .checked_add(leverage)?,
                )?;
                let denominator = d_prev
                    .checked_mul(U384::from(ann.checked_sub(1)?))?
                    .checked_add(d_prod.checked_mul(U384::from(n_coins + 1))?)?;
                d = numerator.checked_div(denominator)?;

                // Equality with the precision of 1
                if d > d_prev {
                    if d.checked_sub(d_prev)? <= U384::from(1) {
                        break;
                    }
                } else if d_prev.checked_sub(d)? <= U384::from(1) {
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
        d: U384,             // invariant of current_c_amounts
    ) -> Option<U384> {
        let n_coins = current_c_amounts.len() as u128;
        let amp_factor = self.compute_amp_factor()?;
        let ann = amp_factor.checked_mul(n_coins.checked_pow(n_coins as u32)?)?;
        let mut s_ = x_c_amount;
        let mut c = d.checked_mul(d)?.checked_div(U384::from(x_c_amount))?;
        for (idx, c_amount) in current_c_amounts.iter().enumerate() {
            if idx != index_x && idx != index_y {
                s_ += *c_amount;
                c = c.checked_mul(d)?.checked_div(U384::from(*c_amount))?;
            }
        }
        c = c.checked_mul(d)?.checked_div(U384::from(
            ann.checked_mul(n_coins.checked_pow(n_coins as u32)?)?,
        ))?;

        let b = d
            .checked_div(U384::from(ann))?
            .checked_add(U384::from(s_))?; // d will be subtracted later

        // Solve for y by approximating: y**2 + b*y = c
        let mut y_prev: U384;
        let mut y = d;
        for _ in 0..256 {
            y_prev = y;
            // $ y_{k+1} = \frac{y_k^2 + c}{2y_k + b - D} $
            let y_numerator = y.checked_mul(y)?.checked_add(c)?;
            let y_denominator = y
                .checked_mul(U384::from(2))?
                .checked_add(b)?
                .checked_sub(d)?;
            y = y_numerator.checked_div(y_denominator)?;
            if y > y_prev {
                if y.checked_sub(y_prev)? <= U384::from(1) {
                    break;
                }
            } else if y_prev.checked_sub(y)? <= U384::from(1) {
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
        d: Option<U384>, // invariant of current_c_amounts if known
    ) -> Option<SwapResult> {
        let degen_in = self.degens[token_in_idx];
        let degen_out = self.degens[token_out_idx];

        // * degen input
        let token_in_amount_degen = self.mul_degen(token_in_amount, degen_in);
        let current_c_amounts_degen = self.degen_balances(current_c_amounts);
        let d = match d {
            Some(d) => d,
            None => self.compute_d(&current_c_amounts_degen)?,
        };

        let y = self
            .compute_y(
                token_in_amount_degen + current_c_amounts_degen[token_in_idx],
                &current_c_amounts_degen,
                token_in_idx,
                token_out_idx,
                d,
            )?
            .to::<u128>();

        let dy = current_c_amounts_degen[token_out_idx]
            .checked_sub(y)?
            .saturating_sub(1); // * ? curve sub -1 just in case there were some rounding errors

        let trade_fee = fees.trade_fee(dy);
        let amount_swapped = dy.checked_sub(trade_fee)?;

        let new_destination_amount =
            current_c_amounts_degen[token_out_idx].checked_sub(amount_swapped)?;
        let new_source_amount =
            current_c_amounts_degen[token_in_idx].checked_add(token_in_amount_degen)?;

        // * degen back result
        Some(SwapResult::new(
            self.div_degen(new_source_amount, degen_in),
            self.div_degen(new_destination_amount, degen_out),
            self.div_degen(amount_swapped, degen_out),
        ))
    }
}
