#![deny(clippy::float_arithmetic)]
#![allow(clippy::manual_div_ceil)]

mod math;
mod pool;

use bigdecimal::{BigDecimal, RoundingMode};
use futures_util::StreamExt;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, BlockId, BlockReference, U128};
use near_min_api::utils::dec_format;
use num_traits::{FromPrimitive, ToPrimitive};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;
use warp::Filter;

use near_min_api::{QueryFinality, RpcClient, types::Finality};
use serde::{Deserialize, Serialize};
use tracing::{Level, error, info, warn};

use crate::math::{LEFT_MOST_POINT, RIGHT_MOST_POINT, SwapResult, U512, as_u128, mul_div_floor};
use crate::pool::{Pool, PoolInit};

#[derive(Serialize)]
struct ApiResponse<T> {
    result_code: i32,
    result_message: String,
    result_data: Option<T>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "quote_type", rename_all = "snake_case")]
enum RouteApiResponse {
    ExactIn {
        route: ExactInRoute,
        contract_in: AccountId,
        contract_out: AccountId,
        amount_in: U128,
        amount_out: U128,
    },
    ExactOut {
        route: ExactOutRoute,
        contract_in: AccountId,
        contract_out: AccountId,
        amount_in: U128,
        max_amount_in: U128,
        amount_out: U128,
    },
}

#[derive(Serialize, Debug)]
struct ExactInRoute {
    pools: Vec<ApiPoolStep>,
    amount_in: U128,
    min_amount_out: U128,
    amount_out: U128,
}

#[derive(Serialize, Debug)]
struct ExactOutRoute {
    // In path order, SwapByOutput takes them in reverse order
    pools: Vec<ApiPoolStep>,
    amount_in: U128,
    max_amount_in: U128,
    amount_out: U128,
}

#[derive(Serialize, Debug)]
struct ApiPoolStep {
    pool_id: String,
    token_in: AccountId,
    token_out: AccountId,
}

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";
// The swap math was verified against this build (2.3.13), quoting stops if the code changes
const VERIFIED_CODE_HASH: &str = "7jHmiuDCFiCr6VDPszptYNPHWaYvg7aQoeLSGzqqtNNn";
const LIST_POOLS_PAGE_SIZE: u64 = 300;
const POINT_DATA_BATCH_SIZE: usize = 50;
const POINT_DATA_CONCURRENCY: usize = 4;
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);
const SLIPPAGE_DENOMINATOR: u128 = 10_000;

const RC_SUCCESS: i32 = 0;
const RC_POOL_FETCH_ERROR: i32 = 1;
const RC_ROUTE_ERROR: i32 = 2;
const RC_RESPONSE_BUILD_ERROR: i32 = 3;
const RC_INVALID_SLIPPAGE: i32 = 4;

#[derive(Deserialize)]
struct Metadata {
    state: String,
    #[serde(with = "dec_format")]
    pool_count: u64,
    protocol_fee_rate: u32,
}

#[derive(Deserialize)]
struct PoolInfo {
    pool_id: String,
    token_x: AccountId,
    token_y: AccountId,
    fee: u32,
    point_delta: i32,
    current_point: i32,
    #[serde(with = "dec_format")]
    liquidity: Balance,
    #[serde(with = "dec_format")]
    liquidity_x: Balance,
    #[serde(with = "dec_format")]
    total_fee_x_charged: Balance,
    #[serde(with = "dec_format")]
    total_fee_y_charged: Balance,
    total_liquidity: String,
    total_order_x: String,
    total_order_y: String,
    state: String,
}

#[derive(Deserialize)]
struct LiquiditySegment {
    left_point: i32,
    right_point: i32,
    #[serde(with = "dec_format")]
    amount_l: Balance,
}

