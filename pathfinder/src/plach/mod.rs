mod storage;

use self::storage::StoredPool;
pub use self::storage::watch;

use near_min_api::types::{Balance, BlockHeight, NearGas};

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use tokio::time::Instant;

pub use intear_dex_common::AssetId;
use near_sdk::Timestamp;
use swap_router_types::{
    hook::{HookPool, HookPools, HookSwapRoute, PoolState},
    indexed::AccountState,
    types::{Amount, Gas},
};
use xyk_dex_types::math::{exact_in_amount_out, exact_out_amount_in};
use xyk_dex_types::{
    AssetIndex, CurrentFees, FeeDiscountFraction, FeePayoutType, FeeReceiver, PayoutGroups,
    PoolAssets, PoolId, RouteFraction, SwapReserves, SwapRoute, TokenAmount, TotalShares,
    protocol_fee_payout,
};

use crate::search::QuoteAmount;
use tracing::warn;

pub const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);

pub struct Pool {
    id: u32,
    tokens: [AssetId; 2],
    reserves: PoolAssets<TokenAmount>,
    info: PoolInfo,
}

/// What swaps don't change
struct PoolInfo {
    /// Pool and protocol fees at the block time
    fees: CurrentFees,
    /// Swaps fail if they leave less than this of a token. Launch pools keep their phantom
    /// liquidity of the quote token.
    min_reserves: PoolAssets<TokenAmount>,
    /// `None` for pools that keep all of their fee in their liquidity
    liquidity_provider_payouts: Option<LiquidityProviderPayouts>,
}

/// How a public pool pays its fee out to its liquidity providers
struct LiquidityProviderPayouts {
    total_shares: TotalShares,
    groups: PayoutGroups,
}

impl Pool {
    /// `None` for a pool of NEP-171 assets, which the contract can't swap in
    fn new(id: u32, data: StoredPool, timestamp: Timestamp) -> Option<Self> {
        let (assets, fee_configuration, min_reserves, liquidity_provider_payouts) = match data {
            StoredPool::PrivateV2 { assets, fees, .. } => {
                (assets, fees, PoolAssets([TokenAmount::ZERO; 2]), None)
            }
            StoredPool::LaunchV2 {
                quote_asset,
                launched_asset,
                fees,
                phantom_liquidity,
            } => (
                (quote_asset, launched_asset),
                fees,
                PoolAssets([phantom_liquidity, TokenAmount::ZERO]),
                None,
            ),
            StoredPool::PublicV3 {
                assets,
                fees,
                total_shares,
                payout_groups,
                ..
            } => (
                assets,
                fees,
                PoolAssets([TokenAmount::ZERO; 2]),
                Some(LiquidityProviderPayouts {
                    total_shares,
                    groups: *payout_groups,
                }),
            ),
        };
        let tokens = [assets.0.asset_id, assets.1.asset_id];
        if tokens
            .iter()
            .any(|token| matches!(token, AssetId::Nep171(..)))
        {
            return None;
        }
        Some(Self {
            id,
            tokens,
            reserves: PoolAssets([assets.0.balance, assets.1.balance]),
            info: PoolInfo {
                fees: fee_configuration.with_protocol_fee(timestamp),
                min_reserves,
                liquidity_provider_payouts,
            },
        })
    }

