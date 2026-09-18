use std::{collections::HashMap, fmt::Display, future::Future, pin::Pin, str::FromStr};

use base64::{prelude::BASE64_STANDARD, Engine};
use borsh::{BorshDeserialize, BorshSerialize};
use near_min_api::{
    types::{AccountId, Action, Balance, FunctionCallAction, Gas, NearGas, NearToken, U128},
    utils::dec_format,
};
use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};
use tracing::info;

use bigdecimal::BigDecimal;

use crate::{
    shared_utils::{
        convert_to_native, convert_to_nep141, deposit_storage_if_needed, get_slippage, is_near,
        Mainnet, NetworkView, DEFAULT_REFERRER_ID, REQWEST_CLIENT,
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

#[derive(Clone, Copy, Debug)]
enum MaxHops {
    DirectOnly,
    Four,
}

impl Display for MaxHops {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaxHops::DirectOnly => write!(f, "DirectOnly"),
            MaxHops::Four => write!(f, "Four"),
        }
    }
}

trait PlachQuotes: Send + Sync {
    fn find_path(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount: Amount,
        max_hops: MaxHops,
        slippage: &BigDecimal,
    ) -> impl Future<Output = Option<SplitRouteApiResponse>> + Send;
}

struct MainnetPlachQuotes;

impl PlachQuotes for MainnetPlachQuotes {
    async fn find_path(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount: Amount,
        max_hops: MaxHops,
        slippage: &BigDecimal,
    ) -> Option<SplitRouteApiResponse> {
        let amount_query = match amount {
            Amount::AmountIn(amount_in) => format!("amountIn={amount_in}"),
            Amount::AmountOut(amount_out) => format!("amountOut={amount_out}"),
        };
        let url = format!("http://localhost:12346/findPath?tokenIn={token_in}&tokenOut={token_out}&maxHops={max_hops}&slippage={slippage}&{amount_query}");
        info!("URL: {url}");
        let response = REQWEST_CLIENT.get(url).send().await.ok()?;
        let response = response.json::<ApiResponse>().await.ok()?;
        info!("Found Plach route: {:?}", response.result_data);
        response.result_data
    }
}

impl Provider for IntearPlachProvider {
    fn dex_id(&self) -> DexId {
        DexId::Plach
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move { route(request, &Mainnet, &MainnetPlachQuotes).await })
    }
}

