use std::{future::Future, pin::Pin};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{Action, Finality, FunctionCallAction, Gas, NearGas, NearToken, U128},
    QueryFinality,
};
use num_traits::ToPrimitive;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, Mainnet, NetworkView, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

const RHEA_CONTRACT: &str = "token.rhealab.near";
const XRHEA_CONTRACT: &str = "xtoken.rhealab.near";

pub struct XRheaProvider;

trait XRheaQuotes: Send + Sync {
    fn get_virtual_price(&self) -> impl Future<Output = Option<U128>> + Send;
}

struct MainnetXRheaQuotes;

impl XRheaQuotes for MainnetXRheaQuotes {
    async fn get_virtual_price(&self) -> Option<U128> {
        RPC_CLIENT
            .call::<U128>(
                XRHEA_CONTRACT.parse().unwrap(),
                "get_virtual_price",
                serde_json::json!({}),
                QueryFinality::Finality(Finality::Final),
            )
            .await
            .ok()
    }
}

impl Provider for XRheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::XRhea
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetXRheaQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl XRheaQuotes,
) -> Option<Route> {
    let (_input_to_nep141, nep141_in) = convert_to_nep141(&request.token_in, None, 0).await?;

    if nep141_in == RHEA_CONTRACT {
        let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;
        if nep141_out != XRHEA_CONTRACT {
            return None;
        }

        // RHEA -> XRHEA

        let virtual_price = quotes.get_virtual_price().await?;
        let virtual_price = BigDecimal::from(*virtual_price) / BigDecimal::from(10u128.pow(8));

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
                network,
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

        let virtual_price = quotes.get_virtual_price().await?;
        let virtual_price = BigDecimal::from(*virtual_price) / BigDecimal::from(10u128.pow(8));

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
                network,
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
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::shared_utils::{
        create_rhea_withdraw_action, create_storage_deposit_action_for_contract, TestNetworkView,
    };
    use crate::types::Slippage;

    struct TestXRheaQuotes {
        virtual_price: Option<U128>,
    }

    impl Default for TestXRheaQuotes {
        fn default() -> Self {
            Self {
                virtual_price: Some(U128::from(100_000_000)),
            }
        }
    }

    impl TestXRheaQuotes {
        fn none() -> Self {
            Self {
                virtual_price: None,
            }
        }
    }

    impl XRheaQuotes for TestXRheaQuotes {
        async fn get_virtual_price(&self) -> Option<U128> {
            self.virtual_price
        }
    }

    #[tokio::test]
    async fn amount_in_rhea_to_xrhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(100),
                worst_case_amount: Amount::AmountOut(100),
                dex_id: DexId::XRhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": XRHEA_CONTRACT,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Stake": {}
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(50)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_on_rhea_to_xrhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(RHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(100),
                worst_case_amount: Amount::AmountOut(100),
                dex_id: DexId::XRhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &RHEA_CONTRACT.parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": XRHEA_CONTRACT,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Stake": {}
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(50)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_xrhea_to_rhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(100),
                worst_case_amount: Amount::AmountOut(100),
                dex_id: DexId::XRhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "unstake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "amount": "100",
                                "msg": ""
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(50)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_rhea_to_xrhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountIn(80),
                worst_case_amount: Amount::AmountIn(80),
                dex_id: DexId::XRhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": XRHEA_CONTRACT,
                                "amount": "80",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Stake": {}
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(50)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_xrhea_to_rhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountIn(80),
                worst_case_amount: Amount::AmountIn(80),
                dex_id: DexId::XRhea,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_CONTRACT.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: XRHEA_CONTRACT.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "unstake".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "amount": "80",
                                "msg": ""
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(50)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn wrong_pair_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
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
                    token_in: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::none(),
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
                    token_in: TokenId::Nep141(RHEA_CONTRACT.parse().unwrap()),
                    token_out: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
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
                &TestXRheaQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: false,
                estimated_amount: Amount::AmountOut(100),
                worst_case_amount: Amount::AmountOut(100),
                dex_id: DexId::XRhea,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: RHEA_CONTRACT.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": XRHEA_CONTRACT,
                            "amount": "100",
                            "msg": serde_json::to_string(&serde_json::json!({
                                "Stake": {}
                            }))
                            .unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(XRHEA_CONTRACT.parse().unwrap()),
            })
        );
    }
}
