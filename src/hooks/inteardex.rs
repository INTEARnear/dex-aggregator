use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use intear_dex_types::{AssetId, ReferralFees};
use near_min_api::types::{AccountId, NearToken};
use near_sdk::json_types::U128;
use swap_router_types::hook::{HookQuote, IntearDexTrade};

use crate::{
    calls::{
        CallBlueprint, IntearAsset, IntearDexOperations, IntearDexSwapOperation,
        IntearDexSwapOption, IntearDexWithdrawAmount, IntearDexWithdrawal,
    },
    route::{RouteBlueprint, RouteStep},
    shared_utils::{
        base_token, convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage,
        is_near, payable_referral_fees, register_input_in_wallet, NetworkView, DEFAULT_REFERRER_ID,
    },
    types::TokenId,
    Amount, DexId, SwapRequest,
};

pub const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";

/// The swap on Intear DEX that the request goes through, which hooks quote. None if its tokens
/// can't be swapped there.
pub fn trade_of_request(
    network: &impl NetworkView,
    request: &SwapRequest,
) -> Option<IntearDexTrade> {
    let amount_hint = match request.amount {
        Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
    };
    let asset_in = intear_dex_asset(&request.token_in, amount_hint)?;
    let asset_out = intear_dex_asset(&request.token_out, amount_hint)?;
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
fn intear_dex_asset(token_id: &TokenId, amount_hint: u128) -> Option<AssetId> {
    match token_id {
        TokenId::TokenOnIntearDex(asset_id) => Some(asset_id.clone()),
        token_id if is_near(token_id) => {
            convert_to_native(token_id, None, NearToken::from_yoctonear(amount_hint))?;
            Some(AssetId::Near)
        }
        token_id => convert_to_nep141(token_id, None, 0).map(|(_, nep141)| AssetId::Nep141(nep141)),
    }
}

fn fees_total(fees: Vec<(AccountId, U128)>) -> u128 {
    fees.iter().map(|(_, fee)| fee.0).sum()
}

/// A swap on Intear DEX for an [`IntearDexTrade`]: on one DEX, or on the first of several that
/// fills it within the trade's slippage
pub struct IntearDexSwap {
    /// Best first
    options: Vec<IntearDexSwapOption>,
    /// Of the first option, as in [`HookQuote`]
    estimated_amount: u128,
    /// The least the swap gives, or the most it takes, as in [`HookQuote`]
    worst_case_amount: u128,
    discount: Option<U128>,
}

impl IntearDexSwap {
    pub fn of_quote(quote: HookQuote) -> Self {
        Self {
            estimated_amount: quote.estimated_amount.0,
            worst_case_amount: quote.worst_case_amount.0,
            discount: quote.discount,
            options: vec![swap_option(quote)],
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
            discount: best.discount,
            options: quotes.into_iter().map(swap_option).collect(),
        })
    }
}

fn swap_option(quote: HookQuote) -> IntearDexSwapOption {
    IntearDexSwapOption {
        dex_id: quote.dex_id,
        message: quote.message,
        gas: quote.gas,
    }
}

