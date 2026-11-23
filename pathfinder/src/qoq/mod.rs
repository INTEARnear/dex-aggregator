mod storage;

use self::storage::StoredPool;
pub use self::storage::watch;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use amp_dex_types::math::{RatedPool, SwapAmounts};
use amp_dex_types::{
    Amplification, AssetIndex, FeeDiscountFraction, PoolAssets, PoolId, PoolRateSource, Price,
    PricesRequest, RouteFraction, SwapFees, SwapRoute, TokenAmount,
};
use intear_dex_common::{AssetId, SwapRequestAmount};
use near_min_api::types::{Balance, BlockHeight};
use near_sdk::PublicKeyHandle;
use swap_router_types::{
    hook::{HookPool, HookPools, HookSwapRoute, OracleFees as HookOracleFees, PoolState},
    indexed::AccountState,
    types::{Amount, Gas},
};
use tokio::time::Instant;
use tracing::warn;

use crate::intear_dex::{self, OracleFees};

pub const QOQ_DEX_ID: &str = "slimedragon.near/amp";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);

const SWAP_GAS: Gas = Gas::from_ggas(6_820, 6_820);
/// Measured as another route through the same pool, a hop through another pool also loads it
const ORACLE_HOP_GAS: Gas = Gas::from_ggas(6_030, 6_030);
const FIXED_PRICE_HOP_GAS: Gas = Gas::from_ggas(1_990, 1_990);

/// A pool of the amp DEX, rated at its fixed price or the median of its oracle's prices
pub struct Pool {
    id: PoolId,
    tokens: [AssetId; 2],
    balances: PoolAssets<TokenAmount>,
    decimals: PoolAssets<u8>,
    amplification: Amplification,
    /// Of a whole token of the first asset in the second, which swaps rate the pool at: its fixed
    /// price, or the median of its oracle's prices
    price: Price,
    fees: SwapFees,
    /// `None` for a pool with a fixed price
    oracle_key: Option<PublicKeyHandle>,
}

impl Pool {
    fn hop_gas(&self) -> Gas {
        match self.oracle_key {
            Some(_) => ORACLE_HOP_GAS,
            None => FIXED_PRICE_HOP_GAS,
        }
    }

    fn new(id: PoolId, stored: &StoredPool, price: Price) -> Self {
        Self {
            id,
            tokens: [
                stored.assets.0.asset_id.clone(),
                stored.assets.1.asset_id.clone(),
            ],
            balances: PoolAssets([stored.assets.0.balance, stored.assets.1.balance]),
            decimals: PoolAssets([stored.decimals.0, stored.decimals.1]),
            amplification: stored.amplification,
            price,
            fees: SwapFees::of_pool_fee(stored.fee),
            oracle_key: match &stored.rate_source {
                PoolRateSource::Oracle { key, .. } => Some(key.clone()),
                PoolRateSource::Fixed(_) => None,
            },
        }
    }

    /// Swaps the asset at `in_index` like the contract's `swap_in_pool` in the pool with
    /// `balances`, which it changes like the swap would. `None` for a swap the contract fails. The
    /// contract pays parts of the pool fee out to providers, converting some of them into the
    /// other asset, where this leaves all of it in the pool's liquidity, so a later swap on the
    /// pool in the same simulation is off by a fraction of the fee.
    fn swap(
        &self,
        balances: &mut PoolAssets<TokenAmount>,
        in_index: AssetIndex,
        amount: SwapRequestAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<SwapAmounts> {
        // Simulations panic on overflows like the contract
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.swap_unless_panicked(balances, in_index, amount, fee_discount)
        }))
        .unwrap_or_else(|_| {
            warn!("Panicked while emulating a swap in Qoq pool {}", self.id);
            None
        })
    }

    fn swap_unless_panicked(
        &self,
        balances: &mut PoolAssets<TokenAmount>,
        in_index: AssetIndex,
        amount: SwapRequestAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<SwapAmounts> {
        if balances.0.iter().any(|balance| balance.is_zero()) {
            return None;
        }
        let rated_pool = RatedPool {
            balances: *balances,
            decimals: self.decimals,
            price: self.price,
            amplification: self.amplification,
        };
        let fees = match fee_discount {
            Some(fee_discount) => self.fees.discounted(fee_discount),
            None => self.fees,
        };
        let swap_amounts = rated_pool.swap_amounts(in_index, amount, fees).ok()?;
        let out_index = in_index.other();
        let mut swapped = *balances;
        swapped[in_index] = swapped[in_index]
            .checked_add(swap_amounts.amount_in_after_fees())?
            .checked_add(swap_amounts.pool_fee)?;
        swapped[out_index] = swapped[out_index].checked_sub(swap_amounts.amount_out)?;
        *balances = swapped;
        Some(swap_amounts)
    }
}

