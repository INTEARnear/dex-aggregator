mod storage;

pub use self::storage::watch;

use bigdecimal::BigDecimal;
use borsh::{BorshDeserialize, BorshSerialize};
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, NearGas, U128};
use near_min_api::utils::dec_format;

use std::fmt::{self, Display};
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

use pool_indexer::{AccountState, PoolIndexer};

use crate::search::{
    self, Graph, MaxHops, PoolsDelta, QuoteAmount, Settings, SplitRoute, split_exactly,
};
use serde::{Deserialize, Serialize};
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
        swap_gas: NearGas,
    },
    ExactOut {
        routes: Vec<ExactOutRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        amount_in: U128,
        max_amount_in: U128,
        amount_out: U128,
        swap_gas: NearGas,
    },
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactInRoute {
    pub pools: Vec<ExactInPoolStep>,
    pub amount_in: U128,
    pub min_amount_out: U128,
    pub amount_out: U128,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactOutRoute {
    pub pools: Vec<ExactOutPoolStep>,
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

type U256 = ruint::aliases::U256;

#[derive(Debug, Clone)]
pub struct Pool {
    id: u32,
    reserves: [Balance; 2],
    /// What swaps don't change, shared so simulations clone pools without allocating
    info: Arc<PoolInfo>,
}

#[derive(Debug)]
struct PoolInfo {
    tokens: [AssetId; 2],
    fees: CurrentFees,
    /// Swaps fail if they leave less than this of a token. Launch pools keep their phantom NEAR
    /// liquidity.
    min_reserves: [Balance; 2],
}

impl Pool {
    fn new(id: u32, data: PoolData) -> Self {
        let (tokens, reserves, fees, min_reserves) = match data {
            PoolData::Private { assets, fees, .. } | PoolData::Public { assets, fees, .. } => (
                [assets.0.asset_id, assets.1.asset_id],
                [assets.0.balance.0, assets.1.balance.0],
                fees,
                [0, 0],
            ),
            PoolData::Launch {
                near_amount,
                launched_asset,
                fees,
                phantom_liquidity_near,
                ..
            } => (
                [AssetId::Near, launched_asset.asset_id],
                [near_amount.0, launched_asset.balance.0],
                fees,
                [phantom_liquidity_near.0, 0],
            ),
        };
        Self {
            id,
            reserves,
            info: Arc::new(PoolInfo {
                tokens,
                fees,
                min_reserves,
            }),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, BorshDeserialize, PartialEq, Clone)]
pub enum PoolData {
    Private {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: CurrentFees,
        fee_configuration: FeeConfiguration,
        owner_id: AccountId,
        locked: bool,
    },
    Public {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: CurrentFees,
        fee_configuration: FeeConfiguration,
        total_shares: Option<U128>,
    },
    Launch {
        near_amount: U128,
        launched_asset: AssetWithBalance,
        fees: CurrentFees,
        fee_configuration: FeeConfiguration,
        phantom_liquidity_near: U128,
    },
}

#[derive(Debug, Serialize, Deserialize, BorshDeserialize, PartialEq, Clone)]
// #[serde(untagged)]
pub enum FeeConfiguration {
    V1(CurrentFees),
    V2(V1FeeConfiguration),
}

#[derive(Debug, Serialize, Deserialize, BorshDeserialize, PartialEq, Clone)]
pub struct V1FeeConfiguration {
    receivers: Vec<(FeeReceiver, FeeAmount)>,
}

type Timestamp = u64;

#[derive(Debug, Serialize, Deserialize, BorshDeserialize, PartialEq, Clone)]
pub enum FeeAmount {
    Fixed(FeeFraction),
    Scheduled {
        start: (Timestamp, FeeFraction),
        end: (Timestamp, FeeFraction),
        curve: ScheduledFeeCurve,
    },
    Dynamic {
        min: FeeFraction,
        max: FeeFraction,
    },
}

#[derive(Debug, Serialize, Deserialize, BorshDeserialize, PartialEq, Clone)]
pub enum ScheduledFeeCurve {
    Linear,
}

impl FeeAmount {
    pub fn get_fee_fraction(&self, current_timestamp: Timestamp) -> FeeFraction {
        match self {
            FeeAmount::Fixed(fee_fraction) => *fee_fraction,
            FeeAmount::Scheduled { start, end, curve } => {
                let (start_time, start_fee_fraction) = *start;
                let (end_time, end_fee_fraction) = *end;
                let Some(time_elapsed) = current_timestamp.checked_sub(start_time) else {
                    return start_fee_fraction;
                };
                if current_timestamp >= end_time {
                    return end_fee_fraction;
                }

                // Was checked in .validate() check
                let total_duration = end_time.checked_sub(start_time).unwrap();
                let fee_range = start_fee_fraction.checked_sub(end_fee_fraction).unwrap();

                let fee_decrease = match curve {
                    ScheduledFeeCurve::Linear => {
                        #[allow(clippy::arithmetic_side_effects)]
                        // Multiplying u128 by u128 can't overflow u256, and total_duration
                        // is not 0 due to .validate() check
                        FeeFraction::try_from(u256_to_u128(
                            u128_to_u256(fee_range as u128) * u128_to_u256(time_elapsed as u128)
                                / u128_to_u256(total_duration as u128),
                        ))
                        .expect("Fee decrease overflows u32")
                    }
                };

                assert!(
                    fee_decrease <= fee_range,
                    "Fee decrease must be less than end and start fee difference"
                );

                start_fee_fraction
                    .checked_sub(fee_decrease)
                    .expect("Fee calculation underflow")
            }
            FeeAmount::Dynamic { min: _, max: _ } => {
                unimplemented!("Dynamic fee configuration is not implemented yet");
            }
        }
    }
}

#[derive(Serialize, Deserialize, BorshDeserialize, Debug, PartialEq, Clone)]
pub struct CurrentFees {
    pub receivers: Vec<(FeeReceiver, FeeFraction)>,
}

#[derive(Serialize, Deserialize, BorshDeserialize, Debug, PartialEq, Clone)]
pub enum FeeReceiver {
    Account(AccountId),
    Pool,
}

type FeeFraction = u32;

const MAX_FEE_FRACTION: FeeFraction = 1_000_000;

#[derive(Serialize, Deserialize, BorshDeserialize, Debug, PartialEq, Clone)]
pub struct AssetWithBalance {
    pub asset_id: AssetId,
    pub balance: U128,
}

#[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize, Clone, PartialOrd, Eq, Ord, Hash)]
pub enum AssetId {
    Near,
    Nep141(AccountId),
    Nep245(AccountId, String),
    Nep171(AccountId, String),
}

impl Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Near => write!(f, "near"),
            Self::Nep141(contract_id) => write!(f, "nep141:{contract_id}"),
            Self::Nep245(contract_id, token_id) => write!(f, "nep245:{contract_id}:{token_id}"),
            Self::Nep171(contract_id, token_id) => write!(f, "nep171:{contract_id}:{token_id}"),
        }
    }
}

