use std::collections::HashSet;

use bigdecimal::num_bigint::BigUint;
use intear_dex_common::{AssetId, DexId, RouteFraction, SwapRequestAmount, TokenAmount};
use intear_dex_types::{
    DirectWithdrawAmount, Operation, OracleFeeSwap, QuoteImprovementFee, SwapAdvancedMode,
    SwapOperationAmount, SwapOption, WithdrawAmount,
};
use near_min_api::types::{
    near_account_id::AccountType, AccountId, Action, Balance, FunctionCallAction, NearGas,
    NearToken,
};
use near_sdk::json_types::Base64VecU8;
use pathfinder::dcl::DclWork;
use swap_router_types::{
    fee_discount::{FeeDiscounter, FEE_DISCOUNT_GAS},
    hook::{OracleRequest, QuoteFeeDiscount},
};
use tracing::warn;

use crate::{
    hooks::inteardex::INTEAR_DEX_CONTRACT_ID,
    providers::{rhea::RHEA_CONTRACT_ID, rhea_dcl::RHEA_DCL_CONTRACT_ID},
    shared_utils::WRAP_NEAR,
    types::{Amount, Gas, IntearAssetId, QuoteFees},
};

/// A function call of a route, with everything it needs known. Each call has a fixed receiver,
/// method and gas, `Input*` calls are about the NEP-141 that the route swaps from.
#[derive(Debug, Clone, PartialEq)]
pub enum CallBlueprint {
    /// The minimum storage of `account_id` on the NEP-141, of the signer without it
    Nep141StorageDeposit {
        token_id: AccountId,
        account_id: Option<AccountId>,
        amount: NearToken,
    },
    /// Moves the NEP-141 into the signer's balance on Rhea
    Nep141DepositOnRhea {
        token_id: AccountId,
        amount: Balance,
    },
    /// Moves the NEP-141 into the signer's balance on Intear DEX
    Nep141DepositOnIntearDex {
        token_id: AccountId,
        amount: Balance,
    },
    WrapNear {
        amount: NearToken,
    },
    /// Sends the NEAR to the signer, `trader_account_id`
    UnwrapNear {
        amount: NearToken,
        trader_account_id: Option<AccountId>,
    },
    /// Swaps the input on Rhea, which sends the output to the signer
    InputNep141TransferCallRheaSwap {
        token_id: AccountId,
        amount: Balance,
        actions: Vec<RheaSwapAction>,
        swap_gas: Gas,
        output: RheaOutput,
        referral_id: AccountId,
    },
    /// Swaps the input on Rhea DCL through `pool_ids`, in the order of the route, to `token_out`,
    /// which it sends to the signer
    InputNep141TransferCallRheaDclSwap {
        token_id: AccountId,
        amount: Balance,
        pool_ids: Vec<String>,
        token_out: AccountId,
        swap: RheaDclSwap,
        swap_gas: Gas,
        work: DclWork,
        output: RheaOutput,
        referral_id: AccountId,
    },
    /// Runs the operations on Intear DEX with the input deposited into the signer's balance
    InputNep141TransferCallIntearDex {
        token_id: AccountId,
        amount: Balance,
        operations: IntearDexOperations,
    },
    /// A referral fee out of the signer's wallet balance of the input. The memo names the swap's
    /// referrer, for DEX backend to attribute the fee to.
    InputNep141FeeTransfer {
        token_id: AccountId,
        receiver_id: AccountId,
        amount: Balance,
        swap_referrer_id: AccountId,
    },
    /// What a swap gave above its quote's estimate, or took below it, out of the signer's wallet
    /// balance. The memo is for DEX backend to tell it apart from referral fees.
    QuoteImprovementFeeTransfer {
        token_id: AccountId,
        receiver_id: AccountId,
        amount: Balance,
    },
    /// Storage of `account_id` on Rhea, of the signer without it
    RheaStorageDeposit {
        account_id: Option<AccountId>,
        amount: NearToken,
    },
    RheaRegisterTokens {
        token_ids: Vec<AccountId>,
    },
    /// Swaps within the signer's balances on Rhea
    RheaSwap {
        actions: Vec<RheaSwapAction>,
        swap_gas: Gas,
        referral_id: AccountId,
    },
    /// A referral fee out of the signer's balance of the input on Rhea. The memo names the swap's
    /// referrer, for DEX backend to attribute the fee to.
    InputRheaFeeMftTransfer {
        token_id: AccountId,
        receiver_id: AccountId,
        amount: Balance,
        swap_referrer_id: AccountId,
    },
    RheaWithdraw {
        token_id: AccountId,
        amount: Balance,
        output: RheaOutput,
    },
    /// The signer's storage on Rhea DCL
    RheaDclStorageDeposit {
        amount: NearToken,
    },
    /// The signer's storage on Intear DEX
    IntearDexStorageDeposit {
        amount: NearToken,
    },
    /// Registers balances of the assets for `for_account`, for the signer without it
    IntearDexRegisterAssets {
        asset_ids: Vec<IntearAssetId>,
        for_account: Option<AccountId>,
    },
    /// Into the signer's balance on Intear DEX, then runs the operations with it
    IntearDexDepositNear {
        amount: NearToken,
        operations: Option<IntearDexOperations>,
    },
    /// Runs the operations with the signer's balances on Intear DEX
    IntearDexExecuteOperations {
        operations: IntearDexOperations,
    },
    /// Sends the asset to the signer, `trader_account_id`
    IntearDexWithdraw {
        asset: IntearAssetId,
        amount: Balance,
        trader_account_id: Option<AccountId>,
    },
}

/// A hop of a swap on Rhea
#[derive(Debug, Clone, PartialEq)]
pub struct RheaSwapAction {
    pub pool_id: u64,
    pub token_in: AccountId,
    pub token_out: AccountId,
    /// None for a hop that swaps what the hop before it gave
    pub amount_in: Option<Balance>,
    pub amount_out: Balance,
    pub min_amount_out: Balance,
}

