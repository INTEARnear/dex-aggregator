use std::{future::Future, pin::Pin};

use near_min_api::types::{AccountId, NearGas, NearToken};
use pathfinder::{
    rhea::{self, SplitRouteApiResponse},
    MaxHops,
};
use tracing::info;

use crate::{
    route::{ProviderRoute, RouteCall, RouteTransaction},
    shared_utils::{
        base_token, convert_to_nep141, convert_to_nep141_with_fees,
        create_rhea_fee_mft_transfer_action, deposit_storage_if_needed, ft_transfer_call_gas,
        get_slippage, near_receive_gas, payable_referral_fees, price_raw, referrer_fees_of_total,
        register_input_in_wallet, rhea_fee_receivers_registration, rhea_registration,
        total_fee_amount, BaseTokenId, Mainnet, NetworkView, DEFAULT_REFERRER_ID,
        TOKEN_FT_TRANSFER_GAS,
    },
    types::{Gas, TokenId},
    Amount, DexId, Provider, SwapRequest,
};

pub struct RheaProvider;

pub const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";

const SWAP_GAS: Gas = Gas::from_ggas(2_800, 2_800);
const FT_ON_TRANSFER_GAS: Gas = Gas::from_ggas(3_600, 3_600);
const OUTPUT_FT_TRANSFER_ATTACHED: NearGas = NearGas::from_ggas(20_900);
const EXCHANGE_CALLBACK_POST_WITHDRAW_GAS: Gas = Gas::from_ggas(2_200, 5_900);
const OUTPUT_NEAR_WITHDRAW_GAS: Gas = Gas::from_ggas(2_000, 20_900);
const EXCHANGE_CALLBACK_POST_WITHDRAW_NEAR_GAS: Gas = Gas::from_ggas(2_600, 10_900);
const GAS_MARGIN: Gas = Gas::from_ggas(0, 3_000);
pub const STORAGE_DEPOSIT_GAS: Gas = Gas::from_ggas(2_610, 3_000);
pub const REGISTER_TOKENS_GAS: Gas = Gas::from_ggas(2_500, 3_000);
pub const REGISTERED_TOKEN_GAS: Gas = Gas::from_ggas(500, 500);
pub const MFT_TRANSFER_GAS: Gas = Gas::from_ggas(3_590, 4_000);
const WITHDRAW_GAS: Gas = Gas::from_ggas(3_700, 3_700);
pub const FT_ON_TRANSFER_DEPOSIT_GAS: Gas = Gas::from_ggas(2_890, 2_500);

/// Of `withdraw`ing a token from the trader's balance, as NEAR if `unwrap_near`
pub fn withdraw_gas(unwrap_near: bool, trader_account_id: Option<&AccountId>) -> Gas {
    WITHDRAW_GAS + output_gas(unwrap_near, trader_account_id)
}

/// Of sending a token out of the trader's balance, as NEAR if `unwrap_near`
fn output_gas(unwrap_near: bool, trader_account_id: Option<&AccountId>) -> Gas {
    if unwrap_near {
        OUTPUT_NEAR_WITHDRAW_GAS
            + EXCHANGE_CALLBACK_POST_WITHDRAW_NEAR_GAS
            + Gas {
                estimated_burn: near_receive_gas(trader_account_id).estimated_burn,
                needs_attached: NearGas::from_gas(0),
            }
    } else {
        Gas {
            estimated_burn: TOKEN_FT_TRANSFER_GAS.estimated_burn,
            needs_attached: OUTPUT_FT_TRANSFER_ATTACHED,
        } + EXCHANGE_CALLBACK_POST_WITHDRAW_GAS
    }
}

trait RheaQuotes: Send + Sync {
    fn find_path(
        &self,
        request: rhea::Request,
    ) -> impl Future<Output = Option<SplitRouteApiResponse>> + Send;
}

struct MainnetRheaQuotes;

impl RheaQuotes for MainnetRheaQuotes {
    async fn find_path(&self, request: rhea::Request) -> Option<SplitRouteApiResponse> {
        rhea::find_path(request)
            .await
            .inspect_err(|e| info!("No Rhea route: {e:#}"))
            .ok()
    }
}

