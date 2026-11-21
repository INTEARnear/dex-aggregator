use std::sync::{Arc, RwLock};

use intear_dex_common::FeeDiscountFraction;
use near_min_api::types::AccountId;
use near_sdk::json_types::Base64VecU8;
use pathfinder::plach::{self, PLACH_DEX_ID};
use swap_router_types::{
    export_hook,
    hook::{Hook, HookPools, HookSwap, HookSwapRequest, PoolState, PoolsSnapshot},
    indexed::{AccountState, Watch},
    types::Amount,
};
use xyk_dex_types::SwapArgs;

struct Plach {
    pools: RwLock<Option<Arc<PlachPools>>>,
}

/// The pools at a block
struct PlachPools {
    pools: plach::Pools,
}

export_hook!(Plach);

impl Hook for Plach {
    type Pools = PlachPools;

    fn init() -> Result<Self, String> {
        Ok(Self {
            pools: RwLock::default(),
        })
    }

    fn watches(&self) -> Vec<Watch> {
        vec![plach::watch()]
    }

    fn on_indexed_state(&self, states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
        let [(_, state)] = states.as_slice() else {
            return Err(format!("Plach watches one account, got {}", states.len()));
        };
        let height = state.block.height;
        let pools = plach::build_indexed_pools(state)
            .map_err(|err| format!("Failed to build Plach pools at block {height}: {err:#}"))?;
        *self.pools.write().unwrap() = Some(Arc::new(PlachPools { pools }));
        Ok(())
    }

    fn pools(&self) -> Option<Arc<PlachPools>> {
        let pools = self.pools.read().unwrap().clone()?;
        pools.pools.check_fresh().ok()?;
        Some(pools)
    }
}

impl PoolsSnapshot for PlachPools {
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
        let dex_id = PLACH_DEX_ID.parse().unwrap();
        let routes = self.pools.swap_routes(&request.routes)?;
        let gas = plach::swap_gas(&routes);
        Ok(HookSwap {
            dex_id,
            message: Base64VecU8(borsh::to_vec(&SwapArgs { routes }).unwrap()),
            gas,
            oracle_request: None,
        })
    }
}
