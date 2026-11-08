use std::collections::HashMap;

use borsh::BorshDeserialize;
use swap_router_types::indexed::{AccountState, Watch};

pub const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
/// `dex_storage: LookupMap<(DexId, Vec<u8>), Vec<u8>>` of the DEX engine
const DEX_STORAGE_PREFIX: u8 = 1;
/// `pools: Vector<Pool>` of the xyk and amp DEXes, keys are the prefix and u32 index
const POOLS_PREFIX: u8 = 0;

/// Keys of the DEX in the engine's storage
fn storage_prefix(dex_id: &str) -> Vec<u8> {
    let (deployer, id) = dex_id.split_once('/').unwrap();
    let mut prefix = vec![DEX_STORAGE_PREFIX];
    prefix.extend(borsh::to_vec(&(deployer, id)).unwrap());
    prefix
}

pub fn watch(dex_id: &str) -> Watch {
    Watch {
        account_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![storage_prefix(dex_id)],
    }
}

/// The pools of a DEX whose state starts with `pools: Vector<Pool>`, as the xyk and amp DEXes'
pub fn pools<T: BorshDeserialize>(
    state: &AccountState,
    dex_id: &str,
) -> Result<Vec<T>, anyhow::Error> {
    let prefix = storage_prefix(dex_id);
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
        let pool = borsh::from_slice::<T>(value)
            .map_err(|e| anyhow::anyhow!("Invalid pool {pool_id}: {e}"))?;
        pools.push(pool);
    }
    Ok(pools)
}

#[cfg(test)]
mod tests {
    use swap_router_types::indexed::BlockInfo;

    use super::*;

    /// A key and value of `dex_storage` of the engine, as it stores the DEX's storage
    fn engine_entry(dex_id: &str, key: Vec<u8>, value: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
        let (deployer, id) = dex_id.split_once('/').unwrap();
        let mut engine_key = vec![DEX_STORAGE_PREFIX];
        engine_key.extend(borsh::to_vec(&((deployer, id), key)).unwrap());
        (engine_key, borsh::to_vec(&value).unwrap())
    }

    fn pool_key(pool_id: u32) -> Vec<u8> {
        [vec![POOLS_PREFIX], pool_id.to_le_bytes().to_vec()].concat()
    }

    #[test]
    fn reads_the_pools_of_one_dex_from_the_engine() {
        // The state has the pools' Vector length and prefix, then the other fields
        let state = |pool_count: u32| {
            [
                pool_count.to_le_bytes().to_vec(),
                vec![1, 0, 0, 0, POOLS_PREFIX, 7],
            ]
            .concat()
        };
        let entries = [
            engine_entry("owner.near/dex", b"STATE".to_vec(), state(2)),
            engine_entry("owner.near/dex", pool_key(0), vec![10]),
            engine_entry("owner.near/dex", pool_key(1), vec![11]),
            engine_entry("owner.near/other", b"STATE".to_vec(), state(1)),
            engine_entry("owner.near/other", pool_key(0), vec![20]),
        ];
        let state = AccountState {
            block: BlockInfo {
                height: 1,
                hash: Default::default(),
                timestamp_nanosec: 0,
            },
            entries: entries.into_iter().collect(),
        };
        assert_eq!(pools::<u8>(&state, "owner.near/dex").unwrap(), vec![10, 11]);
        assert_eq!(pools::<u8>(&state, "owner.near/other").unwrap(), vec![20]);
        assert!(pools::<u8>(&state, "owner.near/missing").is_err());
    }
}
