use std::{future::Future, pin::Pin};

use intear_dex_types::{
    Operation, ReferralFees, SwapOperationAmount, SwapRequestAmount, WithdrawAmount,
};
use lazy_static::lazy_static;
use near_min_api::types::{
    near_crypto::{SecretKey, Signature},
    AccountId, Action, Balance, BlockHeight, FunctionCallAction, NearGas, NearToken,
};
use near_sdk::json_types::{Base64VecU8, U128};
use pathfinder::{
    plach::{self, SplitRouteApiResponse},
    MaxHops, QuoteAmount,
};
use sha2::{Digest, Sha256};
use tracing::info;
use xyk_dex_types::{
    FeeDiscount, FeeFractionFraction, SignedFeeDiscount, SwapArgs, SwapRoute,
    FULL_FEE_FRACTION_FRACTION,
};

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage,
        intear_registration, is_near, payable_referral_fees, price_raw, register_input_in_wallet,
        swap_call_gas, Mainnet, NetworkView, DEFAULT_REFERRER_ID,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct IntearPlachProvider;

pub const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const FT_TRANSFER_CALL_RESERVED_GAS: NearGas = NearGas::from_tgas(20);
const WITHDRAWAL_TRANSFER_GAS: NearGas = NearGas::from_tgas(10);
const WITHDRAWAL_CALLBACK_GAS: NearGas = NearGas::from_tgas(5);
const SPARE_RESERVED_GAS: NearGas = NearGas::from_tgas(2);

fn reserved_gas(operations: &[Operation]) -> NearGas {
    let withdrawals_gas = operations
        .iter()
        .map(|operation| match operation {
            Operation::Withdraw {
                asset_id: AssetId::Near,
                ..
            } => WITHDRAWAL_CALLBACK_GAS.as_gas(),
            Operation::Withdraw { .. } => {
                WITHDRAWAL_TRANSFER_GAS.as_gas() + WITHDRAWAL_CALLBACK_GAS.as_gas()
            }
            _ => 0,
        })
        .sum::<u64>();
    NearGas::from_gas(SPARE_RESERVED_GAS.as_gas() + withdrawals_gas)
}

pub use intear_dex_types::AssetId;

const TEAR_TOKEN_ID: &str = "token.intear.near";
const ONE_TEAR: Balance = 10u128.pow(24);

const TEAR_FEE_DISCOUNTS: [(Balance, FeeFractionFraction); 3] = [
    (100_000 * ONE_TEAR, FULL_FEE_FRACTION_FRACTION * 3 / 4), // 100,000 = 75% discount
    (1_000 * ONE_TEAR, FULL_FEE_FRACTION_FRACTION / 4),       // 1,000 = 25% discount
    (10 * ONE_TEAR, FULL_FEE_FRACTION_FRACTION / 10),         // 10 = 10% discount
];

/// How long a quote's fee discount can be used, ~10 minutes
const FEE_DISCOUNT_VALIDITY_BLOCKS: BlockHeight = 1000;

fn tear_fee_discount(tear_balance: Balance) -> Option<FeeFractionFraction> {
    TEAR_FEE_DISCOUNTS
        .into_iter()
        .find(|(min_balance, _)| tear_balance >= *min_balance)
        .map(|(_, fee_discount)| fee_discount)
}

lazy_static! {
    pub static ref FEE_DISCOUNTER: FeeDiscounter = FeeDiscounter::from_env();
}

/// Signs fee discounts with the key set as the xyk DEX's fee discount signer
pub struct FeeDiscounter {
    secret_key: SecretKey,
}

impl FeeDiscounter {
    fn from_env() -> Self {
        let account_id: AccountId = std::env::var("FEE_DISCOUNTER_ACCOUNT_ID")
            .expect("FEE_DISCOUNTER_ACCOUNT_ID must be set")
            .parse()
            .expect("Invalid FEE_DISCOUNTER_ACCOUNT_ID");
        let secret_key: SecretKey = std::env::var("FEE_DISCOUNTER_PRIVATE_KEY")
            .expect("FEE_DISCOUNTER_PRIVATE_KEY must be set")
            .parse()
            .expect("Invalid FEE_DISCOUNTER_PRIVATE_KEY");
        info!(
            "Signing Plach fee discounts as {account_id} with {}",
            secret_key.public_key()
        );
        Self { secret_key }
    }

