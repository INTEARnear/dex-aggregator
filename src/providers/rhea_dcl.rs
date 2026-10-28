use std::{future::Future, pin::Pin};

use intear_dex_types::FULL_FEE_FRACTION;
use near_min_api::types::AccountId;
use pathfinder::{
    dcl::{self, RouteApiResponse},
    MaxHops, QuoteAmount,
};
use tracing::info;

use crate::{
    calls::{CallBlueprint, RheaDclSwap, RheaOutput},
    route::{RouteBlueprint, RouteStep},
    shared_utils::{
        base_token, convert_to_nep141, convert_to_nep141_with_fees, deposit_storage_if_needed,
        get_slippage, payable_referral_fees, price_raw, referrer_fees_of_total,
        register_input_in_wallet, total_fee_amount, BaseTokenId, Mainnet, NetworkView, ReferrerFee,
        DEFAULT_REFERRER_ID,
    },
    types::TokenId,
    Amount, DexId, Provider, SwapRequest,
};

pub struct RheaDclProvider;

pub const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";

trait RheaDclQuotes: Send + Sync {
    fn find_path(
        &self,
        request: dcl::Request,
    ) -> impl Future<Output = Option<RouteApiResponse>> + Send;
}

struct MainnetRheaDclQuotes;

impl RheaDclQuotes for MainnetRheaDclQuotes {
    async fn find_path(&self, request: dcl::Request) -> Option<RouteApiResponse> {
        dcl::find_path(request)
            .await
            .inspect_err(|e| info!("No Rhea DCL route: {e:#}"))
            .ok()
    }
}

