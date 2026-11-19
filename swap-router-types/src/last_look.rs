use intear_dex_common::{FeeDiscountFraction, FULL_FEE_DISCOUNT};
use near_min_api::types::NearToken;

use crate::{
    fees::amount_in_with_referral_fees,
    hook::IntearDexRouteCosts,
    types::{net_amount, Amount, DexId, Gas, Quote, QuoteFees},
};

const MAX_LAST_LOOK_FEE_DISCOUNT_PERCENT: u32 = 99;

/// The best quote of the DEXes outside of Intear DEX, which a route on Intear DEX that gives less
/// matches with a fee discount, net of costs as the aggregator ranks them
pub struct LastLook {
    pub best_other_amount: Amount,
    /// The gas fee and storage deposits of the best other quote
    pub best_other_cost: NearToken,
    pub referral_fees: QuoteFees,
    pub route_costs: IntearDexRouteCosts,
}

/// What a swap on Intear DEX gives for the trade, as in a `HookQuote`
pub struct SwapEstimate {
    pub estimated_amount: u128,
    pub gas: Gas,
    /// Of the oracle that the swap's operations ask first, as in an `OracleRequest`. Already
    /// taken out of the input that an exact-in swap gives `estimated_amount` for, and on top of
    /// the `estimated_amount` that an exact-out swap takes.
    pub oracle_fee: Option<u128>,
}

impl LastLook {
    /// Of `quotes`, best first. None when no other DEX quotes the request.
    pub fn of(
        quotes: &[Quote],
        referral_fees: &QuoteFees,
        route_costs: &IntearDexRouteCosts,
    ) -> Option<Self> {
        let best_other_quote = quotes
            .iter()
            .find(|quote| !matches!(quote.route.dex_id, DexId::IntearDex | DexId::Hook(_)))?;
        Some(Self {
            best_other_amount: best_other_quote.route.estimated_amount,
            best_other_cost: best_other_quote
                .estimated_gas_usage
                .saturating_add(best_other_quote.storage_deposits),
            referral_fees: referral_fees.clone(),
            route_costs: route_costs.clone(),
        })
    }

    /// The smallest fee discount, in whole percents up to 99%, with which the swap matches
    /// the best other quote. None when it matches without one, or no discount makes it match.
    pub fn fee_discount(
        &self,
        swap_with: impl Fn(Option<FeeDiscountFraction>) -> Option<SwapEstimate>,
    ) -> Option<FeeDiscountFraction> {
        let percent =
            |percent: u32| FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 100 * percent);
        let matches_with =
            |fee_discount| swap_with(fee_discount).is_some_and(|swap| self.is_matched_by(&swap));
        if matches_with(None) || !matches_with(Some(percent(MAX_LAST_LOOK_FEE_DISCOUNT_PERCENT))) {
            return None;
        }
        let percents = (1..=MAX_LAST_LOOK_FEE_DISCOUNT_PERCENT).collect::<Vec<_>>();
        let first_matching = percents
            .partition_point(|&discount_percent| !matches_with(Some(percent(discount_percent))));
        Some(percent(percents[first_matching]))
    }

    /// Whether the route of the swap gets at least as much net of its costs as the best
    /// other quote
    fn is_matched_by(&self, swap: &SwapEstimate) -> bool {
        let route_amount = match self.best_other_amount {
            // The referral fees come out of the input of both
            Amount::AmountOut(_) => Amount::AmountOut(swap.estimated_amount),
            // The engine takes the referral fees and the oracle's fee on top of what the
            // DEX takes
            Amount::AmountIn(_) => Amount::AmountIn(
                amount_in_with_referral_fees(&self.referral_fees, swap.estimated_amount)
                    + swap.oracle_fee.unwrap_or_default(),
            ),
        };
        let route_cost = match swap.oracle_fee {
            Some(_) => self.route_costs.with_oracle_request,
            None => self.route_costs.without_oracle_request,
        };
        let swap_gas_cost = self
            .route_costs
            .gas_price
            .saturating_mul(swap.gas.estimated_burn.as_gas().into());
        let prices = self.route_costs.prices.as_ref();
        net_amount(
            route_amount,
            route_cost.saturating_add(swap_gas_cost),
            prices,
        ) >= net_amount(self.best_other_amount, self.best_other_cost, prices)
    }
}

/// `tear_fee_discount` taken off what's left of the fees after `last_look_fee_discount`
pub fn with_tear_fee_discount(
    last_look_fee_discount: Option<FeeDiscountFraction>,
    tear_fee_discount: Option<FeeDiscountFraction>,
) -> Option<FeeDiscountFraction> {
    match (last_look_fee_discount, tear_fee_discount) {
        (Some(last_look_fee_discount), Some(tear_fee_discount)) => {
            let full = u64::from(FULL_FEE_DISCOUNT.get());
            let last_look = u64::from(last_look_fee_discount.get());
            let tear = u64::from(tear_fee_discount.get());
            let combined = last_look + (full - last_look) * tear / full;
            Some(FeeDiscountFraction::new(
                u32::try_from(combined).expect("Combined fee discount is at most 100%"),
            ))
        }
        (last_look_fee_discount, tear_fee_discount) => last_look_fee_discount.or(tear_fee_discount),
    }
}