impl Provider for RheaProvider {
    fn dex_id(&self) -> DexId {
        DexId::Rhea
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<ProviderRoute>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetRheaQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl RheaQuotes,
) -> Option<ProviderRoute> {
    let Amount::AmountIn(exact_amount_in) = request.amount else {
        // smartrouter.ref.finance/findPath doesn't support AmountOut
        return None;
    };

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

    // Fast path: swap directly within Rhea ledger if both in/out are Rhea balances
    let fast_path = matches!(request.token_in, TokenId::Nep141OnRhea(_))
        && matches!(request.token_out, TokenId::Nep141OnRhea(_));

    let swap_referrer_id = request
        .referrer_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string());

    let referrer_fees =
        referrer_fees_of_total(&payable_referral_fees(&request.fees), exact_amount_in);
    // The fees come out of the input, so the rest is what's quoted and swapped
    let swap_amount_in = exact_amount_in - total_fee_amount(&referrer_fees);

    let response = quotes
        .find_path(rhea::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: swap_amount_in,
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &BaseTokenId::Near)?,
            token_out_price_raw: price_raw(network, &base_token(&request.token_out).unwrap())?,
        })
        .await?;

    if response.routes.is_empty() {
        return None;
    }

    let total_min_amount_out = response
        .routes
        .iter()
        .map(|route| route.min_amount_out)
        .sum();
    let swap_gas = response.swap_gas;
    let steps = response
        .routes
        .into_iter()
        .flat_map(|route| route.pools)
        .collect::<Vec<_>>();
    let route_tokens = [&token_in]
        .into_iter()
        .chain(steps.iter().map(|step| &step.token_out))
        .collect::<Vec<_>>();
    let registration_transactions = match (&request.token_in, &request.trader_account_id) {
        (TokenId::Nep141OnRhea(_), Some(trader_account_id))
            if matches!(request.token_out, TokenId::Nep141OnRhea(_)) =>
        {
            rhea_registration(network, trader_account_id, &route_tokens).await?
        }
        _ => vec![],
    };

    let actions: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            let step = serde_json::to_value(step).unwrap();
            let mut new_step = step.clone();
            if let Some(pool) = step.get("pool_id") {
                if let Some(pool_str) = pool.as_str() {
                    if let Ok(pool_u64) = pool_str.parse::<u64>() {
                        new_step["pool_id"] = serde_json::Value::from(pool_u64);
                    }
                }
            }
            if let Some(amount_in) = step.get("amount_in") {
                if amount_in == "0" {
                    new_step.as_object_mut().unwrap().remove("amount_in");
                }
            }
            new_step
        })
        .collect();

    if fast_path {
        let swap_call = RouteCall::new(
            "swap",
            serde_json::json!({
                "actions": actions,
                "referral_id": swap_referrer_id,
                "skip_degen_price_sync": true,
            }),
            NearToken::from_yoctonear(1),
            SWAP_GAS + swap_gas + GAS_MARGIN,
        );

        let fee_receivers_registration =
            rhea_fee_receivers_registration(network, &token_in, &referrer_fees).await?;
        let fee_calls = referrer_fees
            .iter()
            .map(|fee| {
                create_rhea_fee_mft_transfer_action(
                    &token_in,
                    &fee.referrer_id,
                    fee.amount,
                    &swap_referrer_id,
                )
            })
            .collect();
        let transactions = [
            registration_transactions,
            vec![RouteTransaction {
                receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                calls: [vec![swap_call], fee_receivers_registration, fee_calls].concat(),
                is_fee_transaction: false,
            }],
        ]
        .concat();

        return Some(ProviderRoute {
            dex_id: DexId::Rhea,
            estimated_amount: Amount::AmountOut(response.amount_out),
            worst_case_amount: Amount::AmountOut(total_min_amount_out),
            transactions,
            discount: None,
            token_output: request.token_out.clone(),
        });
    }

    let unwrapping_near = request.token_out == TokenId::Near;
    let swap_call_gas = ft_transfer_call_gas(
        FT_ON_TRANSFER_GAS
            + swap_gas
            + output_gas(unwrapping_near, request.trader_account_id.as_ref())
            + GAS_MARGIN,
    );
    let ft_transfer_call_swap = RouteCall::new(
        "ft_transfer_call",
        serde_json::json!({
            "receiver_id": RHEA_CONTRACT_ID,
            "amount": swap_amount_in.to_string(),
            "msg": serde_json::to_string(&serde_json::json!({
                "force": 0,
                "actions": actions,
                "skip_degen_price_sync": true,
                "skip_unwrap_near": !unwrapping_near,
                "referral_id": swap_referrer_id,
            })).unwrap(),
        }),
        NearToken::from_yoctonear(1),
        swap_call_gas,
    );
    let swap_transactions = vec![RouteTransaction {
        receiver_id: token_in,
        calls: vec![ft_transfer_call_swap],
        is_fee_transaction: false,
    }];

    let (input_to_nep141, send_fees, input_nep141) = convert_to_nep141_with_fees(
        network,
        &request.token_in,
        request.trader_account_id.clone(),
        exact_amount_in,
        &referrer_fees,
        &swap_referrer_id,
    )
    .await?;

    let transactions = [
        deposit_storage_if_needed(
            network,
            &if unwrapping_near {
                TokenId::Near
            } else {
                TokenId::Nep141(token_out.clone())
            },
            request.trader_account_id.clone(),
        )
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

    Some(ProviderRoute {
        dex_id: DexId::Rhea,
        estimated_amount: Amount::AmountOut(response.amount_out),
        worst_case_amount: Amount::AmountOut(total_min_amount_out),
        transactions,
        discount: None,
        token_output: if unwrapping_near {
            TokenId::Near
        } else {
            TokenId::Nep141(token_out)
        },
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{NearGas, NearToken};

    use super::*;
    use crate::shared_utils::{
        create_fee_ft_transfer_action, create_intear_dex_withdraw_action,
        create_rhea_fee_mft_transfer_action, create_rhea_withdraw_action,
        create_storage_deposit_action_for_contract, create_storage_deposit_action_for_someone,
        create_wrap_action, TestNetworkView, TOKEN_STORAGE_DEPOSIT_GAS, WRAP_NEAR,
    };
    use crate::types::Slippage;
    use intear_dex_types::AssetId;
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;

    #[derive(Clone)]
    struct TestRheaQuotes {
        enabled: bool,
        empty_routes: bool,
    }

    impl Default for TestRheaQuotes {
        fn default() -> Self {
            Self {
                enabled: true,
                empty_routes: false,
            }
        }
    }

    impl TestRheaQuotes {
        fn none() -> Self {
            Self {
                enabled: false,
                empty_routes: false,
            }
        }

        fn empty_routes() -> Self {
            Self {
                enabled: true,
                empty_routes: true,
            }
        }
    }

    impl RheaQuotes for TestRheaQuotes {
        async fn find_path(&self, request: rhea::Request) -> Option<SplitRouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let routes = if self.empty_routes {
                vec![]
            } else {
                vec![rhea::ApiResponseRoute {
                    pools: vec![rhea::ApiResponsePoolStep {
                        pool_id: 1,
                        token_in: request.token_in.clone(),
                        token_out: request.token_out.clone(),
                        amount_in: request.amount_in,
                        amount_out: 0,
                        min_amount_out: 79,
                    }],
                    amount_in: request.amount_in,
                    min_amount_out: 79,
                    amount_out: 0,
                }]
            };
            Some(SplitRouteApiResponse {
                routes,
                contract_in: request.token_in,
                contract_out: request.token_out,
                amount_in: request.amount_in,
                amount_out: 80,
                swap_gas: Gas::from_ggas(5_500, 5_500),
            })
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        calls: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            false,
                            None
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "dex.intear.near".parse().unwrap(),
                        calls: vec![create_intear_dex_withdraw_action(&AssetId::Near, 100, None)],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_to_rhea_fast_path() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestNetworkView::default()
                    .with_test_prices()
                    .with_storage(
                        RHEA_CONTRACT_ID,
                        "trader.near",
                        NearToken::from_millinear(100),
                        NearToken::from_micronear(500),
                    )
                    .with_rhea_registered("trader.near", "ft", true),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            create_storage_deposit_action_for_contract(
                                "0.00098 NEAR".parse().unwrap(),
                                false,
                                STORAGE_DEPOSIT_GAS
                            ),
                            RouteCall {
                                method_name: "register_tokens".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "token_ids": ["other"],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_000, 3_500),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "swap".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "actions": vec![serde_json::json!({
                                    "pool_id": 1,
                                    "token_in": "ft",
                                    "token_out": "other",
                                    "amount_in": "100",
                                    "amount_out": "0",
                                    "min_amount_out": "79",
                                })],
                                "referral_id": DEFAULT_REFERRER_ID,
                                "skip_degen_price_sync": true,
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(8_300, 11_300),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![RouteCall {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": RHEA_CONTRACT_ID,
                            "amount": "100",
                            "msg": serde_json::to_string(&serde_json::json!({
                                "force": 0,
                                "actions": vec![serde_json::json!({
                                    "pool_id": 1,
                                    "token_in": "ft",
                                    "token_out": WRAP_NEAR,
                                    "amount_in": "100",
                                    "amount_out": "0",
                                    "min_amount_out": "79",
                                })],
                                "skip_degen_price_sync": true,
                                "skip_unwrap_near": false,
                                "referral_id": DEFAULT_REFERRER_ID,
                            }))
                            .unwrap(),
                        }))
                        .unwrap(),
                        gas: Gas {
                            estimated_burn: NearGas::from_gas(19_323_182_562_500),
                            needs_attached: NearGas::from_gas(74_900_000_000_000)
                        },
                        deposit: NearToken::from_yoctonear(1),
                    }],
                    is_fee_transaction: false,
                },],
                discount: None,
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                        calls: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                            None
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": "ft",
                                        "token_out": WRAP_NEAR,
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": false,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas {
                                estimated_burn: NearGas::from_gas(19_323_182_562_500),
                                needs_attached: NearGas::from_gas(74_900_000_000_000)
                            },
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_rhea_ft_outputs_wallet_nep141() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
                    token_out: TokenId::Nep141OnRhea("other".parse().unwrap()),
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: "other".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": "ft",
                                        "token_out": "other",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Nep141("other".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_returns_none() {
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
                &TestRheaQuotes::default(),
            )
            .await,
            None
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
                &TestRheaQuotes::default(),
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
                &TestRheaQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn empty_routes_returns_none() {
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
                &TestRheaQuotes::empty_routes(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn unpriced_token_out_has_no_route() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("unpriced".parse().unwrap()),
                    amount: Amount::AmountIn(100),
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
                &TestRheaQuotes::default(),
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
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_swap_msg() {
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
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: Some("ref.near".parse().unwrap()),
                    fees: Default::default(),
                },
                &TestNetworkView::default().with_test_prices(),
                &TestRheaQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Rhea,
                transactions: vec![
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_wrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": RHEA_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "force": 0,
                                    "actions": vec![serde_json::json!({
                                        "pool_id": 1,
                                        "token_in": WRAP_NEAR,
                                        "token_out": "ft",
                                        "amount_in": "100",
                                        "amount_out": "0",
                                        "min_amount_out": "79",
                                    })],
                                    "skip_degen_price_sync": true,
                                    "skip_unwrap_near": true,
                                    "referral_id": "ref.near",
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(18_700, 69_900),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    fn rhea_swap_msg(
        token_in: &str,
        token_out: &str,
        amount_in: &str,
        referral_id: &str,
    ) -> String {
        serde_json::to_string(&serde_json::json!({
            "force": 0,
            "actions": vec![serde_json::json!({
                "pool_id": 1,
                "token_in": token_in,
                "token_out": token_out,
                "amount_in": amount_in,
                "amount_out": "0",
                "min_amount_out": "79",
            })],
            "skip_degen_price_sync": true,
            "skip_unwrap_near": true,
            "referral_id": referral_id,
        }))
        .unwrap()
    }

    fn ft_transfer_call_to_rhea(amount: &str, msg: String) -> RouteCall {
        RouteCall {
            method_name: "ft_transfer_call".to_string(),
            args: serde_json::to_vec(&serde_json::json!({
                "receiver_id": RHEA_CONTRACT_ID,
                "amount": amount,
                "msg": msg,
            }))
            .unwrap(),
            gas: Gas::from_ggas(18_700, 69_900),
            deposit: NearToken::from_yoctonear(1),
        }
    }

    fn referred_request(token_in: TokenId, token_out: TokenId) -> SwapRequest {
        SwapRequest {
            token_in,
            token_out,
            amount: Amount::AmountIn(1000),
            slippage: Slippage::Fixed {
                slippage: "0.01".parse().unwrap(),
            },
            dexes: None,
            trader_account_id: None,
            signing_public_key: None,
            referrer_id: Some("ref.near".parse().unwrap()),
            fees: "{\"ref.near\":0.01}".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn referrer_fee_in_near_is_sent_as_wnear_after_the_swap() {
        let route = route(
            referred_request(TokenId::Near, TokenId::Nep141("ft".parse().unwrap())),
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
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions,
            vec![
                // All of the input is wrapped, the fees too
                RouteTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    calls: vec![create_wrap_action(NearToken::from_yoctonear(1000))],
                    is_fee_transaction: false,
                },
                RouteTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    calls: vec![ft_transfer_call_to_rhea(
                        "990",
                        rhea_swap_msg(WRAP_NEAR, "ft", "990", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                // After the swap, 1% of 1000, 80% to the referrer and 20% to the protocol
                RouteTransaction {
                    receiver_id: WRAP_NEAR.parse().unwrap(),
                    calls: vec![
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8, "ref.near"),
                        create_fee_ft_transfer_action(
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            2,
                            "ref.near"
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn referrer_fee_in_ft_is_transferred_after_the_swap() {
        let route = route(
            referred_request(
                TokenId::Nep141("ft".parse().unwrap()),
                TokenId::Nep141("other".parse().unwrap()),
            ),
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    "ft",
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions,
            vec![
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![ft_transfer_call_to_rhea(
                        "990",
                        rhea_swap_msg("ft", "other", "990", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8, "ref.near"),
                        create_fee_ft_transfer_action(
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            2,
                            "ref.near"
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn referrer_without_storage_on_input_is_registered_before_its_fee() {
        let route = route(
            referred_request(
                TokenId::Nep141("ft".parse().unwrap()),
                TokenId::Nep141("other".parse().unwrap()),
            ),
            &TestNetworkView::default().with_test_prices(),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions,
            vec![
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![ft_transfer_call_to_rhea(
                        "990",
                        rhea_swap_msg("ft", "other", "990", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![
                        create_storage_deposit_action_for_someone(
                            "0.00125 NEAR".parse().unwrap(),
                            &"ref.near".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        ),
                        create_storage_deposit_action_for_someone(
                            "0.00125 NEAR".parse().unwrap(),
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        ),
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8, "ref.near"),
                        create_fee_ft_transfer_action(
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            2,
                            "ref.near"
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn referrer_fee_on_fast_path_moves_rhea_balance() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true)
                .with_rhea_registered("ref.near", "ft", true)
                .with_rhea_registered(AGGREGATOR_FEE_RECEIVER_ID.as_str(), "ft", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions,
            vec![RouteTransaction {
                receiver_id: RHEA_CONTRACT_ID.parse().unwrap(),
                calls: vec![
                    RouteCall {
                        method_name: "swap".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "actions": vec![serde_json::json!({
                                "pool_id": 1,
                                "token_in": "ft",
                                "token_out": "other",
                                "amount_in": "990",
                                "amount_out": "0",
                                "min_amount_out": "79",
                            })],
                            "referral_id": "ref.near",
                            "skip_degen_price_sync": true,
                        }))
                        .unwrap(),
                        gas: Gas::from_ggas(8_300, 11_300),
                        deposit: NearToken::from_yoctonear(1),
                    },
                    create_rhea_fee_mft_transfer_action(
                        &"ft".parse().unwrap(),
                        &"ref.near".parse().unwrap(),
                        8,
                        "ref.near",
                    ),
                    create_rhea_fee_mft_transfer_action(
                        &"ft".parse().unwrap(),
                        &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                        2,
                        "ref.near",
                    ),
                ],
                is_fee_transaction: false,
            }]
        );
    }

    #[tokio::test]
    async fn referrer_without_rhea_registration_gets_storage_for_its_fee_on_fast_path() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        let RouteTransaction { calls, .. } = &route.transactions[0];
        // A new Rhea account with a balance of one token
        let account_with_token = NearToken::from_yoctonear(250 * 10u128.pow(19));
        assert_eq!(
            calls[1..],
            [
                create_storage_deposit_action_for_someone(
                    account_with_token,
                    &"ref.near".parse().unwrap(),
                    false,
                    STORAGE_DEPOSIT_GAS
                ),
                create_storage_deposit_action_for_someone(
                    account_with_token,
                    &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    false,
                    STORAGE_DEPOSIT_GAS
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"ref.near".parse().unwrap(),
                    8,
                    "ref.near",
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    2,
                    "ref.near",
                ),
            ]
        );
    }

    #[tokio::test]
    async fn each_referrer_gets_its_fee() {
        let route = route(
            SwapRequest {
                fees: r#"{"ref.near":0.01,"ref2.near":0.02,"nostorage.near":0.01}"#
                    .parse()
                    .unwrap(),
                ..referred_request(
                    TokenId::Nep141("ft".parse().unwrap()),
                    TokenId::Nep141("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    "ft",
                    "ref.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    "ref2.near",
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                )
                .with_storage(
                    "ft",
                    AGGREGATOR_FEE_RECEIVER_ID.as_str(),
                    "0.00125 NEAR".parse().unwrap(),
                    NearToken::from_yoctonear(0),
                ),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions,
            vec![
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    calls: vec![ft_transfer_call_to_rhea(
                        "960",
                        rhea_swap_msg("ft", "other", "960", "ref.near"),
                    )],
                    is_fee_transaction: false,
                },
                RouteTransaction {
                    receiver_id: "ft".parse().unwrap(),
                    // 80% of each fee to its referrer, the protocol's 20% of all of them together
                    calls: vec![
                        create_storage_deposit_action_for_someone(
                            "0.00125 NEAR".parse().unwrap(),
                            &"nostorage.near".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS
                        ),
                        create_fee_ft_transfer_action(
                            &"nostorage.near".parse().unwrap(),
                            8,
                            "ref.near"
                        ),
                        create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8, "ref.near"),
                        create_fee_ft_transfer_action(
                            &"ref2.near".parse().unwrap(),
                            16,
                            "ref.near"
                        ),
                        create_fee_ft_transfer_action(
                            &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                            8,
                            "ref.near"
                        ),
                    ],
                    is_fee_transaction: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn each_referrer_fee_in_near_is_its_own_wnear_transfer() {
        let route = route(
            SwapRequest {
                fees: r#"{"ref.near":0.01,"ref2.near":0.02}"#.parse().unwrap(),
                ..referred_request(TokenId::Near, TokenId::Nep141("ft".parse().unwrap()))
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
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            route.transactions[0],
            RouteTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                calls: vec![create_wrap_action(NearToken::from_yoctonear(1000))],
                is_fee_transaction: false,
            }
        );
        // After the swap
        assert_eq!(
            route.transactions.last().unwrap(),
            &RouteTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                calls: vec![
                    create_fee_ft_transfer_action(&"ref.near".parse().unwrap(), 8, "ref.near"),
                    create_fee_ft_transfer_action(&"ref2.near".parse().unwrap(), 16, "ref.near"),
                    create_fee_ft_transfer_action(
                        &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                        6,
                        "ref.near"
                    ),
                ],
                is_fee_transaction: true,
            }
        );
    }

    #[tokio::test]
    async fn fast_path_moves_rhea_balance_to_each_referrer() {
        let route = route(
            SwapRequest {
                trader_account_id: Some("trader.near".parse().unwrap()),
                fees: r#"{"ref.near":0.01,"ref2.near":0.02,"unregistered.near":0.01}"#
                    .parse()
                    .unwrap(),
                ..referred_request(
                    TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    TokenId::Nep141OnRhea("other".parse().unwrap()),
                )
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_rhea_registered("trader.near", "ft", true)
                .with_rhea_registered("trader.near", "other", true)
                .with_rhea_registered("ref.near", "ft", true)
                .with_rhea_registered("ref2.near", "ft", true)
                .with_rhea_registered(AGGREGATOR_FEE_RECEIVER_ID.as_str(), "ft", true),
            &TestRheaQuotes::default(),
        )
        .await
        .unwrap();
        let RouteTransaction { calls, .. } = &route.transactions[0];
        assert_eq!(
            calls[1..],
            [
                create_storage_deposit_action_for_someone(
                    NearToken::from_yoctonear(250 * 10u128.pow(19)),
                    &"unregistered.near".parse().unwrap(),
                    false,
                    STORAGE_DEPOSIT_GAS
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"ref.near".parse().unwrap(),
                    8,
                    "ref.near",
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"ref2.near".parse().unwrap(),
                    16,
                    "ref.near",
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &"unregistered.near".parse().unwrap(),
                    8,
                    "ref.near",
                ),
                create_rhea_fee_mft_transfer_action(
                    &"ft".parse().unwrap(),
                    &AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    8,
                    "ref.near",
                ),
            ]
        );
    }
}
