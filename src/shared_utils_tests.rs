use std::collections::HashMap;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use near_min_api::types::{AccountId, NearToken};
use num_traits::FromPrimitive;

use super::*;
use crate::calls::{CallBlueprint, IntearAsset, RheaOutput};
use crate::route::{resolve_steps, RouteStep};
use crate::types::{Slippage, TokenId};
use intear_dex_types::AssetId;

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

fn storage_deposit_amount() -> NearToken {
    NearToken::from_near(1)
}

/// The minimum storage of a token in [`TestNetworkView`]
fn token_min_storage() -> NearToken {
    storage_bytes(125)
}

fn call(call: CallBlueprint) -> RouteStep {
    RouteStep::Call(call)
}

fn wrap(amount: u128) -> RouteStep {
    call(CallBlueprint::WrapNear {
        amount: NearToken::from_yoctonear(amount),
    })
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

/// `NearToken` of `bytes` of storage
fn storage_bytes(bytes: u128) -> NearToken {
    STORAGE_BYTE_COST.saturating_mul(bytes)
}

/// The storage deposit of a registration on Rhea or Intear DEX, None if it has none
fn registration_deposit(calls: &[CallBlueprint]) -> Option<NearToken> {
    let is_registration = |call: &CallBlueprint| {
        matches!(
            call,
            CallBlueprint::RheaRegisterTokens { .. }
                | CallBlueprint::IntearDexRegisterAssets { .. }
        )
    };
    match calls {
        [registration] if is_registration(registration) => None,
        [storage_deposit, registration] if is_registration(registration) => {
            Some(storage_deposit.storage_deposit()).filter(|deposit| !deposit.is_zero())
        }
        _ => panic!("Not a registration: {calls:?}"),
    }
}

#[test]
fn deposit_storage_if_needed_registers_where_the_token_is() {
    assert_eq!(
        deposit_storage_if_needed(&ft(), Some(&trader())),
        vec![RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id: account("ft"),
            trader_account_id: trader(),
        }]
    );
    assert_eq!(
        deposit_storage_if_needed(&rhea_ft(), Some(&trader())),
        vec![RouteStep::RegisterOnRheaIfNeeded {
            trader_account_id: trader(),
            token_ids: vec![account("ft")],
        }]
    );
    assert_eq!(
        deposit_storage_if_needed(&intear_ft(), Some(&trader())),
        vec![RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id: trader(),
            asset_ids: vec![IntearAsset::Nep141(account("ft"))],
            fee_receivers: vec![],
        }]
    );
    assert_eq!(
        deposit_storage_if_needed(&TokenId::Near, Some(&trader())),
        vec![]
    );
    assert_eq!(deposit_storage_if_needed(&ft(), None), vec![]);
}

