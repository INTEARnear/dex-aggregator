//! Builds the pools source from raw v2.ref-finance.near storage, the same way `get_pools` and
//! `get_pool_detail_infos` build it

use std::collections::{BTreeMap, HashMap};

use borsh::BorshDeserialize;
use near_min_api::types::{AccountId, Balance};
use pool_indexer::{AccountState, Watch};

use super::{DegenPoolInfo, U256};
use super::{
    Pool, PoolDetailInfo, RHEA_CONTRACT_ID, RatedPoolInfo, SimplePoolInfo, StablePoolInfo, degen,
    rated, stable,
};

/// `pools: Vector<Pool>`, keys are the prefix and u64 index
const POOLS_PREFIX: &[u8] = &[0];
const RATES_KEY: &[u8] = b"custom_rate_key";
const DEGENS_KEY: &[u8] = b"custom_degen_key";
const DEGEN_ORACLES_KEY: &[u8] = b"custom_degen_oracle_config_key";
/// Rated pools refuse swaps with a rate this old
const RATE_EXPIRY_NANOS: u64 = 24 * 60 * 60 * 1_000_000_000;

pub fn watch() -> Watch {
    Watch {
        account_id: RHEA_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![
            POOLS_PREFIX.to_vec(),
            RATES_KEY.to_vec(),
            DEGENS_KEY.to_vec(),
            DEGEN_ORACLES_KEY.to_vec(),
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
    _shares_total_supply: Balance,
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
    _shares_total_supply: Balance,
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
    rates_updated_at: u64,
    _contract_id: String,
}

#[derive(BorshDeserialize)]
struct StoredOracleRate {
    stored_rates: Balance,
    rates_updated_at: u64,
    _contract_id: String,
    _oracle_kind: u8,
    _oracle_id: String,
    _base_token: String,
    _oracle_params: (u32, u32),
}

impl StoredRate {
    /// The rate and when it was updated
    fn stored_rates(&self) -> (Balance, u64) {
        match self {
            Self::Stnear(rate) | Self::Linear(rate) | Self::Nearx(rate) => {
                (rate.stored_rates, rate.rates_updated_at)
            }
            Self::Sfrax(rate) => (rate.stored_rates, rate.rates_updated_at),
        }
    }
}

/// Only the layout every degen uses now is known: kind 1 with oracle 1
#[derive(BorshDeserialize)]
struct StoredDegen {
    kind: u8,
    oracle: u8,
    price: Balance,
    updated_at: u64,
    _price_id: [u8; 32],
}

/// How long the prices of each oracle are valid, degen pools refuse swaps with older prices
#[derive(BorshDeserialize)]
enum StoredDegenOracle {
    PriceOracle {
        _oracle_id: String,
        _expire_ts: u64,
        _maximum_recency_duration_sec: u32,
        _maximum_staleness_duration_sec: u32,
    },
    PythOracle {
        _oracle_id: String,
        expire_ts: u64,
        _pyth_price_valid_duration_sec: u32,
    },
}

/// Rate of tokens without a stored rate
const DEFAULT_RATE: Balance = rated::PRECISION;

/// A rate or degen price, `None` if the contract would refuse to swap with it
type Price = Option<Balance>;

pub(super) fn pools(state: &AccountState) -> Result<Vec<Pool>, anyhow::Error> {
    let timestamp = state.block.timestamp_nanosec;
    let rates = match state.entries.get(RATES_KEY) {
        Some(value) => borsh::from_slice::<Vec<(AccountId, StoredRate)>>(value)
            .map_err(|e| anyhow::anyhow!("Invalid {}: {e}", String::from_utf8_lossy(RATES_KEY)))?
            .into_iter()
            .map(|(token_id, rate)| {
                let (rate, updated_at) = rate.stored_rates();
                let valid = timestamp.saturating_sub(updated_at) < RATE_EXPIRY_NANOS;
                (token_id, valid.then_some(rate))
            })
            .collect(),
        None => HashMap::new(),
    };
    let mut degens = HashMap::new();
    if let Some(value) = state.entries.get(DEGENS_KEY) {
        let pyth_expiry = pyth_expiry(state)?;
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
            let valid = timestamp.saturating_sub(degen.updated_at) < pyth_expiry;
            degens.insert(token_id, valid.then_some(degen.price));
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
    for (expected_index, (index, value)) in stored_pools.into_iter().enumerate() {
        if index != expected_index as u64 {
            anyhow::bail!("Pool {expected_index} is missing");
        }
        let pool = borsh::from_slice::<StoredPool>(value)
            .map_err(|e| anyhow::anyhow!("Invalid pool {index}: {e}"))?;
        let Some((tokens, detail)) = pool_detail(pool, timestamp, &rates, &degens)
            .map_err(|e| anyhow::anyhow!("Pool {index}: {e}"))?
        else {
            continue;
        };
        pools.push(Pool {
            id: index,
            tokens: tokens.into(),
            detail: detail.with_invariant(),
        });
    }
    Ok(pools)
}

/// How long the degens' oracle keeps prices valid
fn pyth_expiry(state: &AccountState) -> Result<u64, anyhow::Error> {
    let value = state
        .entries
        .get(DEGEN_ORACLES_KEY)
        .ok_or_else(|| anyhow::anyhow!("No {}", String::from_utf8_lossy(DEGEN_ORACLES_KEY)))?;
    borsh::from_slice::<Vec<(String, StoredDegenOracle)>>(value)
        .map_err(|e| {
            anyhow::anyhow!(
                "Invalid {}: {e}",
                String::from_utf8_lossy(DEGEN_ORACLES_KEY)
            )
        })?
        .into_iter()
        .find_map(|(_, oracle)| match oracle {
            StoredDegenOracle::PythOracle { expire_ts, .. } => Some(expire_ts),
            StoredDegenOracle::PriceOracle { .. } => None,
        })
        .ok_or_else(|| anyhow::anyhow!("No Pyth oracle config for degens"))
}

/// The pool's tokens and what swaps are simulated with, `None` if a rate or degen price of
/// the pool is too old to swap
fn pool_detail(
    pool: StoredPool,
    timestamp: u64,
    rates: &HashMap<AccountId, Price>,
    degens: &HashMap<AccountId, Price>,
) -> Result<Option<(Vec<AccountId>, PoolDetailInfo)>, anyhow::Error> {
    enum CurvePoolKind {
        Stable,
        Rated,
        Degen,
    }

    let (kind, pool) = match pool {
        StoredPool::SimplePool(pool) => {
            let detail = PoolDetailInfo::SimplePoolInfo(SimplePoolInfo {
                amounts: pool.amounts.into(),
                total_fee: pool.total_fee,
            });
            return Ok(Some((pool.token_account_ids, detail)));
        }
        StoredPool::StableSwapPool(pool) => (CurvePoolKind::Stable, pool),
        StoredPool::RatedSwapPool(pool) => (CurvePoolKind::Rated, pool),
        StoredPool::DegenSwapPool(pool) => (CurvePoolKind::Degen, pool),
    };
    let target_decimal = match kind {
        CurvePoolKind::Stable => stable::TARGET_DECIMAL,
        CurvePoolKind::Rated => rated::TARGET_DECIMAL,
        CurvePoolKind::Degen => degen::TARGET_DECIMAL,
    };
    // Like get_pool_detail_infos, fails for amounts it can't convert
    for (&c_amount, &decimals) in pool.c_amounts.iter().zip(&pool.token_decimals) {
        c_amount_to_amount(c_amount, decimals, target_decimal)?;
    }
    let amp = amp_factor(&pool, timestamp)?;
    let detail = match kind {
        CurvePoolKind::Stable => PoolDetailInfo::StablePoolInfo(StablePoolInfo {
            decimals: pool.token_decimals.into(),
            c_amounts: pool.c_amounts.into(),
            total_fee: pool.total_fee,
            amp,
            d: None,
        }),
        CurvePoolKind::Rated => PoolDetailInfo::RatedPoolInfo(RatedPoolInfo {
            rates: match pool
                .token_account_ids
                .iter()
                .map(|token_id| rates.get(token_id).copied().unwrap_or(Some(DEFAULT_RATE)))
                .collect()
            {
                Some(rates) => rates,
                None => return Ok(None),
            },
            decimals: pool.token_decimals.into(),
            c_amounts: pool.c_amounts.into(),
            total_fee: pool.total_fee,
            amp,
            d: None,
        }),
        CurvePoolKind::Degen => PoolDetailInfo::DegenPoolInfo(DegenPoolInfo {
            degens: match pool
                .token_account_ids
                .iter()
                .map(|token_id| {
                    degens
                        .get(token_id)
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("No degen for {token_id}"))
                })
                .collect::<Result<Option<_>, _>>()?
            {
                Some(degens) => degens,
                None => return Ok(None),
            },
            decimals: pool.token_decimals.into(),
            c_amounts: pool.c_amounts.into(),
            total_fee: pool.total_fee,
            amp,
            d: None,
        }),
    };
    Ok(Some((pool.token_account_ids, detail)))
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
            (U256::from(from - to) * U256::from(time_delta) / U256::from(time_range)).to::<u128>()
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
