use borsh::BorshDeserialize;
use near_min_api::types::AccountId;
use swap_router_types::indexed::{AccountState, Watch};
use xyk_dex_types::{AssetWithBalance, FeeConfigurationV2, PayoutGroups, TokenAmount, TotalShares};

use super::{PLACH_DEX_ID, PoolsSource};
use crate::intear_dex;

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
    intear_dex::watch(PLACH_DEX_ID)
}

pub(super) fn pools_source(state: &AccountState) -> Result<PoolsSource, anyhow::Error> {
    Ok(PoolsSource {
        block_height: state.block.height,
        timestamp: state.block.timestamp_nanosec,
        pools: intear_dex::pools(state, PLACH_DEX_ID)?,
    })
}
