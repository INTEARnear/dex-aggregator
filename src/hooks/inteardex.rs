use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use intear_dex_types::{
    AssetId, Operation, ReferralFees, SwapAdvancedMode, SwapOperationAmount, SwapOption,
    SwapRequestAmount, WithdrawAmount,
};
use near_min_api::types::{AccountId, NearGas, NearToken};
use near_sdk::json_types::U128;
use swap_router_types::{
    hook::{HookQuote, IntearDexTrade},
    types::Gas,
};

use crate::{
    route::{ProviderRoute, RouteCall, RouteTransaction},
    shared_utils::{
        base_token, convert_to_native, convert_to_nep141, deposit_storage_if_needed,
        ft_transfer_call_gas, get_slippage, is_near, near_receive_gas, payable_referral_fees,
        register_assets_on_intear, register_input_in_wallet, NetworkView, DEFAULT_REFERRER_ID,
        TOKEN_FT_TRANSFER_GAS,
    },
    types::TokenId,
    Amount, DexId, SwapRequest,
};

pub const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";

/// Of the engine called directly, with `execute_operations` or `deposit_near`
const ENGINE_CALL_GAS: Gas = Gas::from_ggas(3_600, 3_600);
const ENGINE_FT_ON_TRANSFER_GAS: Gas = Gas::from_ggas(2_600, 2_600);
const WITHDRAWAL_GAS: Gas = Gas::from_ggas(500, 500);
const REFERRAL_FEE_GAS: Gas = Gas::from_ggas(600, 600);
const AFTER_WITHDRAW_GAS: Gas = Gas::from_ggas(2_900, 5_900);
const WITHDRAWAL_FT_TRANSFER_ATTACHED: NearGas = NearGas::from_ggas(10_900);
const GAS_MARGIN: Gas = Gas::from_ggas(0, 3_000);

pub const STORAGE_DEPOSIT_GAS: Gas = Gas::from_ggas(3_080, 3_500);
pub const REGISTER_ASSETS_GAS: Gas = Gas::from_ggas(3_270, 3_770);
pub const REGISTERED_ASSET_GAS: Gas = Gas::from_ggas(340, 340);
pub const DEPOSIT_NEAR_GAS: Gas = Gas::from_ggas(3_720, 4_000);
const DIRECT_WITHDRAW_GAS: Gas = Gas::from_ggas(4_400, 4_400);
pub const FT_ON_TRANSFER_DEPOSIT_GAS: Gas = Gas::from_ggas(3_670, 3_300);

/// Of `withdraw`ing an asset from the trader's balance
pub fn withdraw_gas(asset_id: &AssetId, trader_account_id: Option<&AccountId>) -> Gas {
    DIRECT_WITHDRAW_GAS + sending_out_gas(asset_id, trader_account_id)
}

fn sending_out_gas(asset_id: &AssetId, trader_account_id: Option<&AccountId>) -> Gas {
    AFTER_WITHDRAW_GAS
        + match asset_id {
            AssetId::Near => near_receive_gas(trader_account_id),
            AssetId::Nep141(_) => Gas {
                estimated_burn: TOKEN_FT_TRANSFER_GAS.estimated_burn,
                needs_attached: WITHDRAWAL_FT_TRANSFER_ATTACHED,
            },
            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
        }
}

/// Of the engine's call that swaps, `engine_gas` without the swap and the operations around it
fn swap_call_gas(
    engine_gas: Gas,
    swap_gas: Gas,
    operations: &[Operation],
    referral_fees: &ReferralFees,
    trader_account_id: Option<&AccountId>,
) -> Gas {
    let withdrawals = operations
        .iter()
        .filter_map(|operation| match operation {
            Operation::Withdraw { asset_id, .. } => {
                Some(WITHDRAWAL_GAS + sending_out_gas(asset_id, trader_account_id))
            }
            _ => None,
        })
        .sum::<Gas>();
    engine_gas
        + REFERRAL_FEE_GAS * referral_fees.0.len() as u64
        + swap_gas
        + withdrawals
        + GAS_MARGIN
}

/// The swap on Intear DEX that the request goes through, which hooks quote. None if its tokens
/// can't be swapped there.
pub async fn trade_of_request(
    network: &impl NetworkView,
    request: &SwapRequest,
) -> Option<IntearDexTrade> {
    let amount_hint = match request.amount {
        Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
    };
    let asset_in = intear_dex_asset(&request.token_in, amount_hint).await?;
    let asset_out = intear_dex_asset(&request.token_out, amount_hint).await?;
    if asset_in == asset_out {
        return None;
    }
    let slippage = get_slippage(
        network,
        request.slippage.clone(),
        &base_token(&request.token_in)?,
        &base_token(&request.token_out)?,
    );
    // The engine takes them out of the input before the DEX gets it
    let referral_fees = ReferralFees::from(&payable_referral_fees(&request.fees));
    let amount = match request.amount {
        Amount::AmountIn(amount_in) => {
            Amount::AmountIn(amount_in - fees_total(referral_fees.amounts_out_of(U128(amount_in))))
        }
        Amount::AmountOut(amount_out) => Amount::AmountOut(amount_out),
    };
    Some(IntearDexTrade {
        asset_in,
        asset_out,
        amount,
        slippage,
        trader_account_id: request.trader_account_id.clone(),
    })
}

