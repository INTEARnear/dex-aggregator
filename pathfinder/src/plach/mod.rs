mod storage;

use self::storage::StoredPool;
pub use self::storage::watch;

use bigdecimal::BigDecimal;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, NearGas, U128};
use near_min_api::utils::dec_format;

use std::collections::HashMap;
use std::num::NonZeroU128;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

pub use intear_dex_types::AssetId;
use near_sdk::Timestamp;
use pool_indexer::{AccountState, PoolIndexer};
use xyk_dex_types::math::{
    exact_in_amount_out, exact_out_amount_in, fee_converted_to_quote_asset, mul_div,
};
use xyk_dex_types::{
    CurrentFees, FULL_SPLIT_FRACTION, FeeConfiguration, FeeFractionFraction, FeeReceiver,
    SplitFraction, asset_account_ids,
};

use crate::search::{
    self, Graph, MaxHops, PoolsDelta, QuoteAmount, Settings, SplitRoute, split_exactly,
};
use serde::Serialize;
use tracing::{info, warn};

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "quote_type", rename_all = "snake_case")]
pub enum SplitRouteApiResponse {
    ExactIn {
        routes: Vec<ExactInRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        amount_in: U128,
        amount_out: U128,
        /// Added to `amount_out` by the fee discount, 0 without one
        discount: U128,
        swap_gas: NearGas,
    },
    ExactOut {
        routes: Vec<ExactOutRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        amount_in: U128,
        max_amount_in: U128,
        amount_out: U128,
        /// Taken off `amount_in` by the fee discount, 0 without one
        discount: U128,
        swap_gas: NearGas,
    },
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactInRoute {
    pub pools: Vec<ExactInPoolStep>,
    pub split_fraction: SplitFraction,
    pub amount_in: U128,
    pub min_amount_out: U128,
    pub amount_out: U128,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactOutRoute {
    pub pools: Vec<ExactOutPoolStep>,
    pub split_fraction: SplitFraction,
    pub amount_in: U128,
    pub max_amount_in: U128,
    pub amount_out: U128,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactInPoolStep {
    #[serde(with = "dec_format")]
    pub pool_id: u32,
    pub token_in: AssetId,
    pub token_out: AssetId,
    pub amount_in: U128,
    pub min_amount_out: U128,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactOutPoolStep {
    #[serde(with = "dec_format")]
    pub pool_id: u32,
    pub token_in: AssetId,
    pub token_out: AssetId,
    pub amount_in: U128,
    pub amount_out: U128,
    pub max_amount_in: U128,
}

const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);
const BASIS_POINTS: NonZeroU128 = NonZeroU128::new(10_000).unwrap();

#[derive(Clone)]
pub struct Pool {
    id: u32,
    reserves: [Balance; 2],
    /// What swaps don't change, shared so simulations clone pools without allocating
    info: Arc<PoolInfo>,
}

#[derive(Clone)]
struct PoolInfo {
    tokens: [AssetId; 2],
    /// Pool and protocol fees at the block time
    fees: CurrentFees,
    /// Swaps fail if they leave less than this of a token. Launch pools keep their phantom
    /// liquidity of the quote token.
    min_reserves: [Balance; 2],
    /// Which token a launch pool converts its fees to
    launch_quote_token: Option<usize>,
}

impl Pool {
    /// `None` for a pool of NEP-171 assets, which the contract can't swap in
    fn new(id: u32, data: StoredPool, timestamp: Timestamp) -> Option<Self> {
        let launch_quote_token = match &data {
            StoredPool::LaunchV1 { .. } | StoredPool::LaunchV2 { .. } => Some(0),
            StoredPool::PrivateV1 { .. }
            | StoredPool::PublicV1 { .. }
            | StoredPool::PrivateV2 { .. }
            | StoredPool::PublicV2 { .. } => None,
        };
        let (tokens, reserves, fee_configuration, min_reserves) = match data {
            StoredPool::PrivateV1 { assets, fees, .. }
            | StoredPool::PublicV1 { assets, fees, .. } => (
                [assets.0.asset_id, assets.1.asset_id],
                [assets.0.balance.0, assets.1.balance.0],
                FeeConfiguration::V1(fees),
                [0, 0],
            ),
            StoredPool::PrivateV2 { assets, fees, .. }
            | StoredPool::PublicV2 { assets, fees, .. } => (
                [assets.0.asset_id, assets.1.asset_id],
                [assets.0.balance.0, assets.1.balance.0],
                fees,
                [0, 0],
            ),
            StoredPool::LaunchV1 {
                near_amount,
                launched_asset,
                fees,
                phantom_liquidity_near,
            } => (
                [AssetId::Near, launched_asset.asset_id],
                [near_amount.0, launched_asset.balance.0],
                fees,
                [phantom_liquidity_near.0, 0],
            ),
            StoredPool::LaunchV2 {
                quote_asset,
                launched_asset,
                fees,
                phantom_liquidity,
            } => (
                [quote_asset.asset_id, launched_asset.asset_id],
                [quote_asset.balance.0, launched_asset.balance.0],
                fees,
                [phantom_liquidity.0, 0],
            ),
        };
        if tokens
            .iter()
            .any(|token| matches!(token, AssetId::Nep171(..)))
        {
            return None;
        }
        let fees = fee_configuration
            .with_protocol_fee(&asset_account_ids([&tokens[0], &tokens[1]]), timestamp);
        Some(Self {
            id,
            reserves,
            info: Arc::new(PoolInfo {
                tokens,
                fees,
                min_reserves,
                launch_quote_token,
            }),
        })
    }

    /// The pool with its pool and protocol fees discounted, as a signed fee discount makes the
    /// xyk DEX charge them
    fn with_fee_discount(&self, fee_discount: FeeFractionFraction) -> Self {
        if fee_discount == 0 {
            return self.clone();
        }
        let fees = self.info.fees.clone().discounted(fee_discount);
        Self {
            info: Arc::new(PoolInfo {
                fees,
                ..(*self.info).clone()
            }),
            ..self.clone()
        }
    }
}

struct CollectedFees {
    amount_in_after_fees: u128,
    /// What the pool keeps of the fees, added to its input reserve
    pool_fee: u128,
}

/// One side of a pool swap: input and output reserves and the fees on it
struct Swap<'a> {
    in_balance: &'a mut Balance,
    out_balance: &'a mut Balance,
    fees: &'a CurrentFees,
    /// Swaps fail if they leave less than this of the output token
    min_out_balance: Balance,
    /// Whether it's a launch pool selling into its quote token, which converts the fees it
    /// doesn't keep to it
    converts_fees: bool,
}

impl Swap<'_> {
    fn collect_fees(&self, amount_in: u128) -> CollectedFees {
        let mut total_fees = 0u128;
        let mut pool_fee = 0u128;
        for (receiver, fee_amount) in self.fees.amounts(amount_in) {
            total_fees = total_fees.checked_add(fee_amount).expect("Overflow");
            if matches!(receiver, FeeReceiver::Pool) {
                pool_fee = pool_fee.checked_add(fee_amount).expect("Overflow");
            }
        }
        CollectedFees {
            amount_in_after_fees: amount_in.checked_sub(total_fees).expect("Fee exceeds 100%"),
            pool_fee,
        }
    }

    /// Moves the reserves like the contract after a swap: the pool keeps its fee, and a launch
    /// pool selling into its quote token then swaps each other fee to it, in receiver order.
    /// `None` if that leaves too little of the output token.
    fn settle(self, amount_in: u128, fees: CollectedFees, amount_out: u128) -> Option<()> {
        *self.in_balance = self
            .in_balance
            .checked_add(fees.amount_in_after_fees)
            .expect("Overflow")
            .checked_add(fees.pool_fee)
            .expect("Overflow");
        *self.out_balance = self.out_balance.checked_sub(amount_out).expect("Underflow");
        if self.converts_fees {
            for (receiver, fee_amount) in self.fees.amounts(amount_in) {
                if matches!(receiver, FeeReceiver::Pool) {
                    continue;
                }
                let fee_amount_out =
                    fee_converted_to_quote_asset(fee_amount, *self.in_balance, *self.out_balance);
                *self.in_balance = self.in_balance.checked_add(fee_amount).expect("Overflow");
                *self.out_balance = self
                    .out_balance
                    .checked_sub(fee_amount_out)
                    .expect("Underflow");
            }
        }
        (*self.out_balance >= self.min_out_balance).then_some(())
    }
}

impl Pool {
    /// The swap putting in token 0 if `first_in`, else token 1. `None` for a swap the contract
    /// fails: out of an empty pool, or with a community fee collected in another asset than NEAR.
    fn swap(&mut self, first_in: bool) -> Option<Swap<'_>> {
        let (token_in, token_out) = if first_in { (0, 1) } else { (1, 0) };
        if self.reserves.contains(&0) {
            return None;
        }
        let converts_fees = self.info.launch_quote_token == Some(token_out);
        let fees = &self.info.fees;
        let fee_token = if converts_fees { token_out } else { token_in };
        if self.info.tokens[fee_token] != AssetId::Near
            && fees
                .receivers
                .iter()
                .any(|(receiver, _)| matches!(receiver, FeeReceiver::Community(_)))
        {
            return None;
        }
        let [balance0, balance1] = &mut self.reserves;
        let (in_balance, out_balance) = if first_in {
            (balance0, balance1)
        } else {
            (balance1, balance0)
        };
        Some(Swap {
            in_balance,
            out_balance,
            fees,
            min_out_balance: self.info.min_reserves[token_out],
            converts_fees,
        })
    }

