use std::{future::Future, pin::Pin};

use near_min_api::{
    types::{AccountId, Action, Finality, FunctionCallAction, Gas, NearGas, NearToken, U128},
    QueryFinality,
};
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, get_slippage_f64, DEFAULT_REFERRER_ID,
        RPC_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct AidolsProvider;

const AIDOLS_CONTRACT_ID: &str = "aidols.near";

impl Provider for AidolsProvider {
    fn dex_id(&self) -> DexId {
        DexId::Aidols
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let (_, nep141_in) = convert_to_nep141(&request.token_in, None, 0).await?;
            let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
            let is_buy = nep141_in == WRAP_NEAR;

            let aidol_token = if is_buy {
                nep141_out.clone()
            } else {
                nep141_in.clone()
            };

            if !aidol_token.is_sub_account_of(AIDOLS_CONTRACT_ID.parse::<AccountId>().unwrap()) {
                return None;
            }

            match request.amount {
                Amount::AmountIn(exact_amount_in) => {
                    let Ok((estimated_amount_out, _fee, _is_deployed, is_tradable)): Result<
                        (U128, U128, bool, bool),
                        _,
                    > = RPC_CLIENT
                        .call(
                            AIDOLS_CONTRACT_ID.parse().unwrap(),
                            "emulate_swap",
                            serde_json::json!({
                                "input_token": nep141_in,
                                "output_token": nep141_out,
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
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": AIDOLS_CONTRACT_ID,
                            "amount": exact_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!({
                                "token": if is_buy {
                                    Some(aidol_token.to_string())
                                } else {
                                    None
                                },
                                "min_swap_amount": min_amount_out.to_string(),
                                "referral": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                            })).unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }));

                    let transactions = [
                        deposit_storage_if_needed(
                            &TokenId::Nep141(nep141_out.clone()),
                            request.trader_account_id.clone(),
                        )
                        .await,
                        deposit_storage_if_needed(
                            &TokenId::Nep141(nep141_in.clone()),
                            request.trader_account_id.clone(),
                        )
                        .await,
                        convert_to_nep141(&request.token_in, None, exact_amount_in)
                            .await?
                            .0,
                        vec![ExecutionInstruction::NearTransaction {
                            receiver_id: nep141_in,
                            actions: vec![swap_action],
                        }],
                    ]
                    .concat();
                    let route = Route {
                        dex_id: DexId::Aidols,
                        deadline: None,
                        has_slippage: true,
                        estimated_amount: Amount::AmountOut(estimated_amount_out),
                        worst_case_amount: Amount::AmountOut(min_amount_out),
                        execution_instructions: transactions,
                        token_output: TokenId::Nep141(nep141_out.clone()),
                        deprecated_needs_unwrap_always_false: false,
                    };

                    Some(route)
                }
                Amount::AmountOut(exact_amount_out) => {
                    let Ok((required_amount_in, _fee, _is_deployed, is_tradable)): Result<
                        (U128, U128, bool, bool),
                        _,
                    > = RPC_CLIENT
                        .call(
                            AIDOLS_CONTRACT_ID.parse().unwrap(),
                            "emulate_swap_by_out",
                            serde_json::json!({
                                "input_token": nep141_in,
                                "output_token": nep141_out,
                                "amount_out": exact_amount_out.to_string(),
                            }),
                            QueryFinality::Finality(Finality::DoomSlug),
                        )
                        .await
                    else {
                        return None;
                    };
                    let required_amount_in = *required_amount_in;

                    if !is_tradable {
                        return None;
                    }

                    info!("Required amount in: {}", required_amount_in);

                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let max_amount_in = required_amount_in as f64 / (1.0 - slippage);
                    let max_amount_in = max_amount_in as u128;

                    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": AIDOLS_CONTRACT_ID,
                            "amount": max_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!({
                                "token": if is_buy {
                                    Some(aidol_token.to_string())
                                } else {
                                    None
                                },
                                "amount_out": exact_amount_out.to_string(),
                                "min_swap_amount": u128::MAX.to_string(), // not used but required
                                "referral": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                            })).unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }));

                    let transactions = [
                        deposit_storage_if_needed(
                            &TokenId::Nep141(nep141_out.clone()),
                            request.trader_account_id.clone(),
                        )
                        .await,
                        deposit_storage_if_needed(
                            &TokenId::Nep141(nep141_in.clone()),
                            request.trader_account_id.clone(),
                        )
                        .await,
                        convert_to_nep141(&request.token_in, None, 0).await?.0,
                        vec![ExecutionInstruction::NearTransaction {
                            receiver_id: nep141_in,
                            actions: vec![swap_action],
                        }],
                    ]
                    .concat();
                    let route = Route {
                        dex_id: DexId::Aidols,
                        deadline: None,
                        has_slippage: true,
                        estimated_amount: Amount::AmountIn(required_amount_in),
                        worst_case_amount: Amount::AmountIn(max_amount_in),
                        execution_instructions: transactions,
                        deprecated_needs_unwrap_always_false: false,
                        token_output: TokenId::Nep141(nep141_out.clone()),
                    };

                    Some(route)
                }
            }
        })
    }
}
