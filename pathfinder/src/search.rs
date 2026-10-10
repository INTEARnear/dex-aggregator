//! Route search shared by all DEXes. Candidate paths of up to four hops take the best pool for each
//! hop for the amount it gets. The answer is the best of single routes and splits in 1% steps: the
//! best split between each two of the best `Settings::top_routes` routes, and the routes that get
//! the most when the amount is filled a step at a time into more of the best routes. A DEX provides
//! pool simulation (`Pool`) and turns the found `SplitRoute` into its own response.

use std::collections::HashMap;
use std::hash::Hash;
use std::rc::Rc;
use std::time::Instant;

use near_min_api::types::Balance;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use smallvec::{SmallVec, smallvec};
use tracing::info;

const SPLIT_STEP: u32 = 1; // %
const SMALL_AMOUNT_SPLIT_STEP: u32 = 25; // %
/// Inputs or outputs below this are split in `SMALL_AMOUNT_SPLIT_STEP`s
const SMALL_AMOUNT_THRESHOLD: Balance = 1000;
/// How many pools of a pair are compared for a hop
const DIRECT_ROUTES_COUNT: usize = 3;
/// The amount is filled into this many times `Settings::top_routes` routes to find which split well
const FILL_ROUTES_FACTOR: usize = 4;
/// Routes that got the most of the filled amount, paired with each other
const FILLED_ROUTES_PAIRED: usize = 4;
/// Four-hop routes whose two intermediate tokens share no pool go through this many of the input's
/// and the output's neighbors with the most pools, since trying all of them between tokens with
/// hundreds of neighbors takes too long
const UNPAIRED_INTERMEDIATE_TOKENS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QuoteAmount {
    ExactIn(Balance),
    ExactOut(Balance),
}

impl QuoteAmount {
    pub fn value(self) -> Balance {
        match self {
            Self::ExactIn(amount) | Self::ExactOut(amount) => amount,
        }
    }

    /// Whether `candidate` (output for exact-in, input for exact-out) beats `current`
    fn is_better(self, candidate: Balance, current: Balance) -> bool {
        match self {
            Self::ExactIn(_) => candidate > current,
            Self::ExactOut(_) => candidate < current,
        }
    }

    /// Sort key, higher is better, failed simulations are the worst
    fn key(self, metric: Option<Balance>) -> i128 {
        let metric = metric.and_then(|metric| i128::try_from(metric).ok());
        match self {
            Self::ExactIn(_) => metric.unwrap_or_default(),
            Self::ExactOut(_) => metric.map(|metric| -metric).unwrap_or(i128::MIN),
        }
    }

