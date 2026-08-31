use std::collections::HashMap;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use near_min_api::types::{AccountId, Action, FunctionCallAction, Gas, NearGas, NearToken};
use num_traits::FromPrimitive;

use super::*;
use crate::providers::intear_plach::AssetId;
use crate::types::{ExecutionInstruction, Slippage, TokenId};

fn account(id: &str) -> AccountId {
    id.parse().unwrap()
}

fn wnear() -> TokenId {
    TokenId::Nep141(account(WRAP_NEAR))
}

fn ft() -> TokenId {
    TokenId::Nep141(account("ft"))
}

fn rhea_wnear() -> TokenId {
    TokenId::Nep141OnRhea(account(WRAP_NEAR))
}

fn rhea_ft() -> TokenId {
    TokenId::Nep141OnRhea(account("ft"))
}

fn intear_near() -> TokenId {
    TokenId::TokenOnIntearDex(AssetId::Near)
}

fn intear_wnear() -> TokenId {
    TokenId::TokenOnIntearDex(AssetId::Nep141(account(WRAP_NEAR)))
}

fn intear_ft() -> TokenId {
    TokenId::TokenOnIntearDex(AssetId::Nep141(account("ft")))
}

fn intear_nep171() -> TokenId {
    TokenId::TokenOnIntearDex(AssetId::Nep171(account("nft.near"), "1".to_string()))
}

fn intear_nep245() -> TokenId {
    TokenId::TokenOnIntearDex(AssetId::Nep245(account("mft.near"), "1".to_string()))
}

fn trader() -> AccountId {
    account("trader.near")
}

fn function_call_from_action(action: &Action) -> &FunctionCallAction {
    match action {
        Action::FunctionCall(function_call) => function_call,
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}

fn args_from_action(action: &Action) -> serde_json::Value {
    serde_json::from_slice(&function_call_from_action(action).args)
        .expect("function call args should be JSON")
}

fn near_tx(instruction: &ExecutionInstruction) -> (&AccountId, &[Action]) {
    match instruction {
        ExecutionInstruction::NearTransaction {
            receiver_id,
            actions,
        } => (receiver_id, actions),
    }
}

fn assert_near_tx(
    instruction: &ExecutionInstruction,
    expected_receiver: &str,
    expected_actions: &[Action],
) {
    let (receiver_id, actions) = near_tx(instruction);
    assert_eq!(receiver_id.as_str(), expected_receiver);
    assert_eq!(actions, expected_actions);
}

fn wrap_tx(receiver: &str, amount: u128) -> ExecutionInstruction {
    ExecutionInstruction::NearTransaction {
        receiver_id: account(receiver),
        actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
    }
}

fn storage_deposit_amount() -> NearToken {
    NearToken::from_near(1)
}

fn bd(value: &str) -> BigDecimal {
    BigDecimal::from_str(value).unwrap()
}

fn expected_auto_slippage(min: &str, max: &str, scale: BigDecimal) -> BigDecimal {
    let min = bd(min);
    let max = bd(max);
    let optimal = min.clone() + (max.clone() - min.clone()) * scale;
    optimal.clamp(min, max).clamp(
        BigDecimal::from_f64(0.0001).unwrap(),
        BigDecimal::from_f64(0.9999).unwrap(),
    )
}

#[test]
fn create_wrap_action_fields() {
    let amount = NearToken::from_near(2);
    let action = create_wrap_action(amount);
    let function_call = function_call_from_action(&action);
    assert_eq!(function_call.method_name, "near_deposit");
    assert_eq!(args_from_action(&action), serde_json::json!({}));
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(2)));
    assert_eq!(function_call.deposit, amount);
}

#[test]
fn create_unwrap_action_fields() {
    let amount = NearToken::from_near(3);
    let action = create_unwrap_action(amount);
    let function_call = function_call_from_action(&action);
    assert_eq!(function_call.method_name, "near_withdraw");
    assert_eq!(
        args_from_action(&action),
        serde_json::json!({ "amount": amount })
    );
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(5)));
    assert_eq!(function_call.deposit, NearToken::from_yoctonear(1));
}

#[test]
fn create_rhea_withdraw_action_skip_unwrap_near_is_inverted() {
    let token_id = account(WRAP_NEAR);
    let unwrap = create_rhea_withdraw_action(&token_id, 10, true);
    let skip = create_rhea_withdraw_action(&token_id, 10, false);
    let unwrap_call = function_call_from_action(&unwrap);

    assert_eq!(unwrap_call.method_name, "withdraw");
    assert_eq!(unwrap_call.gas, Gas(NearGas::from_tgas(50)));
    assert_eq!(unwrap_call.deposit, NearToken::from_yoctonear(1));
    assert_eq!(function_call_from_action(&skip).method_name, "withdraw");
    assert_eq!(
        args_from_action(&unwrap),
        serde_json::json!({
            "token_id": WRAP_NEAR,
            "amount": "10",
            "skip_unwrap_near": false,
        })
    );
    assert_eq!(
        args_from_action(&skip),
        serde_json::json!({
            "token_id": WRAP_NEAR,
            "amount": "10",
            "skip_unwrap_near": true,
        })
    );
}

