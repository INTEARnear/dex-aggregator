use std::{collections::HashMap, str::FromStr, time::Duration};

use bigdecimal::BigDecimal;
use cached::proc_macro::cached;
use lazy_static::lazy_static;
use near_min_api::{
    types::{
        AccountId, AccountIdRef, Action, Balance, BlockHeight, BlockReference, Finality,
        FunctionCallAction, Gas, NearGas, NearToken,
    },
    utils::dec_format,
    QueryFinality, RpcClient,
};
use num_traits::{FromPrimitive, Zero};
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Deserializer};
use tokio::sync::Mutex;

use crate::{
    providers::intear_plach::AssetId,
    types::{ExecutionInstruction, Slippage, TokenId},
};

pub const WRAP_NEAR: &str = "wrap.near";
pub const DEFAULT_REFERRER_ID: &str = "dex-aggregator.intear.near";

pub fn create_wrap_action(amount: NearToken) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "near_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
        gas: Gas(NearGas::from_tgas(2)),
        deposit: amount,
    }))
}

pub fn create_unwrap_action(amount: NearToken) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "near_withdraw".to_string(),
        args: serde_json::to_string(&serde_json::json!({
            "amount": amount,
        }))
        .unwrap()
        .as_bytes()
        .to_vec(),
        gas: Gas(NearGas::from_tgas(5)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub fn create_rhea_withdraw_action(
    token_id: &AccountId,
    amount: Balance,
    unwrap_near: bool,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "withdraw".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "token_id": token_id,
            "amount": amount.to_string(),
            "skip_unwrap_near": !unwrap_near,
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(50)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub fn create_intear_dex_withdraw_action(asset_id: &AssetId, amount: Balance) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "withdraw".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "asset_id": asset_id,
            "amount": {
                "Exact": amount.to_string(),
            },
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(50)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub fn create_rhea_nep141_deposit_action(contract_id: &AccountId, amount: Balance) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": contract_id,
            "amount": amount.to_string(),
            "msg": "",
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(50)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub fn create_intear_nep141_deposit_action(contract_id: &AccountId, amount: Balance) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer_call".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": contract_id,
            "amount": amount.to_string(),
            "msg": "",
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(50)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

async fn create_ft_deposit_registrations(
    contract_id: &AccountId,
    token_id: &AccountId,
    trader_account_id: Option<AccountId>,
) -> Vec<ExecutionInstruction> {
    let mut actions = vec![];
    if let Some(trader_account_id) = trader_account_id {
        if needs_storage_deposit_for_contract(&trader_account_id, token_id).await {
            actions.push(create_storage_deposit_action_for_contract(
                "0.00125 NEAR".parse().unwrap(),
            ));
        }
    }
    if needs_storage_deposit_for_contract(contract_id, token_id).await {
        actions.push(create_storage_deposit_action_for_someone(
            "0.00125 NEAR".parse().unwrap(),
            contract_id,
        ));
    }
    if actions.is_empty() {
        vec![]
    } else {
        vec![ExecutionInstruction::NearTransaction {
            receiver_id: token_id.clone(),
            actions,
        }]
    }
}

pub async fn needs_storage_deposit(account_id: &AccountId, token_id: &TokenId) -> bool {
    match token_id {
        TokenId::Near => false,
        TokenId::Nep141(token_id) => needs_storage_deposit_for_contract(account_id, token_id).await,
        TokenId::Nep141OnRhea(token_id) => {
            let is_registered = RPC_CLIENT
                .call::<bool>(
                    "v2.ref-finance.near".parse().unwrap(),
                    "token_register_of",
                    serde_json::json!({
                        "account_id": account_id,
                        "token_id": token_id,
                    }),
                    QueryFinality::Finality(Finality::DoomSlug),
                )
                .await
                .unwrap_or_default();
            let has_storage_deposit = RPC_CLIENT
                .call::<StorageDeposit>(
                    "v2.ref-finance.near".parse().unwrap(),
                    "storage_balance_of",
                    serde_json::json!({
                        "account_id": account_id,
                    }),
                    QueryFinality::Finality(Finality::DoomSlug),
                )
                .await
                .map(|s| s.available > NearToken::from_millinear(10))
                .unwrap_or_default();
            !has_storage_deposit || !is_registered
        }
        TokenId::TokenOnIntearDex(asset_id) => {
            let is_registered = RPC_CLIENT
                .call::<bool>(
                    "dex.intear.near".parse().unwrap(),
                    "are_assets_registered",
                    serde_json::json!({
                        "asset_ids": [asset_id],
                        "for": {
                            "Account": account_id,
                        }
                    }),
                    QueryFinality::Finality(Finality::DoomSlug),
                )
                .await
                .unwrap_or_default();
            let has_storage_deposit = RPC_CLIENT
                .call::<StorageDeposit>(
                    "dex.intear.near".parse().unwrap(),
                    "storage_balance_of",
                    serde_json::json!({
                        "account_id": account_id,
                    }),
                    QueryFinality::Finality(Finality::DoomSlug),
                )
                .await
                .map(|s| s.available > NearToken::from_millinear(1))
                .unwrap_or_default();
            !has_storage_deposit || !is_registered
        }
    }
}

pub async fn needs_storage_deposit_for_contract(
    account_id: &AccountId,
    contract_id: &AccountIdRef,
) -> bool {
    let Ok(storage_deposit) = RPC_CLIENT
        .call::<StorageDeposit>(
            contract_id.to_owned(),
            "storage_balance_of",
            serde_json::json!({
                "account_id": account_id,
            }),
            QueryFinality::Finality(Finality::DoomSlug),
        )
        .await
    else {
        return true;
    };
    storage_deposit.total.is_zero()
}

#[derive(Debug, Deserialize)]
struct StorageDeposit {
    #[allow(dead_code)]
    available: NearToken,
    total: NearToken,
}

pub async fn create_storage_deposit_action(token_id: &TokenId) -> Vec<ExecutionInstruction> {
    match token_id {
        TokenId::Nep141(token_account_id) => vec![ExecutionInstruction::NearTransaction {
            receiver_id: token_account_id.clone(),
            actions: vec![create_storage_deposit_action_for_contract(
                "0.00125 NEAR".parse().unwrap(),
            )],
        }],
        TokenId::Near => panic!("NEAR doesn't need a storage deposit"),
        TokenId::Nep141OnRhea(token_id) => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![
                    create_storage_deposit_action_for_contract(NearToken::from_millinear(10)),
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "register_tokens".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "token_ids": [token_id],
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(1),
                    })),
                ],
            }]
        }
        TokenId::TokenOnIntearDex(asset_id) => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "dex.intear.near".parse().unwrap(),
                actions: vec![
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "storage_deposit".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: "0.005 NEAR".parse().unwrap(),
                    })),
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "register_assets".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "asset_ids": [asset_id],
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(10)),
                        deposit: NearToken::from_yoctonear(1),
                    })),
                ],
            }]
        }
    }
}