/// The pools of the amp DEX at a block, which swaps rate at their oracles' prices
pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    pools: Vec<StoredPool>,
    oracle_fees: OracleFees,
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    Ok(Pools {
        block_height: state.block.height,
        updated_at: Instant::now(),
        pools: storage::stored_pools(state)?,
        oracle_fees: intear_dex::oracle_fees(state)?,
    })
}

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

    /// The assets of each pool, which swaps ask its oracle for the prices of
    pub fn assets(&self) -> Vec<(PoolId, [AssetId; 2])> {
        self.pools
            .iter()
            .enumerate()
            .map(|(id, pool)| {
                (
                    PoolId::new(id as u32),
                    [
                        pool.assets.0.asset_id.clone(),
                        pool.assets.1.asset_id.clone(),
                    ],
                )
            })
            .collect()
    }

    /// The pools that have a fixed price or ask the oracle of `oracle_prices`, which only the
    /// pools with a fixed price are without, at those prices
    pub fn rated(&self, oracle_prices: Option<&OraclePrices>) -> RatedPools {
        let pools = self
            .pools
            .iter()
            .enumerate()
            .filter_map(|(id, stored)| {
                let id = PoolId::new(id as u32);
                let price = match &stored.rate_source {
                    PoolRateSource::Oracle { key, .. } => {
                        let oracle_prices =
                            oracle_prices.filter(|prices| prices.oracle_key == *key)?;
                        *oracle_prices.of_pools.get(&id)?
                    }
                    PoolRateSource::Fixed(price) => *price,
                };
                Some(Pool::new(id, stored, price))
            })
            .collect::<Vec<_>>();
        let oracle_fees = oracle_prices
            .filter(|oracle_prices| {
                pools
                    .iter()
                    .any(|pool| pool.oracle_key.as_ref() == Some(&oracle_prices.oracle_key))
            })
            .and_then(|oracle_prices| {
                let fees = self.oracle_fees.get(&oracle_prices.oracle_key)?;
                Some(HookOracleFees {
                    oracle_key: oracle_prices.oracle_key.clone(),
                    fees: fees
                        .iter()
                        .map(|(asset_id, fee)| (asset_id.clone(), fee.get().into()))
                        .collect(),
                })
            });
        RatedPools {
            pools,
            oracle_fees: oracle_fees.into_iter().collect(),
        }
    }
}

/// The medians of an oracle's prices that swaps rate pools at now
pub struct OraclePrices {
    pub oracle_key: PublicKeyHandle,
    /// Of the pools that ask the oracle, of a whole token of their first asset in their second
    pub of_pools: HashMap<PoolId, Price>,
}

/// Pools at the prices that swaps rate them at
pub struct RatedPools {
    pools: Vec<Pool>,
    oracle_fees: Vec<HookOracleFees>,
}

impl RatedPools {
    /// The pools, which routes refer to by their position here
    pub fn hook_pools(&self) -> HookPools {
        HookPools {
            pools: self
                .pools
                .iter()
                .map(|pool| HookPool {
                    tokens: pool.tokens.clone(),
                    state: PoolState::of_balances(pool.balances.0.map(TokenAmount::get)),
                    hop_gas: pool.hop_gas().estimated_burn,
                    oracle_key: pool.oracle_key.clone(),
                })
                .collect(),
            oracle_fees: self.oracle_fees.clone(),
        }
    }