/// The asset the token is swapped as on Intear DEX: NEAR and wNEAR as NEAR, a token outside of it
/// as the NEP-141 that the route converts it to
async fn intear_dex_asset(token_id: &TokenId, amount_hint: u128) -> Option<AssetId> {
    match token_id {
        TokenId::TokenOnIntearDex(asset_id) => Some(asset_id.clone()),
        token_id if is_near(token_id) => {
            convert_to_native(token_id, None, NearToken::from_yoctonear(amount_hint)).await?;
            Some(AssetId::Near)
        }
        token_id => convert_to_nep141(token_id, None, 0)
            .await
            .map(|(_, nep141)| AssetId::Nep141(nep141)),
    }
}

fn fees_total(fees: Vec<(AccountId, U128)>) -> u128 {
    fees.iter().map(|(_, fee)| fee.0).sum()
}

/// A swap on Intear DEX for an [`IntearDexTrade`]: on one DEX, or on the first of several that
/// fills it within the trade's slippage
pub struct IntearDexSwap {
    /// Best first
    options: Vec<SwapOption>,
    /// Of the first option, as in [`HookQuote`]
    estimated_amount: u128,
    /// The least the swap gives, or the most it takes, as in [`HookQuote`]
    worst_case_amount: u128,
    /// Burns what the first option does, which usually fills the swap, and needs attached what all
    /// options do, which the engine tries until one fills it
    gas: Gas,
    discount: Option<U128>,
}

impl IntearDexSwap {
    pub fn of_quote(quote: HookQuote) -> Self {
        Self {
            options: vec![SwapOption {
                dex_id: quote.dex_id,
                message: quote.message,
            }],
            estimated_amount: quote.estimated_amount.0,
            worst_case_amount: quote.worst_case_amount.0,
            gas: quote.gas,
            discount: quote.discount,
        }
    }

    /// The quotes, best first, as options that the engine falls back through when one fails or
    /// doesn't meet the trade's slippage from the best one. None without quotes, one quote is
    /// swapped on alone.
    pub fn fallback_queue(trade: &IntearDexTrade, mut quotes: Vec<HookQuote>) -> Option<Self> {
        match trade.amount {
            Amount::AmountIn(_) => {
                quotes.sort_by_key(|quote| std::cmp::Reverse(quote.estimated_amount.0))
            }
            Amount::AmountOut(_) => quotes.sort_by_key(|quote| quote.estimated_amount.0),
        }
        if quotes.len() < 2 {
            return quotes.into_iter().next().map(Self::of_quote);
        }
        let best = &quotes[0];
        let estimated_amount = BigDecimal::from(best.estimated_amount.0);
        let worst_case_amount = match trade.amount {
            Amount::AmountIn(_) => (estimated_amount * (BigDecimal::from(1) - &trade.slippage))
                .with_scale_round(0, RoundingMode::Down),
            Amount::AmountOut(_) => (estimated_amount * (BigDecimal::from(1) + &trade.slippage))
                .with_scale_round(0, RoundingMode::Up),
        }
        .to_u128()?;
        Some(Self {
            estimated_amount: best.estimated_amount.0,
            worst_case_amount,
            gas: Gas {
                estimated_burn: best.gas.estimated_burn,
                needs_attached: NearGas::from_gas(
                    quotes
                        .iter()
                        .map(|quote| quote.gas.needs_attached.as_gas())
                        .sum(),
                ),
            },
            discount: best.discount,
            options: quotes
                .into_iter()
                .map(|quote| SwapOption {
                    dex_id: quote.dex_id,
                    message: quote.message,
                })
                .collect(),
        })
    }
}

