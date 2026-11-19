//! Routes through the pools of hooks on Intear DEX. One search goes through the pools of every hook,
//! and a route that goes through the pools of several becomes swaps on their DEXes one after
//! another. A fee discount is shared by all pools of a route: the last look's, to match the best
//! quote of the other DEXes, and the TEAR holder's on top of it.

use std::sync::Arc;

use bigdecimal::num_bigint::BigUint;
use intear_dex_common::{
    AssetId, FeeDiscountFraction, RouteFraction, TokenAmount, FULL_ROUTE_FRACTION,
};
use near_min_api::types::{Balance, BlockHeight};
use near_sdk::{json_types::U128, PublicKeyHandle};
use pathfinder::{
    search::{self, Graph, Hop, MaxHops, PoolsDelta, QuoteAmount, Settings},
    BASIS_POINTS,
};
use swap_router_types::{
    hook::{
        HookQuote, HookSwap, HookSwapRequest, HookSwapRoute, IntearDexTrade, OracleRequest,
        HOOK_ANSWER_TIMEOUT,
    },
    last_look::{with_tear_fee_discount, LastLook, SwapEstimate},
    types::{CostPrices, Gas},
};
use tokio::sync::Semaphore;
use tracing::warn;

use super::{
    inteardex::{trade_after_oracle_fee, IntearDexSwap},
    pools::{HookPool, PoolsGraphOf, PoolsScope, RoutingPools},
};
use crate::{
    calls::{IntearDexOracleRequest, IntearDexSwapOption},
    stats::RouteOutcome,
    Amount, SwapRequest,
};

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 8,
    allow_unused_input: false,
    keep_shorter_routes: true,
};
const MAX_SEARCHES_RUNNING: usize = 64;

static SEARCHES_RUNNING: Semaphore = Semaphore::const_new(MAX_SEARCHES_RUNNING);

/// What routes through pools are found for
pub struct PoolRoutesRequest {
    pub request: SwapRequest,
    pub trade: IntearDexTrade,
    pub block_height: BlockHeight,
    /// Of the NEAR that hops cost and of the quoted token
    pub prices: CostPrices,
    /// None for no last look discount
    pub last_look: Option<LastLook>,
    pub tear_fee_discount: Option<FeeDiscountFraction>,
}

/// Swaps of routes through pools
#[derive(Default)]
pub struct PoolRoutes {
    /// Of routes on one DEX, which can be fallbacks of each other
    pub quotes: Vec<HookQuote>,
    /// Of routes on several DEXes one after another
    pub chains: Vec<IntearDexSwap>,
}

/// [`find`] on a blocking thread, and how it went
pub async fn find_in_background(
    pools: Arc<RoutingPools>,
    snapshot: Option<usize>,
    request: PoolRoutesRequest,
) -> (RouteOutcome, PoolRoutes) {
    let Ok(search_running) = SEARCHES_RUNNING.try_acquire() else {
        return (RouteOutcome::TimedOut, PoolRoutes::default());
    };
    let routes = tokio::task::spawn_blocking(move || {
        let _search_running = search_running;
        find(&pools, snapshot, &request)
    });
    match tokio::time::timeout(HOOK_ANSWER_TIMEOUT, routes).await {
        Ok(Ok(routes)) if routes.quotes.is_empty() && routes.chains.is_empty() => {
            (RouteOutcome::NotFound, routes)
        }
        Ok(Ok(routes)) => (RouteOutcome::Found, routes),
        // The panic is logged where it happened
        Ok(Err(_)) => (RouteOutcome::Panicked, PoolRoutes::default()),
        Err(_) => (RouteOutcome::TimedOut, PoolRoutes::default()),
    }
}

/// Routes through the pools of the hook of `snapshot`, an index in [`RoutingPools::snapshots`],
/// or of every hook: the best one through pools that ask no oracle, and the best one through the
/// pools of each oracle that takes its fee in the asset in
pub fn find(
    pools: &RoutingPools,
    snapshot: Option<usize>,
    request: &PoolRoutesRequest,
) -> PoolRoutes {
    let without_oracle = PoolsScope {
        snapshot,
        oracle_key: None,
    };
    let with_oracles = pools
        .oracle_fees_in(snapshot, &request.trade.asset_in)
        .into_iter()
        .filter_map(|(oracle_key, oracle_fee)| {
            let trade = trade_after_oracle_fee(&request.request, &request.trade, oracle_fee)?;
            let scope = PoolsScope {
                snapshot,
                oracle_key: Some(oracle_key.clone()),
            };
            Some((scope, trade, Some((oracle_key, oracle_fee))))
        });
    let mut routes = PoolRoutes::default();
    for (scope, trade, oracle) in [(without_oracle, request.trade.clone(), None)]
        .into_iter()
        .chain(with_oracles)
    {
        let asks_oracle = oracle.is_some();
        let Some(route) = found_route(pools, &scope, &trade, oracle, request) else {
            continue;
        };
        // Through pools that ask no oracle, it's found without one
        if asks_oracle && route.oracle_request.is_none() {
            continue;
        }
        match <[_; 1]>::try_from(route.swaps) {
            Ok([(_, _, swap)]) => routes.quotes.push(HookQuote {
                dex_id: swap.dex_id,
                message: swap.message,
                estimated_amount: U128(route.estimated_amount),
                worst_case_amount: U128(route.worst_case_amount),
                gas: swap.gas,
                discount: route.discount.map(U128),
                oracle_request: route
                    .oracle_request
                    .map(|(oracle_request, _)| oracle_request),
            }),
            Err(swaps) => routes.chains.push(IntearDexSwap::chain(
                swaps
                    .into_iter()
                    .map(|(asset_in, asset_out, swap)| {
                        let option = IntearDexSwapOption {
                            dex_id: swap.dex_id,
                            message: swap.message,
                            gas: swap.gas,
                        };
                        (asset_in, asset_out, option)
                    })
                    .collect(),
                route.estimated_amount,
                route.worst_case_amount,
                route.discount.map(U128),
                route
                    .oracle_request
                    .map(|(request, dex_id)| IntearDexOracleRequest { dex_id, request }),
            )),
        }
    }
    routes
}