#[test]
fn create_intear_dex_withdraw_action_uses_exact_amount() {
    let asset_id = AssetId::Nep141(account(WRAP_NEAR));
    let action = create_intear_dex_withdraw_action(&asset_id, 42);
    let function_call = function_call_from_action(&action);
    assert_eq!(function_call.method_name, "withdraw");
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(50)));
    assert_eq!(function_call.deposit, NearToken::from_yoctonear(1));
    assert_eq!(
        args_from_action(&action),
        serde_json::json!({
            "asset_id": AssetId::Nep141(account(WRAP_NEAR)),
            "amount": { "Exact": "42" },
        })
    );
}

#[test]
fn create_rhea_and_intear_nep141_deposit_actions_match_ft_transfer_call_shape() {
    let contract_id = account("v2.ref-finance.near");
    let rhea = create_rhea_nep141_deposit_action(&contract_id, 7);
    let intear = create_intear_nep141_deposit_action(&contract_id, 7);
    for action in [&rhea, &intear] {
        let function_call = function_call_from_action(action);
        assert_eq!(function_call.method_name, "ft_transfer_call");
        assert_eq!(function_call.gas, Gas(NearGas::from_tgas(50)));
        assert_eq!(function_call.deposit, NearToken::from_yoctonear(1));
        assert_eq!(
            args_from_action(action),
            serde_json::json!({
                "receiver_id": "v2.ref-finance.near",
                "amount": "7",
                "msg": "",
            })
        );
    }
}

#[test]
fn create_storage_deposit_action_for_contract_and_someone() {
    let amount = NearToken::from_millinear(1250);
    let for_self = create_storage_deposit_action_for_contract(amount);
    let for_someone = create_storage_deposit_action_for_someone(amount, &account("bob.near"));

    let self_call = function_call_from_action(&for_self);
    assert_eq!(self_call.method_name, "storage_deposit");
    assert_eq!(self_call.gas, Gas(NearGas::from_tgas(10)));
    assert_eq!(self_call.deposit, amount);
    assert_eq!(
        args_from_action(&for_self),
        serde_json::json!({ "registration_only": true })
    );

    let someone_call = function_call_from_action(&for_someone);
    assert_eq!(someone_call.method_name, "storage_deposit");
    assert_eq!(someone_call.deposit, amount);
    assert_eq!(
        args_from_action(&for_someone),
        serde_json::json!({
            "registration_only": true,
            "account_id": "bob.near",
        })
    );
}

#[tokio::test]
async fn create_storage_deposit_action_nep141() {
    let instructions = create_storage_deposit_action(&ft()).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
        )],
    );
}

#[tokio::test]
async fn create_storage_deposit_action_rhea() {
    let instructions = create_storage_deposit_action(&rhea_ft()).await;
    assert_eq!(instructions.len(), 1);
    let (receiver_id, actions) = near_tx(&instructions[0]);
    assert_eq!(receiver_id.as_str(), "v2.ref-finance.near");
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0],
        create_storage_deposit_action_for_contract(NearToken::from_millinear(10))
    );
    let register = function_call_from_action(&actions[1]);
    assert_eq!(register.method_name, "register_tokens");
    assert_eq!(register.gas, Gas(NearGas::from_tgas(10)));
    assert_eq!(register.deposit, NearToken::from_yoctonear(1));
    assert_eq!(
        args_from_action(&actions[1]),
        serde_json::json!({ "token_ids": ["ft"] })
    );
}

#[tokio::test]
async fn create_storage_deposit_action_intear() {
    let instructions = create_storage_deposit_action(&intear_ft()).await;
    assert_eq!(instructions.len(), 1);
    let (receiver_id, actions) = near_tx(&instructions[0]);
    assert_eq!(receiver_id.as_str(), "dex.intear.near");
    assert_eq!(actions.len(), 2);

    let storage = function_call_from_action(&actions[0]);
    assert_eq!(storage.method_name, "storage_deposit");
    assert_eq!(storage.gas, Gas(NearGas::from_tgas(10)));
    assert_eq!(storage.deposit, "0.005 NEAR".parse().unwrap());
    assert_eq!(args_from_action(&actions[0]), serde_json::json!({}));

    let register = function_call_from_action(&actions[1]);
    assert_eq!(register.method_name, "register_assets");
    assert_eq!(register.gas, Gas(NearGas::from_tgas(10)));
    assert_eq!(register.deposit, NearToken::from_yoctonear(1));
    assert_eq!(
        args_from_action(&actions[1]),
        serde_json::json!({ "asset_ids": ["nep141:ft"] })
    );
}

#[tokio::test]
#[should_panic(expected = "NEAR doesn't need a storage deposit")]
async fn create_storage_deposit_action_panics_for_near() {
    create_storage_deposit_action(&TokenId::Near).await;
}

#[test]
fn optimize_empty_input() {
    assert!(optimize_execution_instructions(vec![]).is_empty());
}

#[test]
fn optimize_skips_empty_actions() {
    assert!(
        optimize_execution_instructions(vec![ExecutionInstruction::NearTransaction {
            receiver_id: account(WRAP_NEAR),
            actions: vec![],
        }])
        .is_empty()
    );
}

#[test]
fn optimize_merges_neighboring_same_receiver() {
    let optimized = optimize_execution_instructions(vec![
        wrap_tx(WRAP_NEAR, 1),
        wrap_tx(WRAP_NEAR, 2),
        wrap_tx(WRAP_NEAR, 3),
    ]);
    assert_eq!(optimized.len(), 1);
    assert_near_tx(
        &optimized[0],
        WRAP_NEAR,
        &[
            create_wrap_action(NearToken::from_yoctonear(1)),
            create_wrap_action(NearToken::from_yoctonear(2)),
            create_wrap_action(NearToken::from_yoctonear(3)),
        ],
    );
}

