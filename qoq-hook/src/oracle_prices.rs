use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use amp_dex_types::{PoolId, PricesRequest, PricesResponse};
use intear_dex_common::AssetId;
use near_sdk::PublicKeyHandle;
use pathfinder::qoq::OraclePrices;
use serde::Deserialize;
use tokio::time::MissedTickBehavior;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const FETCH_TIMEOUT: Duration = Duration::from_secs(1);
/// The oracle responds to a swap with its prices of a few blocks later, so quotes aren't made at
/// older prices than this
const MAX_PRICES_AGE: Duration = Duration::from_secs(3);

/// The assets of each pool, which swaps on it ask the oracle for the prices of
type PoolsAssets = Vec<(PoolId, [AssetId; 2])>;
/// The latest prices, and when they were fetched
type LatestPrices = Option<(Arc<OraclePrices>, Instant)>;

/// The medians of the oracle's prices of the pools, fetched from its HTTP API every
/// [`POLL_INTERVAL`]
pub struct OraclePricesFeed {
    pools: Arc<RwLock<PoolsAssets>>,
    latest: Arc<RwLock<LatestPrices>>,
}

impl OraclePricesFeed {
    pub fn start(oracle_url: String) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("Can't start a runtime to fetch oracle prices: {err}"))?;
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|err| format!("Can't build a client to fetch oracle prices: {err}"))?;
        let pools = Arc::<RwLock<PoolsAssets>>::default();
        let latest = Arc::<RwLock<LatestPrices>>::default();
        let (pools_to_price, fetched) = (Arc::clone(&pools), Arc::clone(&latest));
        let prices_url = format!("{oracle_url}/prices");
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let mut interval = tokio::time::interval(POLL_INTERVAL);
                interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let pools = pools_to_price.read().unwrap().clone();
                    if pools.is_empty() {
                        continue;
                    }
                    match fetch(&client, &prices_url).await {
                        Ok(view) => {
                            *fetched.write().unwrap() = Some((
                                Arc::new(oracle_prices_of_pools(view, &pools)),
                                Instant::now(),
                            ))
                        }
                        Err(err) => eprintln!("No Qoq oracle prices from {prices_url}: {err}"),
                    }
                }
            })
        });
        Ok(Self { pools, latest })
    }

    /// The pools whose prices are fetched from now on
    pub fn set_pools(&self, pools: PoolsAssets) {
        *self.pools.write().unwrap() = pools;
    }

    /// Without prices, which it doesn't fetch
    #[cfg(test)]
    pub fn without_prices() -> Self {
        Self {
            pools: Arc::default(),
            latest: Arc::default(),
        }
    }

    /// The latest prices, unless they're older than [`MAX_PRICES_AGE`]
    pub fn fresh(&self) -> Option<Arc<OraclePrices>> {
        let latest = self.latest.read().unwrap();
        let (oracle_prices, fetched_at) = latest.as_ref()?;
        (fetched_at.elapsed() <= MAX_PRICES_AGE).then(|| Arc::clone(oracle_prices))
    }
}

/// What `GET /prices` of the oracle's HTTP API answers: every asset it prices, and the response
/// to a request of the prices of all of them
#[derive(Deserialize)]
struct PricesView {
    oracle_key: PublicKeyHandle,
    asset_ids: Vec<AssetId>,
    response: PricesResponse,
}

async fn fetch(client: &reqwest::Client, prices_url: &str) -> Result<PricesView, reqwest::Error> {
    client
        .get(prices_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

/// The medians that swaps would rate the pools at, of the pools whose assets the oracle prices
/// within the spread the dex allows
fn oracle_prices_of_pools(view: PricesView, pools: &PoolsAssets) -> OraclePrices {
    let request = PricesRequest {
        asset_ids: view.asset_ids,
    };
    OraclePrices {
        oracle_key: view.oracle_key,
        of_pools: pools
            .iter()
            .filter_map(|(pool_id, [asset_0, asset_1])| {
                let price = view
                    .response
                    .median_price(&request, asset_0, asset_1)
                    .ok()?;
                Some((*pool_id, price))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";

    fn pool(pool_id: u32, asset_1: &str) -> (PoolId, [AssetId; 2]) {
        (
            PoolId::new(pool_id),
            [AssetId::Near, AssetId::Nep141(asset_1.parse().unwrap())],
        )
    }

    #[test]
    fn reads_the_prices_the_oracle_serves() {
        let view: PricesView = serde_json::from_value(serde_json::json!({
            "oracle_key": ORACLE_KEY,
            "asset_ids": ["near", "nep141:usdc.near"],
            "response": {
                "exchanges": [{ "exchange": "okx", "prices": ["3.741", "1.001"] }],
            },
        }))
        .unwrap();
        assert_eq!(view.oracle_key, ORACLE_KEY.parse().unwrap());
        assert_eq!(
            view.asset_ids,
            vec![AssetId::Near, AssetId::Nep141("usdc.near".parse().unwrap())]
        );
        assert_eq!(
            view.response.exchanges[0].prices[1],
            "1.001".parse().unwrap()
        );
    }

    #[test]
    fn prices_the_pools_that_swaps_would_rate_at_the_median() {
        // NEAR, USDC and USDT, which okx prices more than 0.5% apart from the others
        let view: PricesView = serde_json::from_value(serde_json::json!({
            "oracle_key": ORACLE_KEY,
            "asset_ids": ["near", "nep141:usdc.near", "nep141:usdt.near"],
            "response": {
                "exchanges": [
                    { "exchange": "binance", "prices": ["2.4", "1", "1"] },
                    { "exchange": "kraken", "prices": ["2.41", "1", "1"] },
                    { "exchange": "okx", "prices": ["2.405", "1", "1.04"] },
                ],
            },
        }))
        .unwrap();
        let pools = vec![
            pool(0, "usdc.near"),
            pool(1, "usdt.near"),
            // An asset the oracle doesn't price
            pool(2, "eurc.near"),
        ];
        let oracle_prices = oracle_prices_of_pools(view, &pools);
        assert_eq!(oracle_prices.oracle_key, ORACLE_KEY.parse().unwrap());
        assert_eq!(
            oracle_prices.of_pools,
            HashMap::from([(PoolId::new(0), "2.405".parse().unwrap())])
        );
    }
}