/// A route through pools, as the swaps of its steps on their DEXes
struct FoundRoute {
    /// Of the asset in and out of each step
    swaps: Vec<(AssetId, AssetId, HookSwap)>,
    estimated_amount: Balance,
    worst_case_amount: Balance,
    /// What the TEAR holder's fee discount saves
    discount: Option<Balance>,
    /// And the DEX that reads the response
    oracle_request: Option<(OracleRequest, intear_dex_common::DexId)>,
}

/// The best route through the pools of `scope` for `trade`, which is after the fee of `oracle`
fn found_route(
    pools: &RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    oracle: Option<(PublicKeyHandle, u128)>,
    request: &PoolRoutesRequest,
) -> Option<FoundRoute> {
    let amount = match trade.amount {
        Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
        Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
    };
    let SearchedRoute {
        pools_graph, route, ..
    } = best_route(pools, scope, trade, amount, &request.prices)?;
    let graph = &pools_graph.graph;

    let build_swaps = |emulated: &Emulated, fee_discount: Option<FeeDiscountFraction>| {
        route.swaps(
            pools,
            graph,
            trade,
            emulated,
            fee_discount,
            request.block_height,
        )
    };
    let undiscounted = route.emulate(graph, amount, None)?;
    let swaps_gas = build_swaps(&undiscounted, None)?
        .iter()
        .map(|(_, _, swap)| swap.gas)
        .sum::<Gas>();
    let last_look_fee_discount = request.last_look.as_ref().and_then(|last_look| {
        last_look.fee_discount(|fee_discount| {
            Some(SwapEstimate {
                estimated_amount: route.emulate(graph, amount, fee_discount)?.amount,
                gas: swaps_gas,
                oracle_fee: oracle.as_ref().map(|(_, oracle_fee)| *oracle_fee),
            })
        })
    });
    let fee_discount = with_tear_fee_discount(last_look_fee_discount, request.tear_fee_discount);
    let emulated = route.emulate(graph, amount, fee_discount)?;
    // Only what the TEAR holder's discount adds is shown as the discount
    let discount = match request.tear_fee_discount {
        Some(_) => {
            let counted_from = route.emulate(graph, amount, last_look_fee_discount)?.amount;
            Some(emulated.amount.abs_diff(counted_from))
        }
        None => None,
    };
    let swaps = build_swaps(&emulated, fee_discount)?;
    let slippage_bp = pathfinder::slippage_bp(&trade.slippage).ok()?;
    let worst_case_amount = match amount {
        QuoteAmount::ExactIn(_) => {
            pathfinder::with_slippage(emulated.amount, BASIS_POINTS - slippage_bp)
        }
        QuoteAmount::ExactOut(_) => {
            pathfinder::with_slippage(emulated.amount, BASIS_POINTS + slippage_bp)
        }
    };
    let oracle_requests = swaps
        .iter()
        .filter_map(|(_, _, swap)| Some((swap.oracle_request.clone()?, swap.dex_id.clone())))
        .collect::<Vec<_>>();
    let oracle_request = match (oracle_requests.as_slice(), oracle) {
        ([], _) => None,
        ([(request, dex_id)], Some((oracle_key, fee))) => Some((
            OracleRequest {
                oracle_key,
                request: request.clone(),
                fee: U128(fee),
            },
            dex_id.clone(),
        )),
        // One oracle request starts the operations, for one DEX to read
        _ => return None,
    };
    Some(FoundRoute {
        swaps,
        estimated_amount: emulated.amount,
        worst_case_amount,
        discount,
        oracle_request,
    })
}

/// A route found in a graph of pools
struct SearchedRoute<'a> {
    pools_graph: PoolsGraphOf<'a>,
    route: StepsRoute,
    /// The output for exact in, the input for exact out
    amount: Balance,
    /// Whether it has every part of the split route that the search found
    complete: bool,
}

