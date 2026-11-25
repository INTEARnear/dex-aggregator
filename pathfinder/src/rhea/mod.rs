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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

#[cfg(feature = "indexer")]
use pool_indexer::PoolIndexer;
use serde::Serialize;
use swap_router_types::{indexed::AccountState, types::Gas};
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
    /// What the route's pools burn in Rhea's call that swaps
    pub swap_gas: Gas,
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
    max_splits: 8,
    allow_unused_input: true,
    keep_shorter_routes: true,
};
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);
/// Searches split the amount in 1% steps, so they leave out a pool that can't take a step of a
/// large amount, like a curve pool near its reserves, however much it gives. Rhea refunds the input
/// that the actions don't swap, so a tenth of the amount is searched too, and a tenth of that,
/// while they get more net of their hops.
const NARROWING_DIVISOR: Balance = 10;
/// A smaller amount is only searched when price impact takes this much of what the route gets
const NARROWING_PRICE_IMPACT_BP: u128 = 100;

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
    /// The contract's admin fee rate, out of FEE_DIVISOR of the trade fee
    admin_fee_bps: u32,
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
        // The admin fee is added back as LP shares after the reserve check
        self.c_amounts[out_idx] = result.new_destination_amount + result.admin_fee;
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
            &stable::Fees::new(self.total_fee, self.admin_fee_bps),
            self.d,
        )
    }
}

#[derive(Debug, Clone)]
struct RatedPoolInfo {
    decimals: Arc<[u8]>,
    c_amounts: Amounts,
    total_fee: u32,
    /// The contract's admin fee rate, out of FEE_DIVISOR of the trade fee
    admin_fee_bps: u32,
    amp: u64,
    rates: Arc<[Balance]>,
    /// Invariant of `c_amounts`, `None` until computed
    d: Option<U384>,
}

