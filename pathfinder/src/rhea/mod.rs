mod degen;
mod rated;
mod stable;
mod storage;

pub use self::storage::watch;

use bigdecimal::BigDecimal;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, NearGas};
use near_min_api::utils::dec_format;
use smallvec::SmallVec;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

use pool_indexer::{AccountState, PoolIndexer};
use serde::Serialize;
use tracing::{info, warn};

use crate::search::{
    self, Graph, MaxHops, PoolsDelta, QuoteAmount, Settings, SplitRoute, split_exactly,
};

use self::degen::DegenSwap;
use self::rated::RatedSwap;
use self::stable::StableSwap;

#[derive(Serialize, Debug, Clone)]
pub struct SplitRouteApiResponse {
    pub routes: Vec<ApiResponseRoute>,
    pub contract_in: AccountId,
    pub contract_out: AccountId,
    #[serde(with = "dec_format")]
    pub amount_in: Balance,
    #[serde(with = "dec_format")]
    pub amount_out: Balance,
    pub swap_gas: NearGas,
}

#[derive(Serialize, Debug, Clone)]
pub struct ApiResponseRoute {
    pub pools: Vec<ApiResponsePoolStep>,
    #[serde(with = "dec_format")]
    pub amount_in: Balance,
    #[serde(with = "dec_format")]
    pub min_amount_out: Balance,
    #[serde(with = "dec_format")]
    pub amount_out: Balance,
}

#[derive(Serialize, Debug, Clone)]
pub struct ApiResponsePoolStep {
    #[serde(with = "dec_format")]
    pub pool_id: u64,
    pub token_in: AccountId,
    pub token_out: AccountId,
    #[serde(with = "dec_format")]
    pub amount_in: Balance,
    #[serde(with = "dec_format")]
    pub amount_out: Balance,
    #[serde(with = "dec_format")]
    pub min_amount_out: Balance,
}

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";
const FEE_DIVISOR: u32 = 10_000;

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 2,
    allow_unused_input: true,
    keep_shorter_routes: true,
};
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);

type U256 = ruint::aliases::U256;
type U384 = ruint::Uint<384, 6>;

/// Balances of a pool's tokens, inline so simulations clone pools without allocating
type Amounts = SmallVec<[Balance; 4]>;

#[derive(Debug, Clone)]
pub struct Pool {
    id: u64,
    tokens: Arc<[AccountId]>,
    detail: PoolDetailInfo,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
enum PoolDetailInfo {
    SimplePoolInfo(SimplePoolInfo),
    StablePoolInfo(StablePoolInfo),
    RatedPoolInfo(RatedPoolInfo),
    DegenPoolInfo(DegenPoolInfo),
}

impl PoolDetailInfo {
    /// Computes the invariant of curve pools, so swaps on this state don't
    fn with_invariant(mut self) -> Self {
        match &mut self {
            Self::SimplePoolInfo(_) => {}
            Self::StablePoolInfo(info) => info.d = info.get_invariant().compute_d(&info.c_amounts),
            Self::RatedPoolInfo(info) => {
                info.d = RatedSwap::new(info.amp, &info.rates).invariant(&info.c_amounts)
            }
            Self::DegenPoolInfo(info) => {
                info.d = DegenSwap::new(info.amp, &info.degens).invariant(&info.c_amounts)
            }
        }
        self
    }
}

impl search::Pool for Pool {
    type Token = AccountId;

    fn tokens(&self) -> Vec<AccountId> {
        self.tokens.to_vec()
    }

    fn swap_exact_in(
        &mut self,
        token_in: usize,
        token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        self.unless_panicked(|detail| match detail {
            PoolDetailInfo::SimplePoolInfo(info) => info.swap(token_in, token_out, amount_in),
            PoolDetailInfo::StablePoolInfo(info) => info.swap(token_in, token_out, amount_in),
            PoolDetailInfo::RatedPoolInfo(info) => info.swap(token_in, token_out, amount_in),
            PoolDetailInfo::DegenPoolInfo(info) => info.swap(token_in, token_out, amount_in),
        })
    }

    fn quote_exact_in(
        &self,
        token_in: usize,
        token_out: usize,
        amount_in: Balance,
    ) -> Option<Balance> {
        unless_panicked(self.id, || match &self.detail {
            PoolDetailInfo::SimplePoolInfo(info) => info.quote(token_in, token_out, amount_in),
            PoolDetailInfo::StablePoolInfo(info) => info
                .quote(token_in, token_out, amount_in)
                .map(|(amount_out, _)| amount_out),
            PoolDetailInfo::RatedPoolInfo(info) => info
                .quote(token_in, token_out, amount_in)
                .map(|(amount_out, _)| amount_out),
            PoolDetailInfo::DegenPoolInfo(info) => info
                .quote(token_in, token_out, amount_in)
                .map(|(amount_out, _)| amount_out),
        })
    }