/// The best route through the pools of `scope`. When some parts of the split route found through
/// the pools of several hooks can't be swapped together with the others, the route through each
/// hook's pools alone can be better than the rest of the parts.
fn best_route<'a>(
    pools: &'a RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    amount: QuoteAmount,
    prices: &CostPrices,
) -> Option<SearchedRoute<'a>> {
    let searched = searched_route(pools, scope, trade, amount, prices);
    if scope.snapshot.is_some() || searched.as_ref().is_some_and(|searched| searched.complete) {
        return searched;
    }
    (0..pools.snapshots.len())
        .filter_map(|snapshot| {
            let scope = PoolsScope {
                snapshot: Some(snapshot),
                oracle_key: scope.oracle_key.clone(),
            };
            searched_route(pools, &scope, trade, amount, prices)
        })
        .chain(searched)
        .max_by(|searched, other| more_for(amount, searched.amount, other.amount))
}

fn searched_route<'a>(
    pools: &'a RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    amount: QuoteAmount,
    prices: &CostPrices,
) -> Option<SearchedRoute<'a>> {
    let pools_graph = pools.graph(scope)?;
    let graph = &pools_graph.graph;
    let token_in = graph.token_index(&pools.token_index(&trade.asset_in)?)?;
    let token_out = graph.token_index(&pools.token_index(&trade.asset_out)?)?;
    let hop_cost = pathfinder::gas_cost(
        pools_graph.min_hop_gas,
        &prices.near_price_raw,
        &prices.quoted_token_price_raw,
    )
    .inspect_err(|err| warn!("No cost of a hop: {err}"))
    .ok()?;
    let split_route = search::route(
        graph,
        token_in,
        token_out,
        amount,
        MaxHops::Four,
        SETTINGS,
        hop_cost,
    )
    .ok()?;
    let (route, emulated, complete) = StepsRoute::best_of(pools, graph, &split_route, amount)?;
    Some(SearchedRoute {
        pools_graph,
        route,
        amount: emulated,
        complete,
    })
}

/// A route through the pools of hooks, as steps on one hook's pools each, every step swapping
/// what the one before it gave
struct StepsRoute {
    steps: Vec<Step>,
    /// Of [`FULL_ROUTE_FRACTION`], that each route of the first step takes
    first_fractions: Vec<u32>,
}

/// Routes on the pools of one hook, between the same tokens
struct Step {
    /// Index in [`RoutingPools::snapshots`]
    snapshot: usize,
    /// Indices in [`RoutingPools::tokens`]
    token_in: u32,
    token_out: u32,
    /// Each through pools one after another. A route of a step is a part of the same route of
    /// the step before it.
    routes: Vec<Vec<Hop>>,
}

/// What a route swaps, and the fraction of each step that each of its routes takes
struct Emulated {
    amount: Balance,
    fractions: Vec<Vec<u32>>,
}

impl StepsRoute {
    /// Parts of a split route can only be swapped together when they go through the same hooks
    /// and tokens between them, so of the parts that do, the ones that swap best, with the
    /// amount split between them alone. A route of an exact-out swap is through one hook, since
    /// only exact-in swaps can swap what the one before them gave. With what it swaps, and whether
    /// it has every part.
    fn best_of(
        pools: &RoutingPools,
        graph: &Graph<HookPool>,
        split_route: &search::SplitRoute,
        amount: QuoteAmount,
    ) -> Option<(Self, Balance, bool)> {
        let mut routes = Vec::<(Vec<(usize, u32, u32)>, Self, Vec<u32>)>::new();
        for part in &split_route.parts {
            let steps = steps_of(pools, graph, &part.route.hops);
            let shape = steps
                .iter()
                .map(|step| (step.snapshot, step.token_in, step.token_out))
                .collect::<Vec<_>>();
            match routes
                .iter_mut()
                .find(|(other_shape, ..)| *other_shape == shape)
            {
                Some((_, route, weights)) => {
                    for (step, part_step) in route.steps.iter_mut().zip(steps) {
                        step.routes.extend(part_step.routes);
                    }
                    weights.push(part.weight_percentage);
                }
                None => routes.push((
                    shape,
                    Self {
                        steps,
                        first_fractions: Vec::new(),
                    },
                    vec![part.weight_percentage],
                )),
            }
        }
        let complete = routes.len() == 1;
        routes
            .into_iter()
            .filter(|(shape, ..)| shape.len() == 1 || matches!(amount, QuoteAmount::ExactIn(_)))
            .filter_map(|(_, mut route, weights)| {
                route.first_fractions =
                    fractions_of(&weights.iter().map(|&w| w.into()).collect::<Vec<_>>())?;
                let emulated = route.emulate(graph, amount, None)?;
                Some((route, emulated.amount))
            })
            .max_by(|(_, emulated), (_, other)| more_for(amount, *emulated, *other))
            .map(|(route, emulated)| (route, emulated, complete))
    }

