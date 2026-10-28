#[cfg(test)]
use std::collections::HashSet;
use std::{
    collections::HashMap,
    future::Future,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

use bigdecimal::BigDecimal;
use cached::proc_macro::cached;
use intear_dex_types::{AssetId, FULL_FEE_FRACTION};
use lazy_static::lazy_static;
use near_min_api::{
    types::{
        AccountId, AccountIdRef, Balance, BlockHeight, BlockReference, CryptoHash, Finality,
        NearToken,
    },
    utils::dec_format,
    QueryFinality, RpcClient,
};
use num_traits::{FromPrimitive, Zero};
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Deserializer};
pub use swap_router_types::fees::payable_referral_fees;
use tracing::{error, info};

use crate::{
    calls::{CallBlueprint, IntearAsset, RheaOutput},
    hooks::inteardex::INTEAR_DEX_CONTRACT_ID,
    providers::rhea::RHEA_CONTRACT_ID,
    route::RouteStep,
    types::{QuoteFees, Slippage, TokenId},
};

pub const WRAP_NEAR: &str = "wrap.near";
pub const DEFAULT_REFERRER_ID: &str = "dex-aggregator.intear.near";
pub const STORAGE_BYTE_COST: NearToken = NearToken::from_yoctonear(10u128.pow(19));

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

impl StorageBalanceBounds {
    pub fn min(&self) -> NearToken {
        self.min
    }
}

/// Rhea's storage of an account as `get_user_storage_state` counts it, which can use more than its
/// deposit from older storage rules
#[derive(Debug, Clone, Deserialize)]
pub struct RheaStorageState {
    deposit: NearToken,
    usage: NearToken,
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

    fn rhea_storage_state(
        &self,
        account_id: &AccountId,
    ) -> impl Future<Output = Result<Option<RheaStorageState>, String>> + Send;

    fn intear_balances_exist(
        &self,
        account_id: &AccountId,
    ) -> impl Future<Output = Result<bool, String>> + Send;

    fn is_intear_asset_in_custody(
        &self,
        asset_id: &AssetId,
    ) -> impl Future<Output = Result<bool, String>> + Send;

    fn token_infos(&self) -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String>;

    fn current_block_height(&self) -> Result<BlockHeight, String>;
}

/// A referrer's fee in the input token of a route
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferrerFee {
    pub referrer_id: AccountId,
    pub amount: Balance,
}

/// Each referrer's fee out of `total`, without the ones rounded down to nothing
pub fn referrer_fees_of_total(referral_fees: &QuoteFees, total: Balance) -> Vec<ReferrerFee> {
    referral_fees
        .0
        .iter()
        .map(|(referrer_id, fee_fraction)| ReferrerFee {
            referrer_id: referrer_id.clone(),
            // Split so it doesn't overflow for large amounts, rounded down
            amount: total / FULL_FEE_FRACTION as u128 * *fee_fraction as u128
                + total % FULL_FEE_FRACTION as u128 * *fee_fraction as u128
                    / FULL_FEE_FRACTION as u128,
        })
        .filter(|fee| fee.amount > 0)
        .collect()
}

pub fn total_fee_amount(fees: &[ReferrerFee]) -> Balance {
    fees.iter().map(|fee| fee.amount).sum()
}

