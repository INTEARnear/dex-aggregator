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
use intear_dex_types::DirectWithdrawAmount;
use intear_dex_types::FULL_FEE_FRACTION;
use lazy_static::lazy_static;
use near_min_api::{
    types::{
        near_account_id::AccountType, AccountId, AccountIdRef, Action, Balance, BlockHeight,
        BlockReference, CryptoHash, Finality, FunctionCallAction, Gas, NearGas, NearToken, U128,
    },
    utils::dec_format,
    QueryFinality, RpcClient,
};
use num_traits::{FromPrimitive, Zero};
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Deserializer};
pub use swap_router_types::fees::payable_referral_fees;
use tracing::{error, info, warn};

use crate::{
    providers::intear_plach::AssetId,
    types::{ExecutionInstruction, QuoteFees, Slippage, TokenId},
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
const SWAP_GAS_MARGIN: NearGas = NearGas::from_tgas(18);

/// Gas to attach to a call that swaps: the gas the DEX is predicted to burn for the swaps with a
/// margin, `reserved` for everything else the DEX needs in the call, its callbacks and transfers,
/// and `FT_TRANSFER_CALL_GAS` if it's `ft_transfer_call`.
pub fn swap_call_gas(swap_gas: NearGas, reserved: NearGas, ft_transfer_call: bool) -> Gas {
    let gas = swap_gas.as_gas()
        + SWAP_GAS_MARGIN.as_gas()
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

/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L43-L47
const ACTION_RECEIPT_EXECUTION_GAS: NearGas = NearGas::from_gas(108_059_500_000);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L88-L92
const TRANSFER_EXECUTION_GAS: NearGas = NearGas::from_gas(115_123_062_500);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/85.yaml#L16-L27
const CREATE_ACCOUNT_EXECUTION_GAS: NearGas = NearGas::from_gas(7_200_000_000_000);
/// https://github.com/near/nearcore/blob/44f7ae6cd7ef08bab604e20a473bf77e35d4c993/core/parameters/res/runtime_configs/parameters.yaml#L98-L102
const ADD_FULL_ACCESS_KEY_EXECUTION_GAS: NearGas = NearGas::from_gas(101_765_125_000);

/// Share of the swap gas the pathfinder predicts for dexes to burn compared to what is needed to be attached.
pub const SWAP_GAS_BURNT_PERCENT: u64 = 80;

fn near_transfer_gas(trader_account_id: Option<&AccountId>) -> NearGas {
    let account_creation_gas =
        match trader_account_id.map(|account_id| account_id.get_account_type()) {
            Some(AccountType::NearImplicitAccount) => {
                CREATE_ACCOUNT_EXECUTION_GAS.as_gas() + ADD_FULL_ACCESS_KEY_EXECUTION_GAS.as_gas()
            }
            Some(AccountType::EthImplicitAccount | AccountType::NearDeterministicAccount) => {
                CREATE_ACCOUNT_EXECUTION_GAS.as_gas()
            }
            Some(AccountType::NamedAccount) | None => 0,
        };
    NearGas::from_gas(
        ACTION_RECEIPT_EXECUTION_GAS.as_gas()
            + TRANSFER_EXECUTION_GAS.as_gas()
            + account_creation_gas,
    )
}

/// Gas a swap call is expected to cost with all the receipts it causes: `swap_gas` the DEX is
/// predicted to burn for the swaps, `overhead` for the rest of the DEX's and the tokens' work, and
/// `near_transfers` of NEAR to the trader. Its refunds burn gas too, but nobody pays for it.
pub fn swap_call_gas_paid(
    swap_gas: NearGas,
    overhead: NearGas,
    near_transfers: u64,
    trader_account_id: Option<&AccountId>,
) -> NearGas {
    NearGas::from_gas(
        swap_gas.as_gas()
            + overhead.as_gas()
            + near_transfers * near_transfer_gas(trader_account_id).as_gas(),
    )
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

/// The memo names the swap's referrer, for DEX backend to attribute the fee to
pub fn create_fee_ft_transfer_action(
    receiver_id: &AccountId,
    amount: Balance,
    swap_referrer_id: &str,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "ft_transfer".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "receiver_id": receiver_id,
            "amount": amount.to_string(),
            "memo": format!("Trading fee ({swap_referrer_id})"),
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

/// The memo names the swap's referrer, for DEX backend to attribute the fee to
pub fn create_rhea_fee_mft_transfer_action(
    token_id: &AccountId,
    receiver_id: &AccountId,
    amount: Balance,
    swap_referrer_id: &str,
) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "mft_transfer".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "token_id": token_id,
            "receiver_id": receiver_id,
            "amount": amount.to_string(),
            "memo": format!("Trading fee ({swap_referrer_id})"),
        }))
        .unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: NearToken::from_yoctonear(1),
    }))
}

