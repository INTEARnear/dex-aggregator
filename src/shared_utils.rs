use std::time::Duration;

use lazy_static::lazy_static;
use near_min_api::{
    types::{AccountId, Action, Balance, Finality, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
    QueryFinality, RpcClient,
};
use reqwest::{Client, ClientBuilder};
use serde::Deserialize;

use crate::types::TokenId;

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