    /// Simulations panic on overflows like the contract
    fn unless_panicked(
        &mut self,
        swap: impl FnOnce(&mut Self) -> Option<Balance>,
    ) -> Option<Balance> {
        let id = self.id;
        std::panic::catch_unwind(AssertUnwindSafe(|| swap(self))).unwrap_or_else(|_| {
            warn!("Panicked while emulating a swap in pool {id}");
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
            let swap = pool.swap(token_in == 0)?;
            let fees = swap.collect_fees(amount_in);
            let amount_out = exact_in_amount_out(
                fees.amount_in_after_fees,
                *swap.in_balance,
                *swap.out_balance,
            );
            swap.settle(amount_in, fees, amount_out)?;
            Some(amount_out)
        })
    }

    fn swap_exact_out(
        &mut self,
        token_in: usize,
        _token_out: usize,
        amount_out: Balance,
    ) -> Option<Balance> {
        self.unless_panicked(|pool| {
            if amount_out == 0 {
                return None;
            }
            let swap = pool.swap(token_in == 0)?;
            let (in_balance, out_balance) = (*swap.in_balance, *swap.out_balance);
            if amount_out >= out_balance || out_balance - amount_out < swap.min_out_balance {
                return None;
            }
            let amount_in = exact_out_amount_in(amount_out, in_balance, out_balance, swap.fees);
            let fees = swap.collect_fees(amount_in);
            swap.settle(amount_in, fees, amount_out)?;
            Some(amount_in)
        })
    }
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    /// Pools without a fee discount
    graph: Arc<Graph<Pool>>,
    /// Pools with a fee discount, made the first time it's asked for. A snapshot only lives for
    /// a block, so this only holds the ones asked for in it.
    discounted_graphs: Mutex<HashMap<FeeFractionFraction, Arc<Graph<Pool>>>>,
}