    /// Swaps the token at `in_index` like the contract's `swap_in_pool` in the pool with
    /// `reserves`, which it changes like the swap would, and returns the amounts in and out.
    /// `None` for a swap the contract fails.
    fn swap(
        &self,
        reserves: &mut PoolAssets<TokenAmount>,
        in_index: AssetIndex,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<(Balance, Balance)> {
        let id = self.id;
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.swap_unless_panicked(reserves, in_index, amount, fee_discount)
        }))
        .unwrap_or_else(|_| {
            warn!("Panicked while emulating a swap in pool {id}");
            None
        })
    }

    /// Panics on overflows like the contract
    fn swap_unless_panicked(
        &self,
        reserves: &mut PoolAssets<TokenAmount>,
        in_index: AssetIndex,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<(Balance, Balance)> {
        let out_index = match in_index {
            AssetIndex::First => AssetIndex::Second,
            AssetIndex::Second => AssetIndex::First,
        };
        let info = &self.info;
        // As a signed fee discount makes the xyk DEX charge them
        let discounted_fees;
        let fees = match fee_discount {
            Some(fee_discount) => {
                discounted_fees = info.fees.clone().discounted(fee_discount);
                &discounted_fees
            }
            None => &info.fees,
        };
        let mut balances = *reserves;
        if balances.0.iter().any(|balance| balance.is_zero()) {
            return None;
        }
        let swap_reserves = |balances: &PoolAssets<TokenAmount>| SwapReserves {
            input: balances[in_index],
            output: balances[out_index],
        };
        let amount_in = match amount {
            QuoteAmount::ExactIn(0) | QuoteAmount::ExactOut(0) => return None,
            QuoteAmount::ExactIn(amount_in) => TokenAmount::new(amount_in),
            QuoteAmount::ExactOut(amount_out) => {
                if amount_out >= balances[out_index].get() {
                    return None;
                }
                exact_out_amount_in(TokenAmount::new(amount_out), swap_reserves(&balances), fees)
            }
        };
        // The parts of each receiver's fee that are paid out, and with which payout type. The
        // rest of the pool's fee goes into its liquidity.
        let receiver_fees = fees
            .amounts(amount_in)
            .map(|(receiver, fee_amount)| {
                let paid_out_fees: Vec<(FeePayoutType, TokenAmount)> =
                    match (receiver, &info.liquidity_provider_payouts) {
                        (FeeReceiver::Pool, Some(payouts)) => payouts
                            .groups
                            .split_pool_fee(fee_amount, payouts.total_shares)
                            .into_iter()
                            .filter(|(_, paid_out_fee)| !paid_out_fee.is_zero())
                            .collect(),
                        (FeeReceiver::Pool, None) => Vec::new(),
                        (FeeReceiver::Account { payout_type, .. }, _) => {
                            vec![(*payout_type, fee_amount)]
                        }
                        (FeeReceiver::Protocol, _) => vec![(
                            protocol_fee_payout((&self.tokens[0], &self.tokens[1])),
                            fee_amount,
                        )],
                    };
                (receiver, fee_amount, paid_out_fees)
            })
            .collect::<Vec<_>>();
        let mut total_fees = TokenAmount::ZERO;
        let mut fees_added_to_liquidity = TokenAmount::ZERO;
        for (_, fee_amount, paid_out_fees) in &receiver_fees {
            total_fees = total_fees.checked_add(*fee_amount).expect("Overflow");
            let fee_added_to_liquidity = paid_out_fees
                .iter()
                .try_fold(*fee_amount, |rest, (_, paid_out_fee)| {
                    rest.checked_sub(*paid_out_fee)
                })
                .expect("Paid out more than the fee");
            fees_added_to_liquidity = fees_added_to_liquidity
                .checked_add(fee_added_to_liquidity)
                .expect("Overflow");
        }
        let amount_in_after_fees = amount_in.checked_sub(total_fees).expect("Fee exceeds 100%");
        let amount_out = match amount {
            QuoteAmount::ExactIn(_) => {
                exact_in_amount_out(amount_in_after_fees, swap_reserves(&balances))
            }
            QuoteAmount::ExactOut(amount_out) => TokenAmount::new(amount_out),
        };
        balances[in_index] = balances[in_index]
            .checked_add(amount_in_after_fees)
            .expect("Overflow")
            .checked_add(fees_added_to_liquidity)
            .expect("Overflow");
        balances[out_index] = balances[out_index]
            .checked_sub(amount_out)
            .expect("Underflow");
        for (receiver, _, paid_out_fees) in receiver_fees {
            for (payout_type, paid_out_fee) in paid_out_fees {
                let payout_asset_index = payout_type.asset_index(in_index);
                let payout_amount = if payout_asset_index == in_index {
                    paid_out_fee
                } else {
                    // The pool swaps the fee into the other token without fees
                    let converted_fee = exact_in_amount_out(paid_out_fee, swap_reserves(&balances));
                    balances[in_index] = balances[in_index]
                        .checked_add(paid_out_fee)
                        .expect("Overflow");
                    balances[out_index] = balances[out_index]
                        .checked_sub(converted_fee)
                        .expect("Underflow");
                    converted_fee
                };
                if let (FeeReceiver::Pool, Some(payouts)) =
                    (receiver, &info.liquidity_provider_payouts)
                {
                    let fee_added_to_liquidity = match payout_type {
                        FeePayoutType::Reinvest => payout_amount,
                        FeePayoutType::KeepBoth
                        | FeePayoutType::ConvertToFirstAsset
                        | FeePayoutType::ConvertToSecondAsset => payouts
                            .groups
                            .group(payout_type)
                            .clone()
                            .share_fee(payout_asset_index, payout_amount),
                    };
                    balances[payout_asset_index] = balances[payout_asset_index]
                        .checked_add(fee_added_to_liquidity)
                        .expect("Overflow");
                }
            }
        }
        if AssetIndex::BOTH
            .into_iter()
            .any(|index| balances[index] < info.min_reserves[index])
        {
            return None;
        }
        *reserves = balances;
        Some((amount_in.get(), amount_out.get()))
    }
}

pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    pools: Vec<Pool>,
}

/// Everything a pools snapshot is built from, as stored by the contract at one block
struct PoolsSource {
    block_height: BlockHeight,
    /// Of the block, which scheduled fees depend on
    timestamp: Timestamp,
    pools: Vec<StoredPool>,
}

fn build_pools(source: PoolsSource) -> Pools {
    let PoolsSource {
        block_height,
        timestamp,
        pools,
    } = source;
    Pools {
        block_height,
        updated_at: Instant::now(),
        pools: pools
            .into_iter()
            .enumerate()
            .filter_map(|(i, data)| Pool::new(i as u32, data, timestamp))
            .collect(),
    }
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    storage::pools_source(state).map(build_pools)
}

const SWAP_GAS: Gas = Gas::from_ggas(5_470, 5_470);
/// What a swap burns less when Intear DEX already swapped on Plach in the call, which loads its swap
/// code once. Simulated on mainnet state, a second swap through a pool burnt 2.29 Tgas.
pub const REPEATED_SWAP_SAVING: NearGas = NearGas::from_ggas(4_100);
const HOP_GAS: Gas = Gas::from_ggas(920, 920);
const REPEATED_HOP_GAS: Gas = Gas::from_ggas(360, 360);

impl Pools {
    pub fn block_height(&self) -> BlockHeight {
        self.block_height
    }

    /// An error if they weren't updated in a while, which they are after every block
    pub fn check_fresh(&self) -> Result<(), anyhow::Error> {
        let age = self.updated_at.elapsed();
        if age > MAX_SNAPSHOT_AGE {
            anyhow::bail!(
                "Pools were last updated {age:?} ago, at block {}",
                self.block_height
            );
        }
        Ok(())
    }

    /// The pools, which routes refer to by their position here
    pub fn hook_pools(&self) -> HookPools {
        HookPools {
            pools: self
                .pools
                .iter()
                .map(|pool| HookPool {
                    tokens: pool.tokens.clone(),
                    state: PoolState::of_balances(pool.reserves.0.map(TokenAmount::get)),
                    hop_gas: HOP_GAS.estimated_burn,
                    oracle_key: None,
                    fees: pool
                        .info
                        .fees
                        .receivers
                        .iter()
                        .map(|(_, fee_fraction)| *fee_fraction)
                        .collect(),
                })
                .collect(),
            oracle_fees: Vec::new(),
        }
    }

