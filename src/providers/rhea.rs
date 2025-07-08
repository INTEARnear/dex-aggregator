use std::{future::Future, pin::Pin};

use near_min_api::{
    types::{Action, Balance, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
};
use serde::Deserialize;
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, get_slippage_f64, REQWEST_CLIENT,
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

            let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
            let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

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

            if response.result_data.routes.is_empty() {
                return None;
            }

            let route = response.result_data.routes.remove(0);

            let unwrapping_near = request.token_out == TokenId::Near;
            let ft_transfer_call_swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
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
                        "skip_unwrap_near": !unwrapping_near,
                    })).unwrap(),
                }))
                .unwrap(),
                gas: NearGas::from_tgas(150).as_gas(),
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
                    &if unwrapping_near {
                        TokenId::Near
                    } else {
                        TokenId::Nep141(token_out.clone())
                    },
                    request.trader_account_id.clone(),
                )
                .await,
                deposit_storage_if_needed(
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
                estimated_amount: Amount::AmountOut(response.result_data.amount_out),
                worst_case_amount: Amount::AmountOut(route.min_amount_out),
                execution_instructions: transactions,
                has_leftover_after_slippage_that_needs_unwrapping: false,
                token_output: if unwrapping_near {
                    TokenId::Near
                } else {
                    TokenId::Nep141(token_out)
                },
            })
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
