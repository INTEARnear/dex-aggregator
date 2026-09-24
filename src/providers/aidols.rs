use std::{future::Future, pin::Pin};

use bigdecimal::{BigDecimal, RoundingMode};
use near_min_api::{
    types::{
        AccountId, Action, Balance, Finality, FunctionCallAction, Gas, NearGas, NearToken, U128,
    },
    QueryFinality,
};
use num_traits::ToPrimitive;
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, get_slippage, Mainnet, NetworkView,
        DEFAULT_REFERRER_ID, RPC_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct AidolsProvider;

const AIDOLS_CONTRACT_ID: &str = "aidols.near";

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct AidolsEmulateSwap {
    amount: Balance,
    fee: Balance,
    is_deployed: bool,
    is_tradable: bool,
}

impl From<(U128, U128, bool, bool)> for AidolsEmulateSwap {
    fn from((amount, fee, is_deployed, is_tradable): (U128, U128, bool, bool)) -> Self {
        Self {
            amount: *amount,
            fee: *fee,
            is_deployed,
            is_tradable,
        }
    }
}

trait AidolsQuotes: Send + Sync {
    fn emulate_swap(
        &self,
        input_token: &AccountId,
        output_token: &AccountId,
        amount: Balance,
    ) -> impl Future<Output = Option<AidolsEmulateSwap>> + Send;

    fn emulate_swap_by_out(
        &self,
        input_token: &AccountId,
        output_token: &AccountId,
        amount_out: Balance,
    ) -> impl Future<Output = Option<AidolsEmulateSwap>> + Send;
}

struct MainnetAidolsQuotes;

