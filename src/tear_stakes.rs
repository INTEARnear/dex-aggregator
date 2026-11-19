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

const TEAR_STAKING_CONTRACT_ID: &str = "stakeyourtears.intear.near";
const ONE_TEAR: Balance = 10u128.pow(24);
/// A stake only changes when its trader stakes or starts unlocking, unlike a wallet's balance
const MAX_STAKE_AGE: Duration = Duration::from_secs(60 * 60);

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

/// The discount of a trader who stakes `staked` TEAR
fn tear_fee_discount(staked: Balance) -> Option<FeeDiscountFraction> {
    TEAR_FEE_DISCOUNTS
        .into_iter()
        .find(|(min_stake, _)| staked >= *min_stake)
        .map(|(_, fee_discount)| fee_discount)
}

static TEAR_STAKES: OnceLock<TearStakes> = OnceLock::new();

/// Fetches the stakes of the traders that fee discounts are asked for, on the current runtime
pub fn start_fetching() {
    if TEAR_STAKES.set(TearStakes::start()).is_err() {
        panic!("TEAR stakes are fetched once");
    }
}

/// The fee discount of the trader's TEAR stake, `None` until it's fetched
pub fn fee_discount_of(trader_id: &AccountId) -> Option<FeeDiscountFraction> {
    TEAR_STAKES
        .get()
        .expect("TEAR stakes are fetched from startup")
        .fee_discount_of(trader_id)
}

/// The fee discount of the trader's TEAR stake as of now, `None` if it's too little for one
pub async fn fetch_fee_discount(
    trader_id: &AccountId,
) -> Result<Option<FeeDiscountFraction>, String> {
    Ok(tear_fee_discount(fetch_stake(trader_id).await?))
}

/// What a trader stakes, `None` until it's fetched, and when it was last asked for
type StakeByTrader = Arc<Mutex<HashMap<AccountId, (Option<Balance>, Instant)>>>;

struct TearStakes {
    by_trader: StakeByTrader,
    fetch: UnboundedSender<AccountId>,
}

impl TearStakes {
    fn start() -> Self {
        let by_trader = StakeByTrader::default();
        let (fetch, mut traders_to_fetch) = unbounded_channel::<AccountId>();
        let fetched = Arc::clone(&by_trader);
        tokio::spawn(async move {
            while let Some(trader_id) = traders_to_fetch.recv().await {
                let fetched = Arc::clone(&fetched);
                tokio::spawn(async move {
                    match fetch_stake(&trader_id).await {
                        Ok(stake) => {
                            fetched
                                .lock()
                                .unwrap()
                                .insert(trader_id, (Some(stake), Instant::now()));
                        }
                        Err(err) => warn!("No TEAR stake of {trader_id}: {err}"),
                    }
                });
            }
        });
        Self { by_trader, fetch }
    }

    /// The fee discount of the trader's stake, `None` until it's fetched. Fetches it if it's
    /// missing or old.
    fn fee_discount_of(&self, trader_id: &AccountId) -> Option<FeeDiscountFraction> {
        self.of(trader_id).and_then(tear_fee_discount)
    }

    /// What the trader stakes, `None` until it's fetched. Fetches it if it's missing or old.
    fn of(&self, trader_id: &AccountId) -> Option<Balance> {
        let mut by_trader = self.by_trader.lock().unwrap();
        match by_trader.get_mut(trader_id) {
            Some((stake, asked_at)) => {
                if asked_at.elapsed() > MAX_STAKE_AGE {
                    *asked_at = Instant::now();
                    self.fetch_in_background(trader_id);
                }
                *stake
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
            .expect("TEAR stakes are fetched as long as the aggregator runs");
    }

    /// With the traders it's asked to fetch, which it doesn't fetch
    #[cfg(test)]
    fn known(
        stakes: impl IntoIterator<Item = (AccountId, Balance)>,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<AccountId>) {
        let (fetch, traders_to_fetch) = unbounded_channel();
        let by_trader = stakes
            .into_iter()
            .map(|(trader_id, stake)| (trader_id, (Some(stake), Instant::now())))
            .collect();
        let tear_stakes = Self {
            by_trader: Arc::new(Mutex::new(by_trader)),
            fetch,
        };
        (tear_stakes, traders_to_fetch)
    }
}

/// TEAR that the trader has staked and isn't unlocking
async fn fetch_stake(trader_id: &AccountId) -> Result<Balance, String> {
    #[derive(Deserialize)]
    struct FtLockupAccount {
        locked: U128,
    }

    let account = RPC_CLIENT
        .call::<Option<FtLockupAccount>>(
            TEAR_STAKING_CONTRACT_ID.parse().unwrap(),
            "get_account",
            serde_json::json!({ "account_id": trader_id }),
            QueryFinality::Finality(Finality::DoomSlug),
        )
        .await
        .map_err(|err| format!("{err:?}"))?;
    Ok(account.map_or(0, |account| account.locked.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_discount_grows_with_the_stake() {
        let fee_discount_of = |staked| tear_fee_discount(staked).map(FeeDiscountFraction::get);
        assert_eq!(fee_discount_of(0), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR - 1), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR - 1), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR), Some(250_000));
        assert_eq!(fee_discount_of(100_000 * ONE_TEAR), Some(750_000));
        assert_eq!(fee_discount_of(u128::MAX), Some(750_000));
    }

    #[test]
    fn no_discount_until_the_stake_is_known() {
        let (tear_stakes, mut traders_to_fetch) = TearStakes::known([]);
        let trader_id: AccountId = "trader.near".parse().unwrap();
        assert_eq!(tear_stakes.fee_discount_of(&trader_id), None);
        assert_eq!(traders_to_fetch.try_recv(), Ok(trader_id.clone()));
        // Asked for once while it's fetched
        assert_eq!(tear_stakes.fee_discount_of(&trader_id), None);
        assert!(traders_to_fetch.try_recv().is_err());
    }

    #[test]
    fn a_known_stake_gives_its_discount_without_fetching_it() {
        let trader_id: AccountId = "trader.near".parse().unwrap();
        let (tear_stakes, mut traders_to_fetch) =
            TearStakes::known([(trader_id.clone(), 1_000 * ONE_TEAR)]);
        assert_eq!(
            tear_stakes
                .fee_discount_of(&trader_id)
                .map(FeeDiscountFraction::get),
            Some(250_000)
        );
        assert!(traders_to_fetch.try_recv().is_err());
    }
}
