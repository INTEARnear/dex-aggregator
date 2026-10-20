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
            ..
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
        is_fee_transaction: false,
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
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(5)));
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
    assert_eq!(function_call.gas, Gas(NearGas::from_tgas(30)));
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
    let for_self = create_storage_deposit_action_for_contract(amount, true);
    let for_someone = create_storage_deposit_action_for_someone(amount, &account("bob.near"), true);

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

/// `NearToken` of `bytes` of storage
fn storage_bytes(bytes: u128) -> NearToken {
    STORAGE_BYTE_COST.saturating_mul(bytes)
}

/// The storage deposit and registration `deposit_storage_if_needed` makes on Rhea or Intear DEX
fn registration_deposit(instructions: &[ExecutionInstruction]) -> Option<NearToken> {
    assert_eq!(instructions.len(), 1, "{instructions:?}");
    let (_, actions) = near_tx(&instructions[0]);
    match actions {
        [registration] => {
            assert!(function_call_from_action(registration)
                .method_name
                .starts_with("register_"));
            None
        }
        [storage_deposit, registration] => {
            assert_eq!(
                function_call_from_action(storage_deposit).method_name,
                "storage_deposit"
            );
            assert!(function_call_from_action(registration)
                .method_name
                .starts_with("register_"));
            Some(function_call_from_action(storage_deposit).deposit)
        }
        _ => panic!("Not a registration: {actions:?}"),
    }
}

#[tokio::test]
async fn deposit_storage_if_needed_nep141_deposits_the_token_minimum() {
    let instructions = deposit_storage_if_needed(&TestNetworkView::default(), &ft(), trader())
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
            true,
        )],
    );
    assert_eq!(
        deposit_storage_if_needed(&TestNetworkView::default(), &TokenId::Near, trader()).await,
        Some(vec![])
    );
}