pub fn create_storage_deposit_action_for_contract(amount: NearToken) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "storage_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "registration_only": true,
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: amount,
    }))
}

pub fn create_storage_deposit_action_for_someone(
    amount: NearToken,
    account_id: &AccountId,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "storage_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "registration_only": true,
            "account_id": account_id,
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: amount,
    }))
}

lazy_static! {
    pub static ref REQWEST_CLIENT: Client = ClientBuilder::new()
        .timeout(Duration::from_secs(65)) // max 60 seconds + latency
        .user_agent("Intear Swap Router")
        .build()
        .unwrap();
    pub static ref RPC_CLIENT: RpcClient = RpcClient::new(
        std::env::var("RPC_URLS")
            .unwrap_or_else(|_| {
                "https://rpc.intea.rs,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                    .to_string()
            })
            .split(',')
            .map(|url| url.to_string())
            .collect::<Vec<_>>(),
    );
    pub static ref TOKEN_PRICES: Mutex<HashMap<AccountId, BigDecimal>> = {
        let prices = Mutex::new(HashMap::new());
        tokio::spawn(update_token_prices_loop());
        prices
    };
}

pub async fn get_slippage(
    slippage: Slippage,
    token_in: &TokenId,
    token_out: &TokenId,
) -> BigDecimal {
    match slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => {
            let get_token_volatiltiy = |token_info: TokenInfo| async move {
                let current_block_height = match get_current_block_height().await {
                    Ok(height) => height,
                    Err(_) => return BigDecimal::from_f64(0.005).unwrap(),
                };
                if token_info.created_at > current_block_height.saturating_sub(100) {
                    // New token
                    BigDecimal::from(1)
                } else if token_info.created_at > current_block_height.saturating_sub(1000) {
                    // New token, but not that new
                    BigDecimal::from_f64(0.6).unwrap()
                } else {
                    let mut scale = BigDecimal::from(0);

                    if !token_info.price_usd_raw.is_zero() {
                        let price_change_24h = (token_info.price_usd_raw_24h_ago
                            - token_info.price_usd_raw.clone())
                        .abs();
                        let price_change_24h_relative =
                            price_change_24h / token_info.price_usd_raw.clone();

                        if price_change_24h_relative > BigDecimal::from_f64(0.5).unwrap() {
                            scale += BigDecimal::from_f64(0.1).unwrap();
                        }
                        if price_change_24h_relative > BigDecimal::from_f64(0.2).unwrap() {
                            scale += BigDecimal::from_f64(0.05).unwrap();
                        }

                        let market_cap = BigDecimal::from(token_info.circulating_supply)
                            * token_info.price_usd_raw;
                        if !market_cap.is_zero() {
                            let volume_to_mcap_ratio =
                                token_info.volume_usd_24h.clone() / market_cap;
                            if volume_to_mcap_ratio > 1 {
                                scale += BigDecimal::from_f64(0.1).unwrap();
                            }
                            if volume_to_mcap_ratio > BigDecimal::from_f64(0.2).unwrap() {
                                scale += BigDecimal::from_f64(0.05).unwrap();
                            }
                        }
                    }

                    if !token_info.liquidity_usd.is_zero() {
                        let volume_to_liquidity_ratio =
                            token_info.volume_usd_24h / token_info.liquidity_usd;
                        if volume_to_liquidity_ratio > 1 {
                            scale += BigDecimal::from_f64(0.15).unwrap();
                        }
                        if volume_to_liquidity_ratio > BigDecimal::from_f64(0.5).unwrap() {
                            scale += BigDecimal::from_f64(0.1).unwrap();
                        }
                        if volume_to_liquidity_ratio > BigDecimal::from_f64(0.2).unwrap() {
                            scale += BigDecimal::from_f64(0.05).unwrap();
                        }
                    }

                    scale.clamp(BigDecimal::from(0), BigDecimal::from(1))
                }
            };

            let optimal_slippage_scale_input = if let Ok(mut tokens) = get_all_tokens().await {
                if let Some(token_info) = tokens.remove(token_in) {
                    get_token_volatiltiy(token_info).await
                } else {
                    // Maybe it's a new token, but not 100% sure
                    BigDecimal::from_f64(0.8).unwrap()
                }
            } else {
                // An error occurred
                BigDecimal::from_f64(0.005).unwrap()
            };
            let optimal_slippage_scale_output = if let Ok(mut tokens) = get_all_tokens().await {
                if let Some(token_info) = tokens.remove(token_out) {
                    get_token_volatiltiy(token_info).await
                } else {
                    // Maybe it's a new token, but not 100% sure
                    BigDecimal::from_f64(0.8).unwrap()
                }
            } else {
                // An error occurred
                BigDecimal::from_f64(0.005).unwrap()
            };
            let optimal_slippage_scale =
                optimal_slippage_scale_input.max(optimal_slippage_scale_output);
            let optimal_slippage = min_slippage.clone()
                + (max_slippage.clone() - min_slippage.clone()) * optimal_slippage_scale;
            let optimal_slippage = optimal_slippage.clamp(min_slippage, max_slippage);

            optimal_slippage.clamp(
                BigDecimal::from_f64(0.0001).unwrap(),
                BigDecimal::from_f64(0.9999).unwrap(),
            )
        }
        Slippage::Fixed { slippage } => slippage.clamp(
            BigDecimal::from_f64(0.0001).unwrap(),
            BigDecimal::from_f64(0.9999).unwrap(),
        ),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TokenInfo {
    #[serde(deserialize_with = "deserialize_bigdecimal")]
    pub price_usd_raw: BigDecimal,
    #[serde(deserialize_with = "deserialize_bigdecimal")]
    pub price_usd_raw_24h_ago: BigDecimal,
    #[serde(with = "dec_format")]
    pub circulating_supply: Balance,
    #[serde(deserialize_with = "deserialize_bigdecimal")]
    pub liquidity_usd: BigDecimal,
    #[serde(deserialize_with = "deserialize_bigdecimal")]
    pub volume_usd_24h: BigDecimal,
    pub created_at: BlockHeight,
}

fn deserialize_bigdecimal<'de, D>(deserializer: D) -> Result<BigDecimal, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => BigDecimal::from_str(&s).map_err(serde::de::Error::custom),
        serde_json::Value::Number(n) => {
            BigDecimal::from_str(&n.to_string()).map_err(serde::de::Error::custom)
        }
        _ => Err(serde::de::Error::custom("expected number or string")),
    }
}