/// How Rhea sends a token out to the signer
#[derive(Debug, Clone, PartialEq)]
pub enum RheaOutput {
    Nep141,
    /// Unwrapped from wNEAR, to `trader_account_id`
    Near {
        trader_account_id: Option<AccountId>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RheaDclSwap {
    ExactIn { min_amount_out: Balance },
    ExactOut { amount_out: Balance },
}

/// A swap on Intear DEX's engine, and the withdrawals to the signer after it
#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexOperations {
    pub swap: IntearDexSwapOperation,
    pub withdrawals: Vec<IntearDexWithdrawal>,
    /// Also takes what the engine can't send to the trader
    pub referrer: AccountId,
    /// The engine takes them out of the swap's input
    pub referral_fees: QuoteFees,
    /// The signer, who withdrawn NEAR goes to
    pub trader_account_id: Option<AccountId>,
    /// On every swap of the operations, which the engine takes when it's signed for them
    pub fee_discount: Option<QuoteFeeDiscount>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapOperation {
    /// `AmountIn` swaps an exact input, `AmountOut` gets an exact output
    pub amount: Amount,
    /// The least output of an exact-in swap, or the most input of an exact-out one
    pub constraint: Balance,
    /// One after another, each swapping its share of `amount`. Only exact-in swaps have more than
    /// one.
    pub branches: Vec<IntearDexSwapBranch>,
    /// What the operations ask before the swap, for the options that read the response, for the
    /// oracle's fee in the asset in, which `amount` and `constraint` are after. Boxed, as it's
    /// most of the size of a call that has one.
    pub oracle_request: Option<Box<IntearDexOracleRequest>>,
    pub quote_improvement_fee: Option<IntearDexQuoteImprovementFee>,
}

/// Swaps of a share of a swap's amount
#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapBranch {
    pub fraction: RouteFraction,
    /// What the swap's estimated amount has of the branch
    pub estimated_amount: Balance,
    /// In order, each after the first swapping what the one before it gave. Only exact-in swaps
    /// have more than one.
    pub steps: Vec<IntearDexSwapStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexQuoteImprovementFee {
    pub quote_expected_amount: Balance,
    pub quote_improvement_receiver: AccountId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapStep {
    pub asset_in: IntearAssetId,
    pub asset_out: IntearAssetId,
    /// Best first. One is swapped on alone, the engine falls back through more of them when one
    /// fails or, in the last step, doesn't meet the constraint.
    pub options: Vec<IntearDexSwapOption>,
}

impl IntearDexSwapOperation {
    pub fn asset_in(&self) -> &IntearAssetId {
        &self.steps().next().expect("A swap has steps").asset_in
    }

    pub fn asset_out(&self) -> &IntearAssetId {
        &self.steps().last().expect("A swap has steps").asset_out
    }

    /// Of every branch
    pub fn steps(&self) -> impl Iterator<Item = &IntearDexSwapStep> {
        self.branches.iter().flat_map(|branch| &branch.steps)
    }
}

/// An oracle request that `dex_id` reads the response to
#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexOracleRequest {
    pub dex_id: intear_dex_common::DexId,
    pub request: OracleRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapOption {
    pub dex_id: intear_dex_common::DexId,
    /// The swap's message to the DEX
    pub message: Base64VecU8,
    /// Of the DEX's swap, as its quote has it
    pub gas: Gas,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexWithdrawal {
    pub asset: IntearAssetId,
    pub amount: IntearDexWithdrawAmount,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntearDexWithdrawAmount {
    Exact(Balance),
    /// The whole balance, which fails if it's less than `at_least`
    Full {
        at_least: Option<Balance>,
    },
}

/// What `ft_transfer_call` and `ft_resolve_transfer` burn with the fees of `ft_on_transfer`: the
/// median of what the router's swaps of two days on mainnet burnt. It needs attached what the token
/// keeps, `ft_on_transfer` gets the rest. Simulated on mainnet state for a token of each contract
/// code, tokens keep 9 to 47 (linear-protocol.near) Tgas.
const FT_TRANSFER_CALL_GAS: Gas = Gas::from_ggas(4_530, 50_000);
/// What a NEP-141 transfer burns, with 10 Tgas attached on top: the median of the 50 tokens with
/// the most volume in the week to 2026-11-24, simulated on mainnet state. 9 in 10 of them burnt
/// 1.91 to 2.69 Tgas, and stNEAR 9.73.
const FT_TRANSFER_GAS: Gas = Gas::from_ggas(2_180, 12_180);
/// What a NEP-141 storage deposit burns, with 10 Tgas attached on top: the median of the same
/// tokens. 9 in 10 of them burnt 1.65 to 2.62 Tgas.
const FT_STORAGE_DEPOSIT_GAS: Gas = Gas::from_ggas(1_910, 11_910);
/// What wrap.near's near_deposit adds to the transaction of a swap of native NEAR on Rhea or Rhea
/// DCL, which batches it with the swap, with 10 Tgas attached on top. On mainnet state, it added
/// the same for 3 to 300 NEAR.
const WRAP_NEAR_DEPOSIT_GAS: Gas = Gas::from_ggas(1_606, 11_606);
/// What a transfer of a trader's balance on Rhea burns, with 10 Tgas attached on top
const RHEA_MFT_TRANSFER_GAS: Gas = Gas::from_ggas(3_590, 13_590);
/// Attached on top of what the transaction of a swap burnt when it was simulated
const SWAP_MARGIN: NearGas = NearGas::from_tgas(100);
/// Attached on top of what the transaction of a deposit into or a withdrawal out of a DEX burnt
/// when it was simulated, which reserve gas for the callbacks of their transfers
const TRANSFER_MARGIN: NearGas = NearGas::from_tgas(50);
/// Attached on top of what the transaction of wrapping, unwrapping, storage or a NEP-141 transfer
/// burnt when it was simulated
const CALL_MARGIN: NearGas = NearGas::from_tgas(10);

/// Not burnt, attached so a call doesn't run out of gas when it burns more than estimated
const GAS_MARGIN: Gas = Gas::from_ggas(0, 3_000);
/// Intear DEX runs DEXes in an interpreter, simulated Plach and OTC swaps burnt up to 0.8 Tgas
/// more than estimated
const INTEAR_DEX_SWAP_MARGIN: NearGas = NearGas::from_tgas(4);
const INTEAR_DEX_SWAP_MARGIN_PERCENT: u64 = 15;
/// Simulated Qoq swaps of 1 to 500 NEAR burnt up to 0.2 Tgas more than estimated, its StableSwap
/// math takes more steps the further a pool is from its balance
const QOQ_SWAP_MARGIN: NearGas = NearGas::from_tgas(4);
const QOQ_SWAP_MARGIN_PERCENT: u64 = 15;
/// Intear DEX holds the operations after an oracle request until the oracle responds, then runs
/// them in a receipt of their own, and pays the oracle its fee in a call after them that it
/// attaches 10 Tgas to. On mainnet, holding them burnt 0.9 Tgas more than the call that makes the
/// request burns without it, and the call after them 2.77. The oracle pays for its response.
const ORACLE_REQUEST_GAS: Gas = Gas::from_ggas(3_670, 14_700);
/// What Intear DEX attaches to the last call after the operations of a deposit that waited for
/// an oracle, which keeps the deposit if refunding it failed. That's the call after the
/// operations, which ORACLE_REQUEST_GAS counts the burn of.
const HELD_DEPOSIT_GAS: Gas = Gas::from_ggas(0, 10_000);
/// What Intear DEX attaches on top of that to refund an oracle's fee that a swap of a deposit
/// bought, which only burns when the oracle doesn't respond
const SWAPPED_ORACLE_FEE_REFUND_GAS: Gas = Gas::from_ggas(0, 15_000);
/// Under what a transaction can attach, with room for the other calls in it
const MAX_CALL_GAS: NearGas = NearGas::from_tgas(900);

/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L43-L47
const ACTION_RECEIPT_EXECUTION_GAS: NearGas = NearGas::from_gas(108_059_500_000);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L88-L92
const TRANSFER_EXECUTION_GAS: NearGas = NearGas::from_gas(115_123_062_500);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/85.yaml#L16-L27
const CREATE_ACCOUNT_EXECUTION_GAS: NearGas = NearGas::from_gas(7_200_000_000_000);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L98-L102
const ADD_FULL_ACCESS_KEY_EXECUTION_GAS: NearGas = NearGas::from_gas(101_765_125_000);

impl CallBlueprint {
    pub fn receiver_id(&self) -> AccountId {
        match self {
            CallBlueprint::Nep141StorageDeposit { token_id, .. }
            | CallBlueprint::Nep141DepositOnRhea { token_id, .. }
            | CallBlueprint::Nep141DepositOnIntearDex { token_id, .. }
            | CallBlueprint::InputNep141TransferCallRheaSwap { token_id, .. }
            | CallBlueprint::InputNep141TransferCallRheaDclSwap { token_id, .. }
            | CallBlueprint::InputNep141TransferCallIntearDex { token_id, .. }
            | CallBlueprint::InputNep141FeeTransfer { token_id, .. }
            | CallBlueprint::QuoteImprovementFeeTransfer { token_id, .. } => token_id.clone(),
            CallBlueprint::WrapNear { .. } | CallBlueprint::UnwrapNear { .. } => {
                WRAP_NEAR.parse().unwrap()
            }
            CallBlueprint::RheaStorageDeposit { .. }
            | CallBlueprint::RheaRegisterTokens { .. }
            | CallBlueprint::RheaSwap { .. }
            | CallBlueprint::InputRheaFeeMftTransfer { .. }
            | CallBlueprint::RheaWithdraw { .. } => RHEA_CONTRACT_ID.parse().unwrap(),
            CallBlueprint::RheaDclStorageDeposit { .. } => RHEA_DCL_CONTRACT_ID.parse().unwrap(),
            CallBlueprint::IntearDexStorageDeposit { .. }
            | CallBlueprint::IntearDexRegisterAssets { .. }
            | CallBlueprint::IntearDexDepositNear { .. }
            | CallBlueprint::IntearDexExecuteOperations { .. }
            | CallBlueprint::IntearDexWithdraw { .. } => INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        }
    }

    /// The call with a hundredth of the least output of its swap, 99% slippage, so that its
    /// simulation doesn't fail when the pools moved since its quote, but does when the swap gives
    /// next to nothing. What an oracle's fee swap must get stays, as the oracle takes it.
    pub fn with_99_percent_slippage(mut self) -> Self {
        let slipped = |least_output: Balance| least_output / 100;
        match &mut self {
            CallBlueprint::InputNep141TransferCallRheaSwap { actions, .. }
            | CallBlueprint::RheaSwap { actions, .. } => {
                for action in actions {
                    action.min_amount_out = slipped(action.min_amount_out);
                }
            }
            CallBlueprint::InputNep141TransferCallRheaDclSwap {
                swap: RheaDclSwap::ExactIn { min_amount_out },
                ..
            } => *min_amount_out = slipped(*min_amount_out),
            CallBlueprint::InputNep141TransferCallIntearDex { operations, .. }
            | CallBlueprint::IntearDexExecuteOperations { operations }
            | CallBlueprint::IntearDexDepositNear {
                operations: Some(operations),
                ..
            } => {
                if let Amount::AmountIn(_) = operations.swap.amount {
                    operations.swap.constraint = slipped(operations.swap.constraint);
                }
                for withdrawal in &mut operations.withdrawals {
                    if let IntearDexWithdrawAmount::Full {
                        at_least: Some(at_least),
                    } = &mut withdrawal.amount
                    {
                        *at_least = slipped(*at_least);
                    }
                }
            }
            _ => {}
        }
        self
    }

    /// The call with 100 times the most input of its exact-out swap and of the swap that buys its
    /// oracle's fee, sending what they may take more of, so that its simulation doesn't fail when
    /// the pools moved since its quote. Wrapping NEAR wraps 100 times as much, for the swap after
    /// it. Amounts saturate, so the simulation fails on what the trader can't send.
    pub fn with_100x_max_input(mut self) -> Self {
        let hundredfold = |max_input: Balance| max_input.saturating_mul(100);
        // What the swaps of the operations may take more of
        let raise_max_inputs = |operations: &mut IntearDexOperations| {
            let swap = &mut operations.swap;
            let exact_out_max_input = match swap.amount {
                Amount::AmountOut(_) => Some(&mut swap.constraint),
                Amount::AmountIn(_) => None,
            };
            let oracle_fee_swap_max_input = swap
                .oracle_request
                .as_mut()
                .filter(|oracle_request| oracle_request.request.fee_swap.is_some())
                .map(|oracle_request| &mut oracle_request.request.fee.0);
            exact_out_max_input
                .into_iter()
                .chain(oracle_fee_swap_max_input)
                .map(|max_input| {
                    let raised = hundredfold(*max_input);
                    let raised_by = raised - *max_input;
                    *max_input = raised;
                    raised_by
                })
                .fold(0, Balance::saturating_add)
        };
        match &mut self {
            CallBlueprint::InputNep141TransferCallRheaDclSwap {
                amount,
                swap: RheaDclSwap::ExactOut { .. },
                ..
            } => *amount = hundredfold(*amount),
            CallBlueprint::InputNep141TransferCallIntearDex {
                amount, operations, ..
            } => *amount = amount.saturating_add(raise_max_inputs(operations)),
            CallBlueprint::IntearDexDepositNear {
                amount,
                operations: Some(operations),
            } => {
                *amount =
                    amount.saturating_add(NearToken::from_yoctonear(raise_max_inputs(operations)));
            }
            CallBlueprint::WrapNear { amount } => *amount = amount.saturating_mul(100),
            _ => {}
        }
        self
    }

    /// What the call burns and needs attached without a simulation, the same for every token, and
    /// for wrap.near's deposits of native NEAR. None for the calls that simulations measure.
    pub fn fixed_gas(&self) -> Option<Gas> {
        match self {
            CallBlueprint::WrapNear { .. } => Some(WRAP_NEAR_DEPOSIT_GAS),
            CallBlueprint::Nep141StorageDeposit { .. } => Some(FT_STORAGE_DEPOSIT_GAS),
            CallBlueprint::InputNep141FeeTransfer { .. }
            | CallBlueprint::QuoteImprovementFeeTransfer { .. } => Some(FT_TRANSFER_GAS),
            CallBlueprint::InputRheaFeeMftTransfer { .. } => Some(RHEA_MFT_TRANSFER_GAS),
            _ => None,
        }
    }

    /// Attached on top of what the call's transaction burnt when it was simulated
    pub fn margin(&self) -> NearGas {
        match self {
            CallBlueprint::InputNep141TransferCallRheaSwap { .. }
            | CallBlueprint::InputNep141TransferCallRheaDclSwap { .. }
            | CallBlueprint::InputNep141TransferCallIntearDex { .. }
            | CallBlueprint::RheaSwap { .. }
            | CallBlueprint::IntearDexDepositNear {
                operations: Some(_),
                ..
            }
            | CallBlueprint::IntearDexExecuteOperations { .. } => SWAP_MARGIN,
            CallBlueprint::Nep141DepositOnRhea { .. }
            | CallBlueprint::Nep141DepositOnIntearDex { .. }
            | CallBlueprint::RheaWithdraw { .. }
            | CallBlueprint::IntearDexWithdraw { .. } => TRANSFER_MARGIN,
            CallBlueprint::Nep141StorageDeposit { .. }
            | CallBlueprint::InputNep141FeeTransfer { .. }
            | CallBlueprint::QuoteImprovementFeeTransfer { .. }
            | CallBlueprint::InputRheaFeeMftTransfer { .. }
            | CallBlueprint::WrapNear { .. }
            | CallBlueprint::UnwrapNear { .. }
            | CallBlueprint::RheaStorageDeposit { .. }
            | CallBlueprint::RheaRegisterTokens { .. }
            | CallBlueprint::RheaDclStorageDeposit { .. }
            | CallBlueprint::IntearDexStorageDeposit { .. }
            | CallBlueprint::IntearDexRegisterAssets { .. }
            | CallBlueprint::IntearDexDepositNear {
                operations: None, ..
            } => CALL_MARGIN,
        }
    }

    /// Of calls that aren't simulated: the next steps after a swap, and the routes on Intear DEX
    /// without a swap, which the last look weighs routes on Intear DEX with. Source: sandbox on
    /// mainnet state. What's attached is capped at [`MAX_CALL_GAS`].
    pub fn planned_gas(&self) -> Gas {
        let gas = match self {
            CallBlueprint::Nep141StorageDeposit { .. }
            | CallBlueprint::WrapNear { .. }
            | CallBlueprint::InputNep141FeeTransfer { .. }
            | CallBlueprint::QuoteImprovementFeeTransfer { .. }
            | CallBlueprint::InputRheaFeeMftTransfer { .. } => {
                self.fixed_gas().expect("these calls are of fixed gas")
            }
            CallBlueprint::Nep141DepositOnRhea { .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(2_890, 2_500)
            }
            CallBlueprint::Nep141DepositOnIntearDex { .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(3_670, 3_300)
            }
            CallBlueprint::UnwrapNear {
                trader_account_id, ..
            } => Gas::from_ggas(1_900, 2_500) + near_receive_gas(trader_account_id.as_ref()),
            CallBlueprint::InputNep141TransferCallRheaSwap {
                swap_gas, output, ..
            } => {
                FT_TRANSFER_CALL_GAS
                    + Gas::from_ggas(4_700, 4_700)
                    + *swap_gas
                    + rhea_output_gas(output)
                    + GAS_MARGIN
            }
            CallBlueprint::InputNep141TransferCallRheaDclSwap {
                swap_gas, output, ..
            } => {
                // DCL reserves the same gas for sending either output
                let output_gas = match output {
                    RheaOutput::Nep141 => {
                        Gas::from_ggas(2_510, 31_800) + burn_only(FT_TRANSFER_GAS)
                    }
                    RheaOutput::Near { trader_account_id } => {
                        Gas::from_ggas(4_790, 31_800)
                            + burn_only(near_receive_gas(trader_account_id.as_ref()))
                    }
                };
                FT_TRANSFER_CALL_GAS + *swap_gas + output_gas
            }
            CallBlueprint::InputNep141TransferCallIntearDex { operations, .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(2_430, 2_600) + operations.gas_with_deposit()
            }
            CallBlueprint::RheaStorageDeposit { .. } => Gas::from_ggas(2_610, 3_000),
            CallBlueprint::RheaRegisterTokens { token_ids } => {
                Gas::from_ggas(2_500, 3_000) + Gas::from_ggas(500, 500) * token_ids.len() as u64
            }
            CallBlueprint::RheaSwap { swap_gas, .. } => {
                Gas::from_ggas(2_800, 2_800) + *swap_gas + GAS_MARGIN
            }
            CallBlueprint::RheaWithdraw { output, .. } => {
                Gas::from_ggas(3_700, 3_700) + rhea_output_gas(output)
            }
            CallBlueprint::RheaDclStorageDeposit { .. } => Gas::from_ggas(3_540, 4_000),
            CallBlueprint::IntearDexStorageDeposit { .. } => Gas::from_ggas(2_790, 3_500),
            CallBlueprint::IntearDexRegisterAssets { asset_ids, .. } => {
                Gas::from_ggas(2_950, 3_770) + Gas::from_ggas(340, 340) * asset_ids.len() as u64
            }
            CallBlueprint::IntearDexDepositNear {
                operations: None, ..
            } => Gas::from_ggas(3_600, 4_000),
            CallBlueprint::IntearDexDepositNear {
                operations: Some(operations),
                ..
            } => Gas::from_ggas(2_920, 3_600) + operations.gas_with_deposit(),
            CallBlueprint::IntearDexExecuteOperations { operations } => {
                Gas::from_ggas(3_480, 3_600) + operations.gas()
            }
            CallBlueprint::IntearDexWithdraw {
                asset,
                trader_account_id,
                ..
            } => {
                Gas::from_ggas(4_280, 4_400)
                    + intear_dex_sending_out_gas(asset, trader_account_id.as_ref())
            }
        };
        if gas.needs_attached > MAX_CALL_GAS {
            warn!(
                "A call to {} needs {}, attaching {MAX_CALL_GAS}",
                self.receiver_id(),
                gas.needs_attached
            );
            return Gas {
                needs_attached: MAX_CALL_GAS,
                ..gas
            };
        }
        gas
    }

    /// NEAR the call attaches to a storage deposit
    pub fn storage_deposit(&self) -> NearToken {
        match self {
            CallBlueprint::Nep141StorageDeposit { amount, .. }
            | CallBlueprint::RheaStorageDeposit { amount, .. }
            | CallBlueprint::RheaDclStorageDeposit { amount }
            | CallBlueprint::IntearDexStorageDeposit { amount } => *amount,
            CallBlueprint::Nep141DepositOnRhea { .. }
            | CallBlueprint::Nep141DepositOnIntearDex { .. }
            | CallBlueprint::WrapNear { .. }
            | CallBlueprint::UnwrapNear { .. }
            | CallBlueprint::InputNep141TransferCallRheaSwap { .. }
            | CallBlueprint::InputNep141TransferCallRheaDclSwap { .. }
            | CallBlueprint::InputNep141TransferCallIntearDex { .. }
            | CallBlueprint::InputNep141FeeTransfer { .. }
            | CallBlueprint::QuoteImprovementFeeTransfer { .. }
            | CallBlueprint::RheaRegisterTokens { .. }
            | CallBlueprint::RheaSwap { .. }
            | CallBlueprint::InputRheaFeeMftTransfer { .. }
            | CallBlueprint::RheaWithdraw { .. }
            | CallBlueprint::IntearDexRegisterAssets { .. }
            | CallBlueprint::IntearDexDepositNear { .. }
            | CallBlueprint::IntearDexExecuteOperations { .. }
            | CallBlueprint::IntearDexWithdraw { .. } => NearToken::from_yoctonear(0),
        }
    }

    /// What the call pays to referrers out of the signer's wallet balance of the input
    pub fn wallet_fee_amount(&self) -> Balance {
        match self {
            CallBlueprint::InputNep141FeeTransfer { amount, .. } => *amount,
            CallBlueprint::QuoteImprovementFeeTransfer { .. }
            | CallBlueprint::Nep141StorageDeposit { .. }
            | CallBlueprint::Nep141DepositOnRhea { .. }
            | CallBlueprint::Nep141DepositOnIntearDex { .. }
            | CallBlueprint::WrapNear { .. }
            | CallBlueprint::UnwrapNear { .. }
            | CallBlueprint::InputNep141TransferCallRheaSwap { .. }
            | CallBlueprint::InputNep141TransferCallRheaDclSwap { .. }
            | CallBlueprint::InputNep141TransferCallIntearDex { .. }
            | CallBlueprint::RheaStorageDeposit { .. }
            | CallBlueprint::RheaRegisterTokens { .. }
            | CallBlueprint::RheaSwap { .. }
            | CallBlueprint::InputRheaFeeMftTransfer { .. }
            | CallBlueprint::RheaWithdraw { .. }
            | CallBlueprint::RheaDclStorageDeposit { .. }
            | CallBlueprint::IntearDexStorageDeposit { .. }
            | CallBlueprint::IntearDexRegisterAssets { .. }
            | CallBlueprint::IntearDexDepositNear { .. }
            | CallBlueprint::IntearDexExecuteOperations { .. }
            | CallBlueprint::IntearDexWithdraw { .. } => 0,
        }
    }

    pub fn into_action(self, attached: NearGas) -> Action {
        let one_yocto = NearToken::from_yoctonear(1);
        let (method_name, args, deposit) = match self {
            CallBlueprint::Nep141StorageDeposit {
                account_id: None,
                amount,
                ..
            } => (
                "storage_deposit",
                serde_json::json!({ "registration_only": true }),
                amount,
            ),
            CallBlueprint::Nep141StorageDeposit {
                account_id: Some(account_id),
                amount,
                ..
            } => (
                "storage_deposit",
                serde_json::json!({
                    "registration_only": true,
                    "account_id": account_id,
                }),
                amount,
            ),
            CallBlueprint::Nep141DepositOnRhea { amount, .. } => (
                "ft_transfer_call",
                serde_json::json!({
                    "receiver_id": RHEA_CONTRACT_ID,
                    "amount": amount.to_string(),
                    "msg": "",
                }),
                one_yocto,
            ),
            CallBlueprint::Nep141DepositOnIntearDex { amount, .. } => (
                "ft_transfer_call",
                serde_json::json!({
                    "receiver_id": INTEAR_DEX_CONTRACT_ID,
                    "amount": amount.to_string(),
                    "msg": "",
                }),
                one_yocto,
            ),
            CallBlueprint::WrapNear { amount } => ("near_deposit", serde_json::json!({}), amount),
            CallBlueprint::UnwrapNear { amount, .. } => (
                "near_withdraw",
                serde_json::json!({ "amount": amount }),
                one_yocto,
            ),
            CallBlueprint::InputNep141TransferCallRheaSwap {
                amount,
                actions,
                output,
                referral_id,
                ..
            } => (
                "ft_transfer_call",
                serde_json::json!({
                    "receiver_id": RHEA_CONTRACT_ID,
                    "amount": amount.to_string(),
                    "msg": serde_json::to_string(&serde_json::json!({
                        "force": 0,
                        "actions": rhea_actions_json(&actions),
                        "skip_degen_price_sync": true,
                        "skip_unwrap_near": output == RheaOutput::Nep141,
                        "referral_id": referral_id,
                    }))
                    .unwrap(),
                }),
                one_yocto,
            ),
            CallBlueprint::InputNep141TransferCallRheaDclSwap {
                amount,
                mut pool_ids,
                token_out,
                swap,
                output,
                referral_id,
                ..
            } => {
                let skip_unwrap_near = output == RheaOutput::Nep141;
                let message = match swap {
                    RheaDclSwap::ExactIn { min_amount_out } => serde_json::json!({
                        "Swap": {
                            "pool_ids": pool_ids,
                            "output_token": token_out,
                            "min_output_amount": min_amount_out.to_string(),
                            "skip_unwrap_near": skip_unwrap_near,
                            "referral_id": referral_id,
                        }
                    }),
                    RheaDclSwap::ExactOut { amount_out } => {
                        // SwapByOutput walks the pools from the output token
                        pool_ids.reverse();
                        serde_json::json!({
                            "SwapByOutput": {
                                "pool_ids": pool_ids,
                                "output_token": token_out,
                                "output_amount": amount_out.to_string(),
                                "skip_unwrap_near": skip_unwrap_near,
                                "referral_id": referral_id,
                            }
                        })
                    }
                };
                (
                    "ft_transfer_call",
                    serde_json::json!({
                        "receiver_id": RHEA_DCL_CONTRACT_ID,
                        "amount": amount.to_string(),
                        "msg": serde_json::to_string(&message).unwrap(),
                    }),
                    one_yocto,
                )
            }
            CallBlueprint::InputNep141TransferCallIntearDex {
                amount, operations, ..
            } => (
                "ft_transfer_call",
                serde_json::json!({
                    "receiver_id": INTEAR_DEX_CONTRACT_ID,
                    "amount": amount.to_string(),
                    "msg": serde_json::to_string(&operations.engine_args()).unwrap(),
                }),
                one_yocto,
            ),
            CallBlueprint::InputNep141FeeTransfer {
                receiver_id,
                amount,
                swap_referrer_id,
                ..
            } => (
                "ft_transfer",
                serde_json::json!({
                    "receiver_id": receiver_id,
                    "amount": amount.to_string(),
                    "memo": format!("Trading fee ({swap_referrer_id})"),
                }),
                one_yocto,
            ),
            CallBlueprint::QuoteImprovementFeeTransfer {
                receiver_id,
                amount,
                ..
            } => (
                "ft_transfer",
                serde_json::json!({
                    "receiver_id": receiver_id,
                    "amount": amount.to_string(),
                    "memo": "Quote improvement fee",
                }),
                one_yocto,
            ),
            CallBlueprint::RheaStorageDeposit {
                account_id: None,
                amount,
            } => (
                "storage_deposit",
                serde_json::json!({ "registration_only": false }),
                amount,
            ),
            CallBlueprint::RheaStorageDeposit {
                account_id: Some(account_id),
                amount,
            } => (
                "storage_deposit",
                serde_json::json!({
                    "registration_only": false,
                    "account_id": account_id,
                }),
                amount,
            ),
            CallBlueprint::RheaRegisterTokens { token_ids } => (
                "register_tokens",
                serde_json::json!({ "token_ids": token_ids }),
                one_yocto,
            ),
            CallBlueprint::RheaSwap {
                actions,
                referral_id,
                ..
            } => (
                "swap",
                serde_json::json!({
                    "actions": rhea_actions_json(&actions),
                    "referral_id": referral_id,
                    "skip_degen_price_sync": true,
                }),
                one_yocto,
            ),
            CallBlueprint::InputRheaFeeMftTransfer {
                token_id,
                receiver_id,
                amount,
                swap_referrer_id,
            } => (
                "mft_transfer",
                serde_json::json!({
                    "token_id": token_id,
                    "receiver_id": receiver_id,
                    "amount": amount.to_string(),
                    "memo": format!("Trading fee ({swap_referrer_id})"),
                }),
                one_yocto,
            ),
            CallBlueprint::RheaWithdraw {
                token_id,
                amount,
                output,
            } => (
                "withdraw",
                serde_json::json!({
                    "token_id": token_id,
                    "amount": amount.to_string(),
                    "skip_unwrap_near": output == RheaOutput::Nep141,
                }),
                one_yocto,
            ),
            CallBlueprint::RheaDclStorageDeposit { amount } => (
                "storage_deposit",
                serde_json::json!({ "registration_only": true }),
                amount,
            ),
            CallBlueprint::IntearDexStorageDeposit { amount } => {
                ("storage_deposit", serde_json::json!({}), amount)
            }
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids,
                for_account: None,
            } => (
                "register_assets",
                serde_json::json!({ "asset_ids": asset_ids.iter().map(AssetId::from).collect::<Vec<_>>() }),
                one_yocto,
            ),
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids,
                for_account: Some(account_id),
            } => (
                "register_assets",
                serde_json::json!({
                    "asset_ids": asset_ids.iter().map(AssetId::from).collect::<Vec<_>>(),
                    "for": { "Account": account_id },
                }),
                one_yocto,
            ),
            CallBlueprint::IntearDexDepositNear {
                amount,
                operations: None,
            } => ("deposit_near", serde_json::json!({}), amount),
            CallBlueprint::IntearDexDepositNear {
                amount,
                operations: Some(operations),
            } => (
                "deposit_near",
                serde_json::json!({ "operations": operations.engine_args() }),
                amount,
            ),
            CallBlueprint::IntearDexExecuteOperations { operations } => {
                ("execute_operations", operations.engine_args(), one_yocto)
            }
            CallBlueprint::IntearDexWithdraw { asset, amount, .. } => (
                "withdraw",
                serde_json::json!({
                    "asset_id": AssetId::from(&asset),
                    "amount": DirectWithdrawAmount::Exact(TokenAmount::new(amount)),
                }),
                one_yocto,
            ),
        };
        Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: method_name.to_string(),
            args: serde_json::to_vec(&args).unwrap(),
            gas: near_min_api::types::Gas(attached),
            deposit,
        }))
    }
}