#[tokio::test]
async fn rhea_registration_shape() {
    let instructions = deposit_storage_if_needed(&TestNetworkView::default(), &rhea_ft(), trader())
        .await
        .unwrap();
    let (receiver_id, actions) = near_tx(&instructions[0]);
    assert_eq!(receiver_id.as_str(), "v2.ref-finance.near");
    // A new account and one token
    assert_eq!(
        actions[0],
        create_storage_deposit_action_for_contract(storage_bytes(102 + 148), false)
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

/// Rhea's storage as get_user_storage_state reported it on mainnet: 102 bytes per account and
/// 148 per token, whatever the account ID
#[tokio::test]
async fn rhea_registration_pays_what_the_contract_counts() {
    let tokens = ["a.near", "b.near", "c.near", "d.near", "e.near"].map(account);
    let token_refs = tokens.iter().collect::<Vec<_>>();
    let deposit = |network: &TestNetworkView, account_id: &str, token_count: usize| {
        let network = network.clone();
        let account_id = account(account_id);
        let token_refs = token_refs[..token_count].to_vec();
        async move {
            registration_deposit(
                &rhea_registration(&network, &account_id, &token_refs)
                    .await
                    .unwrap(),
            )
        }
    };

    // 9421fd3a…e82c registered with 102 bytes of usage at block 220398361
    let new_account = "9421fd3a82539a62f76731e4cef32b0d4bef17db405d2ee086589d5847a0e82c";
    let network = TestNetworkView::default();
    assert_eq!(
        deposit(&network, new_account, 1).await,
        Some(storage_bytes(102 + 148))
    );

    // slimedragon.near: 23 tokens, 3506 bytes used of a 4.101e22 deposit at block 220407351
    let network = TestNetworkView::default().with_rhea_storage_state(
        "slimedragon.near",
        NearToken::from_yoctonear(41_010_000_000_000_000_000_000),
        storage_bytes(102 + 148 * 23),
    );
    assert_eq!(deposit(&network, "slimedragon.near", 1).await, None);
    assert_eq!(
        deposit(&network, "slimedragon.near", 5).await,
        Some(
            storage_bytes(102 + 148 * 28)
                .saturating_sub(NearToken::from_yoctonear(41_010_000_000_000_000_000_000))
        )
    );

    // An account using more than its deposit from older storage rules pays that too
    let network = TestNetworkView::default().with_rhea_storage_state(
        "old.near",
        storage_bytes(1000),
        storage_bytes(1200),
    );
    assert_eq!(
        deposit(&network, "old.near", 1).await,
        Some(storage_bytes(200 + 148))
    );

    // Registered tokens need nothing
    let network = TestNetworkView::default().with_rhea_registered("trader.near", "a.near", true);
    assert!(rhea_registration(&network, &trader(), &[&tokens[0]])
        .await
        .unwrap()
        .is_empty());
}

/// Intear DEX storage as storage_balance_of reported it on mainnet for accounts that registered
/// from scratch, and as storage_deposit takes it
#[tokio::test]
async fn intear_registration_pays_what_the_contract_charges() {
    let deposit = |network: TestNetworkView, account_id: &str, asset_ids: Vec<AssetId>| {
        let account_id = account(account_id);
        async move {
            registration_deposit(
                &intear_registration(
                    &network,
                    &account_id,
                    &asset_ids.iter().collect::<Vec<_>>(),
                    &[],
                )
                .await
                .unwrap(),
            )
        }
    };
    let tear = AssetId::Nep141(account("token.intear.near"));

    // tx 6CziqdMLqww3akUuAQiXfainuBM46KAr1NNhgFJLnpE2 paid exactly this
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "starship9.near",
            vec![AssetId::Near, tear.clone()]
        )
        .await,
        Some(storage_bytes(537))
    );
    // tx 928t92DqXw4dQ3poDc1AsWGyyChdea8vsw3NpdKGC2AU
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "01b94be2d588a799498f89cb4eb5e54c965e7f6115b6768d4e154bf210a69139",
            vec![AssetId::Near, tear.clone()]
        )
        .await,
        Some(storage_bytes(837))
    );
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "criptomem.near",
            vec![
                AssetId::Nep141(account("usdt.tether-token.near")),
                AssetId::Nep141(account("qtc.omft.near"))
            ]
        )
        .await,
        Some(storage_bytes(559))
    );
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "decentgui.tg",
            vec![
                AssetId::Near,
                AssetId::Nep141(account(
                    "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1"
                ))
            ]
        )
        .await,
        Some(storage_bytes(572))
    );

    // One asset takes less than the 5e21 minimum of storage_deposit
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "bjirken.near",
            vec![tear.clone()]
        )
        .await,
        Some(NearToken::from_millinear(5))
    );
    // The first registration of an asset also stores its total in custody: 89 for the
    // account, 105 for its balances, 194 for the balance and 178 for the total
    let raen = AssetId::Nep141(account("raen-e1328c.launchpad.justhoot.near"));
    assert_eq!(
        deposit(
            TestNetworkView::default().with_intear_asset_untracked(&raen),
            "bjirken.near",
            vec![raen]
        )
        .await,
        Some(storage_bytes(89 + 105 + 194 + 178))
    );

    // slimedragon.near's registration of a 64 character token took 227 bytes, its balances
    // existing already
    let long_asset = AssetId::Nep141(account(
        "88888.0000000000000000000000000000000000000000000000000000000000",
    ));
    let with_balances = |available: NearToken| {
        TestNetworkView::default()
            .with_storage(
                "dex.intear.near",
                "slimedragon.near",
                NearToken::from_near(1),
                available,
            )
            .with_intear_balances("slimedragon.near")
    };
    assert_eq!(
        deposit(
            with_balances(storage_bytes(227)),
            "slimedragon.near",
            vec![long_asset.clone()]
        )
        .await,
        None
    );
    assert_eq!(
        deposit(
            with_balances(storage_bytes(226)),
            "slimedragon.near",
            vec![long_asset]
        )
        .await,
        Some(NearToken::from_millinear(5))
    );

    // A registered account without balances pays for the map of them
    assert_eq!(
        deposit(
            TestNetworkView::default().with_storage(
                "dex.intear.near",
                "9ce82f7c55c2f649.trade.unrlzd.near",
                NearToken::from_near(1),
                storage_bytes(171 + 177),
            ),
            "9ce82f7c55c2f649.trade.unrlzd.near",
            vec![AssetId::Near]
        )
        .await,
        None
    );
}