impl FromStr for AssetId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "near" => Ok(Self::Near),
            _ => match s.split_once(':') {
                Some(("nep141", contract_id)) => {
                    Ok(Self::Nep141(contract_id.parse().map_err(|e| {
                        format!("Invalid account id {contract_id}: {e}")
                    })?))
                }
                Some(("nep245", rest)) => {
                    if let Some((contract_id, token_id)) = rest.split_once(':') {
                        Ok(Self::Nep245(
                            contract_id
                                .parse()
                                .map_err(|e| format!("Invalid account id {contract_id}: {e}"))?,
                            token_id.to_string(),
                        ))
                    } else {
                        Err(format!("Invalid asset id: {s}"))
                    }
                }
                Some(("nep171", rest)) => {
                    if let Some((contract_id, token_id)) = rest.split_once(':') {
                        Ok(Self::Nep171(
                            contract_id
                                .parse()
                                .map_err(|e| format!("Invalid account id {contract_id}: {e}"))?,
                            token_id.to_string(),
                        ))
                    } else {
                        Err(format!("Invalid asset id: {s}"))
                    }
                }
                _ => Err(format!("Invalid asset id: {s}")),
            },
        }
    }
}

impl Serialize for AssetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Serialize::serialize(&self.to_string(), serializer)
    }
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as Deserialize<'de>>::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

fn u128_to_u256(value: u128) -> U256 {
    U256::from(value)
}

fn u256_to_u128(value: U256) -> u128 {
    u128::try_from(value).expect("Value must be less than 128 bits")
}

fn total_fee_fraction(fees: &CurrentFees) -> u128 {
    fees.receivers
        .iter()
        .map(|(_, fee)| *fee as u128)
        .sum::<u128>()
}

fn collect_fees(amount_in: u128, fees: &CurrentFees) -> u128 {
    let mut total_fees = 0u128;
    for (_, fee_fraction) in fees.receivers.iter() {
        let fee_amount = u256_to_u128(
            u128_to_u256(amount_in) * u128_to_u256(*fee_fraction as u128)
                / u128_to_u256(MAX_FEE_FRACTION as u128),
        );
        total_fees = total_fees.checked_add(fee_amount).expect("Overflow");
    }
    amount_in.checked_sub(total_fees).expect("Fee exceeds 100%")
}

