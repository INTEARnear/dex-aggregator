use std::{collections::HashMap, fmt::Display, future::Future, pin::Pin, str::FromStr};

use base64::{prelude::BASE64_STANDARD, Engine};
use borsh::{BorshDeserialize, BorshSerialize};
use near_min_api::{
    types::{AccountId, Action, Balance, Finality, FunctionCallAction, NearGas, NearToken, U128},
    utils::dec_format,
    QueryFinality,
};
use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};
use tracing::info;

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage_f64, is_near,
        REQWEST_CLIENT, RPC_CLIENT,
    },
    types::{ExecutionInstruction, TokenId},
    Amount, DexId, Provider, Route, SwapRequest,
};

pub struct IntearPlachProvider;

const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";

#[derive(PartialEq, Eq, Hash, Clone, PartialOrd, Ord, Debug, BorshSerialize, BorshDeserialize)]
pub enum AssetId {
    Near,
    Nep141(AccountId),
    Nep245(AccountId, String),
    Nep171(AccountId, String),
}

impl Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Near => write!(f, "near"),
            Self::Nep141(contract_id) => write!(f, "nep141:{contract_id}"),
            Self::Nep245(contract_id, token_id) => write!(f, "nep245:{contract_id}:{token_id}"),
            Self::Nep171(contract_id, token_id) => write!(f, "nep171:{contract_id}:{token_id}"),
        }
    }
}

impl FromStr for AssetId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "near" => Ok(Self::Near),
            _ => match s.split_once(':') {
                Some(("nep141", contract_id)) => {
                    Ok(Self::Nep141(contract_id.parse().map_err(|e| {
                        format!("Invalid account id {contract_id}: {e}")
                    })?))
                }
                Some(("nep245", rest)) => {
                    if let Some((contract_id, token_id)) = rest.split_once(':') {
                        Ok(Self::Nep245(
                            contract_id
                                .parse()
                                .map_err(|e| format!("Invalid account id {contract_id}: {e}"))?,
                            token_id.to_string(),
                        ))
                    } else {
                        Err(format!("Invalid asset id: {s}"))
                    }
                }
                Some(("nep171", rest)) => {
                    if let Some((contract_id, token_id)) = rest.split_once(':') {
                        Ok(Self::Nep171(
                            contract_id
                                .parse()
                                .map_err(|e| format!("Invalid account id {contract_id}: {e}"))?,
                            token_id.to_string(),
                        ))
                    } else {
                        Err(format!("Invalid asset id: {s}"))
                    }
                }
                _ => Err(format!("Invalid asset id: {s}")),
            },
        }
    }
}

impl Serialize for AssetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde::Serialize::serialize(&self.to_string(), serializer)
    }
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(BorshSerialize, Serialize, Clone, Debug)]
enum SwapRequestAmount {
    ExactIn(U128),
    #[allow(dead_code)]
    ExactOut(U128),
}

#[derive(Serialize, Clone, Debug)]
#[allow(dead_code)]
enum Operation {
    RegisterAssets {
        asset_ids: Vec<AssetId>,
        r#for: Option<AccountOrDexId>,
    },
    Withdraw {
        asset_id: AssetId,
        amount: WithdrawAmount,
        to: Option<AccountId>,
        rescue_address: Option<AccountId>,
    },
    SwapSimple {
        dex_id: String,
        message: String,
        asset_in: AssetId,
        asset_out: AssetId,
        amount: SwapOperationAmount,
        constraint: Option<U128>,
    },
    DexCall {
        dex_id: String,
        method: String,
        args: String,
        attached_assets: HashMap<AssetId, U128>,
    },
    TransferAsset {
        to: AccountOrDexId,
        asset_id: AssetId,
        amount: U128,
    },
    StorageDeposit {
        amount: U128,
        r#for: Option<AccountOrDexId>,
    },
}
#[derive(Clone, Debug)]
enum AccountOrDexId {
    Account(AccountId),
    Dex(String),
}

