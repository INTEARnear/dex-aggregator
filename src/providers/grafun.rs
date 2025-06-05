use std::{future::Future, pin::Pin};

use near_min_api::{
    types::{AccountId, Action, Finality, FunctionCallAction, NearGas, NearToken, U128},
    QueryFinality,
};
use tracing::info;

use crate::{
    shared_utils::{
        create_storage_deposit_action, create_wrap_action, get_slippage_f64, needs_storage_deposit,
        RPC_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct GraFunProvider;

const GRAFUN_CONTRACT_ID: &str = "gra-fun.near";

impl Provider for GraFunProvider {
    fn dex_id(&self) -> DexId {
        DexId::GraFun
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let (is_buy, needs_to_wrap) = if request.token_in == TokenId::Near {
                (true, true)
            } else if request.token_in == TokenId::Nep141(WRAP_NEAR.parse().unwrap()) {
                (true, false)
            } else if request.token_out == TokenId::Near {
                (false, true)
            } else if request.token_out == TokenId::Nep141(WRAP_NEAR.parse().unwrap()) {
                (false, false)
            } else {
                return None;
            };

            let other_token = if is_buy {
                match request.token_out {
                    TokenId::Nep141(ref account_id) => account_id.clone(),
                    _ => return None,
                }
            } else {
                match request.token_in {
                    TokenId::Nep141(ref account_id) => account_id.clone(),
                    _ => return None,
                }
            };

            if !other_token.is_sub_account_of(&GRAFUN_CONTRACT_ID.parse::<AccountId>().unwrap()) {
                return None;
            }

            match request.amount {
                Amount::AmountIn(exact_amount_in) => {
                    let Ok((estimated_amount_out, _fee, _is_deployed, is_tradable)): Result<
                        (U128, U128, bool, bool),
                        _,
                    > = RPC_CLIENT
                        .call(
                            GRAFUN_CONTRACT_ID.parse().unwrap(),
                            "emulate_swap",
                            serde_json::json!({
                                "input_token": match request.token_in {
                                    TokenId::Near => WRAP_NEAR.to_string(),
                                    TokenId::Nep141(ref account_id) => account_id.to_string(),
                                },
                                "output_token": match request.token_out {
                                    TokenId::Near => WRAP_NEAR.to_string(),
                                    TokenId::Nep141(ref account_id) => account_id.to_string(),
                                },
                                "amount": exact_amount_in.to_string(),
                            }),
                            QueryFinality::Finality(Finality::DoomSlug),
                        )
                        .await
                    else {
                        return None;
                    };
                    let estimated_amount_out = *estimated_amount_out;

                    if !is_tradable {
                        return None;
                    }

                    info!("Estimated amount out: {}", estimated_amount_out);

                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let min_amount_out = estimated_amount_out as f64 * (1.0 - slippage);
                    let min_amount_out = min_amount_out as u128;

                    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_string(&serde_json::json!({
                            "receiver_id": GRAFUN_CONTRACT_ID,
                            "amount": exact_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!({
                                "token": if is_buy {
                                    Some(other_token.to_string())
                                } else {
                                    None
                                },
                                "min_swap_amount": min_amount_out.to_string(),
                            })).unwrap(),
                        }))
                        .unwrap()
                        .as_bytes()
                        .to_vec(),
                        gas: NearGas::from_tgas(50).as_gas(),
                        deposit: NearToken::from_yoctonear(1),
                    }));

                    let mut actions = vec![swap_action];
                    if needs_to_wrap && is_buy {
                        // Wrap NEAR -> wNEAR before buying a token
                        actions.insert(
                            0,
                            create_wrap_action(NearToken::from_yoctonear(exact_amount_in)),
                        );
                        if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                            if needs_storage_deposit(
                                trader_account_id,
                                &TokenId::Nep141(WRAP_NEAR.parse::<AccountId>().unwrap()),
                            )
                            .await
                            {
                                actions.insert(
                                    0,
                                    create_storage_deposit_action(TokenId::Nep141(
                                        WRAP_NEAR.parse::<AccountId>().unwrap(),
                                    ))
                                    .await,
                                );
                            }
                        }
                    }
                    let mut transactions = vec![ExecutionInstruction::NearTransaction {
                        receiver_id: if is_buy {
                            WRAP_NEAR.parse().unwrap()
                        } else {
                            other_token
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
                                        create_storage_deposit_action(request.token_out).await,
                                    ],
                                    continue_if_failed: false,
                                },
                            );
                        }
                    }
                    let route = Route {
                        dex_id: DexId::GraFun,
                        deadline: None,
                        has_slippage: true,
                        estimated_amount: Amount::AmountOut(estimated_amount_out),
                        worst_case_amount: Amount::AmountOut(min_amount_out),
                        execution_instructions: transactions,
                        needs_unwrap: needs_to_wrap && !is_buy,
                    };

                    Some(route)
                }
                Amount::AmountOut(_exact_amount_out) => {
                    None // doesn't support AmountOut
                }
            }
        })
    }
}