#[test]
fn optimize_does_not_merge_different_receivers() {
    let optimized = optimize_execution_instructions(vec![wrap_tx(WRAP_NEAR, 1), wrap_tx("ft", 2)]);
    assert_eq!(optimized.len(), 2);
    assert_near_tx(
        &optimized[0],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(1))],
    );
    assert_near_tx(
        &optimized[1],
        "ft",
        &[create_wrap_action(NearToken::from_yoctonear(2))],
    );
}

#[test]
fn optimize_does_not_merge_non_adjacent_same_receiver() {
    let optimized = optimize_execution_instructions(vec![
        wrap_tx(WRAP_NEAR, 1),
        wrap_tx("ft", 2),
        wrap_tx(WRAP_NEAR, 3),
    ]);
    assert_eq!(optimized.len(), 3);
}

#[test]
fn optimize_skips_empty_actions_then_merges_neighbors() {
    let optimized = optimize_execution_instructions(vec![
        wrap_tx(WRAP_NEAR, 1),
        ExecutionInstruction::NearTransaction {
            receiver_id: account(WRAP_NEAR),
            actions: vec![],
        },
        wrap_tx(WRAP_NEAR, 2),
    ]);
    assert_eq!(optimized.len(), 1);
    assert_near_tx(
        &optimized[0],
        WRAP_NEAR,
        &[
            create_wrap_action(NearToken::from_yoctonear(1)),
            create_wrap_action(NearToken::from_yoctonear(2)),
        ],
    );
}

#[test]
fn is_near_for_all_token_variants() {
    assert!(is_near(&TokenId::Near));
    assert!(is_near(&wnear()));
    assert!(is_near(&rhea_wnear()));
    assert!(is_near(&intear_near()));
    assert!(is_near(&intear_wnear()));
    assert!(!is_near(&ft()));
    assert!(!is_near(&rhea_ft()));
    assert!(!is_near(&intear_ft()));
    assert!(!is_near(&intear_nep245()));
    assert!(!is_near(&intear_nep171()));
}

#[test]
fn base_token_for_all_token_variants() {
    assert_eq!(base_token(&TokenId::Near), Some(BaseTokenId::Near));
    assert_eq!(
        base_token(&wnear()),
        Some(BaseTokenId::Nep141(account(WRAP_NEAR)))
    );
    assert_eq!(
        base_token(&rhea_wnear()),
        Some(BaseTokenId::Nep141(account(WRAP_NEAR)))
    );
    assert_eq!(
        base_token(&rhea_ft()),
        Some(BaseTokenId::Nep141(account("ft")))
    );
    assert_eq!(base_token(&intear_near()), Some(BaseTokenId::Near));
    assert_eq!(
        base_token(&intear_wnear()),
        Some(BaseTokenId::Nep141(account(WRAP_NEAR)))
    );
    assert_eq!(
        base_token(&intear_ft()),
        Some(BaseTokenId::Nep141(account("ft")))
    );
    assert_eq!(base_token(&intear_nep245()), None);
    assert_eq!(base_token(&intear_nep171()), None);
}

#[test]
fn token_info_deserializes_numbers_and_strings() {
    let json = serde_json::json!({
        "price_usd_raw": "1.5",
        "price_usd_raw_24h_ago": 1.25,
        "circulating_supply": "1000",
        "liquidity_usd": "20",
        "volume_usd_24h": 5,
        "created_at": 42,
    });
    let info: TokenInfo = serde_json::from_value(json).unwrap();
    assert_eq!(info.price_usd_raw, bd("1.5"));
    assert_eq!(info.price_usd_raw_24h_ago, bd("1.25"));
    assert_eq!(info.circulating_supply, 1000);
    assert_eq!(info.liquidity_usd, bd("20"));
    assert_eq!(info.volume_usd_24h, bd("5"));
    assert_eq!(info.created_at, 42);
}

#[test]
fn token_info_rejects_invalid_bigdecimal_type() {
    let json = serde_json::json!({
        "price_usd_raw": true,
        "price_usd_raw_24h_ago": "1",
        "circulating_supply": "1",
        "liquidity_usd": "1",
        "volume_usd_24h": "1",
        "created_at": 1,
    });
    let err = serde_json::from_value::<TokenInfo>(json).unwrap_err();
    assert!(err.to_string().contains("expected number or string"));
}

#[tokio::test]
async fn convert_to_nep141_near_wraps_when_amount_positive() {
    let (instructions, nep141) = convert_to_nep141(&TokenId::Near, None, 5).await.unwrap();
    assert_eq!(nep141.as_str(), WRAP_NEAR);
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );
}

#[tokio::test]
async fn convert_to_nep141_near_skips_wrap_when_amount_zero() {
    let (instructions, nep141) = convert_to_nep141(&TokenId::Near, None, 0).await.unwrap();
    assert_eq!(nep141.as_str(), WRAP_NEAR);
    assert!(instructions.is_empty());
}

#[tokio::test]
async fn convert_to_nep141_nep141_is_identity() {
    let (instructions, nep141) = convert_to_nep141(&ft(), None, 9).await.unwrap();
    assert!(instructions.is_empty());
    assert_eq!(nep141.as_str(), "ft");
}

#[tokio::test]
async fn convert_to_nep141_rhea_withdraws_without_unwrap() {
    let (instructions, nep141) = convert_to_nep141(&rhea_ft(), None, 9).await.unwrap();
    assert_eq!(nep141.as_str(), "ft");
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "v2.ref-finance.near",
        &[create_rhea_withdraw_action(&account("ft"), 9, false)],
    );
}

