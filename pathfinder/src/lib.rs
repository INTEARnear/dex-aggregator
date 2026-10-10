#![deny(clippy::float_arithmetic)]
#![allow(clippy::manual_div_ceil)]

//! Route search for Rhea, Plach and Rhea DCL over pools indexed from contract storage

use std::sync::{Arc, Mutex};

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use lazy_static::lazy_static;
use near_min_api::RpcClient;
use near_min_api::types::{Balance, BlockHeight, NearGas, NearToken};
use pool_indexer::PoolIndexer;
use tokio::sync::watch;

pub mod dcl;
pub mod plach;
pub mod rhea;
mod search;

pub use search::{MaxHops, QuoteAmount};

pub const MIN_GAS_PRICE: NearToken = NearToken::from_yoctonear(100_000_000);

/// Indexes the pools of every DEX and keeps their snapshots up to date, returns why it stopped
pub async fn run(rpc: Arc<RpcClient>) -> anyhow::Error {
    let (indexer, indexer_task) =
        PoolIndexer::start(vec![rhea::watch(), plach::watch(), dcl::watch()], rpc);
    tokio::select! {
        error = rhea::update_pools(indexer.clone()) => error.context("Rhea"),
        error = plach::update_pools(indexer.clone()) => error.context("Plach"),
        error = dcl::update_pools(indexer) => error.context("Rhea DCL"),
        result = indexer_task => match result {
            Ok(error) => error.context("Pool indexer"),
            Err(e) => anyhow::anyhow!("Pool indexer panicked: {e}"),
        },
    }
}

/// DEXes whose pools are built from indexed storage
#[derive(Debug, Clone, Copy)]
enum PoolsDex {
    Rhea,
    Plach,
    Dcl,
}

lazy_static! {
    static ref POOLS_HEIGHTS: Mutex<[BlockHeight; 3]> = Mutex::new([0; 3]);
    static ref POOLS_UPDATED: watch::Sender<BlockHeight> = watch::Sender::new(0);
}

/// Block height that pools of every DEX are at least as new as. Changes once all DEXes have
/// rebuilt their pools for a newer block, so a route found after a change sees all of them.
pub fn pools_updated() -> watch::Receiver<BlockHeight> {
    POOLS_UPDATED.subscribe()
}

fn pools_built(dex: PoolsDex, height: BlockHeight) {
    let mut heights = POOLS_HEIGHTS.lock().unwrap();
    heights[dex as usize] = height;
    let oldest = *heights.iter().min().unwrap();
    POOLS_UPDATED.send_if_modified(|updated| {
        let newer = oldest > *updated;
        if newer {
            *updated = oldest;
        }
        newer
    });
}

/// Slippage from 0 to 1 in basis points, rounded down
fn slippage_bp(slippage: &BigDecimal) -> Result<u128, anyhow::Error> {
    if *slippage < 0 || *slippage > 1 {
        anyhow::bail!("Invalid slippage {slippage}");
    }
    Ok((slippage * BigDecimal::from(10_000))
        .with_scale_round(0, RoundingMode::Down)
        .to_u128()
        .unwrap())
}

/// What `gas` costs in raw units of a token
fn gas_cost(
    gas: NearGas,
    near_price_raw: &BigDecimal,
    token_price_raw: &BigDecimal,
) -> Result<Balance, anyhow::Error> {
    if *near_price_raw <= 0 || *token_price_raw <= 0 {
        anyhow::bail!("Prices must be positive, NEAR {near_price_raw}, token {token_price_raw}");
    }
    let cost = MIN_GAS_PRICE
        .saturating_mul(gas.as_gas().into())
        .as_yoctonear();
    Ok((BigDecimal::from(cost) * near_price_raw / token_price_raw)
        .with_scale_round(0, RoundingMode::Up)
        .to_u128()
        .unwrap_or(Balance::MAX))
}