    /// Swaps in the pool at `pool` of [`RatedPools::hook_pools`], whose balances are in `state`
    pub fn swap(
        &self,
        pool: u32,
        state: &mut PoolState,
        token_in: usize,
        amount: Amount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Balance> {
        let pool = &self.pools[pool as usize];
        let mut balances = PoolAssets(state.balances().map(TokenAmount::new));
        let swap_amounts = pool.swap(
            &mut balances,
            AssetIndex::BOTH[token_in],
            match amount {
                Amount::AmountIn(amount_in) => {
                    SwapRequestAmount::ExactIn(TokenAmount::new(amount_in))
                }
                Amount::AmountOut(amount_out) => {
                    SwapRequestAmount::ExactOut(TokenAmount::new(amount_out))
                }
            },
            fee_discount,
        )?;
        *state = PoolState::of_balances(balances.0.map(TokenAmount::get));
        Some(match amount {
            Amount::AmountIn(_) => swap_amounts.amount_out.get(),
            Amount::AmountOut(_) => swap_amounts.amount_in.get(),
        })
    }

    /// What a swap on `routes` burns
    pub fn swap_gas(&self, routes: &[HookSwapRoute]) -> Result<Gas, String> {
        Ok(SWAP_GAS
            + self
                .pools_of(routes)?
                .into_iter()
                .flatten()
                .map(Pool::hop_gas)
                .sum::<Gas>())
    }

    fn pools_of(&self, routes: &[HookSwapRoute]) -> Result<Vec<Vec<&Pool>>, String> {
        routes
            .iter()
            .map(|route| {
                route
                    .pools
                    .iter()
                    .map(|&pool| {
                        self.pools
                            .get(pool as usize)
                            .ok_or_else(|| format!("No pool {pool} in the snapshot"))
                    })
                    .collect()
            })
            .collect()
    }

    /// The routes of a swap through pools by their position in [`RatedPools::hook_pools`]
    pub fn swap_routes(&self, routes: &[HookSwapRoute]) -> Result<Vec<SwapRoute>, String> {
        Ok(self
            .pools_of(routes)?
            .into_iter()
            .zip(routes)
            .map(|(pools, route)| SwapRoute {
                fraction: RouteFraction::new(route.fraction),
                pool_ids: pools.iter().map(|pool| pool.id).collect(),
            })
            .collect())
    }

    /// What a swap on `routes` asks their oracle for before it swaps: the prices of the assets
    /// of the pools that the oracle rates, in their order. `None` if every pool of the routes has
    /// a fixed price.
    pub fn prices_request(
        &self,
        routes: &[HookSwapRoute],
    ) -> Result<Option<PricesRequest>, String> {
        let oracle_rated_pools = self
            .pools_of(routes)?
            .into_iter()
            .flatten()
            .filter(|pool| pool.oracle_key.is_some())
            .collect::<Vec<_>>();
        Ok((!oracle_rated_pools.is_empty()).then(|| {
            PricesRequest::of_pools(
                oracle_rated_pools
                    .iter()
                    .map(|pool| (&pool.tokens[0], &pool.tokens[1])),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use amp_dex_types::{
        AssetWithBalance, FULL_ROUTE_FRACTION, FeeFraction, PayoutGroups, TotalShares,
    };

    use super::*;

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const OTHER_ORACLE_KEY: &str = "ed25519:US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx";

    fn near() -> AssetId {
        AssetId::Near
    }

    fn usdc() -> AssetId {
        AssetId::Nep141("usdc.near".parse().unwrap())
    }

    fn xaut() -> AssetId {
        AssetId::Nep141("xaut.near".parse().unwrap())
    }

    /// A pool of 0.1% fee with `balances` of assets of `decimals`
    fn stored_pool(
        assets: [AssetId; 2],
        decimals: (u8, u8),
        balances: (Balance, Balance),
        oracle_key: &str,
    ) -> StoredPool {
        let [asset_0, asset_1] = assets;
        StoredPool {
            assets: (
                AssetWithBalance {
                    asset_id: asset_0,
                    balance: TokenAmount::new(balances.0),
                },
                AssetWithBalance {
                    asset_id: asset_1,
                    balance: TokenAmount::new(balances.1),
                },
            ),
            decimals,
            amplification: Amplification::new(100),
            rate_source: PoolRateSource::Oracle {
                key: oracle_key.parse().unwrap(),
                last_price: Price::from_units(0),
            },
            fee: FeeFraction::new(1_000),
            _providers_prefix: vec![],
            _total_shares: TotalShares::NONE,
            _payout_groups: PayoutGroups::default(),
        }
    }

    /// NEAR/USDC and USDC/XAUT pools of the oracle, which takes 0.001 NEAR or 0.003 USDC for a
    /// request, and a NEAR/XAUT pool of another one
    fn pools() -> Pools {
        Pools {
            block_height: 1,
            updated_at: Instant::now(),
            pools: vec![
                stored_pool(
                    [near(), usdc()],
                    (24, 6),
                    (10u128.pow(30), 2_400_000 * 10u128.pow(6)),
                    ORACLE_KEY,
                ),
                stored_pool(
                    [usdc(), xaut()],
                    (6, 6),
                    (4_000_000 * 10u128.pow(6), 1_000 * 10u128.pow(6)),
                    ORACLE_KEY,
                ),
                stored_pool(
                    [near(), xaut()],
                    (24, 6),
                    (10u128.pow(30), 600 * 10u128.pow(6)),
                    OTHER_ORACLE_KEY,
                ),
            ],
            oracle_fees: HashMap::from([(
                ORACLE_KEY.parse().unwrap(),
                HashMap::from([
                    (near(), TokenAmount::new(10u128.pow(21))),
                    (usdc(), TokenAmount::new(3_000)),
                ]),
            )]),
        }
    }

    fn oracle_prices() -> OraclePrices {
        OraclePrices {
            oracle_key: ORACLE_KEY.parse().unwrap(),
            of_pools: HashMap::from([
                (PoolId::new(0), "2.4".parse().unwrap()),
                (PoolId::new(1), "0.00025".parse().unwrap()),
                (PoolId::new(2), "0.0006".parse().unwrap()),
            ]),
        }
    }

    /// What the contract swaps in pool `pool_id` of `pools` at the price of [`oracle_prices`]
    fn contract_swap(
        pools: &Pools,
        pool_id: u32,
        amount: SwapRequestAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> SwapAmounts {
        let stored = &pools.pools[pool_id as usize];
        let fees = SwapFees::of_pool_fee(stored.fee);
        RatedPool {
            balances: PoolAssets([stored.assets.0.balance, stored.assets.1.balance]),
            decimals: PoolAssets([stored.decimals.0, stored.decimals.1]),
            price: oracle_prices().of_pools[&PoolId::new(pool_id)],
            amplification: stored.amplification,
        }
        .swap_amounts(
            AssetIndex::First,
            amount,
            fee_discount.map_or(fees, |fee_discount| fees.discounted(fee_discount)),
        )
        .unwrap()
    }

    fn route(pools: &[u32]) -> HookSwapRoute {
        HookSwapRoute {
            fraction: FULL_ROUTE_FRACTION.get(),
            pools: pools.to_vec(),
        }
    }

    #[test]
    fn rates_only_the_pools_of_the_oracle() {
        let rated = pools().rated(Some(&oracle_prices()));
        let hook_pools = rated.hook_pools();
        // NEAR/XAUT asks another oracle
        assert_eq!(
            hook_pools
                .pools
                .iter()
                .map(|pool| pool.tokens.clone())
                .collect::<Vec<_>>(),
            vec![[near(), usdc()], [usdc(), xaut()]]
        );
        let oracle_key: PublicKeyHandle = ORACLE_KEY.parse().unwrap();
        assert!(
            hook_pools
                .pools
                .iter()
                .all(|pool| pool.oracle_key.as_ref() == Some(&oracle_key))
        );
        // The oracle takes its fee in the assets it set it in
        let [oracle_fees] = hook_pools.oracle_fees.as_slice() else {
            panic!("Not the fees of one oracle");
        };
        assert_eq!(oracle_fees.oracle_key, oracle_key);
        assert_eq!(oracle_fees.fees[&near()], 10u128.pow(21).into());
        assert_eq!(oracle_fees.fees[&usdc()], 3_000.into());
        assert!(!oracle_fees.fees.contains_key(&xaut()));
    }

    #[test]
    fn swaps_like_the_contract_in_the_state_the_aggregator_keeps() {
        let pools = pools();
        let rated = pools.rated(Some(&oracle_prices()));
        let near_in = 10u128.pow(26);
        let mut state = rated.hook_pools().pools[0].state;
        let usdc_out = rated
            .swap(0, &mut state, 0, Amount::AmountIn(near_in), None)
            .unwrap();
        let contract = contract_swap(
            &pools,
            0,
            SwapRequestAmount::ExactIn(TokenAmount::new(near_in)),
            None,
        );
        assert_eq!(usdc_out, contract.amount_out.get());
        let [near_balance, usdc_balance] = state.balances();
        assert_eq!(
            near_balance,
            10u128.pow(30) + contract.amount_in_after_fees().get() + contract.pool_fee.get()
        );
        assert_eq!(usdc_balance, 2_400_000 * 10u128.pow(6) - usdc_out);

        let usdc_wanted = 10u128.pow(6) * 100;
        let mut state = rated.hook_pools().pools[0].state;
        assert_eq!(
            rated.swap(0, &mut state, 0, Amount::AmountOut(usdc_wanted), None),
            Some(
                contract_swap(
                    &pools,
                    0,
                    SwapRequestAmount::ExactOut(TokenAmount::new(usdc_wanted)),
                    None
                )
                .amount_in
                .get()
            )
        );
    }

    #[test]
    fn fee_discount_takes_off_pool_and_protocol_fees() {
        let pools = pools();
        let rated = pools.rated(Some(&oracle_prices()));
        let near_in = 10u128.pow(26);
        let fee_discount = FeeDiscountFraction::new(250_000);
        let mut state = rated.hook_pools().pools[0].state;
        let discounted = rated
            .swap(
                0,
                &mut state,
                0,
                Amount::AmountIn(near_in),
                Some(fee_discount),
            )
            .unwrap();
        let contract = contract_swap(
            &pools,
            0,
            SwapRequestAmount::ExactIn(TokenAmount::new(near_in)),
            Some(fee_discount),
        );
        assert_eq!(discounted, contract.amount_out.get());
        let mut state = rated.hook_pools().pools[0].state;
        assert!(
            rated
                .swap(0, &mut state, 0, Amount::AmountIn(near_in), None)
                .unwrap()
                < discounted
        );
    }

    #[test]
    fn pools_without_a_price_are_left_out() {
        let mut oracle_prices = oracle_prices();
        oracle_prices.of_pools.remove(&PoolId::new(1));
        let hook_pools = pools().rated(Some(&oracle_prices)).hook_pools();
        assert_eq!(hook_pools.pools.len(), 1);
        assert_eq!(hook_pools.pools[0].tokens, [near(), usdc()]);
        // Without the oracle's prices, neither it nor its fees
        let hook_pools = pools().rated(None).hook_pools();
        assert!(hook_pools.pools.is_empty());
        assert!(hook_pools.oracle_fees.is_empty());
    }

    /// [`pools`] with the USDC/XAUT pool at a fixed price, the one that the oracle has for it
    fn pools_with_a_fixed_price() -> Pools {
        let mut pools = pools();
        pools.pools[1].rate_source =
            PoolRateSource::Fixed(oracle_prices().of_pools[&PoolId::new(1)]);
        pools
    }

    #[test]
    fn asks_the_oracle_only_for_the_pools_it_rates() {
        let rated = pools_with_a_fixed_price().rated(Some(&oracle_prices()));
        let request = rated.prices_request(&[route(&[0, 1])]).unwrap().unwrap();
        assert_eq!(request.asset_ids, vec![near(), usdc()]);
        assert_eq!(rated.prices_request(&[route(&[1])]).unwrap(), None);
        assert!(rated.prices_request(&[route(&[2])]).is_err());
        assert_eq!(
            rated.swap_routes(&[route(&[0, 1])]).unwrap(),
            vec![SwapRoute {
                fraction: FULL_ROUTE_FRACTION,
                pool_ids: vec![PoolId::new(0), PoolId::new(1)],
            }]
        );
    }

    #[test]
    fn pools_with_a_fixed_price_need_no_oracle() {
        let hook_pools = pools_with_a_fixed_price().rated(None).hook_pools();
        assert_eq!(hook_pools.pools.len(), 1);
        assert_eq!(hook_pools.pools[0].tokens, [usdc(), xaut()]);
        assert_eq!(hook_pools.pools[0].oracle_key, None);
        assert!(hook_pools.oracle_fees.is_empty());
    }

    #[test]
    fn swap_gas_counts_hops_by_their_pool() {
        let rated = pools_with_a_fixed_price().rated(Some(&oracle_prices()));
        assert_eq!(
            rated.swap_gas(&[route(&[0, 1]), route(&[1])]).unwrap(),
            SWAP_GAS + ORACLE_HOP_GAS + FIXED_PRICE_HOP_GAS * 2
        );
        assert!(rated.swap_gas(&[route(&[2])]).is_err());
    }
}
