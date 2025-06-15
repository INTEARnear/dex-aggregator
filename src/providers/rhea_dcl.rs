use std::{future::Future, pin::Pin};

use futures_util::TryFutureExt;
use near_min_api::{
    types::{Action, Balance, Finality, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use serde::Deserialize;

use crate::{
    shared_utils::{convert_to_nep141, deposit_storage_if_needed, get_slippage_f64, RPC_CLIENT},
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaDclProvider;

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";

impl Provider for RheaDclProvider {
    fn dex_id(&self) -> DexId {
        DexId::RheaDcl
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<(Route, TokenId)>> + Send>> {
        Box::pin(async move {
            let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
            let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

            if token_in == token_out {
                return None;
            }

            let (token_x, token_y) = if token_in < token_out {
                (token_in.clone(), token_out.clone())
            } else {
                (token_out.clone(), token_in.clone())
            };

            const FEE_TIERS: [u64; 4] = [100, 400, 2000, 10000]; // 0.01%, 0.04%, 0.2%, 1%

            match request.amount {
                Amount::AmountIn(exact_amount_in) => {
                    let futures = FEE_TIERS.into_iter().map(|fee| {
                        let pool_id = format!("{token_x}|{token_y}|{fee}");
                        RPC_CLIENT
                            .call::<RheaDclQuoteResponse>(
                                RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                                "quote",
                                serde_json::json!({
                                    "pool_ids": vec![pool_id.clone()],
                                    "input_token": token_in,
                                    "output_token": token_out,
                                    "input_amount": exact_amount_in.to_string(),
                                }),
                                QueryFinality::Finality(Finality::DoomSlug),
                            )
                            .map_ok(|v| (pool_id, v))
                    });
                    let routes = futures_util::future::join_all(futures)
                        .await
                        .into_iter()
                        .filter_map(|f| f.ok())
                        .filter(|(_, v)| v.amount > 0)
                        .collect::<Vec<_>>();
                    let best_route = routes.iter().max_by_key(|(_, v)| v.amount);
                    if let Some((pool_id, quote)) = best_route {
                        let min_amount_out = (quote.amount as f64
                            * (1.0
                                - get_slippage_f64(
                                    request.slippage,
                                    &request.token_in,
                                    &request.token_out,
                                )
                                .await))
                            .floor() as Balance;

                        let unwrapping_near = request.token_out == TokenId::Near;
                        let ft_transfer_call_swap_action =
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "ft_transfer_call".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "receiver_id": RHEA_DCL_CONTRACT_ID,
                                    "amount": exact_amount_in.to_string(),
                                    "msg": serde_json::to_string(&serde_json::json!({
                                        "Swap": {
                                            "pool_ids": vec![pool_id],
                                            "output_token": token_out,
                                            "min_output_amount": min_amount_out.to_string(),
                                            "skip_unwrap_near": !unwrapping_near,
                                        }
                                    })).unwrap(),
                                }))
                                .unwrap(),
                                gas: NearGas::from_tgas(70).as_gas(),
                                deposit: NearToken::from_yoctonear(1),
                            }));

                        let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                            receiver_id: token_in,
                            actions: vec![ft_transfer_call_swap_action],
                        }];
                        let transactions = [
                            deposit_storage_if_needed(
                                &request.token_out,
                                request.trader_account_id.clone(),
                            )
                            .await,
                            deposit_storage_if_needed(
                                &request.token_in,
                                request.trader_account_id.clone(),
                            )
                            .await,
                            convert_to_nep141(
                                &request.token_in,
                                request.trader_account_id.clone(),
                                exact_amount_in,
                            )
                            .await?
                            .0,
                            swap_transactions,
                        ]
                        .concat();
                        Some((
                            Route {
                                dex_id: DexId::RheaDcl,
                                estimated_amount: Amount::AmountOut(quote.amount),
                                deadline: None,
                                has_slippage: true,
                                worst_case_amount: Amount::AmountOut(min_amount_out),
                                execution_instructions: transactions,
                                needs_unwrap: false,
                            },
                            if unwrapping_near {
                                TokenId::Near
                            } else {
                                TokenId::Nep141(token_out)
                            },
                        ))
                    } else {
                        None
                    }
                }
                Amount::AmountOut(exact_amount_out) => {
                    let futures = FEE_TIERS.into_iter().map(|fee| {
                        let pool_id = format!("{token_x}|{token_y}|{fee}");
                        RPC_CLIENT
                            .call::<RheaDclQuoteResponse>(
                                RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                                "quote_by_output",
                                serde_json::json!({
                                    "pool_ids": vec![pool_id.clone()],
                                    "input_token": token_in,
                                    "output_token": token_out,
                                    "output_amount": exact_amount_out.to_string(),
                                }),
                                QueryFinality::Finality(Finality::DoomSlug),
                            )
                            .map_ok(|v| (pool_id, v))
                    });
                    let routes = futures_util::future::join_all(futures)
                        .await
                        .into_iter()
                        .filter_map(|f| f.ok())
                        .filter(|(_, v)| v.amount > 0)
                        .collect::<Vec<_>>();
                    let best_route = routes.iter().min_by_key(|(_, v)| v.amount);
                    if let Some((pool_id, quote)) = best_route {
                        let max_amount_in = (quote.amount as f64
                            / (1.0
                                - get_slippage_f64(
                                    request.slippage,
                                    &request.token_in,
                                    &request.token_out,
                                )
                                .await))
                            .floor() as Balance;

                        let unwrapping_near = request.token_out == TokenId::Near;
                        let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": max_amount_in.to_string(),
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "SwapByOutput": {
                                        "pool_ids": vec![pool_id],
                                        "output_token": token_out,
                                        "output_amount": exact_amount_out.to_string(),
                                        "skip_unwrap_near": !unwrapping_near,
                                    }
                                })).unwrap(),
                            }))
                            .unwrap(),
                            gas: NearGas::from_tgas(70).as_gas(),
                            deposit: NearToken::from_yoctonear(1),
                        }));

                        let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                            receiver_id: token_in,
                            actions: vec![swap_action],
                        }];
                        let transactions = [
                            deposit_storage_if_needed(
                                &request.token_out,
                                request.trader_account_id.clone(),
                            )
                            .await,
                            deposit_storage_if_needed(
                                &request.token_in,
                                request.trader_account_id.clone(),
                            )
                            .await,
                            convert_to_nep141(
                                &request.token_in,
                                request.trader_account_id.clone(),
                                max_amount_in,
                            )
                            .await?
                            .0,
                            swap_transactions,
                        ]
                        .concat();
                        Some((
                            Route {
                                dex_id: DexId::RheaDcl,
                                estimated_amount: Amount::AmountIn(quote.amount),
                                deadline: None,
                                has_slippage: true,
                                worst_case_amount: Amount::AmountIn(max_amount_in),
                                execution_instructions: transactions,
                                needs_unwrap: false,
                            },
                            if unwrapping_near {
                                TokenId::Near
                            } else {
                                TokenId::Nep141(token_out)
                            },
                        ))
                    } else {
                        None
                    }
                }
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct RheaDclQuoteResponse {
    #[serde(with = "dec_format")]
    amount: Balance,
}
