mod fee_discount;
mod tear_holdings;

use std::sync::{Arc, RwLock};

use bigdecimal::Zero;
use intear_dex_types::{AssetId, SwapRequestAmount};
use near_min_api::types::{AccountId, BlockHeight};
use near_sdk::json_types::{Base64VecU8, U128};
use pathfinder::{
    MaxHops, QuoteAmount,
    plach::{self, Pools, SplitRouteApiResponse},
};
use swap_router_types::{
    export_hook,
    hook::{Hook, HookInput, HookQuote, IntearDexTrade, Prices},
    indexed::{AccountState, Watch},
    types::Amount,
};
use xyk_dex_types::{FeeDiscount, SwapArgs, SwapRoute};

use crate::{
    fee_discount::{FEE_DISCOUNT_VALIDITY_BLOCKS, FeeDiscounter, tear_fee_discount},
    tear_holdings::TearHoldings,
};

const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";

struct Plach {
    fee_discounter: FeeDiscounter,
    tear_holdings: TearHoldings,
    pools: RwLock<Option<Arc<Pools>>>,
    prices: RwLock<Prices>,
}

export_hook!(Plach);

impl Hook for Plach {
    fn init() -> Result<Self, String> {
        Ok(Self {
            fee_discounter: FeeDiscounter::from_env()?,
            tear_holdings: TearHoldings::start()?,
            pools: RwLock::default(),
            prices: RwLock::default(),
        })
    }

    fn watches(&self) -> Vec<Watch> {
        vec![plach::watch()]
    }

    fn on_indexed_state(&self, states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
        let [(_, state)] = states.as_slice() else {
            return Err(format!("Plach watches one account, got {}", states.len()));
        };
        let height = state.block.height;
        let pools = plach::build_indexed_pools(state)
            .map_err(|err| format!("Failed to build Plach pools at block {height}: {err:#}"))?;
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
        let quote = self.quote(&input.trade, input.block_height, |request| {
            plach::find_path_in(&pools, request)
        });
        Ok(quote.into_iter().collect())
    }
}

impl Plach {
    fn quote(
        &self,
        trade: &IntearDexTrade,
        block_height: BlockHeight,
        find_path: impl FnOnce(&plach::Request) -> Result<SplitRouteApiResponse, anyhow::Error>,
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
        let fee_discount = trade
            .trader_account_id
            .as_ref()
            .and_then(|trader_id| self.tear_holdings.of(trader_id))
            .and_then(tear_fee_discount);

        let response = find_path(&plach::Request {
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
            fee_discount: fee_discount.unwrap_or(0),
        })
        .ok()?;

        let swap_route = |split_fraction, pool_ids| SwapRoute {
            fraction: split_fraction,
            pool_ids,
        };
        // `dex_amount` is what the DEX swaps
        let (swap_routes, dex_amount, estimated_amount, worst_case_amount, discount, swap_gas) =
            match response {
                SplitRouteApiResponse::ExactIn {
                    routes,
                    amount_in,
                    amount_out,
                    discount,
                    swap_gas,
                    ..
                } => (
                    routes
                        .iter()
                        .map(|route| {
                            swap_route(
                                route.split_fraction,
                                route.pools.iter().map(|step| step.pool_id).collect(),
                            )
                        })
                        .collect::<Vec<_>>(),
                    SwapRequestAmount::ExactIn(U128(amount_in.0)),
                    amount_out.0,
                    routes.iter().map(|route| route.min_amount_out.0).sum(),
                    discount.0,
                    swap_gas,
                ),
                SplitRouteApiResponse::ExactOut {
                    routes,
                    amount_in,
                    max_amount_in,
                    amount_out,
                    discount,
                    swap_gas,
                    ..
                } => (
                    routes
                        .iter()
                        .map(|route| {
                            swap_route(
                                route.split_fraction,
                                route.pools.iter().map(|step| step.pool_id).collect(),
                            )
                        })
                        .collect(),
                    SwapRequestAmount::ExactOut(U128(amount_out.0)),
                    amount_in.0,
                    max_amount_in.0,
                    discount.0,
                    swap_gas,
                ),
            };
        if swap_routes.is_empty() {
            return None;
        }
        let signed_fee_discount = fee_discount.map(|fee_discount| {
            self.fee_discounter.sign(FeeDiscount {
                routes: swap_routes.clone(),
                asset_in: trade.asset_in.clone(),
                asset_out: trade.asset_out.clone(),
                trader_id: trade.trader_account_id.clone(),
                amount: dex_amount,
                fee_discount,
                expiration_block_height: block_height + FEE_DISCOUNT_VALIDITY_BLOCKS,
            })
        });
        Some(HookQuote {
            dex_id: PLACH_DEX_ID.parse().unwrap(),
            message: Base64VecU8(
                borsh::to_vec(&SwapArgs {
                    routes: swap_routes,
                    fee_discount: signed_fee_discount,
                })
                .unwrap(),
            ),
            estimated_amount: U128(estimated_amount),
            worst_case_amount: U128(worst_case_amount),
            gas: swap_gas,
            discount: fee_discount.map(|_| U128(discount)),
        })
    }
}

#[cfg(test)]
mod tests {
    use near_min_api::types::near_crypto::{KeyType, SecretKey, Signature};
    use sha2::{Digest, Sha256};
    use swap_router_types::types::Gas;
    use tokio::sync::mpsc::UnboundedReceiver;
    use xyk_dex_types::{FULL_SPLIT_FRACTION, SignedFeeDiscount};