pub fn create_intear_dex_withdraw_action(asset_id: &AssetId, amount: Balance) -> Action {
    Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "withdraw".to_string(),
        args: serde_json::to_vec(&serde_json::json!({
            "asset_id": asset_id,
            "amount": DirectWithdrawAmount::Exact(near_sdk::json_types::U128(amount)),
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

    fn ft_balance(
        &self,
        token_id: AccountId,
        account_id: &AccountId,
    ) -> impl Future<Output = Result<Balance, String>> + Send;

    fn ft_lockup_balance(
        &self,
        lockup_id: AccountId,
        account_id: &AccountId,
    ) -> impl Future<Output = Result<Balance, String>> + Send;

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

/// [`convert_to_nep141`] of `total`, and the fee transaction that sends `fees` of it to the
/// referrers with `ft_transfer`, to go after the swap. Referrers without storage on the NEP-141
/// get it in the same transaction first. NEAR is wrapped whole, so its fees are paid in wNEAR too.
pub async fn convert_to_nep141_with_fees(
    network: &impl NetworkView,
    token_id: &TokenId,
    trader_account_id: Option<AccountId>,
    total: Balance,
    fees: &[ReferrerFee],
    swap_referrer_id: &str,
) -> Option<(
    Vec<ExecutionInstruction>,
    Vec<ExecutionInstruction>,
    AccountId,
)> {
    let (to_nep141, nep141) = convert_to_nep141(token_id, trader_account_id, total).await?;
    let send_fees = if fees.is_empty() {
        Vec::new()
    } else {
        let mut actions = Vec::new();
        for fee in fees {
            if needs_storage_deposit_for_contract(network, &fee.referrer_id, &nep141).await {
                actions.push(
                    create_nep141_storage_deposit_action(network, &nep141, Some(&fee.referrer_id))
                        .await?,
                );
            }
        }
        actions.extend(fees.iter().map(|fee| {
            create_fee_ft_transfer_action(&fee.referrer_id, fee.amount, swap_referrer_id)
        }));
        vec![ExecutionInstruction::NearTransaction {
            receiver_id: nep141.clone(),
            actions,
            is_fee_transaction: true,
        }]
    };
    Some((to_nep141, send_fees, nep141))
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

    async fn ft_balance(
        &self,
        token_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Balance, String> {
        RPC_CLIENT
            .call::<near_sdk::json_types::U128>(
                token_id,
                "ft_balance_of",
                serde_json::json!({
                    "account_id": account_id,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .map(|balance| balance.0)
            .map_err(|err| format!("{err:?}"))
    }

    async fn ft_lockup_balance(
        &self,
        lockup_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Balance, String> {
        #[derive(Debug, Clone, Deserialize)]
        struct FtLockupAccount {
            locked: U128,
        }

        RPC_CLIENT
            .call::<Option<FtLockupAccount>>(
                lockup_id,
                "get_account",
                serde_json::json!({
                    "account_id": account_id,
                }),
                QueryFinality::Finality(Finality::DoomSlug),
            )
            .await
            .map(|account| account.map_or(0, |account| account.locked.0))
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
    ft_balances: HashMap<(AccountId, AccountId), Balance>,
    ft_lockup_balances: HashMap<(AccountId, AccountId), Balance>,
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
            ft_balances: HashMap::new(),
            ft_lockup_balances: HashMap::new(),
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

    pub(crate) fn with_ft_balance(
        mut self,
        token_id: &str,
        account_id: &str,
        amount: Balance,
    ) -> Self {
        self.ft_balances.insert(
            (token_id.parse().unwrap(), account_id.parse().unwrap()),
            amount,
        );
        self
    }

    pub(crate) fn with_ft_lockup_balance(
        mut self,
        lockup_id: &str,
        account_id: &str,
        amount: Balance,
    ) -> Self {
        self.ft_lockup_balances.insert(
            (lockup_id.parse().unwrap(), account_id.parse().unwrap()),
            amount,
        );
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

    async fn ft_balance(
        &self,
        token_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Balance, String> {
        Ok(self
            .ft_balances
            .get(&(token_id, account_id.clone()))
            .copied()
            .unwrap_or(0))
    }

    async fn ft_lockup_balance(
        &self,
        lockup_id: AccountId,
        account_id: &AccountId,
    ) -> Result<Balance, String> {
        Ok(self
            .ft_lockup_balances
            .get(&(lockup_id, account_id.clone()))
            .copied()
            .unwrap_or(0))
    }

    fn token_infos(&self) -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String> {
        self.tokens.clone()
    }

    fn current_block_height(&self) -> Result<BlockHeight, String> {
        self.block_height.clone()
    }
}

/// Registers the contract that a deposit of `token_id` goes to on the token, so it can receive it
async fn register_receiver_on_token(
    network: &impl NetworkView,
    contract_id: &AccountId,
    token_id: &AccountId,
) -> Option<Vec<ExecutionInstruction>> {
    if !needs_storage_deposit_for_contract(network, contract_id, token_id).await {
        return Some(vec![]);
    }
    Some(vec![ExecutionInstruction::NearTransaction {
        receiver_id: token_id.clone(),
        actions: vec![
            create_nep141_storage_deposit_action(network, token_id, Some(contract_id)).await?,
        ],
        is_fee_transaction: false,
    }])
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
    token_ids: &[&AccountId],
) -> Option<Vec<ExecutionInstruction>> {
    let mut unregistered = Vec::new();
    for &token_id in token_ids {
        if !unregistered.contains(&token_id)
            && !network.is_rhea_token_registered(account_id, token_id).await
        {
            unregistered.push(token_id);
        }
    }
    if unregistered.is_empty() {
        return Some(vec![]);
    }
    let lacking = rhea_storage_lacking(network, account_id, unregistered.len()).await?;
    let mut actions = Vec::new();
    if !lacking.is_zero() {
        actions.push(create_storage_deposit_action_for_contract(lacking, false));
    }
    actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
        method_name: "register_tokens".to_string(),
        args: serde_json::to_vec(&serde_json::json!({ "token_ids": unregistered })).unwrap(),
        gas: Gas(NearGas::from_tgas(10)),
        deposit: NearToken::from_yoctonear(1),
    })));
    Some(vec![ExecutionInstruction::NearTransaction {
        receiver_id: "v2.ref-finance.near".parse().unwrap(),
        actions,
        is_fee_transaction: false,
    }])
}

/// Storage deposits for the referrers that have no balance of `token_id` on Rhea yet, for the
/// storage a balance of it takes. Only the account itself can `register_tokens`, but a transfer
/// adds the balance when the account has the storage for it.
pub async fn rhea_fee_receivers_registration(
    network: &impl NetworkView,
    token_id: &AccountId,
    fees: &[ReferrerFee],
) -> Option<Vec<Action>> {
    let mut actions = Vec::new();
    for fee in fees {
        if network
            .is_rhea_token_registered(&fee.referrer_id, token_id)
            .await
        {
            continue;
        }
        let lacking = rhea_storage_lacking(network, &fee.referrer_id, 1).await?;
        if !lacking.is_zero() {
            actions.push(create_storage_deposit_action_for_someone(
                lacking,
                &fee.referrer_id,
                false,
            ));
        }
    }
    Some(actions)
}

const INTEAR_RECORD_BYTES: u128 = 40;
const ACCOUNT_ID_LENGTH_BYTES: u128 = 4;

/// Bytes of a balance of each of `asset_ids` for `account_id` on Intear DEX, and of its map of
/// balances when it has none yet
fn intear_balances_bytes(
    account_id: &AccountId,
    asset_ids: &[&AssetId],
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
    let assets_bytes = asset_ids
        .iter()
        .map(|asset_id| {
            let asset = borsh::to_vec(asset_id).unwrap().len() as u128;
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
    asset_ids: &[&AssetId],
    fee_receivers: &[(&AccountId, &AssetId)],
) -> Option<Vec<ExecutionInstruction>> {
    let contract_id: AccountId = "dex.intear.near".parse().unwrap();
    let mut unregistered = Vec::new();
    let mut any_registered = false;
    for &asset_id in asset_ids {
        if unregistered.contains(&asset_id) {
            continue;
        }
        if network
            .is_intear_asset_registered(account_id, asset_id)
            .await
        {
            any_registered = true;
        } else {
            unregistered.push(asset_id);
        }
    }
    let mut unregistered_fee_receivers = Vec::new();
    for &fee_receiver in fee_receivers {
        let (receiver_id, asset_id) = fee_receiver;
        if !unregistered_fee_receivers.contains(&fee_receiver)
            && !network
                .is_intear_asset_registered(receiver_id, asset_id)
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
    for &(receiver_id, asset_id) in &unregistered_fee_receivers {
        // Others can register assets for an account without storage, so it can have balances
        let balances_exist = network
            .intear_balances_exist(receiver_id)
            .await
            .inspect_err(|e| info!("Failed to get the balances of {receiver_id}: {e}"))
            .ok()?;
        bytes += intear_balances_bytes(receiver_id, &[asset_id], balances_exist);
    }
    let mut new_assets = unregistered.clone();
    for &(_, asset_id) in &unregistered_fee_receivers {
        if !new_assets.contains(&asset_id) {
            new_assets.push(asset_id);
        }
    }
    for asset_id in new_assets {
        if !network
            .is_intear_asset_in_custody(asset_id)
            .await
            .inspect_err(|e| info!("Failed to get the total in custody of {asset_id}: {e}"))
            .ok()?
        {
            bytes += 138 + borsh::to_vec(asset_id).unwrap().len() as u128;
        }
    }
    let needed = STORAGE_BYTE_COST.saturating_mul(bytes);
    let available = storage.map_or(NearToken::from_yoctonear(0), |storage| storage.available());
    let mut actions = Vec::new();
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
        actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "storage_deposit".to_string(),
            args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
            gas: Gas(NearGas::from_tgas(5)),
            deposit: needed.saturating_sub(available).max(min),
        })));
    }
    if !unregistered.is_empty() {
        actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "register_assets".to_string(),
            args: serde_json::to_vec(&serde_json::json!({ "asset_ids": unregistered })).unwrap(),
            gas: Gas(NearGas::from_tgas(5)),
            deposit: NearToken::from_yoctonear(1),
        })));
    }
    for (receiver_id, asset_id) in unregistered_fee_receivers {
        actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "register_assets".to_string(),
            args: serde_json::to_vec(&serde_json::json!({
                "asset_ids": [asset_id],
                "for": { "Account": receiver_id },
            }))
            .unwrap(),
            gas: Gas(NearGas::from_tgas(5)),
            deposit: NearToken::from_yoctonear(1),
        })));
    }
    Some(vec![ExecutionInstruction::NearTransaction {
        receiver_id: contract_id,
        actions,
        is_fee_transaction: false,
    }])
}

