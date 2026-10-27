use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use near_min_api::{
    QueryFinality, RpcClient,
    types::{AccountId, Balance, Finality},
};
use near_sdk::json_types::U128;
use serde::Deserialize;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::fee_discount::{TEAR_STAKING_CONTRACT_ID, TEAR_TOKEN_ID};

const MAX_HOLDINGS_AGE: Duration = Duration::from_secs(60);

/// What a trader holds, `None` until it's fetched, and when it was last asked for
type HoldingsByTrader = Arc<Mutex<HashMap<AccountId, (Option<Balance>, Instant)>>>;

pub struct TearHoldings {
    by_trader: HoldingsByTrader,
    fetch: UnboundedSender<AccountId>,
}

impl TearHoldings {
    pub fn start() -> Result<Self, String> {
        let rpc_urls = std::env::var("RPC_URLS").unwrap_or_else(|_| {
            "https://rpc.intea.rs,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                .to_string()
        });
        let rpc = Arc::new(RpcClient::new(rpc_urls.split(',')));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("Can't start a runtime to fetch TEAR holdings: {err}"))?;
        let by_trader = HoldingsByTrader::default();
        let (fetch, mut traders_to_fetch) = unbounded_channel::<AccountId>();
        let fetched = Arc::clone(&by_trader);
        std::thread::spawn(move || {
            runtime.block_on(async move {
                while let Some(trader_id) = traders_to_fetch.recv().await {
                    let rpc = Arc::clone(&rpc);
                    let fetched = Arc::clone(&fetched);
                    tokio::spawn(async move {
                        match fetch_holdings(&rpc, &trader_id).await {
                            Ok(holdings) => {
                                fetched
                                    .lock()
                                    .unwrap()
                                    .insert(trader_id, (Some(holdings), Instant::now()));
                            }
                            Err(err) => eprintln!("No TEAR holdings of {trader_id}: {err}"),
                        }
                    });
                }
            })
        });
        Ok(Self { by_trader, fetch })
    }

    /// What the trader holds, `None` until it's fetched. Fetches it if it's missing or old.
    pub fn of(&self, trader_id: &AccountId) -> Option<Balance> {
        let mut by_trader = self.by_trader.lock().unwrap();
        match by_trader.get_mut(trader_id) {
            Some((holdings, asked_at)) => {
                if asked_at.elapsed() > MAX_HOLDINGS_AGE {
                    *asked_at = Instant::now();
                    self.fetch_in_background(trader_id);
                }
                *holdings
            }
            None => {
                by_trader.insert(trader_id.clone(), (None, Instant::now()));
                self.fetch_in_background(trader_id);
                None
            }
        }
    }

    fn fetch_in_background(&self, trader_id: &AccountId) {
        self.fetch
            .send(trader_id.clone())
            .expect("TEAR holdings are fetched as long as the hook is loaded");
    }

    /// With the traders it's asked to fetch
    #[cfg(test)]
    pub fn known(
        holdings: impl IntoIterator<Item = (AccountId, Balance)>,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<AccountId>) {
        let (fetch, traders_to_fetch) = unbounded_channel();
        let by_trader = holdings
            .into_iter()
            .map(|(trader_id, holdings)| (trader_id, (Some(holdings), Instant::now())))
            .collect();
        let tear_holdings = Self {
            by_trader: Arc::new(Mutex::new(by_trader)),
            fetch,
        };
        (tear_holdings, traders_to_fetch)
    }
}

/// TEAR in the trader's wallet and staked
async fn fetch_holdings(rpc: &RpcClient, trader_id: &AccountId) -> Result<Balance, String> {
    #[derive(Deserialize)]
    struct FtLockupAccount {
        locked: U128,
    }

    let finality = || QueryFinality::Finality(Finality::DoomSlug);
    let (wallet, staked) = tokio::try_join!(
        rpc.call::<U128>(
            TEAR_TOKEN_ID.parse().unwrap(),
            "ft_balance_of",
            serde_json::json!({ "account_id": trader_id }),
            finality(),
        ),
        rpc.call::<Option<FtLockupAccount>>(
            TEAR_STAKING_CONTRACT_ID.parse().unwrap(),
            "get_account",
            serde_json::json!({ "account_id": trader_id }),
            finality(),
        ),
    )
    .map_err(|err| format!("{err:?}"))?;
    Ok(wallet.0 + staked.map_or(0, |account| account.locked.0))
}
