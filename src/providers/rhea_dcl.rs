use std::{future::Future, pin::Pin};

use bigdecimal::{BigDecimal, RoundingMode};
use near_min_api::{
    types::{AccountId, Action, Balance, Finality, FunctionCallAction, Gas, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use num_traits::ToPrimitive;
use serde::Deserialize;

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, deposit_storage_on_contract_if_needed,
        get_slippage, needs_storage_deposit_for_contract, Mainnet, NetworkView, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaDclProvider;

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";
const FEE_TIERS: [u64; 4] = [100, 400, 2000, 10000]; // 0.01%, 0.04%, 0.2%, 1%

#[derive(Debug, Deserialize)]
struct RheaDclQuoteResponse {
    #[serde(with = "dec_format")]
    amount: Balance,
}

trait RheaDclQuotes: Send + Sync {
    fn quote(
        &self,
        pool_id: &str,
        input_token: &AccountId,
        output_token: &AccountId,
        input_amount: Balance,
    ) -> impl Future<Output = Option<Balance>> + Send;

    fn quote_by_output(
        &self,
        pool_id: &str,
        input_token: &AccountId,
        output_token: &AccountId,
        output_amount: Balance,
    ) -> impl Future<Output = Option<Balance>> + Send;
}

struct MainnetRheaDclQuotes;

impl RheaDclQuotes for MainnetRheaDclQuotes {
    async fn quote(
        &self,
        pool_id: &str,
        input_token: &AccountId,
        output_token: &AccountId,
        input_amount: Balance,
    ) -> Option<Balance> {
        RPC_CLIENT
            .call::<RheaDclQuoteResponse>(
                RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                "quote",
                serde_json::json!({
                    "pool_ids": vec![pool_id],
                    "input_token": input_token,
                    "output_token": output_token,
                    "input_amount": input_amount.to_string(),
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .ok()
            .map(|quote| quote.amount)
    }

    async fn quote_by_output(
        &self,
        pool_id: &str,
        input_token: &AccountId,
        output_token: &AccountId,
        output_amount: Balance,
    ) -> Option<Balance> {
        RPC_CLIENT
            .call::<RheaDclQuoteResponse>(
                RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                "quote_by_output",
                serde_json::json!({
                    "pool_ids": vec![pool_id],
                    "input_token": input_token,
                    "output_token": output_token,
                    "output_amount": output_amount.to_string(),
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .ok()
            .map(|quote| quote.amount)
    }
}

impl Provider for RheaDclProvider {
    fn dex_id(&self) -> DexId {
        DexId::RheaDcl
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaDclQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaDclQuotes,
) -> Option<Route> {
    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

    if token_in == token_out {
        return None;
    }

    let (token_x, token_y) = if token_in < token_out {
        (token_in.clone(), token_out.clone())
    } else {
        (token_out.clone(), token_in.clone())
    };

    match request.amount {
        Amount::AmountIn(exact_amount_in) => {
            let futures = FEE_TIERS.into_iter().map(|fee| {
                let pool_id = format!("{token_x}|{token_y}|{fee}");
                let token_in = token_in.clone();
                let token_out = token_out.clone();
                async move {
                    quotes
                        .quote(&pool_id, &token_in, &token_out, exact_amount_in)
                        .await
                        .map(|amount| (pool_id, amount))
                }
            });
            let routes = futures_util::future::join_all(futures)
                .await
                .into_iter()
                .flatten()
                .filter(|(_, amount)| *amount > 0)
                .collect::<Vec<_>>();
            let best_route = routes.iter().max_by_key(|(_, amount)| *amount);
            if let Some((pool_id, quote_amount)) = best_route {
                let slippage = get_slippage(
                    network,
                    request.slippage,
                    &request.token_in,
                    &request.token_out,
                )
                .await;
                let min_amount_out = ToPrimitive::to_u128(
                    &(BigDecimal::from(*quote_amount) * (BigDecimal::from(1) - slippage))
                        .with_scale_round(0, RoundingMode::Down),
                )?;

                let unwrapping_near = request.token_out == TokenId::Near;
                let ft_transfer_call_swap_action =
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": RHEA_DCL_CONTRACT_ID,
                            "amount": exact_amount_in.to_string(),
                            "msg": serde_json::to_string(&serde_json::json!({
                                "Swap": {
                                    "pool_ids": vec![pool_id],
                                    "output_token": token_out,
                                    "min_output_amount": min_amount_out.to_string(),
                                    "skip_unwrap_near": !unwrapping_near,
                                }
                            })).unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(100)),
                        deposit: NearToken::from_yoctonear(1),
                    }));

                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: token_in,
                    actions: vec![ft_transfer_call_swap_action],
                }];
                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    exact_amount_in,
                )
                .await?;
                if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                    if needs_storage_deposit_for_contract(
                        network,
                        trader_account_id,
                        &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                    )
                    .await
                    {
                        if let Ok(amount) = network.native_balance(trader_account_id).await {
                            // Don't use Rhea DCL for accounts with less than 1 NEAR, since
                            // the storage deposit of 0.5 NEAR is usually too high for them.
                            if amount < NearToken::from_near(1) {
                                return None;
                            }
                        }
                    }
                }
                let transactions = [
                    deposit_storage_on_contract_if_needed(
                        network,
                        &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                        request.trader_account_id.clone(),
                        NearToken::from_millinear(500),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
                        &request.token_out,
                        request.trader_account_id.clone(),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
                        &TokenId::Nep141(input_nep141),
                        request.trader_account_id.clone(),
                    )
                    .await,
                    input_to_nep141,
                    swap_transactions,
                ]
                .concat();
                Some(Route {
                    dex_id: DexId::RheaDcl,
                    estimated_amount: Amount::AmountOut(*quote_amount),
                    deadline: None,
                    has_slippage: true,
                    worst_case_amount: Amount::AmountOut(min_amount_out),
                    execution_instructions: transactions,
                    deprecated_needs_unwrap_always_false: false,
                    token_output: if unwrapping_near {
                        TokenId::Near
                    } else {
                        TokenId::Nep141(token_out)
                    },
                })
            } else {
                None
            }
        }
        Amount::AmountOut(exact_amount_out) => {
            let futures = FEE_TIERS.into_iter().map(|fee| {
                let pool_id = format!("{token_x}|{token_y}|{fee}");
                let token_in = token_in.clone();
                let token_out = token_out.clone();
                async move {
                    quotes
                        .quote_by_output(&pool_id, &token_in, &token_out, exact_amount_out)
                        .await
                        .map(|amount| (pool_id, amount))
                }
            });
            let routes = futures_util::future::join_all(futures)
                .await
                .into_iter()
                .flatten()
                .filter(|(_, amount)| *amount > 0)
                .collect::<Vec<_>>();
            let best_route = routes.iter().min_by_key(|(_, amount)| *amount);
            if let Some((pool_id, quote_amount)) = best_route {
                let slippage = get_slippage(
                    network,
                    request.slippage,
                    &request.token_in,
                    &request.token_out,
                )
                .await;
                let max_amount_in = ToPrimitive::to_u128(
                    &(BigDecimal::from(*quote_amount) / (BigDecimal::from(1) - slippage))
                        .with_scale_round(0, RoundingMode::Down),
                )?;

                let unwrapping_near = request.token_out == TokenId::Near;
                let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "ft_transfer_call".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "receiver_id": RHEA_DCL_CONTRACT_ID,
                        "amount": max_amount_in.to_string(),
                        "msg": serde_json::to_string(&serde_json::json!({
                            "SwapByOutput": {
                                "pool_ids": vec![pool_id],
                                "output_token": token_out,
                                "output_amount": exact_amount_out.to_string(),
                                "skip_unwrap_near": !unwrapping_near,
                            }
                        })).unwrap(),
                    }))
                    .unwrap(),
                    gas: Gas(NearGas::from_tgas(100)),
                    deposit: NearToken::from_yoctonear(1),
                }));

                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: token_in,
                    actions: vec![swap_action],
                }];
                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    max_amount_in,
                )
                .await?;
                let transactions = [
                    deposit_storage_on_contract_if_needed(
                        network,
                        &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                        request.trader_account_id.clone(),
                        NearToken::from_millinear(500),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
                        &if unwrapping_near {
                            TokenId::Near
                        } else {
                            TokenId::Nep141(token_out.clone())
                        },
                        request.trader_account_id.clone(),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
                        &TokenId::Nep141(input_nep141),
                        request.trader_account_id.clone(),
                    )
                    .await,
                    input_to_nep141,
                    swap_transactions,
                ]
                .concat();
                Some(Route {
                    dex_id: DexId::RheaDcl,
                    estimated_amount: Amount::AmountIn(*quote_amount),
                    deadline: None,
                    has_slippage: true,
                    worst_case_amount: Amount::AmountIn(max_amount_in),
                    execution_instructions: transactions,
                    deprecated_needs_unwrap_always_false: false,
                    token_output: if unwrapping_near {
                        TokenId::Near
                    } else {
                        TokenId::Nep141(token_out)
                    },
                })
            } else {
                None
            }
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
        create_storage_deposit_action_for_contract, create_wrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;

    #[derive(Clone)]
    struct TestRheaDclQuotes {
        enabled: bool,
        zero_quotes: bool,
    }

    impl Default for TestRheaDclQuotes {
        fn default() -> Self {
            Self {
                enabled: true,
                zero_quotes: false,
            }
        }
    }

    impl TestRheaDclQuotes {
        fn none() -> Self {
            Self {
                enabled: false,
                zero_quotes: false,
            }
        }

        fn zero_quotes() -> Self {
            Self {
                enabled: true,
                zero_quotes: true,
            }
        }
    }

    fn is_preferred_pool(pool_id: &str) -> bool {
        pool_id.ends_with("|2000")
    }

    impl RheaDclQuotes for TestRheaDclQuotes {
        async fn quote(
            &self,
            pool_id: &str,
            _input_token: &AccountId,
            _output_token: &AccountId,
            _input_amount: Balance,
        ) -> Option<Balance> {
            if !self.enabled {
                return None;
            }
            if self.zero_quotes {
                return Some(0);
            }
            Some(if is_preferred_pool(pool_id) { 80 } else { 70 })
        }

        async fn quote_by_output(
            &self,
            pool_id: &str,
            _input_token: &AccountId,
            _output_token: &AccountId,
            _output_amount: Balance,
        ) -> Option<Balance> {
            if !self.enabled {
                return None;
            }
            if self.zero_quotes {
                return Some(0);
            }
            Some(if is_preferred_pool(pool_id) { 100 } else { 110 })
        }
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_wnear_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_wnear_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_intear_near_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_to_rhea() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                NearToken::from_millinear(10),
                                false,
                            ),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_tokens".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "token_ids": ["other"],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(10)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|other|2000"],
                                        "output_token": "other",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("other".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": WRAP_NEAR,
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": false,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_ft_to_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": WRAP_NEAR,
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": false,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_rhea_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![
                            create_storage_deposit_action_for_contract(
                                NearToken::from_millinear(10),
                                false
                            ),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_tokens".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "token_ids": ["other"],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(10)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|other|2000"],
                                        "output_token": "other",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("other".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_ft() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "SwapByOutput": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "output_amount": "80",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_ft_to_near() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            NearToken::from_millinear(500),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "101",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "SwapByOutput": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": WRAP_NEAR,
                                        "output_amount": "80",
                                        "skip_unwrap_near": false,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
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
                &TestRheaDclQuotes::default(),
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
                &TestRheaDclQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn zero_quotes_returns_none() {
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
                &TestRheaDclQuotes::zero_quotes(),
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
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
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
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
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
                                "receiver_id": RHEA_DCL_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "Swap": {
                                        "pool_ids": vec!["ft|wrap.near|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(100)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn poor_account_returns_none() {
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
                &TestNetworkView::default()
                    .with_native_balance("trader.near", NearToken::from_millinear(500)),
                &TestRheaDclQuotes::default(),
            )
            .await,
            None
        );
    }
}