#[tokio::test]
async fn convert_to_nep141_intear_near_withdraws_and_wraps() {
    let (instructions, nep141) = convert_to_nep141(&intear_near(), None, 4).await.unwrap();
    assert_eq!(nep141.as_str(), WRAP_NEAR);
    assert_eq!(instructions.len(), 2);
    assert_near_tx(
        &instructions[0],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(&AssetId::Near, 4)],
    );
    assert_near_tx(
        &instructions[1],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(4))],
    );
}

#[tokio::test]
async fn convert_to_nep141_intear_near_amount_zero_has_no_transactions() {
    let (instructions, nep141) = convert_to_nep141(&intear_near(), None, 0).await.unwrap();
    assert_eq!(nep141.as_str(), WRAP_NEAR);
    assert!(instructions.is_empty());
}

#[tokio::test]
async fn convert_to_nep141_intear_nep141_withdraws_only() {
    let (instructions, nep141) = convert_to_nep141(&intear_ft(), None, 8).await.unwrap();
    assert_eq!(nep141.as_str(), "ft");
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(
            &AssetId::Nep141(account("ft")),
            8,
        )],
    );
}

#[tokio::test]
async fn convert_to_nep141_unsupported_assets_return_none() {
    assert!(convert_to_nep141(&intear_nep245(), None, 1).await.is_none());
    assert!(convert_to_nep141(&intear_nep171(), None, 1).await.is_none());
}

#[tokio::test]
async fn convert_to_native_near_is_empty() {
    let instructions = convert_to_native(&TokenId::Near, None, NearToken::from_near(1))
        .await
        .unwrap();
    assert!(instructions.is_empty());
}

#[tokio::test]
async fn convert_to_native_wrap_near_unwraps() {
    let amount = NearToken::from_near(1);
    let instructions = convert_to_native(&wnear(), None, amount).await.unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(&instructions[0], WRAP_NEAR, &[create_unwrap_action(amount)]);
}

#[tokio::test]
async fn convert_to_native_wrap_near_zero_returns_none() {
    assert!(convert_to_native(&wnear(), None, NearToken::ZERO)
        .await
        .is_none());
}

#[tokio::test]
async fn convert_to_native_non_wrap_nep141_returns_none() {
    assert!(convert_to_native(&ft(), None, NearToken::from_near(1))
        .await
        .is_none());
}

#[tokio::test]
async fn convert_to_native_rhea_wrap_withdraws_with_unwrap() {
    let amount = NearToken::from_near(1);
    let instructions = convert_to_native(&rhea_wnear(), None, amount)
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "v2.ref-finance.near",
        &[create_rhea_withdraw_action(
            &account(WRAP_NEAR),
            amount.as_yoctonear(),
            true,
        )],
    );
}

#[tokio::test]
async fn convert_to_native_rhea_wrap_zero_returns_none() {
    assert!(convert_to_native(&rhea_wnear(), None, NearToken::ZERO)
        .await
        .is_none());
}

#[tokio::test]
async fn convert_to_native_rhea_non_wrap_returns_none() {
    assert!(convert_to_native(&rhea_ft(), None, NearToken::from_near(1))
        .await
        .is_none());
}

#[tokio::test]
async fn convert_to_native_intear_near_withdraws_only() {
    let amount = NearToken::from_near(1);
    let instructions = convert_to_native(&intear_near(), None, amount)
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(
            &AssetId::Near,
            amount.as_yoctonear(),
        )],
    );
}

#[tokio::test]
async fn convert_to_native_intear_wrap_withdraws_and_unwraps() {
    let amount = NearToken::from_near(1);
    let instructions = convert_to_native(&intear_wnear(), None, amount)
        .await
        .unwrap();
    assert_eq!(instructions.len(), 2);
    assert_near_tx(
        &instructions[0],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(
            &AssetId::Nep141(account(WRAP_NEAR)),
            amount.as_yoctonear(),
        )],
    );
    assert_near_tx(&instructions[1], WRAP_NEAR, &[create_unwrap_action(amount)]);
}

#[tokio::test]
async fn convert_to_native_intear_unsupported_or_zero_returns_none() {
    let amount = NearToken::from_near(1);
    assert!(convert_to_native(&intear_ft(), None, amount)
        .await
        .is_none());
    assert!(convert_to_native(&intear_nep245(), None, amount)
        .await
        .is_none());
    assert!(convert_to_native(&intear_nep171(), None, amount)
        .await
        .is_none());
    assert!(convert_to_native(&intear_near(), None, NearToken::ZERO)
        .await
        .is_none());
}

#[tokio::test]
async fn needs_storage_deposit_for_contract_uses_total_and_rpc_error() {
    let contract = account("ft");
    let account_id = trader();

    let missing = TestNetworkView::default();
    assert!(
        needs_storage_deposit_for_contract(&missing, &account_id, &contract).await,
        "missing RPC result should need a deposit"
    );

    let zero_total = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        NearToken::ZERO,
        NearToken::ZERO,
    );
    assert!(needs_storage_deposit_for_contract(&zero_total, &account_id, &contract).await);

    let funded_total = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert!(!needs_storage_deposit_for_contract(&funded_total, &account_id, &contract).await);
}

#[tokio::test]
async fn needs_storage_deposit_near_is_always_false() {
    let network = TestNetworkView::default();
    assert!(!needs_storage_deposit(&network, &trader(), &TokenId::Near).await);
}