#[tokio::test]
async fn storage_lookups_that_fail_give_no_registration() {
    let network = TestNetworkView::default().with_storage_balance_bounds_error("dex.intear.near");
    assert_eq!(
        deposit_storage_if_needed(&network, &intear_ft(), trader()).await,
        None
    );
    let network = TestNetworkView::default().with_storage_balance_bounds_error("ft");
    assert_eq!(
        deposit_storage_if_needed(&network, &ft(), trader()).await,
        None
    );
}

#[tokio::test]
async fn wallet_inputs_need_no_registration() {
    // The trader holds a wallet-level input already, so its storage isn't looked up
    let network = TestNetworkView::default().with_storage_balance_bounds_error("ft");
    assert_eq!(
        register_input_in_wallet(&network, &ft(), account("ft"), trader()).await,
        Some(vec![])
    );
    // Withdrawn from Rhea, it lands in their wallet
    let instructions = register_input_in_wallet(
        &TestNetworkView::default(),
        &rhea_ft(),
        account("ft"),
        trader(),
    )
    .await
    .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
            true,
        )],
    );
}

#[tokio::test]
async fn token_storage_deposit_amount_uses_token_min_below_limit() {
    let network = TestNetworkView::default()
        .with_storage_balance_bounds("ft", "0.0025 NEAR".parse().unwrap());
    assert_eq!(
        token_storage_deposit_amount(&network, &account("ft")).await,
        Some("0.0025 NEAR".parse().unwrap())
    );
}

#[tokio::test]
async fn token_storage_deposit_amount_refuses_at_or_above_limit() {
    let at_limit =
        TestNetworkView::default().with_storage_balance_bounds("ft", NearToken::from_millinear(10));
    assert_eq!(
        token_storage_deposit_amount(&at_limit, &account("ft")).await,
        None
    );

    let above_limit =
        TestNetworkView::default().with_storage_balance_bounds("ft", NearToken::from_near(1));
    assert_eq!(
        token_storage_deposit_amount(&above_limit, &account("ft")).await,
        None
    );
}

#[tokio::test]
async fn token_storage_deposit_amount_is_none_on_query_error() {
    let network = TestNetworkView::default().with_storage_balance_bounds_error("ft");
    assert_eq!(
        token_storage_deposit_amount(&network, &account("ft")).await,
        None
    );
}

