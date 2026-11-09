mod storage;

use self::storage::StoredPool;
pub use self::storage::watch;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use amp_dex_types::math::{RatedPool, SwapAmounts};
use amp_dex_types::{
    Amplification, AssetIndex, FULL_ROUTE_FRACTION, PoolAssets, PoolId, Price, PricesRequest,
    RouteFraction, SwapFees, SwapRoute, TokenAmount,
};
use bigdecimal::BigDecimal;
use intear_dex_common::{AssetId, SwapRequestAmount};
use near_min_api::types::{Balance, BlockHeight};
use near_sdk::PublicKeyHandle;
use swap_router_types::{indexed::AccountState, types::Gas};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::search::{self, Graph, MaxHops, QuoteAmount, Settings};

pub const QOQ_DEX_ID: &str = "slimedragon.near/amp";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 8,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

const SWAP_GAS: Gas = Gas::from_ggas(20_700, 20_700);
const HOP_GAS: Gas = Gas::from_ggas(4_100, 4_100);

/// A pool of the amp DEX, rated at the median of its oracle's prices
#[derive(Clone)]
pub struct Pool {
    id: PoolId,
    balances: PoolAssets<TokenAmount>,
    /// What swaps don't change, shared so simulations clone pools without allocating
    info: Arc<PoolInfo>,
}

struct PoolInfo {
    tokens: [AssetId; 2],
    decimals: PoolAssets<u8>,
    amplification: Amplification,
    /// The median of the oracle's prices of a whole token of the first asset in the second, which
    /// swaps rate the pool at
    price: Price,
    fees: SwapFees,
}

impl Pool {
    fn new(id: PoolId, stored: &StoredPool, price: Price) -> Self {
        Self {
            id,
            balances: PoolAssets([stored.assets.0.balance, stored.assets.1.balance]),
            info: Arc::new(PoolInfo {
                tokens: [
                    stored.assets.0.asset_id.clone(),
                    stored.assets.1.asset_id.clone(),
                ],
                decimals: PoolAssets([stored.decimals.0, stored.decimals.1]),
                amplification: stored.amplification,
                price,
                fees: SwapFees::of_pool_fee(stored.fee),
            }),
        }
    }

    /// Swaps the asset at `in_index` like the contract's `swap_in_pool`. `None` for a swap the
    /// contract fails. The contract pays parts of the pool fee out to providers, converting some
    /// of them into the other asset, where this leaves all of it in the pool's liquidity, so a
    /// later swap on the pool in the same simulation is off by a fraction of the fee.
    fn swap(&mut self, in_index: AssetIndex, amount: SwapRequestAmount) -> Option<SwapAmounts> {
        if self.balances.0.iter().any(|balance| balance.is_zero()) {
            return None;
        }
        let rated_pool = RatedPool {
            balances: self.balances,
            decimals: self.info.decimals,
            price: self.info.price,
            amplification: self.info.amplification,
        };
        let swap_amounts = rated_pool
            .swap_amounts(in_index, amount, self.info.fees)
            .ok()?;
        let out_index = in_index.other();
        self.balances[in_index] = self.balances[in_index]
            .checked_add(swap_amounts.amount_in_after_fees())?
            .checked_add(swap_amounts.pool_fee)?;
        self.balances[out_index] = self.balances[out_index].checked_sub(swap_amounts.amount_out)?;
        Some(swap_amounts)
    }

    /// Simulations panic on overflows like the contract
    fn unless_panicked(
        &mut self,
        swap: impl FnOnce(&mut Self) -> Option<Balance>,
    ) -> Option<Balance> {
        let id = self.id;
        std::panic::catch_unwind(AssertUnwindSafe(|| swap(self))).unwrap_or_else(|_| {
            warn!("Panicked while emulating a swap in Qoq pool {id}");
            None
        })
    }
}

impl search::Pool for Pool {
    type Token = AssetId;

    fn tokens(&self) -> Vec<AssetId> {
        self.info.tokens.to_vec()
    }

    fn swap_exact_in(
        &mut self,
        token_in: usize,
        _token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        self.unless_panicked(|pool| {
            let swap_amounts = pool.swap(
                AssetIndex::BOTH[token_in],
                SwapRequestAmount::ExactIn(TokenAmount::new(amount_in)),
            )?;
            Some(swap_amounts.amount_out.get())
        })
    }