#[tokio::test]
async fn needs_storage_deposit_nep141_delegates_to_contract() {
    let account_id = trader();
    let token = ft();

    let needs = TestNetworkView::default();
    assert!(needs_storage_deposit(&needs, &account_id, &token).await);

    let already = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert!(!needs_storage_deposit(&already, &account_id, &token).await);
}

#[tokio::test]
async fn needs_storage_deposit_rhea_requires_registration_and_available_balance() {
    let account_id = trader();
    let token = rhea_ft();
    let over_threshold = NearToken::from_millinear(11);
    let at_threshold = NearToken::from_millinear(10);

    let missing = TestNetworkView::default();
    assert!(needs_storage_deposit(&missing, &account_id, &token).await);

    let registered_but_low = TestNetworkView::default()
        .with_rhea_registered("trader.near", "ft", true)
        .with_storage(
            "v2.ref-finance.near",
            "trader.near",
            storage_deposit_amount(),
            at_threshold,
        );
    assert!(needs_storage_deposit(&registered_but_low, &account_id, &token).await);

    let funded_but_unregistered = TestNetworkView::default()
        .with_rhea_registered("trader.near", "ft", false)
        .with_storage(
            "v2.ref-finance.near",
            "trader.near",
            storage_deposit_amount(),
            over_threshold,
        );
    assert!(needs_storage_deposit(&funded_but_unregistered, &account_id, &token).await);

    let ready = TestNetworkView::default()
        .with_rhea_registered("trader.near", "ft", true)
        .with_storage(
            "v2.ref-finance.near",
            "trader.near",
            storage_deposit_amount(),
            over_threshold,
        );
    assert!(!needs_storage_deposit(&ready, &account_id, &token).await);
}

#[tokio::test]
async fn needs_storage_deposit_intear_requires_registration_and_available_balance() {
    let account_id = trader();
    let token = intear_ft();
    let over_threshold = NearToken::from_millinear(2);
    let at_threshold = NearToken::from_millinear(1);
    let asset_id = AssetId::Nep141(account("ft"));

    let missing = TestNetworkView::default();
    assert!(needs_storage_deposit(&missing, &account_id, &token).await);

    let registered_but_low = TestNetworkView::default()
        .with_intear_registered("trader.near", &asset_id, true)
        .with_storage(
            "dex.intear.near",
            "trader.near",
            storage_deposit_amount(),
            at_threshold,
        );
    assert!(needs_storage_deposit(&registered_but_low, &account_id, &token).await);

    let funded_but_unregistered = TestNetworkView::default()
        .with_intear_registered("trader.near", &asset_id, false)
        .with_storage(
            "dex.intear.near",
            "trader.near",
            storage_deposit_amount(),
            over_threshold,
        );
    assert!(needs_storage_deposit(&funded_but_unregistered, &account_id, &token).await);

    let ready = TestNetworkView::default()
        .with_intear_registered("trader.near", &asset_id, true)
        .with_storage(
            "dex.intear.near",
            "trader.near",
            storage_deposit_amount(),
            over_threshold,
        );
    assert!(!needs_storage_deposit(&ready, &account_id, &token).await);
}

#[tokio::test]
async fn create_ft_deposit_registrations_covers_trader_contract_both_and_neither() {
    let contract_id = account("v2.ref-finance.near");
    let token_id = account("ft");

    let neither = TestNetworkView::default()
        .with_storage(
            "ft",
            "trader.near",
            storage_deposit_amount(),
            storage_deposit_amount(),
        )
        .with_storage(
            "ft",
            "v2.ref-finance.near",
            storage_deposit_amount(),
            storage_deposit_amount(),
        );
    assert!(
        create_ft_deposit_registrations(&neither, &contract_id, &token_id, Some(trader()))
            .await
            .is_empty()
    );

    let trader_only = TestNetworkView::default().with_storage(
        "ft",
        "v2.ref-finance.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    let instructions =
        create_ft_deposit_registrations(&trader_only, &contract_id, &token_id, Some(trader()))
            .await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
        )],
    );

    let contract_only = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    let instructions =
        create_ft_deposit_registrations(&contract_only, &contract_id, &token_id, Some(trader()))
            .await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &contract_id,
        )],
    );

    let both = TestNetworkView::default();
    let instructions =
        create_ft_deposit_registrations(&both, &contract_id, &token_id, Some(trader())).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[
            create_storage_deposit_action_for_contract("0.00125 NEAR".parse().unwrap()),
            create_storage_deposit_action_for_someone(
                "0.00125 NEAR".parse().unwrap(),
                &contract_id,
            ),
        ],
    );

    let no_trader_contract_needs = TestNetworkView::default();
    let instructions =
        create_ft_deposit_registrations(&no_trader_contract_needs, &contract_id, &token_id, None)
            .await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &contract_id,
        )],
    );
}

#[tokio::test]
async fn deposit_storage_if_needed_none_trader_is_empty() {
    let network = TestNetworkView::default();
    assert!(deposit_storage_if_needed(&network, &ft(), None)
        .await
        .is_empty());
}

#[tokio::test]
async fn deposit_storage_if_needed_with_trader_depends_on_existing_deposit() {
    let already = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert!(deposit_storage_if_needed(&already, &ft(), Some(trader()))
        .await
        .is_empty());

    let needs = TestNetworkView::default();
    let instructions = deposit_storage_if_needed(&needs, &ft(), Some(trader())).await;
    let expected = create_storage_deposit_action(&ft()).await;
    assert_eq!(instructions.len(), expected.len());
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
        )],
    );
}

