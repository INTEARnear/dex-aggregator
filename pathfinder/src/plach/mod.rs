mod storage;

pub use self::storage::watch;

use bigdecimal::BigDecimal;
use borsh::BorshDeserialize;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, BlockHeight, NearGas, U128};
use near_min_api::utils::dec_format;

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

pub use intear_dex_types::AssetId;
use pool_indexer::{AccountState, PoolIndexer};
use xyk_dex_types::{FULL_SPLIT_FRACTION, SplitFraction};

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

type U256 = ruint::aliases::U256;

#[derive(Debug, Clone)]
pub struct Pool {
    id: u32,
    reserves: [Balance; 2],
    /// The referrer's fee on swaps that put in token 0 and token 1, on top of `info.fees`. 0
    /// without a referrer, or for one that isn't charged on the swap.
    referral_fees: [FeeFraction; 2],
    /// What swaps don't change, shared so simulations clone pools without allocating
    info: Arc<PoolInfo>,
}

#[derive(Debug)]
struct PoolInfo {
    tokens: [AssetId; 2],
    fees: CurrentFees,
    /// Swaps fail if they leave less than this of a token. Launch pools keep their phantom
    /// liquidity of the quote token.
    min_reserves: [Balance; 2],
    /// Which token a launch pool converts its fees to
    launch_quote_token: Option<usize>,
    /// Whether both tokens are stable assets, which pay reduced protocol and referral fees.
    /// `None` when the contract can't tell, for NEP-171 tokens, which makes it panic on swaps.
    stable_pair: Option<bool>,
}

/// A referrer's fee on Plach swaps, as the xyk DEX charges it
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Referrer {
    pub fee_fraction: FeeFraction,
    /// Instead of `fee_fraction` on swaps between two stable assets
    pub fee_fraction_reduced: FeeFraction,
    /// The assets the referrer collects fees in. A swap whose fee would be in another asset
    /// doesn't charge one.
    pub fee_assets: HashSet<AssetId>,
}