#[tokio::test]
async fn trader_storage_on_nep141_is_the_token_minimum_if_they_have_none() {
    let steps = deposit_storage_if_needed(&ft(), Some(&trader()));
    assert_eq!(
        resolve_steps(&TestNetworkView::default(), steps.clone()).await,
        Some(vec![CallBlueprint::Nep141StorageDeposit {
            token_id: account("ft"),
            account_id: None,
            amount: token_min_storage(),
        }])
    );
    let registered = TestNetworkView::default().with_storage(
        "ft",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(resolve_steps(&registered, steps).await, Some(vec![]));
}

#[tokio::test]
async fn rhea_registration_shape() {
    // A new account and one token
    assert_eq!(
        rhea_registration(&TestNetworkView::default(), &trader(), &[account("ft")]).await,
        Some(vec![
            CallBlueprint::RheaStorageDeposit {
                account_id: None,
                amount: storage_bytes(102 + 148),
            },
            CallBlueprint::RheaRegisterTokens {
                token_ids: vec![account("ft")],
            },
        ])
    );
}

/// Rhea's storage as get_user_storage_state reported it on mainnet: 102 bytes per account and
/// 148 per token, whatever the account ID
#[tokio::test]
async fn rhea_registration_pays_what_the_contract_counts() {
    let tokens = ["a.near", "b.near", "c.near", "d.near", "e.near"].map(account);
    let deposit = |network: &TestNetworkView, account_id: &str, token_count: usize| {
        let network = network.clone();
        let account_id = account(account_id);
        let tokens = tokens[..token_count].to_vec();
        async move {
            registration_deposit(
                &rhea_registration(&network, &account_id, &tokens)
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
    assert!(rhea_registration(&network, &trader(), &tokens[..1])
        .await
        .unwrap()
        .is_empty());
}

/// Intear DEX storage as storage_balance_of reported it on mainnet for accounts that registered
/// from scratch, and as storage_deposit takes it
#[tokio::test]
async fn intear_registration_pays_what_the_contract_charges() {
    let deposit = |network: TestNetworkView, account_id: &str, assets: Vec<IntearAsset>| {
        let account_id = account(account_id);
        async move {
            registration_deposit(
                &register_assets_on_intear(&network, &account_id, &assets, &[])
                    .await
                    .unwrap(),
            )
        }
    };
    let tear = IntearAsset::Nep141(account("token.intear.near"));

    // tx 6CziqdMLqww3akUuAQiXfainuBM46KAr1NNhgFJLnpE2 paid exactly this
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "starship9.near",
            vec![IntearAsset::Near, tear.clone()]
        )
        .await,
        Some(storage_bytes(537))
    );
    // tx 928t92DqXw4dQ3poDc1AsWGyyChdea8vsw3NpdKGC2AU
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "01b94be2d588a799498f89cb4eb5e54c965e7f6115b6768d4e154bf210a69139",
            vec![IntearAsset::Near, tear.clone()]
        )
        .await,
        Some(storage_bytes(837))
    );
    assert_eq!(
        deposit(
            TestNetworkView::default(),
            "criptomem.near",
            vec![
                IntearAsset::Nep141(account("usdt.tether-token.near")),
                IntearAsset::Nep141(account("qtc.omft.near"))
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
                IntearAsset::Near,
                IntearAsset::Nep141(account(
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
    let raen = IntearAsset::Nep141(account("raen-e1328c.launchpad.justhoot.near"));
    assert_eq!(
        deposit(
            TestNetworkView::default().with_intear_asset_untracked(&AssetId::from(&raen)),
            "bjirken.near",
            vec![raen]
        )
        .await,
        Some(storage_bytes(89 + 105 + 194 + 178))
    );

    // slimedragon.near's registration of a 64 character token took 227 bytes, its balances
    // existing already
    let long_asset = IntearAsset::Nep141(account(
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
            vec![IntearAsset::Near]
        )
        .await,
        None
    );
}

#[tokio::test]
async fn storage_lookups_that_fail_give_no_registration() {
    let network = TestNetworkView::default().with_storage_balance_bounds_error("dex.intear.near");
    assert_eq!(
        resolve_steps(
            &network,
            deposit_storage_if_needed(&intear_ft(), Some(&trader()))
        )
        .await,
        None
    );
    let network = TestNetworkView::default().with_storage_balance_bounds_error("ft");
    assert_eq!(
        resolve_steps(&network, deposit_storage_if_needed(&ft(), Some(&trader()))).await,
        None
    );
}

#[test]
fn wallet_inputs_need_no_registration() {
    // The trader holds a wallet-level input already
    assert_eq!(
        register_input_in_wallet(&ft(), account("ft"), Some(&trader())),
        vec![]
    );
    // Withdrawn from Rhea, it lands in their wallet
    assert_eq!(
        register_input_in_wallet(&rhea_ft(), account("ft"), Some(&trader())),
        vec![RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id: account("ft"),
            trader_account_id: trader(),
        }]
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
async fn storage_of_someone_else_on_nep141_is_the_token_minimum() {
    let network = TestNetworkView::default()
        .with_storage_balance_bounds("ft", "0.0025 NEAR".parse().unwrap());
    assert_eq!(
        resolve_steps(
            &network,
            vec![RouteStep::StorageDepositOnNep141IfNeeded {
                token_id: account("ft"),
                account_id: account("v2.ref-finance.near"),
            }]
        )
        .await,
        Some(vec![CallBlueprint::Nep141StorageDeposit {
            token_id: account("ft"),
            account_id: Some(account("v2.ref-finance.near")),
            amount: "0.0025 NEAR".parse().unwrap(),
        }])
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

#[test]
fn convert_to_nep141_near_wraps_when_amount_positive() {
    assert_eq!(
        convert_to_nep141(&TokenId::Near, None, 5),
        Some((vec![wrap(5)], account(WRAP_NEAR)))
    );
}

#[test]
fn convert_to_nep141_near_skips_wrap_when_amount_zero() {
    assert_eq!(
        convert_to_nep141(&TokenId::Near, None, 0),
        Some((vec![], account(WRAP_NEAR)))
    );
}

#[test]
fn convert_to_nep141_nep141_is_identity() {
    assert_eq!(
        convert_to_nep141(&ft(), None, 9),
        Some((vec![], account("ft")))
    );
}

#[test]
fn convert_to_nep141_rhea_withdraws_without_unwrap() {
    assert_eq!(
        convert_to_nep141(&rhea_ft(), None, 9),
        Some((
            vec![call(CallBlueprint::RheaWithdraw {
                token_id: account("ft"),
                amount: 9,
                output: RheaOutput::Nep141,
            })],
            account("ft")
        ))
    );
}

#[test]
fn convert_to_nep141_rhea_amount_zero_has_no_steps() {
    // Rhea would withdraw the whole balance for 0
    assert_eq!(
        convert_to_nep141(&rhea_ft(), None, 0),
        Some((vec![], account("ft")))
    );
}

#[test]
fn convert_to_nep141_intear_near_withdraws_and_wraps() {
    assert_eq!(
        convert_to_nep141(&intear_near(), None, 4),
        Some((
            vec![
                call(CallBlueprint::IntearDexWithdraw {
                    asset: IntearAsset::Near,
                    amount: 4,
                    trader_account_id: None,
                }),
                wrap(4),
            ],
            account(WRAP_NEAR)
        ))
    );
}

#[test]
fn convert_to_nep141_intear_near_amount_zero_has_no_steps() {
    assert_eq!(
        convert_to_nep141(&intear_near(), None, 0),
        Some((vec![], account(WRAP_NEAR)))
    );
}

#[test]
fn convert_to_nep141_intear_nep141_withdraws_only() {
    assert_eq!(
        convert_to_nep141(&intear_ft(), Some(&trader()), 8),
        Some((
            vec![call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::Nep141(account("ft")),
                amount: 8,
                trader_account_id: Some(trader()),
            })],
            account("ft")
        ))
    );
}

#[test]
fn convert_to_nep141_unsupported_assets_return_none() {
    assert!(convert_to_nep141(&intear_nep245(), None, 1).is_none());
    assert!(convert_to_nep141(&intear_nep171(), None, 1).is_none());
}

#[test]
fn convert_to_native_near_is_empty() {
    assert_eq!(
        convert_to_native(&TokenId::Near, None, NearToken::from_near(1)),
        Some(vec![])
    );
}

#[test]
fn convert_to_native_wrap_near_unwraps() {
    let amount = NearToken::from_near(1);
    assert_eq!(
        convert_to_native(&wnear(), Some(&trader()), amount),
        Some(vec![call(CallBlueprint::UnwrapNear {
            amount,
            trader_account_id: Some(trader()),
        })])
    );
}

#[test]
fn convert_to_native_wrap_near_zero_returns_none() {
    assert!(convert_to_native(&wnear(), None, NearToken::ZERO).is_none());
}

#[test]
fn convert_to_native_non_wrap_nep141_returns_none() {
    assert!(convert_to_native(&ft(), None, NearToken::from_near(1)).is_none());
}

#[test]
fn convert_to_native_rhea_wrap_withdraws_with_unwrap() {
    let amount = NearToken::from_near(1);
    assert_eq!(
        convert_to_native(&rhea_wnear(), None, amount),
        Some(vec![call(CallBlueprint::RheaWithdraw {
            token_id: account(WRAP_NEAR),
            amount: amount.as_yoctonear(),
            output: RheaOutput::Near {
                trader_account_id: None,
            },
        })])
    );
}

#[test]
fn convert_to_native_rhea_wrap_zero_returns_none() {
    assert!(convert_to_native(&rhea_wnear(), None, NearToken::ZERO).is_none());
}

#[test]
fn convert_to_native_rhea_non_wrap_returns_none() {
    assert!(convert_to_native(&rhea_ft(), None, NearToken::from_near(1)).is_none());
}

#[test]
fn convert_to_native_intear_near_withdraws_only() {
    let amount = NearToken::from_near(1);
    assert_eq!(
        convert_to_native(&intear_near(), None, amount),
        Some(vec![call(CallBlueprint::IntearDexWithdraw {
            asset: IntearAsset::Near,
            amount: amount.as_yoctonear(),
            trader_account_id: None,
        })])
    );
}

#[test]
fn convert_to_native_intear_wrap_withdraws_and_unwraps() {
    let amount = NearToken::from_near(1);
    assert_eq!(
        convert_to_native(&intear_wnear(), None, amount),
        Some(vec![
            call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::Nep141(account(WRAP_NEAR)),
                amount: amount.as_yoctonear(),
                trader_account_id: None,
            }),
            call(CallBlueprint::UnwrapNear {
                amount,
                trader_account_id: None,
            }),
        ])
    );
}

#[test]
fn convert_to_native_intear_unsupported_or_zero_returns_none() {
    let amount = NearToken::from_near(1);
    assert!(convert_to_native(&intear_ft(), None, amount).is_none());
    assert!(convert_to_native(&intear_nep245(), None, amount).is_none());
    assert!(convert_to_native(&intear_nep171(), None, amount).is_none());
    assert!(convert_to_native(&intear_near(), None, NearToken::ZERO).is_none());
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
        resolve_steps(
            &rhea,
            deposit_storage_if_needed(&rhea_ft(), Some(&trader()))
        )
        .await,
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
        resolve_steps(
            &intear,
            deposit_storage_if_needed(&intear_ft(), Some(&trader()))
        )
        .await,
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
            &resolve_steps(
                &unregistered,
                deposit_storage_if_needed(&rhea_ft(), Some(&trader()))
            )
            .await
            .unwrap()
        ),
        None
    );
}

#[tokio::test]
async fn storage_of_a_contract_on_a_token_only_when_it_has_none() {
    let step = RouteStep::StorageDepositOnNep141IfNeeded {
        token_id: account("ft"),
        account_id: account("v2.ref-finance.near"),
    };
    let registered = TestNetworkView::default().with_storage(
        "ft",
        "v2.ref-finance.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        resolve_steps(&registered, vec![step.clone()]).await,
        Some(vec![])
    );
    assert_eq!(
        resolve_steps(&TestNetworkView::default(), vec![step]).await,
        Some(vec![CallBlueprint::Nep141StorageDeposit {
            token_id: account("ft"),
            account_id: Some(account("v2.ref-finance.near")),
            amount: token_min_storage(),
        }])
    );
}

#[tokio::test]
async fn trader_storage_on_rhea_dcl_is_the_contract_minimum_if_they_have_none() {
    let step = RouteStep::TraderStorageDepositOnRheaDclIfNeeded {
        trader_account_id: trader(),
    };
    let already = TestNetworkView::default().with_storage(
        "dclv2.ref-labs.near",
        "trader.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        resolve_steps(&already, vec![step.clone()]).await,
        Some(vec![])
    );

    // dclv2.ref-labs.near's storage_balance_bounds min on mainnet
    assert_eq!(
        resolve_steps(&TestNetworkView::default(), vec![step]).await,
        Some(vec![CallBlueprint::RheaDclStorageDeposit {
            amount: NearToken::from_millinear(500),
        }])
    );
}

#[test]
fn convert_to_same_token_is_empty() {
    assert!(convert_to(&ft(), &ft(), 1, None).is_empty());
}

#[test]
fn convert_to_unsupported_assets_is_empty() {
    assert!(convert_to(&intear_nep245(), &TokenId::Near, 1, None).is_empty());
    assert!(convert_to(&TokenId::Near, &intear_nep171(), 1, None).is_empty());
}

#[test]
fn convert_to_near_and_wrap_near() {
    assert_eq!(convert_to(&TokenId::Near, &wnear(), 5, None), vec![wrap(5)]);
    assert_eq!(
        convert_to(&wnear(), &TokenId::Near, 5, None),
        vec![call(CallBlueprint::UnwrapNear {
            amount: NearToken::from_yoctonear(5),
            trader_account_id: None,
        })]
    );
    assert!(convert_to(&TokenId::Near, &wnear(), 0, None).is_empty());
    assert!(convert_to(&wnear(), &TokenId::Near, 0, None).is_empty());
}

#[test]
fn convert_to_rhea_wrap_to_near_withdraws_with_unwrap() {
    assert_eq!(
        convert_to(&rhea_wnear(), &TokenId::Near, 5, None),
        vec![call(CallBlueprint::RheaWithdraw {
            token_id: account(WRAP_NEAR),
            amount: 5,
            output: RheaOutput::Near {
                trader_account_id: None,
            },
        })]
    );
}

#[test]
fn convert_to_rhea_wrap_to_nep141_wrap_withdraws_without_unwrap() {
    assert_eq!(
        convert_to(&rhea_wnear(), &wnear(), 5, None),
        vec![call(CallBlueprint::RheaWithdraw {
            token_id: account(WRAP_NEAR),
            amount: 5,
            output: RheaOutput::Nep141,
        })]
    );
}

#[test]
fn convert_to_intear_near_to_wrap_near_withdraws_and_wraps() {
    assert_eq!(
        convert_to(&intear_near(), &wnear(), 5, None),
        vec![
            call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::Near,
                amount: 5,
                trader_account_id: None,
            }),
            wrap(5),
        ]
    );
}