    fn swap_exact_out(
        &mut self,
        token_in: usize,
        _token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance> {
        self.unless_panicked(|pool| {
            let swap_amounts = pool.swap(
                AssetIndex::BOTH[token_in],
                SwapRequestAmount::ExactOut(TokenAmount::new(amount_out)),
            )?;
            Some(swap_amounts.amount_in.get())
        })
    }
}

/// The pools of the amp DEX at a block, which swaps rate at their oracles' prices
pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    pools: Vec<StoredPool>,
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    Ok(Pools {
        block_height: state.block.height,
        updated_at: Instant::now(),
        pools: storage::stored_pools(state)?,
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
}

/// The medians of an oracle's prices that swaps rate pools at now
pub struct OraclePrices {
    pub oracle_key: PublicKeyHandle,
    /// Of the pools that ask the oracle, of a whole token of their first asset in their second
    pub of_pools: HashMap<PoolId, Price>,
}

pub struct Request {
    pub token_in: AssetId,
    pub token_out: AssetId,
    pub amount: QuoteAmount,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
    pub near_price_raw: BigDecimal,
    pub quoted_token_price_raw: BigDecimal,
}

/// A swap on routes of pools that ask one oracle, which the swap asks for the prices of their
/// assets first
pub struct Quote {
    pub routes: Vec<SwapRoute>,
    /// What the swap gives for exact in, or takes for exact out
    pub estimated_amount: Balance,
    /// The least it gives, or the most it takes, with the slippage
    pub worst_case_amount: Balance,
    pub oracle_key: PublicKeyHandle,
    /// As the contract asks for it, of the assets of the pools of the routes in their order
    pub prices_request: PricesRequest,
    pub swap_gas: Gas,
}

/// The best route through the pools that ask the oracle of `oracle_prices`
pub fn find_path_in(
    pools: &Pools,
    oracle_prices: &OraclePrices,
    request: &Request,
) -> Result<Quote, anyhow::Error> {
    let slippage_bp = crate::slippage_bp(&request.slippage)?;
    let graph = Graph::new(
        pools
            .pools
            .iter()
            .enumerate()
            .filter(|(_, stored)| stored.oracle_key == oracle_prices.oracle_key)
            .filter_map(|(id, stored)| {
                let id = PoolId::new(id as u32);
                Some(Pool::new(id, stored, *oracle_prices.of_pools.get(&id)?))
            })
            .collect(),
    );
    let (Some(token_in), Some(token_out)) = (
        graph.token_index(&request.token_in),
        graph.token_index(&request.token_out),
    ) else {
        anyhow::bail!("No routes found");
    };
    let split_route = search::route(
        &graph,
        token_in,
        token_out,
        request.amount,
        request.max_hops,
        SETTINGS,
        crate::gas_cost(
            HOP_GAS.estimated_burn,
            &request.near_price_raw,
            &request.quoted_token_price_raw,
        )?,
    )?;
    let estimated_amount = split_route.emulate_total(&graph, request.amount)?;
    let worst_case_amount = match request.amount {
        QuoteAmount::ExactIn(_) => {
            crate::with_slippage(estimated_amount, crate::BASIS_POINTS - slippage_bp)
        }
        QuoteAmount::ExactOut(_) => {
            crate::with_slippage(estimated_amount, crate::BASIS_POINTS + slippage_bp)
        }
    };
    let route_pools = |part: &search::SplitRoutePart| {
        part.route
            .hops
            .iter()
            .map(|hop| &graph.pools()[hop.pool])
            .collect::<Vec<_>>()
    };
    let routes = split_route
        .parts
        .iter()
        .map(|part| SwapRoute {
            fraction: RouteFraction::new(
                part.weight_percentage * (FULL_ROUTE_FRACTION.get() / 100),
            ),
            pool_ids: route_pools(part).iter().map(|pool| pool.id).collect(),
        })
        .collect();
    let prices_request = PricesRequest::of_pools(
        split_route
            .parts
            .iter()
            .flat_map(route_pools)
            .map(|pool| (&pool.info.tokens[0], &pool.info.tokens[1])),
    );
    info!(
        "Qoq route from {} to {}: {estimated_amount} {}, block {}",
        request.token_in,
        request.token_out,
        match request.amount {
            QuoteAmount::ExactIn(_) => "out",
            QuoteAmount::ExactOut(_) => "in",
        },
        pools.block_height
    );
    Ok(Quote {
        routes,
        estimated_amount,
        worst_case_amount,
        oracle_key: oracle_prices.oracle_key.clone(),
        prices_request,
        swap_gas: SWAP_GAS + HOP_GAS * split_route.hops() as u64,
    })
}

#[cfg(test)]
mod tests {
    use amp_dex_types::{AssetWithBalance, FeeFraction, PayoutGroups, TotalShares};

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
            oracle_key: oracle_key.parse().unwrap(),
            _last_oracle_price: Price::from_units(0),
            fee: FeeFraction::new(1_000),
            _providers_prefix: vec![],
            _total_shares: TotalShares::NONE,
            _payout_groups: PayoutGroups::default(),
        }
    }