#[derive(Deserialize)]
struct PointOrder {
    point: i32,
    #[serde(with = "dec_format")]
    amount_x: Balance,
    #[serde(with = "dec_format")]
    amount_y: Balance,
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

struct Pools {
    pools: Vec<Pool>,
    tokens: Vec<AccountId>,
    token_ids: HashMap<AccountId, usize>,
    pair_pools: HashMap<(usize, usize), Vec<usize>>,
    neighbors: Vec<Vec<usize>>,
    block_height: BlockHeight,
    updated_at: Instant,
}

impl Pools {
    fn new(pools: Vec<Pool>, block_height: BlockHeight) -> Self {
        let mut tokens = Vec::new();
        let mut token_ids = HashMap::new();
        let mut pair_pools: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        let mut token_id = |token: &AccountId| -> usize {
            *token_ids.entry(token.clone()).or_insert_with(|| {
                tokens.push(token.clone());
                tokens.len() - 1
            })
        };
        for (pool_index, pool) in pools.iter().enumerate() {
            let x = token_id(&pool.data.token_x);
            let y = token_id(&pool.data.token_y);
            pair_pools
                .entry((x.min(y), x.max(y)))
                .or_default()
                .push(pool_index);
        }
        let mut neighbors = vec![Vec::new(); tokens.len()];
        for &(a, b) in pair_pools.keys() {
            neighbors[a].push(b);
            neighbors[b].push(a);
        }
        Self {
            pools,
            tokens,
            token_ids,
            pair_pools,
            neighbors,
            block_height,
            updated_at: Instant::now(),
        }
    }

    fn pair_pools(&self, a: usize, b: usize) -> &[usize] {
        self.pair_pools
            .get(&(a.min(b), a.max(b)))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn has_pair(&self, a: usize, b: usize) -> bool {
        self.pair_pools.contains_key(&(a.min(b), a.max(b)))
    }
}

#[derive(Debug)]
struct CodeHashChanged(String);

impl Display for CodeHashChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{RHEA_DCL_CONTRACT_ID} code hash is {}, but the swap math was verified for {VERIFIED_CODE_HASH}",
            self.0
        )
    }
}

impl std::error::Error for CodeHashChanged {}

#[derive(Default)]
struct SyncedPools {
    pools: HashMap<String, (serde_json::Value, Option<Pool>)>,
}

