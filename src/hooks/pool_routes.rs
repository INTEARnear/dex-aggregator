use std::{collections::HashMap, sync::Arc};

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
    fee_discount::{FEE_DISCOUNT_GAS, FEE_DISCOUNT_VALIDITY_BLOCKS},
    hook::{
        HookQuote, HookSwap, HookSwapRequest, HookSwapRoute, IntearDexTrade, OracleFeeSwap,
        OracleFees, OracleRequest, QuoteFeeDiscount, HOOK_ANSWER_TIMEOUT,
    },
    last_look::{with_tear_fee_discount, LastLook, SwapEstimate},
    types::{CostPrices, Gas, PoolFee},
};
use tracing::warn;

use super::{
    inteardex::{trade_after_oracle_fee, ChainedSwap, IntearDexSwap},
    pools::{HookPool, PoolsGraphOf, PoolsScope, RoutingPools},
};
use crate::{
    calls::{swaps_burn, IntearDexOracleRequest, IntearDexSwapOption},
    Amount, SwapRequest,
};

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 8,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

/// What routes through pools are found for
pub struct PoolRoutesRequest {
    pub request: SwapRequest,
    pub trade: IntearDexTrade,
    pub block_height: BlockHeight,
    /// Of the NEAR that hops cost and of the quoted token
    pub prices: CostPrices,
    /// The same with the trade's asset in, which swaps that buy an oracle's fee are quoted in.
    /// None for no such swaps.
    pub asset_in_prices: Option<CostPrices>,
    /// None for no last look discount
    pub last_look: Option<LastLook>,
    pub tear_fee_discount: Option<FeeDiscountFraction>,
}

/// Swaps of routes through pools
#[derive(Default)]
pub struct PoolRoutes {
    /// Of routes on one DEX, which can be fallbacks of each other
    pub quotes: Vec<HookQuote>,
    /// Of routes on several DEXes one after another or split between them
    pub chains: Vec<IntearDexSwap>,
}

/// [`find`] on a blocking thread. None when it takes too long or it panics.
pub async fn find_in_background(
    pools: Arc<RoutingPools>,
    snapshot: Option<usize>,
    request: PoolRoutesRequest,
) -> PoolRoutes {
    let trade = request.trade.clone();
    let routes = tokio::task::spawn_blocking(move || find(&pools, snapshot, &request));
    match tokio::time::timeout(HOOK_ANSWER_TIMEOUT, routes).await {
        Ok(Ok(routes)) => routes,
        // The panic is logged where it happened
        Ok(Err(_)) => PoolRoutes::default(),
        Err(_) => {
            warn!(
                "Routes through pools of hooks took over {HOOK_ANSWER_TIMEOUT:?} to find, leaving \
                 them out of {trade:?}"
            );
            PoolRoutes::default()
        }
    }
}

/// Routes through the pools of the hook of `snapshot`, an index in [`RoutingPools::snapshots`],
/// or of every hook: the best ones through pools that ask no oracle, and the best ones through the
/// pools of each oracle, which the route pays in the asset in or in what a swap of it buys
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
        .oracle_fees(snapshot)
        .into_iter()
        .filter_map(|oracle_fees| {
            let payment = oracle_fee_payment(pools, oracle_fees, request)?;
            let trade = trade_after_oracle_fee(&request.request, &request.trade, payment.cost)?;
            let oracle_key = oracle_fees.oracle_key.clone();
            let scope = PoolsScope {
                snapshot,
                oracle_key: Some(oracle_key.clone()),
            };
            Some((scope, trade, Some((oracle_key, payment))))
        });
    let mut routes = PoolRoutes::default();
    for (scope, trade, oracle) in [(without_oracle, request.trade.clone(), None)]
        .into_iter()
        .chain(with_oracles)
    {
        let asks_oracle = oracle.is_some();
        for route in found_routes(pools, &scope, &trade, oracle, request) {
            // Through pools that ask no oracle, it's found without one
            if asks_oracle && route.oracle_request.is_none() {
                continue;
            }
            let oracle_request = route
                .oracle_request
                .map(|(request, dex_id)| IntearDexOracleRequest { dex_id, request });
            match <[_; 1]>::try_from(route.branches) {
                Ok([branch]) => match <[_; 1]>::try_from(branch.swaps) {
                    Ok([(_, _, swap)]) => routes.quotes.push(HookQuote {
                        dex_id: swap.dex_id,
                        message: swap.message,
                        estimated_amount: U128(route.estimated_amount),
                        worst_case_amount: U128(route.worst_case_amount),
                        gas: swap.gas,
                        discount: route.discount.map(U128),
                        oracle_request: oracle_request.map(|oracle_request| oracle_request.request),
                        fee_discount: route.fee_discount,
                        pool_fee: route.pool_fee,
                    }),
                    Err(swaps) => routes.chains.push(IntearDexSwap::chain(
                        swap_options(swaps),
                        route.estimated_amount,
                        route.worst_case_amount,
                        route.discount.map(U128),
                        oracle_request,
                        route.fee_discount,
                        route.pool_fee,
                    )),
                },
                Err(branches) => routes.chains.push(IntearDexSwap::split(
                    branches
                        .into_iter()
                        .map(|branch| {
                            (
                                branch.fraction,
                                branch.estimated_amount,
                                swap_options(branch.swaps),
                            )
                        })
                        .collect(),
                    route.estimated_amount,
                    route.worst_case_amount,
                    route.discount.map(U128),
                    oracle_request,
                    route.fee_discount,
                    route.pool_fee,
                )),
            }
        }
    }
    routes
}

fn swap_options(swaps: Vec<(AssetId, AssetId, HookSwap)>) -> Vec<ChainedSwap> {
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
        .collect()
}

/// How a route pays an oracle: with `cost` of the trade's asset in, the oracle's fee or the most
/// that `swap` swaps to buy it
#[derive(Clone)]
struct OracleFeePayment {
    cost: Balance,
    swap: Option<OracleFeeSwap>,
}