#[derive(Debug, Deserialize)]
struct TokenData {
    price_usd_raw: String,
}

#[cached(time = 5, result = true)]
pub async fn get_all_tokens() -> Result<HashMap<TokenId, TokenInfo>, String> {
    let endpoint = std::env::var("INTEAR_PRICES_API_ENDPOINT")
        .unwrap_or_else(|_| "https://prices.intear.tech".to_string());
    let url = format!("{endpoint}/tokens");
    let Ok(response) = REQWEST_CLIENT.get(url).send().await else {
        return Err("Failed to get all tokens".to_string());
    };
    let Ok(mut tokens) = response.json::<HashMap<TokenId, TokenInfo>>().await else {
        return Err("Failed to parse all tokens".to_string());
    };
    if let Some(wnear_info) = tokens.get(&TokenId::Nep141(WRAP_NEAR.parse().unwrap())) {
        tokens.insert(TokenId::Near, wnear_info.clone());
    }
    Ok(tokens)
}

#[cached(time = 1, result = true)]
pub async fn get_current_block_height() -> Result<u64, String> {
    let Ok(response) = RPC_CLIENT
        .block(BlockReference::Finality(Finality::None))
        .await
    else {
        return Err("Failed to get current block".to_string());
    };
    Ok(response.header.height)
}