impl FromStr for AccountOrDexId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(account) = s.strip_prefix("account:") {
            Ok(Self::Account(
                account
                    .parse()
                    .map_err(|e| format!("Invalid account id: {e}"))?,
            ))
        } else if let Some(dex) = s.strip_prefix("dex:") {
            Ok(Self::Dex(dex.to_string()))
        } else {
            Err(
                "Invalid format. Use 'account:<account_id>' or 'dex:<deployer>/<dex_name>'"
                    .to_string(),
            )
        }
    }
}

impl Serialize for AccountOrDexId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Account(account_id) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Account", account_id)?;
                map.end()
            }
            Self::Dex(dex_id) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Dex", dex_id)?;
                map.end()
            }
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[allow(dead_code)]
enum SwapOperationAmount {
    Amount(SwapRequestAmount),
    OutputOfLastIn,
    EntireBalanceIn,
}

#[derive(Serialize, Clone, Debug)]
#[allow(dead_code)]
enum WithdrawAmount {
    Full { at_least: Option<U128> },
    Exact(U128),
    PreviousSwapOutput,
}

impl Provider for IntearPlachProvider {
    fn dex_id(&self) -> DexId {
        DexId::Plach
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            let Amount::AmountIn(exact_amount_in) = request.amount else {
                // smartrouter.ref.finance/findPath doesn't support AmountOut
                return None;
            };

            let slippage =
                get_slippage_f64(request.slippage, &request.token_in, &request.token_out).await;

            let (_, token_in) = match &request.token_in {
                t if is_near(t) => (
                    convert_to_native(t, None, NearToken::from_yoctonear(exact_amount_in)).await?,
                    AssetId::Near,
                ),
                _ => convert_to_nep141(&request.token_in, None, 0)
                    .await
                    .map(|(v, t)| (v, AssetId::Nep141(t)))?,
            };
            let (_, token_out) = match &request.token_out {
                t if is_near(t) => (
                    convert_to_native(t, None, NearToken::from_yoctonear(exact_amount_in)).await?,
                    AssetId::Near,
                ),
                _ => convert_to_nep141(&request.token_out, None, 0)
                    .await
                    .map(|(v, t)| (v, AssetId::Nep141(t)))?,
            };

            if token_in == token_out {
                return None;
            }

            let url = format!("http://localhost:12346/findPath?tokenIn={token_in}&tokenOut={token_out}&maxHops=Four&slippage={slippage}&amountIn={exact_amount_in}");
            info!("URL: {url}");

            let Ok(response) = REQWEST_CLIENT.get(url).send().await else {
                return None;
            };

            let Ok(response) = dbg!(response.json::<ApiResponse>().await) else {
                return None;
            };

            info!("Found Plach route: {:?}", response.result_data);
            let Some(response) = response.result_data else {
                return None;
            };

            if response.routes.is_empty() {
                return None;
            }

            let total_min_amount_out = response
                .routes
                .iter()
                .map(|route| route.min_amount_out)
                .sum();
            let steps = response
                .routes
                .into_iter()
                .flat_map(|route| route.pools)
                .collect::<Vec<_>>();
            let swaps = steps
                .iter()
                .map(|step| Operation::SwapSimple {
                    dex_id: PLACH_DEX_ID.to_string(),
                    message: BASE64_STANDARD.encode(borsh::to_vec(&step.pool_id).unwrap()),
                    asset_in: step.token_in.clone(),
                    asset_out: step.token_out.clone(),
                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(
                        step.amount_in,
                    ))),
                    constraint: Some(U128::from(step.min_amount_out)),
                })
                .collect::<Vec<_>>();

            let mut operations = swaps;
            operations.push(Operation::Withdraw {
                asset_id: token_out.clone(),
                amount: WithdrawAmount::Full {
                    at_least: Some(U128::from(total_min_amount_out)),
                },
                to: None,
                rescue_address: None,
            });
            operations.push(Operation::Withdraw {
                asset_id: token_in.clone(),
                amount: WithdrawAmount::Full { at_least: None },
                to: None,
                rescue_address: None,
            });

            let both_registered =
                if let Some(trader_account_id) = request.trader_account_id.as_ref() {
                    RPC_CLIENT
                        .call::<bool>(
                            INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                            "are_assets_registered",
                            serde_json::json!({
                                "asset_ids": vec![token_in.clone(), token_out.clone()],
                                "for": {
                                    "Account": trader_account_id,
                                }
                            }),
                            QueryFinality::Finality(Finality::Final),
                        )
                        .await
                        .unwrap_or(false)
                } else {
                    true
                };
            let registration_transactions = if !both_registered {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![
                        Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "storage_deposit".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                            gas: NearGas::from_tgas(10).as_gas(),
                            deposit: "0.005 NEAR".parse().unwrap(),
                        })),
                        Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "register_assets".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "asset_ids": vec![token_in.clone(), token_out.clone()],
                            }))
                            .unwrap(),
                            gas: NearGas::from_tgas(10).as_gas(),
                            deposit: NearToken::from_yoctonear(1),
                        })),
                    ],
                }]
            } else {
                vec![]
            };

            let transactions = match token_in {
                AssetId::Near => {
                    let input_to_native = convert_to_native(
                        &request.token_in,
                        request.trader_account_id.clone(),
                        NearToken::from_yoctonear(exact_amount_in),
                    )
                    .await?;
                    let deposit_near_action = Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_near".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": operations,
                        }))
                        .unwrap(),
                        gas: NearGas::from_tgas(150).as_gas(),
                        deposit: NearToken::from_yoctonear(exact_amount_in),
                    }));
                    let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![deposit_near_action],
                    }];
                    [
                        deposit_storage_if_needed(
                            &match &token_out {
                                AssetId::Near => TokenId::Near,
                                AssetId::Nep141(token_out_id) => {
                                    TokenId::Nep141(token_out_id.clone())
                                }
                                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                            },
                            request.trader_account_id.clone(),
                        )
                        .await,
                        deposit_storage_if_needed(
                            &TokenId::Near,
                            request.trader_account_id.clone(),
                        )
                        .await,
                        input_to_native,
                        swap_transactions,
                    ]
                    .concat()
                }
                AssetId::Nep141(token_in_id) => {
                    let ft_transfer_call_swap_action =
                        Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                "amount": exact_amount_in.to_string(),
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "operations": operations,
                                })).unwrap(),
                            }))
                            .unwrap(),
                            gas: NearGas::from_tgas(150).as_gas(),
                            deposit: NearToken::from_yoctonear(1),
                        }));
                    let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                        receiver_id: token_in_id.clone(),
                        actions: vec![ft_transfer_call_swap_action],
                    }];

                    let (input_to_nep141, input_nep141) = convert_to_nep141(
                        &request.token_in,
                        request.trader_account_id.clone(),
                        exact_amount_in,
                    )
                    .await?;
                    assert_eq!(token_in_id, input_nep141);

                    [
                        deposit_storage_if_needed(
                            &match &token_out {
                                AssetId::Near => TokenId::Near,
                                AssetId::Nep141(token_out_id) => {
                                    TokenId::Nep141(token_out_id.clone())
                                }
                                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                            },
                            request.trader_account_id.clone(),
                        )
                        .await,
                        deposit_storage_if_needed(
                            &TokenId::Nep141(input_nep141),
                            request.trader_account_id.clone(),
                        )
                        .await,
                        input_to_nep141,
                        swap_transactions,
                    ]
                    .concat()
                }
                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => {
                    unreachable!()
                }
            };

            Some(Route {
                dex_id: DexId::Plach,
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(response.amount_out),
                worst_case_amount: Amount::AmountOut(total_min_amount_out),
                execution_instructions: [registration_transactions, transactions].concat(),
                has_leftover_after_slippage_that_needs_unwrapping: false,
                token_output: match token_out {
                    AssetId::Near => TokenId::Near,
                    AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                    AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                },
            })
        })
    }
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ApiResponse {
    result_code: i32,
    result_message: String,
    result_data: Option<SplitRouteApiResponse>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct SplitRouteApiResponse {
    routes: Vec<ApiResponseRoute>,
    contract_in: AssetId,
    contract_out: AssetId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ApiResponseRoute {
    pools: Vec<ApiResponsePoolStep>,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ApiResponsePoolStep {
    #[serde(with = "dec_format")]
    pool_id: u32,
    token_in: AssetId,
    token_out: AssetId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
}