/// How a route pays the oracle that takes `oracle_fees`: in the trade's asset in, or with the swap
/// of it that buys one of its fees for the least, with the gas of the swap. A fee in NEAR is
/// bought when the oracle takes one, since the engine refunds a bought fee when the oracle doesn't
/// respond, and a refund of a NEP-141 to a trader without its storage is lost. None if it can't.
fn oracle_fee_payment(
    pools: &RoutingPools,
    oracle_fees: &OracleFees,
    request: &PoolRoutesRequest,
) -> Option<OracleFeePayment> {
    if let Some(fee) = oracle_fees.fees.get(&request.trade.asset_in) {
        return Some(OracleFeePayment {
            cost: fee.0,
            swap: None,
        });
    }
    let prices = request.asset_in_prices.as_ref()?;
    let takes_near = oracle_fees.fees.contains_key(&AssetId::Near);
    oracle_fees
        .fees
        .iter()
        .filter(|(fee_asset, _)| !takes_near || **fee_asset == AssetId::Near)
        .filter_map(|(fee_asset, fee)| {
            let fee_request = PoolRoutesRequest {
                request: request.request.clone(),
                trade: IntearDexTrade {
                    asset_out: fee_asset.clone(),
                    amount: Amount::AmountOut(fee.0),
                    ..request.trade.clone()
                },
                block_height: request.block_height,
                prices: prices.clone(),
                asset_in_prices: None,
                last_look: None,
                tear_fee_discount: None,
            };
            let all_pools_without_oracles = PoolsScope {
                snapshot: None,
                oracle_key: None,
            };
            let route = found_routes(
                pools,
                &all_pools_without_oracles,
                &fee_request.trade,
                None,
                &fee_request,
            )
            .into_iter()
            .next()?;
            // Exact-out routes are through one hook
            let [branch] = <[_; 1]>::try_from(route.branches).ok()?;
            let [(_, _, swap)] = <[_; 1]>::try_from(branch.swaps).ok()?;
            let gas_cost = pathfinder::gas_cost(
                swap.gas.estimated_burn,
                &prices.near_price_raw,
                &prices.quoted_token_price_raw,
            )
            .ok()?;
            let payment = OracleFeePayment {
                cost: route.worst_case_amount,
                swap: Some(OracleFeeSwap {
                    dex_id: swap.dex_id,
                    message: swap.message,
                    gas: swap.gas,
                    fee: (fee_asset.clone(), *fee),
                }),
            };
            Some((route.worst_case_amount + gas_cost, payment))
        })
        .min_by_key(|(cost_with_gas, _)| *cost_with_gas)
        .map(|(_, payment)| payment)
}

/// A route through pools, as the swaps of its branches on their DEXes
struct FoundRoute {
    branches: Vec<FoundBranch>,
    estimated_amount: Balance,
    worst_case_amount: Balance,
    /// What the TEAR staker's fee discount saves
    discount: Option<Balance>,
    /// And the DEX that reads the response
    oracle_request: Option<(OracleRequest, intear_dex_common::DexId)>,
    /// Of every swap of the route, which `estimated_amount` is with
    fee_discount: Option<QuoteFeeDiscount>,
    /// With `fee_discount`
    pool_fee: PoolFee,
}

/// Swaps of the share of the amount that the route splits to the branch
struct FoundBranch {
    /// Of [`FULL_ROUTE_FRACTION`]
    fraction: u32,
    /// Of the asset in and out of each step
    swaps: Vec<(AssetId, AssetId, HookSwap)>,
    estimated_amount: Balance,
}

/// The best routes through the pools of `scope` for `trade`, which is after paying `oracle`
fn found_routes(
    pools: &RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    oracle: Option<(PublicKeyHandle, OracleFeePayment)>,
    request: &PoolRoutesRequest,
) -> Vec<FoundRoute> {
    let amount = match trade.amount {
        Amount::AmountIn(amount_in) => QuoteAmount::ExactIn(amount_in),
        Amount::AmountOut(amount_out) => QuoteAmount::ExactOut(amount_out),
    };
    best_routes(pools, scope, trade, amount, &request.prices)
        .into_iter()
        .filter_map(|searched| found_route(pools, searched, amount, trade, oracle.clone(), request))
        .collect()
}

