use std::{future::Future, pin::Pin};

use near_min_api::types::AccountId;
use pathfinder::{
    rhea::{self, SplitRouteApiResponse},
    MaxHops,
};
use tracing::info;

use crate::{
    calls::{CallBlueprint, RheaOutput, RheaSwapAction},
    route::{RouteBlueprint, RouteStep},
    shared_utils::{
        base_token, convert_to_nep141, convert_to_nep141_with_fees, deposit_storage_if_needed,
        get_slippage, payable_referral_fees, price_raw, referrer_fees_of_total,
        register_input_in_wallet, total_fee_amount, BaseTokenId, Mainnet, NetworkView,
        DEFAULT_REFERRER_ID,
    },
    types::TokenId,
    Amount, DexId, Provider, SwapRequest,
};

pub struct RheaProvider;

pub const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";

trait RheaQuotes: Send + Sync {
    fn find_path(
        &self,
        request: rhea::Request,
    ) -> impl Future<Output = Option<SplitRouteApiResponse>> + Send;
}

struct MainnetRheaQuotes;

impl RheaQuotes for MainnetRheaQuotes {
    async fn find_path(&self, request: rhea::Request) -> Option<SplitRouteApiResponse> {
        rhea::find_path(request)
            .await
            .inspect_err(|e| info!("No Rhea route: {e:#}"))
            .ok()
    }
}

