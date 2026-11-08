use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use amp_dex_types::{PoolId, PricesRequest, PricesResponse};
use intear_dex_common::AssetId;
use near_sdk::PublicKeyHandle;
use pathfinder::qoq::OraclePrices;
use serde::Deserialize;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The oracle responds to a swap with its prices of a few blocks later, so quotes aren't made at
/// older prices than this
const MAX_PRICES_AGE: Duration = Duration::from_secs(3);

/// The assets of each pool, which its prices are asked for with
type PoolsAssets = Vec<(PoolId, [AssetId; 2])>;
/// The latest prices, and when they were fetched
type LatestPrices = Option<(Arc<OraclePrices>, Instant)>;

/// The medians of the oracle's prices of the pools, fetched from its HTTP API every second
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
            .timeout(POLL_INTERVAL)
            .build()
            .map_err(|err| format!("Can't build a client to fetch oracle prices: {err}"))?;
        let pools = Arc::<RwLock<PoolsAssets>>::default();
        let latest = Arc::<RwLock<LatestPrices>>::default();
        let (pools_to_price, fetched) = (Arc::clone(&pools), Arc::clone(&latest));
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let mut interval = tokio::time::interval(POLL_INTERVAL);
                loop {
                    interval.tick().await;
                    let pools = pools_to_price.read().unwrap().clone();
                    if pools.is_empty() {
                        continue;
                    }
                    match fetch(&client, &oracle_url, &pools).await {
                        Ok(oracle_prices) => {
                            *fetched.write().unwrap() =
                                Some((Arc::new(oracle_prices), Instant::now()))
                        }
                        Err(err) => eprintln!("No Qoq oracle prices: {err}"),
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

/// What `GET /prices/{asset_ids}` of the oracle's HTTP API answers
#[derive(Deserialize)]
struct PricesView {
    oracle_key: PublicKeyHandle,
    response: PricesResponse,
}

async fn fetch(
    client: &reqwest::Client,
    oracle_url: &str,
    pools: &PoolsAssets,
) -> Result<OraclePrices, String> {
    let views =
        futures_util::future::join_all(pools.iter().map(|(_, [asset_0, asset_1])| async move {
            let url = format!("{oracle_url}/prices/{asset_0},{asset_1}");
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|err| format!("{url}: {err}"))?;
            // Of assets that the oracle doesn't price, so their pools ask another oracle
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let response = response
                .error_for_status()
                .map_err(|err| format!("{url}: {err}"))?;
            response
                .json::<PricesView>()
                .await
                .map(Some)
                .map_err(|err| format!("{url}: {err}"))
        }))
        .await;
    let views = views.into_iter().collect::<Result<Vec<_>, _>>()?;
    oracle_prices_of_pools(pools.iter().zip(views))
}

/// The medians that swaps would rate the pools at, of the pools that the oracle prices within the
/// spread the dex allows
fn oracle_prices_of_pools<'a>(
    views: impl IntoIterator<Item = (&'a (PoolId, [AssetId; 2]), Option<PricesView>)>,
) -> Result<OraclePrices, String> {
    let mut oracle_key = None;
    let mut of_pools = HashMap::new();
    for ((pool_id, [asset_0, asset_1]), view) in views {
        let Some(PricesView {
            oracle_key: pool_oracle_key,
            response,
        }) = view
        else {
            continue;
        };
        match &oracle_key {
            None => oracle_key = Some(pool_oracle_key),
            Some(oracle_key) if *oracle_key != pool_oracle_key => {
                return Err(format!(
                    "The oracle answered with {oracle_key} and {pool_oracle_key}"
                ));
            }
            Some(_) => {}
        }
        let request = PricesRequest::of_pools([(asset_0, asset_1)]);
        if let Ok(price) = response.median_price(&request, asset_0, asset_1) {
            of_pools.insert(*pool_id, price);
        }
    }
    Ok(OraclePrices {
        oracle_key: oracle_key.ok_or("The oracle prices none of the pools")?,
        of_pools,
    })
}

#[cfg(test)]
mod tests {
    use amp_dex_types::ExchangePrices;

    use super::*;

    const ORACLE_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const OTHER_ORACLE_KEY: &str = "ed25519:US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx";

    fn pool(pool_id: u32, asset_1: &str) -> (PoolId, [AssetId; 2]) {
        (
            PoolId::new(pool_id),
            [AssetId::Near, AssetId::Nep141(asset_1.parse().unwrap())],
        )
    }

    /// Of NEAR and another asset on three exchanges, where NEAR is `near_prices`
    fn view(oracle_key: &str, near_prices: [&str; 3]) -> PricesView {
        PricesView {
            oracle_key: oracle_key.parse().unwrap(),
            response: PricesResponse {
                exchanges: ["binance", "kraken", "okx"]
                    .into_iter()
                    .zip(near_prices)
                    .map(|(exchange, near_price)| ExchangePrices {
                        exchange: exchange.to_string(),
                        prices: vec![near_price.parse().unwrap(), "1".parse().unwrap()],
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn prices_the_pools_that_swaps_would_rate_at_the_median() {
        let pools = [
            pool(0, "usdc.near"),
            pool(1, "usdt.near"),
            pool(2, "eurc.near"),
        ];
        let oracle_prices = oracle_prices_of_pools([
            (&pools[0], Some(view(ORACLE_KEY, ["2.4", "2.41", "2.405"]))),
            // More than 0.5% apart, so swaps on the pool are denied
            (&pools[1], Some(view(ORACLE_KEY, ["2.4", "2.5", "2.45"]))),
            // An asset the oracle doesn't price
            (&pools[2], None),
        ])
        .unwrap();
        assert_eq!(oracle_prices.oracle_key, ORACLE_KEY.parse().unwrap());
        assert_eq!(
            oracle_prices.of_pools,
            HashMap::from([(PoolId::new(0), "2.405".parse().unwrap())])
        );
    }

    #[test]
    fn an_oracle_answers_with_one_key() {
        let pools = [pool(0, "usdc.near"), pool(1, "usdt.near")];
        assert!(
            oracle_prices_of_pools([
                (&pools[0], Some(view(ORACLE_KEY, ["2.4", "2.4", "2.4"]))),
                (
                    &pools[1],
                    Some(view(OTHER_ORACLE_KEY, ["2.4", "2.4", "2.4"]))
                ),
            ])
            .is_err()
        );
        assert!(oracle_prices_of_pools([(&pools[0], None)]).is_err());
    }
}