async fn update_token_prices() {
    let endpoint = std::env::var("INTEAR_PRICES_API_ENDPOINT")
        .unwrap_or_else(|_| "https://prices.intear.tech".to_string());
    let url = format!("{endpoint}/tokens");

    if let Ok(response) = REQWEST_CLIENT.get(url).send().await {
        if let Ok(tokens) = response.json::<HashMap<String, TokenData>>().await {
            let mut token_prices = TOKEN_PRICES.lock().await;
            token_prices.clear();

            for (token_id_str, token_data) in tokens {
                if let Ok(account_id) = token_id_str.parse::<AccountId>() {
                    if let Ok(price_raw) = token_data.price_usd_raw.parse::<BigDecimal>() {
                        token_prices.insert(account_id, price_raw / BigDecimal::from(1_000_000));
                    }
                }
            }
        }
    }
}

async fn update_token_prices_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        update_token_prices().await;
    }
}

#[allow(dead_code)]
pub async fn get_token_price(token_id: &AccountId) -> Option<BigDecimal> {
    let token_prices = TOKEN_PRICES.lock().await;
    token_prices.get(token_id).cloned()
}

/// Merge all neighboring NearTransactions with the same receiver_id
pub fn optimize_execution_instructions(
    execution_instructions: Vec<ExecutionInstruction>,
) -> Vec<ExecutionInstruction> {
    let mut optimized_execution_instructions = vec![];
    for next_instruction in execution_instructions {
        match &next_instruction {
            ExecutionInstruction::NearTransaction {
                receiver_id,
                actions,
            } => {
                if actions.is_empty() {
                    continue;
                }
                if let Some(ExecutionInstruction::NearTransaction {
                    receiver_id: prev_receiver_id,
                    actions: prev_actions,
                }) = optimized_execution_instructions.last_mut()
                {
                    if prev_receiver_id == receiver_id {
                        prev_actions.extend(actions.iter().cloned());
                    } else {
                        optimized_execution_instructions.push(next_instruction);
                    }
                } else {
                    optimized_execution_instructions.push(next_instruction);
                }
            }
        }
    }
    optimized_execution_instructions
}

