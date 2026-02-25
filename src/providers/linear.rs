use std::{future::Future, pin::Pin};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{Action, Finality, FunctionCallAction, Gas, NearGas, NearToken},
    QueryFinality,
};
use num_traits::ToPrimitive;
use serde::Deserialize;

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, is_near, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const LINEAR_CONTRACT: &str = "linear-protocol.near";

pub struct LinearProvider;

#[derive(Debug, Deserialize)]
struct LinearContractState {
    ft_price: NearToken,
}

impl Provider for LinearProvider {
    fn dex_id(&self) -> DexId {
        DexId::Linear
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            if is_near(&request.token_in) {
                let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
                if nep141_out != LINEAR_CONTRACT {
                    return None;
                }

                // NEAR -> LiNEAR

                let Ok(state) = RPC_CLIENT
                    .call::<LinearContractState>(
                        LINEAR_CONTRACT.parse().unwrap(),
                        "get_summary",
                        serde_json::json!({}),
                        QueryFinality::Finality(Finality::Final),
                    )
                    .await
                else {
                    return None;
                };

                let amount_near_in = match request.amount {
                    Amount::AmountIn(amount) => amount,
                    Amount::AmountOut(amount) => {
                        let amount_near_in = BigDecimal::from(amount)
                            / BigDecimal::from(10u128.pow(24))
                            * BigDecimal::from(state.ft_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_near_in)?
                    }
                };
                let amount_linear_out = match request.amount {
                    Amount::AmountIn(amount) => {
                        let amount_linear_out = BigDecimal::from(amount)
                            * BigDecimal::from(10u128.pow(24))
                            / BigDecimal::from(state.ft_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_linear_out)?.saturating_sub(1)
                    }
                    Amount::AmountOut(amount) => amount,
                };

                let input_to_native = convert_to_native(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    NearToken::from_yoctonear(amount_near_in),
                )
                .await?;

                let stake_instructions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_and_stake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(amount_near_in),
                    }))],
                }];

                let execution_instructions = [
                    input_to_native,
                    deposit_storage_if_needed(
                        &TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
                        request.trader_account_id,
                    )
                    .await,
                    stake_instructions,
                ]
                .concat();

                return Some(Route {
                    deadline: None,
                    has_slippage: false,
                    estimated_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_linear_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_linear_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
                    },
                    dex_id: DexId::Linear,
                    execution_instructions,
                    deprecated_needs_unwrap_always_false: false,
                    token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
                });
            }
            // No liquid unstake available, app.linearprotocol.org just uses Rhea pools

            None
        })
    }
}
