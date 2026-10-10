#![allow(clippy::manual_div_ceil)]

mod storage;

pub use self::storage::watch;

use bigdecimal::BigDecimal;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, NearGas, U128};
use near_min_api::utils::dec_format;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

use pool_indexer::{AccountState, PoolIndexer};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use rhea_dcl_math::math::{SwapResult, U512, as_u128, mul_div_floor};
use rhea_dcl_math::pool::{Pool, PoolInit, Work};

use crate::search::{self, Graph, MaxHops, PoolsDelta, QuoteAmount, Route, Settings};

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "quote_type", rename_all = "snake_case")]
pub enum RouteApiResponse {
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

#[derive(Serialize, Debug, Clone)]
pub struct ExactInRoute {
    pub pools: Vec<ApiPoolStep>,
    pub amount_in: U128,
    pub min_amount_out: U128,
    pub amount_out: U128,
    pub swap_gas: NearGas,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactOutRoute {
    // In path order, SwapByOutput takes them in reverse order
    pub pools: Vec<ApiPoolStep>,
    pub amount_in: U128,
    pub max_amount_in: U128,
    pub amount_out: U128,
    pub swap_gas: NearGas,
}

#[derive(Serialize, Debug, Clone)]
pub struct ApiPoolStep {
    pub pool_id: String,
    pub token_in: AccountId,
    pub token_out: AccountId,
}

const RHEA_DCL_CONTRACT_ID: &str = "dclv2.ref-labs.near";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);
const SLIPPAGE_DENOMINATOR: u128 = 10_000;

#[derive(Serialize, Deserialize)]
struct Metadata {
    state: String,
    #[serde(with = "dec_format")]
    pool_count: u64,
    protocol_fee_rate: u32,
}

#[derive(Serialize, Deserialize)]
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
    whitelist: Option<Vec<AccountId>>,
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

pub struct Pools {
    graph: Graph<Pool>,
    block_height: BlockHeight,
    updated_at: Instant,
}

impl Pools {
    fn new(mut pools: Vec<Pool>, block_height: BlockHeight) -> Self {
        // Fixed order, so that routes with equal output are always picked the same way
        pools.sort_by(|a, b| a.data.id.cmp(&b.data.id));
        Self {
            graph: Graph::new(pools),
            block_height,
            updated_at: Instant::now(),
        }
    }
}

impl search::Pool for Pool {
    type Token = AccountId;

    fn tokens(&self) -> Vec<AccountId> {
        vec![self.data.token_x.clone(), self.data.token_y.clone()]
    }

    fn swap_exact_in(
        &mut self,
        token_in: usize,
        _token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        let data = Arc::clone(&self.data);
        let token_in = if token_in == 0 {
            &data.token_x
        } else {
            &data.token_y
        };
        Pool::swap_exact_in(self, token_in, amount_in).ok()
    }

    fn swap_exact_out(
        &mut self,
        _token_in: usize,
        token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance> {
        let data = Arc::clone(&self.data);
        let token_out = if token_out == 0 {
            &data.token_x
        } else {
            &data.token_y
        };
        Pool::swap_exact_out(self, token_out, amount_out).ok()
    }
}

/// Pools that are not running or have nothing to trade are not loaded
fn is_tradable(info: &PoolInfo) -> bool {
    let has_liquidity_or_orders = [
        &info.total_liquidity,
        &info.total_order_x,
        &info.total_order_y,
    ]
    .iter()
    .any(|amount| *amount != "0");
    info.state == "Running" && has_liquidity_or_orders && info.whitelist.is_none()
}

/// Builds a pool from liquidity segments (left, right, liquidity) and orders (point, selling x,
/// selling y), as `get_liquidity_range` and `get_pointorder_range` return them
fn build_pool(
    info: PoolInfo,
    segments: impl IntoIterator<Item = (i32, i32, Balance)>,
    orders: impl IntoIterator<Item = (i32, Balance, Balance)>,
    protocol_fee_rate: u32,
) -> Result<Pool, anyhow::Error> {
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
            protocol_fee_rate,
        },
        segments,
        orders,
    )
}

