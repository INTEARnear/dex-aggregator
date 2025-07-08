use std::{future::Future, pin::Pin};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{Action, Finality, FunctionCallAction, NearGas, NearToken},
    QueryFinality,
};
use num_traits::{FromPrimitive, ToPrimitive};
use serde::Deserialize;
use tracing::error;

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage_f64, is_near,
        RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const METAPOOL_CONTRACT: &str = "meta-pool.near";
const MIN_DEPOSIT_AMOUNT_NEAR: NearToken = NearToken::from_near(1);
const MIN_LIQUID_UNSTAKE_AMOUNT_STNEAR: NearToken = NearToken::from_near(1);

pub struct MetapoolProvider;

#[derive(Debug, Deserialize)]
struct MetapoolContractState {
    st_near_price: NearToken,
    nslp_liquidity: NearToken,
}

impl Provider for MetapoolProvider {
    fn dex_id(&self) -> DexId {
        DexId::MetaPool
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            if let Some(input_to_native) =
                convert_to_native(&request.token_in, None, NearToken::from_yoctonear(0)).await
            {
                let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
                if nep141_out != METAPOOL_CONTRACT {
                    return None;
                }

                // NEAR -> STNEAR

                let Ok(state) = RPC_CLIENT
                    .call::<MetapoolContractState>(
                        METAPOOL_CONTRACT.parse().unwrap(),
                        "get_contract_state",
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
                            * BigDecimal::from(state.st_near_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_near_in)?
                    }
                };
                if amount_near_in < MIN_DEPOSIT_AMOUNT_NEAR.as_yoctonear() {
                    return None;
                }
                let amount_stnear_out = match request.amount {
                    Amount::AmountIn(amount) => {
                        let amount_stnear_out = BigDecimal::from(amount)
                            * BigDecimal::from(10u128.pow(24))
                            / BigDecimal::from(state.st_near_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_stnear_out)?
                    }
                    Amount::AmountOut(amount) => amount,
                };

                let stake_instructions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_and_stake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: NearGas::from_tgas(10).as_gas(),
                        deposit: NearToken::from_yoctonear(amount_near_in),
                    }))],
                }];

                let execution_instructions = [
                    deposit_storage_if_needed(
                        &TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                        request.trader_account_id,
                    )
                    .await,
                    input_to_native,
                    stake_instructions,
                ]
                .concat();

                return Some(Route {
                    deadline: None,
                    has_slippage: false,
                    estimated_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_stnear_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_stnear_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
                    },
                    dex_id: DexId::MetaPool,
                    execution_instructions,
                    has_leftover_after_slippage_that_needs_unwrapping: false,
                    token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                });
            } else if is_near(&request.token_out) {
                let (input_to_nep141, nep141_in) =
                    convert_to_nep141(&request.token_in, None, 0).await?;
                if nep141_in != METAPOOL_CONTRACT {
                    return None;
                }

                // STNEAR -> NEAR

                let Ok(state) = RPC_CLIENT
                    .call::<MetapoolContractState>(
                        METAPOOL_CONTRACT.parse().unwrap(),
                        "get_contract_state",
                        serde_json::json!({}),
                        QueryFinality::Finality(Finality::Final),
                    )
                    .await
                else {
                    return None;
                };

                let amount_stnear_in = match request.amount {
                    Amount::AmountIn(amount_in) => amount_in,
                    Amount::AmountOut(amount_out) => {
                        let amount_stnear_in = BigDecimal::from(amount_out)
                            * BigDecimal::from(10u128.pow(24))
                            / BigDecimal::from(state.st_near_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_stnear_in)?
                    }
                };
                if amount_stnear_in < MIN_LIQUID_UNSTAKE_AMOUNT_STNEAR.as_yoctonear() {
                    return None;
                }
                if amount_stnear_in > state.nslp_liquidity.as_yoctonear() {
                    return None;
                }
                let amount_near_out = match request.amount {
                    Amount::AmountIn(amount_in) => {
                        let amount_near_out = BigDecimal::from(amount_in)
                            / BigDecimal::from(10u128.pow(24))
                            * BigDecimal::from(state.st_near_price.as_yoctonear());
                        ToPrimitive::to_u128(&amount_near_out)?
                    }
                    Amount::AmountOut(amount_out) => amount_out,
                };

                let fee = match RPC_CLIENT
                    .call::<u16>(
                        METAPOOL_CONTRACT.parse().unwrap(),
                        "nslp_get_discount_basis_points",
                        serde_json::json!({
                            "stnear_to_sell": amount_stnear_in.to_string(),
                        }),
                        QueryFinality::Finality(Finality::Final),
                    )
                    .await
                {
                    Ok(discount_basis_points) => discount_basis_points as f64 / 10000.0,
                    Err(_) => {
                        error!(
                            "Failed to get discount basis points for liquid unstake for amount {}",
                            amount_stnear_in
                        );
                        return None;
                    }
                };
                let amount_stnear_in = if let Amount::AmountOut(_) = request.amount {
                    ToPrimitive::to_u128(
                        &(BigDecimal::from(amount_stnear_in)
                            / BigDecimal::from_f64(1.0 - fee).unwrap()),
                    )
                    .unwrap()
                } else {
                    amount_stnear_in
                };
                let amount_near_out = if let Amount::AmountIn(_) = request.amount {
                    ToPrimitive::to_u128(
                        &(BigDecimal::from(amount_near_out)
                            / BigDecimal::from_f64(1.0 + fee).unwrap()),
                    )
                    .unwrap()
                } else {
                    amount_near_out
                };

                let max_amount_stnear_in = if let Amount::AmountOut(_) = request.amount {
                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let worst_case_amount_stnear_in = BigDecimal::from(amount_stnear_in)
                        / BigDecimal::from_f64(1.0 - slippage).unwrap();
                    ToPrimitive::to_u128(&worst_case_amount_stnear_in)?
                } else {
                    amount_stnear_in
                };
                let min_amount_near_out = if let Amount::AmountIn(_) = request.amount {
                    let slippage =
                        get_slippage_f64(request.slippage, &request.token_in, &request.token_out)
                            .await;
                    let worst_case_amount_near_out = BigDecimal::from(amount_near_out)
                        / BigDecimal::from_f64(1.0 + slippage).unwrap();
                    ToPrimitive::to_u128(&worst_case_amount_near_out)?
                } else {
                    amount_near_out
                };

                let liquid_unstake_instructions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "liquid_unstake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "st_near_to_burn": max_amount_stnear_in.to_string(),
                            "min_expected_near": min_amount_near_out.to_string(),
                        }))
                        .unwrap(),
                        gas: NearGas::from_tgas(10).as_gas(),
                        deposit: NearToken::from_yoctonear(0),
                    }))],
                }];

                let execution_instructions =
                    [input_to_nep141, liquid_unstake_instructions].concat();

                return Some(Route {
                    deadline: None,
                    has_slippage: true,
                    estimated_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(amount_near_out),
                        Amount::AmountOut(_) => Amount::AmountIn(amount_stnear_in),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(_) => Amount::AmountOut(min_amount_near_out),
                        Amount::AmountOut(_) => Amount::AmountIn(max_amount_stnear_in),
                    },
                    dex_id: DexId::MetaPool,
                    execution_instructions,
                    has_leftover_after_slippage_that_needs_unwrapping: false,
                    token_output: TokenId::Near,
                });
            }

            None
        })
    }
}