impl IntearDexOperations {
    fn gas(&self) -> Gas {
        let swap_gas = Gas {
            estimated_burn: swaps_burn(self.swap.steps().map(|step| {
                let option = &step.options[0];
                (&option.dex_id, option.gas.estimated_burn)
            })),
            needs_attached: self
                .swap
                .steps()
                .map(IntearDexSwapStep::gas)
                .sum::<Gas>()
                .needs_attached,
        };
        let withdrawals_gas = self
            .withdrawals
            .iter()
            .map(|withdrawal| {
                Gas::from_ggas(500, 500)
                    + intear_dex_sending_out_gas(&withdrawal.asset, self.trader_account_id.as_ref())
            })
            .sum::<Gas>();
        let oracle_request_gas = match &self.swap.oracle_request {
            Some(oracle_request) => match &oracle_request.request.fee_swap {
                Some(fee_swap) => ORACLE_REQUEST_GAS + fee_swap.gas,
                None => ORACLE_REQUEST_GAS,
            },
            None => Gas::from_ggas(0, 0),
        };
        let fee_receiver_count =
            self.referral_fees.0.len() + usize::from(self.swap.quote_improvement_fee.is_some());
        let fee_discount_gas = match self.fee_discount {
            Some(_) => FEE_DISCOUNT_GAS,
            None => Gas::from_ggas(0, 0),
        };
        Gas::from_ggas(600, 600) * fee_receiver_count as u64
            + fee_discount_gas
            + oracle_request_gas
            + swap_gas
            + withdrawals_gas
            + GAS_MARGIN
    }

