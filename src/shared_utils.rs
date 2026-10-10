use std::{
    collections::HashMap,
    future::Future,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

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
use tracing::{error, info, warn};

use crate::{
    providers::intear_plach::AssetId,
    types::{ExecutionInstruction, Slippage, TokenId},
};

pub const WRAP_NEAR: &str = "wrap.near";
pub const DEFAULT_REFERRER_ID: &str = "dex-aggregator.intear.near";
pub const STORAGE_BYTE_COST: NearToken = NearToken::from_yoctonear(10u128.pow(19));

// Fees burnt on top of the gas attached to actions, `send_not_sir` + `execution` of mainnet's
// runtime config, since a trader's transactions are received by other accounts

/// Fee for the receipt of a transaction
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L43-L47
const TRANSACTION_FEE_GAS: NearGas = NearGas::from_gas(108_059_500_000 + 108_059_500_000);
/// Fee for a function call action
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/66.yaml#L3-L14
const FUNCTION_CALL_FEE_GAS: NearGas = NearGas::from_gas(200_000_000_000 + 780_000_000_000);
/// Fee for each byte of a function call's method name and arguments
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/69.yaml#L34-L45
const FUNCTION_CALL_BYTE_FEE_GAS: NearGas = NearGas::from_gas(47_683_715 + 2_235_934);

const MAX_SWAP_GAS: NearGas = NearGas::from_tgas(900);
const FT_TRANSFER_CALL_GAS: NearGas = NearGas::from_tgas(10);
const SWAP_GAS_MARGIN_FIXED_PART: NearGas = NearGas::from_tgas(20);
const SWAP_GAS_MARGIN_PERCENT_PART: u64 = 10;

/// Gas to attach to a call that swaps: the gas the DEX is predicted to burn for the swaps with a
/// margin, `reserved` for everything else the DEX needs in the call, its callbacks and transfers,
/// and `FT_TRANSFER_CALL_GAS` if it's `ft_transfer_call`.
pub fn swap_call_gas(swap_gas: NearGas, reserved: NearGas, ft_transfer_call: bool) -> Gas {
    let gas = swap_gas.as_gas() * (100 + SWAP_GAS_MARGIN_PERCENT_PART) / 100
        + SWAP_GAS_MARGIN_FIXED_PART.as_gas()
        + reserved.as_gas()
        + if ft_transfer_call {
            FT_TRANSFER_CALL_GAS.as_gas()
        } else {
            0
        };
    let gas = NearGas::from_gas(gas);
    if gas > MAX_SWAP_GAS {
        warn!("A swap needs {gas}, attaching {MAX_SWAP_GAS}");
        return Gas(MAX_SWAP_GAS);
    }
    Gas(gas)
}

pub fn create_wrap_action(amount: NearToken) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "near_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
        gas: Gas(NearGas::from_tgas(5)),
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
        gas: Gas(NearGas::from_tgas(30)),
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

#[derive(Debug, Clone, Deserialize)]
pub struct StorageDeposit {
    available: NearToken,
    total: NearToken,
}

impl StorageDeposit {
    pub fn available(&self) -> NearToken {
        self.available
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageBalanceBounds {
    min: NearToken,
}

pub trait NetworkView: Send + Sync {
    fn storage_balance(
        &self,
        contract_id: AccountId,
        account_id: &AccountId,
    ) -> impl Future<Output = Result<Option<StorageDeposit>, String>> + Send;

    fn storage_balance_bounds(
        &self,
        contract_id: AccountId,
    ) -> impl Future<Output = Result<StorageBalanceBounds, String>> + Send;

    fn is_rhea_token_registered(
        &self,
        account_id: &AccountId,
        token_id: &AccountId,
    ) -> impl Future<Output = bool> + Send;

    fn is_intear_asset_registered(
        &self,
        account_id: &AccountId,
        asset_id: &AssetId,
    ) -> impl Future<Output = bool> + Send;

    fn token_infos(&self) -> Result<Arc<HashMap<TokenId, TokenInfo>>, String>;

    fn current_block_height(&self) -> Result<BlockHeight, String>;
}

pub struct Mainnet;

impl NetworkView for Mainnet {
    async fn storage_balance(
        &self,
        contract_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Option<StorageDeposit>, String> {
        RPC_CLIENT
            .call::<Option<StorageDeposit>>(
                contract_id,
                "storage_balance_of",
                serde_json::json!({
                    "account_id": account_id,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .inspect_err(|err| println!("Error checking storage balance: {err:?}"))
            .map_err(|err| format!("{err:?}"))
    }

    async fn storage_balance_bounds(
        &self,
        contract_id: AccountId,
    ) -> Result<StorageBalanceBounds, String> {
        get_storage_balance_bounds(contract_id)
            .await
            .inspect_err(|err| println!("Error checking storage balance bounds: {err:?}"))
    }

    async fn is_rhea_token_registered(&self, account_id: &AccountId, token_id: &AccountId) -> bool {
        RPC_CLIENT
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
            .inspect_err(|err| println!("Error checking rhea token registration: {err:?}"))
            .unwrap_or_default()
    }

    async fn is_intear_asset_registered(&self, account_id: &AccountId, asset_id: &AssetId) -> bool {
        RPC_CLIENT
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
            .inspect_err(|err| println!("Error checking intear asset registration: {err:?}"))
            .unwrap_or_default()
    }

    fn token_infos(&self) -> Result<Arc<HashMap<TokenId, TokenInfo>>, String> {
        TOKEN_INFOS.read().unwrap().clone()
    }

    fn current_block_height(&self) -> Result<BlockHeight, String> {
        BLOCK_HEIGHT.read().unwrap().clone()
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestNetworkView {
    storage_balances: HashMap<(AccountId, AccountId), StorageDeposit>,
    storage_balance_bounds: HashMap<AccountId, StorageBalanceBounds>,
    rhea_token_registers: HashMap<(AccountId, AccountId), bool>,
    intear_asset_registers: HashMap<(AccountId, AssetId), bool>,
    tokens: Result<Arc<HashMap<TokenId, TokenInfo>>, String>,
    block_height: Result<BlockHeight, String>,
    native_balances: HashMap<AccountId, NearToken>,
}

#[cfg(test)]
impl Default for TestNetworkView {
    fn default() -> Self {
        Self {
            storage_balances: HashMap::new(),
            storage_balance_bounds: HashMap::new(),
            rhea_token_registers: HashMap::new(),
            intear_asset_registers: HashMap::new(),
            tokens: Ok(Arc::default()),
            block_height: Ok(1_000_000),
            native_balances: HashMap::new(),
        }
    }
}

#[cfg(test)]
impl TestNetworkView {
    pub(crate) fn with_storage(
        mut self,
        contract_id: &str,
        account_id: &str,
        total: NearToken,
        available: NearToken,
    ) -> Self {
        self.storage_balances.insert(
            (contract_id.parse().unwrap(), account_id.parse().unwrap()),
            StorageDeposit { total, available },
        );
        self
    }

    pub(crate) fn with_storage_balance_bounds(mut self, contract_id: &str, min: NearToken) -> Self {
        self.storage_balance_bounds
            .insert(contract_id.parse().unwrap(), StorageBalanceBounds { min });
        self
    }

    pub(crate) fn with_rhea_registered(
        mut self,
        account_id: &str,
        token_id: &str,
        registered: bool,
    ) -> Self {
        self.rhea_token_registers.insert(
            (account_id.parse().unwrap(), token_id.parse().unwrap()),
            registered,
        );
        self
    }

    pub(crate) fn with_intear_registered(
        mut self,
        account_id: &str,
        asset_id: &AssetId,
        registered: bool,
    ) -> Self {
        self.intear_asset_registers
            .insert((account_id.parse().unwrap(), asset_id.clone()), registered);
        self
    }

    pub(crate) fn with_tokens(mut self, tokens: HashMap<TokenId, TokenInfo>) -> Self {
        self.tokens = Ok(Arc::new(tokens));
        self
    }

    /// Prices NEAR, wNEAR and the `ft` and `other` tokens the provider tests swap, which DEXes
    /// with routes need to weigh gas against amounts
    pub(crate) fn with_test_prices(self) -> Self {
        [TokenId::Near, TokenId::Nep141(WRAP_NEAR.parse().unwrap())]
            .into_iter()
            .chain(["ft", "other"].map(|token| TokenId::Nep141(token.parse().unwrap())))
            .fold(self, |network, token_id| {
                network.with_price(token_id, BigDecimal::from_str("1e-20").unwrap())
            })
    }

    pub(crate) fn with_price(mut self, token_id: TokenId, price_usd_raw: BigDecimal) -> Self {
        Arc::make_mut(self.tokens.as_mut().unwrap()).insert(
            token_id,
            TokenInfo {
                price_usd_raw_24h_ago: price_usd_raw.clone(),
                price_usd_raw,
                circulating_supply: 0,
                liquidity_usd: BigDecimal::zero(),
                volume_usd_24h: BigDecimal::zero(),
                created_at: 0,
            },
        );
        self
    }

    pub(crate) fn with_tokens_error(mut self) -> Self {
        self.tokens = Err("Failed to get all tokens".to_string());
        self
    }

    pub(crate) fn with_block_height(mut self, height: u64) -> Self {
        self.block_height = Ok(height);
        self
    }

    pub(crate) fn with_block_height_error(mut self) -> Self {
        self.block_height = Err("Failed to get current block".to_string());
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_native_balance(mut self, account_id: &str, amount: NearToken) -> Self {
        self.native_balances
            .insert(account_id.parse().unwrap(), amount);
        self
    }
}

#[cfg(test)]
impl NetworkView for TestNetworkView {
    async fn storage_balance(
        &self,
        contract_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Option<StorageDeposit>, String> {
        Ok(self
            .storage_balances
            .get(&(contract_id, account_id.clone()))
            .cloned())
    }

    async fn storage_balance_bounds(
        &self,
        contract_id: AccountId,
    ) -> Result<StorageBalanceBounds, String> {
        self.storage_balance_bounds
            .get(&contract_id)
            .cloned()
            .ok_or_else(|| "Failed to get storage balance bounds".to_string())
    }

    async fn is_rhea_token_registered(&self, account_id: &AccountId, token_id: &AccountId) -> bool {
        self.rhea_token_registers
            .get(&(account_id.clone(), token_id.clone()))
            .copied()
            .unwrap_or(false)
    }

    async fn is_intear_asset_registered(&self, account_id: &AccountId, asset_id: &AssetId) -> bool {
        self.intear_asset_registers
            .get(&(account_id.clone(), asset_id.clone()))
            .copied()
            .unwrap_or(false)
    }

    fn token_infos(&self) -> Result<Arc<HashMap<TokenId, TokenInfo>>, String> {
        self.tokens.clone()
    }

    fn current_block_height(&self) -> Result<BlockHeight, String> {
        self.block_height.clone()
    }
}

async fn create_ft_deposit_registrations(
    network: &impl NetworkView,
    contract_id: &AccountId,
    token_id: &AccountId,
    trader_account_id: Option<AccountId>,
) -> Vec<ExecutionInstruction> {
    let mut actions = vec![];
    if let Some(trader_account_id) = trader_account_id {
        if needs_storage_deposit_for_contract(network, &trader_account_id, token_id).await {
            actions.push(create_nep141_storage_deposit_action(network, token_id, None).await);
        }
    }
    if needs_storage_deposit_for_contract(network, contract_id, token_id).await {
        actions
            .push(create_nep141_storage_deposit_action(network, token_id, Some(contract_id)).await);
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

pub async fn needs_storage_deposit(
    network: &impl NetworkView,
    account_id: &AccountId,
    token_id: &TokenId,
) -> bool {
    match token_id {
        TokenId::Near => false,
        TokenId::Nep141(token_id) => {
            needs_storage_deposit_for_contract(network, account_id, token_id).await
        }
        TokenId::Nep141OnRhea(token_id) => {
            let is_registered = network.is_rhea_token_registered(account_id, token_id).await;
            let has_storage_deposit = network
                .storage_balance("v2.ref-finance.near".parse().unwrap(), account_id)
                .await
                .ok()
                .flatten()
                .map(|s| s.available > NearToken::from_millinear(10))
                .unwrap_or(false);
            !has_storage_deposit || !is_registered
        }
        TokenId::TokenOnIntearDex(asset_id) => {
            let is_registered = network
                .is_intear_asset_registered(account_id, asset_id)
                .await;
            let has_storage_deposit = network
                .storage_balance("dex.intear.near".parse().unwrap(), account_id)
                .await
                .ok()
                .flatten()
                .map(|s| s.available > NearToken::from_millinear(1))
                .unwrap_or(false);
            !has_storage_deposit || !is_registered
        }
    }
}

pub async fn needs_storage_deposit_for_contract(
    network: &impl NetworkView,
    account_id: &AccountId,
    contract_id: &AccountIdRef,
) -> bool {
    let Ok(Some(storage_deposit)) = network
        .storage_balance(contract_id.to_owned(), account_id)
        .await
    else {
        return true;
    };
    storage_deposit.total.is_zero()
}

pub async fn create_storage_deposit_action(
    network: &impl NetworkView,
    token_id: &TokenId,
) -> Vec<ExecutionInstruction> {
    match token_id {
        TokenId::Nep141(token_account_id) => vec![ExecutionInstruction::NearTransaction {
            receiver_id: token_account_id.clone(),
            actions: vec![
                create_nep141_storage_deposit_action(network, token_account_id, None).await,
            ],
        }],
        TokenId::Near => panic!("NEAR doesn't need a storage deposit"),
        TokenId::Nep141OnRhea(token_id) => {
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "v2.ref-finance.near".parse().unwrap(),
                actions: vec![
                    create_storage_deposit_action_for_contract(
                        NearToken::from_millinear(10),
                        false,
                    ),
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

const DEFAULT_TOKEN_STORAGE_DEPOSIT: NearToken = NearToken::from_micronear(1250); // 0.00125 NEAR
const MAX_TOKEN_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10); // 0.01 NEAR

pub async fn token_storage_deposit_amount(
    network: &impl NetworkView,
    token_id: &AccountIdRef,
) -> NearToken {
    match network.storage_balance_bounds(token_id.to_owned()).await {
        Ok(bounds) if bounds.min < MAX_TOKEN_STORAGE_DEPOSIT => bounds.min,
        _ => DEFAULT_TOKEN_STORAGE_DEPOSIT,
    }
}

pub async fn create_nep141_storage_deposit_action(
    network: &impl NetworkView,
    token_id: &AccountIdRef,
    account_id: Option<&AccountId>,
) -> Action {
    let amount = token_storage_deposit_amount(network, token_id).await;
    match account_id {
        Some(account_id) => create_storage_deposit_action_for_someone(amount, account_id, true),
        None => create_storage_deposit_action_for_contract(amount, true),
    }
}

pub fn create_storage_deposit_action_for_contract(
    amount: NearToken,
    registration_only: bool,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "storage_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "registration_only": registration_only,
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: amount,
    }))
}

pub fn create_storage_deposit_action_for_someone(
    amount: NearToken,
    account_id: &AccountId,
    registration_only: bool,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "storage_deposit".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "registration_only": registration_only,
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

    static ref TOKEN_INFOS: RwLock<Result<Arc<HashMap<TokenId, TokenInfo>>, String>> =
        RwLock::new(Err("Token infos are not loaded yet".to_string()));
    static ref BLOCK_HEIGHT: RwLock<Result<BlockHeight, String>> =
        RwLock::new(Err("Block height is not loaded yet".to_string()));
}

const TOKEN_INFOS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const BLOCK_HEIGHT_REFRESH_INTERVAL: Duration = Duration::from_millis(200);

/// Loads token infos and the current block height, then keeps refreshing them in
/// the background, so requests never wait for them.
pub async fn start_background_refresh() {
    tokio::join!(
        refresh(&TOKEN_INFOS, fetch_token_infos),
        refresh(&BLOCK_HEIGHT, fetch_current_block_height),
    );
    tokio::spawn(refresh_loop(
        &TOKEN_INFOS,
        fetch_token_infos,
        TOKEN_INFOS_REFRESH_INTERVAL,
    ));
    tokio::spawn(refresh_loop(
        &BLOCK_HEIGHT,
        fetch_current_block_height,
        BLOCK_HEIGHT_REFRESH_INTERVAL,
    ));
}

async fn refresh_loop<T, Fut>(
    value: &RwLock<Result<T, String>>,
    fetch: impl Fn() -> Fut,
    interval: Duration,
) where
    Fut: Future<Output = Result<T, String>>,
{
    loop {
        tokio::time::sleep(interval).await;
        refresh(value, &fetch).await;
    }
}

/// A failed refresh keeps the previous value, so a short outage doesn't affect requests.
async fn refresh<T, Fut>(value: &RwLock<Result<T, String>>, fetch: impl Fn() -> Fut)
where
    Fut: Future<Output = Result<T, String>>,
{
    match fetch().await {
        Ok(fresh) => *value.write().unwrap() = Ok(fresh),
        Err(err) => error!("{err}, keeping the previous value"),
    }
}

pub fn get_slippage(
    network: &impl NetworkView,
    slippage: Slippage,
    token_in: &TokenId,
    token_out: &TokenId,
) -> BigDecimal {
    match slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => {
            let tokens_result = network.token_infos();
            let optimal_slippage_scale_input =
                slippage_scale_for_token(network, &tokens_result, token_in);
            let optimal_slippage_scale_output =
                slippage_scale_for_token(network, &tokens_result, token_out);
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

fn token_volatility_scale(token_info: &TokenInfo, current_block_height: u64) -> BigDecimal {
    if token_info.created_at > current_block_height.saturating_sub(100) {
        BigDecimal::from(1)
    } else if token_info.created_at > current_block_height.saturating_sub(1000) {
        BigDecimal::from_f64(0.6).unwrap()
    } else {
        let mut scale = BigDecimal::from(0);

        if !token_info.price_usd_raw.is_zero() {
            let price_change_24h =
                (token_info.price_usd_raw_24h_ago.clone() - token_info.price_usd_raw.clone()).abs();
            let price_change_24h_relative = price_change_24h / token_info.price_usd_raw.clone();

            if price_change_24h_relative > BigDecimal::from_f64(0.5).unwrap() {
                scale += BigDecimal::from_f64(0.1).unwrap();
            }
            if price_change_24h_relative > BigDecimal::from_f64(0.2).unwrap() {
                scale += BigDecimal::from_f64(0.05).unwrap();
            }

            let market_cap =
                BigDecimal::from(token_info.circulating_supply) * token_info.price_usd_raw.clone();
            if !market_cap.is_zero() {
                let volume_to_mcap_ratio = token_info.volume_usd_24h.clone() / market_cap;
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
                token_info.volume_usd_24h.clone() / token_info.liquidity_usd.clone();
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
}

fn slippage_scale_for_token(
    network: &impl NetworkView,
    tokens_result: &Result<Arc<HashMap<TokenId, TokenInfo>>, String>,
    token_id: &TokenId,
) -> BigDecimal {
    match tokens_result {
        Ok(tokens) => {
            if let Some(token_info) = tokens.get(token_id) {
                match network.current_block_height() {
                    Ok(height) => token_volatility_scale(token_info, height),
                    Err(_) => BigDecimal::from_f64(0.005).unwrap(),
                }
            } else {
                BigDecimal::from_f64(0.8).unwrap()
            }
        }
        Err(_) => BigDecimal::from_f64(0.005).unwrap(),
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

async fn fetch_token_infos() -> Result<Arc<HashMap<TokenId, TokenInfo>>, String> {
    let endpoint = std::env::var("INTEAR_PRICES_API_ENDPOINT")
        .unwrap_or_else(|_| "https://prices.intear.tech".to_string());
    let url = format!("{endpoint}/tokens");
    let mut tokens = REQWEST_CLIENT
        .get(url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("Failed to get all tokens: {err}"))?
        .json::<HashMap<TokenId, TokenInfo>>()
        .await
        .map_err(|err| format!("Failed to parse all tokens: {err}"))?;
    if let Some(wnear_info) = tokens.get(&TokenId::Nep141(WRAP_NEAR.parse().unwrap())) {
        tokens.insert(TokenId::Near, wnear_info.clone());
    }
    Ok(Arc::new(tokens))
}

#[cached(time = 3600, result = true)]
pub async fn get_storage_balance_bounds(
    contract_id: AccountId,
) -> Result<StorageBalanceBounds, String> {
    RPC_CLIENT
        .call::<StorageBalanceBounds>(
            contract_id,
            "storage_balance_bounds",
            serde_json::json!({}),
            QueryFinality::Finality(Finality::DoomSlug),
        )
        .await
        .map_err(|err| format!("{err:?}"))
}

async fn fetch_current_block_height() -> Result<BlockHeight, String> {
    RPC_CLIENT
        .block(BlockReference::Finality(Finality::None))
        .await
        .map(|block| block.header.height)
        .map_err(|err| format!("Failed to get current block: {err:?}"))
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
    network: &impl NetworkView,
    contract_id: &AccountIdRef,
    trader_account_id: impl Into<Option<AccountId>>,
    amount: NearToken,
) -> Vec<ExecutionInstruction> {
    if let Some(trader_account_id) = trader_account_id.into() {
        if needs_storage_deposit_for_contract(network, &trader_account_id, contract_id).await {
            return vec![ExecutionInstruction::NearTransaction {
                receiver_id: contract_id.to_owned(),
                actions: vec![create_storage_deposit_action_for_contract(amount, true)],
            }];
        }
    }
    vec![]
}

pub async fn deposit_storage_if_needed(
    network: &impl NetworkView,
    token_id: &TokenId,
    trader_account_id: impl Into<Option<AccountId>>,
) -> Vec<ExecutionInstruction> {
    if let Some(trader_account_id) = trader_account_id.into() {
        if needs_storage_deposit(network, &trader_account_id, token_id).await {
            create_storage_deposit_action(network, token_id).await
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
    network: &impl NetworkView,
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
                deposit_storage_if_needed(network, to, trader_account_id.clone()).await,
                create_ft_deposit_registrations(
                    network,
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
                deposit_storage_if_needed(network, to, trader_account_id.clone()).await,
                create_ft_deposit_registrations(
                    network,
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
            deposit_storage_if_needed(network, to, trader_account_id.clone()).await,
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

/// What the execution instructions cost in gas if all gas attached to them is burnt, with the fees
/// of their transactions and actions. Most of the attached gas is usually refunded.
pub fn max_gas_cost(execution_instructions: &[ExecutionInstruction]) -> NearToken {
    let gas = execution_instructions
        .iter()
        .map(|instruction| match instruction {
            ExecutionInstruction::NearTransaction { actions, .. } => {
                TRANSACTION_FEE_GAS.as_gas()
                    + actions
                        .iter()
                        .map(|action| match action {
                            Action::FunctionCall(call) => {
                                FUNCTION_CALL_FEE_GAS.as_gas()
                                    + FUNCTION_CALL_BYTE_FEE_GAS.as_gas()
                                        * (call.method_name.len() + call.args.len()) as u64
                                    + call.gas.as_gas()
                            }
                            _ => 0,
                        })
                        .sum::<u64>()
            }
        })
        .sum::<u64>();
    pathfinder::MIN_GAS_PRICE.saturating_mul(gas as u128)
}

/// NEAR the execution instructions attach to storage deposits
pub fn storage_deposits(execution_instructions: &[ExecutionInstruction]) -> NearToken {
    execution_instructions
        .iter()
        .flat_map(|instruction| match instruction {
            ExecutionInstruction::NearTransaction { actions, .. } => actions,
        })
        .filter_map(|action| match action {
            Action::FunctionCall(call) if call.method_name == "storage_deposit" => {
                Some(call.deposit)
            }
            _ => None,
        })
        .fold(NearToken::from_yoctonear(0), NearToken::saturating_add)
}

/// Current USD price of a raw unit of `token_id` (a yoctoNEAR for NEAR), `None` if it has no price
pub fn price_raw(network: &impl NetworkView, token_id: &TokenId) -> Option<BigDecimal> {
    let token_id = match base_token(token_id)? {
        BaseTokenId::Near => TokenId::Near,
        BaseTokenId::Nep141(account_id) => TokenId::Nep141(account_id),
    };
    network
        .token_infos()
        .ok()?
        .get(&token_id)
        .map(|info| info.price_usd_raw.clone())
        .filter(|price| !price.is_zero())
        .or_else(|| {
            info!("No price of {token_id}");
            None
        })
}

#[cfg(test)]
#[path = "shared_utils_tests.rs"]
mod tests;
