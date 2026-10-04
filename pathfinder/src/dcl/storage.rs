//! Builds pools from raw dclv2.ref-labs.near storage. The contract is not open source, so the
//! layout was worked out by comparing storage with view methods at the same block. Fields whose
//! meaning is unknown are kept to check the layout, and bytes that hold the running state are
//! only accepted while they're all zero (every pool and the contract are running now).

use std::collections::{BTreeMap, BTreeSet};

use borsh::BorshDeserialize;
use near_min_api::types::{AccountId, Balance};
use pool_indexer::{AccountState, Watch};
use rhea_dcl_math::math::{LEFT_MOST_POINT, RIGHT_MOST_POINT};

use super::{Metadata, PoolInfo, RHEA_DCL_CONTRACT_ID};

const STATE_KEY: &[u8] = b"STATE";
/// Values of `pools: UnorderedMap<PoolId, Pool>`, keys are the prefix and u64 index
const POOLS_PREFIX: &[u8] = &[1, b'v'];
/// Points of all pools, keys are the prefix, borsh pool id and i32 point
const POINTS_PREFIX: &[u8] = &[0x0c];
/// Elements of the frozen token set, keys are the prefix and u64 index
const FROZEN_TOKENS_PREFIX: &[u8] = &[2, b'e'];
const CONTRACT_VERSION: u8 = 3;

pub fn watch() -> Watch {
    Watch {
        account_id: RHEA_DCL_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![
            STATE_KEY.to_vec(),
            POOLS_PREFIX.to_vec(),
            POINTS_PREFIX.to_vec(),
            FROZEN_TOKENS_PREFIX.to_vec(),
        ],
    }
}

#[derive(BorshDeserialize)]
struct StoredVector {
    len: u64,
    prefix: Vec<u8>,
}

#[derive(BorshDeserialize)]
struct StoredUnorderedSet {
    _index_prefix: Vec<u8>,
    elements: StoredVector,
}

#[derive(BorshDeserialize)]
struct StoredUnorderedMap {
    _index_prefix: Vec<u8>,
    _keys: StoredVector,
    values: StoredVector,
}

#[derive(BorshDeserialize)]
struct StoredContract {
    version: u8,
    _unknown: u8,
    _lookup_map_0x12: Vec<u8>,
    _operators: StoredUnorderedSet,
    frozen_tokens: StoredUnorderedSet,
    _lookup_map_0x03: Vec<u8>,
    _fee_tier_point_deltas: Vec<(u32, u32)>,
    protocol_fee_rate: u32,
    _unordered_map_0x11: StoredUnorderedMap,
    _farming_contract_id: String,
    /// Owner transfer and the running state
    state: [u8; 4],
    pools: StoredUnorderedMap,
    _lookup_map_0x05: Vec<u8>,
    _lookup_map_0x07: Vec<u8>,
    _lookup_map_0x08: Vec<u8>,
    _unordered_map_0x0f: StoredUnorderedMap,
    _lookup_map_0x09: Vec<u8>,
    _counters: (u128, u128, u64, u64),
}

#[derive(BorshDeserialize)]
enum StoredPool {
    V0(StoredPoolV0),
    V1 {
        pool: StoredPoolV0,
        _whitelist: Option<Vec<String>>,
    },
}

#[derive(BorshDeserialize)]
struct StoredPoolV0 {
    pool_id: String,
    token_x: AccountId,
    token_y: AccountId,
    fee: u32,
    point_delta: i32,
    current_point: i32,
    _sqrt_price: [u8; 32],
    liquidity: Balance,
    liquidity_x: Balance,
    _max_liquidity_per_point: Balance,
    _fee_scales: [[u8; 32]; 2],
    total_fee_x_charged: Balance,
    total_fee_y_charged: Balance,
    _volumes: [[u8; 32]; 4],
    total_liquidity: Balance,
    total_order_x: Balance,
    total_order_y: Balance,
    _total_x: Balance,
    _total_y: Balance,
    _points_prefix: Vec<u8>,
    _slot_bitmap_prefix: Vec<u8>,
    _unknown: [u8; 8],
    _orders_prefix: Vec<u8>,
    /// Includes the running state
    state: [u8; 9],
}

#[derive(BorshDeserialize)]
struct StoredPoint {
    liquidity: Option<StoredPointLiquidity>,
    order: Option<StoredPointOrder>,
}

#[derive(BorshDeserialize)]
struct StoredPointLiquidity {
    _liquidity_sum: Balance,
    liquidity_delta: i128,
    _acc_fee_out: [[u8; 32]; 2],
}

#[derive(BorshDeserialize)]
struct StoredPointOrder {
    selling_x: Balance,
    _unknown_x: [u8; 96],
    selling_y: Balance,
    _unknown_y: [u8; 104],
}

/// Liquidity segments and orders of a pool, the same as `get_liquidity_range` and
/// `get_pointorder_range` return over the full point range: segments are split at every endpoint,
/// the current point and the range bounds, and only orders that still sell something are
/// included. Both sorted by point.
pub(super) struct PointData {
    pub segments: Vec<(i32, i32, Balance)>,
    pub orders: Vec<(i32, Balance, Balance)>,
}

