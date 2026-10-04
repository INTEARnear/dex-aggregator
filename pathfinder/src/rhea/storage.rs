//! Builds the pools source from raw v2.ref-finance.near storage, the same way `get_pools` and
//! `get_pool_detail_infos` build it

use std::collections::{BTreeMap, HashMap};

use borsh::BorshDeserialize;
use near_min_api::types::{AccountId, Balance};
use pool_indexer::{AccountState, Watch};

use super::{DegenPoolInfo, U256};
use super::{
    PoolDetailInfo, PoolInfo, PoolKind, PoolsSource, RHEA_CONTRACT_ID, RatedPoolInfo,
    SimplePoolInfo, StablePoolInfo, degen, rated, stable,
};

/// `pools: Vector<Pool>`, keys are the prefix and u64 index
const POOLS_PREFIX: &[u8] = &[0];
const RATES_KEY: &[u8] = b"custom_rate_key";
const DEGENS_KEY: &[u8] = b"custom_degen_key";

pub fn watch() -> Watch {
    Watch {
        account_id: RHEA_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![
            POOLS_PREFIX.to_vec(),
            RATES_KEY.to_vec(),
            DEGENS_KEY.to_vec(),
        ],
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(BorshDeserialize)]
enum StoredPool {
    SimplePool(StoredSimplePool),
    StableSwapPool(StoredStablePool),
    RatedSwapPool(StoredStablePool),
    DegenSwapPool(StoredStablePool),
}

#[derive(BorshDeserialize)]
struct StoredSimplePool {
    token_account_ids: Vec<AccountId>,
    amounts: Vec<Balance>,
    _volumes: Vec<(u128, u128)>,
    total_fee: u32,
    _exchange_fee: u32,
    _referral_fee: u32,
    _shares_prefix: Vec<u8>,
    shares_total_supply: Balance,
}

/// Stable, rated and degen pools share the layout
#[derive(BorshDeserialize)]
struct StoredStablePool {
    token_account_ids: Vec<AccountId>,
    token_decimals: Vec<u8>,
    c_amounts: Vec<Balance>,
    _volumes: Vec<(u128, u128)>,
    total_fee: u32,
    _shares_prefix: Vec<u8>,
    shares_total_supply: Balance,
    init_amp_factor: u128,
    target_amp_factor: u128,
    init_amp_time: u64,
    stop_amp_time: u64,
}

/// Variant names are guessed from the tokens that use them
#[derive(BorshDeserialize)]
enum StoredRate {
    Stnear(StoredContractRate),
    Linear(StoredContractRate),
    Nearx(StoredContractRate),
    Sfrax(StoredOracleRate),
}

#[derive(BorshDeserialize)]
struct StoredContractRate {
    stored_rates: Balance,
    _rates_updated_at: u64,
    _contract_id: String,
}

#[derive(BorshDeserialize)]
struct StoredOracleRate {
    stored_rates: Balance,
    _rates_updated_at: u64,
    _contract_id: String,
    _oracle_kind: u8,
    _oracle_id: String,
    _base_token: String,
    _oracle_params: (u32, u32),
}

impl StoredRate {
    fn stored_rates(&self) -> Balance {
        match self {
            Self::Stnear(rate) | Self::Linear(rate) | Self::Nearx(rate) => rate.stored_rates,
            Self::Sfrax(rate) => rate.stored_rates,
        }
    }
}

/// Only the layout every degen uses now is known: kind 1 with oracle 1
#[derive(BorshDeserialize)]
struct StoredDegen {
    kind: u8,
    oracle: u8,
    price: Balance,
    _updated_at: u64,
    _price_id: [u8; 32],
}

/// Rate of tokens without a stored rate
const DEFAULT_RATE: Balance = rated::PRECISION;

pub(super) fn pools_source(state: &AccountState) -> Result<PoolsSource, anyhow::Error> {
    let rates = match state.entries.get(RATES_KEY) {
        Some(value) => borsh::from_slice::<Vec<(AccountId, StoredRate)>>(value)
            .map_err(|e| anyhow::anyhow!("Invalid {}: {e}", String::from_utf8_lossy(RATES_KEY)))?
            .into_iter()
            .map(|(token_id, rate)| (token_id, rate.stored_rates()))
            .collect(),
        None => HashMap::new(),
    };
    let mut degens = HashMap::new();
    if let Some(value) = state.entries.get(DEGENS_KEY) {
        for (token_id, degen) in borsh::from_slice::<Vec<(AccountId, StoredDegen)>>(value)
            .map_err(|e| anyhow::anyhow!("Invalid {}: {e}", String::from_utf8_lossy(DEGENS_KEY)))?
        {
            if (degen.kind, degen.oracle) != (1, 1) {
                anyhow::bail!(
                    "Degen of {token_id} has unknown kind {} and oracle {}",
                    degen.kind,
                    degen.oracle
                );
            }
            degens.insert(token_id, degen.price);
        }
    }

    let mut stored_pools = BTreeMap::new();
    for (key, value) in state.with_prefix(POOLS_PREFIX) {
        let index: [u8; 8] = key[POOLS_PREFIX.len()..]
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid pool key {key:02x?}"))?;
        stored_pools.insert(u64::from_le_bytes(index), value);
    }

    let mut pools = Vec::with_capacity(stored_pools.len());
    let mut detail_infos = Vec::with_capacity(stored_pools.len());
    for (expected_index, (index, value)) in stored_pools.into_iter().enumerate() {
        if index != expected_index as u64 {
            anyhow::bail!("Pool {expected_index} is missing");
        }
        let pool = borsh::from_slice::<StoredPool>(value)
            .map_err(|e| anyhow::anyhow!("Invalid pool {index}: {e}"))?;
        let (info, detail) = pool_info(pool, state.block.timestamp_nanosec, &rates, &degens)
            .map_err(|e| anyhow::anyhow!("Pool {index}: {e}"))?;
        pools.push(info);
        detail_infos.push(detail);
    }
    Ok(PoolsSource {
        block_height: state.block.height,
        pools,
        detail_infos,
    })
}

fn pool_info(
    pool: StoredPool,
    timestamp: u64,
    rates: &HashMap<AccountId, Balance>,
    degens: &HashMap<AccountId, Balance>,
) -> Result<(PoolInfo, PoolDetailInfo), anyhow::Error> {
    let (kind, pool) = match pool {
        StoredPool::SimplePool(pool) => {
            let info = PoolInfo {
                amounts: pool.amounts.clone(),
                amp: 0,
                pool_kind: PoolKind::SimplePool,
                shares_total_supply: pool.shares_total_supply.to_string(),
                token_account_ids: pool.token_account_ids.clone(),
                total_fee: pool.total_fee as u64,
            };
            let detail = PoolDetailInfo::SimplePoolInfo(SimplePoolInfo {
                token_account_ids: pool.token_account_ids,
                amounts: pool.amounts,
                total_fee: pool.total_fee,
                shares_total_supply: pool.shares_total_supply,
            });
            return Ok((info, detail));
        }
        StoredPool::StableSwapPool(pool) => (PoolKind::StableSwap, pool),
        StoredPool::RatedSwapPool(pool) => (PoolKind::RatedSwap, pool),
        StoredPool::DegenSwapPool(pool) => (PoolKind::DegenSwap, pool),
    };
    let target_decimal = match kind {
        PoolKind::StableSwap => stable::TARGET_DECIMAL,
        PoolKind::RatedSwap => rated::TARGET_DECIMAL,
        PoolKind::DegenSwap => degen::TARGET_DECIMAL,
        PoolKind::SimplePool => unreachable!(),
    };
    let amounts = pool
        .c_amounts
        .iter()
        .zip(&pool.token_decimals)
        .map(|(&c_amount, &decimals)| c_amount_to_amount(c_amount, decimals, target_decimal))
        .collect::<Result<Vec<_>, _>>()?;
    let amp = amp_factor(&pool, timestamp)?;
    let info = PoolInfo {
        amounts: amounts.clone(),
        amp,
        pool_kind: kind.clone(),
        shares_total_supply: pool.shares_total_supply.to_string(),
        token_account_ids: pool.token_account_ids.clone(),
        total_fee: pool.total_fee as u64,
    };
    let detail = match kind {
        PoolKind::StableSwap => PoolDetailInfo::StablePoolInfo(StablePoolInfo {
            token_account_ids: pool.token_account_ids,
            decimals: pool.token_decimals,
            amounts,
            c_amounts: pool.c_amounts,
            total_fee: pool.total_fee,
            shares_total_supply: pool.shares_total_supply,
            amp,
        }),
        PoolKind::RatedSwap => PoolDetailInfo::RatedPoolInfo(RatedPoolInfo {
            rates: pool
                .token_account_ids
                .iter()
                .map(|token_id| rates.get(token_id).copied().unwrap_or(DEFAULT_RATE))
                .collect(),
            token_account_ids: pool.token_account_ids,
            decimals: pool.token_decimals,
            amounts,
            c_amounts: pool.c_amounts,
            total_fee: pool.total_fee,
            shares_total_supply: pool.shares_total_supply,
            amp,
        }),
        PoolKind::DegenSwap => PoolDetailInfo::DegenPoolInfo(DegenPoolInfo {
            degens: pool
                .token_account_ids
                .iter()
                .map(|token_id| {
                    degens
                        .get(token_id)
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("No degen for {token_id}"))
                })
                .collect::<Result<_, _>>()?,
            token_account_ids: pool.token_account_ids,
            decimals: pool.token_decimals,
            amounts,
            c_amounts: pool.c_amounts,
            total_fee: pool.total_fee,
            shares_total_supply: pool.shares_total_supply,
            amp,
        }),
        PoolKind::SimplePool => unreachable!(),
    };
    Ok((info, detail))
}