    /// Rhea routes are exact-in only
    fn swap_exact_out(
        &mut self,
        _token_in: usize,
        _token_out: usize,
        _amount_out: Balance,
    ) -> Option<Balance> {
        None
    }
}

impl Pool {
    fn unless_panicked(
        &mut self,
        swap: impl FnOnce(&mut PoolDetailInfo) -> Option<Balance>,
    ) -> Option<Balance> {
        unless_panicked(self.id, || swap(&mut self.detail))
    }
}

/// Simulations panic on overflows like the contract
fn unless_panicked(id: u64, swap: impl FnOnce() -> Option<Balance>) -> Option<Balance> {
    std::panic::catch_unwind(AssertUnwindSafe(swap)).unwrap_or_else(|_| {
        warn!("Panicked while emulating a swap in pool {id}");
        None
    })
}

#[derive(Debug, Clone)]
struct SimplePoolInfo {
    amounts: Amounts,
    total_fee: u32,
}

impl SimplePoolInfo {
    fn swap(&mut self, token_in: usize, token_out: usize, amount_in: Balance) -> Option<Balance> {
        let received = self.quote(token_in, token_out, amount_in)?;
        self.amounts[token_in] += amount_in;
        self.amounts[token_out] -= received;
        Some(received)
    }

    /// Like `swap`, without modifying the pool
    fn quote(&self, token_in: usize, token_out: usize, amount_in: Balance) -> Option<Balance> {
        let in_balance = U256::from(self.amounts[token_in]);
        let out_balance = U256::from(self.amounts[token_out]);
        if in_balance.is_zero() || out_balance.is_zero() || token_in == token_out || amount_in == 0
        {
            return None;
        }
        let amount_with_fee = U256::from(amount_in) * U256::from(FEE_DIVISOR - self.total_fee);
        // The contract fails if it overflows
        let received = (amount_with_fee.checked_mul(out_balance)?
            / (U256::from(FEE_DIVISOR) * in_balance + amount_with_fee))
            .to::<u128>();
        // The swap fails if the balance overflows
        self.amounts[token_in].checked_add(amount_in)?;
        Some(received)
    }
}

#[derive(Debug, Clone)]
struct StablePoolInfo {
    decimals: Arc<[u8]>,
    c_amounts: Amounts,
    total_fee: u32,
    amp: u64,
    /// Invariant of `c_amounts`, `None` until computed
    d: Option<U256>,
}

pub fn u128_ratio(a: u128, num: u128, denom: u128) -> u128 {
    (U256::from(a) * U256::from(num) / U256::from(denom)).to::<u128>()
}

impl StablePoolInfo {
    fn swap(&mut self, in_idx: usize, out_idx: usize, amount_in: Balance) -> Option<Balance> {
        let (amount_swapped, result) = self.quote(in_idx, out_idx, amount_in)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        self.d = None;
        Some(amount_swapped)
    }

    /// The output and the new balances, without modifying the pool
    fn quote(
        &self,
        in_idx: usize,
        out_idx: usize,
        amount_in: Balance,
    ) -> Option<(Balance, stable::SwapResult)> {
        let result = self.internal_get_return(in_idx, amount_in, out_idx)?;
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx);
        if result.new_destination_amount < stable::MIN_RESERVE {
            return None;
        }
        Some((amount_swapped, result))
    }
    fn c_amount_to_amount(&self, c_amount: u128, index: usize) -> u128 {
        let value = self.decimals[index];
        if value <= stable::TARGET_DECIMAL {
            let factor = 10_u128
                .checked_pow((stable::TARGET_DECIMAL - value) as u32)
                .unwrap();
            c_amount.checked_div(factor).expect("Cannot divide")
        } else {
            let factor = 10_u128
                .checked_pow((value - stable::TARGET_DECIMAL) as u32)
                .unwrap();
            c_amount.checked_mul(factor).expect("Cannot multiply")
        }
    }

    fn amount_to_c_amount(&self, amount: u128, index: usize) -> u128 {
        let value = self.decimals[index];
        if value <= stable::TARGET_DECIMAL {
            let factor = 10_u128
                .checked_pow((stable::TARGET_DECIMAL - value) as u32)
                .unwrap();
            amount.checked_mul(factor).expect("Cannot multiply")
        } else {
            let factor = 10_u128
                .checked_pow((value - stable::TARGET_DECIMAL) as u32)
                .unwrap();
            amount.checked_div(factor).expect("Cannot divide")
        }
    }

