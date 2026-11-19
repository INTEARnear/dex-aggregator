use std::sync::{Arc, RwLock};

use intear_dex_common::FeeDiscountFraction;
use near_min_api::types::AccountId;
use near_sdk::json_types::Base64VecU8;
use pathfinder::plach::{self, PLACH_DEX_ID};
use swap_router_types::{
    export_hook,
    fee_discount::FeeDiscounter,
    hook::{Hook, HookPools, HookSwap, HookSwapRequest, PoolState, PoolsSnapshot},
    indexed::{AccountState, Watch},
    types::Amount,
};
use xyk_dex_types::SwapArgs;

struct Plach {
    fee_discounter: Arc<FeeDiscounter>,
    pools: RwLock<Option<Arc<PlachPools>>>,
}

/// The pools at a block
struct PlachPools {
    pools: plach::Pools,
    fee_discounter: Arc<FeeDiscounter>,
}

export_hook!(Plach);

impl Hook for Plach {
    type Pools = PlachPools;

    fn init() -> Result<Self, String> {
        Ok(Self {
            fee_discounter: Arc::new(FeeDiscounter::from_env()?),
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
        *self.pools.write().unwrap() = Some(Arc::new(PlachPools {
            pools,
            fee_discounter: Arc::clone(&self.fee_discounter),
        }));
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
        let fee_discount = self
            .fee_discounter
            .sign_swap(&dex_id, routes.clone(), &request)?;
        let gas = plach::swap_gas(&routes, request.fee_discount.is_some());
        Ok(HookSwap {
            dex_id,
            message: Base64VecU8(
                borsh::to_vec(&SwapArgs {
                    routes,
                    fee_discount,
                })
                .unwrap(),
            ),
            gas,
            oracle_request: None,
        })
    }
}
