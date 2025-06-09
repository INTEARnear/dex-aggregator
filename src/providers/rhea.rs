use std::{future::Future, pin::Pin};

use near_min_api::{
    types::{Action, Balance, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
};
use serde::Deserialize;
use tracing::info;

use crate::{
    shared_utils::{
        create_storage_deposit_action, create_wrap_action, get_slippage_f64, needs_storage_deposit,
        REQWEST_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaProvider;

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";

impl Provider for RheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::Rhea
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let Amount::AmountIn(exact_amount_in) = request.amount else {
                // smartrouter.ref.finance/findPath doesn't support AmountOut
                return None;
            };

            let slippage =
                get_slippage_f64(request.slippage, &request.token_in, &request.token_out).await;

            let token_in = match request.token_in {
                TokenId::Near => WRAP_NEAR.to_string(),
                TokenId::Nep141(ref account_id) => account_id.to_string(),
            };
            let token_out = match request.token_out {
                TokenId::Near => WRAP_NEAR.to_string(),
                TokenId::Nep141(ref account_id) => account_id.to_string(),
            };

            if token_in == token_out {
                return None;
            }

            let url = format!("https://smartrouter.ref.finance/findPath?tokenIn={token_in}&tokenOut={token_out}&pathDeep=3&slippage={slippage}&amountIn={exact_amount_in}");

            let Ok(response) = REQWEST_CLIENT.get(url).send().await else {
                return None;
            };

            let Ok(mut response) = response.json::<RheaSmartRouterResponse>().await else {
                return None;
            };

            info!("Found Rhea route: {:?}", response.result_data);

            let route = if !response.result_data.routes.is_empty() {
                let route = response.result_data.routes.remove(0);

                let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "ft_transfer_call".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "receiver_id": RHEA_CONTRACT_ID,
                        "amount": exact_amount_in.to_string(),
                        "msg": serde_json::to_string(&serde_json::json!({
                            "force": 0,
                            "actions": route
                                .pools
                                .iter()
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
                                .collect::<Vec<_>>(),
                            "skip_degen_price_sync": true,
                            "skip_unwrap_near": request.token_out != TokenId::Near,
                        })).unwrap(),
                    }))
                    .unwrap(),
                    gas: NearGas::from_tgas(90).as_gas(),
                    deposit: NearToken::from_yoctonear(1),
                }));

                let mut actions = vec![swap_action];
                if request.token_in == TokenId::Near {
                    actions.insert(
                        0,
                        create_wrap_action(NearToken::from_yoctonear(exact_amount_in)),
                    );
                    if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                        if needs_storage_deposit(
                            trader_account_id,
                            &TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                        )
                        .await
                        {
                            actions.insert(
                                0,
                                create_storage_deposit_action(&TokenId::Nep141(
                                    WRAP_NEAR.parse().unwrap(),
                                ))
                                .await,
                            );
                        }
                    }
                }
                let mut transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: match request.token_in {
                        TokenId::Near => WRAP_NEAR.parse().unwrap(),
                        TokenId::Nep141(account_id) => account_id,
                    },
                    actions,
                    continue_if_failed: false,
                }];
                if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                    if needs_storage_deposit(trader_account_id, &request.token_out).await {
                        transactions.insert(
                            0,
                            ExecutionInstruction::NearTransaction {
                                receiver_id: match request.token_out {
                                    TokenId::Near => unreachable!(),
                                    TokenId::Nep141(ref account_id) => account_id.clone(),
                                },
                                actions: vec![
                                    create_storage_deposit_action(&request.token_out).await,
                                ],
                                continue_if_failed: false,
                            },
                        );
                    }
                }
                let route = Route {
                    dex_id: DexId::Rhea,
                    deadline: None,
                    has_slippage: true,
                    estimated_amount: Amount::AmountOut(response.result_data.amount_out),
                    worst_case_amount: Amount::AmountOut(route.min_amount_out),
                    execution_instructions: transactions,
                    needs_unwrap: false,
                };
                Some(route)
            } else {
                None
            };

            route
        })
    }
}

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
    // #[allow(dead_code)] // serialized to JSON
    pools: Vec<serde_json::Value>,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
}