fn found_route(
    pools: &RoutingPools,
    SearchedRoute {
        pools_graph, route, ..
    }: SearchedRoute,
    amount: QuoteAmount,
    trade: &IntearDexTrade,
    oracle: Option<(PublicKeyHandle, OracleFeePayment)>,
    request: &PoolRoutesRequest,
) -> Option<FoundRoute> {
    let graph = &pools_graph.graph;

    let branch_swaps = route.swaps(pools, graph)?;
    let oracle_fee_swap_gas = oracle
        .as_ref()
        .and_then(|(_, payment)| payment.swap.as_ref())
        .map(|swap| swap.gas);
    let swaps = || branch_swaps.iter().flatten().map(|(_, _, swap)| swap);
    // As the planned gas of the route has it, which its simulation is measured against: a swap on
    // a DEX that the call already swapped on burns less
    let swaps_gas = Gas {
        estimated_burn: swaps_burn(swaps().map(|swap| (&swap.dex_id, swap.gas.estimated_burn))),
        needs_attached: swaps().map(|swap| swap.gas).sum::<Gas>().needs_attached,
    };
    let last_look_fee_discount = request.last_look.as_ref().and_then(|last_look| {
        last_look.fee_discount(|fee_discount| {
            let estimated_amount = total_of(&route.emulate(graph, amount, fee_discount)?)?;
            // The engine checks the discount once for all the swaps
            let fee_discount_gas = fee_discount.map(|_| FEE_DISCOUNT_GAS);
            Some(SwapEstimate {
                estimated_amount,
                gas: [swaps_gas]
                    .into_iter()
                    .chain(oracle_fee_swap_gas)
                    .chain(fee_discount_gas)
                    .sum(),
                oracle_fee: oracle.as_ref().map(|(_, payment)| payment.cost),
            })
        })
    });
    let fee_discount = with_tear_fee_discount(last_look_fee_discount, request.tear_fee_discount);
    let emulated = route.emulate(graph, amount, fee_discount)?;
    let hop_fee =
        |hop: &Hop| pools.origins[graph.pools()[hop.pool].index as usize].pool_fee(fee_discount);
    let step_fee = |step: &Step| {
        PoolFee::split(step.routes.iter().map(|step_route| {
            (
                step_route.fraction,
                PoolFee::chained(step_route.hops.iter().map(hop_fee)),
            )
        }))
    };
    let pool_fee = PoolFee::split(route.branches.iter().map(|branch| {
        (
            branch.fraction,
            PoolFee::chained(branch.route.steps.iter().map(step_fee)),
        )
    }));
    let estimated_amount = total_of(&emulated)?;
    // Only what the TEAR staker's discount adds is shown as the discount
    let discount = match request.tear_fee_discount {
        Some(_) => {
            let counted_from = total_of(&route.emulate(graph, amount, last_look_fee_discount)?)?;
            Some(estimated_amount.abs_diff(counted_from))
        }
        None => None,
    };
    let slippage_bp = pathfinder::slippage_bp(&trade.slippage).ok()?;
    let worst_case_amount = match amount {
        QuoteAmount::ExactIn(_) => {
            pathfinder::with_slippage(estimated_amount, BASIS_POINTS - slippage_bp)
        }
        QuoteAmount::ExactOut(_) => {
            pathfinder::with_slippage(estimated_amount, BASIS_POINTS + slippage_bp)
        }
    };
    let oracle_requests = branch_swaps
        .iter()
        .flatten()
        .filter_map(|(_, _, swap)| Some((swap.oracle_request.clone()?, swap.dex_id.clone())))
        .collect::<Vec<_>>();
    let oracle_request = match (oracle_requests.as_slice(), oracle) {
        ([], _) => None,
        ([(request, dex_id)], Some((oracle_key, payment))) => Some((
            OracleRequest {
                oracle_key,
                request: request.clone(),
                fee: U128(payment.cost),
                fee_swap: payment.swap,
            },
            dex_id.clone(),
        )),
        // One oracle request starts the operations, for one DEX to read
        _ => return None,
    };
    let branches = route
        .branches
        .iter()
        .zip(branch_swaps)
        .zip(emulated)
        .map(|((branch, swaps), estimated_amount)| FoundBranch {
            fraction: branch.fraction,
            swaps,
            estimated_amount,
        })
        .collect();
    Some(FoundRoute {
        branches,
        estimated_amount,
        worst_case_amount,
        discount,
        oracle_request,
        fee_discount: fee_discount.map(|fee_discount| QuoteFeeDiscount {
            fee_discount,
            expiration_block_height: request.block_height + FEE_DISCOUNT_VALIDITY_BLOCKS,
        }),
        pool_fee,
    })
}

/// A route found in a graph of pools
struct SearchedRoute<'a> {
    pools_graph: PoolsGraphOf<'a>,
    route: BranchedRoute,
    /// The output for exact in, the input for exact out
    amount: Balance,
    /// Whether it has every part of the split route that the search found
    complete: bool,
}

/// The best route through the pools of `scope` that swaps as one, and the split route found
/// through the pools of several hooks with its parts split between them. When some parts of the
/// split route can't be swapped together with the others, the route through each hook's pools
/// alone can be better than the rest of the parts.
fn best_routes<'a>(
    pools: &'a RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    amount: QuoteAmount,
    prices: &CostPrices,
) -> Vec<SearchedRoute<'a>> {
    let (searched, split) = match searched_route(pools, scope, trade, amount, prices) {
        Some((searched, split)) => (Some(searched), split),
        None => (None, None),
    };
    let alone = if scope.snapshot.is_some()
        || searched.as_ref().is_some_and(|searched| searched.complete)
    {
        searched
    } else {
        (0..pools.snapshots.len())
            .filter_map(|snapshot| {
                let scope = PoolsScope {
                    snapshot: Some(snapshot),
                    oracle_key: scope.oracle_key.clone(),
                };
                searched_route(pools, &scope, trade, amount, prices).map(|(searched, _)| searched)
            })
            .chain(searched)
            .max_by(|searched, other| more_for(amount, searched.amount, other.amount))
    };
    alone.into_iter().chain(split).collect()
}

/// The best of the routes that swap as one, and the parts of an exact-in split route split
/// between their hooks when they can't be swapped as one
fn searched_route<'a>(
    pools: &'a RoutingPools,
    scope: &PoolsScope,
    trade: &IntearDexTrade,
    amount: QuoteAmount,
    prices: &CostPrices,
) -> Option<(SearchedRoute<'a>, Option<SearchedRoute<'a>>)> {
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
    let parts_by_shape = parts_by_shape(pools, graph, &split_route);
    let complete = parts_by_shape.len() == 1;
    let split = match amount {
        QuoteAmount::ExactIn(amount_in) if !complete => split_between_hooks(
            pools,
            scope,
            graph,
            parts_by_shape.clone(),
            amount_in,
            hop_cost,
            prices,
        )
        .and_then(|(route, emulated)| {
            Some(SearchedRoute {
                pools_graph: pools.graph(scope)?,
                route,
                amount: emulated,
                complete: true,
            })
        }),
        _ => None,
    };
    let (route, emulated) =
        StepsRoute::best_of(pools, scope, graph, parts_by_shape, amount, hop_cost)?;
    let searched = SearchedRoute {
        pools_graph,
        route: BranchedRoute::whole(route),
        amount: emulated,
        complete,
    };
    Some((searched, split))
}

/// The steps of a part of a split route, and its weight in percent
type Part = (Vec<Step>, u32);

