mod storage;

use self::storage::StoredPool;
pub use self::storage::watch;

use bigdecimal::BigDecimal;
use near_min_api::types::{Balance, BlockHeight, U128};
use near_min_api::utils::dec_format;

use std::collections::{BTreeMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

pub use intear_dex_common::AssetId;
use near_sdk::Timestamp;
use swap_router_types::{indexed::AccountState, types::Gas};
use xyk_dex_types::math::{exact_in_amount_out, exact_out_amount_in};
use xyk_dex_types::{
    AssetIndex, CurrentFees, FULL_ROUTE_FRACTION, FeeDiscountFraction, FeePayoutType, FeeReceiver,
    PayoutGroups, PoolAssets, SwapReserves, TokenAmount, TotalShares, protocol_fee_payout,
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
        swap_gas: Gas,
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
        swap_gas: Gas,
    },
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactInRoute {
    pub pools: Vec<ExactInPoolStep>,
    /// [`xyk_dex_types::RouteFraction`] of the route
    pub split_fraction: u32,
    pub amount_in: U128,
    pub min_amount_out: U128,
    pub amount_out: U128,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExactOutRoute {
    pub pools: Vec<ExactOutPoolStep>,
    /// [`xyk_dex_types::RouteFraction`] of the route
    pub split_fraction: u32,
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

const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Pool {
    id: u32,
    reserves: PoolAssets<TokenAmount>,
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
    min_reserves: PoolAssets<TokenAmount>,
    /// `None` for pools that keep all of their fee in their liquidity
    liquidity_provider_payouts: Option<LiquidityProviderPayouts>,
}

/// How a public pool pays its fee out to its liquidity providers
#[derive(Clone)]
struct LiquidityProviderPayouts {
    total_shares: TotalShares,
    groups: PayoutGroups,
}

impl Pool {
    /// `None` for a pool of NEP-171 assets, which the contract can't swap in
    fn new(id: u32, data: StoredPool, timestamp: Timestamp) -> Option<Self> {
        let (assets, fee_configuration, min_reserves, liquidity_provider_payouts) = match data {
            StoredPool::PrivateV2 { assets, fees, .. } => {
                (assets, fees, PoolAssets([TokenAmount::ZERO; 2]), None)
            }
            StoredPool::LaunchV2 {
                quote_asset,
                launched_asset,
                fees,
                phantom_liquidity,
            } => (
                (quote_asset, launched_asset),
                fees,
                PoolAssets([phantom_liquidity, TokenAmount::ZERO]),
                None,
            ),
            StoredPool::PublicV3 {
                assets,
                fees,
                total_shares,
                payout_groups,
                ..
            } => (
                assets,
                fees,
                PoolAssets([TokenAmount::ZERO; 2]),
                Some(LiquidityProviderPayouts {
                    total_shares,
                    groups: *payout_groups,
                }),
            ),
        };
        let tokens = [assets.0.asset_id, assets.1.asset_id];
        if tokens
            .iter()
            .any(|token| matches!(token, AssetId::Nep171(..)))
        {
            return None;
        }
        Some(Self {
            id,
            reserves: PoolAssets([assets.0.balance, assets.1.balance]),
            info: Arc::new(PoolInfo {
                tokens,
                fees: fee_configuration.with_protocol_fee(timestamp),
                min_reserves,
                liquidity_provider_payouts,
            }),
        })
    }

    /// The pool with its pool and protocol fees discounted, as a signed fee discount makes the
    /// xyk DEX charge them
    fn with_fee_discount(&self, fee_discount: FeeDiscountFraction) -> Self {
        let fees = self.info.fees.clone().discounted(fee_discount);
        Self {
            info: Arc::new(PoolInfo {
                fees,
                ..(*self.info).clone()
            }),
            ..self.clone()
        }
    }

    /// Swaps the token at `in_index` like the contract's `swap_in_pool`, and returns the amounts
    /// in and out. `None` for a swap the contract fails, which panics like the contract on
    /// overflows.
    fn swap(&mut self, in_index: AssetIndex, amount: QuoteAmount) -> Option<(Balance, Balance)> {
        let out_index = match in_index {
            AssetIndex::First => AssetIndex::Second,
            AssetIndex::Second => AssetIndex::First,
        };
        let info = &*self.info;
        let mut balances = self.reserves;
        if balances.0.iter().any(|balance| balance.is_zero()) {
            return None;
        }
        let swap_reserves = |balances: &PoolAssets<TokenAmount>| SwapReserves {
            input: balances[in_index],
            output: balances[out_index],
        };
        let amount_in = match amount {
            QuoteAmount::ExactIn(0) | QuoteAmount::ExactOut(0) => return None,
            QuoteAmount::ExactIn(amount_in) => TokenAmount::new(amount_in),
            QuoteAmount::ExactOut(amount_out) => {
                if amount_out >= balances[out_index].get() {
                    return None;
                }
                exact_out_amount_in(
                    TokenAmount::new(amount_out),
                    swap_reserves(&balances),
                    &info.fees,
                )
            }
        };
        // The parts of each receiver's fee that are paid out, and with which payout type. The
        // rest of the pool's fee goes into its liquidity.
        let receiver_fees = info
            .fees
            .amounts(amount_in)
            .map(|(receiver, fee_amount)| {
                let paid_out_fees: Vec<(FeePayoutType, TokenAmount)> =
                    match (receiver, &info.liquidity_provider_payouts) {
                        (FeeReceiver::Pool, Some(payouts)) => payouts
                            .groups
                            .split_pool_fee(fee_amount, payouts.total_shares)
                            .into_iter()
                            .filter(|(_, paid_out_fee)| !paid_out_fee.is_zero())
                            .collect(),
                        (FeeReceiver::Pool, None) => Vec::new(),
                        (FeeReceiver::Account { payout_type, .. }, _) => {
                            vec![(*payout_type, fee_amount)]
                        }
                        (FeeReceiver::Protocol, _) => vec![(
                            protocol_fee_payout((&info.tokens[0], &info.tokens[1])),
                            fee_amount,
                        )],
                    };
                (receiver, fee_amount, paid_out_fees)
            })
            .collect::<Vec<_>>();
        let mut total_fees = TokenAmount::ZERO;
        let mut fees_added_to_liquidity = TokenAmount::ZERO;
        for (_, fee_amount, paid_out_fees) in &receiver_fees {
            total_fees = total_fees.checked_add(*fee_amount).expect("Overflow");
            let fee_added_to_liquidity = paid_out_fees
                .iter()
                .try_fold(*fee_amount, |rest, (_, paid_out_fee)| {
                    rest.checked_sub(*paid_out_fee)
                })
                .expect("Paid out more than the fee");
            fees_added_to_liquidity = fees_added_to_liquidity
                .checked_add(fee_added_to_liquidity)
                .expect("Overflow");
        }
        let amount_in_after_fees = amount_in.checked_sub(total_fees).expect("Fee exceeds 100%");
        let amount_out = match amount {
            QuoteAmount::ExactIn(_) => {
                exact_in_amount_out(amount_in_after_fees, swap_reserves(&balances))
            }
            QuoteAmount::ExactOut(amount_out) => TokenAmount::new(amount_out),
        };
        balances[in_index] = balances[in_index]
            .checked_add(amount_in_after_fees)
            .expect("Overflow")
            .checked_add(fees_added_to_liquidity)
            .expect("Overflow");
        balances[out_index] = balances[out_index]
            .checked_sub(amount_out)
            .expect("Underflow");
        for (receiver, _, paid_out_fees) in receiver_fees {
            for (payout_type, paid_out_fee) in paid_out_fees {
                let payout_asset_index = payout_type.asset_index(in_index);
                let payout_amount = if payout_asset_index == in_index {
                    paid_out_fee
                } else {
                    // The pool swaps the fee into the other token without fees
                    let converted_fee = exact_in_amount_out(paid_out_fee, swap_reserves(&balances));
                    balances[in_index] = balances[in_index]
                        .checked_add(paid_out_fee)
                        .expect("Overflow");
                    balances[out_index] = balances[out_index]
                        .checked_sub(converted_fee)
                        .expect("Underflow");
                    converted_fee
                };
                if let (FeeReceiver::Pool, Some(payouts)) =
                    (receiver, &info.liquidity_provider_payouts)
                {
                    let fee_added_to_liquidity = match payout_type {
                        FeePayoutType::Reinvest => payout_amount,
                        FeePayoutType::KeepBoth
                        | FeePayoutType::ConvertToFirstAsset
                        | FeePayoutType::ConvertToSecondAsset => payouts
                            .groups
                            .group(payout_type)
                            .clone()
                            .share_fee(payout_asset_index, payout_amount),
                    };
                    balances[payout_asset_index] = balances[payout_asset_index]
                        .checked_add(fee_added_to_liquidity)
                        .expect("Overflow");
                }
            }
        }
        if AssetIndex::BOTH
            .into_iter()
            .any(|index| balances[index] < info.min_reserves[index])
        {
            return None;
        }
        self.reserves = balances;
        Some((amount_in.get(), amount_out.get()))
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
            let (_, amount_out) =
                pool.swap(AssetIndex::BOTH[token_in], QuoteAmount::ExactIn(amount_in))?;
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
            let (amount_in, _) = pool.swap(
                AssetIndex::BOTH[token_in],
                QuoteAmount::ExactOut(amount_out),
            )?;
            Some(amount_in)
        })
    }
}

pub struct Pools {
    block_height: BlockHeight,
    updated_at: Instant,
    /// Pools without a fee discount
    graph: Arc<Graph<Pool>>,
    /// Pools with a fee discount, made the first time it's asked for. A snapshot only lives for
    /// a block, so this only holds the ones asked for in it.
    discounted_graphs: Mutex<BTreeMap<FeeDiscountFraction, Arc<Graph<Pool>>>>,
}

impl Pools {
    /// Pools as a swap with `fee_discount` goes through them
    fn graph_for(&self, fee_discount: Option<FeeDiscountFraction>) -> Arc<Graph<Pool>> {
        let Some(fee_discount) = fee_discount else {
            return self.graph.clone();
        };
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

impl Pools {
    pub fn block_height(&self) -> BlockHeight {
        self.block_height
    }

    /// An error if they weren't updated in a while, which they are after every block
    pub fn check_fresh(&self) -> Result<(), anyhow::Error> {
        let age = self.updated_at.elapsed();
        if age > MAX_SNAPSHOT_AGE {
            anyhow::bail!(
                "Pools were last updated {age:?} ago, at block {}",
                self.block_height
            );
        }
        Ok(())
    }
}

const SETTINGS: Settings = Settings {
    top_routes: 10,
    max_splits: 8,
    allow_unused_input: false,
    keep_shorter_routes: true,
};

const SWAP_GAS: Gas = Gas::from_ggas(7_200, 7_200);
const HOP_GAS: Gas = Gas::from_ggas(2_000, 2_000);
const REPEATED_HOP_GAS: Gas = Gas::from_ggas(800, 800);
const FEE_DISCOUNT_GAS: Gas = Gas::from_ggas(1_000, 1_000);

fn swap_gas(split_route: &SplitRoute) -> Gas {
    let hops = split_route
        .parts
        .iter()
        .flat_map(|part| &part.route.hops)
        .map(|hop| hop.pool)
        .collect::<Vec<_>>();
    let pools = hops.iter().collect::<HashSet<_>>().len() as u64;
    let repeated_hops = hops.len() as u64 - pools;
    SWAP_GAS + HOP_GAS * pools + REPEATED_HOP_GAS * repeated_hops
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
                let min_amount_out =
                    crate::with_slippage(estimated_out, crate::BASIS_POINTS - slippage_bp);

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
                    split_fraction: part.weight_percentage * (FULL_ROUTE_FRACTION.get() / 100),
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
                let max_amount_in =
                    crate::with_slippage(amount_in, crate::BASIS_POINTS + slippage_bp);

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
                        max_amount_in: U128(crate::with_slippage(
                            step_amount.0,
                            crate::BASIS_POINTS + slippage_bp,
                        )),
                    });

                    current_token = graph.token(hop.token_out).clone();
                }

                routes_resp.push(ExactOutRoute {
                    pools: pools_resp,
                    split_fraction: part.weight_percentage * (FULL_ROUTE_FRACTION.get() / 100),
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

pub struct Request {
    pub token_in: AssetId,
    pub token_out: AssetId,
    pub amount: QuoteAmount,
    pub max_hops: MaxHops,
    /// 0.005 is 0.5%
    pub slippage: BigDecimal,
    pub near_price_raw: BigDecimal,
    pub quoted_token_price_raw: BigDecimal,
    /// Taken off the pool and protocol fees of every pool by a signed fee discount
    pub fee_discount: Option<FeeDiscountFraction>,
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
            HOP_GAS.estimated_burn,
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
    if request.fee_discount.is_some() {
        // Compared to the same route without the fee discount
        let undiscounted = split_route.emulate_total(&pools.graph_for(None), request.amount)?;
        let (saved, discount, swap_gas) = match &mut response {
            SplitRouteApiResponse::ExactIn {
                amount_out,
                discount,
                swap_gas,
                ..
            } => (amount_out.0.checked_sub(undiscounted), discount, swap_gas),
            SplitRouteApiResponse::ExactOut {
                amount_in,
                discount,
                swap_gas,
                ..
            } => (undiscounted.checked_sub(amount_in.0), discount, swap_gas),
        };
        *discount =
            U128(saved.ok_or_else(|| anyhow::anyhow!("Fee discount made the route worse"))?);
        *swap_gas = *swap_gas + FEE_DISCOUNT_GAS;
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

    use xyk_dex_types::{
        AssetWithBalance, FeeAmount, FeeConfigurationV2, FeeFraction, PayoutGroupShares,
        ProviderShares,
    };

    const POOL_FEE: FeeFraction = FeeFraction::new(3_000); // 0.3%
    /// The protocol's 20% of [`POOL_FEE`]
    const PROTOCOL_FEE: FeeFraction = FeeFraction::new(600);
    const POOL_FEE_AFTER_PROTOCOL: FeeFraction = FeeFraction::new(2_400);

    fn nep141(account_id: &str) -> AssetId {
        AssetId::Nep141(account_id.parse().unwrap())
    }

    fn amount(amount: Balance) -> TokenAmount {
        TokenAmount::new(amount)
    }

    fn fee(amount_in: Balance, fee_fraction: FeeFraction) -> Balance {
        fee_fraction.of(amount(amount_in)).get()
    }

    fn reserves(pool: &Pool) -> [Balance; 2] {
        pool.reserves.0.map(TokenAmount::get)
    }

    /// The pool fee, which the contract takes the protocol's share of
    fn fees() -> FeeConfigurationV2 {
        FeeConfigurationV2 {
            receivers: vec![(FeeReceiver::Pool, FeeAmount::Fixed(POOL_FEE))],
        }
    }

    fn asset(asset_id: AssetId, balance: Balance) -> AssetWithBalance {
        AssetWithBalance {
            asset_id,
            balance: amount(balance),
        }
    }

    fn stored_pool(token0: AssetId, token1: AssetId) -> StoredPool {
        StoredPool::PrivateV2 {
            assets: (
                asset(token0, 10u128.pow(24)),
                asset(token1, 2 * 10u128.pow(24)),
            ),
            _owner_id: "owner.near".parse().unwrap(),
            fees: fees(),
            _locked: false,
        }
    }

    fn private_pool(token0: AssetId, token1: AssetId) -> Pool {
        Pool::new(0, stored_pool(token0, token1), 0).unwrap()
    }

    /// All liquidity providers take their fees with `payout_type`
    fn public_pool(token0: AssetId, token1: AssetId, payout_type: FeePayoutType) -> Pool {
        let shares = ProviderShares::new(10u128.pow(18));
        let mut payout_groups = PayoutGroups::default();
        payout_groups.group_mut(payout_type).shares =
            PayoutGroupShares::NONE.joined_by(shares).unwrap();
        Pool::new(
            0,
            StoredPool::PublicV3 {
                assets: (
                    asset(token0, 10u128.pow(24)),
                    asset(token1, 2 * 10u128.pow(24)),
                ),
                fees: fees(),
                _providers_prefix: vec![],
                total_shares: TotalShares::NONE.minted(shares).unwrap(),
                payout_groups: Box::new(payout_groups),
            },
            0,
        )
        .unwrap()
    }

    fn launch_pool(launched: AssetId) -> Pool {
        launch_pool_with_phantom_liquidity(launched, 0)
    }

    fn launch_pool_with_phantom_liquidity(launched: AssetId, phantom_liquidity: Balance) -> Pool {
        Pool::new(
            0,
            StoredPool::LaunchV2 {
                quote_asset: asset(AssetId::Near, 10u128.pow(24)),
                launched_asset: asset(launched, 2 * 10u128.pow(24)),
                fees: fees(),
                phantom_liquidity: amount(phantom_liquidity),
            },
            0,
        )
        .unwrap()
    }

    #[test]
    fn private_pool_keeps_its_fee() {
        let mut pool = private_pool(nep141("a.near"), nep141("b.near"));
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );

        let mut pool = private_pool(nep141("a.near"), nep141("b.near"));
        let amount_in = pool.swap_exact_out(0, 1, amount_out).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn public_pool_pays_its_fee_to_liquidity_providers() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"), FeePayoutType::KeepBoth);
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in
                    - fee(amount_in, PROTOCOL_FEE)
                    - fee(amount_in, POOL_FEE_AFTER_PROTOCOL),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn public_pool_keeps_the_fee_its_liquidity_providers_reinvest() {
        let mut pool = public_pool(nep141("a.near"), nep141("b.near"), FeePayoutType::Reinvest);
        let [in_before, out_before] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let amount_out = pool.swap_exact_in(0, 1, amount_in).unwrap();
        assert_eq!(
            reserves(&pool),
            [
                in_before + amount_in - fee(amount_in, PROTOCOL_FEE),
                out_before - amount_out
            ]
        );
    }

    #[test]
    fn protocol_fee_is_swapped_to_the_stable_token() {
        let mut pool = launch_pool(nep141("meme.near"));
        let [mut near, mut tokens] = reserves(&pool);
        let amount_in = 10u128.pow(22);
        let after_fees =
            amount_in - fee(amount_in, POOL_FEE_AFTER_PROTOCOL) - fee(amount_in, PROTOCOL_FEE);
        let amount_out = exact_in_amount_out(
            amount(after_fees),
            SwapReserves {
                input: amount(tokens),
                output: amount(near),
            },
        )
        .get();
        tokens += after_fees + fee(amount_in, POOL_FEE_AFTER_PROTOCOL);
        near -= amount_out;
        // The protocol fee, swapped to NEAR on the moved reserves
        near -= exact_in_amount_out(
            amount(fee(amount_in, PROTOCOL_FEE)),
            SwapReserves {
                input: amount(tokens),
                output: amount(near),
            },
        )
        .get();
        tokens += fee(amount_in, PROTOCOL_FEE);

        assert_eq!(pool.swap_exact_in(1, 0, amount_in), Some(amount_out));
        assert_eq!(reserves(&pool), [near, tokens]);
    }

    #[test]
    fn launch_pools_keep_phantom_liquidity_after_converting_fees() {
        let phantom_liquidity = 10u128.pow(23);
        let pool = launch_pool_with_phantom_liquidity(nep141("meme.near"), phantom_liquidity);
        // Leaves 1 more than the phantom liquidity, but converting the fees takes more than that
        let amount_out = reserves(&pool)[0] - phantom_liquidity - 1;
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
        let fee_discount = FeeDiscountFraction::new(100_000);
        assert!(Arc::ptr_eq(&pools.graph_for(None), &pools.graph));
        let discounted = pools.graph_for(Some(fee_discount));
        assert!(!Arc::ptr_eq(&discounted, &pools.graph));
        assert!(Arc::ptr_eq(
            &discounted,
            &pools.graph_for(Some(fee_discount))
        ));
        assert!(
            discounted.pools()[0].info.fees
                == pools.graph.pools()[0]
                    .info
                    .fees
                    .clone()
                    .discounted(fee_discount)
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
                fee_discount: None,
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            SplitRouteApiResponse::ExactIn { swap_gas, .. } if swap_gas == Gas::from_ggas(9_200, 9_200)
        ));
    }
}
