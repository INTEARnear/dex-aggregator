use std::{future::Future, pin::Pin};

use near_min_api::types::{AccountId, Action, FunctionCallAction, NearGas, NearToken};
use pathfinder::{
    dcl::{self, RouteApiResponse},
    MaxHops, QuoteAmount,
};
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, create_storage_withdraw_action, deposit_storage_if_needed,
        deposit_storage_on_contract_if_needed, get_slippage, price_raw, swap_call_gas, Mainnet,
        NetworkView,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaDclProvider;

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";
const RESERVED_GAS: NearGas = NearGas::from_tgas(62);
/// The contract takes no less storage deposit than this
const STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(500);
/// Rhea DCL doesn't let you deposit 0.1 NEAR, but it lets you withdraw 0.4 after depositing 0.5
const STORAGE_WITHDRAWN: NearToken = NearToken::from_millinear(400);

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

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaDclQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaDclQuotes,
) -> Option<Route> {
    let slippage = get_slippage(
        network,
        request.slippage,
        &request.token_in,
        &request.token_out,
    );

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

    if token_in == token_out {
        return None;
    }

    let response = quotes
        .find_path(dcl::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount: match request.amount {
                Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &TokenId::Near)?,
            quoted_token_price_raw: price_raw(
                network,
                match request.amount {
                    Amount::AmountIn(_) => &request.token_out,
                    Amount::AmountOut(_) => &request.token_in,
                },
            )?,
        })
        .await?;

    let unwrapping_near = request.token_out == TokenId::Near;
    let (amount_in, message, estimated_amount, worst_case_amount, swap_gas) = match response {
        RouteApiResponse::ExactIn { route, .. } => {
            let pool_ids = route
                .pools
                .iter()
                .map(|step| step.pool_id.as_str())
                .collect::<Vec<_>>();
            let message = serde_json::json!({
                "Swap": {
                    "pool_ids": pool_ids,
                    "output_token": token_out,
                    "min_output_amount": route.min_amount_out.0.to_string(),
                    "skip_unwrap_near": !unwrapping_near,
                }
            });
            (
                route.amount_in.0,
                message,
                Amount::AmountOut(route.amount_out.0),
                Amount::AmountOut(route.min_amount_out.0),
                route.swap_gas,
            )
        }
        RouteApiResponse::ExactOut { route, .. } => {
            // SwapByOutput walks the pools from the output token
            let pool_ids = route
                .pools
                .iter()
                .rev()
                .map(|step| step.pool_id.as_str())
                .collect::<Vec<_>>();
            let message = serde_json::json!({
                "SwapByOutput": {
                    "pool_ids": pool_ids,
                    "output_token": token_out,
                    "output_amount": route.amount_out.0.to_string(),
                    "skip_unwrap_near": !unwrapping_near,
                }
            });
            (
                route.max_amount_in.0,
                message,
                Amount::AmountIn(route.amount_in.0),
                Amount::AmountIn(route.max_amount_in.0),
                route.swap_gas,
            )
        }
    };

    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": RHEA_DCL_CONTRACT_ID,
            "amount": amount_in.to_string(),
            "msg": serde_json::to_string(&message).unwrap(),
        }))
        .unwrap(),
        gas: swap_call_gas(swap_gas, RESERVED_GAS, true),
        deposit: NearToken::from_yoctonear(1),
    }));
    let swap_transactions = vec![ExecutionInstruction::NearTransaction {
        receiver_id: token_in,
        actions: vec![swap_action],
    }];

    let (input_to_nep141, input_nep141) = convert_to_nep141(
        &request.token_in,
        request.trader_account_id.clone(),
        amount_in,
    )
    .await?;
    let token_output = if unwrapping_near {
        TokenId::Near
    } else {
        TokenId::Nep141(token_out)
    };
    let mut storage_deposit = deposit_storage_on_contract_if_needed(
        network,
        &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
        request.trader_account_id.clone(),
        STORAGE_DEPOSIT,
    )
    .await;
    if let [ExecutionInstruction::NearTransaction { actions, .. }] = storage_deposit.as_mut_slice()
    {
        actions.push(create_storage_withdraw_action(STORAGE_WITHDRAWN));
    }
    let transactions = [
        storage_deposit,
        deposit_storage_if_needed(network, &token_output, request.trader_account_id.clone()).await,
        deposit_storage_if_needed(
            network,
            &TokenId::Nep141(input_nep141),
            request.trader_account_id.clone(),
        )
        .await,
        input_to_nep141,
        swap_transactions,
    ]
    .concat();
    Some(Route {
        dex_id: DexId::RheaDcl,
        estimated_amount,
        deadline: None,
        has_slippage: true,
        worst_case_amount,
        execution_instructions: transactions,
        deprecated_needs_unwrap_always_false: false,
        token_output,
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken, U128};

    use super::*;
    use crate::shared_utils::{
        create_storage_deposit_action_for_contract, create_wrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;

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
                QuoteAmount::ExactIn(_) => Some(RouteApiResponse::ExactIn {
                    route: dcl::ExactInRoute {
                        pools,
                        amount_in: U128(100),
                        min_amount_out: U128(79),
                        amount_out: U128(80),
                        swap_gas: NearGas::from_tgas(20),
                    },
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: U128(100),
                    amount_out: U128(80),
                }),
                QuoteAmount::ExactOut(_) => Some(RouteApiResponse::ExactOut {
                    route: dcl::ExactOutRoute {
                        pools,
                        amount_in: U128(100),
                        max_amount_in: U128(101),
                        amount_out: U128(80),
                        swap_gas: NearGas::from_tgas(20),
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

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                NearToken::from_millinear(500),
                                true
                            ),
                            create_storage_withdraw_action(NearToken::from_millinear(400)),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["wrap.near|usdc|100", "ft|usdc|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(114)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                NearToken::from_millinear(500),
                                true
                            ),
                            create_storage_withdraw_action(NearToken::from_millinear(400)),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|usdc|100", "wrap.near|usdc|2000"],
                                        "output_token": WRAP_NEAR,
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": false,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(114)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountOut(80),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                NearToken::from_millinear(500),
                                true
                            ),
                            create_storage_withdraw_action(NearToken::from_millinear(400)),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(101))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "SwapByOutput": {
                                        "pool_ids": vec!["ft|usdc|2000", "wrap.near|usdc|100"],
                                        "output_token": "ft",
                                        "output_amount": "80",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(114)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountOut(80),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(101))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "SwapByOutput": {
                                        "pool_ids": vec!["ft|usdc|2000", "wrap.near|usdc|100"],
                                        "output_token": "ft",
                                        "output_amount": "80",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(114)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }
}