#[tokio::test]
async fn create_token_storage_deposit_action_for_someone_uses_token_min() {
    let network = TestNetworkView::default()
        .with_storage_balance_bounds("ft", "0.0025 NEAR".parse().unwrap());
    let contract_id = account("v2.ref-finance.near");
    assert_eq!(
        create_nep141_storage_deposit_action(&network, &account("ft"), Some(&contract_id)).await,
        Some(create_storage_deposit_action_for_someone(
            "0.0025 NEAR".parse().unwrap(),
            &contract_id,
            true
        ))
    );
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
            is_fee_transaction: false,
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
fn optimize_merges_fee_transactions_only_with_each_other() {
    let fee_tx = |amount| ExecutionInstruction::NearTransaction {
        receiver_id: account(WRAP_NEAR),
        actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
        is_fee_transaction: true,
    };
    let optimized =
        optimize_execution_instructions(vec![wrap_tx(WRAP_NEAR, 1), fee_tx(2), fee_tx(3)]);
    assert_eq!(
        optimized,
        vec![
            wrap_tx(WRAP_NEAR, 1),
            ExecutionInstruction::NearTransaction {
                receiver_id: account(WRAP_NEAR),
                actions: vec![
                    create_wrap_action(NearToken::from_yoctonear(2)),
                    create_wrap_action(NearToken::from_yoctonear(3)),
                ],
                is_fee_transaction: true,
            },
        ]
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
            is_fee_transaction: false,
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
async fn convert_to_nep141_rhea_amount_zero_has_no_transactions() {
    // Rhea would withdraw the whole balance for 0
    let (instructions, nep141) = convert_to_nep141(&rhea_ft(), None, 0).await.unwrap();
    assert_eq!(nep141.as_str(), "ft");
    assert!(instructions.is_empty());
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
async fn registered_balances_need_no_storage_deposit() {
    // Rhea and Intear DEX take no more storage to deposit a registered token, however little is
    // available
    let rhea = TestNetworkView::default()
        .with_rhea_registered("trader.near", "ft", true)
        .with_storage(
            "v2.ref-finance.near",
            "trader.near",
            storage_deposit_amount(),
            NearToken::ZERO,
        );
    assert_eq!(
        deposit_storage_if_needed(&rhea, &rhea_ft(), trader()).await,
        Some(vec![])
    );
    let intear = TestNetworkView::default()
        .with_intear_registered("trader.near", &AssetId::Nep141(account("ft")), true)
        .with_storage(
            "dex.intear.near",
            "trader.near",
            storage_deposit_amount(),
            NearToken::ZERO,
        );
    assert_eq!(
        deposit_storage_if_needed(&intear, &intear_ft(), trader()).await,
        Some(vec![])
    );

    // Storage without the registration isn't enough
    let unregistered = TestNetworkView::default().with_storage(
        "v2.ref-finance.near",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        registration_deposit(
            &deposit_storage_if_needed(&unregistered, &rhea_ft(), trader())
                .await
                .unwrap()
        ),
        None
    );
}

#[tokio::test]
async fn register_receiver_on_token_only_when_the_contract_has_no_storage() {
    let contract_id = account("v2.ref-finance.near");
    let token_id = account("ft");

    let registered = TestNetworkView::default().with_storage(
        "ft",
        "v2.ref-finance.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        register_receiver_on_token(&registered, &contract_id, &token_id).await,
        Some(vec![])
    );

    // The trader's storage on the token is left to whoever puts the token in their wallet
    let unregistered = TestNetworkView::default();
    let instructions = register_receiver_on_token(&unregistered, &contract_id, &token_id)
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &contract_id,
            true,
        )],
    );
}

#[tokio::test]
async fn deposit_storage_if_needed_none_trader_is_empty() {
    let network = TestNetworkView::default();
    assert_eq!(
        deposit_storage_if_needed(&network, &ft(), None).await,
        Some(vec![])
    );
}

#[tokio::test]
async fn deposit_storage_if_needed_with_trader_depends_on_existing_deposit() {
    let already = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        deposit_storage_if_needed(&already, &ft(), Some(trader())).await,
        Some(vec![])
    );

    let needs = TestNetworkView::default();
    let instructions = deposit_storage_if_needed(&needs, &ft(), Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "ft",
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
            true,
        )],
    );
}

#[tokio::test]
async fn deposit_storage_on_contract_if_needed_none_trader_is_empty() {
    let network = TestNetworkView::default();
    let contract = account("dclv2.ref-labs.near");
    assert_eq!(
        deposit_storage_on_contract_if_needed(&network, &contract, None).await,
        Some(vec![])
    );
}

#[tokio::test]
async fn deposit_storage_on_contract_if_needed_deposits_the_contract_minimum() {
    let contract = account("dclv2.ref-labs.near");

    let already = TestNetworkView::default().with_storage(
        "dclv2.ref-labs.near",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        deposit_storage_on_contract_if_needed(&already, &contract, Some(trader())).await,
        Some(vec![])
    );

    // dclv2.ref-labs.near's storage_balance_bounds min on mainnet
    let needs = TestNetworkView::default();
    let instructions = deposit_storage_on_contract_if_needed(&needs, &contract, Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1);
    assert_near_tx(
        &instructions[0],
        "dclv2.ref-labs.near",
        &[create_storage_deposit_action_for_contract(
            NearToken::from_millinear(500),
            true,
        )],
    );
}

