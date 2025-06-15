use std::{future::Future, pin::Pin, time::Duration};

use chrono::{DateTime, Utc};
use near_min_api::{
    types::{Action, Balance, CryptoHash, Finality, FunctionCallAction, NearGas, NearToken},
    utils::dec_format,
    QueryFinality,
};
use serde::{Deserialize, Serialize};
use tracing::info;

const INTENTS_RPC_URL: &str = "https://solver-relay-v2.chaindefuser.com/rpc";
const INTENTS_CONTRACT_ID: &str = "intents.near";

use crate::{
    shared_utils::{
        convert_to_nep141, deposit_storage_if_needed, REQWEST_CLIENT, RPC_CLIENT, WRAP_NEAR,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct NearIntentsProvider;

impl Provider for NearIntentsProvider {
    fn dex_id(&self) -> DexId {
        DexId::NearIntents
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<(Route, TokenId)>> + Send>> {
        Box::pin(async move {
            let Some(account_id) = request.trader_account_id.as_ref() else {
                // Need to have trader's account ID for intents
                return None;
            };

            let response = REQWEST_CLIENT
                .post(INTENTS_RPC_URL)
                .json(&NearIntentsQuoteRequest {
                    jsonrpc: "2.0".to_string(),
                    id: "dontcare".to_string(),
                    method: "quote".to_string(),
                    params: vec![NearIntentsQuoteParams {
                        defuse_asset_identifier_out: match request.token_out {
                            TokenId::Near => format!("nep141:{WRAP_NEAR}"),
                            TokenId::Nep141(ref account_id) => format!("nep141:{account_id}"),
                        },
                        defuse_asset_identifier_in: match request.token_in {
                            TokenId::Near => format!("nep141:{WRAP_NEAR}"),
                            TokenId::Nep141(ref account_id) => format!("nep141:{account_id}"),
                        },
                        amount: request.amount.into(),
                        min_deadline_ms: Duration::from_secs(15).as_millis() as u64,
                        wait_ms: (request.max_wait_ms - 500).min(5000), // account for latency
                    }],
                })
                .send()
                .await
                .unwrap();

            let Ok(response) = response.json::<NearIntentsQuoteResponse>().await else {
                return None;
            };

            info!("Near Intents quotes: {:#?}", response.result);

            let best_quote = response
                .result
                .into_iter()
                .filter(|quote| {
                    quote.defuse_asset_identifier_in
                        == match request.token_in {
                            TokenId::Near => format!("nep141:{WRAP_NEAR}"),
                            TokenId::Nep141(ref account_id) => format!("nep141:{account_id}"),
                        }
                        && quote.defuse_asset_identifier_out
                            == match request.token_out {
                                TokenId::Near => format!("nep141:{WRAP_NEAR}"),
                                TokenId::Nep141(ref account_id) => format!("nep141:{account_id}"),
                            }
                })
                .min_by_key(|quote| match request.amount {
                    // If 2 or more quotes return amount more than i128::MAX, don't care about these
                    // stupidly large token amounts, usually normal tokens don't go so close to
                    // limits of u128.
                    Amount::AmountIn(_) => -(quote.amount_out.try_into().unwrap_or(i128::MAX)),
                    Amount::AmountOut(_) => quote.amount_in.try_into().unwrap_or(i128::MAX),
                })?;
            let (input_to_nep141, input_nep141_id) = convert_to_nep141(
                &request.token_in,
                Some(account_id.clone()),
                best_quote.amount_in,
            )
            .await?;

            let token_diff_intent = serde_json::json!({
                "intent": "token_diff",
                "diff": {
                    best_quote.defuse_asset_identifier_in.clone(): format!("-{}", best_quote.amount_in.to_string()),
                    best_quote.defuse_asset_identifier_out.clone(): best_quote.amount_out.to_string(),
                }
            });
            let (withdraw_intent, withdraw_token) = (
                serde_json::json!({
                    "intent": match request.token_out {
                        TokenId::Near => "native_withdraw",
                        TokenId::Nep141(_) => "ft_withdraw",
                    },
                    "token": match request.token_out {
                        TokenId::Near => None,
                        TokenId::Nep141(ref token_id) => Some(token_id.clone()),
                    },
                    "receiver_id": account_id,
                    "amount": best_quote.amount_out.to_string(),
                }),
                match request.token_out {
                    TokenId::Near => TokenId::Near,
                    TokenId::Nep141(ref account_id) => TokenId::Nep141(account_id.clone()),
                },
            );
            let message = serde_json::json!({
                "deadline": best_quote.expiration_time,
                "intents": vec![token_diff_intent, withdraw_intent],
                "signer_id": account_id,
            });

            let intents = vec![ExecutionInstruction::IntentsQuote {
                message_to_sign: serde_json::to_string(&message).unwrap(),
                quote_hash: best_quote.quote_hash,
            }];
            let deposit_instructions = vec![ExecutionInstruction::NearTransaction {
                receiver_id: input_nep141_id,
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "ft_transfer_call".to_string(),
                    args: serde_json::to_string(&serde_json::json!({
                        "receiver_id": INTENTS_CONTRACT_ID,
                        "amount": best_quote.amount_in.to_string(),
                        "msg": "",
                    }))
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
                    gas: NearGas::from_tgas(40).as_gas(),
                    deposit: NearToken::from_yoctonear(1),
                }))],
            }];
            let add_public_key_instructions =
                if let Some(signing_public_key) = request.signing_public_key {
                    if let Ok(false) = RPC_CLIENT
                        .call::<bool>(
                            INTENTS_CONTRACT_ID.parse().unwrap(),
                            "has_public_key",
                            serde_json::json!({
                                "account_id": account_id,
                                "public_key": signing_public_key,
                            }),
                            QueryFinality::Finality(Finality::DoomSlug),
                        )
                        .await
                    {
                        vec![ExecutionInstruction::NearTransaction {
                            receiver_id: INTENTS_CONTRACT_ID.parse().unwrap(),
                            actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "add_public_key".to_string(),
                                args: serde_json::to_string(&serde_json::json!({
                                    "public_key": signing_public_key,
                                }))
                                .unwrap()
                                .as_bytes()
                                .to_vec(),
                                gas: NearGas::from_tgas(5).as_gas(),
                                deposit: NearToken::from_yoctonear(1),
                            }))],
                        }]
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };
            let instructions = [
                add_public_key_instructions,
                deposit_storage_if_needed(&request.token_out, account_id.clone()).await,
                deposit_storage_if_needed(&request.token_in, account_id.clone()).await,
                input_to_nep141,
                deposit_instructions,
                intents,
            ]
            .concat();
            let route = Route {
                dex_id: DexId::NearIntents,
                estimated_amount: match request.amount {
                    Amount::AmountIn(_) => Amount::AmountOut(best_quote.amount_out),
                    Amount::AmountOut(_) => Amount::AmountIn(best_quote.amount_in),
                },
                worst_case_amount: match request.amount {
                    Amount::AmountIn(_) => Amount::AmountOut(best_quote.amount_out),
                    Amount::AmountOut(_) => Amount::AmountIn(best_quote.amount_in),
                },
                deadline: Some(best_quote.expiration_time),
                execution_instructions: instructions,
                has_slippage: false,
                needs_unwrap: false,
            };

            // TODO only outputs NEP-141 or NEAR
            Some((route, withdraw_token))
        })
    }
}

