mod degen;
mod rated;
mod stable;

use itertools::Itertools;
use lazy_static::lazy_static;
use near_min_api::types::{AccountId, AccountIdRef, Balance};
use near_min_api::utils::dec_format;
use rand::Rng;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;
use uint::construct_uint;
use warp::Filter;

use near_min_api::{QueryFinality, RpcClient, types::Finality};
use serde::{Deserialize, Serialize};

use crate::degen::DegenSwap;
use crate::rated::RatedSwap;
use crate::stable::StableSwap;

#[derive(Serialize)]
struct ApiResponse<T> {
    result_code: i32,
    result_message: String,
    result_data: Option<T>,
}

#[derive(Serialize)]
struct SplitRouteApiResponse {
    routes: Vec<ApiResponseROute>,
    contract_in: AccountId,
    contract_out: AccountId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Serialize)]
struct ApiResponseROute {
    pools: Vec<ApiResponsePoolStep>,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
}

#[derive(Serialize)]
struct ApiResponsePoolStep {
    #[serde(with = "dec_format")]
    pool_id: u64,
    token_in: AccountId,
    token_out: AccountId,
    #[serde(with = "dec_format")]
    amount_in: Balance,
    #[serde(with = "dec_format")]
    amount_out: Balance,
    #[serde(with = "dec_format")]
    min_amount_out: Balance,
}

const RHEA_CONTRACT_ID: &str = "v2.ref-finance.near";
const SPLIT_ROUTE_STEP_SIZE: u32 = 1; // %
const MAX_SPLITS_COUNT: usize = 10;
const FEE_DIVISOR: u32 = 10_000;
const FETCH_POOLS_BATCH_SIZE: usize = 1000;
const TOP_ROUTES_COUNT: usize = 10;

const RC_SUCCESS: i32 = 0;
const RC_POOL_FETCH_ERROR: i32 = 1;
const RC_ROUTE_ERROR: i32 = 2;
const RC_RESPONSE_BUILD_ERROR: i32 = 3;
const RC_INVALID_SLIPPAGE: i32 = 4;

construct_uint! {
    struct U256(4);
}