/// The route of a swap for `trade`, which [`trade_of_request`] made of `request`
pub async fn route(
    network: &impl NetworkView,
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swap: IntearDexSwap,
) -> Option<ProviderRoute> {
    let token_in = trade.asset_in.clone();
    let token_out = trade.asset_out.clone();
    let referral_fees = ReferralFees::from(&payable_referral_fees(&request.fees));
    let discount = swap.discount.map(|discount| match request.amount {
        Amount::AmountIn(_) => Amount::AmountOut(discount.0),
        Amount::AmountOut(_) => Amount::AmountIn(discount.0),
    });
    // `swap_amount` is what the engine takes with the fees
    let (swap_amount, constraint, estimated_amount, worst_case_amount) = match request.amount {
        Amount::AmountIn(amount_in_with_fees) => (
            SwapRequestAmount::ExactIn(U128(amount_in_with_fees)),
            swap.worst_case_amount,
            Amount::AmountOut(swap.estimated_amount),
            Amount::AmountOut(swap.worst_case_amount),
        ),
        Amount::AmountOut(amount_out) => {
            // Fees on top of the input the DEX takes, so the most it can take with them is the
            // constraint
            let with_fees = |amount_in: u128| {
                amount_in + fees_total(referral_fees.amounts_on_top_of(U128(amount_in)))
            };
            (
                SwapRequestAmount::ExactOut(U128(amount_out)),
                with_fees(swap.worst_case_amount),
                Amount::AmountIn(with_fees(swap.estimated_amount)),
                Amount::AmountIn(with_fees(swap.worst_case_amount)),
            )
        }
    };
    let swap_gas = swap.gas;
    let mut options = swap.options;
    let swaps = vec![if options.len() == 1 {
        let SwapOption { dex_id, message } = options.remove(0);
        Operation::SwapSimple {
            dex_id,
            message,
            asset_in: token_in.clone(),
            asset_out: token_out.clone(),
            amount: SwapOperationAmount::Amount(swap_amount),
            constraint: Some(U128(constraint)),
        }
    } else {
        Operation::SwapAdvanced {
            asset_in: token_in.clone(),
            asset_out: token_out.clone(),
            amount: SwapOperationAmount::Amount(swap_amount),
            constraint: Some(U128(constraint)),
            options,
            mode: SwapAdvancedMode::FallbackQueue,
        }
    }];

    let use_fast_path = matches!(request.token_in, TokenId::TokenOnIntearDex(_));

    let mut operations = swaps;
    let output_withdrawn = if use_fast_path {
        // Unlike the deposit paths, `execute_operations` reads `Full` from the
        // trader's whole inner balance, which would also sweep tokens they held
        // before the swap. So the output is only withdrawn when its amount can
        // be named exactly, and otherwise left on the DEX for the caller to
        // convert once the received amount is known.
        let exact_output = match request.amount {
            Amount::AmountOut(amount_out) => Some(WithdrawAmount::Exact(U128::from(amount_out))),
            Amount::AmountIn(_) => None,
        };
        match exact_output {
            Some(amount) if !matches!(request.token_out, TokenId::TokenOnIntearDex(_)) => {
                operations.push(Operation::Withdraw {
                    asset_id: token_out.clone(),
                    amount,
                    to: None,
                    rescue_address: None,
                });
                true
            }
            _ => false,
        }
    } else {
        operations.push(Operation::Withdraw {
            asset_id: token_out.clone(),
            amount: WithdrawAmount::Full {
                at_least: Some(match request.amount {
                    Amount::AmountIn(_) => U128::from(constraint),
                    Amount::AmountOut(amount_out) => U128::from(amount_out),
                }),
            },
            to: None,
            rescue_address: None,
        });
        if matches!(request.amount, Amount::AmountOut(_)) {
            operations.push(Operation::Withdraw {
                asset_id: token_in.clone(),
                amount: WithdrawAmount::Full { at_least: None },
                to: None,
                rescue_address: None,
            });
        }
        true
    };

    // The fast path swaps from the input's balance, and exact-out withdraws what's left of the
    // input, so only exact-in deposits are swapped whole without a balance of the input
    let registered_assets: &[&AssetId] =
        if use_fast_path || matches!(request.amount, Amount::AmountOut(_)) {
            &[&token_in, &token_out]
        } else {
            &[&token_out]
        };
    let fee_receivers = referral_fees
        .0
        .iter()
        .map(|(receiver_id, _)| (receiver_id, &token_in))
        .collect::<Vec<_>>();
    let registration_transactions = match request.trader_account_id.as_ref() {
        Some(trader_account_id) => {
            register_assets_on_intear(
                network,
                trader_account_id,
                registered_assets,
                &fee_receivers,
            )
            .await?
        }
        None => vec![],
    };

    let input_amount = match request.amount {
        Amount::AmountIn(amount_in) => amount_in,
        Amount::AmountOut(_) => constraint,
    };

    let call_gas = |engine_gas| {
        swap_call_gas(
            engine_gas,
            swap_gas,
            &operations,
            &referral_fees,
            request.trader_account_id.as_ref(),
        )
    };
    let transactions = if use_fast_path {
        let execute_operations_call = RouteCall::new(
            "execute_operations",
            serde_json::json!({
                "operations": operations,
                "referrer": request.referrer_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                "referral_fees": referral_fees,
            }),
            NearToken::from_yoctonear(1),
            call_gas(ENGINE_CALL_GAS),
        );
        let output_storage = if output_withdrawn {
            deposit_storage_if_needed(
                network,
                &match &token_out {
                    AssetId::Near => TokenId::Near,
                    AssetId::Nep141(token_id) => TokenId::Nep141(token_id.clone()),
                    AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                },
                request.trader_account_id.clone(),
            )
            .await?
        } else {
            vec![]
        };
        [
            output_storage,
            vec![RouteTransaction {
                receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                calls: vec![execute_operations_call],
                is_fee_transaction: false,
            }],
        ]
        .concat()
    } else {
        match token_in {
            AssetId::Near => {
                let input_to_native = convert_to_native(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    NearToken::from_yoctonear(input_amount),
                )
                .await?;
                let deposit_near_call = RouteCall::new(
                    "deposit_near",
                    serde_json::json!({
                        "operations": {
                            "operations": operations,
                            "referrer": request.referrer_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                            "referral_fees": referral_fees,
                        },
                    }),
                    NearToken::from_yoctonear(input_amount),
                    call_gas(ENGINE_CALL_GAS),
                );
                let swap_transactions = vec![RouteTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    calls: vec![deposit_near_call],
                    is_fee_transaction: false,
                }];
                [
                    deposit_storage_if_needed(
                        network,
                        &match &token_out {
                            AssetId::Near => TokenId::Near,
                            AssetId::Nep141(token_id) => TokenId::Nep141(token_id.clone()),
                            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                        },
                        request.trader_account_id.clone(),
                    )
                    .await?,
                    deposit_storage_if_needed(
                        network,
                        &TokenId::Near,
                        request.trader_account_id.clone(),
                    )
                    .await?,
                    input_to_native,
                    swap_transactions,
                ]
                .concat()
            }
            AssetId::Nep141(token_in_id) => {
                let ft_transfer_call_swap = RouteCall::new(
                    "ft_transfer_call",
                    serde_json::json!({
                        "receiver_id": INTEAR_DEX_CONTRACT_ID,
                        "amount": input_amount.to_string(),
                        "msg": serde_json::to_string(&serde_json::json!({
                            "operations": operations,
                            "referrer": request.referrer_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                            "referral_fees": referral_fees,
                        })).unwrap(),
                    }),
                    NearToken::from_yoctonear(1),
                    ft_transfer_call_gas(call_gas(ENGINE_FT_ON_TRANSFER_GAS)),
                );
                let swap_transactions = vec![RouteTransaction {
                    receiver_id: token_in_id.clone(),
                    calls: vec![ft_transfer_call_swap],
                    is_fee_transaction: false,
                }];

                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    input_amount,
                )
                .await?;
                assert_eq!(token_in_id, input_nep141);

                [
                    deposit_storage_if_needed(
                        network,
                        &match &token_out {
                            AssetId::Near => TokenId::Near,
                            AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
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
                ]
                .concat()
            }
            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => {
                unreachable!()
            }
        }
    };

    Some(ProviderRoute {
        dex_id,
        estimated_amount,
        worst_case_amount,
        transactions: [registration_transactions, transactions].concat(),
        discount,
        token_output: if output_withdrawn {
            match token_out {
                AssetId::Near => TokenId::Near,
                AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
            }
        } else {
            TokenId::TokenOnIntearDex(token_out)
        },
    })
}

