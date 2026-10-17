use std::collections::HashMap;

use borsh::BorshDeserialize;
use near_min_api::types::AccountId;
use near_sdk::json_types::U128;
use pool_indexer::{AccountState, Watch};
use xyk_dex_types::{AssetWithBalance, CurrentFees, FeeConfiguration, SharesBalance};

use super::{AssetId, FeeAssets, INTEAR_DEX_CONTRACT_ID, PLACH_DEX_ID, PoolsSource};

/// `dex_storage: LookupMap<(DexId, Vec<u8>), Vec<u8>>` of the DEX engine
const DEX_STORAGE_PREFIX: u8 = 1;
/// `pools: Vector<Pool>` of the xyk DEX, keys are the prefix and u32 index
const POOLS_PREFIX: u8 = 0;
/// `fees_collected_by_users: LookupMap<(AccountId, AssetId), U128>` of the xyk DEX: an account
/// only collects fees in the assets it has an entry for
const FEES_COLLECTED_BY_USERS_PREFIX: u8 = 2;

/// Keys of the Plach DEX in the engine's storage
fn storage_prefix() -> Vec<u8> {
    let (deployer, id) = PLACH_DEX_ID.split_once('/').unwrap();
    let mut prefix = vec![DEX_STORAGE_PREFIX];
    prefix.extend(borsh::to_vec(&(deployer, id)).unwrap());
    prefix
}

/// The xyk DEX's `Pool`, with each `LookupMap` of public pool shares read as its prefix: the map
/// can't be dropped outside of a contract
#[derive(BorshDeserialize)]
pub enum StoredPool {
    PrivateV1 {
        assets: (AssetWithBalance, AssetWithBalance),
        _owner_id: AccountId,
        fees: CurrentFees,
    },
    PublicV1 {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: CurrentFees,
        _user_shares_prefix: Vec<u8>,
        _total_shares: Option<SharesBalance>,
    },
    LaunchV1 {
        near_amount: U128,
        launched_asset: AssetWithBalance,
        fees: FeeConfiguration,
        phantom_liquidity_near: U128,
    },
    PrivateV2 {
        assets: (AssetWithBalance, AssetWithBalance),
        _owner_id: AccountId,
        fees: FeeConfiguration,
        _locked: bool,
    },
    PublicV2 {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: FeeConfiguration,
        _user_shares_prefix: Vec<u8>,
        _total_shares: Option<SharesBalance>,
    },
    /// The first asset is the quote asset, which the phantom liquidity is in
    LaunchV2 {
        quote_asset: AssetWithBalance,
        launched_asset: AssetWithBalance,
        fees: FeeConfiguration,
        phantom_liquidity: U128,
    },
}

pub fn watch() -> Watch {
    Watch {
        account_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![storage_prefix()],
    }
}

pub(super) fn pools_source(state: &AccountState) -> Result<PoolsSource, anyhow::Error> {
    let prefix = storage_prefix();
    let mut storage = HashMap::new();
    for (key, value) in state.with_prefix(&prefix) {
        let inner_key = borsh::from_slice::<Vec<u8>>(&key[prefix.len()..])
            .map_err(|e| anyhow::anyhow!("Invalid key {key:02x?}: {e}"))?;
        let inner_value = borsh::from_slice::<Vec<u8>>(value)
            .map_err(|e| anyhow::anyhow!("Invalid value of {inner_key:02x?}: {e}"))?;
        storage.insert(inner_key, inner_value);
    }
    let contract_state = storage
        .get(b"STATE".as_slice())
        .ok_or_else(|| anyhow::anyhow!("STATE is missing"))?;
    // `pools: Vector<Pool>` is the first field, its length is the first u32
    let pool_count = u32::from_le_bytes(
        contract_state
            .get(..4)
            .ok_or_else(|| anyhow::anyhow!("STATE is too short"))?
            .try_into()
            .unwrap(),
    );
    let mut pools = Vec::with_capacity(pool_count as usize);
    for pool_id in 0..pool_count {
        let mut key = vec![POOLS_PREFIX];
        key.extend(pool_id.to_le_bytes());
        let value = storage
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("Pool {pool_id} is missing"))?;
        let pool = borsh::from_slice::<StoredPool>(value)
            .map_err(|e| anyhow::anyhow!("Invalid pool {pool_id}: {e}"))?;
        pools.push(pool);
    }
    Ok(PoolsSource {
        block_height: state.block.height,
        timestamp: state.block.timestamp_nanosec,
        pools,
        fee_assets: fee_assets(&storage)?,
    })
}

/// The assets each account collects fees in
fn fee_assets(storage: &HashMap<Vec<u8>, Vec<u8>>) -> Result<FeeAssets, anyhow::Error> {
    let mut fee_assets = FeeAssets::new();
    for key in storage.keys() {
        if let Some((&FEES_COLLECTED_BY_USERS_PREFIX, entry)) = key.split_first() {
            let (account_id, asset_id) = borsh::from_slice::<(AccountId, AssetId)>(entry)
                .map_err(|e| anyhow::anyhow!("Invalid collected fees key {key:02x?}: {e}"))?;
            fee_assets.entry(account_id).or_default().insert(asset_id);
        }
    }
    Ok(fee_assets)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn key(prefix: u8, entry: impl borsh::BorshSerialize) -> Vec<u8> {
        [vec![prefix], borsh::to_vec(&entry).unwrap()].concat()
    }

    #[test]
    fn every_account_comes_with_the_assets_it_collects_fees_in() {
        let referrer: AccountId = "ref.near".parse().unwrap();
        let other: AccountId = "pool-owner.near".parse().unwrap();
        let usdt = AssetId::Nep141("usdt.tether-token.near".parse().unwrap());
        let storage = HashMap::from([
            (
                key(FEES_COLLECTED_BY_USERS_PREFIX, (&referrer, &AssetId::Near)),
                borsh::to_vec(&0u128).unwrap(),
            ),
            (
                key(FEES_COLLECTED_BY_USERS_PREFIX, (&referrer, &usdt)),
                borsh::to_vec(&5u128).unwrap(),
            ),
            (
                key(FEES_COLLECTED_BY_USERS_PREFIX, (&other, &usdt)),
                borsh::to_vec(&5u128).unwrap(),
            ),
            // Referral settings the DEX had before referral fees came with swaps
            (key(3, &referrer), vec![0; 9]),
        ]);
        assert_eq!(
            fee_assets(&storage).unwrap(),
            FeeAssets::from([
                (referrer, HashSet::from([AssetId::Near, usdt.clone()])),
                (other, HashSet::from([usdt])),
            ])
        );
    }
}
