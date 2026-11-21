mod oracle_prices;

use std::sync::{Arc, Mutex, RwLock};

use amp_dex_types::SwapArgs;
use intear_dex_common::FeeDiscountFraction;
use near_min_api::types::AccountId;
use near_sdk::json_types::Base64VecU8;
use pathfinder::qoq::{self, OraclePrices, Pools, QOQ_DEX_ID, RatedPools};
use swap_router_types::{
    export_hook,
    hook::{Hook, HookPools, HookSwap, HookSwapRequest, PoolState, PoolsSnapshot},
    indexed::{AccountState, Watch},
    types::Amount,
};

use crate::oracle_prices::OraclePricesFeed;

struct Qoq {
    pools: RwLock<Option<Arc<Pools>>>,
    oracle_prices: OraclePricesFeed,
    snapshot: Mutex<Option<Snapshot>>,
}

/// The latest snapshot, and the pools and oracle prices it was rated from
struct Snapshot {
    pools: Arc<Pools>,
    oracle_prices: Option<Arc<OraclePrices>>,
    rated: Arc<QoqPools>,
}

/// The pools at a block, rated at fixed prices and the oracle's prices of a moment
struct QoqPools {
    pools: RatedPools,
}

export_hook!(Qoq);

impl Hook for Qoq {
    type Pools = QoqPools;

    fn init() -> Result<Self, String> {
        let oracle_url = std::env::var("QOQ_ORACLE_URL")
            .map_err(|err| format!("QOQ_ORACLE_URL, the oracle's HTTP API: {err}"))?;
        Ok(Self {
            pools: RwLock::default(),
            oracle_prices: OraclePricesFeed::start(oracle_url)?,
            snapshot: Mutex::default(),
        })
    }

    fn watches(&self) -> Vec<Watch> {
        vec![qoq::watch()]
    }

    fn on_indexed_state(&self, states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
        let [(_, state)] = states.as_slice() else {
            return Err(format!("Qoq watches one account, got {}", states.len()));
        };
        let height = state.block.height;
        let pools = qoq::build_indexed_pools(state)
            .map_err(|err| format!("Failed to build Qoq pools at block {height}: {err:#}"))?;
        self.oracle_prices.set_pools(pools.assets());
        *self.pools.write().unwrap() = Some(Arc::new(pools));
        Ok(())
    }

    /// The pools with a fixed price, and those whose oracle's prices are fresh
    fn pools(&self) -> Option<Arc<QoqPools>> {
        let pools = self.pools.read().unwrap().clone()?;
        pools.check_fresh().ok()?;
        let oracle_prices = self.oracle_prices.fresh();
        let mut snapshot = self.snapshot.lock().unwrap();
        let rated_from_these = snapshot.as_ref().is_some_and(|snapshot| {
            Arc::ptr_eq(&snapshot.pools, &pools)
                && match (&snapshot.oracle_prices, &oracle_prices) {
                    (Some(rated_at), Some(latest)) => Arc::ptr_eq(rated_at, latest),
                    (None, None) => true,
                    _ => false,
                }
        });
        if !rated_from_these {
            *snapshot = Some(Snapshot {
                rated: Arc::new(QoqPools {
                    pools: pools.rated(oracle_prices.as_deref()),
                }),
                pools,
                oracle_prices,
            });
        }
        snapshot
            .as_ref()
            .map(|snapshot| Arc::clone(&snapshot.rated))
    }
}

impl PoolsSnapshot for QoqPools {
    fn pools(&self) -> HookPools {
        self.pools.hook_pools()
    }

    fn swap(
        &self,
        pool: u32,
        state: &mut PoolState,
        token_in: usize,
        _token_out: usize,
        amount: Amount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<u128> {
        self.pools.swap(pool, state, token_in, amount, fee_discount)
    }

    fn build_swap(&self, request: HookSwapRequest) -> Result<HookSwap, String> {
        let dex_id = QOQ_DEX_ID.parse().unwrap();
        let routes = self.pools.swap_routes(&request.routes)?;
        let prices_request = self.pools.prices_request(&request.routes)?;
        let gas = qoq::swap_gas(&routes);
        Ok(HookSwap {
            dex_id,
            message: Base64VecU8(borsh::to_vec(&SwapArgs { routes }).unwrap()),
            gas,
            oracle_request: prices_request
                .map(|prices_request| Base64VecU8(borsh::to_vec(&prices_request).unwrap())),
        })
    }
}
