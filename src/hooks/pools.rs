//! The pools of every hook in one graph, which routes are searched in. Swaps in them are simulated
//! by the hooks that gave them.

use std::{
    collections::HashMap,
    ffi::{c_void, CString},
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use intear_dex_common::{AssetId, FeeDiscountFraction};
use near_min_api::types::{Balance, NearGas};
use near_sdk::PublicKeyHandle;
use pathfinder::search::{self, Graph, QuoteAmount};
use swap_router_types::hook::{HookPools, HookSwap, HookSwapRequest, PoolState};

use super::{answer, loaded, Hook, HookPlugin, SwapFunction};
use crate::types::IntearAssetId;

/// A snapshot of a hook's pools, released when it's dropped
pub struct PoolsSnapshot {
    plugin: HookPlugin,
    raw: *const c_void,
}

// SAFETY: hooks can be called from any thread, and a snapshot doesn't change
unsafe impl Send for PoolsSnapshot {}
unsafe impl Sync for PoolsSnapshot {}

impl PoolsSnapshot {
    /// None while the hook has no pools
    fn current(hook: &Hook) -> Option<Self> {
        // SAFETY: the hook returns a snapshot that's released when this is dropped, or null
        let raw = unsafe { (hook.plugin.pools_snapshot)() };
        (!raw.is_null()).then(|| Self {
            plugin: hook.plugin.clone(),
            raw,
        })
    }

    fn pools(&self) -> Result<HookPools, String> {
        // SAFETY: the snapshot isn't released yet
        let output = unsafe { (self.plugin.snapshot_pools)(self.raw) };
        answer(&self.plugin, "snapshot_pools", output)
    }

    /// Of the hook
    pub fn name(&self) -> &str {
        &self.plugin.name
    }

    pub fn build_swap(&self, request: &HookSwapRequest) -> Result<HookSwap, String> {
        let request =
            CString::new(serde_json::to_string(request).unwrap()).expect("JSON has no NUL bytes");
        // SAFETY: the snapshot isn't released yet, and the hook gets a NUL-terminated CString
        // that outlives the call
        let output = unsafe { (self.plugin.build_swap)(self.raw, request.as_ptr()) };
        answer(&self.plugin, "build_swap", output)
    }
}

#[cfg(test)]
impl PoolsSnapshot {
    /// Of a hook made of `plugin`'s functions
    pub(super) fn of_plugin(plugin: HookPlugin, raw: *const c_void) -> Self {
        Self { plugin, raw }
    }
}

impl Drop for PoolsSnapshot {
    fn drop(&mut self) {
        // SAFETY: the snapshot isn't used after this
        unsafe { (self.plugin.release_pools_snapshot)(self.raw) }
    }
}

/// A pool of a hook, whose swaps the hook simulates. Copying it is copying its state.
#[derive(Clone)]
pub struct HookPool {
    swap: SwapFunction,
    /// Of the snapshot of [`RoutingPools::snapshots`] that has the pool, which outlives the pool
    snapshot: *const c_void,
    /// Index in the hook's [`HookPools::pools`]
    pub pool: u32,
    /// Index in [`RoutingPools::pools`]
    pub index: u32,
    /// Indices in [`RoutingPools::tokens`]
    tokens: [u32; 2],
    state: PoolState,
}

// SAFETY: the snapshot that a pool points into can be used from any thread, and outlives it
unsafe impl Send for HookPool {}
unsafe impl Sync for HookPool {}

impl HookPool {
    /// Between tokens by their index in [`RoutingPools::tokens`]: the output for exact in, the
    /// input for exact out
    pub fn swap_tokens(
        &mut self,
        token_in: u32,
        token_out: u32,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Balance> {
        let position = |token| {
            self.tokens
                .iter()
                .position(|&pool_token| pool_token == token)
        };
        self.swap(
            position(token_in)?,
            position(token_out)?,
            amount,
            fee_discount,
        )
    }

    fn swap(
        &mut self,
        position_in: usize,
        position_out: usize,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Balance> {
        let (amount, exact_in) = match amount {
            QuoteAmount::ExactIn(amount_in) => (amount_in, true),
            QuoteAmount::ExactOut(amount_out) => (amount_out, false),
        };
        let mut result = 0;
        // SAFETY: the snapshot outlives the pool, and the state and result are valid for the call
        let swapped = unsafe {
            (self.swap)(
                self.snapshot,
                self.pool,
                &mut self.state,
                position_in as u8,
                position_out as u8,
                amount,
                exact_in,
                fee_discount.map_or(0, FeeDiscountFraction::get),
                &mut result,
            )
        };
        swapped.then_some(result)
    }
}

impl search::Pool for HookPool {
    type Token = u32;

    fn tokens(&self) -> Vec<u32> {
        self.tokens.to_vec()
    }

    fn swap_exact_in(
        &mut self,
        token_in: usize,
        token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        self.swap(token_in, token_out, QuoteAmount::ExactIn(amount_in), None)
    }

    fn swap_exact_out(
        &mut self,
        token_in: usize,
        token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance> {
        self.swap(token_in, token_out, QuoteAmount::ExactOut(amount_out), None)
    }
}

/// A hook's snapshot of pools
pub struct HookSnapshot {
    pub snapshot: PoolsSnapshot,
    pools: HookPools,
}

/// What [`HookPool`] doesn't need for simulations
pub struct PoolOrigin {
    /// Index in [`RoutingPools::snapshots`]
    pub snapshot: usize,
    hop_gas: NearGas,
    pub oracle_key: Option<PublicKeyHandle>,
}

/// Which pools a route can go through: of one hook, or all of them, and the ones that read the
/// prices of an oracle besides those that don't
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolsScope {
    /// Index in [`RoutingPools::snapshots`]
    pub snapshot: Option<usize>,
    pub oracle_key: Option<PublicKeyHandle>,
}

/// A graph of [`RoutingPools`], which can't outlive them, as its pools point into their snapshots
pub struct PoolsGraphOf<'a> {
    graph: Arc<PoolsGraph>,
    _pools: PhantomData<&'a RoutingPools>,
}

impl std::ops::Deref for PoolsGraphOf<'_> {
    type Target = PoolsGraph;

    fn deref(&self) -> &PoolsGraph {
        &self.graph
    }
}

pub struct PoolsGraph {
    pub graph: Graph<HookPool>,
    /// The least a hop through a pool of the graph burns
    pub min_hop_gas: NearGas,
}

/// The pools of all hooks at a moment
pub struct RoutingPools {
    pub snapshots: Vec<HookSnapshot>,
    /// Of the pools, which pools refer to by index
    pub tokens: Vec<AssetId>,
    token_indices: HashMap<AssetId, u32>,
    pools: Vec<HookPool>,
    pub origins: Vec<PoolOrigin>,
    /// Made the first time they're asked for
    graphs: Mutex<HashMap<PoolsScope, Option<Arc<PoolsGraph>>>>,
}

static ROUTING_POOLS: Mutex<Option<Arc<RoutingPools>>> = Mutex::new(None);

/// The pools that hooks have now. None if no hook has pools.
pub fn routing_pools() -> Option<Arc<RoutingPools>> {
    let snapshots = loaded()
        .iter()
        .filter_map(PoolsSnapshot::current)
        .collect::<Vec<_>>();
    if snapshots.is_empty() {
        return None;
    }
    let mut routing_pools = ROUTING_POOLS.lock().unwrap();
    let unchanged = routing_pools.as_ref().is_some_and(|routing_pools| {
        routing_pools.snapshots.len() == snapshots.len()
            && routing_pools
                .snapshots
                .iter()
                .zip(&snapshots)
                .all(|(previous, snapshot)| previous.snapshot.raw == snapshot.raw)
    });
    if !unchanged {
        let snapshots = snapshots
            .into_iter()
            .filter_map(|snapshot| {
                let unchanged_pools = routing_pools.as_ref().and_then(|routing_pools| {
                    routing_pools
                        .snapshots
                        .iter()
                        .find(|previous| previous.snapshot.raw == snapshot.raw)
                        .map(|previous| previous.pools.clone())
                });
                let pools = match unchanged_pools {
                    Some(pools) => pools,
                    None => snapshot
                        .pools()
                        .inspect_err(|err| {
                            tracing::error!("No pools of {}: {err}", snapshot.name())
                        })
                        .ok()?,
                };
                Some(HookSnapshot { snapshot, pools })
            })
            .collect();
        *routing_pools = Some(Arc::new(RoutingPools::new(snapshots)));
    }
    routing_pools.clone()
}

impl RoutingPools {
    fn new(snapshots: Vec<HookSnapshot>) -> Self {
        let mut tokens = Vec::new();
        let mut token_indices = HashMap::new();
        let mut pools = Vec::new();
        let mut origins = Vec::new();
        for (snapshot_index, snapshot) in snapshots.iter().enumerate() {
            for (pool_index, pool) in snapshot.pools.pools.iter().enumerate() {
                // Routes on Intear DEX only go through NEAR and NEP-141s
                if pool
                    .tokens
                    .iter()
                    .any(|token| IntearAssetId::try_from(token.clone()).is_err())
                {
                    continue;
                }
                let pool_tokens = pool.tokens.clone().map(|token| {
                    *token_indices.entry(token.clone()).or_insert_with(|| {
                        tokens.push(token);
                        tokens.len() as u32 - 1
                    })
                });
                pools.push(HookPool {
                    swap: snapshot.snapshot.plugin.swap,
                    snapshot: snapshot.snapshot.raw,
                    pool: pool_index as u32,
                    index: pools.len() as u32,
                    tokens: pool_tokens,
                    state: pool.state,
                });
                origins.push(PoolOrigin {
                    snapshot: snapshot_index,
                    hop_gas: pool.hop_gas,
                    oracle_key: pool.oracle_key.clone(),
                });
            }
        }
        Self {
            snapshots,
            tokens,
            token_indices,
            pools,
            origins,
            graphs: Mutex::default(),
        }
    }

    /// With every pool of the snapshots
    #[cfg(test)]
    pub(super) fn of_snapshots(snapshots: Vec<PoolsSnapshot>) -> Self {
        Self::new(
            snapshots
                .into_iter()
                .map(|snapshot| HookSnapshot {
                    pools: snapshot.pools().unwrap(),
                    snapshot,
                })
                .collect(),
        )
    }

    pub fn token_index(&self, asset_id: &AssetId) -> Option<u32> {
        self.token_indices.get(asset_id).copied()
    }

    /// Index in [`RoutingPools::snapshots`] of the hook's snapshot
    pub fn snapshot_of(&self, hook: &Hook) -> Option<usize> {
        self.snapshots
            .iter()
            .position(|snapshot| snapshot.snapshot.plugin.name == hook.plugin.name)
    }

    /// What each oracle that pools in `snapshot`, or any of them, read takes for a request in
    /// `asset_id`
    pub fn oracle_fees_in(
        &self,
        snapshot: Option<usize>,
        asset_id: &AssetId,
    ) -> Vec<(PublicKeyHandle, u128)> {
        self.snapshots
            .iter()
            .enumerate()
            .filter(|(index, _)| snapshot.is_none_or(|snapshot| snapshot == *index))
            .flat_map(|(_, snapshot)| &snapshot.pools.oracle_fees)
            .filter_map(|oracle_fees| {
                let fee = oracle_fees.fees.get(asset_id)?;
                Some((oracle_fees.oracle_key.clone(), fee.0))
            })
            .collect::<HashMap<_, _>>()
            .into_iter()
            .collect()
    }

    /// The pools of the scope, None without any. Its pools are only used while these pools live.
    pub fn graph(&self, scope: &PoolsScope) -> Option<PoolsGraphOf<'_>> {
        let graph = self
            .graphs
            .lock()
            .unwrap()
            .entry(scope.clone())
            .or_insert_with(|| {
                let in_scope = self
                    .pools
                    .iter()
                    .zip(&self.origins)
                    .filter(|(_, origin)| {
                        scope
                            .snapshot
                            .is_none_or(|snapshot| snapshot == origin.snapshot)
                            && origin
                                .oracle_key
                                .as_ref()
                                .is_none_or(|key| scope.oracle_key.as_ref() == Some(key))
                    })
                    .collect::<Vec<_>>();
                let min_hop_gas = in_scope.iter().map(|(_, origin)| origin.hop_gas).min()?;
                Some(Arc::new(PoolsGraph {
                    graph: Graph::new(in_scope.into_iter().map(|(pool, _)| pool.clone()).collect()),
                    min_hop_gas,
                }))
            })
            .clone()?;
        Some(PoolsGraphOf {
            graph,
            _pools: PhantomData,
        })
    }
}