/// Metadata and frozen tokens
pub(super) fn contract(state: &AccountState) -> Result<(Metadata, Vec<AccountId>), anyhow::Error> {
    let value = state
        .entries
        .get(STATE_KEY)
        .ok_or_else(|| anyhow::anyhow!("STATE is missing"))?;
    let contract = borsh::from_slice::<StoredContract>(value)
        .map_err(|e| anyhow::anyhow!("Invalid STATE {value:02x?}: {e}"))?;
    if contract.version != CONTRACT_VERSION {
        anyhow::bail!("Unknown contract version {}", contract.version);
    }
    if contract.state != [0; 4] {
        anyhow::bail!("Unknown contract state bytes {:02x?}", contract.state);
    }
    if contract.pools.values.prefix != POOLS_PREFIX {
        anyhow::bail!(
            "Pools moved to prefix {:02x?}",
            contract.pools.values.prefix
        );
    }
    if contract.frozen_tokens.elements.prefix != FROZEN_TOKENS_PREFIX {
        anyhow::bail!(
            "Frozen tokens moved to prefix {:02x?}",
            contract.frozen_tokens.elements.prefix
        );
    }
    let frozen_tokens = (0..contract.frozen_tokens.elements.len)
        .map(|index| {
            let mut key = FROZEN_TOKENS_PREFIX.to_vec();
            key.extend(index.to_le_bytes());
            let value = state
                .entries
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("Frozen token {index} is missing"))?;
            borsh::from_slice::<AccountId>(value)
                .map_err(|e| anyhow::anyhow!("Invalid frozen token {index}: {e}"))
        })
        .collect::<Result<_, _>>()?;
    let metadata = Metadata {
        state: "Running".to_string(),
        pool_count: contract.pools.values.len,
        protocol_fee_rate: contract.protocol_fee_rate,
    };
    Ok((metadata, frozen_tokens))
}

/// Pools in the order of `list_pools`
pub(super) fn pools(state: &AccountState, pool_count: u64) -> Result<Vec<PoolInfo>, anyhow::Error> {
    (0..pool_count)
        .map(|index| {
            let mut key = POOLS_PREFIX.to_vec();
            key.extend(index.to_le_bytes());
            let value = state
                .entries
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("Pool {index} is missing"))?;
            let pool = match borsh::from_slice::<StoredPool>(value)
                .map_err(|e| anyhow::anyhow!("Invalid pool {index}: {e}"))?
            {
                StoredPool::V0(pool) | StoredPool::V1 { pool, .. } => pool,
            };
            if pool.state != [0; 9] {
                anyhow::bail!(
                    "Pool {} has unknown state bytes {:02x?}",
                    pool.pool_id,
                    pool.state
                );
            }
            Ok(PoolInfo {
                pool_id: pool.pool_id,
                token_x: pool.token_x,
                token_y: pool.token_y,
                fee: pool.fee,
                point_delta: pool.point_delta,
                current_point: pool.current_point,
                liquidity: pool.liquidity,
                liquidity_x: pool.liquidity_x,
                total_fee_x_charged: pool.total_fee_x_charged,
                total_fee_y_charged: pool.total_fee_y_charged,
                total_liquidity: pool.total_liquidity.to_string(),
                total_order_x: pool.total_order_x.to_string(),
                total_order_y: pool.total_order_y.to_string(),
                state: "Running".to_string(),
            })
        })
        .collect()
}

pub(super) fn point_data(
    state: &AccountState,
    pool_id: &str,
    current_point: i32,
) -> Result<PointData, anyhow::Error> {
    let mut prefix = POINTS_PREFIX.to_vec();
    prefix.extend(borsh::to_vec(pool_id)?);
    let mut deltas = BTreeMap::new();
    let mut orders = Vec::new();
    for (key, value) in state.with_prefix(&prefix) {
        let point = i32::from_le_bytes(
            key[prefix.len()..]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid point key {key:02x?}"))?,
        );
        let stored = borsh::from_slice::<StoredPoint>(value)
            .map_err(|e| anyhow::anyhow!("Invalid point {point}: {e}"))?;
        if let Some(liquidity) = stored.liquidity {
            deltas.insert(point, liquidity.liquidity_delta);
        }
        if let Some(order) = stored.order
            && (order.selling_x > 0 || order.selling_y > 0)
        {
            orders.push((point, order.selling_x, order.selling_y));
        }
    }
    orders.sort_unstable();

    let mut boundaries = deltas.keys().copied().collect::<BTreeSet<_>>();
    boundaries.extend([current_point, LEFT_MOST_POINT, RIGHT_MOST_POINT]);
    let mut segments = Vec::with_capacity(boundaries.len());
    let mut liquidity: i128 = 0;
    for (&left_point, &right_point) in boundaries.iter().zip(boundaries.iter().skip(1)) {
        liquidity = liquidity
            .checked_add(deltas.get(&left_point).copied().unwrap_or(0))
            .ok_or_else(|| anyhow::anyhow!("Liquidity overflows at point {left_point}"))?;
        let amount = Balance::try_from(liquidity).map_err(|_| {
            anyhow::anyhow!("Negative liquidity {liquidity} after point {left_point}")
        })?;
        segments.push((left_point, right_point, amount));
    }
    let end_liquidity = liquidity + deltas.get(&RIGHT_MOST_POINT).copied().unwrap_or(0);
    if end_liquidity != 0 {
        anyhow::bail!("Liquidity {end_liquidity} doesn't end at the last point");
    }
    Ok(PointData { segments, orders })
}