#[derive(Debug, Serialize)]
struct NearIntentsQuoteParams {
    defuse_asset_identifier_out: String,
    defuse_asset_identifier_in: String,
    #[serde(flatten)]
    amount: ExactAmount,
    min_deadline_ms: u64,
    wait_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct NearIntentsQuoteRequest {
    jsonrpc: String,
    id: String,
    method: String,
    params: Vec<NearIntentsQuoteParams>,
}

#[derive(Debug, Deserialize)]
pub struct NearIntentsQuoteResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: String,
    result: Vec<NearIntentsQuote>,
}

#[derive(Debug, Deserialize)]
struct NearIntentsQuote {
    defuse_asset_identifier_in: String,
    defuse_asset_identifier_out: String,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
    expiration_time: DateTime<Utc>,
    quote_hash: CryptoHash,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExactAmount {
    ExactAmountIn(#[serde(with = "dec_format")] Balance),
    ExactAmountOut(#[serde(with = "dec_format")] Balance),
}

impl From<Amount> for ExactAmount {
    fn from(amount: Amount) -> Self {
        match amount {
            Amount::AmountIn(amount) => ExactAmount::ExactAmountIn(amount),
            Amount::AmountOut(amount) => ExactAmount::ExactAmountOut(amount),
        }
    }
}
