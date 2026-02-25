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
use num_traits::FromPrimitive;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::Mutex;

use crate::types::{ExecutionInstruction, Slippage, TokenId};

pub const WRAP_NEAR: &str = "wrap.near";
pub const DEFAULT_REFERRER_ID: &str = "dex-aggregator.intear.near";

impl TokenId {
    pub fn location(&self) -> TokenLocation {
        match self {
            TokenId::Near => TokenLocation::Native,
            TokenId::Nep141(_) => TokenLocation::Nep141,
            TokenId::Nep141OnRhea(_) => TokenLocation::Nep141OnRhea,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenLocation {
    Native,
    Nep141,
    Nep141OnRhea,
}

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

pub async fn needs_storage_deposit(account_id: &AccountId, token_id: &TokenId) -> bool {
    match token_id {
        TokenId::Near => false,
        TokenId::Nep141(token_id) => needs_storage_deposit_for_contract(account_id, token_id).await,
        TokenId::Nep141OnRhea(_token_id) => {
            let is_registered = RPC_CLIENT
                .call::<bool>(
                    "v2.ref-finance.near".parse().unwrap(),
                    "token_register_of",
                    serde_json::json!({
                        "account_id": account_id,
                        "token_id": token_id.get_account_id(),
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
        TokenId::Nep141OnRhea(_token_id) => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![
                    create_storage_deposit_action_for_contract(NearToken::from_millinear(10)),
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "register_tokens".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "token_ids": [token_id.get_account_id()],
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
    pub static ref TOKEN_PRICES: Mutex<HashMap<AccountId, f64>> = {
        let prices = Mutex::new(HashMap::new());
        tokio::spawn(update_token_prices_loop());
        prices
    };
}

pub async fn get_slippage_f64(slippage: Slippage, token_in: &TokenId, token_out: &TokenId) -> f64 {
    match slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => {
            let get_token_volatiltiy = |token_info: TokenInfo| async move {
                if token_info.created_at > get_current_block_height().await.unwrap() - 100 {
                    // New token
                    1.0
                } else if token_info.created_at > get_current_block_height().await.unwrap() - 1000 {
                    // New token, but not that new
                    0.6
                } else {
                    let price_change_24h =
                        (token_info.price_usd_raw_24h_ago - token_info.price_usd_raw.clone()).abs();
                    let price_change_24h_relative =
                        price_change_24h / token_info.price_usd_raw.clone();

                    let mut scale = 0f64;
                    if price_change_24h_relative > BigDecimal::from_f64(0.5).unwrap() {
                        scale += 0.1;
                    }
                    if price_change_24h_relative > BigDecimal::from_f64(0.2).unwrap() {
                        scale += 0.05;
                    }

                    let volume_to_mcap_ratio = token_info.volume_usd_24h
                        / (BigDecimal::from(token_info.circulating_supply)
                            * token_info.price_usd_raw);
                    if volume_to_mcap_ratio > BigDecimal::from_f64(1.00).unwrap() {
                        scale += 0.1
                    }
                    if volume_to_mcap_ratio > BigDecimal::from_f64(0.2).unwrap() {
                        scale += 0.05;
                    }

                    let volume_to_liquidity_ratio =
                        token_info.volume_usd_24h / token_info.liquidity_usd;
                    if volume_to_liquidity_ratio > 1.00 {
                        scale += 0.15;
                    }
                    if volume_to_liquidity_ratio > 0.5 {
                        scale += 0.1;
                    }
                    if volume_to_liquidity_ratio > 0.2 {
                        scale += 0.05;
                    }

                    scale.clamp(0.0, 1.0)
                }
            };

            let optimal_slippage_scale_input = if let Ok(mut tokens) = get_all_tokens().await {
                if let Some(token_info) = tokens.remove(token_in) {
                    get_token_volatiltiy(token_info).await
                } else {
                    // Maybe it's a new token, but not 100% sure
                    0.8
                }
            } else {
                // An error occurred
                0.5
            };
            let optimal_slippage_scale_output = if let Ok(mut tokens) = get_all_tokens().await {
                if let Some(token_info) = tokens.remove(token_out) {
                    get_token_volatiltiy(token_info).await
                } else {
                    // Maybe it's a new token, but not 100% sure
                    0.8
                }
            } else {
                // An error occurred
                0.5
            };
            let optimal_slippage_scale =
                optimal_slippage_scale_input.max(optimal_slippage_scale_output);
            let optimal_slippage =
                min_slippage + (max_slippage - min_slippage) * optimal_slippage_scale;
            let optimal_slippage = optimal_slippage.clamp(min_slippage, max_slippage);

            optimal_slippage.clamp(0.0001, 0.9999)
        }
        Slippage::Fixed { slippage } => slippage.clamp(0.0001, 0.9999),
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
    pub liquidity_usd: f64,
    pub volume_usd_24h: f64,
    pub created_at: BlockHeight,
}

fn deserialize_bigdecimal<'de, D>(deserializer: D) -> Result<BigDecimal, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    BigDecimal::from_str(&s).map_err(serde::de::Error::custom)
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
                    if let Ok(price_raw) = token_data.price_usd_raw.parse::<f64>() {
                        token_prices.insert(account_id, price_raw / 1e6);
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
pub async fn get_token_price(token_id: &AccountId) -> Option<f64> {
    let token_prices = TOKEN_PRICES.lock().await;
    token_prices.get(token_id).copied()
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
            _ => optimized_execution_instructions.push(next_instruction),
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
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "withdraw".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "token_id": token_id.to_string(),
                        "amount": amount.to_string(),
                        "skip_unwrap_near": true,
                    }))
                    .unwrap(),
                    gas: Gas(NearGas::from_tgas(50)),
                    deposit: NearToken::from_yoctonear(1),
                }))],
            }],
            token_id.clone(),
        )),
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
            if !amount.is_zero() {
                Some(vec![ExecutionInstruction::NearTransaction {
                    receiver_id: "v2.ref-finance.near".parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "withdraw".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "token_id": account_id.to_string(),
                            "amount": amount.as_yoctonear().to_string(),
                            "skip_unwrap_near": false,
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    }))],
                }])
            } else {
                None
            }
        }
        TokenId::Nep141OnRhea(_non_wrap_near) => None,
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
    }
}

