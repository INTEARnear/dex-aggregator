#![deny(clippy::float_arithmetic)]

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use bigdecimal::{BigDecimal, RoundingMode};
use borsh::{BorshDeserialize, BorshSerialize};
use crypto_bigint::U256;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, Balance, U128};
use near_min_api::utils::dec_format;
use num_traits::{FromPrimitive, ToPrimitive};
use rand::Rng;
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{self, Display};
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::Instant;
use warp::Filter;

use near_min_api::{QueryFinality, RpcClient, types::Finality};
use serde::{Deserialize, Serialize};
use tracing::{Level, error, info, warn};

#[derive(Serialize)]
struct ApiResponse<T> {
    result_code: i32,
    result_message: String,
    result_data: Option<T>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "quote_type", rename_all = "snake_case")]
enum SplitRouteApiResponse {
    ExactIn {
        routes: Vec<ExactInRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        amount_in: U128,
        amount_out: U128,
    },
    ExactOut {
        routes: Vec<ExactOutRoute>,
        contract_in: AssetId,
        contract_out: AssetId,
        amount_in: U128,
        max_amount_in: U128,
        amount_out: U128,
    },
}

#[derive(Serialize, Debug)]
struct ExactInRoute {
    pools: Vec<ExactInPoolStep>,
    amount_in: U128,
    min_amount_out: U128,
    amount_out: U128,
}

#[derive(Serialize, Debug)]
struct ExactOutRoute {
    pools: Vec<ExactOutPoolStep>,
    amount_in: U128,
    max_amount_in: U128,
    amount_out: U128,
}

#[derive(Serialize, Debug)]
struct ExactInPoolStep {
    #[serde(with = "dec_format")]
    pool_id: u32,
    token_in: AssetId,
    token_out: AssetId,
    amount_in: U128,
    min_amount_out: U128,
}

#[derive(Serialize, Debug)]
struct ExactOutPoolStep {
    #[serde(with = "dec_format")]
    pool_id: u32,
    token_in: AssetId,
    token_out: AssetId,
    amount_in: U128,
    amount_out: U128,
    max_amount_in: U128,
}

const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const SPLIT_ROUTE_STEP_SIZE: u32 = 1; // %
const SMALL_AMOUNT_ROUTE_STEP_SIZE: u32 = 25; // %
const MAX_SPLITS_COUNT: usize = 2;
const FETCH_POOLS_BATCH_SIZE: u32 = 20;
const TOP_ROUTES_COUNT: usize = 2;

const RC_SUCCESS: i32 = 0;
const RC_POOL_FETCH_ERROR: i32 = 1;
const RC_ROUTE_ERROR: i32 = 2;
const RC_RESPONSE_BUILD_ERROR: i32 = 3;
const RC_INVALID_SLIPPAGE: i32 = 4;

#[derive(Debug, Clone)]
pub struct Pool {
    id: u32,
    data: PoolData,
}

#[derive(Debug, BorshDeserialize, PartialEq, Clone)]
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

#[derive(Debug, BorshDeserialize, PartialEq, Clone)]
// #[serde(untagged)]
pub enum FeeConfiguration {
    V1(CurrentFees),
    V2(V1FeeConfiguration),
}

#[derive(Debug, BorshDeserialize, PartialEq, Clone)]
pub struct V1FeeConfiguration {
    receivers: Vec<(FeeReceiver, FeeAmount)>,
}

type Timestamp = u64;

#[derive(Debug, BorshDeserialize, PartialEq, Clone)]
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

#[derive(Debug, BorshDeserialize, PartialEq, Clone)]
pub enum ScheduledFeeCurve {
    Linear,
}