    /// Of the operations of a deposit, which Intear DEX holds while they wait for an oracle
    fn gas_with_deposit(&self) -> Gas {
        match &self.swap.oracle_request {
            Some(oracle_request) => match oracle_request.request.fee_swap {
                Some(_) => self.gas() + HELD_DEPOSIT_GAS + SWAPPED_ORACLE_FEE_REFUND_GAS,
                None => self.gas() + HELD_DEPOSIT_GAS,
            },
            None => self.gas(),
        }
    }

    fn engine_args(&self) -> serde_json::Value {
        let swap = &self.swap;
        let branch_amounts = match swap.amount {
            Amount::AmountIn(amount) => RouteFraction::split(
                TokenAmount::new(amount),
                swap.branches.iter().map(|branch| branch.fraction),
            )
            .into_iter()
            .map(SwapRequestAmount::ExactIn)
            .collect::<Vec<_>>(),
            Amount::AmountOut(amount) => {
                assert_eq!(swap.branches.len(), 1, "Only exact-in swaps have branches");
                vec![SwapRequestAmount::ExactOut(TokenAmount::new(amount))]
            }
        };
        // Each branch gets the share of the constraint and of the quote improvement fee's
        // expected amount that it has of the quote's estimate, which add up to them
        let estimated_amount = swap
            .branches
            .iter()
            .map(|branch| branch.estimated_amount)
            .sum::<Balance>();
        let shares_of = |amount: Balance| {
            let mut shares = swap
                .branches
                .iter()
                .map(|branch| {
                    Balance::try_from(
                        BigUint::from(amount) * branch.estimated_amount
                            / BigUint::from(estimated_amount),
                    )
                    .expect("A branch's share is at most the whole")
                })
                .collect::<Vec<_>>();
            shares[0] += amount - shares.iter().sum::<Balance>();
            shares
        };
        let constraints = shares_of(swap.constraint);
        let quote_improvement_fees = match &swap.quote_improvement_fee {
            Some(fee) => shares_of(fee.quote_expected_amount)
                .into_iter()
                .map(|quote_expected_amount| {
                    Some(QuoteImprovementFee {
                        quote_expected_amount: TokenAmount::new(quote_expected_amount),
                        quote_improvement_receiver: fee.quote_improvement_receiver.clone(),
                    })
                })
                .collect(),
            None => vec![None; swap.branches.len()],
        };
        let swap_operations = swap
            .branches
            .iter()
            .zip(branch_amounts)
            .zip(constraints)
            .zip(quote_improvement_fees)
            .flat_map(
                |(((branch, branch_amount), constraint), quote_improvement_fee)| {
                    let last_step = branch.steps.len() - 1;
                    branch.steps.iter().enumerate().map(move |(index, step)| {
                        swap_operation(
                            step,
                            match index {
                                0 => SwapOperationAmount::Amount(branch_amount),
                                _ => SwapOperationAmount::OutputOfLastIn,
                            },
                            // Of the output of the last step, which the quote estimates
                            (index == last_step).then(|| TokenAmount::new(constraint)),
                            quote_improvement_fee.clone().filter(|_| index == last_step),
                        )
                    })
                },
            )
            .collect::<Vec<_>>();
        let withdrawals = self
            .withdrawals
            .iter()
            .map(|withdrawal| Operation::Withdraw {
                asset_id: AssetId::from(&withdrawal.asset),
                amount: match withdrawal.amount {
                    IntearDexWithdrawAmount::Exact(amount) => {
                        WithdrawAmount::Exact(TokenAmount::new(amount))
                    }
                    IntearDexWithdrawAmount::Full { at_least } => WithdrawAmount::Full {
                        at_least: at_least.map(TokenAmount::new),
                    },
                },
                to: None,
                rescue_address: Some(self.referrer.clone()),
            });
        let oracle_request = swap.oracle_request.iter().map(|oracle_request| {
            let OracleRequest {
                oracle_key,
                request,
                fee,
                fee_swap,
            } = &oracle_request.request;
            let asset_in = AssetId::from(swap.asset_in());
            let (fee, swap) = match fee_swap {
                Some(fee_swap) => {
                    let (fee_asset, oracle_fee) = &fee_swap.fee;
                    (
                        (fee_asset.clone(), TokenAmount::new(oracle_fee.0)),
                        Some(OracleFeeSwap {
                            dex_id: fee_swap.dex_id.clone(),
                            message: fee_swap.message.clone(),
                            asset_in,
                            constraint: Some(TokenAmount::new(fee.0)),
                        }),
                    )
                }
                None => ((asset_in, TokenAmount::new(fee.0)), None),
            };
            Operation::OracleRequest {
                oracle_key: oracle_key.clone(),
                dex_id: oracle_request.dex_id.clone(),
                request: request.clone(),
                fee,
                swap,
                rescue_address: Some(self.referrer.clone()),
            }
        });
        let operations = oracle_request
            .chain(swap_operations)
            .chain(withdrawals)
            .collect::<Vec<_>>();
        let mut args = serde_json::json!({
            "operations": operations,
            "referrer": self.referrer,
            "referral_fees": intear_dex_types::ReferralFees::from(&self.referral_fees),
        });
        if let Some(fee_discount) = self.fee_discount {
            args["fee_discount"] = serde_json::json!(FeeDiscounter::signing().sign_operations(
                &operations,
                self.trader_account_id.clone(),
                fee_discount,
            ));
        }
        args
    }
}

