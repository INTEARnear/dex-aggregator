use std::{future::Future, pin::Pin};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{Action, Finality, FunctionCallAction, Gas, NearGas, NearToken, U128},
    QueryFinality,
};
use num_traits::ToPrimitive;

use crate::{
    shared_utils::{convert_to_nep141, deposit_storage_if_needed, RPC_CLIENT},
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const RHEA_CONTRACT: &str = "token.rhealab.near";
const XRHEA_CONTRACT: &str = "xtoken.rhealab.near";

pub struct XRheaProvider;

impl Provider for XRheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::XRhea
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let (_input_to_nep141, nep141_in) =
                convert_to_nep141(&request.token_in, None, 0).await?;

            if nep141_in == RHEA_CONTRACT {
                let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
                if nep141_out != XRHEA_CONTRACT {
                    return None;
                }

                // RHEA -> XRHEA

                let Ok(virtual_price) = RPC_CLIENT
                    .call::<U128>(
                        XRHEA_CONTRACT.parse().unwrap(),
                        "get_virtual_price",
                        serde_json::json!({}),
                        QueryFinality::Finality(Finality::Final),
                    )
                    .await
                else {
                    return None;
                };
                let virtual_price =
                    BigDecimal::from(*virtual_price) / BigDecimal::from(10u128.pow(8));

                let amount_rhea_in = match request.amount {
                    Amount::AmountIn(amount) => amount,
                    Amount::AmountOut(amount) => {
                        ToPrimitive::to_u128(&(BigDecimal::from(amount) * &virtual_price))?
                    }
                };
                let amount_xrhea_out = match request.amount {
                    Amount::AmountIn(amount) => {
                        ToPrimitive::to_u128(&(BigDecimal::from(amount) / &virtual_price))?
                    }
                    Amount::AmountOut(amount) => amount,
                };

                let (input_to_nep141, _nep141_in) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    amount_rhea_in,
                )
                .await?;

                let stake_instructions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: RHEA_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": XRHEA_CONTRACT,
                            "amount": amount_rhea_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!({
                                "Stake": {}
                            }))
                            .unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                }];

                let execution_instructions = [
                    deposit_storage_if_needed(
                        &TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
                        request.trader_account_id,
                    )
                    .await,
                    input_to_nep141,
                    stake_instructions,
                ]
                .concat();

                return Some(Route {
                    deadline: None,
                    has_slippage: false, // TODO add slippage, the price of XRHEA is not stable even between epochs, unlike metapool or linear
                    estimated_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_xrhea_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_rhea_in),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_xrhea_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_rhea_in),
                    },
                    dex_id: DexId::XRhea,
                    execution_instructions,
                    deprecated_needs_unwrap_always_false: false,
                    token_output: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
                });
            } else if nep141_in == XRHEA_CONTRACT {
                let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
                if nep141_out != RHEA_CONTRACT {
                    return None;
                }

                // XRHEA -> RHEA

                let Ok(virtual_price) = RPC_CLIENT
                    .call::<U128>(
                        XRHEA_CONTRACT.parse().unwrap(),
                        "get_virtual_price",
                        serde_json::json!({}),
                        QueryFinality::Finality(Finality::Final),
                    )
                    .await
                else {
                    return None;
                };
                let virtual_price =
                    BigDecimal::from(*virtual_price) / BigDecimal::from(10u128.pow(8));

                let amount_xrhea_in = match request.amount {
                    Amount::AmountIn(amount_in) => amount_in,
                    Amount::AmountOut(amount_out) => {
                        ToPrimitive::to_u128(&(BigDecimal::from(amount_out) / &virtual_price))?
                    }
                };
                let amount_rhea_out = match request.amount {
                    Amount::AmountIn(amount_in) => {
                        ToPrimitive::to_u128(&(BigDecimal::from(amount_in) * &virtual_price))?
                    }
                    Amount::AmountOut(amount_out) => amount_out,
                };

                let (input_to_nep141, _nep141_in) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    amount_xrhea_in,
                )
                .await?;

                let unstake_instructions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "unstake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "amount": amount_xrhea_in.to_string(),
                            "msg": ""
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                }];

                let execution_instructions = [
                    deposit_storage_if_needed(
                        &TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                        request.trader_account_id,
                    )
                    .await,
                    input_to_nep141,
                    unstake_instructions,
                ]
                .concat();

                return Some(Route {
                    deadline: None,
                    has_slippage: false,
                    estimated_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_rhea_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_xrhea_in),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_rhea_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_xrhea_in),
                    },
                    dex_id: DexId::XRhea,
                    execution_instructions,
                    deprecated_needs_unwrap_always_false: false,
                    token_output: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                });
            }

            None
        })
    }
}