pub async fn convert_to_nep141(
    token_id: &TokenId,
    _trader_account_id: Option<AccountId>,
    amount: Balance,
) -> Option<(Vec<ExecutionInstruction>, AccountId)> {
    match token_id {
        TokenId::Near => {
            let mut transactions = vec![];
            if amount > 0 {
                transactions.push(ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                    actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
                });
            }
            Some((transactions, WRAP_NEAR.parse::<AccountId>().unwrap()))
        }
        TokenId::Nep141(token_id) => Some((vec![], token_id.clone())),
        TokenId::Nep141OnRhea(token_id) => Some((
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![create_rhea_withdraw_action(token_id, amount, false)],
            }],
            token_id.clone(),
        )),
        TokenId::TokenOnIntearDex(asset_id) => {
            let withdraw = if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: "dex.intear.near".parse().unwrap(),
                    actions: vec![create_intear_dex_withdraw_action(asset_id, amount)],
                }]
            } else {
                vec![]
            };
            match asset_id {
                AssetId::Near => {
                    let wrap = if amount > 0 {
                        vec![ExecutionInstruction::NearTransaction {
                            receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                            actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
                        }]
                    } else {
                        vec![]
                    };
                    Some((
                        [withdraw, wrap].concat(),
                        WRAP_NEAR.parse::<AccountId>().unwrap(),
                    ))
                }
                AssetId::Nep141(token_id) => Some((withdraw, token_id.clone())),
                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => None,
            }
        }
    }
}

pub async fn convert_to_native(
    token_id: &TokenId,
    _trader_account_id: Option<AccountId>,
    amount: NearToken,
) -> Option<Vec<ExecutionInstruction>> {
    match token_id {
        TokenId::Near => Some(vec![]),
        TokenId::Nep141(account_id) if account_id == WRAP_NEAR => {
            if !amount.is_zero() {
                Some(vec![ExecutionInstruction::NearTransaction {
                    receiver_id: account_id.clone(),
                    actions: vec![create_unwrap_action(amount)],
                }])
            } else {
                None
            }
        }
        TokenId::Nep141(_non_wrap_near) => None,
        TokenId::Nep141OnRhea(account_id) if account_id == WRAP_NEAR => {
            if amount.is_zero() {
                return None;
            }
            Some(vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![create_rhea_withdraw_action(
                    account_id,
                    amount.as_yoctonear(),
                    true,
                )],
            }])
        }
        TokenId::Nep141OnRhea(_non_wrap_near) => None,
        TokenId::TokenOnIntearDex(asset_id) => {
            if amount.is_zero() {
                return None;
            }
            let withdraw = ExecutionInstruction::NearTransaction {
                receiver_id: "dex.intear.near".parse().unwrap(),
                actions: vec![create_intear_dex_withdraw_action(
                    asset_id,
                    amount.as_yoctonear(),
                )],
            };
            match asset_id {
                AssetId::Near => Some(vec![withdraw]),
                AssetId::Nep141(account_id) if account_id == WRAP_NEAR => Some(vec![
                    withdraw,
                    ExecutionInstruction::NearTransaction {
                        receiver_id: account_id.clone(),
                        actions: vec![create_unwrap_action(amount)],
                    },
                ]),
                AssetId::Nep141(_) | AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => None,
            }
        }
    }
}

pub async fn deposit_storage_on_contract_if_needed(
    contract_id: &AccountIdRef,
    trader_account_id: impl Into<Option<AccountId>>,
    amount: NearToken,
) -> Vec<ExecutionInstruction> {
    if let Some(trader_account_id) = trader_account_id.into() {
        if needs_storage_deposit_for_contract(&trader_account_id, contract_id).await {
            return vec![ExecutionInstruction::NearTransaction {
                receiver_id: contract_id.to_owned(),
                actions: vec![create_storage_deposit_action_for_contract(amount)],
            }];
        }
    }
    vec![]
}

pub async fn deposit_storage_if_needed(
    token_id: &TokenId,
    trader_account_id: impl Into<Option<AccountId>>,
) -> Vec<ExecutionInstruction> {
    let trader_account_id = trader_account_id.into();
    if let Some(trader_account_id) = trader_account_id {
        if needs_storage_deposit(&trader_account_id, token_id).await {
            create_storage_deposit_action(token_id).await
        } else {
            vec![]
        }
    } else {
        vec![]
    }
}

