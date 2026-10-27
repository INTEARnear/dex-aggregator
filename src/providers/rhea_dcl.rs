use std::{future::Future, pin::Pin};

use intear_dex_types::FULL_FEE_FRACTION;
use near_min_api::types::{AccountId, Action, FunctionCallAction, NearGas, NearToken};
use pathfinder::{
    dcl::{self, RouteApiResponse},
    MaxHops, QuoteAmount,
};
use tracing::info;

use crate::{
    shared_utils::{
        base_token, convert_to_nep141, convert_to_nep141_with_fees, deposit_storage_if_needed,
        deposit_storage_on_contract_if_needed, estimated_gas_paid, get_slippage, near_receive_gas,
        payable_referral_fees, price_raw, referrer_fees_of_total, register_input_in_wallet,
        total_fee_amount, BaseTokenId, Mainnet, NetworkView, ReferrerFee, DEFAULT_REFERRER_ID,
        TOKEN_GAS,
    },
    types::{ExecutionInstruction, Gas, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct RheaDclProvider;

pub const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";

const OUTPUT_RESERVED_GAS: NearGas = NearGas::from_ggas(31_800);
const CALLBACK_POST_WITHDRAW_ASSET_GAS: NearGas = NearGas::from_ggas(2_360);
const OUTPUT_NEAR_WITHDRAW_GAS: NearGas = NearGas::from_ggas(2_000);
const CALLBACK_POST_WITHDRAW_NEAR_GAS: NearGas = NearGas::from_ggas(3_160);
const STORAGE_DEPOSIT_GAS: Gas = Gas::from_ggas(2_000, 3_000);

pub fn helper_call_gas(method_name: &str) -> Gas {
    match method_name {
        "storage_deposit" => STORAGE_DEPOSIT_GAS,
        method_name => panic!("Routes don't call {method_name} on Rhea DCL"),
    }
}

trait RheaDclQuotes: Send + Sync {
    fn find_path(
        &self,
        request: dcl::Request,
    ) -> impl Future<Output = Option<RouteApiResponse>> + Send;
}

struct MainnetRheaDclQuotes;

impl RheaDclQuotes for MainnetRheaDclQuotes {
    async fn find_path(&self, request: dcl::Request) -> Option<RouteApiResponse> {
        dcl::find_path(request)
            .await
            .inspect_err(|e| info!("No Rhea DCL route: {e:#}"))
            .ok()
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
    let slippage = get_slippage(
        network,
        request.slippage,
        &base_token(&request.token_in).unwrap(),
        &base_token(&request.token_out).unwrap(),
    );

    let (_, token_in) = convert_to_nep141(&request.token_in, None, 0).await?;
    let (_, token_out) = convert_to_nep141(&request.token_out, None, 0).await?;

    if token_in == token_out {
        return None;
    }

    // The fees come out of the input. Exact-in quotes the rest of it, exact-out adds the fees on
    // top of the estimated input once it's quoted.
    let referral_fees = payable_referral_fees(&request.fees);
    let exact_in_fees = match request.amount {
        Amount::AmountIn(amount_in) => referrer_fees_of_total(&referral_fees, amount_in),
        Amount::AmountOut(_) => Vec::new(),
    };
    let swap_referrer_id = request
        .referrer_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string());

    let response = quotes
        .find_path(dcl::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount: match request.amount {
                Amount::AmountIn(amount_in) => {
                    QuoteAmount::ExactIn(amount_in - total_fee_amount(&exact_in_fees))
                }
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &BaseTokenId::Near)?,
            quoted_token_price_raw: price_raw(
                network,
                &match request.amount {
                    Amount::AmountIn(_) => base_token(&request.token_out).unwrap(),
                    Amount::AmountOut(_) => base_token(&request.token_in).unwrap(),
                },
            )?,
        })
        .await?;

    let unwrapping_near = request.token_out == TokenId::Near;
    let (amount_in, message, estimated_amount, worst_case_amount, swap_gas, fees) = match response {
        RouteApiResponse::ExactIn { route, .. } => {
            let pool_ids = route
                .pools
                .iter()
                .map(|step| step.pool_id.as_str())
                .collect::<Vec<_>>();
            let message = serde_json::json!({
                "Swap": {
                    "pool_ids": pool_ids,
                    "output_token": token_out,
                    "min_output_amount": route.min_amount_out.0.to_string(),
                    "skip_unwrap_near": !unwrapping_near,
                    "referral_id": swap_referrer_id,
                }
            });
            (
                route.amount_in.0,
                message,
                Amount::AmountOut(route.amount_out.0),
                Amount::AmountOut(route.min_amount_out.0),
                route.swap_gas,
                exact_in_fees,
            )
        }
        RouteApiResponse::ExactOut { route, .. } => {
            // SwapByOutput walks the pools from the output token
            let pool_ids = route
                .pools
                .iter()
                .rev()
                .map(|step| step.pool_id.as_str())
                .collect::<Vec<_>>();
            let message = serde_json::json!({
                "SwapByOutput": {
                    "pool_ids": pool_ids,
                    "output_token": token_out,
                    "output_amount": route.amount_out.0.to_string(),
                    "skip_unwrap_near": !unwrapping_near,
                    "referral_id": swap_referrer_id,
                }
            });
            // Each referrer's share of the estimated input with all fees, not the worst case, so
            // high slippage doesn't raise the fees
            let rest = (FULL_FEE_FRACTION
                - referral_fees
                    .0
                    .iter()
                    .map(|(_, fee_fraction)| fee_fraction)
                    .sum::<u32>()) as u128;
            let amount = route.amount_in.0;
            let fees = referral_fees
                .0
                .iter()
                .map(|(referrer_id, fee_fraction)| ReferrerFee {
                    referrer_id: referrer_id.clone(),
                    amount: amount / rest * *fee_fraction as u128
                        + amount % rest * *fee_fraction as u128 / rest,
                })
                .filter(|fee| fee.amount > 0)
                .collect::<Vec<_>>();
            let fee_amount = total_fee_amount(&fees);
            (
                route.max_amount_in.0,
                message,
                Amount::AmountIn(route.amount_in.0 + fee_amount),
                Amount::AmountIn(route.max_amount_in.0 + fee_amount),
                route.swap_gas,
                fees,
            )
        }
    };

    let output_burn = if unwrapping_near {
        OUTPUT_NEAR_WITHDRAW_GAS.as_gas()
            + CALLBACK_POST_WITHDRAW_NEAR_GAS.as_gas()
            + near_receive_gas(request.trader_account_id.as_ref())
                .estimated_burn
                .as_gas()
    } else {
        TOKEN_GAS.ft_transfer.estimated_burn.as_gas() + CALLBACK_POST_WITHDRAW_ASSET_GAS.as_gas()
    };
    let swap_call_gas = TOKEN_GAS.ft_transfer_call(
        swap_gas
            + Gas {
                estimated_burn: NearGas::from_gas(output_burn),
                needs_attached: OUTPUT_RESERVED_GAS,
            },
    );
    let swap_action = Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": RHEA_DCL_CONTRACT_ID,
            "amount": amount_in.to_string(),
            "msg": serde_json::to_string(&message).unwrap(),
        }))
        .unwrap(),
        gas: swap_call_gas.attached(),
        deposit: NearToken::from_yoctonear(1),
    }));
    let swap_transactions = vec![ExecutionInstruction::NearTransaction {
        receiver_id: token_in,
        actions: vec![swap_action.clone()],
        is_fee_transaction: false,
    }];

    let (input_to_nep141, send_fees, input_nep141) = convert_to_nep141_with_fees(
        network,
        &request.token_in,
        request.trader_account_id.clone(),
        amount_in + total_fee_amount(&fees),
        &fees,
        &swap_referrer_id,
    )
    .await?;
    let token_output = if unwrapping_near {
        TokenId::Near
    } else {
        TokenId::Nep141(token_out)
    };
    let storage_deposit = deposit_storage_on_contract_if_needed(
        network,
        &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
        request.trader_account_id.clone(),
    )
    .await?;
    let transactions = [
        storage_deposit,
        deposit_storage_if_needed(network, &token_output, request.trader_account_id.clone())
            .await?,
        register_input_in_wallet(
            network,
            &request.token_in,
            input_nep141,
            request.trader_account_id.clone(),
        )
        .await?,
        input_to_nep141,
        swap_transactions,
        send_fees,
    ]
    .concat();
    let estimated_gas_paid = estimated_gas_paid(
        &transactions,
        Some((&swap_action, swap_call_gas.estimated_burn)),
        request.trader_account_id.as_ref(),
    );
    Some(Route {
        dex_id: DexId::RheaDcl,
        estimated_amount,

        worst_case_amount,
        execution_instructions: transactions,
        deprecated_needs_unwrap_always_false: false,
        discount: None,
        token_output,
        estimated_gas_paid,
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{
        Action, FunctionCallAction, Gas as AttachedGas, NearGas, NearToken, U128,
    };

    use super::*;
    use crate::shared_utils::{
        create_fee_ft_transfer_action, create_storage_deposit_action_for_contract,
        create_wrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;

    #[derive(Clone)]
    struct TestRheaDclQuotes {
        enabled: bool,
    }

    impl Default for TestRheaDclQuotes {
        fn default() -> Self {
            Self { enabled: true }
        }
    }

    impl TestRheaDclQuotes {
        fn none() -> Self {
            Self { enabled: false }
        }
    }

    fn step(pool_id: String, token_in: &AccountId, token_out: &AccountId) -> dcl::ApiPoolStep {
        dcl::ApiPoolStep {
            pool_id,
            token_in: token_in.clone(),
            token_out: token_out.clone(),
        }
    }

    impl RheaDclQuotes for TestRheaDclQuotes {
        async fn find_path(&self, request: dcl::Request) -> Option<RouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let (token_in, token_out) = (&request.token_in, &request.token_out);
            let usdc: AccountId = "usdc".parse().unwrap();
            let pools = vec![
                step(format!("{token_in}|usdc|100"), token_in, &usdc),
                step(format!("{token_out}|usdc|2000"), &usdc, token_out),
            ];
            match request.amount {
                QuoteAmount::ExactIn(amount_in) => Some(RouteApiResponse::ExactIn {
                    route: dcl::ExactInRoute {
                        pools,
                        amount_in: U128(amount_in),
                        min_amount_out: U128(79),
                        amount_out: U128(80),
                        swap_gas: Gas::from_ggas(20_000, 20_000),
                    },
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: U128(amount_in),
                    amount_out: U128(80),
                }),
                QuoteAmount::ExactOut(_) => Some(RouteApiResponse::ExactOut {
                    route: dcl::ExactOutRoute {
                        pools,
                        amount_in: U128(100),
                        max_amount_in: U128(101),
                        amount_out: U128(80),
                        swap_gas: Gas::from_ggas(20_000, 20_000),
                    },
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: U128(100),
                    max_amount_in: U128(101),
                    amount_out: U128(80),
                }),
            }
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                            NearToken::from_millinear(500),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &"ft".parse::<AccountId>().unwrap(),
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
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
                                        "pool_ids": vec!["wrap.near|usdc|100", "ft|usdc|2000"],
                                        "output_token": "ft",
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": true,
                                        "referral_id": DEFAULT_REFERRER_ID,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: AttachedGas(NearGas::from_ggas(81_800)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
                estimated_gas_paid: NearGas::from_gas(39_628_674_297_143),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                            NearToken::from_millinear(500),
                            true
                        )],
                        is_fee_transaction: false,
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
                                        "pool_ids": vec!["ft|usdc|100", "wrap.near|usdc|2000"],
                                        "output_token": WRAP_NEAR,
                                        "min_output_amount": "79",
                                        "skip_unwrap_near": false,
                                        "referral_id": DEFAULT_REFERRER_ID,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: AttachedGas(NearGas::from_ggas(81_800)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Near,
                estimated_gas_paid: NearGas::from_gas(34_191_344_930_531),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &RHEA_DCL_CONTRACT_ID.parse::<AccountId>().unwrap(),
                            NearToken::from_millinear(500),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &"ft".parse::<AccountId>().unwrap(),
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            "0.00125 NEAR".parse().unwrap(),
                            true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(101))],
                        is_fee_transaction: false,
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
                                        "pool_ids": vec!["ft|usdc|2000", "wrap.near|usdc|100"],
                                        "output_token": "ft",
                                        "output_amount": "80",
                                        "skip_unwrap_near": true,
                                        "referral_id": DEFAULT_REFERRER_ID,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: AttachedGas(NearGas::from_ggas(81_800)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
                estimated_gas_paid: NearGas::from_gas(39_628_873_975_739),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
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
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::none(),
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
                    amount: Amount::AmountOut(80),
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaDclQuotes::default(),
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::RheaDcl,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_wrap_action(NearToken::from_yoctonear(101))],
                        is_fee_transaction: false,
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
                                        "pool_ids": vec!["ft|usdc|2000", "wrap.near|usdc|100"],
                                        "output_token": "ft",
                                        "output_amount": "80",
                                        "skip_unwrap_near": true,
                                        "referral_id": DEFAULT_REFERRER_ID,
                                    }
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: AttachedGas(NearGas::from_ggas(81_800)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
                estimated_gas_paid: NearGas::from_gas(31_650_495_858_912),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_referrer_fee_is_on_top_of_estimated_input() {
        let route = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141("ft".parse().unwrap()),
                amount: Amount::AmountOut(80),
                slippage: Slippage::Fixed {
                    slippage: "0.01".parse().unwrap(),
                },
                dexes: None,
                trader_account_id: None,
                signing_public_key: None,
                referrer_id: None,
                fees: "{\"ref.near\":0.05}".parse().unwrap(),
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    WRAP_NEAR,
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaDclQuotes::default(),
        )
        .await
        .unwrap();
        // 5% of the estimated input with the fee: 100 * 5% / 95%, 80% of it to the referrer and
        // 20% to the protocol, each rounded down
        assert_eq!(route.estimated_amount, Amount::AmountIn(105));
        assert_eq!(route.worst_case_amount, Amount::AmountIn(106));
        assert_eq!(
            route.execution_instructions,
            vec![
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![create_wrap_action(NearToken::from_yoctonear(106))],
                    is_fee_transaction: false,
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
                                    "pool_ids": vec!["ft|usdc|2000", "wrap.near|usdc|100"],
                                    "output_token": "ft",
                                    "output_amount": "80",
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }
                            }))
                            .unwrap(),
                        }))
                        .unwrap(),
                        gas: AttachedGas(NearGas::from_ggas(81_800)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                    is_fee_transaction: false,
                },
                // After the swap
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![
                        create_fee_ft_transfer_action(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            &"ref.near".parse().unwrap(),
                            4,
                            DEFAULT_REFERRER_ID
                        ),
                        create_fee_ft_transfer_action(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            1,
                            DEFAULT_REFERRER_ID
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn amount_in_referrer_fee_comes_out_of_the_quoted_input() {
        let route = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141("ft".parse().unwrap()),
                amount: Amount::AmountIn(1000),
                slippage: Slippage::Fixed {
                    slippage: "0.01".parse().unwrap(),
                },
                dexes: None,
                trader_account_id: None,
                signing_public_key: None,
                referrer_id: None,
                fees: "{\"ref.near\":0.01}".parse().unwrap(),
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    WRAP_NEAR,
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaDclQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.execution_instructions[0],
            ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![create_wrap_action(NearToken::from_yoctonear(1000))],
                is_fee_transaction: false,
            }
        );
        // After the swap
        assert_eq!(
            route.execution_instructions[route.execution_instructions.len() - 1..],
            [
                // 1% of 1000, 80% to the referrer and 20% to the protocol
                ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    actions: vec![
                        create_fee_ft_transfer_action(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            &"ref.near".parse().unwrap(),
                            8,
                            DEFAULT_REFERRER_ID
                        ),
                        create_fee_ft_transfer_action(
                            &WRAP_NEAR.parse::<AccountId>().unwrap(),
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            2,
                            DEFAULT_REFERRER_ID
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn amount_out_referrer_fees_share_the_estimated_input() {
        let route = route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141("ft".parse().unwrap()),
                amount: Amount::AmountOut(80),
                slippage: Slippage::Fixed {
                    slippage: "0.01".parse().unwrap(),
                },
                dexes: None,
                trader_account_id: None,
                signing_public_key: None,
                referrer_id: None,
                fees: r#"{"ref.near":0.02,"ref2.near":0.03}"#.parse().unwrap(),
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    WRAP_NEAR,
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    "ref2.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    WRAP_NEAR,
                    AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaDclQuotes::default(),
        )
        .await
        .unwrap();
        // Each is its share of the estimated input with all fees: 100 * fee / 95% rounded down,
        // 80% of each fee to its referrer and the protocol's 20% of both together
        assert_eq!(route.estimated_amount, Amount::AmountIn(104));
        assert_eq!(route.worst_case_amount, Amount::AmountIn(105));
        assert_eq!(
            route.execution_instructions[0],
            ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![create_wrap_action(NearToken::from_yoctonear(105))],
                is_fee_transaction: false,
            }
        );
        // After the swap
        assert_eq!(
            route.execution_instructions[route.execution_instructions.len() - 1..],
            [ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![
                    create_fee_ft_transfer_action(
                        &WRAP_NEAR.parse::<AccountId>().unwrap(),
                        &"ref.near".parse().unwrap(),
                        1,
                        DEFAULT_REFERRER_ID
                    ),
                    create_fee_ft_transfer_action(
                        &WRAP_NEAR.parse::<AccountId>().unwrap(),
                        &"ref2.near".parse().unwrap(),
                        2,
                        DEFAULT_REFERRER_ID
                    ),
                    create_fee_ft_transfer_action(
                        &WRAP_NEAR.parse::<AccountId>().unwrap(),
                        &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                        1,
                        DEFAULT_REFERRER_ID
                    ),
                ],
                is_fee_transaction: true,
            },]
        );
    }
}
