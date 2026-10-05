#![deny(clippy::float_arithmetic)]
#![allow(clippy::manual_div_ceil)]

//! Route search for Rhea, Plach and Rhea DCL over pools indexed from contract storage

use std::sync::Arc;

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use near_min_api::RpcClient;
use pool_indexer::PoolIndexer;

pub mod dcl;
pub mod plach;
pub mod rhea;
mod search;

pub use search::{MaxHops, QuoteAmount};

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