impl FeeAmount {
    pub fn get_fee_fraction(&self) -> FeeFraction {
        match self {
            FeeAmount::Fixed(fee_fraction) => *fee_fraction,
            FeeAmount::Scheduled { start, end, curve } => {
                let (start_time, start_fee_fraction) = *start;
                let (end_time, end_fee_fraction) = *end;
                let current_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as Timestamp;
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

#[derive(Debug, PartialEq, BorshDeserialize, Clone, PartialOrd, Eq, Ord, Hash)]
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

#[derive(Debug, Default)]
struct PoolsDelta {
    changed_pools: HashMap<u32, Pool>,
}

fn u128_to_u256(value: u128) -> U256 {
    U256::from(value)
}

fn u256_to_u128(value: U256) -> u128 {
    assert!(value.bits() <= 128, "Value must be less than 128 bits");
    let bytes = value.to_le_bytes();
    let first_chunk = bytes.first_chunk().unwrap();
    u128::from_le_bytes(*first_chunk)
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
    fn emulate_swap(
        &mut self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let self_before_modifications = self.clone();
        let mut unwind_safe_self = AssertUnwindSafe(&mut *self);
        let result = std::panic::catch_unwind(move || {
            let (asset0_id, asset0_balance, asset1_id, asset1_balance, fees) =
                match &mut unwind_safe_self.data {
                    PoolData::Private { assets, fees, .. }
                    | PoolData::Public { assets, fees, .. } => (
                        assets.0.asset_id.clone(),
                        &mut assets.0.balance,
                        assets.1.asset_id.clone(),
                        &mut assets.1.balance,
                        fees,
                    ),
                    PoolData::Launch {
                        near_amount,
                        launched_asset,
                        fees,
                        ..
                    } => (
                        AssetId::Near,
                        near_amount,
                        launched_asset.asset_id.clone(),
                        &mut launched_asset.balance,
                        fees,
                    ),
                };
            let first_in = match (
                asset0_id == *token_in && asset1_id == *token_out,
                asset1_id == *token_in && asset0_id == *token_out,
            ) {
                (true, false) => true,
                (false, true) => false,
                _ => panic!("Invalid assets or pool ID"),
            };
            let (in_balance, out_balance) = if first_in {
                (&mut asset0_balance.0, &mut asset1_balance.0)
            } else {
                (&mut asset1_balance.0, &mut asset0_balance.0)
            };
            println!("in_balance: {in_balance}");
            println!("out_balance: {out_balance}");
            let amount_in_after_fees = collect_fees(amount_in, fees);
            // u128 * u128 or u128 + u128 can't overflow u256; in_balance was checked to be positive
            #[allow(clippy::arithmetic_side_effects)]
            let amount_out = u256_to_u128(
                u128_to_u256(amount_in_after_fees) * u128_to_u256(*out_balance)
                    / (u128_to_u256(*in_balance) + u128_to_u256(amount_in_after_fees)),
            );
            println!("amount_out: {amount_out}");
            *in_balance = in_balance
                .checked_add(amount_in_after_fees)
                .expect("Overflow");
            *out_balance = out_balance.checked_sub(amount_out).expect("Underflow");
            Ok(amount_out)
        });
        match result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                // println!("Error emulating swap {amount_in} {token_in} -> {token_out}: {e:?}");
                Err(e)
            }
            Err(e) => {
                *self = self_before_modifications;
                warn!(
                    "Panicked while emulating swap {} -> {}",
                    token_in, token_out
                );
                Err(anyhow::anyhow!("Panic: {:?}", e))
            }
        }
    }

    fn emulate_swap_exact_out(
        &mut self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_out: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let self_before_modifications = self.clone();
        let mut unwind_safe_self = AssertUnwindSafe(&mut *self);
        let result = std::panic::catch_unwind(move || {
            if amount_out == 0 {
                return Err(anyhow::anyhow!("Amount must be greater than 0"));
            }

            let (asset0_id, asset0_balance, asset1_id, asset1_balance, fees) =
                match &mut unwind_safe_self.data {
                    PoolData::Private { assets, fees, .. }
                    | PoolData::Public { assets, fees, .. } => (
                        assets.0.asset_id.clone(),
                        &mut assets.0.balance,
                        assets.1.asset_id.clone(),
                        &mut assets.1.balance,
                        fees,
                    ),
                    PoolData::Launch {
                        near_amount,
                        launched_asset,
                        fees,
                        ..
                    } => (
                        AssetId::Near,
                        near_amount,
                        launched_asset.asset_id.clone(),
                        &mut launched_asset.balance,
                        fees,
                    ),
                };
            let first_in = match (
                asset0_id == *token_in && asset1_id == *token_out,
                asset1_id == *token_in && asset0_id == *token_out,
            ) {
                (true, false) => true,
                (false, true) => false,
                _ => panic!("Invalid assets or pool ID"),
            };
            let (in_balance, out_balance) = if first_in {
                (&mut asset0_balance.0, &mut asset1_balance.0)
            } else {
                (&mut asset1_balance.0, &mut asset0_balance.0)
            };
            if amount_out >= *out_balance {
                return Err(anyhow::anyhow!("Amount must be less than out balance"));
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
            Ok(amount_in)
        });
        match result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(e),
            Err(e) => {
                *self = self_before_modifications;
                warn!(
                    "Panicked while emulating swap {} -> {}",
                    token_in, token_out
                );
                Err(anyhow::anyhow!("Panic: {:?}", e))
            }
        }
    }
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

struct Pools {
    pools: Vec<Pool>,
    pools_existing: HashSet<BTreeSet<AssetId>>,
    token_to_pools: HashMap<AssetId, Vec<usize>>,
    pair_to_pools: HashMap<(AssetId, AssetId), Vec<usize>>,
}

impl Pools {
    fn has_direct_pool(&self, token_a: &AssetId, token_b: &AssetId) -> bool {
        let pair = BTreeSet::from([token_a.clone(), token_b.clone()]);
        self.pools_existing.contains(&pair)
    }

    fn direct_pools<'a>(
        &'a self,
        token_a: &AssetId,
        token_b: &AssetId,
    ) -> impl Iterator<Item = &'a Pool> {
        let key = if token_a <= token_b {
            (token_a.clone(), token_b.clone())
        } else {
            (token_b.clone(), token_a.clone())
        };
        self.pair_to_pools
            .get(&key)
            .into_iter()
            .flat_map(move |indices| indices.iter().map(move |&idx| &self.pools[idx]))
    }

    fn pools_with_token<'a>(&'a self, token: &AssetId) -> impl Iterator<Item = &'a Pool> {
        self.token_to_pools
            .get(token)
            .into_iter()
            .flat_map(move |indices| indices.iter().map(move |&idx| &self.pools[idx]))
    }
}

