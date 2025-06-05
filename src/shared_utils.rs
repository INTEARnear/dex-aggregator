use std::{collections::HashMap, time::Duration};

use cached::proc_macro::cached;
use lazy_static::lazy_static;
use near_min_api::{
    types::{
        AccountId, Action, Balance, BlockHeight, BlockReference, Finality, FunctionCallAction,
        NearGas, NearToken,
    },
    utils::dec_format,
    QueryFinality, RpcClient,
};
use reqwest::{Client, ClientBuilder};
use serde::Deserialize;

use crate::types::{Slippage, TokenId};

pub const WRAP_NEAR: &str = "wrap.near";

pub fn create_wrap_action(amount: NearToken) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "near_deposit".to_string(),
        args: serde_json::to_string(&serde_json::json!({}))
            .unwrap()
            .as_bytes()
            .to_vec(),
        gas: NearGas::from_tgas(2).as_gas(),
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
        gas: NearGas::from_tgas(5).as_gas(),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub async fn needs_storage_deposit(account_id: &AccountId, token_id: &TokenId) -> bool {
    match token_id {
        TokenId::Near => false,
        TokenId::Nep141(token_id) => {
            let Ok(storage_deposit) = RPC_CLIENT
                .call::<StorageDeposit>(
                    token_id.clone(),
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
            storage_deposit.total == 0
        }
    }
}

#[derive(Debug, Deserialize)]
struct StorageDeposit {
    #[allow(dead_code)]
    #[serde(with = "dec_format")]
    available: Balance,
    #[serde(with = "dec_format")]
    total: Balance,
}

pub async fn create_storage_deposit_action(token_id: TokenId) -> Action {
    match token_id {
        TokenId::Nep141(_token_id) => Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "storage_deposit".to_string(),
            args: serde_json::to_string(&serde_json::json!({
                "registration_only": true,
            }))
            .unwrap()
            .as_bytes()
            .to_vec(),
            gas: NearGas::from_tgas(10).as_gas(),
            deposit: "0.00125 NEAR".parse().unwrap(),
        })),
        TokenId::Near => panic!("NEAR doesn't need a storage deposit"),
    }
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
                "https://rpc.intear.tech,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                    .to_string()
            })
            .split(',')
            .map(|url| url.to_string())
            .collect::<Vec<_>>(),
    );
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
                        (token_info.price_usd_raw_24h_ago - token_info.price_usd_raw).abs();
                    let price_change_24h_relative = price_change_24h / token_info.price_usd_raw;

                    let mut scale = 0f64;
                    if price_change_24h_relative > 0.5 {
                        scale += 0.1;
                    }
                    if price_change_24h_relative > 0.2 {
                        scale += 0.05;
                    }

                    let volume_to_mcap_ratio = token_info.volume_usd_24h
                        / (token_info.circulating_supply as f64 * token_info.price_usd_raw);
                    if volume_to_mcap_ratio > 1.00 {
                        scale += 0.1
                    }
                    if volume_to_mcap_ratio > 0.2 {
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
        Slippage::Fixed(slippage) => slippage.clamp(0.0001, 0.9999),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TokenInfo {
    #[serde(with = "dec_format")]
    pub price_usd_raw: f64,
    #[serde(with = "dec_format")]
    pub price_usd_raw_24h_ago: f64,
    #[serde(with = "dec_format")]
    pub circulating_supply: Balance,
    pub liquidity_usd: f64,
    pub volume_usd_24h: f64,
    pub created_at: BlockHeight,
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