    /// Signs sha256(borsh(`discount`)), which the DEX verifies
    fn sign(&self, discount: FeeDiscount) -> SignedFeeDiscount {
        let hash = Sha256::digest(borsh::to_vec(&discount).unwrap());
        let signature = match self.secret_key.sign(&hash) {
            Signature::ED25519(signature) => signature.to_bytes().to_vec(),
            // Borsh puts the key type before the 65 bytes of r, s, and the recovery id
            signature @ Signature::SECP256K1(_) => borsh::to_vec(&signature).unwrap()[1..].to_vec(),
            Signature::MLDSA65(_) => panic!("The DEX can't verify ML-DSA-65 fee discounts"),
        };
        SignedFeeDiscount {
            discount,
            signature,
        }
    }
}

trait PlachQuotes: Send + Sync {
    fn find_path(
        &self,
        request: plach::Request,
    ) -> impl Future<Output = Option<SplitRouteApiResponse>> + Send;
}

struct MainnetPlachQuotes;

impl PlachQuotes for MainnetPlachQuotes {
    async fn find_path(&self, request: plach::Request) -> Option<SplitRouteApiResponse> {
        plach::find_path(request)
            .await
            .inspect_err(|e| info!("No Plach route: {e:#}"))
            .ok()
    }
}

impl Provider for IntearPlachProvider {
    fn dex_id(&self) -> DexId {
        DexId::Plach
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            route(
                request,
                &Mainnet,
                &MainnetPlachQuotes,
                Some(&*FEE_DISCOUNTER),
            )
            .await
        })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl PlachQuotes,
    fee_discounter: Option<&FeeDiscounter>,
) -> Option<Route> {
    let amount_hint = match request.amount {
        Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
    };

    let slippage = get_slippage(
        network,
        request.slippage,
        &request.token_in,
        &request.token_out,
    );

    let (_, token_in) = match &request.token_in {
        TokenId::TokenOnIntearDex(asset_id) => (Vec::new(), asset_id.clone()),
        t if is_near(t) => (
            convert_to_native(t, None, NearToken::from_yoctonear(amount_hint)).await?,
            AssetId::Near,
        ),
        _ => convert_to_nep141(&request.token_in, None, 0)
            .await
            .map(|(v, t)| (v, AssetId::Nep141(t)))?,
    };
    let (_, token_out) = match &request.token_out {
        TokenId::TokenOnIntearDex(asset_id) => (Vec::new(), asset_id.clone()),
        t if is_near(t) => (
            convert_to_native(t, None, NearToken::from_yoctonear(amount_hint)).await?,
            AssetId::Near,
        ),
        _ => convert_to_nep141(&request.token_out, None, 0)
            .await
            .map(|(v, t)| (v, AssetId::Nep141(t)))?,
    };

    if token_in == token_out {
        return None;
    }

    // The engine takes them out of the input before the DEX gets it, and only pays referrers that
    // registered the input, which the route does for them
    let referral_fees = ReferralFees::from(&payable_referral_fees(&request.fees));
    let fees_total = |fees: Vec<(AccountId, U128)>| fees.iter().map(|(_, fee)| fee.0).sum::<u128>();

    let fee_discount = match (fee_discounter, &request.trader_account_id) {
        (Some(fee_discounter), Some(trader_account_id)) => {
            let tear_balance = network
                .ft_balance(TEAR_TOKEN_ID.parse().unwrap(), trader_account_id)
                .await
                .inspect_err(|e| info!("No TEAR balance to discount fees by: {e}"))
                .ok()?;
            tear_fee_discount(tear_balance).map(|fee_discount| (fee_discounter, fee_discount))
        }
        _ => None,
    };

    let response = quotes
        .find_path(plach::Request {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount: match request.amount {
                Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(
                    amount_in - fees_total(referral_fees.amounts_out_of(U128(amount_in))),
                ),
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            max_hops: MaxHops::Four,
            slippage,
            near_price_raw: price_raw(network, &TokenId::Near)?,
            quoted_token_price_raw: price_raw(
                network,
                match request.amount {
                    Amount::AmountIn(_) => &request.token_out,
                    Amount::AmountOut(_) => &request.token_in,
                },
            )?,
            fee_discount: fee_discount.map_or(0, |(_, fee_discount)| fee_discount),
        })
        .await?;

    let swap_gas = match response {
        SplitRouteApiResponse::ExactIn { swap_gas, .. }
        | SplitRouteApiResponse::ExactOut { swap_gas, .. } => swap_gas,
    };
    let discount = fee_discount.map(|_| match response {
        SplitRouteApiResponse::ExactIn { discount, .. } => Amount::AmountOut(discount.0),
        SplitRouteApiResponse::ExactOut { discount, .. } => Amount::AmountIn(discount.0),
    });
    // `dex_amount` is what the DEX swaps, `swap_amount` what the engine takes with the fees
    let (swap_routes, dex_amount, swap_amount, constraint, estimated_amount, worst_case_amount) =
        match response {
            SplitRouteApiResponse::ExactIn {
                routes,
                amount_in,
                amount_out,
                ..
            } => {
                let total_min_amount_out = routes.iter().map(|route| route.min_amount_out.0).sum();
                let swap_routes = routes
                    .into_iter()
                    .map(|route| SwapRoute {
                        fraction: route.split_fraction,
                        pool_ids: route.pools.iter().map(|step| step.pool_id).collect(),
                    })
                    .collect::<Vec<_>>();
                let Amount::AmountIn(amount_in_with_fees) = request.amount else {
                    unreachable!("Exact in is quoted for an amount in");
                };
                (
                    swap_routes,
                    SwapRequestAmount::ExactIn(U128(amount_in.0)),
                    SwapRequestAmount::ExactIn(U128(amount_in_with_fees)),
                    total_min_amount_out,
                    Amount::AmountOut(amount_out.0),
                    Amount::AmountOut(total_min_amount_out),
                )
            }
            SplitRouteApiResponse::ExactOut {
                routes,
                amount_in,
                max_amount_in,
                amount_out,
                ..
            } => {
                let swap_routes = routes
                    .into_iter()
                    .map(|route| SwapRoute {
                        fraction: route.split_fraction,
                        pool_ids: route.pools.iter().map(|step| step.pool_id).collect(),
                    })
                    .collect::<Vec<_>>();
                // Fees on top of the input the DEX takes, so the most it can take with them is the
                // constraint
                let with_fees = |amount_in: u128| {
                    amount_in + fees_total(referral_fees.amounts_on_top_of(U128(amount_in)))
                };
                (
                    swap_routes,
                    SwapRequestAmount::ExactOut(U128(amount_out.0)),
                    SwapRequestAmount::ExactOut(U128(amount_out.0)),
                    with_fees(max_amount_in.0),
                    Amount::AmountIn(with_fees(amount_in.0)),
                    Amount::AmountIn(with_fees(max_amount_in.0)),
                )
            }
        };
    if swap_routes.is_empty() {
        return None;
    }
    let fee_discount = match fee_discount {
        Some((fee_discounter, fee_discount)) => {
            let current_block_height = network
                .current_block_height()
                .inspect_err(|e| info!("No block height to expire a fee discount at: {e}"))
                .ok()?;
            Some(fee_discounter.sign(FeeDiscount {
                routes: swap_routes.clone(),
                asset_in: token_in.clone(),
                asset_out: token_out.clone(),
                trader_id: request.trader_account_id.clone(),
                amount: dex_amount,
                fee_discount,
                expiration_block_height: current_block_height + FEE_DISCOUNT_VALIDITY_BLOCKS,
            }))
        }
        None => None,
    };
    let swaps = vec![Operation::SwapSimple {
        dex_id: PLACH_DEX_ID.parse().unwrap(),
        message: Base64VecU8(
            borsh::to_vec(&SwapArgs {
                routes: swap_routes,
                fee_discount,
            })
            .unwrap(),
        ),
        asset_in: token_in.clone(),
        asset_out: token_out.clone(),
        amount: SwapOperationAmount::Amount(swap_amount),
        constraint: Some(U128(constraint)),
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
            intear_registration(
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

    let reserved_gas = reserved_gas(&operations);
    let transactions = if use_fast_path {
        let execute_operations_action =
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "execute_operations".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": operations,
                            "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                            "referral_fees": referral_fees,
                        }))
                        .unwrap(),
                        gas: swap_call_gas(swap_gas, reserved_gas, false),
                        deposit: NearToken::from_yoctonear(1),
                    }));
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
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                actions: vec![execute_operations_action],
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
                let deposit_near_action = Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": operations,
                                    "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                                    "referral_fees": referral_fees,
                                },
                            }))
                            .unwrap(),
                            gas: swap_call_gas(swap_gas, reserved_gas, false),
                            deposit: NearToken::from_yoctonear(input_amount),
                        }));
                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![deposit_near_action],
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
                let ft_transfer_call_swap_action =
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "ft_transfer_call".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                    "amount": input_amount.to_string(),
                                    "msg": serde_json::to_string(&serde_json::json!({
                                        "operations": operations,
                                        "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                                        "referral_fees": referral_fees,
                                    })).unwrap(),
                                }))
                                .unwrap(),
                                gas: swap_call_gas(
                                    swap_gas,
                                    NearGas::from_gas(
                                        reserved_gas.as_gas()
                                            + FT_TRANSFER_CALL_RESERVED_GAS.as_gas(),
                                    ),
                                    true,
                                ),
                                deposit: NearToken::from_yoctonear(1),
                            }));
                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: token_in_id.clone(),
                    actions: vec![ft_transfer_call_swap_action],
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

    Some(Route {
        dex_id: DexId::Plach,

        estimated_amount,
        worst_case_amount,
        execution_instructions: [registration_transactions, transactions].concat(),
        deprecated_needs_unwrap_always_false: false,
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

    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::shared_utils::{
        create_rhea_withdraw_action, create_storage_deposit_action_for_contract,
        create_unwrap_action, TestNetworkView, AGGREGATOR_FEE_RECEIVER_ID, WRAP_NEAR,
    };
    use crate::types::Slippage;

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

    impl PlachQuotes for TestPlachQuotes {
        async fn find_path(&self, request: plach::Request) -> Option<SplitRouteApiResponse> {
            if !self.enabled {
                return None;
            }
            let (token_in, token_out) = (&request.token_in, &request.token_out);
            if self.empty_routes {
                return Some(SplitRouteApiResponse::ExactIn {
                    routes: vec![],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: near_min_api::types::U128(100),
                    amount_out: near_min_api::types::U128(80),
                    discount: near_min_api::types::U128(0),
                    swap_gas: NearGas::from_tgas(34),
                });
            }
            let discount = near_min_api::types::U128(match request.fee_discount {
                0 => 0,
                _ => 5,
            });
            match request.amount {
                QuoteAmount::ExactIn(amount_in) => Some(SplitRouteApiResponse::ExactIn {
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
                    swap_gas: NearGas::from_tgas(34),
                }),
                QuoteAmount::ExactOut(_) => Some(SplitRouteApiResponse::ExactOut {
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
                    swap_gas: NearGas::from_tgas(34),
                }),
            }
        }
    }

    /// Operations are typed with the engine's own types, so this pins the JSON the engine reads
    #[tokio::test]
    async fn operations_json_matches_engine_format() {
        let route = route(
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
            None,
        )
        .await
        .unwrap();
        let Some(ExecutionInstruction::NearTransaction { actions, .. }) =
            route.execution_instructions.last()
        else {
            panic!("No swap transaction");
        };
        let [Action::FunctionCall(deposit_near)] = actions.as_slice() else {
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

    #[test]
    fn fee_discount_grows_with_tear_holdings() {
        assert_eq!(tear_fee_discount(0), None);
        assert_eq!(tear_fee_discount(10 * ONE_TEAR - 1), None);
        assert_eq!(tear_fee_discount(10 * ONE_TEAR), Some(100_000));
        assert_eq!(tear_fee_discount(1_000 * ONE_TEAR - 1), Some(100_000));
        assert_eq!(tear_fee_discount(1_000 * ONE_TEAR), Some(250_000));
        assert_eq!(tear_fee_discount(100_000 * ONE_TEAR), Some(750_000));
        assert_eq!(tear_fee_discount(u128::MAX), Some(750_000));
    }

    #[tokio::test]
    async fn no_fee_discount_without_tear() {
        use near_min_api::types::near_crypto::KeyType;

        let fee_discounter = FeeDiscounter {
            secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
        };
        let route = route(
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
            &TestNetworkView::default()
                .with_test_prices()
                .with_ft_balance(TEAR_TOKEN_ID, "trader.near", 10 * ONE_TEAR - 1),
            &TestPlachQuotes::default(),
            Some(&fee_discounter),
        )
        .await
        .unwrap();
        assert_eq!(route.discount, None);
    }

    #[tokio::test]
    async fn signs_fee_discount() {
        use base64::Engine;
        use near_min_api::types::near_crypto::KeyType;

        let fee_discounter = FeeDiscounter {
            secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
        };
        let route = route(
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
            &TestNetworkView::default()
                .with_test_prices()
                .with_ft_balance(TEAR_TOKEN_ID, "trader.near", 1_000 * ONE_TEAR),
            &TestPlachQuotes::default(),
            Some(&fee_discounter),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&route).unwrap()["discount"],
            serde_json::json!({ "amount_out": "5" })
        );
        let Some(ExecutionInstruction::NearTransaction { actions, .. }) =
            route.execution_instructions.last()
        else {
            panic!("No swap transaction");
        };
        let [Action::FunctionCall(deposit_near)] = actions.as_slice() else {
            panic!("Swap transaction isn't a single function call");
        };
        let args = serde_json::from_slice::<serde_json::Value>(&deposit_near.args).unwrap();
        let message = base64::engine::general_purpose::STANDARD
            .decode(
                args["operations"]["operations"][0]["SwapSimple"]["message"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        let SwapArgs {
            routes,
            fee_discount:
                Some(SignedFeeDiscount {
                    discount,
                    signature,
                }),
            ..
        } = borsh::from_slice(&message).unwrap()
        else {
            panic!("No fee discount");
        };
        assert_eq!(discount.routes, routes);
        assert_eq!(discount.asset_in, AssetId::Near);
        assert_eq!(discount.asset_out, AssetId::Nep141("ft".parse().unwrap()));
        assert_eq!(discount.trader_id, Some("trader.near".parse().unwrap()));
        assert_eq!(discount.amount, SwapRequestAmount::ExactIn(U128(100)));
        assert_eq!(discount.fee_discount, 250_000);
        assert_eq!(discount.expiration_block_height, 1_001_000);
        assert!(Signature::from_parts(KeyType::ED25519, &signature)
            .unwrap()
            .verify(
                &Sha256::digest(borsh::to_vec(&discount).unwrap()),
                &fee_discounter.secret_key.public_key(),
            ));
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(69)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_unwrap_action(NearToken::from_yoctonear(100))],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(69)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            true,
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(69)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(54)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(89)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                // A new trader with one balance takes less than the minimum
                                deposit: NearToken::from_millinear(5),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(89)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),true
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(74)),
                            deposit: NearToken::from_yoctonear(101),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
            route(
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
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                            true,
                        )],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(69)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
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
            route(
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
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NEW_TRADER_NEAR_AND_FT_STORAGE,
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                        is_fee_transaction: false,
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                            gas: Gas(NearGas::from_tgas(54)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                        is_fee_transaction: false,
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::TokenOnIntearDex(token_out),
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
                &TestPlachQuotes::default(),
                None,
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
                &TestPlachQuotes::none(),
                None,
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
                &TestPlachQuotes::empty_routes(),
                None,
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_registration_and_storage() {
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                        gas: Gas(NearGas::from_tgas(69)),
                        deposit: NearToken::from_yoctonear(100),
                    }))],
                    is_fee_transaction: false,
                }],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_operations() {
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
                &TestPlachQuotes::default(),
                None,
            )
            .await,
            Some(Route {
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
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
                        gas: Gas(NearGas::from_tgas(69)),
                        deposit: NearToken::from_yoctonear(100),
                    }))],
                    is_fee_transaction: false,
                }],
                deprecated_needs_unwrap_always_false: false,
                discount: None,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn referral_fees_are_taken_by_the_engine_and_their_receivers_registered() {
        let route = route(
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
            None,
        )
        .await
        .unwrap();
        // ref.near and the aggregator haven't registered NEAR, so the trader registers it for them
        let ExecutionInstruction::NearTransaction {
            receiver_id,
            actions,
            ..
        } = &route.execution_instructions[0];
        assert_eq!(receiver_id, INTEAR_DEX_CONTRACT_ID);
        let registered_for = actions
            .iter()
            .map(|action| match action {
                Action::FunctionCall(call) => {
                    assert_eq!(call.method_name, "register_assets");
                    serde_json::from_slice::<serde_json::Value>(&call.args).unwrap()
                }
                _ => panic!("Not a function call"),
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

        let Some(ExecutionInstruction::NearTransaction { actions, .. }) =
            route.execution_instructions.last()
        else {
            panic!("No swap transaction");
        };
        let [Action::FunctionCall(deposit_near)] = actions.as_slice() else {
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
                fees: r#"{"ref.near":0.05}"#.parse().unwrap(),
            },
            &TestNetworkView::default().with_test_prices(),
            &TestPlachQuotes::default(),
            None,
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