fn c_amount_to_amount(
    c_amount: Balance,
    decimals: u8,
    target_decimal: u8,
) -> Result<Balance, anyhow::Error> {
    let amount = if decimals <= target_decimal {
        10u128
            .checked_pow((target_decimal - decimals) as u32)
            .map(|factor| c_amount / factor)
    } else {
        10u128
            .checked_pow((decimals - target_decimal) as u32)
            .and_then(|factor| c_amount.checked_mul(factor))
    };
    amount
        .ok_or_else(|| anyhow::anyhow!("Can't convert c_amount {c_amount} to {decimals} decimals"))
}

/// Amplification ramps linearly from the initial to the target factor
fn amp_factor(pool: &StoredStablePool, timestamp: u64) -> Result<u64, anyhow::Error> {
    let amp = if timestamp < pool.stop_amp_time {
        let time_range = pool.stop_amp_time - pool.init_amp_time;
        let time_delta = timestamp.checked_sub(pool.init_amp_time).ok_or_else(|| {
            anyhow::anyhow!("Amp ramp starts at {} after block time", pool.init_amp_time)
        })?;
        let ramp = |from: u128, to: u128| {
            (U256::from(from - to) * U256::from(time_delta) / U256::from(time_range)).as_u128()
        };
        if pool.target_amp_factor > pool.init_amp_factor {
            pool.init_amp_factor + ramp(pool.target_amp_factor, pool.init_amp_factor)
        } else {
            pool.init_amp_factor - ramp(pool.init_amp_factor, pool.target_amp_factor)
        }
    } else {
        pool.target_amp_factor
    };
    Ok(u64::try_from(amp)?)
}
