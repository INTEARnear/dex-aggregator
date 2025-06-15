use std::{future::Future, pin::Pin};

use near_min_api::types::NearToken;

use crate::{
    shared_utils::{
        create_unwrap_action, create_wrap_action, deposit_storage_if_needed, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct WrapProvider;

impl Provider for WrapProvider {
    fn dex_id(&self) -> DexId {
        DexId::Wrap
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<(Route, TokenId)>> + Send>> {
        Box::pin(async move {
            match (request.token_in, request.token_out) {
                (TokenId::Near, TokenId::Nep141(token)) if token == WRAP_NEAR => {
                    // Wrap NEAR to wNEAR
                    Some((
                        Route {
                            deadline: None,
                            has_slippage: false,
                            estimated_amount: match request.amount {
                                Amount::AmountIn(amount) => Amount::AmountOut(amount),
                                Amount::AmountOut(amount) => Amount::AmountIn(amount),
                            },
                            worst_case_amount: match request.amount {
                                Amount::AmountIn(amount) => Amount::AmountOut(amount),
                                Amount::AmountOut(amount) => Amount::AmountIn(amount),
                            },
                            dex_id: DexId::Wrap,
                            execution_instructions: [
                                deposit_storage_if_needed(
                                    &TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                                    request.trader_account_id.clone(),
                                )
                                .await,
                                vec![ExecutionInstruction::NearTransaction {
                                    receiver_id: WRAP_NEAR.parse().unwrap(),
                                    actions: vec![create_wrap_action(NearToken::from_yoctonear(
                                        match request.amount {
                                            Amount::AmountIn(amount) => amount,
                                            Amount::AmountOut(amount) => amount,
                                        },
                                    ))],
                                }],
                            ]
                            .concat(),
                            needs_unwrap: false,
                        },
                        TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    ))
                }
                (TokenId::Nep141(token), TokenId::Near) if token == WRAP_NEAR => {
                    // Unwrap wNEAR to NEAR
                    Some((
                        Route {
                            deadline: None,
                            has_slippage: false,
                            estimated_amount: match request.amount {
                                Amount::AmountIn(amount) => Amount::AmountOut(amount),
                                Amount::AmountOut(amount) => Amount::AmountIn(amount),
                            },
                            worst_case_amount: match request.amount {
                                Amount::AmountIn(amount) => Amount::AmountOut(amount),
                                Amount::AmountOut(amount) => Amount::AmountIn(amount),
                            },
                            dex_id: DexId::Wrap,
                            execution_instructions: [vec![ExecutionInstruction::NearTransaction {
                                receiver_id: WRAP_NEAR.parse().unwrap(),
                                actions: vec![create_unwrap_action(NearToken::from_yoctonear(
                                    match request.amount {
                                        Amount::AmountIn(amount) => amount,
                                        Amount::AmountOut(amount) => amount,
                                    },
                                ))],
                            }]]
                            .concat(),
                            needs_unwrap: false,
                        },
                        TokenId::Near,
                    ))
                }
                _ => None,
            }
        })
    }
}
