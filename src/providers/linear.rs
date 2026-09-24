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
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, is_near, Mainnet,
        NetworkView, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const LINEAR_CONTRACT: &str = "linear-protocol.near";

pub struct LinearProvider;

#[derive(Debug, Clone, Deserialize)]
struct LinearContractState {
    ft_price: NearToken,
}

trait LinearQuotes: Send + Sync {
    fn get_summary(&self) -> impl Future<Output = Option<LinearContractState>> + Send;
}

struct MainnetLinearQuotes;

impl LinearQuotes for MainnetLinearQuotes {
    async fn get_summary(&self) -> Option<LinearContractState> {
        RPC_CLIENT
            .call::<LinearContractState>(
                LINEAR_CONTRACT.parse().unwrap(),
                "get_summary",
                serde_json::json!({}),
                QueryFinality::Finality(Finality::Final),
            )
            .await
            .ok()
    }
}

impl Provider for LinearProvider {
    fn dex_id(&self) -> DexId {
        DexId::Linear
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetLinearQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl LinearQuotes,
) -> Option<Route> {
    if is_near(&request.token_in) {
        let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
        if nep141_out != LINEAR_CONTRACT {
            return None;
        }

        // NEAR -> LiNEAR

        let state = quotes.get_summary().await?;

        let amount_near_in = match request.amount {
            Amount::AmountIn(amount) => amount,
            Amount::AmountOut(amount) => {
                let amount_near_in = BigDecimal::from(amount) / BigDecimal::from(10u128.pow(24))
                    * BigDecimal::from(state.ft_price.as_yoctonear());
                ToPrimitive::to_u128(&amount_near_in)?
            }
        };
        let amount_linear_out = match request.amount {
            Amount::AmountIn(amount) => {
                let amount_linear_out = BigDecimal::from(amount) * BigDecimal::from(10u128.pow(24))
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
                network,
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

    struct TestLinearQuotes {
        state: Option<LinearContractState>,
    }

    impl Default for TestLinearQuotes {
        fn default() -> Self {
            Self {
                state: Some(LinearContractState {
                    ft_price: NearToken::from_near(1),
                }),
            }
        }
    }

    impl TestLinearQuotes {
        fn none() -> Self {
            Self { state: None }
        }
    }

    impl LinearQuotes for TestLinearQuotes {
        async fn get_summary(&self) -> Option<LinearContractState> {
            self.state.clone()
        }
    }

    #[tokio::test]
    async fn amount_in_near_to_linear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(99),
                worst_case_amount: Amount::AmountOut(99),
                dex_id: DexId::Linear,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_wnear_to_linear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(99),
                worst_case_amount: Amount::AmountOut(99),
                dex_id: DexId::Linear,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_unwrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_wnear_to_linear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(99),
                worst_case_amount: Amount::AmountOut(99),
                dex_id: DexId::Linear,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            true,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_intear_near_to_linear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(99),
                worst_case_amount: Amount::AmountOut(99),
                dex_id: DexId::Linear,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(&AssetId::Near, 100)],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_linear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountOut(80),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountIn(80),
                worst_case_amount: Amount::AmountIn(80),
                dex_id: DexId::Linear,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_and_stake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: Gas(NearGas::from_tgas(10)),
                            deposit: NearToken::from_yoctonear(80),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn wrong_output_token_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
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
                &TestLinearQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn non_near_input_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::default(),
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
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
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
                &TestLinearQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
                    amount: Amount::AmountIn(100),
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
                &TestLinearQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(99),
                worst_case_amount: Amount::AmountOut(99),
                dex_id: DexId::Linear,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: LINEAR_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_and_stake".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(100),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(LINEAR_CONTRACT.parse().unwrap()),
            })
        );
    }
}