impl Pool {
    /// Input and output reserves, the fees, and the least the output reserve can be left with
    fn reserves(&mut self, first_in: bool) -> (&mut Balance, &mut Balance, &CurrentFees, Balance) {
        let [balance0, balance1] = &mut self.reserves;
        let [min0, min1] = self.info.min_reserves;
        if first_in {
            (balance0, balance1, &self.info.fees, min1)
        } else {
            (balance1, balance0, &self.info.fees, min0)
        }
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
            let (in_balance, out_balance, fees, min_out_balance) = pool.reserves(token_in == 0);
            if *in_balance == 0 {
                return None;
            }
            let amount_in_after_fees = collect_fees(amount_in, fees);
            // u128 * u128 or u128 + u128 can't overflow u256; in_balance was checked to be positive
            #[allow(clippy::arithmetic_side_effects)]
            let amount_out = u256_to_u128(
                u128_to_u256(amount_in_after_fees) * u128_to_u256(*out_balance)
                    / (u128_to_u256(*in_balance) + u128_to_u256(amount_in_after_fees)),
            );
            *in_balance = in_balance
                .checked_add(amount_in_after_fees)
                .expect("Overflow");
            *out_balance = out_balance.checked_sub(amount_out).expect("Underflow");
            if *out_balance < min_out_balance {
                return None;
            }
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
            let (in_balance, out_balance, fees, min_out_balance) = pool.reserves(token_in == 0);
            if amount_out >= *out_balance || *out_balance - amount_out < min_out_balance {
                return None;
            }

            #[allow(clippy::arithmetic_side_effects)]
            let amount_in_without_fees = u256_to_u128(
                (u128_to_u256(*in_balance) * u128_to_u256(amount_out))
                    / (u128_to_u256(*out_balance) - u128_to_u256(amount_out)),
            )
            .checked_add(1)
            .expect("Overflow");
            let total_fee_fraction = total_fee_fraction(fees);
            let fee_denominator = (MAX_FEE_FRACTION as u128)
                .checked_sub(total_fee_fraction)
                .expect("Fee fraction somehow above 100%");
            let fee_denominator_minus_one = fee_denominator
                .checked_sub(1)
                .expect("Fee fraction somehow equals 100%");
            #[allow(clippy::arithmetic_side_effects)]
            let amount_in = u256_to_u128(
                (u128_to_u256(amount_in_without_fees) * u128_to_u256(MAX_FEE_FRACTION as u128)
                    + u128_to_u256(fee_denominator_minus_one))
                    / u128_to_u256(fee_denominator),
            );
            let amount_in_after_fees = collect_fees(amount_in, fees);
            *in_balance = in_balance
                .checked_add(amount_in_after_fees)
                .expect("Overflow");
            *out_balance = out_balance.checked_sub(amount_out).expect("Underflow");
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
    graph: Graph<Pool>,
}

/// Everything a pools snapshot is built from, as returned by RPC at one block
#[derive(Serialize, Deserialize)]
struct PoolsSource {
    block_height: BlockHeight,
    pools: Vec<PoolData>,
}

fn build_pools(source: PoolsSource) -> Pools {
    let PoolsSource {
        block_height,
        pools,
    } = source;
    let pools = pools
        .into_iter()
        .enumerate()
        .map(|(i, data)| Pool::new(i as u32, data))
        .collect::<Vec<_>>();
    Pools {
        block_height,
        updated_at: Instant::now(),
        graph: Graph::new(pools),
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
            Ok(Ok(pools)) => *POOLS_CACHE.write().await = Some(Arc::new(pools)),
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
    top_routes: 5,
    max_splits: 2,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

fn swap_gas(split_route: &SplitRoute) -> NearGas {
    let pools = split_route
        .parts
        .iter()
        .map(|part| part.route.hops.len() as u64)
        .sum::<u64>();
    NearGas::from_tgas(8 + 26 * pools)
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
                let min_amount_out = u256_to_u128(
                    u128_to_u256(estimated_out) * u128_to_u256(10_000u128 - slippage_bp)
                        / u128_to_u256(10_000u128),
                );

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
                let max_amount_in = u256_to_u128(
                    u128_to_u256(amount_in) * u128_to_u256(10_000u128 + slippage_bp)
                        / u128_to_u256(10_000),
                );

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
                        max_amount_in: U128(u256_to_u128(
                            u128_to_u256(step_amount.0) * u128_to_u256(10_000u128 + slippage_bp)
                                / u128_to_u256(10_000),
                        )),
                    });

                    current_token = graph.token(hop.token_out).clone();
                }

                routes_resp.push(ExactOutRoute {
                    pools: pools_resp,
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
                swap_gas: swap_gas(split_route),
            })
        }
    }
}

pub struct Request {
    pub token_in: AssetId,
    pub token_out: AssetId,
    pub amount: QuoteAmount,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
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
    )?;
    let response = to_api_response(
        &split_route,
        graph,
        &request.token_in,
        &request.token_out,
        request.amount,
        slippage_bp,
    )?;
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
