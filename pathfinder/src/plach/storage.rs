//! Builds the pools source from raw dex.intear.near storage of the Plach DEX, the same way its
//! `get_pools` view builds it. Mirrors the storage types and fee logic of the xyk DEX in
//! https://github.com/INTEARnear/dex

use std::collections::HashMap;

use borsh::BorshDeserialize;
use near_min_api::types::{AccountId, U128};
use pool_indexer::{AccountState, Watch};

use super::{
    AssetId, AssetWithBalance, CurrentFees, FeeAmount, FeeConfiguration, FeeFraction, FeeReceiver,
    INTEAR_DEX_CONTRACT_ID, MAX_FEE_FRACTION, PLACH_DEX_ID, PoolData, PoolsSource, Timestamp,
    V1FeeConfiguration,
};

/// `dex_storage: LookupMap<(DexId, Vec<u8>), Vec<u8>>` of the DEX engine
const DEX_STORAGE_PREFIX: u8 = 1;
/// `pools: Vector<Pool>` of the xyk DEX, keys are the prefix and u32 index
const POOLS_PREFIX: u8 = 0;
const PROTOCOL_FEE: FeeFraction = MAX_FEE_FRACTION / 1000;
const PROTOCOL_FEE_REDUCED: FeeFraction = 1;
const PROTOCOL_FEE_RECEIVER_ID: &str = "plach.intear.near";
const PROTOCOL_FEE_REDUCE_ASSET_ACCOUNTS: &[&str] = &[
    "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1",
    "near",
];
const PROTOCOL_FEE_REDUCE_ASSET_PARENT_ACCOUNTS: &[&str] =
    &["omft.near", "omni.hot.tg", "tether-token.near"];
/// Share of a scheduled fee that goes to the protocol until the schedule ends
const SCHEDULED_FEE_PROTOCOL_SHARE_PERCENT: FeeFraction = 5;

/// Keys of the Plach DEX in the engine's storage
fn storage_prefix() -> Vec<u8> {
    let (deployer, id) = PLACH_DEX_ID.split_once('/').unwrap();
    let mut prefix = vec![DEX_STORAGE_PREFIX];
    prefix.extend(borsh::to_vec(&(deployer, id)).unwrap());
    prefix
}

pub fn watch() -> Watch {
    Watch {
        account_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        prefixes: vec![storage_prefix()],
    }
}

#[derive(BorshDeserialize)]
enum StoredPool {
    PrivateV1 {
        assets: (AssetWithBalance, AssetWithBalance),
        owner_id: AccountId,
        fees: StoredCurrentFees,
    },
    PublicV1 {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: StoredCurrentFees,
        _user_shares_prefix: Vec<u8>,
        total_shares: Option<u128>,
    },
    LaunchV1 {
        near_amount: U128,
        launched_asset: AssetWithBalance,
        fees: StoredFeeConfiguration,
        phantom_liquidity_near: U128,
    },
    PrivateV2 {
        assets: (AssetWithBalance, AssetWithBalance),
        owner_id: AccountId,
        fees: StoredFeeConfiguration,
        locked: bool,
    },
    PublicV2 {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: StoredFeeConfiguration,
        _user_shares_prefix: Vec<u8>,
        total_shares: Option<u128>,
    },
    LaunchV2 {
        quote_asset: AssetWithBalance,
        launched_asset: AssetWithBalance,
        fees: StoredFeeConfiguration,
        phantom_liquidity: U128,
    },
}

#[derive(BorshDeserialize, Clone)]
struct StoredCurrentFees {
    receivers: Vec<(StoredFeeReceiver, FeeFraction)>,
}

#[derive(BorshDeserialize, Clone)]
enum StoredFeeConfiguration {
    V1(StoredCurrentFees),
    V2 {
        receivers: Vec<(StoredFeeReceiver, FeeAmount)>,
    },
}

#[derive(BorshDeserialize, Clone)]
enum StoredFeeReceiver {
    Account(AccountId),
    Pool,
    Community(AccountId),
}

impl From<StoredFeeReceiver> for FeeReceiver {
    fn from(receiver: StoredFeeReceiver) -> Self {
        match receiver {
            StoredFeeReceiver::Account(account_id) | StoredFeeReceiver::Community(account_id) => {
                FeeReceiver::Account(account_id)
            }
            StoredFeeReceiver::Pool => FeeReceiver::Pool,
        }
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
        pools.push(
            pool_data(pool, state.block.timestamp_nanosec)
                .map_err(|e| anyhow::anyhow!("Pool {pool_id}: {e}"))?,
        );
    }
    Ok(PoolsSource {
        block_height: state.block.height,
        pools,
    })
}

