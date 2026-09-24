use std::{future::Future, pin::Pin};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{AccountId, Action, Balance, FunctionCallAction, Gas, NearGas, NearToken},
    utils::dec_format,
};
use serde::Deserialize;
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, get_slippage, Mainnet, NetworkView,
        DEFAULT_REFERRER_ID, REQWEST_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaProvider;

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";

#[derive(Debug, Deserialize)]
struct RheaSmartRouterResponse {
    result_data: RheaSmartRouterResultData,
}

#[derive(Debug, Deserialize)]
struct RheaSmartRouterResultData {
    routes: Vec<RheaSmartRouterRoute>,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Debug, Deserialize)]
struct RheaSmartRouterRoute {
    pools: Vec<serde_json::Value>,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
}

trait RheaQuotes: Send + Sync {
    fn find_path(
        &self,
        token_in: &AccountId,
        token_out: &AccountId,
        amount_in: Balance,
        slippage: &BigDecimal,
    ) -> impl Future<Output = Option<RheaSmartRouterResultData>> + Send;
}

struct MainnetRheaQuotes;

impl RheaQuotes for MainnetRheaQuotes {
    async fn find_path(
        &self,
        token_in: &AccountId,
        token_out: &AccountId,
        amount_in: Balance,
        slippage: &BigDecimal,
    ) -> Option<RheaSmartRouterResultData> {
        let url = format!("http://localhost:12345/findPath?tokenIn={token_in}&tokenOut={token_out}&maxHops=Four&slippage={slippage}&amountIn={amount_in}");
        info!("URL: {url}");
        let response = REQWEST_CLIENT.get(url).send().await.ok()?;
        let response = response.json::<RheaSmartRouterResponse>().await.ok()?;
        info!("Found Rhea route: {:?}", response.result_data);
        Some(response.result_data)
    }
}