    /// Swaps like the DEXes would: each step splits what it swaps between its routes by their
    /// fractions, and the routes swap one after another. The steps after the first, which can't
    /// be signed a fee discount for, swap without `fee_discount`.
    fn emulate(
        &self,
        graph: &Graph<HookPool>,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Emulated> {
        let mut pools_delta = PoolsDelta::default();
        let mut fractions = Vec::with_capacity(self.steps.len());
        let mut step_amount = amount.value();
        let mut route_amounts = Vec::new();
        for (index, step) in self.steps.iter().enumerate() {
            let step_fractions = match index {
                0 => self.first_fractions.clone(),
                _ => fractions_of(&route_amounts)?,
            };
            let split = RouteFraction::split(
                TokenAmount::new(step_amount),
                step_fractions
                    .iter()
                    .map(|&fraction| RouteFraction::new(fraction)),
            );
            let step_fee_discount = fee_discount.filter(|_| index == 0);
            route_amounts = step
                .routes
                .iter()
                .zip(split)
                .map(|(hops, route_amount)| {
                    let route_amount = match amount {
                        QuoteAmount::ExactIn(_) => QuoteAmount::ExactIn(route_amount.get()),
                        QuoteAmount::ExactOut(_) => QuoteAmount::ExactOut(route_amount.get()),
                    };
                    swap_hops(
                        graph,
                        hops,
                        route_amount,
                        step_fee_discount,
                        &mut pools_delta,
                    )
                })
                .collect::<Option<Vec<_>>>()?;
            step_amount = route_amounts
                .iter()
                .try_fold(0u128, |total, amount| total.checked_add(*amount))?;
            fractions.push(step_fractions);
        }
        Some(Emulated {
            amount: step_amount,
            fractions,
        })
    }

    /// The swaps of the steps, the first one of `trade`'s amount with `fee_discount`
    fn swaps(
        &self,
        pools: &RoutingPools,
        graph: &Graph<HookPool>,
        trade: &IntearDexTrade,
        emulated: &Emulated,
        fee_discount: Option<FeeDiscountFraction>,
        block_height: BlockHeight,
    ) -> Option<Vec<(AssetId, AssetId, HookSwap)>> {
        self.steps
            .iter()
            .zip(&emulated.fractions)
            .enumerate()
            .map(|(index, (step, fractions))| {
                let asset_in = pools.tokens[step.token_in as usize].clone();
                let asset_out = pools.tokens[step.token_out as usize].clone();
                let request = HookSwapRequest {
                    asset_in: asset_in.clone(),
                    asset_out: asset_out.clone(),
                    routes: step
                        .routes
                        .iter()
                        .zip(fractions)
                        .map(|(hops, &fraction)| HookSwapRoute {
                            fraction,
                            pools: hops
                                .iter()
                                .map(|hop| graph.pools()[hop.pool].pool)
                                .collect(),
                        })
                        .collect(),
                    amount: (index == 0).then_some(trade.amount),
                    fee_discount: fee_discount
                        .filter(|_| index == 0)
                        .map(FeeDiscountFraction::get),
                    trader_account_id: trade.trader_account_id.clone(),
                    block_height,
                };
                let snapshot = &pools.snapshots[step.snapshot];
                let swap = snapshot
                    .snapshot
                    .build_swap(&request)
                    .inspect_err(|err| {
                        warn!("{} failed to build a swap: {err}", snapshot.snapshot.name())
                    })
                    .ok()?;
                Some((asset_in, asset_out, swap))
            })
            .collect()
    }
}

/// Orders the output of exact-in swaps from the least, and the input of exact-out ones from the
/// most
fn more_for(amount: QuoteAmount, swapped: Balance, other: Balance) -> std::cmp::Ordering {
    match amount {
        QuoteAmount::ExactIn(_) => swapped.cmp(&other),
        QuoteAmount::ExactOut(_) => other.cmp(&swapped),
    }
}

/// The hops as steps on one hook's pools each
fn steps_of(pools: &RoutingPools, graph: &Graph<HookPool>, hops: &[Hop]) -> Vec<Step> {
    let mut steps = Vec::<Step>::new();
    for hop in hops {
        let snapshot = pools.origins[graph.pools()[hop.pool].index as usize].snapshot;
        let token_out = *graph.token(hop.token_out);
        match steps.last_mut() {
            Some(step) if step.snapshot == snapshot => {
                step.routes[0].push(*hop);
                step.token_out = token_out;
            }
            _ => steps.push(Step {
                snapshot,
                token_in: *graph.token(hop.token_in),
                token_out,
                routes: vec![vec![*hop]],
            }),
        }
    }
    steps
}

/// Through the hops one after another: the output for exact in, the input for exact out
fn swap_hops(
    graph: &Graph<HookPool>,
    hops: &[Hop],
    amount: QuoteAmount,
    fee_discount: Option<FeeDiscountFraction>,
    pools_delta: &mut PoolsDelta<HookPool>,
) -> Option<Balance> {
    let mut swap = |hop: &Hop, amount: QuoteAmount| {
        let mut pool = pools_delta
            .get(&hop.pool)
            .unwrap_or(&graph.pools()[hop.pool])
            .clone();
        let swapped = pool.swap_tokens(
            *graph.token(hop.token_in),
            *graph.token(hop.token_out),
            amount,
            fee_discount,
        )?;
        pools_delta.insert(hop.pool, pool);
        Some(swapped)
    };
    match amount {
        QuoteAmount::ExactIn(amount_in) => hops.iter().try_fold(amount_in, |amount, hop| {
            swap(hop, QuoteAmount::ExactIn(amount))
        }),
        QuoteAmount::ExactOut(amount_out) => {
            hops.iter().rev().try_fold(amount_out, |amount, hop| {
                swap(hop, QuoteAmount::ExactOut(amount))
            })
        }
    }
}

/// Fractions of [`FULL_ROUTE_FRACTION`] in proportion to `amounts`, which add up to it with the
/// rounding remainder in the first. None if one of them would be nothing.
fn fractions_of(amounts: &[Balance]) -> Option<Vec<u32>> {
    let full = u128::from(FULL_ROUTE_FRACTION.get());
    let total = amounts
        .iter()
        .try_fold(0u128, |total, amount| total.checked_add(*amount))?;
    if total == 0 {
        return None;
    }
    let mut fractions = amounts
        .iter()
        .map(|&amount| {
            let fraction = BigUint::from(amount) * full / BigUint::from(total);
            u32::try_from(fraction)
                .ok()
                .filter(|fraction| *fraction > 0)
        })
        .collect::<Option<Vec<_>>>()?;
    let rest = FULL_ROUTE_FRACTION.get() - fractions.iter().sum::<u32>();
    fractions[0] += rest;
    Some(fractions)
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_void, CStr, CString};

