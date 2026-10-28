use intear_dex_types::{
    AssetId, DirectWithdrawAmount, Operation, SwapAdvancedMode, SwapOperationAmount, SwapOption,
    SwapRequestAmount, WithdrawAmount,
};
use near_min_api::types::{
    near_account_id::AccountType, AccountId, Action, Balance, FunctionCallAction, NearGas,
    NearToken,
};
use near_sdk::json_types::{Base64VecU8, U128};
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
    pub referrer: AccountId,
    /// The engine takes them out of the swap's input
    pub referral_fees: QuoteFees,
    /// The signer, who withdrawn NEAR goes to
    pub trader_account_id: Option<AccountId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapOperation {
    pub asset_in: IntearAssetId,
    pub asset_out: IntearAssetId,
    /// `AmountIn` swaps an exact input, `AmountOut` gets an exact output
    pub amount: Amount,
    /// The least output of an exact-in swap, or the most input of an exact-out one
    pub constraint: Balance,
    /// Best first. One is swapped on alone, the engine falls back through more of them when one
    /// fails or doesn't meet the constraint.
    pub options: Vec<IntearDexSwapOption>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntearDexSwapOption {
    pub dex_id: intear_dex_types::DexId,
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

/// Burns `ft_transfer_call` and `ft_resolve_transfer` with the fees of `ft_on_transfer`, and needs
/// attached what the token keeps, `ft_on_transfer` gets the rest
const FT_TRANSFER_CALL_GAS: Gas = Gas::from_ggas(5_400, 31_000);
/// Not burnt, attached so a call doesn't run out of gas when it burns more than estimated
const GAS_MARGIN: Gas = Gas::from_ggas(0, 3_000);
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
            | CallBlueprint::InputNep141FeeTransfer { token_id, .. } => token_id.clone(),
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

    /// Source: sandbox on mainnet state. What's attached is capped at [`MAX_CALL_GAS`].
    pub fn gas(&self) -> Gas {
        let gas = match self {
            CallBlueprint::Nep141StorageDeposit { .. } => Gas::from_ggas(2_300, 11_000),
            CallBlueprint::Nep141DepositOnRhea { .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(2_890, 2_500)
            }
            CallBlueprint::Nep141DepositOnIntearDex { .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(3_670, 3_300)
            }
            CallBlueprint::WrapNear { .. } => Gas::from_ggas(1_700, 2_500),
            CallBlueprint::UnwrapNear {
                trader_account_id, ..
            } => Gas::from_ggas(1_900, 2_500) + near_receive_gas(trader_account_id.as_ref()),
            CallBlueprint::InputNep141TransferCallRheaSwap {
                swap_gas, output, ..
            } => {
                FT_TRANSFER_CALL_GAS
                    + Gas::from_ggas(3_600, 3_600)
                    + *swap_gas
                    + rhea_output_gas(output)
                    + GAS_MARGIN
            }
            CallBlueprint::InputNep141TransferCallRheaDclSwap {
                swap_gas, output, ..
            } => {
                // DCL reserves the same gas for sending either output
                let output_burn = match output {
                    RheaOutput::Nep141 => NearGas::from_ggas(4_360),
                    RheaOutput::Near { trader_account_id } => NearGas::from_gas(
                        NearGas::from_ggas(5_160).as_gas()
                            + near_receive_gas(trader_account_id.as_ref())
                                .estimated_burn
                                .as_gas(),
                    ),
                };
                FT_TRANSFER_CALL_GAS
                    + *swap_gas
                    + Gas {
                        estimated_burn: output_burn,
                        needs_attached: NearGas::from_ggas(31_800),
                    }
            }
            CallBlueprint::InputNep141TransferCallIntearDex { operations, .. } => {
                FT_TRANSFER_CALL_GAS + Gas::from_ggas(2_600, 2_600) + operations.gas()
            }
            CallBlueprint::InputNep141FeeTransfer { .. } => Gas::from_ggas(2_000, 10_000),
            CallBlueprint::RheaStorageDeposit { .. } => Gas::from_ggas(2_610, 3_000),
            CallBlueprint::RheaRegisterTokens { token_ids } => {
                Gas::from_ggas(2_500, 3_000) + Gas::from_ggas(500, 500) * token_ids.len() as u64
            }
            CallBlueprint::RheaSwap { swap_gas, .. } => {
                Gas::from_ggas(2_800, 2_800) + *swap_gas + GAS_MARGIN
            }
            CallBlueprint::InputRheaFeeMftTransfer { .. } => Gas::from_ggas(3_590, 4_000),
            CallBlueprint::RheaWithdraw { output, .. } => {
                Gas::from_ggas(3_700, 3_700) + rhea_output_gas(output)
            }
            CallBlueprint::RheaDclStorageDeposit { .. } => Gas::from_ggas(3_000, 4_000),
            CallBlueprint::IntearDexStorageDeposit { .. } => Gas::from_ggas(3_080, 3_500),
            CallBlueprint::IntearDexRegisterAssets { asset_ids, .. } => {
                Gas::from_ggas(3_270, 3_770) + Gas::from_ggas(340, 340) * asset_ids.len() as u64
            }
            CallBlueprint::IntearDexDepositNear {
                operations: None, ..
            } => Gas::from_ggas(3_720, 4_000),
            CallBlueprint::IntearDexDepositNear {
                operations: Some(operations),
                ..
            }
            | CallBlueprint::IntearDexExecuteOperations { operations } => {
                Gas::from_ggas(3_600, 3_600) + operations.gas()
            }
            CallBlueprint::IntearDexWithdraw {
                asset,
                trader_account_id,
                ..
            } => {
                Gas::from_ggas(4_400, 4_400)
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
            CallBlueprint::Nep141StorageDeposit { .. }
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

    pub fn into_action(self) -> Action {
        let gas = self.gas();
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
                    "amount": DirectWithdrawAmount::Exact(U128(amount)),
                }),
                one_yocto,
            ),
        };
        Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: method_name.to_string(),
            args: serde_json::to_vec(&args).unwrap(),
            gas: gas.attached(),
            deposit,
        }))
    }
}