impl Pools {
    /// Pools as a swap with `fee_discount` goes through them
    fn graph_for(&self, fee_discount: FeeFractionFraction) -> Arc<Graph<Pool>> {
        if fee_discount == 0 {
            return self.graph.clone();
        }
        self.discounted_graphs
            .lock()
            .unwrap()
            .entry(fee_discount)
            .or_insert_with(|| {
                Arc::new(
                    self.graph
                        .map_pools(|pool| pool.with_fee_discount(fee_discount)),
                )
            })
            .clone()
    }
}

/// Everything a pools snapshot is built from, as stored by the contract at one block
struct PoolsSource {
    block_height: BlockHeight,
    /// Of the block, which scheduled fees depend on
    timestamp: Timestamp,
    pools: Vec<StoredPool>,
}

fn build_pools(source: PoolsSource) -> Pools {
    let PoolsSource {
        block_height,
        timestamp,
        pools,
    } = source;
    let pools = pools
        .into_iter()
        .enumerate()
        .filter_map(|(i, data)| Pool::new(i as u32, data, timestamp))
        .collect::<Vec<_>>();
    Pools {
        block_height,
        updated_at: Instant::now(),
        graph: Arc::new(Graph::new(pools)),
        discounted_graphs: Mutex::default(),
    }
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    storage::pools_source(state).map(build_pools)
}