#[tokio::test]
async fn convert_to_same_token_is_empty() {
    let network = TestNetworkView::default();
    assert!(convert_to(&network, &ft(), &ft(), 1, None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn convert_to_unsupported_assets_is_empty() {
    let network = TestNetworkView::default();
    assert!(
        convert_to(&network, &intear_nep245(), &TokenId::Near, 1, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        convert_to(&network, &TokenId::Near, &intear_nep171(), 1, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn convert_to_near_and_wrap_near() {
    let network = TestNetworkView::default();
    let wrap = convert_to(&network, &TokenId::Near, &wnear(), 5, None)
        .await
        .unwrap();
    assert_eq!(wrap.len(), 1);
    assert_near_tx(
        &wrap[0],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );

    let unwrap = convert_to(&network, &wnear(), &TokenId::Near, 5, None)
        .await
        .unwrap();
    assert_eq!(unwrap.len(), 1);
    assert_near_tx(
        &unwrap[0],
        WRAP_NEAR,
        &[create_unwrap_action(NearToken::from_yoctonear(5))],
    );

    assert!(convert_to(&network, &TokenId::Near, &wnear(), 0, None)
        .await
        .unwrap()
        .is_empty());
    assert!(convert_to(&network, &wnear(), &TokenId::Near, 0, None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn convert_to_rhea_wrap_to_near_withdraws_with_unwrap() {
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &rhea_wnear(), &TokenId::Near, 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &rhea_wnear(), &wnear(), 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &intear_near(), &wnear(), 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &TokenId::Near, &rhea_wnear(), 5, None)
        .await
        .unwrap();
    assert_eq!(instructions.len(), 3, "{instructions:?}");
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &account("v2.ref-finance.near"),
            true,
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
    let instructions = convert_to(&network, &TokenId::Near, &rhea_wnear(), 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &ft(), &rhea_ft(), 5, Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 3, "{instructions:?}");
    let (rhea_receiver, rhea_actions) = near_tx(&instructions[0]);
    assert_eq!(rhea_receiver.as_str(), "v2.ref-finance.near");
    assert_eq!(rhea_actions.len(), 2);
    // A new account with one token
    assert_eq!(
        rhea_actions[0],
        create_storage_deposit_action_for_contract(storage_bytes(102 + 148), false)
    );
    assert_eq!(
        function_call_from_action(&rhea_actions[1]).method_name,
        "register_tokens"
    );

    // The trader holds ft already, so only Rhea is registered on it
    assert_near_tx(
        &instructions[1],
        "ft",
        &[create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            &account("v2.ref-finance.near"),
            true,
        )],
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
    let instructions = convert_to(&network, &ft(), &intear_ft(), 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &TokenId::Near, &intear_near(), 5, None)
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &TokenId::Near, &intear_near(), 5, Some(trader()))
        .await
        .unwrap();
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
    let instructions = convert_to(&network, &rhea_wnear(), &intear_wnear(), 5, Some(trader()))
        .await
        .unwrap();
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
async fn convert_to_registers_the_trader_on_a_withdrawn_token() {
    // The withdrawn wNEAR goes to the trader's wallet before it's unwrapped
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &intear_wnear(), &TokenId::Near, 5, Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 3, "{instructions:?}");
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
            true,
        )],
    );
    assert_near_tx(
        &instructions[1],
        "dex.intear.near",
        &[create_intear_dex_withdraw_action(
            &AssetId::Nep141(account(WRAP_NEAR)),
            5,
        )],
    );
    assert_near_tx(
        &instructions[2],
        WRAP_NEAR,
        &[create_unwrap_action(NearToken::from_yoctonear(5))],
    );

    let registered = TestNetworkView::default().with_storage(
        WRAP_NEAR,
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    let instructions = convert_to(
        &registered,
        &intear_wnear(),
        &TokenId::Near,
        5,
        Some(trader()),
    )
    .await
    .unwrap();
    assert_eq!(instructions.len(), 2, "{instructions:?}");
}

#[tokio::test]
async fn convert_to_registers_the_trader_on_wrapped_near() {
    // Otherwise wrap.near registers them out of the deposit, and they get less than the amount
    let network = TestNetworkView::default();
    let instructions = convert_to(&network, &TokenId::Near, &wnear(), 5, Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 2, "{instructions:?}");
    assert_near_tx(
        &instructions[0],
        WRAP_NEAR,
        &[create_storage_deposit_action_for_contract(
            "0.00125 NEAR".parse().unwrap(),
            true,
        )],
    );
    assert_near_tx(
        &instructions[1],
        WRAP_NEAR,
        &[create_wrap_action(NearToken::from_yoctonear(5))],
    );
}

#[tokio::test]
async fn convert_to_doesnt_register_the_trader_on_their_wallet_token() {
    // Whoever put the token in the wallet registered the trader on it
    let network = TestNetworkView::default()
        .with_storage(
            "ft",
            "dex.intear.near",
            storage_deposit_amount(),
            storage_deposit_amount(),
        )
        .with_intear_registered("trader.near", &AssetId::Nep141(account("ft")), true)
        .with_storage(
            "dex.intear.near",
            "trader.near",
            storage_deposit_amount(),
            storage_deposit_amount(),
        );
    let instructions = convert_to(&network, &ft(), &intear_ft(), 5, Some(trader()))
        .await
        .unwrap();
    assert_eq!(instructions.len(), 1, "{instructions:?}");
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
async fn get_slippage_fixed_passthrough_and_clamp() {
    let network = TestNetworkView::default();
    let passthrough = get_slippage(
        &network,
        Slippage::Fixed {
            slippage: bd("0.01"),
        },
        &TokenId::Near,
        &wnear(),
    );
    assert_eq!(passthrough, bd("0.01"));

    let too_small = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("0") },
        &TokenId::Near,
        &wnear(),
    );
    assert_eq!(too_small, BigDecimal::from_f64(0.0001).unwrap());

    let too_large = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("2") },
        &TokenId::Near,
        &wnear(),
    );
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
    );
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
    );
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
    );
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
    );
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
    );
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
    );
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.05).unwrap())
    );
}

