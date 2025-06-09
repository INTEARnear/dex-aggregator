use std::{future::Future, pin::Pin};

use futures_util::TryFutureExt;
use near_min_api::{
    types::{Action, Balance, Finality, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use serde::Deserialize;

use crate::{
    shared_utils::{
        create_storage_deposit_action, create_wrap_action, get_slippage_f64, needs_storage_deposit,
        RPC_CLIENT, WRAP_NEAR,
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
            let token_in = match request.token_in {
                TokenId::Near => WRAP_NEAR.to_string(),
                TokenId::Nep141(ref token_id) => token_id.to_string(),
            };
            let token_out = match request.token_out {
                TokenId::Near => WRAP_NEAR.to_string(),
                TokenId::Nep141(ref token_id) => token_id.to_string(),
            };

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

                        let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": exact_amount_in.to_string(),
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec![pool_id],
                                        "output_token": token_out,
                                        "min_output_amount": min_amount_out.to_string(),
                                        "skip_unwrap_near": request.token_out != TokenId::Near,
                                    }
                                })).unwrap(),
                            }))
                            .unwrap(),
                            gas: NearGas::from_tgas(70).as_gas(),
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
                        let transactions = vec![ExecutionInstruction::NearTransaction {
                            receiver_id: match &request.token_in {
                                TokenId::Near => WRAP_NEAR.parse().unwrap(),
                                TokenId::Nep141(account_id) => account_id.clone(),
                            },
                            actions,
                            continue_if_failed: false,
                        }];
                        Some(Route {
                            dex_id: DexId::RheaDcl,
                            estimated_amount: Amount::AmountOut(quote.amount),
                            deadline: None,
                            has_slippage: true,
                            worst_case_amount: Amount::AmountOut(min_amount_out),
                            execution_instructions: transactions,
                            needs_unwrap: false,
                        })
                    } else {
                        None
                    }
                }
                Amount::AmountOut(_exact_amount_out) => {
                    // Not supported
                    None
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