    /// Swaps in the pool at `pool` of [`Pools::hook_pools`], whose reserves are in `state`
    pub fn swap(
        &self,
        pool: u32,
        state: &mut PoolState,
        token_in: usize,
        amount: Amount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Balance> {
        let pool = &self.pools[pool as usize];
        let mut reserves = PoolAssets(state.balances().map(TokenAmount::new));
        let (amount_in, amount_out) = pool.swap(
            &mut reserves,
            AssetIndex::BOTH[token_in],
            match amount {
                Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
                Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
            },
            fee_discount,
        )?;
        *state = PoolState::of_balances(reserves.0.map(TokenAmount::get));
        Some(match amount {
            Amount::AmountIn(_) => amount_out,
            Amount::AmountOut(_) => amount_in,
        })
    }

    /// The routes of a swap through pools by their position in [`Pools::hook_pools`]
    pub fn swap_routes(&self, routes: &[HookSwapRoute]) -> Result<Vec<SwapRoute>, String> {
        routes
            .iter()
            .map(|route| {
                let pool_ids = route
                    .pools
                    .iter()
                    .map(|&pool| {
                        let pool = self
                            .pools
                            .get(pool as usize)
                            .ok_or_else(|| format!("No pool {pool} in the snapshot"))?;
                        Ok(PoolId::new(pool.id))
                    })
                    .collect::<Result<_, String>>()?;
                Ok(SwapRoute {
                    fraction: RouteFraction::new(route.fraction),
                    pool_ids,
                })
            })
            .collect()
    }
}

/// What a swap on `routes` burns
pub fn swap_gas(routes: &[SwapRoute]) -> Gas {
    let hops = routes
        .iter()
        .flat_map(|route| &route.pool_ids)
        .collect::<Vec<_>>();
    let pools = hops.iter().collect::<HashSet<_>>().len() as u64;
    let repeated_hops = hops.len() as u64 - pools;
    SWAP_GAS + HOP_GAS * pools + REPEATED_HOP_GAS * repeated_hops
}

#[cfg(test)]
mod tests {
    use super::*;

    use xyk_dex_types::{
        AssetWithBalance, FeeAmount, FeeConfigurationV2, FeeFraction, PayoutGroupShares,
        ProviderShares,
    };

    const POOL_FEE: FeeFraction = FeeFraction::new(3_000); // 0.3%
    /// The protocol's 20% of [`POOL_FEE`]
    const PROTOCOL_FEE: FeeFraction = FeeFraction::new(600);
    const POOL_FEE_AFTER_PROTOCOL: FeeFraction = FeeFraction::new(2_400);

    fn nep141(account_id: &str) -> AssetId {
        AssetId::Nep141(account_id.parse().unwrap())
    }

    fn amount(amount: Balance) -> TokenAmount {
        TokenAmount::new(amount)
    }

    fn fee(amount_in: Balance, fee_fraction: FeeFraction) -> Balance {
        fee_fraction.of(amount(amount_in)).get()
    }

    /// Swaps that change the pool's own reserves
    impl Pool {
        fn swap_exact_in(
            &mut self,
            token_in: usize,
            _token_out: usize,
            amount_in: Balance,
        ) -> Option<Balance> {
            let mut reserves = self.reserves;
            let (_, amount_out) = self.swap(
                &mut reserves,
                AssetIndex::BOTH[token_in],
                QuoteAmount::ExactIn(amount_in),
                None,
            )?;
            self.reserves = reserves;
            Some(amount_out)
        }

        fn swap_exact_out(
            &mut self,
            token_in: usize,
            _token_out: usize,
            amount_out: Balance,
        ) -> Option<Balance> {
            let mut reserves = self.reserves;
            let (amount_in, _) = self.swap(
                &mut reserves,
                AssetIndex::BOTH[token_in],
                QuoteAmount::ExactOut(amount_out),
                None,
            )?;
            self.reserves = reserves;
            Some(amount_in)
        }