#[tokio::test]
async fn deposit_storage_on_contract_if_needed_none_trader_is_empty() {
    let network = TestNetworkView::default();
    let contract = account("ft");
    assert!(deposit_storage_on_contract_if_needed(
        &network,
        &contract,
        None,
        NearToken::from_millinear(1),
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn deposit_storage_on_contract_if_needed_with_trader() {
    let contract = account("ft");
    let amount = NearToken::from_millinear(1);

    let already = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert!(
        deposit_storage_on_contract_if_needed(&already, &contract, Some(trader()), amount)
            .await
            .is_empty()
    );

    let needs = TestNetworkView::default();
    let instructions =
        deposit_storage_on_contract_if_needed(&needs, &contract, Some(trader()), amount).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(amount)],
    );
}

#[tokio::test]
async fn convert_to_same_token_is_empty() {
    let network = TestNetworkView::default();
    assert!(convert_to(&network, &ft(), &ft(), 1, None).await.is_empty());
}

#[tokio::test]
async fn convert_to_unsupported_assets_is_empty() {
    let network = TestNetworkView::default();
    assert!(
        convert_to(&network, &intear_nep245(), &TokenId::Near, 1, None)
            .await
            .is_empty()
    );
    assert!(
        convert_to(&network, &TokenId::Near, &intear_nep171(), 1, None)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn convert_to_near_and_wrap_near() {
    let network = TestNetworkView::default();
    let wrap = convert_to(&network, &TokenId::Near, &wnear(), 5, None).await;
    assert_eq!(wrap.len(), 1);
    assert_near_tx(
        &wrap[0],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );

    let unwrap = convert_to(&network, &wnear(), &TokenId::Near, 5, None).await;
    assert_eq!(unwrap.len(), 1);
    assert_near_tx(
        &unwrap[0],
        WRAP_NEAR,
        &[create_unwrap_action(NearToken::from_yoctonear(5))],
    );

    assert!(convert_to(&network, &TokenId::Near, &wnear(), 0, None)
        .await
        .is_empty());
    assert!(convert_to(&network, &wnear(), &TokenId::Near, 0, None)
        .await
        .is_empty());
}

#[tokio::test]
async fn convert_to_rhea_wrap_to_near_withdraws_with_unwrap() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &rhea_wnear(), &TokenId::Near, 5, None).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "v2.ref-finance.near",
        &[create_rhea_withdraw_action(&account(WRAP_NEAR), 5, true)],
    );
}

#[tokio::test]
async fn convert_to_rhea_wrap_to_nep141_wrap_withdraws_without_unwrap() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &rhea_wnear(), &wnear(), 5, None).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "v2.ref-finance.near",
        &[create_rhea_withdraw_action(&account(WRAP_NEAR), 5, false)],
    );
}

#[tokio::test]
async fn convert_to_intear_near_to_wrap_near_withdraws_and_wraps() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &intear_near(), &wnear(), 5, None).await;
    assert_eq!(instructions.len(), 2);
    assert_near_tx(
        &instructions[0],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(&AssetId::Near, 5)],
    );
    assert_near_tx(
        &instructions[1],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );
}

#[tokio::test]
async fn convert_to_rhea_destination_without_trader_registers_contract_then_deposits() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &TokenId::Near, &rhea_wnear(), 5, None).await;
    assert_eq!(instructions.len(), 3, "{instructions:?}");
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &account("v2.ref-finance.near"),
        )],
    );
    assert_near_tx(
        &instructions[1],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );
    assert_near_tx(
        &instructions[2],
        WRAP_NEAR,
        &[create_rhea_nep141_deposit_action(
            &account("v2.ref-finance.near"),
            5,
        )],
    );
}

#[tokio::test]
async fn convert_to_rhea_destination_skips_ft_registration_when_contract_is_storage_deposit_amount()
{
    let network = TestNetworkView::default().with_storage(
        WRAP_NEAR,
        "v2.ref-finance.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    let instructions = convert_to(&network, &TokenId::Near, &rhea_wnear(), 5, None).await;
    assert_eq!(instructions.len(), 2, "{instructions:?}");
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );
    assert_near_tx(
        &instructions[1],
        WRAP_NEAR,
        &[create_rhea_nep141_deposit_action(
            &account("v2.ref-finance.near"),
            5,
        )],
    );
}

#[tokio::test]
async fn convert_to_rhea_destination_with_trader_includes_rhea_and_ft_storage() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &ft(), &rhea_ft(), 5, Some(trader())).await;
    assert_eq!(instructions.len(), 3, "{instructions:?}");
    let (rhea_receiver, rhea_actions) = near_tx(&instructions[0]);
    assert_eq!(rhea_receiver.as_str(), "v2.ref-finance.near");
    assert_eq!(rhea_actions.len(), 2);
    assert_eq!(
        rhea_actions[0],
        create_storage_deposit_action_for_contract(NearToken::from_millinear(10))
    );
    assert_eq!(
        function_call_from_action(&rhea_actions[1]).method_name,
        "register_tokens"
    );

    assert_near_tx(
        &instructions[1],
        "ft",
        &[
            create_storage_deposit_action_for_contract("0.00125 NEAR".parse().unwrap()),
            create_storage_deposit_action_for_someone(
                "0.00125 NEAR".parse().unwrap(),
                &account("v2.ref-finance.near"),
            ),
        ],
    );
    assert_near_tx(
        &instructions[2],
        "ft",
        &[create_rhea_nep141_deposit_action(
            &account("v2.ref-finance.near"),
            5,
        )],
    );
}