#[tokio::test]
async fn convert_to_rhea_destination_without_trader_registers_contract_then_deposits() {
    let steps = convert_to(&TokenId::Near, &rhea_wnear(), 5, None);
    let deposit = call(CallBlueprint::Nep141DepositOnRhea {
        token_id: account(WRAP_NEAR),
        amount: 5,
    });
    assert_eq!(
        steps,
        vec![
            RouteStep::StorageDepositOnNep141IfNeeded {
                token_id: account(WRAP_NEAR),
                account_id: account("v2.ref-finance.near"),
            },
            wrap(5),
            deposit.clone(),
        ]
    );

    // Rhea has storage on wNEAR already
    let network = TestNetworkView::default().with_storage(
        WRAP_NEAR,
        "v2.ref-finance.near",
        storage_deposit_amount(),
        storage_deposit_amount(),
    );
    assert_eq!(
        resolve_steps(&network, steps).await,
        Some(vec![
            CallBlueprint::WrapNear {
                amount: NearToken::from_yoctonear(5)
            },
            CallBlueprint::Nep141DepositOnRhea {
                token_id: account(WRAP_NEAR),
                amount: 5,
            },
        ])
    );
}

#[tokio::test]
async fn convert_to_rhea_destination_with_trader_includes_rhea_and_ft_storage() {
    let steps = convert_to(&ft(), &rhea_ft(), 5, Some(&trader()));
    assert_eq!(
        resolve_steps(&TestNetworkView::default(), steps).await,
        Some(vec![
            // A new account with one token
            CallBlueprint::RheaStorageDeposit {
                account_id: None,
                amount: storage_bytes(102 + 148),
            },
            CallBlueprint::RheaRegisterTokens {
                token_ids: vec![account("ft")],
            },
            // The trader holds ft already, so only Rhea is registered on it
            CallBlueprint::Nep141StorageDeposit {
                token_id: account("ft"),
                account_id: Some(account("v2.ref-finance.near")),
                amount: token_min_storage(),
            },
            CallBlueprint::Nep141DepositOnRhea {
                token_id: account("ft"),
                amount: 5,
            },
        ])
    );
}