impl RatedPoolInfo {
    fn swap(&mut self, in_idx: usize, out_idx: usize, amount_in: Balance) -> Option<Balance> {
        let (amount_swapped, result) = self.quote(in_idx, out_idx, amount_in)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        // The admin fee is added back as LP shares after the reserve check
        self.c_amounts[out_idx] = result.new_destination_amount + result.admin_fee;
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
            &rated::Fees::new(self.total_fee, self.admin_fee_bps),
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
    /// The contract's admin fee rate, out of FEE_DIVISOR of the trade fee
    admin_fee_bps: u32,
    amp: u64,
    degens: Arc<[Balance]>,
    /// Invariant of `c_amounts`, `None` until computed
    d: Option<U384>,
}

impl DegenPoolInfo {
    fn swap(&mut self, in_idx: usize, out_idx: usize, amount_in: Balance) -> Option<Balance> {
        let (amount_swapped, result) = self.quote(in_idx, out_idx, amount_in)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        // The admin fee is added back as LP shares after the reserve check
        self.c_amounts[out_idx] = result.new_destination_amount + result.admin_fee;
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
            &degen::Fees::new(self.total_fee, self.admin_fee_bps),
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
#[cfg(feature = "indexer")]
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

const HOP_GAS: Gas = Gas::from_ggas(2_540, 2_540);
/// What the route search weighs each hop by, which the router fits to what simulated swaps burnt
static SEARCH_HOP_GAS: AtomicU64 = AtomicU64::new(HOP_GAS.estimated_burn.as_gas());
const STABLE_POOL_TOKEN_GAS: Gas = Gas::from_ggas(1_070, 1_070);
const RATED_POOL_GAS: Gas = Gas::from_ggas(1_530, 1_530);
const RATED_POOL_TOKEN_GAS: Gas = Gas::from_ggas(1_290, 1_290);
const DEGEN_POOL_GAS: Gas = Gas::from_ggas(3_940, 3_940);
/// Simulated swaps burnt up to 3.5 Tgas more than estimated
const GAS_MARGIN: NearGas = NearGas::from_tgas(4);
const GAS_MARGIN_PERCENT: u64 = 15;

impl Pool {
    fn hop_gas(&self) -> Gas {
        match &self.detail {
            PoolDetailInfo::SimplePoolInfo(_) => HOP_GAS,
            PoolDetailInfo::StablePoolInfo(info) => {
                STABLE_POOL_TOKEN_GAS * info.c_amounts.len() as u64
            }
            PoolDetailInfo::RatedPoolInfo(info) => {
                RATED_POOL_GAS + RATED_POOL_TOKEN_GAS * info.c_amounts.len() as u64
            }
            PoolDetailInfo::DegenPoolInfo(_) => DEGEN_POOL_GAS,
        }
    }
}

fn swap_gas(split_route: &SplitRoute, graph: &Graph<Pool>) -> Gas {
    split_route
        .parts
        .iter()
        .flat_map(|part| &part.route.hops)
        .map(|hop| graph.pools()[hop.pool].hop_gas())
        .sum::<Gas>()
        .with_margin(GAS_MARGIN, GAS_MARGIN_PERCENT)
}

/// Whether `split_route` gets [`NARROWING_PRICE_IMPACT_BP`] less for `amount_in` than its heaviest
/// route would at the rate it swaps a hundredth of it, which a route through pools too thin for the
/// amount does
fn loses_to_price_impact(
    graph: &Graph<Pool>,
    split_route: &SplitRoute,
    amount_in: Balance,
) -> Result<bool, anyhow::Error> {
    let amount_out = split_route.emulate_total(graph, QuoteAmount::ExactIn(amount_in))?;
    let Some(hundredth_amount_out) = split_route.parts[0].route.emulate_exact_in(
        graph,
        amount_in / 100,
        &mut PoolsDelta::default(),
    ) else {
        return Ok(false);
    };
    Ok(U256::from(amount_out) * U256::from(crate::BASIS_POINTS)
        < U256::from(hundredth_amount_out)
            * U256::from(100)
            * U256::from(crate::BASIS_POINTS - NARROWING_PRICE_IMPACT_BP))
}

/// Of `split_route` found for `split_amount_in` of the `total_amount_in`, which Rhea refunds the
/// rest of
fn exact_in_response(
    split_route: &SplitRoute,
    graph: &Graph<Pool>,
    token_in: &AccountId,
    token_out: &AccountId,
    total_amount_in: Balance,
    split_amount_in: Balance,
    slippage_bp: u128,
) -> Result<SplitRouteApiResponse, anyhow::Error> {
    let mut routes_resp = Vec::new();
    let mut total_estimated_out: Balance = 0;
    let mut pools_delta = PoolsDelta::default();
    let amount_parts = split_exactly(split_amount_in, &split_route.parts);
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
        swap_gas: swap_gas(split_route, graph),
    })
}

pub struct Request {
    pub token_in: AccountId,
    pub token_out: AccountId,
    pub amount_in: Balance,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
    pub near_price_raw: BigDecimal,
    pub token_out_price_raw: BigDecimal,
}

pub fn set_search_hop_gas(gas: NearGas) {
    SEARCH_HOP_GAS.store(gas.as_gas(), Ordering::Relaxed);
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
    let hop_cost = crate::gas_cost(
        NearGas::from_gas(SEARCH_HOP_GAS.load(Ordering::Relaxed)),
        &request.near_price_raw,
        &request.token_out_price_raw,
    )?;
    let search_for = |amount_in| {
        search::route(
            graph,
            token_in,
            token_out,
            QuoteAmount::ExactIn(amount_in),
            request.max_hops,
            SETTINGS,
            hop_cost,
        )
    };
    let net_amount_out = |split_route: &SplitRoute, amount_in| {
        let amount_out = split_route.emulate_total(graph, QuoteAmount::ExactIn(amount_in))?;
        let hops_cost = hop_cost.saturating_mul(split_route.hops() as Balance);
        Ok::<_, anyhow::Error>(amount_out.saturating_sub(hops_cost))
    };
    let mut split_amount_in = request.amount_in;
    let mut split_route = search_for(split_amount_in)?;
    let mut split_net_amount_out = net_amount_out(&split_route, split_amount_in)?;
    while loses_to_price_impact(graph, &split_route, split_amount_in)? {
        let narrower_amount_in = split_amount_in / NARROWING_DIVISOR;
        let Ok(narrower_route) = search_for(narrower_amount_in) else {
            break;
        };
        let narrower_net_amount_out = net_amount_out(&narrower_route, narrower_amount_in)?;
        if narrower_net_amount_out <= split_net_amount_out {
            break;
        }
        (split_amount_in, split_route, split_net_amount_out) =
            (narrower_amount_in, narrower_route, narrower_net_amount_out);
    }
    let response = exact_in_response(
        &split_route,
        graph,
        &request.token_in,
        &request.token_out,
        request.amount_in,
        split_amount_in,
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

#[cfg(test)]
mod tests {
    //! Pools as they were at block 220410272 (220409387 for pool 6063), when Rhea's
    //! admin_fee_bps was 2000. The largest amounts in are the ones predict_swap_actions accepted,
    //! one more panicked with E69, and the balances after swaps are the stored ones.

    use super::*;

    const ADMIN_FEE_BPS: u32 = 2000;

    fn stable_3364() -> StablePoolInfo {
        StablePoolInfo {
            decimals: [8, 18].into(),
            c_amounts: [4728633852979921, 2132136082966149134]
                .into_iter()
                .collect(),
            total_fee: 5,
            admin_fee_bps: ADMIN_FEE_BPS,
            amp: 240,
            d: None,
        }
    }

    fn rated_6555() -> RatedPoolInfo {
        RatedPoolInfo {
            decimals: [8, 6].into(),
            c_amounts: [10093117847152207451007248, 187317906690495280563184]
                .into_iter()
                .collect(),
            total_fee: 5,
            admin_fee_bps: ADMIN_FEE_BPS,
            amp: 240,
            rates: [rated::PRECISION, rated::PRECISION].into(),
            d: None,
        }
    }

    fn rated_5442() -> RatedPoolInfo {
        RatedPoolInfo {
            decimals: [18, 6].into(),
            c_amounts: [1876992073846267069397942, 835317192937631297879702]
                .into_iter()
                .collect(),
            total_fee: 5,
            admin_fee_bps: ADMIN_FEE_BPS,
            amp: 50,
            rates: [7349129361886197798869, rated::PRECISION].into(),
            d: None,
        }
    }

    fn degen_5949() -> DegenPoolInfo {
        DegenPoolInfo {
            decimals: [8, 24].into(),
            c_amounts: [98854474005878566674618, 1581482389019972205941446576]
                .into_iter()
                .collect(),
            total_fee: 30,
            admin_fee_bps: ADMIN_FEE_BPS,
            amp: 10,
            degens: [81461590000000000000000000000, 4974390980000000000000000].into(),
            d: None,
        }
    }

    fn degen_6063() -> DegenPoolInfo {
        DegenPoolInfo {
            decimals: [24, 6].into(),
            c_amounts: [
                44765685592100102719570412788,
                214496408673817124584898056474,
            ]
            .into_iter()
            .collect(),
            total_fee: 30,
            admin_fee_bps: ADMIN_FEE_BPS,
            amp: 15,
            degens: [4956421690000000000000000, 998710000000000000000000].into(),
            d: None,
        }
    }

    #[test]
    fn stable_reserve_check_takes_off_the_admin_fee() {
        let pool = stable_3364();
        assert_eq!(
            pool.quote(1, 0, 535475272052493974)
                .map(|(amount_out, _)| amount_out),
            Some(372826)
        );
        // Enough reserve without the admin fee, not with it
        assert!(pool.quote(1, 0, 535475272052493975).is_none());
        assert!(pool.quote(1, 0, 535688159890984265).is_none());
    }

    #[test]
    fn stable_swap_keeps_the_admin_fee_in_the_pool() {
        let mut pool = stable_3364();
        assert_eq!(pool.swap(1, 0, 10u128.pow(17)), Some(174562));
        assert_eq!(
            pool.c_amounts.as_slice(),
            [2983006903108461, 2232136082966149134]
        );
    }

    #[test]
    fn rated_reserve_check_takes_off_the_admin_fee() {
        let pool = rated_6555();
        assert_eq!(
            pool.quote(0, 1, 1285429751)
                .map(|(amount_out, _)| amount_out),
            Some(186299)
        );
        assert!(pool.quote(0, 1, 1285429752).is_none());
        assert!(pool.quote(0, 1, 1302259774).is_none());

        let pool = rated_5442();
        assert!(pool.quote(1, 0, 18185064).is_some());
        assert!(pool.quote(1, 0, 18185065).is_none());
        assert!(pool.quote(1, 0, 37040665).is_none());
    }

    #[test]
    fn rated_swap_rates_back_the_admin_fee_separately() {
        let mut pool = rated_5442();
        assert_eq!(pool.swap(1, 0, 10000000), Some(1875259620710702099));
        assert_eq!(
            pool.c_amounts.as_slice(),
            [1732453135564970295753, 10835317192937631297879702]
        );
    }

    #[test]
    fn degen_reserve_check_takes_off_the_admin_fee() {
        let pool = degen_5949();
        assert_eq!(
            pool.quote(1, 0, 4305575812476891564929063435)
                .map(|(amount_out, _)| amount_out),
            Some(9779562)
        );
        assert!(pool.quote(1, 0, 4305575812476891564929063436).is_none());
        assert!(pool.quote(1, 0, 4461406343133028890162344986).is_none());
    }

    #[test]
    fn a_curve_pool_that_cant_take_a_step_of_the_amount_swaps_with_less_of_it() {
        let nbtc: AccountId = "nbtc.bridge.near".parse().unwrap();
        let wnear: AccountId = "wrap.near".parse().unwrap();
        let tokens: Arc<[AccountId]> = [nbtc.clone(), wnear.clone()].into();
        // Pool 5949 takes up to 4,305 NEAR, under 1% of the amount, the other one 0.01 BTC for 1
        // NEAR
        let pools = Pools {
            block_height: 220410272,
            updated_at: Instant::now(),
            graph: Graph::new(vec![
                Pool {
                    id: 5949,
                    tokens: tokens.clone(),
                    detail: PoolDetailInfo::DegenPoolInfo(degen_5949()).with_invariant(),
                },
                Pool {
                    id: 1,
                    tokens,
                    detail: PoolDetailInfo::SimplePoolInfo(SimplePoolInfo {
                        amounts: [1_000_000, 10u128.pow(24)].into_iter().collect(),
                        total_fee: 30,
                    }),
                },
            ]),
        };
        let response = find_path_in(
            &pools,
            &Request {
                token_in: wnear,
                token_out: nbtc,
                amount_in: 1_000_000 * 10u128.pow(24),
                max_hops: MaxHops::Four,
                slippage: "0.005".parse().unwrap(),
                near_price_raw: "2.877e-24".parse().unwrap(),
                token_out_price_raw: "9e-4".parse().unwrap(),
            },
        )
        .unwrap();
        let swapped = response
            .routes
            .iter()
            .map(|route| (route.pools[0].pool_id, route.amount_in))
            .collect::<Vec<_>>();
        // A hundredth of the amount, as a whole, only gets under 0.01 BTC of the other pool. The
        // rest of the input is refunded.
        assert_eq!(response.amount_out, 10779161);
        assert_eq!(
            swapped,
            [(1, 5_700 * 10u128.pow(24)), (5949, 4_300 * 10u128.pow(24))]
        );
        assert_eq!(response.amount_in, 1_000_000 * 10u128.pow(24));
    }

    #[test]
    fn degen_swaps_in_a_row_keep_the_stored_balances() {
        let mut pool = degen_6063();
        pool.swap(1, 0, 16596280).unwrap();
        pool.swap(1, 0, 10275843).unwrap();
        assert_eq!(
            pool.c_amounts.as_slice(),
            [
                44760281050521462864333929217,
                214523280796817124584898056472
            ]
        );
    }
}