const MAX_TOKEN_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10);

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

pub async fn create_nep141_storage_deposit_action(
    network: &impl NetworkView,
    token_id: &AccountIdRef,
    account_id: Option<&AccountId>,
) -> Option<Action> {
    let amount = token_storage_deposit_amount(network, token_id).await?;
    Some(match account_id {
        Some(account_id) => create_storage_deposit_action_for_someone(amount, account_id, true),
        None => create_storage_deposit_action_for_contract(amount, true),
    })
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

const TOKEN_INFOS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
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
                is_fee_transaction,
            } => {
                if actions.is_empty() {
                    continue;
                }
                if let Some(ExecutionInstruction::NearTransaction {
                    receiver_id: prev_receiver_id,
                    actions: prev_actions,
                    is_fee_transaction: prev_is_fee_transaction,
                }) = optimized_execution_instructions.last_mut()
                {
                    // Fee transactions only merge with each other, so failed swap
                    // ft_trasnsfer_call doesn't merge with fee ft_transfer
                    if prev_receiver_id == receiver_id
                        && prev_is_fee_transaction == is_fee_transaction
                    {
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
                    is_fee_transaction: false,
                });
            }
            Some((transactions, WRAP_NEAR.parse::<AccountId>().unwrap()))
        }
        TokenId::Nep141(token_id) => Some((vec![], token_id.clone())),
        TokenId::Nep141OnRhea(token_id) => {
            // Rhea withdraws the whole balance for 0, so nothing is withdrawn instead
            let withdraw = if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: "v2.ref-finance.near".parse().unwrap(),
                    actions: vec![create_rhea_withdraw_action(token_id, amount, false)],
                    is_fee_transaction: false,
                }]
            } else {
                vec![]
            };
            Some((withdraw, token_id.clone()))
        }
        TokenId::TokenOnIntearDex(asset_id) => {
            let withdraw = if amount > 0 {
                vec![ExecutionInstruction::NearTransaction {
                    receiver_id: "dex.intear.near".parse().unwrap(),
                    actions: vec![create_intear_dex_withdraw_action(asset_id, amount)],
                    is_fee_transaction: false,
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
                            is_fee_transaction: false,
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
                    is_fee_transaction: false,
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
                is_fee_transaction: false,
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
                is_fee_transaction: false,
            };
            match asset_id {
                AssetId::Near => Some(vec![withdraw]),
                AssetId::Nep141(account_id) if account_id == WRAP_NEAR => Some(vec![
                    withdraw,
                    ExecutionInstruction::NearTransaction {
                        receiver_id: account_id.clone(),
                        actions: vec![create_unwrap_action(amount)],
                        is_fee_transaction: false,
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
) -> Option<Vec<ExecutionInstruction>> {
    let Some(trader_account_id) = trader_account_id.into() else {
        return Some(vec![]);
    };
    if !needs_storage_deposit_for_contract(network, &trader_account_id, contract_id).await {
        return Some(vec![]);
    }
    let min = network
        .storage_balance_bounds(contract_id.to_owned())
        .await
        .inspect_err(|e| info!("Failed to get the storage balance bounds of {contract_id}: {e}"))
        .ok()?
        .min();
    Some(vec![ExecutionInstruction::NearTransaction {
        receiver_id: contract_id.to_owned(),
        actions: vec![create_storage_deposit_action_for_contract(min, true)],
        is_fee_transaction: false,
    }])
}

/// Registers the trader where `token_id` is, with the storage it needs, if they aren't yet.
/// `None` if what it needs can't be found out.
pub async fn deposit_storage_if_needed(
    network: &impl NetworkView,
    token_id: &TokenId,
    trader_account_id: impl Into<Option<AccountId>>,
) -> Option<Vec<ExecutionInstruction>> {
    let Some(trader_account_id) = trader_account_id.into() else {
        return Some(vec![]);
    };
    match token_id {
        TokenId::Near => Some(vec![]),
        TokenId::Nep141(token_id) => {
            if !needs_storage_deposit_for_contract(network, &trader_account_id, token_id).await {
                return Some(vec![]);
            }
            Some(vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions: vec![create_nep141_storage_deposit_action(network, token_id, None).await?],
                is_fee_transaction: false,
            }])
        }
        TokenId::Nep141OnRhea(token_id) => {
            rhea_registration(network, &trader_account_id, &[token_id]).await
        }
        TokenId::TokenOnIntearDex(asset_id) => {
            register_assets_on_intear(network, &trader_account_id, &[asset_id], &[]).await
        }
    }
}

/// Registers the trader on `input_nep141`, the NEP-141 a route swaps from, when the route withdraws
/// or wraps `token_in` into their wallet. A wallet-level input is in their wallet already.
pub async fn register_input_in_wallet(
    network: &impl NetworkView,
    token_in: &TokenId,
    input_nep141: AccountId,
    trader_account_id: impl Into<Option<AccountId>>,
) -> Option<Vec<ExecutionInstruction>> {
    match token_in {
        TokenId::Nep141(_) => Some(vec![]),
        TokenId::Near | TokenId::Nep141OnRhea(_) | TokenId::TokenOnIntearDex(_) => {
            deposit_storage_if_needed(network, &TokenId::Nep141(input_nep141), trader_account_id)
                .await
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

pub async fn convert_to(
    network: &impl NetworkView,
    from: &TokenId,
    to: &TokenId,
    amount: Balance,
    trader_account_id: Option<AccountId>,
) -> Option<Vec<ExecutionInstruction>> {
    if from == to {
        return Some(vec![]);
    }
    let (Some(from_base_token), Some(to_base_token)) = (base_token(from), base_token(to)) else {
        return Some(vec![]);
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
                    is_fee_transaction: false,
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
                is_fee_transaction: false,
            }],
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
        Some(token_id) => {
            deposit_storage_if_needed(
                network,
                &TokenId::Nep141(token_id),
                trader_account_id.clone(),
            )
            .await?
        }
        None => vec![],
    };

    let (register, deposit) = match to {
        TokenId::Nep141OnRhea(token_id) => (
            [
                deposit_storage_if_needed(network, to, trader_account_id.clone()).await?,
                register_in_wallet,
                register_receiver_on_token(
                    network,
                    &"v2.ref-finance.near".parse().unwrap(),
                    token_id,
                )
                .await?,
            ]
            .concat(),
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions: vec![create_rhea_nep141_deposit_action(
                    &"v2.ref-finance.near".parse().unwrap(),
                    amount,
                )],
                is_fee_transaction: false,
            }],
        ),
        TokenId::TokenOnIntearDex(AssetId::Nep141(token_id)) => (
            [
                deposit_storage_if_needed(network, to, trader_account_id.clone()).await?,
                register_in_wallet,
                register_receiver_on_token(network, &"dex.intear.near".parse().unwrap(), token_id)
                    .await?,
            ]
            .concat(),
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: token_id.clone(),
                actions: vec![create_intear_nep141_deposit_action(
                    &"dex.intear.near".parse().unwrap(),
                    amount,
                )],
                is_fee_transaction: false,
            }],
        ),
        TokenId::TokenOnIntearDex(AssetId::Near) => (
            [
                deposit_storage_if_needed(network, to, trader_account_id.clone()).await?,
                register_in_wallet,
            ]
            .concat(),
            vec![ExecutionInstruction::NearTransaction {
                receiver_id: "dex.intear.near".parse().unwrap(),
                actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "deposit_near".to_string(),
                    args: serde_json::to_vec(&serde_json::json!({})).unwrap(),
                    gas: Gas(NearGas::from_tgas(10)),
                    deposit: NearToken::from_yoctonear(amount),
                }))],
                is_fee_transaction: false,
            }],
        ),
        TokenId::Near | TokenId::Nep141(_) => (register_in_wallet, vec![]),
        TokenId::TokenOnIntearDex(AssetId::Nep245(_, _) | AssetId::Nep171(_, _)) => {
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
                    is_fee_transaction: false,
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
                    is_fee_transaction: false,
                }]
            } else {
                vec![]
            }
        }
        (BaseTokenId::Nep141(_), BaseTokenId::Near) => vec![],
    };

    Some([register, withdraw, convert, deposit].concat())
}