    use near_min_api::types::NearToken;
    use near_sdk::json_types::Base64VecU8;
    use swap_router_types::hook::{
        HookPool as HookPoolInfo, HookPools, IntearDexRouteCosts, OracleFees, PoolState,
    };

    use super::*;
    use crate::{
        hooks::{pools::PoolsSnapshot, HookPlugin},
        types::{Slippage, TokenId},
    };

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const ORACLE_FEE: u128 = 1_000;

    /// A DEX of pools that keep their whole 1% fee, which [`test_plugin`] is the hook of
    struct TestDex {
        dex_id: &'static str,
        pools: HookPools,
    }

    fn asset(id: &str) -> AssetId {
        match id {
            "near" => AssetId::Near,
            id => AssetId::Nep141(id.parse().unwrap()),
        }
    }

    fn pool(tokens: [&str; 2], reserves: [u128; 2], oracle_key: Option<&str>) -> HookPoolInfo {
        HookPoolInfo {
            tokens: tokens.map(asset),
            state: PoolState::of_balances(reserves),
            hop_gas: near_min_api::types::NearGas::from_tgas(2),
            oracle_key: oracle_key.map(|key| key.parse().unwrap()),
        }
    }

    /// The fee in millionths, less by a fee discount in millionths
    fn fee_millionths(fee_discount: u32) -> u128 {
        10_000 * u128::from(1_000_000 - fee_discount) / 1_000_000
    }

    /// The output for exact in, or the input for exact out, and the reserves after the swap
    fn constant_product(
        reserves: [u128; 2],
        amount: QuoteAmount,
        fee_discount: u32,
    ) -> Option<(u128, [u128; 2])> {
        let fee = fee_millionths(fee_discount);
        let [reserve_in, reserve_out] = reserves;
        match amount {
            QuoteAmount::ExactIn(amount_in) => {
                let after_fee = amount_in * (1_000_000 - fee) / 1_000_000;
                let amount_out = after_fee * reserve_out / (reserve_in + after_fee);
                (amount_out > 0).then_some((
                    amount_out,
                    [reserve_in + amount_in, reserve_out - amount_out],
                ))
            }
            QuoteAmount::ExactOut(amount_out) => {
                let after_fee =
                    (amount_out * reserve_in).div_ceil(reserve_out.checked_sub(amount_out)?);
                let amount_in = (after_fee * 1_000_000).div_ceil(1_000_000 - fee);
                Some((
                    amount_in,
                    [reserve_in + amount_in, reserve_out - amount_out],
                ))
            }
        }
    }

    fn output(answer: Result<impl serde::Serialize, String>) -> *mut c_char {
        CString::new(serde_json::to_string(&answer).unwrap())
            .unwrap()
            .into_raw()
    }