/// The route of a swap for `trade`, which [`trade_of_request`] made of `request`
pub fn route(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swap: IntearDexSwap,
) -> Option<RouteBlueprint> {
    let token_in = IntearAsset::try_from(&trade.asset_in)
        .expect("Trades are only of NEAR and NEP-141s, which have a base token");
    let token_out = IntearAsset::try_from(&trade.asset_out)
        .expect("Trades are only of NEAR and NEP-141s, which have a base token");
    let referral_fees = payable_referral_fees(&request.fees);
    let engine_referral_fees = ReferralFees::from(&referral_fees);
    let discount = swap.discount.map(|discount| match request.amount {
        Amount::AmountIn(_) => Amount::AmountOut(discount.0),
        Amount::AmountOut(_) => Amount::AmountIn(discount.0),
    });
    // The engine takes the fees out of the input of an exact-in swap
    let (constraint, estimated_amount, worst_case_amount) = match request.amount {
        Amount::AmountIn(_) => (
            swap.worst_case_amount,
            Amount::AmountOut(swap.estimated_amount),
            Amount::AmountOut(swap.worst_case_amount),
        ),
        Amount::AmountOut(_) => {
            // Fees on top of the input the DEX takes, so the most it can take with them is the
            // constraint
            let with_fees = |amount_in: u128| {
                amount_in + fees_total(engine_referral_fees.amounts_on_top_of(U128(amount_in)))
            };
            (
                with_fees(swap.worst_case_amount),
                Amount::AmountIn(with_fees(swap.estimated_amount)),
                Amount::AmountIn(with_fees(swap.worst_case_amount)),
            )
        }
    };

    let use_fast_path = matches!(request.token_in, TokenId::TokenOnIntearDex(_));

    let withdrawals = if use_fast_path {
        // Unlike the deposit paths, `execute_operations` reads `Full` from the
        // trader's whole inner balance, which would also sweep tokens they held
        // before the swap. So the output is only withdrawn when its amount can
        // be named exactly, and otherwise left on the DEX for the caller to
        // convert once the received amount is known.
        match request.amount {
            Amount::AmountOut(amount_out)
                if !matches!(request.token_out, TokenId::TokenOnIntearDex(_)) =>
            {
                vec![IntearDexWithdrawal {
                    asset: token_out.clone(),
                    amount: IntearDexWithdrawAmount::Exact(amount_out),
                }]
            }
            Amount::AmountIn(_) | Amount::AmountOut(_) => vec![],
        }
    } else {
        let output = IntearDexWithdrawal {
            asset: token_out.clone(),
            amount: IntearDexWithdrawAmount::Full {
                at_least: Some(match request.amount {
                    Amount::AmountIn(_) => constraint,
                    Amount::AmountOut(amount_out) => amount_out,
                }),
            },
        };
        match request.amount {
            Amount::AmountIn(_) => vec![output],
            Amount::AmountOut(_) => vec![
                output,
                IntearDexWithdrawal {
                    asset: token_in.clone(),
                    amount: IntearDexWithdrawAmount::Full { at_least: None },
                },
            ],
        }
    };
    let output_withdrawn = !withdrawals.is_empty();

    // The fast path swaps from the input's balance, and exact-out withdraws what's left of the
    // input, so only exact-in deposits are swapped whole without a balance of the input
    let registered_assets = if use_fast_path || matches!(request.amount, Amount::AmountOut(_)) {
        vec![token_in.clone(), token_out.clone()]
    } else {
        vec![token_out.clone()]
    };
    let registration = match &request.trader_account_id {
        Some(trader_account_id) => vec![RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id: trader_account_id.clone(),
            asset_ids: registered_assets,
            fee_receivers: referral_fees
                .0
                .iter()
                .map(|(receiver_id, _)| (receiver_id.clone(), token_in.clone()))
                .collect(),
        }],
        None => vec![],
    };

    let input_amount = match request.amount {
        Amount::AmountIn(amount_in) => amount_in,
        Amount::AmountOut(_) => constraint,
    };
    let operations = IntearDexOperations {
        swap: IntearDexSwapOperation {
            asset_in: token_in.clone(),
            asset_out: token_out.clone(),
            amount: request.amount,
            constraint,
            options: swap.options,
        },
        withdrawals,
        referrer: request
            .referrer_id
            .clone()
            .unwrap_or_else(|| DEFAULT_REFERRER_ID.parse().unwrap()),
        referral_fees,
        trader_account_id: request.trader_account_id.clone(),
    };
    let output_in_wallet = match &token_out {
        IntearAsset::Near => TokenId::Near,
        IntearAsset::Nep141(token_id) => TokenId::Nep141(token_id.clone()),
    };
    let output_storage =
        deposit_storage_if_needed(&output_in_wallet, request.trader_account_id.as_ref());

    let steps = if use_fast_path {
        let execute_operations =
            RouteStep::Call(CallBlueprint::IntearDexExecuteOperations { operations });
        if output_withdrawn {
            [output_storage, vec![execute_operations]].concat()
        } else {
            vec![execute_operations]
        }
    } else {
        match &token_in {
            IntearAsset::Near => {
                let input_to_native = convert_to_native(
                    &request.token_in,
                    request.trader_account_id.as_ref(),
                    NearToken::from_yoctonear(input_amount),
                )?;
                let deposit_near = RouteStep::Call(CallBlueprint::IntearDexDepositNear {
                    amount: NearToken::from_yoctonear(input_amount),
                    operations: Some(operations),
                });
                [output_storage, input_to_native, vec![deposit_near]].concat()
            }
            IntearAsset::Nep141(token_in_id) => {
                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.as_ref(),
                    input_amount,
                )?;
                assert_eq!(token_in_id, &input_nep141);
                let swap = RouteStep::Call(CallBlueprint::InputNep141TransferCallIntearDex {
                    token_id: token_in_id.clone(),
                    amount: input_amount,
                    operations,
                });
                [
                    output_storage,
                    register_input_in_wallet(
                        &request.token_in,
                        input_nep141,
                        request.trader_account_id.as_ref(),
                    ),
                    input_to_nep141,
                    vec![swap],
                ]
                .concat()
            }
        }
    };

    Some(RouteBlueprint {
        dex_id,
        estimated_amount,
        worst_case_amount,
        token_output: if output_withdrawn {
            output_in_wallet
        } else {
            TokenId::TokenOnIntearDex(trade.asset_out.clone())
        },
        discount,
        steps: [registration, steps].concat(),
        fee_steps: vec![],
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, NearGas};
    use near_sdk::json_types::Base64VecU8;
    use pathfinder::plach::{self, SplitRouteApiResponse};
    use swap_router_types::fees::AGGREGATOR_FEE_RECEIVER_ID;
    use xyk_dex_types::{SwapArgs, SwapRoute};

    use super::*;
    use crate::calls::RheaOutput;
    use crate::route::resolve;
    use crate::shared_utils::{TestNetworkView, WRAP_NEAR};
    use crate::types::{Gas, QuoteFees, Slippage};

    const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
    const PLACH_SWAP_GAS: Gas = Gas::from_ggas(34_000, 34_000);

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
                    swap_gas: PLACH_SWAP_GAS,
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
                    swap_gas: PLACH_SWAP_GAS,
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
                    swap_gas: PLACH_SWAP_GAS,
                }),
            }
        }
    }

    /// The swap message of the test quotes, through pool 1 without a fee discount
    fn plach_message() -> Base64VecU8 {
        Base64VecU8(
            borsh::to_vec(&SwapArgs {
                routes: vec![SwapRoute {
                    fraction: xyk_dex_types::FULL_SPLIT_FRACTION,
                    pool_ids: vec![1],
                }],
                fee_discount: None,
            })
            .unwrap(),
        )
    }

    /// The route of the quote that the Plach hook makes of the test quotes, without a fee discount
    fn plach_route(
        request: SwapRequest,
        network: &impl NetworkView,
        quotes: &TestPlachQuotes,
    ) -> Option<RouteBlueprint> {
        let trade = trade_of_request(network, &request)?;
        let (route_count, estimated_amount, worst_case_amount, swap_gas) =
            match quotes.find_path(&trade)? {
                SplitRouteApiResponse::ExactIn {
                    routes,
                    amount_out,
                    swap_gas,
                    ..
                } => (
                    routes.len(),
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
                } => (routes.len(), amount_in.0, max_amount_in.0, swap_gas),
            };
        if route_count == 0 {
            return None;
        }
        let quote = HookQuote {
            dex_id: PLACH_DEX_ID.parse().unwrap(),
            message: plach_message(),
            estimated_amount: U128(estimated_amount),
            worst_case_amount: U128(worst_case_amount),
            gas: swap_gas,
            discount: None,
        };
        route(
            DexId::Hook("Plach".into()),
            &request,
            &trade,
            IntearDexSwap::of_quote(quote),
        )
    }

    fn test_route(request: SwapRequest) -> Option<RouteBlueprint> {
        plach_route(
            request,
            &TestNetworkView::default().with_test_prices(),
            &TestPlachQuotes::default(),
        )
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

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    fn ft() -> IntearAsset {
        IntearAsset::Nep141(account("ft"))
    }

    fn request(token_in: TokenId, token_out: TokenId, amount: Amount) -> SwapRequest {
        SwapRequest {
            token_in,
            token_out,
            amount,
            slippage: Slippage::Fixed {
                slippage: "0.01".parse().unwrap(),
            },
            dexes: None,
            trader_account_id: Some(trader()),
            signing_public_key: None,
            referrer_id: None,
            fees: Default::default(),
        }
    }

    fn plach_operations(
        asset_in: IntearAsset,
        asset_out: IntearAsset,
        amount: Amount,
        constraint: u128,
        withdrawals: Vec<IntearDexWithdrawal>,
    ) -> IntearDexOperations {
        IntearDexOperations {
            swap: IntearDexSwapOperation {
                asset_in,
                asset_out,
                amount,
                constraint,
                options: vec![IntearDexSwapOption {
                    dex_id: PLACH_DEX_ID.parse().unwrap(),
                    message: plach_message(),
                    gas: PLACH_SWAP_GAS,
                }],
            },
            withdrawals,
            referrer: account(DEFAULT_REFERRER_ID),
            referral_fees: QuoteFees::default(),
            trader_account_id: Some(trader()),
        }
    }

    fn withdrawal(asset: IntearAsset, amount: IntearDexWithdrawAmount) -> IntearDexWithdrawal {
        IntearDexWithdrawal { asset, amount }
    }

    /// Exact-in of 100 that withdraws at least 79 of the output
    fn exact_in_operations(asset_in: IntearAsset, asset_out: IntearAsset) -> IntearDexOperations {
        plach_operations(
            asset_in,
            asset_out.clone(),
            Amount::AmountIn(100),
            79,
            vec![withdrawal(
                asset_out,
                IntearDexWithdrawAmount::Full { at_least: Some(79) },
            )],
        )
    }

    fn registration(asset_ids: Vec<IntearAsset>) -> RouteStep {
        RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id: trader(),
            asset_ids,
            fee_receivers: vec![],
        }
    }

    fn trader_storage(token_id: &str) -> RouteStep {
        RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id: account(token_id),
            trader_account_id: trader(),
        }
    }

    fn deposit_near(amount: u128, operations: IntearDexOperations) -> RouteStep {
        RouteStep::Call(CallBlueprint::IntearDexDepositNear {
            amount: NearToken::from_yoctonear(amount),
            operations: Some(operations),
        })
    }

    fn plach_route_of(
        estimated_amount: Amount,
        worst_case_amount: Amount,
        token_output: TokenId,
        steps: Vec<RouteStep>,
    ) -> RouteBlueprint {
        RouteBlueprint {
            dex_id: DexId::Hook("Plach".into()),
            estimated_amount,
            worst_case_amount,
            token_output,
            discount: None,
            steps,
            fee_steps: vec![],
        }
    }

    fn ft_output() -> TokenId {
        TokenId::Nep141(account("ft"))
    }

    /// The args of the last call of the route, and the gas attached to it
    fn last_call(route: &RouteBlueprint) -> (serde_json::Value, NearGas) {
        let Some(RouteStep::Call(call)) = route.steps.last() else {
            panic!("The route doesn't end with a call");
        };
        let gas = call.gas().needs_attached;
        let Action::FunctionCall(function_call) = call.clone().into_action() else {
            panic!("Calls are function calls");
        };
        (serde_json::from_slice(&function_call.args).unwrap(), gas)
    }

    /// Gas attached to swap on DEXes that need `swap_attached` with `deposit_near`, and withdraw a
    /// NEP-141
    fn attached_gas(swap_attached: NearGas) -> NearGas {
        let engine_and_withdrawal = NearGas::from_ggas(3_600 + 500 + 16_800 + 3_000);
        NearGas::from_gas(engine_and_withdrawal.as_gas() + swap_attached.as_gas())
    }

    /// The operations of the route's swap from NEAR, and the gas attached to them
    fn swap_operations(amount: Amount, quotes: Vec<HookQuote>) -> (serde_json::Value, NearGas) {
        let request = SwapRequest {
            trader_account_id: None,
            ..request(TokenId::Near, ft_output(), amount)
        };
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).unwrap();
        let swap = IntearDexSwap::fallback_queue(&trade, quotes).unwrap();
        let route = route(DexId::IntearDex, &request, &trade, swap).unwrap();
        let (args, gas) = last_call(&route);
        (args["operations"]["operations"].clone(), gas)
    }

    #[test]
    fn exact_in_falls_back_from_the_best_quote_within_slippage_of_it() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![
                quote("slimedragon.near/xyk", 80, 79, 34),
                quote("slimedragon.near/otc", 82, 82, 27),
            ],
        );
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

    #[test]
    fn exact_out_falls_back_from_the_cheapest_quote_within_slippage_of_it() {
        let (operations, _) = swap_operations(
            Amount::AmountOut(80),
            vec![
                quote("slimedragon.near/otc", 101, 101, 27),
                quote("slimedragon.near/xyk", 100, 102, 34),
            ],
        );
        let swap = &operations[0]["SwapAdvanced"];
        // 1% over the cheapest quote's 100
        assert_eq!(swap["constraint"], "101");
        assert_eq!(swap["options"][0]["dex_id"], "slimedragon.near/xyk");
        assert_eq!(swap["options"][1]["dex_id"], "slimedragon.near/otc");
    }

    #[test]
    fn one_quote_is_swapped_on_alone() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![quote("slimedragon.near/otc", 82, 82, 27)],
        );
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
        assert_eq!(operations[0]["SwapSimple"]["constraint"], "82");
        assert_eq!(gas, attached_gas(NearGas::from_tgas(37)));
    }

    /// Pins the JSON the engine reads
    #[test]
    fn operations_json_matches_engine_format() {
        let route = test_route(SwapRequest {
            trader_account_id: None,
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(100))
        })
        .unwrap();
        assert_eq!(
            last_call(&route).0,
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

    #[test]
    fn amount_in_near_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Near, ft_output(), Amount::AmountIn(100))),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                ft_output(),
                vec![
                    registration(vec![ft()]),
                    trader_storage("ft"),
                    deposit_near(100, exact_in_operations(IntearAsset::Near, ft())),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_wnear_to_ft() {
        assert_eq!(
            test_route(request(
                TokenId::Nep141(account(WRAP_NEAR)),
                ft_output(),
                Amount::AmountIn(100)
            )),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                ft_output(),
                vec![
                    registration(vec![ft()]),
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::UnwrapNear {
                        amount: NearToken::from_yoctonear(100),
                        trader_account_id: Some(trader()),
                    }),
                    deposit_near(100, exact_in_operations(IntearAsset::Near, ft())),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_rhea_wnear_to_ft() {
        assert_eq!(
            test_route(request(
                TokenId::Nep141OnRhea(account(WRAP_NEAR)),
                ft_output(),
                Amount::AmountIn(100)
            )),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                ft_output(),
                vec![
                    registration(vec![ft()]),
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account(WRAP_NEAR),
                        amount: 100,
                        output: RheaOutput::Near {
                            trader_account_id: Some(trader()),
                        },
                    }),
                    deposit_near(100, exact_in_operations(IntearAsset::Near, ft())),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_intear_near_to_ft_fast_path() {
        assert_eq!(
            test_route(request(
                TokenId::TokenOnIntearDex(AssetId::Near),
                ft_output(),
                Amount::AmountIn(100)
            )),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                TokenId::TokenOnIntearDex(AssetId::Nep141(account("ft"))),
                vec![
                    registration(vec![IntearAsset::Near, ft()]),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAsset::Near,
                            ft(),
                            Amount::AmountIn(100),
                            79,
                            vec![]
                        ),
                    }),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_ft_to_near() {
        assert_eq!(
            test_route(request(ft_output(), TokenId::Near, Amount::AmountIn(100))),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                TokenId::Near,
                vec![
                    registration(vec![IntearAsset::Near]),
                    RouteStep::Call(CallBlueprint::InputNep141TransferCallIntearDex {
                        token_id: account("ft"),
                        amount: 100,
                        operations: exact_in_operations(ft(), IntearAsset::Near),
                    }),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_rhea_ft_to_near() {
        assert_eq!(
            test_route(request(
                TokenId::Nep141OnRhea(account("ft")),
                TokenId::Near,
                Amount::AmountIn(100)
            )),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                TokenId::Near,
                vec![
                    registration(vec![IntearAsset::Near]),
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account("ft"),
                        amount: 100,
                        output: RheaOutput::Nep141,
                    }),
                    RouteStep::Call(CallBlueprint::InputNep141TransferCallIntearDex {
                        token_id: account("ft"),
                        amount: 100,
                        operations: exact_in_operations(ft(), IntearAsset::Near),
                    }),
                ],
            ))
        );
    }

    #[test]
    fn amount_out_near_to_ft() {
        assert_eq!(
            test_route(request(TokenId::Near, ft_output(), Amount::AmountOut(80))),
            Some(plach_route_of(
                Amount::AmountIn(100),
                Amount::AmountIn(101),
                ft_output(),
                vec![
                    registration(vec![IntearAsset::Near, ft()]),
                    trader_storage("ft"),
                    deposit_near(
                        101,
                        plach_operations(
                            IntearAsset::Near,
                            ft(),
                            Amount::AmountOut(80),
                            101,
                            vec![
                                withdrawal(
                                    ft(),
                                    IntearDexWithdrawAmount::Full { at_least: Some(80) }
                                ),
                                // What's left of the input
                                withdrawal(
                                    IntearAsset::Near,
                                    IntearDexWithdrawAmount::Full { at_least: None }
                                ),
                            ]
                        )
                    ),
                ],
            ))
        );
    }

    #[test]
    fn amount_out_intear_near_to_ft_withdraws_exact() {
        assert_eq!(
            test_route(request(
                TokenId::TokenOnIntearDex(AssetId::Near),
                ft_output(),
                Amount::AmountOut(80)
            )),
            Some(plach_route_of(
                Amount::AmountIn(100),
                Amount::AmountIn(101),
                ft_output(),
                vec![
                    registration(vec![IntearAsset::Near, ft()]),
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAsset::Near,
                            ft(),
                            Amount::AmountOut(80),
                            101,
                            vec![withdrawal(ft(), IntearDexWithdrawAmount::Exact(80))]
                        ),
                    }),
                ],
            ))
        );
    }

    #[test]
    fn amount_out_intear_to_intear_leaves_output_on_dex() {
        let intear_ft = TokenId::TokenOnIntearDex(AssetId::Nep141(account("ft")));
        assert_eq!(
            test_route(request(
                TokenId::TokenOnIntearDex(AssetId::Near),
                intear_ft.clone(),
                Amount::AmountOut(80)
            )),
            Some(plach_route_of(
                Amount::AmountIn(100),
                Amount::AmountIn(101),
                intear_ft,
                vec![
                    registration(vec![IntearAsset::Near, ft()]),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAsset::Near,
                            ft(),
                            Amount::AmountOut(80),
                            101,
                            vec![]
                        ),
                    }),
                ],
            ))
        );
    }

    #[test]
    fn same_near_family_returns_none() {
        assert_eq!(
            test_route(request(
                TokenId::Near,
                TokenId::Nep141(account(WRAP_NEAR)),
                Amount::AmountIn(100)
            )),
            None
        );
    }

    #[test]
    fn missing_quote_returns_none() {
        assert_eq!(
            plach_route(
                request(TokenId::Near, ft_output(), Amount::AmountIn(100)),
                &TestNetworkView::default().with_test_prices(),
                &TestPlachQuotes::none(),
            ),
            None
        );
    }

    #[test]
    fn empty_routes_returns_none() {
        assert_eq!(
            plach_route(
                request(TokenId::Near, ft_output(), Amount::AmountIn(100)),
                &TestNetworkView::default().with_test_prices(),
                &TestPlachQuotes::empty_routes(),
            ),
            None
        );
    }

    #[test]
    fn no_trader_omits_registration_and_storage() {
        assert_eq!(
            test_route(SwapRequest {
                trader_account_id: None,
                ..request(TokenId::Near, ft_output(), Amount::AmountIn(100))
            }),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                ft_output(),
                vec![deposit_near(
                    100,
                    IntearDexOperations {
                        trader_account_id: None,
                        ..exact_in_operations(IntearAsset::Near, ft())
                    }
                )],
            ))
        );
    }

    #[test]
    fn custom_referrer_id_is_passed_in_operations() {
        let route = test_route(SwapRequest {
            trader_account_id: None,
            referrer_id: Some(account("ref.near")),
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(100))
        })
        .unwrap();
        assert_eq!(last_call(&route).0["operations"]["referrer"], "ref.near");
    }

    #[tokio::test]
    async fn referral_fees_are_taken_by_the_engine_and_their_receivers_registered() {
        let route = test_route(SwapRequest {
            amount: Amount::AmountIn(1_000_000),
            fees: r#"{"ref.near":0.01,"ref2.near":0.005}"#.parse().unwrap(),
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(1_000_000))
        })
        .unwrap();
        assert_eq!(
            route.steps[0],
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![ft()],
                fee_receivers: vec![
                    (account("ref.near"), IntearAsset::Near),
                    (account("ref2.near"), IntearAsset::Near),
                    (AGGREGATOR_FEE_RECEIVER_ID.to_owned(), IntearAsset::Near),
                ],
            }
        );

        // ref.near and the aggregator haven't registered NEAR, so the trader registers it for them
        let network = TestNetworkView::default()
            .with_storage(
                INTEAR_DEX_CONTRACT_ID,
                "trader.near",
                NearToken::from_near(1),
                NearToken::from_near(1),
            )
            .with_storage(
                "ft",
                "trader.near",
                NearToken::from_near(1),
                NearToken::from_near(1),
            )
            .with_intear_registered("trader.near", &AssetId::Nep141(account("ft")), true)
            .with_intear_registered("ref2.near", &AssetId::Near, true);
        let resolved = resolve(&network, route.clone()).await.unwrap();
        assert_eq!(
            resolved.calls[..2],
            [
                CallBlueprint::IntearDexRegisterAssets {
                    asset_ids: vec![IntearAsset::Near],
                    for_account: Some(account("ref.near")),
                },
                CallBlueprint::IntearDexRegisterAssets {
                    asset_ids: vec![IntearAsset::Near],
                    for_account: Some(AGGREGATOR_FEE_RECEIVER_ID.to_owned()),
                },
            ]
        );

        let (args, _) = last_call(&route);
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
        assert!(matches!(
            route.steps.last(),
            Some(RouteStep::Call(CallBlueprint::IntearDexDepositNear { amount, .. }))
                if *amount == NearToken::from_yoctonear(1_000_000)
        ));
    }

    #[test]
    fn exact_out_referral_fees_go_on_top_of_the_input() {
        let route = test_route(SwapRequest {
            trader_account_id: None,
            fees: r#"{"ref.near":0.05}"#.parse().unwrap(),
            ..request(TokenId::Near, ft_output(), Amount::AmountOut(80))
        })
        .unwrap();
        // The referrer's 4% and the aggregator's 1% of the total, rounded up like the engine:
        // 100 / 95% * 4% = 4.21 -> 5 and 100 / 95% * 1% = 1.05 -> 2
        assert_eq!(route.estimated_amount, Amount::AmountIn(100 + 5 + 2));
        // And of the most the DEX can take: 4.25 -> 5 and 1.06 -> 2
        assert_eq!(route.worst_case_amount, Amount::AmountIn(101 + 5 + 2));
    }
}