async fn get_all_pools(client: &RpcClient) -> Result<Pools, anyhow::Error> {
    let number_of_pools: String = client
        .call(
            INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
            "dex_view",
            json!({
                "dex_id": PLACH_DEX_ID,
                "method": "get_pool_count",
                "args": BASE64_STANDARD.encode(borsh::to_vec(&()).unwrap()),
            }),
            QueryFinality::Finality(Finality::None),
        )
        .await?;
    let number_of_pools = BASE64_STANDARD.decode(number_of_pools)?;
    let number_of_pools = borsh::from_slice(&number_of_pools)?;

    let mut pools_batch_requests = Vec::new();

    for i in (0..number_of_pools).step_by(FETCH_POOLS_BATCH_SIZE as usize) {
        #[derive(BorshSerialize)]
        struct RequestArgs {
            start_index: u32,
            limit: u32,
        }
        let request_args = RequestArgs {
            start_index: i,
            limit: FETCH_POOLS_BATCH_SIZE,
        };

        pools_batch_requests.push((
            INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
            "dex_view",
            json!({
                "dex_id": PLACH_DEX_ID,
                "method": "get_pools",
                "args": BASE64_STANDARD.encode(borsh::to_vec(&request_args).unwrap()),
            }),
            QueryFinality::Finality(Finality::None),
        ));
    }

    let batches: Vec<Result<String, _>> = client.batch_call(pools_batch_requests).await?;
    let mut pools = Vec::new();
    for result in batches {
        let pool_batch = result?;
        let pool_batch = BASE64_STANDARD.decode(pool_batch)?;
        let pool_batch: Vec<PoolData> = borsh::from_slice(&pool_batch)?;
        pools.extend(pool_batch);
    }
    let pools = pools
        .into_iter()
        .enumerate()
        .map(|(i, data)| Pool { id: i as u32, data })
        .collect::<Vec<_>>();

    let pools_existing = {
        let mut pools_existing = HashSet::new();
        for pool in pools.iter() {
            let (asset0_id, asset1_id) = match &pool.data {
                PoolData::Private { assets, .. } | PoolData::Public { assets, .. } => {
                    (assets.0.asset_id.clone(), assets.1.asset_id.clone())
                }
                PoolData::Launch { launched_asset, .. } => {
                    (AssetId::Near, launched_asset.asset_id.clone())
                }
            };
            pools_existing.insert(BTreeSet::from_iter([asset0_id.clone(), asset1_id.clone()]));
            pools_existing.insert(BTreeSet::from_iter([asset1_id, asset0_id]));
        }
        pools_existing
    };

    let mut token_to_pools: HashMap<AssetId, Vec<usize>> = HashMap::new();
    let mut pair_to_pools: HashMap<(AssetId, AssetId), Vec<usize>> = HashMap::new();

    for (idx, pool) in pools.iter().enumerate() {
        let (asset0_id, asset1_id) = match &pool.data {
            PoolData::Private { assets, .. } | PoolData::Public { assets, .. } => {
                (assets.0.asset_id.clone(), assets.1.asset_id.clone())
            }
            PoolData::Launch { launched_asset, .. } => {
                (AssetId::Near, launched_asset.asset_id.clone())
            }
        };
        token_to_pools
            .entry(asset0_id.clone())
            .or_default()
            .push(idx);
        token_to_pools
            .entry(asset1_id.clone())
            .or_default()
            .push(idx);

        let mut tokens_sorted = [asset0_id, asset1_id];
        tokens_sorted.sort_unstable();
        let key = (tokens_sorted[0].clone(), tokens_sorted[1].clone());
        pair_to_pools.entry(key).or_default().push(idx);
    }
    info!("Updated {number_of_pools} pools");

    Ok(Pools {
        pools,
        pools_existing,
        token_to_pools,
        pair_to_pools,
    })
}

