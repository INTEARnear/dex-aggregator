use std::{future::Future, pin::Pin};

use near_min_api::{
    types::{AccountId, Action, Balance, Finality, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use serde::Deserialize;

use crate::{
    shared_utils::{
        create_storage_deposit_action, create_storage_deposit_action_for_contract,
        create_wrap_action, get_slippage_f64, needs_storage_deposit,
        needs_storage_deposit_for_contract, RPC_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct VeaxProvider;

const VEAX_CONTRACT_ID: &str = "veax.near";

impl Provider for VeaxProvider {
    fn dex_id(&self) -> DexId {
        DexId::Veax
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let token_in = match request.token_in {
                TokenId::Near => WRAP_NEAR.parse::<AccountId>().unwrap(),
                TokenId::Nep141(ref token_id) => token_id.clone(),
            };
            let token_out = match request.token_out {
                TokenId::Near => WRAP_NEAR.parse::<AccountId>().unwrap(),
                TokenId::Nep141(ref token_id) => token_id.clone(),
            };

            let Ok(estimate) = RPC_CLIENT
                .call::<VeaxEstimateResponse>(
                    VEAX_CONTRACT_ID.parse().unwrap(),
                    "estimate_swap_exact",
                    serde_json::json!({
                        "is_exact_in": match request.amount {
                            Amount::AmountIn(_) => true,
                            Amount::AmountOut(_) => false,
                        },
                        "token_in": token_in,
                        "token_out": token_out,
                        "amount": match request.amount {
                            Amount::AmountIn(amount) => amount.to_string(),
                            Amount::AmountOut(amount) => amount.to_string(),
                        },
                        "slippage_tolerance_bp": 0, // will be calculated by us
                    }),
                    QueryFinality::Finality(Finality::DoomSlug),
                )
                .await
            else {
                return None;
            };
            let (swap_action, estimated_amount, worst_case_amount) = match request.amount {
                Amount::AmountIn(exact_amount_in) => {
                    let estimated_amount_out = estimate.result;
                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let min_amount_out =
                        (estimated_amount_out as f64 * (1.0 - slippage)).floor() as Balance;

                    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": VEAX_CONTRACT_ID,
                            "amount": exact_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!([
                                "Deposit",
                                {
                                    "SwapExactIn": {
                                        "token_in": token_in,
                                        "token_out": token_out,
                                        "amount": exact_amount_in.to_string(),
                                        "amount_limit": min_amount_out.to_string()
                                    }
                                },
                                {
                                    "Withdraw": [token_in, "0", null]
                                },
                                {
                                    "Withdraw": [token_out, "0", null]
                                }
                            ])).unwrap(),
                        }))
                        .unwrap(),
                        gas: NearGas::from_tgas(120).as_gas(),
                        deposit: NearToken::from_yoctonear(1),
                    }));
                    (
                        swap_action,
                        Amount::AmountOut(estimated_amount_out),
                        Amount::AmountOut(min_amount_out),
                    )
                }
                Amount::AmountOut(exact_amount_out) => {
                    let estimated_amount_in = estimate.result;
                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let max_amount_in =
                        (estimated_amount_in as f64 * (1.0 + slippage)).ceil() as Balance;
                    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": VEAX_CONTRACT_ID,
                            "amount": max_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!([
                                "Deposit",
                                {
                                    "SwapExactOut": {
                                        "token_in": token_in,
                                        "token_out": token_out,
                                        "amount": exact_amount_out.to_string(),
                                        "amount_limit": max_amount_in.to_string()
                                    }
                                },
                                {
                                    "Withdraw": [token_in, "0", null]
                                },
                                {
                                    "Withdraw": [token_out, "0", null]
                                }
                            ])).unwrap(),
                        }))
                        .unwrap(),
                        gas: NearGas::from_tgas(200).as_gas(),
                        deposit: NearToken::from_yoctonear(1),
                    }));
                    (
                        swap_action,
                        Amount::AmountIn(estimated_amount_in),
                        Amount::AmountIn(max_amount_in),
                    )
                }
            };

            let mut actions = vec![swap_action];
            if request.token_in == TokenId::Near {
                if let (Amount::AmountIn(near_deposit_amount), _)
                | (_, Amount::AmountIn(near_deposit_amount)) =
                    (request.amount, worst_case_amount)
                {
                    actions.insert(
                        0,
                        create_wrap_action(NearToken::from_yoctonear(near_deposit_amount)),
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
            }

            let mut transactions = vec![ExecutionInstruction::NearTransaction {
                receiver_id: match &request.token_in {
                    TokenId::Near => WRAP_NEAR.parse().unwrap(),
                    TokenId::Nep141(account_id) => account_id.clone(),
                },
                actions,
                continue_if_failed: false,
            }];
            if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                let mut veax_transaction = None;
                if needs_storage_deposit_for_contract(
                    trader_account_id,
                    &VEAX_CONTRACT_ID.parse::<AccountId>().unwrap(),
                )
                .await
                {
                    transactions.insert(
                        0,
                        ExecutionInstruction::NearTransaction {
                            receiver_id: VEAX_CONTRACT_ID.parse().unwrap(),
                            actions: vec![create_storage_deposit_action_for_contract(
                                "0.00366 NEAR".parse().unwrap(),
                            )],
                            continue_if_failed: false,
                        },
                    );
                    veax_transaction = Some(transactions.get_mut(0).unwrap());
                }
                if let Ok(user_tokens) = RPC_CLIENT
                    .call::<Vec<AccountId>>(
                        VEAX_CONTRACT_ID.parse().unwrap(),
                        "get_user_tokens",
                        serde_json::json!({
                            "account_id": trader_account_id,
                        }),
                        QueryFinality::Finality(Finality::DoomSlug),
                    )
                    .await
                {
                    let tokens_that_need_deposit = [token_in, token_out]
                        .into_iter()
                        .filter(|token| !user_tokens.contains(token))
                        .collect::<Vec<_>>();
                    if !tokens_that_need_deposit.is_empty() {
                        let veax_transaction = if let Some(veax_transaction) = veax_transaction {
                            veax_transaction
                        } else {
                            transactions.insert(
                                0,
                                ExecutionInstruction::NearTransaction {
                                    receiver_id: VEAX_CONTRACT_ID.parse().unwrap(),
                                    actions: vec![],
                                    continue_if_failed: false,
                                },
                            );
                            transactions.get_mut(0).unwrap()
                        };
                        match veax_transaction {
                            ExecutionInstruction::NearTransaction { actions, .. } => {
                                actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
                                    method_name: "storage_deposit".to_string(),
                                    args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                    gas: NearGas::from_tgas(10).as_gas(),
                                    deposit: "0.00284 NEAR"
                                        .parse::<NearToken>()
                                        .unwrap()
                                        .checked_mul(tokens_that_need_deposit.len() as u128)
                                        .unwrap(),
                                })));
                                actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
                                    method_name: "register_tokens".to_string(),
                                    args: serde_json::to_vec(&serde_json::json!({
                                        "token_ids": tokens_that_need_deposit,
                                    }))
                                    .unwrap(),
                                    gas: NearGas::from_tgas(10).as_gas(),
                                    deposit: NearToken::from_yoctonear(1),
                                })));
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            Some(Route {
                dex_id: DexId::Veax,
                estimated_amount,
                deadline: None,
                has_slippage: true,
                worst_case_amount,
                execution_instructions: transactions,
                needs_unwrap: request.token_out == TokenId::Near
                    || request.token_in == TokenId::Near,
            })
        })
    }
}

#[derive(Debug, Deserialize)]
struct VeaxEstimateResponse {
    #[serde(with = "dec_format")]
    result: Balance,
}