#[cfg(test)]
mod tests {
    /// trader.near (11 bytes) with balances of `near` (1 byte) and `nep141:ft` (7 bytes)
    const NEW_TRADER_NEAR_AND_FT_STORAGE: NearToken = NearToken::from_yoctonear(
        ((77 + 11) + (69 + 3 * 11) + (142 + 11 + 1) + (142 + 11 + 7)) * 10u128.pow(19),
    );

    use near_min_api::types::{NearGas, NearToken};

    use near_sdk::json_types::Base64VecU8;
    use pathfinder::plach::{self, SplitRouteApiResponse};
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;
    use xyk_dex_types::{SwapArgs, SwapRoute};

    use super::*;
    use crate::shared_utils::{
        create_rhea_withdraw_action, create_storage_deposit_action_for_contract,
        create_unwrap_action, TestNetworkView, TOKEN_STORAGE_DEPOSIT_GAS, WRAP_NEAR,
    };
    use crate::types::Slippage;

    const PLACH_DEX_ID: &str = "slimedragon.near/xyk";

    #[derive(Clone)]
    struct TestPlachQuotes {
        enabled: bool,
        empty_routes: bool,
    }

    impl Default for TestPlachQuotes {
        fn default() -> Self {
            Self {
                enabled: true,
                empty_routes: false,
            }
        }
    }

    impl TestPlachQuotes {
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