/// What swaps one after another in a call burn, each on its DEX: a swap on a DEX that the call
/// already swapped on burns less, as Intear DEX loads the swap code of each DEX once per call
pub fn swaps_burn<'a>(swaps: impl IntoIterator<Item = (&'a DexId, NearGas)>) -> NearGas {
    let qoq_dex_id: DexId = pathfinder::qoq::QOQ_DEX_ID.parse().unwrap();
    let plach_dex_id: DexId = pathfinder::plach::PLACH_DEX_ID.parse().unwrap();
    let mut swapped_on = HashSet::new();
    swaps
        .into_iter()
        .map(|(dex_id, burn)| match swapped_on.insert(dex_id) {
            true => burn,
            false if *dex_id == qoq_dex_id => {
                burn.saturating_sub(pathfinder::qoq::REPEATED_SWAP_SAVING)
            }
            false if *dex_id == plach_dex_id => {
                burn.saturating_sub(pathfinder::plach::REPEATED_SWAP_SAVING)
            }
            false => burn,
        })
        .fold(NearGas::from_gas(0), NearGas::saturating_add)
}

fn swap_operation(
    step: &IntearDexSwapStep,
    amount: SwapOperationAmount,
    constraint: Option<TokenAmount>,
    quote_improvement_fee: Option<QuoteImprovementFee>,
) -> Operation {
    let asset_in = AssetId::from(&step.asset_in);
    let asset_out = AssetId::from(&step.asset_out);
    match step.options.as_slice() {
        [option] => Operation::SwapSimple {
            dex_id: option.dex_id.clone(),
            message: option.message.clone(),
            asset_in,
            asset_out,
            amount,
            constraint,
            quote_improvement_fee,
        },
        options => Operation::SwapAdvanced {
            asset_in,
            asset_out,
            amount,
            constraint,
            quote_improvement_fee,
            options: options
                .iter()
                .map(|option| SwapOption {
                    dex_id: option.dex_id.clone(),
                    message: option.message.clone(),
                })
                .collect(),
            mode: SwapAdvancedMode::FallbackQueue,
        },
    }
}

impl IntearDexSwapStep {
    fn gas(&self) -> Gas {
        let qoq_dex_id: DexId = pathfinder::qoq::QOQ_DEX_ID.parse().unwrap();
        let options_gas = |of_qoq: bool| {
            self.options
                .iter()
                .filter(|option| (option.dex_id == qoq_dex_id) == of_qoq)
                .map(|option| option.gas)
                .reduce(|total, gas| total + gas)
        };
        // The first option usually fills the swap, the engine tries the others until one does
        let all_options_gas = [
            options_gas(true).map(|gas| gas.with_margin(QOQ_SWAP_MARGIN, QOQ_SWAP_MARGIN_PERCENT)),
            options_gas(false)
                .map(|gas| gas.with_margin(INTEAR_DEX_SWAP_MARGIN, INTEAR_DEX_SWAP_MARGIN_PERCENT)),
        ]
        .into_iter()
        .flatten()
        .sum::<Gas>();
        Gas {
            estimated_burn: self.options[0].gas.estimated_burn,
            needs_attached: all_options_gas.needs_attached,
        }
    }
}