impl Provider for RheaDclProvider {
    fn dex_id(&self) -> DexId {
        DexId::RheaDcl
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<RouteBlueprint>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaDclQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaDclQuotes,
) -> Option<RouteBlueprint> {
    let slippage = get_slippage(
        network,
        request.slippage,
        &base_token(&request.token_in),
        &base_token(&request.token_out),
    );

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0);
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0);

    if token_in == token_out {
        return None;
    }

    // The fees come out of the input. Exact-in quotes the rest of it, exact-out adds the fees on
    // top of the estimated input once it's quoted.
    let referral_fees = payable_referral_fees(&request.fees);
    let exact_in_fees = match request.amount {
        Amount::AmountIn(amount_in) => referrer_fees_of_total(&referral_fees, amount_in),
        Amount::AmountOut(_) => Vec::new(),
    };
    let swap_referrer_id: AccountId = request
        .referrer_id
        .clone()
        .unwrap_or_else(|| DEFAULT_REFERRER_ID.parse().unwrap());

    let response = quotes
        .find_path(dcl::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount: match request.amount {
                Amount::AmountIn(amount_in) => {
                    QuoteAmount::ExactIn(amount_in - total_fee_amount(&exact_in_fees))
                }
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &BaseTokenId::Near)?,
            quoted_token_price_raw: price_raw(
                network,
                &match request.amount {
                    Amount::AmountIn(_) => base_token(&request.token_out),
                    Amount::AmountOut(_) => base_token(&request.token_in),
                },
            )?,
        })
        .await?;

    let (amount_in, pool_ids, swap, estimated_amount, worst_case_amount, swap_gas, fees) =
        match response {
            RouteApiResponse::ExactIn { route, .. } => (
                route.amount_in.0,
                route.pools.into_iter().map(|step| step.pool_id).collect(),
                RheaDclSwap::ExactIn {
                    min_amount_out: route.min_amount_out.0,
                },
                Amount::AmountOut(route.amount_out.0),
                Amount::AmountOut(route.min_amount_out.0),
                route.swap_gas,
                exact_in_fees,
            ),
            RouteApiResponse::ExactOut { route, .. } => {
                // Each referrer's share of the estimated input with all fees, not the worst case,
                // so high slippage doesn't raise the fees
                let rest = (FULL_FEE_FRACTION
                    - referral_fees
                        .0
                        .iter()
                        .map(|(_, fee_fraction)| fee_fraction)
                        .sum::<u32>()) as u128;
                let amount = route.amount_in.0;
                let fees = referral_fees
                    .0
                    .iter()
                    .map(|(referrer_id, fee_fraction)| ReferrerFee {
                        referrer_id: referrer_id.clone(),
                        amount: amount / rest * *fee_fraction as u128
                            + amount % rest * *fee_fraction as u128 / rest,
                    })
                    .filter(|fee| fee.amount > 0)
                    .collect::<Vec<_>>();
                let fee_amount = total_fee_amount(&fees);
                (
                    route.max_amount_in.0,
                    route.pools.into_iter().map(|step| step.pool_id).collect(),
                    RheaDclSwap::ExactOut {
                        amount_out: route.amount_out.0,
                    },
                    Amount::AmountIn(route.amount_in.0 + fee_amount),
                    Amount::AmountIn(route.max_amount_in.0 + fee_amount),
                    route.swap_gas,
                    fees,
                )
            }
        };

    let unwrapping_near = request.token_out == TokenId::Near;
    let swap = RouteStep::Call(CallBlueprint::InputNep141TransferCallRheaDclSwap {
        token_id: token_in,
        amount: amount_in,
        pool_ids,
        token_out: token_out.clone(),
        swap,
        swap_gas,
        output: if unwrapping_near {
            RheaOutput::Near {
                trader_account_id: request.trader_account_id.clone(),
            }
        } else {
            RheaOutput::Nep141
        },
        referral_id: swap_referrer_id.clone(),
    });

    let (input_to_nep141, fee_steps, input_nep141) = convert_to_nep141_with_fees(
        &request.token_in,
        request.trader_account_id.as_ref(),
        amount_in + total_fee_amount(&fees),
        &fees,
        &swap_referrer_id,
    );
    let token_output = if unwrapping_near {
        TokenId::Near
    } else {
        TokenId::Nep141(token_out)
    };
    let storage_deposit = match &request.trader_account_id {
        Some(trader_account_id) => vec![RouteStep::TraderStorageDepositOnRheaDclIfNeeded {
            trader_account_id: trader_account_id.clone(),
        }],
        None => vec![],
    };
    Some(RouteBlueprint {
        dex_id: DexId::RheaDcl,
        estimated_amount,
        worst_case_amount,
        steps: [
            storage_deposit,
            deposit_storage_if_needed(&token_output, request.trader_account_id.as_ref()),
            register_input_in_wallet(
                &request.token_in,
                input_nep141,
                request.trader_account_id.as_ref(),
            ),
            input_to_nep141,
            vec![swap],
        ]
        .concat(),
        fee_steps,
        token_output,
        discount: None,
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{NearToken, U128};

    use super::*;
    use crate::route::resolve;
    use crate::shared_utils::{TestNetworkView, WRAP_NEAR};
    use crate::types::{Gas, Slippage};
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;

    #[derive(Clone)]
    struct TestRheaDclQuotes {
        enabled: bool,
    }

    impl Default for TestRheaDclQuotes {
        fn default() -> Self {
            Self { enabled: true }
        }
    }

    impl TestRheaDclQuotes {
        fn none() -> Self {
            Self { enabled: false }
        }
    }

    fn step(pool_id: String, token_in: &AccountId, token_out: &AccountId) -> dcl::ApiPoolStep {
        dcl::ApiPoolStep {
            pool_id,
            token_in: token_in.clone(),
            token_out: token_out.clone(),
        }
    }

    const SWAP_GAS: Gas = Gas::from_ggas(20_000, 20_000);

    impl RheaDclQuotes for TestRheaDclQuotes {
        async fn find_path(&self, request: dcl::Request) -> Option<RouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let (token_in, token_out) = (&request.token_in, &request.token_out);
            let usdc: AccountId = "usdc".parse().unwrap();
            let pools = vec![
                step(format!("{token_in}|usdc|100"), token_in, &usdc),
                step(format!("{token_out}|usdc|2000"), &usdc, token_out),
            ];
            match request.amount {
                QuoteAmount::ExactIn(amount_in) => Some(RouteApiResponse::ExactIn {
                    route: dcl::ExactInRoute {
                        pools,
                        amount_in: U128(amount_in),
                        min_amount_out: U128(79),
                        amount_out: U128(80),
                        swap_gas: SWAP_GAS,
                    },
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: U128(amount_in),
                    amount_out: U128(80),
                }),
                QuoteAmount::ExactOut(_) => Some(RouteApiResponse::ExactOut {
                    route: dcl::ExactOutRoute {
                        pools,
                        amount_in: U128(100),
                        max_amount_in: U128(101),
                        amount_out: U128(80),
                        swap_gas: SWAP_GAS,
                    },
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: U128(100),
                    max_amount_in: U128(101),
                    amount_out: U128(80),
                }),
            }
        }
    }

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    fn ft() -> TokenId {
        TokenId::Nep141(account("ft"))
    }

    fn request(token_in: TokenId, token_out: TokenId, amount: Amount) -> SwapRequest {
        SwapRequest {
            token_in,
            token_out,
            amount,
            slippage: Slippage::Fixed {
                slippage: "0.01".parse().unwrap(),
            },
            dexes: None,
            trader_account_id: Some(trader()),
            signing_public_key: None,
            referrer_id: None,
            fees: Default::default(),
        }
    }

    async fn test_route(request: SwapRequest) -> Option<RouteBlueprint> {
        route(
            request,
            &TestNetworkView::default().with_test_prices(),
            &TestRheaDclQuotes::default(),
        )
        .await
    }

    fn swap(
        token_in: &str,
        token_out: &str,
        amount_in: u128,
        swap: RheaDclSwap,
        output: RheaOutput,
    ) -> RouteStep {
        RouteStep::Call(CallBlueprint::InputNep141TransferCallRheaDclSwap {
            token_id: account(token_in),
            amount: amount_in,
            pool_ids: vec![
                format!("{token_in}|usdc|100"),
                format!("{token_out}|usdc|2000"),
            ],
            token_out: account(token_out),
            swap,
            swap_gas: SWAP_GAS,
            output,
            referral_id: account(DEFAULT_REFERRER_ID),
        })
    }

    fn dcl_storage() -> RouteStep {
        RouteStep::TraderStorageDepositOnRheaDclIfNeeded {
            trader_account_id: trader(),
        }
    }

    fn trader_storage(token_id: &str) -> RouteStep {
        RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id: account(token_id),
            trader_account_id: trader(),
        }
    }

    fn wrap(amount: u128) -> RouteStep {
        RouteStep::Call(CallBlueprint::WrapNear {
            amount: NearToken::from_yoctonear(amount),
        })
    }

    fn fee_receiver_storage(account_id: &str) -> RouteStep {
        RouteStep::StorageDepositOnNep141IfNeeded {
            token_id: account(WRAP_NEAR),
            account_id: account(account_id),
        }
    }

    fn fee_transfer(receiver_id: &str, amount: u128) -> RouteStep {
        RouteStep::Call(CallBlueprint::InputNep141FeeTransfer {
            token_id: account(WRAP_NEAR),
            receiver_id: account(receiver_id),
            amount,
            swap_referrer_id: account(DEFAULT_REFERRER_ID),
        })
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Near, ft(), Amount::AmountIn(100))).await,
            Some(RouteBlueprint {
                dex_id: DexId::RheaDcl,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                token_output: ft(),
                discount: None,
                steps: vec![
                    dcl_storage(),
                    trader_storage("ft"),
                    trader_storage(WRAP_NEAR),
                    wrap(100),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaDclSwap::ExactIn { min_amount_out: 79 },
                        RheaOutput::Nep141
                    ),
                ],
                fee_steps: vec![],
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        assert_eq!(
            test_route(request(ft(), TokenId::Near, Amount::AmountIn(100))).await,
            Some(RouteBlueprint {
                dex_id: DexId::RheaDcl,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                token_output: TokenId::Near,
                discount: None,
                steps: vec![
                    dcl_storage(),
                    swap(
                        "ft",
                        WRAP_NEAR,
                        100,
                        RheaDclSwap::ExactIn { min_amount_out: 79 },
                        RheaOutput::Near {
                            trader_account_id: Some(trader())
                        }
                    ),
                ],
                fee_steps: vec![],
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_ft() {
        let route = test_route(request(TokenId::Near, ft(), Amount::AmountOut(80)))
            .await
            .unwrap();
        assert_eq!(
            route,
            RouteBlueprint {
                dex_id: DexId::RheaDcl,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                token_output: ft(),
                discount: None,
                steps: vec![
                    dcl_storage(),
                    trader_storage("ft"),
                    trader_storage(WRAP_NEAR),
                    wrap(101),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        101,
                        RheaDclSwap::ExactOut { amount_out: 80 },
                        RheaOutput::Nep141
                    ),
                ],
                fee_steps: vec![],
            }
        );

        // Registered on DCL and the tokens, so only wrapping and the swap are left
        let network = TestNetworkView::default()
            .with_storage(
                RHEA_DCL_CONTRACT_ID,
                "trader.near",
                NearToken::from_near(1),
                NearToken::from_near(1),
            )
            .with_storage(
                "ft",
                "trader.near",
                NearToken::from_near(1),
                NearToken::from_near(1),
            )
            .with_storage(
                WRAP_NEAR,
                "trader.near",
                NearToken::from_near(1),
                NearToken::from_near(1),
            );
        let calls = resolve(&network, route).await.unwrap().calls;
        assert!(
            matches!(
                calls.as_slice(),
                [
                    CallBlueprint::WrapNear { .. },
                    CallBlueprint::InputNep141TransferCallRheaDclSwap { .. }
                ]
            ),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            test_route(request(
                TokenId::Near,
                TokenId::Nep141(account(WRAP_NEAR)),
                Amount::AmountIn(100)
            ))
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            route(
                request(TokenId::Near, ft(), Amount::AmountIn(100)),
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage() {
        let route = test_route(SwapRequest {
            trader_account_id: None,
            ..request(TokenId::Near, ft(), Amount::AmountIn(100))
        })
        .await
        .unwrap();
        assert_eq!(
            route.steps,
            vec![
                wrap(100),
                swap(
                    WRAP_NEAR,
                    "ft",
                    100,
                    RheaDclSwap::ExactIn { min_amount_out: 79 },
                    RheaOutput::Nep141
                ),
            ]
        );
    }

    fn referred_request(amount: Amount, fees: &str) -> SwapRequest {
        SwapRequest {
            trader_account_id: None,
            fees: fees.parse().unwrap(),
            ..request(TokenId::Near, ft(), amount)
        }
    }

    #[tokio::test]
    async fn amount_out_referrer_fee_is_on_top_of_estimated_input() {
        let route = test_route(referred_request(
            Amount::AmountOut(80),
            "{\"ref.near\":0.05}",
        ))
        .await
        .unwrap();
        // 5% of the estimated input with the fee: 100 * 5% / 95%, 80% of it to the referrer and
        // 20% to the protocol, each rounded down
        assert_eq!(route.estimated_amount, Amount::AmountIn(105));
        assert_eq!(route.worst_case_amount, Amount::AmountIn(106));
        assert_eq!(
            route.steps,
            vec![
                wrap(106),
                swap(
                    WRAP_NEAR,
                    "ft",
                    101,
                    RheaDclSwap::ExactOut { amount_out: 80 },
                    RheaOutput::Nep141
                ),
            ]
        );
        // After the swap
        assert_eq!(
            route.fee_steps,
            vec![
                fee_receiver_storage("ref.near"),
                fee_receiver_storage(AGGREGATOR_FEE_RECEIVER_ID.as_str()),
                fee_transfer("ref.near", 4),
                fee_transfer(AGGREGATOR_FEE_RECEIVER_ID.as_str(), 1),
            ]
        );
    }

    #[tokio::test]
    async fn amount_in_referrer_fee_comes_out_of_the_quoted_input() {
        let route = test_route(referred_request(
            Amount::AmountIn(1000),
            "{\"ref.near\":0.01}",
        ))
        .await
        .unwrap();
        assert_eq!(
            route.steps,
            vec![
                wrap(1000),
                swap(
                    WRAP_NEAR,
                    "ft",
                    990,
                    RheaDclSwap::ExactIn { min_amount_out: 79 },
                    RheaOutput::Nep141
                ),
            ]
        );
        // After the swap, 1% of 1000, 80% to the referrer and 20% to the protocol
        assert_eq!(
            route.fee_steps[2..],
            [
                fee_transfer("ref.near", 8),
                fee_transfer(AGGREGATOR_FEE_RECEIVER_ID.as_str(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn amount_out_referrer_fees_share_the_estimated_input() {
        let route = test_route(referred_request(
            Amount::AmountOut(80),
            r#"{"ref.near":0.02,"ref2.near":0.03}"#,
        ))
        .await
        .unwrap();
        // Each is its share of the estimated input with all fees: 100 * fee / 95% rounded down,
        // 80% of each fee to its referrer and the protocol's 20% of both together
        assert_eq!(route.estimated_amount, Amount::AmountIn(104));
        assert_eq!(route.worst_case_amount, Amount::AmountIn(105));
        assert_eq!(route.steps[0], wrap(105));
        assert_eq!(
            route.fee_steps[3..],
            [
                fee_transfer("ref.near", 1),
                fee_transfer("ref2.near", 2),
                fee_transfer(AGGREGATOR_FEE_RECEIVER_ID.as_str(), 1),
            ]
        );
    }
}