    use super::*;
    use crate::fee_discount::fee_discount_signature_payload;

    const ONE_TEAR: u128 = 10u128.pow(24);

    /// With the traders whose holdings it fetches
    fn plach(
        tear_holdings: impl IntoIterator<Item = (AccountId, u128)>,
    ) -> (Plach, UnboundedReceiver<AccountId>) {
        let (tear_holdings, traders_to_fetch) = TearHoldings::known(tear_holdings);
        let plach = Plach {
            fee_discounter: FeeDiscounter {
                secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
            },
            tear_holdings,
            pools: RwLock::default(),
            prices: RwLock::new(Prices::from([
                (AssetId::Near, "0.000000000000000000000005".parse().unwrap()),
                (
                    AssetId::Nep141("ft".parse().unwrap()),
                    "0.000001".parse().unwrap(),
                ),
            ])),
        };
        (plach, traders_to_fetch)
    }

    fn trade() -> IntearDexTrade {
        IntearDexTrade {
            asset_in: AssetId::Near,
            asset_out: AssetId::Nep141("ft".parse().unwrap()),
            amount: Amount::AmountIn(100),
            slippage: "0.01".parse().unwrap(),
            trader_account_id: Some("trader.near".parse().unwrap()),
        }
    }

    /// One pool, 5 more output with a fee discount
    fn find_path(request: &plach::Request) -> Result<SplitRouteApiResponse, anyhow::Error> {
        let QuoteAmount::ExactIn(amount_in) = request.amount else {
            panic!("Exact in");
        };
        let discount = if request.fee_discount == 0 { 0 } else { 5 };
        Ok(SplitRouteApiResponse::ExactIn {
            routes: vec![plach::ExactInRoute {
                split_fraction: FULL_SPLIT_FRACTION,
                pools: vec![plach::ExactInPoolStep {
                    pool_id: 1,
                    token_in: request.token_in.clone(),
                    token_out: request.token_out.clone(),
                    amount_in: near_min_api::types::U128(amount_in),
                    min_amount_out: near_min_api::types::U128(79 + discount),
                }],
                amount_in: near_min_api::types::U128(amount_in),
                min_amount_out: near_min_api::types::U128(79 + discount),
                amount_out: near_min_api::types::U128(80 + discount),
            }],
            contract_in: request.token_in.clone(),
            contract_out: request.token_out.clone(),
            amount_in: near_min_api::types::U128(amount_in),
            amount_out: near_min_api::types::U128(80 + discount),
            discount: near_min_api::types::U128(discount),
            swap_gas: Gas::from_ggas(25_000, 28_000),
        })
    }

    #[test]
    fn quotes_the_route_without_a_discount_for_too_little_tear() {
        let (plach, _) = plach([("trader.near".parse().unwrap(), 10 * ONE_TEAR - 1)]);
        let quote = plach.quote(&trade(), 1_000_000, find_path).unwrap();
        assert_eq!(quote.dex_id, PLACH_DEX_ID.parse().unwrap());
        assert_eq!(quote.estimated_amount, U128(80));
        assert_eq!(quote.worst_case_amount, U128(79));
        assert_eq!(quote.gas, Gas::from_ggas(25_000, 28_000));
        assert_eq!(quote.discount, None);
        let swap_args: SwapArgs = borsh::from_slice(&quote.message.0).unwrap();
        assert_eq!(swap_args.routes[0].pool_ids, vec![1]);
        assert!(swap_args.fee_discount.is_none());
    }

    #[test]
    fn no_discount_until_holdings_are_known() {
        let (plach, mut traders_to_fetch) = plach([]);
        let quote = plach.quote(&trade(), 1_000_000, find_path).unwrap();
        assert_eq!(quote.discount, None);
        assert_eq!(
            traders_to_fetch.try_recv(),
            Ok("trader.near".parse().unwrap())
        );
        // Asked for once while it's fetched
        plach.quote(&trade(), 1_000_000, find_path).unwrap();
        assert!(traders_to_fetch.try_recv().is_err());
    }

    #[test]
    fn signs_fee_discount() {
        let (plach, _) = plach([("trader.near".parse().unwrap(), 1_000 * ONE_TEAR)]);
        let quote = plach.quote(&trade(), 1_000_000, find_path).unwrap();
        assert_eq!(quote.discount, Some(U128(5)));
        assert_eq!(quote.estimated_amount, U128(85));
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
        assert_eq!(discount.routes, routes);
        assert_eq!(discount.asset_in, AssetId::Near);
        assert_eq!(discount.asset_out, AssetId::Nep141("ft".parse().unwrap()));
        assert_eq!(discount.trader_id, Some("trader.near".parse().unwrap()));
        assert_eq!(discount.amount, SwapRequestAmount::ExactIn(U128(100)));
        assert_eq!(discount.fee_discount, 250_000);
        assert_eq!(discount.expiration_block_height, 1_001_000);
        assert!(
            Signature::from_parts(KeyType::ED25519, &signature)
                .unwrap()
                .verify(
                    &Sha256::digest(
                        borsh::to_vec(&fee_discount_signature_payload(&discount)).unwrap()
                    ),
                    &plach.fee_discounter.secret_key.public_key(),
                )
        );
    }

    #[test]
    fn no_quote_without_prices() {
        let (plach, _) = plach([]);
        plach.prices.write().unwrap().clear();
        assert!(plach.quote(&trade(), 1_000_000, find_path).is_none());
    }
}