async fn start_pools_update_task(client: Arc<RpcClient>) {
    tokio::spawn(async move {
        loop {
            match get_all_pools(&client).await {
                Ok(pools) => {
                    let mut cache = POOLS_CACHE.write().await;
                    *cache = Some(Arc::new(pools));
                }
                Err(e) => {
                    error!("Failed to update pools: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

async fn get_pools() -> Result<Arc<Pools>, anyhow::Error> {
    loop {
        if let Some(pools) = POOLS_CACHE.read().await.as_ref() {
            return Ok(Arc::clone(pools));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

    info!("Server listening on http://localhost:12346/findPath ...");
    warp::serve(api).run(([127, 0, 0, 1], 12346)).await;

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
}

async fn route<'a>(
    token_in: &'a AssetId,
    token_out: &'a AssetId,
    amount: QuoteAmount,
    pools: &'a Pools,
    max_hops: MaxHops,
) -> Result<SplitRoute<'a>, anyhow::Error> {
    let span = tracing::span!(
        Level::INFO,
        "find_best_routes",
        token_in = token_in.to_string(),
        token_out = token_out.to_string(),
        amount = amount.value(),
    );
    let _enter = span.enter();
    let now = Instant::now();
    let routes = find_best_routes(
        pools,
        amount,
        token_in,
        token_out,
        max_hops,
        TOP_ROUTES_COUNT,
    )?;
    info!("Found routes: {routes:#?}");
    let duration = now.elapsed();
    info!("Time to find best routes: {:?}", duration);

    if routes.is_empty() {
        return Err(anyhow::anyhow!("No routes found"));
    }

    let now = Instant::now();
    let Some(best_split) = find_best_split_route(routes.clone(), amount, token_in, token_out)
    else {
        return Err(anyhow::anyhow!("No valid split route found"));
    };
    let duration = now.elapsed();
    info!("Time to find best split route {best_split:#?} {duration:?}");

    let best_split_metric =
        best_split.emulate_swap(token_in, token_out, amount, &mut PoolsDelta::default())?;

    if best_split_metric == 0 {
        warn!("Estimated amount is 0");
        return Err(anyhow::anyhow!("Estimated amount is 0"));
    }

    // There could be a bug, sometimes a bad route is chose. Make sure the best
    // route is at least better or equal to the simplest (top 1) route.
    let mut best_split = best_split;
    let mut best_split_metric = best_split_metric;
    for route in routes {
        let single_split = SplitRoute::new(vec![SplitRouteStep { route, weight: 100 }]);
        let single_split_metric = if let Ok(metric) =
            single_split.emulate_swap(token_in, token_out, amount, &mut PoolsDelta::default())
        {
            metric
        } else {
            continue;
        };
        let is_better = match amount {
            QuoteAmount::ExactIn(_) => single_split_metric > best_split_metric,
            QuoteAmount::ExactOut(_) => single_split_metric < best_split_metric,
        };
        if is_better {
            best_split = single_split;
            best_split_metric = single_split_metric;
        }
    }
    info!("Best split route: {best_split:#?} {best_split_metric:?}");

    Ok(best_split)
}

#[derive(Debug, Clone)]
struct SplitRoute<'a> {
    steps: Vec<SplitRouteStep<'a>>,
}

impl<'a> Display for SplitRoute<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Split Route:")?;
        for (i, step) in self.steps.iter().enumerate() {
            write!(f, "  {}% {}", step.weight, step.route)?;
            if i < self.steps.len() - 1 {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

impl<'a> SplitRoute<'a> {
    fn new(steps: Vec<SplitRouteStep<'a>>) -> Self {
        Self { steps }
    }

    fn emulate_swap(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount: QuoteAmount,
        pools_delta: &mut PoolsDelta,
    ) -> Result<Balance, anyhow::Error> {
        match amount {
            QuoteAmount::ExactIn(amount_in) => {
                self.emulate_swap_exact_in(token_in, token_out, amount_in, pools_delta)
            }
            QuoteAmount::ExactOut(amount_out) => {
                self.emulate_swap_exact_out(token_in, token_out, amount_out, pools_delta)
            }
        }
    }

    fn emulate_swap_exact_in(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_in: Balance,
        pools_delta: &mut PoolsDelta,
    ) -> Result<Balance, anyhow::Error> {
        let mut total_out: Balance = 0;
        for step in &self.steps {
            let amount_part = u256_to_u128(
                u128_to_u256(amount_in) * u128_to_u256(step.weight as u128) / u128_to_u256(100),
            );
            let out =
                step.route
                    .emulate_swap_exact_in(token_in, token_out, amount_part, pools_delta)?;
            total_out += out;
        }
        Ok(total_out)
    }

    fn emulate_swap_exact_out(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_out: Balance,
        pools_delta: &mut PoolsDelta,
    ) -> Result<Balance, anyhow::Error> {
        let mut total_in: Balance = 0;
        for step in &self.steps {
            let amount_part_out = u256_to_u128(
                u128_to_u256(amount_out) * u128_to_u256(step.weight as u128) / u128_to_u256(100),
            );
            let amount_in = step
                .route
                .emulate_swap_exact_out(token_in, token_out, amount_part_out, pools_delta)?
                .0;
            total_in = total_in
                .checked_add(amount_in)
                .ok_or_else(|| anyhow::anyhow!("Total estimated in overflows u128"))?;
        }
        Ok(total_in)
    }
}

impl<'a> SplitRoute<'a> {
    fn to_api_response(
        &self,
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
                let mut amount_parts: Vec<Balance> = self
                    .steps
                    .iter()
                    .map(|step| {
                        u256_to_u128(
                            u128_to_u256(total_amount_in) * u128_to_u256(step.weight as u128)
                                / u128_to_u256(100),
                        )
                    })
                    .collect();
                let total_split_amount_in: Balance = amount_parts.iter().copied().sum();
                if total_split_amount_in < total_amount_in {
                    let leftover = total_amount_in - total_split_amount_in;
                    const ROUNDING_ERROR_THRESHOLD: u128 = 50;
                    if leftover < ROUNDING_ERROR_THRESHOLD {
                        if let Some(first_amount) = amount_parts.first_mut() {
                            *first_amount = first_amount.checked_add(leftover).unwrap_or_else(|| {
                                panic!(
                                    "Overflow while adding leftover to first split: first={}, leftover={}",
                                    *first_amount, leftover
                                )
                            });
                        } else {
                            panic!(
                                "No split steps available to apply leftover amount: leftover={}",
                                leftover
                            );
                        }
                    } else {
                        panic!(
                            "Total split amount_in is less than requested amount_in by too much: split_total={}, requested={}, leftover={}",
                            total_split_amount_in, total_amount_in, leftover
                        );
                    }
                } else if total_split_amount_in > total_amount_in {
                    panic!(
                        "Total split amount_in exceeds requested amount_in: split_total={}, requested={}",
                        total_split_amount_in, total_amount_in
                    );
                }

                for (step, amount_part) in self.steps.iter().zip(amount_parts) {
                    let estimated_out = step.route.emulate_swap_exact_in(
                        token_in,
                        token_out,
                        amount_part,
                        &mut pools_delta,
                    )?;
                    let min_amount_out = estimated_out * (10_000u128 - slippage_bp) / 10_000u128;

                    let mut pools_resp = Vec::new();
                    let mut current_token = token_in.clone();
                    for (idx, route_step) in step.route.steps.iter().enumerate() {
                        let is_first = idx == 0;
                        let is_last = idx + 1 == step.route.steps.len();

                        pools_resp.push(ExactInPoolStep {
                            pool_id: route_step.pool.id,
                            token_in: current_token.clone(),
                            token_out: route_step.token_out.to_owned(),
                            amount_in: U128(if is_first { amount_part } else { 0 }),
                            min_amount_out: U128(if is_last { min_amount_out } else { 0 }),
                        });

                        current_token = route_step.token_out.to_owned();
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
                })
            }
            QuoteAmount::ExactOut(total_amount_out) => {
                let mut routes_resp = Vec::new();
                let mut total_estimated_in: Balance = 0;
                let mut total_max_amount_in: Balance = 0;
                let mut pools_delta = PoolsDelta::default();
                for step in &self.steps {
                    let amount_out_part = u256_to_u128(
                        u128_to_u256(total_amount_out) * u128_to_u256(step.weight as u128)
                            / u128_to_u256(100),
                    );
                    let (amount_in, step_amounts) = step.route.emulate_swap_exact_out(
                        token_in,
                        token_out,
                        amount_out_part,
                        &mut pools_delta,
                    )?;
                    let max_amount_in = u256_to_u128(
                        u128_to_u256(amount_in) * u128_to_u256(10_000u128 + slippage_bp)
                            / u128_to_u256(10_000),
                    );

                    let mut pools_resp = Vec::new();
                    let mut current_token = token_in.clone();
                    for (idx, route_step) in step.route.steps.iter().enumerate() {
                        let step_amount = step_amounts
                            .get(idx)
                            .ok_or_else(|| anyhow::anyhow!("Missing step amount"))?;

                        pools_resp.push(ExactOutPoolStep {
                            pool_id: route_step.pool.id,
                            token_in: current_token.clone(),
                            token_out: route_step.token_out.to_owned(),
                            amount_in: U128(step_amount.0),
                            amount_out: U128(step_amount.1),
                            max_amount_in: U128(u256_to_u128(
                                u128_to_u256(step_amount.0)
                                    * u128_to_u256(10_000u128 + slippage_bp)
                                    / u128_to_u256(10_000),
                            )),
                        });

                        current_token = route_step.token_out.to_owned();
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
                })
            }
        }
    }
}

fn find_best_split_route<'a>(
    routes: Vec<Route<'a>>,
    total_amount: QuoteAmount,
    token_in: &'a AssetId,
    token_out: &'a AssetId,
) -> Option<SplitRoute<'a>> {
    if routes.is_empty() {
        return None;
    }
    const SMALL_AMOUNT_THRESHOLD: Balance = 1000;
    let is_small_amount = match total_amount {
        QuoteAmount::ExactIn(amount) => {
            amount < SMALL_AMOUNT_THRESHOLD
                || routes[0]
                    .emulate_swap_exact_in(token_in, token_out, amount, &mut PoolsDelta::default())
                    .map_or(true, |t| t < SMALL_AMOUNT_THRESHOLD)
        }
        QuoteAmount::ExactOut(amount) => {
            amount < SMALL_AMOUNT_THRESHOLD
                || routes[0]
                    .emulate_swap_exact_out(token_in, token_out, amount, &mut PoolsDelta::default())
                    .map_or(true, |t| t.0 < SMALL_AMOUNT_THRESHOLD)
        }
    };
    let step = if is_small_amount {
        SMALL_AMOUNT_ROUTE_STEP_SIZE
    } else {
        SPLIT_ROUTE_STEP_SIZE
    };
    let slices = 100 / step;
    let mut weights: Vec<u32> = vec![0; routes.len()];

    let mut best_split: Option<SplitRoute<'a>> = None;
    let mut best_metric: Balance = match total_amount {
        QuoteAmount::ExactIn(_) => 0,
        QuoteAmount::ExactOut(_) => Balance::MAX,
    };

    for _ in 0..slices {
        let mut local_best_metric = match total_amount {
            QuoteAmount::ExactIn(_) => best_metric,
            QuoteAmount::ExactOut(_) => Balance::MAX,
        };
        let mut local_best_idx: Option<usize> = None;

        for (idx, _route) in routes.iter().enumerate() {
            if weights[idx] == 0 {
                // Route not yet chosen, check if we can still add new route
                if weights.iter().filter(|&&w| w > 0).count() >= MAX_SPLITS_COUNT {
                    continue;
                }
            }
            if weights[idx] + step > 100 {
                continue;
            }
            let mut candidate_weights = weights.clone();
            candidate_weights[idx] += step;
            // Build candidate split route
            let mut candidate_steps: Vec<SplitRouteStep<'a>> = routes
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let w = candidate_weights[i];
                    if w > 0 {
                        Some(SplitRouteStep {
                            route: r.clone(),
                            weight: w,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            candidate_steps.sort_by_key(|s| std::cmp::Reverse(s.weight));
            let candidate_split = SplitRoute::new(candidate_steps);
            if let Ok(metric) = candidate_split.emulate_swap(
                token_in,
                token_out,
                total_amount,
                &mut PoolsDelta::default(),
            ) {
                let is_better = match total_amount {
                    QuoteAmount::ExactIn(_) => metric > local_best_metric,
                    QuoteAmount::ExactOut(_) => metric < local_best_metric,
                };
                if is_better {
                    local_best_metric = metric;
                    local_best_idx = Some(idx);
                }
            }
        }

        if let Some(idx) = local_best_idx {
            // Accept the improvement
            weights[idx] += step;
            best_metric = local_best_metric;
            // Rebuild best_split
            let mut steps: Vec<SplitRouteStep<'a>> = routes
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let w = weights[i];
                    if w > 0 {
                        Some(SplitRouteStep {
                            route: r.clone(),
                            weight: w,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            steps.sort_by_key(|s| std::cmp::Reverse(s.weight));
            best_split = Some(SplitRoute::new(steps));
        } else {
            // No further improvement found
            break;
        }
    }

    if weights.iter().copied().sum::<u32>() != 100 {
        return None;
    }

    best_split
}

#[derive(Debug, Clone)]
struct SplitRouteStep<'a> {
    route: Route<'a>,
    weight: u32,
}

#[derive(Debug, Clone)]
struct Route<'a> {
    steps: Vec<RouteStep<'a>>,
}

#[derive(Debug, Clone, Copy)]
struct RouteStep<'a> {
    pool: &'a Pool,
    token_in: &'a AssetId,
    token_out: &'a AssetId,
}

impl<'a> Display for Route<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(first_step) = self.steps.first() else {
            return write!(f, "(empty route)");
        };
        f.write_str("Route: ")?;
        f.write_str(first_step.token_in.to_string().as_str())?;
        for step in self.steps.iter() {
            write!(f, " --- ({}) ---> {}", step.pool.id, step.token_out)?;
        }
        Ok(())
    }
}

impl Route<'_> {
    fn emulate_swap_exact_in(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_in: Balance,
        pools_delta: &mut PoolsDelta,
    ) -> Result<Balance, anyhow::Error> {
        let mut current_token = token_in;
        let mut current_amount = amount_in;
        for step in self.steps.iter() {
            if current_token != step.token_in {
                return Err(anyhow::anyhow!("Invalid route"));
            }
            let pool = if let Some(pool) = pools_delta.changed_pools.get(&step.pool.id) {
                pool
            } else {
                step.pool
            };
            let mut new_pool = pool.clone();

            current_amount =
                new_pool.emulate_swap(current_token, step.token_out, current_amount)?;

            pools_delta.changed_pools.insert(new_pool.id, new_pool);
            current_token = step.token_out;
        }
        if current_token != token_out {
            return Err(anyhow::anyhow!("Invalid route"));
        }
        Ok(current_amount)
    }

    fn emulate_swap_exact_out(
        &self,
        token_in: &AssetId,
        token_out: &AssetId,
        amount_out: Balance,
        pools_delta: &mut PoolsDelta,
    ) -> Result<(Balance, Vec<(Balance, Balance)>), anyhow::Error> {
        let mut current_token = token_out;
        let mut current_amount_out = amount_out;
        let mut step_amounts = Vec::with_capacity(self.steps.len());
        for step in self.steps.iter().rev() {
            if current_token != step.token_out {
                return Err(anyhow::anyhow!("Invalid route"));
            }
            let pool = if let Some(pool) = pools_delta.changed_pools.get(&step.pool.id) {
                pool
            } else {
                step.pool
            };
            let mut new_pool = pool.clone();
            let amount_in = new_pool.emulate_swap_exact_out(
                step.token_in,
                step.token_out,
                current_amount_out,
            )?;
            pools_delta.changed_pools.insert(new_pool.id, new_pool);
            step_amounts.push((amount_in, current_amount_out));
            current_amount_out = amount_in;
            current_token = step.token_in;
        }
        if current_token != token_in {
            return Err(anyhow::anyhow!("Invalid route"));
        }
        step_amounts.reverse();
        Ok((current_amount_out, step_amounts))
    }
}

#[derive(Deserialize, Debug)]
enum MaxHops {
    DirectOnly,
    Two,
    Three,
    Four,
    Max,
}

fn select_top_k<T, F, K>(mut items: Vec<T>, k: usize, key_fn: F) -> Vec<T>
where
    F: Fn(&T) -> K,
    K: Ord + Clone,
{
    if k == 0 {
        panic!("k cannot be 0");
    }

    if items.len() <= k {
        items.sort_unstable_by_key(|a| std::cmp::Reverse(key_fn(a))); // descending order
        return items;
    }

    items.select_nth_unstable_by_key(k - 1, |a| std::cmp::Reverse(key_fn(a)));

    items.truncate(k);
    items.sort_unstable_by_key(|a| std::cmp::Reverse(key_fn(a))); // descending order

    items
}

fn find_best_route<'a>(
    pools: &'a Pools,
    amount: QuoteAmount,
    token_in: &'a AssetId,
    token_out: &'a AssetId,
    max_hops: MaxHops,
) -> Result<Route<'a>, anyhow::Error> {
    let routes = find_best_routes(pools, amount, token_in, token_out, max_hops, 1)?;
    if let [route, ..] = &routes[..] {
        return Ok(route.clone());
    }
    Err(anyhow::anyhow!("No route found"))
}

fn find_best_routes<'a>(
    pools: &'a Pools,
    amount: QuoteAmount,
    token_in: &'a AssetId,
    token_out: &'a AssetId,
    max_hops: MaxHops,
    count: usize,
) -> Result<Vec<Route<'a>>, anyhow::Error> {
    if !pools.has_direct_pool(token_in, token_out) && matches!(max_hops, MaxHops::DirectOnly) {
        return Ok(Vec::new());
    }
    let single_pool_candidates = select_top_k(
        pools.direct_pools(token_in, token_out).collect(),
        3,
        |pool| {
            let metric = match amount {
                QuoteAmount::ExactIn(amount_in) => (*pool)
                    .clone()
                    .emulate_swap(token_in, token_out, amount_in)
                    .ok(),
                QuoteAmount::ExactOut(amount_out) => (*pool)
                    .clone()
                    .emulate_swap_exact_out(token_in, token_out, amount_out)
                    .ok(),
            };
            match amount {
                QuoteAmount::ExactIn(_) => metric
                    .and_then(|value| i128::try_from(value).ok())
                    .unwrap_or_default(),
                QuoteAmount::ExactOut(_) => metric
                    .and_then(|value| i128::try_from(value).ok())
                    .map(|value| -value)
                    .unwrap_or(i128::MIN),
            }
        },
    );

    let single_pool_routes = single_pool_candidates
        .into_iter()
        .map(|pool| Route {
            steps: vec![RouteStep {
                pool,
                token_in,
                token_out,
            }],
        })
        .collect::<Vec<_>>();

    if matches!(max_hops, MaxHops::DirectOnly) {
        // already sorted
        return Ok(single_pool_routes.into_iter().take(count).collect());
    }

    // Two hops

    let starting_pool_candidates = pools.pools_with_token(token_in);
    let ending_pool_candidates = pools.pools_with_token(token_out);

    let starting_pool_tokens = starting_pool_candidates
        .flat_map(|pool| match &pool.data {
            PoolData::Private { assets, .. } | PoolData::Public { assets, .. } => {
                vec![&assets.0.asset_id, &assets.1.asset_id]
            }
            PoolData::Launch { launched_asset, .. } => {
                vec![&AssetId::Near, &launched_asset.asset_id]
            }
        })
        .collect::<HashSet<_>>();
    let ending_pool_tokens = ending_pool_candidates
        .flat_map(|pool| match &pool.data {
            PoolData::Private { assets, .. } | PoolData::Public { assets, .. } => {
                vec![&assets.0.asset_id, &assets.1.asset_id]
            }
            PoolData::Launch { launched_asset, .. } => {
                vec![&AssetId::Near, &launched_asset.asset_id]
            }
        })
        .collect::<HashSet<_>>();
    let possible_intermediate_tokens = starting_pool_tokens
        .intersection(&ending_pool_tokens)
        .filter(|&&token| token != token_in && token != token_out)
        .collect::<Vec<_>>();

    let mut routes = Vec::new();
    routes.extend(single_pool_routes);

    match amount {
        QuoteAmount::ExactIn(amount_in) => {
            for intermediate_token in possible_intermediate_tokens {
                if !pools.has_direct_pool(token_in, intermediate_token)
                    || !pools.has_direct_pool(intermediate_token, token_out)
                {
                    continue;
                }

                let Ok(first_route) = find_best_route(
                    pools,
                    QuoteAmount::ExactIn(amount_in),
                    token_in,
                    intermediate_token,
                    MaxHops::DirectOnly,
                ) else {
                    continue;
                };
                let Ok(intermediate_amount) = first_route.emulate_swap_exact_in(
                    token_in,
                    intermediate_token,
                    amount_in,
                    &mut PoolsDelta::default(),
                ) else {
                    continue;
                };
                let Ok(second_route) = find_best_route(
                    pools,
                    QuoteAmount::ExactIn(intermediate_amount),
                    intermediate_token,
                    token_out,
                    MaxHops::DirectOnly,
                ) else {
                    continue;
                };

                let mut combined_steps = Vec::new();
                combined_steps.extend(first_route.steps);
                combined_steps.extend(second_route.steps);

                routes.push(Route {
                    steps: combined_steps,
                });
            }

            if matches!(max_hops, MaxHops::Three | MaxHops::Four | MaxHops::Max) {
                // Three hops
                for first_intermediate_token in starting_pool_tokens.iter() {
                    if *first_intermediate_token == token_in
                        || *first_intermediate_token == token_out
                    {
                        continue;
                    }
                    for second_intermediate_token in ending_pool_tokens.iter() {
                        if *second_intermediate_token == token_in
                            || *second_intermediate_token == token_out
                        {
                            continue;
                        }
                        if first_intermediate_token == second_intermediate_token {
                            continue;
                        }

                        if !pools.has_direct_pool(token_in, first_intermediate_token)
                            || !pools.has_direct_pool(
                                first_intermediate_token,
                                second_intermediate_token,
                            )
                            || !pools.has_direct_pool(second_intermediate_token, token_out)
                        {
                            continue;
                        }

                        let Ok(in_to_first) = find_best_route(
                            pools,
                            QuoteAmount::ExactIn(amount_in),
                            token_in,
                            first_intermediate_token,
                            match max_hops {
                                MaxHops::Three | MaxHops::Four => MaxHops::DirectOnly,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let Ok(first_intermediate_amount) = in_to_first.emulate_swap_exact_in(
                            token_in,
                            first_intermediate_token,
                            amount_in,
                            &mut PoolsDelta::default(),
                        ) else {
                            continue;
                        };
                        let Ok(first_to_second) = find_best_route(
                            pools,
                            QuoteAmount::ExactIn(first_intermediate_amount),
                            first_intermediate_token,
                            second_intermediate_token,
                            match max_hops {
                                MaxHops::Three => MaxHops::DirectOnly,
                                MaxHops::Four => MaxHops::Two,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let Ok(second_intermediate_amount) = first_to_second.emulate_swap_exact_in(
                            first_intermediate_token,
                            second_intermediate_token,
                            first_intermediate_amount,
                            &mut PoolsDelta::default(),
                        ) else {
                            continue;
                        };
                        let Ok(second_to_out) = find_best_route(
                            pools,
                            QuoteAmount::ExactIn(second_intermediate_amount),
                            second_intermediate_token,
                            token_out,
                            match max_hops {
                                MaxHops::Three | MaxHops::Four => MaxHops::DirectOnly,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let mut combined_steps = Vec::new();
                        combined_steps.extend(in_to_first.steps);
                        combined_steps.extend(first_to_second.steps);
                        combined_steps.extend(second_to_out.steps);
                        routes.push(Route {
                            steps: combined_steps,
                        });
                    }
                }
            }
        }
        QuoteAmount::ExactOut(amount_out) => {
            for intermediate_token in possible_intermediate_tokens {
                if !pools.has_direct_pool(token_in, intermediate_token)
                    || !pools.has_direct_pool(intermediate_token, token_out)
                {
                    continue;
                }

                let Ok(second_route) = find_best_route(
                    pools,
                    QuoteAmount::ExactOut(amount_out),
                    intermediate_token,
                    token_out,
                    MaxHops::DirectOnly,
                ) else {
                    continue;
                };
                let Ok((intermediate_amount, _)) = second_route.emulate_swap_exact_out(
                    intermediate_token,
                    token_out,
                    amount_out,
                    &mut PoolsDelta::default(),
                ) else {
                    continue;
                };
                let Ok(first_route) = find_best_route(
                    pools,
                    QuoteAmount::ExactOut(intermediate_amount),
                    token_in,
                    intermediate_token,
                    MaxHops::DirectOnly,
                ) else {
                    continue;
                };

                let mut combined_steps = Vec::new();
                combined_steps.extend(first_route.steps);
                combined_steps.extend(second_route.steps);

                routes.push(Route {
                    steps: combined_steps,
                });
            }

            if matches!(max_hops, MaxHops::Three | MaxHops::Four | MaxHops::Max) {
                // Three hops
                for first_intermediate_token in starting_pool_tokens.iter() {
                    if *first_intermediate_token == token_in
                        || *first_intermediate_token == token_out
                    {
                        continue;
                    }
                    for second_intermediate_token in ending_pool_tokens.iter() {
                        if *second_intermediate_token == token_in
                            || *second_intermediate_token == token_out
                        {
                            continue;
                        }
                        if first_intermediate_token == second_intermediate_token {
                            continue;
                        }

                        if !pools.has_direct_pool(token_in, first_intermediate_token)
                            || !pools.has_direct_pool(
                                first_intermediate_token,
                                second_intermediate_token,
                            )
                            || !pools.has_direct_pool(second_intermediate_token, token_out)
                        {
                            continue;
                        }

                        let Ok(second_to_out) = find_best_route(
                            pools,
                            QuoteAmount::ExactOut(amount_out),
                            second_intermediate_token,
                            token_out,
                            match max_hops {
                                MaxHops::Three | MaxHops::Four => MaxHops::DirectOnly,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let Ok((second_intermediate_amount, _)) = second_to_out
                            .emulate_swap_exact_out(
                                second_intermediate_token,
                                token_out,
                                amount_out,
                                &mut PoolsDelta::default(),
                            )
                        else {
                            continue;
                        };
                        let Ok(first_to_second) = find_best_route(
                            pools,
                            QuoteAmount::ExactOut(second_intermediate_amount),
                            first_intermediate_token,
                            second_intermediate_token,
                            match max_hops {
                                MaxHops::Three => MaxHops::DirectOnly,
                                MaxHops::Four => MaxHops::Two,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let Ok((first_intermediate_amount, _)) = first_to_second
                            .emulate_swap_exact_out(
                                first_intermediate_token,
                                second_intermediate_token,
                                second_intermediate_amount,
                                &mut PoolsDelta::default(),
                            )
                        else {
                            continue;
                        };
                        let Ok(in_to_first) = find_best_route(
                            pools,
                            QuoteAmount::ExactOut(first_intermediate_amount),
                            token_in,
                            first_intermediate_token,
                            match max_hops {
                                MaxHops::Three | MaxHops::Four => MaxHops::DirectOnly,
                                MaxHops::Max => MaxHops::Three,
                                _ => unreachable!(),
                            },
                        ) else {
                            continue;
                        };
                        let mut combined_steps = Vec::new();
                        combined_steps.extend(in_to_first.steps);
                        combined_steps.extend(first_to_second.steps);
                        combined_steps.extend(second_to_out.steps);
                        routes.push(Route {
                            steps: combined_steps,
                        });
                    }
                }
            }
        }
    }

    if matches!(max_hops, MaxHops::Three | MaxHops::Four | MaxHops::Max) {
        info!("Choosing from {} routes", routes.len());
    }

    // Remove duplicate tokens
    routes.retain(|route| {
        if route.steps.is_empty() {
            return false;
        }
        let mut seen: HashSet<&AssetId> = HashSet::new();
        std::iter::once(route.steps[0].token_in)
            .chain(route.steps.iter().map(|s| s.token_out))
            .all(|token| seen.insert(token))
    });

    let best_routes = select_top_k(routes, count, |route| {
        let metric = match amount {
            QuoteAmount::ExactIn(amount_in) => route
                .emulate_swap_exact_in(token_in, token_out, amount_in, &mut PoolsDelta::default())
                .ok(),
            QuoteAmount::ExactOut(amount_out) => route
                .emulate_swap_exact_out(token_in, token_out, amount_out, &mut PoolsDelta::default())
                .map(|(amount, _)| amount)
                .ok(),
        };
        match amount {
            QuoteAmount::ExactIn(_) => metric
                .and_then(|value| i128::try_from(value).ok())
                .unwrap_or_default(),
            QuoteAmount::ExactOut(_) => metric
                .and_then(|value| i128::try_from(value).ok())
                .map(|value| -value)
                .unwrap_or(i128::MIN),
        }
    });
    Ok(best_routes.clone())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindPathQuery {
    #[serde(default, with = "dec_format")]
    amount_in: Option<Balance>,
    #[serde(default, with = "dec_format")]
    amount_out: Option<Balance>,
    token_in: AssetId,
    token_out: AssetId,
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
            let resp: ApiResponse<()> = ApiResponse {
                result_code: RC_ROUTE_ERROR,
                result_message: "Provide either amountIn or amountOut".into(),
                result_data: None,
            };
            return Ok(warp::reply::json(&resp));
        }
    };

    if query.slippage < 0 || query.slippage > 1 {
        let resp: ApiResponse<()> = ApiResponse {
            result_code: RC_INVALID_SLIPPAGE,
            result_message: "Invalid slippage".into(),
            result_data: None,
        };
        return Ok(warp::reply::json(&resp));
    }
    let slippage_bp = (query.slippage * BigDecimal::from_u32(10_000).unwrap())
        .with_scale_round(0, RoundingMode::Down)
        .to_u128()
        .unwrap();

    match get_pools().await {
        Ok(pools) => match route(
            &query.token_in,
            &query.token_out,
            amount,
            &pools,
            query.max_hops,
        )
        .await
        {
            Ok(split_route) => {
                let estimated_amount = split_route
                    .emulate_swap(
                        &query.token_in,
                        &query.token_out,
                        amount,
                        &mut PoolsDelta::default(),
                    )
                    .unwrap_or(0);
                match amount {
                    QuoteAmount::ExactIn(_) => {
                        info!(
                            "Request id: {:06}, Estimated amount out: {}",
                            request_id, estimated_amount
                        );
                    }
                    QuoteAmount::ExactOut(_) => {
                        info!(
                            "Request id: {:06}, Estimated amount in: {}",
                            request_id, estimated_amount
                        );
                    }
                }

                match split_route.to_api_response(
                    &query.token_in,
                    &query.token_out,
                    amount,
                    slippage_bp,
                ) {
                    Ok(data) => {
                        let resp = ApiResponse {
                            result_code: RC_SUCCESS,
                            result_message: "".into(),
                            result_data: Some(data),
                        };
                        Ok(warp::reply::json(&resp))
                    }
                    Err(e) => {
                        let resp: ApiResponse<()> = ApiResponse {
                            result_code: RC_RESPONSE_BUILD_ERROR,
                            result_message: e.to_string(),
                            result_data: None,
                        };
                        Ok(warp::reply::json(&resp))
                    }
                }
            }
            Err(e) => {
                let resp: ApiResponse<()> = ApiResponse {
                    result_code: RC_ROUTE_ERROR,
                    result_message: e.to_string(),
                    result_data: None,
                };
                Ok(warp::reply::json(&resp))
            }
        },
        Err(e) => {
            let resp: ApiResponse<()> = ApiResponse {
                result_code: RC_POOL_FETCH_ERROR,
                result_message: e.to_string(),
                result_data: None,
            };
            Ok(warp::reply::json(&resp))
        }
    }
}