    fn test_dex(raw: *const c_void) -> &'static TestDex {
        unsafe { &*raw.cast::<TestDex>() }
    }

    unsafe extern "C" fn snapshot_pools(raw: *const c_void) -> *mut c_char {
        output(Ok(test_dex(raw).pools.clone()))
    }

    #[allow(improper_ctypes_definitions)]
    unsafe extern "C" fn swap(
        _raw: *const c_void,
        _pool: u32,
        state: *mut PoolState,
        token_in: u8,
        token_out: u8,
        amount: u128,
        exact_in: bool,
        fee_discount: u32,
        result: *mut u128,
    ) -> bool {
        let state = unsafe { &mut *state };
        let balances = state.balances::<2>();
        let reserves = [balances[token_in as usize], balances[token_out as usize]];
        let amount = match exact_in {
            true => QuoteAmount::ExactIn(amount),
            false => QuoteAmount::ExactOut(amount),
        };
        let Some((swapped, [reserve_in, reserve_out])) =
            constant_product(reserves, amount, fee_discount)
        else {
            return false;
        };
        let mut balances = balances;
        balances[token_in as usize] = reserve_in;
        balances[token_out as usize] = reserve_out;
        *state = PoolState::of_balances(balances);
        unsafe { *result = swapped };
        true
    }

    /// The message is the request, and pools of the oracle make it ask for `prices`
    unsafe extern "C" fn build_swap(raw: *const c_void, request: *const c_char) -> *mut c_char {
        let dex = test_dex(raw);
        let request_json = unsafe { CStr::from_ptr(request) }.to_str().unwrap();
        let request: HookSwapRequest = serde_json::from_str(request_json).unwrap();
        let asks_oracle = request
            .routes
            .iter()
            .flat_map(|route| &route.pools)
            .any(|&pool| dex.pools.pools[pool as usize].oracle_key.is_some());
        output(Ok(HookSwap {
            dex_id: dex.dex_id.parse().unwrap(),
            message: Base64VecU8(request_json.as_bytes().to_vec()),
            gas: Gas::from_ggas(5_000, 5_000),
            oracle_request: asks_oracle.then(|| Base64VecU8(b"prices".to_vec())),
        }))
    }

    unsafe extern "C" fn free(output: *mut c_char) {
        drop(unsafe { CString::from_raw(output) });
    }

    unsafe extern "C" fn release(_raw: *const c_void) {}

    unsafe extern "C" fn unused() -> *mut c_char {
        unreachable!()
    }

    unsafe extern "C" fn unused_with_input(_: *const c_char) -> *mut c_char {
        unreachable!()
    }

    unsafe extern "C" fn unused_on_indexed_state(_: *const u8, _: usize) -> *mut c_char {
        unreachable!()
    }

    unsafe extern "C" fn unused_snapshot() -> *const c_void {
        unreachable!()
    }

    fn snapshot(name: &str, dex_id: &'static str, pools: HookPools) -> PoolsSnapshot {
        let plugin = HookPlugin {
            name: name.into(),
            on_indexed_state: unused_on_indexed_state,
            on_prices: unused_with_input,
            run: unused_with_input,
            free,
            pools_snapshot: unused_snapshot,
            release_pools_snapshot: release,
            snapshot_pools,
            swap,
            build_swap,
        };
        let _ = unused;
        let dex: &'static TestDex = Box::leak(Box::new(TestDex { dex_id, pools }));
        PoolsSnapshot::of_plugin(plugin, (dex as *const TestDex).cast())
    }

    const USDC_NEAR: [u128; 2] = [1_000_000_000, 2_000_000_000];
    const NEAR_TEAR: [u128; 2] = [1_000_000_000, 3_000_000_000];

    /// USDC/NEAR on the first hook, NEAR/TEAR on the second, read the oracle's prices if
    /// `tear_pool_oracle`
    fn routing_pools(tear_pool_oracle: bool) -> RoutingPools {
        let oracle_key = tear_pool_oracle.then_some(ORACLE_KEY);
        RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![pool(["usdc", "near"], USDC_NEAR, None)],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![pool(["near", "tear"], NEAR_TEAR, oracle_key)],
                    oracle_fees: oracle_key
                        .map(|oracle_key| OracleFees {
                            oracle_key: oracle_key.parse().unwrap(),
                            fees: [(asset("usdc"), ORACLE_FEE.into())].into(),
                        })
                        .into_iter()
                        .collect(),
                },
            ),
        ])
    }

    fn request(token_in: &str, token_out: &str, amount: Amount) -> PoolRoutesRequest {
        let token = |id: &str| match asset(id) {
            AssetId::Near => TokenId::Near,
            AssetId::Nep141(account_id) => TokenId::Nep141(account_id),
            _ => unreachable!(),
        };
        PoolRoutesRequest {
            request: SwapRequest {
                token_in: token(token_in),
                token_out: token(token_out),
                amount,
                slippage: Slippage::Fixed {
                    slippage: "0.01".parse().unwrap(),
                },
                dexes: None,
                trader_account_id: Some("trader.near".parse().unwrap()),
                signing_public_key: None,
                referrer_id: None,
                fees: Default::default(),
                quote_improvement_receiver: None,
            },
            trade: IntearDexTrade {
                asset_in: asset(token_in),
                asset_out: asset(token_out),
                amount,
                slippage: "0.01".parse().unwrap(),
                trader_account_id: Some("trader.near".parse().unwrap()),
            },
            block_height: 1_000,
            prices: CostPrices {
                near_price_raw: "0.000000000000000000000001".parse().unwrap(),
                quoted_token_price_raw: 1.into(),
            },
            last_look: None,
            tear_fee_discount: None,
        }
    }

    /// The requests that the swaps of a chain were built of
    fn chain_requests(swap: &IntearDexSwap) -> Vec<(HookSwapRequest, intear_dex_common::DexId)> {
        swap.options()
            .map(|option| {
                (
                    serde_json::from_slice(&option.message.0).unwrap(),
                    option.dex_id.clone(),
                )
            })
            .collect()
    }

    fn out(reserves: [u128; 2], amount_in: u128, fee_discount: u32) -> u128 {
        constant_product(reserves, QuoteAmount::ExactIn(amount_in), fee_discount)
            .unwrap()
            .0
    }

    #[test]
    fn routes_through_the_pools_of_several_hooks_one_after_another() {
        let pools = routing_pools(false);
        let routes = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountIn(1_000_000)),
        );
        assert!(routes.quotes.is_empty());
        let [chain] = routes.chains.as_slice() else {
            panic!("Not one chain");
        };
        let tear_out = out(NEAR_TEAR, out(USDC_NEAR, 1_000_000, 0), 0);
        assert_eq!(chain.estimated_amount(), tear_out);
        assert_eq!(chain.worst_case_amount(), tear_out * 99 / 100);
        let [(first, first_dex), (second, second_dex)] = chain_requests(chain).try_into().unwrap();
        assert_eq!(first_dex, "slimedragon.near/xyk".parse().unwrap());
        assert_eq!(
            (first.asset_in, first.asset_out),
            (asset("usdc"), asset("near"))
        );
        assert_eq!(first.amount, Some(Amount::AmountIn(1_000_000)));
        assert_eq!(
            first.routes,
            vec![HookSwapRoute {
                fraction: FULL_ROUTE_FRACTION.get(),
                pools: vec![0],
            }]
        );
        assert_eq!(second_dex, "slimedragon.near/amp".parse().unwrap());
        assert_eq!(
            (second.asset_in, second.asset_out),
            (asset("near"), asset("tear"))
        );
        // Swaps what the first gave
        assert_eq!(second.amount, None);
        assert_eq!(first.fee_discount, None);
        assert_eq!(second.fee_discount, None);
    }

    #[test]
    fn routes_through_the_pools_of_one_hook_when_it_is_asked_alone() {
        let pools = routing_pools(false);
        let plach = pools
            .snapshots
            .iter()
            .position(|snapshot| snapshot.snapshot.name() == "Plach");
        let alone = |token_out| {
            find(
                &pools,
                plach,
                &request("usdc", token_out, Amount::AmountIn(1_000_000)),
            )
        };
        let to_tear = alone("tear");
        assert!(to_tear.quotes.is_empty() && to_tear.chains.is_empty());
        let to_near = alone("near");
        let [quote] = to_near.quotes.as_slice() else {
            panic!("Not one quote");
        };
        assert_eq!(quote.dex_id, "slimedragon.near/xyk".parse().unwrap());
        assert_eq!(quote.estimated_amount, U128(out(USDC_NEAR, 1_000_000, 0)));
        assert!(quote.oracle_request.is_none());
    }

    #[test]
    fn exact_out_only_goes_through_one_hook() {
        let pools = routing_pools(false);
        let to_tear = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountOut(1_000_000)),
        );
        assert!(to_tear.quotes.is_empty() && to_tear.chains.is_empty());
        let to_near = find(
            &pools,
            None,
            &request("usdc", "near", Amount::AmountOut(1_000_000)),
        );
        let [quote] = to_near.quotes.as_slice() else {
            panic!("Not one quote");
        };
        let usdc_in = constant_product(USDC_NEAR, QuoteAmount::ExactOut(1_000_000), 0)
            .unwrap()
            .0;
        assert_eq!(quote.estimated_amount, U128(usdc_in));
        assert_eq!(quote.worst_case_amount, U128(usdc_in * 101 / 100));
    }

    #[test]
    fn last_look_discounts_the_fees_of_the_first_step_to_match_the_best_other_quote() {
        let pools = routing_pools(false);
        let undiscounted = out(NEAR_TEAR, out(USDC_NEAR, 1_000_000, 0), 0);
        let routes = find(
            &pools,
            None,
            &PoolRoutesRequest {
                last_look: Some(LastLook {
                    best_other_amount: Amount::AmountOut(undiscounted + 50),
                    best_other_cost: NearToken::from_yoctonear(0),
                    referral_fees: Default::default(),
                    route_costs: IntearDexRouteCosts {
                        without_oracle_request: NearToken::from_yoctonear(0),
                        with_oracle_request: NearToken::from_yoctonear(0),
                        gas_price: NearToken::from_yoctonear(0),
                        prices: None,
                    },
                }),
                ..request("usdc", "tear", Amount::AmountIn(1_000_000))
            },
        );
        let chain = &routes.chains[0];
        let [(first, _), (second, _)] = chain_requests(chain).try_into().unwrap();
        // The smallest whole percent that matches, shared by the pools of the first step, as
        // the second's swap isn't of a known amount to sign it for
        let fee_discount = first.fee_discount.unwrap();
        assert_eq!(second.fee_discount, None);
        let with = |fee_discount| out(NEAR_TEAR, out(USDC_NEAR, 1_000_000, fee_discount), 0);
        assert!(with(fee_discount) >= undiscounted + 50);
        assert!(with(fee_discount - 10_000) < undiscounted + 50);
        assert_eq!(chain.estimated_amount(), with(fee_discount));
        // Only a TEAR holder's discount is shown
        assert_eq!(chain.discount(), None);
    }

    #[test]
    fn tear_holders_discount_is_shown() {
        let pools = routing_pools(false);
        let routes = find(
            &pools,
            None,
            &PoolRoutesRequest {
                tear_fee_discount: Some(FeeDiscountFraction::new(250_000)),
                ..request("usdc", "near", Amount::AmountIn(1_000_000))
            },
        );
        let quote = &routes.quotes[0];
        let discounted = out(USDC_NEAR, 1_000_000, 250_000);
        assert_eq!(quote.estimated_amount, U128(discounted));
        assert_eq!(
            quote.discount,
            Some(U128(discounted - out(USDC_NEAR, 1_000_000, 0)))
        );
        let request: HookSwapRequest = serde_json::from_slice(&quote.message.0).unwrap();
        assert_eq!(request.fee_discount, Some(250_000));
    }

    #[test]
    fn pools_of_an_oracle_are_routed_through_after_its_fee() {
        let pools = routing_pools(true);
        let routes = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountIn(1_000_000)),
        );
        // Only the route through the oracle's pool reaches TEAR
        let [chain] = routes.chains.as_slice() else {
            panic!("Not one chain");
        };
        let tear_out = out(NEAR_TEAR, out(USDC_NEAR, 1_000_000 - ORACLE_FEE, 0), 0);
        assert_eq!(chain.estimated_amount(), tear_out);
        let [(first, _), _] = chain_requests(chain).try_into().unwrap();
        assert_eq!(first.amount, Some(Amount::AmountIn(1_000_000 - ORACLE_FEE)));
        let oracle_request = chain.oracle_request().unwrap();
        assert_eq!(
            oracle_request.dex_id,
            "slimedragon.near/amp".parse().unwrap()
        );
        assert_eq!(
            oracle_request.request.oracle_key,
            ORACLE_KEY.parse().unwrap()
        );
        assert_eq!(oracle_request.request.request.0, b"prices".to_vec());
        assert_eq!(oracle_request.request.fee, U128(ORACLE_FEE));

        // NEAR pays no oracle fee, so the oracle's pools aren't routed through from it
        let from_near = find(
            &pools,
            None,
            &request("near", "tear", Amount::AmountIn(1_000_000)),
        );
        assert!(from_near.quotes.is_empty() && from_near.chains.is_empty());
    }

    #[test]
    fn a_route_is_at_least_as_good_as_its_parts_that_can_be_swapped_together() {
        const USDC_TEAR: [u128; 2] = [1_000_000_000, 6_000_000_000];
        let pools = RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![
                        pool(["usdc", "near"], USDC_NEAR, None),
                        pool(["usdc", "tear"], USDC_TEAR, None),
                    ],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![pool(["near", "tear"], NEAR_TEAR, None)],
                    oracle_fees: vec![],
                },
            ),
        ]);
        // Big enough for a split between the direct pool and the pools of both hooks, which
        // can't be swapped together
        let usdc_in = 300_000_000;
        let routes = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountIn(usdc_in)),
        );
        let estimated_amount = match (routes.quotes.as_slice(), routes.chains.as_slice()) {
            ([quote], []) => quote.estimated_amount.0,
            ([], [chain]) => chain.estimated_amount(),
            _ => panic!("Not one route"),
        };
        let direct = out(USDC_TEAR, usdc_in, 0);
        let through_near = out(NEAR_TEAR, out(USDC_NEAR, usdc_in, 0), 0);
        assert!(estimated_amount >= direct.max(through_near));
    }

    #[test]
    fn fractions_add_up_to_the_full_fraction_with_the_remainder_in_the_first() {
        assert_eq!(
            fractions_of(&[1, 1, 1]),
            Some(vec![333_334, 333_333, 333_333])
        );
        assert_eq!(
            fractions_of(&[u128::MAX / 2, u128::MAX / 2]),
            Some(vec![500_000, 500_000])
        );
        assert_eq!(fractions_of(&[0, 5]), None);
        assert_eq!(fractions_of(&[0]), None);
    }
}
