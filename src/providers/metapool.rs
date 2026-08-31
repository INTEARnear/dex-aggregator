use std::{future::Future, pin::Pin};

use bigdecimal::{BigDecimal, RoundingMode};
use near_min_api::{
    types::{Action, Finality, FunctionCallAction, Gas, NearGas, NearToken},
    QueryFinality,
};
use num_traits::{ToPrimitive, Zero};
use serde::Deserialize;
use tracing::error;

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage, is_near,
        Mainnet, NetworkView, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const METAPOOL_CONTRACT: &str = "meta-pool.near";
const MIN_DEPOSIT_AMOUNT_NEAR: NearToken = NearToken::from_near(1);
const MIN_LIQUID_UNSTAKE_AMOUNT_STNEAR: NearToken = NearToken::from_near(1);

pub struct MetapoolProvider;

#[derive(Debug, Clone, Deserialize)]
struct MetapoolContractState {
    st_near_price: NearToken,
    nslp_liquidity: NearToken,
}

trait MetapoolQuotes: Send + Sync {
    fn get_contract_state(&self) -> impl Future<Output = Option<MetapoolContractState>> + Send;

    fn nslp_get_discount_basis_points(
        &self,
        stnear_to_sell: u128,
    ) -> impl Future<Output = Option<u16>> + Send;
}

struct MainnetMetapoolQuotes;

impl MetapoolQuotes for MainnetMetapoolQuotes {
    async fn get_contract_state(&self) -> Option<MetapoolContractState> {
        RPC_CLIENT
            .call::<MetapoolContractState>(
                METAPOOL_CONTRACT.parse().unwrap(),
                "get_contract_state",
                serde_json::json!({}),
                QueryFinality::Finality(Finality::Final),
            )
            .await
            .ok()
    }

    async fn nslp_get_discount_basis_points(&self, stnear_to_sell: u128) -> Option<u16> {
        RPC_CLIENT
            .call::<u16>(
                METAPOOL_CONTRACT.parse().unwrap(),
                "nslp_get_discount_basis_points",
                serde_json::json!({
                    "stnear_to_sell": stnear_to_sell.to_string(),
                }),
                QueryFinality::Finality(Finality::Final),
            )
            .await
            .ok()
    }
}