/// Pools that can be routed through, given the contract state
fn active_pools(
    pools: impl IntoIterator<Item = Pool>,
    metadata: &Metadata,
    frozen_tokens: Vec<AccountId>,
) -> Vec<Pool> {
    if metadata.state == "Running" {
        let frozen_tokens = frozen_tokens.into_iter().collect::<HashSet<_>>();
        pools
            .into_iter()
            .filter(|pool| {
                !frozen_tokens.contains(&pool.data.token_x)
                    && !frozen_tokens.contains(&pool.data.token_y)
            })
            .map(|mut pool| {
                pool.set_protocol_fee_rate(metadata.protocol_fee_rate);
                pool
            })
            .collect()
    } else {
        warn!("{RHEA_DCL_CONTRACT_ID} is {}", metadata.state);
        Vec::new()
    }
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    let block_height = state.block.height;
    let (metadata, frozen_tokens) = storage::contract(state)?;
    let mut built = Vec::new();
    for info in storage::pools(state, metadata.pool_count)? {
        if !is_tradable(&info) {
            continue;
        }
        let pool_id = info.pool_id.clone();
        let point_data = storage::point_data(state, &pool_id, info.current_point)?;
        match build_pool(
            info,
            point_data.segments,
            point_data.orders,
            metadata.protocol_fee_rate,
        ) {
            Ok(pool) => built.push(pool),
            // Pool::new checks that liquidity at the current point matches the pool
            Err(e) => error!("Failed to load pool {pool_id} at block {block_height}: {e}"),
        }
    }
    Ok(Pools::new(
        active_pools(built, &metadata, frozen_tokens),
        block_height,
    ))
}