    /// NEAR/USDC and USDC/XAUT pools of the oracle, and a NEAR/XAUT pool of another one
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

    fn request(amount: QuoteAmount) -> Request {
        Request {
            token_in: near(),
            token_out: xaut(),
            amount,
            max_hops: MaxHops::Four,
            slippage: "0.01".parse().unwrap(),
            near_price_raw: "0.000000000000000000000002".parse().unwrap(),
            quoted_token_price_raw: "0.004".parse().unwrap(),
        }
    }

    /// What the contract swaps in pool `pool_id` of [`pools`] at the price of [`oracle_prices`]
    fn contract_swap(pool_id: u32, amount: SwapRequestAmount) -> SwapAmounts {
        let stored = &pools().pools[pool_id as usize];
        RatedPool {
            balances: PoolAssets([stored.assets.0.balance, stored.assets.1.balance]),
            decimals: PoolAssets([stored.decimals.0, stored.decimals.1]),
            price: oracle_prices().of_pools[&PoolId::new(pool_id)],
            amplification: stored.amplification,
        }
        .swap_amounts(AssetIndex::First, amount, SwapFees::of_pool_fee(stored.fee))
        .unwrap()
    }

    #[test]
    fn routes_only_through_the_pools_of_the_oracle() {
        let near_in = 10u128.pow(26);
        let quote = find_path_in(
            &pools(),
            &oracle_prices(),
            &request(QuoteAmount::ExactIn(near_in)),
        )
        .unwrap();
        let [route] = quote.routes.as_slice() else {
            panic!("Not one route");
        };
        assert_eq!(route.fraction, FULL_ROUTE_FRACTION);
        assert_eq!(route.pool_ids, vec![PoolId::new(0), PoolId::new(1)]);
        let usdc_between = contract_swap(0, SwapRequestAmount::ExactIn(TokenAmount::new(near_in)));
        let xaut_out = contract_swap(1, SwapRequestAmount::ExactIn(usdc_between.amount_out));
        assert_eq!(quote.estimated_amount, xaut_out.amount_out.get());
        assert_eq!(quote.worst_case_amount, quote.estimated_amount * 99 / 100);
        assert_eq!(quote.oracle_key, oracle_prices().oracle_key);
        // One request of the prices of every asset of the route
        assert_eq!(quote.prices_request.asset_ids, vec![near(), usdc(), xaut()]);
        assert_eq!(quote.swap_gas, SWAP_GAS + HOP_GAS * 2);
    }

    #[test]
    fn exact_out_takes_what_each_pool_needs_from_the_last_back() {
        let xaut_out = 10u128.pow(6) / 100;
        let quote = find_path_in(
            &pools(),
            &oracle_prices(),
            &request(QuoteAmount::ExactOut(xaut_out)),
        )
        .unwrap();
        let usdc_between =
            contract_swap(1, SwapRequestAmount::ExactOut(TokenAmount::new(xaut_out)));
        let near_in = contract_swap(0, SwapRequestAmount::ExactOut(usdc_between.amount_in));
        assert_eq!(quote.estimated_amount, near_in.amount_in.get());
        assert_eq!(quote.worst_case_amount, quote.estimated_amount * 101 / 100);
    }

    #[test]
    fn pools_without_a_price_are_left_out() {
        let mut oracle_prices = oracle_prices();
        oracle_prices.of_pools.remove(&PoolId::new(1));
        assert!(
            find_path_in(
                &pools(),
                &oracle_prices,
                &request(QuoteAmount::ExactIn(10u128.pow(26)))
            )
            .is_err()
        );
    }
}
