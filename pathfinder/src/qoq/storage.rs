use amp_dex_types::{
    Amplification, AssetWithBalance, FeeConfiguration, PayoutGroups, Price, TotalShares,
};
use borsh::BorshDeserialize;
use near_sdk::PublicKeyHandle;
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
    pub oracle_key: PublicKeyHandle,
    pub _last_oracle_price: Price,
    pub fees: FeeConfiguration,
    pub _providers_prefix: Vec<u8>,
    pub _total_shares: TotalShares,
    pub _payout_groups: PayoutGroups,
}

pub fn watch() -> Watch {
    intear_dex::watch(QOQ_DEX_ID)
}

pub(super) fn stored_pools(state: &AccountState) -> Result<Vec<StoredPool>, anyhow::Error> {
    intear_dex::pools(state, QOQ_DEX_ID)
}
