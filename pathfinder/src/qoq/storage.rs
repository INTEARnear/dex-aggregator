use amp_dex_types::{
    Amplification, AssetWithBalance, FeeFraction, PayoutGroups, PoolRateSource, TotalShares,
};
use borsh::BorshDeserialize;
use swap_router_types::indexed::{AccountState, Watch};

use super::QOQ_DEX_ID;
use crate::intear_dex;

/// The amp DEX's `Pool`, with the `LookupMap` of its providers read as its prefix: the map can't
/// be dropped outside of a contract
#[derive(BorshDeserialize)]
pub struct StoredPool {
    pub assets: (AssetWithBalance, AssetWithBalance),
    pub decimals: (u8, u8),
    pub amplification: Amplification,
    pub rate_source: PoolRateSource,
    pub fee: FeeFraction,
    pub _providers_prefix: Vec<u8>,
    pub _total_shares: TotalShares,
    pub _payout_groups: PayoutGroups,
}

pub fn watch() -> Watch {
    intear_dex::watch_with_oracle_fees(QOQ_DEX_ID)
}

pub(super) fn stored_pools(state: &AccountState) -> Result<Vec<StoredPool>, anyhow::Error> {
    intear_dex::pools(state, QOQ_DEX_ID)
}