/// Builds pools from indexed storage after every block, returns why it stopped
pub async fn update_pools(indexer: PoolIndexer) -> anyhow::Error {
    let account_id: AccountId = INTEAR_DEX_CONTRACT_ID.parse().unwrap();
    let mut blocks = indexer.blocks();
    loop {
        if blocks.changed().await.is_err() {
            return anyhow::anyhow!("Pool indexer stopped");
        }
        let Some(state) = indexer.account_state(&account_id) else {
            continue;
        };
        let height = state.block.height;
        let pools = tokio::task::spawn_blocking(move || build_indexed_pools(&state)).await;
        match pools {
            Ok(Ok(pools)) => {
                *POOLS_CACHE.write().await = Some(Arc::new(pools));
                crate::pools_built(crate::PoolsDex::Plach, height);
            }
            Ok(Err(e)) => {
                return e.context(format!("Failed to build Plach pools at block {height}"));
            }
            Err(e) => {
                return anyhow::anyhow!("Building Plach pools at block {height} panicked: {e}");
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

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 8,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

const SWAP_BASE_GAS: NearGas = NearGas::from_tgas(23);
const HOP_GAS: NearGas = NearGas::from_tgas(2);

/// Gas the DEX burns for `split_route`
fn swap_gas(split_route: &SplitRoute) -> NearGas {
    let hops = split_route
        .parts
        .iter()
        .map(|part| part.route.hops.len())
        .sum::<usize>();
    NearGas::from_gas(SWAP_BASE_GAS.as_gas() + HOP_GAS.as_gas() * hops as u64)
}

fn to_api_response(
    split_route: &SplitRoute,
    graph: &Graph<Pool>,
    token_in: &AssetId,
    token_out: &AssetId,
    amount: QuoteAmount,
    slippage_bp: u128,
) -> Result<SplitRouteApiResponse, anyhow::Error> {
    match amount {
        QuoteAmount::ExactIn(total_amount_in) => {
            let mut routes_resp = Vec::new();
            let mut total_estimated_out: Balance = 0;
            let mut pools_delta = PoolsDelta::default();
            let amount_parts = split_exactly(total_amount_in, &split_route.parts);

            for (part, amount_part) in split_route.parts.iter().zip(amount_parts) {
                let estimated_out = part
                    .route
                    .emulate_exact_in(graph, amount_part, &mut pools_delta)
                    .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
                let min_amount_out = mul_div(estimated_out, 10_000 - slippage_bp, BASIS_POINTS);

                let mut pools_resp = Vec::new();
                let mut current_token = token_in.clone();
                for (idx, hop) in part.route.hops.iter().enumerate() {
                    let is_first = idx == 0;
                    let is_last = idx + 1 == part.route.hops.len();

                    pools_resp.push(ExactInPoolStep {
                        pool_id: graph.pools()[hop.pool].id,
                        token_in: current_token.clone(),
                        token_out: graph.token(hop.token_out).clone(),
                        amount_in: U128(if is_first { amount_part } else { 0 }),
                        min_amount_out: U128(if is_last { min_amount_out } else { 0 }),
                    });

                    current_token = graph.token(hop.token_out).clone();
                }

                routes_resp.push(ExactInRoute {
                    pools: pools_resp,
                    split_fraction: part.weight_percentage * (FULL_SPLIT_FRACTION / 100),
                    amount_in: U128(amount_part),
                    min_amount_out: U128(min_amount_out),
                    amount_out: U128(estimated_out),
                });
                total_estimated_out = total_estimated_out
                    .checked_add(estimated_out)
                    .ok_or_else(|| anyhow::anyhow!("Total estimated out overflows u128"))?;
            }
            Ok(SplitRouteApiResponse::ExactIn {
                routes: routes_resp,
                contract_in: token_in.clone(),
                contract_out: token_out.clone(),
                amount_in: U128(total_amount_in),
                amount_out: U128(total_estimated_out),
                discount: U128(0),
                swap_gas: swap_gas(split_route),
            })
        }
        QuoteAmount::ExactOut(total_amount_out) => {
            let mut routes_resp = Vec::new();
            let mut total_estimated_in: Balance = 0;
            let mut total_max_amount_in: Balance = 0;
            let mut pools_delta = PoolsDelta::default();
            for (part, amount_out_part) in split_route
                .parts
                .iter()
                .zip(split_exactly(total_amount_out, &split_route.parts))
            {
                let (amount_in, step_amounts) = part
                    .route
                    .emulate_exact_out(graph, amount_out_part, &mut pools_delta)
                    .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
                let max_amount_in = mul_div(amount_in, 10_000 + slippage_bp, BASIS_POINTS);

                let mut pools_resp = Vec::new();
                let mut current_token = token_in.clone();
                for (idx, hop) in part.route.hops.iter().enumerate() {
                    let step_amount = step_amounts
                        .get(idx)
                        .ok_or_else(|| anyhow::anyhow!("Missing step amount"))?;

                    pools_resp.push(ExactOutPoolStep {
                        pool_id: graph.pools()[hop.pool].id,
                        token_in: current_token.clone(),
                        token_out: graph.token(hop.token_out).clone(),
                        amount_in: U128(step_amount.0),
                        amount_out: U128(step_amount.1),
                        max_amount_in: U128(mul_div(
                            step_amount.0,
                            10_000 + slippage_bp,
                            BASIS_POINTS,
                        )),
                    });

                    current_token = graph.token(hop.token_out).clone();
                }

                routes_resp.push(ExactOutRoute {
                    pools: pools_resp,
                    split_fraction: part.weight_percentage * (FULL_SPLIT_FRACTION / 100),
                    amount_in: U128(amount_in),
                    max_amount_in: U128(max_amount_in),
                    amount_out: U128(amount_out_part),
                });
                total_estimated_in = total_estimated_in
                    .checked_add(amount_in)
                    .ok_or_else(|| anyhow::anyhow!("Total estimated in overflows u128"))?;
                total_max_amount_in = total_max_amount_in
                    .checked_add(max_amount_in)
                    .ok_or_else(|| anyhow::anyhow!("Total max in overflows u128"))?;
            }
            Ok(SplitRouteApiResponse::ExactOut {
                routes: routes_resp,
                contract_in: token_in.clone(),
                contract_out: token_out.clone(),
                amount_in: U128(total_estimated_in),
                max_amount_in: U128(total_max_amount_in),
                amount_out: U128(total_amount_out),
                discount: U128(0),
                swap_gas: swap_gas(split_route),
            })
        }
    }
}

/// What `split_route` swaps on `graph` in total: the output for exact in, the input for exact out
fn emulate_total(
    split_route: &SplitRoute,
    graph: &Graph<Pool>,
    amount: QuoteAmount,
) -> Result<Balance, anyhow::Error> {
    let mut total: Balance = 0;
    let mut pools_delta = PoolsDelta::default();
    for (part, amount_part) in split_route
        .parts
        .iter()
        .zip(split_exactly(amount.value(), &split_route.parts))
    {
        let swapped = match amount {
            QuoteAmount::ExactIn(_) => {
                part.route
                    .emulate_exact_in(graph, amount_part, &mut pools_delta)
            }
            QuoteAmount::ExactOut(_) => part
                .route
                .emulate_exact_out(graph, amount_part, &mut pools_delta)
                .map(|(amount_in, _)| amount_in),
        }
        .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
        total = total
            .checked_add(swapped)
            .ok_or_else(|| anyhow::anyhow!("Total amount overflows u128"))?;
    }
    Ok(total)
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
    /// Taken off the pool and protocol fees of every pool by a signed fee discount, 0 for none
    pub fee_discount: FeeFractionFraction,
}

pub async fn find_path(request: Request) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let pools = get_pools().await?;
    tokio::task::spawn_blocking(move || find_path_in(&pools, &request)).await?
}

pub fn find_path_in(
    pools: &Pools,
    request: &Request,
) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let slippage_bp = crate::slippage_bp(&request.slippage)?;
    let graph = &*pools.graph_for(request.fee_discount);
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
    let mut response = to_api_response(
        &split_route,
        graph,
        &request.token_in,
        &request.token_out,
        request.amount,
        slippage_bp,
    )?;
    if request.fee_discount != 0 {
        // Compared to the same route without the fee discount
        let undiscounted = emulate_total(&split_route, &pools.graph_for(0), request.amount)?;
        let (saved, discount) = match &mut response {
            SplitRouteApiResponse::ExactIn {
                amount_out,
                discount,
                ..
            } => (amount_out.0.checked_sub(undiscounted), discount),
            SplitRouteApiResponse::ExactOut {
                amount_in,
                discount,
                ..
            } => (undiscounted.checked_sub(amount_in.0), discount),
        };
        *discount =
            U128(saved.ok_or_else(|| anyhow::anyhow!("Fee discount made the route worse"))?);
    }
    match &response {
        SplitRouteApiResponse::ExactIn { amount_out, .. } => info!(
            "Plach route from {} to {}: {} out, block {}",
            request.token_in, request.token_out, amount_out.0, pools.block_height
        ),
        SplitRouteApiResponse::ExactOut { amount_in, .. } => info!(
            "Plach route from {} to {}: {} in, block {}",
            request.token_in, request.token_out, amount_in.0, pools.block_height
        ),
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Pool as _;

    use near_sdk::json_types::U128;
    use xyk_dex_types::math::mul_div_sum;
    use xyk_dex_types::{AssetWithBalance, FULL_FEE_FRACTION, FeeFraction};

    const POOL_FEE: FeeFraction = 3_000; // 0.3%
    const PROTOCOL_FEE: FeeFraction = 1_000; // 0.1%

    fn nep141(account_id: &str) -> AssetId {
        AssetId::Nep141(account_id.parse().unwrap())
    }

    /// The pool fee, which the contract adds the protocol fee to
    fn fees() -> CurrentFees {
        CurrentFees {
            receivers: vec![(FeeReceiver::Pool, POOL_FEE)],
        }
    }

    fn asset(asset_id: AssetId, balance: Balance) -> AssetWithBalance {
        AssetWithBalance {
            asset_id,
            balance: U128(balance),
        }
    }

    fn stored_pool(token0: AssetId, token1: AssetId) -> StoredPool {
        StoredPool::PrivateV1 {
            assets: (
                asset(token0, 10u128.pow(24)),
                asset(token1, 2 * 10u128.pow(24)),
            ),
            _owner_id: "owner.near".parse().unwrap(),
            fees: fees(),
        }
    }

    fn public_pool(token0: AssetId, token1: AssetId) -> Pool {
        Pool::new(0, stored_pool(token0, token1), 0).unwrap()
    }

    fn launch_pool(launched: AssetId) -> Pool {
        launch_pool_with_phantom_liquidity(launched, 0)
    }

    fn launch_pool_with_phantom_liquidity(launched: AssetId, phantom_liquidity: Balance) -> Pool {
        Pool::new(
            0,
            StoredPool::LaunchV1 {
                near_amount: U128(10u128.pow(24)),
                launched_asset: asset(launched, 2 * 10u128.pow(24)),
                fees: FeeConfiguration::V1(fees()),
                phantom_liquidity_near: U128(phantom_liquidity),
            },
            0,
        )
        .unwrap()
    }

    #[test]
    fn pool_fee_stays_in_the_pool() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"));
        let [in_before, out_before] = pool.reserves;
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        let protocol_fee = amount_in * PROTOCOL_FEE as u128 / FULL_FEE_FRACTION as u128;
        assert_eq!(
            pool.reserves,
            [
                in_before + amount_in - protocol_fee,
                out_before - amount_out
            ]
        );

        let mut pool = public_pool(nep141("a.near"), nep141("b.near"));
        let amount_in = pool.swap_exact_out(0, 1, amount_out).unwrap();
        let protocol_fee = amount_in * PROTOCOL_FEE as u128 / FULL_FEE_FRACTION as u128;
        assert_eq!(
            pool.reserves,
            [
                in_before + amount_in - protocol_fee,
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn launch_pools_convert_fees_when_selling_into_their_quote_token() {
        let launched = nep141("meme.near");
        let mut pool = launch_pool(launched);
        let [mut near, mut tokens] = pool.reserves;
        let amount_in = 10u128.pow(22);
        let fee = |fraction: FeeFraction| amount_in * fraction as u128 / FULL_FEE_FRACTION as u128;
        let after_fees = amount_in - fee(POOL_FEE) - fee(PROTOCOL_FEE);
        let amount_out = mul_div_sum(after_fees, near, tokens, after_fees);
        tokens += after_fees + fee(POOL_FEE);
        near -= amount_out;
        // The protocol fee, swapped to NEAR on the moved reserves
        near -= mul_div_sum(fee(PROTOCOL_FEE), near, tokens, fee(PROTOCOL_FEE));
        tokens += fee(PROTOCOL_FEE);

        assert_eq!(pool.swap_exact_in(1, 0, amount_in), Some(amount_out));
        assert_eq!(pool.reserves, [near, tokens]);
    }

    #[test]
    fn launch_pools_keep_phantom_liquidity_after_converting_fees() {
        let phantom_liquidity = 10u128.pow(23);
        let pool = launch_pool_with_phantom_liquidity(nep141("meme.near"), phantom_liquidity);
        // Leaves 1 more than the phantom liquidity, but converting the fees takes more than that
        let amount_out = pool.reserves[0] - phantom_liquidity - 1;
        assert_eq!(pool.quote_exact_out(1, 0, amount_out), None);
        assert!(pool.quote_exact_out(1, 0, amount_out / 2).is_some());
        // Buying doesn't convert fees, and only grows NEAR
        assert!(pool.quote_exact_in(0, 1, 10u128.pow(24)).is_some());
    }

    #[test]
    fn each_fee_discount_gets_its_own_graph_once() {
        let pools = build_pools(PoolsSource {
            block_height: 1,
            timestamp: 0,
            pools: vec![stored_pool(nep141("a.near"), nep141("b.near"))],
        });
        assert!(Arc::ptr_eq(&pools.graph_for(0), &pools.graph));
        let discounted = pools.graph_for(100_000);
        assert!(!Arc::ptr_eq(&discounted, &pools.graph));
        assert!(Arc::ptr_eq(&discounted, &pools.graph_for(100_000)));
        assert!(
            discounted.pools()[0].info.fees
                == pools.graph.pools()[0].info.fees.clone().discounted(100_000)
        );
    }

    #[test]
    fn swap_gas_counts_hops() {
        let pools = build_pools(PoolsSource {
            block_height: 1,
            timestamp: 0,
            pools: vec![stored_pool(nep141("a.near"), nep141("b.near"))],
        });
        let response = find_path_in(
            &pools,
            &Request {
                token_in: nep141("a.near"),
                token_out: nep141("b.near"),
                amount: QuoteAmount::ExactIn(10u128.pow(21)),
                max_hops: MaxHops::Four,
                slippage: "0.01".parse().unwrap(),
                near_price_raw: BigDecimal::from(1),
                quoted_token_price_raw: BigDecimal::from(1),
                fee_discount: 0,
            },
        )
        .unwrap();
        // 23 base and 2 for the hop
        assert!(matches!(
            response,
            SplitRouteApiResponse::ExactIn { swap_gas, .. } if swap_gas == NearGas::from_tgas(25)
        ));
    }
}
