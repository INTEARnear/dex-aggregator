use std::{future::Future, pin::Pin};

use futures_util::TryFutureExt;
use near_min_api::{
    types::{AccountId, Action, Balance, Finality, FunctionCallAction, Gas, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use serde::Deserialize;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, deposit_storage_on_contract_if_needed,
        get_slippage_f64, needs_storage_deposit_for_contract, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaDclProvider;

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";

impl Provider for RheaDclProvider {
    fn dex_id(&self) -> DexId {
        DexId::RheaDcl
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
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
                                gas: Gas(NearGas::from_tgas(100)),
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
                        if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                            if needs_storage_deposit_for_contract(
                                trader_account_id,
                                &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                            )
                            .await
                            {
                                if let Ok(account) = RPC_CLIENT
                                    .view_account(
                                        trader_account_id.clone(),
                                        QueryFinality::Finality(Finality::None),
                                    )
                                    .await
                                {
                                    // Don't use Rhea DCL for accounts with less than 1 NEAR, since
                                    // the storage deposit of 0.5 NEAR is usually too high for them.
                                    if account.amount < NearToken::from_near(1) {
                                        return None;
                                    }
                                }
                            }
                        }
                        let transactions = [
                            deposit_storage_on_contract_if_needed(
                                &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                                request.trader_account_id.clone(),
                                NearToken::from_millinear(500),
                            )
                            .await,
                            deposit_storage_if_needed(
                                &request.token_out,
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
                            dex_id: DexId::RheaDcl,
                            estimated_amount: Amount::AmountOut(quote.amount),
                            deadline: None,
                            has_slippage: true,
                            worst_case_amount: Amount::AmountOut(min_amount_out),
                            execution_instructions: transactions,
                            deprecated_needs_unwrap_always_false: false,
                            token_output: if unwrapping_near {
                                TokenId::Near
                            } else {
                                TokenId::Nep141(token_out)
                            },
                        })
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
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }));

                        let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                            receiver_id: token_in,
                            actions: vec![swap_action],
                        }];
                        let (input_to_nep141, input_nep141) = convert_to_nep141(
                            &request.token_in,
                            request.trader_account_id.clone(),
                            max_amount_in,
                        )
                        .await?;
                        let transactions = [
                            deposit_storage_on_contract_if_needed(
                                &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                                request.trader_account_id.clone(),
                                NearToken::from_millinear(500),
                            )
                            .await,
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
                            dex_id: DexId::RheaDcl,
                            estimated_amount: Amount::AmountIn(quote.amount),
                            deadline: None,
                            has_slippage: true,
                            worst_case_amount: Amount::AmountIn(max_amount_in),
                            execution_instructions: transactions,
                            deprecated_needs_unwrap_always_false: false,
                            token_output: if unwrapping_near {
                                TokenId::Near
                            } else {
                                TokenId::Nep141(token_out)
                            },
                        })
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