/// Parts of a split route can only be swapped together when they go through the same hooks and
/// tokens between them, so the parts of each of those shapes
fn parts_by_shape(
    pools: &RoutingPools,
    graph: &Graph<HookPool>,
    split_route: &search::SplitRoute,
) -> Vec<Vec<Part>> {
    let mut parts_by_shape = Vec::<(Vec<(usize, u32, u32)>, Vec<Part>)>::new();
    for part in &split_route.parts {
        let steps = steps_of(pools, graph, &part.route.hops);
        let shape = steps
            .iter()
            .map(|step| (step.snapshot, step.token_in, step.token_out))
            .collect::<Vec<_>>();
        let part = (steps, part.weight_percentage);
        match parts_by_shape
            .iter_mut()
            .find(|(other_shape, _)| *other_shape == shape)
        {
            Some((_, parts)) => parts.push(part),
            None => parts_by_shape.push((shape, vec![part])),
        }
    }
    parts_by_shape.into_iter().map(|(_, parts)| parts).collect()
}

/// The parts of an exact-in split route that can't be swapped as one, split between their hooks,
/// with what they give. The search weighs the hops of each part, but each branch is a swap of its own, so the
/// lightest branch is left out while the others give more without it, net of what their swaps
/// burn. None when one branch is left.
fn split_between_hooks(
    pools: &RoutingPools,
    scope: &PoolsScope,
    graph: &Graph<HookPool>,
    mut parts_by_shape: Vec<Vec<Part>>,
    amount_in: Balance,
    hop_cost: Balance,
    prices: &CostPrices,
) -> Option<(BranchedRoute, Balance)> {
    let net_of = |parts_by_shape: &[Vec<Part>]| -> Option<(BranchedRoute, Balance, Balance)> {
        let route = BranchedRoute::split(pools, scope, graph, parts_by_shape, amount_in, hop_cost)?;
        let emulated = total_of(&route.emulate(graph, QuoteAmount::ExactIn(amount_in), None)?)?;
        let swaps = route.swaps(pools, graph)?;
        let burn = swaps_burn(
            swaps
                .iter()
                .flatten()
                .map(|(_, _, swap)| (&swap.dex_id, swap.gas.estimated_burn)),
        );
        let gas_cost =
            pathfinder::gas_cost(burn, &prices.near_price_raw, &prices.quoted_token_price_raw)
                .ok()?;
        Some((route, emulated, emulated.saturating_sub(gas_cost)))
    };
    let (mut route, mut emulated, mut net) = net_of(&parts_by_shape)?;
    while parts_by_shape.len() > 1 {
        let lightest = parts_by_shape
            .iter()
            .enumerate()
            .min_by_key(|(_, parts)| weight_of(parts))
            .map(|(index, _)| index)?;
        let mut without = parts_by_shape.clone();
        without.remove(lightest);
        match net_of(&without) {
            Some((route_without, emulated_without, net_without)) if net_without >= net => {
                (parts_by_shape, route, emulated, net) =
                    (without, route_without, emulated_without, net_without);
            }
            _ => break,
        }
    }
    (parts_by_shape.len() > 1).then_some((route, emulated))
}

/// Of the parts, in percent
fn weight_of(parts: &[Part]) -> Balance {
    parts.iter().map(|(_, weight)| Balance::from(*weight)).sum()
}

/// Routes that split the amount, swapped one after another, each on its fraction of it
struct BranchedRoute {
    branches: Vec<Branch>,
}

struct Branch {
    route: StepsRoute,
    /// Of [`FULL_ROUTE_FRACTION`]
    fraction: u32,
}

impl BranchedRoute {
    /// Of one route, which swaps all of the amount
    fn whole(route: StepsRoute) -> Self {
        Self {
            branches: vec![Branch {
                route,
                fraction: FULL_ROUTE_FRACTION.get(),
            }],
        }
    }

    /// The parts of each shape as a branch, with the share of `amount_in` that their weights have,
    /// and the later steps of each found for what it swaps
    fn split(
        pools: &RoutingPools,
        scope: &PoolsScope,
        graph: &Graph<HookPool>,
        parts_by_shape: &[Vec<Part>],
        amount_in: Balance,
        hop_cost: Balance,
    ) -> Option<Self> {
        let fractions = fractions_of(
            &parts_by_shape
                .iter()
                .map(|parts| weight_of(parts))
                .collect::<Vec<_>>(),
        )?;
        let split = RouteFraction::split(
            TokenAmount::new(amount_in),
            fractions
                .iter()
                .map(|&fraction| RouteFraction::new(fraction)),
        );
        let branches = parts_by_shape
            .iter()
            .zip(fractions)
            .zip(split)
            .map(|((parts, fraction), branch_amount_in)| {
                let (route, _) = StepsRoute::of_parts(
                    pools,
                    scope,
                    graph,
                    parts.clone(),
                    QuoteAmount::ExactIn(branch_amount_in.get()),
                    hop_cost,
                )?;
                Some(Branch { route, fraction })
            })
            .collect::<Option<_>>()?;
        Some(Self { branches })
    }

