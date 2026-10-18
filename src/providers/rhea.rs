use std::{future::Future, pin::Pin};

use near_min_api::types::{Action, FunctionCallAction, NearGas, NearToken};
use pathfinder::{
    rhea::{self, SplitRouteApiResponse},
    MaxHops,
};
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, convert_to_nep141_with_fees, create_rhea_fee_mft_transfer_action,
        deposit_storage_if_needed, get_slippage, needs_storage_deposit_for_contract,
        payable_referral_fees, price_raw, referrer_fees_of_total, register_input_in_wallet,
        rhea_registration, swap_call_gas, total_fee_amount, Mainnet, NetworkView,
        DEFAULT_REFERRER_ID,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaProvider;

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";
const FT_TRANSFER_CALL_RESERVED_GAS: NearGas = NearGas::from_tgas(57);
const SWAP_RESERVED_GAS: NearGas = NearGas::from_tgas(10);
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

    // Fast path: swap directly within Rhea ledger if both in/out are Rhea balances
    let fast_path = matches!(request.token_in, TokenId::Nep141OnRhea(_))
        && matches!(request.token_out, TokenId::Nep141OnRhea(_));

    let token_in_ref = &token_in;
    let payable_referral_fees =
        payable_referral_fees(&request.fees, |referrer_id| async move {
            if fast_path {
                network
                    .is_rhea_token_registered(&referrer_id, token_in_ref)
                    .await
            } else {
                !needs_storage_deposit_for_contract(network, &referrer_id, token_in_ref).await
            }
        })
        .await;
    let referrer_fees = referrer_fees_of_total(&payable_referral_fees, exact_amount_in);
    // The fees come out of the input, so the rest is what's quoted and swapped
    let swap_amount_in = exact_amount_in - total_fee_amount(&referrer_fees);

    let response = quotes
        .find_path(rhea::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: swap_amount_in,
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &TokenId::Near)?,
            token_out_price_raw: price_raw(network, &request.token_out)?,
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
            rhea_registration(network, trader_account_id, &route_tokens).await?
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

    if fast_path {
        let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "swap".to_string(),
            args: serde_json::to_vec(&serde_json::json!({
                "actions": actions,
                "referral_id": request.referrer_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                "skip_degen_price_sync": true,
            }))
            .unwrap(),
            gas: swap_call_gas(swap_gas, SWAP_RESERVED_GAS, false),
            deposit: NearToken::from_yoctonear(1),
        }));

        let fee_actions = referrer_fees
            .iter()
            .map(|fee| create_rhea_fee_mft_transfer_action(&token_in, &fee.referrer_id, fee.amount))
            .collect();
        let transactions = [
            registration_transactions,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                actions: [vec![swap_action], fee_actions].concat(),
                is_fee_transaction: false,
            }],
        ]
        .concat();

        return Some(Route {
            dex_id: DexId::Rhea,
            estimated_amount: Amount::AmountOut(response.amount_out),
            worst_case_amount: Amount::AmountOut(total_min_amount_out),
            execution_instructions: transactions,
            deprecated_needs_unwrap_always_false: false,
            discount: None,
            token_output: request.token_out.clone(),
        });
    }

    let unwrapping_near = request.token_out == TokenId::Near;
    let ft_transfer_call_swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": RHEA_CONTRACT_ID,
            "amount": swap_amount_in.to_string(),
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
        is_fee_transaction: false,
    }];

    let (input_to_nep141, send_fees, input_nep141) = convert_to_nep141_with_fees(
        &request.token_in,
        request.trader_account_id.clone(),
        exact_amount_in,
        &referrer_fees,
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
        .await?,
        register_input_in_wallet(
            network,
            &request.token_in,
            input_nep141,
            request.trader_account_id.clone(),
        )
        .await?,
        input_to_nep141,
        swap_transactions,
        send_fees,
    ]
    .concat();

    Some(Route {
        dex_id: DexId::Rhea,
        estimated_amount: Amount::AmountOut(response.amount_out),
        worst_case_amount: Amount::AmountOut(total_min_amount_out),
        execution_instructions: transactions,
        deprecated_needs_unwrap_always_false: false,
        discount: None,
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
        create_fee_ft_transfer_action, create_intear_dex_withdraw_action,
        create_rhea_fee_mft_transfer_action, create_rhea_withdraw_action,
        create_storage_deposit_action_for_contract, create_wrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;
    use xyk_dex_types::PROTOCOL_FEE_RECEIVER_ID;

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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            false,
                        )],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(&AssetId::Near, 100)],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default()
                    .with_test_prices()
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
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(33_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
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
                        gas: Gas(NearGas::from_ggas(90_500)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                    is_fee_transaction: false,
                },],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
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
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
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
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
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
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("unpriced".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: Some("ref.near".parse().unwrap()),
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
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
                            gas: Gas(NearGas::from_ggas(90_500)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    fn rhea_swap_msg(
        token_in: &str,
        token_out: &str,
        amount_in: &str,
        referral_id: &str,
    ) -> String {
        serde_json::to_string(&serde_json::json!({
            "force": 0,
            "actions": vec![serde_json::json!({
                "pool_id": 1,
                "token_in": token_in,
                "token_out": token_out,
                "amount_in": amount_in,
                "amount_out": "0",
                "min_amount_out": "79",
            })],
            "skip_degen_price_sync": true,
            "skip_unwrap_near": true,
            "referral_id": referral_id,
        }))
        .unwrap()
    }

    fn ft_transfer_call_to_rhea(amount: &str, msg: String) -> Action {
        Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "ft_transfer_call".to_string(),
            args: serde_json::to_vec(&serde_json::json!({
                "receiver_id": RHEA_CONTRACT_ID,
                "amount": amount,
                "msg": msg,
            }))
            .unwrap(),
            gas: Gas(NearGas::from_ggas(90_500)),
            deposit: NearToken::from_yoctonear(1),
        }))
    }

    fn referred_request(token_in: TokenId, token_out: TokenId) -> SwapRequest {
        SwapRequest {
            token_in,
            token_out,
            amount: Amount::AmountIn(1000),
            slippage: Slippage::Fixed {
                slippage: "0.01".parse().unwrap(),
            },
            dexes: None,
            trader_account_id: None,
            signing_public_key: None,
            referrer_id: Some("ref.near".parse().unwrap()),
            fees: "{\"ref.near\":0.01}".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn referrer_fee_in_near_is_sent_as_wnear_after_the_swap() {
        let route = route(
            referred_request(TokenId::Near, TokenId::Nep141("ft".parse().unwrap())),
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    WRAP_NEAR,
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    PROTOCOL_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions,
            vec![
                // All of the input is wrapped, the fees too
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![create_wrap_action(NearToken::from_yoctonear(1000))],
                    is_fee_transaction: false,
                },
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![ft_transfer_call_to_rhea(
                        "990",
                        rhea_swap_msg(WRAP_NEAR, "ft", "990", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                // After the swap, 1% of 1000, 80% to the referrer and 20% to the protocol
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8),
                        create_fee_ft_transfer_action(&PROTOCOL_FEE_RECEIVER_ID.to_owned(), 2),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn referrer_fee_in_ft_is_transferred_after_the_swap() {
        let route = route(
            referred_request(
                TokenId::Nep141("ft".parse().unwrap()),
                TokenId::Nep141("other".parse().unwrap()),
            ),
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    "ft",
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    PROTOCOL_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions,
            vec![
                ExecutionInstruction::NearTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    actions: vec![ft_transfer_call_to_rhea(
                        "990",
                        rhea_swap_msg("ft", "other", "990", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                ExecutionInstruction::NearTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    actions: vec![
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8),
                        create_fee_ft_transfer_action(&PROTOCOL_FEE_RECEIVER_ID.to_owned(), 2),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn referrer_without_storage_on_input_gets_no_fee() {
        let route = route(
            referred_request(
                TokenId::Nep141("ft".parse().unwrap()),
                TokenId::Nep141("other".parse().unwrap()),
            ),
            &TestNetworkView::default().with_test_prices(),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "ft".parse().unwrap(),
                actions: vec![ft_transfer_call_to_rhea(
                    "1000",
                    rhea_swap_msg("ft", "other", "1000", "ref.near"),
                )],
                is_fee_transaction: false,
            }]
        );
    }

    #[tokio::test]
    async fn referrer_fee_on_fast_path_moves_rhea_balance() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true)
                .with_rhea_registered("ref.near", "ft", true)
                .with_rhea_registered(PROTOCOL_FEE_RECEIVER_ID.as_str(), "ft", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                actions: vec![
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "swap".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "actions": vec![serde_json::json!({
                                "pool_id": 1,
                                "token_in": "ft",
                                "token_out": "other",
                                "amount_in": "990",
                                "amount_out": "0",
                                "min_amount_out": "79",
                            })],
                            "referral_id": "ref.near",
                            "skip_degen_price_sync": true,
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_ggas(33_500)),
                        deposit: NearToken::from_yoctonear(1),
                    })),
                    create_rhea_fee_mft_transfer_action(
                        &"ft".parse().unwrap(),
                        &"ref.near".parse().unwrap(),
                        8,
                    ),
                    create_rhea_fee_mft_transfer_action(
                        &"ft".parse().unwrap(),
                        &PROTOCOL_FEE_RECEIVER_ID.to_owned(),
                        2,
                    ),
                ],
                is_fee_transaction: false,
            }]
        );
    }

    #[tokio::test]
    async fn referrer_without_rhea_registration_gets_no_fee_on_fast_path() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        let ExecutionInstruction::NearTransaction { actions, .. } =
            &route.execution_instructions[0];
        assert_eq!(actions.len(), 1, "Only the swap, no fee transfer");
    }

    #[tokio::test]
    async fn each_referrer_that_can_receive_the_input_gets_its_fee() {
        let route = route(
            SwapRequest {
                fees: r#"{"ref.near":0.01,"ref2.near":0.02,"nostorage.near":0.01}"#
                    .parse()
                    .unwrap(),
                ..referred_request(
                    TokenId::Nep141("ft".parse().unwrap()),
                    TokenId::Nep141("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    "ft",
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    "ref2.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    PROTOCOL_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions,
            vec![
                ExecutionInstruction::NearTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    actions: vec![ft_transfer_call_to_rhea(
                        "970",
                        rhea_swap_msg("ft", "other", "970", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                ExecutionInstruction::NearTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    // 80% of each fee to its referrer, the protocol's 20% of both together
                    actions: vec![
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8),
                        create_fee_ft_transfer_action(&"ref2.near".parse().unwrap(), 16),
                        create_fee_ft_transfer_action(&PROTOCOL_FEE_RECEIVER_ID.to_owned(), 6),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn each_referrer_fee_in_near_is_its_own_wnear_transfer() {
        let route = route(
            SwapRequest {
                fees: r#"{"ref.near":0.01,"ref2.near":0.02}"#.parse().unwrap(),
                ..referred_request(TokenId::Near, TokenId::Nep141("ft".parse().unwrap()))
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    WRAP_NEAR,
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    "ref2.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    PROTOCOL_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions[0],
            ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![create_wrap_action(NearToken::from_yoctonear(1000))],
                is_fee_transaction: false,
            }
        );
        // After the swap
        assert_eq!(
            route.execution_instructions.last().unwrap(),
            &ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![
                    create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8),
                    create_fee_ft_transfer_action(&"ref2.near".parse().unwrap(), 16),
                    create_fee_ft_transfer_action(&PROTOCOL_FEE_RECEIVER_ID.to_owned(), 6),
                ],
                is_fee_transaction: true,
            }
        );
    }

    #[tokio::test]
    async fn fast_path_moves_rhea_balance_to_each_registered_referrer() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                fees: r#"{"ref.near":0.01,"ref2.near":0.02,"unregistered.near":0.01}"#
                    .parse()
                    .unwrap(),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true)
                .with_rhea_registered("ref.near", "ft", true)
                .with_rhea_registered("ref2.near", "ft", true)
                .with_rhea_registered(PROTOCOL_FEE_RECEIVER_ID.as_str(), "ft", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        let ExecutionInstruction::NearTransaction { actions, .. } =
            &route.execution_instructions[0];
        assert_eq!(
            actions[1..],
            [
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"ref.near".parse().unwrap(),
                    8,
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"ref2.near".parse().unwrap(),
                    16,
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &PROTOCOL_FEE_RECEIVER_ID.to_owned(),
                    6,
                ),
            ]
        );
    }
}
