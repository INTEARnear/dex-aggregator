use std::{future::Future, pin::Pin};

use near_min_api::types::{AccountId, Action, FunctionCallAction, Gas, NearGas, NearToken};
use pathfinder::{
    rhea::{self, SplitRouteApiResponse},
    MaxHops,
};
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, create_storage_deposit_action_for_contract, deposit_storage_if_needed,
        get_slippage, swap_call_gas, Mainnet, NetworkView, DEFAULT_REFERRER_ID, STORAGE_BYTE_COST,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaProvider;

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";
const FT_TRANSFER_CALL_RESERVED_GAS: NearGas = NearGas::from_tgas(57);
const SWAP_RESERVED_GAS: NearGas = NearGas::from_tgas(10);
const ACCOUNT_STORAGE_BYTES: u128 = 102;
const TOKEN_STORAGE_BYTES: u128 = 148;

/// Registers the tokens the trader has no balance of on Rhea yet, with the storage deposit they
/// lack. `swap` adds a balance of every token it outputs, also the ones in the middle of a route.
async fn registration(
    network: &impl NetworkView,
    trader_account_id: &AccountId,
    token_ids: &[&AccountId],
) -> Option<Vec<ExecutionInstruction>> {
    let mut unregistered = Vec::new();
    for &token_id in token_ids {
        if !unregistered.contains(&token_id)
            && !network
                .is_rhea_token_registered(trader_account_id, token_id)
                .await
        {
            unregistered.push(token_id);
        }
    }
    if unregistered.is_empty() {
        return Some(vec![]);
    }
    let storage = network
        .storage_balance(RHEA_CONTRACT_ID.parse().unwrap(), trader_account_id)
        .await
        .inspect_err(|e| info!("Failed to get the storage balance of {trader_account_id}: {e}"))
        .ok()?;
    let account_bytes = match storage {
        Some(_) => 0,
        None => ACCOUNT_STORAGE_BYTES,
    };
    let available = storage.map_or(NearToken::from_yoctonear(0), |storage| storage.available());
    let needed = STORAGE_BYTE_COST
        .saturating_mul(account_bytes + TOKEN_STORAGE_BYTES * unregistered.len() as u128);
    let mut actions = Vec::new();
    if needed > available {
        actions.push(create_storage_deposit_action_for_contract(
            needed.saturating_sub(available),
            false,
        ));
    }
    actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "register_tokens".to_string(),
        args: serde_json::to_vec(&serde_json::json!({ "token_ids": unregistered })).unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: NearToken::from_yoctonear(1),
    })));
    Some(vec![ExecutionInstruction::NearTransaction {
        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
        actions,
    }])
}

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
    );

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

    if token_in == token_out {
        return None;
    }

    let response = quotes
        .find_path(rhea::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: exact_amount_in,
            max_hops: MaxHops::Four,
            slippage,
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
    let swap_gas = response.swap_gas;
    let steps = response
        .routes
        .into_iter()
        .flat_map(|route| route.pools)
        .collect::<Vec<_>>();
    let route_tokens = [&token_in]
        .into_iter()
        .chain(steps.iter().map(|step| &step.token_out))
        .collect::<Vec<_>>();
    let registration_transactions = match (&request.token_in, &request.trader_account_id) {
        (TokenId::Nep141OnRhea(_), Some(trader_account_id))
            if matches!(request.token_out, TokenId::Nep141OnRhea(_)) =>
        {
            registration(network, trader_account_id, &route_tokens).await?
        }
        _ => vec![],
    };

    let actions: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            let step = serde_json::to_value(step).unwrap();
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
                "skip_degen_price_sync": true,
            }))
            .unwrap(),
            gas: swap_call_gas(swap_gas, SWAP_RESERVED_GAS, false),
            deposit: NearToken::from_yoctonear(1),
        }));

        let transactions = [
            registration_transactions,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                actions: vec![swap_action],
            }],
        ]
        .concat();

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
        gas: swap_call_gas(swap_gas, FT_TRANSFER_CALL_RESERVED_GAS, true),
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
                        amount_in: 100,
                        amount_out: 0,
                        min_amount_out: 79,
                    }],
                    amount_in: 100,
                    min_amount_out: 79,
                    amount_out: 0,
                }]
            };
            Some(SplitRouteApiResponse {
                routes,
                contract_in: request.token_in,
                contract_out: request.token_out,
                amount_in: 100,
                amount_out: 80,
                swap_gas: NearGas::from_ggas(5_500),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                &TestNetworkView::default()
                    .with_storage(
                        RHEA_CONTRACT_ID,
                        "trader.near",
                        NearToken::from_millinear(100),
                        NearToken::from_micronear(500),
                    )
                    .with_rhea_registered("trader.near", "ft", true),
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
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                "0.00098 NEAR".parse().unwrap(),
                                false,
                            ),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_tokens".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "token_ids": ["other"],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(10)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
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
                                "skip_degen_price_sync": true,
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(37)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
                            gas: Gas(NearGas::from_tgas(94)),
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