fn pool_data(pool: StoredPool, timestamp: Timestamp) -> Result<PoolData, anyhow::Error> {
    Ok(match pool {
        StoredPool::PrivateV1 {
            assets,
            owner_id,
            fees,
        } => {
            let fees = StoredFeeConfiguration::V1(fees);
            PoolData::Private {
                fees: current_fees(&fees, [&assets.0.asset_id, &assets.1.asset_id], timestamp)?,
                fee_configuration: fee_configuration(fees),
                assets,
                owner_id,
                locked: false,
            }
        }
        StoredPool::PublicV1 {
            assets,
            fees,
            total_shares,
            ..
        } => {
            let fees = StoredFeeConfiguration::V1(fees);
            PoolData::Public {
                fees: current_fees(&fees, [&assets.0.asset_id, &assets.1.asset_id], timestamp)?,
                fee_configuration: fee_configuration(fees),
                assets,
                total_shares: total_shares.map(U128),
            }
        }
        StoredPool::LaunchV1 {
            near_amount,
            launched_asset,
            fees,
            phantom_liquidity_near,
        } => PoolData::Launch {
            fees: current_fees(&fees, [&AssetId::Near, &launched_asset.asset_id], timestamp)?,
            fee_configuration: fee_configuration(fees),
            near_amount,
            launched_asset,
            phantom_liquidity_near,
        },
        StoredPool::PrivateV2 {
            assets,
            owner_id,
            fees,
            locked,
        } => PoolData::Private {
            fees: current_fees(&fees, [&assets.0.asset_id, &assets.1.asset_id], timestamp)?,
            fee_configuration: fee_configuration(fees),
            assets,
            owner_id,
            locked,
        },
        StoredPool::PublicV2 {
            assets,
            fees,
            total_shares,
            ..
        } => PoolData::Public {
            fees: current_fees(&fees, [&assets.0.asset_id, &assets.1.asset_id], timestamp)?,
            fee_configuration: fee_configuration(fees),
            assets,
            total_shares: total_shares.map(U128),
        },
        StoredPool::LaunchV2 {
            quote_asset,
            launched_asset,
            fees,
            phantom_liquidity,
        } => PoolData::LaunchV2 {
            fees: current_fees(
                &fees,
                [&quote_asset.asset_id, &launched_asset.asset_id],
                timestamp,
            )?,
            fee_configuration: fee_configuration(fees),
            quote_asset,
            launched_asset,
            phantom_liquidity,
        },
    })
}

fn fee_configuration(fees: StoredFeeConfiguration) -> FeeConfiguration {
    match fees {
        StoredFeeConfiguration::V1(fees) => FeeConfiguration::V1(CurrentFees {
            receivers: fees
                .receivers
                .into_iter()
                .map(|(receiver, fraction)| (receiver.into(), fraction))
                .collect(),
        }),
        StoredFeeConfiguration::V2 { receivers } => FeeConfiguration::V2(V1FeeConfiguration {
            receivers: receivers
                .into_iter()
                .map(|(receiver, amount)| (receiver.into(), amount))
                .collect(),
        }),
    }
}

/// Fees at the block time, with the protocol fee added
fn current_fees(
    fees: &StoredFeeConfiguration,
    assets: [&AssetId; 2],
    timestamp: Timestamp,
) -> Result<CurrentFees, anyhow::Error> {
    let is_empty = match fees {
        StoredFeeConfiguration::V1(fees) => fees.receivers.is_empty(),
        StoredFeeConfiguration::V2 { receivers } => receivers.is_empty(),
    };
    let mut protocol_fee = if is_empty {
        0
    } else if should_reduce_fee(assets)? {
        PROTOCOL_FEE_REDUCED
    } else {
        PROTOCOL_FEE
    };
    let mut receivers = Vec::new();
    match fees {
        StoredFeeConfiguration::V1(fees) => {
            for (receiver, fraction) in &fees.receivers {
                receivers.push((receiver.clone().into(), *fraction));
            }
        }
        StoredFeeConfiguration::V2 {
            receivers: configured,
        } => {
            for (receiver, amount) in configured {
                let fraction = match amount {
                    FeeAmount::Fixed(fraction) => *fraction,
                    FeeAmount::Scheduled {
                        end: (end_time, _), ..
                    } => {
                        let mut fraction = amount.get_fee_fraction(timestamp);
                        if timestamp < *end_time {
                            let protocol_share =
                                fraction * SCHEDULED_FEE_PROTOCOL_SHARE_PERCENT / 100;
                            fraction -= protocol_share;
                            protocol_fee += protocol_share;
                        }
                        fraction
                    }
                    FeeAmount::Dynamic { .. } => {
                        anyhow::bail!("Dynamic fees are not implemented by the contract")
                    }
                };
                receivers.push((receiver.clone().into(), fraction));
            }
        }
    }
    receivers.push((
        FeeReceiver::Account(PROTOCOL_FEE_RECEIVER_ID.parse().unwrap()),
        protocol_fee,
    ));
    Ok(CurrentFees { receivers })
}

/// Whether both assets are stable assets, which pay a reduced protocol fee
fn should_reduce_fee(assets: [&AssetId; 2]) -> Result<bool, anyhow::Error> {
    for asset in assets {
        let account_id = match asset {
            AssetId::Near => "near",
            AssetId::Nep141(account_id) | AssetId::Nep245(account_id, _) => account_id.as_str(),
            AssetId::Nep171(_, _) => anyhow::bail!("NEP-171 assets are not supported"),
        };
        let is_reduced = PROTOCOL_FEE_REDUCE_ASSET_ACCOUNTS.contains(&account_id)
            || PROTOCOL_FEE_REDUCE_ASSET_PARENT_ACCOUNTS
                .iter()
                .any(|parent| is_direct_sub_account(account_id, parent));
        if !is_reduced {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Same as `AccountIdRef::is_sub_account_of`
fn is_direct_sub_account(account_id: &str, parent: &str) -> bool {
    account_id
        .strip_suffix(parent)
        .and_then(|rest| rest.strip_suffix('.'))
        .is_some_and(|name| !name.is_empty() && !name.contains('.'))
}