#[tokio::test]
async fn convert_to_intear_nep141_destination_deposits_via_ft_transfer_call() {
    let network = TestNetworkView::default().with_storage(
        "ft",
        "dex.intear.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    let instructions = convert_to(&network, &ft(), &intear_ft(), 5, None).await;
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_intear_nep141_deposit_action(
            &account("dex.intear.near"),
            5,
        )],
    );
}

#[tokio::test]
async fn convert_to_intear_near_destination_uses_deposit_near() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &TokenId::Near, &intear_near(), 5, None).await;
    assert_eq!(instructions.len(), 1);
    let (receiver_id, actions) = near_tx(&instructions[0]);
    assert_eq!(receiver_id.as_str(), "dex.intear.near");
    assert_eq!(actions.len(), 1);
    let function_call = function_call_from_action(&actions[0]);
    assert_eq!(function_call.method_name, "deposit_near");
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(10)));
    assert_eq!(function_call.deposit, NearToken::from_yoctonear(5));
    assert_eq!(args_from_action(&actions[0]), serde_json::json!({}));
}

#[tokio::test]
async fn convert_to_intear_near_destination_with_trader_prepends_storage() {
    let network = TestNetworkView::default();
    let instructions =
        convert_to(&network, &TokenId::Near, &intear_near(), 5, Some(trader())).await;
    assert_eq!(instructions.len(), 2, "{instructions:?}");
    let (storage_receiver, storage_actions) = near_tx(&instructions[0]);
    assert_eq!(storage_receiver.as_str(), "dex.intear.near");
    assert_eq!(
        function_call_from_action(&storage_actions[0]).method_name,
        "storage_deposit"
    );
    assert_eq!(
        function_call_from_action(&storage_actions[1]).method_name,
        "register_assets"
    );
    assert_eq!(near_tx(&instructions[1]).0.as_str(), "dex.intear.near");
    assert_eq!(
        function_call_from_action(&near_tx(&instructions[1]).1[0]).method_name,
        "deposit_near"
    );
}

#[tokio::test]
async fn convert_to_instruction_order_is_register_withdraw_convert_deposit() {
    let network = TestNetworkView::default();
    let instructions =
        convert_to(&network, &rhea_wnear(), &intear_wnear(), 5, Some(trader())).await;
    let methods: Vec<&str> = instructions
        .iter()
        .flat_map(|instruction| near_tx(instruction).1)
        .map(|action| function_call_from_action(action).method_name.as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "storage_deposit",
            "register_assets",
            "storage_deposit",
            "storage_deposit",
            "withdraw",
            "ft_transfer_call",
        ]
    );
}

#[tokio::test]
async fn get_slippage_fixed_passthrough_and_clamp() {
    let network = TestNetworkView::default();
    let passthrough = get_slippage(
        &network,
        Slippage::Fixed {
            slippage: bd("0.01"),
        },
        &TokenId::Near,
        &wnear(),
    )
    .await;
    assert_eq!(passthrough, bd("0.01"));

    let too_small = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("0") },
        &TokenId::Near,
        &wnear(),
    )
    .await;
    assert_eq!(too_small, BigDecimal::from_f64(0.0001).unwrap());

    let too_large = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("2") },
        &TokenId::Near,
        &wnear(),
    )
    .await;
    assert_eq!(too_large, BigDecimal::from_f64(0.9999).unwrap());
}

#[tokio::test]
async fn get_slippage_auto_missing_token_uses_scale_0_8() {
    let network = TestNetworkView::default();
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.8).unwrap())
    );
}

#[tokio::test]
async fn get_slippage_auto_token_fetch_error_uses_scale_0_005() {
    let network = TestNetworkView::default().with_tokens_error();
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.005).unwrap())
    );
}

#[tokio::test]
async fn get_slippage_auto_block_height_error_uses_scale_0_005_for_known_token() {
    let mut tokens = HashMap::new();
    tokens.insert(
        TokenId::Near,
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    tokens.insert(
        ft(),
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    let network = TestNetworkView::default()
        .with_tokens(tokens)
        .with_block_height_error();
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.005).unwrap())
    );
}

#[tokio::test]
async fn get_slippage_auto_new_token_uses_max_slippage() {
    let mut tokens = HashMap::new();
    tokens.insert(
        TokenId::Near,
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 999_950,
        },
    );
    tokens.insert(
        ft(),
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    let network = TestNetworkView::default()
        .with_tokens(tokens)
        .with_block_height(1_000_000);
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(slippage, bd("0.51"));
}

#[tokio::test]
async fn get_slippage_auto_somewhat_new_token_uses_scale_0_6() {
    let mut tokens = HashMap::new();
    tokens.insert(
        TokenId::Near,
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 999_200,
        },
    );
    tokens.insert(
        ft(),
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    let network = TestNetworkView::default()
        .with_tokens(tokens)
        .with_block_height(1_000_000);
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.6).unwrap())
    );
}