/// Rhea takes `pool_id` as a number, and no `amount_in` for a hop that swaps what the hop before it
/// gave
fn rhea_actions_json(actions: &[RheaSwapAction]) -> Vec<serde_json::Value> {
    actions
        .iter()
        .map(|action| {
            let mut json = serde_json::json!({
                "pool_id": action.pool_id,
                "token_in": action.token_in,
                "token_out": action.token_out,
            });
            if let Some(amount_in) = action.amount_in {
                json["amount_in"] = amount_in.to_string().into();
            }
            json["amount_out"] = action.amount_out.to_string().into();
            json["min_amount_out"] = action.min_amount_out.to_string().into();
            json
        })
        .collect()
}

/// Of a call that a contract attaches its own gas to, so only what it burns counts
fn burn_only(gas: Gas) -> Gas {
    Gas {
        estimated_burn: gas.estimated_burn,
        needs_attached: NearGas::from_gas(0),
    }
}

/// Of Rhea sending a token out of the signer's balance
fn rhea_output_gas(output: &RheaOutput) -> Gas {
    match output {
        RheaOutput::Nep141 => Gas::from_ggas(2_170, 26_800) + burn_only(FT_TRANSFER_GAS),
        // Rhea attaches no gas to the transfer of NEAR
        RheaOutput::Near { trader_account_id } => {
            Gas::from_ggas(4_460, 31_800) + burn_only(near_receive_gas(trader_account_id.as_ref()))
        }
    }
}

/// Of Intear DEX sending an asset out of the signer's balance
fn intear_dex_sending_out_gas(asset: &IntearAssetId, trader_account_id: Option<&AccountId>) -> Gas {
    match asset {
        // after_withdraw burnt 2.34 Tgas on mainnet
        IntearAssetId::Near => Gas::from_ggas(2_340, 5_900) + near_receive_gas(trader_account_id),
        // The DEX attaches 10 Tgas to ft_transfer
        IntearAssetId::Nep141(_) => Gas::from_ggas(2_340, 16_800) + burn_only(FT_TRANSFER_GAS),
    }
}