#[cfg(test)]
mod tests {
    use crate::types::CostPrices;

    use super::*;

    /// A unit of the quoted token is worth a Ggas, at 1 yoctoNEAR per gas
    const UNIT: NearToken = NearToken::from_yoctonear(10u128.pow(9));

    /// Without costs
    fn last_look(best_other_amount: Amount, referral_fees: &str) -> LastLook {
        LastLook {
            best_other_amount,
            best_other_cost: NearToken::from_yoctonear(0),
            referral_fees: referral_fees.parse().unwrap(),
            route_costs: IntearDexRouteCosts {
                without_oracle_request: NearToken::from_yoctonear(0),
                with_oracle_request: NearToken::from_yoctonear(0),
                gas_price: NearToken::from_yoctonear(1),
                prices: None,
            },
        }
    }

    fn percent_of(fee_discount: Option<FeeDiscountFraction>) -> u128 {
        u128::from(fee_discount.map_or(0, FeeDiscountFraction::get) / 10_000)
    }

    /// 1,000 and a unit more per percent of fee discount, burning 5 units of gas
    fn swap_out_with(fee_discount: Option<FeeDiscountFraction>, asks_oracle: bool) -> SwapEstimate {
        SwapEstimate {
            estimated_amount: 1_000 + percent_of(fee_discount),
            gas: Gas::from_ggas(5, 5),
            oracle_fee: asks_oracle.then_some(0),
        }
    }

    fn estimated_amount_out_with(
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<SwapEstimate> {
        Some(swap_out_with(fee_discount, false))
    }

    #[test]
    fn matches_the_best_other_quote_with_the_smallest_whole_percent() {
        let fee_discount = |best_amount_out| {
            last_look(Amount::AmountOut(best_amount_out), "{}")
                .fee_discount(estimated_amount_out_with)
                .map(FeeDiscountFraction::get)
        };
        assert_eq!(fee_discount(1_000), None);
        assert_eq!(fee_discount(1_001), Some(10_000));
        assert_eq!(fee_discount(1_050), Some(500_000));
        assert_eq!(fee_discount(1_099), Some(990_000));
        // Not worth giving away when it still isn't the best
        assert_eq!(fee_discount(1_100), None);
    }

    #[test]
    fn exact_out_matches_with_the_referral_fees_on_top() {
        let referral_fees = r#"{"ref.near":0.01}"#;
        let fee_discount = |best_amount_in| {
            last_look(Amount::AmountIn(best_amount_in), referral_fees)
                .fee_discount(|fee_discount| {
                    Some(SwapEstimate {
                        estimated_amount: 1_100 - percent_of(fee_discount),
                        gas: Gas::from_ggas(5, 5),
                        oracle_fee: None,
                    })
                })
                .map(FeeDiscountFraction::get)
        };
        let with_fees =
            |amount_in| amount_in_with_referral_fees(&referral_fees.parse().unwrap(), amount_in);
        assert_eq!(fee_discount(with_fees(1_100)), None);
        assert_eq!(fee_discount(with_fees(1_050)), Some(500_000));
        // 1,038 with 12 of fees on top
        assert_eq!(fee_discount(1_050), Some(620_000));
    }

    #[test]
    fn matches_net_of_the_costs_of_both_routes() {
        let costing = |prices| LastLook {
            best_other_amount: Amount::AmountOut(1_050),
            best_other_cost: UNIT.saturating_mul(10),
            referral_fees: Default::default(),
            route_costs: IntearDexRouteCosts {
                without_oracle_request: UNIT.saturating_mul(20),
                with_oracle_request: UNIT.saturating_mul(30),
                gas_price: NearToken::from_yoctonear(1),
                prices,
            },
        };
        let fee_discount = |last_look: &LastLook, asks_oracle| {
            last_look
                .fee_discount(|fee_discount| Some(swap_out_with(fee_discount, asks_oracle)))
                .map(FeeDiscountFraction::get)
        };
        let with_prices = costing(Some(CostPrices {
            near_price_raw: 1.into(),
            quoted_token_price_raw: 10u64.pow(9).into(),
        }));
        // 1,050 - 10 of the best other quote, and 20 for the route and 5 for the swap's gas
        assert_eq!(fee_discount(&with_prices, false), Some(650_000));
        assert_eq!(fee_discount(&with_prices, true), Some(750_000));
        // Amounts alone without prices
        assert_eq!(fee_discount(&costing(None), true), Some(500_000));
    }

    #[test]
    fn tear_fee_discount_comes_off_what_last_look_leaves() {
        let combined = |last_look: Option<u32>, tear: Option<u32>| {
            with_tear_fee_discount(
                last_look.map(FeeDiscountFraction::new),
                tear.map(FeeDiscountFraction::new),
            )
            .map(FeeDiscountFraction::get)
        };
        assert_eq!(combined(Some(500_000), Some(100_000)), Some(550_000));
        assert_eq!(combined(Some(990_000), Some(750_000)), Some(997_500));
        assert_eq!(combined(Some(500_000), None), Some(500_000));
        assert_eq!(combined(None, Some(250_000)), Some(250_000));
        assert_eq!(combined(None, None), None);
    }
}