#[test]
fn token_volatility_scale_new_and_somewhat_new_and_stable() {
    let height = 1_000_000;
    let brand_new = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 1,
        liquidity_usd: bd("1"),
        volume_usd_24h: bd("0"),
        created_at: height - 50,
    };
    assert_eq!(
        token_volatility_scale(&brand_new, height),
        BigDecimal::from(1)
    );

    let somewhat_new = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 1,
        liquidity_usd: bd("1"),
        volume_usd_24h: bd("0"),
        created_at: height - 500,
    };
    assert_eq!(
        token_volatility_scale(&somewhat_new, height),
        BigDecimal::from_f64(0.6).unwrap()
    );

    let stable = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 1_000,
        liquidity_usd: bd("1000"),
        volume_usd_24h: bd("0"),
        created_at: 0,
    };
    assert_eq!(token_volatility_scale(&stable, height), BigDecimal::from(0));
}

#[test]
fn token_volatility_scale_price_change_addends() {
    let height = 1_000_000;
    let over_50 = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1.6"),
        circulating_supply: 1,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("0"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_50, height),
        BigDecimal::from_f64(0.1).unwrap() + BigDecimal::from_f64(0.05).unwrap()
    );

    let over_20 = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1.3"),
        circulating_supply: 1,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("0"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_20, height),
        BigDecimal::from_f64(0.05).unwrap()
    );
}

#[test]
fn token_volatility_scale_volume_to_mcap_addends() {
    let height = 1_000_000;
    let over_one = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 100,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("150"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_one, height),
        BigDecimal::from_f64(0.1).unwrap() + BigDecimal::from_f64(0.05).unwrap()
    );

    let over_point_two = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 100,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("30"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_point_two, height),
        BigDecimal::from_f64(0.05).unwrap()
    );
}

#[test]
fn token_volatility_scale_volume_to_liquidity_addends() {
    let height = 1_000_000;
    let over_one = TokenInfo {
        price_usd_raw: bd("0"),
        price_usd_raw_24h_ago: bd("0"),
        circulating_supply: 0,
        liquidity_usd: bd("10"),
        volume_usd_24h: bd("15"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_one, height),
        BigDecimal::from_f64(0.15).unwrap()
            + BigDecimal::from_f64(0.1).unwrap()
            + BigDecimal::from_f64(0.05).unwrap()
    );

    let over_half = TokenInfo {
        price_usd_raw: bd("0"),
        price_usd_raw_24h_ago: bd("0"),
        circulating_supply: 0,
        liquidity_usd: bd("10"),
        volume_usd_24h: bd("6"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_half, height),
        BigDecimal::from_f64(0.1).unwrap() + BigDecimal::from_f64(0.05).unwrap()
    );

    let over_point_two = TokenInfo {
        price_usd_raw: bd("0"),
        price_usd_raw_24h_ago: bd("0"),
        circulating_supply: 0,
        liquidity_usd: bd("10"),
        volume_usd_24h: bd("3"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&over_point_two, height),
        BigDecimal::from_f64(0.05).unwrap()
    );
}

#[test]
fn token_volatility_scale_zero_price_skips_price_and_mcap() {
    let height = 1_000_000;
    let info = TokenInfo {
        price_usd_raw: bd("0"),
        price_usd_raw_24h_ago: bd("100"),
        circulating_supply: 1,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("1000"),
        created_at: 0,
    };
    assert_eq!(token_volatility_scale(&info, height), BigDecimal::from(0));
}

#[test]
fn token_volatility_scale_zero_liquidity_skips_volume_to_liquidity() {
    let height = 1_000_000;
    let info = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("1"),
        circulating_supply: 100,
        liquidity_usd: bd("0"),
        volume_usd_24h: bd("1000"),
        created_at: 0,
    };
    assert_eq!(
        token_volatility_scale(&info, height),
        BigDecimal::from_f64(0.1).unwrap() + BigDecimal::from_f64(0.05).unwrap()
    );
}

#[test]
fn token_volatility_scale_all_addends_stay_at_or_below_one() {
    let height = 1_000_000;
    let info = TokenInfo {
        price_usd_raw: bd("1"),
        price_usd_raw_24h_ago: bd("2"),
        circulating_supply: 10,
        liquidity_usd: bd("1"),
        volume_usd_24h: bd("20"),
        created_at: 0,
    };
    let scale = token_volatility_scale(&info, height);
    assert!(scale <= 1);
    assert_eq!(
        scale,
        BigDecimal::from_f64(0.1).unwrap()
            + BigDecimal::from_f64(0.05).unwrap()
            + BigDecimal::from_f64(0.1).unwrap()
            + BigDecimal::from_f64(0.05).unwrap()
            + BigDecimal::from_f64(0.15).unwrap()
            + BigDecimal::from_f64(0.1).unwrap()
            + BigDecimal::from_f64(0.05).unwrap()
    );
}

#[tokio::test]
async fn get_slippage_auto_uses_max_of_input_and_output_scales() {
    let mut tokens = HashMap::new();
    tokens.insert(
        TokenId::Near,
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1"),
            circulating_supply: 1,
            liquidity_usd: bd("1"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    tokens.insert(
        ft(),
        TokenInfo {
            price_usd_raw: bd("1"),
            price_usd_raw_24h_ago: bd("1.3"),
            circulating_supply: 1,
            liquidity_usd: bd("0"),
            volume_usd_24h: bd("0"),
            created_at: 0,
        },
    );
    let network = TestNetworkView::default().with_tokens(tokens);
    let slippage = get_slippage(
        &network,
        Slippage::Auto {
            max_slippage: bd("0.51"),
            min_slippage: bd("0.01"),
        },
        &TokenId::Near,
        &ft(),
    )
    .await;
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.05).unwrap())
    );
}
