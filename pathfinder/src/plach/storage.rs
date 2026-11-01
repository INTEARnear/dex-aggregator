use std::collections::HashMap;

use borsh::BorshDeserialize;
use near_min_api::types::AccountId;
use swap_router_types::indexed::{AccountState, Watch};
use xyk_dex_types::{AssetWithBalance, FeeConfigurationV2, PayoutGroups, TokenAmount, TotalShares};

use super::{INTEAR_DEX_CONTRACT_ID, PLACH_DEX_ID, PoolsSource};

/// `dex_storage: LookupMap<(DexId, Vec<u8>), Vec<u8>>` of the DEX engine
const DEX_STORAGE_PREFIX: u8 = 1;
/// `pools: Vector<Pool>` of the xyk DEX, keys are the prefix and u32 index
const POOLS_PREFIX: u8 = 0;

/// Keys of the Plach DEX in the engine's storage
fn storage_prefix() -> Vec<u8> {
    let (deployer, id) = PLACH_DEX_ID.split_once('/').unwrap();
    let mut prefix = vec![DEX_STORAGE_PREFIX];
    prefix.extend(borsh::to_vec(&(deployer, id)).unwrap());
    prefix
}

/// The xyk DEX's `Pool`, with the `LookupMap` of public pool providers read as its prefix: the
/// map can't be dropped outside of a contract
#[derive(BorshDeserialize)]
#[borsh(use_discriminant = true)]
#[repr(u8)]
pub enum StoredPool {
    PrivateV2 {
        assets: (AssetWithBalance, AssetWithBalance),
        _owner_id: AccountId,
        fees: FeeConfigurationV2,
        _locked: bool,
    } = 3,
    LaunchV2 {
        quote_asset: AssetWithBalance,
        launched_asset: AssetWithBalance,
        fees: FeeConfigurationV2,
        phantom_liquidity: TokenAmount,
    } = 5,
    PublicV3 {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: FeeConfigurationV2,
        _providers_prefix: Vec<u8>,
        total_shares: TotalShares,
        payout_groups: Box<PayoutGroups>,
    } = 6,
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
    })
}