/// Gas that sending NEAR to the trader burns, creating their account if it's implicit
pub fn near_receive_gas(trader_account_id: Option<&AccountId>) -> Gas {
    let account_creation_gas =
        match trader_account_id.map(|account_id| account_id.get_account_type()) {
            Some(AccountType::NearImplicitAccount) => {
                CREATE_ACCOUNT_EXECUTION_GAS.as_gas() + ADD_FULL_ACCESS_KEY_EXECUTION_GAS.as_gas()
            }
            Some(
                AccountType::EthImplicitAccount
                | AccountType::NearDeterministicAccount
                | AccountType::UniversalAccount,
            ) => CREATE_ACCOUNT_EXECUTION_GAS.as_gas(),
            Some(AccountType::NamedAccount) | None => 0,
        };
    let gas = NearGas::from_gas(
        ACTION_RECEIPT_EXECUTION_GAS.as_gas()
            + TRANSFER_EXECUTION_GAS.as_gas()
            + account_creation_gas,
    );
    Gas {
        estimated_burn: gas,
        needs_attached: gas,
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;

    use super::*;

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    /// The method, JSON args and deposit of the call
    fn encoded(call: CallBlueprint) -> (String, serde_json::Value, NearToken) {
        let attached = call.planned_gas().needs_attached;
        let Action::FunctionCall(function_call) = call.into_action(attached) else {
            panic!("Calls are function calls");
        };
        (
            function_call.method_name,
            serde_json::from_slice(&function_call.args).unwrap(),
            function_call.deposit,
        )
    }

    fn one_yocto() -> NearToken {
        NearToken::from_yoctonear(1)
    }

    fn intear_operations(withdrawals: Vec<IntearDexWithdrawal>) -> IntearDexOperations {
        IntearDexOperations {
            swap: IntearDexSwapOperation {
                amount: Amount::AmountIn(100),
                constraint: 79,
                branches: vec![IntearDexSwapBranch {
                    fraction: intear_dex_common::FULL_ROUTE_FRACTION,
                    estimated_amount: 80,
                    steps: vec![IntearDexSwapStep {
                        asset_in: IntearAssetId::Near,
                        asset_out: IntearAssetId::Nep141(account("ft")),
                        options: vec![IntearDexSwapOption {
                            dex_id: "slimedragon.near/xyk".parse().unwrap(),
                            message: Base64VecU8(vec![1, 0, 0, 0]),
                            gas: Gas::from_ggas(34_000, 34_000),
                        }],
                    }],
                }],
                oracle_request: None,
                quote_improvement_fee: None,
            },
            withdrawals,
            referrer: account("ref.near"),
            referral_fees: QuoteFees::default(),
            trader_account_id: Some(trader()),
            fee_discount: None,
        }
    }

    fn withdraw_ft() -> IntearDexWithdrawal {
        IntearDexWithdrawal {
            asset: IntearAssetId::Nep141(account("ft")),
            amount: IntearDexWithdrawAmount::Full { at_least: Some(79) },
        }
    }

    fn withdraw_near() -> IntearDexWithdrawal {
        IntearDexWithdrawal {
            asset: IntearAssetId::Near,
            amount: IntearDexWithdrawAmount::Full { at_least: None },
        }
    }

    fn rhea_actions() -> Vec<RheaSwapAction> {
        vec![
            RheaSwapAction {
                pool_id: 1,
                token_in: account("wrap.near"),
                token_out: account("usdc"),
                amount_in: Some(100),
                amount_out: 0,
                min_amount_out: 0,
            },
            RheaSwapAction {
                pool_id: 2,
                token_in: account("usdc"),
                token_out: account("ft"),
                amount_in: None,
                amount_out: 0,
                min_amount_out: 79,
            },
        ]
    }

    fn gas(burn: u64, attached: u64) -> Gas {
        Gas {
            estimated_burn: NearGas::from_gas(burn),
            needs_attached: NearGas::from_gas(attached),
        }
    }

    #[test]
    fn near_receive_gas_creates_implicit_accounts() {
        // Named and NEAR implicit accounts as measured on mainnet
        let burn = |trader_account_id: &str| {
            near_receive_gas(Some(&account(trader_account_id)))
                .estimated_burn
                .as_gas()
        };
        assert_eq!(burn("trader.near"), 223_182_562_500);
        assert_eq!(
            burn("0000000000000000000000000000000000000000000000000000000000000000"),
            7_524_947_687_500
        );
        // An ETH implicit account is charged for creating the account, but not for its key
        assert_eq!(
            burn("0x0000000000000000000000000000000000000000"),
            7_423_182_562_500
        );
    }

    #[test]
    fn swap_calls_have_the_gas_of_their_swap_and_output() {
        let rhea_swap = |output| CallBlueprint::InputNep141TransferCallRheaSwap {
            token_id: account("wrap.near"),
            amount: 100,
            actions: rhea_actions(),
            swap_gas: Gas::from_ggas(5_500, 5_500),
            output,
            referral_id: account("ref.near"),
        };
        assert_eq!(
            rhea_swap(RheaOutput::Nep141).planned_gas(),
            Gas::from_ggas(19_080, 90_000)
        );
        assert_eq!(
            rhea_swap(RheaOutput::Near {
                trader_account_id: Some(trader())
            })
            .planned_gas(),
            gas(19_413_182_562_500, 95_000_000_000_000)
        );
        assert_eq!(
            CallBlueprint::RheaSwap {
                actions: rhea_actions(),
                swap_gas: Gas::from_ggas(5_500, 5_500),
                referral_id: account("ref.near"),
            }
            .planned_gas(),
            Gas::from_ggas(8_300, 11_300)
        );

        let rhea_dcl_swap = |output| CallBlueprint::InputNep141TransferCallRheaDclSwap {
            token_id: account("wrap.near"),
            amount: 100,
            pool_ids: vec!["wrap.near|ft|100".to_string()],
            token_out: account("ft"),
            swap: RheaDclSwap::ExactIn { min_amount_out: 79 },
            swap_gas: Gas::from_ggas(20_000, 20_000),
            work: DclWork::default(),
            output,
            referral_id: account("ref.near"),
        };
        assert_eq!(
            rhea_dcl_swap(RheaOutput::Nep141).planned_gas(),
            Gas::from_ggas(29_220, 101_800)
        );
        assert_eq!(
            rhea_dcl_swap(RheaOutput::Near {
                trader_account_id: Some(trader())
            })
            .planned_gas(),
            gas(29_543_182_562_500, 101_800_000_000_000)
        );

        assert_eq!(
            CallBlueprint::IntearDexDepositNear {
                amount: NearToken::from_yoctonear(100),
                operations: Some(intear_operations(vec![withdraw_ft()])),
            }
            .planned_gas(),
            Gas::from_ggas(41_940, 67_000)
        );
        assert_eq!(
            CallBlueprint::IntearDexDepositNear {
                amount: NearToken::from_yoctonear(100),
                operations: Some(intear_operations(vec![withdraw_ft(), withdraw_near()])),
            }
            .planned_gas(),
            gas(45_003_182_562_500, 73_623_182_562_500)
        );
        assert_eq!(
            CallBlueprint::IntearDexExecuteOperations {
                operations: intear_operations(vec![]),
            }
            .planned_gas(),
            Gas::from_ggas(37_480, 49_700)
        );
        assert_eq!(
            CallBlueprint::InputNep141TransferCallIntearDex {
                token_id: account("ft"),
                amount: 100,
                operations: intear_operations(vec![withdraw_near()]),
            }
            .planned_gas(),
            gas(44_023_182_562_500, 105_323_182_562_500)
        );
    }

    #[test]
    fn qoq_swap_options_get_their_own_margin() {
        let plach_only = intear_operations(vec![]);
        let mut with_qoq = intear_operations(vec![]);
        with_qoq.swap.branches[0].steps[0]
            .options
            .push(IntearDexSwapOption {
                dex_id: pathfinder::qoq::QOQ_DEX_ID.parse().unwrap(),
                message: Base64VecU8(vec![2, 0, 0, 0]),
                gas: Gas::from_ggas(24_800, 24_800),
            });
        // 24.8 Tgas, 4 Tgas and 15% of 24.8 Tgas
        assert_eq!(
            with_qoq.gas().needs_attached.as_gas() - plach_only.gas().needs_attached.as_gas(),
            NearGas::from_ggas(32_520).as_gas()
        );
    }

    #[test]
    fn a_swap_on_a_dex_that_the_call_already_swapped_on_burns_less() {
        let qoq: DexId = pathfinder::qoq::QOQ_DEX_ID.parse().unwrap();
        let plach: DexId = pathfinder::plach::PLACH_DEX_ID.parse().unwrap();
        let other: DexId = "someone.near/dex".parse().unwrap();
        let burn = swaps_burn([
            (&qoq, NearGas::from_ggas(12_000)),
            (&plach, NearGas::from_ggas(6_400)),
            (&plach, NearGas::from_ggas(6_400)),
            (&qoq, NearGas::from_ggas(8_200)),
            (&other, NearGas::from_ggas(5_000)),
            (&other, NearGas::from_ggas(5_000)),
        ]);
        assert_eq!(
            burn,
            NearGas::from_ggas(12_000 + 6_400 + (6_400 - 4_100) + (8_200 - 5_430) + 5_000 + 5_000)
        );
    }

    #[test]
    fn branches_swap_their_shares_of_the_amount_and_the_constraint() {
        let step = |asset_in, asset_out, dex| IntearDexSwapStep {
            asset_in,
            asset_out,
            options: vec![IntearDexSwapOption {
                dex_id: format!("slimedragon.near/{dex}").parse().unwrap(),
                message: Base64VecU8(vec![1, 0, 0, 0]),
                gas: Gas::from_ggas(10_000, 10_000),
            }],
        };
        let ft = || IntearAssetId::Nep141(account("ft"));
        let usdc = || IntearAssetId::Nep141(account("usdc"));
        let mut operations = intear_operations(vec![withdraw_ft()]);
        operations.swap.amount = Amount::AmountIn(1_001);
        operations.swap.quote_improvement_fee = Some(IntearDexQuoteImprovementFee {
            quote_expected_amount: 81,
            quote_improvement_receiver: account("app.near"),
        });
        operations.swap.branches = vec![
            IntearDexSwapBranch {
                fraction: RouteFraction::new(700_000),
                estimated_amount: 61,
                steps: vec![step(IntearAssetId::Near, ft(), "xyk")],
            },
            IntearDexSwapBranch {
                fraction: RouteFraction::new(300_000),
                estimated_amount: 20,
                steps: vec![
                    step(IntearAssetId::Near, usdc(), "xyk"),
                    step(usdc(), ft(), "amp"),
                ],
            },
        ];
        let args = operations.engine_args();
        let swaps = args["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|operation| operation.get("SwapSimple"))
            .map(|swap| {
                (
                    swap["dex_id"].as_str().unwrap(),
                    swap["amount"].clone(),
                    swap["constraint"].clone(),
                    swap["quote_improvement_fee"]["quote_expected_amount"].clone(),
                )
            })
            .collect::<Vec<_>>();
        // The rounding remainders go to the first branch, so the shares add up to the whole
        assert_eq!(
            swaps,
            [
                (
                    "slimedragon.near/xyk",
                    serde_json::json!({ "Amount": { "ExactIn": "701" } }),
                    serde_json::json!("60"),
                    serde_json::json!("61"),
                ),
                (
                    "slimedragon.near/xyk",
                    serde_json::json!({ "Amount": { "ExactIn": "300" } }),
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                ),
                (
                    "slimedragon.near/amp",
                    serde_json::json!("OutputOfLastIn"),
                    serde_json::json!("19"),
                    serde_json::json!("20"),
                ),
            ]
        );
        assert_eq!(
            operations.gas().needs_attached.as_gas()
                - intear_operations(vec![withdraw_ft()])
                    .gas()
                    .needs_attached
                    .as_gas()
                - NearGas::from_ggas(600).as_gas(),
            // Three swaps of 10 Tgas and their margins of 4 Tgas and 15%, instead of one of 34
            NearGas::from_ggas(3 * 15_500 - 43_100).as_gas()
        );
    }

    #[test]
    fn registrations_have_gas_for_each_token() {
        assert_eq!(
            CallBlueprint::RheaRegisterTokens {
                token_ids: vec![account("ft")],
            }
            .planned_gas(),
            Gas::from_ggas(3_000, 3_500)
        );
        assert_eq!(
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids: vec![IntearAssetId::Nep141(account("ft"))],
                for_account: None,
            }
            .planned_gas(),
            Gas::from_ggas(3_290, 4_110)
        );
        assert_eq!(
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids: vec![IntearAssetId::Near, IntearAssetId::Nep141(account("ft"))],
                for_account: None,
            }
            .planned_gas(),
            Gas::from_ggas(3_630, 4_450)
        );
    }

    #[test]
    fn wrapping_calls() {
        let amount = NearToken::from_near(2);
        let wrap = CallBlueprint::WrapNear { amount };
        assert_eq!(wrap.receiver_id(), account("wrap.near"));
        assert_eq!(
            encoded(wrap),
            ("near_deposit".to_string(), serde_json::json!({}), amount)
        );
        assert_eq!(
            encoded(CallBlueprint::UnwrapNear {
                amount,
                trader_account_id: None,
            }),
            (
                "near_withdraw".to_string(),
                serde_json::json!({ "amount": amount }),
                one_yocto()
            )
        );
    }

    #[test]
    fn rhea_withdraw_skips_unwrap_near_for_nep141s() {
        let withdraw = |output| {
            encoded(CallBlueprint::RheaWithdraw {
                token_id: account("wrap.near"),
                amount: 10,
                output,
            })
        };
        assert_eq!(
            withdraw(RheaOutput::Near {
                trader_account_id: None
            }),
            (
                "withdraw".to_string(),
                serde_json::json!({
                    "token_id": "wrap.near",
                    "amount": "10",
                    "skip_unwrap_near": false,
                }),
                one_yocto()
            )
        );
        assert_eq!(
            withdraw(RheaOutput::Nep141).1["skip_unwrap_near"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn simulated_swaps_get_a_hundredth_of_their_least_output() {
        let rhea_swap = |actions| CallBlueprint::RheaSwap {
            actions,
            swap_gas: Gas::from_ggas(20_000, 20_000),
            referral_id: account("ref.near"),
        };
        let mut slipped_actions = rhea_actions();
        slipped_actions[1].min_amount_out = 0;
        assert_eq!(
            rhea_swap(rhea_actions()).with_99_percent_slippage(),
            rhea_swap(slipped_actions)
        );

        let rhea_dcl_swap = |swap| CallBlueprint::InputNep141TransferCallRheaDclSwap {
            token_id: account("wrap.near"),
            amount: 100,
            pool_ids: vec!["wrap.near|ft|100".to_string()],
            token_out: account("ft"),
            swap,
            swap_gas: Gas::from_ggas(20_000, 20_000),
            work: DclWork::default(),
            output: RheaOutput::Nep141,
            referral_id: account("ref.near"),
        };
        assert_eq!(
            rhea_dcl_swap(RheaDclSwap::ExactIn {
                min_amount_out: 7_900
            })
            .with_99_percent_slippage(),
            rhea_dcl_swap(RheaDclSwap::ExactIn { min_amount_out: 79 })
        );
        let exact_out = rhea_dcl_swap(RheaDclSwap::ExactOut { amount_out: 80 });
        assert_eq!(exact_out.clone().with_99_percent_slippage(), exact_out);

        let mut operations = intear_operations(vec![withdraw_ft(), withdraw_near()]);
        operations.swap.constraint = 7_900;
        operations.withdrawals[0].amount = IntearDexWithdrawAmount::Full {
            at_least: Some(7_900),
        };
        let CallBlueprint::IntearDexExecuteOperations { operations } =
            CallBlueprint::IntearDexExecuteOperations { operations }.with_99_percent_slippage()
        else {
            unreachable!();
        };
        assert_eq!(operations.swap.constraint, 79);
        assert_eq!(
            operations.withdrawals[0].amount,
            IntearDexWithdrawAmount::Full { at_least: Some(79) }
        );
        assert_eq!(
            operations.withdrawals[1].amount,
            IntearDexWithdrawAmount::Full { at_least: None }
        );
    }

    #[test]
    fn simulated_swaps_may_take_100_times_their_most_input() {
        let rhea_dcl_swap = |amount, swap| CallBlueprint::InputNep141TransferCallRheaDclSwap {
            token_id: account("wrap.near"),
            amount,
            pool_ids: vec!["wrap.near|ft|100".to_string()],
            token_out: account("ft"),
            swap,
            swap_gas: Gas::from_ggas(20_000, 20_000),
            work: DclWork::default(),
            output: RheaOutput::Nep141,
            referral_id: account("ref.near"),
        };
        let exact_out = RheaDclSwap::ExactOut { amount_out: 80 };
        assert_eq!(
            rhea_dcl_swap(101, exact_out.clone()).with_100x_max_input(),
            rhea_dcl_swap(10_100, exact_out)
        );
        let exact_in = rhea_dcl_swap(100, RheaDclSwap::ExactIn { min_amount_out: 79 });
        assert_eq!(exact_in.clone().with_100x_max_input(), exact_in);
        assert_eq!(
            CallBlueprint::WrapNear {
                amount: NearToken::from_near(3)
            }
            .with_100x_max_input(),
            CallBlueprint::WrapNear {
                amount: NearToken::from_near(300)
            }
        );

        let with_oracle_fee = |mut operations: IntearDexOperations, fee_swap| {
            operations.swap.oracle_request = Some(Box::new(IntearDexOracleRequest {
                dex_id: "slimedragon.near/amp".parse().unwrap(),
                request: OracleRequest {
                    oracle_key: "ed25519:11111111111111111111111111111111".parse().unwrap(),
                    request: Base64VecU8(vec![]),
                    fee: U128(25),
                    fee_swap,
                },
            }));
            operations
        };
        let fee_swap = swap_router_types::hook::OracleFeeSwap {
            dex_id: "slimedragon.near/xyk".parse().unwrap(),
            message: Base64VecU8(vec![1, 0, 0, 0]),
            gas: Gas::from_ggas(5_000, 6_000),
            fee: (AssetId::Nep141(account("usdc.near")), U128(1_000)),
        };
        let deposit_near = |amount, operations| CallBlueprint::IntearDexDepositNear {
            amount: NearToken::from_yoctonear(amount),
            operations: Some(operations),
        };

        // Of at most 101 for the swap and 25 for the oracle's fee
        let mut exact_out = intear_operations(vec![withdraw_ft(), withdraw_near()]);
        exact_out.swap.amount = Amount::AmountOut(80);
        exact_out.swap.constraint = 101;
        let exact_out = with_oracle_fee(exact_out, Some(fee_swap.clone()));
        let mut raised = exact_out.clone();
        raised.swap.constraint = 10_100;
        raised.swap.oracle_request.as_mut().unwrap().request.fee = U128(2_500);
        assert_eq!(
            deposit_near(126, exact_out).with_100x_max_input(),
            deposit_near(12_600, raised)
        );

        // Of exactly 100 for the swap after the oracle's fee
        let exact_in = with_oracle_fee(
            intear_operations(vec![withdraw_ft(), withdraw_near()]),
            Some(fee_swap),
        );
        let mut raised = exact_in.clone();
        raised.swap.oracle_request.as_mut().unwrap().request.fee = U128(2_500);
        assert_eq!(
            deposit_near(125, exact_in.clone()).with_100x_max_input(),
            deposit_near(2_600, raised)
        );

        // The oracle takes its fee in the asset in, without a swap
        let fee_in_asset_in = deposit_near(
            125,
            with_oracle_fee(intear_operations(vec![withdraw_ft()]), None),
        );
        assert_eq!(
            fee_in_asset_in.clone().with_100x_max_input(),
            fee_in_asset_in
        );

        // Swaps the trader's balances on Intear DEX
        let execute_operations = CallBlueprint::IntearDexExecuteOperations {
            operations: exact_in,
        };
        assert_eq!(
            execute_operations.clone().with_100x_max_input(),
            execute_operations
        );
    }

    #[test]
    fn intear_dex_withdraw_is_of_an_exact_amount() {
        assert_eq!(
            encoded(CallBlueprint::IntearDexWithdraw {
                asset: IntearAssetId::Nep141(account("wrap.near")),
                amount: 42,
                trader_account_id: None,
            }),
            (
                "withdraw".to_string(),
                serde_json::json!({
                    "asset_id": "nep141:wrap.near",
                    "amount": { "Exact": "42" },
                }),
                one_yocto()
            )
        );
    }

    #[test]
    fn deposits_are_ft_transfer_calls_without_a_message() {
        for (call, receiver_id) in [
            (
                CallBlueprint::Nep141DepositOnRhea {
                    token_id: account("ft"),
                    amount: 7,
                },
                "v2.ref-finance.near",
            ),
            (
                CallBlueprint::Nep141DepositOnIntearDex {
                    token_id: account("ft"),
                    amount: 7,
                },
                "dex.intear.near",
            ),
        ] {
            assert_eq!(call.receiver_id(), account("ft"));
            assert_eq!(
                encoded(call),
                (
                    "ft_transfer_call".to_string(),
                    serde_json::json!({
                        "receiver_id": receiver_id,
                        "amount": "7",
                        "msg": "",
                    }),
                    one_yocto()
                )
            );
        }
    }

    #[test]
    fn storage_deposits_name_the_account_unless_its_the_signer() {
        let amount = NearToken::from_millinear(1250);
        let storage_deposit = |account_id| CallBlueprint::Nep141StorageDeposit {
            token_id: account("ft"),
            account_id,
            amount,
        };
        assert_eq!(storage_deposit(None).storage_deposit(), amount);
        assert_eq!(
            encoded(storage_deposit(None)),
            (
                "storage_deposit".to_string(),
                serde_json::json!({ "registration_only": true }),
                amount
            )
        );
        assert_eq!(
            encoded(storage_deposit(Some(account("bob.near")))).1,
            serde_json::json!({
                "registration_only": true,
                "account_id": "bob.near",
            })
        );
        assert_eq!(
            encoded(CallBlueprint::RheaStorageDeposit {
                account_id: Some(account("bob.near")),
                amount,
            })
            .1,
            serde_json::json!({
                "registration_only": false,
                "account_id": "bob.near",
            })
        );
        assert_eq!(
            encoded(CallBlueprint::IntearDexRegisterAssets {
                asset_ids: vec![IntearAssetId::Near],
                for_account: Some(account("bob.near")),
            })
            .1,
            serde_json::json!({ "asset_ids": ["near"], "for": { "Account": "bob.near" } })
        );
    }

    #[test]
    fn fee_transfers_have_a_trading_fee_memo_naming_the_swap_referrer() {
        let fee_transfer = CallBlueprint::InputNep141FeeTransfer {
            token_id: account("ft"),
            receiver_id: account("ref.near"),
            amount: 8,
            swap_referrer_id: account("app.near"),
        };
        assert_eq!(fee_transfer.wallet_fee_amount(), 8);
        assert_eq!(
            encoded(fee_transfer),
            (
                "ft_transfer".to_string(),
                serde_json::json!({
                    "receiver_id": "ref.near",
                    "amount": "8",
                    "memo": "Trading fee (app.near)",
                }),
                one_yocto()
            )
        );
        let mft_transfer = CallBlueprint::InputRheaFeeMftTransfer {
            token_id: account("ft"),
            receiver_id: account("ref.near"),
            amount: 8,
            swap_referrer_id: account("app.near"),
        };
        // Out of the Rhea balance, not the wallet
        assert_eq!(mft_transfer.wallet_fee_amount(), 0);
        assert_eq!(
            encoded(mft_transfer).1,
            serde_json::json!({
                "token_id": "ft",
                "receiver_id": "ref.near",
                "amount": "8",
                "memo": "Trading fee (app.near)",
            })
        );
    }

    #[test]
    fn rhea_swap_message_skips_the_input_of_later_hops() {
        let (method_name, args, _) = encoded(CallBlueprint::InputNep141TransferCallRheaSwap {
            token_id: account("wrap.near"),
            amount: 100,
            actions: rhea_actions(),
            swap_gas: Gas::from_ggas(5_500, 5_500),
            output: RheaOutput::Nep141,
            referral_id: account("ref.near"),
        });
        assert_eq!(method_name, "ft_transfer_call");
        assert_eq!(args["receiver_id"], "v2.ref-finance.near");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(args["msg"].as_str().unwrap()).unwrap(),
            serde_json::json!({
                "force": 0,
                "actions": [
                    {
                        "pool_id": 1,
                        "token_in": "wrap.near",
                        "token_out": "usdc",
                        "amount_in": "100",
                        "amount_out": "0",
                        "min_amount_out": "0",
                    },
                    {
                        "pool_id": 2,
                        "token_in": "usdc",
                        "token_out": "ft",
                        "amount_out": "0",
                        "min_amount_out": "79",
                    },
                ],
                "skip_degen_price_sync": true,
                "skip_unwrap_near": true,
                "referral_id": "ref.near",
            })
        );
    }

    #[test]
    fn rhea_dcl_swap_by_output_walks_the_pools_from_the_output() {
        let message = |swap| {
            let (_, args, _) = encoded(CallBlueprint::InputNep141TransferCallRheaDclSwap {
                token_id: account("wrap.near"),
                amount: 101,
                pool_ids: vec!["wrap.near|usdc|100".to_string(), "ft|usdc|2000".to_string()],
                token_out: account("ft"),
                swap,
                swap_gas: Gas::from_ggas(20_000, 20_000),
                work: DclWork::default(),
                output: RheaOutput::Nep141,
                referral_id: account("ref.near"),
            });
            assert_eq!(args["receiver_id"], "dclv2.ref-labs.near");
            serde_json::from_str::<serde_json::Value>(args["msg"].as_str().unwrap()).unwrap()
        };
        assert_eq!(
            message(RheaDclSwap::ExactIn { min_amount_out: 79 }),
            serde_json::json!({
                "Swap": {
                    "pool_ids": ["wrap.near|usdc|100", "ft|usdc|2000"],
                    "output_token": "ft",
                    "min_output_amount": "79",
                    "skip_unwrap_near": true,
                    "referral_id": "ref.near",
                }
            })
        );
        assert_eq!(
            message(RheaDclSwap::ExactOut { amount_out: 80 }),
            serde_json::json!({
                "SwapByOutput": {
                    "pool_ids": ["ft|usdc|2000", "wrap.near|usdc|100"],
                    "output_token": "ft",
                    "output_amount": "80",
                    "skip_unwrap_near": true,
                    "referral_id": "ref.near",
                }
            })
        );
    }
}