        fn quote_exact_in(
            &self,
            token_in: usize,
            _token_out: usize,
            amount_in: Balance,
        ) -> Option<Balance> {
            let (_, amount_out) = self.swap(
                &mut self.reserves.clone(),
                AssetIndex::BOTH[token_in],
                QuoteAmount::ExactIn(amount_in),
                None,
            )?;
            Some(amount_out)
        }

        fn quote_exact_out(
            &self,
            token_in: usize,
            _token_out: usize,
            amount_out: Balance,
        ) -> Option<Balance> {
            let (amount_in, _) = self.swap(
                &mut self.reserves.clone(),
                AssetIndex::BOTH[token_in],
                QuoteAmount::ExactOut(amount_out),
                None,
            )?;
            Some(amount_in)
        }
    }

    fn reserves(pool: &Pool) -> [Balance; 2] {
        pool.reserves.0.map(TokenAmount::get)
    }

    /// The pool fee, which the contract takes the protocol's share of
    fn fees() -> FeeConfigurationV2 {
        FeeConfigurationV2 {
            receivers: vec![(FeeReceiver::Pool, FeeAmount::Fixed(POOL_FEE))],
        }
    }

    fn asset(asset_id: AssetId, balance: Balance) -> AssetWithBalance {
        AssetWithBalance {
            asset_id,
            balance: amount(balance),
        }
    }

    fn stored_pool(token0: AssetId, token1: AssetId) -> StoredPool {
        StoredPool::PrivateV2 {
            assets: (
                asset(token0, 10u128.pow(24)),
                asset(token1, 2 * 10u128.pow(24)),
            ),
            _owner_id: "owner.near".parse().unwrap(),
            fees: fees(),
            _locked: false,
        }
    }

    fn private_pool(token0: AssetId, token1: AssetId) -> Pool {
        Pool::new(0, stored_pool(token0, token1), 0).unwrap()
    }

    /// All liquidity providers take their fees with `payout_type`
    fn public_pool(token0: AssetId, token1: AssetId, payout_type: FeePayoutType) -> Pool {
        let shares = ProviderShares::new(10u128.pow(18));
        let mut payout_groups = PayoutGroups::default();
        payout_groups.group_mut(payout_type).shares =
            PayoutGroupShares::NONE.joined_by(shares).unwrap();
        Pool::new(
            0,
            StoredPool::PublicV3 {
                assets: (
                    asset(token0, 10u128.pow(24)),
                    asset(token1, 2 * 10u128.pow(24)),
                ),
                fees: fees(),
                _providers_prefix: vec![],
                total_shares: TotalShares::NONE.minted(shares).unwrap(),
                payout_groups: Box::new(payout_groups),
            },
            0,
        )
        .unwrap()
    }

    fn launch_pool(launched: AssetId) -> Pool {
        launch_pool_with_phantom_liquidity(launched, 0)
    }

    fn launch_pool_with_phantom_liquidity(launched: AssetId, phantom_liquidity: Balance) -> Pool {
        Pool::new(
            0,
            StoredPool::LaunchV2 {
                quote_asset: asset(AssetId::Near, 10u128.pow(24)),
                launched_asset: asset(launched, 2 * 10u128.pow(24)),
                fees: fees(),
                phantom_liquidity: amount(phantom_liquidity),
            },
            0,
        )
        .unwrap()
    }

    #[test]
    fn private_pool_keeps_its_fee() {
        let mut pool = private_pool(nep141("a.near"), nep141("b.near"));
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );

