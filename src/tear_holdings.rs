use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use intear_dex_common::{FeeDiscountFraction, FULL_FEE_DISCOUNT};
use near_min_api::{
    types::{AccountId, Balance, Finality},
    QueryFinality,
};
use near_sdk::json_types::U128;
use serde::Deserialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::warn;

use crate::shared_utils::RPC_CLIENT;

const TEAR_TOKEN_ID: &str = "token.intear.near";
const TEAR_STAKING_CONTRACT_ID: &str = "stakeyourtears.intear.near";
const ONE_TEAR: Balance = 10u128.pow(24);
const MAX_HOLDINGS_AGE: Duration = Duration::from_secs(60);

const TEAR_FEE_DISCOUNTS: [(Balance, FeeDiscountFraction); 3] = [
    // 100,000 = 75% discount
    (
        100_000 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 4 * 3),
    ),
    // 1,000 = 25% discount
    (
        1_000 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 4),
    ),
    // 10 = 10% discount
    (
        10 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 10),
    ),
];

/// The discount of a trader who holds and stakes `tear_balance` TEAR
pub fn tear_fee_discount(tear_balance: Balance) -> Option<FeeDiscountFraction> {
    TEAR_FEE_DISCOUNTS
        .into_iter()
        .find(|(min_balance, _)| tear_balance >= *min_balance)
        .map(|(_, fee_discount)| fee_discount)
}

static TEAR_HOLDINGS: OnceLock<TearHoldings> = OnceLock::new();

/// Fetches holdings of the traders that fee discounts are asked for, on the current runtime
pub fn start_fetching() {
    if TEAR_HOLDINGS.set(TearHoldings::start()).is_err() {
        panic!("TEAR holdings are fetched once");
    }
}

/// The fee discount of what the trader holds, `None` until it's fetched
pub fn fee_discount_of(trader_id: &AccountId) -> Option<FeeDiscountFraction> {
    TEAR_HOLDINGS
        .get()
        .expect("TEAR holdings are fetched from startup")
        .fee_discount_of(trader_id)
}

/// What a trader holds, `None` until it's fetched, and when it was last asked for
type HoldingsByTrader = Arc<Mutex<HashMap<AccountId, (Option<Balance>, Instant)>>>;

struct TearHoldings {
    by_trader: HoldingsByTrader,
    fetch: UnboundedSender<AccountId>,
}

impl TearHoldings {
    fn start() -> Self {
        let by_trader = HoldingsByTrader::default();
        let (fetch, mut traders_to_fetch) = unbounded_channel::<AccountId>();
        let fetched = Arc::clone(&by_trader);
        tokio::spawn(async move {
            while let Some(trader_id) = traders_to_fetch.recv().await {
                let fetched = Arc::clone(&fetched);
                tokio::spawn(async move {
                    match fetch_holdings(&trader_id).await {
                        Ok(holdings) => {
                            fetched
                                .lock()
                                .unwrap()
                                .insert(trader_id, (Some(holdings), Instant::now()));
                        }
                        Err(err) => warn!("No TEAR holdings of {trader_id}: {err}"),
                    }
                });
            }
        });
        Self { by_trader, fetch }
    }

    /// The fee discount of what the trader holds, `None` until it's fetched. Fetches it if it's
    /// missing or old.
    fn fee_discount_of(&self, trader_id: &AccountId) -> Option<FeeDiscountFraction> {
        self.of(trader_id).and_then(tear_fee_discount)
    }

    /// What the trader holds, `None` until it's fetched. Fetches it if it's missing or old.
    fn of(&self, trader_id: &AccountId) -> Option<Balance> {
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
            .expect("TEAR holdings are fetched as long as the aggregator runs");
    }

    /// With the traders it's asked to fetch, which it doesn't fetch
    #[cfg(test)]
    fn known(
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
async fn fetch_holdings(trader_id: &AccountId) -> Result<Balance, String> {
    #[derive(Deserialize)]
    struct FtLockupAccount {
        locked: U128,
    }

    let finality = || QueryFinality::Finality(Finality::DoomSlug);
    let (wallet, staked) = tokio::try_join!(
        RPC_CLIENT.call::<U128>(
            TEAR_TOKEN_ID.parse().unwrap(),
            "ft_balance_of",
            serde_json::json!({ "account_id": trader_id }),
            finality(),
        ),
        RPC_CLIENT.call::<Option<FtLockupAccount>>(
            TEAR_STAKING_CONTRACT_ID.parse().unwrap(),
            "get_account",
            serde_json::json!({ "account_id": trader_id }),
            finality(),
        ),
    )
    .map_err(|err| format!("{err:?}"))?;
    Ok(wallet.0 + staked.map_or(0, |account| account.locked.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_discount_grows_with_tear_holdings() {
        let fee_discount_of =
            |tear_balance| tear_fee_discount(tear_balance).map(FeeDiscountFraction::get);
        assert_eq!(fee_discount_of(0), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR - 1), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR - 1), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR), Some(250_000));
        assert_eq!(fee_discount_of(100_000 * ONE_TEAR), Some(750_000));
        assert_eq!(fee_discount_of(u128::MAX), Some(750_000));
    }

    #[test]
    fn no_discount_until_holdings_are_known() {
        let (tear_holdings, mut traders_to_fetch) = TearHoldings::known([]);
        let trader_id: AccountId = "trader.near".parse().unwrap();
        assert_eq!(tear_holdings.fee_discount_of(&trader_id), None);
        assert_eq!(traders_to_fetch.try_recv(), Ok(trader_id.clone()));
        // Asked for once while it's fetched
        assert_eq!(tear_holdings.fee_discount_of(&trader_id), None);
        assert!(traders_to_fetch.try_recv().is_err());
    }
}
