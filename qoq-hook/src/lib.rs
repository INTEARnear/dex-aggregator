mod oracle_prices;

use std::sync::{Arc, RwLock};

use amp_dex_types::{FeeDiscount, SwapArgs};
use bigdecimal::Zero;
use intear_dex_common::{AssetId, SwapRequestAmount, TokenAmount};
use near_min_api::types::{AccountId, BlockHeight};
use near_sdk::json_types::{Base64VecU8, U128};
use pathfinder::{
    MaxHops, QuoteAmount,
    qoq::{self, Pools},
};
use swap_router_types::{
    export_hook,
    fee_discount::{FEE_DISCOUNT_VALIDITY_BLOCKS, FeeDiscounter, tear_fee_discount},
    hook::{Hook, HookInput, HookQuote, IntearDexTrade, OracleRequest, Prices},
    indexed::{AccountState, Watch},
    last_look::{LastLook, SwapEstimate, with_tear_fee_discount},
    tear_holdings::TearHoldings,
    types::Amount,
};

use crate::oracle_prices::OraclePricesFeed;

const QOQ_DEX_ID: &str = "slimedragon.near/amp";

struct Qoq {
    fee_discounter: FeeDiscounter,
    tear_holdings: TearHoldings,
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
            fee_discounter: FeeDiscounter::from_env()?,
            tear_holdings: TearHoldings::start()?,
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
        let quote = self.quote(
            &input.trade,
            input.block_height,
            LastLook::of(&input).as_ref(),
            |request| qoq::find_path_in(&pools, &oracle_prices, request),
        );
        Ok(quote.into_iter().collect())
    }
}