        let mut pool = private_pool(nep141("a.near"), nep141("b.near"));
        let amount_in = pool.swap_exact_out(0, 1, amount_out).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn public_pool_pays_its_fee_to_liquidity_providers() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"), FeePayoutType::KeepBoth);
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in
                    - fee(amount_in, PROTOCOL_FEE)
                    - fee(amount_in, POOL_FEE_AFTER_PROTOCOL),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn public_pool_keeps_the_fee_its_liquidity_providers_reinvest() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"), FeePayoutType::Reinvest);
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn protocol_fee_is_swapped_to_the_stable_token() {
        let mut pool = launch_pool(nep141("meme.near"));
        let [mut near, mut tokens] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let after_fees =
            amount_in - fee(amount_in, POOL_FEE_AFTER_PROTOCOL) - fee(amount_in, PROTOCOL_FEE);
        let amount_out = exact_in_amount_out(
            amount(after_fees),
            SwapReserves {
                input: amount(tokens),
                output: amount(near),
            },
        )
        .get();
        tokens += after_fees + fee(amount_in, POOL_FEE_AFTER_PROTOCOL);
        near -= amount_out;
        // The protocol fee, swapped to NEAR on the moved reserves
        near -= exact_in_amount_out(
            amount(fee(amount_in, PROTOCOL_FEE)),
            SwapReserves {
                input: amount(tokens),
                output: amount(near),
            },
        )
        .get();
        tokens += fee(amount_in, PROTOCOL_FEE);

        assert_eq!(pool.swap_exact_in(1, 0, amount_in), Some(amount_out));
        assert_eq!(reserves(&pool), [near, tokens]);
    }

    #[test]
    fn launch_pools_keep_phantom_liquidity_after_converting_fees() {
        let phantom_liquidity = 10u128.pow(23);
        let pool = launch_pool_with_phantom_liquidity(nep141("meme.near"), phantom_liquidity);
        // Leaves 1 more than the phantom liquidity, but converting the fees takes more than that
        let amount_out = reserves(&pool)[0] - phantom_liquidity - 1;
        assert_eq!(pool.quote_exact_out(1, 0, amount_out), None);
        assert!(pool.quote_exact_out(1, 0, amount_out / 2).is_some());
        // Buying doesn't convert fees, and only grows NEAR
        assert!(pool.quote_exact_in(0, 1, 10u128.pow(24)).is_some());
    }

    #[test]
    fn swaps_in_the_state_the_aggregator_keeps() {
        let pools = build_pools(PoolsSource {
            block_height: 1,
            timestamp: 0,
            pools: vec![stored_pool(nep141("a.near"), nep141("b.near"))],
        });
        let hook_pools = pools.hook_pools();
        assert_eq!(
            hook_pools.pools[0].tokens,
            [nep141("a.near"), nep141("b.near")]
        );
        let mut state = hook_pools.pools[0].state;
        let amount_in = 10u128.pow(22);
        let amount_out = pools
            .swap(0, &mut state, 0, Amount::AmountIn(amount_in), None)
            .unwrap();
        let mut pool = private_pool(nep141("a.near"), nep141("b.near"));
        assert_eq!(pool.swap_exact_in(0, 1, amount_in), Some(amount_out));
        assert_eq!(state.balances(), reserves(&pool));

        let mut discounted_state = hook_pools.pools[0].state;
        let discounted_amount_out = pools
            .swap(
                0,
                &mut discounted_state,
                0,
                Amount::AmountIn(amount_in),
                Some(FeeDiscountFraction::new(500_000)),
            )
            .unwrap();
        assert!(discounted_amount_out > amount_out);
        // Exact out gives the input
        let mut state = hook_pools.pools[0].state;
        assert_eq!(
            pools.swap(0, &mut state, 0, Amount::AmountOut(amount_out), None),
            Some(amount_in)
        );
    }

    #[test]
    fn swap_gas_counts_hops() {
        let route = |pool_ids: &[u32]| SwapRoute {
            fraction: RouteFraction::new(500_000),
            pool_ids: pool_ids.iter().map(|&id| PoolId::new(id)).collect(),
        };
        assert_eq!(swap_gas(&[route(&[1])]), SWAP_GAS + HOP_GAS);
        // Another route through the same pool costs less than a new pool
        assert_eq!(
            swap_gas(&[route(&[1, 2]), route(&[1])]),
            SWAP_GAS + HOP_GAS * 2 + REPEATED_HOP_GAS
        );
    }
}