/// Builds pools from indexed storage after every block, returns why it stopped
pub async fn update_pools(indexer: PoolIndexer) -> anyhow::Error {
    let account_id: AccountId = RHEA_DCL_CONTRACT_ID.parse().unwrap();
    let mut blocks = indexer.blocks();
    loop {
        if blocks.changed().await.is_err() {
            return anyhow::anyhow!("Pool indexer stopped");
        }
        let Some(state) = indexer.account_state(&account_id) else {
            continue;
        };
        let height = state.block.height;
        match tokio::task::spawn_blocking(move || build_indexed_pools(&state)).await {
            Ok(Ok(pools)) => {
                *POOLS_CACHE.write().await = Some(Arc::new(pools));
                crate::pools_built(crate::PoolsDex::Dcl, height);
            }
            Ok(Err(e)) => {
                return e.context(format!("Failed to build Rhea DCL pools at block {height}"));
            }
            Err(e) => {
                return anyhow::anyhow!("Building Rhea DCL pools at block {height} panicked: {e}");
            }
        }
    }
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

// Only one route: DCL's Swap / SwapByOutput take a single path, and splitting the amount between
// several ft_transfer_calls wouldn't be atomic
const SETTINGS: Settings = Settings {
    top_routes: 5,
    max_splits: 1,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

fn apply_slippage(amount: Balance, numerator: u128) -> SwapResult<Balance> {
    as_u128(mul_div_floor(
        U512::from(amount),
        U512::from(numerator),
        U512::from(SLIPPAGE_DENOMINATOR),
    )?)
}

const HOP_GAS: NearGas = NearGas::from_ggas(273 + 2_855);

/// Gas the contract burns for the swaps of a route in the receipt that swaps, from the steps the
/// swaps take. Fitted to fuzzing of random quotes on mainnet state.
fn swap_gas(route: &Route, pools_delta: &PoolsDelta<Pool>) -> NearGas {
    let work = route
        .hops
        .iter()
        .map(|hop| pools_delta.get(&hop.pool).unwrap().work())
        .fold(Work::default(), |total, work| Work {
            ranges: total.ranges + work.ranges,
            crossings: total.crossings + work.crossings,
            orders: total.orders + work.orders,
            words: total.words + work.words,
        });
    NearGas::from_ggas(
        7_050
            + 273 * route.hops.len() as u64
            + 2_855 * work.ranges
            + 511 * work.crossings
            + 4_075 * work.orders
            + 54 * work.words,
    )
}

fn to_api_response(
    route: &Route,
    graph: &Graph<Pool>,
    token_in: usize,
    token_out: usize,
    amount: QuoteAmount,
    slippage_bp: u128,
) -> Result<RouteApiResponse, anyhow::Error> {
    let pools = route
        .hops
        .iter()
        .map(|hop| ApiPoolStep {
            pool_id: graph.pools()[hop.pool].data.id.clone(),
            token_in: graph.token(hop.token_in).clone(),
            token_out: graph.token(hop.token_out).clone(),
        })
        .collect();
    match amount {
        QuoteAmount::ExactIn(amount_in) => {
            let mut pools_delta = PoolsDelta::default();
            let amount_out = route
                .emulate_exact_in(graph, amount_in, &mut pools_delta)
                .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
            Ok(RouteApiResponse::ExactIn {
                route: ExactInRoute {
                    pools,
                    amount_in: U128(amount_in),
                    min_amount_out: U128(apply_slippage(
                        amount_out,
                        SLIPPAGE_DENOMINATOR - slippage_bp,
                    )?),
                    amount_out: U128(amount_out),
                    swap_gas: swap_gas(route, &pools_delta),
                },
                contract_in: graph.token(token_in).clone(),
                contract_out: graph.token(token_out).clone(),
                amount_in: U128(amount_in),
                amount_out: U128(amount_out),
            })
        }
        QuoteAmount::ExactOut(amount_out) => {
            let mut pools_delta = PoolsDelta::default();
            let (amount_in, _) = route
                .emulate_exact_out(graph, amount_out, &mut pools_delta)
                .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
            let max_amount_in = apply_slippage(amount_in, SLIPPAGE_DENOMINATOR + slippage_bp)?;
            Ok(RouteApiResponse::ExactOut {
                route: ExactOutRoute {
                    pools,
                    amount_in: U128(amount_in),
                    max_amount_in: U128(max_amount_in),
                    amount_out: U128(amount_out),
                    swap_gas: swap_gas(route, &pools_delta),
                },
                contract_in: graph.token(token_in).clone(),
                contract_out: graph.token(token_out).clone(),
                amount_in: U128(amount_in),
                max_amount_in: U128(max_amount_in),
                amount_out: U128(amount_out),
            })
        }
    }
}

pub struct Request {
    pub token_in: AccountId,
    pub token_out: AccountId,
    pub amount: QuoteAmount,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
    pub near_price_raw: BigDecimal,
    pub quoted_token_price_raw: BigDecimal,
}

/// Finds a route on the newest pools
pub async fn find_path(request: Request) -> Result<RouteApiResponse, anyhow::Error> {
    let pools = get_pools().await?;
    tokio::task::spawn_blocking(move || find_path_in(&pools, &request)).await?
}

/// Finds a route on `pools`. CPU-bound, emulates swaps.
pub fn find_path_in(pools: &Pools, request: &Request) -> Result<RouteApiResponse, anyhow::Error> {
    if request.amount.value() == 0 || request.token_in == request.token_out {
        anyhow::bail!("Amount must be positive and tokens must be different");
    }
    let slippage_bp = crate::slippage_bp(&request.slippage)?;
    let graph = &pools.graph;
    let (Some(token_in), Some(token_out)) = (
        graph.token_index(&request.token_in),
        graph.token_index(&request.token_out),
    ) else {
        anyhow::bail!("No routes found");
    };

    let split_route = search::route(
        graph,
        token_in,
        token_out,
        request.amount,
        request.max_hops,
        SETTINGS,
        crate::gas_cost(
            HOP_GAS,
            &request.near_price_raw,
            &request.quoted_token_price_raw,
        )?,
    )?;
    let data = to_api_response(
        &split_route.parts[0].route,
        graph,
        token_in,
        token_out,
        request.amount,
        slippage_bp,
    )?;

    match &data {
        RouteApiResponse::ExactIn { amount_out, .. } => info!(
            "Rhea DCL route from {} to {}: {} out, block {}",
            request.token_in, request.token_out, amount_out.0, pools.block_height
        ),
        RouteApiResponse::ExactOut { amount_in, .. } => info!(
            "Rhea DCL route from {} to {}: {} in, block {}",
            request.token_in, request.token_out, amount_in.0, pools.block_height
        ),
    }
    Ok(data)
}