    impl TestPlachQuotes {
        fn find_path(&self, trade: &IntearDexTrade) -> Option<SplitRouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let (token_in, token_out) = (&trade.asset_in, &trade.asset_out);
            if self.empty_routes {
                return Some(SplitRouteApiResponse::ExactIn {
                    routes: vec![],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: near_min_api::types::U128(100),
                    amount_out: near_min_api::types::U128(80),
                    discount: near_min_api::types::U128(0),
                    swap_gas: Gas::from_ggas(34_000, 34_000),
                });
            }
            let discount = near_min_api::types::U128(0);
            match trade.amount {
                Amount::AmountIn(amount_in) => Some(SplitRouteApiResponse::ExactIn {
                    routes: vec![plach::ExactInRoute {
                        split_fraction: xyk_dex_types::FULL_SPLIT_FRACTION,
                        pools: vec![plach::ExactInPoolStep {
                            pool_id: 1,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            amount_in: near_min_api::types::U128(amount_in),
                            min_amount_out: near_min_api::types::U128(79),
                        }],
                        amount_in: near_min_api::types::U128(amount_in),
                        min_amount_out: near_min_api::types::U128(79),
                        amount_out: near_min_api::types::U128(80),
                    }],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: near_min_api::types::U128(amount_in),
                    amount_out: near_min_api::types::U128(80),
                    discount,
                    swap_gas: Gas::from_ggas(34_000, 34_000),
                }),
                Amount::AmountOut(_) => Some(SplitRouteApiResponse::ExactOut {
                    routes: vec![plach::ExactOutRoute {
                        split_fraction: xyk_dex_types::FULL_SPLIT_FRACTION,
                        pools: vec![plach::ExactOutPoolStep {
                            pool_id: 1,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            amount_in: near_min_api::types::U128(100),
                            amount_out: near_min_api::types::U128(80),
                            max_amount_in: near_min_api::types::U128(101),
                        }],
                        amount_in: near_min_api::types::U128(100),
                        max_amount_in: near_min_api::types::U128(101),
                        amount_out: near_min_api::types::U128(80),
                    }],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: near_min_api::types::U128(100),
                    max_amount_in: near_min_api::types::U128(101),
                    amount_out: near_min_api::types::U128(80),
                    discount,
                    swap_gas: Gas::from_ggas(34_000, 34_000),
                }),
            }
        }
    }

    /// The route of the quote that the Plach hook makes of the test quotes, without a fee discount
    async fn plach_route(
        request: SwapRequest,
        network: &impl NetworkView,
        quotes: &TestPlachQuotes,
    ) -> Option<ProviderRoute> {
        let trade = trade_of_request(network, &request).await?;
        let swap_route = |split_fraction, pool_ids| SwapRoute {
            fraction: split_fraction,
            pool_ids,
        };
        let (swap_routes, estimated_amount, worst_case_amount, swap_gas) =
            match quotes.find_path(&trade)? {
                SplitRouteApiResponse::ExactIn {
                    routes,
                    amount_out,
                    swap_gas,
                    ..
                } => (
                    routes
                        .iter()
                        .map(|route| {
                            swap_route(
                                route.split_fraction,
                                route.pools.iter().map(|step| step.pool_id).collect(),
                            )
                        })
                        .collect::<Vec<_>>(),
                    amount_out.0,
                    routes.iter().map(|route| route.min_amount_out.0).sum(),
                    swap_gas,
                ),
                SplitRouteApiResponse::ExactOut {
                    routes,
                    amount_in,
                    max_amount_in,
                    swap_gas,
                    ..
                } => (
                    routes
                        .iter()
                        .map(|route| {
                            swap_route(
                                route.split_fraction,
                                route.pools.iter().map(|step| step.pool_id).collect(),
                            )
                        })
                        .collect(),
                    amount_in.0,
                    max_amount_in.0,
                    swap_gas,
                ),
            };
        if swap_routes.is_empty() {
            return None;
        }
        let quote = HookQuote {
            dex_id: PLACH_DEX_ID.parse().unwrap(),
            message: Base64VecU8(
                borsh::to_vec(&SwapArgs {
                    routes: swap_routes,
                    fee_discount: None,
                })
                .unwrap(),
            ),
            estimated_amount: U128(estimated_amount),
            worst_case_amount: U128(worst_case_amount),
            gas: swap_gas,
            discount: None,
        };
        route(
            network,
            DexId::Hook("Plach".into()),
            &request,
            &trade,
            IntearDexSwap::of_quote(quote),
        )
        .await
    }

    /// A quote of a swap that burns `tgas`, and needs 10 TGas more attached
    fn quote(
        dex_id: &str,
        estimated_amount: u128,
        worst_case_amount: u128,
        tgas: u64,
    ) -> HookQuote {
        HookQuote {
            dex_id: dex_id.parse().unwrap(),
            message: Base64VecU8(dex_id.as_bytes().to_vec()),
            estimated_amount: U128(estimated_amount),
            worst_case_amount: U128(worst_case_amount),
            gas: Gas::from_ggas(tgas * 1_000, (tgas + 10) * 1_000),
            discount: None,
        }
    }

    /// Gas attached to swap on DEXes that need `swap_attached`, and withdraw a NEP-141
    fn attached_gas(swap_attached: NearGas) -> NearGas {
        (ENGINE_CALL_GAS
            + Gas {
                estimated_burn: NearGas::from_gas(0),
                needs_attached: swap_attached,
            }
            + WITHDRAWAL_GAS
            + sending_out_gas(&AssetId::Nep141("ft".parse().unwrap()), None)
            + GAS_MARGIN)
            .needs_attached
    }