impl Provider for MetapoolProvider {
    fn dex_id(&self) -> DexId {
        DexId::MetaPool
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetMetapoolQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl MetapoolQuotes,
) -> Option<Route> {
    if is_near(&request.token_in) {
        let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
        if nep141_out != METAPOOL_CONTRACT {
            return None;
        }

        // NEAR -> STNEAR

        let state = quotes.get_contract_state().await?;

        let amount_near_in = match request.amount {
            Amount::AmountIn(amount) => amount,
            Amount::AmountOut(amount) => {
                let amount_near_in = BigDecimal::from(amount) / BigDecimal::from(10u128.pow(24))
                    * BigDecimal::from(state.st_near_price.as_yoctonear());
                ToPrimitive::to_u128(&amount_near_in)?
            }
        };
        if amount_near_in < MIN_DEPOSIT_AMOUNT_NEAR.as_yoctonear() {
            return None;
        }
        let amount_stnear_out = match request.amount {
            Amount::AmountIn(amount) => {
                let amount_stnear_out = BigDecimal::from(amount) * BigDecimal::from(10u128.pow(24))
                    / BigDecimal::from(state.st_near_price.as_yoctonear());
                ToPrimitive::to_u128(&amount_stnear_out)?.saturating_sub(1)
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
            receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
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
                network,
                &TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
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
                Amount::AmountIn(_) => Amount::AmountOut(amount_stnear_out),
                Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
            },
            worst_case_amount: match request.amount {
                Amount::AmountIn(_) => Amount::AmountOut(amount_stnear_out),
                Amount::AmountOut(_) => Amount::AmountIn(amount_near_in),
            },
            dex_id: DexId::MetaPool,
            execution_instructions,
            deprecated_needs_unwrap_always_false: false,
            token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
        });
    } else if is_near(&request.token_out) {
        let (_input_to_nep141, nep141_in) = convert_to_nep141(&request.token_in, None, 0).await?;
        if nep141_in != METAPOOL_CONTRACT {
            return None;
        }

        // STNEAR -> NEAR

        let state = quotes.get_contract_state().await?;

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
                ToPrimitive::to_u128(&amount_near_out)?.saturating_sub(1)
            }
            Amount::AmountOut(amount_out) => amount_out,
        };

        let Some(discount_basis_points) = quotes
            .nslp_get_discount_basis_points(amount_stnear_in)
            .await
        else {
            error!(
                "Failed to get discount basis points for liquid unstake for amount {}",
                amount_stnear_in
            );
            return None;
        };
        let fee = BigDecimal::from(discount_basis_points) / BigDecimal::from(10000);
        let amount_stnear_in = if let Amount::AmountOut(_) = request.amount {
            ToPrimitive::to_u128(
                &(BigDecimal::from(amount_stnear_in) / (BigDecimal::from(1) - &fee))
                    .with_scale_round(0, RoundingMode::Down),
            )
            .unwrap()
        } else {
            amount_stnear_in
        };
        let amount_near_out = if let Amount::AmountIn(_) = request.amount {
            ToPrimitive::to_u128(
                &(BigDecimal::from(amount_near_out) / (BigDecimal::from(1) + &fee))
                    .with_scale_round(0, RoundingMode::Down),
            )
            .unwrap()
        } else {
            amount_near_out
        };

        let max_amount_stnear_in = if let Amount::AmountOut(_) = request.amount {
            let slippage = get_slippage(
                network,
                request.slippage.clone(),
                &request.token_in,
                &request.token_out,
            )
            .await;
            let one_minus_slippage = BigDecimal::from(1) - slippage;
            if one_minus_slippage.is_zero() {
                return None;
            }
            let worst_case_amount_stnear_in =
                BigDecimal::from(amount_stnear_in) / one_minus_slippage;
            ToPrimitive::to_u128(
                &worst_case_amount_stnear_in.with_scale_round(0, RoundingMode::Down),
            )?
        } else {
            amount_stnear_in
        };
        let min_amount_near_out = if let Amount::AmountIn(_) = request.amount {
            let slippage = get_slippage(
                network,
                request.slippage.clone(),
                &request.token_in,
                &request.token_out,
            )
            .await;
            let worst_case_amount_near_out =
                BigDecimal::from(amount_near_out) / (BigDecimal::from(1) + slippage);
            ToPrimitive::to_u128(
                &worst_case_amount_near_out.with_scale_round(0, RoundingMode::Down),
            )?
        } else {
            amount_near_out
        };

        let (input_to_nep141, _nep141_in) = convert_to_nep141(
            &request.token_in,
            request.trader_account_id.clone(),
            max_amount_stnear_in,
        )
        .await?;

        let liquid_unstake_instructions = vec![ExecutionInstruction::NearTransaction {
            receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
            actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "liquid_unstake".to_string(),
                args: serde_json::to_vec(&serde_json::json!({
                    "st_near_to_burn": max_amount_stnear_in.to_string(),
                    "min_expected_near": min_amount_near_out.to_string(),
                }))
                .unwrap(),
                gas: Gas(NearGas::from_tgas(10)),
                deposit: NearToken::from_yoctonear(0),
            }))],
        }];

        let execution_instructions = [input_to_nep141, liquid_unstake_instructions].concat();

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
            deprecated_needs_unwrap_always_false: false,
            token_output: TokenId::Near,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::providers::intear_plach::AssetId;
    use crate::shared_utils::{
        create_intear_dex_withdraw_action, create_rhea_withdraw_action,
        create_storage_deposit_action_for_contract, create_unwrap_action, TestNetworkView,
        WRAP_NEAR,
    };
    use crate::types::Slippage;

    struct TestMetapoolQuotes {
        state: Option<MetapoolContractState>,
        discount_basis_points: Option<u16>,
    }

    impl Default for TestMetapoolQuotes {
        fn default() -> Self {
            Self {
                state: Some(MetapoolContractState {
                    st_near_price: NearToken::from_near(1),
                    nslp_liquidity: NearToken::from_near(1000),
                }),
                discount_basis_points: Some(0),
            }
        }
    }

    impl TestMetapoolQuotes {
        fn none() -> Self {
            Self {
                state: None,
                discount_basis_points: Some(0),
            }
        }

        fn no_discount() -> Self {
            Self {
                state: Some(MetapoolContractState {
                    st_near_price: NearToken::from_near(1),
                    nslp_liquidity: NearToken::from_near(1000),
                }),
                discount_basis_points: None,
            }
        }

        fn low_liquidity() -> Self {
            Self {
                state: Some(MetapoolContractState {
                    st_near_price: NearToken::from_near(1),
                    nslp_liquidity: NearToken::from_millinear(500),
                }),
                discount_basis_points: Some(0),
            }
        }
    }

    impl MetapoolQuotes for TestMetapoolQuotes {
        async fn get_contract_state(&self) -> Option<MetapoolContractState> {
            self.state.clone()
        }

        async fn nslp_get_discount_basis_points(&self, _stnear_to_sell: u128) -> Option<u16> {
            self.discount_basis_points
        }
    }

    #[tokio::test]
    async fn amount_in_near_to_stnear() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(one_near - 1),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_near(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_wnear_to_stnear() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(one_near - 1),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_unwrap_action(NearToken::from_near(1))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_near(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_wnear_to_stnear() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(one_near - 1),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            one_near,
                            true,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_near(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_intear_near_to_stnear() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(one_near - 1),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(&AssetId::Near, one_near)],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_near(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_stnear() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountOut(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountIn(one_near),
                worst_case_amount: Amount::AmountIn(one_near),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_near(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_stnear_to_near() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(990099009900990099009900),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "liquid_unstake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "st_near_to_burn": one_near.to_string(),
                            "min_expected_near": "990099009900990099009900",
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(0),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_stnear_to_near() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(990099009900990099009900),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &METAPOOL_CONTRACT.parse().unwrap(),
                            one_near,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "liquid_unstake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "st_near_to_burn": one_near.to_string(),
                                "min_expected_near": "990099009900990099009900",
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(0),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_out_stnear_to_near() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountOut(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(one_near),
                worst_case_amount: Amount::AmountIn(1010101010101010101010101),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "liquid_unstake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "st_near_to_burn": "1010101010101010101010101",
                            "min_expected_near": one_near.to_string(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(0),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn deposit_below_minimum_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn unstake_below_minimum_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn insufficient_liquidity_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(NearToken::from_near(1).as_yoctonear()),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::low_liquidity(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(NearToken::from_near(1).as_yoctonear()),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_discount_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(NearToken::from_near(1).as_yoctonear()),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::no_discount(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn wrong_token_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(NearToken::from_near(1).as_yoctonear()),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage() {
        let one_near = NearToken::from_near(1).as_yoctonear();
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(one_near),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestMetapoolQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(one_near - 1),
                worst_case_amount: Amount::AmountOut(one_near - 1),
                dex_id: DexId::MetaPool,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: METAPOOL_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_and_stake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_near(1),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(METAPOOL_CONTRACT.parse().unwrap()),
            })
        );
    }
}