pub async fn convert_to(
    from: &TokenId,
    to: TokenLocation,
    amount: Balance,
    trader_account_id: Option<AccountId>,
) -> Vec<ExecutionInstruction> {
    match (from, to) {
        (TokenId::Near, TokenLocation::Native) => vec![],
        (TokenId::Near, TokenLocation::Nep141) => {
            if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                    actions: vec![create_wrap_action(NearToken::from_yoctonear(amount))],
                }]
            } else {
                vec![]
            }
        }
        (TokenId::Near, TokenLocation::Nep141OnRhea) => [
            deposit_storage_if_needed(
                &TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                trader_account_id,
            )
            .await,
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: WRAP_NEAR.parse().unwrap(),
                actions: vec![
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "near_deposit".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                        gas: Gas(NearGas::from_tgas(2)),
                        deposit: NearToken::from_yoctonear(amount),
                    })),
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "ft_transfer_call".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "receiver_id": "v2.ref-finance.near",
                            "amount": amount.to_string(),
                            "msg": "",
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(50)),
                        deposit: NearToken::from_yoctonear(1),
                    })),
                ],
            }],
        ]
        .concat(),
        (TokenId::Nep141(token_id), TokenLocation::Native) if token_id == WRAP_NEAR => {
            if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: WRAP_NEAR.parse::<AccountId>().unwrap(),
                    actions: vec![create_unwrap_action(NearToken::from_yoctonear(amount))],
                }]
            } else {
                vec![]
            }
        }
        (TokenId::Nep141(_), TokenLocation::Native) => vec![],
        (TokenId::Nep141(_), TokenLocation::Nep141) => vec![],
        (TokenId::Nep141(token_id), TokenLocation::Nep141OnRhea) => {
            let mut actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "ft_transfer_call".to_string(),
                args: serde_json::to_vec(&serde_json::json!({
                    "receiver_id": "v2.ref-finance.near",
                    "amount": amount.to_string(),
                    "msg": "",
                }))
                .unwrap(),
                gas: Gas(NearGas::from_tgas(50)),
                deposit: NearToken::from_yoctonear(1),
            }))];
            if needs_storage_deposit(
                &"v2.ref-finance.near".parse().unwrap(),
                &TokenId::Nep141(token_id.clone()),
            )
            .await
            {
                actions.insert(
                    0,
                    create_storage_deposit_action_for_someone(
                        "0.00125 NEAR".parse().unwrap(),
                        &"v2.ref-finance.near".parse().unwrap(),
                    ),
                );
            }
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions,
            }]
        }
        (TokenId::Nep141OnRhea(token_id), TokenLocation::Native) if token_id == WRAP_NEAR => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "withdraw".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "token_id": token_id.to_string(),
                        "amount": amount.to_string(),
                        "skip_unwrap_near": false,
                    }))
                    .unwrap(),
                    gas: Gas(NearGas::from_tgas(50)),
                    deposit: NearToken::from_yoctonear(1),
                }))],
            }]
        }
        (TokenId::Nep141OnRhea(_), TokenLocation::Native) => vec![],
        (TokenId::Nep141OnRhea(token_id), TokenLocation::Nep141) => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "withdraw".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "token_id": token_id.to_string(),
                        "amount": amount.to_string(),
                        "skip_unwrap_near": true,
                    }))
                    .unwrap(),
                    gas: Gas(NearGas::from_tgas(50)),
                    deposit: NearToken::from_yoctonear(1),
                }))],
            }]
        }
        (TokenId::Nep141OnRhea(_), TokenLocation::Nep141OnRhea) => vec![],
    }
}