impl IntearDexOperations {
    fn gas(&self) -> Gas {
        let swap_options = &self.swap.options;
        // The first option usually fills the swap, the engine tries the others until one does
        let swap_gas = Gas {
            estimated_burn: swap_options[0].gas.estimated_burn,
            needs_attached: NearGas::from_gas(
                swap_options
                    .iter()
                    .map(|option| option.gas.needs_attached.as_gas())
                    .sum(),
            ),
        };
        let withdrawals_gas = self
            .withdrawals
            .iter()
            .map(|withdrawal| {
                Gas::from_ggas(500, 500)
                    + intear_dex_sending_out_gas(&withdrawal.asset, self.trader_account_id.as_ref())
            })
            .sum::<Gas>();
        Gas::from_ggas(600, 600) * self.referral_fees.0.len() as u64
            + swap_gas
            + withdrawals_gas
            + GAS_MARGIN
    }

    fn engine_args(&self) -> serde_json::Value {
        let swap = &self.swap;
        let asset_in = AssetId::from(&swap.asset_in);
        let asset_out = AssetId::from(&swap.asset_out);
        let amount = SwapOperationAmount::Amount(match swap.amount {
            Amount::AmountIn(amount) => SwapRequestAmount::ExactIn(U128(amount)),
            Amount::AmountOut(amount) => SwapRequestAmount::ExactOut(U128(amount)),
        });
        let constraint = Some(U128(swap.constraint));
        let swap_operation = match swap.options.as_slice() {
            [option] => Operation::SwapSimple {
                dex_id: option.dex_id.clone(),
                message: option.message.clone(),
                asset_in,
                asset_out,
                amount,
                constraint,
            },
            options => Operation::SwapAdvanced {
                asset_in,
                asset_out,
                amount,
                constraint,
                options: options
                    .iter()
                    .map(|option| SwapOption {
                        dex_id: option.dex_id.clone(),
                        message: option.message.clone(),
                    })
                    .collect(),
                mode: SwapAdvancedMode::FallbackQueue,
            },
        };
        let withdrawals = self
            .withdrawals
            .iter()
            .map(|withdrawal| Operation::Withdraw {
                asset_id: AssetId::from(&withdrawal.asset),
                amount: match withdrawal.amount {
                    IntearDexWithdrawAmount::Exact(amount) => WithdrawAmount::Exact(U128(amount)),
                    IntearDexWithdrawAmount::Full { at_least } => WithdrawAmount::Full {
                        at_least: at_least.map(U128),
                    },
                },
                to: None,
                rescue_address: None,
            });
        let operations = [swap_operation]
            .into_iter()
            .chain(withdrawals)
            .collect::<Vec<_>>();
        serde_json::json!({
            "operations": operations,
            "referrer": self.referrer,
            "referral_fees": intear_dex_types::ReferralFees::from(&self.referral_fees),
        })
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

/// Of Rhea sending a token out of the signer's balance
fn rhea_output_gas(output: &RheaOutput) -> Gas {
    match output {
        RheaOutput::Nep141 => Gas::from_ggas(4_200, 26_800),
        // Rhea attaches no gas to the transfer of NEAR
        RheaOutput::Near { trader_account_id } => {
            Gas::from_ggas(4_600, 31_800)
                + Gas {
                    estimated_burn: near_receive_gas(trader_account_id.as_ref()).estimated_burn,
                    needs_attached: NearGas::from_gas(0),
                }
        }
    }
}

/// Of Intear DEX sending an asset out of the signer's balance
fn intear_dex_sending_out_gas(asset: &IntearAssetId, trader_account_id: Option<&AccountId>) -> Gas {
    match asset {
        IntearAssetId::Near => Gas::from_ggas(2_900, 5_900) + near_receive_gas(trader_account_id),
        IntearAssetId::Nep141(_) => Gas::from_ggas(4_900, 16_800),
    }
}

/// Gas that sending NEAR to the trader burns, creating their account if it's implicit
pub fn near_receive_gas(trader_account_id: Option<&AccountId>) -> Gas {
    let account_creation_gas =
        match trader_account_id.map(|account_id| account_id.get_account_type()) {
            Some(AccountType::NearImplicitAccount) => {
                CREATE_ACCOUNT_EXECUTION_GAS.as_gas() + ADD_FULL_ACCESS_KEY_EXECUTION_GAS.as_gas()
            }
            Some(AccountType::EthImplicitAccount | AccountType::NearDeterministicAccount) => {
                CREATE_ACCOUNT_EXECUTION_GAS.as_gas()
            }
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
    use super::*;

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn trader() -> AccountId {
        account("trader.near")
    }

    /// The method, JSON args and deposit of the call
    fn encoded(call: CallBlueprint) -> (String, serde_json::Value, NearToken) {
        let Action::FunctionCall(function_call) = call.into_action() else {
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
                asset_in: IntearAssetId::Near,
                asset_out: IntearAssetId::Nep141(account("ft")),
                amount: Amount::AmountIn(100),
                constraint: 79,
                options: vec![IntearDexSwapOption {
                    dex_id: "slimedragon.near/xyk".parse().unwrap(),
                    message: Base64VecU8(vec![1, 0, 0, 0]),
                    gas: Gas::from_ggas(34_000, 34_000),
                }],
            },
            withdrawals,
            referrer: account("ref.near"),
            referral_fees: QuoteFees::default(),
            trader_account_id: Some(trader()),
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
            rhea_swap(RheaOutput::Nep141).gas(),
            Gas::from_ggas(18_700, 69_900)
        );
        assert_eq!(
            rhea_swap(RheaOutput::Near {
                trader_account_id: Some(trader())
            })
            .gas(),
            gas(19_323_182_562_500, 74_900_000_000_000)
        );
        assert_eq!(
            CallBlueprint::RheaSwap {
                actions: rhea_actions(),
                swap_gas: Gas::from_ggas(5_500, 5_500),
                referral_id: account("ref.near"),
            }
            .gas(),
            Gas::from_ggas(8_300, 11_300)
        );

        let rhea_dcl_swap = |output| CallBlueprint::InputNep141TransferCallRheaDclSwap {
            token_id: account("wrap.near"),
            amount: 100,
            pool_ids: vec!["wrap.near|ft|100".to_string()],
            token_out: account("ft"),
            swap: RheaDclSwap::ExactIn { min_amount_out: 79 },
            swap_gas: Gas::from_ggas(20_000, 20_000),
            output,
            referral_id: account("ref.near"),
        };
        assert_eq!(
            rhea_dcl_swap(RheaOutput::Nep141).gas(),
            Gas::from_ggas(29_760, 82_800)
        );
        assert_eq!(
            rhea_dcl_swap(RheaOutput::Near {
                trader_account_id: Some(trader())
            })
            .gas(),
            gas(30_783_182_562_500, 82_800_000_000_000)
        );

        assert_eq!(
            CallBlueprint::IntearDexDepositNear {
                amount: NearToken::from_yoctonear(100),
                operations: Some(intear_operations(vec![withdraw_ft()])),
            }
            .gas(),
            Gas::from_ggas(43_000, 57_900)
        );
        assert_eq!(
            CallBlueprint::IntearDexDepositNear {
                amount: NearToken::from_yoctonear(100),
                operations: Some(intear_operations(vec![withdraw_ft(), withdraw_near()])),
            }
            .gas(),
            gas(46_623_182_562_500, 64_523_182_562_500)
        );
        assert_eq!(
            CallBlueprint::IntearDexExecuteOperations {
                operations: intear_operations(vec![]),
            }
            .gas(),
            Gas::from_ggas(37_600, 40_600)
        );
        assert_eq!(
            CallBlueprint::InputNep141TransferCallIntearDex {
                token_id: account("ft"),
                amount: 100,
                operations: intear_operations(vec![withdraw_near()]),
            }
            .gas(),
            gas(45_623_182_562_500, 77_223_182_562_500)
        );
    }

    #[test]
    fn registrations_have_gas_for_each_token() {
        assert_eq!(
            CallBlueprint::RheaRegisterTokens {
                token_ids: vec![account("ft")],
            }
            .gas(),
            Gas::from_ggas(3_000, 3_500)
        );
        assert_eq!(
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids: vec![IntearAssetId::Nep141(account("ft"))],
                for_account: None,
            }
            .gas(),
            Gas::from_ggas(3_610, 4_110)
        );
        assert_eq!(
            CallBlueprint::IntearDexRegisterAssets {
                asset_ids: vec![IntearAssetId::Near, IntearAssetId::Nep141(account("ft"))],
                for_account: None,
            }
            .gas(),
            Gas::from_ggas(3_950, 4_450)
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