#[tokio::test]
async fn convert_to_intear_near_destination_uses_deposit_near() {
    let deposit_near = call(CallBlueprint::IntearDexDepositNear {
        amount: NearToken::from_yoctonear(5),
        operations: None,
    });
    assert_eq!(
        convert_to(&TokenId::Near, &intear_near(), 5, None),
        vec![deposit_near.clone()]
    );

    let steps = convert_to(&TokenId::Near, &intear_near(), 5, Some(&trader()));
    assert_eq!(
        steps,
        vec![
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![IntearAsset::Near],
                fee_receivers: vec![],
            },
            deposit_near,
        ]
    );
    let calls = resolve_steps(&TestNetworkView::default(), steps)
        .await
        .unwrap();
    assert!(
        matches!(
            calls.as_slice(),
            [
                CallBlueprint::IntearDexStorageDeposit { .. },
                CallBlueprint::IntearDexRegisterAssets { .. },
                CallBlueprint::IntearDexDepositNear { .. },
            ]
        ),
        "{calls:?}"
    );
}

#[test]
fn convert_to_step_order_is_register_withdraw_convert_deposit() {
    assert_eq!(
        convert_to(&rhea_wnear(), &intear_wnear(), 5, Some(&trader())),
        vec![
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![IntearAsset::Nep141(account(WRAP_NEAR))],
                fee_receivers: vec![],
            },
            RouteStep::TraderStorageDepositOnNep141IfNeeded {
                token_id: account(WRAP_NEAR),
                trader_account_id: trader(),
            },
            RouteStep::StorageDepositOnNep141IfNeeded {
                token_id: account(WRAP_NEAR),
                account_id: account("dex.intear.near"),
            },
            call(CallBlueprint::RheaWithdraw {
                token_id: account(WRAP_NEAR),
                amount: 5,
                output: RheaOutput::Nep141,
            }),
            call(CallBlueprint::Nep141DepositOnIntearDex {
                token_id: account(WRAP_NEAR),
                amount: 5,
            }),
        ]
    );
}