    /// The operations of the route's swap, and the gas attached to them
    async fn swap_operations(
        amount: Amount,
        quotes: Vec<HookQuote>,
    ) -> (serde_json::Value, NearGas) {
        let request = SwapRequest {
            token_in: TokenId::Near,
            token_out: TokenId::Nep141("ft".parse().unwrap()),
            amount,
            slippage: Slippage::Fixed {
                slippage: "0.01".parse().unwrap(),
            },
            dexes: None,
            trader_account_id: None,
            signing_public_key: None,
            referrer_id: None,
            fees: Default::default(),
        };
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).await.unwrap();
        let swap = IntearDexSwap::fallback_queue(&trade, quotes).unwrap();
        let route = route(&network, DexId::IntearDex, &request, &trade, swap)
            .await
            .unwrap();
        let Some(RouteTransaction { calls, .. }) = route.transactions.last() else {
            panic!("No swap transaction");
        };
        let [deposit_near] = calls.as_slice() else {
            panic!("Swap transaction isn't a single function call");
        };
        let args = serde_json::from_slice::<serde_json::Value>(&deposit_near.args).unwrap();
        (
            args["operations"]["operations"].clone(),
            deposit_near.gas.needs_attached,
        )
    }

    #[tokio::test]
    async fn exact_in_falls_back_from_the_best_quote_within_slippage_of_it() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![
                quote("slimedragon.near/xyk", 80, 79, 34),
                quote("slimedragon.near/otc", 82, 82, 27),
            ],
        )
        .await;
        assert_eq!(
            operations[0],
            serde_json::json!({
                "SwapAdvanced": {
                    "asset_in": "near",
                    "asset_out": "nep141:ft",
                    "amount": { "Amount": { "ExactIn": "100" } },
                    // 1% under the best quote's 82, so the fallback's 80 still fills it
                    "constraint": "81",
                    "options": [
                        { "dex_id": "slimedragon.near/otc", "message": "c2xpbWVkcmFnb24ubmVhci9vdGM=" },
                        { "dex_id": "slimedragon.near/xyk", "message": "c2xpbWVkcmFnb24ubmVhci94eWs=" },
                    ],
                    "mode": "FallbackQueue",
                },
            })
        );
        assert_eq!(
            operations[1]["Withdraw"]["amount"]["Full"]["at_least"],
            "81"
        );
        // Enough to try both
        assert_eq!(gas, attached_gas(NearGas::from_tgas(44 + 37)));
    }

    #[tokio::test]
    async fn exact_out_falls_back_from_the_cheapest_quote_within_slippage_of_it() {
        let (operations, _) = swap_operations(
            Amount::AmountOut(80),
            vec![
                quote("slimedragon.near/otc", 101, 101, 27),
                quote("slimedragon.near/xyk", 100, 102, 34),
            ],
        )
        .await;
        let swap = &operations[0]["SwapAdvanced"];
        // 1% over the cheapest quote's 100
        assert_eq!(swap["constraint"], "101");
        assert_eq!(swap["options"][0]["dex_id"], "slimedragon.near/xyk");
        assert_eq!(swap["options"][1]["dex_id"], "slimedragon.near/otc");
    }

    #[tokio::test]
    async fn one_quote_is_swapped_on_alone() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![quote("slimedragon.near/otc", 82, 82, 27)],
        )
        .await;
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
        assert_eq!(operations[0]["SwapSimple"]["constraint"], "82");
        assert_eq!(gas, attached_gas(NearGas::from_tgas(37)));
    }

    /// Operations are typed with the engine's own types, so this pins the JSON the engine reads
    #[tokio::test]
    async fn operations_json_matches_engine_format() {
        let route = plach_route(
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
            &TestPlachQuotes::default(),
        )
        .await
        .unwrap();
        let Some(RouteTransaction { calls, .. }) = route.transactions.last() else {
            panic!("No swap transaction");
        };
        let [deposit_near] = calls.as_slice() else {
            panic!("Swap transaction isn't a single function call");
        };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&deposit_near.args).unwrap(),
            serde_json::json!({
                "operations": {
                    "operations": [
                        {
                            "SwapSimple": {
                                "dex_id": "slimedragon.near/xyk",
                                "message": "AQAAAA==",
                                "asset_in": "near",
                                "asset_out": "nep141:ft",
                                "amount": { "Amount": { "ExactIn": "100" } },
                                "constraint": "79",
                            },
                        },
                        {
                            "Withdraw": {
                                "asset_id": "nep141:ft",
                                "amount": { "Full": { "at_least": "79" } },
                                "to": null,
                                "rescue_address": null,
                            },
                        },
                    ],
                    "referrer": DEFAULT_REFERRER_ID,
                    "referral_fees": [],
                },
            })
        );
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_610, 4_110),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                },
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(43_000, 57_900),
                            deposit: NearToken::from_yoctonear(100),
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
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_610, 4_110),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        calls: vec![create_unwrap_action(NearToken::from_yoctonear(100), None)],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                },
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(43_000, 57_900),
                            deposit: NearToken::from_yoctonear(100),
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
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_610, 4_110),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        calls: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            true, None)],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                },
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(43_000, 57_900),
                            deposit: NearToken::from_yoctonear(100),
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
    async fn amount_in_intear_near_to_ft_fast_path() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_950, 4_450),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![Operation::SwapSimple {
                                    dex_id: PLACH_DEX_ID.parse().unwrap(),
                                    message: Base64VecU8(vec![1, 0, 0, 0]),
                                    asset_in: token_in.clone(),
                                    asset_out: token_out.clone(),
                                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                    constraint: Some(U128::from(79)),
                                }],
                                "referrer": DEFAULT_REFERRER_ID,
                                "referral_fees": [],
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(37_600, 40_600),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::TokenOnIntearDex(token_out),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        let token_in = AssetId::Nep141("ft".parse().unwrap());
        let token_out = AssetId::Near;
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_610, 4_110),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas {
                                estimated_burn: NearGas::from_gas(45_623_182_562_500),
                                needs_attached: NearGas::from_gas(77_223_182_562_500),
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
    async fn amount_in_rhea_ft_to_near() {
        let token_in = AssetId::Nep141("ft".parse().unwrap());
        let token_out = AssetId::Near;
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_610, 4_110),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        calls: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false, None)],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas {
                                estimated_burn: NearGas::from_gas(45_623_182_562_500),
                                needs_attached: NearGas::from_gas(77_223_182_562_500),
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
    async fn amount_out_near_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_950, 4_450),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.parse().unwrap(),
                                            message: Base64VecU8(vec![1, 0, 0, 0]),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                            constraint: Some(U128::from(101)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(80)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_in.clone(),
                                            amount: WithdrawAmount::Full { at_least: None },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                    "referral_fees": [],
                                },
                            }))
                            .unwrap(),
                            gas: Gas {
                                estimated_burn: NearGas::from_gas(46_623_182_562_500),
                                needs_attached: NearGas::from_gas(64_523_182_562_500),
                            },
                            deposit: NearToken::from_yoctonear(101),
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
    async fn amount_out_intear_near_to_ft_withdraws_exact() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_950, 4_450),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        calls: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                            TOKEN_STORAGE_DEPOSIT_GAS,
                        )],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.parse().unwrap(),
                                        message: Base64VecU8(vec![1, 0, 0, 0]),
                                        asset_in: token_in.clone(),
                                        asset_out: token_out.clone(),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                        constraint: Some(U128::from(101)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: token_out.clone(),
                                        amount: WithdrawAmount::Exact(U128::from(80)),
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": DEFAULT_REFERRER_ID,
                                "referral_fees": [],
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(43_000, 57_900),
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
    async fn amount_out_intear_to_intear_leaves_output_on_dex() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            plach_route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::TokenOnIntearDex(AssetId::Nep141("ft".parse().unwrap())),
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![
                            RouteCall {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas::from_ggas(3_080, 3_500),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            },
                            RouteCall {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas::from_ggas(3_950, 4_450),
                                deposit: NearToken::from_yoctonear(1),
                            },
                        ],
                        is_fee_transaction: false,
                    },
                    RouteTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        calls: vec![RouteCall {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![Operation::SwapSimple {
                                    dex_id: PLACH_DEX_ID.parse().unwrap(),
                                    message: Base64VecU8(vec![1, 0, 0, 0]),
                                    asset_in: token_in.clone(),
                                    asset_out: token_out.clone(),
                                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                    constraint: Some(U128::from(101)),
                                }],
                                "referrer": DEFAULT_REFERRER_ID,
                                "referral_fees": [],
                            }))
                            .unwrap(),
                            gas: Gas::from_ggas(37_600, 40_600),
                            deposit: NearToken::from_yoctonear(1),
                        }],
                        is_fee_transaction: false,
                    },
                ],
                discount: None,
                token_output: TokenId::TokenOnIntearDex(token_out),
            })
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn empty_routes_returns_none() {
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::empty_routes(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_registration_and_storage() {
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![RouteTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    calls: vec![RouteCall {
                        method_name: "deposit_near".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": {
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.parse().unwrap(),
                                        message: Base64VecU8(vec![1, 0, 0, 0]),
                                        asset_in: AssetId::Near,
                                        asset_out: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                        constraint: Some(U128::from(79)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: WithdrawAmount::Full {
                                            at_least: Some(U128::from(79)),
                                        },
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": DEFAULT_REFERRER_ID,
                                "referral_fees": [],
                            },
                        }))
                        .unwrap(),
                        gas: Gas::from_ggas(43_000, 57_900),
                        deposit: NearToken::from_yoctonear(100),
                    }],
                    is_fee_transaction: false,
                }],
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_operations() {
        assert_eq!(
            plach_route(
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
                &TestPlachQuotes::default(),
            )
            .await,
            Some(ProviderRoute {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Hook("Plach".into()),
                transactions: vec![RouteTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    calls: vec![RouteCall {
                        method_name: "deposit_near".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": {
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.parse().unwrap(),
                                        message: Base64VecU8(vec![1, 0, 0, 0]),
                                        asset_in: AssetId::Near,
                                        asset_out: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                        constraint: Some(U128::from(79)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: WithdrawAmount::Full {
                                            at_least: Some(U128::from(79)),
                                        },
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": "ref.near",
                                "referral_fees": [],
                            },
                        }))
                        .unwrap(),
                        gas: Gas::from_ggas(43_000, 57_900),
                        deposit: NearToken::from_yoctonear(100),
                    }],
                    is_fee_transaction: false,
                }],
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn referral_fees_are_taken_by_the_engine_and_their_receivers_registered() {
        let route = plach_route(
            SwapRequest {
                token_in: TokenId::Near,
                token_out: TokenId::Nep141("ft".parse().unwrap()),
                amount: Amount::AmountIn(1_000_000),
                slippage: Slippage::Fixed {
                    slippage: "0.01".parse().unwrap(),
                },
                dexes: None,
                trader_account_id: Some("trader.near".parse().unwrap()),
                signing_public_key: None,
                referrer_id: None,
                fees: r#"{"ref.near":0.01,"ref2.near":0.005}"#.parse().unwrap(),
            },
            &TestNetworkView::default()
                .with_test_prices()
                .with_storage(
                    INTEAR_DEX_CONTRACT_ID,
                    "trader.near",
                    NearToken::from_near(1),
                    NearToken::from_near(1),
                )
                .with_intear_registered(
                    "trader.near",
                    &AssetId::Nep141("ft".parse().unwrap()),
                    true,
                )
                .with_intear_registered("ref2.near", &AssetId::Near, true),
            &TestPlachQuotes::default(),
        )
        .await
        .unwrap();
        // ref.near and the aggregator haven't registered NEAR, so the trader registers it for them
        let RouteTransaction {
            receiver_id,
            calls: actions,
            ..
        } = &route.transactions[0];
        assert_eq!(receiver_id, INTEAR_DEX_CONTRACT_ID);
        let registered_for = actions
            .iter()
            .map(|call| {
                assert_eq!(call.method_name, "register_assets");
                serde_json::from_slice::<serde_json::Value>(&call.args).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            registered_for,
            vec![
                serde_json::json!({ "asset_ids": ["near"], "for": { "Account": "ref.near" } }),
                serde_json::json!({
                    "asset_ids": ["near"],
                    "for": { "Account": AGGREGATOR_FEE_RECEIVER_ID },
                }),
            ]
        );

        let Some(RouteTransaction { calls, .. }) = route.transactions.last() else {
            panic!("No swap transaction");
        };
        let [deposit_near] = calls.as_slice() else {
            panic!("Swap transaction isn't a single function call");
        };
        let args = serde_json::from_slice::<serde_json::Value>(&deposit_near.args).unwrap();
        // 80% of each fee to its referrer, 20% of both to the aggregator
        assert_eq!(
            args["operations"]["referral_fees"],
            serde_json::json!([
                ["ref.near", 8_000],
                ["ref2.near", 4_000],
                [AGGREGATOR_FEE_RECEIVER_ID, 3_000],
            ])
        );
        // The engine takes the whole input and the fees out of it, the DEX swaps the rest
        assert_eq!(
            args["operations"]["operations"][0]["SwapSimple"]["amount"],
            serde_json::json!({ "Amount": { "ExactIn": "1000000" } })
        );
        assert_eq!(deposit_near.deposit, NearToken::from_yoctonear(1_000_000));
        let message = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            args["operations"]["operations"][0]["SwapSimple"]["message"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let swap_args = borsh::from_slice::<SwapArgs>(&message).unwrap();
        assert_eq!(swap_args.routes[0].pool_ids, vec![1]);
    }

    #[tokio::test]
    async fn exact_out_referral_fees_go_on_top_of_the_input() {
        let route = plach_route(
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
                fees: r#"{"ref.near":0.05}"#.parse().unwrap(),
            },
            &TestNetworkView::default().with_test_prices(),
            &TestPlachQuotes::default(),
        )
        .await
        .unwrap();
        // The referrer's 4% and the aggregator's 1% of the total, rounded up like the engine:
        // 100 / 95% * 4% = 4.21 -> 5 and 100 / 95% * 1% = 1.05 -> 2
        assert_eq!(route.estimated_amount, Amount::AmountIn(100 + 5 + 2));
        // And of the most the DEX can take: 4.25 -> 5 and 1.06 -> 2
        assert_eq!(route.worst_case_amount, Amount::AmountIn(101 + 5 + 2));
    }
}