pub fn is_near(token_id: &TokenId) -> bool {
    match token_id {
        TokenId::Near => true,
        TokenId::Nep141(token_id) => token_id == WRAP_NEAR,
        TokenId::Nep141OnRhea(token_id) => token_id == WRAP_NEAR,
        TokenId::TokenOnIntearDex(asset_id) => match asset_id {
            AssetId::Near => true,
            AssetId::Nep141(token_id) => token_id == WRAP_NEAR,
            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => false,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseTokenId {
    Near,
    Nep141(AccountId),
}

fn base_token(token_id: &TokenId) -> Option<BaseTokenId> {
    match token_id {
        TokenId::Near => Some(BaseTokenId::Near),
        TokenId::Nep141(account_id) => Some(BaseTokenId::Nep141(account_id.clone())),
        TokenId::Nep141OnRhea(account_id)
        | TokenId::TokenOnIntearDex(AssetId::Nep141(account_id)) => {
            Some(BaseTokenId::Nep141(account_id.clone()))
        }
        TokenId::TokenOnIntearDex(AssetId::Near) => Some(BaseTokenId::Near),
        TokenId::TokenOnIntearDex(AssetId::Nep245(_, _) | AssetId::Nep171(_, _)) => None,
    }
}

pub async fn convert_to(
    from: &TokenId,
    to: &TokenId,
    amount: Balance,
    trader_account_id: Option<AccountId>,
) -> Vec<ExecutionInstruction> {
    if from == to {
        return vec![];
    }
    let (Some(from_base_token), Some(to_base_token)) = (base_token(from), base_token(to)) else {
        return vec![];
    };

    // Inner balances are withdrawn to their wallet-level token before anything
    // else happens, and deposited from it at the very end, so the conversion in
    // between only ever deals with native NEAR and NEP-141s.
    let (withdraw, from_base_token) = match from {
        TokenId::Nep141OnRhea(token_id) => {
            let unwrap_near = token_id == WRAP_NEAR && to_base_token == BaseTokenId::Near;
            (
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: "v2.ref-finance.near".parse().unwrap(),
                    actions: vec![create_rhea_withdraw_action(token_id, amount, unwrap_near)],
                }],
                if unwrap_near {
                    BaseTokenId::Near
                } else {
                    from_base_token
                },
            )
        }
        TokenId::TokenOnIntearDex(asset_id) => (
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "dex.intear.near".parse().unwrap(),
                actions: vec![create_intear_dex_withdraw_action(asset_id, amount)],
            }],
            from_base_token,
        ),
        TokenId::Near | TokenId::Nep141(_) => (vec![], from_base_token),
    };

    let (register, deposit) = match to {
        TokenId::Nep141OnRhea(token_id) => (
            [
                deposit_storage_if_needed(to, trader_account_id.clone()).await,
                create_ft_deposit_registrations(
                    &"v2.ref-finance.near".parse().unwrap(),
                    token_id,
                    trader_account_id.clone(),
                )
                .await,
            ]
            .concat(),
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions: vec![create_rhea_nep141_deposit_action(
                    &"v2.ref-finance.near".parse().unwrap(),
                    amount,
                )],
            }],
        ),
        TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)) => (
            [
                deposit_storage_if_needed(to, trader_account_id.clone()).await,
                create_ft_deposit_registrations(
                    &"dex.intear.near".parse().unwrap(),
                    token_id,
                    trader_account_id.clone(),
                )
                .await,
            ]
            .concat(),
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions: vec![create_intear_nep141_deposit_action(
                    &"dex.intear.near".parse().unwrap(),
                    amount,
                )],
            }],
        ),
        TokenId::TokenOnIntearDex(AssetId::Near) => (
            deposit_storage_if_needed(to, trader_account_id.clone()).await,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "dex.intear.near".parse().unwrap(),
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "deposit_near".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                    gas: Gas(NearGas::from_tgas(10)),
                    deposit: NearToken::from_yoctonear(amount),
                }))],
            }],
        ),
        TokenId::Near
        | TokenId::Nep141(_)
        | TokenId::TokenOnIntearDex(AssetId::Nep245(_, _) | AssetId::Nep171(_, _)) => {
            (vec![], vec![])
        }
    };

    let convert = match (&from_base_token, &to_base_token) {
        (BaseTokenId::Near, BaseTokenId::Near)
        | (BaseTokenId::Nep141(_), BaseTokenId::Nep141(_)) => vec![],
        (BaseTokenId::Near, BaseTokenId::Nep141(_)) => {
            if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                    actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
                }]
            } else {
                vec![]
            }
        }
        (BaseTokenId::Nep141(token_id), BaseTokenId::Near) if token_id == WRAP_NEAR => {
            if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                    actions: vec![create_unwrap_action(NearToken::from_yoctonear(amount))],
                }]
            } else {
                vec![]
            }
        }
        (BaseTokenId::Nep141(_), BaseTokenId::Near) => vec![],
    };

    [register, withdraw, convert, deposit].concat()
}