impl Pool {
    fn new(id: u32, data: PoolData) -> Self {
        let launch_quote_token = match &data {
            PoolData::Launch { .. } | PoolData::LaunchV2 { .. } => Some(0),
            PoolData::Private { .. } | PoolData::Public { .. } => None,
        };
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
            PoolData::LaunchV2 {
                quote_asset,
                launched_asset,
                fees,
                phantom_liquidity,
                ..
            } => (
                [quote_asset.asset_id, launched_asset.asset_id],
                [quote_asset.balance.0, launched_asset.balance.0],
                fees,
                [phantom_liquidity.0, 0],
            ),
        };
        let stable_pair = storage::should_reduce_fee([&tokens[0], &tokens[1]]).ok();
        Self {
            id,
            reserves,
            referral_fees: [0, 0],
            info: Arc::new(PoolInfo {
                tokens,
                fees,
                min_reserves,
                launch_quote_token,
                stable_pair,
            }),
        }
    }

    /// The pool with the fee `referrer` is charged on each swap through it, as the xyk DEX's
    /// `with_referral_fee` adds it: the reduced fee between two stable assets, and only when the
    /// referrer collects fees in the asset they're paid in. That's the asset put in, but for a
    /// launch pool selling into its quote token, which converts fees to it.
    fn with_referrer(&self, referrer: &Referrer) -> Self {
        let Some(stable_pair) = self.info.stable_pair else {
            return self.clone();
        };
        let fee_fraction = if stable_pair {
            referrer.fee_fraction_reduced
        } else {
            referrer.fee_fraction
        };
        let referral_fees = [0, 1].map(|token_in| {
            let token_out = 1 - token_in;
            let fee_token = if self.info.launch_quote_token == Some(token_out) {
                token_out
            } else {
                token_in
            };
            if referrer.fee_assets.contains(&self.info.tokens[fee_token]) {
                fee_fraction
            } else {
                0
            }
        });
        Self {
            referral_fees,
            ..self.clone()
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
    LaunchV2 {
        quote_asset: AssetWithBalance,
        launched_asset: AssetWithBalance,
        fees: CurrentFees,
        fee_configuration: FeeConfiguration,
        phantom_liquidity: U128,
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

fn u128_to_u256(value: u128) -> U256 {
    U256::from(value)
}

fn u256_to_u128(value: U256) -> u128 {
    u128::try_from(value).expect("Value must be less than 128 bits")
}

/// Fractions of every fee receiver and whether the pool keeps it, the referrer's last as the
/// contract adds it
fn fee_fractions(
    fees: &CurrentFees,
    referral_fee: FeeFraction,
) -> impl Iterator<Item = (bool, FeeFraction)> + '_ {
    fees.receivers
        .iter()
        .map(|(receiver, fee)| (matches!(receiver, FeeReceiver::Pool), *fee))
        .chain((referral_fee > 0).then_some((false, referral_fee)))
}

fn total_fee_fraction(fees: &CurrentFees, referral_fee: FeeFraction) -> u128 {
    fee_fractions(fees, referral_fee)
        .map(|(_, fee)| u128::from(fee))
        .sum::<u128>()
}

fn fee_amount(amount_in: u128, fee_fraction: FeeFraction) -> u128 {
    u256_to_u128(
        u128_to_u256(amount_in) * u128_to_u256(fee_fraction as u128)
            / u128_to_u256(MAX_FEE_FRACTION as u128),
    )
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
    referral_fee: FeeFraction,
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
        for (kept_by_pool, fee_fraction) in fee_fractions(self.fees, self.referral_fee) {
            let fee_amount = fee_amount(amount_in, fee_fraction);
            total_fees = total_fees.checked_add(fee_amount).expect("Overflow");
            if kept_by_pool {
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
            for (kept_by_pool, fee_fraction) in fee_fractions(self.fees, self.referral_fee) {
                if kept_by_pool {
                    continue;
                }
                let fee_amount = fee_amount(amount_in, fee_fraction);
                // in_balance is positive after the swap, u128 + u128 can't overflow u256
                #[allow(clippy::arithmetic_side_effects)]
                let fee_amount_out = u256_to_u128(
                    u128_to_u256(fee_amount) * u128_to_u256(*self.out_balance)
                        / (u128_to_u256(*self.in_balance) + u128_to_u256(fee_amount)),
                );
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
    /// The swap putting in token 0 if `first_in`, else token 1. `None` for a pool the contract
    /// can't swap in.
    fn swap(&mut self, first_in: bool) -> Option<Swap<'_>> {
        self.info.stable_pair?;
        let token_out = if first_in { 1 } else { 0 };
        let [balance0, balance1] = &mut self.reserves;
        let (in_balance, out_balance) = if first_in {
            (balance0, balance1)
        } else {
            (balance1, balance0)
        };
        Some(Swap {
            in_balance,
            out_balance,
            fees: &self.info.fees,
            referral_fee: self.referral_fees[1 - token_out],
            min_out_balance: self.info.min_reserves[token_out],
            converts_fees: self.info.launch_quote_token == Some(token_out),
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
            if *swap.in_balance == 0 {
                return None;
            }
            let fees = swap.collect_fees(amount_in);
            // u128 * u128 or u128 + u128 can't overflow u256; in_balance was checked to be positive
            #[allow(clippy::arithmetic_side_effects)]
            let amount_out = u256_to_u128(
                u128_to_u256(fees.amount_in_after_fees) * u128_to_u256(*swap.out_balance)
                    / (u128_to_u256(*swap.in_balance) + u128_to_u256(fees.amount_in_after_fees)),
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

            #[allow(clippy::arithmetic_side_effects)]
            let amount_in_without_fees = u256_to_u128(
                (u128_to_u256(in_balance) * u128_to_u256(amount_out))
                    / (u128_to_u256(out_balance) - u128_to_u256(amount_out)),
            )
            .checked_add(1)
            .expect("Overflow");
            let total_fee_fraction = total_fee_fraction(swap.fees, swap.referral_fee);
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
    /// Pools without referral fees
    graph: Arc<Graph<Pool>>,
    referrers: HashMap<AccountId, Referrer>,
    /// Pools with the referral fees of a referrer, made the first time it asks for a route
    referrer_graphs: Mutex<HashMap<AccountId, Arc<Graph<Pool>>>>,
}

impl Pools {
    /// Pools as `referrer` swaps through them, with its referral fees
    fn graph_for(&self, referrer: &AccountId) -> Arc<Graph<Pool>> {
        let Some(settings) = self.referrers.get(referrer) else {
            return self.graph.clone();
        };
        self.referrer_graphs
            .lock()
            .unwrap()
            .entry(referrer.clone())
            .or_insert_with(|| Arc::new(self.graph.map_pools(|pool| pool.with_referrer(settings))))
            .clone()
    }
}

/// Everything a pools snapshot is built from, as returned by RPC at one block
#[derive(Serialize, Deserialize)]
struct PoolsSource {
    block_height: BlockHeight,
    pools: Vec<PoolData>,
    referrers: HashMap<AccountId, Referrer>,
}

fn build_pools(source: PoolsSource) -> Pools {
    let PoolsSource {
        block_height,
        pools,
        referrers,
    } = source;
    let pools = pools
        .into_iter()
        .enumerate()
        .map(|(i, data)| Pool::new(i as u32, data))
        .collect::<Vec<_>>();
    Pools {
        block_height,
        updated_at: Instant::now(),
        graph: Arc::new(Graph::new(pools)),
        referrers,
        referrer_graphs: Mutex::default(),
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

const SWAP_BASE_GAS: NearGas = NearGas::from_tgas(26);
const HOP_GAS: NearGas = NearGas::from_ggas(4_500);

fn swap_gas(split_route: &SplitRoute) -> NearGas {
    NearGas::from_gas(SWAP_BASE_GAS.as_gas() + HOP_GAS.as_gas() * split_route.hops() as u64)
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
    pub near_price_raw: BigDecimal,
    pub quoted_token_price_raw: BigDecimal,
    /// The swap's referrer: its referral fee, if it charges one, is taken out of every pool
    pub referrer: AccountId,
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
    let graph = &*pools.graph_for(&request.referrer);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Pool as _;

    const POOL_FEE: FeeFraction = 3_000; // 0.3%
    const PROTOCOL_FEE: FeeFraction = 1_000; // 0.1%
    const REFERRAL_FEE: FeeFraction = 10_000; // 1%
    const REFERRAL_FEE_REDUCED: FeeFraction = 500; // 0.05%

    fn nep141(account_id: &str) -> AssetId {
        AssetId::Nep141(account_id.parse().unwrap())
    }

    fn fees() -> CurrentFees {
        CurrentFees {
            receivers: vec![
                (FeeReceiver::Pool, POOL_FEE),
                (
                    FeeReceiver::Account("plach.intear.near".parse().unwrap()),
                    PROTOCOL_FEE,
                ),
            ],
        }
    }

    fn asset(asset_id: AssetId, balance: Balance) -> AssetWithBalance {
        AssetWithBalance {
            asset_id,
            balance: U128(balance),
        }
    }

    fn public_pool(token0: AssetId, token1: AssetId) -> Pool {
        Pool::new(
            0,
            PoolData::Public {
                assets: (
                    asset(token0, 10u128.pow(24)),
                    asset(token1, 2 * 10u128.pow(24)),
                ),
                fees: fees(),
                fee_configuration: FeeConfiguration::V1(fees()),
                total_shares: None,
            },
        )
    }

    fn launch_pool(launched: AssetId) -> Pool {
        launch_pool_with_phantom_liquidity(launched, 0)
    }

    fn launch_pool_with_phantom_liquidity(launched: AssetId, phantom_liquidity: Balance) -> Pool {
        Pool::new(
            0,
            PoolData::Launch {
                near_amount: U128(10u128.pow(24)),
                launched_asset: asset(launched, 2 * 10u128.pow(24)),
                fees: fees(),
                fee_configuration: FeeConfiguration::V1(fees()),
                phantom_liquidity_near: U128(phantom_liquidity),
            },
        )
    }

    fn referrer(fee_assets: &[AssetId]) -> Referrer {
        Referrer {
            fee_fraction: REFERRAL_FEE,
            fee_fraction_reduced: REFERRAL_FEE_REDUCED,
            fee_assets: fee_assets.iter().cloned().collect(),
        }
    }

    /// What the contract's x*y=k swap gives for `amount_in` after taking each fee, rounded down
    fn contract_exact_in(
        pool: &Pool,
        token_in: usize,
        amount_in: u128,
        fees: &[FeeFraction],
    ) -> u128 {
        let taken: u128 = fees
            .iter()
            .map(|&fee| amount_in * fee as u128 / MAX_FEE_FRACTION as u128)
            .sum();
        let after_fees = amount_in - taken;
        let (in_balance, out_balance) = (pool.reserves[token_in], pool.reserves[1 - token_in]);
        u256_to_u128(
            u128_to_u256(after_fees) * u128_to_u256(out_balance)
                / (u128_to_u256(in_balance) + u128_to_u256(after_fees)),
        )
    }

    #[test]
    fn referral_fee_is_taken_from_the_input_it_collects_in() {
        let pool = public_pool(nep141("a.near"), nep141("b.near"));
        let referred = pool.with_referrer(&referrer(&[nep141("a.near")]));
        assert_eq!(referred.referral_fees, [REFERRAL_FEE, 0]);

        let amount = 10u128.pow(22);
        assert_eq!(
            referred.quote_exact_in(0, 1, amount),
            Some(contract_exact_in(
                &pool,
                0,
                amount,
                &[POOL_FEE, PROTOCOL_FEE, REFERRAL_FEE]
            ))
        );
        // b.near isn't an asset the referrer collects fees in, so swaps putting it in don't pay one
        assert_eq!(
            referred.quote_exact_in(1, 0, amount),
            pool.quote_exact_in(1, 0, amount)
        );
        assert!(referred.quote_exact_in(0, 1, amount) < pool.quote_exact_in(0, 1, amount));
    }

    #[test]
    fn referral_fee_is_reduced_between_stable_assets() {
        let usdt = nep141("usdt.tether-token.near");
        let usdc = nep141("17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1");
        let pool = public_pool(usdt.clone(), usdc.clone()).with_referrer(&referrer(&[usdt, usdc]));
        assert_eq!(
            pool.referral_fees,
            [REFERRAL_FEE_REDUCED, REFERRAL_FEE_REDUCED]
        );
    }

    #[test]
    fn launch_pools_take_referral_fees_in_their_quote_token() {
        let launched = nep141("meme.near");
        let pool = launch_pool(launched.clone());
        // Buying puts the quote token in, selling converts the fee to it, so only it counts
        assert_eq!(
            pool.with_referrer(&referrer(&[AssetId::Near]))
                .referral_fees,
            [REFERRAL_FEE, REFERRAL_FEE]
        );
        assert_eq!(
            pool.with_referrer(&referrer(&[launched])).referral_fees,
            [0, 0]
        );
    }

    #[test]
    fn exact_out_with_a_referral_fee_pays_enough_for_the_output() {
        let pool = public_pool(nep141("a.near"), nep141("b.near"))
            .with_referrer(&referrer(&[nep141("a.near")]));
        let amount_out = 10u128.pow(22);
        let amount_in = pool.quote_exact_out(0, 1, amount_out).unwrap();
        let without_referral = public_pool(nep141("a.near"), nep141("b.near"))
            .quote_exact_out(0, 1, amount_out)
            .unwrap();
        assert!(amount_in > without_referral);
        assert!(pool.quote_exact_in(0, 1, amount_in).unwrap() >= amount_out);
        assert!(pool.quote_exact_in(0, 1, amount_in - 2).unwrap() < amount_out);
    }

    /// `a * b / (c + d)`, rounded down, like the contract's `mul_div_sum`
    fn mul_div_sum(a: u128, b: u128, c: u128, d: u128) -> u128 {
        u256_to_u128(u128_to_u256(a) * u128_to_u256(b) / (u128_to_u256(c) + u128_to_u256(d)))
    }

    #[test]
    fn pool_fee_stays_in_the_pool() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"));
        let [in_before, out_before] = pool.reserves;
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        let protocol_fee = amount_in * PROTOCOL_FEE as u128 / MAX_FEE_FRACTION as u128;
        assert_eq!(
            pool.reserves,
            [
                in_before + amount_in - protocol_fee,
                out_before - amount_out
            ]
        );

        let mut pool = public_pool(nep141("a.near"), nep141("b.near"));
        let amount_in = pool.swap_exact_out(0, 1, amount_out).unwrap();
        let protocol_fee = amount_in * PROTOCOL_FEE as u128 / MAX_FEE_FRACTION as u128;
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
        let mut pool = launch_pool(launched).with_referrer(&referrer(&[AssetId::Near]));
        let [mut near, mut tokens] = pool.reserves;
        let amount_in = 10u128.pow(22);
        let fee = |fraction: FeeFraction| amount_in * fraction as u128 / MAX_FEE_FRACTION as u128;
        let after_fees = amount_in - fee(POOL_FEE) - fee(PROTOCOL_FEE) - fee(REFERRAL_FEE);
        let amount_out = mul_div_sum(after_fees, near, tokens, after_fees);
        tokens += after_fees + fee(POOL_FEE);
        near -= amount_out;
        // The protocol fee, then the referral fee, each swapped to NEAR on the moved reserves
        for fraction in [PROTOCOL_FEE, REFERRAL_FEE] {
            near -= mul_div_sum(fee(fraction), near, tokens, fee(fraction));
            tokens += fee(fraction);
        }

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
    fn each_referrer_gets_its_own_graph_once() {
        let referrer_id: AccountId = "ref.near".parse().unwrap();
        let pools = build_pools(PoolsSource {
            block_height: 1,
            pools: vec![PoolData::Public {
                assets: (
                    asset(nep141("a.near"), 10u128.pow(24)),
                    asset(nep141("b.near"), 10u128.pow(24)),
                ),
                fees: fees(),
                fee_configuration: FeeConfiguration::V1(fees()),
                total_shares: None,
            }],
            referrers: HashMap::from([(referrer_id.clone(), referrer(&[nep141("a.near")]))]),
        });
        let without_settings = pools.graph_for(&"other.near".parse().unwrap());
        assert!(Arc::ptr_eq(&without_settings, &pools.graph));
        let referred = pools.graph_for(&referrer_id);
        assert!(!Arc::ptr_eq(&referred, &pools.graph));
        assert!(Arc::ptr_eq(&referred, &pools.graph_for(&referrer_id)));
        assert_eq!(referred.pools()[0].referral_fees, [REFERRAL_FEE, 0]);
        assert_eq!(pools.graph.pools()[0].referral_fees, [0, 0]);
    }
}