    fn get_invariant(&self) -> StableSwap {
        StableSwap::new(self.amp)
    }

    fn internal_get_return(
        &self,
        token_in: usize,
        amount_in: Balance,
        token_out: usize,
    ) -> Option<stable::SwapResult> {
        // make amounts into comparable-amounts
        let c_amount_in = self.amount_to_c_amount(amount_in, token_in);

        self.get_invariant().swap_to(
            token_in,
            c_amount_in,
            token_out,
            &self.c_amounts,
            &stable::Fees::new(self.total_fee),
            self.d,
        )
    }
}

#[derive(Debug, Clone)]
struct RatedPoolInfo {
    decimals: Arc<[u8]>,
    c_amounts: Amounts,
    total_fee: u32,
    amp: u64,
    rates: Arc<[Balance]>,
    /// Invariant of `c_amounts`, `None` until computed
    d: Option<U384>,
}

impl RatedPoolInfo {
    fn swap(&mut self, in_idx: usize, out_idx: usize, amount_in: Balance) -> Option<Balance> {
        let (amount_swapped, result) = self.quote(in_idx, out_idx, amount_in)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        self.d = None;
        Some(amount_swapped)
    }

    /// The output and the new balances, without modifying the pool
    fn quote(
        &self,
        in_idx: usize,
        out_idx: usize,
        amount_in: Balance,
    ) -> Option<(Balance, rated::SwapResult)> {
        let c_amount_in = self.amount_to_c_amount(amount_in, in_idx);
        let result = RatedSwap::new(self.amp, &self.rates).swap_to(
            in_idx,
            c_amount_in,
            out_idx,
            &self.c_amounts,
            &rated::Fees::new(self.total_fee),
            self.d,
        )?;
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx);
        if result.new_destination_amount < rated::MIN_RESERVE {
            return None;
        }
        Some((amount_swapped, result))
    }

    fn amount_to_c_amount(&self, amount: u128, index: usize) -> u128 {
        let value = self.decimals.get(index).unwrap();
        let factor = 10_u128
            .checked_pow((rated::TARGET_DECIMAL - value) as u32)
            .unwrap();
        amount.checked_mul(factor).unwrap()
    }

    fn c_amount_to_amount(&self, c_amount: u128, index: usize) -> u128 {
        let value = self.decimals.get(index).unwrap();
        let factor = 10_u128
            .checked_pow((rated::TARGET_DECIMAL - value) as u32)
            .unwrap();
        c_amount.checked_div(factor).unwrap()
    }
}

#[derive(Debug, Clone)]
struct DegenPoolInfo {
    decimals: Arc<[u8]>,
    c_amounts: Amounts,
    total_fee: u32,
    amp: u64,
    degens: Arc<[Balance]>,
    /// Invariant of `c_amounts`, `None` until computed
    d: Option<U384>,
}

impl DegenPoolInfo {
    fn swap(&mut self, in_idx: usize, out_idx: usize, amount_in: Balance) -> Option<Balance> {
        let (amount_swapped, result) = self.quote(in_idx, out_idx, amount_in)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        self.d = None;
        Some(amount_swapped)
    }

    /// The output and the new balances, without modifying the pool
    fn quote(
        &self,
        in_idx: usize,
        out_idx: usize,
        amount_in: Balance,
    ) -> Option<(Balance, degen::SwapResult)> {
        let c_amount_in = self.amount_to_c_amount(amount_in, in_idx);
        let result = DegenSwap::new(self.amp, &self.degens).swap_to(
            in_idx,
            c_amount_in,
            out_idx,
            &self.c_amounts,
            &degen::Fees::new(self.total_fee),
            self.d,
        )?;
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx);
        if result.new_destination_amount < degen::MIN_RESERVE {
            return None;
        }
        Some((amount_swapped, result))
    }

    fn amount_to_c_amount(&self, amount: u128, index: usize) -> u128 {
        let value = self.decimals.get(index).unwrap();
        let factor = 10_u128
            .checked_pow((degen::TARGET_DECIMAL - value) as u32)
            .unwrap();
        amount.checked_mul(factor).unwrap()
    }

    fn c_amount_to_amount(&self, c_amount: u128, index: usize) -> u128 {
        let value = self.decimals.get(index).unwrap();
        let factor = 10_u128
            .checked_pow((degen::TARGET_DECIMAL - value) as u32)
            .unwrap();
        c_amount.checked_div(factor).unwrap()
    }
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    graph: Graph<Pool>,
}