    /// What each branch swaps, after the ones before it
    fn emulate(
        &self,
        graph: &Graph<HookPool>,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<Vec<Balance>> {
        let mut pools_delta = PoolsDelta::default();
        let split = RouteFraction::split(
            TokenAmount::new(amount.value()),
            self.branches
                .iter()
                .map(|branch| RouteFraction::new(branch.fraction)),
        );
        self.branches
            .iter()
            .zip(split)
            .map(|(branch, branch_amount)| {
                branch.route.emulate(
                    graph,
                    with_value(amount, branch_amount.get()),
                    fee_discount,
                    &mut pools_delta,
                )
            })
            .collect()
    }

    /// The swaps of each branch
    fn swaps(
        &self,
        pools: &RoutingPools,
        graph: &Graph<HookPool>,
    ) -> Option<Vec<Vec<(AssetId, AssetId, HookSwap)>>> {
        self.branches
            .iter()
            .map(|branch| branch.route.swaps(pools, graph))
            .collect()
    }
}

fn total_of(amounts: &[Balance]) -> Option<Balance> {
    amounts
        .iter()
        .try_fold(0u128, |total, amount| total.checked_add(*amount))
}

/// A route through the pools of hooks, as steps on one hook's pools each, every step swapping
/// what the one before it gave
struct StepsRoute {
    steps: Vec<Step>,
}

/// Routes on the pools of one hook, between the same tokens
#[derive(Clone)]
struct Step {
    /// Index in [`RoutingPools::snapshots`]
    snapshot: usize,
    /// Indices in [`RoutingPools::tokens`]
    token_in: u32,
    token_out: u32,
    routes: Vec<StepRoute>,
}

/// Pools one after another, which swap a fraction of what the step swaps
#[derive(Clone)]
struct StepRoute {
    hops: Vec<Hop>,
    /// Of [`FULL_ROUTE_FRACTION`]
    fraction: u32,
}

impl StepsRoute {
    /// Of the parts of each shape as a route, the one that swaps best. A route of an exact-out swap
    /// is through one hook, since only exact-in swaps can swap what the one before them gave. With
    /// what it swaps.
    fn best_of(
        pools: &RoutingPools,
        scope: &PoolsScope,
        graph: &Graph<HookPool>,
        parts_by_shape: Vec<Vec<Part>>,
        amount: QuoteAmount,
        hop_cost: Balance,
    ) -> Option<(Self, Balance)> {
        parts_by_shape
            .into_iter()
            .filter(|parts| parts[0].0.len() == 1 || matches!(amount, QuoteAmount::ExactIn(_)))
            .filter_map(|parts| Self::of_parts(pools, scope, graph, parts, amount, hop_cost))
            .max_by(|(_, emulated), (_, other)| more_for(amount, *emulated, *other))
    }

    /// Parts through the same hooks and tokens between them, as one route, with what it swaps.
    /// The first step splits the amount between the parts' routes by their weights. The search
    /// weighs parts as if each swapped through all of its steps alone, so the weights don't tell
    /// how to split what a step that parts share gives, and each later step is split as found
    /// for what the step before it gives.
    fn of_parts(
        pools: &RoutingPools,
        scope: &PoolsScope,
        graph: &Graph<HookPool>,
        parts: Vec<Part>,
        amount: QuoteAmount,
        hop_cost: Balance,
    ) -> Option<(Self, Balance)> {
        let mut first_step_routes = Vec::<(Vec<Hop>, Balance)>::new();
        for (steps, weight) in &parts {
            let [part_route] = steps[0].routes.as_slice() else {
                unreachable!("A part's step is one route");
            };
            let same_pools = |(hops, _): &&mut (Vec<Hop>, Balance)| {
                hops.len() == part_route.hops.len()
                    && hops.iter().zip(&part_route.hops).all(|(hop, part_hop)| {
                        (hop.pool, hop.token_in, hop.token_out)
                            == (part_hop.pool, part_hop.token_in, part_hop.token_out)
                    })
            };
            match first_step_routes.iter_mut().find(same_pools) {
                Some((_, route_weight)) => *route_weight += Balance::from(*weight),
                None => first_step_routes.push((part_route.hops.clone(), Balance::from(*weight))),
            }
        }
        let fractions = fractions_of(
            &first_step_routes
                .iter()
                .map(|(_, weight)| *weight)
                .collect::<Vec<_>>(),
        )?;
        let (shape_steps, _) = &parts[0];
        let first_step = Step {
            routes: first_step_routes
                .into_iter()
                .zip(fractions)
                .map(|((hops, _), fraction)| StepRoute { hops, fraction })
                .collect(),
            ..shape_steps[0]
        };
        let mut pools_delta = PoolsDelta::default();
        let mut step_amount = first_step.swap(graph, amount, None, &mut pools_delta)?;
        let mut steps = vec![first_step];
        for (index, shape_step) in shape_steps.iter().enumerate().skip(1) {
            let asks_oracle = parts
                .iter()
                .flat_map(|(steps, _)| &steps[index].routes)
                .flat_map(|route| &route.hops)
                .any(|hop| {
                    pools.origins[graph.pools()[hop.pool].index as usize]
                        .oracle_key
                        .is_some()
                });
            // The cost of hops is in the trade's output, which only the last step gives
            let step_hop_cost = if index == shape_steps.len() - 1 {
                hop_cost
            } else {
                0
            };
            let step = searched_step(
                pools,
                scope,
                graph,
                shape_step,
                step_amount,
                asks_oracle,
                step_hop_cost,
            )?;
            step_amount = step.swap(
                graph,
                QuoteAmount::ExactIn(step_amount),
                None,
                &mut pools_delta,
            )?;
            steps.push(step);
        }
        Some((Self { steps }, step_amount))
    }

    /// Swaps like the DEXes would, every step with `fee_discount`, which the engine takes for all
    /// swaps of the operations
    fn emulate(
        &self,
        graph: &Graph<HookPool>,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
        pools_delta: &mut PoolsDelta<HookPool>,
    ) -> Option<Balance> {
        self.steps
            .iter()
            .try_fold(amount.value(), |step_amount, step| {
                step.swap(
                    graph,
                    with_value(amount, step_amount),
                    fee_discount,
                    pools_delta,
                )
            })
    }