    /// Like `key`, net of `hops` that cost `hop_cost` each in the metric's token
    fn net_key(self, metric: Option<Balance>, hops: usize, hop_cost: Balance) -> i128 {
        if metric.is_none() {
            return i128::MIN;
        }
        let cost = hop_cost.saturating_mul(hops as Balance);
        self.key(metric)
            .saturating_sub(i128::try_from(cost).unwrap_or(i128::MAX))
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaxHops {
    DirectOnly,
    Two,
    Three,
    Four,
    Max,
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Best routes the amount can be split between
    pub top_routes: usize,
    /// Routes the amount is split between at most
    pub max_splits: usize,
    /// Exact-in routes may use only part of the input when more of it doesn't add output, the DEX
    /// returns the rest
    pub allow_unused_input: bool,
    /// With `MaxHops::Four` or `Max`, also try every route that fewer hops would find. Otherwise
    /// such a route is lost when the best longer leg in its place goes through the input or output
    /// token. With splits, the extra routes can push routes that split well out of `top_routes`.
    pub keep_shorter_routes: bool,
}

/// A pool whose swaps can be simulated. Simulations change the pool like the swap would.
pub trait Pool: Clone {
    type Token: Clone + Eq + Hash;

    /// Tokens the pool trades, swaps refer to them by position
    fn tokens(&self) -> Vec<Self::Token>;

    /// Returns the output
    fn swap_exact_in(
        &mut self,
        token_in: usize,
        token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance>;

    /// Returns the input
    fn swap_exact_out(
        &mut self,
        token_in: usize,
        token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance>;

    /// Like `swap_exact_in`, without changing the pool
    fn quote_exact_in(
        &self,
        token_in: usize,
        token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        self.clone().swap_exact_in(token_in, token_out, amount_in)
    }

    /// Like `swap_exact_out`, without changing the pool
    fn quote_exact_out(
        &self,
        token_in: usize,
        token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance> {
        self.clone().swap_exact_out(token_in, token_out, amount_out)
    }
}

/// Pools changed by simulated swaps, by pool index. Simulations change a few pools, so they're
/// looked up in a list.
#[derive(Clone)]
pub struct PoolsDelta<P> {
    indices: SmallVec<[usize; 8]>,
    pools: SmallVec<[P; 4]>,
}

impl<P> Default for PoolsDelta<P> {
    fn default() -> Self {
        Self {
            indices: SmallVec::new(),
            pools: SmallVec::new(),
        }
    }
}

impl<P> PoolsDelta<P> {
    fn position(&self, pool: usize) -> Option<usize> {
        self.indices.iter().position(|&index| index == pool)
    }

    pub fn get(&self, pool: &usize) -> Option<&P> {
        self.position(*pool).map(|position| &self.pools[position])
    }

    pub fn contains_key(&self, pool: &usize) -> bool {
        self.position(*pool).is_some()
    }

    pub fn insert(&mut self, pool: usize, value: P) {
        match self.position(pool) {
            Some(position) => self.pools[position] = value,
            None => {
                self.indices.push(pool);
                self.pools.push(value);
            }
        }
    }

    pub fn extend(&mut self, other: Self) {
        for (pool, value) in other.indices.into_iter().zip(other.pools) {
            self.insert(pool, value);
        }
    }
}

pub struct Graph<P: Pool> {
    pools: Vec<P>,
    /// Token indices of each pool, in the pool's order
    pool_tokens: Vec<Vec<usize>>,
    tokens: Vec<P::Token>,
    token_indices: HashMap<P::Token, usize>,
    /// Pools of each pair, lower token index first
    pair_pools: FxHashMap<(usize, usize), Vec<usize>>,
    /// Tokens sharing a pool with each token, sorted
    neighbors: Vec<Vec<usize>>,
}

impl<P: Pool> Graph<P> {
    pub fn new(pools: Vec<P>) -> Self {
        let mut tokens = Vec::new();
        let mut token_indices = HashMap::new();
        let mut pool_tokens = Vec::with_capacity(pools.len());
        let mut pair_pools: FxHashMap<(usize, usize), Vec<usize>> = FxHashMap::default();
        for (pool_index, pool) in pools.iter().enumerate() {
            let indices = pool
                .tokens()
                .into_iter()
                .map(|token| match token_indices.get(&token) {
                    Some(&index) => index,
                    None => {
                        tokens.push(token.clone());
                        token_indices.insert(token, tokens.len() - 1);
                        tokens.len() - 1
                    }
                })
                .collect::<Vec<_>>();
            for (i, &a) in indices.iter().enumerate() {
                for &b in &indices[i + 1..] {
                    if a != b {
                        pair_pools.entry(pair(a, b)).or_default().push(pool_index);
                    }
                }
            }
            pool_tokens.push(indices);
        }
        let mut neighbors = vec![Vec::new(); tokens.len()];
        for &(a, b) in pair_pools.keys() {
            neighbors[a].push(b);
            neighbors[b].push(a);
        }
        for token_neighbors in &mut neighbors {
            token_neighbors.sort_unstable();
        }
        Self {
            pools,
            pool_tokens,
            tokens,
            token_indices,
            pair_pools,
            neighbors,
        }
    }

    pub fn pools(&self) -> &[P] {
        &self.pools
    }

    pub fn token(&self, index: usize) -> &P::Token {
        &self.tokens[index]
    }

    pub fn token_index(&self, token: &P::Token) -> Option<usize> {
        self.token_indices.get(token).copied()
    }

    fn has_pair(&self, a: usize, b: usize) -> bool {
        self.pair_pools.contains_key(&pair(a, b))
    }

    fn pair_pools(&self, a: usize, b: usize) -> &[usize] {
        self.pair_pools
            .get(&pair(a, b))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn hop(&self, pool: usize, token_in: usize, token_out: usize) -> Hop {
        let position = |token| {
            self.pool_tokens[pool]
                .iter()
                .position(|&pool_token| pool_token == token)
                .unwrap()
        };
        Hop {
            pool,
            token_in,
            token_out,
            position_in: position(token_in),
            position_out: position(token_out),
        }
    }
}

fn pair(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

#[derive(Debug, Clone, Copy)]
pub struct Hop {
    /// Index in `Graph::pools`
    pub pool: usize,
    /// Index in the graph's tokens
    pub token_in: usize,
    pub token_out: usize,
    /// Positions in the pool's tokens
    position_in: usize,
    position_out: usize,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub hops: SmallVec<[Hop; 4]>,
}

impl Route {
    /// The legs one after another
    fn through(legs: &[Rc<Candidate>]) -> Self {
        Self {
            hops: legs
                .iter()
                .flat_map(|leg| leg.route.hops.iter().copied())
                .collect(),
        }
    }

    fn pools(&self) -> Vec<usize> {
        self.hops.iter().map(|hop| hop.pool).collect()
    }

    fn has_repeated_pools(&self) -> bool {
        self.hops
            .iter()
            .enumerate()
            .any(|(i, hop)| self.hops[..i].iter().any(|other| other.pool == hop.pool))
    }

    fn has_repeated_tokens(&self) -> bool {
        let mut seen = Vec::with_capacity(self.hops.len() + 1);
        seen.push(self.hops[0].token_in);
        for hop in &self.hops {
            if seen.contains(&hop.token_out) {
                return true;
            }
            seen.push(hop.token_out);
        }
        false
    }

    pub fn emulate_exact_in<P: Pool>(
        &self,
        graph: &Graph<P>,
        amount_in: Balance,
        pools_delta: &mut PoolsDelta<P>,
    ) -> Option<Balance> {
        let mut amount = amount_in;
        for hop in &self.hops {
            let mut pool = pools_delta
                .get(&hop.pool)
                .unwrap_or(&graph.pools[hop.pool])
                .clone();
            amount = pool.swap_exact_in(hop.position_in, hop.position_out, amount)?;
            pools_delta.insert(hop.pool, pool);
        }
        Some(amount)
    }

    /// Returns the input and (input, output) of each hop in path order
    pub fn emulate_exact_out<P: Pool>(
        &self,
        graph: &Graph<P>,
        amount_out: Balance,
        pools_delta: &mut PoolsDelta<P>,
    ) -> Option<(Balance, Vec<(Balance, Balance)>)> {
        let mut amount = amount_out;
        let mut hop_amounts = Vec::with_capacity(self.hops.len());
        for hop in self.hops.iter().rev() {
            let mut pool = pools_delta
                .get(&hop.pool)
                .unwrap_or(&graph.pools[hop.pool])
                .clone();
            let amount_in = pool.swap_exact_out(hop.position_in, hop.position_out, amount)?;
            pools_delta.insert(hop.pool, pool);
            hop_amounts.push((amount_in, amount));
            amount = amount_in;
        }
        hop_amounts.reverse();
        Some((amount, hop_amounts))
    }

    /// Like `emulate` on the pools after `before`, the pools it changes are put in `changed`
    fn emulate_after<P: Pool>(
        &self,
        graph: &Graph<P>,
        amount: QuoteAmount,
        before: &PoolsDelta<P>,
        changed: &mut PoolsDelta<P>,
    ) -> Option<Balance> {
        let mut current = amount.value();
        match amount {
            QuoteAmount::ExactIn(_) => {
                for hop in &self.hops {
                    let mut pool = changed
                        .get(&hop.pool)
                        .or_else(|| before.get(&hop.pool))
                        .unwrap_or(&graph.pools[hop.pool])
                        .clone();
                    current = pool.swap_exact_in(hop.position_in, hop.position_out, current)?;
                    changed.insert(hop.pool, pool);
                }
            }
            QuoteAmount::ExactOut(_) => {
                for hop in self.hops.iter().rev() {
                    let mut pool = changed
                        .get(&hop.pool)
                        .or_else(|| before.get(&hop.pool))
                        .unwrap_or(&graph.pools[hop.pool])
                        .clone();
                    current = pool.swap_exact_out(hop.position_in, hop.position_out, current)?;
                    changed.insert(hop.pool, pool);
                }
            }
        }
        Some(current)
    }

    /// Output for exact-in, input for exact-out, on the pools as they are
    fn quote<P: Pool>(&self, graph: &Graph<P>, amount: QuoteAmount) -> Option<Balance> {
        if self.has_repeated_pools() {
            return self.emulate(graph, amount, &mut PoolsDelta::default());
        }
        let mut current = amount.value();
        match amount {
            QuoteAmount::ExactIn(_) => {
                for hop in &self.hops {
                    current = graph.pools[hop.pool].quote_exact_in(
                        hop.position_in,
                        hop.position_out,
                        current,
                    )?;
                }
            }
            QuoteAmount::ExactOut(_) => {
                for hop in self.hops.iter().rev() {
                    current = graph.pools[hop.pool].quote_exact_out(
                        hop.position_in,
                        hop.position_out,
                        current,
                    )?;
                }
            }
        }
        Some(current)
    }

    /// Output for exact-in, input for exact-out
    fn emulate<P: Pool>(
        &self,
        graph: &Graph<P>,
        amount: QuoteAmount,
        pools_delta: &mut PoolsDelta<P>,
    ) -> Option<Balance> {
        match amount {
            QuoteAmount::ExactIn(amount_in) => self.emulate_exact_in(graph, amount_in, pools_delta),
            QuoteAmount::ExactOut(amount_out) => self
                .emulate_exact_out(graph, amount_out, pools_delta)
                .map(|(amount_in, _)| amount_in),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SplitRoutePart {
    pub route: Route,
    /// Percentage of the amount
    pub weight: u32,
}

#[derive(Debug, Clone)]
pub struct SplitRoute {
    /// Heaviest first
    pub parts: Vec<SplitRoutePart>,
}

impl SplitRoute {
    fn new(routes: &[Route], weights: &[u32]) -> Self {
        let mut parts = routes
            .iter()
            .zip(weights)
            .filter(|(_, weight)| **weight > 0)
            .map(|(route, &weight)| SplitRoutePart {
                route: route.clone(),
                weight,
            })
            .collect::<Vec<_>>();
        parts.sort_by_key(|part| std::cmp::Reverse(part.weight));
        Self { parts }
    }

    /// Sum of hops of all parts
    pub fn hops(&self) -> usize {
        self.parts.iter().map(|part| part.route.hops.len()).sum()
    }
}

/// The amount of each part of a split route that adds up to the parts' total weight of `amount`
/// (all of it, unless some input is left unused), the rounding remainder goes to the first part,
/// which is the heaviest
pub fn split_exactly(amount: Balance, parts: &[SplitRoutePart]) -> Vec<Balance> {
    let mut amounts = parts
        .iter()
        .map(|part| split_amount(amount, part.weight))
        .collect::<Vec<_>>();
    let total_weight = parts.iter().map(|part| part.weight).sum();
    amounts[0] += split_amount(amount, total_weight) - amounts.iter().sum::<Balance>();
    amounts
}

/// `weight`% of `amount`, rounded down
pub fn split_amount(amount: Balance, weight: u32) -> Balance {
    let weight = Balance::from(weight);
    amount / 100 * weight + amount % 100 * weight / 100
}

/// The output for exact-in or input for exact-out of a simulation, and the pools it changed
type Emulated<P> = (Balance, PoolsDelta<P>);

/// The best leg between each two tokens of a route
type Legs = SmallVec<[Rc<Candidate>; 3]>;

/// The routes of one search, with every split of routes that share no pool added up from each
/// route's results instead of simulated, and those results cached
struct RouteSet<'a, P: Pool> {
    graph: &'a Graph<P>,
    amount: QuoteAmount,
    routes: Vec<Route>,
    /// Sorted pools of each route
    pools: Vec<Vec<usize>>,
    indices: FxHashMap<Vec<usize>, usize>,
    /// Output for exact-in or input for exact-out of a route with a weight of the amount, on the
    /// pools as they are
    metrics: FxHashMap<(usize, u32), Option<Balance>>,
    /// Like `metrics`, with the pools the route changes
    first_parts: FxHashMap<(usize, u32), Option<Emulated<P>>>,
}

impl<'a, P: Pool> RouteSet<'a, P> {
    fn new(graph: &'a Graph<P>, amount: QuoteAmount) -> Self {
        Self {
            graph,
            amount,
            routes: Vec::new(),
            pools: Vec::new(),
            indices: FxHashMap::default(),
            metrics: FxHashMap::default(),
            first_parts: FxHashMap::default(),
        }
    }

    /// The route's index, the same for the same hops
    fn add(&mut self, route: &Route) -> usize {
        let key = route
            .hops
            .iter()
            .flat_map(|hop| [hop.pool, hop.token_in, hop.token_out])
            .collect::<Vec<_>>();
        *self.indices.entry(key).or_insert_with(|| {
            let mut pools = route.pools();
            pools.sort_unstable();
            self.routes.push(route.clone());
            self.pools.push(pools);
            self.routes.len() - 1
        })
    }

    /// `weight`% of the amount
    fn part_amount(&self, weight: u32) -> QuoteAmount {
        let part_amount = split_amount(self.amount.value(), weight);
        match self.amount {
            QuoteAmount::ExactIn(_) => QuoteAmount::ExactIn(part_amount),
            QuoteAmount::ExactOut(_) => QuoteAmount::ExactOut(part_amount),
        }
    }

    fn metric(&mut self, route: usize, weight: u32) -> Option<Balance> {
        let part_amount = self.part_amount(weight);
        let (graph, routes) = (self.graph, &self.routes);
        *self
            .metrics
            .entry((route, weight))
            .or_insert_with(|| routes[route].quote(graph, part_amount))
    }

    fn share_pools(&self, a: usize, b: usize) -> bool {
        let (mut a, mut b) = (
            self.pools[a].iter().peekable(),
            self.pools[b].iter().peekable(),
        );
        while let (Some(x), Some(y)) = (a.peek(), b.peek()) {
            match x.cmp(y) {
                std::cmp::Ordering::Less => _ = a.next(),
                std::cmp::Ordering::Greater => _ = b.next(),
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    /// Like `SplitRoute::emulate` on the pools as they are. The heaviest part goes first, on the
    /// pools as they are, so its result is cached.
    fn emulate_split(&mut self, mut parts: Vec<(usize, u32)>) -> Option<Balance> {
        parts.sort_by_key(|(_, weight)| std::cmp::Reverse(*weight));
        let Some(&(first_route, first_weight)) = parts.first() else {
            return Some(0);
        };
        let part_amount = self.part_amount(first_weight);
        let (graph, routes) = (self.graph, &self.routes);
        let (mut total, mut pools_delta) = self
            .first_parts
            .entry((first_route, first_weight))
            .or_insert_with(|| {
                let mut pools_delta = PoolsDelta::default();
                let metric = routes[first_route].emulate(graph, part_amount, &mut pools_delta)?;
                Some((metric, pools_delta))
            })
            .clone()?;
        for &(route, weight) in &parts[1..] {
            let metric =
                self.routes[route].emulate(graph, self.part_amount(weight), &mut pools_delta)?;
            total = total.checked_add(metric)?;
        }
        Some(total)
    }

    fn split(&self, parts: &[(usize, u32)]) -> SplitRoute {
        let (routes, weights): (Vec<_>, Vec<_>) = parts
            .iter()
            .map(|&(route, weight)| (self.routes[route].clone(), weight))
            .unzip();
        SplitRoute::new(&routes, &weights)
    }

    /// Output for exact-in or input for exact-out of the split (route, weight)s, on the pools as
    /// they are
    fn split_metric(&mut self, parts: &[(usize, u32)]) -> Option<Balance> {
        let parts = parts
            .iter()
            .copied()
            .filter(|&(_, weight)| weight > 0)
            .collect::<Vec<_>>();
        let independent = parts.iter().enumerate().all(|(i, &(a, _))| {
            parts[i + 1..]
                .iter()
                .all(|&(b, _)| a != b && !self.share_pools(a, b))
        });
        if !independent {
            return self.emulate_split(parts);
        }
        let mut total: Balance = 0;
        for (route, weight) in parts {
            total = total.checked_add(self.metric(route, weight)?)?;
        }
        Some(total)
    }
}

/// Finds the best split route from `token_in` to `token_out` (token indices)
pub fn route<P: Pool>(
    graph: &Graph<P>,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
    max_hops: MaxHops,
    settings: Settings,
    hop_cost: Balance,
) -> Result<SplitRoute, anyhow::Error> {
    let started = Instant::now();
    let mut search = Search {
        graph,
        settings,
        best_routes: FxHashMap::default(),
    };
    let mut candidates = search.candidates(amount, token_in, token_out, max_hops);
    for kind in [
        &mut candidates.paired,
        &mut candidates.shorter,
        &mut candidates.unpaired,
    ] {
        kind.sort_by_key(|candidate| {
            std::cmp::Reverse(amount.net_key(
                candidate.metric,
                candidate.route.hops.len(),
                hop_cost,
            ))
        });
    }
    let mut set = RouteSet::new(graph, amount);
    let mut best_routes = |count| {
        Search::<P>::best(&candidates, count, amount)
            .into_iter()
            .map(|candidate| set.add(&candidate.route))
            .collect::<Vec<_>>()
    };
    let routes = best_routes(settings.top_routes);
    let fill_routes = if settings.max_splits > 1 {
        best_routes(settings.top_routes * FILL_ROUTES_FACTOR)
    } else {
        Vec::new()
    };
    info!("Found {} routes in {:?}", routes.len(), started.elapsed());
    if routes.is_empty() {
        return Err(anyhow::anyhow!("No routes found"));
    }

    let mut best: Option<(SplitRoute, Balance)> = None;
    fn consider(
        split: SplitRoute,
        metric: Balance,
        amount: QuoteAmount,
        hop_cost: Balance,
        best: &mut Option<(SplitRoute, Balance)>,
    ) {
        let net_key =
            |split: &SplitRoute, metric| amount.net_key(Some(metric), split.hops(), hop_cost);
        if best.as_ref().is_none_or(|(best_split, best_metric)| {
            net_key(&split, metric) > net_key(best_split, *best_metric)
        }) {
            *best = Some((split, metric));
        }
    }
    // Splitting can only get less than a single route because of its 1% steps, and costs more hops
    for &route in &routes {
        if let Some(metric) = set.metric(route, 100) {
            consider(
                set.split(&[(route, 100)]),
                metric,
                amount,
                hop_cost,
                &mut best,
            );
        }
    }
    if settings.max_splits > 1 {
        if let Some((split, metric)) = best_pair_split(&mut set, &routes) {
            consider(split, metric, amount, hop_cost, &mut best);
        }
        for (split, metric) in fill_splits(&mut set, &fill_routes, settings) {
            consider(split, metric, amount, hop_cost, &mut best);
        }
    }
    let Some((best, metric)) = best else {
        return Err(anyhow::anyhow!("No valid split route found"));
    };
    if metric == 0 {
        return Err(anyhow::anyhow!("Estimated amount is 0"));
    }
    info!(
        "Best split route: {best:?} {metric}, found in {:?}",
        started.elapsed()
    );
    Ok(best)
}

/// Splits from filling the amount into the routes: the filled split itself if it uses few enough
/// routes, and the best split between two of the routes that got the most
fn fill_splits<P: Pool>(
    set: &mut RouteSet<P>,
    routes: &[usize],
    settings: Settings,
) -> Vec<(SplitRoute, Balance)> {
    let amount = set.amount;
    let step = split_step(set, routes);
    let weights = fill(set, routes, step);
    let total_weight = weights.iter().sum::<u32>();
    if total_weight == 0
        || total_weight != 100
            && !(settings.allow_unused_input && matches!(amount, QuoteAmount::ExactIn(_)))
    {
        return Vec::new();
    }
    let mut filled = routes
        .iter()
        .copied()
        .zip(weights)
        .filter(|(_, weight)| *weight > 0)
        .collect::<Vec<_>>();
    filled.sort_by_key(|(_, weight)| std::cmp::Reverse(*weight));

    let mut splits = Vec::new();
    if filled.len() <= settings.max_splits
        && let Some(metric) = set.split_metric(&filled)
    {
        splits.push((set.split(&filled), metric));
    }
    let paired = filled
        .iter()
        .take(FILLED_ROUTES_PAIRED)
        .map(|(route, _)| *route)
        .collect::<Vec<_>>();
    if paired.len() > 1 {
        splits.extend(best_pair_split(set, &paired));
    }
    splits
}

/// Fills the amount into the routes a step at a time, each step into the route that gains the
/// most from it after the previous steps changed the pools. Returns each route's weight.
fn fill<P: Pool>(set: &mut RouteSet<P>, routes: &[usize], step: u32) -> Vec<u32> {
    let (graph, amount) = (set.graph, set.amount);
    let mut weights = vec![0; routes.len()];
    let step_amount = split_amount(amount.value(), step);
    if step_amount == 0 {
        return weights;
    }
    let step_amount = match amount {
        QuoteAmount::ExactIn(_) => QuoteAmount::ExactIn(step_amount),
        QuoteAmount::ExactOut(_) => QuoteAmount::ExactOut(step_amount),
    };
    let mut pools_delta = PoolsDelta::default();
    // A step into each route and the pools it changes, until a step changes the route's pools
    let mut steps: Vec<Option<(Option<Balance>, PoolsDelta<P>)>> = vec![None; routes.len()];
    for _ in 0..100 / step {
        let mut best: Option<(usize, Balance)> = None;
        for (index, &route) in routes.iter().enumerate() {
            let (metric, _) = steps[index].get_or_insert_with(|| {
                let mut changed = PoolsDelta::default();
                let metric =
                    set.routes[route].emulate_after(graph, step_amount, &pools_delta, &mut changed);
                (metric, changed)
            });
            if let Some(metric) = *metric
                && best
                    .as_ref()
                    .is_none_or(|(_, best_metric)| step_amount.is_better(metric, *best_metric))
            {
                best = Some((index, metric));
            }
        }
        let Some((index, metric)) = best else {
            break;
        };
        // More input doesn't add output
        if metric == 0 && matches!(amount, QuoteAmount::ExactIn(_)) {
            break;
        }
        weights[index] += step;
        let (_, changed) = steps[index].take().unwrap();
        for (other, other_step) in routes.iter().zip(&mut steps) {
            if set.pools[*other]
                .iter()
                .any(|pool| changed.contains_key(pool))
            {
                *other_step = None;
            }
        }
        pools_delta.extend(changed);
    }
    weights
}

/// The best split between two of the routes. For a pair, the amount split by weight is close to
/// unimodal in the weight, so it's found with a ternary search over the weights.
fn best_pair_split<P: Pool>(
    set: &mut RouteSet<P>,
    routes: &[usize],
) -> Option<(SplitRoute, Balance)> {
    let amount = set.amount;
    let step = split_step(set, routes);
    let steps = 100 / step;
    let better = |a: Option<Balance>, b: Option<Balance>| match (a, b) {
        (Some(a), Some(b)) => amount.is_better(a, b),
        (Some(_), None) => true,
        (None, _) => false,
    };
    let mut best: Option<([(usize, u32); 2], Balance)> = None;
    let mut metrics = vec![None; steps as usize + 1];
    for (i, &first) in routes.iter().enumerate() {
        for &second in &routes[i + 1..] {
            let split = |k: u32| [(first, k * step), (second, 100 - k * step)];
            metrics.fill(None);
            let mut metric_at = |k: u32| -> Option<Balance> {
                *metrics[k as usize].get_or_insert_with(|| set.split_metric(&split(k)))
            };
            let (mut low, mut high) = (1, steps - 1);
            while high - low > 2 {
                let third = (high - low) / 3;
                let (a, b) = (low + third, high - third);
                if better(metric_at(a), metric_at(b)) {
                    high = b;
                } else {
                    low = a;
                }
            }
            for k in low..=high {
                if let Some(metric) = metric_at(k)
                    && best
                        .as_ref()
                        .is_none_or(|(_, best_metric)| amount.is_better(metric, *best_metric))
                {
                    best = Some((split(k), metric));
                }
            }
        }
    }
    best.map(|(split, metric)| (set.split(&split), metric))
}

fn split_step<P: Pool>(set: &mut RouteSet<P>, routes: &[usize]) -> u32 {
    let is_small_amount = set.amount.value() < SMALL_AMOUNT_THRESHOLD
        || set
            .metric(routes[0], 100)
            .is_none_or(|metric| metric < SMALL_AMOUNT_THRESHOLD);
    if is_small_amount {
        SMALL_AMOUNT_SPLIT_STEP
    } else {
        SPLIT_STEP
    }
}

/// A route and its output for exact-in or input for exact-out, `None` if it fails
#[derive(Clone)]
struct Candidate {
    route: Route,
    metric: Option<Balance>,
}

/// Candidate routes by kind, best first. The best of each kind are kept, so that new kinds of
/// routes don't push out routes that split well.
#[derive(Default)]
struct Candidates {
    /// Direct, through one intermediate token, or two that share a pool
    paired: Vec<Candidate>,
    /// Only found with shorter legs (`Settings::keep_shorter_routes`)
    shorter: Vec<Candidate>,
    /// Through two intermediate tokens that share no pool
    unpaired: Vec<Candidate>,
}

struct Search<'a, P: Pool> {
    graph: &'a Graph<P>,
    settings: Settings,
    /// `find_best_route` results of this search, they don't depend on anything else
    best_routes: FxHashMap<(usize, usize, QuoteAmount, MaxHops), Option<Rc<Candidate>>>,
}

impl<P: Pool> Search<'_, P> {
    fn find_best_route(
        &mut self,
        amount: QuoteAmount,
        from: usize,
        to: usize,
        max_hops: MaxHops,
    ) -> Option<Rc<Candidate>> {
        let key = (from, to, amount, max_hops);
        if let Some(candidate) = self.best_routes.get(&key) {
            return candidate.clone();
        }
        let candidate = match max_hops {
            MaxHops::DirectOnly => self.best_direct(amount, from, to),
            MaxHops::Two => self.best_two_hops(amount, from, to),
            MaxHops::Three | MaxHops::Four | MaxHops::Max => self
                .find_best_routes(amount, from, to, max_hops, 1)
                .into_iter()
                .next(),
        }
        .map(Rc::new);
        self.best_routes.insert(key, candidate.clone());
        candidate
    }

    /// Like `find_best_routes` with `MaxHops::DirectOnly` and a count of 1, without collecting
    /// and sorting the candidates: the first of the best
    fn best_direct(&self, amount: QuoteAmount, from: usize, to: usize) -> Option<Candidate> {
        let graph = self.graph;
        let mut best: Option<(i128, Hop, Option<Balance>)> = None;
        for &pool in graph.pair_pools(from, to) {
            let hop = graph.hop(pool, from, to);
            let pool = &graph.pools[pool];
            let metric = match amount {
                QuoteAmount::ExactIn(amount_in) => {
                    pool.quote_exact_in(hop.position_in, hop.position_out, amount_in)
                }
                QuoteAmount::ExactOut(amount_out) => {
                    pool.quote_exact_out(hop.position_in, hop.position_out, amount_out)
                }
            };
            let key = amount.key(metric);
            if best.as_ref().is_none_or(|(best_key, ..)| key > *best_key) {
                best = Some((key, hop, metric));
            }
        }
        best.map(|(_, hop, metric)| Candidate {
            route: Route {
                hops: smallvec![hop],
            },
            metric,
        })
    }

    /// Like `find_best_routes` with `MaxHops::Two` and a count of 1: the first of the best of the
    /// direct routes, then the routes through each intermediate token. A route is only built
    /// when it's the best so far.
    fn best_two_hops(&mut self, amount: QuoteAmount, from: usize, to: usize) -> Option<Candidate> {
        let graph = self.graph;
        let mut best = self
            .find_best_route(amount, from, to, MaxHops::DirectOnly)
            .map(|candidate| (amount.key(candidate.metric), Candidate::clone(&candidate)));
        let to_neighbors = &graph.neighbors[to];
        for &middle in &graph.neighbors[from] {
            if middle == to || to_neighbors.binary_search(&middle).is_err() {
                continue;
            }
            let Some((legs, leg_amount)) =
                self.legs(amount, &[from, middle, to], &[MaxHops::DirectOnly; 2])
            else {
                continue;
            };
            let mut route = None;
            let metric = if legs_share_pools(&legs) {
                route.insert(Route::through(&legs)).quote(graph, amount)
            } else {
                leg_amount
            };
            let key = amount.key(metric);
            if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                let route = route.unwrap_or_else(|| Route::through(&legs));
                if !route.has_repeated_tokens() {
                    best = Some((key, Candidate { route, metric }));
                }
            }
        }
        best.map(|(_, candidate)| candidate)
    }

    /// Keeps the `count` best candidates
    fn top(mut candidates: Vec<Candidate>, count: usize, amount: QuoteAmount) -> Vec<Candidate> {
        candidates.sort_by_key(|candidate| std::cmp::Reverse(amount.key(candidate.metric)));
        candidates.truncate(count);
        candidates
    }

    /// The `count` best routes, and the `count` best of those only found with shorter legs
    fn find_best_routes(
        &mut self,
        amount: QuoteAmount,
        from: usize,
        to: usize,
        max_hops: MaxHops,
        count: usize,
    ) -> Vec<Candidate> {
        let candidates = self.candidates(amount, from, to, max_hops);
        Self::best(&candidates, count, amount)
    }

    /// The `count` best routes of each kind
    fn best(candidates: &Candidates, count: usize, amount: QuoteAmount) -> Vec<Candidate> {
        let mut best = Vec::new();
        for kind in [
            &candidates.paired,
            &candidates.shorter,
            &candidates.unpaired,
        ] {
            best.extend_from_slice(&kind[..count.min(kind.len())]);
        }
        if count == 1 {
            best = Self::top(best, 1, amount);
        }
        best
    }

    /// Routes through intermediate tokens, each hop through the best pool for the amount it gets,
    /// by kind, best first
    fn candidates(
        &mut self,
        amount: QuoteAmount,
        from: usize,
        to: usize,
        max_hops: MaxHops,
    ) -> Candidates {
        let graph = self.graph;
        if max_hops == MaxHops::DirectOnly && !graph.has_pair(from, to) {
            return Candidates::default();
        }
        let direct_routes = graph
            .pair_pools(from, to)
            .iter()
            .map(|&pool| {
                let route = Route {
                    hops: smallvec![graph.hop(pool, from, to)],
                };
                let metric = route.quote(graph, amount);
                Candidate { route, metric }
            })
            .collect();
        let mut candidates = Self::top(direct_routes, DIRECT_ROUTES_COUNT, amount);
        if max_hops == MaxHops::DirectOnly {
            return Candidates {
                paired: candidates,
                ..Candidates::default()
            };
        }

        let from_neighbors = &graph.neighbors[from];
        let to_neighbors = &graph.neighbors[to];
        for &middle in from_neighbors {
            if middle == to || to_neighbors.binary_search(&middle).is_err() {
                continue;
            }
            if let Some(candidate) =
                self.through(amount, &[from, middle, to], &[MaxHops::DirectOnly; 2])
            {
                candidates.push(candidate);
            }
        }

        // Hops of the legs to the first intermediate token, between the two, and from the second
        const THREE: [MaxHops; 3] = [MaxHops::DirectOnly; 3];
        const FOUR: [MaxHops; 3] = [MaxHops::DirectOnly, MaxHops::Two, MaxHops::DirectOnly];
        const MAX: [MaxHops; 3] = [MaxHops::Three; 3];
        let leg_hops: &[[MaxHops; 3]] = match (max_hops, self.settings.keep_shorter_routes) {
            (MaxHops::DirectOnly | MaxHops::Two, _) => &[],
            (MaxHops::Three, _) => &[THREE],
            (MaxHops::Four, false) => &[FOUR],
            (MaxHops::Four, true) => &[THREE, FOUR],
            (MaxHops::Max, false) => &[MAX],
            (MaxHops::Max, true) => &[THREE, FOUR, MAX],
        };
        let mut shorter_candidates = Vec::new();
        let mut unpaired_candidates = Vec::new();
        if !leg_hops.is_empty() {
            let most_connected = |tokens: &[usize]| {
                let mut tokens = tokens.to_vec();
                tokens.sort_by_key(|&token| std::cmp::Reverse(graph.neighbors[token].len()));
                tokens.truncate(UNPAIRED_INTERMEDIATE_TOKENS);
                tokens
            };
            let (connected_firsts, connected_seconds) =
                (most_connected(from_neighbors), most_connected(to_neighbors));
            for &first in from_neighbors {
                if first == to {
                    continue;
                }
                for &second in to_neighbors {
                    if second == from || second == first {
                        continue;
                    }
                    // A middle leg of more than one hop doesn't need a pool of the two tokens
                    let routable = |hops: &[MaxHops; 3]| {
                        graph.has_pair(first, second)
                            || hops[1] != MaxHops::DirectOnly
                                && connected_firsts.contains(&first)
                                && connected_seconds.contains(&second)
                    };
                    let (longest, shorter) = leg_hops.split_last().unwrap();
                    if routable(longest)
                        && let Some(candidate) =
                            self.through(amount, &[from, first, second, to], longest)
                    {
                        if graph.has_pair(first, second) {
                            candidates.push(candidate);
                        } else {
                            unpaired_candidates.push(candidate);
                        }
                    }
                    for hops in shorter.iter().filter(|hops| routable(hops)) {
                        if let Some(candidate) =
                            self.through(amount, &[from, first, second, to], hops)
                        {
                            shorter_candidates.push(candidate);
                        }
                    }
                }
            }
        }

        candidates.retain(|candidate| !candidate.route.has_repeated_tokens());
        if !shorter_candidates.is_empty() {
            let pools = candidates
                .iter()
                .map(|candidate| candidate.route.pools())
                .collect::<FxHashSet<_>>();
            shorter_candidates.retain(|candidate| {
                !candidate.route.has_repeated_tokens() && !pools.contains(&candidate.route.pools())
            });
        }
        unpaired_candidates.retain(|candidate| !candidate.route.has_repeated_tokens());
        Candidates {
            paired: Self::top(candidates, usize::MAX, amount),
            shorter: Self::top(shorter_candidates, usize::MAX, amount),
            unpaired: Self::top(unpaired_candidates, usize::MAX, amount),
        }
    }

    /// The best route through `tokens` in order, legs between them limited to `hops`. Exact-in
    /// legs are chosen from the input, exact-out legs from the output. Fails if a leg before the
    /// last one (by direction of the search) fails, a failing last leg makes the route fail.
    fn through(
        &mut self,
        amount: QuoteAmount,
        tokens: &[usize],
        hops: &[MaxHops],
    ) -> Option<Candidate> {
        let (legs, leg_amount) = self.legs(amount, tokens, hops)?;
        let route = Route::through(&legs);
        // Each leg was simulated on its own, together they only change each other through shared pools
        let metric = if route.has_repeated_pools() {
            route.quote(self.graph, amount)
        } else {
            leg_amount
        };
        Some(Candidate { route, metric })
    }

    /// The best leg between each two of `tokens` in path order, and the metric of the leg
    /// simulated last, like `through`
    fn legs(
        &mut self,
        amount: QuoteAmount,
        tokens: &[usize],
        hops: &[MaxHops],
    ) -> Option<(Legs, Option<Balance>)> {
        let legs = tokens.len() - 1;
        let mut found = SmallVec::new();
        let mut leg_amount = Some(amount.value());
        match amount {
            QuoteAmount::ExactIn(_) => {
                for leg in 0..legs {
                    let candidate = self.find_best_route(
                        QuoteAmount::ExactIn(leg_amount?),
                        tokens[leg],
                        tokens[leg + 1],
                        hops[leg],
                    )?;
                    leg_amount = candidate.metric;
                    found.push(candidate);
                }
            }
            QuoteAmount::ExactOut(_) => {
                for leg in (0..legs).rev() {
                    let candidate = self.find_best_route(
                        QuoteAmount::ExactOut(leg_amount?),
                        tokens[leg],
                        tokens[leg + 1],
                        hops[leg],
                    )?;
                    leg_amount = candidate.metric;
                    found.push(candidate);
                }
                found.reverse();
            }
        }
        Some((found, leg_amount))
    }
}

/// Whether two of the legs go through the same pool
fn legs_share_pools(legs: &[Rc<Candidate>]) -> bool {
    let mut pools = legs
        .iter()
        .flat_map(|leg| leg.route.hops.iter().map(|hop| hop.pool));
    let mut seen = SmallVec::<[usize; 8]>::new();
    pools.any(|pool| {
        let repeated = seen.contains(&pool);
        seen.push(pool);
        repeated
    })
}