/// Builds pools from indexed storage
pub fn build_indexed_pools(state: &AccountState) -> Result<Pools, anyhow::Error> {
    Ok(Pools {
        block_height: state.block.height,
        updated_at: Instant::now(),
        graph: Graph::new(storage::pools(state)?),
    })
}

/// Builds pools from indexed storage after every block, returns why it stopped
pub async fn update_pools(indexer: PoolIndexer) -> anyhow::Error {
    let account_id: AccountId = RHEA_CONTRACT_ID.parse().unwrap();
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
                crate::pools_built(crate::PoolsDex::Rhea, height);
            }
            Ok(Err(e)) => {
                return e.context(format!("Failed to build Rhea pools at block {height}"));
            }
            Err(e) => {
                return anyhow::anyhow!("Building Rhea pools at block {height} panicked: {e}");
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

fn swap_gas(split_route: &SplitRoute) -> NearGas {
    let pools = split_route
        .parts
        .iter()
        .map(|part| part.route.hops.len() as u64)
        .sum::<u64>();
    NearGas::from_ggas(3_000 + 2_500 * pools)
}

fn exact_in_response(
    split_route: &SplitRoute,
    graph: &Graph<Pool>,
    token_in: &AccountId,
    token_out: &AccountId,
    total_amount_in: Balance,
    slippage_bp: u128,
) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let mut routes_resp = Vec::new();
    let mut total_estimated_out: Balance = 0;
    let mut pools_delta = PoolsDelta::default();
    let amount_parts = split_exactly(total_amount_in, &split_route.parts);
    for (part, amount_part) in split_route.parts.iter().zip(amount_parts) {
        let estimated_out = part
            .route
            .emulate_exact_in(graph, amount_part, &mut pools_delta)
            .ok_or_else(|| anyhow::anyhow!("Failed to emulate route"))?;
        let min_amount_out = u128_ratio(estimated_out, 10_000u128 - slippage_bp, 10_000u128);

        let mut pools_response = Vec::new();
        let mut current_token = token_in.clone();
        for (idx, hop) in part.route.hops.iter().enumerate() {
            let is_first = idx == 0;
            let is_last = idx + 1 == part.route.hops.len();

            pools_response.push(ApiResponsePoolStep {
                pool_id: graph.pools()[hop.pool].id,
                token_in: current_token.clone(),
                token_out: graph.token(hop.token_out).clone(),
                amount_in: if is_first { amount_part } else { 0 },
                amount_out: 0,
                min_amount_out: if is_last { min_amount_out } else { 0 },
            });

            current_token = graph.token(hop.token_out).clone();
        }

        routes_resp.push(ApiResponseRoute {
            pools: pools_response,
            amount_in: amount_part,
            min_amount_out,
            amount_out: 0,
        });
        total_estimated_out = total_estimated_out
            .checked_add(estimated_out)
            .ok_or_else(|| anyhow::anyhow!("Total estimated out overflows u128"))?;
    }
    Ok(SplitRouteApiResponse {
        routes: routes_resp,
        contract_in: token_in.clone(),
        contract_out: token_out.clone(),
        amount_in: total_amount_in,
        amount_out: total_estimated_out,
        swap_gas: swap_gas(split_route),
    })
}

pub struct Request {
    pub token_in: AccountId,
    pub token_out: AccountId,
    pub amount_in: Balance,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
}

/// Finds a route on the newest pools
pub async fn find_path(request: Request) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let pools = get_pools().await?;
    tokio::task::spawn_blocking(move || find_path_in(&pools, &request)).await?
}

/// Finds a route on `pools`. CPU-bound, emulates swaps.
pub fn find_path_in(
    pools: &Pools,
    request: &Request,
) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let slippage_bp = crate::slippage_bp(&request.slippage)?;
    let graph = &pools.graph;
    let (Some(token_in), Some(token_out)) = (
        graph.token_index(&request.token_in),
        graph.token_index(&request.token_out),
    ) else {
        anyhow::bail!("No routes found");
    };
    let amount = QuoteAmount::ExactIn(request.amount_in);
    let split_route = search::route(
        graph,
        token_in,
        token_out,
        amount,
        request.max_hops,
        SETTINGS,
    )?;
    let response = exact_in_response(
        &split_route,
        graph,
        &request.token_in,
        &request.token_out,
        request.amount_in,
        slippage_bp,
    )?;
    info!(
        "Rhea route from {} to {}: {} for {}, block {}",
        request.token_in,
        request.token_out,
        response.amount_out,
        request.amount_in,
        pools.block_height
    );
    Ok(response)
}