#[test]
fn storage_deposits_add_up_every_deposit() {
    let execution_instructions = vec![
        ExecutionInstruction::NearTransaction {
            receiver_id: account("dclv2.ref-labs.near"),
            actions: vec![create_storage_deposit_action_for_contract(
                NearToken::from_millinear(500),
                true,
            )],
            is_fee_transaction: false,
        },
        ExecutionInstruction::NearTransaction {
            receiver_id: account("ft"),
            actions: vec![create_storage_deposit_action_for_contract(
                NearToken::from_micronear(1_250),
                true,
            )],
            is_fee_transaction: false,
        },
    ];
    assert_eq!(
        storage_deposits(&execution_instructions),
        NearToken::from_micronear(501_250)
    );
}

#[test]
fn fee_transactions_amount_adds_up_fee_transfers_only() {
    let execution_instructions = vec![
        ExecutionInstruction::NearTransaction {
            receiver_id: account("ft"),
            actions: vec![create_fee_ft_transfer_action(
                &account("trader.near"),
                1000,
                "app.near",
            )],
            is_fee_transaction: false,
        },
        ExecutionInstruction::NearTransaction {
            receiver_id: account("ft"),
            actions: vec![
                create_fee_ft_transfer_action(&account("ref.near"), 8, "app.near"),
                create_fee_ft_transfer_action(
                    &account("dex-aggregator.intear.near"),
                    2,
                    "app.near",
                ),
            ],
            is_fee_transaction: true,
        },
    ];
    assert_eq!(fee_transactions_amount(&execution_instructions), 10);
}

