use bigdecimal::Zero;
use intear_dex_common::AssetId;
use near_min_api::types::NearToken;
use near_sdk::json_types::{Base64VecU8, U128};
use swap_router_types::{
    fees::{amount_in_after_referral_fees, amount_in_with_referral_fees},
    hook::{HookQuote, IntearDexTrade, OracleRequest, QuoteFeeDiscount},
    types::Gas,
};

use crate::{
    calls::{
        CallBlueprint, IntearDexOperations, IntearDexOracleRequest, IntearDexQuoteImprovementFee,
        IntearDexSwapOperation, IntearDexSwapOption, IntearDexSwapStep, IntearDexWithdrawAmount,
        IntearDexWithdrawal,
    },
    route::{RouteBlueprint, RouteStep},
    shared_utils::{
        base_token, convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage,
        is_near, payable_referral_fees, register_input_in_wallet, NetworkView, DEFAULT_REFERRER_ID,
    },
    types::{IntearAssetId, TokenId},
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
        &base_token(&request.token_in),
        &base_token(&request.token_out),
    );
    let amount = match request.amount {
        Amount::AmountIn(amount_in) => {
            Amount::AmountIn(amount_in_after_referral_fees(&request.fees, amount_in))
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

/// The trade of a swap whose operations start with a request to an oracle that takes `oracle_fee`
/// of the asset in: the engine takes it out of the input of an exact-in swap before the app fees,
/// and on top of the input of an exact-out one. None if the fee leaves nothing of the input.
pub fn trade_after_oracle_fee(
    request: &SwapRequest,
    trade: &IntearDexTrade,
    oracle_fee: u128,
) -> Option<IntearDexTrade> {
    let amount = match request.amount {
        Amount::AmountIn(amount_in) => {
            let amount_in_after_oracle_fee = amount_in
                .checked_sub(oracle_fee)
                .filter(|amount_in| *amount_in > 0)?;
            Amount::AmountIn(amount_in_after_referral_fees(
                &request.fees,
                amount_in_after_oracle_fee,
            ))
        }
        Amount::AmountOut(amount_out) => Amount::AmountOut(amount_out),
    };
    Some(IntearDexTrade {
        amount,
        ..trade.clone()
    })
}

/// The asset the token is swapped as on Intear DEX: NEAR and wNEAR as NEAR, a token outside of it
/// as the NEP-141 that the route converts it to
fn intear_dex_asset(token_id: &TokenId, amount_hint: u128) -> Option<AssetId> {
    match token_id {
        TokenId::TokenOnIntearDex(asset_id) => Some(asset_id.into()),
        token_id if is_near(token_id) => {
            convert_to_native(token_id, None, NearToken::from_yoctonear(amount_hint))?;
            Some(AssetId::Near)
        }
        token_id => Some(AssetId::Nep141(convert_to_nep141(token_id, None, 0).1)),
    }
}

/// A swap on Intear DEX for an [`IntearDexTrade`]: on one DEX, on the first of several that
/// fills it, or on DEXes one after another
pub struct IntearDexSwap {
    /// Each after the first swaps what the one before it gave
    steps: Vec<IntearDexSwapStep>,
    /// Of the first option, as in [`HookQuote`]
    estimated_amount: u128,
    /// The least the swap gives, or the most it takes, as in [`HookQuote`]
    worst_case_amount: u128,
    discount: Option<U128>,
    /// Of the options that read an oracle's response
    oracle_request: Option<IntearDexOracleRequest>,
    /// On every swap of the steps, which the estimated amount is with
    fee_discount: Option<QuoteFeeDiscount>,
}

impl IntearDexSwap {
    pub fn of_quote(trade: &IntearDexTrade, quote: HookQuote) -> Self {
        Self {
            estimated_amount: quote.estimated_amount.0,
            worst_case_amount: quote.worst_case_amount.0,
            discount: quote.discount,
            oracle_request: oracle_request_of(&quote),
            fee_discount: quote.fee_discount,
            steps: vec![single_step(trade, vec![swap_option(quote)])],
        }
    }

    /// Swaps on `steps` one after another, each of an asset in and out and the option it swaps
    /// on. Amounts are as in [`HookQuote`], of the whole swap.
    pub fn chain(
        steps: Vec<(AssetId, AssetId, IntearDexSwapOption)>,
        estimated_amount: u128,
        worst_case_amount: u128,
        discount: Option<U128>,
        oracle_request: Option<IntearDexOracleRequest>,
        fee_discount: Option<QuoteFeeDiscount>,
    ) -> Self {
        Self {
            steps: steps
                .into_iter()
                .map(|(asset_in, asset_out, option)| IntearDexSwapStep {
                    asset_in: intear_asset(asset_in),
                    asset_out: intear_asset(asset_out),
                    options: vec![option],
                })
                .collect(),
            estimated_amount,
            worst_case_amount,
            discount,
            oracle_request,
            fee_discount,
        }
    }

    /// A swap whose costs are only those of its route: it burns no gas and asks the oracle, if it
    /// does, for nothing and without a fee. Its quote gives less in the worst case when the trade
    /// has slippage, as the other quotes do, so the route converts the output the same way.
    pub fn burning_no_gas(trade: &IntearDexTrade, asks_oracle: bool) -> Self {
        let trade_amount = match trade.amount {
            Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
        };
        let worst_case_amount = match trade.amount {
            _ if trade.slippage.is_zero() => trade_amount,
            Amount::AmountIn(_) => trade_amount - 1,
            Amount::AmountOut(_) => trade_amount + 1,
        };
        Self::of_quote(
            trade,
            HookQuote {
                dex_id: "placeholder.near/burning-no-gas".parse().unwrap(),
                message: Base64VecU8(vec![]),
                estimated_amount: U128(trade_amount),
                worst_case_amount: U128(worst_case_amount),
                gas: Gas::from_ggas(0, 0),
                discount: None,
                oracle_request: asks_oracle.then(|| OracleRequest {
                    oracle_key: "ed25519:11111111111111111111111111111111".parse().unwrap(),
                    request: Base64VecU8(vec![]),
                    fee: U128(0),
                    fee_swap: None,
                }),
                fee_discount: None,
            },
        )
    }

    /// The quotes, best first, as options that the engine falls back through when one fails. The
    /// swap is constrained to the loosest of their worst cases, but no looser than the trade's
    /// slippage from the best quote, so when a quote without slippage fails, one that gives a bit
    /// less still fills it, and the quotes that give less than the slippage allows are left out.
    /// One oracle request starts the operations, so the quotes that ask an oracle for something
    /// else than the best quote does are left out. The operations have the fee discount of the
    /// best quote, which the other quotes swap with too when they're fallen back to, as far as
    /// the constraint allows. None without quotes, one quote is swapped on alone.
    pub fn fallback_queue(trade: &IntearDexTrade, mut quotes: Vec<HookQuote>) -> Option<Self> {
        match trade.amount {
            Amount::AmountIn(_) => {
                quotes.sort_by_key(|quote| std::cmp::Reverse(quote.estimated_amount.0))
            }
            // The oracle's fee goes on top of what the DEX takes
            Amount::AmountOut(_) => quotes.sort_by_key(|quote| {
                quote.estimated_amount.0
                    + quote
                        .oracle_request
                        .as_ref()
                        .map_or(0, |oracle_request| oracle_request.fee.0)
            }),
        }
        let best = quotes.first()?;
        let best_oracle_request = oracle_request_of(best);
        let slippage_bp =
            pathfinder::slippage_bp(&trade.slippage).expect("Slippage of a trade is from 0 to 1");
        let worst_case_within_slippage = match trade.amount {
            Amount::AmountIn(_) => pathfinder::with_slippage(
                best.estimated_amount.0,
                pathfinder::BASIS_POINTS - slippage_bp,
            ),
            Amount::AmountOut(_) => pathfinder::with_slippage(
                best.estimated_amount.0,
                pathfinder::BASIS_POINTS + slippage_bp,
            ),
        };
        quotes.retain(|quote| {
            let oracle_request = oracle_request_of(quote);
            let within_slippage = match trade.amount {
                Amount::AmountIn(_) => quote.estimated_amount.0 >= worst_case_within_slippage,
                Amount::AmountOut(_) => quote.estimated_amount.0 <= worst_case_within_slippage,
            };
            within_slippage && (oracle_request.is_none() || oracle_request == best_oracle_request)
        });
        if quotes.len() < 2 {
            return quotes
                .into_iter()
                .next()
                .map(|quote| Self::of_quote(trade, quote));
        }
        let best = &quotes[0];
        let worst_case_amounts = quotes.iter().map(|quote| quote.worst_case_amount.0);
        let worst_case_amount = match trade.amount {
            Amount::AmountIn(_) => worst_case_amounts
                .min()
                .expect("There are several quotes")
                .max(worst_case_within_slippage),
            Amount::AmountOut(_) => worst_case_amounts
                .max()
                .expect("There are several quotes")
                .min(worst_case_within_slippage),
        };
        Some(Self {
            estimated_amount: best.estimated_amount.0,
            worst_case_amount,
            discount: best.discount,
            oracle_request: best_oracle_request,
            fee_discount: best.fee_discount,
            steps: vec![single_step(
                trade,
                quotes.into_iter().map(swap_option).collect(),
            )],
        })
    }

    /// The assets that the swap goes through between its input and output
    fn intermediate_assets(&self) -> impl Iterator<Item = &IntearAssetId> {
        self.steps[1..].iter().map(|step| &step.asset_in)
    }
}

#[cfg(test)]
impl IntearDexSwap {
    pub fn estimated_amount(&self) -> u128 {
        self.estimated_amount
    }

    pub fn worst_case_amount(&self) -> u128 {
        self.worst_case_amount
    }

    pub fn discount(&self) -> Option<U128> {
        self.discount
    }

    pub fn oracle_request(&self) -> Option<&IntearDexOracleRequest> {
        self.oracle_request.as_ref()
    }

    pub fn fee_discount(&self) -> Option<QuoteFeeDiscount> {
        self.fee_discount
    }

    /// Of the steps, in order
    pub fn options(&self) -> impl Iterator<Item = &IntearDexSwapOption> {
        self.steps.iter().flat_map(|step| &step.options)
    }
}

/// The only step of a swap of the trade
fn single_step(trade: &IntearDexTrade, options: Vec<IntearDexSwapOption>) -> IntearDexSwapStep {
    IntearDexSwapStep {
        asset_in: intear_asset(trade.asset_in.clone()),
        asset_out: intear_asset(trade.asset_out.clone()),
        options,
    }
}

/// Trades and pools that routes go through are only of NEAR and NEP-141s
fn intear_asset(asset_id: AssetId) -> IntearAssetId {
    IntearAssetId::try_from(asset_id).expect("Swaps are only of NEAR and NEP-141s")
}

fn swap_option(quote: HookQuote) -> IntearDexSwapOption {
    IntearDexSwapOption {
        dex_id: quote.dex_id,
        message: quote.message,
        gas: quote.gas,
    }
}

fn oracle_request_of(quote: &HookQuote) -> Option<IntearDexOracleRequest> {
    quote
        .oracle_request
        .clone()
        .map(|request| IntearDexOracleRequest {
            dex_id: quote.dex_id.clone(),
            request,
        })
}

/// The route of a swap for `trade`, which [`trade_of_request`] made of `request`
pub fn route(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swap: IntearDexSwap,
) -> Option<RouteBlueprint> {
    let token_in = intear_asset(trade.asset_in.clone());
    let token_out = intear_asset(trade.asset_out.clone());
    let referral_fees = payable_referral_fees(&request.fees);
    let discount = swap.discount.map(|discount| match request.amount {
        Amount::AmountIn(_) => Amount::AmountOut(discount.0),
        Amount::AmountOut(_) => Amount::AmountIn(discount.0),
    });
    // The engine takes it from the input before the swap, whose quote is of a trade after it
    let oracle_fee = swap
        .oracle_request
        .as_ref()
        .map_or(0, |oracle_request| oracle_request.request.fee.0);
    // It's the most that the swap that buys the fee takes, which leaves the rest in the input
    let oracle_fee_is_swapped = swap
        .oracle_request
        .as_ref()
        .is_some_and(|oracle_request| oracle_request.request.fee_swap.is_some());
    // The engine takes the fees out of the input of an exact-in swap
    let (swap_amount, constraint, estimated_amount, worst_case_amount) = match request.amount {
        Amount::AmountIn(amount_in) => (
            Amount::AmountIn(amount_in - oracle_fee),
            swap.worst_case_amount,
            Amount::AmountOut(swap.estimated_amount),
            Amount::AmountOut(swap.worst_case_amount),
        ),
        Amount::AmountOut(amount_out) => {
            // Fees on top of the input the DEX takes, so the most it can take with them is the
            // constraint
            let with_fees =
                |amount_in: u128| amount_in_with_referral_fees(&request.fees, amount_in);
            (
                Amount::AmountOut(amount_out),
                with_fees(swap.worst_case_amount),
                Amount::AmountIn(with_fees(swap.estimated_amount) + oracle_fee),
                Amount::AmountIn(with_fees(swap.worst_case_amount) + oracle_fee),
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
        let rest_of_input = IntearDexWithdrawal {
            asset: token_in.clone(),
            amount: IntearDexWithdrawAmount::Full { at_least: None },
        };
        match request.amount {
            Amount::AmountIn(_) if oracle_fee_is_swapped => vec![output, rest_of_input],
            Amount::AmountIn(_) => vec![output],
            Amount::AmountOut(_) => vec![output, rest_of_input],
        }
    };
    let output_withdrawn = !withdrawals.is_empty();

    // The fast path swaps from the input's balance, and exact-out withdraws what's left of the
    // input, as does exact-in after a swap that buys an oracle's fee, so only the other exact-in
    // deposits are swapped whole without a balance of the input. The fast path also puts what each
    // step gives into the trader's balance.
    let registered_assets = if use_fast_path {
        [token_in.clone()]
            .into_iter()
            .chain(swap.intermediate_assets().cloned())
            .chain([token_out.clone()])
            .collect()
    } else if matches!(request.amount, Amount::AmountOut(_)) || oracle_fee_is_swapped {
        vec![token_in.clone(), token_out.clone()]
    } else {
        vec![token_out.clone()]
    };
    // The quote improvement fee is paid out of the output of an exact-in swap, and on top of the
    // input of an exact-out one
    let quote_improvement_fee_asset = match request.amount {
        Amount::AmountIn(_) => &token_out,
        Amount::AmountOut(_) => &token_in,
    };
    let registration = match &request.trader_account_id {
        Some(trader_account_id) => vec![RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id: trader_account_id.clone(),
            asset_ids: registered_assets,
            fee_receivers: referral_fees
                .0
                .iter()
                .map(|(receiver_id, _)| (receiver_id.clone(), token_in.clone()))
                .chain(
                    request
                        .quote_improvement_receiver
                        .iter()
                        .map(|receiver_id| {
                            (receiver_id.clone(), quote_improvement_fee_asset.clone())
                        }),
                )
                .collect(),
        }],
        None => vec![],
    };

    let input_amount = match request.amount {
        Amount::AmountIn(amount_in) => amount_in,
        Amount::AmountOut(_) => constraint + oracle_fee,
    };
    let quote_expected_amount = match estimated_amount {
        Amount::AmountOut(amount_out) => amount_out,
        Amount::AmountIn(amount_in) => amount_in - oracle_fee,
    };
    let quote_improvement_fee =
        request
            .quote_improvement_receiver
            .clone()
            .map(|quote_improvement_receiver| IntearDexQuoteImprovementFee {
                quote_expected_amount,
                quote_improvement_receiver,
            });
    let operations = IntearDexOperations {
        swap: IntearDexSwapOperation {
            amount: swap_amount,
            constraint,
            steps: swap.steps,
            oracle_request: swap.oracle_request.map(Box::new),
            quote_improvement_fee,
        },
        withdrawals,
        referrer: request
            .referrer_id
            .clone()
            .unwrap_or_else(|| DEFAULT_REFERRER_ID.parse().unwrap()),
        referral_fees,
        trader_account_id: request.trader_account_id.clone(),
        fee_discount: swap.fee_discount,
    };
    let output_in_wallet = match &token_out {
        IntearAssetId::Near => TokenId::Near,
        IntearAssetId::Nep141(token_id) => TokenId::Nep141(token_id.clone()),
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
            IntearAssetId::Near => {
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
            IntearAssetId::Nep141(token_in_id) => {
                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.as_ref(),
                    input_amount,
                );
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
            TokenId::TokenOnIntearDex(token_out.clone())
        },
        discount,
        steps: [registration, steps].concat(),
        fee_steps: vec![],
    })
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{AccountId, Action, NearGas};
    use near_sdk::json_types::Base64VecU8;
    use swap_router_types::{fees::AGGREGATOR_FEE_RECEIVER_ID, hook::OracleRequest};
    use xyk_dex_types::{PoolId, SwapArgs, SwapRoute, FULL_ROUTE_FRACTION};

    use super::*;
    use crate::calls::RheaOutput;
    use crate::route::resolve;
    use crate::shared_utils::{TestNetworkView, WRAP_NEAR};
    use crate::types::{Gas, QuoteFees, Slippage};

    const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
    const PLACH_SWAP_GAS: Gas = Gas::from_ggas(34_000, 34_000);
    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const ORACLE_FEE: u128 = 3;

    /// The swap message of the test quotes, through pool 1
    fn plach_message() -> Base64VecU8 {
        Base64VecU8(
            borsh::to_vec(&SwapArgs {
                routes: vec![SwapRoute {
                    fraction: FULL_ROUTE_FRACTION,
                    pool_ids: vec![PoolId::new(1)],
                }],
            })
            .unwrap(),
        )
    }

    /// The route of a Plach quote through pool 1 without a fee discount: 80 out of 100 in, or 100
    /// in for 80 out, with 1 of slippage
    fn test_route(request: SwapRequest) -> Option<RouteBlueprint> {
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request)?;
        let (estimated_amount, worst_case_amount) = match trade.amount {
            Amount::AmountIn(_) => (80, 79),
            Amount::AmountOut(_) => (100, 101),
        };
        let quote = HookQuote {
            dex_id: PLACH_DEX_ID.parse().unwrap(),
            message: plach_message(),
            estimated_amount: U128(estimated_amount),
            worst_case_amount: U128(worst_case_amount),
            gas: PLACH_SWAP_GAS,
            discount: None,
            oracle_request: None,
            fee_discount: None,
        };
        route(
            DexId::Hook("Plach".into()),
            &request,
            &trade,
            IntearDexSwap::of_quote(&trade, quote),
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
            oracle_request: None,
            fee_discount: None,
        }
    }

    /// A [`quote`] of a swap that reads the oracle's response to `request`, which the oracle
    /// takes [`ORACLE_FEE`] of the input for
    fn oracle_quote(
        dex_id: &str,
        estimated_amount: u128,
        worst_case_amount: u128,
        tgas: u64,
        request: &[u8],
    ) -> HookQuote {
        HookQuote {
            oracle_request: Some(OracleRequest {
                oracle_key: ORACLE_KEY.parse().unwrap(),
                request: Base64VecU8(request.to_vec()),
                fee: U128(ORACLE_FEE),
                fee_swap: None,
            }),
            ..quote(dex_id, estimated_amount, worst_case_amount, tgas)
        }
    }

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    fn ft() -> IntearAssetId {
        IntearAssetId::Nep141(account("ft"))
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
            quote_improvement_receiver: None,
        }
    }

    fn plach_operations(
        asset_in: IntearAssetId,
        asset_out: IntearAssetId,
        amount: Amount,
        constraint: u128,
        withdrawals: Vec<IntearDexWithdrawal>,
    ) -> IntearDexOperations {
        IntearDexOperations {
            swap: IntearDexSwapOperation {
                amount,
                constraint,
                steps: vec![IntearDexSwapStep {
                    asset_in,
                    asset_out,
                    options: vec![IntearDexSwapOption {
                        dex_id: PLACH_DEX_ID.parse().unwrap(),
                        message: plach_message(),
                        gas: PLACH_SWAP_GAS,
                    }],
                }],
                oracle_request: None,
                quote_improvement_fee: None,
            },
            withdrawals,
            referrer: account(DEFAULT_REFERRER_ID),
            referral_fees: QuoteFees::default(),
            trader_account_id: Some(trader()),
            fee_discount: None,
        }
    }

    fn withdrawal(asset: IntearAssetId, amount: IntearDexWithdrawAmount) -> IntearDexWithdrawal {
        IntearDexWithdrawal { asset, amount }
    }

    /// Exact-in of 100 that withdraws at least 79 of the output
    fn exact_in_operations(
        asset_in: IntearAssetId,
        asset_out: IntearAssetId,
    ) -> IntearDexOperations {
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

    fn registration(asset_ids: Vec<IntearAssetId>) -> RouteStep {
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

    /// Gas attached to swap on DEXes that need `swap_attached` with `deposit_near`, which burn
    /// `swaps_burn` together, and withdraw a NEP-141
    fn attached_gas(swap_attached: NearGas, swaps_burn: NearGas) -> NearGas {
        let engine_and_withdrawal = NearGas::from_ggas(3_600 + 500 + 16_800 + 3_000);
        let swap_margin = NearGas::from_tgas(4).as_gas() + swaps_burn.as_gas() * 15 / 100;
        NearGas::from_gas(engine_and_withdrawal.as_gas() + swap_attached.as_gas() + swap_margin)
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
    fn exact_in_falls_back_from_the_best_quote_within_its_slippage() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![
                quote("slimedragon.near/xyk", 81, 80, 34),
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
                    // The least of the worst cases, so the fallback fills it when the best fails,
                    // but no less than 1% slippage from the best quote
                    "constraint": "81",
                    "quote_improvement_fee": null,
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
        assert_eq!(
            gas,
            attached_gas(NearGas::from_tgas(44 + 37), NearGas::from_tgas(34 + 27))
        );
    }

    #[test]
    fn exact_out_falls_back_from_the_cheapest_quote_within_its_slippage() {
        let (operations, _) = swap_operations(
            Amount::AmountOut(80),
            vec![
                quote("slimedragon.near/xyk", 101, 102, 34),
                quote("slimedragon.near/otc", 100, 100, 27),
            ],
        );
        let swap = &operations[0]["SwapAdvanced"];
        // The most of the worst cases, but no more than 1% slippage from the cheapest quote
        assert_eq!(swap["constraint"], "101");
        assert_eq!(swap["options"][0]["dex_id"], "slimedragon.near/otc");
        assert_eq!(swap["options"][1]["dex_id"], "slimedragon.near/xyk");
    }

    #[test]
    fn quotes_that_give_less_than_the_slippage_allows_from_the_best_are_left_out() {
        let (operations, _) = swap_operations(
            Amount::AmountIn(100),
            vec![
                quote("slimedragon.near/xyk", 80, 79, 34),
                quote("slimedragon.near/otc", 82, 82, 27),
            ],
        );
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
        assert_eq!(operations[0]["SwapSimple"]["constraint"], "82");
    }

    #[test]
    fn exact_out_quotes_that_take_more_than_the_slippage_allows_from_the_cheapest_are_left_out() {
        let (operations, _) = swap_operations(
            Amount::AmountOut(80),
            vec![
                quote("slimedragon.near/xyk", 102, 103, 34),
                quote("slimedragon.near/otc", 100, 100, 27),
            ],
        );
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
    }

    #[test]
    fn the_best_quote_starts_the_operations_with_its_oracle_request() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![
                quote("slimedragon.near/xyk", 81, 80, 34),
                oracle_quote("slimedragon.near/amp", 82, 82, 25, b"near,usdc"),
                // Can't be a fallback, since it asks for something else
                oracle_quote("slimedragon.near/amp", 81, 81, 25, b"near,usdt"),
            ],
        );
        assert_eq!(
            operations[0],
            serde_json::json!({
                "OracleRequest": {
                    "oracle_key": ORACLE_KEY,
                    "dex_id": "slimedragon.near/amp",
                    "request": "bmVhcix1c2Rj",
                    "fee": ["near", "3"],
                    "swap": null,
                },
            })
        );
        // The engine takes the oracle's fee from the deposit of 100 before the swap
        assert_eq!(
            operations[1]["SwapAdvanced"]["amount"],
            serde_json::json!({ "Amount": { "ExactIn": "97" } })
        );
        let options = &operations[1]["SwapAdvanced"]["options"];
        assert_eq!(options.as_array().unwrap().len(), 2);
        assert_eq!(options[0]["dex_id"], "slimedragon.near/amp");
        assert_eq!(options[1]["dex_id"], "slimedragon.near/xyk");
        // The deposit is held while the operations wait for the oracle
        let oracle_request_and_held_deposit = NearGas::from_ggas(14_700 + 10_000);
        // Qoq's option has a fixed margin of its own besides the other options'
        let qoq_swap_margin = NearGas::from_tgas(4);
        assert_eq!(
            gas,
            attached_gas(NearGas::from_tgas(44 + 35), NearGas::from_tgas(34 + 25))
                .checked_add(oracle_request_and_held_deposit)
                .unwrap()
                .checked_add(qoq_swap_margin)
                .unwrap()
        );
    }

    #[test]
    fn a_swap_of_the_deposit_buys_the_oracle_fee_in_another_asset() {
        let fee_swap = swap_router_types::hook::OracleFeeSwap {
            dex_id: "slimedragon.near/xyk".parse().unwrap(),
            message: plach_message(),
            gas: Gas::from_ggas(5_000, 6_000),
            fee: (AssetId::Nep141("usdc.near".parse().unwrap()), U128(1_000)),
        };
        let fee_in_deposit = || oracle_quote("slimedragon.near/amp", 82, 82, 25, b"near,usdc");
        let mut fee_bought = fee_in_deposit();
        fee_bought.oracle_request.as_mut().unwrap().fee_swap = Some(fee_swap);
        let (operations, gas) = swap_operations(Amount::AmountIn(100), vec![fee_bought]);
        assert_eq!(
            operations[0],
            serde_json::json!({
                "OracleRequest": {
                    "oracle_key": ORACLE_KEY,
                    "dex_id": "slimedragon.near/amp",
                    "request": "bmVhcix1c2Rj",
                    "fee": ["nep141:usdc.near", "1000"],
                    "swap": {
                        "dex_id": "slimedragon.near/xyk",
                        "message": plach_message(),
                        "asset_in": "near",
                        "constraint": "3",
                    },
                },
            })
        );
        // The swap takes at most 3 of the deposit of 100, and what it doesn't take is withdrawn
        assert_eq!(
            operations[1]["SwapSimple"]["amount"],
            serde_json::json!({ "Amount": { "ExactIn": "97" } })
        );
        assert_eq!(
            operations[3],
            serde_json::json!({
                "Withdraw": {
                    "asset_id": "near",
                    "amount": { "Full": { "at_least": null } },
                    "to": null,
                    "rescue_address": null,
                },
            })
        );
        // The fee's swap, its refund if the oracle doesn't respond, and the withdrawal
        let (_, gas_with_fee_in_deposit) =
            swap_operations(Amount::AmountIn(100), vec![fee_in_deposit()]);
        let rest_withdrawal =
            Gas::from_ggas(500 + 2_900, 500 + 5_900) + crate::calls::near_receive_gas(None);
        assert_eq!(
            gas,
            gas_with_fee_in_deposit
                .checked_add(NearGas::from_tgas(6 + 15))
                .unwrap()
                .checked_add(rest_withdrawal.needs_attached)
                .unwrap()
        );
    }

    #[test]
    fn exact_out_deposits_the_oracle_fee_on_top_of_what_the_swap_takes() {
        let request = SwapRequest {
            trader_account_id: None,
            ..request(TokenId::Near, ft_output(), Amount::AmountOut(80))
        };
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).unwrap();
        let swap = IntearDexSwap::of_quote(
            &trade,
            oracle_quote("slimedragon.near/amp", 100, 101, 25, b"near,usdc"),
        );
        let route = route(DexId::IntearDex, &request, &trade, swap).unwrap();
        assert_eq!(route.estimated_amount, Amount::AmountIn(103));
        assert_eq!(route.worst_case_amount, Amount::AmountIn(104));
        let Some(RouteStep::Call(CallBlueprint::IntearDexDepositNear { amount, .. })) =
            route.steps.last()
        else {
            panic!("The route doesn't end with a deposit of NEAR");
        };
        assert_eq!(*amount, NearToken::from_yoctonear(104));
        let (args, _) = last_call(&route);
        let swap = &args["operations"]["operations"][1]["SwapSimple"];
        assert_eq!(
            swap["amount"],
            serde_json::json!({ "Amount": { "ExactOut": "80" } })
        );
        assert_eq!(swap["constraint"], "101");
    }

    #[test]
    fn exact_out_ranks_quotes_with_the_oracle_fee_on_top() {
        let (operations, _) = swap_operations(
            Amount::AmountOut(80),
            vec![
                // Takes 102 with the oracle's fee
                oracle_quote("slimedragon.near/amp", 99, 99, 25, b"near,usdc"),
                quote("slimedragon.near/otc", 100, 100, 27),
            ],
        );
        assert_eq!(operations.as_array().unwrap().len(), 3);
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
    }

    #[test]
    fn quotes_with_oracle_requests_are_left_out_when_another_quote_is_best() {
        let (operations, gas) = swap_operations(
            Amount::AmountIn(100),
            vec![
                oracle_quote("slimedragon.near/amp", 81, 81, 25, b"near,usdc"),
                quote("slimedragon.near/otc", 82, 82, 27),
            ],
        );
        assert_eq!(operations.as_array().unwrap().len(), 2);
        assert_eq!(
            operations[0]["SwapSimple"]["dex_id"],
            "slimedragon.near/otc"
        );
        assert_eq!(
            gas,
            attached_gas(NearGas::from_tgas(37), NearGas::from_tgas(27))
        );
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
        assert_eq!(
            gas,
            attached_gas(NearGas::from_tgas(37), NearGas::from_tgas(27))
        );
    }

    fn usdc() -> AssetId {
        AssetId::Nep141(account("usdc.near"))
    }

    /// NEAR to USDC on `first_dex`, then USDC to FT on `second_dex`, swaps that burn 34 and 27 TGas
    fn chain(first_dex: &str, second_dex: &str) -> IntearDexSwap {
        let option = |dex_id: &str, tgas: u64| IntearDexSwapOption {
            dex_id: dex_id.parse().unwrap(),
            message: Base64VecU8(dex_id.as_bytes().to_vec()),
            gas: Gas::from_ggas(tgas * 1_000, (tgas + 10) * 1_000),
        };
        IntearDexSwap::chain(
            vec![
                (AssetId::Near, usdc(), option(first_dex, 34)),
                (
                    usdc(),
                    AssetId::Nep141(account("ft")),
                    option(second_dex, 27),
                ),
            ],
            80,
            79,
            None,
            None,
            None,
        )
    }

    #[test]
    fn a_fee_discount_is_signed_for_every_swap_of_the_operations() {
        use intear_dex_common::FeeDiscountFraction;
        use intear_dex_types::{FeeDiscount, Operation, OperationsFeeDiscount, SignedFeeDiscount};
        use near_min_api::types::near_crypto::{KeyType, SecretKey, Signature};
        use sha2::{Digest, Sha256};
        use swap_router_types::fee_discount::{fee_discount_signature_payload, FeeDiscounter};

        let secret_key = SecretKey::from_seed(KeyType::ED25519, "fee discounter");
        // Tests that sign start it with the same key
        let _ = FeeDiscounter {
            secret_key: secret_key.clone(),
        }
        .start();
        let request = request(TokenId::Near, ft_output(), Amount::AmountIn(100));
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).unwrap();
        let fee_discount = QuoteFeeDiscount {
            fee_discount: FeeDiscountFraction::new(250_000),
            expiration_block_height: 1_001_000,
        };
        let undiscounted = chain("slimedragon.near/xyk", "slimedragon.near/otc");
        let (undiscounted_args, undiscounted_gas) =
            last_call(&route(DexId::IntearDex, &request, &trade, undiscounted).unwrap());
        assert!(undiscounted_args["operations"]
            .get("fee_discount")
            .is_none());
        let swap = IntearDexSwap {
            fee_discount: Some(fee_discount),
            ..chain("slimedragon.near/xyk", "slimedragon.near/otc")
        };
        let (args, gas) = last_call(&route(DexId::IntearDex, &request, &trade, swap).unwrap());
        let operations: Vec<Operation> =
            serde_json::from_value(args["operations"]["operations"].clone()).unwrap();
        assert_eq!(operations.len(), 3);
        let signed: SignedFeeDiscount =
            serde_json::from_value(args["operations"]["fee_discount"].clone()).unwrap();
        assert_eq!(
            signed.discount,
            FeeDiscount::ForOperations(OperationsFeeDiscount {
                operations_hash: <[u8; 32]>::from(Sha256::digest(
                    borsh::to_vec(&operations).unwrap()
                ))
                .into(),
                trader_id: Some(trader()),
                fee_discount: fee_discount.fee_discount,
                expiration_block_height: fee_discount.expiration_block_height,
            })
        );
        assert!(Signature::from_parts(KeyType::ED25519, &signed.signature.0)
            .unwrap()
            .verify(
                &Sha256::digest(
                    borsh::to_vec(&fee_discount_signature_payload(&signed.discount)).unwrap()
                ),
                &secret_key.public_key(),
            ));
        // The engine checks it once for both swaps
        assert!(gas > undiscounted_gas);
    }

    #[test]
    fn a_chain_swaps_each_step_on_what_the_step_before_gave() {
        let request = SwapRequest {
            trader_account_id: None,
            quote_improvement_receiver: Some(account("improvement.near")),
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(100))
        };
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).unwrap();
        let swap = chain("slimedragon.near/xyk", "slimedragon.near/otc");
        let route = route(DexId::IntearDex, &request, &trade, swap).unwrap();
        let (args, gas) = last_call(&route);
        assert_eq!(
            args["operations"]["operations"],
            serde_json::json!([
                {
                    "SwapSimple": {
                        "dex_id": "slimedragon.near/xyk",
                        "message": "c2xpbWVkcmFnb24ubmVhci94eWs=",
                        "asset_in": "near",
                        "asset_out": "nep141:usdc.near",
                        "amount": { "Amount": { "ExactIn": "100" } },
                        // Only the output of the last step is constrained, and improves on the
                        // quote
                        "constraint": null,
                        "quote_improvement_fee": null,
                    },
                },
                {
                    "SwapSimple": {
                        "dex_id": "slimedragon.near/otc",
                        "message": "c2xpbWVkcmFnb24ubmVhci9vdGM=",
                        "asset_in": "nep141:usdc.near",
                        "asset_out": "nep141:ft",
                        "amount": "OutputOfLastIn",
                        "constraint": "79",
                        "quote_improvement_fee": {
                            "quote_expected_amount": "80",
                            "quote_improvement_receiver": "improvement.near",
                        },
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
            ])
        );
        // Each step with its own margin
        let second_step_margin =
            NearGas::from_tgas(4).as_gas() + NearGas::from_tgas(27).as_gas() * 15 / 100;
        assert_eq!(
            gas,
            NearGas::from_gas(
                attached_gas(NearGas::from_tgas(44 + 37), NearGas::from_tgas(34)).as_gas()
                    + second_step_margin
                    // Paying the quote improvement
                    + NearGas::from_ggas(600).as_gas()
            )
        );
    }

    #[test]
    fn a_chain_on_the_fast_path_registers_what_each_step_gives() {
        let request = request(
            TokenId::TokenOnIntearDex(IntearAssetId::Near),
            ft_output(),
            Amount::AmountIn(100),
        );
        let network = TestNetworkView::default().with_test_prices();
        let trade = trade_of_request(&network, &request).unwrap();
        let swap = chain("slimedragon.near/xyk", "slimedragon.near/otc");
        let route = route(DexId::IntearDex, &request, &trade, swap).unwrap();
        assert_eq!(
            route.steps[0],
            registration(vec![
                IntearAssetId::Near,
                IntearAssetId::Nep141(account("usdc.near")),
                ft(),
            ])
        );
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
                                "quote_improvement_fee": null,
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
                    deposit_near(100, exact_in_operations(IntearAssetId::Near, ft())),
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
                    deposit_near(100, exact_in_operations(IntearAssetId::Near, ft())),
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
                    RouteStep::RegisterOnRheaIfNeeded {
                        trader_account_id: trader(),
                        token_ids: vec![account(WRAP_NEAR)],
                    },
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account(WRAP_NEAR),
                        amount: 100,
                        output: RheaOutput::Near {
                            trader_account_id: Some(trader()),
                        },
                    }),
                    deposit_near(100, exact_in_operations(IntearAssetId::Near, ft())),
                ],
            ))
        );
    }

    #[test]
    fn amount_in_intear_near_to_ft_fast_path() {
        assert_eq!(
            test_route(request(
                TokenId::TokenOnIntearDex(IntearAssetId::Near),
                ft_output(),
                Amount::AmountIn(100)
            )),
            Some(plach_route_of(
                Amount::AmountOut(80),
                Amount::AmountOut(79),
                TokenId::TokenOnIntearDex(IntearAssetId::Nep141(account("ft"))),
                vec![
                    registration(vec![IntearAssetId::Near, ft()]),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAssetId::Near,
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
                    registration(vec![IntearAssetId::Near]),
                    RouteStep::Call(CallBlueprint::InputNep141TransferCallIntearDex {
                        token_id: account("ft"),
                        amount: 100,
                        operations: exact_in_operations(ft(), IntearAssetId::Near),
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
                    registration(vec![IntearAssetId::Near]),
                    trader_storage("ft"),
                    RouteStep::RegisterOnRheaIfNeeded {
                        trader_account_id: trader(),
                        token_ids: vec![account("ft")],
                    },
                    RouteStep::Call(CallBlueprint::RheaWithdraw {
                        token_id: account("ft"),
                        amount: 100,
                        output: RheaOutput::Nep141,
                    }),
                    RouteStep::Call(CallBlueprint::InputNep141TransferCallIntearDex {
                        token_id: account("ft"),
                        amount: 100,
                        operations: exact_in_operations(ft(), IntearAssetId::Near),
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
                    registration(vec![IntearAssetId::Near, ft()]),
                    trader_storage("ft"),
                    deposit_near(
                        101,
                        plach_operations(
                            IntearAssetId::Near,
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
                                    IntearAssetId::Near,
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
                TokenId::TokenOnIntearDex(IntearAssetId::Near),
                ft_output(),
                Amount::AmountOut(80)
            )),
            Some(plach_route_of(
                Amount::AmountIn(100),
                Amount::AmountIn(101),
                ft_output(),
                vec![
                    registration(vec![IntearAssetId::Near, ft()]),
                    trader_storage("ft"),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAssetId::Near,
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
        let intear_ft = TokenId::TokenOnIntearDex(IntearAssetId::Nep141(account("ft")));
        assert_eq!(
            test_route(request(
                TokenId::TokenOnIntearDex(IntearAssetId::Near),
                intear_ft.clone(),
                Amount::AmountOut(80)
            )),
            Some(plach_route_of(
                Amount::AmountIn(100),
                Amount::AmountIn(101),
                intear_ft,
                vec![
                    registration(vec![IntearAssetId::Near, ft()]),
                    RouteStep::Call(CallBlueprint::IntearDexExecuteOperations {
                        operations: plach_operations(
                            IntearAssetId::Near,
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
                        ..exact_in_operations(IntearAssetId::Near, ft())
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
    async fn a_swap_burning_no_gas_costs_what_the_route_costs_besides_the_swap() {
        let network = TestNetworkView::default().with_test_prices();
        let request = request(TokenId::Near, ft_output(), Amount::AmountIn(10u128.pow(24)));
        let trade = trade_of_request(&network, &request).unwrap();
        let resolved = |route| async { resolve(&network, route).await.unwrap() };
        let plach = resolved(test_route(request.clone()).unwrap()).await;
        let burning_no_gas = |asks_oracle| {
            route(
                DexId::IntearDex,
                &request,
                &trade,
                IntearDexSwap::burning_no_gas(&trade, asks_oracle),
            )
            .unwrap()
        };
        let without_oracle_request = resolved(burning_no_gas(false)).await;
        let with_oracle_request = resolved(burning_no_gas(true)).await;

        assert_eq!(
            plach.gas().estimated_burn.as_gas(),
            without_oracle_request.gas().estimated_burn.as_gas()
                + PLACH_SWAP_GAS.estimated_burn.as_gas()
        );
        assert_eq!(
            plach.storage_deposits(),
            without_oracle_request.storage_deposits()
        );
        assert_eq!(plach.token_output, without_oracle_request.token_output);
        assert!(
            with_oracle_request.gas().estimated_burn > without_oracle_request.gas().estimated_burn
        );
    }

    #[test]
    fn oracle_fee_comes_out_of_an_exact_input_before_the_app_fees() {
        let network = TestNetworkView::default().with_test_prices();
        let exact_in = SwapRequest {
            fees: r#"{"app.near":0.005}"#.parse().unwrap(),
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(1_000_000))
        };
        let trade = trade_of_request(&network, &exact_in).unwrap();
        // 0.5% of the 800,000 that the oracle's fee leaves goes to the app and the aggregator
        assert_eq!(
            trade_after_oracle_fee(&exact_in, &trade, 200_000)
                .unwrap()
                .amount,
            Amount::AmountIn(796_000)
        );
        assert!(trade_after_oracle_fee(&exact_in, &trade, 1_000_000).is_none());

        let exact_out = request(TokenId::Near, ft_output(), Amount::AmountOut(1_000_000));
        let trade = trade_of_request(&network, &exact_out).unwrap();
        assert_eq!(
            trade_after_oracle_fee(&exact_out, &trade, 200_000)
                .unwrap()
                .amount,
            Amount::AmountOut(1_000_000)
        );
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
                    (account("ref.near"), IntearAssetId::Near),
                    (account("ref2.near"), IntearAssetId::Near),
                    (AGGREGATOR_FEE_RECEIVER_ID.to_owned(), IntearAssetId::Near),
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
                    asset_ids: vec![IntearAssetId::Near],
                    for_account: Some(account("ref.near")),
                },
                CallBlueprint::IntearDexRegisterAssets {
                    asset_ids: vec![IntearAssetId::Near],
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

    #[test]
    fn quote_improvement_receiver_takes_what_beats_the_estimate() {
        let exact_in = test_route(SwapRequest {
            quote_improvement_receiver: Some(account("improvement.near")),
            ..request(TokenId::Near, ft_output(), Amount::AmountIn(1_000_000))
        })
        .unwrap();
        let Amount::AmountOut(estimated_amount_out) = exact_in.estimated_amount else {
            panic!("An exact-in route estimates its output");
        };
        let (args, _) = last_call(&exact_in);
        assert_eq!(
            args["operations"]["operations"][0]["SwapSimple"]["quote_improvement_fee"],
            serde_json::json!({
                "quote_expected_amount": estimated_amount_out.to_string(),
                "quote_improvement_receiver": "improvement.near",
            })
        );
        // The trader registers the receiver for the output, which it's paid in
        assert_eq!(
            exact_in.steps[0],
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![ft()],
                fee_receivers: vec![(account("improvement.near"), ft())],
            }
        );

        // Of an exact output, the estimated input with the referral fees
        let exact_out = test_route(SwapRequest {
            fees: r#"{"ref.near":0.05}"#.parse().unwrap(),
            quote_improvement_receiver: Some(account("improvement.near")),
            ..request(TokenId::Near, ft_output(), Amount::AmountOut(80))
        })
        .unwrap();
        assert_eq!(exact_out.estimated_amount, Amount::AmountIn(100 + 5 + 2));
        // And for the input, along with the referrers
        assert_eq!(
            exact_out.steps[0],
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![IntearAssetId::Near, ft()],
                fee_receivers: vec![
                    (account("ref.near"), IntearAssetId::Near),
                    (AGGREGATOR_FEE_RECEIVER_ID.to_owned(), IntearAssetId::Near),
                    (account("improvement.near"), IntearAssetId::Near),
                ],
            }
        );
        let (args, _) = last_call(&exact_out);
        assert_eq!(
            args["operations"]["operations"][0]["SwapSimple"]["quote_improvement_fee"],
            serde_json::json!({
                "quote_expected_amount": "107",
                "quote_improvement_receiver": "improvement.near",
            })
        );
    }
}