#[test]
fn convert_to_registers_the_trader_on_a_withdrawn_token() {
    // The withdrawn wNEAR goes to the trader's wallet before it's unwrapped
    assert_eq!(
        convert_to(&intear_wnear(), &TokenId::Near, 5, Some(&trader())),
        vec![
            RouteStep::TraderStorageDepositOnNep141IfNeeded {
                token_id: account(WRAP_NEAR),
                trader_account_id: trader(),
            },
            call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::Nep141(account(WRAP_NEAR)),
                amount: 5,
                trader_account_id: Some(trader()),
            }),
            call(CallBlueprint::UnwrapNear {
                amount: NearToken::from_yoctonear(5),
                trader_account_id: Some(trader()),
            }),
        ]
    );
}

#[test]
fn convert_to_registers_the_trader_on_wrapped_near() {
    // Otherwise wrap.near registers them out of the deposit, and they get less than the amount
    assert_eq!(
        convert_to(&TokenId::Near, &wnear(), 5, Some(&trader())),
        vec![
            RouteStep::TraderStorageDepositOnNep141IfNeeded {
                token_id: account(WRAP_NEAR),
                trader_account_id: trader(),
            },
            wrap(5),
        ]
    );
}

#[test]
fn convert_to_doesnt_register_the_trader_on_their_wallet_token() {
    // Whoever put the token in the wallet registered the trader on it
    assert_eq!(
        convert_to(&ft(), &intear_ft(), 5, Some(&trader())),
        vec![
            RouteStep::RegisterOnIntearDexIfNeeded {
                trader_account_id: trader(),
                asset_ids: vec![IntearAsset::Nep141(account("ft"))],
                fee_receivers: vec![],
            },
            RouteStep::StorageDepositOnNep141IfNeeded {
                token_id: account("ft"),
                account_id: account("dex.intear.near"),
            },
            call(CallBlueprint::Nep141DepositOnIntearDex {
                token_id: account("ft"),
                amount: 5,
            }),
        ]
    );
}