/// Gas of the execution instructions: the fees of their transactions and function calls, and
/// `execution_gas` of each function call
fn total_gas(
    execution_instructions: &[ExecutionInstruction],
    execution_gas: impl Fn(&FunctionCallAction) -> u64,
) -> u64 {
    execution_instructions
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
                                    + execution_gas(call)
                            }
                            _ => 0,
                        })
                        .sum::<u64>()
            }
        })
        .sum()
}

/// What the execution instructions cost in gas if all gas attached to them is burnt, with the fees
/// of transactions and actions
pub fn max_gas_cost(execution_instructions: &[ExecutionInstruction]) -> NearToken {
    let gas = total_gas(execution_instructions, |call| call.gas.as_gas());
    pathfinder::MIN_GAS_PRICE.saturating_mul(gas.into())
}

/// Gas the execution instructions are expected to cost: the fees of their transactions and function
/// calls, and what each function call burns. `swap_call` costs the gas paired with it, other calls
/// burn what's attached to them but the swap margin. Refunds of unused gas burn gas too, but nobody
/// pays for it.
pub fn estimated_gas_paid(
    execution_instructions: &[ExecutionInstruction],
    swap_call: Option<(&Action, NearGas)>,
) -> NearGas {
    let transactions = optimize_execution_instructions(execution_instructions.to_vec());
    NearGas::from_gas(total_gas(&transactions, |call| match swap_call {
        Some((Action::FunctionCall(swap), gas_paid)) if **swap == *call => gas_paid.as_gas(),
        _ => call.gas.as_gas().saturating_sub(SWAP_GAS_MARGIN.as_gas()),
    }))
}

/// What the fee transactions of a route transfer, all in the NEP-141 of its input token
pub fn fee_transactions_amount(execution_instructions: &[ExecutionInstruction]) -> Balance {
    #[derive(Deserialize)]
    struct FtTransferArgs {
        #[serde(with = "dec_format")]
        amount: Balance,
    }
    execution_instructions
        .iter()
        .flat_map(|instruction| match instruction {
            ExecutionInstruction::NearTransaction {
                actions,
                is_fee_transaction: true,
                ..
            } => actions.as_slice(),
            ExecutionInstruction::NearTransaction { .. } => &[],
        })
        .map(|action| match action {
            Action::FunctionCall(call) if call.method_name == "ft_transfer" => {
                serde_json::from_slice::<FtTransferArgs>(&call.args)
                    .expect("Fee transfers are built with an amount")
                    .amount
            }
            _ => 0,
        })
        .sum()
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
