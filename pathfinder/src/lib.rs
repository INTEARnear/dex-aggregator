#![deny(clippy::float_arithmetic)]
#![allow(clippy::manual_div_ceil)]

#[cfg(feature = "indexer")]
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
#[cfg(feature = "indexer")]
use lazy_static::lazy_static;
#[cfg(feature = "indexer")]
use near_min_api::RpcClient;
#[cfg(feature = "indexer")]
use near_min_api::types::{AccountId, BlockHeight};
use near_min_api::types::{Balance, NearGas, NearToken};
#[cfg(feature = "indexer")]
use pool_indexer::PoolIndexer;
#[cfg(feature = "indexer")]
use swap_router_types::indexed::{AccountState, Watch};
#[cfg(feature = "indexer")]
use tokio::sync::watch;

pub mod dcl;
pub mod plach;
pub mod rhea;
mod search;

pub use search::{MaxHops, QuoteAmount};

pub const MIN_GAS_PRICE: NearToken = NearToken::from_yoctonear(100_000_000);

/// Gets the watched storage after a block, by account
#[cfg(feature = "indexer")]
pub type OnIndexedState =
    Arc<dyn Fn(Vec<(AccountId, AccountState)>) -> Result<(), String> + Send + Sync>;

/// A hook that follows indexed storage
#[cfg(feature = "indexer")]
pub struct IndexedHook {
    pub name: String,
    pub watches: Vec<Watch>,
    /// Called after every block, on a blocking thread
    pub on_state: OnIndexedState,
}

/// Indexes the pools of Rhea and Rhea DCL, and the storage that hooks watch, and keeps their
/// snapshots up to date. Returns why it stopped.
#[cfg(feature = "indexer")]
pub async fn run(rpc: Arc<RpcClient>, hooks: Vec<IndexedHook>) -> anyhow::Error {
    let watches = [rhea::watch(), dcl::watch()]
        .into_iter()
        .chain(hooks.iter().flat_map(|hook| hook.watches.clone()));
    let (indexer, indexer_task) = PoolIndexer::start(merge_watches(watches), rpc);
    POOLS_HEIGHTS
        .lock()
        .unwrap()
        .resize(PoolsDex::Hook(hooks.len()).index(), 0);
    let mut hook_updates = tokio::task::JoinSet::new();
    for (index, hook) in hooks.into_iter().enumerate() {
        hook_updates.spawn(update_hook(indexer.clone(), hook, index));
    }
    tokio::select! {
        error = rhea::update_pools(indexer.clone()) => error.context("Rhea"),
        error = dcl::update_pools(indexer) => error.context("Rhea DCL"),
        Some(result) = hook_updates.join_next() => match result {
            Ok(error) => error,
            Err(e) => anyhow::anyhow!("Updating a hook panicked: {e}"),
        },
        result = indexer_task => match result {
            Ok(error) => error.context("Pool indexer"),
            Err(e) => anyhow::anyhow!("Pool indexer panicked: {e}"),
        },
    }
}

/// One watch per account, with the prefixes of all of its watches
#[cfg(feature = "indexer")]
fn merge_watches(watches: impl IntoIterator<Item = Watch>) -> Vec<Watch> {
    let mut prefixes_by_account = BTreeMap::<AccountId, Vec<Vec<u8>>>::new();
    for watch in watches {
        let prefixes = prefixes_by_account.entry(watch.account_id).or_default();
        for prefix in watch.prefixes {
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
    }
    prefixes_by_account
        .into_iter()
        .map(|(account_id, prefixes)| Watch {
            account_id,
            prefixes,
        })
        .collect()
}

/// Gives the hook its watched storage after every block, returns why it stopped
#[cfg(feature = "indexer")]
async fn update_hook(indexer: PoolIndexer, hook: IndexedHook, index: usize) -> anyhow::Error {
    let mut blocks = indexer.blocks();
    loop {
        if blocks.changed().await.is_err() {
            return anyhow::anyhow!("Pool indexer stopped");
        }
        let states = hook
            .watches
            .iter()
            .map(|watch| {
                let mut state = indexer.account_state(&watch.account_id)?;
                state
                    .entries
                    .retain(|key, _| watch.prefixes.iter().any(|prefix| key.starts_with(prefix)));
                Some((watch.account_id.clone(), state))
            })
            .collect::<Option<Vec<_>>>();
        let Some(states) = states else {
            continue;
        };
        let Some(height) = states.iter().map(|(_, state)| state.block.height).min() else {
            continue;
        };
        let on_state = hook.on_state.clone();
        match tokio::task::spawn_blocking(move || on_state(states)).await {
            Ok(Ok(())) => pools_built(PoolsDex::Hook(index), height),
            Ok(Err(err)) => {
                return anyhow::anyhow!("{} failed at block {height}: {err}", hook.name);
            }
            Err(e) => return anyhow::anyhow!("{} panicked at block {height}: {e}", hook.name),
        }
    }
}

/// DEXes and hooks whose pools are built from indexed storage
#[cfg(feature = "indexer")]
#[derive(Debug, Clone, Copy)]
enum PoolsDex {
    Rhea,
    Dcl,
    Hook(usize),
}

#[cfg(feature = "indexer")]
impl PoolsDex {
    fn index(self) -> usize {
        match self {
            PoolsDex::Rhea => 0,
            PoolsDex::Dcl => 1,
            PoolsDex::Hook(hook) => 2 + hook,
        }
    }
}

#[cfg(feature = "indexer")]
lazy_static! {
    /// By [`PoolsDex::index`]
    static ref POOLS_HEIGHTS: Mutex<Vec<BlockHeight>> =
        Mutex::new(vec![0; PoolsDex::Hook(0).index()]);
    static ref POOLS_UPDATED: watch::Sender<BlockHeight> = watch::Sender::new(0);
}

/// Block height that pools of every DEX and hook are at least as new as. Changes once all of them
/// have rebuilt their pools for a newer block, so a route found after a change sees all of them.
#[cfg(feature = "indexer")]
pub fn pools_updated() -> watch::Receiver<BlockHeight> {
    POOLS_UPDATED.subscribe()
}

#[cfg(feature = "indexer")]
fn pools_built(dex: PoolsDex, height: BlockHeight) {
    let mut heights = POOLS_HEIGHTS.lock().unwrap();
    heights[dex.index()] = height;
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
