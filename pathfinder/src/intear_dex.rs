use std::collections::HashMap;

use borsh::BorshDeserialize;
use intear_dex_common::{AssetId, TokenAmount};
use near_sdk::PublicKeyHandle;
use swap_router_types::indexed::{AccountState, Watch};

pub const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
/// `dex_storage: LookupMap<(DexId, Vec<u8>), Vec<u8>>` of the DEX engine
const DEX_STORAGE_PREFIX: u8 = 1;
/// `oracle_fees: LookupMap<PublicKeyHandle, HashMap<AssetId, TokenAmount>>` of the DEX engine
const ORACLE_FEES_PREFIX: u8 = 14;
/// `pools: Vector<Pool>` of the xyk and amp DEXes, keys are the prefix and u32 index
const POOLS_PREFIX: u8 = 0;
const HASHED_KEY_BYTES: usize = 32;

/// What each oracle takes for a request, in each asset that it takes fees in
pub type OracleFees = HashMap<PublicKeyHandle, HashMap<AssetId, TokenAmount>>;

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

/// The DEX with the fees of the oracles that swaps on it ask
pub fn watch_with_oracle_fees(dex_id: &str) -> Watch {
    Watch {
        account_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![storage_prefix(dex_id), vec![ORACLE_FEES_PREFIX]],
    }
}

pub fn oracle_fees(state: &AccountState) -> Result<OracleFees, anyhow::Error> {
    state
        .with_prefix(&[ORACLE_FEES_PREFIX])
        .filter(|(key, _)| key.len() != HASHED_KEY_BYTES)
        .map(|(key, value)| {
            let oracle_key = borsh::from_slice::<PublicKeyHandle>(&key[1..])
                .map_err(|e| anyhow::anyhow!("Invalid oracle key {key:02x?}: {e}"))?;
            let fees = borsh::from_slice(value)
                .map_err(|e| anyhow::anyhow!("Invalid fees of oracle {oracle_key}: {e}"))?;
            Ok((oracle_key, fees))
        })
        .collect()
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

    #[test]
    fn reads_the_fees_of_oracles_from_the_engine() {
        let oracle_key: PublicKeyHandle = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp"
            .parse()
            .unwrap();
        let fees = HashMap::from([(AssetId::Near, TokenAmount::new(10u128.pow(21)))]);
        let state = AccountState {
            block: BlockInfo {
                height: 1,
                hash: Default::default(),
                timestamp_nanosec: 0,
            },
            entries: [
                engine_entry("owner.near/dex", b"STATE".to_vec(), vec![0; 4]),
                (
                    [
                        vec![ORACLE_FEES_PREFIX],
                        borsh::to_vec(&oracle_key).unwrap(),
                    ]
                    .concat(),
                    borsh::to_vec(&fees).unwrap(),
                ),
                // A balance of another map, under a hash that starts with the prefix
                (
                    [vec![ORACLE_FEES_PREFIX], vec![0x0c; HASHED_KEY_BYTES - 1]].concat(),
                    vec![0; 20],
                ),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            oracle_fees(&state).unwrap(),
            HashMap::from([(oracle_key, fees)])
        );
    }
}