/// [`convert_to_nep141`] of `total`, and the fee steps that send `fees` of it to the referrers with
/// `ft_transfer`, after the swap. Referrers without storage on the NEP-141 get it first. NEAR is
/// wrapped whole, so its fees are paid in wNEAR too.
pub fn convert_to_nep141_with_fees(
    token_id: &TokenId,
    trader_account_id: Option<&AccountId>,
    total: Balance,
    fees: &[ReferrerFee],
    swap_referrer_id: &AccountId,
) -> Option<(Vec<RouteStep>, Vec<RouteStep>, AccountId)> {
    let (to_nep141, nep141) = convert_to_nep141(token_id, trader_account_id, total)?;
    let fee_receivers_storage = fees
        .iter()
        .map(|fee| RouteStep::StorageDepositOnNep141IfNeeded {
            token_id: nep141.clone(),
            account_id: fee.referrer_id.clone(),
        });
    let fee_transfers = fees.iter().map(|fee| {
        RouteStep::Call(CallBlueprint::InputNep141FeeTransfer {
            token_id: nep141.clone(),
            receiver_id: fee.referrer_id.clone(),
            amount: fee.amount,
            swap_referrer_id: swap_referrer_id.clone(),
        })
    });
    let fee_steps = fee_receivers_storage.chain(fee_transfers).collect();
    Some((to_nep141, fee_steps, nep141))
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

    async fn rhea_storage_state(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<RheaStorageState>, String> {
        RPC_CLIENT
            .call::<Option<RheaStorageState>>(
                "v2.ref-finance.near".parse().unwrap(),
                "get_user_storage_state",
                serde_json::json!({
                    "account_id": account_id,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .map_err(|err| format!("{err:?}"))
    }

    async fn intear_balances_exist(&self, account_id: &AccountId) -> Result<bool, String> {
        RPC_CLIENT
            .call::<Vec<serde_json::Value>>(
                "dex.intear.near".parse().unwrap(),
                "registered_assets_of",
                serde_json::json!({
                    "account_id": account_id,
                    "from_index": 0,
                    "limit": 1,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .map(|assets| !assets.is_empty())
            .map_err(|err| format!("{err:?}"))
    }

    async fn is_intear_asset_in_custody(&self, asset_id: &AssetId) -> Result<bool, String> {
        RPC_CLIENT
            .call::<Option<near_sdk::json_types::U128>>(
                "dex.intear.near".parse().unwrap(),
                "total_in_custody",
                serde_json::json!({
                    "asset_id": asset_id,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .map(|total| total.is_some())
            .map_err(|err| format!("{err:?}"))
    }

    fn token_infos(&self) -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String> {
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
    tokens: Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String>,
    block_height: Result<BlockHeight, String>,
    native_balances: HashMap<AccountId, NearToken>,
    storage_balance_bounds_errors: HashSet<AccountId>,
    intear_balances: HashSet<AccountId>,
    intear_untracked_assets: HashSet<AssetId>,
    rhea_storage_states: HashMap<AccountId, RheaStorageState>,
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
            storage_balance_bounds_errors: Default::default(),
            intear_balances: Default::default(),
            intear_untracked_assets: Default::default(),
            rhea_storage_states: HashMap::new(),
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

    pub(crate) fn with_storage_balance_bounds_error(mut self, contract_id: &str) -> Self {
        self.storage_balance_bounds_errors
            .insert(contract_id.parse().unwrap());
        self
    }

    /// Rhea's own count of an account's storage, which can use more than its deposit
    pub(crate) fn with_rhea_storage_state(
        mut self,
        account_id: &str,
        deposit: NearToken,
        usage: NearToken,
    ) -> Self {
        self.rhea_storage_states.insert(
            account_id.parse().unwrap(),
            RheaStorageState { deposit, usage },
        );
        self
    }

    pub(crate) fn with_intear_balances(mut self, account_id: &str) -> Self {
        self.intear_balances.insert(account_id.parse().unwrap());
        self
    }

    pub(crate) fn with_intear_asset_untracked(mut self, asset_id: &AssetId) -> Self {
        self.intear_untracked_assets.insert(asset_id.clone());
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

    pub(crate) fn with_tokens(mut self, tokens: HashMap<BaseTokenId, TokenInfo>) -> Self {
        self.tokens = Ok(Arc::new(tokens));
        self
    }

    /// Prices NEAR, wNEAR and the `ft` and `other` tokens the provider tests swap, which DEXes
    /// with routes need to weigh gas against amounts
    pub(crate) fn with_test_prices(self) -> Self {
        [
            BaseTokenId::Near,
            BaseTokenId::Nep141(WRAP_NEAR.parse().unwrap()),
        ]
        .into_iter()
        .chain(["ft", "other"].map(|token| BaseTokenId::Nep141(token.parse().unwrap())))
        .fold(self, |network, token_id| {
            network.with_price(token_id, BigDecimal::from_str("1e-20").unwrap())
        })
    }

    pub(crate) fn with_price(mut self, token_id: BaseTokenId, price_usd_raw: BigDecimal) -> Self {
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
        if self.storage_balance_bounds_errors.contains(&contract_id) {
            return Err("Failed to get storage balance bounds".to_string());
        }
        if let Some(bounds) = self.storage_balance_bounds.get(&contract_id) {
            return Ok(bounds.clone());
        }
        let min = match contract_id.as_str() {
            "dex.intear.near" => NearToken::from_millinear(5),
            "dclv2.ref-labs.near" => NearToken::from_millinear(500),
            "v2.ref-finance.near" => {
                NearToken::from_yoctonear(102 * STORAGE_BYTE_COST.as_yoctonear())
            }
            _ => NearToken::from_yoctonear(125 * STORAGE_BYTE_COST.as_yoctonear()),
        };
        Ok(StorageBalanceBounds { min })
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

    async fn rhea_storage_state(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<RheaStorageState>, String> {
        if let Some(state) = self.rhea_storage_states.get(account_id) {
            return Ok(Some(state.clone()));
        }
        Ok(self
            .storage_balances
            .get(&("v2.ref-finance.near".parse().unwrap(), account_id.clone()))
            .map(|storage| RheaStorageState {
                deposit: storage.total,
                usage: storage.total.saturating_sub(storage.available),
            }))
    }

    async fn intear_balances_exist(&self, account_id: &AccountId) -> Result<bool, String> {
        Ok(self.intear_balances.contains(account_id))
    }

    async fn is_intear_asset_in_custody(&self, asset_id: &AssetId) -> Result<bool, String> {
        Ok(!self.intear_untracked_assets.contains(asset_id))
    }

    fn token_infos(&self) -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String> {
        self.tokens.clone()
    }

    fn current_block_height(&self) -> Result<BlockHeight, String> {
        self.block_height.clone()
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

/// Rhea's storage of an account: 102 bytes, and 148 more for each token it has a balance of
const RHEA_ACCOUNT_STORAGE_BYTES: u128 = 102;
const RHEA_TOKEN_STORAGE_BYTES: u128 = 148;

/// NEAR the account lacks on Rhea for a balance of `new_tokens` more tokens.
async fn rhea_storage_lacking(
    network: &impl NetworkView,
    account_id: &AccountId,
    new_tokens: usize,
) -> Option<NearToken> {
    let (deposit, usage) = match network
        .rhea_storage_state(account_id)
        .await
        .inspect_err(|e| info!("Failed to get the Rhea storage of {account_id}: {e}"))
        .ok()?
    {
        Some(state) => (state.deposit, state.usage),
        None => (
            NearToken::from_yoctonear(0),
            STORAGE_BYTE_COST.saturating_mul(RHEA_ACCOUNT_STORAGE_BYTES),
        ),
    };
    let needed = usage.saturating_add(
        STORAGE_BYTE_COST.saturating_mul(RHEA_TOKEN_STORAGE_BYTES * new_tokens as u128),
    );
    Some(needed.saturating_sub(deposit))
}

/// Registers the tokens the account has no balance of on Rhea yet, with the storage deposit they
/// lack. `swap` adds a balance of every token it outputs, also the ones in the middle of a route.
pub async fn rhea_registration(
    network: &impl NetworkView,
    account_id: &AccountId,
    token_ids: &[AccountId],
) -> Option<Vec<CallBlueprint>> {
    let mut unregistered = Vec::new();
    for token_id in token_ids {
        if !unregistered.contains(token_id)
            && !network.is_rhea_token_registered(account_id, token_id).await
        {
            unregistered.push(token_id.clone());
        }
    }
    if unregistered.is_empty() {
        return Some(vec![]);
    }
    let lacking = rhea_storage_lacking(network, account_id, unregistered.len()).await?;
    let mut calls = Vec::new();
    if !lacking.is_zero() {
        calls.push(CallBlueprint::RheaStorageDeposit {
            account_id: None,
            amount: lacking,
        });
    }
    calls.push(CallBlueprint::RheaRegisterTokens {
        token_ids: unregistered,
    });
    Some(calls)
}

/// Storage deposits for the referrers that have no balance of `token_id` on Rhea yet, for the
/// storage a balance of it takes. Only the account itself can `register_tokens`, but a transfer
/// adds the balance when the account has the storage for it.
pub async fn rhea_fee_receivers_registration(
    network: &impl NetworkView,
    token_id: &AccountId,
    receiver_ids: &[AccountId],
) -> Option<Vec<CallBlueprint>> {
    let mut calls = Vec::new();
    for receiver_id in receiver_ids {
        if network
            .is_rhea_token_registered(receiver_id, token_id)
            .await
        {
            continue;
        }
        let lacking = rhea_storage_lacking(network, receiver_id, 1).await?;
        if !lacking.is_zero() {
            calls.push(CallBlueprint::RheaStorageDeposit {
                account_id: Some(receiver_id.clone()),
                amount: lacking,
            });
        }
    }
    Some(calls)
}

const INTEAR_RECORD_BYTES: u128 = 40;
const ACCOUNT_ID_LENGTH_BYTES: u128 = 4;

/// Bytes of a balance of each of `assets` for `account_id` on Intear DEX, and of its map of
/// balances when it has none yet
fn intear_balances_bytes(
    account_id: &AccountId,
    assets: &[&IntearAsset],
    balances_exist: bool,
) -> u128 {
    let id = account_id.len() as u128;
    let key_prefix = 1 + ACCOUNT_ID_LENGTH_BYTES + id + 1;
    let balances_bytes = if balances_exist {
        0
    } else {
        INTEAR_RECORD_BYTES
            + (1 + ACCOUNT_ID_LENGTH_BYTES + id)
            + 4
            + (4 + key_prefix)
            + (4 + key_prefix)
    };
    let assets_bytes = assets
        .iter()
        .map(|&asset| {
            let asset = borsh::to_vec(&AssetId::from(asset)).unwrap().len() as u128;
            (INTEAR_RECORD_BYTES + key_prefix + 4 + asset) + (INTEAR_RECORD_BYTES + 32 + 16 + 4)
        })
        .sum::<u128>();
    balances_bytes + assets_bytes
}

/// Registers the assets the account has no balance of on Intear DEX yet, and each fee receiver's
/// asset it hasn't registered, so the engine pays it its fee. The account deposits the storage
/// all of them lack.
pub async fn register_assets_on_intear(
    network: &impl NetworkView,
    account_id: &AccountId,
    assets: &[IntearAsset],
    fee_receivers: &[(AccountId, IntearAsset)],
) -> Option<Vec<CallBlueprint>> {
    let contract_id: AccountId = INTEAR_DEX_CONTRACT_ID.parse().unwrap();
    let mut unregistered = Vec::new();
    let mut any_registered = false;
    for asset in assets {
        if unregistered.contains(&asset) {
            continue;
        }
        if network
            .is_intear_asset_registered(account_id, &AssetId::from(asset))
            .await
        {
            any_registered = true;
        } else {
            unregistered.push(asset);
        }
    }
    let mut unregistered_fee_receivers = Vec::new();
    for fee_receiver in fee_receivers {
        let (receiver_id, asset) = fee_receiver;
        if !unregistered_fee_receivers.contains(&fee_receiver)
            && !network
                .is_intear_asset_registered(receiver_id, &AssetId::from(asset))
                .await
        {
            unregistered_fee_receivers.push(fee_receiver);
        }
    }
    if unregistered.is_empty() && unregistered_fee_receivers.is_empty() {
        return Some(vec![]);
    }
    let storage = network
        .storage_balance(contract_id.clone(), account_id)
        .await
        .inspect_err(|e| info!("Failed to get the storage balance of {account_id}: {e}"))
        .ok()?;
    let mut bytes = match storage {
        Some(_) => 0,
        None => INTEAR_RECORD_BYTES + (1 + ACCOUNT_ID_LENGTH_BYTES + account_id.len() as u128) + 32,
    };
    if !unregistered.is_empty() {
        // The account's map of balances exists once it has registered any asset
        let balances_exist = any_registered
            || (storage.is_some()
                && network
                    .intear_balances_exist(account_id)
                    .await
                    .inspect_err(|e| info!("Failed to get the balances of {account_id}: {e}"))
                    .ok()?);
        bytes += intear_balances_bytes(account_id, &unregistered, balances_exist);
    }
    for (receiver_id, asset) in &unregistered_fee_receivers {
        // Others can register assets for an account without storage, so it can have balances
        let balances_exist = network
            .intear_balances_exist(receiver_id)
            .await
            .inspect_err(|e| info!("Failed to get the balances of {receiver_id}: {e}"))
            .ok()?;
        bytes += intear_balances_bytes(receiver_id, &[asset], balances_exist);
    }
    let mut new_assets = unregistered.clone();
    for (_, asset) in &unregistered_fee_receivers {
        if !new_assets.contains(&asset) {
            new_assets.push(asset);
        }
    }
    for asset in new_assets {
        let asset_id = AssetId::from(asset);
        if !network
            .is_intear_asset_in_custody(&asset_id)
            .await
            .inspect_err(|e| info!("Failed to get the total in custody of {asset_id}: {e}"))
            .ok()?
        {
            bytes += 138 + borsh::to_vec(&asset_id).unwrap().len() as u128;
        }
    }
    let needed = STORAGE_BYTE_COST.saturating_mul(bytes);
    let available = storage.map_or(NearToken::from_yoctonear(0), |storage| storage.available());
    let mut calls = Vec::new();
    if needed > available {
        // storage_deposit takes no less than the minimum, also from registered accounts
        let min = network
            .storage_balance_bounds(contract_id.clone())
            .await
            .inspect_err(|e| {
                info!("Failed to get the storage balance bounds of {contract_id}: {e}")
            })
            .ok()?
            .min();
        calls.push(CallBlueprint::IntearDexStorageDeposit {
            amount: needed.saturating_sub(available).max(min),
        });
    }
    if !unregistered.is_empty() {
        calls.push(CallBlueprint::IntearDexRegisterAssets {
            asset_ids: unregistered.into_iter().cloned().collect(),
            for_account: None,
        });
    }
    for (receiver_id, asset) in unregistered_fee_receivers {
        calls.push(CallBlueprint::IntearDexRegisterAssets {
            asset_ids: vec![asset.clone()],
            for_account: Some(receiver_id.clone()),
        });
    }
    Some(calls)
}

const MAX_TOKEN_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10);

/// The minimum storage of the token, None if it's too much to route through it
pub async fn token_storage_deposit_amount(
    network: &impl NetworkView,
    token_id: &AccountIdRef,
) -> Option<NearToken> {
    let min = network
        .storage_balance_bounds(token_id.to_owned())
        .await
        .inspect_err(|e| info!("Failed to get the storage balance bounds of {token_id}: {e}"))
        .ok()?
        .min();
    if min >= MAX_TOKEN_STORAGE_DEPOSIT {
        info!("{token_id} takes {min} of storage, not routing through it");
        return None;
    }
    Some(min)
}

lazy_static! {
    pub static ref REQWEST_CLIENT: Client = ClientBuilder::new()
        .timeout(Duration::from_secs(65)) // max 60 seconds + latency
        .user_agent("Intear Swap Router")
        .build()
        .unwrap();
    /// The RPCs to use, in order: a request goes to the next one when one fails
    pub static ref RPC_URLS: Vec<String> = std::env::var("RPC_URLS")
        .unwrap_or_else(|_| {
            "https://rpc.intea.rs,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                .to_string()
        })
        .split(',')
        .map(|url| url.to_string())
        .collect();
    pub static ref RPC_CLIENT: RpcClient = RpcClient::new(RPC_URLS.iter());

    static ref TOKEN_INFOS: RwLock<Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String>> =
        RwLock::new(Err("Token infos are not loaded yet".to_string()));
    static ref BLOCK_HEIGHT: RwLock<Result<BlockHeight, String>> =
        RwLock::new(Err("Block height is not loaded yet".to_string()));
    static ref FINAL_BLOCK_HASH: RwLock<Result<CryptoHash, String>> =
        RwLock::new(Err("Final block hash is not loaded yet".to_string()));
}

pub const TOKEN_INFOS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const BLOCK_HEIGHT_REFRESH_INTERVAL: Duration = Duration::from_millis(200);
const FINAL_BLOCK_HASH_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// A recent final block's hash, for quotes to sign their transactions with
pub fn final_block_hash() -> Result<CryptoHash, String> {
    FINAL_BLOCK_HASH.read().unwrap().clone()
}

/// Loads token infos, the current block height and a final block's hash, then keeps refreshing
/// them in the background, so requests never wait for them.
pub async fn start_background_refresh() {
    tokio::join!(
        refresh(&TOKEN_INFOS, fetch_token_infos),
        refresh(&BLOCK_HEIGHT, fetch_current_block_height),
        refresh(&FINAL_BLOCK_HASH, fetch_final_block_hash),
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
    tokio::spawn(refresh_loop(
        &FINAL_BLOCK_HASH,
        fetch_final_block_hash,
        FINAL_BLOCK_HASH_REFRESH_INTERVAL,
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
    token_in: &BaseTokenId,
    token_out: &BaseTokenId,
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
    tokens_result: &Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String>,
    token_id: &BaseTokenId,
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

async fn fetch_token_infos() -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String> {
    let endpoint = std::env::var("INTEAR_PRICES_API_ENDPOINT")
        .unwrap_or_else(|_| "https://prices.intear.tech".to_string());
    let url = format!("{endpoint}/tokens");
    let mut tokens = REQWEST_CLIENT
        .get(url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("Failed to get all tokens: {err}"))?
        .json::<HashMap<BaseTokenId, TokenInfo>>()
        .await
        .map_err(|err| format!("Failed to parse all tokens: {err}"))?;
    if let Some(wnear_info) = tokens.get(&BaseTokenId::Nep141(WRAP_NEAR.parse().unwrap())) {
        tokens.insert(BaseTokenId::Near, wnear_info.clone());
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

async fn fetch_final_block_hash() -> Result<CryptoHash, String> {
    RPC_CLIENT
        .block(BlockReference::Finality(Finality::Final))
        .await
        .map(|block| block.header.hash)
        .map_err(|err| format!("Failed to get the final block: {err:?}"))
}

/// Steps that move `amount` of the token to its NEP-141 in the trader's wallet, and the NEP-141.
/// None for tokens without one.
pub fn convert_to_nep141(
    token_id: &TokenId,
    trader_account_id: Option<&AccountId>,
    amount: Balance,
) -> Option<(Vec<RouteStep>, AccountId)> {
    let wrap_near = || {
        RouteStep::Call(CallBlueprint::WrapNear {
            amount: NearToken::from_yoctonear(amount),
        })
    };
    match token_id {
        TokenId::Near => {
            let wrap = if amount > 0 {
                vec![wrap_near()]
            } else {
                vec![]
            };
            Some((wrap, WRAP_NEAR.parse().unwrap()))
        }
        TokenId::Nep141(token_id) => Some((vec![], token_id.clone())),
        TokenId::Nep141OnRhea(token_id) => {
            // Rhea withdraws the whole balance for 0, so nothing is withdrawn instead
            let withdraw = if amount > 0 {
                vec![RouteStep::Call(CallBlueprint::RheaWithdraw {
                    token_id: token_id.clone(),
                    amount,
                    output: RheaOutput::Nep141,
                })]
            } else {
                vec![]
            };
            Some((withdraw, token_id.clone()))
        }
        TokenId::TokenOnIntearDex(AssetId::Nep245(_, _) | AssetId::Nep171(_, _)) => None,
        TokenId::TokenOnIntearDex(AssetId::Near) => {
            let withdraw_and_wrap = if amount > 0 {
                vec![
                    RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                        asset: IntearAsset::Near,
                        amount,
                        trader_account_id: trader_account_id.cloned(),
                    }),
                    wrap_near(),
                ]
            } else {
                vec![]
            };
            Some((withdraw_and_wrap, WRAP_NEAR.parse().unwrap()))
        }
        TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)) => {
            let withdraw = if amount > 0 {
                vec![RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                    asset: IntearAsset::Nep141(token_id.clone()),
                    amount,
                    trader_account_id: trader_account_id.cloned(),
                })]
            } else {
                vec![]
            };
            Some((withdraw, token_id.clone()))
        }
    }
}

/// Steps that move `amount` of the token to native NEAR in the trader's wallet. None for tokens
/// that aren't NEAR, or nothing of them.
pub fn convert_to_native(
    token_id: &TokenId,
    trader_account_id: Option<&AccountId>,
    amount: NearToken,
) -> Option<Vec<RouteStep>> {
    if token_id != &TokenId::Near && amount.is_zero() {
        return None;
    }
    let unwrap_near = || {
        RouteStep::Call(CallBlueprint::UnwrapNear {
            amount,
            trader_account_id: trader_account_id.cloned(),
        })
    };
    match token_id {
        TokenId::Near => Some(vec![]),
        TokenId::Nep141(token_id) if token_id == WRAP_NEAR => Some(vec![unwrap_near()]),
        TokenId::Nep141OnRhea(token_id) if token_id == WRAP_NEAR => {
            Some(vec![RouteStep::Call(CallBlueprint::RheaWithdraw {
                token_id: token_id.clone(),
                amount: amount.as_yoctonear(),
                output: RheaOutput::Near {
                    trader_account_id: trader_account_id.cloned(),
                },
            })])
        }
        TokenId::TokenOnIntearDex(AssetId::Near) => {
            Some(vec![RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::Near,
                amount: amount.as_yoctonear(),
                trader_account_id: trader_account_id.cloned(),
            })])
        }
        TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)) if token_id == WRAP_NEAR => {
            Some(vec![
                RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                    asset: IntearAsset::Nep141(token_id.clone()),
                    amount: amount.as_yoctonear(),
                    trader_account_id: trader_account_id.cloned(),
                }),
                unwrap_near(),
            ])
        }
        TokenId::Nep141(_)
        | TokenId::Nep141OnRhea(_)
        | TokenId::TokenOnIntearDex(
            AssetId::Nep141(_) | AssetId::Nep245(_, _) | AssetId::Nep171(_, _),
        ) => None,
    }
}

/// Registers the trader where `token_id` is, with the storage it needs, if they aren't yet
pub fn deposit_storage_if_needed(
    token_id: &TokenId,
    trader_account_id: Option<&AccountId>,
) -> Vec<RouteStep> {
    let Some(trader_account_id) = trader_account_id.cloned() else {
        return vec![];
    };
    match token_id {
        TokenId::Near => vec![],
        TokenId::Nep141(token_id) => vec![RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id: token_id.clone(),
            trader_account_id,
        }],
        TokenId::Nep141OnRhea(token_id) => vec![RouteStep::RegisterOnRheaIfNeeded {
            trader_account_id,
            token_ids: vec![token_id.clone()],
        }],
        TokenId::TokenOnIntearDex(asset_id) => vec![RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id,
            asset_ids: vec![IntearAsset::try_from(asset_id)
                .expect("Routes go only through NEAR and NEP-141s on Intear DEX")],
            fee_receivers: vec![],
        }],
    }
}

/// Registers the trader on `input_nep141`, the NEP-141 a route swaps from, when the route withdraws
/// or wraps `token_in` into their wallet. A wallet-level input is in their wallet already.
pub fn register_input_in_wallet(
    token_in: &TokenId,
    input_nep141: AccountId,
    trader_account_id: Option<&AccountId>,
) -> Vec<RouteStep> {
    match token_in {
        TokenId::Nep141(_) => vec![],
        TokenId::Near | TokenId::Nep141OnRhea(_) | TokenId::TokenOnIntearDex(_) => {
            deposit_storage_if_needed(&TokenId::Nep141(input_nep141), trader_account_id)
        }
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BaseTokenId {
    Near,
    Nep141(AccountId),
}

impl<'de> Deserialize<'de> for BaseTokenId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let token_id: TokenId = serde::Deserialize::deserialize(deserializer)?;
        base_token(&token_id).ok_or_else(|| serde::de::Error::custom("invalid token id"))
    }
}

pub fn base_token(token_id: &TokenId) -> Option<BaseTokenId> {
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

pub fn convert_to(
    from: &TokenId,
    to: &TokenId,
    amount: Balance,
    trader_account_id: Option<&AccountId>,
) -> Vec<RouteStep> {
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
                vec![RouteStep::Call(CallBlueprint::RheaWithdraw {
                    token_id: token_id.clone(),
                    amount,
                    output: if unwrap_near {
                        RheaOutput::Near {
                            trader_account_id: trader_account_id.cloned(),
                        }
                    } else {
                        RheaOutput::Nep141
                    },
                })],
                if unwrap_near {
                    BaseTokenId::Near
                } else {
                    from_base_token
                },
            )
        }
        TokenId::TokenOnIntearDex(asset_id) => (
            vec![RouteStep::Call(CallBlueprint::IntearDexWithdraw {
                asset: IntearAsset::try_from(asset_id)
                    .expect("Assets without a base token have no conversion"),
                amount,
                trader_account_id: trader_account_id.cloned(),
            })],
            from_base_token,
        ),
        TokenId::Near | TokenId::Nep141(_) => (vec![], from_base_token),
    };

    let received_in_wallet = match (from, &from_base_token, &to_base_token) {
        (
            TokenId::Nep141OnRhea(token_id) | TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)),
            BaseTokenId::Nep141(_),
            _,
        ) => Some(token_id.clone()),
        (_, BaseTokenId::Near, BaseTokenId::Nep141(_)) => Some(WRAP_NEAR.parse().unwrap()),
        _ => None,
    };
    let register_in_wallet = match received_in_wallet {
        Some(token_id) => deposit_storage_if_needed(&TokenId::Nep141(token_id), trader_account_id),
        None => vec![],
    };

    let (register, deposit) = match to {
        TokenId::Nep141OnRhea(token_id) => (
            [
                deposit_storage_if_needed(to, trader_account_id),
                register_in_wallet,
                vec![RouteStep::StorageDepositOnNep141IfNeeded {
                    token_id: token_id.clone(),
                    account_id: RHEA_CONTRACT_ID.parse().unwrap(),
                }],
            ]
            .concat(),
            vec![RouteStep::Call(CallBlueprint::Nep141DepositOnRhea {
                token_id: token_id.clone(),
                amount,
            })],
        ),
        TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)) => (
            [
                deposit_storage_if_needed(to, trader_account_id),
                register_in_wallet,
                vec![RouteStep::StorageDepositOnNep141IfNeeded {
                    token_id: token_id.clone(),
                    account_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
                }],
            ]
            .concat(),
            vec![RouteStep::Call(CallBlueprint::Nep141DepositOnIntearDex {
                token_id: token_id.clone(),
                amount,
            })],
        ),
        TokenId::TokenOnIntearDex(AssetId::Near) => (
            [
                deposit_storage_if_needed(to, trader_account_id),
                register_in_wallet,
            ]
            .concat(),
            vec![RouteStep::Call(CallBlueprint::IntearDexDepositNear {
                amount: NearToken::from_yoctonear(amount),
                operations: None,
            })],
        ),
        TokenId::Near | TokenId::Nep141(_) => (register_in_wallet, vec![]),
        TokenId::TokenOnIntearDex(AssetId::Nep245(_, _) | AssetId::Nep171(_, _)) => {
            (vec![], vec![])
        }
    };

    let convert = match (&from_base_token, &to_base_token) {
        (BaseTokenId::Near, BaseTokenId::Near)
        | (BaseTokenId::Nep141(_), BaseTokenId::Nep141(_)) => vec![],
        (BaseTokenId::Near, BaseTokenId::Nep141(_)) if amount > 0 => {
            vec![RouteStep::Call(CallBlueprint::WrapNear {
                amount: NearToken::from_yoctonear(amount),
            })]
        }
        (BaseTokenId::Nep141(token_id), BaseTokenId::Near)
            if token_id == WRAP_NEAR && amount > 0 =>
        {
            vec![RouteStep::Call(CallBlueprint::UnwrapNear {
                amount: NearToken::from_yoctonear(amount),
                trader_account_id: trader_account_id.cloned(),
            })]
        }
        (BaseTokenId::Near, BaseTokenId::Nep141(_))
        | (BaseTokenId::Nep141(_), BaseTokenId::Near) => {
            vec![]
        }
    };

    [register, withdraw, convert, deposit].concat()
}

pub fn price_raw(network: &impl NetworkView, token_id: &BaseTokenId) -> Option<BigDecimal> {
    network
        .token_infos()
        .ok()?
        .get(token_id)
        .map(|info| info.price_usd_raw.clone())
        .filter(|price| !price.is_zero())
        .or_else(|| {
            info!("No price of {token_id:?}");
            None
        })
}

#[cfg(test)]
#[path = "shared_utils_tests.rs"]
mod tests;