#[test]
fn fees_are_paid_after_the_swap_to_referrers_registered_first() {
    let fees = [
        ReferrerFee {
            referrer_id: account("ref.near"),
            amount: 8,
        },
        ReferrerFee {
            referrer_id: account("app.near"),
            amount: 2,
        },
    ];
    let (to_nep141, fee_steps, nep141) = convert_to_nep141_with_fees(
        &TokenId::Near,
        Some(&trader()),
        110,
        &fees,
        &account("app.near"),
    )
    .unwrap();
    assert_eq!(nep141, account(WRAP_NEAR));
    // NEAR is wrapped whole, with the fees
    assert_eq!(to_nep141, vec![wrap(110)]);
    let fee_transfer = |receiver_id: &str, amount| {
        call(CallBlueprint::InputNep141FeeTransfer {
            token_id: account(WRAP_NEAR),
            receiver_id: account(receiver_id),
            amount,
            swap_referrer_id: account("app.near"),
        })
    };
    let storage = |account_id: &str| RouteStep::StorageDepositOnNep141IfNeeded {
        token_id: account(WRAP_NEAR),
        account_id: account(account_id),
    };
    assert_eq!(
        fee_steps,
        vec![
            storage("ref.near"),
            storage("app.near"),
            fee_transfer("ref.near", 8),
            fee_transfer("app.near", 2),
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
        &BaseTokenId::Near,
        &base_token(&wnear()).unwrap(),
    );
    assert_eq!(passthrough, bd("0.01"));

    let too_small = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("0") },
        &BaseTokenId::Near,
        &base_token(&wnear()).unwrap(),
    );
    assert_eq!(too_small, BigDecimal::from_f64(0.0001).unwrap());

    let too_large = get_slippage(
        &network,
        Slippage::Fixed { slippage: bd("2") },
        &BaseTokenId::Near,
        &base_token(&wnear()).unwrap(),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
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
        BaseTokenId::Near,
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
        BaseTokenId::Nep141(account("ft")),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
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
        BaseTokenId::Near,
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
        BaseTokenId::Nep141(account("ft")),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
    );
    assert_eq!(slippage, bd("0.51"));
}

#[tokio::test]
async fn get_slippage_auto_somewhat_new_token_uses_scale_0_6() {
    let mut tokens = HashMap::new();
    tokens.insert(
        BaseTokenId::Near,
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
        BaseTokenId::Nep141(account("ft")),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
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
        BaseTokenId::Near,
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
        base_token(&ft()).unwrap(),
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
        &BaseTokenId::Near,
        &base_token(&ft()).unwrap(),
    );
    assert_eq!(
        slippage,
        expected_auto_slippage("0.01", "0.51", BigDecimal::from_f64(0.05).unwrap())
    );
}
