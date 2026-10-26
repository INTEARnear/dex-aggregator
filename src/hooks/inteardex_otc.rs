use intear_dex_types::{Operation, SwapOperationAmount, SwapRequestAmount, WithdrawAmount};
use near_min_api::types::{Action, FunctionCallAction, NearGas, NearToken};
use near_sdk::json_types::{Base64VecU8, U128};
use otc_dex_types::AuthorizedTradeIntent;
use swap_router_types::otc::OtcTrade;

use crate::{
    providers::intear_plach::{
        reserved_gas, swap_call_gas_paid_with_operations, AssetId, FT_TRANSFER_CALL_RESERVED_GAS,
        FT_TRANSFER_CALL_SWAP_OVERHEAD_GAS, INTEAR_DEX_CONTRACT_ID, SWAP_OVERHEAD_GAS,
    },
    shared_utils::{
        deposit_storage_if_needed, estimated_gas_paid, register_assets_on_intear, swap_call_gas,
        NetworkView, DEFAULT_REFERRER_ID,
    },
    types::ExecutionInstruction,
    Amount, DexId, Route, SwapRequest,
};

const OTC_SWAP_GAS: NearGas = NearGas::from_ggas(27_500);

/// The route that fills `maker_intent`, quoted by the hook `dex_id`. An intent that doesn't take
/// the other side of the request's [`OtcTrade`] panics.
pub async fn route(
    network: &impl NetworkView,
    dex_id: DexId,
    request: &SwapRequest,
    maker_intent: Base64VecU8,
) -> Option<Route> {
    let otc_trade = OtcTrade::of_request(request)
        .unwrap_or_else(|| panic!("{dex_id} quoted a swap that can't go through the OTC DEX"));
    let maker_trade = near_sdk::borsh::from_slice::<AuthorizedTradeIntent>(&maker_intent.0)
        .unwrap_or_else(|err| {
            panic!("{dex_id} quoted an intent that isn't an AuthorizedTradeIntent: {err}")
        })
        .trade_intent;
    if let Err(err) = otc_trade.check_maker_trade(&maker_trade) {
        panic!("{dex_id} quoted: {err}");
    }

    let fees = match request.amount {
        Amount::AmountIn(amount_in) => otc_trade.referral_fees.amounts_out_of(U128(amount_in)),
        Amount::AmountOut(_) => otc_trade
            .referral_fees
            .amounts_on_top_of(maker_trade.amount_out),
    }
    .into_iter()
    .filter(|(_, fee)| fee.0 > 0)
    .collect::<Vec<_>>();
    let deposit_amount = maker_trade.amount_out.0 + fees.iter().map(|(_, fee)| fee.0).sum::<u128>();
    let output_amount = maker_trade.amount_in;
    let (swap_amount, constraint, estimated_amount) = match request.amount {
        Amount::AmountIn(amount_in) => (
            SwapRequestAmount::ExactIn(U128(amount_in)),
            output_amount,
            Amount::AmountOut(output_amount.0),
        ),
        Amount::AmountOut(amount_out) => (
            SwapRequestAmount::ExactOut(U128(amount_out)),
            U128(deposit_amount),
            Amount::AmountIn(deposit_amount),
        ),
    };

    let asset_in = &otc_trade.asset_in;
    let asset_out = &otc_trade.asset_out;
    let operations = vec![
        Operation::SwapSimple {
            dex_id: "slimedragon.near/otc".parse().unwrap(),
            message: maker_intent,
            asset_in: asset_in.clone(),
            asset_out: asset_out.clone(),
            amount: SwapOperationAmount::Amount(swap_amount),
            constraint: Some(constraint),
        },
        Operation::Withdraw {
            asset_id: asset_out.clone(),
            amount: WithdrawAmount::Full {
                at_least: Some(output_amount),
            },
            to: None,
            rescue_address: None,
        },
    ];
    let operations_args = serde_json::json!({
        "operations": operations,
        "referrer": request.referrer_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
        "referral_fees": otc_trade.referral_fees,
    });

    let reserved_gas = reserved_gas(&operations);
    // The output withdrawn as NEAR is sent to the trader
    let near_transfers = if *asset_out == AssetId::Near { 1 } else { 0 };
    let swap_call_gas_paid = |entry_overhead| {
        swap_call_gas_paid_with_operations(
            OTC_SWAP_GAS,
            &operations,
            &otc_trade.referral_fees,
            entry_overhead,
            near_transfers,
            Some(&otc_trade.trader_account_id),
        )
    };
    let (receiver_id, swap_action, swap_gas_paid) = match asset_in {
        AssetId::Near => (
            INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
            Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "deposit_near".to_string(),
                args: serde_json::to_vec(&serde_json::json!({ "operations": operations_args }))
                    .unwrap(),
                gas: swap_call_gas(OTC_SWAP_GAS, reserved_gas, false),
                deposit: NearToken::from_yoctonear(deposit_amount),
            })),
            swap_call_gas_paid(SWAP_OVERHEAD_GAS),
        ),
        AssetId::Nep141(token_in_id) => (
            token_in_id.clone(),
            Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "ft_transfer_call".to_string(),
                args: serde_json::to_vec(&serde_json::json!({
                    "receiver_id": INTEAR_DEX_CONTRACT_ID,
                    "amount": deposit_amount.to_string(),
                    "msg": operations_args.to_string(),
                }))
                .unwrap(),
                gas: swap_call_gas(
                    OTC_SWAP_GAS,
                    NearGas::from_gas(
                        reserved_gas.as_gas() + FT_TRANSFER_CALL_RESERVED_GAS.as_gas(),
                    ),
                    true,
                ),
                deposit: NearToken::from_yoctonear(1),
            })),
            swap_call_gas_paid(FT_TRANSFER_CALL_SWAP_OVERHEAD_GAS),
        ),
        AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
    };

    // The engine pays fees only to receivers that registered the input, and a failed output
    // withdrawal goes back to the trader's balance of it, so the route registers both
    let fee_receivers = fees
        .iter()
        .map(|(receiver_id, _)| (receiver_id, asset_in))
        .collect::<Vec<_>>();
    let execution_instructions = [
        register_assets_on_intear(
            network,
            &otc_trade.trader_account_id,
            &[asset_out],
            &fee_receivers,
        )
        .await?,
        deposit_storage_if_needed(
            network,
            &request.token_out,
            otc_trade.trader_account_id.clone(),
        )
        .await?,
        vec![ExecutionInstruction::NearTransaction {
            receiver_id,
            actions: vec![swap_action.clone()],
            is_fee_transaction: false,
        }],
    ]
    .concat();
    Some(Route {
        dex_id,
        estimated_amount,
        // fills exactly or not at all
        worst_case_amount: estimated_amount,
        estimated_gas_paid: estimated_gas_paid(
            &execution_instructions,
            Some((&swap_action, swap_gas_paid)),
        ),
        execution_instructions,
        deprecated_needs_unwrap_always_false: false,
        discount: None,
        token_output: request.token_out.clone(),
    })
}