async fn route(
    request: SwapRequest,
    network: &impl NetworkView,
    quotes: &impl PlachQuotes,
) -> Option<Route> {
    let amount_hint = match request.amount {
        Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
    };

    let slippage = get_slippage(
        network,
        request.slippage,
        &request.token_in,
        &request.token_out,
    )
    .await;

    let (_, token_in) = match &request.token_in {
        TokenId::TokenOnIntearDex(asset_id) => (Vec::new(), asset_id.clone()),
        t if is_near(t) => (
            convert_to_native(t, None, NearToken::from_yoctonear(amount_hint)).await?,
            AssetId::Near,
        ),
        _ => convert_to_nep141(&request.token_in, None, 0)
            .await
            .map(|(v, t)| (v, AssetId::Nep141(t)))?,
    };
    let (_, token_out) = match &request.token_out {
        TokenId::TokenOnIntearDex(asset_id) => (Vec::new(), asset_id.clone()),
        t if is_near(t) => (
            convert_to_native(t, None, NearToken::from_yoctonear(amount_hint)).await?,
            AssetId::Near,
        ),
        _ => convert_to_nep141(&request.token_out, None, 0)
            .await
            .map(|(v, t)| (v, AssetId::Nep141(t)))?,
    };

    if token_in == token_out {
        return None;
    }

    let max_hops = match request.amount {
        Amount::AmountIn(_) => MaxHops::Four,
        // Intear dex doesn't support multi-step routing with AmountOut yet
        Amount::AmountOut(_) => MaxHops::DirectOnly,
    };
    let response = quotes
        .find_path(&token_in, &token_out, request.amount, max_hops, &slippage)
        .await?;

    let (swaps, total_min_amount_out, total_max_amount_in, estimated_amount, worst_case_amount) =
        match response {
            SplitRouteApiResponse::ExactIn {
                routes, amount_out, ..
            } => {
                if routes.is_empty() {
                    return None;
                }
                let total_min_amount_out = routes.iter().map(|route| route.min_amount_out).sum();
                let steps = routes
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
                        amount: if step.amount_in > 0 {
                            SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(
                                step.amount_in,
                            )))
                        } else {
                            SwapOperationAmount::OutputOfLastIn
                        },
                        constraint: (step.min_amount_out > 0)
                            .then(|| U128::from(step.min_amount_out)),
                    })
                    .collect::<Vec<_>>();
                (
                    swaps,
                    total_min_amount_out,
                    0,
                    Amount::AmountOut(amount_out),
                    Amount::AmountOut(total_min_amount_out),
                )
            }
            SplitRouteApiResponse::ExactOut {
                routes,
                amount_in,
                max_amount_in,
                ..
            } => {
                if routes.is_empty() {
                    return None;
                }
                let steps = routes
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
                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(
                            U128::from(step.amount_out),
                        )),
                        constraint: (step.max_amount_in > 0)
                            .then(|| U128::from(step.max_amount_in)),
                    })
                    .collect::<Vec<_>>();
                (
                    swaps,
                    0,
                    max_amount_in,
                    Amount::AmountIn(amount_in),
                    Amount::AmountIn(max_amount_in),
                )
            }
        };

    let use_fast_path = matches!(request.token_in, TokenId::TokenOnIntearDex(_));

    let mut operations = swaps;
    let output_withdrawn = if use_fast_path {
        // Unlike the deposit paths, `execute_operations` reads `Full` from the
        // trader's whole inner balance, which would also sweep tokens they held
        // before the swap. So the output is only withdrawn when its amount can
        // be named exactly, and otherwise left on the DEX for the caller to
        // convert once the received amount is known.
        let exact_output = match request.amount {
            Amount::AmountOut(amount_out) => Some(WithdrawAmount::Exact(U128::from(amount_out))),
            Amount::AmountIn(_) => None,
        };
        match exact_output {
            Some(amount) if !matches!(request.token_out, TokenId::TokenOnIntearDex(_)) => {
                operations.push(Operation::Withdraw {
                    asset_id: token_out.clone(),
                    amount,
                    to: None,
                    rescue_address: None,
                });
                true
            }
            _ => false,
        }
    } else {
        operations.push(Operation::Withdraw {
            asset_id: token_out.clone(),
            amount: WithdrawAmount::Full {
                at_least: Some(match request.amount {
                    Amount::AmountIn(_) => U128::from(total_min_amount_out),
                    Amount::AmountOut(amount_out) => U128::from(amount_out),
                }),
            },
            to: None,
            rescue_address: None,
        });
        if matches!(request.amount, Amount::AmountOut(_)) {
            operations.push(Operation::Withdraw {
                asset_id: token_in.clone(),
                amount: WithdrawAmount::Full { at_least: None },
                to: None,
                rescue_address: None,
            });
        }
        true
    };

    let both_registered = if let Some(trader_account_id) = request.trader_account_id.as_ref() {
        network
            .is_intear_asset_registered(trader_account_id, &token_in)
            .await
            && network
                .is_intear_asset_registered(trader_account_id, &token_out)
                .await
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
                    gas: Gas(NearGas::from_tgas(5)),
                    deposit: "0.005 NEAR".parse().unwrap(),
                })),
                Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "register_assets".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({
                        "asset_ids": vec![token_in.clone(), token_out.clone()],
                    }))
                    .unwrap(),
                    gas: Gas(NearGas::from_tgas(5)),
                    deposit: NearToken::from_yoctonear(1),
                })),
            ],
        }]
    } else {
        vec![]
    };

    let input_amount = match request.amount {
        Amount::AmountIn(amount_in) => amount_in,
        Amount::AmountOut(_) => total_max_amount_in,
    };

    let transactions = if use_fast_path {
        let execute_operations_action =
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "execute_operations".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": operations,
                            "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(280)),
                        deposit: NearToken::from_yoctonear(1),
                    }));
        vec![ExecutionInstruction::NearTransaction {
            receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
            actions: vec![execute_operations_action],
        }]
    } else {
        match token_in {
            AssetId::Near => {
                let input_to_native = convert_to_native(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    NearToken::from_yoctonear(input_amount),
                )
                .await?;
                let deposit_near_action = Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": operations,
                                    "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                                },
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(input_amount),
                        }));
                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![deposit_near_action],
                }];
                [
                    deposit_storage_if_needed(
                        network,
                        &match &token_out {
                            AssetId::Near => TokenId::Near,
                            AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                        },
                        request.trader_account_id.clone(),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
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
                                    "amount": input_amount.to_string(),
                                    "msg": serde_json::to_string(&serde_json::json!({
                                        "operations": operations,
                                        "referrer": request.referrer_id.map(|id| id.to_string()).unwrap_or_else(|| DEFAULT_REFERRER_ID.to_string()),
                                    })).unwrap(),
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(280)),
                                deposit: NearToken::from_yoctonear(1),
                            }));
                let swap_transactions = vec![ExecutionInstruction::NearTransaction {
                    receiver_id: token_in_id.clone(),
                    actions: vec![ft_transfer_call_swap_action],
                }];

                let (input_to_nep141, input_nep141) = convert_to_nep141(
                    &request.token_in,
                    request.trader_account_id.clone(),
                    input_amount,
                )
                .await?;
                assert_eq!(token_in_id, input_nep141);

                [
                    deposit_storage_if_needed(
                        network,
                        &match &token_out {
                            AssetId::Near => TokenId::Near,
                            AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                            AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
                        },
                        request.trader_account_id.clone(),
                    )
                    .await,
                    deposit_storage_if_needed(
                        network,
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
        }
    };

    Some(Route {
        dex_id: DexId::Plach,
        deadline: None,
        has_slippage: true,
        estimated_amount,
        worst_case_amount,
        execution_instructions: [registration_transactions, transactions].concat(),
        deprecated_needs_unwrap_always_false: false,
        token_output: if output_withdrawn {
            match token_out {
                AssetId::Near => TokenId::Near,
                AssetId::Nep141(token_out_id) => TokenId::Nep141(token_out_id.clone()),
                AssetId::Nep245(_, _) | AssetId::Nep171(_, _) => unreachable!(),
            }
        } else {
            TokenId::TokenOnIntearDex(token_out)
        },
    })
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct ApiResponse {
    result_code: i32,
    result_message: String,
    result_data: Option<SplitRouteApiResponse>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
#[serde(tag = "quote_type", rename_all = "snake_case")]
enum SplitRouteApiResponse {
    ExactIn {
        routes: Vec<ExactInRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        #[serde(with = "dec_format")]
        amount_in: Balance,
        #[serde(with = "dec_format")]
        amount_out: Balance,
    },
    ExactOut {
        routes: Vec<ExactOutRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        #[serde(with = "dec_format")]
        amount_in: Balance,
        #[serde(with = "dec_format")]
        max_amount_in: Balance,
        #[serde(with = "dec_format")]
        amount_out: Balance,
    },
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct ExactInRoute {
    pools: Vec<ExactInPoolStep>,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct ExactOutRoute {
    pools: Vec<ExactOutPoolStep>,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    max_amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct ExactInPoolStep {
    #[serde(with = "dec_format")]
    pool_id: u32,
    token_in: AssetId,
    token_out: AssetId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct ExactOutPoolStep {
    #[serde(with = "dec_format")]
    pool_id: u32,
    token_in: AssetId,
    token_out: AssetId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
    #[serde(with = "dec_format")]
    max_amount_in: Balance,
}

#[cfg(test)]
mod tests {
    use near_min_api::types::{Action, FunctionCallAction, Gas, NearGas, NearToken};

    use super::*;
    use crate::shared_utils::{
        create_rhea_withdraw_action, create_storage_deposit_action_for_contract,
        create_unwrap_action, TestNetworkView, WRAP_NEAR,
    };
    use crate::types::Slippage;

    #[derive(Clone)]
    struct TestPlachQuotes {
        enabled: bool,
        empty_routes: bool,
    }

    impl Default for TestPlachQuotes {
        fn default() -> Self {
            Self {
                enabled: true,
                empty_routes: false,
            }
        }
    }

    impl TestPlachQuotes {
        fn none() -> Self {
            Self {
                enabled: false,
                empty_routes: false,
            }
        }

        fn empty_routes() -> Self {
            Self {
                enabled: true,
                empty_routes: true,
            }
        }
    }

    impl PlachQuotes for TestPlachQuotes {
        async fn find_path(
            &self,
            token_in: &AssetId,
            token_out: &AssetId,
            amount: Amount,
            _max_hops: MaxHops,
            _slippage: &BigDecimal,
        ) -> Option<SplitRouteApiResponse> {
            if !self.enabled {
                return None;
            }
            if self.empty_routes {
                return Some(SplitRouteApiResponse::ExactIn {
                    routes: vec![],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: 100,
                    amount_out: 80,
                });
            }
            match amount {
                Amount::AmountIn(_) => Some(SplitRouteApiResponse::ExactIn {
                    routes: vec![ExactInRoute {
                        pools: vec![ExactInPoolStep {
                            pool_id: 1,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            amount_in: 100,
                            min_amount_out: 79,
                        }],
                        amount_in: 100,
                        min_amount_out: 79,
                        amount_out: 80,
                    }],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: 100,
                    amount_out: 80,
                }),
                Amount::AmountOut(_) => Some(SplitRouteApiResponse::ExactOut {
                    routes: vec![ExactOutRoute {
                        pools: vec![ExactOutPoolStep {
                            pool_id: 1,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            amount_in: 100,
                            amount_out: 80,
                            max_amount_in: 101,
                        }],
                        amount_in: 100,
                        max_amount_in: 101,
                        amount_out: 80,
                    }],
                    contract_in: token_in.clone(),
                    contract_out: token_out.clone(),
                    amount_in: 100,
                    max_amount_in: 101,
                    amount_out: 80,
                }),
            }
        }
    }

    #[tokio::test]
    async fn amount_in_near_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                },
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_wnear_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: WRAP_NEAR.parse().unwrap(),
                        actions: vec![create_unwrap_action(NearToken::from_yoctonear(100))],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                },
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_wnear_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea(WRAP_NEAR.parse().unwrap()),
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &WRAP_NEAR.parse().unwrap(),
                            100,
                            true,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                },
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(100),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_intear_near_to_ft_fast_path() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![Operation::SwapSimple {
                                    dex_id: PLACH_DEX_ID.to_string(),
                                    message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                    asset_in: token_in.clone(),
                                    asset_out: token_out.clone(),
                                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                    constraint: Some(U128::from(79)),
                                }],
                                "referrer": DEFAULT_REFERRER_ID,
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::TokenOnIntearDex(token_out),
            })
        );
    }

    #[tokio::test]
    async fn amount_in_ft_to_near() {
        let token_in = AssetId::Nep141("ft".parse().unwrap());
        let token_out = AssetId::Near;
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141("ft".parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_in_rhea_ft_to_near() {
        let token_in = AssetId::Nep141("ft".parse().unwrap());
        let token_out = AssetId::Near;
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Nep141OnRhea("ft".parse().unwrap()),
                    token_out: TokenId::Near,
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "v2.ref-finance.near".parse().unwrap(),
                        actions: vec![create_rhea_withdraw_action(
                            &"ft".parse().unwrap(),
                            100,
                            false,
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "ft_transfer_call".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "receiver_id": INTEAR_DEX_CONTRACT_ID,
                                "amount": "100",
                                "msg": serde_json::to_string(&serde_json::json!({
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                            constraint: Some(U128::from(79)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(79)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                }))
                                .unwrap(),
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
            })
        );
    }

    #[tokio::test]
    async fn amount_out_near_to_ft() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountOut(80),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: "ft".parse().unwrap(),
                        actions: vec![create_storage_deposit_action_for_contract(
                            "0.00125 NEAR".parse().unwrap(),
                        )],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "deposit_near".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": {
                                    "operations": vec![
                                        Operation::SwapSimple {
                                            dex_id: PLACH_DEX_ID.to_string(),
                                            message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                            asset_in: token_in.clone(),
                                            asset_out: token_out.clone(),
                                            amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                            constraint: Some(U128::from(101)),
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_out.clone(),
                                            amount: WithdrawAmount::Full {
                                                at_least: Some(U128::from(80)),
                                            },
                                            to: None,
                                            rescue_address: None,
                                        },
                                        Operation::Withdraw {
                                            asset_id: token_in.clone(),
                                            amount: WithdrawAmount::Full { at_least: None },
                                            to: None,
                                            rescue_address: None,
                                        },
                                    ],
                                    "referrer": DEFAULT_REFERRER_ID,
                                },
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(101),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_intear_near_to_ft_withdraws_exact() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountOut(80),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.to_string(),
                                        message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                        asset_in: token_in.clone(),
                                        asset_out: token_out.clone(),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                        constraint: Some(U128::from(101)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: token_out.clone(),
                                        amount: WithdrawAmount::Exact(U128::from(80)),
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": DEFAULT_REFERRER_ID,
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn amount_out_intear_to_intear_leaves_output_on_dex() {
        let token_in = AssetId::Near;
        let token_out = AssetId::Nep141("ft".parse().unwrap());
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::TokenOnIntearDex(AssetId::Near),
                    token_out: TokenId::TokenOnIntearDex(AssetId::Nep141("ft".parse().unwrap())),
                    amount: Amount::AmountOut(80),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountIn(100),
                worst_case_amount: Amount::AmountIn(101),
                dex_id: DexId::Plach,
                execution_instructions: vec![
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "storage_deposit".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: "0.005 NEAR".parse().unwrap(),
                            })),
                            Action::FunctionCall(Box::new(FunctionCallAction {
                                method_name: "register_assets".to_string(),
                                args: serde_json::to_vec(&serde_json::json!({
                                    "asset_ids": vec![token_in.clone(), token_out.clone()],
                                }))
                                .unwrap(),
                                gas: Gas(NearGas::from_tgas(5)),
                                deposit: NearToken::from_yoctonear(1),
                            })),
                        ],
                    },
                    ExecutionInstruction::NearTransaction {
                        receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                            method_name: "execute_operations".to_string(),
                            args: serde_json::to_vec(&serde_json::json!({
                                "operations": vec![Operation::SwapSimple {
                                    dex_id: PLACH_DEX_ID.to_string(),
                                    message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                    asset_in: token_in.clone(),
                                    asset_out: token_out.clone(),
                                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactOut(U128::from(80))),
                                    constraint: Some(U128::from(101)),
                                }],
                                "referrer": DEFAULT_REFERRER_ID,
                            }))
                            .unwrap(),
                            gas: Gas(NearGas::from_tgas(280)),
                            deposit: NearToken::from_yoctonear(1),
                        }))],
                    },
                ],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::TokenOnIntearDex(token_out),
            })
        );
    }

    #[tokio::test]
    async fn same_near_family_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141(WRAP_NEAR.parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn missing_quote_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::none(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn empty_routes_returns_none() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: Some("trader.near".parse().unwrap()),
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::empty_routes(),
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn no_trader_omits_registration_and_storage() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: None,
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_near".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": {
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.to_string(),
                                        message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                        asset_in: AssetId::Near,
                                        asset_out: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                        constraint: Some(U128::from(79)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: WithdrawAmount::Full {
                                            at_least: Some(U128::from(79)),
                                        },
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": DEFAULT_REFERRER_ID,
                            },
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(280)),
                        deposit: NearToken::from_yoctonear(100),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }

    #[tokio::test]
    async fn custom_referrer_id_is_passed_in_operations() {
        assert_eq!(
            route(
                SwapRequest {
                    token_in: TokenId::Near,
                    token_out: TokenId::Nep141("ft".parse().unwrap()),
                    amount: Amount::AmountIn(100),
                    max_wait_ms: 1_000,
                    slippage: Slippage::Fixed {
                        slippage: "0.01".parse().unwrap(),
                    },
                    dexes: None,
                    trader_account_id: None,
                    signing_public_key: None,
                    referrer_id: Some("ref.near".parse().unwrap()),
                },
                &TestNetworkView::default(),
                &TestPlachQuotes::default(),
            )
            .await,
            Some(Route {
                deadline: None,
                has_slippage: true,
                estimated_amount: Amount::AmountOut(80),
                worst_case_amount: Amount::AmountOut(79),
                dex_id: DexId::Plach,
                execution_instructions: vec![ExecutionInstruction::NearTransaction {
                    receiver_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                    actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: "deposit_near".to_string(),
                        args: serde_json::to_vec(&serde_json::json!({
                            "operations": {
                                "operations": vec![
                                    Operation::SwapSimple {
                                        dex_id: PLACH_DEX_ID.to_string(),
                                        message: BASE64_STANDARD.encode(borsh::to_vec(&1u32).unwrap()),
                                        asset_in: AssetId::Near,
                                        asset_out: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128::from(100))),
                                        constraint: Some(U128::from(79)),
                                    },
                                    Operation::Withdraw {
                                        asset_id: AssetId::Nep141("ft".parse().unwrap()),
                                        amount: WithdrawAmount::Full {
                                            at_least: Some(U128::from(79)),
                                        },
                                        to: None,
                                        rescue_address: None,
                                    },
                                ],
                                "referrer": "ref.near",
                            },
                        }))
                        .unwrap(),
                        gas: Gas(NearGas::from_tgas(280)),
                        deposit: NearToken::from_yoctonear(100),
                    }))],
                }],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Nep141("ft".parse().unwrap()),
            })
        );
    }
}