async fn get_all_pools(
    client: &RpcClient,
    synced: &mut SyncedPools,
) -> Result<Pools, anyhow::Error> {
    let contract_id: AccountId = RHEA_DCL_CONTRACT_ID.parse().unwrap();
    let block_height = client
        .block(BlockReference::Finality(Finality::None))
        .await?
        .header
        .height;
    let at = QueryFinality::BlockId(BlockId::Height(block_height));

    let (account, metadata, frozen_tokens) = tokio::try_join!(
        async {
            Ok::<_, anyhow::Error>(client.view_account(contract_id.clone(), at.clone()).await?)
        },
        async {
            Ok::<_, anyhow::Error>(
                client
                    .call::<Metadata>(contract_id.clone(), "get_metadata", json!({}), at.clone())
                    .await?,
            )
        },
        async {
            Ok::<_, anyhow::Error>(
                client
                    .call::<Vec<AccountId>>(
                        contract_id.clone(),
                        "get_frozenlist_tokens",
                        json!({}),
                        at.clone(),
                    )
                    .await?,
            )
        },
    )?;
    let code_hash = account.code_hash.to_string();
    if code_hash != VERIFIED_CODE_HASH {
        return Err(CodeHashChanged(code_hash).into());
    }

    let page_requests = (0..metadata.pool_count)
        .step_by(LIST_POOLS_PAGE_SIZE as usize)
        .map(|from_index| {
            (
                contract_id.clone(),
                "list_pools",
                json!({ "from_index": from_index, "limit": LIST_POOLS_PAGE_SIZE }),
                at.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut entries = Vec::new();
    for page in client
        .batch_call::<Vec<serde_json::Value>>(page_requests)
        .await?
    {
        entries.extend(page?);
    }

    let mut changed = Vec::new();
    for entry in entries {
        let info: PoolInfo = serde_json::from_value(entry.clone())?;
        if synced
            .pools
            .get(&info.pool_id)
            .is_some_and(|(last_entry, _)| *last_entry == entry)
        {
            continue;
        }
        let has_liquidity_or_orders = [
            &info.total_liquidity,
            &info.total_order_x,
            &info.total_order_y,
        ]
        .iter()
        .any(|amount| *amount != "0");
        if info.state == "Running" && has_liquidity_or_orders {
            changed.push((info, entry));
        } else {
            synced.pools.insert(info.pool_id.clone(), (entry, None));
        }
    }

    if !changed.is_empty() {
        let pool_ids = changed
            .iter()
            .map(|(info, _)| info.pool_id.clone())
            .collect::<Vec<_>>();
        let (segments, orders) = tokio::try_join!(
            fetch_point_data::<HashMap<String, LiquiditySegment>>(
                client,
                &contract_id,
                "get_liquidity_range",
                &pool_ids,
                &at,
            ),
            fetch_point_data::<HashMap<String, PointOrder>>(
                client,
                &contract_id,
                "get_pointorder_range",
                &pool_ids,
                &at,
            ),
        )?;
        for (((info, entry), segments), orders) in changed.into_iter().zip(segments).zip(orders) {
            let pool_id = info.pool_id.clone();
            let pool = segments.and_then(|segments| {
                let orders = orders?;
                Pool::new(
                    PoolInit {
                        id: info.pool_id,
                        token_x: info.token_x,
                        token_y: info.token_y,
                        fee: info.fee,
                        point_delta: info.point_delta,
                        current_point: info.current_point,
                        liquidity: info.liquidity,
                        liquidity_x: info.liquidity_x,
                        fee_charged_x: info.total_fee_x_charged,
                        fee_charged_y: info.total_fee_y_charged,
                        protocol_fee_rate: metadata.protocol_fee_rate,
                    },
                    segments
                        .into_values()
                        .map(|segment| (segment.left_point, segment.right_point, segment.amount_l)),
                    orders
                        .into_values()
                        .map(|order| (order.point, order.amount_x, order.amount_y)),
                )
            });
            match pool {
                Ok(pool) => {
                    synced.pools.insert(pool_id, (entry, Some(pool)));
                }
                Err(e) => {
                    // Not remembered, so it's fetched again on the next refresh
                    error!("Failed to load pool {pool_id} at block {block_height}: {e}");
                    synced.pools.remove(&pool_id);
                }
            }
        }
        info!(
            "Loaded {} changed pools at block {block_height}",
            pool_ids.len()
        );
    }

    let pools = if metadata.state == "Running" {
        let frozen_tokens = frozen_tokens.into_iter().collect::<HashSet<_>>();
        synced
            .pools
            .values()
            .filter_map(|(_, pool)| pool.as_ref())
            .filter(|pool| {
                !frozen_tokens.contains(&pool.data.token_x)
                    && !frozen_tokens.contains(&pool.data.token_y)
            })
            .map(|pool| {
                let mut pool = pool.clone();
                pool.set_protocol_fee_rate(metadata.protocol_fee_rate);
                pool
            })
            .collect()
    } else {
        warn!("{RHEA_DCL_CONTRACT_ID} is {}", metadata.state);
        Vec::new()
    };
    Ok(Pools::new(pools, block_height))
}

async fn fetch_point_data<R: DeserializeOwned>(
    client: &RpcClient,
    contract_id: &AccountId,
    method: &'static str,
    pool_ids: &[String],
    at: &QueryFinality,
) -> Result<Vec<Result<R, anyhow::Error>>, anyhow::Error> {
    let requests = pool_ids
        .iter()
        .map(|pool_id| {
            (
                contract_id.clone(),
                method,
                json!({
                    "pool_id": pool_id,
                    "left_point": LEFT_MOST_POINT,
                    "right_point": RIGHT_MOST_POINT,
                }),
                at.clone(),
            )
        })
        .collect::<Vec<_>>();
    // A plain loop instead of a closure: a closure here makes the spawned update task fail
    // the `Send` check ("implementation of `FnOnce` is not general enough")
    let mut batch_futures = Vec::new();
    for chunk in requests.chunks(POINT_DATA_BATCH_SIZE) {
        batch_futures.push(client.batch_call::<R>(chunk.to_vec()));
    }
    let batches = futures_util::stream::iter(batch_futures)
        .buffered(POINT_DATA_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut results = Vec::with_capacity(pool_ids.len());
    for batch in batches {
        results.extend(batch?.into_iter().map(|result| result.map_err(Into::into)));
    }
    Ok(results)
}

async fn start_pools_update_task(client: Arc<RpcClient>) {
    tokio::spawn(async move {
        let mut synced = SyncedPools::default();
        loop {
            match get_all_pools(&client, &mut synced).await {
                Ok(pools) => {
                    let mut cache = POOLS_CACHE.write().await;
                    *cache = Some(Arc::new(pools));
                }
                Err(e) => {
                    if e.downcast_ref::<CodeHashChanged>().is_some() {
                        let mut cache = POOLS_CACHE.write().await;
                        *cache = None;
                    }
                    error!("Failed to update pools: {e}");
                }
            }

            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

async fn get_pools() -> Result<Arc<Pools>, anyhow::Error> {
    let Some(pools) = POOLS_CACHE.read().await.clone() else {
        return Err(anyhow::anyhow!("Pools are not loaded"));
    };
    let age = pools.updated_at.elapsed();
    if age > MAX_SNAPSHOT_AGE {
        return Err(anyhow::anyhow!(
            "Pools were last updated {age:?} ago, at block {}",
            pools.block_height
        ));
    }
    Ok(pools)
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .pretty()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::INFO)
        .init();

    let client = Arc::new(RpcClient::new(
        std::env::var("RPC_URLS")
            .unwrap_or_else(|_| {
                "https://rpc.intea.rs,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                    .to_string()
            })
            .split(',')
            .map(|url| url.to_string())
            .collect::<Vec<_>>(),
    ));

    start_pools_update_task(client.clone()).await;

    let api = warp::path("findPath")
        .and(warp::query::<FindPathQuery>())
        .and_then(handle_find_path);

    info!("Server listening on http://localhost:12347/findPath ...");
    warp::serve(api).run(([127, 0, 0, 1], 12347)).await;

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum QuoteAmount {
    ExactIn(Balance),
    ExactOut(Balance),
}

impl QuoteAmount {
    fn value(self) -> Balance {
        match self {
            Self::ExactIn(amount) | Self::ExactOut(amount) => amount,
        }
    }

    fn is_better(self, candidate: Balance, current: Balance) -> bool {
        match self {
            Self::ExactIn(_) => candidate > current,
            Self::ExactOut(_) => candidate < current,
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MaxHops {
    DirectOnly,
    Two,
    Three,
}

#[derive(Debug, Clone)]
struct RouteStep {
    pool: usize,
    token_in: usize,
    token_out: usize,
}

#[derive(Debug, Clone)]
struct Route {
    steps: Vec<RouteStep>,
}

type PoolsDelta = HashMap<usize, Pool>;

impl Route {
    fn emulate(
        &self,
        pools: &Pools,
        amount: QuoteAmount,
        pools_delta: &mut PoolsDelta,
    ) -> SwapResult<Balance> {
        let mut current_amount = amount.value();
        match amount {
            QuoteAmount::ExactIn(_) => {
                for step in &self.steps {
                    let mut pool = pools_delta
                        .get(&step.pool)
                        .unwrap_or(&pools.pools[step.pool])
                        .clone();
                    current_amount =
                        pool.swap_exact_in(&pools.tokens[step.token_in], current_amount)?;
                    pools_delta.insert(step.pool, pool);
                }
            }
            QuoteAmount::ExactOut(_) => {
                for step in self.steps.iter().rev() {
                    let mut pool = pools_delta
                        .get(&step.pool)
                        .unwrap_or(&pools.pools[step.pool])
                        .clone();
                    current_amount =
                        pool.swap_exact_out(&pools.tokens[step.token_out], current_amount)?;
                    pools_delta.insert(step.pool, pool);
                }
            }
        }
        Ok(current_amount)
    }

    fn api_steps(&self, pools: &Pools) -> Vec<ApiPoolStep> {
        self.steps
            .iter()
            .map(|step| ApiPoolStep {
                pool_id: pools.pools[step.pool].data.id.clone(),
                token_in: pools.tokens[step.token_in].clone(),
                token_out: pools.tokens[step.token_out].clone(),
            })
            .collect()
    }
}

impl Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Route:")?;
        for step in &self.steps {
            write!(f, " --- ({}) --->", step.pool)?;
        }
        Ok(())
    }
}

fn best_hop(
    pools: &Pools,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
) -> Option<(usize, Balance)> {
    let mut best: Option<(usize, Balance)> = None;
    for &pool_index in pools.pair_pools(token_in, token_out) {
        let mut pool = pools.pools[pool_index].clone();
        let result = match amount {
            QuoteAmount::ExactIn(amount_in) => {
                pool.swap_exact_in(&pools.tokens[token_in], amount_in)
            }
            QuoteAmount::ExactOut(amount_out) => {
                pool.swap_exact_out(&pools.tokens[token_out], amount_out)
            }
        };
        let Ok(metric) = result else {
            continue;
        };
        if metric > 0 && best.is_none_or(|(_, best_metric)| amount.is_better(metric, best_metric)) {
            best = Some((pool_index, metric));
        }
    }
    best
}

struct PathFinder<'a> {
    pools: &'a Pools,
    amount: QuoteAmount,
    edge_hops: HashMap<usize, Option<(usize, Balance)>>,
}

impl<'a> PathFinder<'a> {
    fn new(pools: &'a Pools, amount: QuoteAmount) -> Self {
        Self {
            pools,
            amount,
            edge_hops: HashMap::new(),
        }
    }

    fn path(&mut self, tokens: &[usize]) -> Option<(Route, Balance)> {
        let mut steps = Vec::with_capacity(tokens.len() - 1);
        let mut current_amount = self.amount.value();
        match self.amount {
            QuoteAmount::ExactIn(_) => {
                for (hop, pair) in tokens.windows(2).enumerate() {
                    let (pool, amount_out) = if hop == 0 {
                        let (pools, amount) = (self.pools, self.amount);
                        *self
                            .edge_hops
                            .entry(pair[1])
                            .or_insert_with(|| best_hop(pools, pair[0], pair[1], amount))
                    } else {
                        best_hop(
                            self.pools,
                            pair[0],
                            pair[1],
                            QuoteAmount::ExactIn(current_amount),
                        )
                    }?;
                    steps.push(RouteStep {
                        pool,
                        token_in: pair[0],
                        token_out: pair[1],
                    });
                    current_amount = amount_out;
                }
            }
            QuoteAmount::ExactOut(_) => {
                for (hop, pair) in tokens.windows(2).rev().enumerate() {
                    let (pool, amount_in) = if hop == 0 {
                        let (pools, amount) = (self.pools, self.amount);
                        *self
                            .edge_hops
                            .entry(pair[0])
                            .or_insert_with(|| best_hop(pools, pair[0], pair[1], amount))
                    } else {
                        best_hop(
                            self.pools,
                            pair[0],
                            pair[1],
                            QuoteAmount::ExactOut(current_amount),
                        )
                    }?;
                    steps.push(RouteStep {
                        pool,
                        token_in: pair[0],
                        token_out: pair[1],
                    });
                    current_amount = amount_in;
                }
                steps.reverse();
            }
        }
        Some((Route { steps }, current_amount))
    }
}

fn find_routes(
    pools: &Pools,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
    max_hops: MaxHops,
) -> Vec<(Route, Balance)> {
    let mut routes = Vec::new();
    // Every pool of the pair is a candidate
    for &pool in pools.pair_pools(token_in, token_out) {
        let route = Route {
            steps: vec![RouteStep {
                pool,
                token_in,
                token_out,
            }],
        };
        if let Ok(metric) = route.emulate(pools, amount, &mut PoolsDelta::new())
            && metric > 0
        {
            routes.push((route, metric));
        }
    }
    if max_hops == MaxHops::DirectOnly {
        return routes;
    }

    let mut path_finder = PathFinder::new(pools, amount);
    let out_neighbors = pools.neighbors[token_out]
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    for &middle in &pools.neighbors[token_in] {
        if middle != token_out && out_neighbors.contains(&middle) {
            routes.extend(path_finder.path(&[token_in, middle, token_out]));
        }
    }
    if max_hops == MaxHops::Three {
        for &first in &pools.neighbors[token_in] {
            if first == token_out {
                continue;
            }
            for &second in &pools.neighbors[token_out] {
                if second != token_in && second != first && pools.has_pair(first, second) {
                    routes.extend(path_finder.path(&[token_in, first, second, token_out]));
                }
            }
        }
    }
    routes
}

// Only one route: DCL's Swap / SwapByOutput take a single path, and splitting the amount between
// several ft_transfer_calls wouldn't be atomic
fn route(
    pools: &Pools,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
    max_hops: MaxHops,
) -> Result<Route, anyhow::Error> {
    let now = Instant::now();
    let routes = find_routes(pools, token_in, token_out, amount, max_hops);
    info!("Found {} routes in {:?}", routes.len(), now.elapsed());
    let best_route = match amount {
        QuoteAmount::ExactIn(_) => routes.into_iter().max_by_key(|(_, metric)| *metric),
        QuoteAmount::ExactOut(_) => routes.into_iter().min_by_key(|(_, metric)| *metric),
    };
    let Some((best_route, best_route_metric)) = best_route else {
        return Err(anyhow::anyhow!("No routes found"));
    };
    info!("Best route: {best_route} {best_route_metric}");
    Ok(best_route)
}

fn apply_slippage(amount: Balance, numerator: u128) -> SwapResult<Balance> {
    as_u128(mul_div_floor(
        U512::from(amount),
        U512::from(numerator),
        U512::from(SLIPPAGE_DENOMINATOR),
    )?)
}

fn to_api_response(
    route: &Route,
    pools: &Pools,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
    slippage_bp: u128,
) -> Result<RouteApiResponse, anyhow::Error> {
    match amount {
        QuoteAmount::ExactIn(amount_in) => {
            let amount_out = route.emulate(pools, amount, &mut PoolsDelta::new())?;
            Ok(RouteApiResponse::ExactIn {
                route: ExactInRoute {
                    pools: route.api_steps(pools),
                    amount_in: U128(amount_in),
                    min_amount_out: U128(apply_slippage(
                        amount_out,
                        SLIPPAGE_DENOMINATOR - slippage_bp,
                    )?),
                    amount_out: U128(amount_out),
                },
                contract_in: pools.tokens[token_in].clone(),
                contract_out: pools.tokens[token_out].clone(),
                amount_in: U128(amount_in),
                amount_out: U128(amount_out),
            })
        }
        QuoteAmount::ExactOut(amount_out) => {
            let amount_in = route.emulate(pools, amount, &mut PoolsDelta::new())?;
            let max_amount_in = apply_slippage(amount_in, SLIPPAGE_DENOMINATOR + slippage_bp)?;
            Ok(RouteApiResponse::ExactOut {
                route: ExactOutRoute {
                    pools: route.api_steps(pools),
                    amount_in: U128(amount_in),
                    max_amount_in: U128(max_amount_in),
                    amount_out: U128(amount_out),
                },
                contract_in: pools.tokens[token_in].clone(),
                contract_out: pools.tokens[token_out].clone(),
                amount_in: U128(amount_in),
                max_amount_in: U128(max_amount_in),
                amount_out: U128(amount_out),
            })
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindPathQuery {
    #[serde(default, with = "dec_format")]
    amount_in: Option<Balance>,
    #[serde(default, with = "dec_format")]
    amount_out: Option<Balance>,
    token_in: AccountId,
    token_out: AccountId,
    max_hops: MaxHops,
    #[serde(deserialize_with = "deserialize_bigdecimal")]
    slippage: BigDecimal,
}

fn deserialize_bigdecimal<'de, D>(deserializer: D) -> Result<BigDecimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as Deserialize>::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn error_reply(result_code: i32, result_message: String) -> warp::reply::Json {
    let resp: ApiResponse<()> = ApiResponse {
        result_code,
        result_message,
        result_data: None,
    };
    warp::reply::json(&resp)
}

async fn handle_find_path(query: FindPathQuery) -> Result<impl warp::Reply, warp::Rejection> {
    let request_id = rand::thread_rng().gen_range(1..1000000);
    info!(
        "Received request: request_id={:06}, amount_in={:?}, amount_out={:?}, token_in={}, token_out={}, max_hops={:?}, slippage={:?}",
        request_id,
        query.amount_in,
        query.amount_out,
        query.token_in,
        query.token_out,
        query.max_hops,
        query.slippage
    );

    let amount = match (query.amount_in, query.amount_out) {
        (Some(amount_in), None) => QuoteAmount::ExactIn(amount_in),
        (None, Some(amount_out)) => QuoteAmount::ExactOut(amount_out),
        _ => {
            return Ok(error_reply(
                RC_ROUTE_ERROR,
                "Provide either amountIn or amountOut".into(),
            ));
        }
    };
    if amount.value() == 0 || query.token_in == query.token_out {
        return Ok(error_reply(
            RC_ROUTE_ERROR,
            "Amount must be positive and tokens must be different".into(),
        ));
    }

    if query.slippage < 0 || query.slippage > 1 {
        return Ok(error_reply(RC_INVALID_SLIPPAGE, "Invalid slippage".into()));
    }
    let slippage_bp = (query.slippage * BigDecimal::from_u32(10_000).unwrap())
        .with_scale_round(0, RoundingMode::Down)
        .to_u128()
        .unwrap();

    let pools = match get_pools().await {
        Ok(pools) => pools,
        Err(e) => return Ok(error_reply(RC_POOL_FETCH_ERROR, e.to_string())),
    };
    let (Some(&token_in), Some(&token_out)) = (
        pools.token_ids.get(&query.token_in),
        pools.token_ids.get(&query.token_out),
    ) else {
        return Ok(error_reply(RC_ROUTE_ERROR, "No routes found".into()));
    };

    // Emulating swaps is CPU-bound
    let max_hops = query.max_hops;
    let result = tokio::task::spawn_blocking(move || {
        let best_route = route(&pools, token_in, token_out, amount, max_hops)
            .map_err(|e| (RC_ROUTE_ERROR, e))?;
        to_api_response(
            &best_route,
            &pools,
            token_in,
            token_out,
            amount,
            slippage_bp,
        )
        .map_err(|e| (RC_RESPONSE_BUILD_ERROR, e))
    })
    .await
    .unwrap_or_else(|e| Err((RC_ROUTE_ERROR, e.into())));

    match result {
        Ok(data) => {
            match &data {
                RouteApiResponse::ExactIn { amount_out, .. } => info!(
                    "Request id: {:06}, Estimated amount out: {}",
                    request_id, amount_out.0
                ),
                RouteApiResponse::ExactOut { amount_in, .. } => info!(
                    "Request id: {:06}, Estimated amount in: {}",
                    request_id, amount_in.0
                ),
            }
            let resp = ApiResponse {
                result_code: RC_SUCCESS,
                result_message: "".into(),
                result_data: Some(data),
            };
            Ok(warp::reply::json(&resp))
        }
        Err((result_code, e)) => Ok(error_reply(result_code, e.to_string())),
    }
}