    /// The swaps of the steps
    fn swaps(
        &self,
        pools: &RoutingPools,
        graph: &Graph<HookPool>,
    ) -> Option<Vec<(AssetId, AssetId, HookSwap)>> {
        self.steps
            .iter()
            .map(|step| {
                let asset_in = pools.tokens[step.token_in as usize].clone();
                let asset_out = pools.tokens[step.token_out as usize].clone();
                let request = HookSwapRequest {
                    asset_in: asset_in.clone(),
                    asset_out: asset_out.clone(),
                    routes: step
                        .routes
                        .iter()
                        .map(|route| HookSwapRoute {
                            fraction: route.fraction,
                            pools: route
                                .hops
                                .iter()
                                .map(|hop| graph.pools()[hop.pool].pool)
                                .collect(),
                        })
                        .collect(),
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

impl Step {
    /// Splits `amount` between the routes by their fractions, and the routes swap one after
    /// another: the output for exact in, the input for exact out
    fn swap(
        &self,
        graph: &Graph<HookPool>,
        amount: QuoteAmount,
        fee_discount: Option<FeeDiscountFraction>,
        pools_delta: &mut PoolsDelta<HookPool>,
    ) -> Option<Balance> {
        let split = RouteFraction::split(
            TokenAmount::new(amount.value()),
            self.routes
                .iter()
                .map(|route| RouteFraction::new(route.fraction)),
        );
        self.routes
            .iter()
            .zip(split)
            .try_fold(0u128, |total, (route, route_amount)| {
                let swapped = swap_hops(
                    graph,
                    &route.hops,
                    with_value(amount, route_amount.get()),
                    fee_discount,
                    pools_delta,
                )?;
                total.checked_add(swapped)
            })
    }
}

/// The routes through the pools of `step`'s hook that swap `amount_in` best, found in the pools
/// as they are, with their hops in `graph`. Through the pools of the oracle of `scope` only if
/// the step `asks_oracle` already, since one oracle request starts the operations. `hop_cost` is
/// in the step's output.
fn searched_step(
    pools: &RoutingPools,
    scope: &PoolsScope,
    graph: &Graph<HookPool>,
    step: &Step,
    amount_in: Balance,
    asks_oracle: bool,
    hop_cost: Balance,
) -> Option<Step> {
    let step_scope = PoolsScope {
        snapshot: Some(step.snapshot),
        oracle_key: scope.oracle_key.clone().filter(|_| asks_oracle),
    };
    let step_graph = &pools.graph(&step_scope)?.graph;
    let split_route = search::route(
        step_graph,
        step_graph.token_index(&step.token_in)?,
        step_graph.token_index(&step.token_out)?,
        QuoteAmount::ExactIn(amount_in),
        MaxHops::Four,
        SETTINGS,
        hop_cost,
    )
    .ok()?;
    let graph_pools_by_index = graph
        .pools()
        .iter()
        .enumerate()
        .map(|(graph_pool, pool)| (pool.index, graph_pool))
        .collect::<HashMap<_, _>>();
    // The step's scope has a part of the pools of `scope`, which `graph` has
    let graph_token = |step_graph_token| {
        graph
            .token_index(step_graph.token(step_graph_token))
            .unwrap()
    };
    let fractions = fractions_of(
        &split_route
            .parts
            .iter()
            .map(|part| Balance::from(part.weight_percentage))
            .collect::<Vec<_>>(),
    )?;
    Some(Step {
        routes: split_route
            .parts
            .iter()
            .zip(fractions)
            .map(|(part, fraction)| StepRoute {
                hops: part
                    .route
                    .hops
                    .iter()
                    .map(|hop| {
                        graph.hop(
                            graph_pools_by_index[&step_graph.pools()[hop.pool].index],
                            graph_token(hop.token_in),
                            graph_token(hop.token_out),
                        )
                    })
                    .collect(),
                fraction,
            })
            .collect(),
        ..*step
    })
}

/// An amount of the same kind as `amount`
fn with_value(amount: QuoteAmount, value: Balance) -> QuoteAmount {
    match amount {
        QuoteAmount::ExactIn(_) => QuoteAmount::ExactIn(value),
        QuoteAmount::ExactOut(_) => QuoteAmount::ExactOut(value),
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

/// The hops of a part as steps on one hook's pools each, of one route that swaps all of a step
fn steps_of(pools: &RoutingPools, graph: &Graph<HookPool>, hops: &[Hop]) -> Vec<Step> {
    let mut steps = Vec::<Step>::new();
    for hop in hops {
        let snapshot = pools.origins[graph.pools()[hop.pool].index as usize].snapshot;
        let token_out = *graph.token(hop.token_out);
        match steps.last_mut() {
            Some(step) if step.snapshot == snapshot => {
                step.routes[0].hops.push(*hop);
                step.token_out = token_out;
            }
            _ => steps.push(Step {
                snapshot,
                token_in: *graph.token(hop.token_in),
                token_out,
                routes: vec![StepRoute {
                    hops: vec![*hop],
                    fraction: FULL_ROUTE_FRACTION.get(),
                }],
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

    use bigdecimal::BigDecimal;
    use near_min_api::types::{NearGas, NearToken};
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
            fees: vec![intear_dex_common::FeeFraction::new(10_000)],
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
            asset_in_prices: Some(CostPrices {
                near_price_raw: "0.000000000000000000000001".parse().unwrap(),
                quoted_token_price_raw: 1.into(),
            }),
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
        assert_eq!(chain.fee_discount(), None);
        // 1% in each pool, the second of what the first gave
        assert_eq!(chain.pool_fee(), &PoolFee::ratio(199, 10_000));
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
        assert_eq!(quote.pool_fee, PoolFee::ratio(1, 100));
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
    fn last_look_discounts_the_fees_of_every_step_to_beat_the_best_other_quote() {
        let pools = routing_pools(false);
        let undiscounted = out(NEAR_TEAR, out(USDC_NEAR, 1_000_000, 0), 0);
        let best_other_amount_out = undiscounted + 50;
        let beating_amount_out = best_other_amount_out + best_other_amount_out.div_ceil(10_000);
        let routes = find(
            &pools,
            None,
            &PoolRoutesRequest {
                last_look: Some(LastLook {
                    best_other_amount: Amount::AmountOut(best_other_amount_out),
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
        // The smallest whole percent that beats it by 0.01%, which the operations are signed for, so it's
        // taken off the pools of every step
        let QuoteFeeDiscount {
            fee_discount,
            expiration_block_height,
        } = chain.fee_discount().unwrap();
        assert_eq!(
            expiration_block_height,
            1_000 + FEE_DISCOUNT_VALIDITY_BLOCKS
        );
        let fee_discount = fee_discount.get();
        let with = |fee_discount| {
            out(
                NEAR_TEAR,
                out(USDC_NEAR, 1_000_000, fee_discount),
                fee_discount,
            )
        };
        assert!(with(fee_discount) >= beating_amount_out);
        assert!(with(fee_discount - 10_000) < beating_amount_out);
        assert_eq!(chain.estimated_amount(), with(fee_discount));
        // Only a TEAR staker's discount is shown
        assert_eq!(chain.discount(), None);
        let discounted_pool_fee = PoolFee::ratio(fee_millionths(fee_discount), 1_000_000);
        assert_eq!(
            chain.pool_fee(),
            &PoolFee::chained([discounted_pool_fee.clone(), discounted_pool_fee])
        );
    }

    #[test]
    fn tear_stakers_discount_is_shown() {
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
        assert_eq!(
            quote.fee_discount.map(|discount| discount.fee_discount),
            Some(FeeDiscountFraction::new(250_000))
        );
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
    }

    #[test]
    fn an_oracle_that_takes_no_fee_in_the_asset_in_is_paid_with_a_swap_of_it() {
        let pools = routing_pools(true);
        let routes = find(
            &pools,
            None,
            &request("near", "tear", Amount::AmountIn(1_000_000)),
        );
        // The oracle only takes USDC, which a swap on Plach buys with NEAR before the request
        let [quote] = routes.quotes.as_slice() else {
            panic!("Not one quote");
        };
        let near_for_fee = constant_product(
            [USDC_NEAR[1], USDC_NEAR[0]],
            QuoteAmount::ExactOut(ORACLE_FEE),
            0,
        )
        .unwrap()
        .0;
        let most_near_for_fee = near_for_fee * 101 / 100;
        assert_eq!(
            quote.estimated_amount,
            U128(out(NEAR_TEAR, 1_000_000 - most_near_for_fee, 0))
        );
        let oracle_request = quote.oracle_request.as_ref().unwrap();
        assert_eq!(oracle_request.fee, U128(most_near_for_fee));
        let fee_swap = oracle_request.fee_swap.as_ref().unwrap();
        assert_eq!(fee_swap.dex_id, "slimedragon.near/xyk".parse().unwrap());
        assert_eq!(fee_swap.fee, (asset("usdc"), U128(ORACLE_FEE)));
        let fee_swap_request: HookSwapRequest =
            serde_json::from_slice(&fee_swap.message.0).unwrap();
        assert_eq!(
            (fee_swap_request.asset_in, fee_swap_request.asset_out),
            (asset("near"), asset("usdc"))
        );

        // Without a price of the asset in, the swap's gas can't be weighed
        let without_price = find(
            &pools,
            None,
            &PoolRoutesRequest {
                asset_in_prices: None,
                ..request("near", "tear", Amount::AmountIn(1_000_000))
            },
        );
        assert!(without_price.quotes.is_empty() && without_price.chains.is_empty());
    }

    #[test]
    fn an_oracle_fee_is_bought_in_near_when_the_oracle_takes_it() {
        let pools = RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![
                        pool(["xyz", "near"], [1_000_000_000, 1_000_000_000], None),
                        // USDC is bought for less of XYZ
                        pool(["xyz", "usdc"], [1_000_000_000, 2_000_000_000], None),
                    ],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![pool(["xyz", "tear"], NEAR_TEAR, Some(ORACLE_KEY))],
                    oracle_fees: vec![OracleFees {
                        oracle_key: ORACLE_KEY.parse().unwrap(),
                        fees: [
                            (asset("usdc"), ORACLE_FEE.into()),
                            (asset("near"), ORACLE_FEE.into()),
                        ]
                        .into(),
                    }],
                },
            ),
        ]);
        let routes = find(
            &pools,
            None,
            &request("xyz", "tear", Amount::AmountIn(1_000_000)),
        );
        let [quote] = routes.quotes.as_slice() else {
            panic!("Not one quote");
        };
        let fee_swap = quote
            .oracle_request
            .as_ref()
            .and_then(|oracle_request| oracle_request.fee_swap.as_ref())
            .unwrap();
        assert_eq!(fee_swap.fee, (asset("near"), U128(ORACLE_FEE)));
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
        let [estimated_amount] = swapped_as_one(&routes)[..] else {
            panic!("Not one route that swaps as one");
        };
        let direct = out(USDC_TEAR, usdc_in, 0);
        let through_near = out(NEAR_TEAR, out(USDC_NEAR, usdc_in, 0), 0);
        assert!(estimated_amount >= direct.max(through_near));
    }

    /// The estimated amounts of the routes that aren't split
    fn swapped_as_one(routes: &PoolRoutes) -> Vec<u128> {
        routes
            .quotes
            .iter()
            .map(|quote| quote.estimated_amount.0)
            .chain(
                routes
                    .chains
                    .iter()
                    .filter(|chain| chain.branches().len() == 1)
                    .map(IntearDexSwap::estimated_amount),
            )
            .collect()
    }

    /// The direct pool of USDC/TEAR on Plach, and USDC/NEAR on Plach then NEAR/TEAR on Qoq, whose
    /// hops burn `hop_gas`
    fn pools_of_two_routes_through_two_hooks(hop_gas: NearGas) -> RoutingPools {
        let pool = |tokens, reserves| HookPoolInfo {
            hop_gas,
            ..pool(tokens, reserves, None)
        };
        RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![
                        pool(["usdc", "near"], USDC_NEAR),
                        pool(["usdc", "tear"], [1_000_000_000, 6_000_000_000]),
                    ],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![pool(["near", "tear"], NEAR_TEAR)],
                    oracle_fees: vec![],
                },
            ),
        ])
    }

    #[test]
    fn parts_through_several_hooks_are_split_between_them() {
        let routes = find(
            &pools_of_two_routes_through_two_hooks(NearGas::from_tgas(2)),
            None,
            &request("usdc", "tear", Amount::AmountIn(300_000_000)),
        );
        let [split] = routes
            .chains
            .iter()
            .filter(|chain| chain.branches().len() > 1)
            .collect::<Vec<_>>()[..]
        else {
            panic!("Not one split route");
        };
        let best_swapped_as_one = swapped_as_one(&routes).into_iter().max().unwrap();
        assert!(split.estimated_amount() > best_swapped_as_one);
        let branches = split.branches();
        assert_eq!(
            branches
                .iter()
                .map(|branch| branch.fraction.get())
                .sum::<u32>(),
            FULL_ROUTE_FRACTION.get()
        );
        assert_eq!(
            branches
                .iter()
                .map(|branch| branch.estimated_amount)
                .sum::<u128>(),
            split.estimated_amount()
        );
        // Of 1% through the direct pool, and 1% of each of the two pools through NEAR
        let branch_pool_fee = |steps| match steps {
            1 => PoolFee::ratio(1, 100),
            2 => PoolFee::ratio(199, 10_000),
            _ => unreachable!(),
        };
        assert_eq!(
            split.pool_fee(),
            &PoolFee::split(
                branches
                    .iter()
                    .map(|branch| (branch.fraction.get(), branch_pool_fee(branch.steps.len())))
            )
        );
    }

    #[test]
    fn a_branch_that_gives_less_than_its_swaps_burn_is_left_out() {
        // Hops that burn next to nothing, so the search splits the amount whatever gas costs
        let pools = pools_of_two_routes_through_two_hooks(NearGas::from_gas(1));
        let request = request("usdc", "tear", Amount::AmountIn(300_000_000));
        let routes = find(&pools, None, &request);
        let split = routes
            .chains
            .iter()
            .find(|chain| chain.branches().len() > 1)
            .unwrap();
        let gain = split.estimated_amount() - swapped_as_one(&routes).into_iter().max().unwrap();
        // The swaps of the test hooks burn 5 Tgas, and a repeated one on Plach 0.9: at this price
        // any branch's swaps cost more than the split gives
        let near_price_raw = BigDecimal::from(gain)
            / BigDecimal::from(pathfinder::MIN_GAS_PRICE.as_yoctonear() * 500_000_000_000);
        let request = PoolRoutesRequest {
            prices: CostPrices {
                near_price_raw: near_price_raw.clone(),
                quoted_token_price_raw: 1.into(),
            },
            asset_in_prices: Some(CostPrices {
                near_price_raw,
                quoted_token_price_raw: 1.into(),
            }),
            ..request
        };
        let routes = find(&pools, None, &request);
        assert!(routes
            .chains
            .iter()
            .all(|chain| chain.branches().len() == 1));
    }

    #[test]
    fn parts_through_the_same_pools_in_a_step_swap_as_one_route_of_it() {
        let pools = RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![pool(
                        ["usdc", "near"],
                        [100_000_000_000, 200_000_000_000],
                        None,
                    )],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![
                        pool(["near", "tear"], NEAR_TEAR, None),
                        pool(["near", "tear"], NEAR_TEAR, None),
                    ],
                    oracle_fees: vec![],
                },
            ),
        ]);
        // Big enough for the NEAR to be split between both NEAR/TEAR pools
        let usdc_in = 500_000_000;
        let routes = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountIn(usdc_in)),
        );
        let [chain] = routes.chains.as_slice() else {
            panic!("Not one chain");
        };
        let [(first, _), (second, _)] = chain_requests(chain).try_into().unwrap();
        assert_eq!(
            first.routes,
            vec![HookSwapRoute {
                fraction: FULL_ROUTE_FRACTION.get(),
                pools: vec![0],
            }]
        );
        assert_eq!(
            second
                .routes
                .iter()
                .map(|route| route.pools.clone())
                .collect::<Vec<_>>(),
            vec![vec![0], vec![1]]
        );
        // Like the DEXes swap: what the first step gives, split by the second's fractions
        let near = out([100_000_000_000, 200_000_000_000], usdc_in, 0);
        let split = RouteFraction::split(
            TokenAmount::new(near),
            second
                .routes
                .iter()
                .map(|route| RouteFraction::new(route.fraction)),
        );
        let tear = split
            .iter()
            .map(|near_part| out(NEAR_TEAR, near_part.get(), 0))
            .sum::<u128>();
        assert_eq!(chain.estimated_amount(), tear);
    }

    #[test]
    fn a_later_step_splits_what_the_step_before_it_gives() {
        let usdc_near = [1_000_000_000, 1_000_000];
        let big_near_tear = [1_000_000, 3_000_000];
        let small_near_tear = [100_000, 300_000];
        let pools = RoutingPools::of_snapshots(vec![
            snapshot(
                "Plach",
                "slimedragon.near/xyk",
                HookPools {
                    pools: vec![pool(["usdc", "near"], usdc_near, None)],
                    oracle_fees: vec![],
                },
            ),
            snapshot(
                "Qoq",
                "slimedragon.near/amp",
                HookPools {
                    pools: vec![
                        pool(["near", "tear"], big_near_tear, None),
                        pool(["near", "tear"], small_near_tear, None),
                    ],
                    oracle_fees: vec![],
                },
            ),
        ]);
        // Enough to take most of the NEAR of the USDC/NEAR pool, which the search gives to the
        // part that swaps first, so it splits the USDC as if the other part got little of it
        let usdc_in = 10_000_000_000;
        let routes = find(
            &pools,
            None,
            &request("usdc", "tear", Amount::AmountIn(usdc_in)),
        );
        let [chain] = routes.chains.as_slice() else {
            panic!("Not one chain");
        };
        let near = out(usdc_near, usdc_in, 0);
        assert!(chain.estimated_amount() >= out(big_near_tear, near, 0));
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