construct_uint! {
    struct U384(6);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PoolKind {
    SimplePool,
    StableSwap,
    RatedSwap,
    DegenSwap,
}

mod dec_format_vec {
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    pub fn serialize<S, T>(value: &Vec<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: ToString,
    {
        let vec_of_strings = value.iter().map(|v| v.to_string()).collect::<Vec<String>>();
        vec_of_strings.serialize(serializer)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
    {
        let vec_of_strings = Vec::<String>::deserialize(deserializer)?;
        let vec_of_values = vec_of_strings
            .iter()
            .map(|v| {
                v.parse::<T>()
                    .map_err(|_| de::Error::custom("Failed to parse value"))
            })
            .collect::<Result<Vec<T>, D::Error>>()?;
        Ok(vec_of_values)
    }
}

#[derive(Debug)]
struct Pool {
    id: u64,
    info: PoolInfo,
    detail: PoolDetailInfo,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PoolInfo {
    #[serde(with = "dec_format_vec")]
    amounts: Vec<Balance>,
    amp: u64,
    pool_kind: PoolKind,
    shares_total_supply: String,
    token_account_ids: Vec<AccountId>,
    total_fee: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
enum PoolDetailInfo {
    SimplePoolInfo(SimplePoolInfo),
    StablePoolInfo(StablePoolInfo),
    RatedPoolInfo(RatedPoolInfo),
    DegenPoolInfo(DegenPoolInfo),
}

#[derive(Debug, Default)]
struct PoolsDelta {
    changed_pools: HashMap<u64, Pool>,
}

impl PoolDetailInfo {
    fn emulate_swap(
        &mut self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        match self {
            PoolDetailInfo::SimplePoolInfo(info) => {
                info.emulate_swap(token_in, token_out, amount_in)
            }
            PoolDetailInfo::StablePoolInfo(info) => {
                info.emulate_swap(token_in, token_out, amount_in)
            }
            PoolDetailInfo::RatedPoolInfo(info) => {
                info.emulate_swap(token_in, token_out, amount_in)
            }
            PoolDetailInfo::DegenPoolInfo(info) => {
                info.emulate_swap(token_in, token_out, amount_in)
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct SimplePoolInfo {
    token_account_ids: Vec<AccountId>,
    #[serde(with = "dec_format_vec")]
    amounts: Vec<Balance>,
    total_fee: u32,
    #[serde(with = "dec_format")]
    shares_total_supply: Balance,
}

impl SimplePoolInfo {
    fn emulate_swap(
        &mut self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let Some(token_in) = self.token_account_ids.iter().position(|id| id == token_in) else {
            return Err(anyhow::anyhow!("Token in not found"));
        };
        let Some(token_out) = self.token_account_ids.iter().position(|id| id == token_out) else {
            return Err(anyhow::anyhow!("Token out not found"));
        };
        let in_balance = U256::from(self.amounts[token_in]);
        let out_balance = U256::from(self.amounts[token_out]);
        if in_balance == U256::zero()
            || out_balance == U256::zero()
            || token_in == token_out
            || amount_in == 0
        {
            return Err(anyhow::anyhow!("Invalid parameters"));
        }
        let amount_with_fee = U256::from(amount_in) * U256::from(FEE_DIVISOR - self.total_fee);
        let received = (amount_with_fee * out_balance
            / (U256::from(FEE_DIVISOR) * in_balance + amount_with_fee))
            .as_u128();
        self.amounts[token_in] += amount_in;
        self.amounts[token_out] -= received;
        Ok(received)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct StablePoolInfo {
    token_account_ids: Vec<AccountId>,
    decimals: Vec<u8>,
    #[serde(with = "dec_format_vec")]
    amounts: Vec<Balance>,
    #[serde(with = "dec_format_vec")]
    c_amounts: Vec<Balance>,
    total_fee: u32,
    #[serde(with = "dec_format")]
    shares_total_supply: Balance,
    amp: u64,
}

pub fn u128_ratio(a: u128, num: u128, denom: u128) -> u128 {
    (U256::from(a) * U256::from(num) / U256::from(denom)).as_u128()
}

impl StablePoolInfo {
    fn emulate_swap(
        &mut self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let Some(in_idx) = self.token_account_ids.iter().position(|id| id == token_in) else {
            return Err(anyhow::anyhow!("Token in not found"));
        };
        let Some(out_idx) = self.token_account_ids.iter().position(|id| id == token_out) else {
            return Err(anyhow::anyhow!("Token out not found"));
        };
        let result = self.internal_get_return(in_idx, amount_in, out_idx)?;
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx)?;
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        if self.c_amounts[out_idx] < stable::MIN_RESERVE {
            return Err(anyhow::anyhow!("Min reserve not met"));
        }
        Ok(amount_swapped)
    }

    fn c_amount_to_amount(&self, c_amount: u128, index: usize) -> Result<u128, anyhow::Error> {
        let value = self.decimals[index];
        if value <= stable::TARGET_DECIMAL {
            let factor = 10_u128
                .checked_pow((stable::TARGET_DECIMAL - value) as u32)
                .unwrap();
            c_amount
                .checked_div(factor)
                .ok_or_else(|| anyhow::anyhow!("Cannot divide"))
        } else {
            let factor = 10_u128
                .checked_pow((value - stable::TARGET_DECIMAL) as u32)
                .unwrap();
            c_amount
                .checked_mul(factor)
                .ok_or_else(|| anyhow::anyhow!("Cannot multiply"))
        }
    }

    fn amount_to_c_amount(&self, amount: u128, index: usize) -> Result<u128, anyhow::Error> {
        let value = self.decimals[index];
        if value <= stable::TARGET_DECIMAL {
            let factor = 10_u128
                .checked_pow((stable::TARGET_DECIMAL - value) as u32)
                .unwrap();
            amount
                .checked_mul(factor)
                .ok_or_else(|| anyhow::anyhow!("Cannot multiply"))
        } else {
            let factor = 10_u128
                .checked_pow((value - stable::TARGET_DECIMAL) as u32)
                .unwrap();
            amount
                .checked_div(factor)
                .ok_or_else(|| anyhow::anyhow!("Cannot divide"))
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
    ) -> Result<stable::SwapResult, anyhow::Error> {
        // make amounts into comparable-amounts
        let c_amount_in = self.amount_to_c_amount(amount_in, token_in)?;

        self.get_invariant()
            .swap_to(
                token_in,
                c_amount_in,
                token_out,
                &self.c_amounts,
                &stable::Fees::new(self.total_fee),
            )
            .ok_or_else(|| anyhow::anyhow!("Cannot swap"))
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct RatedPoolInfo {
    token_account_ids: Vec<AccountId>,
    decimals: Vec<u8>,
    #[serde(with = "dec_format_vec")]
    amounts: Vec<Balance>,
    #[serde(with = "dec_format_vec")]
    c_amounts: Vec<Balance>,
    total_fee: u32,
    #[serde(with = "dec_format")]
    shares_total_supply: Balance,
    amp: u64,
    #[serde(with = "dec_format_vec")]
    rates: Vec<Balance>,
}

impl RatedPoolInfo {
    fn emulate_swap(
        &mut self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let Some(in_idx) = self.token_account_ids.iter().position(|id| id == token_in) else {
            return Err(anyhow::anyhow!("Token in not found"));
        };
        let Some(out_idx) = self.token_account_ids.iter().position(|id| id == token_out) else {
            return Err(anyhow::anyhow!("Token out not found"));
        };
        let result = self.internal_get_return(in_idx, amount_in, out_idx);
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx);
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        if self.c_amounts[out_idx] < rated::MIN_RESERVE {
            return Err(anyhow::anyhow!("Min reserve not met"));
        }
        Ok(amount_swapped)
    }

    fn internal_get_return(
        &self,
        token_in: usize,
        amount_in: Balance,
        token_out: usize,
    ) -> rated::SwapResult {
        self.internal_get_return_with_rates(token_in, amount_in, token_out, &self.rates)
    }

    fn internal_get_return_with_rates(
        &self,
        token_in: usize,
        amount_in: Balance,
        token_out: usize,
        rates: &Vec<Balance>,
    ) -> rated::SwapResult {
        // make amounts into comparable-amounts
        let c_amount_in = self.amount_to_c_amount(amount_in, token_in);

        self.get_invariant_with_rates(rates)
            .swap_to(
                token_in,
                c_amount_in,
                token_out,
                &self.c_amounts,
                &rated::Fees::new(self.total_fee),
            )
            .expect("Cannot swap")
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

    fn get_invariant_with_rates(&self, rates: &Vec<Balance>) -> RatedSwap {
        RatedSwap::new(self.amp, rates)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct DegenPoolInfo {
    token_account_ids: Vec<AccountId>,
    decimals: Vec<u8>,
    #[serde(with = "dec_format_vec")]
    amounts: Vec<Balance>,
    #[serde(with = "dec_format_vec")]
    c_amounts: Vec<Balance>,
    total_fee: u32,
    #[serde(with = "dec_format")]
    shares_total_supply: Balance,
    amp: u64,
    #[serde(with = "dec_format_vec")]
    degens: Vec<Balance>,
}

impl DegenPoolInfo {
    fn emulate_swap(
        &mut self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
    ) -> Result<Balance, anyhow::Error> {
        let Some(in_idx) = self.token_account_ids.iter().position(|id| id == token_in) else {
            return Err(anyhow::anyhow!("Token in not found"));
        };
        let Some(out_idx) = self.token_account_ids.iter().position(|id| id == token_out) else {
            return Err(anyhow::anyhow!("Token out not found"));
        };
        let result = self.internal_get_return(in_idx, amount_in, out_idx);
        let amount_swapped = self.c_amount_to_amount(result.amount_swapped, out_idx);
        self.c_amounts[in_idx] = result.new_source_amount;
        self.c_amounts[out_idx] = result.new_destination_amount;
        if self.c_amounts[out_idx] < degen::MIN_RESERVE {
            return Err(anyhow::anyhow!("Min reserve not met"));
        }
        Ok(amount_swapped)
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

    fn internal_get_return(
        &self,
        token_in: usize,
        amount_in: Balance,
        token_out: usize,
    ) -> degen::SwapResult {
        self.internal_get_return_with_degens(token_in, amount_in, token_out, &self.degens)
    }

    fn internal_get_return_with_degens(
        &self,
        token_in: usize,
        amount_in: Balance,
        token_out: usize,
        degens: &Vec<Balance>,
    ) -> degen::SwapResult {
        // make amounts into comparable-amounts
        let c_amount_in = self.amount_to_c_amount(amount_in, token_in);

        self.get_invariant_with_degens(degens)
            .swap_to(
                token_in,
                c_amount_in,
                token_out,
                &self.c_amounts,
                &degen::Fees::new(self.total_fee),
            )
            .expect("Cannot swap")
    }

    fn get_invariant_with_degens(&self, degens: &Vec<Balance>) -> DegenSwap {
        DegenSwap::new(self.amp, degens)
    }
}

lazy_static! {
    static ref POOLS_CACHE: Arc<RwLock<Option<Arc<Pools>>>> = Arc::new(RwLock::new(None));
}

struct Pools {
    pools: Vec<Pool>,
    pools_existing: HashSet<BTreeSet<AccountId>>,
    token_to_pools: HashMap<AccountId, Vec<usize>>,
    pair_to_pools: HashMap<(AccountId, AccountId), Vec<usize>>,
}

impl Pools {
    fn has_direct_pool(&self, token_a: &AccountId, token_b: &AccountId) -> bool {
        let pair = BTreeSet::from([token_a.clone(), token_b.clone()]);
        self.pools_existing.contains(&pair)
    }

    fn direct_pools<'a>(
        &'a self,
        token_a: &AccountId,
        token_b: &AccountId,
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

    fn pools_with_token<'a>(&'a self, token: &AccountId) -> impl Iterator<Item = &'a Pool> {
        self.token_to_pools
            .get(token)
            .into_iter()
            .flat_map(move |indices| indices.iter().map(move |&idx| &self.pools[idx]))
    }
}

async fn get_all_pools(client: &RpcClient) -> Result<Pools, anyhow::Error> {
    let number_of_pools: u64 = client
        .call(
            RHEA_CONTRACT_ID.parse().unwrap(),
            "get_number_of_pools",
            (),
            QueryFinality::Finality(Finality::None),
        )
        .await?;
    println!("Number of pools for data fetch: {}", number_of_pools);

    let mut pools_batch_requests = Vec::new();
    let mut detail_infos_batch_requests = Vec::new();

    for i in (0..number_of_pools).step_by(FETCH_POOLS_BATCH_SIZE) {
        let request_args = serde_json::json!({
            "from_index": i,
            "limit": FETCH_POOLS_BATCH_SIZE,
        });

        pools_batch_requests.push((
            RHEA_CONTRACT_ID.parse().unwrap(),
            "get_pools",
            request_args.clone(),
            QueryFinality::Finality(Finality::None),
        ));

        detail_infos_batch_requests.push((
            RHEA_CONTRACT_ID.parse().unwrap(),
            "get_pool_detail_infos",
            request_args,
            QueryFinality::Finality(Finality::None),
        ));
    }

    let (pools, detail_infos) = tokio::try_join!(
        async {
            let batches: Vec<Result<Vec<PoolInfo>, _>> =
                client.batch_call(pools_batch_requests).await?;
            let mut pools = Vec::new();
            for result in batches {
                let pool_batch = result?;
                pools.extend(pool_batch);
            }
            Ok::<Vec<PoolInfo>, anyhow::Error>(pools)
        },
        async {
            let batches: Vec<Result<Vec<PoolDetailInfo>, _>> =
                client.batch_call(detail_infos_batch_requests).await?;
            let mut detail_infos = Vec::new();
            for result in batches {
                let detail_batch = result?;
                detail_infos.extend(detail_batch);
            }
            Ok::<Vec<PoolDetailInfo>, anyhow::Error>(detail_infos)
        }
    )?;

    if pools.len() != detail_infos.len() {
        return Err(anyhow::anyhow!(
            "Pools and detail infos have different lengths"
        ));
    }

    let pools_existing = {
        let mut pools_existing = HashSet::new();
        for pool in pools.iter() {
            for i in 2..=pool.token_account_ids.len() {
                pools_existing.extend(
                    pool.token_account_ids
                        .iter()
                        .cloned()
                        .combinations(i)
                        .map(BTreeSet::from_iter),
                );
            }
        }
        pools_existing
    };

    let pools_vec: Vec<Pool> = pools
        .into_iter()
        .zip(detail_infos.into_iter())
        .enumerate()
        .map(|(i, (info, detail))| Pool {
            id: i as u64,
            info,
            detail,
        })
        .collect();

    let mut token_to_pools: HashMap<AccountId, Vec<usize>> = HashMap::new();
    let mut pair_to_pools: HashMap<(AccountId, AccountId), Vec<usize>> = HashMap::new();

    for (idx, pool) in pools_vec.iter().enumerate() {
        for token in &pool.info.token_account_ids {
            token_to_pools.entry(token.clone()).or_default().push(idx);
        }

        for pair in pool.info.token_account_ids.iter().cloned().combinations(2) {
            let mut tokens_sorted = pair;
            tokens_sorted.sort();
            let key = (tokens_sorted[0].clone(), tokens_sorted[1].clone());
            pair_to_pools.entry(key).or_default().push(idx);
        }
    }

    Ok(Pools {
        pools: pools_vec,
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
                    eprintln!("Failed to update pools: {}", e);
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
    let client = Arc::new(RpcClient::new([
        "https://rpc-ua.intea.rs",
        "https://rpc.intea.rs",
    ]));

    start_pools_update_task(client.clone()).await;

    let api = warp::path("findPath")
        .and(warp::query::<FindPathQuery>())
        .and_then(handle_find_path);

    println!("Server listening on http://localhost:12345/findPath ...");
    warp::serve(api).run(([127, 0, 0, 1], 12345)).await;

    Ok(())
}

async fn route<'a>(
    token_in: &'a AccountId,
    token_out: &'a AccountId,
    amount: Balance,
    pools: &'a Pools,
    max_hops: MaxHops,
) -> Result<SplitRoute<'a>, anyhow::Error> {
    let now = Instant::now();
    let routes = find_best_routes(
        pools,
        amount,
        token_in,
        token_out,
        max_hops,
        TOP_ROUTES_COUNT,
    )?;
    let duration = now.elapsed();
    println!("Time to find best routes: {:?}", duration);

    let now = Instant::now();
    let Some(best_split) = find_best_split_route(routes, amount, token_in, token_out) else {
        return Err(anyhow::anyhow!("No valid split route found"));
    };
    let duration = now.elapsed();
    println!("Time to find best split route: {:?}", duration);

    let estimated_out =
        best_split.emulate_swap(token_in, token_out, amount, &mut PoolsDelta::default())?;

    if estimated_out == 0 {
        return Err(anyhow::anyhow!("Estimated output is 0"));
    }

    Ok(best_split.clone())
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
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
        amount_in: Balance,
        pools_delta: &mut PoolsDelta,
    ) -> Result<Balance, anyhow::Error> {
        let mut total_out: Balance = 0;
        for step in &self.steps {
            let amount_part = (amount_in * step.weight as Balance) / 100;
            let out = step
                .route
                .emulate_swap(token_in, token_out, amount_part, pools_delta)?;
            total_out += out;
        }
        Ok(total_out)
    }
}

impl<'a> SplitRoute<'a> {
    fn to_api_response(
        &self,
        token_in: &AccountId,
        token_out: &AccountId,
        total_amount_in: Balance,
        slippage_bp: u128,
    ) -> Result<SplitRouteApiResponse, anyhow::Error> {
        let mut routes_resp = Vec::new();
        let mut total_estimated_out: Balance = 0;
        let mut pools_delta = PoolsDelta::default();
        for step in &self.steps {
            let amount_part = (total_amount_in * step.weight as Balance) / 100;
            let estimated_out =
                step.route
                    .emulate_swap(token_in, token_out, amount_part, &mut pools_delta)?;
            let min_amount_out = estimated_out * (10_000u128 - slippage_bp) / 10_000u128;

            let mut pools_resp = Vec::new();
            let mut current_token = token_in.clone();
            for (idx, route_step) in step.route.steps.iter().enumerate() {
                let is_first = idx == 0;
                let is_last = idx + 1 == step.route.steps.len();

                pools_resp.push(ApiResponsePoolStep {
                    pool_id: route_step.pool.id,
                    token_in: current_token.clone(),
                    token_out: route_step.token_out.to_owned(),
                    amount_in: if is_first { amount_part } else { 0 },
                    amount_out: 0,
                    min_amount_out: if is_last { min_amount_out } else { 0 },
                });

                current_token = route_step.token_out.to_owned();
            }

            routes_resp.push(ApiResponseROute {
                pools: pools_resp,
                amount_in: amount_part,
                min_amount_out,
                amount_out: 0,
            });
            total_estimated_out += estimated_out;
        }
        Ok(SplitRouteApiResponse {
            routes: routes_resp,
            contract_in: token_in.clone(),
            contract_out: token_out.clone(),
            amount_in: total_amount_in,
            amount_out: total_estimated_out,
        })
    }
}

fn find_best_split_route<'a>(
    routes: Vec<Route<'a>>,
    total_amount: Balance,
    token_in: &'a AccountId,
    token_out: &'a AccountId,
) -> Option<SplitRoute<'a>> {
    // Represent weights in discrete units of `SPLIT_ROUTE_STEP_SIZE` percent.
    let step = SPLIT_ROUTE_STEP_SIZE;
    assert_eq!(100 % step, 0);
    let total_units = 100 / step; // e.g. 5 % step  ⇒ 20 units

    // Pre-compute outputs for each route and each feasible weight unit.
    // route_outputs[i][u] = Some(output) where `u` ∈ 1..=total_units.
    let mut route_outputs: Vec<Vec<Option<Balance>>> = Vec::with_capacity(routes.len());
    for route in routes.iter() {
        let mut vec = vec![None; (total_units + 1) as usize];
        for units in 1..=total_units {
            let weight_pct = units * step;
            let amount_part = (total_amount * weight_pct as Balance) / 100;
            if let Ok(out) =
                route.emulate_swap(token_in, token_out, amount_part, &mut PoolsDelta::default())
            {
                vec[units as usize] = Some(out);
            }
        }
        route_outputs.push(vec);
    }

    // dp[s][u] = best (total_out, steps) using exactly `s` splits and `u` weight units.
    // Dimensions: (MAX_SPLITS_COUNT + 1) × (total_units + 1)
    let mut dp: Vec<Vec<Option<(Balance, Vec<SplitRouteStep<'a>>)>>> =
        vec![vec![None; (total_units + 1) as usize]; MAX_SPLITS_COUNT + 1];
    dp[0][0] = Some((0, Vec::new()));

    for (route_idx, route) in routes.iter().cloned().enumerate() {
        let outputs = &route_outputs[route_idx];
        let mut next_dp = dp.clone();

        for splits in 0..=MAX_SPLITS_COUNT - 1 {
            for used_units in 0..=total_units {
                let Some((current_out, ref steps_vec)) = dp[splits][used_units as usize] else {
                    continue;
                };

                // Try assigning additional weight units to this route
                for add_units in 1..=(total_units - used_units) {
                    let Some(out_val) = outputs[add_units as usize] else {
                        continue;
                    };

                    let new_units = used_units + add_units;
                    let new_splits = splits + 1;
                    let new_out = current_out + out_val;

                    let weight_pct = add_units * step;
                    let mut new_steps = steps_vec.clone();
                    new_steps.push(SplitRouteStep {
                        route: route.clone(),
                        weight: weight_pct,
                    });

                    match &mut next_dp[new_splits][new_units as usize] {
                        Some((best_out, best_steps)) => {
                            if new_out > *best_out {
                                *best_out = new_out;
                                *best_steps = new_steps;
                            }
                        }
                        slot @ None => {
                            *slot = Some((new_out, new_steps));
                        }
                    }
                }
            }
        }

        dp = next_dp;
    }

    // Extract best solution that uses exactly 100 % (i.e., total_units) weight.
    let mut best: Option<(Balance, Vec<SplitRouteStep<'a>>)> = None;
    for splits in 1..=MAX_SPLITS_COUNT {
        if let Some((out, steps)) = dp[splits][total_units as usize].clone()
            && best.as_ref().is_none_or(|(best_out, _)| out > *best_out)
        {
            best = Some((out, steps));
        }
    }

    best.map(|(_, steps)| SplitRoute::new(steps))
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
    token_in: &'a AccountIdRef,
    token_out: &'a AccountIdRef,
}

impl<'a> Display for Route<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(first_step) = self.steps.first() else {
            return write!(f, "(empty route)");
        };
        f.write_str("Route: ")?;
        f.write_str(first_step.token_in.as_str())?;
        for step in self.steps.iter() {
            write!(f, " --- ({}) ---> {}", step.pool.id, step.token_out)?;
        }
        Ok(())
    }
}

impl Route<'_> {
    fn emulate_swap(
        &self,
        token_in: &AccountIdRef,
        token_out: &AccountIdRef,
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
            let mut detail = pool.detail.clone();
            let mut info = pool.info.clone();
            let token_in_idx = info
                .token_account_ids
                .iter()
                .position(|id| id == current_token)
                .unwrap();
            let token_out_idx = info
                .token_account_ids
                .iter()
                .position(|id| id == step.token_out)
                .unwrap();
            info.amounts[token_in_idx] += current_amount;
            current_amount = detail.emulate_swap(current_token, step.token_out, current_amount)?;
            info.amounts[token_out_idx] -= current_amount;
            let new_pool = Pool {
                id: pool.id,
                info,
                detail,
            };
            pools_delta.changed_pools.insert(new_pool.id, new_pool);
            current_token = step.token_out;
        }
        if current_token != token_out {
            return Err(anyhow::anyhow!("Invalid route"));
        }
        Ok(current_amount)
    }
}

#[derive(Deserialize, Debug)]
enum MaxHops {
    DirectOnly,
    Two,
    Three,
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
        items.sort_by(|a, b| key_fn(b).cmp(&key_fn(a))); // descending order
        return items;
    }

    items.select_nth_unstable_by(k - 1, |a, b| key_fn(b).cmp(&key_fn(a)));

    items.truncate(k);
    items.sort_by(|a, b| key_fn(b).cmp(&key_fn(a))); // descending order

    items
}

fn find_best_route<'a>(
    pools: &'a Pools,
    amount: Balance,
    token_in: &'a AccountId,
    token_out: &'a AccountId,
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
    amount: Balance,
    token_in: &'a AccountId,
    token_out: &'a AccountId,
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
            i128::try_from(
                pool.detail
                    .clone()
                    .emulate_swap(token_in, token_out, amount)
                    .unwrap_or_default(),
            )
            .unwrap_or_default()
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
        .flat_map(|pool| &pool.info.token_account_ids)
        .collect::<HashSet<_>>();
    let ending_pool_tokens = ending_pool_candidates
        .flat_map(|pool| &pool.info.token_account_ids)
        .collect::<HashSet<_>>();
    let possible_intermediate_tokens = starting_pool_tokens
        .intersection(&ending_pool_tokens)
        .filter(|&&token| token != token_in && token != token_out)
        .collect::<Vec<_>>();

    let mut routes = Vec::new();
    routes.extend(single_pool_routes);

    for intermediate_token in possible_intermediate_tokens {
        if !pools.has_direct_pool(token_in, intermediate_token)
            || !pools.has_direct_pool(intermediate_token, token_out)
        {
            continue;
        }

        let Ok(first_route) = find_best_route(
            pools,
            amount,
            token_in,
            intermediate_token,
            MaxHops::DirectOnly,
        ) else {
            continue;
        };
        let Ok(intermediate_amount) = first_route.emulate_swap(
            token_in,
            intermediate_token,
            amount,
            &mut PoolsDelta::default(),
        ) else {
            continue;
        };
        let Ok(second_route) = find_best_route(
            pools,
            intermediate_amount,
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

    if matches!(max_hops, MaxHops::Three | MaxHops::Max) {
        // Three hops
        for first_intermediate_token in starting_pool_tokens.iter() {
            if *first_intermediate_token == token_in || *first_intermediate_token == token_out {
                continue;
            }
            for second_intermediate_token in ending_pool_tokens.iter() {
                if *second_intermediate_token == token_in || *second_intermediate_token == token_out
                {
                    continue;
                }
                if first_intermediate_token == second_intermediate_token {
                    continue;
                }

                if !pools.has_direct_pool(token_in, first_intermediate_token)
                    || !pools.has_direct_pool(first_intermediate_token, second_intermediate_token)
                    || !pools.has_direct_pool(second_intermediate_token, token_out)
                {
                    continue;
                }

                let Ok(in_to_first) = find_best_route(
                    pools,
                    amount,
                    token_in,
                    first_intermediate_token,
                    match max_hops {
                        MaxHops::Three => MaxHops::DirectOnly,
                        MaxHops::Max => MaxHops::Two,
                        _ => unreachable!(),
                    },
                ) else {
                    continue;
                };
                let Ok(first_intermediate_amount) = in_to_first.emulate_swap(
                    token_in,
                    first_intermediate_token,
                    amount,
                    &mut PoolsDelta::default(),
                ) else {
                    continue;
                };
                let Ok(first_to_second) = find_best_route(
                    pools,
                    first_intermediate_amount,
                    first_intermediate_token,
                    second_intermediate_token,
                    match max_hops {
                        MaxHops::Three => MaxHops::DirectOnly,
                        MaxHops::Max => MaxHops::Two,
                        _ => unreachable!(),
                    },
                ) else {
                    continue;
                };
                let Ok(second_intermediate_amount) = first_to_second.emulate_swap(
                    first_intermediate_token,
                    second_intermediate_token,
                    first_intermediate_amount,
                    &mut PoolsDelta::default(),
                ) else {
                    continue;
                };
                let Ok(second_to_out) = find_best_route(
                    pools,
                    second_intermediate_amount,
                    second_intermediate_token,
                    token_out,
                    match max_hops {
                        MaxHops::Three => MaxHops::DirectOnly,
                        MaxHops::Max => MaxHops::Two,
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

    for route in routes.iter() {
        let result = route.emulate_swap(token_in, token_out, amount, &mut PoolsDelta::default());
        println!("{}, result: {:?}", route, result);
    }

    let best_routes = select_top_k(routes, count, |route| {
        i128::try_from(
            route
                .emulate_swap(token_in, token_out, amount, &mut PoolsDelta::default())
                .unwrap_or_default(),
        )
        .unwrap_or_default()
    });
    Ok(best_routes.clone())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindPathQuery {
    #[serde(with = "dec_format")]
    amount_in: Balance,
    token_in: AccountId,
    token_out: AccountId,
    max_hops: MaxHops,
    #[serde(default)]
    slippage: Option<f64>, // e.g., 0.005 for 0.5 %
}

async fn handle_find_path(query: FindPathQuery) -> Result<impl warp::Reply, warp::Rejection> {
    let request_id = rand::thread_rng().gen_range(1..1000000);
    println!(
        "Received request: request_id={:06}, amount_in={}, token_in={}, token_out={}, max_hops={:?}, slippage={:?}",
        request_id,
        query.amount_in,
        query.token_in,
        query.token_out,
        query.max_hops,
        query.slippage
    );

    if query.slippage.is_some_and(|s| !(0.0..=1.0).contains(&s)) {
        let resp: ApiResponse<()> = ApiResponse {
            result_code: RC_INVALID_SLIPPAGE,
            result_message: "Invalid slippage".into(),
            result_data: None,
        };
        return Ok(warp::reply::json(&resp));
    }
    let slippage_bp: u128 = query.slippage.map(|v| (v * 10_000.0) as u128).unwrap_or(50);

    match get_pools().await {
        Ok(pools) => match route(
            &query.token_in,
            &query.token_out,
            query.amount_in,
            &pools,
            query.max_hops,
        )
        .await
        {
            Ok(split_route) => {
                let estimated_out = split_route
                    .emulate_swap(
                        &query.token_in,
                        &query.token_out,
                        query.amount_in,
                        &mut PoolsDelta::default(),
                    )
                    .unwrap_or(0);
                println!(
                    "Request id: {:06}, Estimated amount out: {}",
                    request_id, estimated_out
                );

                match split_route.to_api_response(
                    &query.token_in,
                    &query.token_out,
                    query.amount_in,
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