impl AidolsQuotes for MainnetAidolsQuotes {
    async fn emulate_swap(
        &self,
        input_token: &AccountId,
        output_token: &AccountId,
        amount: Balance,
    ) -> Option<AidolsEmulateSwap> {
        RPC_CLIENT
            .call::<(U128, U128, bool, bool)>(
                AIDOLS_CONTRACT_ID.parse().unwrap(),
                "emulate_swap",
                serde_json::json!({
                    "input_token": input_token,
                    "output_token": output_token,
                    "amount": amount.to_string(),
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .ok()
            .map(Into::into)
    }

    async fn emulate_swap_by_out(
        &self,
        input_token: &AccountId,
        output_token: &AccountId,
        amount_out: Balance,
    ) -> Option<AidolsEmulateSwap> {
        RPC_CLIENT
            .call::<(U128, U128, bool, bool)>(
                AIDOLS_CONTRACT_ID.parse().unwrap(),
                "emulate_swap_by_out",
                serde_json::json!({
                    "input_token": input_token,
                    "output_token": output_token,
                    "amount_out": amount_out.to_string(),
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .ok()
            .map(Into::into)
    }
}

impl Provider for AidolsProvider {
    fn dex_id(&self) -> DexId {
        DexId::Aidols
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetAidolsQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl AidolsQuotes,
) -> Option<Route> {
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
            let quote = quotes
                .emulate_swap(&nep141_in, &nep141_out, exact_amount_in)
                .await?;
            if !quote.is_tradable {
                return None;
            }
            let estimated_amount_out = quote.amount;

            info!("Estimated amount out: {}", estimated_amount_out);

            let slippage = get_slippage(
                network,
                request.slippage,
                &request.token_in,
                &request.token_out,
            )
            .await;
            let min_amount_out = ToPrimitive::to_u128(
                &(BigDecimal::from(estimated_amount_out) * (BigDecimal::from(1) - slippage))
                    .with_scale_round(0, RoundingMode::Down),
            )?;

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
                    network,
                    &TokenId::Nep141(nep141_out.clone()),
                    request.trader_account_id.clone(),
                )
                .await,
                deposit_storage_if_needed(
                    network,
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
            let quote = quotes
                .emulate_swap_by_out(&nep141_in, &nep141_out, exact_amount_out)
                .await?;
            if !quote.is_tradable {
                return None;
            }
            let required_amount_in = quote.amount;

            info!("Required amount in: {}", required_amount_in);

            let slippage = get_slippage(
                network,
                request.slippage,
                &request.token_in,
                &request.token_out,
            )
            .await;
            let max_amount_in = ToPrimitive::to_u128(
                &(BigDecimal::from(required_amount_in) / (BigDecimal::from(1) - slippage))
                    .with_scale_round(0, RoundingMode::Down),
            )?;

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
                    network,
                    &TokenId::Nep141(nep141_out.clone()),
                    request.trader_account_id.clone(),
                )
                .await,
                deposit_storage_if_needed(
                    network,
                    &TokenId::Nep141(nep141_in.clone()),
                    request.trader_account_id.clone(),
                )
                .await,
                convert_to_nep141(&request.token_in, None, max_amount_in)
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
                estimated_amount: Amount::AmountIn(required_amount_in),
                worst_case_amount: Amount::AmountIn(max_amount_in),
                execution_instructions: transactions,
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141(nep141_out.clone()),
            };

            Some(route)
        }
    }
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::providers::intear_plach::AssetId;
    use crate::shared_utils::{
        create_intear_dex_withdraw_action, create_rhea_withdraw_action,
        create_storage_deposit_action_for_contract, create_wrap_action, TestNetworkView,
    };
    use crate::types::Slippage;

    #[derive(Clone)]
    struct TestAidolsQuotes {
        emulate_swap: Option<AidolsEmulateSwap>,
        emulate_swap_by_out: Option<AidolsEmulateSwap>,
    }

    impl Default for TestAidolsQuotes {
        fn default() -> Self {
            Self {
                emulate_swap: Some(AidolsEmulateSwap {
                    amount: 80,
                    fee: 0,
                    is_deployed: true,
                    is_tradable: true,
                }),
                emulate_swap_by_out: Some(AidolsEmulateSwap {
                    amount: 100,
                    fee: 0,
                    is_deployed: true,
                    is_tradable: true,
                }),
            }
        }
    }

    impl TestAidolsQuotes {
        fn none() -> Self {
            Self {
                emulate_swap: None,
                emulate_swap_by_out: None,
            }
        }

        fn not_tradable() -> Self {
            Self {
                emulate_swap: Some(AidolsEmulateSwap {
                    amount: 80,
                    fee: 0,
                    is_deployed: true,
                    is_tradable: false,
                }),
                emulate_swap_by_out: Some(AidolsEmulateSwap {
                    amount: 100,
                    fee: 0,
                    is_deployed: true,
                    is_tradable: false,
                }),
            }
        }
    }

    fn is_wrap_aidols_pair(input_token: &AccountId, output_token: &AccountId) -> bool {
        let wrap_near = WRAP_NEAR.parse::<AccountId>().unwrap();
        let aidols = AIDOLS_CONTRACT_ID.parse::<AccountId>().unwrap();
        (input_token == &wrap_near && output_token.is_sub_account_of(&aidols))
            || (output_token == &wrap_near && input_token.is_sub_account_of(&aidols))
    }

    impl AidolsQuotes for TestAidolsQuotes {
        async fn emulate_swap(
            &self,
            input_token: &AccountId,
            output_token: &AccountId,
            _amount: Balance,
        ) -> Option<AidolsEmulateSwap> {
            if !is_wrap_aidols_pair(input_token, output_token) {
                return None;
            }
            self.emulate_swap
        }

        async fn emulate_swap_by_out(
            &self,
            input_token: &AccountId,
            output_token: &AccountId,
            _amount_out: Balance,
        ) -> Option<AidolsEmulateSwap> {
            if !is_wrap_aidols_pair(input_token, output_token) {
                return None;
            }
            self.emulate_swap_by_out
        }
    }

    #[tokio::test]
    async fn amount_in_buy_native_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_buy_wnear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_buy_rhea_wnear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_buy_intear_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(&AssetId::Near, 100)],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_buy_intear_wnear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Nep141(
                        WRAP_NEAR.parse().unwrap()
                    )),
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(
                            &AssetId::Nep141(WRAP_NEAR.parse().unwrap()),
                            100,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_sell_nep141() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("token.aidols.near".parse().unwrap()),
                    token_out: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": null,
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_sell_rhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"token.aidols.near".parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": null,
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_sell_intear() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Nep141(
                        "token.aidols.near".parse().unwrap(),
                    )),
                    token_out: TokenId::TokenOnIntearDex(AssetId::Near),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        actions: vec![create_intear_dex_withdraw_action(
                            &AssetId::Nep141("token.aidols.near".parse().unwrap()),
                            100,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": null,
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_buy_native_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(101))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "amount_out": "80",
                                    "min_swap_amount": u128::MAX.to_string(),
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_sell_nep141() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("token.aidols.near".parse().unwrap()),
                    token_out: TokenId::Near,
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": null,
                                    "amount_out": "80",
                                    "min_swap_amount": u128::MAX.to_string(),
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn buy_output_location_does_not_change_route() {
        let to_nep141 = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
            &TestAidolsQuotes::default(),
        )
        .await;
        let to_rhea = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141OnRhea("token.aidols.near".parse().unwrap()),
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
            &TestAidolsQuotes::default(),
        )
        .await;
        let to_intear = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::TokenOnIntearDex(AssetId::Nep141(
                    "token.aidols.near".parse().unwrap(),
                )),
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
            &TestAidolsQuotes::default(),
        )
        .await;
        assert_eq!(to_nep141, to_rhea);
        assert_eq!(to_nep141, to_intear);
    }

    #[tokio::test]
    async fn non_aidols_token_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141("ft.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            None
        );
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft.near".parse().unwrap()),
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            None
        );
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("token.aidols.near".parse().unwrap()),
                    token_out: TokenId::Nep141("ft.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn not_tradable_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::not_tradable(),
            )
            .await,
            None
        );
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::not_tradable(),
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
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::none(),
            )
            .await,
            None
        );
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_storage_deposits() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
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
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": DEFAULT_REFERRER_ID,
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_swap_msg() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("token.aidols.near".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: Some("ref.near".parse().unwrap()),
                },
                &TestNetworkView::default(),
                &TestAidolsQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Aidols,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "token.aidols.near".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": "aidols.near",
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "token": "token.aidols.near",
                                    "min_swap_amount": "79",
                                    "referral": "ref.near",
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
                token_output: TokenId::Nep141("token.aidols.near".parse().unwrap()),
            })
        );
    }
}