impl Provider for RheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::Rhea
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaQuotes,
) -> Option<Route> {
    let Amount::AmountIn(exact_amount_in) = request.amount else {
        // smartrouter.ref.finance/findPath doesn't support AmountOut
        return None;
    };

    let slippage = get_slippage(
        network,
        request.slippage,
        &request.token_in,
        &request.token_out,
    )
    .await;

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

    if token_in == token_out {
        return None;
    }

    let response = quotes
        .find_path(&token_in, &token_out, exact_amount_in, &slippage)
        .await?;

    if response.routes.is_empty() {
        return None;
    }

    let total_min_amount_out = response
        .routes
        .iter()
        .map(|route| route.min_amount_out)
        .sum();
    let steps = response.routes.into_iter().flat_map(|route| route.pools);

    let actions: Vec<serde_json::Value> = steps
        .map(|step| {
            let mut new_step = step.clone();
            if let Some(pool) = step.get("pool_id") {
                if let Some(pool_str) = pool.as_str() {
                    if let Ok(pool_u64) = pool_str.parse::<u64>() {
                        new_step["pool_id"] = serde_json::Value::from(pool_u64);
                    }
                }
            }
            if let Some(amount_in) = step.get("amount_in") {
                if amount_in == "0" {
                    new_step.as_object_mut().unwrap().remove("amount_in");
                }
            }
            new_step
        })
        .collect();

    // Fast path: swap directly within Rhea ledger if both in/out are Rhea balances
    if matches!(request.token_in, TokenId::Nep141OnRhea(_))
        && matches!(request.token_out, TokenId::Nep141OnRhea(_))
    {
        info!("Using fast path");
        let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "swap".to_string(),
            args: serde_json::to_vec(&serde_json::json!({
                "actions": actions,
                "referral_id": DEFAULT_REFERRER_ID,
            }))
            .unwrap(),
            gas: Gas(NearGas::from_tgas(150)),
            deposit: NearToken::from_yoctonear(1),
        }));

        let transactions = vec![ExecutionInstruction::NearTransaction {
            receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
            actions: vec![swap_action],
        }];

        return Some(Route {
            dex_id: DexId::Rhea,
            deadline: None,
            has_slippage: true,
            estimated_amount: Amount::AmountOut(response.amount_out),
            worst_case_amount: Amount::AmountOut(total_min_amount_out),
            execution_instructions: transactions,
            deprecated_needs_unwrap_always_false: false,
            token_output: request.token_out.clone(),
        });
    }

    let unwrapping_near = request.token_out == TokenId::Near;
    let ft_transfer_call_swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": RHEA_CONTRACT_ID,
            "amount": exact_amount_in.to_string(),
            "msg": serde_json::to_string(&serde_json::json!({
                "force": 0,
                "actions": actions,
                "skip_degen_price_sync": true,
                "skip_unwrap_near": !unwrapping_near,
                "referral_id": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
            })).unwrap(),
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(150)),
        deposit: NearToken::from_yoctonear(1),
    }));
    let swap_transactions = vec![ExecutionInstruction::NearTransaction {
        receiver_id: token_in,
        actions: vec![ft_transfer_call_swap_action],
    }];

    let (input_to_nep141, input_nep141) = convert_to_nep141(
        &request.token_in,
        request.trader_account_id.clone(),
        exact_amount_in,
    )
    .await?;

    let transactions = [
        deposit_storage_if_needed(
            network,
            &if unwrapping_near {
                TokenId::Near
            } else {
                TokenId::Nep141(token_out.clone())
            },
            request.trader_account_id.clone(),
        )
        .await,
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
        dex_id: DexId::Rhea,
        deadline: None,
        has_slippage: true,
        estimated_amount: Amount::AmountOut(response.amount_out),
        worst_case_amount: Amount::AmountOut(total_min_amount_out),
        execution_instructions: transactions,
        deprecated_needs_unwrap_always_false: false,
        token_output: if unwrapping_near {
            TokenId::Near
        } else {
            TokenId::Nep141(token_out)
        },
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::providers::intear_plach::AssetId;
    use crate::shared_utils::{
        create_intear_dex_withdraw_action, create_rhea_withdraw_action,
        create_storage_deposit_action_for_contract, create_wrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;

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
        async fn find_path(
            &self,
            token_in: &AccountId,
            token_out: &AccountId,
            _amount_in: Balance,
            _slippage: &BigDecimal,
        ) -> Option<RheaSmartRouterResultData> {
            if !self.enabled {
                return None;
            }
            if self.empty_routes {
                return Some(RheaSmartRouterResultData {
                    routes: vec![],
                    amount_out: 80,
                });
            }
            Some(RheaSmartRouterResultData {
                routes: vec![RheaSmartRouterRoute {
                    pools: vec![serde_json::json!({
                        "pool_id": "1",
                        "token_in": token_in,
                        "token_out": token_out,
                        "amount_in": "100",
                        "amount_out": "0",
                        "min_amount_out": "79",
                    })],
                    min_amount_out: 79,
                }],
                amount_out: 80,
            })
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
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
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_wnear_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
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
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_rhea_wnear_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
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
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_intear_near_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
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
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(&AssetId::Near, 100)],
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
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_rhea_to_rhea_fast_path() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "swap".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "actions": vec![serde_json::json!({
                                "pool_id": 1,
                                "token_in": "ft",
                                "token_out": "other",
                                "amount_in": "100",
                                "amount_out": "0",
                                "min_amount_out": "79",
                            })],
                            "referral_id": DEFAULT_REFERRER_ID,
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(150)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
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
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": "ft",
                                        "token_out": WRAP_NEAR,
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": false,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_rhea_ft_to_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": "ft",
                                        "token_out": WRAP_NEAR,
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": false,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn amount_in_ft_to_rhea_ft_outputs_wallet_nep141() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "other".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
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
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": "ft",
                                        "token_out": "other",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("other".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_returns_none() {
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            None
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
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
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
                &TestNetworkView::default(),
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
                &TestNetworkView::default(),
                &TestRheaQuotes::empty_routes(),
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
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
    async fn custom_referrer_id_is_passed_in_swap_msg() {
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
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: Some("ref.near".parse().unwrap()),
                },
                &TestNetworkView::default(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": "ref.near",
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(150)),
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
