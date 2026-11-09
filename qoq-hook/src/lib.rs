mod oracle_prices;

use std::sync::{Arc, RwLock};

use amp_dex_types::SwapArgs;
use bigdecimal::Zero;
use intear_dex_common::AssetId;
use near_min_api::types::AccountId;
use near_sdk::json_types::{Base64VecU8, U128};
use pathfinder::{
    MaxHops, QuoteAmount,
    qoq::{self, Pools},
};
use swap_router_types::{
    export_hook,
    hook::{Hook, HookInput, HookQuote, IntearDexTrade, OracleRequest, Prices},
    indexed::{AccountState, Watch},
    types::Amount,
};

use crate::oracle_prices::OraclePricesFeed;

const QOQ_DEX_ID: &str = "slimedragon.near/amp";

struct Qoq {
    pools: RwLock<Option<Arc<Pools>>>,
    oracle_prices: OraclePricesFeed,
    prices: RwLock<Prices>,
}

export_hook!(Qoq);

impl Hook for Qoq {
    fn init() -> Result<Self, String> {
        let oracle_url = std::env::var("QOQ_ORACLE_URL")
            .map_err(|err| format!("QOQ_ORACLE_URL, the oracle's HTTP API: {err}"))?;
        Ok(Self {
            pools: RwLock::default(),
            oracle_prices: OraclePricesFeed::start(oracle_url)?,
            prices: RwLock::default(),
        })
    }

    fn watches(&self) -> Vec<Watch> {
        vec![qoq::watch()]
    }

    fn on_indexed_state(&self, states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
        let [(_, state)] = states.as_slice() else {
            return Err(format!("Qoq watches one account, got {}", states.len()));
        };
        let height = state.block.height;
        let pools = qoq::build_indexed_pools(state)
            .map_err(|err| format!("Failed to build Qoq pools at block {height}: {err:#}"))?;
        self.oracle_prices.set_pools(pools.assets());
        *self.pools.write().unwrap() = Some(Arc::new(pools));
        Ok(())
    }

    fn on_prices(&self, prices: Prices) {
        self.prices.write().unwrap().extend(prices);
    }

    fn run(&self, input: HookInput<'static>) -> Result<Vec<HookQuote>, String> {
        let Some(pools) = self.pools.read().unwrap().clone() else {
            return Ok(vec![]);
        };
        if pools.check_fresh().is_err() {
            return Ok(vec![]);
        }
        let Some(oracle_prices) = self.oracle_prices.fresh() else {
            return Ok(vec![]);
        };
        let quote = self.quote(&input.trade, |request| {
            qoq::find_path_in(&pools, &oracle_prices, request)
        });
        Ok(quote.into_iter().collect())
    }
}