impl Qoq {
    fn quote(
        &self,
        trade: &IntearDexTrade,
        block_height: BlockHeight,
        last_look: Option<&LastLook>,
        find_path: impl Fn(&qoq::Request) -> Result<qoq::Quote, anyhow::Error>,
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
        let tear_fee_discount = trade
            .trader_account_id
            .as_ref()
            .and_then(|trader_id| self.tear_holdings.of(trader_id))
            .and_then(tear_fee_discount);
        let request = |fee_discount, discount_counted_from| qoq::Request {
            token_in: trade.asset_in.clone(),
            token_out: trade.asset_out.clone(),
            amount: match trade.amount {
                Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage: trade.slippage.clone(),
            near_price_raw: near_price_raw.clone(),
            quoted_token_price_raw: quoted_token_price_raw.clone(),
            fee_discount,
            discount_counted_from,
        };
        let last_look_fee_discount = last_look.and_then(|last_look| {
            last_look.fee_discount(|fee_discount| {
                find_path(&request(fee_discount, None))
                    .ok()
                    .map(|quote| SwapEstimate {
                        estimated_amount: quote.estimated_amount,
                        gas: quote.swap_gas,
                        asks_oracle: true,
                    })
            })
        });
        // Only what the TEAR holder's discount adds is shown as the discount
        let fee_discount = with_tear_fee_discount(last_look_fee_discount, tear_fee_discount);
        let quote = find_path(&request(fee_discount, last_look_fee_discount)).ok()?;
        let signed_fee_discount = fee_discount.map(|fee_discount| {
            self.fee_discounter.sign(
                &QOQ_DEX_ID.parse().unwrap(),
                FeeDiscount {
                    route: quote.routes.clone(),
                    asset_in: trade.asset_in.clone(),
                    asset_out: trade.asset_out.clone(),
                    trader_id: trade.trader_account_id.clone(),
                    amount: match trade.amount {
                        Amount::AmountIn(amount_in) => {
                            SwapRequestAmount::ExactIn(TokenAmount::new(amount_in))
                        }
                        Amount::AmountOut(amount_out) => {
                            SwapRequestAmount::ExactOut(TokenAmount::new(amount_out))
                        }
                    },
                    fee_discount,
                    expiration_block_height: block_height + FEE_DISCOUNT_VALIDITY_BLOCKS,
                },
            )
        });
        Some(HookQuote {
            dex_id: QOQ_DEX_ID.parse().unwrap(),
            message: Base64VecU8(
                borsh::to_vec(&SwapArgs {
                    routes: quote.routes,
                    fee_discount: signed_fee_discount,
                })
                .unwrap(),
            ),
            estimated_amount: U128(quote.estimated_amount),
            worst_case_amount: U128(quote.worst_case_amount),
            gas: quote.swap_gas,
            discount: tear_fee_discount.map(|_| U128(quote.discount)),
            oracle_request: Some(OracleRequest {
                oracle_key: quote.oracle_key,
                request: Base64VecU8(borsh::to_vec(&quote.prices_request).unwrap()),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use amp_dex_types::{
        FULL_ROUTE_FRACTION, FeeDiscountFraction, PoolId, PricesRequest, SignedFeeDiscount,
        SwapRoute,
    };
    use near_min_api::types::NearToken;
    use near_min_api::types::near_crypto::{KeyType, SecretKey, Signature};
    use sha2::{Digest, Sha256};
    use swap_router_types::{
        fee_discount::fee_discount_signature_payload,
        hook::IntearDexRouteCosts,
        types::{CostPrices, Gas},
    };
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;

    const ONE_TEAR: u128 = 10u128.pow(24);

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const USDC_TOKEN_ID: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";

    fn usdc() -> AssetId {
        AssetId::Nep141(USDC_TOKEN_ID.parse().unwrap())
    }

    /// Without pools or oracle prices, which [`Qoq::quote`] gets the route of from `find_path`,
    /// with the traders whose TEAR holdings it fetches
    fn qoq(
        tear_holdings: impl IntoIterator<Item = (AccountId, u128)>,
    ) -> (Qoq, UnboundedReceiver<AccountId>) {
        let (tear_holdings, traders_to_fetch) = TearHoldings::known(tear_holdings);
        let qoq = Qoq {
            fee_discounter: FeeDiscounter {
                secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
            },
            tear_holdings,
            pools: RwLock::default(),
            oracle_prices: OraclePricesFeed::without_prices(),
            prices: RwLock::new(Prices::from([
                (AssetId::Near, "0.000000000000000000000002".parse().unwrap()),
                (usdc(), "0.000001".parse().unwrap()),
            ])),
        };
        (qoq, traders_to_fetch)
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

    /// Through pool 1, at a tenth of USDC per NEAR times `usdc_per_near_tenths`, and 5 more output
    /// with a fee discount
    fn find_path_at(
        usdc_per_near_tenths: u128,
    ) -> impl Fn(&qoq::Request) -> Result<qoq::Quote, anyhow::Error> {
        move |request| {
            let near_to_usdc = |near: u128| near * usdc_per_near_tenths / 10u128.pow(19);
            let usdc_to_near = |usdc: u128| usdc * 10u128.pow(19) / usdc_per_near_tenths;
            let estimated_amount = match (&request.token_in, &request.amount) {
                (AssetId::Near, QuoteAmount::ExactIn(near_in)) => near_to_usdc(*near_in),
                (AssetId::Near, QuoteAmount::ExactOut(usdc_out)) => usdc_to_near(*usdc_out),
                (_, QuoteAmount::ExactIn(usdc_in)) => usdc_to_near(*usdc_in),
                (_, QuoteAmount::ExactOut(near_out)) => near_to_usdc(*near_out),
            };
            let discount = if request.fee_discount.is_some() { 5 } else { 0 };
            let estimated_amount = estimated_amount + discount;
            Ok(qoq::Quote {
                routes: vec![SwapRoute {
                    fraction: FULL_ROUTE_FRACTION,
                    pool_ids: vec![PoolId::new(1)],
                }],
                estimated_amount,
                worst_case_amount: estimated_amount * 99 / 100,
                discount,
                oracle_key: ORACLE_KEY.parse().unwrap(),
                prices_request: PricesRequest::of_pools([(&request.token_in, &request.token_out)]),
                swap_gas: Gas::from_ggas(24_800, 24_800),
            })
        }
    }

    #[test]
    fn quotes_for_any_trader() {
        let (qoq, _traders_to_fetch) = qoq([]);
        for trader_id in [Some("slimedragon.near"), Some("trader.near"), None] {
            assert!(
                qoq.quote(&trade(trader_id), 1_000_000, None, find_path_at(24))
                    .is_some()
            );
        }
    }

    #[test]
    fn swaps_after_asking_the_oracle_for_the_prices_of_the_route() {
        let (qoq, _) = qoq([]);
        let quote = qoq
            .quote(&trade(None), 1_000_000, None, find_path_at(24))
            .unwrap();
        assert_eq!(quote.dex_id, QOQ_DEX_ID.parse().unwrap());
        assert_eq!(quote.estimated_amount, U128(2_400_000));
        assert_eq!(quote.worst_case_amount, U128(2_376_000));
        assert_eq!(quote.discount, None);
        let swap_args: SwapArgs = borsh::from_slice(&quote.message.0).unwrap();
        assert_eq!(swap_args.routes[0].pool_ids, vec![PoolId::new(1)]);
        assert!(swap_args.fee_discount.is_none());
        let oracle_request = quote.oracle_request.unwrap();
        assert_eq!(oracle_request.oracle_key, ORACLE_KEY.parse().unwrap());
        let prices_request: PricesRequest = borsh::from_slice(&oracle_request.request.0).unwrap();
        assert_eq!(prices_request.asset_ids, vec![AssetId::Near, usdc()]);
    }

    #[test]
    fn signs_fee_discount_for_tear_holders() {
        let (qoq, _) = qoq([("trader.near".parse().unwrap(), 1_000 * ONE_TEAR)]);
        let quote = qoq
            .quote(
                &trade(Some("trader.near")),
                1_000_000,
                None,
                find_path_at(24),
            )
            .unwrap();
        assert_eq!(quote.estimated_amount, U128(2_400_005));
        assert_eq!(quote.discount, Some(U128(5)));
        let SwapArgs {
            routes,
            fee_discount:
                Some(SignedFeeDiscount {
                    discount,
                    signature,
                }),
        } = borsh::from_slice(&quote.message.0).unwrap()
        else {
            panic!("No fee discount");
        };
        assert_eq!(discount.route, routes);
        assert_eq!(discount.asset_in, AssetId::Near);
        assert_eq!(discount.asset_out, usdc());
        assert_eq!(discount.trader_id, Some("trader.near".parse().unwrap()));
        assert_eq!(
            discount.amount,
            SwapRequestAmount::ExactIn(TokenAmount::new(10u128.pow(24)))
        );
        assert_eq!(discount.fee_discount.get(), 250_000);
        assert_eq!(discount.expiration_block_height, 1_001_000);
        let payload = fee_discount_signature_payload(&QOQ_DEX_ID.parse().unwrap(), &discount);
        assert!(
            Signature::from_parts(KeyType::ED25519, &signature)
                .unwrap()
                .verify(
                    &Sha256::digest(borsh::to_vec(&payload).unwrap()),
                    &qoq.fee_discounter.secret_key.public_key(),
                )
        );
    }

    /// Through pool 1, 2,400,000 out and a unit more per percent of fee discount
    fn find_path_gaining_a_unit_per_discount_percent(
        request: &qoq::Request,
    ) -> Result<qoq::Quote, anyhow::Error> {
        let estimated_amount_with = |fee_discount: Option<FeeDiscountFraction>| {
            2_400_000 + u128::from(fee_discount.map_or(0, FeeDiscountFraction::get) / 10_000)
        };
        let estimated_amount = estimated_amount_with(request.fee_discount);
        let discount = match request.fee_discount {
            Some(_) => estimated_amount - estimated_amount_with(request.discount_counted_from),
            None => 0,
        };
        Ok(qoq::Quote {
            routes: vec![SwapRoute {
                fraction: FULL_ROUTE_FRACTION,
                pool_ids: vec![PoolId::new(1)],
            }],
            estimated_amount,
            worst_case_amount: estimated_amount * 99 / 100,
            discount,
            oracle_key: ORACLE_KEY.parse().unwrap(),
            prices_request: PricesRequest::of_pools([(&request.token_in, &request.token_out)]),
            swap_gas: Gas::from_ggas(24_800, 24_800),
        })
    }

    /// Of the best other quote of `best_other_amount`, without prices to weigh costs with
    fn last_look_by_amounts_alone(best_other_amount: Amount) -> LastLook {
        LastLook {
            best_other_amount,
            best_other_cost: NearToken::from_millinear(1),
            referral_fees: Default::default(),
            route_costs: IntearDexRouteCosts {
                without_oracle_request: NearToken::from_millinear(1),
                with_oracle_request: NearToken::from_millinear(2),
                gas_price: NearToken::from_yoctonear(100_000_000),
                prices: None,
            },
        }
    }

    fn signed_fee_discount_of(quote: &HookQuote) -> Option<u32> {
        let swap_args: SwapArgs = borsh::from_slice(&quote.message.0).unwrap();
        swap_args
            .fee_discount
            .map(|signed| signed.discount.fee_discount.get())
    }

    #[test]
    fn last_look_matches_the_best_other_quote_and_tear_fee_discount_goes_on_top() {
        let last_look = last_look_by_amounts_alone(Amount::AmountOut(2_400_050));
        let (qoq, _) = qoq([("trader.near".parse().unwrap(), 10 * ONE_TEAR)]);
        let quote_for = |trader_id| {
            qoq.quote(
                &trade(trader_id),
                1_000_000,
                Some(&last_look),
                find_path_gaining_a_unit_per_discount_percent,
            )
            .unwrap()
        };

        let without_tear = quote_for(None);
        assert_eq!(signed_fee_discount_of(&without_tear), Some(500_000));
        assert_eq!(without_tear.estimated_amount, U128(2_400_050));
        assert_eq!(without_tear.discount, None);

        // 50% of the fees off to match, and 10% of the rest for the TEAR holder
        let with_tear = quote_for(Some("trader.near"));
        assert_eq!(signed_fee_discount_of(&with_tear), Some(550_000));
        assert_eq!(with_tear.estimated_amount, U128(2_400_055));
        assert_eq!(with_tear.discount, Some(U128(5)));
    }

    #[test]
    fn last_look_counts_the_cost_of_the_oracle_request() {
        // The oracle request costs 30 USDC units, the gas costs nothing
        let last_look = LastLook {
            route_costs: IntearDexRouteCosts {
                without_oracle_request: NearToken::from_near(0),
                with_oracle_request: NearToken::from_near(30),
                gas_price: NearToken::from_yoctonear(0),
                prices: Some(CostPrices {
                    near_price_raw: 1.into(),
                    quoted_token_price_raw: 10u128.pow(24).into(),
                }),
            },
            best_other_cost: NearToken::from_near(0),
            ..last_look_by_amounts_alone(Amount::AmountOut(2_400_050))
        };
        let (qoq, _) = qoq([("trader.near".parse().unwrap(), 10 * ONE_TEAR)]);
        let quote = qoq
            .quote(
                &trade(Some("trader.near")),
                1_000_000,
                Some(&last_look),
                find_path_gaining_a_unit_per_discount_percent,
            )
            .unwrap();
        // 80% of the fees off to match, and 10% of the rest for the TEAR holder
        assert_eq!(signed_fee_discount_of(&quote), Some(820_000));
        assert_eq!(quote.estimated_amount, U128(2_400_082));
        assert_eq!(quote.discount, Some(U128(2)));
    }
}