impl Provider for RheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::Rhea
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<RouteBlueprint>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaQuotes,
) -> Option<RouteBlueprint> {
    let Amount::AmountIn(exact_amount_in) = request.amount else {
        // smartrouter.ref.finance/findPath doesn't support AmountOut
        return None;
    };

    let slippage = get_slippage(
        network,
        request.slippage,
        &base_token(&request.token_in).unwrap(),
        &base_token(&request.token_out).unwrap(),
    );

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0)?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0)?;

    if token_in == token_out {
        return None;
    }

    // Fast path: swap directly within Rhea ledger if both in/out are Rhea balances
    let fast_path = matches!(request.token_in, TokenId::Nep141OnRhea(_))
        && matches!(request.token_out, TokenId::Nep141OnRhea(_));

    let swap_referrer_id: AccountId = request
        .referrer_id
        .clone()
        .unwrap_or_else(|| DEFAULT_REFERRER_ID.parse().unwrap());

    let referrer_fees =
        referrer_fees_of_total(&payable_referral_fees(&request.fees), exact_amount_in);
    // The fees come out of the input, so the rest is what's quoted and swapped
    let swap_amount_in = exact_amount_in - total_fee_amount(&referrer_fees);

    let response = quotes
        .find_path(rhea::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: swap_amount_in,
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &BaseTokenId::Near)?,
            token_out_price_raw: price_raw(network, &base_token(&request.token_out).unwrap())?,
        })
        .await?;

    if response.routes.is_empty() {
        return None;
    }

    let total_min_amount_out = response
        .routes
        .iter()
        .map(|route| route.min_amount_out)
        .sum();
    let actions = response
        .routes
        .into_iter()
        .flat_map(|route| route.pools)
        .map(|step| RheaSwapAction {
            pool_id: step.pool_id,
            token_in: step.token_in,
            token_out: step.token_out,
            amount_in: (step.amount_in != 0).then_some(step.amount_in),
            amount_out: step.amount_out,
            min_amount_out: step.min_amount_out,
        })
        .collect::<Vec<_>>();

    if fast_path {
        let route_tokens = [token_in.clone()]
            .into_iter()
            .chain(actions.iter().map(|action| action.token_out.clone()))
            .collect();
        let registration = match &request.trader_account_id {
            Some(trader_account_id) => vec![RouteStep::RegisterOnRheaIfNeeded {
                trader_account_id: trader_account_id.clone(),
                token_ids: route_tokens,
            }],
            None => vec![],
        };
        let swap = RouteStep::Call(CallBlueprint::RheaSwap {
            actions,
            swap_gas: response.swap_gas,
            referral_id: swap_referrer_id.clone(),
        });
        let fee_receivers_registration = if referrer_fees.is_empty() {
            vec![]
        } else {
            vec![RouteStep::RegisterFeeReceiversOnRheaIfNeeded {
                token_id: token_in.clone(),
                receiver_ids: referrer_fees
                    .iter()
                    .map(|fee| fee.referrer_id.clone())
                    .collect(),
            }]
        };
        let fee_transfers = referrer_fees
            .iter()
            .map(|fee| {
                RouteStep::Call(CallBlueprint::InputRheaFeeMftTransfer {
                    token_id: token_in.clone(),
                    receiver_id: fee.referrer_id.clone(),
                    amount: fee.amount,
                    swap_referrer_id: swap_referrer_id.clone(),
                })
            })
            .collect();

        return Some(RouteBlueprint {
            dex_id: DexId::Rhea,
            estimated_amount: Amount::AmountOut(response.amount_out),
            worst_case_amount: Amount::AmountOut(total_min_amount_out),
            token_output: request.token_out.clone(),
            discount: None,
            steps: [
                registration,
                vec![swap],
                fee_receivers_registration,
                fee_transfers,
            ]
            .concat(),
            fee_steps: vec![],
        });
    }

    let unwrapping_near = request.token_out == TokenId::Near;
    let token_output = if unwrapping_near {
        TokenId::Near
    } else {
        TokenId::Nep141(token_out)
    };
    let swap = RouteStep::Call(CallBlueprint::InputNep141TransferCallRheaSwap {
        token_id: token_in,
        amount: swap_amount_in,
        actions,
        swap_gas: response.swap_gas,
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
        exact_amount_in,
        &referrer_fees,
        &swap_referrer_id,
    )?;

    Some(RouteBlueprint {
        dex_id: DexId::Rhea,
        estimated_amount: Amount::AmountOut(response.amount_out),
        worst_case_amount: Amount::AmountOut(total_min_amount_out),
        steps: [
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
    use near_min_api::types::NearToken;

    use super::*;
    use crate::calls::IntearAsset;
    use crate::route::resolve;
    use crate::shared_utils::{TestNetworkView, WRAP_NEAR};
    use crate::types::{Gas, Slippage};
    use intear_dex_types::AssetId;
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;

    #[derive(Clone)]
    struct TestRheaQuotes {
        enabled: bool,
        empty_routes: bool,
    }

    impl Default for TestRheaQuotes {
        fn default() -> Self {
            Self {
                enabled: true,
                empty_routes: false,
            }
        }
    }

    impl TestRheaQuotes {
        fn none() -> Self {
            Self {
                enabled: false,
                empty_routes: false,
            }
        }

        fn empty_routes() -> Self {
            Self {
                enabled: true,
                empty_routes: true,
            }
        }
    }

    impl RheaQuotes for TestRheaQuotes {
        async fn find_path(&self, request: rhea::Request) -> Option<SplitRouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let routes = if self.empty_routes {
                vec![]
            } else {
                vec![rhea::ApiResponseRoute {
                    pools: vec![rhea::ApiResponsePoolStep {
                        pool_id: 1,
                        token_in: request.token_in.clone(),
                        token_out: request.token_out.clone(),
                        amount_in: request.amount_in,
                        amount_out: 0,
                        min_amount_out: 79,
                    }],
                    amount_in: request.amount_in,
                    min_amount_out: 79,
                    amount_out: 0,
                }]
            };
            Some(SplitRouteApiResponse {
                routes,
                contract_in: request.token_in,
                contract_out: request.token_out,
                amount_in: request.amount_in,
                amount_out: 80,
                swap_gas: SWAP_GAS,
            })
        }
    }

    const SWAP_GAS: Gas = Gas::from_ggas(5_500, 5_500);

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    fn request(token_in: TokenId, token_out: TokenId) -> SwapRequest {
        SwapRequest {
            token_in,
            token_out,
            amount: Amount::AmountIn(100),
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

    fn referred_request(token_in: TokenId, token_out: TokenId) -> SwapRequest {
        SwapRequest {
            amount: Amount::AmountIn(1000),
            trader_account_id: None,
            referrer_id: Some(account("ref.near")),
            fees: "{\"ref.near\":0.01}".parse().unwrap(),
            ..request(token_in, token_out)
        }
    }

    fn ft() -> TokenId {
        TokenId::Nep141(account("ft"))
    }

    fn actions(token_in: &str, token_out: &str, amount_in: u128) -> Vec<RheaSwapAction> {
        vec![RheaSwapAction {
            pool_id: 1,
            token_in: account(token_in),
            token_out: account(token_out),
            amount_in: Some(amount_in),
            amount_out: 0,
            min_amount_out: 79,
        }]
    }

    fn swap(
        token_in: &str,
        token_out: &str,
        amount_in: u128,
        output: RheaOutput,
        referral_id: &str,
    ) -> RouteStep {
        RouteStep::Call(CallBlueprint::InputNep141TransferCallRheaSwap {
            token_id: account(token_in),
            amount: amount_in,
            actions: actions(token_in, token_out, amount_in),
            swap_gas: SWAP_GAS,
            output,
            referral_id: account(referral_id),
        })
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

    fn fee_transfer(token_id: &str, receiver_id: &str, amount: u128) -> RouteStep {
        RouteStep::Call(CallBlueprint::InputNep141FeeTransfer {
            token_id: account(token_id),
            receiver_id: account(receiver_id),
            amount,
            swap_referrer_id: account("ref.near"),
        })
    }

    fn fee_receiver_storage(token_id: &str, account_id: &str) -> RouteStep {
        RouteStep::StorageDepositOnNep141IfNeeded {
            token_id: account(token_id),
            account_id: account(account_id),
        }
    }

    fn mft_fee_transfer(receiver_id: &str, amount: u128) -> RouteStep {
        RouteStep::Call(CallBlueprint::InputRheaFeeMftTransfer {
            token_id: account("ft"),
            receiver_id: account(receiver_id),
            amount,
            swap_referrer_id: account("ref.near"),
        })
    }

    fn rhea_route(
        steps: Vec<RouteStep>,
        fee_steps: Vec<RouteStep>,
        token_output: TokenId,
    ) -> RouteBlueprint {
        RouteBlueprint {
            dex_id: DexId::Rhea,
            estimated_amount: Amount::AmountOut(80),
            worst_case_amount: Amount::AmountOut(79),
            token_output,
            discount: None,
            steps,
            fee_steps,
        }
    }

    async fn test_route(request: SwapRequest) -> Option<RouteBlueprint> {
        route(
            request,
            &TestNetworkView::default().with_test_prices(),
            &TestRheaQuotes::default(),
        )
        .await
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Near, ft())).await,
            Some(rhea_route(
                vec![
                    trader_storage("ft"),
                    trader_storage(WRAP_NEAR),
                    wrap(100),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaOutput::Nep141,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_wnear_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Nep141(account(WRAP_NEAR)), ft())).await,
            Some(rhea_route(
                vec![
                    trader_storage("ft"),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaOutput::Nep141,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_wnear_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Nep141OnRhea(account(WRAP_NEAR)), ft())).await,
            Some(rhea_route(
                vec![
                    trader_storage("ft"),
                    trader_storage(WRAP_NEAR),
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account(WRAP_NEAR),
                        amount: 100,
                        output: RheaOutput::Nep141,
                    }),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaOutput::Nep141,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_intear_near_to_ft() {
        assert_eq!(
            test_route(request(TokenId::TokenOnIntearDex(AssetId::Near), ft())).await,
            Some(rhea_route(
                vec![
                    trader_storage("ft"),
                    trader_storage(WRAP_NEAR),
                    RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                        asset: IntearAsset::Near,
                        amount: 100,
                        trader_account_id: Some(trader()),
                    }),
                    wrap(100),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaOutput::Nep141,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_to_rhea_fast_path() {
        let rhea_other = TokenId::Nep141OnRhea(account("other"));
        let route = test_route(request(
            TokenId::Nep141OnRhea(account("ft")),
            rhea_other.clone(),
        ))
        .await
        .unwrap();
        assert_eq!(
            route,
            rhea_route(
                vec![
                    RouteStep::RegisterOnRheaIfNeeded {
                        trader_account_id: trader(),
                        token_ids: vec![account("ft"), account("other")],
                    },
                    RouteStep::Call(CallBlueprint::RheaSwap {
                        actions: actions("ft", "other", 100),
                        swap_gas: SWAP_GAS,
                        referral_id: account(DEFAULT_REFERRER_ID),
                    }),
                ],
                vec![],
                rhea_other,
            )
        );

        // The trader has a balance of the input and storage for one more token
        let network = TestNetworkView::default()
            .with_storage(
                RHEA_CONTRACT_ID,
                "trader.near",
                NearToken::from_millinear(100),
                NearToken::from_micronear(500),
            )
            .with_rhea_registered("trader.near", "ft", true);
        assert_eq!(
            resolve(&network, route).await.unwrap().calls,
            vec![
                CallBlueprint::RheaStorageDeposit {
                    account_id: None,
                    amount: "0.00098 NEAR".parse().unwrap(),
                },
                CallBlueprint::RheaRegisterTokens {
                    token_ids: vec![account("other")],
                },
                CallBlueprint::RheaSwap {
                    actions: actions("ft", "other", 100),
                    swap_gas: SWAP_GAS,
                    referral_id: account(DEFAULT_REFERRER_ID),
                },
            ]
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        assert_eq!(
            test_route(request(ft(), TokenId::Near)).await,
            Some(rhea_route(
                vec![swap(
                    "ft",
                    WRAP_NEAR,
                    100,
                    RheaOutput::Near {
                        trader_account_id: Some(trader())
                    },
                    DEFAULT_REFERRER_ID
                )],
                vec![],
                TokenId::Near,
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_ft_to_near() {
        assert_eq!(
            test_route(request(TokenId::Nep141OnRhea(account("ft")), TokenId::Near)).await,
            Some(rhea_route(
                vec![
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account("ft"),
                        amount: 100,
                        output: RheaOutput::Nep141,
                    }),
                    swap(
                        "ft",
                        WRAP_NEAR,
                        100,
                        RheaOutput::Near {
                            trader_account_id: Some(trader())
                        },
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                TokenId::Near,
            ))
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_rhea_ft_outputs_wallet_nep141() {
        let other = TokenId::Nep141(account("other"));
        assert_eq!(
            test_route(request(ft(), TokenId::Nep141OnRhea(account("other")))).await,
            Some(rhea_route(
                vec![
                    trader_storage("other"),
                    swap("ft", "other", 100, RheaOutput::Nep141, DEFAULT_REFERRER_ID),
                ],
                vec![],
                other,
            ))
        );
    }

    #[tokio::test]
    async fn amount_out_returns_none() {
        assert_eq!(
            test_route(SwapRequest {
                amount: Amount::AmountOut(80),
                ..request(TokenId::Near, ft())
            })
            .await,
            None
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            test_route(request(TokenId::Near, TokenId::Nep141(account(WRAP_NEAR)))).await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            route(
                request(TokenId::Near, ft()),
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn empty_routes_returns_none() {
        assert_eq!(
            route(
                request(TokenId::Near, ft()),
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::empty_routes(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn unpriced_token_out_has_no_route() {
        assert_eq!(
            test_route(request(TokenId::Near, TokenId::Nep141(account("unpriced")))).await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage() {
        assert_eq!(
            test_route(SwapRequest {
                trader_account_id: None,
                ..request(TokenId::Near, ft())
            })
            .await,
            Some(rhea_route(
                vec![
                    wrap(100),
                    swap(
                        WRAP_NEAR,
                        "ft",
                        100,
                        RheaOutput::Nep141,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_swap_msg() {
        assert_eq!(
            test_route(SwapRequest {
                trader_account_id: None,
                referrer_id: Some(account("ref.near")),
                ..request(TokenId::Near, ft())
            })
            .await,
            Some(rhea_route(
                vec![
                    wrap(100),
                    swap(WRAP_NEAR, "ft", 100, RheaOutput::Nep141, "ref.near"),
                ],
                vec![],
                ft(),
            ))
        );
    }

    #[tokio::test]
    async fn referrer_fee_in_near_is_sent_as_wnear_after_the_swap() {
        let route = test_route(referred_request(TokenId::Near, ft()))
            .await
            .unwrap();
        // All of the input is wrapped, the fees too
        assert_eq!(
            route.steps,
            vec![
                wrap(1000),
                swap(WRAP_NEAR, "ft", 990, RheaOutput::Nep141, "ref.near"),
            ]
        );
        // After the swap, 1% of 1000, 80% to the referrer and 20% to the protocol
        assert_eq!(
            route.fee_steps,
            vec![
                fee_receiver_storage(WRAP_NEAR, "ref.near"),
                fee_receiver_storage(WRAP_NEAR, AGGREGATOR_FEE_RECEIVER_ID.as_str()),
                fee_transfer(WRAP_NEAR, "ref.near", 8),
                fee_transfer(WRAP_NEAR, AGGREGATOR_FEE_RECEIVER_ID.as_str(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn referrer_fee_in_ft_is_transferred_after_the_swap() {
        let route = test_route(referred_request(ft(), TokenId::Nep141(account("other"))))
            .await
            .unwrap();
        assert_eq!(
            route.steps,
            vec![swap("ft", "other", 990, RheaOutput::Nep141, "ref.near")]
        );
        assert_eq!(
            route.fee_steps,
            vec![
                fee_receiver_storage("ft", "ref.near"),
                fee_receiver_storage("ft", AGGREGATOR_FEE_RECEIVER_ID.as_str()),
                fee_transfer("ft", "ref.near", 8),
                fee_transfer("ft", AGGREGATOR_FEE_RECEIVER_ID.as_str(), 2),
            ]
        );

        // Referrers with storage on the input only get their fees
        let network = TestNetworkView::default()
            .with_storage(
                "ft",
                "ref.near",
                "0.00125 NEAR".parse().unwrap(),
                NearToken::from_yoctonear(0),
            )
            .with_storage(
                "ft",
                AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                "0.00125 NEAR".parse().unwrap(),
                NearToken::from_yoctonear(0),
            );
        let resolved = resolve(&network, route).await.unwrap();
        assert_eq!(
            resolved.fee_calls,
            vec![
                CallBlueprint::InputNep141FeeTransfer {
                    token_id: account("ft"),
                    receiver_id: account("ref.near"),
                    amount: 8,
                    swap_referrer_id: account("ref.near"),
                },
                CallBlueprint::InputNep141FeeTransfer {
                    token_id: account("ft"),
                    receiver_id: AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    amount: 2,
                    swap_referrer_id: account("ref.near"),
                },
            ]
        );
        assert_eq!(resolved.fee_amount(), 10);
    }

    #[tokio::test]
    async fn referrer_fee_on_fast_path_moves_rhea_balance() {
        let route = test_route(SwapRequest {
            trader_account_id: Some(trader()),
            ..referred_request(
                TokenId::Nep141OnRhea(account("ft")),
                TokenId::Nep141OnRhea(account("other")),
            )
        })
        .await
        .unwrap();
        assert_eq!(
            route.steps,
            vec![
                RouteStep::RegisterOnRheaIfNeeded {
                    trader_account_id: trader(),
                    token_ids: vec![account("ft"), account("other")],
                },
                RouteStep::Call(CallBlueprint::RheaSwap {
                    actions: actions("ft", "other", 990),
                    swap_gas: SWAP_GAS,
                    referral_id: account("ref.near"),
                }),
                RouteStep::RegisterFeeReceiversOnRheaIfNeeded {
                    token_id: account("ft"),
                    receiver_ids: vec![account("ref.near"), AGGREGATOR_FEE_RECEIVER_ID.to_owned()],
                },
                mft_fee_transfer("ref.near", 8),
                mft_fee_transfer(AGGREGATOR_FEE_RECEIVER_ID.as_str(), 2),
            ]
        );
        assert_eq!(route.fee_steps, vec![]);

        // A new Rhea account with a balance of one token for the referrer that has none
        let network = TestNetworkView::default()
            .with_rhea_registered("trader.near", "ft", true)
            .with_rhea_registered("trader.near", "other", true)
            .with_rhea_registered(AGGREGATOR_FEE_RECEIVER_ID.as_str(), "ft", true);
        let resolved = resolve(&network, route).await.unwrap();
        assert_eq!(
            resolved.calls[1],
            CallBlueprint::RheaStorageDeposit {
                account_id: Some(account("ref.near")),
                amount: NearToken::from_yoctonear(250 * 10u128.pow(19)),
            }
        );
        // The fees come out of the Rhea balance, not the wallet
        assert_eq!(resolved.fee_amount(), 0);
    }

    #[tokio::test]
    async fn each_referrer_gets_its_fee() {
        let route = test_route(SwapRequest {
            fees: r#"{"ref.near":0.01,"ref2.near":0.02,"nostorage.near":0.01}"#
                .parse()
                .unwrap(),
            ..referred_request(ft(), TokenId::Nep141(account("other")))
        })
        .await
        .unwrap();
        assert_eq!(
            route.steps,
            vec![swap("ft", "other", 960, RheaOutput::Nep141, "ref.near")]
        );
        // 80% of each fee to its referrer, the protocol's 20% of all of them together
        assert_eq!(
            route.fee_steps,
            vec![
                fee_receiver_storage("ft", "nostorage.near"),
                fee_receiver_storage("ft", "ref.near"),
                fee_receiver_storage("ft", "ref2.near"),
                fee_receiver_storage("ft", AGGREGATOR_FEE_RECEIVER_ID.as_str()),
                fee_transfer("ft", "nostorage.near", 8),
                fee_transfer("ft", "ref.near", 8),
                fee_transfer("ft", "ref2.near", 16),
                fee_transfer("ft", AGGREGATOR_FEE_RECEIVER_ID.as_str(), 8),
            ]
        );
    }

    #[tokio::test]
    async fn fast_path_moves_rhea_balance_to_each_referrer() {
        let route = test_route(SwapRequest {
            trader_account_id: Some(trader()),
            fees: r#"{"ref.near":0.01,"ref2.near":0.02,"unregistered.near":0.01}"#
                .parse()
                .unwrap(),
            ..referred_request(
                TokenId::Nep141OnRhea(account("ft")),
                TokenId::Nep141OnRhea(account("other")),
            )
        })
        .await
        .unwrap();
        assert_eq!(
            route.steps[2..],
            [
                RouteStep::RegisterFeeReceiversOnRheaIfNeeded {
                    token_id: account("ft"),
                    receiver_ids: vec![
                        account("ref.near"),
                        account("ref2.near"),
                        account("unregistered.near"),
                        AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    ],
                },
                mft_fee_transfer("ref.near", 8),
                mft_fee_transfer("ref2.near", 16),
                mft_fee_transfer("unregistered.near", 8),
                mft_fee_transfer(AGGREGATOR_FEE_RECEIVER_ID.as_str(), 8),
            ]
        );
    }
}