#[test]
fn estimated_gas_usage_excludes_swap_margin() {
    let call = |gas: NearGas| {
        Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "swap".to_string(),
            args: vec![],
            gas: Gas(gas),
            deposit: NearToken::from_yoctonear(1),
        }))
    };
    let transaction = |gas: NearGas| {
        vec![ExecutionInstruction::NearTransaction {
            receiver_id: account("v2.ref-finance.near"),
            actions: vec![call(gas)],
            is_fee_transaction: false,
        }]
    };
    let fees = TRANSACTION_FEE_GAS.as_gas()
        + FUNCTION_CALL_FEE_GAS.as_gas()
        + FUNCTION_CALL_BYTE_FEE_GAS.as_gas() * "swap".len() as u64;
    let cost = |gas: u64| pathfinder::MIN_GAS_PRICE.saturating_mul(gas.into());

    // 5.5 TGas predicted for a swap, attached as swap_call_gas does with 10 TGas reserved
    let attached = swap_call_gas(NearGas::from_ggas(5_500), NearGas::from_tgas(10), false).0;
    assert_eq!(
        estimated_gas_usage(&transaction(attached)),
        cost(fees + attached.as_gas() - NearGas::from_tgas(18).as_gas())
    );
    assert_eq!(
        max_gas_cost(&transaction(attached)),
        cost(fees + attached.as_gas())
    );
    // Less gas than the margin burns only fees
    assert_eq!(
        estimated_gas_usage(&transaction(NearGas::from_tgas(10))),
        cost(fees)
    );
}

#[test]
fn aggregator_gets_its_share_of_each_referral_fee() {
    let referral_fees: QuoteFees = r#"{"ref.near":0.01,"ref2.near":0.02,"zero.near":0}"#
        .parse()
        .unwrap();
    // 80% of each fee to its referrer, 20% of all of them together to the aggregator, and nothing
    // for the fee of 0
    assert_eq!(
        payable_referral_fees(&referral_fees).0,
        vec![
            ("ref.near".parse().unwrap(), 8_000),
            ("ref2.near".parse().unwrap(), 16_000),
            (AGGREGATOR_FEE_RECEIVER_ID.to_owned(), 6_000),
        ]
    );
}

#[test]
fn aggregator_share_adds_to_the_aggregator_as_a_referrer() {
    let referral_fees: QuoteFees = r#"{"ref.near":0.01,"dex-aggregator.intear.near":0.01}"#
        .parse()
        .unwrap();
    assert_eq!(
        payable_referral_fees(&referral_fees).0,
        // Referrers come sorted by account ID. The aggregator's 80% and the 20% of both fees
        vec![
            (AGGREGATOR_FEE_RECEIVER_ID.to_owned(), 12_000),
            ("ref.near".parse().unwrap(), 8_000),
        ]
    );
}

#[test]
fn fee_transfers_have_a_trading_fee_memo_naming_the_swap_referrer() {
    let args = |action: Action| match action {
        Action::FunctionCall(call) => {
            serde_json::from_slice::<serde_json::Value>(&call.args).unwrap()
        }
        _ => panic!("Not a function call"),
    };
    assert_eq!(
        args(create_fee_ft_transfer_action(
            &account("ref.near"),
            8,
            "app.near"
        )),
        serde_json::json!({
            "receiver_id": "ref.near",
            "amount": "8",
            "memo": "Trading fee (app.near)",
        })
    );
    assert_eq!(
        args(create_rhea_fee_mft_transfer_action(
            &account("ft"),
            &account("ref.near"),
            8,
            "app.near",
        )),
        serde_json::json!({
            "token_id": "ft",
            "receiver_id": "ref.near",
            "amount": "8",
            "memo": "Trading fee (app.near)",
        })
    );
}