impl Qoq {
    fn quote(
        &self,
        trade: &IntearDexTrade,
        find_path: impl FnOnce(&qoq::Request) -> Result<qoq::Quote, anyhow::Error>,
    ) -> Option<HookQuote> {
        let (near_price_raw, quoted_token_price_raw) = {
            let prices = self.prices.read().unwrap();
            let price = |asset_id: &AssetId| {
                prices
                    .get(asset_id)
                    .filter(|price| !price.is_zero())
                    .cloned()
            };
            let quoted_token = match trade.amount {
                Amount::AmountIn(_) => &trade.asset_out,
                Amount::AmountOut(_) => &trade.asset_in,
            };
            (price(&AssetId::Near)?, price(quoted_token)?)
        };
        let quote = find_path(&qoq::Request {
            token_in: trade.asset_in.clone(),
            token_out: trade.asset_out.clone(),
            amount: match trade.amount {
                Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage: trade.slippage.clone(),
            near_price_raw,
            quoted_token_price_raw,
        })
        .ok()?;
        Some(HookQuote {
            dex_id: QOQ_DEX_ID.parse().unwrap(),
            message: Base64VecU8(
                borsh::to_vec(&SwapArgs {
                    routes: quote.routes,
                    fee_discount: None,
                })
                .unwrap(),
            ),
            estimated_amount: U128(quote.estimated_amount),
            worst_case_amount: U128(quote.worst_case_amount),
            gas: quote.swap_gas,
            discount: None,
            oracle_request: Some(OracleRequest {
                oracle_key: quote.oracle_key,
                request: Base64VecU8(borsh::to_vec(&quote.prices_request).unwrap()),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use amp_dex_types::{FULL_ROUTE_FRACTION, PoolId, PricesRequest, SwapRoute};
    use swap_router_types::types::Gas;

    use super::*;

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const USDC_TOKEN_ID: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";

    fn usdc() -> AssetId {
        AssetId::Nep141(USDC_TOKEN_ID.parse().unwrap())
    }

    /// Without pools or oracle prices, which [`Qoq::quote`] gets the route of from `find_path`
    fn qoq() -> Qoq {
        Qoq {
            pools: RwLock::default(),
            oracle_prices: OraclePricesFeed::without_prices(),
            prices: RwLock::new(Prices::from([
                (AssetId::Near, "0.000000000000000000000002".parse().unwrap()),
                (usdc(), "0.000001".parse().unwrap()),
            ])),
        }
    }

    fn trade(trader_id: Option<&str>) -> IntearDexTrade {
        IntearDexTrade {
            asset_in: AssetId::Near,
            asset_out: usdc(),
            amount: Amount::AmountIn(10u128.pow(24)),
            slippage: "0.01".parse().unwrap(),
            trader_account_id: trader_id.map(|trader_id| trader_id.parse().unwrap()),
        }
    }

    /// Through pool 1, at a tenth of USDC per NEAR times `usdc_per_near_tenths`
    fn find_path_at(
        usdc_per_near_tenths: u128,
    ) -> impl FnOnce(&qoq::Request) -> Result<qoq::Quote, anyhow::Error> {
        move |request| {
            let near_to_usdc = |near: u128| near * usdc_per_near_tenths / 10u128.pow(19);
            let usdc_to_near = |usdc: u128| usdc * 10u128.pow(19) / usdc_per_near_tenths;
            let estimated_amount = match (&request.token_in, &request.amount) {
                (AssetId::Near, QuoteAmount::ExactIn(near_in)) => near_to_usdc(*near_in),
                (AssetId::Near, QuoteAmount::ExactOut(usdc_out)) => usdc_to_near(*usdc_out),
                (_, QuoteAmount::ExactIn(usdc_in)) => usdc_to_near(*usdc_in),
                (_, QuoteAmount::ExactOut(near_out)) => near_to_usdc(*near_out),
            };
            Ok(qoq::Quote {
                routes: vec![SwapRoute {
                    fraction: FULL_ROUTE_FRACTION,
                    pool_ids: vec![PoolId::new(1)],
                }],
                estimated_amount,
                worst_case_amount: estimated_amount * 99 / 100,
                oracle_key: ORACLE_KEY.parse().unwrap(),
                prices_request: PricesRequest::of_pools([(&request.token_in, &request.token_out)]),
                swap_gas: Gas::from_ggas(24_800, 24_800),
            })
        }
    }

    #[test]
    fn quotes_for_any_trader() {
        let qoq = qoq();
        for trader_id in [Some("slimedragon.near"), Some("trader.near"), None] {
            assert!(qoq.quote(&trade(trader_id), find_path_at(24)).is_some());
        }
    }

    #[test]
    fn swaps_after_asking_the_oracle_for_the_prices_of_the_route() {
        let quote = qoq().quote(&trade(None), find_path_at(24)).unwrap();
        assert_eq!(quote.dex_id, QOQ_DEX_ID.parse().unwrap());
        assert_eq!(quote.estimated_amount, U128(2_400_000));
        assert_eq!(quote.worst_case_amount, U128(2_376_000));
        let swap_args: SwapArgs = borsh::from_slice(&quote.message.0).unwrap();
        assert_eq!(swap_args.routes[0].pool_ids, vec![PoolId::new(1)]);
        assert!(swap_args.fee_discount.is_none());
        let oracle_request = quote.oracle_request.unwrap();
        assert_eq!(oracle_request.oracle_key, ORACLE_KEY.parse().unwrap());
        let prices_request: PricesRequest = borsh::from_slice(&oracle_request.request.0).unwrap();
        assert_eq!(prices_request.asset_ids, vec![AssetId::Near, usdc()]);
    }
}
